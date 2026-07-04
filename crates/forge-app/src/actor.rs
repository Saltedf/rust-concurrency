//! # Actor 模型（M10 子应用之二，呼应 Async Rust 第 8 章）
//!
//! 这一节回答另一个问题：**能不能让"可变状态"永远只被一个线程碰到？**
//!
//! M2 教过"数据竞争 = 共享可变状态 + 并发执行"。M7 给了第一种解法：
//! 用 Mutex 把状态包起来，谁要改谁拿锁。这里给**第二种**解法——
//! **不要让任何人直接碰状态**。状态只活在一个线程里，那个线程就是 actor；
//! 其他人想改它，只能往它的信箱里投消息，actor 自己一条条处理。
//!
//! 两台机器的对照：
//!
//! |        | Mutex 方案           | Actor 方案              |
//! |--------|---------------------|------------------------|
//! | 谁改状态 | 任何抢到锁的线程     | 只有 actor 自己那个线程 |
//! | 等待点  | lock() 阻塞          | send() 投递             |
//! | 失败模式 | 死锁（两把锁循环等待）| 消息丢失（actor 死了）   |
//! | 思维负担 | 每个临界区都要小心    | 串行处理，单线程思维     |
//!
//! Actor 的代价：响应要靠"消息里塞 oneshot sender"才能拿回结果（见下方
//! `CounterMsg::Get`）。代价换来的是**天生无竞态**——因为可变状态从未
//! 被共享过，它只活在 actor 那一个线程里。教程手算例 2 会逐拍画两个
//! Handle 同时发 Inc、actor 的 inbox 队列 [Inc, Inc] 怎么被串行处理。
//!
//! ## 为什么用 `forge_channel::mpsc` 而不是 tokio::mpsc？
//!
//! 因为本教程的运行时还没正式上线（M9b）。我们自研的 mpsc 是阻塞式的
//! `recv`——`recv()` 没消息时会睡眠等待。这正合适：actor 那个线程的
//! 全部工作就是"收消息 → 处理 → 收消息"，循环里 `recv` 阻塞无所谓，
//! 反正那个线程除了干这个没别的事。Async Rust 第 8 章用的是 `await`，
//! 思想完全相同，只是把"睡眠等"换成了"挂起让出执行权"。

use crate::event_bus; // 仅用于示例：actor 之间可以互相发消息
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

// ============================================================================
//  回复通道：'static oneshot
// ============================================================================
//
//  forge_channel::oneshot 是借用版的（'a 借用 Channel），无法塞进消息里
//  跨线程移动——因为 Sender<'a, T> 不是 'static。Actor 的 Get 模式需要
//  把回复端**塞进消息**送到另一个线程，所以我们这里写一个最小的
//  'static + 阻塞回复通道，基于 Arc + Mutex + Condvar。
//
//  语义和 forge_channel::oneshot 等价：发一次、收一次。区别只在生命周期。

/// 一条回复通道的"共享存储"。`Mutex<Option<T>>` 装回复，`Condvar` 用来
/// 阻塞等待回复到达。
struct ReplyCore<T> {
    slot: Mutex<Option<T>>,
    cv: Condvar,
}

/// 回复发送端。塞进 actor 的消息里送到 actor 线程。
pub struct Reply<T> {
    core: Arc<ReplyCore<T>>,
}

/// 回复接收端。留在发起请求的线程上 `await_reply`。
pub struct ReplyRx<T> {
    core: Arc<ReplyCore<T>>,
}

impl<T> Reply<T> {
    pub fn send(self, value: T) {
        let mut slot = self.core.slot.lock().unwrap();
        *slot = Some(value);
        self.core.cv.notify_one();
    }
}

impl<T> Clone for Reply<T> {
    fn clone(&self) -> Self {
        Self {
            core: Arc::clone(&self.core),
        }
    }
}

