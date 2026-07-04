//! `par` —— 在 [`StealingPool`](crate::v3_stealing::StealingPool) 上跑的并行算法。
//!
//! M9a-par 子模块。把 M9a 的工作窃取线程池当执行器,实现三类经典并行算法
//! (Williams《C++ Concurrency in Action》Ch8/Ch10):
//!
//! 1. [`par_sort`] —— 并行快速排序。分治的天然并行:partition 之后左右两半
//!    互不相关,可以分头排。"spawn 一半 + 本线程排另一半" 让本线程永远
//!    有活干,空了再去偷 spawn 出去的那一半(详见教程的递归树手算)。
//! 2. [`par_map`] / [`par_reduce`] —— 切片并行。把切片切成 N 段,每段一个
//!    任务并行 map,最后串行 reduce 汇聚。
//! 3. [`par_iter`] —— rayon 风格的惰性并行迭代器。`.map().filter()` 每个
//!    adapter 返回新的 [`ParIter`],终端方法 [`ParIter::sum`] 是 sink,
//!    触发工作窃取调度遍历。
//!
//! 设计要点:
//! - 所有 API 都接受 `&Arc<StealingPool>`(任务闭包要 `'static` 捕获 pool 句柄)。
//! - 任务粒度有下限(`PAR_SORT_CUTOFF` / `PAR_MAP_CUTOFF`):数据小于阈值时
//!   直接走串行版本,避免 spawn / 偷窃的 ~1–10μs 开销反而拖慢(详见教程
//!   Amdahl 段落的手算)。
//! - 嵌套 spawn 安全:V3 的 `JoinHandle::recv` 在 worker 线程上不 park,
//!   而是边等边跑别的任务,所以 par_sort 递归里 spawn 子任务再 recv 等
//!   结果不会死锁(这正是 M9a 的核心保证,见 m9a_nested_spawn 测试)。

use crate::StealingPool;
use std::sync::Arc;

/// 把一个裸指针包成 `Send + Sync`,让我们能在 spawn 的闭包里把它发到别的线程。
///
/// **安全责任**:使用者必须保证——这个指针指向的内存在闭包运行期间存活,
/// 且本指针与其它指针指向的区间互不重叠(Rust 借用检查器无法静态表达
/// "多个可变借用分给不同线程",所以这里手工保证)。本模块的所有用法都
/// 满足这一约束(切段时各段 start..end 互不重叠、且 recv 在父任务返回前完成)。
struct SendPtr<T>(*mut T);

unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        SendPtr(self.0)
    }
}
impl<T> Copy for SendPtr<T> {}

struct SendConstPtr<T>(*const T);

unsafe impl<T> Send for SendConstPtr<T> {}
unsafe impl<T> Sync for SendConstPtr<T> {}

impl<T> Clone for SendConstPtr<T> {
    fn clone(&self) -> Self {
        SendConstPtr(self.0)
    }
}
impl<T> Copy for SendConstPtr<T> {}

/// 并行快排的串行切换阈值。元素数 ≤ 此值时直接调标准库的稳定排序,
/// 不再 spawn 任务。这个数是工程上常见的折中——
/// 太小会让 spawn 开销 dominate,太大会让并行度不足。
/// 教程里会手算为什么这个数大概在几百到几千之间。
const PAR_SORT_CUTOFF: usize = 1024;

/// 并行 map 的单任务最小切片长度。短于此值就把整段并进一个任务里跑。
const PAR_MAP_CUTOFF: usize = 256;

