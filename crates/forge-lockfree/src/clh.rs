//! # CLH 队列锁 —— Craig / Landin / Hagersten 1993
//!
//! ## 与 MCS 的差别（这是它的"敌人"所在）
//!
//! 复习 mcs.rs：MCS 的 spinner **spin 在自己节点的 granted 字段上**——前驱解锁时
//! 主动写后继节点的 granted。
//!
//! CLH 反过来：每个 spinner **spin 在前驱节点的 locked 字段上**——队列指针在线程
//! 之间传递，每个线程拿着"前驱给我的节点"自旋。锁本身只需要一个 `AtomicPtr` 指向
//! 队列尾，**不需要后继指针**。
//!
//! |  | MCS | CLH |
//! |---|---|---|
//! | spin 在 | 自己节点的 granted | 前驱节点的 locked |
//! | 节点是否跨线程传递 | 否（每人一个） | 是（前驱→后继） |
//! | 后继指针 | 要（前驱解锁要 unpark 后继） | 不要 |
//! | NUMA 表现 | 好（spin 在本地节点） | 差（前驱可能在远端 NUMA） |
//! | unlock 复杂度 | 中（要等 next 写好） | 低（一行 store） |
//!
//! CLH 更简单（解锁仅一行 store）；MCS 在 NUMA 上更快。Williams《C++ Concurrency
//! in Action》第 5 章详细对比过这两种。
//!
//! ## 形态：节点只有一个 `AtomicBool`
//!
//! ```ignore
//! pub struct ClhLock { tail: AtomicPtr<Node> }
//! struct Node { locked: AtomicBool }
//! ```
//!
//! lock 流程：
//! 1. 我分配一个新节点 N，`locked = true`（"我要锁"）。
//! 2. `tail.swap(N)` 换出前驱节点 P（null 表示队首，没人挡我）。
//! 3. **我 spin 在 P.locked 上**，等它变 false。
//!
//! unlock 流程：
//! 1. 把**我自己节点的 locked** 置为 false。（后继正在 spin 在我的节点上，立刻看到。）
//! 2. 我的前驱节点已经在前驱解锁时被设为 false 并被前驱放弃——可以回收。
//!
//! 精髓：**解锁就是把"我自己节点的状态"翻转**——后继一直盯着我的节点，所以一行
//! store 就完成了交接。这是 CLH 比 MCS 简洁的地方。代价：每个 spinner 都在反复读
//! **前驱线程**节点的缓存行——前驱可能在另一个 NUMA 节点上，跨 socket 读比读自己
//! L1 慢一个数量级。MCS 把 spin 目标固定在自己节点上，绕开了这个 NUMA 罚款。
//!
//! ## 节点生命周期：节点在线程间传递
//!
//! lock 时我把节点 N swap 进 tail；unlock 时我把 N 的状态设为 false（后继能看到），
//! 然后回收**前驱**节点（前驱已经放手、它不再被任何线程 spin）。
//!
//! 我的节点 N 不在我 unlock 时回收——因为后继正在 spin 它。后继在它自己的 unlock
//! 时回收 N（那时 N 已经是它的"前驱"了）。
//!
//! 唯一泄漏点：如果某线程是"最后一个进队列的"，它 unlock 时没后继来接管它的节点，
//! 这个节点会泄漏。教学版接受这一泄漏（生产实现用 thread-local 节点池解决）。

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

pub struct ClhLock {
    /// 队列尾：指向最后加入的节点的指针。null = 队列空。
    tail: AtomicPtr<Node>,
}

struct Node {
    /// 该节点对应的"持锁意图"。true = 想锁 / 持有；false = 已放手。
    /// 后继 spin 在前驱节点的这个字段上。
    locked: AtomicBool,
}

unsafe impl Send for ClhLock {}
unsafe impl Sync for ClhLock {}

impl ClhLock {
    pub const fn new() -> Self {
        Self {
            tail: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn lock(&self) -> ClhGuard<'_> {
        let node = Box::into_raw(Box::new(Node {
            locked: AtomicBool::new(true), // "我要锁"
        }));
        // AcqRel：Release 发布我节点的构造；Acquire 看到前驱节点的状态。
        let predecessor = self.tail.swap(node, Ordering::AcqRel);
        if !predecessor.is_null() {
            // 有前驱：spin 在前驱节点的 locked 上。
            // SAFETY: predecessor 是前驱线程放进 tail 的节点。前驱 unlock 时会把
            // predecessor.locked 设为 false（前驱把"自己节点"当作 locked 标志位）。
            // 前驱此后不再回收 predecessor（它把这个责任交给后继，即我）。
            // 所以我 spin 期间 predecessor 合法可读。
            while unsafe { (*predecessor).locked.load(Ordering::Acquire) } {
                std::hint::spin_loop();
            }
        }
        ClhGuard {
            _lock: self,
            my_node: node,
            predecessor,
        }
    }
}

pub struct ClhGuard<'a> {
    _lock: &'a ClhLock,
    my_node: *mut Node,
    /// 前驱节点（可能为 null，若我是队首）。unlock 时回收它（它已被前驱放手）。
    predecessor: *mut Node,
}

impl Drop for ClhGuard<'_> {
    fn drop(&mut self) {
        // 解锁：把我节点的 locked 置 false。后继（若有）正在 spin 我节点，立刻看到。
        // Release：与后继 lock 的 Acquire 配对，建立 happens-before——
        // 我在临界区里的所有写，对后继获锁后可见。
        // SAFETY: my_node 由本 guard 持有。
        unsafe { (*self.my_node).locked.store(false, Ordering::Release) };

        // 回收前驱节点：前驱在前驱的 unlock 里把它设为 false 后就放弃了它，
        // 没有别的线程还会碰它。安全回收。
        if !self.predecessor.is_null() {
            // SAFETY: predecessor 此刻不再被任何线程 spin（前驱已放手），
            // 也已被前驱从自己的 guard 里舍弃。
            unsafe { drop(Box::from_raw(self.predecessor)) };
        }

        // 注意：my_node **不**在这里回收。后继（若有）正在 spin 它。
        // 后继拿到它作为自己的 predecessor，会在后继 unlock 时回收它。
        // 若没有后继，my_node 泄漏（教学取舍，注释见模块顶层）。
    }
}
