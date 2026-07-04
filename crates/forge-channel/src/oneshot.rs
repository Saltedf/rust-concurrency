//! # one-shot 通道（M5 的主角，原书第 5 章最终版）
//!
//! 一个 [`Channel`] 只能发**一条**消息、收**一条**消息。它经历六版演化
//! （unsafe 雏形 → 运行时检查 → 类型级保证 → 借用免分配 → 阻塞接收），
//! 最终得到这里这版：**借用 Channel（无 Arc 分配）+ 阻塞 receive**。
//!
//! 关键设计点（教程里逐条讲）：
//! - `MaybeUninit<T>` 当"可能没初始化"的槽位（比 `Option<T>` 省）；
//! - `unsafe impl Sync where T: Send`；
//! - `Sender`/`Receiver` 各自**消费 self**，保证 send/receive 各只一次（编译期）；
//! - `Channel::split(&mut self)` 把一个独占借用拆成两个共享借用；
//! - `Sender` 持有接收线程句柄，send 后 `unpark` 唤醒；`Receiver` 用 `PhantomData<*const()>`
//!   让自己 `!Send`（必须留在调用 split 的线程上，否则唤醒错线程）；
//! - `Drop`：若消息发了没收，负责 drop 它（不泄漏）。

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::Thread;

pub struct Channel<T> {
    message: UnsafeCell<MaybeUninit<T>>,
    ready: AtomicBool,
}

// 安全性：通道把 T 从发送线程"送"到接收线程，所以要 T: Send。
// 同一时刻只有一方碰 message（靠 ready + 消费 self 保证），不需要 T: Sync。
unsafe impl<T: Send> Sync for Channel<T> {}

impl<T> Channel<T> {
    pub const fn new() -> Self {
        Self {
            message: UnsafeCell::new(MaybeUninit::uninit()),
            ready: AtomicBool::new(false),
        }
    }

    /// 把一个独占借用拆成 `(Sender, Receiver)`。两者都在时，Channel 被借住、不能再用；
    /// 两者都没了，才能再次 split。每次 split 会把 Channel 重置为空（并 drop 上次未读的消息）。
    pub fn split(&mut self) -> (Sender<'_, T>, Receiver<'_, T>) {
        *self = Self::new();
        (
            Sender {
                channel: self,
                receiving_thread: std::thread::current(),
            },
            Receiver {
                channel: self,
                _no_send: PhantomData,
            },
        )
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Sender<'a, T> {
    channel: &'a Channel<T>,
    receiving_thread: Thread,
}

pub struct Receiver<'a, T> {
    channel: &'a Channel<T>,
    // 让 Receiver 不是 Send：它必须留在调用 split 的线程（接收线程）上，
    // 否则 Sender 里存的 receiving_thread 就指向了错误的线程。
    _no_send: PhantomData<*const ()>,
}

impl<T> Sender<'_, T> {
    /// 发送一条消息（消费 self，所以只能发一次）。发完唤醒可能正在等的接收者。
    pub fn send(self, message: T) {
        // 安全：此刻只有我们碰 message（Receiver 要等 ready=true 才读）。
        unsafe {
            (*self.channel.message.get()).write(message);
        }
        // Release：把上面 write 的消息"发布"给接收者的 Acquire。
        self.channel.ready.store(true, Ordering::Release);
        self.receiving_thread.unpark();
    }
}

impl<T> Receiver<'_, T> {
    /// 是否已有消息可收（仅指示用，Relaxed 足够）。
    pub fn is_ready(&self) -> bool {
        self.channel.ready.load(Ordering::Relaxed)
    }

    /// 阻塞地接收消息（消费 self，所以只能收一次）。
    /// 必须循环：park 可能有假唤醒，醒来要重新检查 ready。
    pub fn receive(self) -> T {
        while !self.channel.ready.swap(false, Ordering::Acquire) {
            std::thread::park();
        }
        // 安全：ready 已为 true（我们刚把它换回 false），message 已初始化。
        unsafe { (*self.channel.message.get()).assume_init_read() }
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        // drop 时独占（&mut self），无需原子。若消息发了没收，负责 drop 它。
        if *self.ready.get_mut() {
            unsafe { self.message.get_mut().assume_init_drop() }
        }
    }
}
