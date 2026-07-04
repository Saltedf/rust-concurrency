//! # Epoch-Based Reclamation —— "两 epoch 窗口"批量回收
//!
//! (Maged Michael, 2004:《Safe Memory Reclamation for Dynamic Lock-Free Objects
//! Using Hazard Eras》; crossbeam-epoch 的工程化原型。)
//!
//! ## 敌人：与 hazard pointer 同一个，回收策略不同
//!
//! 无锁结构里"指针刚被释放，别人正在读"。hazard pointer 用"公告栏"让回收者
//! 看见每个读者在读什么；epoch 用**"代次窗口"**做批量——**不需要精确知道每个人
//! 读什么，只要分批：所有 epoch N 之前进入临界区的读者，到 epoch N+2 时一定已退出**。
//!
//! ## 形态：全局 epoch + 每线程局部 epoch + 三袋垃圾
//!
//! - `global_epoch: AtomicUsize` —— 当前全局 epoch（单调递增）。
//! - 每线程一个 `local_epoch: AtomicUsize` —— 我当前 pin 在哪个 epoch；0 = 未参与。
//! - 每线程一个**三袋垃圾**：`garbage[0..3]`——垃圾按"出生 epoch mod 3"分袋。
//!
//! ## 工作流
//!
//! 读者：
//! 1. `pin()`：把 local_epoch 设成 global_epoch（"我现在处在 epoch E"）。
//! 2. 安全访问数据结构。
//! 3. `unpin()`：把 local_epoch 设回 0（"我退出了"）。
//!
//! 写者（retire 旧指针）：
//! 1. 把指针塞进 garbage[global_epoch % 3]。
//! 2. 偶尔调 `try_advance()`：
//!    - 若所有活跃线程的 local_epoch 都已 ≥ 当前 global_epoch → 推进 epoch +1。
//!    - 推进时：旧 epoch（= global_epoch - 1）的垃圾袋可整体回收——
//!      因为两 epoch 之前 pin 的读者必然已 unpin。
//!
//! ## 核心不变量：两 epoch 窗口
//!
//! 为什么"epoch N+2 时 epoch N 的垃圾可回收"？
//!
//! 设某读者在 epoch N 进临界区（local_epoch = N）。回收者要推进 epoch：
//! 它要求**所有活跃线程的 local_epoch ≥ 当前 global_epoch**。所以这个读者要么已 unpin，
//! 要么 local_epoch 还是 N——后者会阻塞推进，直到读者 unpin。
//!
//! 于是 epoch 一旦推进到 N+1，说明所有当时的读者都已退出或更新到 ≥ N+1；
//! 推进到 N+2 时，epoch N 的读者必然已 unpin——garbage[N%3] 可全回收。
//!
//! 三袋垃圾的"两袋旧 + 一袋新"循环：每推进一次 epoch，回收最老那袋。
//!
//! ## 与 hazard pointer 的对比
//!
//! - hazard pointer：读路径多 2 条原子写 + SeqCst fence；回收精确（一退役就尽快回收）。
//! - epoch：读路径多 1 条原子写（pin）；回收有延迟（要等两 epoch 推进）。
//!
//! epoch 在"读极多、回收频率不敏感"场景吞吐更高——crossbeam 的 Skiplist / queue
//! 都用 epoch。
//!
//! ## 教学简化
//!
//! 真实 crossbeam-epoch 用了 thread-local 句柄 + pin 计数 + 异步 GC 线程。我们这版
//! 把"每线程一个 epoch 槽"放进全局数组，简洁优先；段内只串行写路径（不并发改），
//! 满足教学。

use std::sync::atomic::{AtomicUsize, Ordering};

/// 上限：最多 128 个线程参与 epoch。
pub const MAX_EPOCH_THREADS: usize = 128;

/// 每个 epoch 槽的语义：0 = 未参与；>0 = 当前 pin 在 (value - 1) 这个 epoch。
/// 偏移 1 是为了区分"未参与"和"epoch 0"。
const UNPINNED: usize = 0;

struct EpochRegistry {
    global_epoch: AtomicUsize,
    /// 每线程的 local_epoch（0 = 未参与；e+1 = pin 在 epoch e）。
    locals: [AtomicUsize; MAX_EPOCH_THREADS],
    next_slot: AtomicUsize,
}

impl EpochRegistry {
    const fn new() -> Self {
        const ZERO: AtomicUsize = AtomicUsize::new(0);
        Self {
            global_epoch: ZERO,
            locals: [ZERO; MAX_EPOCH_THREADS],
            next_slot: ZERO,
        }
    }
}

static REGISTRY: EpochRegistry = EpochRegistry::new();

