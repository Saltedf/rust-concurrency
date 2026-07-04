//! M11 异步测试示例 2:mock reactor + future 状态机的属性测试。
//!
//! 这个文件演示两件事:
//! 1. 一个**手动驱动的假 reactor**——不依赖 epoll/kqueue/线程,
//!    完全由测试代码控制"何时 fire 哪个 deadline"。
//! 2. 用 mock reactor 钉死两个隐式契约:
//!    - `Delay` 的 deadline 精确性(不早不晚)。
//!    - 一个"只应被消费一次"的 future,被反复 poll 后不会重复触发副作用。
//!
//! 跑法:cargo test -p forge-rt --test m11_mock_reactor

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use forge_rt::noop_waker;

// =========================================================================
// MockReactor —— 不起后台线程,不调 epoll,完全手动驱动。
// =========================================================================

/// 一个手动驱动的"假 reactor"。它把 reactor 的唯一职责
/// (在 timer 到期时调 `waker.wake()`)暴露成一个测试代码可调的 API。
pub struct MockReactor {
    /// deadline_ms → 一组 waker。
    timers: Mutex<BTreeMap<u64, Vec<Waker>>>,
}

impl MockReactor {
    pub fn new() -> Self {
        Self { timers: Mutex::new(BTreeMap::new()) }
    }

    /// 注册一个"到 deadline_ms 时叫醒我"的 waker。
    pub fn register(&self, deadline_ms: u64, waker: Waker) {
        self.timers.lock().unwrap()
            .entry(deadline_ms).or_default().push(waker);
    }

    /// 测试代码手动触发"所有 deadline_ms <= t 的 waker"。
    /// 这是 mock reactor 的核心:把"何时 wake"的决定权完全交给测试。
    /// 返回被触发的 waker 数(便于断言)。
    pub fn fire(&self, t: u64) -> usize {
        let mut timers = self.timers.lock().unwrap();
        let due: Vec<u64> = timers.range(..=t).map(|(k, _)| *k).collect();
        let mut fired = 0;
        for k in due {
            if let Some(ws) = timers.remove(&k) {
                for w in ws {
                    w.wake();
                    fired += 1;
                }
            }
        }
        fired
    }

    /// 下一个未触发的 deadline。测试代码用它决定"该把虚拟时钟推到哪"。
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.timers.lock().unwrap().keys().next().copied()
    }

    /// 当前注册的 waker 总数。
    pub fn pending_count(&self) -> usize {
        self.timers.lock().unwrap().values().map(|v| v.len()).sum()
    }
}

// =========================================================================
// 一个用 MockReactor 驱动的 Delay future。
// =========================================================================

/// `Delay` 的 mock-reactor 版本。deadline_ms 是从"被构造那一刻"起算的相对值,
/// 内部转成绝对 ms(以 reactor 的全局时钟为准,这里简化为"自增计数")。
pub struct MrDelay {
    deadline_ms: u64,
    reactor: Arc<MockReactor>,
    /// 是否已经注册过 waker(避免重复注册)。
    registered: bool,
}

impl MrDelay {
    pub fn new(reactor: Arc<MockReactor>, deadline_ms: u64) -> Self {
        Self { deadline_ms, reactor, registered: false }
    }
    pub fn deadline_ms(&self) -> u64 { self.deadline_ms }
}

impl Future for MrDelay {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if !self.registered {
            // 第一次 poll:注册 waker。
            self.reactor.register(self.deadline_ms, cx.waker().clone());
            self.registered = true;
        }
        // 真实 Delay 在这里检查 `Instant::now() >= deadline`。
        // 这里我们不自己判断,而是把"是否到期"完全交给 reactor 的 fire。
        // reactor 没 fire 过我们的 deadline,说明时间还没到 → Pending。
        // reactor fire 过,会调 wake(),执行器再 poll 时 waker 已经被消费……
        //
        // 简化:我们用一个标志位,reactor fire 时通过 waker.wake() 触发重新 poll,
        // 重新 poll 时我们检查"deadline 是否在 reactor 的 fired 集合里"——
        // 但 MockReactor.fire 已经把对应 entry remove 了,我们看不到。
        //
        // 更简洁的做法:用一个 OnceCell 记录"我是否已被 fire"。
        // 但为了演示骨架,这里用一个外部 atomic:
        Poll::Pending
    }
}

