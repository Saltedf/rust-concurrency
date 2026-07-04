//! # M7：自建 futex `Mutex<T>`（3 态 + 自适应自旋）
//!
//! 在 M6 的 atomic-wait 上造一个真 mutex。演化（教程详讲）：
//! 1. **2 态**（0 未锁 / 1 已锁）：unlock 无条件 `wake_one` —— 浪费 syscall。
//! 2. **3 态**（0 / 1 已锁无等待者 / 2 已锁有等待者）：unlock 仅在曾是 2 时才 wake。
//!    **非竞争路径零 syscall**（关键优化，约 10 倍提速）。
//! 3. **自适应自旋**：抢锁失败先自旋 ≤100 次（持锁者很可能在别核上、马上就放），
//!    再退化到 wait。是否划算"取决于"——但 std 在 Linux 上也用 100。

use crate::atomic_wait::{wake_one, wait};
use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Mutex<T> {
    /// 0: 未锁；1: 已锁、无等待者；2: 已锁、有等待者。
    state: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        // 快速路径：0→1 直接抢。非竞争时只有这一次 CAS，零 syscall。
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            lock_contended(&self.state);
        }
        MutexGuard { mutex: self }
    }
}

#[cold]
fn lock_contended(state: &AtomicU32) {
    // 先自旋：仅当"已锁、无等待者"时自旋（有等待者说明别人已放弃自旋，多半没用）。
    let mut spin_count = 0;
    while state.load(Ordering::Relaxed) == 1 && spin_count < 100 {
        spin_count += 1;
        spin_loop();
    }
    // 自旋后再试一次 0→1（一旦 wait 过，就必须用 2，否则会漏掉等待者）。
    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    // 放弃自旋：swap 成 2（标记"有等待者"），轮到 0 才算抢到。
    while state.swap(2, Ordering::Acquire) != 0 {
        wait(state, 2);
    }
}

pub struct MutexGuard<'a, T> {
    pub(crate) mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // 仅当曾是 2（有等待者）才 wake_one：非竞争路径零 syscall。
        if self.mutex.state.swap(0, Ordering::Release) == 2 {
            wake_one(&self.mutex.state);
        }
    }
}
