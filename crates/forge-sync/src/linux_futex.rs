//! # M6 附属（仅 Linux）：手写 futex syscall，看清"睡/醒"的内核边界
//!
//! 这是 [`crate::atomic_wait`]（以及 std 的 `thread::park`，自 Rust 1.48 起）在
//! Linux 上的真实实现。我们**直接调用 `SYS_futex`**，不经过 libc 的 pthread——
//! 因为 Linux 的 syscall 接口是稳定承诺，允许直连。
//!
//! 只实现最核心的 `FUTEX_WAIT` 和 `FUTEX_WAKE`（带 `FUTEX_PRIVATE_FLAG`，
//! 告诉内核"这是进程内等待"，可跳过一些跨进程步骤，更快）。

#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicU32;

/// futex 操作常量。PRIVATE 表示"仅同进程内"——常见情况，内核可优化。
const FUTEX_WAIT: i32 = libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG;
const FUTEX_WAKE: i32 = libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG;

/// 若 `a` 此刻仍等于 `expected`，则阻塞当前线程（直到被 wake 或假唤醒）。
///
/// "检查 expected" 与 "入睡" 对其它 futex 操作是原子的——这正是唤醒不会丢失的根源。
pub fn wait(a: &AtomicU32, expected: u32) {
    // futex(2) 签名：futex(u32 *addr, int op, u32 val, const timespec *timeout, ...)
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            a as *const AtomicU32, // 要操作的 32 位原子地址
            FUTEX_WAIT,            // 操作
            expected,              // 期望值：不匹配则立刻返回、不入睡
            std::ptr::null::<libc::timespec>(), // 无超时
        );
    }
}

/// 唤醒最多 `n` 个正阻塞在 `a` 上的线程。
pub fn wake_n(a: &AtomicU32, n: i32) {
    unsafe {
        libc::syscall(libc::SYS_futex, a as *const AtomicU32, FUTEX_WAKE, n);
    }
}

/// 唤醒一个正阻塞在 `a` 上的线程。
pub fn wake_one(a: &AtomicU32) {
    wake_n(a, 1);
}

/// 唤醒所有正阻塞在 `a` 上的线程。
pub fn wake_all(a: &AtomicU32) {
    wake_n(a, i32::MAX);
}