/// 建一条回复通道：`(可送出的 Reply, 本地等回复的 ReplyRx)`。
pub fn reply_channel<T>() -> (Reply<T>, ReplyRx<T>) {
    let core = Arc::new(ReplyCore {
        slot: Mutex::new(None),
        cv: Condvar::new(),
    });
    (
        Reply {
            core: Arc::clone(&core),
        },
        ReplyRx { core },
    )
}

impl<T> ReplyRx<T> {
    /// 阻塞等到回复到达，取出来。Condvar + while 循环防假唤醒。
    pub fn await_reply(self) -> T {
        let mut slot = self.core.slot.lock().unwrap();
        while slot.is_none() {
            slot = self.core.cv.wait(slot).unwrap();
        }
        slot.take().unwrap()
    }
}

// ============================================================================
//  自包含 inbox：支持"通道关闭"语义的 mpsc
// ============================================================================
//
//  为什么不用 forge_channel::mpsc？因为那一版的 `recv` 永远阻塞，不报告
//  "通道已关闭"（所有 sender drop）。Actor 的"信箱关了才下班"语义需要
//  收到关闭信号才能退出循环。我们这里写一个最小的、支持关闭检测的
//  `Mutex<VecDeque> + Condvar` 通道——思想与 forge_channel::mpsc 完全相同，
//  只多了"用 Arc 强引用计数 == 1 表示所有 sender 都 drop"的关闭判定。

struct InboxShared<T> {
    queue: Mutex<VecDeque<T>>,
    not_empty: Condvar,
    /// sender 存活计数。每 clone 一个 InboxTx +1，每 drop 一个 InboxTx -1。
    /// receiver 用它判定"所有 sender 是否都已 drop"。
    /// 不能用 Arc::strong_count——Drop 期间计数还没更新，会漏报。
    sender_count: std::sync::atomic::AtomicUsize,
}

struct InboxTx<T> {
    shared: Arc<InboxShared<T>>,
}

struct InboxRx<T> {
    shared: Arc<InboxShared<T>>,
}

impl<T> InboxTx<T> {
    fn send(&self, msg: T) {
        let mut q = self.shared.queue.lock().unwrap();
        q.push_back(msg);
        self.shared.not_empty.notify_one();
    }
}

impl<T> Clone for InboxTx<T> {
    fn clone(&self) -> Self {
        // clone 一个 sender：计数 +1
        self.shared
            .sender_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for InboxTx<T> {
    fn drop(&mut self) {
        // sender 被 drop：计数 -1。如果降到 0，唤醒所有等待的 receiver。
        // 关键修复：不能用 Arc::strong_count——Drop 期间 Arc 计数还没更新，
        // receiver 醒来检查时仍然看到 >= 2，会漏掉关闭信号。所以我们维护
        // 一个独立的 sender_count，在 Drop 内 fetch_sub 之后立刻判断。
        let prev = self
            .shared
            .sender_count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if prev == 1 {
            // 从 1 降到 0：所有 sender 都没了。唤醒等待的 receiver。
            self.shared.not_empty.notify_all();
        }
    }
}

impl<T> InboxRx<T> {
    /// 阻塞收一条。返回 `None` 表示通道已关闭（所有 sender drop 且队列为空）。
    fn recv(&self) -> Option<T> {
        let mut q = self.shared.queue.lock().unwrap();
        loop {
            if let Some(m) = q.pop_front() {
                return Some(m);
            }
            // sender_count == 0 表示所有 sender 都已 drop。
            if self
                .shared
                .sender_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                return None;
            }
            q = self.shared.not_empty.wait(q).unwrap();
        }
    }
}

fn inbox_channel<T>() -> (InboxTx<T>, InboxRx<T>) {
    let shared = Arc::new(InboxShared {
        queue: Mutex::new(VecDeque::new()),
        not_empty: Condvar::new(),
        // 初始：1 个 sender（刚创建的那一个）。receiver 不算。
        sender_count: std::sync::atomic::AtomicUsize::new(1),
    });
    (
        InboxTx {
            shared: Arc::clone(&shared),
        },
        InboxRx { shared },
    )
}

// ============================================================================
//  Actor 抽象：一个 inbox + 一个处理循环
// ============================================================================

/// 一个 actor 在外部世界眼中的样子：能往它的信箱里投消息。
///
/// 它就是 inbox sender 的包装。`Clone` 出多个 Handle 时，所有
/// Handle 都往**同一条**队列投——这正是 mpsc 的多生产者语义，也是 actor
/// 能从多个线程被同时调用的关键。
pub struct Handle<M> {
    tx: InboxTx<M>,
}

impl<M> Handle<M> {
    /// 给 actor 投一条消息。无界 inbox，理论上不阻塞。
    pub fn send(&self, msg: M) {
        self.tx.send(msg);
    }
}

impl<M> Clone for Handle<M> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

/// 一个跑起来的 actor：持有一个后台线程的 `JoinHandle`。
///
/// 让 actor 退出的方式是 **drop 所有 Handle**——inbox 检测到"所有 sender
/// 都没了"，`recv` 返回 `None`，循环自然退出。`shutdown_tx` 就是为此保留：
/// 调用 `shutdown` 或 drop Actor 时丢掉它，触发关闭。
pub struct Actor<M: Send + 'static> {
    handle: Option<JoinHandle<()>>,
    /// 保留一条 sender，避免 inbox 因为"sender 全没了"而提前关闭。
    /// 外部 clone 出去的 Handle 才是真正投消息用的；这一条只用于
    /// `shutdown` / drop 时显式关闭 inbox。
    shutdown_tx: Option<InboxTx<M>>,
}

