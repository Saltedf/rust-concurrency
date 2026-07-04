//! # 模块 M2：用标准库原语搭出"工作者等工作"的循环
//!
//! 这一章我们不发明新东西，而是**用好**标准库已有的 `Mutex` / `RwLock` /
//! `Condvar` / `thread::park`。它们的终点产物是下方的 [`TaskQueue`]——
//! 一个"生产者 push、工作者用条件变量阻塞等待并 pop"的线程安全队列。
//! 它就是 M9a 工作窃取线程池的**种子**（单队列、Condvar 驱动）。
//!
//! 教程把每个原语为什么被发明、怎么用、有什么坑都讲透；这里只放最终代码。
//! 详见 `docs/modules/M2-sharing-and-locking.md`。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// 一个线程安全的 FIFO 任务队列：生产者 [`push`](Self::push)，
/// 工作者用 [`pop_blocking`](Self::pop_blocking) 阻塞地取。
///
/// 它示范了 M2 最重要的模式——**用 `Mutex` 保护数据、用 `Condvar` 让等待者睡到条件成立**。
/// 这正是未来调度器里"工作者等工作"的循环。
pub struct TaskQueue<T> {
    queue: Mutex<VecDeque<T>>,
    /// "队列非空"这个条件。等待者睡在这上面，生产者 push 后 `notify_one` 唤醒一个。
    not_empty: Condvar,
}

impl<T> TaskQueue<T> {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            not_empty: Condvar::new(),
        }
    }

    /// 投入一个任务，并唤醒**一个**正在等待的工作者。
    pub fn push(&self, task: T) {
        // 注意：先 push 再 notify。guard 在这条语句结束时 drop，释放锁。
        self.queue.lock().unwrap().push_back(task);
        self.not_empty.notify_one();
    }

    /// 阻塞地取出一个任务；队列为空就睡，直到被通知。
    ///
    /// **必须循环**：`Condvar::wait` 可能有**假唤醒**（spurious wakeup）——
    /// 没人通知它也可能醒。所以醒来后必须重新检查"队列非空"这个条件，
    /// 不成立就继续等。这是条件变量使用的铁律。
    pub fn pop_blocking(&self) -> T {
        let mut guard = self.queue.lock().unwrap();
        loop {
            if let Some(task) = guard.pop_front() {
                return task;
            }
            // wait 原子地"解锁 + 睡"，醒来后重新加锁并返回新的 guard。
            // 这一步是关键：它消除了"解锁"和"开始等"之间的缝隙，通知不会丢。
            guard = self.not_empty.wait(guard).unwrap();
        }
    }

    /// 非阻塞地尝试取一个；没有就立刻返回 `None`。
    pub fn try_pop(&self) -> Option<T> {
        self.queue.lock().unwrap().pop_front()
    }
}

impl<T> Default for TaskQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}
