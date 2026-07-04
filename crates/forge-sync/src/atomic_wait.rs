//! # M6：跨平台"地址等待"原语 —— futex 的统一抽象
//!
//! 三个操作，全部围绕一个 [`AtomicU32`]：
//! - [`wait`]：若原子**此刻仍等于 `expected`**，把当前线程睡进内核，直到被 wake（或假唤醒）；
//! - [`wake_one`]：唤醒一个（若有）睡在该原子上的线程；
//! - [`wake_all`]：唤醒所有睡在该原子上的线程。
//!
//! 在 Linux 上这是 `futex` syscall；在 macOS 上是相关机制；在 Windows 8+ 上是
//! `WaitOnAddress`/`WakeByAddress*`。这里委托 [`atomic_wait`] crate 给出统一抽象，
//! 内核细节见同模块的 [`crate::linux_futex`]（Linux 手写版）。
//!
//! ## 为什么这套设计能让"唤醒不丢失"？
//!
//! 关键不变量：**"检查 expected" 和 "入睡" 是一条原子操作**（对其它 futex 操作而言）。
//! 于是 `wake` 永远不可能卡在"wait 已检查、还没睡"的缝隙里丢失。配合约定
//! **"先把原子值改掉，再 wake"**，就能保证：要么等待者还没开始 wait（那它后续 load
//! 会看到新值、根本不睡），要么它已在睡（那 wake 会叫醒它）。**非竞争路径完全不需要 syscall。**
//!
//! ## 必须循环！
//! `wait` 可能**假唤醒**（没人 wake 也自己醒）。所以永远这样用：
//! ```ignore
//! while !condition() { wait(&a, expected); }   // 醒来重新检查条件
//! ```

use std::sync::atomic::AtomicU32;

/// 若 `a` 此刻仍等于 `expected`，则阻塞当前线程，直到被 [`wake_one`]/[`wake_all`] 唤醒
/// 或假唤醒。**不保证返回时值没变**——所以必须在循环里用、醒来重检条件。
#[inline]
pub fn wait(a: &AtomicU32, expected: u32) {
    atomic_wait::wait(a, expected);
}

/// 唤醒一个（若有）正阻塞在 `a` 上的线程。
#[inline]
pub fn wake_one(a: &AtomicU32) {
    atomic_wait::wake_one(a);
}

/// 唤醒所有正阻塞在 `a` 上的线程。
#[inline]
pub fn wake_all(a: &AtomicU32) {
    atomic_wait::wake_all(a);
}
