//! `forge-core` —— Forge 并发工具链的地基。
//!
//! 这里只放最底层的、不依赖任何其它 forge crate 的原语：
//!
//! - [`atomics`]：原子类型与内存序（模块 **M1**）
//! - [`spin`]：自建自旋锁（模块 **M3**）
//! - [`arc`]：自建 `Arc` / `Weak`（模块 **M4**）
//!
//! 这些原语之上长出了 `forge-sync`、`forge-channel`、`forge-lockfree`、
//! 最终汇成 `forge-pool`（同步线程池）与 `forge-rt`（异步运行时）。

/// 原子与内存序（模块 **M1**）。详见 `docs/modules/M1-atomics-and-ordering.md`。
pub mod atomics;

/// 自建自旋锁（模块 **M3**）。详见 `docs/modules/M3-spinlock.md`。
pub mod spin;

/// 自建 `Arc` / `Weak`（模块 **M4**）。详见 `docs/modules/M4-arc.md`。
pub mod arc;

/// 协作式取消令牌（模块 **M-cancel**）。详见 `docs/modules/M8-lockfree.md` 末尾。
pub mod cancel;
