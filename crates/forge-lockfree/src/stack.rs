//! # Treiber 无锁栈 —— CAS 在 `AtomicPtr` 上反复施展
//!
//! push：建节点，把 `head` CAS 指向它。pop：读 head、读 next、把 `head` CAS 指向 next。
//! 全程无锁，全靠 CAS 循环。
//!
//! ⚠️ **ABA 问题**：若 pop 出的节点被释放、地址又被 push 复用，可能让某个线程的
//! 过期 CAS 错误成功 → use-after-free。彻底解法是"代位指针"（高位存代次）或
//! hazard pointer / epoch 回收。**这里 pop 后故意不释放节点（泄漏）来规避 ABA**——
//! 教程里讲清如何用代位指针/回收做得更好。
//!
//! ## 不再泄漏的版本
//!
//! `crates/forge-lockfree/src/hazard.rs::HazardStack` 和 `epoch.rs::EpochStack`
//! 是同一份 Treiber 栈的"真实回收版"——前者用 hazard pointer 公告栏（每线程一个
//! hazard 槽 + SeqCst fence + 扫描回收），后者用 epoch-based reclamation（pin +
//! defer_destroy + 两 epoch 窗口批量回收）。两者都 ABA 安全、不泄漏。教学上三版
//! 并存：stack.rs 看 ABA 是怎么发生的（泄漏规避）、hazard.rs 看公告栏思路、
//! epoch.rs 看 epoch 窗口思路。

use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

struct Node<T> {
    value: ManuallyDrop<T>,
    next: *mut Node<T>,
}

pub struct Stack<T> {
    head: AtomicPtr<Node<T>>,
}

// 栈把 T 在线程间转移（push 一线程、pop 另一线程）→ T: Send。
unsafe impl<T: Send> Send for Stack<T> {}
unsafe impl<T: Send> Sync for Stack<T> {}

impl<T> Stack<T> {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn push(&self, value: T) {
        let node = Box::into_raw(Box::new(Node {
            value: ManuallyDrop::new(value),
            next: ptr::null_mut(),
        }));
        // CAS 把 head 从 old 换成 node；失败则更新 old、重试。
        let mut old = self.head.load(Ordering::Relaxed);
        loop {
            unsafe { (*node).next = old };
            match self.head.compare_exchange_weak(
                old,
                node,
                Ordering::Release, // 发布节点内容给 pop 的 Acquire
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => old = actual,
            }
        }
    }

    pub fn pop(&self) -> Option<T> {
        let mut old = self.head.load(Ordering::Acquire);
        loop {
            if old.is_null() {
                return None;
            }
            let next = unsafe { (*old).next };
            if self
                .head
                .compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let value = unsafe { ManuallyDrop::take(&mut (*old).value) };
                // ⚠️ 故意不释放 old（规避 ABA；教学取舍，会泄漏）。
                return Some(value);
            }
            old = self.head.load(Ordering::Acquire);
        }
    }
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Self::new()
    }
}