thread_local! {
    static MY_SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    /// 每线程三袋垃圾：garbage[(出生_epoch) % 3]。
    /// (ptr, destructor) —— destructor 知道怎么回收 ptr。
    static GARBAGE: std::cell::RefCell<[Vec<(*mut (), fn(*mut ()))>; 3]> =
        const { std::cell::RefCell::new([const { Vec::new() }, const { Vec::new() }, const { Vec::new() }]) };
    /// 本线程 pin 次数（支持嵌套 pin/unpin）。
    static PIN_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn ensure_slot() -> usize {
    MY_SLOT.with(|s| {
        let v = s.get();
        if v != usize::MAX {
            return v;
        }
        let v = REGISTRY.next_slot.fetch_add(1, Ordering::Relaxed);
        if v >= MAX_EPOCH_THREADS {
            panic!("epoch: 超过 {} 线程上限", MAX_EPOCH_THREADS);
        }
        s.set(v);
        v
    })
}

/// 进入 epoch 临界区。在 unpin 之前，回收者不会回收当前 epoch 的垃圾。
pub fn pin() {
    PIN_DEPTH.with(|d| {
        let depth = d.get();
        if depth > 0 {
            // 嵌套 pin：直接增加深度，不动 local_epoch（外层已设好）。
            d.set(depth + 1);
            return;
        }
        let slot = ensure_slot();
        let g = REGISTRY.global_epoch.load(Ordering::Acquire);
        // local_epoch = g + 1（偏移 1 区分 UNPINNED）。
        REGISTRY.locals[slot].store(g + 1, Ordering::Release);
        // SeqCst fence：与 try_advance 的 SeqCst fence 互锁，确保回收者看到我已 pin。
        std::sync::atomic::fence(Ordering::SeqCst);
        d.set(1);
    });
}

/// 退出 epoch 临界区。和 pin 配对。
pub fn unpin() {
    PIN_DEPTH.with(|d| {
        let depth = d.get();
        debug_assert!(depth > 0);
        if depth > 1 {
            d.set(depth - 1);
            return;
        }
        // 真正退出：清 local_epoch。
        let slot = ensure_slot();
        REGISTRY.locals[slot].store(UNPINNED, Ordering::Release);
        d.set(0);
        // 顺便尝试推进 epoch（让垃圾有机会被回收）。
        try_advance();
    });
}

/// 一个 RAII guard：构造时 pin，drop 时 unpin。强烈推荐这种用法，避免忘记 unpin。
pub struct EpochGuard(());

impl EpochGuard {
    pub fn new() -> Self {
        pin();
        EpochGuard(())
    }
}

impl Drop for EpochGuard {
    fn drop(&mut self) {
        unpin();
    }
}

/// 退役一个指针：放进本线程的 garbage[global_epoch % 3]。
/// 之后调 try_advance；推进两次后这个指针就会被回收。
///
/// SAFETY: ptr 必须已从数据结构里摘下来、不再被新线程 publish。
pub unsafe fn defer_destroy(ptr: *mut (), destructor: fn(*mut ())) {
    let g = REGISTRY.global_epoch.load(Ordering::Relaxed);
    GARBAGE.with(|bag| {
        bag.borrow_mut()[g % 3].push((ptr, destructor));
    });
    // 攒到一定量才尝试推进（不然每次 retire 都扫所有线程的 local，太贵）。
    let total: usize = GARBAGE.with(|b| b.borrow().iter().map(|v| v.len()).sum());
    if total >= 16 {
        try_advance();
    }
}

/// 尝试推进全局 epoch。若所有活跃线程都已 ≥ 当前 epoch，推进 +1，
/// 并回收最老那袋垃圾。
pub fn try_advance() {
    let cur = REGISTRY.global_epoch.load(Ordering::Acquire);
    let next = cur + 1;

    // 检查所有活跃线程是否都已 ≥ cur。
    let max = REGISTRY.next_slot.load(Ordering::Acquire);
    for i in 0..max {
        let l = REGISTRY.locals[i].load(Ordering::Acquire);
        if l != UNPINNED && l - 1 < cur {
            // 还有线程 pin 在比 cur 旧的 epoch —— 不能推进。
            return;
        }
    }

    // SeqCst fence：与 pin 端的 SeqCst fence 互锁，确保"我看所有 local_epoch 已 ≥ cur"
    // 这一观察不会被并发的 pin 颠覆。
    std::sync::atomic::fence(Ordering::SeqCst);

    // 推进 global_epoch。CAS（而非 store）以应对并发 try_advance。
    if REGISTRY
        .global_epoch
        .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        // 别人已推进，不重复回收。
        return;
    }

    // 推进成功：garbage[cur % 3] 现在是"最老那袋"——但它对应的是"刚被取代的 epoch"，
    // 还需要等一次推进（cur+1 → cur+2）才完全安全。
    // 实际上 crossbeam-epoch 的算法是：推进后，garbage[(next-2) % 3] 才可回收。
    // next = cur + 1，所以回收 (cur-1) % 3 这袋——它出生在 cur-1 epoch，距离 next
    // 已是两 epoch 之前，里面所有指针的读者都必然已 unpin。
    if cur >= 1 {
        let old_bag_idx = (cur.wrapping_sub(1)) % 3;
        GARBAGE.with(|bag| {
            let mut b = bag.borrow_mut();
            let to_drop = std::mem::take(&mut b[old_bag_idx]);
            // SAFETY: 这一袋的指针出生在 epoch cur-1，现在 global_epoch 已是 cur+1，
            // 中间隔了两个 epoch——任何当时 pin 的读者都已 unpin。安全回收。
            for (ptr, dtor) in to_drop {
                // dtor 是 destroy_node（safe fn，内部封装 unsafe）。
                dtor(ptr);
            }
        });
    }
}

