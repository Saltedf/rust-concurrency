//! # MCS 队列锁 —— 每线程一个队列节点，FIFO 公平
//!
//! 与内核维护等待队列不同，MCS **自己**用一条 `AtomicPtr` 链表维护等待者：
//! 每个线程在**自己的栈上**（实际堆）放一个节点，CAS 接到队尾，然后**睡在自己的节点上**
//! （`thread::park`），被前驱 `unpark` 唤醒。解锁时唤醒后继。
//!
//! 好处：FIFO 公平、每个锁只占一个指针大小、cache 争用低（每个线程等自己的节点）。
//! Windows SRW 锁就是这个模式。

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::thread::{self, Thread};

pub struct McsLock {
    /// 队列尾指针：指向最后一个等待节点（或 null）。
    tail: AtomicPtr<Node>,
}

struct Node {
    /// 前驱是否已把锁交给我。false=还在等。
    granted: AtomicBool,
    /// 后继节点（前驱解锁时填）。
    next: AtomicPtr<Node>,
    /// 等待线程的句柄（前驱解锁时 unpark 它）。
    thread: Thread,
}

unsafe impl Send for McsLock {}
unsafe impl Sync for McsLock {}

impl McsLock {
    pub const fn new() -> Self {
        Self {
            tail: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn lock(&self) -> McsGuard<'_> {
        let node = Box::into_raw(Box::new(Node {
            granted: AtomicBool::new(false),
            next: AtomicPtr::new(std::ptr::null_mut()),
            thread: thread::current(),
        }));
        // 把我的节点 swap 接到队尾，换出"前驱"。
        // AcqRel：Release 发布本节点（构造内容）给后继；Acquire 看到前驱发布的数据。
        let predecessor = self.tail.swap(node, Ordering::AcqRel);
        if !predecessor.is_null() {
            // 有前驱：排队等。告诉前驱"我是你的后继"，然后睡自己。
            unsafe {
                (*predecessor).next.store(node, Ordering::Release);
            }
            while !unsafe { (*node).granted.load(Ordering::Acquire) } {
                thread::park();
            }
        }
        McsGuard { lock: self, node }
    }
}

pub struct McsGuard<'a> {
    lock: &'a McsLock,
    node: *mut Node,
}

impl Drop for McsGuard<'_> {
    fn drop(&mut self) {
        // 若我是队尾（没后继），CAS 把 tail 从我清回 null。
        let mut next = unsafe { (*self.node).next.load(Ordering::Acquire) };
        if next.is_null() {
            if self
                .lock
                .tail
                .compare_exchange(self.node, std::ptr::null_mut(), Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                unsafe { drop(Box::from_raw(self.node)) };
                return;
            }
            // CAS 失败：有人正接上后继，等它的 next 写好。
            while next.is_null() {
                next = unsafe { (*self.node).next.load(Ordering::Acquire) };
            }
        }
        // 把锁交给后继：唤醒它。
        // ⚠ 关键顺序:必须先 clone 后继的 Thread 句柄,**再** store(granted=true)。
        // 因为 store 之后后继可能立刻被唤醒 → 它拿锁、干完活、unlock → 在它自己的
        // Drop 里 `Box::from_raw(它的节点= next)` 把 M free 掉。那时本线程若再读
        // `(*next).thread` 就是 use-after-free(miri 抓到的真实 race)。先 clone 出来,
        // grant 之后就不再碰 M,只对 clone 来的 Thread 句柄 unpark。
        unsafe {
            let successor_thread = (*next).thread.clone();
            (*next).granted.store(true, Ordering::Release);
            successor_thread.unpark();
            // 此时 M 可能已被后继 free,绝不能再解引用 next。free 的是自己的节点。
            drop(Box::from_raw(self.node));
        }
    }
}
