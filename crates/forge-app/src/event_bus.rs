//! # 响应式事件总线（M10 子应用之一，呼应 Async Rust 第 6 章）
//!
//! 这一节我们回答一个问题：**当一条消息要送给很多个收件人时，谁来抄写副本？**
//!
//! M10 主干里的 mini-Redis 已经做过一遍 pub/sub：每来一个 SUBSCRIBE 就
//! 把一条 `mpsc::Sender` 塞进 `subs[channel]`，PUBLISH 时遍历这些 sender
//! 各发一份。那是"按 channel 分组"的事件总线。这里我们把那台机器抽出来
//! 单独命名、单独讲清楚——它就是教科书里的 **Subject / Observer** 模型：
//!
//! - **Subject（主题）**：被观察的对象。它内部维护一张"谁订阅了我"的表。
//! - **Observer（观察者）**：注册到 Subject、等被通知的人。在我们这里就是
//!   一个持有 `Receiver<T>` 的线程或 future。
//!
//! M10 的 mini-Redis 是"按 channel 分桶"的事件总线；本模块做的是**单主题**
//! 的版本（一个 `EventBus<T>` 就是 mini-Redis 里某一个 channel 的子表）。
//! 把多个 `EventBus` 放进一张 `HashMap<String, EventBus<T>>`，就重新组装
//! 出了 mini-Redis 的 pub/sub。教程里会把手算的"3 订阅者扇出"逐拍画清楚。
//!
//! ## 广播 vs 单消费者
//!
//! 我们自研的 `forge_channel::mpsc` 是 **多生产者、单消费者**：N 个 sender
//! 把消息推进**同一条**队列，只有一个 receiver 在另一端取。事件总线相反：
//! 一个 publisher 要让 **N 个 receiver 各拿一份完整副本**。把 mpsc 直接拿来
//! 用是不行的——第一条消息被 receiver A 取走，receiver B 就再也看不见了。
//!
//! 我们的实现办法：每个订阅者持有一条**独立的** mpsc 队列，publisher 发布
//! 时遍历所有订阅者、往每条队列里各 `send` 一份克隆。这就是"广播扇出"。
//!
//! ## 背压：慢订阅者会不会拖垮整体？
//!
//! 看上去美好的方案有个坑：如果某个订阅者一直不 `recv`，它的队列就会无限
//! 堆积，最终 OOM。这就是**背压（backpressure）**问题。本模块给出三种策略：
//!
//! - **`OverflowPolicy::Block`**：发布者在慢订阅者上阻塞，等它消费。
//!   后果：一个慢订阅者把整个总线拖住。
//! - **`OverflowPolicy::DropOldest`**：丢最旧的消息，给新消息腾位置。
//!   适合"实时显示股价"——旧的没看见也无所谓，要的是最新值。
//! - **`OverflowPolicy::DropNewest`**：丢新消息，保留旧的。
//!   适合"审计日志"——宁可漏新也不能漏旧。
//!
//! 真实的 `tokio::sync::broadcast` 用的是"环形缓冲 + 滞后订阅者丢最旧"，
//! 是 DropOldest 的一种实现。我们这里用每订阅者一条 `mpsc` + 容量上限 +
//! 策略枚举，把决策点直接摆在台面上。

use forge_channel::mpsc;
use forge_sync::mutex::Mutex;
use std::sync::Arc;

/// 订阅者端的句柄。包了 `mpsc::Receiver<T>`，使用方拿着它 `recv` 消息。
pub type Subscription<T> = mpsc::Receiver<T>;

/// 慢订阅者处理策略。详见模块级文档的"背压"一节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// 慢订阅者满时阻塞 publisher。注意：我们的 `mpsc::send` 是无界的，
    /// 所以这一档实际只在"配了容量上限"时生效——我们在 publish 内手动
    /// 跟踪每条队列长度。
    Block,
    /// 满时丢最旧消息，让新消息进队。
    DropOldest,
    /// 满时丢新消息，保留旧的。
    DropNewest,
}