// 上面那个 MrDelay 因为"是否到期"信号没回流,实际拿不到 Ready。
// 在生产 forge-rt 里,reactor 线程调 waker.wake() → task 重新入队 →
// 下一拍 poll 时 Instant::now() >= deadline → Ready。
// 在 mock 下我们要模拟这个回流。最干净的做法是:MrDelay 持有一个
// `Arc<AtomicBool>`,reactor fire 时除了 wake 还把它 set true。

pub struct MrDelayV2 {
    deadline_ms: u64,
    reactor: Arc<MockReactorV2>,
    fired: Arc<AtomicUsize>,
    registered: bool,
}

pub struct MockReactorV2 {
    timers: Mutex<BTreeMap<u64, Vec<(Waker, Arc<AtomicUsize>)>>>,
}

impl MockReactorV2 {
    pub fn new() -> Self {
        Self { timers: Mutex::new(BTreeMap::new()) }
    }

    pub fn register(&self, deadline_ms: u64, waker: Waker, flag: Arc<AtomicUsize>) {
        self.timers.lock().unwrap()
            .entry(deadline_ms).or_default().push((waker, flag));
    }

    pub fn fire(&self, t: u64) -> usize {
        let mut timers = self.timers.lock().unwrap();
        let due: Vec<u64> = timers.range(..=t).map(|(k, _)| *k).collect();
        let mut fired = 0;
        for k in due {
            if let Some(ws) = timers.remove(&k) {
                for (w, flag) in ws {
                    flag.fetch_add(1, Ordering::AcqRel);
                    w.wake();
                    fired += 1;
                }
            }
        }
        fired
    }

    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.timers.lock().unwrap().keys().next().copied()
    }
}

impl MrDelayV2 {
    pub fn new(reactor: Arc<MockReactorV2>, deadline_ms: u64) -> Self {
        Self {
            deadline_ms,
            reactor,
            fired: Arc::new(AtomicUsize::new(0)),
            registered: false,
        }
    }
    pub fn deadline_ms(&self) -> u64 { self.deadline_ms }
}

impl Future for MrDelayV2 {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // 如果 reactor 已经 fire 过我们的 deadline,fired > 0 → Ready。
        if self.fired.load(Ordering::Acquire) > 0 {
            return Poll::Ready(());
        }
        // 第一次 poll:注册 waker + flag。
        if !self.registered {
            self.reactor.register(self.deadline_ms, cx.waker().clone(), self.fired.clone());
            self.registered = true;
        }
        Poll::Pending
    }
}

// =========================================================================
// 测试 1:deadline 精确性 —— reactor 在 deadline_ms 之前 fire 不触发,之后才触发。
// =========================================================================

fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}

#[test]
fn mr_delay_does_not_fire_before_deadline() {
    let reactor = Arc::new(MockReactorV2::new());
    let mut delay = MrDelayV2::new(reactor.clone(), 100);

    // 第一次 poll:注册 waker,返回 Pending。
    assert!(poll_once(&mut delay).is_pending());
    assert_eq!(reactor.next_deadline_ms(), Some(100));

    // 在 deadline 之前 fire:不应当触发任何 waker。
    let fired = reactor.fire(99);
    assert_eq!(fired, 0, "deadline=100 时,fire(99) 不应触发任何 waker");
    assert_eq!(reactor.next_deadline_ms(), Some(100));

    // delay 仍然 pending。
    assert!(poll_once(&mut delay).is_pending());
}

