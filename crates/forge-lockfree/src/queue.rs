//! # Michael-Scott 无锁队列 —— MPMC 无锁 FIFO 的祖宗
//!
//! 1996 年 Maged Michael 与 Michael Scott 发表的《Simple, Fast, and Practical
//! Non-Blocking and Blocking Concurrent Queue Algorithms》是历史上第一个被证明正确
//! 的 MPMC（multi-producer / multi-consumer）无锁 FIFO。它的形态后来几乎被所有
//! 无锁队列复制：crossbeam_queue::SegQueue、Java ConcurrentLinkedQueue、
//! Linux 内核的 `lqueue` ……都源自这一篇论文。
//!
//! ## 形态：两个原子指针 + 一个哑节点
//!
//! 队列里永远保留一个**哑节点（dummy/sentinel）**——这样"空队列"和"非空队列"
//! 在代码路径上长得一样，不需要特判。两个游标：
//!
//! - `head` —— 指向**最老**的节点（出队从这里"摘下来"）。head 永远不 null。
//! - `tail` —— 指向**最新**的节点（入队往后接）。tail 永远不 null。
//!
//! 不变量：head 和 tail 中间至少有一个节点（哑节点）。出队取出的是 head 的**下一个**
//! 节点的值，然后把 head 推进一格——**老的 head 变成新的哑节点**（值被丢弃）。
//!
//! ## 入队：两步 CAS（这是 MS 队列的精髓）
//!
//! 入队分两条 CAS，缺一不可：
//!
//! 1. CAS `tail.next: null → N` —— 把新节点接在当前 tail 后面。这条 CAS 可能被
//!    别的入队线程抢先；失败就重读 tail、重试。
//! 2. CAS `tail: old_tail → N` —— 把 tail 推进到新节点。**这条 CAS 允许失败**——
//!    因为别的线程（或"乐于助人的旁观者"）可能已经替我们推进了 tail。
//!
//! 注意第 2 步：MS 论文叫它"helping"——任何路过的线程发现 tail 落后于 tail.next
//! 时，都可以顺手把 tail 推进。这保证了"tail 长期落后"不可能发生。
//!
//! ## 出队：CAS head
//!
//! 读 head.next；若为 null 则队列空（只剩哑节点）；否则 CAS `head: old → head.next`。
//! 出队成功后，旧 head（旧哑节点）的内存**不能立刻释放**——别的线程可能正读着它。
//! 这里采用与 stack.rs 同样的教学取舍：**故意泄漏**节点，规避 ABA。
//! 教程在 docs/modules/M8-lockfree.md 里手算 ABA 如何发生、并指向 hazard pointer
//! / epoch 回收两种工业级解法。
//!
//! ## 内存序配方
//!
//! - 入队第 1 条 CAS（tail.next）用 `Release`（成功）：发布新节点内容给出队的 `Acquire`。
//! - 入队第 2 条 CAS（推进 tail）用 `Release`（成功）：发布"tail 已更新"。
//! - 出队的 CAS 用 `AcqRel`（成功）：Acquire 看到入队线程发布的数据，Release 把
//!   "head 已推进"这一事实发布给下一位出队者。
//! - 失败一律 `Relaxed`：失败只是重试，没消费任何东西。
//!
//! ## 一个必选手算：入队的两步 CAS 在并发 dequeue 下的交错
//!
//! 见 `docs/modules/M8-lockfree.md` 第 M8c 节末尾的"MS 队列交错手算"。

use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// 节点用 `Option<T>` 表示——哑节点是 `None`，真实节点是 `Some(v)`。
/// 这让我们不必处理"未初始化 T"的 UB：哑节点根本不持有 T。
struct Node<T> {
    /// `None` 表示哑节点；`Some(v)` 表示真实节点。出队时从这里 `take()`。
    value: Option<T>,
    next: AtomicPtr<Node<T>>,
}

impl<T> Node<T> {
    fn dummy() -> *mut Node<T> {
        Box::into_raw(Box::new(Node {
            value: None,
            next: AtomicPtr::new(ptr::null_mut()),
        }))
    }
}

/// Michael-Scott MPMC 无锁 FIFO。
pub struct Queue<T> {
    head: AtomicPtr<Node<T>>,
    tail: AtomicPtr<Node<T>>,
}

