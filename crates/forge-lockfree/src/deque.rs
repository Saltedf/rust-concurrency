//! # Chase-Lev 工作窃取双端队列 —— 调度器的心脏
//!
//! owner 在 **bottom** 端 push/pop（LIFO，缓存友好）；其它 worker（stealer）从 **top**
//! 端偷（FIFO，少争用）。正是 `crossbeam-deque` / `rayon` 调度器的核心。
//!
//! 固定容量（教学版，不动态扩容）。内存序严格按 Le, Nguyen 等《Correct and Efficient
//! Work-Stealing for Weak Memory Models》放置 fence——弱内存架构下正确的关键。

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{fence, AtomicIsize, Ordering};

const CAP: usize = 4096;

pub struct Deque<T> {
    bottom: AtomicIsize,
    top: AtomicIsize,
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

/// 偷取结果。
pub enum Steal<T> {
    Success(T),
    Empty,
    /// 本次没偷到（被别人抢先），可重试。
    Retry,
}

/// stealer 句柄：可克隆，发给别的 worker 去偷任务。
pub struct Stealer<T> {
    deque: *const Deque<T>,
}

unsafe impl<T: Send> Send for Deque<T> {}
unsafe impl<T: Send> Sync for Deque<T> {}
unsafe impl<T: Send> Send for Stealer<T> {}
unsafe impl<T: Send> Sync for Stealer<T> {}
impl<T: Send> Clone for Stealer<T> {
    fn clone(&self) -> Self {
        Stealer { deque: self.deque }
    }
}

impl<T> Deque<T> {
    pub fn new() -> Self {
        let buffer: Box<[UnsafeCell<MaybeUninit<T>>]> = (0..CAP)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect();
        Self {
            bottom: AtomicIsize::new(0),
            top: AtomicIsize::new(0),
            buffer,
        }
    }

    /// 取某槽位的裸 `MaybeUninit` 指针。
    #[inline]
    unsafe fn slot(&self, i: isize) -> *mut MaybeUninit<T> {
        (*self.buffer.get_unchecked(i as usize % CAP)).get()
    }

    /// owner：在 bottom 端压入一个任务。
    pub fn push(&self, value: T) {
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Acquire);
        assert!((b - t) < CAP as isize, "deque overflow");
        unsafe {
            (*self.slot(b)).write(value);
        }
        fence(Ordering::Release); // Le et al.：先写槽、再 fence、再发布 bottom
        self.bottom.store(b + 1, Ordering::Relaxed);
    }

    /// owner：从 bottom 端弹出一个（LIFO）。空则 None。
    pub fn pop(&self) -> Option<T> {
        let b = self.bottom.load(Ordering::Relaxed) - 1;
        self.bottom.store(b, Ordering::Relaxed);
        fence(Ordering::SeqCst); // Le et al.：协调与 steal 的竞争
        let t = self.top.load(Ordering::Relaxed);
        if t <= b {
            let v = unsafe { (*self.slot(b)).assume_init_read() };
            if t == b {
                // 最后一个：与 steal 竞争。
                let won = self
                    .top
                    .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok();
                self.bottom.store(b + 1, Ordering::Relaxed);
                if won {
                    Some(v)
                } else {
                    std::mem::forget(v); // 被 steal 拿走，v 未被初始化为合法 T，跳过 drop
                    None
                }
            } else {
                Some(v)
            }
        } else {
            self.bottom.store(b + 1, Ordering::Relaxed);
            None
        }
    }

    pub fn stealer(&self) -> Stealer<T> {
        Stealer { deque: self as *const _ }
    }

    fn steal_inner(&self) -> Steal<T> {
        let t = self.top.load(Ordering::Acquire);
        fence(Ordering::Acquire);
        let b = self.bottom.load(Ordering::Acquire);
        if t >= b {
            return Steal::Empty;
        }
        let v = unsafe { (*self.slot(t)).assume_init_read() };
        if self
            .top
            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            Steal::Success(v)
        } else {
            std::mem::forget(v);
            Steal::Retry
        }
    }
}

impl<T> Default for Deque<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Stealer<T> {
    /// 偷一个任务。安全：调用方保证 Deque 比 Stealer 长寿。
    pub fn steal(&self) -> Steal<T> {
        unsafe { (*self.deque).steal_inner() }
    }
}