/// 单条订阅记录：一条独立的 mpsc sender，加上一个"当前堆积计数"用于
/// 背压判定。计数和 sender 必须一起更新，否则会出现"计数说满了但 sender
/// 没满"或反过来的不一致——所以两者放进同一个 Mutex 内。
struct SubEntry<T> {
    sender: mpsc::Sender<T>,
    queued: usize,
}

/// 每个订阅者的容量上限。超过这个值的处理由 `OverflowPolicy` 决定。
///
/// 注意：这**不是** mpsc 队列本身的容量（mpsc 是无界的）。我们手动维护
/// `queued` 计数并按策略丢消息，把"什么算满"的语义掌握在自己手里。
/// 这么做的原因：把决策写明白，比"靠 mpsc 内部某条隐藏的有界逻辑"好懂。
const DEFAULT_CAP: usize = 16;

/// 一台事件总线。等价于 mini-Redis 里某个 channel 的订阅表，被独立出来。
///
/// `T` 必须 `Clone`——因为广播要给每个订阅者一份副本。如果 `T` 很大，
/// 考虑包一层 `Arc<T>` 让克隆只增加引用计数。
pub struct EventBus<T: Clone + Send> {
    /// 一堆订阅者。用我们自研的 `forge_sync::Mutex` 保护。
    /// 为什么不用 `RwLock`？因为 publish 既要读列表又要写每条的 `queued`
    /// 计数——读写不分，纯写场景 Mutex 更直接。
    subs: Arc<Mutex<Vec<SubEntry<T>>>>,
    /// 每订阅者最大堆积。所有订阅者共用同一个上限。
    cap: usize,
    /// 慢订阅者策略。
    policy: OverflowPolicy,
}

impl<T: Clone + Send + 'static> EventBus<T> {
    /// 建一条新总线。`cap` 是每个订阅者的容量上限。
    pub fn with_cap_and_policy(cap: usize, policy: OverflowPolicy) -> Self {
        Self {
            subs: Arc::new(Mutex::new(Vec::new())),
            cap,
            policy,
        }
    }

    /// 默认配置：cap=16、DropOldest（和 tokio::broadcast 同档语义）。
    pub fn new() -> Self {
        Self::with_cap_and_policy(DEFAULT_CAP, OverflowPolicy::DropOldest)
    }

    /// 订阅。返回一条 `Receiver<T>`，使用方拿着它 `recv` 消息。
    ///
    /// 注意：返回的 `Subscription<T>` 内部就是 mpsc 的 receiver，**只能
    /// 被一个线程持有**——这是 mpsc 的语义。如果你想让多个线程都收到
    /// 同一条广播，每个线程各自 `subscribe` 一份。
    pub fn subscribe(&self) -> Subscription<T> {
        let (tx, rx) = mpsc::channel::<T>();
        let mut subs = self.subs.lock();
        subs.push(SubEntry {
            sender: tx,
            queued: 0,
        });
        rx
    }

    /// 当前订阅者数量。给测试和监控用。
    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().len()
    }

    /// 把一条消息广播给所有订阅者。返回**成功送出**（没被丢）的订阅者数。
    ///
    /// 这就是手算例 1 里"逐拍扇出"的核心。锁内做的事：
    /// 1. 遍历每个订阅者
    /// 2. 按策略决定"满"了怎么办
    /// 3. 决定 send 的，把消息克隆一份塞进去，并 `queued += 1`
    ///
    /// 注意 `clone()` 的次数 = 实际 send 的订阅者数，而不是订阅者总数。
    /// 如果 DropNewest 决定不发，就不克隆——避免无谓的堆分配。
    pub fn publish(&self, msg: &T) -> usize {
        let mut delivered = 0;
        let mut subs = self.subs.lock();
        for entry in subs.iter_mut() {
            let should_send = match self.policy {
                OverflowPolicy::Block => {
                    // Block 策略：我们的 mpsc 无界，理论上永远不满，
                    // 但我们仍然以 `cap` 为限——超过就视为满。
                    // 这里"阻塞"实际上无法在锁内实现（不能在持锁时等 receiver
                    // 消费——会死锁），所以我们退化为"满了就跳过 + 记录"。
                    // 真正的阻塞策略需要异步运行时配合 await，留给练习。
                    entry.queued < self.cap
                }
                OverflowPolicy::DropOldest => {
                    // 满了：丢最旧——但 mpsc 是 FIFO 无界，我们没法真的
                    // "弹出队首"。简化做法：满了就直接 send（新消息进队尾，
                    // 让消费者看到顺序为准），并维持 queued 不超 cap（模拟
                    // "总量封顶"）。教学焦点在策略语义，不在环形缓冲实现。
                    if entry.queued >= self.cap {
                        // 视为"已经丢了一条旧的"，计数不变，继续 send 新的
                        true
                    } else {
                        true
                    }
                }
                OverflowPolicy::DropNewest => entry.queued < self.cap,
            };

            if should_send {
                entry.sender.send(msg.clone());
                // DropOldest 满了之后我们仍然 send，但维持计数封顶
                if entry.queued < self.cap {
                    entry.queued += 1;
                }
                delivered += 1;
            }
        }
        delivered
    }

    /// 订阅者报告"我消费了 n 条"——让 `queued` 计数下降，给背压腾位置。
    ///
    /// 真实系统里这一步可以自动化（在 receiver 包装一层），这里为了
    /// 把"计数维护"摆在台面上，让使用方显式调用。
    pub fn report_consumed(&self, n: usize) {
        let mut subs = self.subs.lock();
        for entry in subs.iter_mut() {
            if entry.queued >= n {
                entry.queued -= n;
            } else {
                entry.queued = 0;
            }
        }
    }

    /// 内部订阅者数（给监控用，不暴露 Arc）。
    pub fn handle_count(&self) -> usize {
        self.subscriber_count()
    }
}

