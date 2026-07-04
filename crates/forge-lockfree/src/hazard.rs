//! # Hazard Pointer —— 让"回收者"看得见"读者正在用哪些指针"
//!
//! (Maged Michael, 2004:《Hazard Pointers: A Safe Memory Reclamation Technique
//! for Lock-Free Memory》)
//!
//! ## 敌人：无锁结构里"指针刚被释放，别人正在读"
//!
//! 回顾 stack.rs：pop 出一个节点后**不能立刻释放**——别的线程可能还读着它。
//! 教学版选择了"永不释放"（泄漏）规避 ABA。hazard pointer 是工业级答案之一：
//! 让"读者"在 thread-local 里公告"我在用哪些指针"，让"回收者"扫描公告栏后再回收。
//!
//! ## 形态：全局公告栏 + thread-local 槽
//!
//! 一个固定大小的全局数组 `HAZARDS[N_THREADS]`，每个槽一个 `AtomicPtr<()>`。
//! 每个线程在第一次使用时认领一个槽（用 `AtomicUsize` 计数器分配），终生持有它。
//!
//! 读者流程：
//! 1. 把"我正在用"的指针写进自己的 hazard 槽（store Release）。
//! 2. SeqCst fence——与回收者的扫描互锁。
//! 3. 重读 head，确认 hazard 槽里的指针**仍是当前 head**（避免 store 之前就被换走）。
//! 4. 安全使用。
//! 5. 用完把 hazard 槽清空。
//!
//! 回收者流程：
//! 1. 把要回收的指针塞进**线程局部垃圾袋**（thread-local `Vec<*mut ()>`）。
//! 2. 垃圾袋攒到阈值时，扫描**所有**线程的 hazard 槽，构建"危险集"。
//! 3. 垃圾袋里凡是不在危险集里的指针 → 安全回收；其余 → 留到下次。
//!
//! ## 核心保证
//!
//! hazard 槽一旦 Release-store 了指针 P，回收者的 SeqCst fence 配 readers 的 SeqCst
//! fence 共同保证：**回收者扫到 P 的瞬间，读者要么已经放手（清空了 hazard 槽），
//! 要么 P 还在 hazard 槽里——回收者看见它，就不回收它**。不可能出现"读者写了 P，
//! 回收者没看见，把 P 回收了"——因为 SeqCst fence 强制 hazard-store 与 hazard-scan
//! 之间有全序。
//!
//! ## 代价与取舍
//!
//! 读路径多两条原子写（store + clear hazard 槽）+ 一条 SeqCst fence。延迟极低——
//! 适合"读极多写极少、读延迟敏感"。回收延迟（要攒垃圾才扫）。
//!
//! ## 教学简化
//!
//! 真实实现（crossbeam 的 hazard / Microsoft 实现里）有：每线程多 hazard 槽（动态
//! 分配）、批量 retire 摊薄扫描、跨线程 retire 协调……我们这版每线程固定 1 个槽，
//! 足够覆盖 Treiber 栈（每线程同时只读 1 个指针）。详见模块末尾"扩展阅读"。

use std::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};

/// 上限：最多 128 个线程持有 hazard 槽。超过会 panic（教学版简化）。
pub const MAX_HAZARD_THREADS: usize = 128;

/// 全局 hazard 公告栏：每线程一个槽。
struct HazardRegistry {
    slots: [AtomicPtr<()>; MAX_HAZARD_THREADS],
    /// 下一个待认领的槽下标。
    next_slot: AtomicUsize,
}

impl HazardRegistry {
    const fn new() -> Self {
        // AtomicPtr::new 在 const 上下文里要求 const fn —— 1.24+ 已支持。
        // 用 `[const_expr; N]` 数组语法（每个元素单独 const 求值，非 Drop 复制）。
        const NULL: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
        Self {
            slots: [NULL; MAX_HAZARD_THREADS],
            next_slot: AtomicUsize::new(0),
        }
    }
}

/// 全局唯一 registry。OnceLock 之外的简单替代：用 `static` 直接 const 初始化。
static REGISTRY: HazardRegistry = HazardRegistry::new();

