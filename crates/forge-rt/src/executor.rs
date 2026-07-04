//! # Executor —— 异步执行器（单线程 `block_on` + 多线程工作窃取池）。
//!
//! 执行器是"任务调度员"：它维护一个就绪队列，从里面取 task 跑一次 poll。
//! poll 返回 Pending？把 task 搁一边（它的 Waker 会在将来某个时刻被 reactor
//! 或别的 task 调用，把它重新塞回就绪队列）。返回 Ready？task 完成、销毁。
//!
//! 我们提供两层：
//! - [`block_on`]：单线程，把一个 future 跑到完，期间手动维护一个就绪队列——
//!   这是教学核心，让你看见"executor loop"的赤裸骨架。
//! - [`Runtime`]：多线程工作窃取执行器（N worker 各有本地队列 + 偷），结构
//!   和 M9a 的 [`StealingPool`](forge_pool::v3_stealing::StealingPool) 一致，
//!   只是把"闭包任务"换成"Task"。
//!
//! 教程第四、六章逐拍画过两者的任务流转。

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::oneshot;
use crate::reactor::Reactor;
use crate::task::Task;

// =========================================================================
// 单线程 block_on —— 教学骨架
// =========================================================================

/// 在**当前线程**上把一个 future 跑到完成。期间：
/// 1. 把 future 包成一个 task（注册到本函数私有的就绪队列 + schedule 回调）；
/// 2. 循环：从就绪队列取一个 task，poll 一次；队列空但主 future 还没好，就等
///    被外部 wake（park 当前线程）；
/// 3. 主 future 完成 → 取出结果返回。
///
/// 这个版本**不**起 worker 线程，是真正的"单线程异步"——就像 JavaScript 的
/// event loop。它存在的目的：让你看清 executor 的核心循环长什么样，没有
/// 多线程干扰。
pub fn block_on<F, T>(future: F, reactor: &Reactor) -> T
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // 本地就绪队列 + 主 future 的结果槽。
    let queue: Arc<Mutex<VecDeque<Arc<Task>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let woken: Arc<(Mutex<usize>, Condvar)> = Arc::new((Mutex::new(0), Condvar::new()));

    // schedule 回调：把 task push 进队列、wake 当前线程。
    let queue_for_sched = queue.clone();
    let woken_for_sched = woken.clone();
    let schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static> = Arc::new(move |task| {
        queue_for_sched.lock().unwrap().push_back(task);
        // 增加"待处理"计数并叫醒可能 park 中的 block_on 主线程。
        let mut c = woken_for_sched.0.lock().unwrap();
        *c += 1;
        woken_for_sched.1.notify_one();
    });

    // 主 future 包成 task。
    let (sender, receiver) = oneshot::channel::<T>();
    let main_task = Task::spawn(future, schedule, sender);
    queue.lock().unwrap().push_back(main_task.clone());
    {
        let mut c = woken.0.lock().unwrap();
        *c += 1;
    }

    // 主循环。
    let main_waker = main_task.waker();
    loop {
        // 取一个 task 跑。
        let task = queue.lock().unwrap().pop_front();
        if let Some(t) = task {
            let w = t.waker();
            t.poll(&w);
            continue;
        }

        // 队列空：看主 future 是否完成。
        if let Some(v) = receiver.try_recv() {
            let _ = main_waker; // 主 task 还没 drop（main_task 还活着），不要紧
            return v;
        }

        // 还没完成，等被 wake。Condvar 保证不丢唤醒。
        let mut c = woken.0.lock().unwrap();
        while *c == 0 {
            c = woken.1.wait(c).unwrap();
        }
        *c -= 1;
    }
}

// =========================================================================
// 多线程工作窃取执行器 Runtime
// =========================================================================

/// 每 worker 一个本地队列（Mutex<VecDeque>）。owner 用 LIFO 端、thief 用 FIFO 端。
/// 和 M9a 的 [`LocalQueue`](forge_pool::v3_stealing) 设计完全一致。
struct LocalQueue {
    inner: Mutex<VecDeque<Arc<Task>>>,
}

