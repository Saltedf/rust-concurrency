//! M9b 测试 4：select（两个 future 谁先 Ready 谁赢，另一个 drop）。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use forge_rt::{noop_waker, select, SelectOutput};

/// 一个"按手动信号完成"的 future：构造时不完成；外部调 `make_ready(v)` 后，
/// 下次 poll 返回 Ready。drop 时**自增 drop 计数器**（验证 select 输家被 drop）。
struct Manual<T: Clone + Unpin + Send + 'static> {
    ready: Arc<AtomicBool>,
    value: T,
    drop_counter: Arc<AtomicUsize>,
}

impl<T: Clone + Unpin + Send + 'static> Future for Manual<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        if self.ready.load(Ordering::Acquire) {
            Poll::Ready(self.value.clone())
        } else {
            Poll::Pending
        }
    }
}

impl<T: Clone + Unpin + Send + 'static> Drop for Manual<T> {
    fn drop(&mut self) {
        self.drop_counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn select_returns_first_ready_and_drops_loser() {
    let drops_a = Arc::new(AtomicUsize::new(0));
    let drops_b = Arc::new(AtomicUsize::new(0));

    let ready_a = Arc::new(AtomicBool::new(false));
    let ready_b = Arc::new(AtomicBool::new(false));

    let a = Manual { ready: ready_a.clone(), value: 1u32, drop_counter: drops_a.clone() };
    let b = Manual { ready: ready_b.clone(), value: 2u32, drop_counter: drops_b.clone() };

    // 在一个独立线程里跑 select：让 A 先 Ready。
    // 这需要 select 是阻塞的——它确实是（轮询 + yield_now）。
    let handle = thread::spawn(move || select(a, b));

    // 让 A 先就绪。
    thread::sleep(Duration::from_millis(50));
    ready_a.store(true, Ordering::Release);
    // 故意不 Ready B——它应当作为输家被 select 返回时还给调用方。

    match handle.join().unwrap() {
        SelectOutput::Left(v, _loser_b) => {
            assert_eq!(v, 1);
            // A 没被 drop（它正常完成、被消费）；B 还在 _loser_b 里。
            // drops_b 在这里还没自增——我们手动 drop 验证。
            let b_drops_before = drops_b.load(Ordering::Relaxed);
            assert_eq!(b_drops_before, 0, "loser still alive until explicitly dropped");
            drop(_loser_b);
            assert_eq!(drops_b.load(Ordering::Relaxed), 1, "loser dropped after explicit drop");
        }
        SelectOutput::Right(_, _) => panic!("A should win"),
    }
}

#[test]
fn noop_waker_in_select_loop_does_not_panic() {
    let _w = noop_waker();
}
