//! # M7：自建 futex `Condvar`
//!
//! 思路：用一个 `counter`——每次 notify 都改它，于是 `wait` 只要在解锁 mutex
//! **之前**记下 counter 值，解锁后再 `wait(counter)`，就能保证不漏掉
//! "解锁之后、入睡之前"这段时间的通知（futex 的 expected 检查兜底）。
//! 再加 `num_waiters`：没人在等就跳过 wake，省 syscall。
//!
//! 内存序：counter 和 num_waiters 都用 Relaxed——所需同步全由**配对的 mutex**
//! 提供（解锁-加锁的 happens-before）。教程详讲。

use crate::atomic_wait::{wake_all, wake_one, wait};
use crate::mutex::MutexGuard;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

pub struct Condvar {
    counter: AtomicU32,
    num_waiters: AtomicUsize,
}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            num_waiters: AtomicUsize::new(0),
        }
    }

    pub fn notify_one(&self) {
        if self.num_waiters.load(Relaxed) > 0 {
            self.counter.fetch_add(1, Relaxed);
            wake_one(&self.counter);
        }
    }

    pub fn notify_all(&self) {
        if self.num_waiters.load(Relaxed) > 0 {
            self.counter.fetch_add(1, Relaxed);
            wake_all(&self.counter);
        }
    }

    /// 原子地"解锁 mutex + 等通知"，被唤醒后重新加锁并返回新 guard。
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.num_waiters.fetch_add(1, Relaxed);
        let counter_value = self.counter.load(Relaxed);
        // 记下 mutex 以便事后重新加锁；drop guard 即解锁。
        let mutex = guard.mutex;
        drop(guard);
        // 仅当 counter 仍是旧值才睡 → 不会漏掉解锁后的通知。
        wait(&self.counter, counter_value);
        self.num_waiters.fetch_sub(1, Relaxed);
        mutex.lock()
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}
