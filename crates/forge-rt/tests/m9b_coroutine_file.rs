//! M9b 测试 6:协程"读大文件流"——把 Gen 当成"能暂停的 reader"。
//!
//! 模拟 Maxwell Ch5 的例子:一个生成器每次 resume 读一行,
//! 避免一次性把整个文件加载到内存。
//! 这里用一个内存里的"假文件"(一个 `Vec<String>`),验证生成器逐行吐出。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use forge_rt::coroutine::{Gen, GenState, Generator};

/// 一个"按行吐值"的生成器 future。每次 poll 拿一行放进槽,返回 Pending;
/// 行用尽时返回 Ready(())。
struct LineReader {
    lines: Vec<String>,
    pos: usize,
    slot: forge_rt::coroutine::YieldSlot<String>,
}

impl Future for LineReader {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.pos < self.lines.len() {
            let line = self.lines[self.pos].clone();
            self.pos += 1;
            self.slot.set(line);
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

#[test]
fn gen_streams_lines_one_at_a_time() {
    let data = vec![
        "first".to_string(),
        "second".to_string(),
        "third".to_string(),
    ];
    let mut gen = Gen::<String, (), _>::new(|slot| LineReader {
        lines: data.clone(),
        pos: 0,
        slot,
    });

    // 逐行吐。
    assert_eq!(gen.resume(), Some(GenState::Yielded("first".to_string())));
    assert_eq!(gen.resume(), Some(GenState::Yielded("second".to_string())));
    assert_eq!(gen.resume(), Some(GenState::Yielded("third".to_string())));
    // 然后 Complete。
    assert_eq!(gen.resume(), Some(GenState::Complete(())));
    assert_eq!(gen.resume(), None);
}

#[test]
fn gen_as_iterator_yields_all_lines() {
    let data: Vec<String> = (0..5).map(|i| format!("line-{i}")).collect();
    let gen = Gen::<String, (), _>::new(|slot| LineReader {
        lines: data.clone(),
        pos: 0,
        slot,
    });
    let collected: Vec<String> = gen.collect();
    assert_eq!(collected, data);
}

#[test]
fn gen_with_no_yields_completes_immediately() {
    // 一个永远 Ready 的 future:生成器第一次 resume 就返回 Complete。
    struct ImmediatelyDone;
    impl Future for ImmediatelyDone {
        type Output = u32;
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
            Poll::Ready(42)
        }
    }
    let mut gen = Gen::<u32, u32, _>::new(|_| ImmediatelyDone);
    assert_eq!(gen.resume(), Some(GenState::Complete(42)));
    assert_eq!(gen.resume(), None);
}