/// 并行地把 `slice` 排成升序。
///
/// 算法(Williams Ch10 经典):
/// 1. 选一个 pivot(我们取"首中末三数的中位数",避免有序输入退化成平方);
/// 2. 把 `slice` 切成 `[< pivot | pivot | ≥ pivot]` 三段(三路 partition,
///    对大量重复元素也保持 O(n));
/// 3. 左半 `pool.spawn(par_sort)` 排、右半**本线程递归**排;
/// 4. `recv()` 等 spawn 的左半结果(此时本线程不空等,而是边等边跑别的任务)。
///
/// "spawn 一半 + 本线程排另一半" 是关键:本线程不等 spawn 的结果时
/// 它在干右半的活——相当于免费把自己时间片用满,然后 spawn 出去的
/// 左半被空闲 worker 偷走。如果改成"两半都 spawn 然后都等",本线程
/// 在 recv 之前啥都不做,白白浪费一个核(详见教程递归树手算)。
pub fn par_sort<T: Ord + Clone + Send + Sync + 'static>(
    pool: &Arc<StealingPool>,
    slice: &mut [T],
) {
    // 基线:小切片走串行,不再切分。
    if slice.len() <= PAR_SORT_CUTOFF {
        slice.sort();
        return;
    }

    // 三路 partition:[< pivot | == pivot ... | > pivot]
    // 返回两个下标 (lt_end, gt_start):
    //   slice[..lt_end]            < pivot
    //   slice[lt_end..gt_start]    == pivot
    //   slice[gt_start..]          > pivot
    let (lt_end, gt_start) = partition_three_way(slice);

    // 左半和右半各自并行排。中间等于 pivot 的段已经就位,不动。
    // 左半 spawn 出去(让空闲 worker 偷);右半本线程递归(本线程不空等)。
    let left_len = lt_end;
    let right_start = gt_start;

    // 用 Arc<StealingPool> 在闭包里再持有 pool,递归调用。
    // split_at_mut 给出 (left, right) 两个可变借用——但我们只能让 spawn 的任务
    // 持有左半、本线程持有右半。两个可变借用不能同时存在于闭包和当前栈帧,
    // 所以这里采用一个 trick:先 spawn 一个任务,它内部 par_sort 左半;
    // 由于 spawn 接 FnOnce + 'static,我们没法把带生命周期的 &mut [T] 塞进去。
    //
    // 解决方案:用 unsafe 切片指针(Send 的裸指针,我们自己保证不重叠)。
    // 这是并行算法绕不开的——Rust 的别名规则无法静态表达"两个互不重叠的可变
    // 切片分别交给两个线程"。Rayon 内部同样用 unsafe 实现这一点。
    let left_ptr = SendPtr(slice.as_mut_ptr());
    let right_ptr = SendPtr(unsafe { slice.as_mut_ptr().add(right_start) });

    // spawn 左半:在另一个线程排 slice[..left_len]。
    let pool_clone = Arc::clone(pool);
    let left_handle = pool.spawn(move || {
        // 安全:本任务和本线程各排互不重叠的 [..left_len] 和 [right_start..],
        // 互不别名。生命周期上,我们 recv() 在本函数返回前完成,所以 spawn
        // 的任务访问 slice 时借用仍然有效。
        // 先把 left_ptr 整体 bind 成局部变量再取 .0,避免 2021 edition 的
        // "分字段捕获"把裸指针 *mut T 单独抓进闭包(那样它 !Send)。
        let p = left_ptr;
        let left_slice: &mut [T] = unsafe {
            std::slice::from_raw_parts_mut(p.0, left_len)
        };
        par_sort(&pool_clone, left_slice);
    });

    // 本线程排右半:此时左半任务在队列里(被别的 worker 偷走或在 recv 时被本
    // 线程跑掉)。本线程不停下来等,而是先把右半排完。
    let right_slice: &mut [T] = unsafe {
        std::slice::from_raw_parts_mut(
            right_ptr.0,
            slice.len() - right_start,
        )
    };
    par_sort(pool, right_slice);

    // 等左半完成。如果左半还没被别的 worker 偷走、还在本 worker 的队列里,
    // recv 会把它跑掉(M9a 的"边等边干")。
    left_handle.recv();
}

