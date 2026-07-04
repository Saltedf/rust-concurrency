//! M9b 测试 7:combinators —— Race / Join / Then。
//!
//! 这条测试用一个"手动就绪 future"(Manual<T>)演示 Race 和 Join 的 poll 行为。
//! Manual 的就绪靠外部 Arc<AtomicBool> 控制,不依赖 reactor/线程时序,
//! 所以测试是确定性的(逐拍推进)。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use forge_rt::combinators::{
    join, race, RaceOutput, Ready,
};
use forge_rt::noop_waker;

/// "手动就绪" future:外部 flip 一下 AtomicBool,下次 poll 返回 Ready(v)。
/// poll 时把 waker clone 一份存到 Arc<Mutex<Option<Waker>>>,这样测试线程
/// 能在 flip ready 之后调 wake(),模拟"reactor 唤醒"。
struct Manual<T: Clone + Unpin + Send + 'static> {
    ready: Arc<AtomicBool>,
    value: T,
    polled: Arc<AtomicUsize>,
    waker_slot: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}

impl<T: Clone + Unpin + Send + 'static> Future for Manual<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        // 记录被 poll 的次数。
        self.polled.fetch_add(1, Ordering::Relaxed);
        // 存 waker,让测试线程能 wake。
        *self.waker_slot.lock().unwrap() = Some(cx.waker().clone());
        if self.ready.load(Ordering::Acquire) {
            Poll::Ready(self.value.clone())
        } else {
            Poll::Pending
        }
    }
}

fn make_manual<T: Clone + Unpin + Send + 'static>(value: T) -> (
    Manual<T>,
    Arc<AtomicBool>,           // ready flag
    Arc<AtomicUsize>,          // poll counter
    Arc<std::sync::Mutex<Option<std::task::Waker>>>,
) {
    let ready = Arc::new(AtomicBool::new(false));
    let polled = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(std::sync::Mutex::new(None));
    let m = Manual {
        ready: ready.clone(),
        value,
        polled: polled.clone(),
        waker_slot: waker.clone(),
    };
    (m, ready, polled, waker)
}

/// 用 noop_waker 手动 poll 一个 future,模拟执行器的一拍。
fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}

#[test]
fn race_left_ready_first_wins() {
    let (a, ready_a, polled_a, _) = make_manual(10u32);
    let (b, _ready_b, polled_b, _) = make_manual(20u32);

    let mut race = race(a, b);

    // 拍 1:两边都还没就绪 → Pending。
    assert!(poll_once(&mut race).is_pending());
    // 两边都被 poll 了一次。
    assert_eq!(polled_a.load(Ordering::Relaxed), 1);
    assert_eq!(polled_b.load(Ordering::Relaxed), 1);

    // 让 A 就绪。
    ready_a.store(true, Ordering::Release);

    // 拍 2:A 应当 Ready。race 返回 Left(10, b)。
    match poll_once(&mut race) {
        Poll::Ready(RaceOutput::Left(v, _loser_b)) => {
            assert_eq!(v, 10);
        }
        other => panic!("期望 Left 赢,得到 {:?}", other.map(|_| "Ready(..)")),
    }
}

#[test]
fn race_right_ready_first_wins() {
    let (a, _ready_a, _, _) = make_manual(100u32);
    let (b, ready_b, _, _) = make_manual(200u32);

    let mut race = race(a, b);
    // 先 poll 一次让两边注册。
    assert!(poll_once(&mut race).is_pending());
    ready_b.store(true, Ordering::Release);

    match poll_once(&mut race) {
        Poll::Ready(RaceOutput::Right(v, _loser_a)) => assert_eq!(v, 200),
        other => panic!("期望 Right 赢,得到 {:?}", other.map(|_| "Ready(..)")),
    }
}

#[test]
fn join_returns_both_when_ready_in_opposite_order() {
    // 演示"join 必须存住先 ready 的那一边的结果":让 B 先 ready、A 后 ready。
    let (a, ready_a, polled_a, _) = make_manual("a".to_string());
    let (b, ready_b, polled_b, _) = make_manual("b".to_string());

    let mut j = join(a, b);

    // 拍 1:两边都 pending → 整体 Pending。
    assert!(poll_once(&mut j).is_pending());
    assert_eq!(polled_a.load(Ordering::Relaxed), 1);
    assert_eq!(polled_b.load(Ordering::Relaxed), 1);

    // 让 B 先 ready,A 还没。
    ready_b.store(true, Ordering::Release);

    // 拍 2:B 应当 ready → 但 A 还没,整体仍 Pending。
    assert!(poll_once(&mut j).is_pending());
    // B 这次被 poll(并 ready),A 也被 poll(仍 pending)。
    assert_eq!(polled_a.load(Ordering::Relaxed), 2);
    assert_eq!(polled_b.load(Ordering::Relaxed), 2);

    // 让 A 也 ready。
    ready_a.store(true, Ordering::Release);

    // 拍 3:A 也 ready → 两槽都满 → 整体 Ready(("a", "b"))。
    match poll_once(&mut j) {
        Poll::Ready((va, vb)) => {
            assert_eq!(va, "a");
            assert_eq!(vb, "b");
        }
        Poll::Pending => panic!("两边都 ready 了,join 应当返回 Ready"),
    }
}

#[test]
fn join_with_immediate_futures_returns_immediately() {
    // 两个 Ready 都立即就绪:join 第一次 poll 就应当 Ready。
    let a = Ready::new(1u32);
    let b = Ready::new(2u32);
    let mut j = join(a, b);
    match poll_once(&mut j) {
        Poll::Ready((x, y)) => {
            assert_eq!(x, 1);
            assert_eq!(y, 2);
        }
        Poll::Pending => panic!("两个立即 future 的 join 应当首拍 Ready"),
    }
}

#[test]
fn race_with_immediate_left_does_not_poll_right() {
    // A 立即就绪:race 第一拍就 Left。B 不应被 poll(实现里先 poll A,见 Ready 立即就绪)。
    // 注:当前实现里先 poll A,A Ready 就直接返回 —— B 不被 poll。
    let a = Ready::new("instant".to_string());
    let (b, _ready_b, polled_b, _) = make_manual("never".to_string());
    let b_polled = polled_b.clone();

    let mut r = race(a, b);
    match poll_once(&mut r) {
        Poll::Ready(RaceOutput::Left(v, _)) => assert_eq!(v, "instant"),
        other => panic!("期望 Left 立即赢,得到 {:?}", other.map(|_| "Ready(..)")),
    }
    assert_eq!(b_polled.load(Ordering::Relaxed), 0, "B 不应被 poll");
}