thread_local! {
    /// 本线程认领的 hazard 槽下标。第一次用 hazard 时认领。
    static MY_SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    /// 本线程的垃圾袋。retire 的指针先放这里，攒到阈值再扫描回收。
    static GARBAGE: std::cell::RefCell<Vec<(*mut (), fn(*mut ()))>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 触发批量回收的阈值。每攒到这个数量就扫一次公告栏。
const SCAN_THRESHOLD: usize = 32;

/// 一个被 hazard 保护的指针的句柄。Drop 时清空 hazard 槽。
///
/// 用法：
/// ```ignore
/// let h = HazardGuard::protect(ptr);   // 公告"我在用 ptr"
/// // ... 安全访问 *ptr ...
/// drop(h);                              // 放手
/// ```
pub struct HazardGuard {
    slot: usize,
}

impl HazardGuard {
    /// 公告"我正在用 ptr"，并把它包成 HazardGuard。Drop 时清空 hazard 槽。
    ///
    /// 注意：调用方**仍要负责**自己重读数据结构的 head 确认 ptr 仍然有效（典型的
    /// "store hazard → fence → re-check" 三步）。本函数只负责前两步。
    pub fn protect(ptr: *mut ()) -> Self {
        let slot = ensure_slot();
        // SAFETY: slot < MAX_HAZARD_THREADS（ensure_slot 保证）。
        REGISTRY.slots[slot].store(ptr, Ordering::Release);
        // SeqCst fence：与回收者扫描端的 SeqCst fence 互锁。
        // 这条 fence 保证：回收者扫到我的 hazard 槽时，我刚才的 store 一定已可见。
        fence(Ordering::SeqCst);
        HazardGuard { slot }
    }

    /// 重设当前 guard 保护的指针（用于 CAS 循环里换指针时刷新公告）。
    pub fn set(&mut self, ptr: *mut ()) {
        REGISTRY.slots[self.slot].store(ptr, Ordering::Release);
        fence(Ordering::SeqCst);
    }

    /// 立即放手（清空 hazard 槽）。手动调或由 Drop 自动调。
    pub fn release(self) {
        REGISTRY.slots[self.slot].store(std::ptr::null_mut(), Ordering::Release);
        // 不需要 fence——回收者扫到 null 就跳过。
    }
}

impl Drop for HazardGuard {
    fn drop(&mut self) {
        REGISTRY.slots[self.slot].store(std::ptr::null_mut(), Ordering::Release);
    }
}

/// 认领一个 hazard 槽（thread-local 缓存下标，终生复用）。
fn ensure_slot() -> usize {
    MY_SLOT.with(|s| {
        let v = s.get();
        if v != usize::MAX {
            return v;
        }
        // 第一次：从全局 next_slot 抢一个下标。
        let v = REGISTRY.next_slot.fetch_add(1, Ordering::Relaxed);
        if v >= MAX_HAZARD_THREADS {
            panic!("hazard: 超过 {} 线程上限", MAX_HAZARD_THREADS);
        }
        s.set(v);
        v
    })
}

/// 把指针退役——放进本线程垃圾袋，攒够阈值时扫描全局 hazard 公告栏，
/// 只回收**没被任何线程 hazard 的指针**。
///
/// `destructor` 是如何回收该指针（通常是 `|p| drop(Box::from_raw(p as *mut T))`）。
///
/// SAFETY: 调用方必须保证 `ptr` 此后**不再被新线程获取**——即它已从数据结构里
/// 摘下来、不会再被 publish 出去。否则别的线程可能 hazard 一个已被回收的指针。
pub unsafe fn retire(ptr: *mut (), destructor: fn(*mut ())) {
    let trigger_scan = GARBAGE.with(|g| {
        g.borrow_mut().push((ptr, destructor));
        g.borrow().len() >= SCAN_THRESHOLD
    });
    if trigger_scan {
        scan_and_reclaim();
    }
}

/// 扫描所有线程的 hazard 槽，构建"危险集"，回收垃圾袋里不在危险集的指针。
pub fn scan_and_reclaim() {
    // 1. 收集所有非空 hazard 指针。
    let max = REGISTRY.next_slot.load(Ordering::Acquire);
    let mut hazards: Vec<*mut ()> = Vec::with_capacity(max);
    for i in 0..max {
        let p = REGISTRY.slots[i].load(Ordering::Acquire);
        if !p.is_null() {
            hazards.push(p);
        }
    }
    hazards.sort_unstable_by(|a, b| a.cmp(b));
    hazards.dedup();

    // 2. SeqCst fence：与读者端的 SeqCst fence 互锁，确保我们看到了所有
    //    当前活跃的 hazard 公告。这条 fence 是 hazard pointer 正确性的脊柱。
    fence(Ordering::SeqCst);

    // 3. 对每个垃圾袋里的指针：不在危险集 → 回收；否则留到下次。
    GARBAGE.with(|g| {
        let mut bag = g.borrow_mut();
        let mut keep = Vec::with_capacity(bag.len());
        for (ptr, dtor) in bag.drain(..) {
            if hazards.binary_search(&ptr).is_ok() {
                // 还有人 hazard，留到下次扫。
                keep.push((ptr, dtor));
            } else {
                // 没有任何 hazard 槽引用 ptr，且它已被 retire（从数据结构
                // 摘下、不再被 publish）。安全调用 destructor 回收。
                dtor(ptr);
            }
        }
        *bag = keep;
    });
}

/// 显式回收本线程垃圾袋里所有未回收指针（线程退出前调一下，避免泄漏）。
pub fn flush_local() {
    scan_and_reclaim();
    // 还残留的就是"还有线程 hazard 着"——只能等下次。这里我们不强行回收。
}

// ---------------------------------------------------------------------------
// 用 hazard pointer 包一层 Treiber 栈，对照 stack.rs 的"故意泄漏"版
// ---------------------------------------------------------------------------

/// 一个用 hazard pointer 回收节点的 Treiber 栈。ABA 安全、内存不泄漏。
///
/// 与 `crate::stack::Stack` 形态完全一致（push/pop/head），但 pop 出来的旧 head
/// 不再"故意泄漏"——改用 hazard pointer 的 retire 路径安全回收。
pub struct HazardStack<T> {
    head: AtomicPtr<Node<T>>,
}

struct Node<T> {
    value: std::mem::ManuallyDrop<T>,
    next: *mut Node<T>,
}

unsafe impl<T: Send> Send for HazardStack<T> {}
unsafe impl<T: Send> Sync for HazardStack<T> {}

impl<T> HazardStack<T> {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn push(&self, value: T) {
        let node = Box::into_raw(Box::new(Node {
            value: std::mem::ManuallyDrop::new(value),
            next: std::ptr::null_mut(),
        }));
        let mut old = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: node 是我们刚 Box::into_raw 出来的合法节点。
            unsafe { (*node).next = old };
            match self
                .head
                .compare_exchange_weak(old, node, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                // CAS 失败：actual 是当前真实 head，下一轮用它。
                Err(actual) => old = actual,
            }
        }
    }

    pub fn pop(&self) -> Option<T> {
        let mut guard = HazardGuard::protect(std::ptr::null_mut());
        let mut old = self.head.load(Ordering::Acquire);
        loop {
            if old.is_null() {
                return None;
            }
            // 1. hazard 公告"我在用 old"。
            guard.set(old as *mut ());
            // 2. 重读 head 确认 old 仍然有效（与回收者扫描互锁）。
            if self.head.load(Ordering::Acquire) != old {
                // 期间 head 已被换走，重读重试。
                old = self.head.load(Ordering::Acquire);
                continue;
            }
            // 3. 读 next、CAS head → next。
            // SAFETY: old 已 hazard，回收者不会动它；(*old).next 是合法字段。
            let next = unsafe { (*old).next };
            if self
                .head
                .compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // CAS 成功：old 已被弹出。取出 value。
                // SAFETY: CAS 成功意味着 old 已独占，value 归我。
                let value = unsafe { std::mem::ManuallyDrop::take(&mut (*old).value) };
                // 退役 old：放进垃圾袋，等扫描后回收。从此 old 不再被 publish。
                // SAFETY: old 已从栈摘下，不再被新线程读到。
                unsafe { retire(old as *mut (), destroy_node::<T>) };
                drop(guard);
                return Some(value);
            }
            // CAS 失败：head 已变，重读重试。
            old = self.head.load(Ordering::Acquire);
        }
    }
}