impl LocalQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
        }
    }
    fn push(&self, t: Arc<Task>) {
        self.inner.lock().unwrap().push_front(t);
    }
    fn pop(&self) -> Option<Arc<Task>> {
        self.inner.lock().unwrap().pop_front()
    }
    fn steal(&self) -> Option<Arc<Task>> {
        self.inner.lock().unwrap().pop_back()
    }
}

struct WorkerHandle {
    local: Arc<LocalQueue>,
    queues: Vec<Arc<LocalQueue>>,
    index: usize,
    pool_state: Arc<PoolState>,
}

thread_local! {
    static WORKER: std::cell::RefCell<Option<WorkerHandle>> = std::cell::RefCell::new(None);
}

struct PoolState {
    shutdown: AtomicBool,
    /// 外部线程投递的 task，每 worker 一个槽（轮询分发）。
    injector: Vec<Mutex<VecDeque<Arc<Task>>>>,
    next_external: AtomicUsize,
    worker_threads: Mutex<Vec<std::thread::Thread>>,
    n_workers: usize,
    /// 待处理任务计数 + Condvar。和 M9a 的 StealingPool 一致。
    pending: Mutex<usize>,
    pending_cv: Condvar,
}

/// 多线程异步运行时。clone 一份 = 多一份引用。
#[derive(Clone)]
pub struct Runtime {
    state: Arc<PoolState>,
    reactor: Reactor,
    /// 全局共享的 schedule 闭包。所有 task 的 Waker 都引用它。
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
}

impl Runtime {
    /// 起一台运行时：N worker + 一个 reactor + 全局 schedule 闭包。
    pub fn new(n_workers: usize, reactor: Reactor) -> std::io::Result<Self> {
        let queues: Vec<Arc<LocalQueue>> = (0..n_workers)
            .map(|_| Arc::new(LocalQueue::new()))
            .collect();
        let state = Arc::new(PoolState {
            shutdown: AtomicBool::new(false),
            injector: (0..n_workers)
                .map(|_| Mutex::new(VecDeque::new()))
                .collect(),
            next_external: AtomicUsize::new(0),
            worker_threads: Mutex::new(Vec::with_capacity(n_workers)),
            n_workers,
            pending: Mutex::new(0),
            pending_cv: Condvar::new(),
        });

        // schedule 闭包：决定"task 被 wake 后塞到哪个队列"。
        // 规则：如果当前在 worker 上，push 到本地（LIFO）；否则塞某个 injector 槽。
        let st = state.clone();
        let schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static> =
            Arc::new(move |task: Arc<Task>| {
                let pushed_local = WORKER.with(|w| {
                    if let Some(h) = w.borrow().as_ref() {
                        h.local.push(task.clone());
                        return true;
                    }
                    false
                });
                if !pushed_local {
                    let n = st.n_workers;
                    let idx = st.next_external.fetch_add(1, Ordering::Relaxed) % n;
                    st.injector[idx].lock().unwrap().push_back(task);
                }
                // 加 pending 并通知。
                let mut p = st.pending.lock().unwrap();
                *p += 1;
                st.pending_cv.notify_all();
            });

        // 起 worker 线程。
        let mut workers = Vec::with_capacity(n_workers);
        for index in 0..n_workers {
            let local = queues[index].clone();
            let queues_clone = queues.clone();
            let st = state.clone();
            let sched = schedule.clone();
            let reactor = reactor.clone();
            let handle = std::thread::Builder::new()
                .name(format!("forge-rt-worker-{index}"))
                .spawn(move || {
                    WORKER.with(|w| {
                        *w.borrow_mut() = Some(WorkerHandle {
                            local,
                            queues: queues_clone,
                            index,
                            pool_state: st.clone(),
                        });
                    });
                    worker_loop(st, sched, reactor);
                })?;
            workers.push(handle);
        }
        {
            let mut v = state.worker_threads.lock().unwrap();
            for w in &workers {
                v.push(w.thread().clone());
            }
        }

        Ok(Self {
            state,
            reactor,
            schedule,
        })
    }

    /// 把一个 future 投到运行时上跑。返回 [`JoinHandle`] 拿结果。
    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel::<T>();
        let task = Task::spawn(future, self.schedule.clone(), sender);

