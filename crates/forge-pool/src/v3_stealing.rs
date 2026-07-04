//! V3：每 worker 一个本地队列 + 工作窃取 + **等待时跑别的任务**。
//!
//! 这是本模块的终点，对应 Williams Ch9 Listing 9.7 + 9.8，外加一个 Ch9 文字
//! 提到但代码没展开的关键点——**等待时跑别的任务**（"running other tasks from
//! the queue while waiting for a subtask"），即 `recv()` 阻塞期间，当前 worker
//! 不能真睡，必须继续从自己/别人队列拉任务跑，否则嵌套 spawn 会死锁。
//!
//! 设计：
//! - `Worker` = 一个 OS 线程，跑 `worker_loop`；
//! - 每个 worker 拥有一个 `Arc<LocalQueue>`（Mutex<VecDeque> 包一层；owner 用 LIFO
//!   端 push/pop，thief 用 FIFO 端偷，参考 Williams Listing 9.7）；
//! - 外部线程投递任务时，走一个"注入器"（`Vec<Mutex<VecDeque>>`，每 worker 一个
//!   槽），由对应 worker（或别的 worker 兜底）在本地空时来抓（FIFO）；
//! - spawn 在 worker 内部被调用时，push 到**当前 worker 的本地队列**（thread_local
//!   缓存 `WorkerHandle`）；
//! - 关停信号 + 等待机制用 `Condvar`（保证不会丢唤醒，比 atomic-wait 的 futex 路径
//!   在多测试并行场景下更稳定；原子版作为优化在教程里讨论）；
//! - `JoinHandle::recv()`（在 lib.rs）会自检"我是否在 worker 线程上"：
//!     - 是 → 进入"while 没收到，跑一个挂起的任务"循环，**绝不 park**；
//!     - 否 → 普通 park 等待（外部线程允许睡）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, Thread};

use crate::oneshot;
use crate::Task;

/// 一份 worker 的本地队列：被 owner（worker 自己）和其他 worker（thief）共享访问。
/// owner 用 LIFO 端（front）：push_front / pop_front；thief 用 FIFO 端（back）：
/// pop_back。这种不对称设计是工作窃取的核心，详见教程。
struct LocalQueue {
    inner: Mutex<VecDeque<Task>>,
}

impl LocalQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
        }
    }
    /// owner：从 LIFO 端（front）压入。
    fn push(&self, t: Task) {
        self.inner.lock().unwrap().push_front(t);
    }
    /// owner：从 LIFO 端（front）弹出。
    fn pop(&self) -> Option<Task> {
        self.inner.lock().unwrap().pop_front()
    }
    /// thief：从 FIFO 端（back）偷。
    fn steal(&self) -> Option<Task> {
        self.inner.lock().unwrap().pop_back()
    }
}

/// 给 worker 线程用的本地句柄。thread_local 缓存，让 spawn 知道 push 到哪个队列。
struct WorkerHandle {
    /// 本地队列（被所有 worker 共享，但 owner 用 LIFO 端、thief 用 FIFO 端）。
    local: Arc<LocalQueue>,
    /// 所有 worker 的本地队列（含自己的），用于偷。
    queues: Vec<Arc<LocalQueue>>,
    /// 自己在 workers 数组里的下标。
    index: usize,
    /// 反向指针到全局状态。
    pool_state: Arc<PoolState>,
}

thread_local! {
    static WORKER: std::cell::RefCell<Option<WorkerHandle>> = std::cell::RefCell::new(None);
}

/// 全局唤醒状态：一个计数器记录"还有多少已发布任务未被取走"+ 一个 condvar。
/// 每次 spawn 之后 fetch_add(1) 并 notify_all；worker 取到一个任务后 fetch_sub(1)。
/// worker_loop 在"没活干"时 wait 在这个 condvar 上，绝不丢唤醒。
struct PoolState {
    shutdown: AtomicBool,
    /// 外部线程喂进来的任务，每 worker 一个锁保护队列。
    injector: Vec<Mutex<VecDeque<Task>>>,
    /// 用于生成"下一个接收外部任务"的 worker 下标（轮询）。
    next_external: AtomicUsize,
    /// 所有 worker 的 Thread 句柄，用于 Drop 时 unpark。
    worker_threads: Mutex<Vec<Thread>>,
    n_workers: usize,
    /// 待处理任务计数（外部 inject + worker 内 spawn 的总和）。worker 取一个减一。
    /// 与 notify_count 配合 Condvar 保证不丢唤醒。
    pending: Mutex<usize>,
    pending_cv: Condvar,
}

