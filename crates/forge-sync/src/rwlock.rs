//! # M7：自建写公平 `RwLock<T>`
//!
//! 演化（教程详讲）：
//! 1. **基础**：`state` = 读者数，`u32::MAX` = 写锁。读者 CAS +1、写者 CAS 0→MAX。
//!    问题：写者在大量读者下会**忙转**（读者数不停变，wait 的 expected 老对不上）。
//! 2. **独立写者唤醒计数器 `writer_wake_counter`**：写者等它而非 state，只在真要
//!    唤醒写者时才改它。
//! 3. **写公平（防写饥饿）**：把 state 编码为"读者数×2，+1 表示有写者在等"。
//!    于是**奇数 state 时新读者必须等**，写者终能拿到锁。`u32::MAX`（奇）仍表写锁。

use crate::atomic_wait::{wake_all, wake_one, wait};
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct RwLock<T> {
    /// 读者数 × 2，+1 若有写者在等；`u32::MAX` 表示写锁。
    /// ⇒ state 为**偶**时读者可进（+2），为**奇**时读者必须等。
    state: AtomicU32,
    /// 仅在要唤醒写者时 +1，写者等它（避免被频繁变化的读者数吵醒）。
    writer_wake_counter: AtomicU32,
    value: UnsafeCell<T>,
}

// RwLock 让多个读者线程同时持 &T → 需要 T: Sync；写者会把 T 送别的线程 → T: Send。
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            writer_wake_counter: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            if s % 2 == 0 {
                // 偶数：可读。+2 占一个读锁。
                assert!(s != u32::MAX - 2, "too many readers");
                match self.state.compare_exchange_weak(s, s + 2, Ordering::Acquire, Ordering::Relaxed) {
                    Ok(_) => return ReadGuard { rwlock: self },
                    Err(e) => s = e,
                }
            }
            if s % 2 == 1 {
                // 奇数：有写者在等或已写锁 → 等。
                wait(&self.state, s);
                s = self.state.load(Ordering::Relaxed);
            }
        }
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            // 未锁（0 或 1）→ 抢写锁。
            if s <= 1 {
                match self.state.compare_exchange(s, u32::MAX, Ordering::Acquire, Ordering::Relaxed) {
                    Ok(_) => return WriteGuard { rwlock: self },
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }
            // 把 state 拨成奇数（+1），挡住新读者，防写饥饿。
            if s % 2 == 0 {
                match self.state.compare_exchange(s, s + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {}
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }
            // 等写者唤醒计数器（仅在真要唤醒写者时才变）。
            let w = self.writer_wake_counter.load(Ordering::Acquire);
            s = self.state.load(Ordering::Relaxed);
            if s >= 2 {
                wait(&self.writer_wake_counter, w);
                s = self.state.load(Ordering::Relaxed);
            }
        }
    }
}

pub struct ReadGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}
pub struct WriteGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}
impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}
impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.rwlock.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        // 读者数 -2。若从 3 减到 1：最后一个读者 + 有写者在等 → 唤醒一个写者。
        if self.rwlock.state.fetch_sub(2, Ordering::Release) == 3 {
            self.rwlock.writer_wake_counter.fetch_add(1, Ordering::Release);
            wake_one(&self.rwlock.writer_wake_counter);
        }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.rwlock.state.store(0, Ordering::Release);
        self.rwlock.writer_wake_counter.fetch_add(1, Ordering::Release);
        wake_one(&self.rwlock.writer_wake_counter); // 可能的等待写者
        wake_all(&self.rwlock.state); // 所有等待读者
    }
}
