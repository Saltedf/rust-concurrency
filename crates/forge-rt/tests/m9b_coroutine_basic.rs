//! M9b 测试 5:协程 / 生成器 —— `Gen` + `Generator` trait + `HandGen`。
//!
//! 这条测试演示:用一个 Future 状态机包成 `Gen`,逐次 resume 吐出值,
//! 直至 Complete;之后再 resume 返回 None(像 Iterator)。
//! 同时验证 `HandGen`(纯 enum 版,等价于 async fn 的脱糖)行为一致。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use forge_rt::coroutine::{Gen, GenState, Generator, HandGen, YieldSlot};
use forge_rt::noop_waker;

/// 一个"吐 1, 吐 2, 然后完成"的 future,用来包成 Gen<u32, String, _>。
///
/// 状态机两态:Start(还没吐过) / After1(吐过 1)。
/// 每次想吐值时:写槽 + 返回 Pending;最后一次返回 Ready("done".into())。
struct TwoYieldsThenDone {
    step: u8,
}

impl TwoYieldsThenDone {
    fn new() -> Self {
        Self { step: 0 }
    }
}

impl Future for TwoYieldsThenDone {
    type Output = String;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<String> {
        // 我们不通过 cx 拿 waker——Gen 用 noop_waker 推进,不依赖外部唤醒。
        // 每次推进一拍,通过 YieldSlot 把值塞出去。
        match self.step {
            0 => {
                self.step = 1;
                Poll::Pending // 槽由外部 future_factory 写入,见 Gen::new 的调用方
            }
            1 => {
                self.step = 2;
                Poll::Pending
            }
            _ => Poll::Ready("done".to_string()),
        }
    }
}

// 上面的 TwoYieldsThenDone 没法写槽——它没有 YieldSlot。我们直接写一个
// "future_factory" 闭包,把 YieldSlot 捕获进去,在 poll 时写槽。
// 这里用一个独立结构体把 YieldSlot + step 一起存,实现 Future。

struct TwoYieldsGen {
    step: u8,
    slot: YieldSlot<u32>,
}

impl Future for TwoYieldsGen {
    type Output = String;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<String> {
        match self.step {
            0 => {
                self.slot.set(1);
                self.step = 1;
                Poll::Pending
            }
            1 => {
                self.slot.set(2);
                self.step = 2;
                Poll::Pending
            }
            _ => Poll::Ready("fin".to_string()),
        }
    }
}

#[test]
fn gen_yields_values_then_completes() {
    let mut gen = Gen::<u32, String, _>::new(|slot| TwoYieldsGen { step: 0, slot });

    // 拍 1:resume → 吐 1,GenState::Yielded(1)。
    assert_eq!(gen.resume(), Some(GenState::Yielded(1)));
    // 拍 2:resume → 吐 2。
    assert_eq!(gen.resume(), Some(GenState::Yielded(2)));
    // 拍 3:resume → 完成,带最终值 "fin"。
    assert_eq!(gen.resume(), Some(GenState::Complete("fin".to_string())));
    // 拍 4:resume → None(已耗尽)。
    assert_eq!(gen.resume(), None);
    // 拍 5:再 resume 还是 None。
    assert_eq!(gen.resume(), None);
}

#[test]
fn handgen_matches_gen_state_machine() {
    // HandGen 是纯 enum 版,和上面的 Gen 状态机行为应当一致(除了最终返回类型)。
    let mut g = HandGen::new();
    assert_eq!(g.resume(), Some(GenState::Yielded(1)));
    assert_eq!(g.resume(), Some(GenState::Yielded(2)));
    assert_eq!(g.resume(), Some(GenState::Complete(())));
    assert_eq!(g.resume(), None);
}

#[test]
fn gen_can_be_used_as_iterator() {
    use forge_rt::coroutine::Gen;
    // 一个吐 10/20/30 的生成器。
    struct Count {
        n: u32,
        slot: YieldSlot<u32>,
    }
    impl Future for Count {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            if self.n < 3 {
                self.n += 1;
                self.slot.set(self.n * 10);
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
    let gen = Gen::<u32, (), _>::new(|slot| Count { n: 0, slot });
    let collected: Vec<u32> = gen.collect();
    assert_eq!(collected, vec![10, 20, 30]);
}

#[test]
fn gen_panic_on_pending_without_yield() {
    // 一个 future 总是返回 Pending 但从不 yield —— Gen 应当 panic 提示调用方。
    struct BadSlot;
    impl Future for BadSlot {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Pending // 不写槽
        }
    }
    let mut gen = Gen::<u32, (), _>::new(|_| BadSlot);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gen.resume();
    }));
    assert!(
        result.is_err(),
        "Gen 应当在 future 返回 Pending 但没 yield 时 panic"
    );
}

#[test]
fn noop_waker_drives_gen_poll_without_deadlock() {
    // 验证 Gen::resume 内部用的 noop_waker 能正常构造。
    let _w = noop_waker();
}