/// 三路 partition。返回 `(lt_end, gt_start)`:
/// - `slice[..lt_end]`        全部 `< pivot`
/// - `slice[lt_end..gt_start]` 全部 `== pivot`
/// - `slice[gt_start..]`       全部 `> pivot`
///
/// pivot 取"首、中、末三数的中位数",避免对接近有序的输入退化成平方。
/// 三路(而不是两路)partition 对大量重复元素友好——所有等于 pivot 的元素
/// 一次性归位,下一层递归只处理严格小于和严格大于的部分。
fn partition_three_way<T: Ord + Clone>(slice: &mut [T]) -> (usize, usize) {
    let n = slice.len();
    debug_assert!(n >= 3);

    // pivot 候选:首、中、末。取中位数对应的值。
    // 把三个候选先排好序,中间位置的就是 median。
    // 这里直接取值(不交换),拿到 pivot 的克隆。
    let pivot = {
        let mid = n / 2;
        let a = &slice[0];
        let b = &slice[mid];
        let c = &slice[n - 1];
        // 取三者中位数:T: Ord + Clone
        let mut v = [a, b, c];
        // 不能直接 sort 引用(引用比较就是 T 的 Ord),取中间。
        // 但这里 v 是 [&T;3],sort 按 T 的 Ord 排,中间就是 median。
        v.sort();
        v[1].clone()
    };

    // 经典的 Bentley-McIlroy 三路划分的简化版(Nut-quickselect 风格)。
    // 用 i 扫描,维护三个区间:
    //   [..lt)      < pivot
    //   [lt..i)     == pivot
    //   [i..gt)     未扫描
    //   [gt..)      > pivot
    let mut lt = 0usize;   // 下一个 < pivot 的写入位置
    let mut gt = n;        // 下一个 > pivot 的写入位置(从右往左)
    let mut i = 0usize;
    while i < gt {
        let cmp = slice[i].cmp(&pivot);
        match cmp {
            std::cmp::Ordering::Less => {
                slice.swap(lt, i);
                lt += 1;
                i += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                gt -= 1;
                slice.swap(i, gt);
                // 不动 i:换过来的新元素还没看。
            }
        }
    }
    (lt, gt)
}

/// 并行 map:`pool` 上把 `slice` 切成若干段,每段并行调 `f`,返回 `Vec<U>`。
///
/// 切段策略:粗切——每段至少 `PAR_MAP_CUTOFF` 个元素,且总段数不超过
/// `n_workers * 4`(避免任务过碎)。每个任务负责自己那一段的 map,结果
/// 直接写进预分配的输出 Vec 的对应位置(零拷贝)。
pub fn par_map<T, U, F>(pool: &Arc<StealingPool>, slice: &[T], f: F) -> Vec<U>
where
    // 注:这里要求 T: Send(而不仅是 Sync)是因为 SendConstPtr<T> / SendPtr<T> 的
    // unsafe impl 把 *const T / *mut T 跨线程搬运的 soundness 建立在 T: Send 上。
    // 教学库保守起见用更紧的 bound,真实 rayon 用更精细的 unsafe 推理绕过这一点。
    T: Sync + Send + 'static,
    U: Send + Default + 'static,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
    let n = slice.len();
    if n == 0 {
        return Vec::new();
    }
    let n_workers = std::cmp::max(1, num_workers_hint());
    // 段数:既不能太少(喂不饱 worker),也不能太多(任务太碎)。
    let n_chunks = std::cmp::min(
        std::cmp::max(n / PAR_MAP_CUTOFF, 1),
        n_workers * 4,
    );
    let n_chunks = std::cmp::max(n_chunks, 1);
    let chunk_len = (n + n_chunks - 1) / n_chunks;

    let f_arc = Arc::new(f);
    // 预分配输出:每个槽位一个槽,任务并行填。
    let mut out: Vec<U> = (0..n).map(|_| U::default()).collect();
    // 我们用裸指针 + unsafe 把 out 的不同段交给不同任务(Rust 借用检查器
    // 无法表达"不重叠的多个可变借用分给不同线程")。
    let out_ptr = SendPtr(out.as_mut_ptr());
    let in_ptr = SendConstPtr(slice.as_ptr());

    let mut handles = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end = std::cmp::min(start + chunk_len, n);
        let len = end - start;
        let f_clone = Arc::clone(&f_arc);
        // 每个任务映射 slice[start..end] → out[start..end]。
        let h = pool.spawn(move || {
            // 先 bind wrapper 整体,避免 2021 分字段捕获把裸指针抓走。
            let ip = in_ptr;
            let op = out_ptr;
            let in_slice: &[T] =
                unsafe { std::slice::from_raw_parts(ip.0.add(start), len) };
            let out_slice: &mut [U] =
                unsafe { std::slice::from_raw_parts_mut(op.0.add(start), len) };
            for (i, x) in in_slice.iter().enumerate() {
                out_slice[i] = f_clone(x);
            }
        });
        handles.push(h);
        start = end;
    }
    // 等所有 map 任务完成。recv 在 worker 上不 park,边等边跑别的 chunk。
    for h in handles {
        // h 是 JoinHandle<()>,我们丢掉它的 () 结果。
        let _ = h.recv();
    }
    out
}