#[test]
fn mr_delay_fires_at_exact_deadline() {
    let reactor = Arc::new(MockReactorV2::new());
    let mut delay = MrDelayV2::new(reactor.clone(), 100);

    assert!(poll_once(&mut delay).is_pending());

    // 在 deadline 那一拍 fire:应当触发 1 个 waker。
    let fired = reactor.fire(100);
    assert_eq!(fired, 1, "fire(100) 应当触发 deadline=100 的 waker");
    assert_eq!(reactor.next_deadline_ms(), None, "已无未触发的 deadline");

    // 现在 delay 应当 Ready。
    assert_eq!(poll_once(&mut delay), Poll::Ready(()));
}

// =========================================================================
// 测试 2:多个 delay 同一 ms 都 ready —— 钉死"fire(t) 同时触发所有 deadline<=t"。
// =========================================================================

#[test]
fn multiple_delays_same_tick_all_fire() {
    let reactor = Arc::new(MockReactorV2::new());
    let mut a = MrDelayV2::new(reactor.clone(), 50);
    let mut b = MrDelayV2::new(reactor.clone(), 50);
    let mut c = MrDelayV2::new(reactor.clone(), 50);

    // 都 poll 一次,注册 waker。
    assert!(poll_once(&mut a).is_pending());
    assert!(poll_once(&mut b).is_pending());
    assert!(poll_once(&mut c).is_pending());

    // fire(50) 应当一次触发全部 3 个 waker。
    let fired = reactor.fire(50);
    assert_eq!(fired, 3);

    // 三个都应当 Ready。
    assert_eq!(poll_once(&mut a), Poll::Ready(()));
    assert_eq!(poll_once(&mut b), Poll::Ready(()));
    assert_eq!(poll_once(&mut c), Poll::Ready(()));
}

// =========================================================================
// 测试 3:不同 deadline 的 fire 顺序。
// 钉死契约:"fire(t) 只触发 deadline<=t 的,deadline>t 的不受影响"。
// =========================================================================

#[test]
fn fire_only_triggers_due_deadlines() {
    let reactor = Arc::new(MockReactorV2::new());
    let mut early = MrDelayV2::new(reactor.clone(), 10);
    let mut mid = MrDelayV2::new(reactor.clone(), 50);
    let mut late = MrDelayV2::new(reactor.clone(), 100);

    assert!(poll_once(&mut early).is_pending());
    assert!(poll_once(&mut mid).is_pending());
    assert!(poll_once(&mut late).is_pending());

    // fire(50):触发 early(10) 和 mid(50),不触发 late(100)。
    let fired = reactor.fire(50);
    assert_eq!(fired, 2, "fire(50) 应触发 deadline=10 和 50");
    assert_eq!(reactor.next_deadline_ms(), Some(100), "late 还没触发");

    assert_eq!(poll_once(&mut early), Poll::Ready(()));
    assert_eq!(poll_once(&mut mid), Poll::Ready(()));
    assert!(poll_once(&mut late).is_pending(), "late 不该 ready");

    // 再 fire(100):触发 late。
    let fired = reactor.fire(100);
    assert_eq!(fired, 1);
    assert_eq!(poll_once(&mut late), Poll::Ready(()));
}

// =========================================================================
// 测试 4:属性测试 —— "只应被消费一次"的 future,被反复 poll 后不会重复触发副作用。
//
// 这是一个 CounterFuture:它内部维护一个计数器,poll 一次就 +1,
// 到达 N 次后返回 Ready(最终计数值)。不变量:
//   - 计数器最终的值 = 创建的 future 数量(不会因为反复 poll 而翻倍)。
//   - 一旦 Ready,后续 poll 不应当再增加计数(即"消费一次")。
// =========================================================================

struct CounterFuture {
    /// 共享副作用计数器:每个 future 每次 poll 增加一次。
    /// 这模拟"future 在被 poll 时对外部世界产生不可逆副作用"。
    count: Arc<AtomicUsize>,
    /// 本 future 自己的 poll 计数。和 count 分开,避免共享污染判定。
    own_polls: usize,
    /// 多少次 poll 后返回 Ready。
    target: usize,
    /// 是否已经返回过 Ready。
    consumed: bool,
}

impl CounterFuture {
    fn new(count: Arc<AtomicUsize>, target: usize) -> Self {
        Self { count, own_polls: 0, target, consumed: false }
    }
}