/// 把本线程三袋垃圾里所有可回收的（按当前 global_epoch 判断）扫一遍。
pub fn collect() {
    try_advance();
}

/// 线程退出前调一下：尽力把本线程垃圾袋推到全局（这里简化：直接调 try_advance）。
pub fn flush_local() {
    try_advance();
}

// ---------------------------------------------------------------------------
// 用 epoch 包一层 Treiber 栈（与 hazard.rs 的 HazardStack 形态一致，对照两种回收）
// ---------------------------------------------------------------------------

/// 一个用 epoch 回收的 Treiber 栈。ABA 安全、内存回收延迟到 epoch 推进两次后。
pub struct EpochStack<T> {
    head: std::sync::atomic::AtomicPtr<Node<T>>,
}

struct Node<T> {
    value: std::mem::ManuallyDrop<T>,
    next: *mut Node<T>,
}

unsafe impl<T: Send> Send for EpochStack<T> {}
unsafe impl<T: Send> Sync for EpochStack<T> {}

impl<T> EpochStack<T> {
    pub const fn new() -> Self {
        Self {
            head: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn push(&self, value: T) {
        let node = Box::into_raw(Box::new(Node {
            value: std::mem::ManuallyDrop::new(value),
            next: std::ptr::null_mut(),
        }));
        let mut old = self.head.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            // SAFETY: node 是刚 Box::into_raw 出来的合法节点。
            unsafe { (*node).next = old };
            match self.head.compare_exchange_weak(
                old,
                node,
                std::sync::atomic::Ordering::Release,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return,
                // CAS 失败：actual 是当前真实 head 值，下一轮用它。
                Err(actual) => old = actual,
            }
        }
    }

    pub fn pop(&self) -> Option<T> {
        let _guard = EpochGuard::new(); // pin 整个 pop 过程
        let mut old = self.head.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if old.is_null() {
                return None;
            }
            // SAFETY: pin 期间回收者不会回收 old（old 在当前 epoch 被 retire 的话，
            // 至少要等两 epoch 推进，而我们 pin 期间它不会被推进）。
            let next = unsafe { (*old).next };
            if self
                .head
                .compare_exchange_weak(
                    old,
                    next,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                // SAFETY: CAS 成功意味着 old 已独占，value 归我。
                let value = unsafe { std::mem::ManuallyDrop::take(&mut (*old).value) };
                // SAFETY: old 已从栈摘下，不再被新线程 publish。
                unsafe { defer_destroy(old as *mut (), destroy_node::<T>) };
                return Some(value);
            }
            old = self.head.load(std::sync::atomic::Ordering::Acquire);
        }
    }
}

impl<T> Default for EpochStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for EpochStack<T> {
    fn drop(&mut self) {
        let mut cur = *self.head.get_mut();
        while !cur.is_null() {
            // SAFETY: 先读 next 再 drop 节点。
            unsafe {
                let next = (*cur).next;
                let mut node = Box::from_raw(cur);
                let _ = std::mem::ManuallyDrop::take(&mut (*node).value);
                drop(node);
                cur = next;
            }
        }
    }
}

// 安全包装：destructor 类型是 `fn(*mut ())`（非 unsafe fn），内部封装 unsafe。
// （unsafe fn 不能直接作为 fn 指针传给 Vec<(*mut (), fn(*mut ()))>。）
fn destroy_node<T>(p: *mut ()) {
    // SAFETY: p 是 Box::into_raw(Node<T>) 出来的；ManuallyDrop 不 drop value。
    let _ = unsafe { Box::from_raw(p as *mut Node<T>) };
}