/// 并行 reduce:`slice` 切成多段并行 fold,各段部分结果再串行合并。
///
/// 三个闭包(参考 rayon 的 `fold` + `reduce` 两步设计):
/// - `init`:**每段起点**的零值。每个并行任务自己调一次 `init`,各自从零累加。
/// - `step`:**段内**累加——`(acc, &x) -> acc'`,把一个 `T` 并进 `U`。
/// - `merge`:**段间**合并——`(a, b) -> c`,把两份 `U` 拼成一份。
///
/// 为什么要分 `step` 和 `merge` 两步,而不是一个 `(U, &T) -> U`?
/// 因为分段并行后,我们手里不再有原始 `T`(已经被 fold 成 `U`),只能合并
/// `U` 与 `U`。这两个 closure 的分离是并行 reduce 的本质——
/// `step` 让"扫一段"可以并行(每段独立 fold),`merge` 是串行的"装配"。
///
/// 算法:`n_chunks` 段并行 step → 收集 `Vec<U>` → 主线程串行 merge。
/// 主线程合并是 O(n_chunks) 次串行 merge,通常 `n_chunks` 远小于 `n`,
/// 所以这步开销可忽略。教程里会手算这一节。
pub fn par_reduce<T, U, I, S, M>(
    pool: &Arc<StealingPool>,
    slice: &[T],
    init: I,
    step: S,
    merge: M,
) -> U
where
    // 同 par_map:SendConstPtr<T> 跨线程需要 T: Send 才 sound。
    T: Sync + Send + 'static,
    U: Send + 'static,
    I: Fn() -> U + Send + Sync + 'static,
    S: Fn(U, &T) -> U + Send + Sync + 'static,
    M: Fn(U, U) -> U + Send + Sync + 'static,
{
    let n = slice.len();
    if n == 0 {
        return init();
    }
    let n_workers = std::cmp::max(1, num_workers_hint());
    let n_chunks = std::cmp::min(
        std::cmp::max(n / PAR_MAP_CUTOFF, 1),
        n_workers * 4,
    );
    let n_chunks = std::cmp::max(n_chunks, 1);
    let chunk_len = (n + n_chunks - 1) / n_chunks;

    let init_arc = Arc::new(init);
    let step_arc = Arc::new(step);

    let mut handles = Vec::new();
    let mut start = 0usize;
    let in_ptr = SendConstPtr(slice.as_ptr());
    while start < n {
        let end = std::cmp::min(start + chunk_len, n);
        let len = end - start;
        let init_clone = Arc::clone(&init_arc);
        let step_clone = Arc::clone(&step_arc);
        let in_ptr = in_ptr;
        let h = pool.spawn(move || {
            // 安全:本任务访问 slice[start..end],与其它任务/主线程不重叠。
            // 先 bind wrapper,避免 2021 分字段捕获。
            let ip = in_ptr;
            let in_slice: &[T] =
                unsafe { std::slice::from_raw_parts(ip.0.add(start), len) };
            let mut acc = init_clone();
            for x in in_slice {
                acc = step_clone(acc, x);
            }
            acc
        });
        handles.push(h);
        start = end;
    }

    // 主线程串行合并各段部分结果。handles 按段顺序排列,合并顺序确定。
    let merge_arc = Arc::new(merge);
    let mut acc = init_arc();
    for h in handles {
        let part = h.recv();
        acc = (merge_arc)(acc, part);
    }
    acc
}

/// 给 par_map / par_reduce 一个粗略的"应该开几个任务"的提示。
/// 在测试环境下取 4,真实环境取线程数。这里我们查 StealingPool 没有暴露
/// n_workers(怕泄露内部细节),所以用一个保守的默认 4。
fn num_workers_hint() -> usize {
    // 不查真实池大小(避免给 StealingPool 加 API),用一个保守的常数。
    // 教程里会解释:任务数 > 核数 才有偷窃机会;任务数远大于核数才能均衡。
    4
}

// =====================================================================
// par_iter —— rayon 风格的惰性并行迭代器
// =====================================================================

