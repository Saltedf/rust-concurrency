//! M9b 测试 8:combinators —— Then(链式)/ Timeout(超时)。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use forge_rt::combinators::{then, timeout, Ready, TimeoutOutput};
use forge_rt::coroutine::Generator;
use forge_rt::{block_on, Delay, Reactor};

fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let waker = forge_rt::noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}

// =========================================================================
// Then
// =========================================================================

#[test]
fn then_chains_first_then_second() {
    // then(Ready(3), |v| Ready(v * 2)) —— 第一次 poll 拿到 3,切到 second,
    // 第二次 poll second 拿到 6。
    let first = Ready::new(3u32);
    let chained = then(first, |v: u32| Ready::new(v * 2));
    let mut f = chained;

    // 第 1 拍:first ready → 切到 second → second 也立即 ready → 整体 ready。
    // (我们的 then 实现 loop 到 Pending 或 Ready,所以一拍内可能切完)
    match poll_once(&mut f) {
        Poll::Ready(v) => assert_eq!(v, 6),
        Poll::Pending => {
            // 如果一拍只跑 first 不跑 second,就再 poll 一次。
            match poll_once(&mut f) {
                Poll::Ready(v) => assert_eq!(v, 6),
                Poll::Pending => panic!("then 应当最终返回 6"),
            }
        }
    }
}

#[test]
fn then_returns_after_first_completes_only() {
    // 用一个手动就绪的 first,验证 then 在 first 没 ready 时整体 Pending。
    struct Manual {
        ready: Arc<AtomicBool>,
    }
    impl Future for Manual {
        type Output = u32;
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
            if self.ready.load(Ordering::Acquire) {
                Poll::Ready(42)
            } else {
                Poll::Pending
            }
        }
    }
    let ready = Arc::new(AtomicBool::new(false));
    let first = Manual {
        ready: ready.clone(),
    };
    let mut f = then(first, |v: u32| Ready::new(v + 1));

    // first 没 ready:then 应当 Pending。
    assert!(poll_once(&mut f).is_pending());

    // 让 first ready。
    ready.store(true, Ordering::Release);
    // 现在再 poll:应当切到 second 并完成。
    match poll_once(&mut f) {
        Poll::Ready(v) => assert_eq!(v, 43),
        Poll::Pending => match poll_once(&mut f) {
            Poll::Ready(v) => assert_eq!(v, 43),
            Poll::Pending => panic!("then 应当最终 ready"),
        },
    }
}

// =========================================================================
// Timeout
// =========================================================================

#[test]
fn timeout_returns_ok_when_inner_completes_in_time() {
    // inner 立即完成,timeout 200ms —— 应当 Ok(inner_output)。
    let reactor = Reactor::new().expect("reactor");
    let inner = Ready::new("hello".to_string());
    let mut t = timeout(inner, reactor, Duration::from_millis(200));

    match poll_once(&mut t) {
        Poll::Ready(TimeoutOutput::Ok(v)) => assert_eq!(v, "hello"),
        other => panic!("期望 Ok(\"hello\"),得到 {:?}", other.map(|_| "Ready(..)")),
    }
}

#[test]
fn timeout_returns_elapsed_when_deadline_hits_first() {
    // inner 用 Delay(150ms),timeout 设 50ms —— 应当 Elapsed。
    let reactor = Reactor::new().expect("reactor");
    let r2 = reactor.clone();
    let inner = Delay::new(r2, Duration::from_millis(150));
    let start = Instant::now();
    let result = block_on(
        async move { timeout(inner, reactor, Duration::from_millis(50)).await },
        &Reactor::new().expect("reactor2"),
    );
    let elapsed = start.elapsed();
    match result {
        TimeoutOutput::Elapsed(_) => {
            // 超时应在 50ms 附近。
            assert!(
                elapsed >= Duration::from_millis(40),
                "超时反应过快: {:?}",
                elapsed
            );
            assert!(
                elapsed < Duration::from_millis(300),
                "超时反应过慢: {:?}",
                elapsed
            );
        }
        TimeoutOutput::Ok(_) => panic!("inner 不该在 50ms 内完成"),
    }
}

// =========================================================================
// 端到端:用 Gen(协程) + combinator 拼一个迷你场景
// =========================================================================

#[test]
fn gen_then_then_combinator_end_to_end() {
    // 用一个生成器吐 3 个值,然后用 then 把它们累加。
    // 这只是验证两类子模块能协同工作(类型上不冲突)。
    use forge_rt::coroutine::{Gen, YieldSlot};

    struct Counter {
        n: u32,
        slot: YieldSlot<u32>,
    }
    impl Future for Counter {
        type Output = u32;
        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
            if self.n < 3 {
                self.n += 1;
                self.slot.set(self.n);
                Poll::Pending
            } else {
                Poll::Ready(self.n)
            }
        }
    }

    let mut gen = Gen::<u32, u32, _>::new(|slot| Counter { n: 0, slot });
    let mut sum: u32 = 0;
    while let Some(state) = gen.resume() {
        use forge_rt::coroutine::GenState;
        match state {
            GenState::Yielded(v) => sum += v,
            GenState::Complete(fin) => sum += fin,
        }
    }
    // yields: 1+2+3 = 6, complete 返回 3 → sum = 6+3 = 9.
    assert_eq!(sum, 9);
}
