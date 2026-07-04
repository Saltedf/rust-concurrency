//! # 模块 M1：原子与内存序
//!
//! 这一章解决一个贯穿整个 Forge 的根本问题：**多个线程同时碰同一个变量，
//! 怎样才不会把数据写坏、又不需要每次都上锁？**
//!
//! 答案是**原子操作**——一条要么完整发生、要么完全没发生的指令；
//! 以及**内存序**——告诉编译器和 CPU "这次操作前后，哪些读写不能被调换顺序"。
//!
//! 本模块按教程 `docs/modules/M1-atomics-and-ordering.md` 的 10 个小步，
//! 逐个给出**正确**的实现。每一步的"为什么会出错、怎么修"都在教程里讲透，
//! 这里只放最终能通过测试、能通过 miri 的版本。
//!
//! > 一句话锚点：原子是"两个线程能同时读写、却绝不会看到半截值"的整数/布尔/指针；
//! > 内存序是"配合同一条原子操作、额外强加的排序契约"。二者组合，构成所有并发原语的砖块。

use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::Arc;

// ──────────────────────────────────────────────────────────────────────────
// M1.1  停止位（StopFlag）
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】一个后台线程需要被另一个线程"礼貌地叫停"。最朴素的办法是
// 共享一个 `&mut bool`，但两个线程同时一读一写同一个 `bool` 是**数据竞争**
// （未定义行为，可能撕裂、可能被优化掉）。
//
// 【武器】`AtomicBool`：读和写都是原子的，绝不会看到半截值。
//
// 【为什么这里用 Relaxed 就够】这个标志位本身**不承载任何需要同步的其它数据**——
// 我们只是想知道"该停了吗"。没有"先写数据、再立标志"的发布关系，所以不需要
// Acquire/Release。Relaxed 只保证"这次读/写是原子的"，对顺序不做任何承诺，这恰恰是
// 一个纯标志位所需要的全部。**（这正是 M1.6 要打破的直觉——一旦你要通过标志发布数据，
// Relaxed 就不够了。）**

/// 一个可被任意线程翻转的"停止位"。
///
/// 典型用法：后台线程在循环里 `while !flag.is_stopped() { work(); }`，
/// 主线程在需要时调用 `flag.stop()`。
pub struct StopFlag {
    flag: AtomicBool,
}

impl StopFlag {
    /// 创建一个未停止的标志。
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    /// 请求停止。
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// 是否已被请求停止。
    pub fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for StopFlag {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────────
// M1.2 / M1.3 / M1.4  计数器与 ID 分配
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】`counter += 1` 不是一条机器指令——它是"读-改-写"三步。多个线程并发执行
// 时会互相覆盖（丢失更新）。教程 M1.3 会先写一个**故意错误**的版本让你亲眼看见
// 计数丢失；这里给出正确的 `fetch_add` 版本。
//
// 【同构】`fetch_add` / `fetch_or` / `fetch_max` …… 这些 `fetch_*` 操作形状完全
// 一样：原子地"读旧值、算新值、写回、返回旧值"。唯一不同的是中间那个二元运算。

/// 一个可被多线程并发自增的计数器。
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    pub const fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }

    /// 原子自增 1。
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// 原子加上 `n`。
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// 读取当前值。
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// 单调递增的 ID 分配器。
pub struct IdAllocator {
    next: AtomicUsize,
}

impl IdAllocator {
    pub const fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
        }
    }

    /// 【M1.4 v1】`fetch_add` 分配：简单、快，但会在溢出时**回绕**（从 0 重新开始）。
    /// 如果你不介意回绕（比如 ID 只用于临时标识），这就够了。
    pub fn next_id(&self) -> usize {
        // fetch_add 返回的是**旧值**，所以第一个分配出去的是 0。
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// 【M1.4 v2】带上限的分配：用 `fetch_update`（内部是 CAS 循环）在自增前检查上限，
    /// 超过 `max` 就返回 `None` 而不是回绕。
    ///
    /// `fetch_update(set_order, fetch_order, f)`：反复"读旧值→交给 `f` 算新值→CAS 写回"，
    /// 直到 CAS 成功或 `f` 返回 `None`（表示放弃）。
    pub fn next_id_capped(&self, max: usize) -> Option<usize> {
        self.next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < max).then(|| n + 1)
            })
            .ok()
    }
}

// ──────────────────────────────────────────────────────────────────────────
// M1.5  一次性执行（OnceFlag）—— CAS 循环
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】一段初始化代码在多线程下必须**恰好执行一次**，且不能阻塞太久。
//
// 【武器】compare-and-exchange（CAS）：原子地"如果值还是 false，就把它改成 true"。
// 因为是原子的，全局只有一个线程能赢，其余线程的 CAS 必然失败。于是恰好一个线程
// 真正执行初始化。
//
// 【关于内存序的重要注意】这里用 Relaxed 能保证**恰好一次**（CAS 在 `done` 上的
// 全局修改顺序决定了唯一的赢家），但**不保证**初始化的副作用被其它线程看见。
// 如果你要"初始化产出的数据"被其它线程安全读取，那就需要 Acquire/Release——
// 这正是 M1.6/M1.7 的主题（见 [`LazyBox`]）。所以请把 [`OnceFlag`] 理解为
// "恰好执行一次"的保证，而不是"安全发布数据"的保证。

