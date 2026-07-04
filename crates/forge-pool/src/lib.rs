//! `forge-pool` —— 顶峰#1：同步工作窃取线程池（`rayon` 的脊梁）。
//!
//! 模块 **M9a**。它复用了 [`forge_lockfree`] 的 Chase-Lev 双端队列（做每 worker
//! 本地队列），自研了一个**线程安全的** oneshot（M5 oneshot 的 Receiver 是
//! `!Send`，不能跨线程传结果，所以这里另写一版）。
//!
//! 三步演化：
//! 1. [`v1_shared_queue`] —— 所有 worker 抢同一把 `Mutex<VecDeque>`。**敌人**。
//! 2. （V2 仅是 V3 去掉偷窃，教程里讲，代码并入 V3。）
//! 3. [`v3_stealing`] —— 每 worker 一个本地 Chase-Lev 队列，工作窃取，等待时跑
//!    别的任务，能撑住嵌套 spawn。
//!
//! 这是一个**完整、可交付**的终点：即便不做异步，你也有了一个真实的运行时。
//!
//! 详见 `docs/modules/M9a-thread-pool.md`。

pub mod oneshot;
pub mod par;
pub mod v1_shared_queue;
pub mod v3_stealing;

pub use v1_shared_queue::SharedQueuePool;
pub use v3_stealing::StealingPool;

use crate::oneshot::Receiver;

/// "一个能跑的闭包"，类型擦除成 trait object。newtype 让我们给它写方法（类型别名
/// 不行，因为 orphan 规则禁止给外部 crate 的 `Box<...>` 加 inherent method）。
pub struct Task(pub(crate) Box<dyn FnOnceOnce + Send + 'static>);

/// 把任意 `FnOnce()` 装箱后的统一接口。`call_once_boxed` 消费 `Box<Self>`。
pub trait FnOnceOnce {
    fn call_once_boxed(self: Box<Self>);
}

impl<F: FnOnce()> FnOnceOnce for F {
    fn call_once_boxed(self: Box<Self>) {
        (*self)();
    }
}

impl Task {
    /// 跑这个任务（消费 self）。
    pub(crate) fn run(self) {
        self.0.call_once_boxed();
    }
}

/// `pool.spawn(|| 42)` 返回这个。调 [`recv`](Self::recv) 阻塞等结果。
///
/// **关键设计**：`recv` 在 worker 线程上被调用时**绝不 park**——它会循环
/// "检查结果 → 找一个任务跑 → 再检查结果"。理由：如果任务 A 在 W1 上跑、A
/// spawn 了 B 然后 recv 等 B，而 B 又被调度到 W1 上（worker 都在干活、没人偷 B），
/// 那么 W1 一旦 park 就再也醒不过来——B 永远不会被运行，A 也永远不会等到结果。
/// 这是 rayon/tokio 等运行时"阻塞调度"的核心思想。
pub struct JoinHandle<T> {
    pub(crate) receiver: Receiver<T>,
}

impl<T> JoinHandle<T> {
    /// 阻塞等结果。
    ///
    /// - 在 worker 线程上：**继续跑任务**直到结果就绪。
    /// - 在外部线程上：让操作系统 park 我们（外部线程不在任务图里，park 安全）。
    pub fn recv(self) -> T {
        // 路径 A：当前是 worker 线程——必须持续干活，不能 park。
        let on_worker = v3_stealing::is_on_worker();
        if on_worker {
            loop {
                // 1) 结果就绪？
                if let Some(v) = self.receiver.try_recv() {
                    return v;
                }
                // 2) 没就绪——跑一个挂起的任务（本地 LIFO → 注入 → 偷别人）。
                //    这一步是"阻塞时帮忙"的核心。
                if !v3_stealing::run_one_pending_task() {
                    // 连任务都没有：让出 CPU 一个时间片，避免死循环烧核。
                    // 不能 park（park 会让本 worker 永远睡死）。
                    std::thread::yield_now();
                }
            }
        } else {
            // 路径 B：外部线程——直接 park 等结果，安全。
            self.receiver.recv()
        }
    }
}
