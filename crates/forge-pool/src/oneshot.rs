//! # 线程安全的 oneshot 通道（专门给线程池的 `JoinHandle` 用）
//!
//! M5 那版 oneshot 的 `Receiver` 是 `!Send`（必须留在 split 的那个线程上）。
//! 线程池里任务在 worker 线程跑、结果要送回 spawn 调用方线程——调用方线程
//! 和 worker 线程不一定是同一个，所以我们需要一版**完全 `Send` 的** oneshot：
//! `Sender` 可以跨线程送（worker 把结果送回去），`Receiver` 也可以跨线程收
//! （调用方可能在任意线程上阻塞等）。
//!
//! 实现路线（教程里逐条对照）：
//! - 一块 `Arc` 共享的内层结构，含一个 `UnsafeCell<MaybeUninit<T>>` 和一个
//!   `AtomicU8` 状态字；
//! - 状态字三态：`EMPTY = 0`、`SENT = 1`、`CLOSED = 2`；
//! - "取走值"统一走 CAS `SENT → CLOSED`：拿到值的一方负责"把它读出来"，不再
//!   让 Drop 去碰 slot；
//! - 接收侧三种入口，共享同一个"取值"逻辑：
//!     * `try_recv(&self)`：非阻塞，没就绪返回 None；就绪则 CAS 取走；
//!     * `recv(self)`：阻塞，循环 `try_recv`，没拿到则 `wait`；
//!     * `Drop`：若状态是 SENT 则 CAS 取出来 drop（避免泄漏），再不需要 sleep。
//! - `send` 用 CAS `EMPTY → SENT`；失败说明对端已 CLOSED，把消息还回调用方。
//! - Sender 没 send 就 drop：`swap(CLOSED)` 并 `wake_one`，唤醒睡眠的 receiver。

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use atomic_wait::{wait, wake_one};

const EMPTY: u32 = 0;
const SENT: u32 = 1;
const CLOSED: u32 = 2;

struct Inner<T> {
    state: AtomicU32,
    slot: UnsafeCell<MaybeUninit<T>>,
}

// 安全性：T 从 sender 线程送到 receiver 线程，T: Send 即可。状态字 + CAS 保证
// 同一时刻只有一方碰 slot（取值方拿到 SENT→CLOSED 的 CAS 才能读 slot）。
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        state: AtomicU32::new(EMPTY),
        slot: UnsafeCell::new(MaybeUninit::uninit()),
    });
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

impl<T> Inner<T> {
    /// 内部 helper：尝试用 CAS 把状态从 SENT 翻成 CLOSED 并读出值。
    /// 成功返回 `Some(v)`；失败（状态不是 SENT）返回 None。
    ///
    /// AcqRel：成功的 CAS 同时承担"看到 sender 写入"（Acquire）和"对 Drop 之后的
    /// 任何读取可见"（Release）。
    fn take(&self) -> Option<T> {
        let ok = self
            .state
            .compare_exchange(SENT, CLOSED, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok();
        if ok {
            // 安全：状态是 SENT ⇒ slot 已被 sender 初始化。
            Some(unsafe { (*self.slot.get()).assume_init_read() })
        } else {
            None
        }
    }
}

impl<T> Sender<T> {
    /// 发送一条消息。消费 self ⇒ 只能发一次。
    /// 若接收端已 drop（状态为 CLOSED），返回 `Err(message)`，把消息还给你。
    pub fn send(self, message: T) -> Result<(), T> {
        // 安全：此刻 slot 还没人写过（EMPTY），且 receiver 要等状态变 SENT 才读。
        unsafe {
            (*self.inner.slot.get()).write(message);
        }
        // Release：让上面 write 对将来的 Acquire 可见。
        match self
            .inner
            .state
            .compare_exchange(EMPTY, SENT, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => {
                // 成功发布。叫醒一个可能在 sleep 的 receiver。
                wake_one(self.addr());
                std::mem::forget(self); // 不让 Drop 重复处理（Arc 引用靠 receiver 那份）
                Ok(())
            }
            Err(_) => {
                // 状态不是 EMPTY ⇒ 接收端已 drop。把消息拿回来还给调用方。
                let message = unsafe { (*self.inner.slot.get()).assume_init_read() };
                std::mem::forget(self);
                Err(message)
            }
        }
    }
}

impl<T> Sender<T> {
    fn addr(&self) -> *const AtomicU32 {
        &self.inner.state as *const _
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // 没 send 就 drop：把状态翻成 CLOSED，唤醒等待的 receiver。
        let prev = self.inner.state.swap(CLOSED, Ordering::Release);
        if prev == EMPTY {
            wake_one(self.addr());
        }
    }
}

impl<T> Receiver<T> {
    /// 非阻塞尝试接收。`Some(v)` 表示就绪；`None` 表示还没就绪（或已被取走/关闭）。
    pub fn try_recv(&self) -> Option<T> {
        // 仅当状态为 SENT 时才尝试 take。
        if self.inner.state.load(Ordering::Acquire) == SENT {
            self.inner.take()
        } else {
            None
        }
    }

    /// 阻塞地接收。消费 self ⇒ 只能收一次。
    ///
    /// 若 sender 在没 send 的情况下 drop（状态变 CLOSED），则 panic。
    pub fn recv(self) -> T {
        loop {
            if let Some(v) = self.try_recv() {
                // 不 forget(self)：让 Drop 跑，把 Arc 引用减 1。Drop 此刻看到 CLOSED，
                // 不会再做任何事（不会重复 take）。
                return v;
            }
            // 还没就绪。先看是不是 sender 已经放弃。
            let s = self.inner.state.load(Ordering::Acquire);
            if s == CLOSED {
                panic!("oneshot Receiver::recv: sender dropped without sending");
            }
            // s == EMPTY：等。wait 仅在 *addr == EMPTY 时睡，所以状态在等待期间被
            // 翻成 SENT/CLOSED 时，wake_one 会叫醒我们，循环重检。
            wait(&self.inner.state, EMPTY);
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // 若状态是 SENT（sender 已发但没收），负责把消息取出并 drop 避免泄漏。
        // take 会 CAS 把状态从 SENT 翻成 CLOSED，所以不会和 sender Drop 冲突。
        if self.inner.state.load(Ordering::Acquire) == SENT {
            if let Some(_v) = self.inner.take() {
                // _v 在这里被 drop。
            }
        }
        // 状态若已是 CLOSED（被 try_recv 或 send-after-drop 翻过），啥都不做。
        // 状态若是 EMPTY，啥也不做——slot 还没被写过。
    }
}