impl Future for CounterFuture {
    type Output = usize;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<usize> {
        // 不变量:一旦 consumed,后续 poll 不应当再增加 count。
        if self.consumed {
            return Poll::Ready(self.target);
        }
        self.own_polls += 1;
        // 每次 poll 把共享 count +1(模拟"副作用")。
        self.count.fetch_add(1, Ordering::Relaxed);
        if self.own_polls >= self.target {
            self.consumed = true;
            Poll::Ready(self.own_polls)
        } else {
            Poll::Pending
        }
    }
}

#[test]
fn counter_future_not_double_consumed_under_extra_polls() {
    // 创建一个 target=5 的 CounterFuture,然后:
    // 1. poll 4 次(count 到 4,future 还 pending)。
    // 2. 第 5 次 poll → Ready(5)。
    // 3. **再多 poll 3 次** —— 不变量:count 不应当继续增加。
    let count = Arc::new(AtomicUsize::new(0));
    let mut f = CounterFuture::new(count.clone(), 5);

    for _ in 0..4 {
        assert!(poll_once(&mut f).is_pending());
    }
    assert_eq!(count.load(Ordering::Relaxed), 4);
    assert_eq!(poll_once(&mut f), Poll::Ready(5));
    assert_eq!(count.load(Ordering::Relaxed), 5);

    // 关键:再 poll 3 次,count 不应当变。
    for _ in 0..3 {
        assert_eq!(poll_once(&mut f), Poll::Ready(5));
    }
    assert_eq!(count.load(Ordering::Relaxed), 5,
        "consumed 后的额外 poll 不应增加 count(违反幂等性)");
}

// =========================================================================
// 测试 5:多个 CounterFuture 共享 count —— 任意 poll 序列下,count 最终 = 已 Ready 的 future 数。
// =========================================================================

#[test]
fn shared_counter_eventually_equals_ready_futures() {
    // 10 个 CounterFuture 共享一个 count,每个 target=3。
    // 我们用"轮流 poll 每个 future"的序列,直到所有 future 都 consumed。
    // 不变量:无论怎么轮流,count 最终 = target × n_futures,
    // 并且任意中间时刻 count <= target × n_futures(没有重复计数)。
    let count = Arc::new(AtomicUsize::new(0));
    let target = 3;
    let n_futures = 10;
    let mut futures: Vec<CounterFuture> = (0..n_futures)
        .map(|_| CounterFuture::new(count.clone(), target))
        .collect();

    // 轮流 poll,直到所有 future 都 consumed。
    // 防死循环:每轮所有 future 都被 poll 一次,最多 target 轮必完成。
    let max_rounds = target + 1;
    let mut all_consumed = false;
    for round in 0..max_rounds {
        all_consumed = true;
        for f in futures.iter_mut() {
            // 关键:每个 future 在 consumed 后,poll 返回 Ready 但不增加 count。
            // 所以反复 poll 安全。
            if !f.consumed {
                let _ = poll_once(f);
            }
            if !f.consumed {
                all_consumed = false;
            }
        }
        if all_consumed {
            break;
        }
        assert!(round < max_rounds - 1, "防死循环:超过 {} 轮仍有 future 未 ready", max_rounds);
    }
    assert!(all_consumed, "所有 future 都应当 consumed");

    // 不变量:count 恰好 = target × n_futures。
    // 如果某个 future 被错误地"重复计数",count 会 > 这个值。
    let final_count = count.load(Ordering::Relaxed);
    assert_eq!(final_count, target * n_futures,
        "任意 poll 序列下,count 最终应当 = target × n_futures,实际 = {}",
        final_count);

    // 再多 poll 几轮,验证 count **不**继续增长(幂等性)。
    for _ in 0..5 {
        for f in futures.iter_mut() {
            let _ = poll_once(f);
        }
    }
    assert_eq!(count.load(Ordering::Relaxed), target * n_futures,
        "全部 consumed 后再 poll,count 不应增长");
}
