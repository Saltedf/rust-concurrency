//! # mpsc 通道（多生产者、单消费者，无界、阻塞）
//!
//! 这是原书第 5 章开头那个"`Mutex<VecDeque> + Condvar`"通道的成品化：
//! `Sender` 可克隆（多生产者）、`Receiver` 唯一（单消费者）、`recv` 阻塞。
//! 它是 M9 `JoinHandle` 投递结果、M10 爬虫汇聚结果的工具。

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

struct Shared<T> {
    queue: Mutex<VecDeque<T>>,
    not_empty: Condvar,
}

/// 创建一个无界 mpsc 通道，返回 `(Sender, Receiver)`。Sender 可克隆。
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        not_empty: Condvar::new(),
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    )
}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// 投递一条消息（立即返回，无界队列不会满）。唤醒一个等待的接收者。
    pub fn send(&self, message: T) {
        self.shared.queue.lock().unwrap().push_back(message);
        self.shared.not_empty.notify_one();
    }
}

impl<T> Receiver<T> {
    /// 阻塞地接收一条消息。必须循环以消化假唤醒。
    pub fn recv(&self) -> T {
        let mut guard = self.shared.queue.lock().unwrap();
        loop {
            if let Some(m) = guard.pop_front() {
                return m;
            }
            guard = self.shared.not_empty.wait(guard).unwrap();
        }
    }
}

// 多生产者：Sender 可克隆
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            shared: self.shared.clone(),
        }
    }
}