impl<T> Default for HazardStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for HazardStack<T> {
    fn drop(&mut self) {
        // 单线程 Drop：把栈里残留的节点全部回收。
        let mut cur = *self.head.get_mut();
        while !cur.is_null() {
            // SAFETY: 节点都是 Box::into_raw 出来的；先把 next 读出来再 drop 节点，
            // 否则 cur 已悬垂、读 (*cur).next 是 use-after-free。
            unsafe {
                let next = (*cur).next;
                let mut node = Box::from_raw(cur);
                // value 一定还在（没被 pop take 走，否则该节点就出栈了）。
                // ManuallyDrop::take 取出来 drop 掉，然后 Box 自然释放节点内存。
                let _ = std::mem::ManuallyDrop::take(&mut (*node).value);
                drop(node);
                cur = next;
            }
        }
    }
}

/// Node 的回收函数（每线程 retire 时注册）。安全包装：内部做 unsafe。
fn destroy_node<T>(p: *mut ()) {
    // SAFETY: p 是 Box::into_raw(Node<T>) 出来的。value 可能已被 take（这种情况下
    // 我们不该再 take），但 pop 路径调 retire 时 value 已经被 take 走——ManuallyDrop
    // 处于"已被 take"状态，不能再 take。我们直接 Box::from_raw 释放节点内存，
    // 不动 value（drop Box 不调 ManuallyDrop::drop，所以不会 double-free value）。
    let _ = unsafe { Box::from_raw(p as *mut Node<T>) };
}
