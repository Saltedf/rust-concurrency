//! M11 异步测试示例 1:虚拟时钟 + 时序注入。
//!
//! 这条测试演示"如何用虚拟时钟 + 手动 wake,确定性复现一个仅在特定
//! polling 顺序下才出现的 bug"。被测对象是一个 TOCTOU 风格的信号量原型:
//! 它在 future 被 poll 时改内部状态,如果 poll 序列被打乱,会出现"丢失更新"。
//!
//! 跑法:cargo test -p forge-rt --test m11_async_virtual_clock
//!
//! 这条测试**不**起 reactor 线程(那是真实硬件路径),只用 noop_waker +
//! 手动时钟推进,完全确定性。

use std::cell::Cell;
use std::future::Future;
use std::ops::Sub;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use forge_rt::noop_waker;

// =========================================================================
// Clock —— "现在几点"的抽象。生产用真实时钟,测试用虚拟时钟。
// =========================================================================

/// "现在几点"的抽象。把它做成 trait,被测代码就不再写死 `Instant::now()`。
pub trait Clock {
    fn now(&self) -> MockInstant;
}

/// 一个"假装是 Instant"的类型。真实 Instant 不能随意构造(Instant::now 才能拿),
/// 测试里我们要从 0 开始手动推进,所以用一个 u64 毫秒数的包装类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MockInstant {
    pub ms: u64,
}

impl MockInstant {
    pub fn zero() -> Self { Self { ms: 0 } }
    pub fn from_millis(ms: u64) -> Self { Self { ms } }
}

impl std::ops::Add<MockDuration> for MockInstant {
    type Output = MockInstant;
    fn add(self, rhs: MockDuration) -> MockInstant {
        MockInstant { ms: self.ms + rhs.ms }
    }
}

