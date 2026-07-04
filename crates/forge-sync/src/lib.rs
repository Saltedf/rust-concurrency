//! `forge-sync` —— 同步原语：从标准库封装到自建 futex 锁。
//!
//! - [`std_locks`]：标准库 `Mutex`/`RwLock`/`Condvar`（模块 **M2**）
//! - [`atomic_wait`]：跨平台"地址等待"原语（模块 **M6**）
//! - [`linux_futex`]：Linux 上手写的 futex syscall（模块 **M6**）
//! - [`mutex`] / [`condvar`] / [`rwlock`]：基于 atomic-wait 自建的真锁（模块 **M7**）

/// 标准库同步原语（模块 **M2**）。
pub mod std_locks;

/// 跨平台"地址等待"原语（模块 **M6**）。
pub mod atomic_wait;

/// Linux 上手写的 futex syscall（模块 **M6**）。
#[cfg(target_os = "linux")]
pub mod linux_futex;

/// 自建 futex `Mutex`（模块 **M7**）。
pub mod mutex;

/// 自建 futex `Condvar`（模块 **M7**）。
pub mod condvar;

/// 自建写公平 `RwLock`（模块 **M7**）。
pub mod rwlock;