pub struct StealingPool {
    state: Arc<PoolState>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl StealingPool {
    pub fn new(n_workers: usize) -> Self {
        let queues: Vec<Arc<LocalQueue>> = (0..n_workers)
            .map(|_| Arc::new(LocalQueue::new()))
            .collect();

        let state = Arc::new(PoolState {
            shutdown: AtomicBool::new(false),
            injector: (0..n_workers).map(|_| Mutex::new(VecDeque::new())).collect(),
            next_external: AtomicUsize::new(0),
            worker_threads: Mutex::new(Vec::with_capacity(n_workers)),
            n_workers,
            pending: Mutex::new(0),
            pending_cv: Condvar::new(),
        });

        let mut workers = Vec::with_capacity(n_workers);
        for index in 0..n_workers {
            let local = queues[index].clone();
            let queues_clone = queues.clone();
            let st = state.clone();
            let handle = thread::Builder::new()
                .name(format!("forge-worker-{index}"))
                .spawn(move || {
                    WORKER.with(|w| {
                        *w.borrow_mut() = Some(WorkerHandle {
                            local,
                            queues: queues_clone,
                            index,
                            pool_state: st.clone(),
                        });
                    });
                    worker_loop(st);
                })
                .expect("spawn worker");
            workers.push(handle);
        }
        {
            let mut v = state.worker_threads.lock().unwrap();
            for w in &workers {
                v.push(w.thread().clone());
            }
        }
        Self { state, workers }
    }

    /// 投递任务，返回 `JoinHandle`。
    pub fn spawn<F, T>(&self, f: F) -> crate::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut task_opt = Some(Task(Box::new(move || {
            let value = f();
            let _ = sender.send(value);
        })));

        let pushed_to_local = WORKER.with(|w| {
            if let Some(h) = w.borrow().as_ref() {
                if let Some(t) = task_opt.take() {
                    h.local.push(t);
                    return true;
                }
            }
            false
        });

        if !pushed_to_local {
            // 外部线程：注入到某个 worker 的 injector 槽（轮询）。
            let n = self.state.n_workers;
            let idx = self.state.next_external.fetch_add(1, Ordering::Relaxed) % n;
            let task = task_opt
                .take()
                .expect("task_opt should still be Some when not pushed to local");
            self.state.injector[idx].lock().unwrap().push_back(task);
        }

        // 不管是 push 到本地还是 inject 到外部槽，都增加"待处理"计数并通知 worker。
        // 即便是 worker 内 spawn 的（push 到本地），也要通知——别的 worker 可能
        // 在 wait，新任务可以偷。
        {
            let mut p = self.state.pending.lock().unwrap();
            *p += 1;
        }
        self.state.pending_cv.notify_all();

        crate::JoinHandle { receiver }
    }
}

fn worker_loop(state: Arc<PoolState>) {
    loop {
        // 尝试拿一个任务跑。find_work 自己会从 pending 减 1。
        if let Some(task) = find_work(&state) {
            // catch_unwind：单个任务的 panic 不能让整个 worker 退出，否则同 worker
            // 队列里的剩余任务全都没人跑（Sender drop without send）。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.run();
            }));
            continue;
        }
        // 没活干。
        if state.shutdown.load(Ordering::Acquire) {
            // 关停：再 double-check 一次防丢唤醒，然后退。
            if find_work(&state).is_none() {
                return;
            }
            continue;
        }
        // park 在 condvar 上：等下一个任务被 spawn。
        let mut p = state.pending.lock().unwrap();
        while *p == 0 && !state.shutdown.load(Ordering::Acquire) {
            p = state.pending_cv.wait(p).unwrap();
        }
        // 醒来后回到 loop 顶继续 find_work。
    }
}

/// 找一个任务跑（worker_loop 用）。优先级：本地 LIFO → injector FIFO → 偷别人 FIFO。
fn find_work(state: &PoolState) -> Option<Task> {
    WORKER.with(|w| {
        let wh = w.borrow();
        let h = match wh.as_ref() {
            Some(h) => h,
            None => return None,
        };
        let task = find_work_via_handle(h);
        if task.is_some() {
            // 减 pending 计数。直接 -1。
            let mut p = state.pending.lock().unwrap();
            *p = p.saturating_sub(1);
        }
        task
    })
}

/// 当前线程是否是某个 StealingPool 的 worker？
pub fn is_on_worker() -> bool {
    WORKER.with(|w| w.borrow().is_some())
}

/// 在 worker 线程上跑一个挂起的任务（本地 LIFO → injector → 偷别人）。
/// 返回 `true` 表示跑了一个任务；`false` 表示全空。
///
/// 这是"阻塞时帮忙"的核心：当 worker 在 `JoinHandle::recv` 里等子任务结果时，
/// 必须持续把队列里的任务（包括子任务）跑掉，否则会死锁。
pub fn run_one_pending_task() -> bool {
    WORKER.with(|w| {
        let wh = w.borrow();
        let h = match wh.as_ref() {
            Some(h) => h,
            None => return false,
        };
        let task = find_work_via_handle(h);
        if let Some(t) = task {
            // 减 pending 计数。
            let mut p = h.pool_state.pending.lock().unwrap();
            *p = p.saturating_sub(1);
            drop(p);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                t.run();
            }));
            true
        } else {
            false
        }
    })
}

/// 与 `find_work` 共享的内部 helper：给定 WorkerHandle，按优先级找一个任务。
fn find_work_via_handle(h: &WorkerHandle) -> Option<Task> {
    // 1) 本地 LIFO。
    if let Some(t) = h.local.pop() {
        return Some(t);
    }
    // 2) 所有 injector 槽（FIFO），从自己开始轮询。
    let n_inj = h.pool_state.injector.len();
    for k in 0..n_inj {
        let idx = (h.index + k) % n_inj;
        if let Some(t) = h.pool_state.injector[idx].lock().unwrap().pop_front() {
            return Some(t);
        }
    }
    // 3) 偷别的 worker 的本地队列（FIFO 端）。
    let n = h.queues.len();
    for k in 1..=n {
        let victim_idx = (h.index + k) % n;
        if victim_idx == h.index {
            continue;
        }
        if let Some(t) = h.queues[victim_idx].steal() {
            return Some(t);
        }
    }
    None
}

impl Drop for StealingPool {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        // 叫醒所有可能在 wait 的 worker。
        self.state.pending_cv.notify_all();
        // 也 unpark 一下，万一某些 worker 卡在 yield_now 路径上。
        let threads = self.state.worker_threads.lock().unwrap();
        for t in threads.iter() {
            t.unpark();
        }
        drop(threads);
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        // 关停后看 injector 是否还有任务（应该全部跑完）。
        let remaining: usize = (0..self.state.injector.len())
            .map(|i| self.state.injector[i].lock().unwrap().len())
            .sum();
        if remaining > 0 {
            eprintln!(
                "StealingPool::drop: WARNING: {} tasks still in injector after join",
                remaining
            );
        }
    }
}
