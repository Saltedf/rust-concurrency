//! # Reactor —— 把 epoll/kqueue/IOCP 包成"就绪 → 找 Waker → 唤醒"。
//!
//! 这一层是异步运行时和操作系统之间的"翻译官"。每个 I/O 资源（这里我们用
//! `mio::Waker` 做最简版本，定时器用 BTreeMap 维护到期表）在被注册时拿到一个
//! 唯一的 `Token`（一个 `u64`）。当操作系统说"该醒一下了"的时候，reactor
//! 用 Token 反查出对应的 [`Waker`](std::task::Waker) 并调 `wake()`，把等待它
//! 的任务重新塞回执行器队列。
//!
//! 真实运行时（tokio、async-std、smol）的 reactor 比这复杂得多——它要支持
//! TCP/UDP/Unix socket、文件 I/O（Linux 上靠 io_uring 或线程池）、信号……
//! 但**核心结构就是这里的这一段**：一张"Token → Waker"表 + 一个 `mio::Poll`
//! 循环线程 + 一个"按 deadline 排序的 timer 队列"。
//!
//! 教程在 M9b 第七章逐拍画过这张表的填/查/清。

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::{Duration, Instant};

use mio::{Events, Poll, Token, Waker as MioWaker};

/// "Token → Slot"表里的一项。
struct Slot {
    /// 注册时填上的 Waker；就绪后被 reactor 取出唤醒，并清空（避免重复唤醒）。
    waker: Option<Waker>,
    /// 这个 timer 何时到期。reactor 线程按 deadline 排序遍历。
    deadline: Instant,
}

/// reactor 的共享内层结构：一张 Token 表 + 一个用来打断 poll 的 `mio::Waker`。
pub(crate) struct ReactorInner {
    /// "Token → Waker + deadline"表。被 reactor 线程和外部（注册/反注册）共同访问。
    slots: Mutex<HashMap<u64, Slot>>,
    /// 按 deadline 排序的索引：`(deadline, id)`。reactor 线程靠它快速找最近到期。
    timers: Mutex<BTreeMap<Instant, std::collections::HashSet<u64>>>,
    /// 自增的 Token 生成器。
    next_id: AtomicU64,
    /// 喂给 mio 的"打断 poll"句柄。注册新定时器（或更早到期）时唤醒正在 poll
    /// 的 reactor 线程——否则它一直睡在 poll 上等最早的 timer，新来的更早 timer
    /// 没人理。
    poll_waker: MioWaker,
}

impl ReactorInner {
    fn new(poll_waker: MioWaker) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            timers: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            poll_waker,
        }
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn register_timer(&self, id: u64, deadline: Instant, waker: Waker) {
        {
            let mut slots = self.slots.lock().unwrap();
            slots.insert(
                id,
                Slot {
                    waker: Some(waker),
                    deadline,
                },
            );
        }
        self.timers
            .lock()
            .unwrap()
            .entry(deadline)
            .or_default()
            .insert(id);
        // 一定要叫醒 reactor 线程重算 timeout：可能新 timer 比当前 sleep 的更早。
        let _ = self.poll_waker.wake();
    }

    fn unregister(&self, id: u64) {
        let removed = {
            let mut slots = self.slots.lock().unwrap();
            slots.remove(&id).map(|s| s.deadline)
        };
        if let Some(deadline) = removed {
            let mut timers = self.timers.lock().unwrap();
            if let Some(set) = timers.get_mut(&deadline) {
                set.remove(&id);
                if set.is_empty() {
                    timers.remove(&deadline);
                }
            }
        }
    }

    /// 由 reactor 线程调用：拿出"已到期"的所有 Waker。同时返回下一个未到期的
    /// deadline（用作 `mio::Poll::poll` 的 timeout）。
    fn drain_expired(&self, now: Instant) -> Vec<Waker> {
        let mut wakers = Vec::new();
        let mut slots = self.slots.lock().unwrap();
        let mut timers = self.timers.lock().unwrap();

        // 把所有 deadline <= now 的桶全部取出来。
        let expired_keys: Vec<Instant> = timers.range(..=now).map(|(k, _)| *k).collect();
        for k in expired_keys {
            if let Some(ids) = timers.remove(&k) {
                for id in ids {
                    if let Some(slot) = slots.get_mut(&id) {
                        if let Some(w) = slot.waker.take() {
                            wakers.push(w);
                        }
                    }
                }
            }
        }
        wakers
    }

    /// 由 reactor 线程调用：算下一个 timeout。
    fn next_deadline(&self) -> Option<Instant> {
        self.timers.lock().unwrap().keys().next().copied()
    }
}

