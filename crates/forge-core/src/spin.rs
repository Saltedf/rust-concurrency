//! # 模块 M3：自建自旋锁 `SpinLock<T>`
//!
//! 一个 [`std::sync::Mutex`] 在锁已被占用时会**让线程睡觉**。但如果锁只被持很
//! 短的一瞬、且线程们跑在不同核上，反复"试着锁"（**自旋**）比睡-醒更省延迟。
//! 自旋锁就是干这个的：锁不上就忙等，直到锁上。
//!
//! 我们按原书第 4 章的三步演化（最小版 → UnsafeCell + unsafe → Guard 安全版）
//! 最终得到下面这个**完全安全**的接口。详见 `docs/modules/M3-spinlock.md`。

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

/// 一个自旋锁，保护一个 `T`。
///
/// 用法（和 `std::sync::Mutex` 几乎一样）：`let g = lock.lock();` 拿到守卫，
/// 通过守卫读写 `T`，守卫 `drop` 时自动解锁。
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// 安全性论证：
// - SpinLock 同一时刻只让**一个**线程碰 `T`（靠 `lock` 的原子互斥），
//   所以不需要 `T: Sync`（从不并发共享 `&T`）。
// - 但它会把 `T` 从一个线程"送"到另一个（A 锁-改-解锁，B 后续锁会看到 A 的改），
//   所以要求 `T: Send`。
// - `UnsafeCell` 默认 `!Sync`，会让我们整个类型也没法跨线程共享；
//   我们在这里向编译器**承诺**：只要 `T: Send`，`SpinLock<T>` 跨线程共享是安全的。
unsafe impl<T: Send> Sync for SpinLock<T> {}
// （`SpinLock<T>: Send` 在 `T: Send` 时由 auto-trait 自动成立，无需手写。）

impl<T> SpinLock<T> {
    /// 创建一个保护 `value` 的未锁定自旋锁。
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// 阻塞地获取锁，返回一个守卫 [`Guard`]。守卫 `drop` 时自动解锁。
    pub fn lock(&self) -> Guard<'_, T> {
        // swap(true, Acquire)：原子地"读旧值并写 true"。
        //   - 返回 false（原来是未锁）→ 我们抢到了，跳出循环。
        //   - 返回 true（原来已锁）→ 别人持着，自旋重试。
        // Acquire：与上一次解锁的 Release 建立 happens-before，
        // 保证我们看到上一个临界区里对 T 的所有修改。
        while self.locked.swap(true, Ordering::Acquire) {
            // 告诉 CPU"我在自旋等某个变量变化"，它会优化流水线（不调用 OS 睡眠）。
            spin_loop();
        }
        Guard { lock: self }
    }
}

/// 锁守卫：它的**存在**就是"我已独占该锁"的证明。
/// 行为像 `&mut T`（靠 `Deref`/`DerefMut`），`drop` 时自动解锁（靠 `Drop`）。
pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // 安全：Guard 的存在本身就证明我们独占了锁，没有别的线程在碰 T。
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // 安全：同上，独占访问。
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // 解锁：Release 把本临界区里对 T 的修改"发布"给下一个 `lock` 的 Acquire。
        self.lock.locked.store(false, Ordering::Release);
    }
}