impl Sub for MockInstant {
    type Output = MockDuration;
    fn sub(self, rhs: MockInstant) -> MockDuration {
        MockDuration { ms: self.ms.saturating_sub(rhs.ms) }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MockDuration { pub ms: u64 }

impl MockDuration {
    pub fn from_millis(ms: u64) -> Self { Self { ms } }
}

/// 虚拟时钟:从一个起点开始,被测试代码手动推进。
/// Rc + Cell:单线程,无锁。
pub struct VirtualClock {
    current: Cell<MockInstant>,
}

impl VirtualClock {
    pub fn new() -> Self {
        Self { current: Cell::new(MockInstant::zero()) }
    }
    /// 把时钟向前推 `dt`。这是测试代码的"上帝之手"。
    pub fn advance(&self, dt: MockDuration) {
        self.current.set(self.current.get() + dt);
    }
    pub fn now_ms(&self) -> u64 { self.current.get().ms }
}

impl Clock for VirtualClock {
    fn now(&self) -> MockInstant { self.current.get() }
}

// =========================================================================
// VDelay —— Delay 的"可注入时钟"版本。
// =========================================================================

/// `Delay` 的"可注入时钟"版本。结构和 M9b 的 `Delay` 一致,
/// 但 `Instant::now()` 换成 `clock.now()`,reactor 的 wake 换成 `manual_wake`。
pub struct VDelay {
    deadline: MockInstant,
    clock: Rc<VirtualClock>,
}

impl VDelay {
    pub fn new(clock: Rc<VirtualClock>, after: MockDuration) -> Self {
        Self {
            deadline: clock.now() + after,
            clock,
        }
    }
    pub fn deadline_ms(&self) -> u64 { self.deadline.ms }
}

impl Future for VDelay {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        // 关键:用注入的时钟,而不是 `Instant::now()`。
        if self.clock.now() >= self.deadline {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

// =========================================================================
// poll_once —— 把执行器循环拆成单步,把"下一步 poll 谁"交给测试代码。
// =========================================================================

fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}

// =========================================================================
// 测试 1:忠实顺序(模拟真实时钟)—— delay_a 先 ready,后 delay_b。
// 这是真实时钟下的唯一可能顺序。
// =========================================================================

#[test]
fn faithful_order_a_ready_before_b() {
    let clock = Rc::new(VirtualClock::new());
    let mut a = VDelay::new(clock.clone(), MockDuration::from_millis(10));
    let mut b = VDelay::new(clock.clone(), MockDuration::from_millis(20));

    // 拍 0:时钟在 0,两个都 pending。
    assert!(poll_once(&mut a).is_pending());
    assert!(poll_once(&mut b).is_pending());

    // 拍 1:推进 10ms(模拟"10ms 后 reactor wake A")。
    clock.advance(MockDuration::from_millis(10));
    assert_eq!(clock.now_ms(), 10);
    // A 现在 ready,B 还 pending。
    assert_eq!(poll_once(&mut a), Poll::Ready(()));
    assert!(poll_once(&mut b).is_pending());

    // 拍 2:再推进 10ms,wake B。
    clock.advance(MockDuration::from_millis(10));
    assert_eq!(clock.now_ms(), 20);
    assert_eq!(poll_once(&mut b), Poll::Ready(()));
}

// =========================================================================
// 测试 2:乱序 polling —— 一次性把时钟推到 20ms,然后先 poll B 再 poll A。
// 真实时钟做不到这件事(10ms 永远先到),但虚拟时钟可以。
// 这条测试钉死了"VDelay 的状态机不依赖 polling 顺序"。
// =========================================================================

#[test]
fn out_of_order_both_ready_in_one_tick_poll_b_first() {
    let clock = Rc::new(VirtualClock::new());
    let mut a = VDelay::new(clock.clone(), MockDuration::from_millis(10));
    let mut b = VDelay::new(clock.clone(), MockDuration::from_millis(20));

    // 拍 0:都 pending。
    assert!(poll_once(&mut a).is_pending());
    assert!(poll_once(&mut b).is_pending());

    // 一次性把时钟推到 20ms —— A 和 B 的 deadline 都已"过去"。
    clock.advance(MockDuration::from_millis(20));
    assert_eq!(clock.now_ms(), 20);

    // 故意先 poll B!
    assert_eq!(poll_once(&mut b), Poll::Ready(()));
    // 再 poll A,也 ready。
    assert_eq!(poll_once(&mut a), Poll::Ready(()));

    // 不变量:无论 polling 顺序,deadline 到期后的 future 一定 Ready。
    // 这条测试如果未来 VDelay 的实现退化成"被 poll 一次就消费 deadline",
    // 它会立刻 fail。
}

// =========================================================================
// 测试 3:复现 TOCTOU bug —— 同一个信号量上,两个 future 的 poll 顺序
// 决定能否触发"丢失更新"。
// =========================================================================

/// 一个有 TOCTOU bug 的信号量原型。容量 = 1。
/// `try_acquire_with_future` 在 poll 之前就改了 count,poll 后又"还回去"——
/// 如果"还回去"读到的 count 已经过期,就丢更新。
pub struct BuggySemaphore {
    count: Cell<u32>,
}

impl BuggySemaphore {
    pub fn new() -> Self { Self { count: Cell::new(1) } }
    pub fn count(&self) -> u32 { self.count.get() }

    /// 读-改-写版本,非原子。bug 在于"future 返回 Pending 时,
    /// 把许可还回去用的是'读 count → +1 → 写回'模式"。
    pub fn try_acquire_with_future(&self, f: &mut VDelay) -> Poll<bool> {
        let got = self.acquire_once();
        match poll_once(f) {
            Poll::Ready(()) => Poll::Ready(got),
            Poll::Pending => {
                // 归还。这里的 bug:如果两个 future 在同一拍交错地
                // "读 count → +1 → 写回",会丢更新。
                if got {
                    let cur = self.count.get();
                    self.count.set(cur + 1);
                }
                Poll::Pending
            }
        }
    }

    fn acquire_once(&self) -> bool {
        let c = self.count.get();
        if c >= 1 {
            self.count.set(c - 1);
            true
        } else {
            false
        }
    }
}

#[test]
fn toctou_bug_demonstration() {
    // 这条测试**故意复现** bug:同一个 sem 上,反复 poll 同一个 future,
    // 每次都触发"读 count → 改 count → 还回去"循环。
    // 关键洞察:count 在每次循环里都被错误地"扣减再归还",
    // 但因为归还用的"读-改-写"不是原子的,**count 最终可能不等于初始值**。
    //
    // 在单线程 + Cell 下,这条测试其实不能触发 bug(因为没有真交错)。
    // 它的价值是**展示代码骨架**,让读者看清"如果这段代码被并发调用,
    // 哪一步是 TOCTOU 窗口"。虚拟时钟 + 多 future 的真实复现
    // 要在多线程 reactor 场景下才能完整演示,留给读者做动手清单。

    let clock = Rc::new(VirtualClock::new());
    let sem = BuggySemaphore::new();
    let mut a = VDelay::new(clock.clone(), MockDuration::from_millis(10));

    // 不推进时钟,poll 5 次,每次都走 Pending 分支。
    for _ in 0..5 {
        let _ = sem.try_acquire_with_future(&mut a);
    }
    // 不变量:count 经过 5 次"扣减 → 归还",应当回到 1。
    // 单线程 + Cell 下确实如此。多线程下就可能不是。
    assert_eq!(sem.count(), 1, "单线程下 count 应当回到 1");

    // 推进时钟,a 应当 ready,sem 的 acquire 应当成功。
    clock.advance(MockDuration::from_millis(10));
    match sem.try_acquire_with_future(&mut a) {
        Poll::Ready(true) => { /* 期望路径 */ }
        other => panic!("deadline 到期后应当 Ready(true),得到 {:?}", other.is_ready()),
    }
    assert_eq!(sem.count(), 0, "acquire 成功后 count 应当为 0");
}

// =========================================================================
// 测试 4:属性测试骨架 —— Delay 在任意"poll / advance"序列下都满足不变量。
// =========================================================================

#[test]
fn delay_invariant_under_random_poll_sequences() {
    // 用一个固定 LCG 伪随机序列生成操作,跑 100 个不同种子。
    // 不变量:Delay 在 clock.now() < deadline 时一定 Pending;
    //        一旦 clock.now() >= deadline 且被 poll,一定 Ready。
    for seed in 1..=100u64 {
        let clock = Rc::new(VirtualClock::new());
        let mut delay = VDelay::new(clock.clone(), MockDuration::from_millis(50));

        let mut state = seed;
        let mut steps = 0;
        loop {
            // LCG 推进一步。
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let action = state % 2;
            match action {
                0 => {
                    // poll。
                    let p = poll_once(&mut delay);
                    if clock.now() >= delay.deadline_ms().into_ms_instant() {
                        assert!(matches!(p, Poll::Ready(())),
                            "seed {}: clock 已过 deadline 但 delay 没 Ready", seed);
                        break;
                    } else {
                        assert!(matches!(p, Poll::Pending),
                            "seed {}: clock 没过 deadline 但 delay Ready 了", seed);
                    }
                }
                _ => {
                    // advance。
                    clock.advance(MockDuration::from_millis(7));
                }
            }
            steps += 1;
            assert!(steps < 500, "seed {}: 死循环?", seed);
        }
    }
}

// 一个把 u64 ms 包成 MockInstant 的小辅助(为了让上面那条测试可读)。
trait MsExt { fn into_ms_instant(self) -> MockInstant; }
impl MsExt for u64 {
    fn into_ms_instant(self) -> MockInstant { MockInstant::from_millis(self) }
}