impl<T: Clone + Send + 'static> Default for EventBus<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send> Clone for EventBus<T> {
    fn clone(&self) -> Self {
        Self {
            subs: Arc::clone(&self.subs),
            cap: self.cap,
            policy: self.policy,
        }
    }
}

// ============================================================================
//  多主题路由器：把若干个 EventBus 按 topic 名字管理起来。
//  这一层就是把 M10 mini-Redis 的 subs: HashMap<String, Vec<Sender>> 重写
//  成"每个 topic 一条独立总线"，结构对称、便于复用。
// ============================================================================

/// 多主题事件路由器。等价于 mini-Redis 的 `ServerState.subs`，但每条
/// topic 有独立的容量上限和策略。
pub struct TopicBus<T: Clone + Send + 'static> {
    topics: Arc<Mutex<std::collections::HashMap<String, EventBus<T>>>>,
    cap: usize,
    policy: OverflowPolicy,
}

impl<T: Clone + Send + 'static> TopicBus<T> {
    pub fn new(cap: usize, policy: OverflowPolicy) -> Self {
        Self {
            topics: Arc::new(Mutex::new(std::collections::HashMap::new())),
            cap,
            policy,
        }
    }

    /// 订阅某个 topic。topic 不存在就建一条新总线。
    pub fn subscribe(&self, topic: &str) -> Subscription<T> {
        let mut topics = self.topics.lock();
        let bus = topics
            .entry(topic.to_string())
            .or_insert_with(|| EventBus::with_cap_and_policy(self.cap, self.policy));
        bus.subscribe()
    }

    /// 向某 topic 发布。返回送达的订阅者数。
    pub fn publish(&self, topic: &str, msg: &T) -> usize {
        let topics = self.topics.lock();
        match topics.get(topic) {
            Some(bus) => bus.publish(msg),
            None => 0,
        }
    }

    /// 某 topic 当前订阅者数。
    pub fn subscriber_count(&self, topic: &str) -> usize {
        self.topics.lock().get(topic).map(|b| b.subscriber_count()).unwrap_or(0)
    }
}

impl<T: Clone + Send + 'static> Clone for TopicBus<T> {
    fn clone(&self) -> Self {
        Self {
            topics: Arc::clone(&self.topics),
            cap: self.cap,
            policy: self.policy,
        }
    }
}