/// 一个惰性并行迭代器。`.map()` / `.filter()` 返回新的 `ParIter`,
/// 终端方法(`sum` / `for_each` / `reduce`)才真正触发并行遍历。
///
/// 实现简化:我们把整个迭代器表示成
///   `Arc<dyn Fn(usize, usize) -> ... + Send + Sync>` 的闭包链——
/// 每一个 adapter 包一层闭包。最后 sink 把 [0..n) 切段并行,每段调
/// 最外层闭包。
///
/// 为了不让生命周期和泛型参数爆炸,这里 ParIter 写死 element 类型为 i64
/// (教程里讲清楚 rayon 的全泛型实现需要 GAT / 关联类型,我们简化)。
/// 读者把 i64 当作"任意可并行归约的数值"的占位。
pub struct ParIter {
    /// 元素总数。
    n: usize,
    /// 段的并行 map 函数:给 (start, end),返回这段经过 map/filter 后的
    /// 中间结果 Vec<i64>(已经是 sink 之前最后一步的值)。
    /// 之所以返回 Vec 而不是单个聚合,是为了让 sink 阶段再做最后 reduce。
    chunk_fn: Arc<dyn Fn(usize, usize) -> Vec<i64> + Send + Sync>,
}

impl ParIter {
    /// 从一个 i64 切片构造并行迭代器。
    pub fn from_slice(slice: &[i64]) -> Self {
        let n = slice.len();
        let buf: Vec<i64> = slice.to_vec();
        let buf = Arc::new(buf);
        let buf_clone = Arc::clone(&buf);
        let chunk_fn = Arc::new(move |start: usize, end: usize| {
            buf_clone[start..end].to_vec()
        });
        Self { n, chunk_fn }
    }

    /// `.map(f)`:返回新的 ParIter,内部的 chunk_fn 包一层 map。
    pub fn map<F>(self, f: F) -> Self
    where
        F: Fn(i64) -> i64 + Send + Sync + 'static,
    {
        let prev = self.chunk_fn;
        let f = Arc::new(f);
        let chunk_fn = Arc::new(move |start: usize, end: usize| {
            let mut v = prev(start, end);
            for x in v.iter_mut() {
                *x = f(*x);
            }
            v
        });
        Self { n: self.n, chunk_fn }
    }

    /// `.filter(pred)`:返回新的 ParIter。
    pub fn filter<F>(self, pred: F) -> Self
    where
        F: Fn(i64) -> bool + Send + Sync + 'static,
    {
        let prev = self.chunk_fn;
        let pred = Arc::new(pred);
        let chunk_fn = Arc::new(move |start: usize, end: usize| {
            let v = prev(start, end);
            v.into_iter().filter(|x| pred(*x)).collect()
        });
        Self { n: self.n, chunk_fn }
    }

    /// 终端:sum。切段并行跑 chunk_fn,各段求和,最后串行加总。
    pub fn sum(self, pool: &Arc<StealingPool>) -> i64 {
        let n = self.n;
        if n == 0 {
            return 0;
        }
        let n_workers = std::cmp::max(1, num_workers_hint());
        let n_chunks = std::cmp::min(
            std::cmp::max(n / PAR_MAP_CUTOFF, 1),
            n_workers * 4,
        );
        let n_chunks = std::cmp::max(n_chunks, 1);
        let chunk_len = (n + n_chunks - 1) / n_chunks;

        let chunk_fn = self.chunk_fn;
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < n {
            let end = std::cmp::min(start + chunk_len, n);
            let cf = Arc::clone(&chunk_fn);
            let h = pool.spawn(move || {
                let v = cf(start, end);
                v.into_iter().sum::<i64>()
            });
            handles.push(h);
            start = end;
        }
        let mut total = 0i64;
        for h in handles {
            total += h.recv();
        }
        total
    }

    /// 终端:for_each。不返回值,顺序未指定(各段并行,段内顺序保留)。
    pub fn for_each<F>(self, pool: &Arc<StealingPool>, f: F)
    where
        F: Fn(i64) + Send + Sync + 'static,
    {
        let n = self.n;
        if n == 0 {
            return;
        }
        let chunk_fn = self.chunk_fn;
        let f = Arc::new(f);
        let n_workers = std::cmp::max(1, num_workers_hint());
        let n_chunks = std::cmp::min(
            std::cmp::max(n / PAR_MAP_CUTOFF, 1),
            n_workers * 4,
        );
        let n_chunks = std::cmp::max(n_chunks, 1);
        let chunk_len = (n + n_chunks - 1) / n_chunks;

        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < n {
            let end = std::cmp::min(start + chunk_len, n);
            let cf = Arc::clone(&chunk_fn);
            let f_clone = Arc::clone(&f);
            let h = pool.spawn(move || {
                let v = cf(start, end);
                for x in v {
                    f_clone(x);
                }
            });
            handles.push(h);
            start = end;
        }
        for h in handles {
            let _ = h.recv();
        }
    }
}