/// 保证闭包在所有线程中恰好执行一次。
pub struct OnceFlag {
    done: AtomicBool,
}

impl OnceFlag {
    pub const fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
        }
    }

    /// 若这是第一次调用，则执行 `f` 并返回 `true`；否则什么都不做，返回 `false`。
    pub fn call_once<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        // 快速路径：大概率已经完成，先用便宜的 load 探一下。
        if self.done.load(Ordering::Relaxed) {
            return None;
        }
        // 抢"第一名"。CAS 的全局修改顺序保证只有一个线程的 exchange 成功。
        match self.done.compare_exchange(
            false,
            true,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Some(f()),
            Err(_) => None,
        }
    }

    /// 是否已经执行过。
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// M1.6 / M1.7  延迟初始化（带指针间接）—— 内存序登场
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】现在初始化的产物是一个**堆上的对象**，我们要把它的指针发布给其它线程：
//
//     ptr.store(Box::into_raw(Box::new(make_data())), …);   // 生产者
//     let p = ptr.load(…);                                   // 消费者
//
// 如果用 Relaxed，编译器和 CPU 可以把"构造对象的写"和"存指针"重排——于是消费者
// 可能拿到一个**指针非空、但所指对象尚未构造完**的半成品。这就是数据竞争级别的灾难。
//
// 【武器】Release / Acquire 配对：
//   - 生产者 `store(ptr, Release)`：保证"在 Release 之前的所有写（即对象的构造）
//     都在该 store **之前**完成并可见"。
//   - 消费者 `load(ptr, Acquire)`：保证"在 Acquire 之后的所有读看到的，都是 Release
//     之前完成的写"。
//   合起来：只要消费者 load 到了那个指针，它就一定能看到完整的对象。这叫
//   **happens-before（先于）关系**——Release 和 Acquire 在两个线程间拉起了一根
//   "因果" 的线。
//
// 【低频模型】Release = 把稿子**发布**出去（之前的一切工作随之公开）；
//             Acquire = **订阅**并读到（之后读到的一切都来自那次发布）。
//             就像 git 的 push / pull：你 pull 到了某次提交，就一定能看到它之前的全部历史。

/// 延迟、线程安全地构造一个 `Box<T>`，并用 `Release/Acquire` 安全发布。
///
/// 第一个调用 `get` 的线程真正构造对象；后续调用直接复用。竞争构造的失败方
/// 会回收自己那份，使用赢家那份——绝不泄漏、绝不悬空。
pub struct LazyBox<T> {
    ptr: AtomicPtr<T>,
}

// 安全性论证：`LazyBox` 内部指针要么为 null（还没构造），要么指向一个**已完整构造**
// 的 `T`（由 Release/Acquire 保证可见性）。`get` 只返回 `&T`（共享引用），所以只要
// `T: Send + Sync`，跨线程共享引用就是安全的。
unsafe impl<T: Send + Sync> Sync for LazyBox<T> {}
unsafe impl<T: Send> Send for LazyBox<T> {}

impl<T> LazyBox<T> {
    pub const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// 返回已初始化对象的共享引用；尚未初始化时用 `init` 构造一次。
    pub fn get(&self, init: impl FnOnce() -> T) -> &T {
        // Acquire：如果读到非空指针，就与生产者的 Release 建立 happens-before，
        // 从而看到完整构造好的对象。
        let mut p = self.ptr.load(Ordering::Acquire);

        if p.is_null() {
            // 构造我们自己的副本。
            p = Box::into_raw(Box::new(init()));
            // Release：把我们构造对象的写"发布"出去。成功顺序 Acquire，失败顺序 Acquire
            //（失败时我们也要读赢家写入的指针，同样需要看到完整对象）。
            match self.ptr.compare_exchange(
                std::ptr::null_mut(),
                p,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // 我们赢了，用我们自己的指针。
                }
                Err(winner) => {
                    // 别人赢了。回收我们这份，改用赢家那份。
                    // 安全：p 来自我们自己的 Box::into_raw，独占所有权。
                    unsafe {
                        drop(Box::from_raw(p));
                    }
                    p = winner;
                }
            }
        }

        // 安全：到这里 p 非空，且（通过 Acquire）能看到完整构造的 T。
        unsafe { &*p }
    }
}

impl<T> Drop for LazyBox<T> {
    fn drop(&mut self) {
        // drop 时独占（&mut self），用 get_mut 直接拿裸指针，无需原子操作。
        let p = *self.ptr.get_mut();
        if !p.is_null() {
            // 安全：p 来自 Box::into_raw，转移所有权回来后 drop。
            unsafe {
                drop(Box::from_raw(p));
            }
        }
    }
}