        // 把 task 入队（复用 schedule 闘包）。
        (self.schedule)(task);

        JoinHandle {
            receiver,
            _rt: self.clone(),
        }
    }

    /// 在当前线程上把 future 跑到完。期间帮忙跑别的就绪任务（worker 线程）或
    /// 直接复用单线程 block_on（外部线程）。
    pub fn block_on<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // 简单起见：复用单线程 block_on 的逻辑（当前线程不走 worker 池）。
        // 真实 tokio 的 block_on 在 worker 线程上跑会做更复杂的"主任务 + worker 协作"。
        block_on(future, &self.reactor)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        self.state.pending_cv.notify_all();
        let threads = self.state.worker_threads.lock().unwrap();
        for t in threads.iter() {
            t.unpark();
        }
    }
}

fn worker_loop(
    state: Arc<PoolState>,
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
    _reactor: Reactor,
) {
    let _ = schedule; // worker 通过 WORKER thread_local 的 pool_state 入队；schedule 这里持有备用。
    loop {
        if let Some(task) = find_work(&state) {
            let w = task.waker();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.clone().poll(&w);
            }));
            continue;
        }
        if state.shutdown.load(Ordering::Acquire) {
            if find_work(&state).is_none() {
                return;
            }
            continue;
        }
        let mut p = state.pending.lock().unwrap();
        while *p == 0 && !state.shutdown.load(Ordering::Acquire) {
            p = state.pending_cv.wait(p).unwrap();
        }
    }
}

fn find_work(state: &PoolState) -> Option<Arc<Task>> {
    WORKER.with(|w| {
        let wh = w.borrow();
        let h = wh.as_ref()?;
        // 1) 本地 LIFO。
        if let Some(t) = h.local.pop() {
            dec_pending(state);
            return Some(t);
        }
        // 2) injector（FIFO）。
        let n_inj = h.pool_state.injector.len();
        for k in 0..n_inj {
            let idx = (h.index + k) % n_inj;
            if let Some(t) = h.pool_state.injector[idx].lock().unwrap().pop_front() {
                dec_pending(state);
                return Some(t);
            }
        }
        // 3) 偷别人的本地队列（FIFO 端）。
        let n = h.queues.len();
        for k in 1..=n {
            let victim_idx = (h.index + k) % n;
            if victim_idx == h.index {
                continue;
            }
            if let Some(t) = h.queues[victim_idx].steal() {
                dec_pending(state);
                return Some(t);
            }
        }
        None
    })
}

fn dec_pending(state: &PoolState) {
    let mut p = state.pending.lock().unwrap();
    *p = p.saturating_sub(1);
}

/// `runtime.spawn(...)` 返回这个。await 它（或调 `recv`）拿结果。
pub struct JoinHandle<T> {
    pub(crate) receiver: oneshot::Receiver<T>,
    /// 持有 runtime 一份引用，避免 runtime 在 join 完之前被 drop。
    pub(crate) _rt: Runtime,
}

impl<T> JoinHandle<T> {
    /// 阻塞等结果。
    pub fn recv(self) -> T {
        self.receiver.recv()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<T> {
        // 用 oneshot 的 try_recv + 把 waker 注册到 sender 那一侧……
        // 当前我们的 oneshot 是同步的（forge-pool::oneshot），不支持注册 waker。
        // 简化：把 JoinHandle 等同于"调 recv"——只能阻塞等，不能 await。
        // 真实实现要换成支持 async 的 oneshot。
        // 这里为教学保留 Future impl 的骨架，实际 await JoinHandle 时会退化成阻塞。
        let this = unsafe { self.get_unchecked_mut() };
        if let Some(v) = this.receiver.try_recv() {
            std::task::Poll::Ready(v)
        } else {
            // 这里需要把 cx.waker() 注册到 oneshot 上——同步 oneshot 不支持，
            // 简化处理：直接 recv 阻塞（仅在主 future 内部 spawn 时建议用 recv）。
            // 教程会讲清"为什么需要 async oneshot"，但不在本模块实现。
            std::task::Poll::Pending
        }
    }
}