impl<M: Send + 'static> Actor<M> {
    /// 主动关闭 inbox 并等待 actor 线程退出。
    ///
    /// **前提**：调用方必须先 drop 掉所有从 spawn 拿到的 Handle（以及它们的
    /// 所有 clone）。否则 inbox 里仍有 sender 存活，recv 不会返回 None，
    /// actor 线程不会退出，shutdown 会永远阻塞。
    ///
    /// 这个前提反映了 actor 模型的语义：actor 的生命周期由"还有谁可能投消息"
    /// 决定。如果还有 Handle 持有 sender，说明外部世界仍可能投消息，actor
    /// 不应当退出。
    pub fn shutdown(mut self) -> std::thread::Result<()> {
        self.shutdown_tx.take(); // drop 保留的 sender
        if let Some(h) = self.handle.take() {
            h.join()?;
        }
        Ok(())
    }
}

impl<M: Send + 'static> Drop for Actor<M> {
    fn drop(&mut self) {
        // drop shutdown_tx 触发 inbox 关闭，actor 循环退出
        self.shutdown_tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 启动一个 actor。
///
/// `handler` 是个闭包，每来一条消息被调一次：`fn(&mut State, M)`。
/// 状态由 actor 自己持有，外部永远拿不到引用——这就是"无共享"的本质。
///
/// 我们故意把 handler 写成同步函数而不是 future——因为我们的运行时还没
/// 上线。Async Rust 第 8 章那个版本是 `async fn basic_actor`，思想相同。
pub fn spawn<M, S, F>(initial_state: S, handler: F) -> (Actor<M>, Handle<M>)
where
    M: Send + 'static,
    S: Send + 'static,
    F: Fn(&mut S, M) + Send + 'static,
{
    let (tx, rx) = inbox_channel::<M>();
    let handle = Handle { tx: tx.clone() };
    let shutdown_tx = Some(tx);

    let join = thread::Builder::new()
        .name("forge-actor".into())
        .spawn(move || {
            let mut state = initial_state;
            // —— 核心 actor 循环：一条条处理 inbox 里的消息 ——
            // 这是手算例 2 那张"逐拍"图的本质：队列 [Inc, Inc] 被这个
            // 循环逐条 dequeue、调用 handler、状态从 5 → 6 → 7。
            // 因为只有一个线程碰 state，所以无锁也无竞态。
            while let Some(msg) = rx.recv() {
                handler(&mut state, msg);
            }
        })
        .expect("failed to spawn actor thread");

    let actor = Actor {
        handle: Some(join),
        shutdown_tx,
    };
    (actor, handle)
}

// ============================================================================
//  示例 1：Counter actor —— 内部一个 i64，支持 Inc / Get
// ============================================================================

/// Counter actor 能听懂的消息。
pub enum CounterMsg {
    /// 把计数器加 n。
    Inc(i64),
    /// 查询当前值。回复走一条 `Reply<i64>`——塞进消息送到 actor 线程，
    /// actor 处理完用 `Reply::send` 把答案发回。语义和 oneshot 等价。
    Get(Reply<i64>),
}

/// 一个 Counter actor 的状态。
pub struct CounterState {
    pub value: i64,
}

impl Default for CounterState {
    fn default() -> Self {
        Self { value: 0 }
    }
}

/// Counter 的 handler：纯函数，按消息更新 state。
pub fn counter_handler(state: &mut CounterState, msg: CounterMsg) {
    match msg {
        CounterMsg::Inc(n) => state.value += n,
        CounterMsg::Get(resp) => {
            // 回复失败 = 询问方那边 oneshot receiver 被 drop 了——通常意味着
            // 询问方超时放弃了。我们静默忽略，不让它拖垮 actor。
            let _ = resp.send(state.value);
        }
    }
}

/// 一键启动 Counter actor。教学版便利函数。
pub fn spawn_counter(initial: i64) -> (Actor<CounterMsg>, Handle<CounterMsg>) {
    spawn(CounterState { value: initial }, counter_handler)
}

// ============================================================================
//  示例 2：KV actor —— 内部一个 HashMap，支持 Set / Get / Del
// ============================================================================

/// KV actor 能听懂的消息。
pub enum KvMsg {
    Set(String, String),
    Get(String, Reply<Option<String>>),
    Del(String),
}

pub struct KvState {
    pub map: HashMap<String, String>,
}

pub fn kv_handler(state: &mut KvState, msg: KvMsg) {
    match msg {
        KvMsg::Set(k, v) => {
            state.map.insert(k, v);
        }
        KvMsg::Get(k, resp) => {
            let _ = resp.send(state.map.get(&k).cloned());
        }
        KvMsg::Del(k) => {
            state.map.remove(&k);
        }
    }
}

pub fn spawn_kv() -> (Actor<KvMsg>, Handle<KvMsg>) {
    spawn(
        KvState {
            map: HashMap::new(),
        },
        kv_handler,
    )
}

// ============================================================================
//  示例 3：把 actor 和事件总线连起来——一个"广播" actor。
//  说明 actor 模型可以嵌进更大的系统里：actor 之间通过消息传递通信，
//  actor 也可以通过 EventBus 向外部世界广播自己的状态变化。
// ============================================================================

/// Broadcast actor 收到一条消息，就往它持有的 EventBus 上发一份。
pub struct BroadcastState<T: Clone + Send + 'static> {
    pub bus: event_bus::EventBus<T>,
}

pub fn broadcast_handler<T: Clone + Send + 'static>(state: &mut BroadcastState<T>, msg: T) {
    state.bus.publish(&msg);
}

/// 启动一个广播 actor：往它的 Handle 投消息 = 向该总线的所有订阅者广播。
pub fn spawn_broadcast<T: Clone + Send + 'static>(
    bus: event_bus::EventBus<T>,
) -> (Actor<T>, Handle<T>) {
    spawn(BroadcastState { bus }, broadcast_handler::<T>)
}

// 让 actor 模块里能引用 event_bus（用于 broadcast actor 示例）。
// 注意：这里没有形成循环依赖——event_bus 不引用 actor。
#[allow(unused_imports)]
use event_bus as _event_bus_export;