/// 一个"定时器"注册项的对外句柄。drop 时反注册。
pub struct TimerRegistration {
    id: u64,
    reactor: Arc<ReactorInner>,
}

impl TimerRegistration {
    pub(crate) fn new(
        reactor: Arc<ReactorInner>,
        id: u64,
        deadline: Instant,
        waker: Waker,
    ) -> Self {
        reactor.register_timer(id, deadline, waker);
        Self { id, reactor }
    }
}

impl Drop for TimerRegistration {
    fn drop(&mut self) {
        self.reactor.unregister(self.id);
    }
}

/// 对外暴露的 Reactor 句柄。clone 一份就多一份引用（Arc 在内部）。
#[derive(Clone)]
pub struct Reactor {
    pub(crate) inner: Arc<ReactorInner>,
}

impl Reactor {
    /// 起一台 reactor：开一个后台线程跑 `mio::Poll` 循环，内部维护 timer 队列。
    pub fn new() -> io::Result<Self> {
        let poll: Poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let poll_waker = MioWaker::new(&registry, Token(0))?;

        let inner = Arc::new(ReactorInner::new(poll_waker));
        let inner_clone = inner.clone();

        std::thread::Builder::new()
            .name("forge-reactor".into())
            .spawn(move || reactor_thread(inner_clone, poll))?;

        Ok(Self { inner })
    }

    /// 注册一个"到 `deadline` 时叫醒我"的定时器。返回的句柄 drop 时自动反注册。
    ///
    /// 一次 future 的 poll 调一次 register：如果还没到期，下次 poll 会**重新**
    /// 注册一次新的 deadline（旧句柄 drop 自动反注册，新句柄顶上）。
    pub fn register_timer(&self, deadline: Instant, waker: Waker) -> TimerRegistration {
        let id = self.inner.alloc_id();
        TimerRegistration::new(self.inner.clone(), id, deadline, waker)
    }
}

/// reactor 后台线程的主循环。
///
/// 设计要点（教程会逐拍画）：
/// - 每一轮先取"最近一个 timer 的 deadline"，算出 `mio::Poll::poll` 的 timeout；
/// - 如果没 timer，timeout 设为 `None`（无限等），直到外部 `wake_poll` 把它叫醒；
/// - 醒来后，把"已到期"的 timer 的 Waker 全部取出来 wake。
/// - `mio::Poll::poll` 在我们的最简实现里**只用来"被打断"**：没注册任何真正的
///   fd。但 mio 在这里是 epoll/kqueue/IOCP 的薄包装，**接 TCP/UDP 时只要往同一
///   个 registry 里 register(fd, interest, token) 就行**——结构完全不变。
fn reactor_thread(inner: Arc<ReactorInner>, mut poll: Poll) {
    let mut events = Events::with_capacity(64);
    loop {
        // 1) 算 timeout：下一个 deadline 距 now 多久。
        let timeout: Option<Duration> = inner.next_deadline().map(|dl| {
            let now = Instant::now();
            if dl <= now {
                Duration::ZERO
            } else {
                dl - now
            }
        });

        // 2) 等：要么 timeout，要么被外部 wake_poll 打断。
        let _ = poll.poll(&mut events, timeout);
        events.clear(); // 我们没注册真正的 fd，事件是空的；纯靠 timeout + waker。

        // 3) 把"已到期"的 timer 全部唤醒。
        let now = Instant::now();
        for w in inner.drain_expired(now) {
            w.wake();
        }
    }
}
