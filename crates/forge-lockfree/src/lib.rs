//! `forge-lockfree` —— 第 10 章"灵感"的全部无锁结构，**从零实现**。
//!
//! 模块 **M8** 把 Mara Bos 第 10 章七大主题逐一建出（详见 `docs/modules/M8-lockfree.md`）：
//! - [`semaphore`]：信号量（也是"停止位=延迟初始化位=通道就绪位"的终极同构）
//! - [`rcu`]：Read-Copy-Update + Arc 回收
//! - [`stack`]：Treiber 无锁栈（含 ABA 问题，教学版故意泄漏）
//! - [`queue`]：Michael-Scott MPMC 无锁队列（完整实现，两步 CAS + helping）
//! - [`hazard`]：Hazard pointer 回收（ABA 解法 3 的真实代码版）
//! - [`epoch`]：Epoch-based reclamation（两 epoch 窗口批量回收）
//! - [`mcs`]：MCS 队列锁
//! - [`clh`]：CLH 队列锁（与 MCS 对照）
//! - [`parking_lot`]：parking-lot 式锁（全局"停车场" HashMap）
//! - [`seqlock`]：序列锁
//! - [`deque`]：Chase-Lev 工作窃取双端队列（调度器的心脏）
//! - [`latch`]：倒计数门闩（Latch）+ 可重用屏障（Barrier）（模块 **M8h**）

/// CLH 队列锁，与 MCS 对照（spin 在前驱节点上）。M8d 章节末尾。
pub mod clh;
pub mod deque;
/// Epoch-based reclamation（两 epoch 窗口批量回收）。M8b ISO·ZOOM 节的真实代码版。
pub mod epoch;
/// Hazard pointer 回收（公告栏 + SeqCst fence）。M8c 章节配 stack.rs 的 ABA 解法 3。
pub mod hazard;
pub mod latch;
pub mod mcs;
pub mod parking_lot;
/// Michael-Scott MPMC 无锁 FIFO。见 docs/modules/M8-lockfree.md M8c 节末。
pub mod queue;
pub mod rcu;
pub mod semaphore;
pub mod seqlock;
pub mod stack;
