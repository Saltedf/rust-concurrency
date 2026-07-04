//! `forge-channel` —— 自建通道：one-shot 与 mpsc。
//!
//! 模块 **M5** 跟着 Mara Bos 第 5 章，把一个 one-shot 通道经历六个版本演化，
//! 最终得到 [`oneshot`]（借用 + 阻塞版）。[`mpsc`] 是第 5 章开头那个
//! `Mutex + Condvar` 通用通道的成品化（可多生产者）。
//! 它们将成为运行时的 `JoinHandle`（M9）与爬虫的结果汇聚（M10）。

/// 一次性通道（one-shot）：只能发一条、收一条。详见 `docs/modules/M5-channels.md`。
pub mod oneshot;

/// 多生产者单消费者通道（mpsc），无界、阻塞接收。
pub mod mpsc;
