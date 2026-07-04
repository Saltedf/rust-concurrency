//! # Task —— 一个被异步执行器调度的"有身份"的 Future。
//!
//! 一个 `Task` 把下面三样东西打包成 `Arc`：
//! - **future**：用户 `spawn` 进来的那个 `async` 块 / `Future`（类型擦除成
//!   `Pin<Box<dyn Future<Output = ()>>>`）；
//! - **schedule**：把"这个 task 重新入队"的回调（执行器提供，被 Waker 调用）。
//!
//! 类型擦除的代价：spawn 的 future 类型 `F` 在 `spawn` 里被装箱成
//! `Pin<Box<dyn Future<Output = ()>>>`，所以队列里所有 task 都是同一种类型
//! `Arc<Task>`。结果通过外置的 oneshot 通道送回 [`JoinHandle`](crate::JoinHandle)。
//!
//! 为什么是 `Arc<...>` 而不是 `Box`？因为 task 同时被两处持有：
//! 1. 执行器的就绪队列里有一份（准备 poll）；
//! 2. Waker 里持有一份（被 reactor / 别的 task clone 出来再 wake）。
//! 只有 `Arc` 能让这两处安全共享，并在最后一个引用消失时自动 drop。
//!
//! 教程第五章逐拍画过 task 的三种状态（queued / polled / waiting-in-reactor）。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use crate::oneshot;

const IDLE: u8 = 0;
const QUEUED: u8 = 1;
const RUNNING: u8 = 2;

/// 类型擦除后的 future。spawn 时把用户的 future 包一层：跑完结果送 oneshot。
type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// 一个被调度的任务。所有 spawn 出来的任务统一是这个类型——靠 dyn Future 擦除
/// 具体的 future 类型，结果通过外置 oneshot 送回。
pub(crate) struct Task {
    /// 状态机：IDLE / QUEUED / RUNNING。去重 wake、防止"poll 期间 wake 丢失"。
    state: AtomicU8,
    /// 被钉住的 future。Pin<Box<...>> 保证它的地址从此固定，自引用安全。
    /// Mutex<Option<...>> 是因为 poll 期间要 take 出来独占（不持锁 poll）。
    future: std::sync::Mutex<Option<BoxFuture>>,
    /// 重新入队的回调。被 [`Wake`] 触发：`state IDLE → QUEUED` 时调用。
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
}

impl Task {
    /// 构造一个 task：把用户的 future 包成"跑完送结果"的 wrapper。
    pub(crate) fn spawn<F, T>(
        future: F,
        schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
        sender: oneshot::Sender<T>,
    ) -> Arc<Task>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // 用一个 wrapper future 把 F::Output 转成 ()，并送 oneshot。
        let wrapped: BoxFuture = Box::pin(async move {
            let value = future.await;
            let _ = sender.send(value);
        });
        Arc::new(Task {
            state: AtomicU8::new(QUEUED),
            future: std::sync::Mutex::new(Some(wrapped)),
            schedule,
        })
    }

    /// 从这个 task 派生一个 `Waker`。同一个 task 永远派生出"行为等价"的 waker。
    pub(crate) fn waker(self: &Arc<Task>) -> Waker {
        Waker::from(self.clone())
    }

    /// 跑一次 poll。
    ///
    /// 1. CAS QUEUED → RUNNING（防止 poll 期间 wake 漏掉）；
    /// 2. 从 Mutex 取出 future 独占 poll（不持锁）；
    /// 3. Pending ⇒ 放回 future；CAS RUNNING → IDLE，失败说明 poll 期间被 wake
    ///    写成了 QUEUED，需要重新入队。
    pub(crate) fn poll(self: Arc<Task>, waker: &Waker) {
        // CAS QUEUED → RUNNING。
        if self
            .state
            .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        // 取出 future 独占 poll（不持锁）。
        let mut guard = self.future.lock().unwrap();
        let mut future = match guard.take() {
            Some(f) => f,
            None => {
                drop(guard);
                self.state.store(IDLE, Ordering::Release);
                return;
            }
        };
        drop(guard);

        let mut cx = Context::from_waker(waker);
        // Pin<Box<dyn Future>> → Pin<&mut dyn Future> 才能 poll。
        let result = future.as_mut().poll(&mut cx);

        match result {
            Poll::Ready(()) => {
                // 完成。drop future，state 写 IDLE（不再可调度）。
                drop(future);
                self.state.store(IDLE, Ordering::Release);
            }
            Poll::Pending => {
                // 放回 future。
                *self.future.lock().unwrap() = Some(future);
                // CAS RUNNING → IDLE。失败说明 poll 期间被 wake 写成了 QUEUED。
                if self
                    .state
                    .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
                {
                    // 状态是 QUEUED。把它写回 IDLE 并重新入队。
                    self.state.store(IDLE, Ordering::Release);
                    let schedule = self.schedule.clone();
                    schedule(self);
                }
            }
        }
    }
}

/// `Task` 实现了 `Wake` ⇒ 可以从 `Arc<Task>` 派生 `Waker::from(arc)`。
/// `wake()` / `wake_by_ref()` 通过状态机去重 wake：只有 IDLE → QUEUED 才真入队。
impl Wake for Task {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // 一次 swap 实现"任意状态 → QUEUED，但只在之前是 IDLE 时入队"。
        // 因为 wake 可能在 RUNNING（poll 中）状态下被调用——此时不能立刻入队
        // （会和 worker 的 poll 撞车），poll 里的 CAS 会兜底重新入队。
        let prev = self.state.swap(QUEUED, Ordering::AcqRel);
        if prev == IDLE {
            (self.schedule)(self.clone());
        }
    }
}
