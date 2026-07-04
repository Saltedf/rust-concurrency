//! `forge-rt` —— 顶峰#2：异步执行器 + mio Reactor（`tokio` 的雏形）。
//!
//! 模块 **M9b**。它把 M9a 的同步线程池当作 worker 循环**结构上原样复用**
//! （每 worker 一个本地 LIFO 队列 + 工作窃取 + 等待时帮忙跑别的任务），把
//! "闭包任务"换成"Task = `Arc<dyn Future>`"，再接上一个最小的 I/O Reactor
//! （mio 包 epoll/kqueue/IOCP）。任务是 `Arc<Task>`（教程第五章），唤醒靠
//! `std::task::Waker`（教程第三章）。
//!
//! 详见 `docs/modules/M9b-async-runtime.md`。

pub mod combinators;
pub mod coroutine;
pub mod executor;
pub mod reactor;
pub mod task;

// 教学用的"裸 oneshot"——forge-pool::oneshot 已经够用，重导出方便。
pub use executor::{block_on, JoinHandle, Runtime};
pub use forge_pool::oneshot;
pub use reactor::Reactor;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::reactor::TimerRegistration;

/// 一个"等到 `deadline` 就绪"的 future。教程四、七章用它做"手算 poll 推进"。
///
/// 它的状态机只有两态：`未到期`（每次 poll 都注册一次 timer + 返回 Pending）、
/// `已到期`（直接返回 Ready）。注册 timer 时 reactor 会拿到一个 Waker；reactor
/// 线程在 deadline 到时调 `wake()`，执行器把 task 重新入队，下一轮 poll 看到
/// 已过 deadline，返回 Ready。
pub struct Delay {
    deadline: Instant,
    /// 当前注册的 timer 句柄。drop 自动反注册。每次 poll 重新装填。
    registration: Option<TimerRegistration>,
    /// reactor 引用。clone 一份，方便 poll 时注册 timer。
    reactor: Reactor,
}

impl Delay {
    pub fn new(reactor: Reactor, after: Duration) -> Self {
        Self {
            deadline: Instant::now() + after,
            registration: None,
            reactor,
        }
    }
}

impl Future for Delay {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            return Poll::Ready(());
        }
        // 还没到。注册（覆盖旧 registration）：让 reactor 在 deadline 时 wake 我们。
        // 每次 poll 都重新注册是因为 deadline 可能"被改"（虽然我们这版不支持），
        // 也是为了演示"future 和 reactor 通过 Waker 协作"的最小模式。
        self.registration.take(); // drop 旧的，反注册。
        let reg = self
            .reactor
            .register_timer(self.deadline, cx.waker().clone());
        self.registration = Some(reg);
        Poll::Pending
    }
}

// =========================================================================
// select! —— 两 future 谁先 Ready 谁赢，另一个 drop
// =========================================================================

/// `select` 的返回值：要么左边赢了（带左结果 + 右 future），要么右边赢了。
pub enum SelectOutput<A: Future, B: Future> {
    /// A 先 Ready。带 A 的结果 + 没跑完的 B（**调用方决定 drop 还是接着跑**）。
    Left(<A as Future>::Output, B),
    /// B 先 Ready。带 B 的结果 + 没跑完的 A。
    Right(<B as Future>::Output, A),
}

/// 等两个 future 中**任意一个**完成。返回 [`SelectOutput`]。
///
/// 教程第九章逐拍画过：F1 先 Ready → 返回 `Left(v, F2)`，**F2 必须被显式 drop**
/// （这里通过返回值把 F2 还给调用方，由调用方决定怎么清理——这是 select 的核心
/// 安全保证：输掉的 future 不会偷偷泄漏它持有的资源）。
pub fn select<A, B>(mut a: A, mut b: B) -> SelectOutput<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    // 一个简化的"伪 Context"：用空 waker 反复 poll 直到有一个 Ready。
    // 真实 select! 用 [`poll_once`] 风格让调用方决定 waker，但教学版用循环跑。
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = Pin::new(&mut a).poll(&mut cx) {
            return SelectOutput::Left(v, b);
        }
        if let Poll::Ready(v) = Pin::new(&mut b).poll(&mut cx) {
            return SelectOutput::Right(v, a);
        }
        // 这里我们让出 CPU 一个时间片；真实 select! 在 async 上下文里走"两个
        // future 都返回 Pending 时 yield 回执行器"，但当前 select 是同步函数
        // （在 poll 内部用），所以只能 spin。
        std::thread::yield_now();
    }
}

/// 造一个"什么都不做"的 Waker——它被 wake 时不调任何代码。
/// 教学用：在 select / 手算场景里不需要真调度。
pub fn noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable, Waker};

    unsafe fn no_op(_: *const ()) {}
    unsafe fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
