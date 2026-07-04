//! V1：所有 worker 共享一个 `Mutex<VecDeque<Task>> + Condvar` 的池。
//!
//! 这是敌人。它是 Williams Listing 9.1/9.2 的直译，也是绝大多数"教程线程池"
//! 的样子。它**正确**，但在多核下退化成 ~1 核吞吐：所有 worker 抢同一把锁。
//!
//! 我们在测试 `tests/m9a_*.rs` 里拿它做对照基准：让它和工作窃取 V3 比谁吞吐高。
//! 它输得很难看——这就是为什么需要 V2/V3。

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::oneshot;
use crate::Task;

/// V1 池的外壳。`Drop` 时让所有 worker 退出。
pub struct SharedQueuePool {
    shared: Arc<Shared>,
    workers: Vec<thread::JoinHandle<()>>,
}

struct Shared {
    queue: Mutex<State>,
    cv: Condvar,
}

struct State {
    tasks: VecDeque<Task>,
    shutdown: bool,
}

impl SharedQueuePool {
    pub fn new(n_workers: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(State {
                tasks: VecDeque::new(),
                shutdown: false,
            }),
            cv: Condvar::new(),
        });
        let mut workers = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let shared = shared.clone();
            workers.push(thread::spawn(move || worker_loop(shared)));
        }
        Self { shared, workers }
    }

    /// 投递一个任务，返回可阻塞接收结果的 `JoinHandle`。
    pub fn spawn<F, T>(&self, f: F) -> crate::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let task = Task(Box::new(move || {
            let value = f();
            // 任务跑完后把结果送回 oneshot。若接收端已 drop，结果随之 drop。
            let _ = sender.send(value);
        }));
        let mut s = self.shared.queue.lock().unwrap();
        s.tasks.push_back(task);
        self.shared.cv.notify_one();
        crate::JoinHandle { receiver }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        // 优雅关停：shutdown=true 时不再接新任务，但已有的全部跑完。
        let task = {
            let mut s = shared.queue.lock().unwrap();
            loop {
                if let Some(task) = s.tasks.pop_front() {
                    break Some(task);
                }
                if s.shutdown {
                    break None;
                }
                // 等到队列里有任务、或收到关停信号。
                s = shared.cv.wait(s).unwrap();
            }
        };
        match task {
            Some(t) => t.run(),
            None => return,
        }
    }
}

impl Drop for SharedQueuePool {
    fn drop(&mut self) {
        {
            let mut s = self.shared.queue.lock().unwrap();
            s.shutdown = true;
            self.shared.cv.notify_all();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