// 队列把 T 在线程间转移 → T: Send。
unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    /// 新建空队列（带一个哑节点）。
    pub fn new() -> Self {
        let dummy = Node::<T>::dummy();
        Self {
            head: AtomicPtr::new(dummy),
            tail: AtomicPtr::new(dummy),
        }
    }

    /// 入队：两步 CAS。
    pub fn enqueue(&self, value: T) {
        let node = Box::into_raw(Box::new(Node {
            value: Some(value),
            next: AtomicPtr::new(ptr::null_mut()),
        }));
        let mut tail = self.tail.load(Ordering::Acquire);
        loop {
            // SAFETY: tail 来自队列的 tail 指针，只要队列还活着、tail 一定指向合法节点。
            // 即便 tail 已被推进，老 tail 仍是被泄漏保留的合法节点（教学版不回收）。
            let next = unsafe { (*tail).next.load(Ordering::Acquire) };
            // 重读 tail——如果它已变，说明别的入队推进了 tail，重新评估。
            let tail_now = self.tail.load(Ordering::Acquire);
            if tail == tail_now {
                if next.is_null() {
                    // tail 是真正的队尾：CAS 把新节点接上。
                    // SAFETY: 同上，tail 仍合法。
                    if unsafe {
                        (*tail).next.compare_exchange(
                            ptr::null_mut(),
                            node,
                            Ordering::Release,
                            Ordering::Relaxed,
                        )
                    }
                    .is_ok()
                    {
                        // 第 2 步 CAS 推进 tail。允许失败——别人可能已替我们推进。
                        let _ = self.tail.compare_exchange(
                            tail,
                            node,
                            Ordering::Release,
                            Ordering::Relaxed,
                        );
                        return;
                    }
                    // 第 1 步 CAS 失败：重读 tail 重试。
                } else {
                    // tail 不是真正的队尾（有人接上了节点但还没推进 tail）。
                    // "Helping"：替那个人把 tail 推进。允许失败。
                    let _ = self.tail.compare_exchange(
                        tail,
                        next,
                        Ordering::Release,
                        Ordering::Relaxed,
                    );
                }
                // 落到循环底：重读 tail。
            }
            tail = self.tail.load(Ordering::Acquire);
        }
    }

    /// 出队：返回队首元素。空队列返回 None。
    pub fn dequeue(&self) -> Option<T> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Acquire);
            // SAFETY: head/tail 永远指向合法节点（哑节点保证 head 非 null）。
            let next = unsafe { (*head).next.load(Ordering::Acquire) };
            // 双检：head 仍是当初那个 head 吗？
            if head == self.head.load(Ordering::Acquire) {
                if head == tail {
                    if next.is_null() {
                        // 队列空：只剩哑节点，head.next == null。
                        return None;
                    }
                    // tail 落后于 head：先把 tail 推到 next（helping），下轮再 dequeue。
                    let _ = self.tail.compare_exchange(
                        tail,
                        next,
                        Ordering::Release,
                        Ordering::Relaxed,
                    );
                } else {
                    // 非空：先 CAS head 推进到 next，**成功后**才把 next 的 value take 出来。
                    // 这样绝不会出现"take 出 value 但 CAS 失败、被别的 dequeue 也 take"的双重消费。
                    if self
                        .head
                        .compare_exchange(head, next, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        // SAFETY: CAS 成功意味着 head 已独占地推进到 next，
                        // next.value 现在归我独占。take 出来作为返回值。
                        // 旧 head（旧哑节点）的内存故意不释放：教学版规避 ABA。
                        let value = unsafe { (*next).value.take() };
                        return value;
                    }
                    // CAS 失败：循环重试。
                }
            }
        }
    }
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        // drop 时把队列里残留的节点全部回收（运行期间教学版会泄漏 dequeue 出的旧哑节点；
        // Drop 只能回收"队列本身还活着、未 dequeue 的节点 + 当前哑节点"）。
        // 真实工程用 hazard/epoch 才能回收散落在外的旧哑节点。
        let mut cur = *self.head.get_mut();
        while !cur.is_null() {
            // SAFETY: 每个节点都是 Box::into_raw 出来的；单线程 Drop 不竞争。
            // 先把 next 读出来（避免 use-after-free），再 drop 节点本体。
            // 注意 value：如果是真实节点且未被 take，Option<T> drop 会调用 T::drop，
            // 这是 sound 的——任何还在队列里的真实节点 value 一定没被 take 走。
            unsafe {
                let next = (*cur).next.load(Ordering::Relaxed);
                drop(Box::from_raw(cur));
                cur = next;
            }
        }
    }
}