impl<T> Default for LazyBox<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────────
// M1.8  compare_exchange vs compare_exchange_weak
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】在 ARM 这类"弱内存"架构上，CAS 的底层指令是 load-linked / store-conditional
// （LL/SC）：它可能**莫名其妙地失败**，哪怕值其实没变（因为缓存行被无关写触碰过）。
// 所以标准库提供两个版本：
//   - `compare_exchange`（强）：失败**只**因为值真的不匹配。
//   - `compare_exchange_weak`（弱）：还可能**假失败**（spurious failure）。
//
// 经验法则：**CAS 循环里用 `_weak`**（假失败直接重试即可，省一条循环判断指令）；
// 只重试一次或不重试的场景才用强版本。

/// 用 `compare_exchange_weak` 的 CAS 循环实现原子加法，返回**新值**。
///
/// 这是"`fetch_*` 能表达的一切，都能用 CAS 循环表达"的范例——
/// CAS 循环是所有原子读-改-写的通用底座。
pub fn cas_add(target: &AtomicU64, n: u64) -> u64 {
    let mut current = target.load(Ordering::Relaxed);
    loop {
        let new = current.wrapping_add(n);
        match target.compare_exchange_weak(
            current,
            new,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            // CAS 成功：`current` 就是旧值，新值是我们算出来的 `new`。
            Ok(_old) => return new,
            // 假失败或真失败：都把"当前真实值"拿回来，重算重试。
            Err(actual) => current = actual,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// M1.9  SeqCst 存-载（Dekker 风格）—— 为什么需要最强内存序
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】考虑这个"两个线程都想抢进临界区"的经典场景（Dekker 算法核心）：
//
//     线程 A:  store(A=1);  seenA = load(B);
//     线程 B:  store(B=1);  seenB = load(A);
//
// 直觉上，"两个线程不可能都看到对方是 0"——因为必有一个 store 先发生。
// 但在 Acquire/Release 下，这是**允许**的！因为 store(A) 和 load(B) 之间
// 没有任何同步关系，CPU 可以把它们重排。只有 **SeqCst（顺序一致）** 强制
// 所有 SeqCst 操作排成一个全局总顺序，才能保证"至少一个线程看到对方的 1"。
//
// 这正是 SeqCst 存在的理由——它是"我懒得分别推理每对操作、要一个全局一致视图"
// 时的逃生舱。代价是：在 x86 上 SeqCst 的 store 需要一条 `xchg`（比普通 store 贵）。

/// 一次 Dekker 风格的"存-载"实验：两线程各自存 1 再读对方。
///
/// 返回 `(线程A看到的B, 线程B看到的A)`。在 `Relaxed`/`AcqRel` 下，**两个值都可能是 0**
/// （这是被允许的）；在 `SeqCst` 下，**至少有一个是 1**。
pub fn dekker_store_load(ord: Ordering) -> (bool, bool) {
    let a = Arc::new(AtomicBool::new(false));
    let b = Arc::new(AtomicBool::new(false));

    let a2 = a.clone();
    let b2 = b.clone();

    let h = std::thread::spawn(move || {
        a2.store(true, ord);
        b2.load(ord)
    });

    b.store(true, ord);
    let seen_b_by_a = a.load(ord);

    let seen_a_by_b = h.join().unwrap();
    (seen_b_by_a, seen_a_by_b)
}

// ──────────────────────────────────────────────────────────────────────────
// M1.10  False sharing（伪共享）与缓存行对齐
// ──────────────────────────────────────────────────────────────────────────
//
// 【敌人】两个**逻辑上无关**的原子变量，如果恰好落在 CPU 的同一条缓存行（通常 64 字节），
// 那么一个核改自己的变量，会让另一个核的整条缓存行失效——哪怕另一个核根本不读这个变量。
// 结果：两个本该并行的线程被缓存一致性协议（MESI）逼着串行化，性能暴跌。这叫**伪共享**。
//
// 【武器】把每个"热点"变量用 `#[repr(align(64))]` 单独放到一条缓存行上，彼此互不打扰。
// 基准测试（见 benches/m1_false_sharing.rs）能直接量出 3–5 倍差距。

/// 把内层值强制对齐到一条缓存行（64 字节），用于消除伪共享。
#[repr(align(64))]
pub struct CacheLine<T>(pub T);

impl<T> CacheLine<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

/// 两个**未对齐**、紧挨在一起的计数器——会落入同一条缓存行（伪共享重灾区）。
pub struct AdjacentCounters {
    pub a: AtomicU64,
    pub b: AtomicU64,
}

/// 两个**各自对齐**到独立缓存行的计数器——消除伪共享。
pub struct PaddedCounters {
    pub a: CacheLine<AtomicU64>,
    pub b: CacheLine<AtomicU64>,
}
