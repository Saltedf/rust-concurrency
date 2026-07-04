//! M-cancel CancellationToken 测试：协作式取消。

use forge_core::cancel::CancellationToken;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// 一个"什么也不干"的 Waker——本测试里我们手动 poll，不需要真唤醒。
fn noop_waker() -> Waker {
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[test]
fn token_starts_uncancelled() {
    let t = CancellationToken::new();
    assert!(!t.is_cancelled());
}

#[test]
fn token_cancel_marks_flag() {
    let t = CancellationToken::new();
    t.cancel();
    assert!(t.is_cancelled());
    // 重复 cancel 是空操作
    t.cancel();
    assert!(t.is_cancelled());
}

#[test]
fn token_clone_shares_state() {
    let t = CancellationToken::new();
    let t2 = t.clone();
    t.cancel();
    assert!(t2.is_cancelled(), "克隆的 token 应共享取消状态");
}

#[test]
fn token_cooperative_loop_exits() {
    // 模拟"协作式"：worker 在循环里检查标志。
    let t = Arc::new(CancellationToken::new());
    let iter = Arc::new(AtomicUsize::new(0));

    let stop = Arc::new(AtomicBool::new(false));

    let t_worker = t.clone();
    let iter_worker = iter.clone();
    let stop_worker = stop.clone();

    let worker = std::thread::spawn(move || {
        while !t_worker.is_cancelled() {
            iter_worker.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(1));
            if stop_worker.load(Ordering::Relaxed) {
                break;
            }
        }
    });

    // 让它跑 10 ms 再取消。
    std::thread::sleep(std::time::Duration::from_millis(10));
    let count_before = iter.load(Ordering::Relaxed);
    assert!(count_before > 0, "worker 应该已经跑了几圈");
    t.cancel();
    worker.join().unwrap();
    // 取消后 iter 不再增长（或最多多一次）
    let count_after = iter.load(Ordering::Relaxed);
    assert!(
        count_after <= count_before + 1,
        "取消后 worker 应尽快退出"
    );
}

#[test]
fn cancelled_future_ready_after_cancel() {
    let t = CancellationToken::new();
    let fut = t.cancelled();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    let mut fut = std::pin::pin!(fut);

    // 未取消时：Pending
    let p1 = Pin::as_mut(&mut fut).poll(&mut cx);
    assert!(matches!(p1, Poll::Pending), "未取消时应返回 Pending");

    // 取消后：Ready
    t.cancel();
    let p2 = Pin::as_mut(&mut fut).poll(&mut cx);
    assert!(matches!(p2, Poll::Ready(())), "取消后应返回 Ready");
}

#[test]
fn cancelled_future_ready_if_already_cancelled() {
    // 快路径：token 已取消时，第一次 poll 就 Ready。
    let t = CancellationToken::new();
    t.cancel();
    let fut = t.cancelled();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    let p = Pin::as_mut(&mut fut).poll(&mut cx);
    assert!(matches!(p, Poll::Ready(())));
}

#[test]
fn cancelled_future_wakes_on_cancel_cross_thread() {
    // 跨线程验证：register waker → cancel → waker 被调用 → poll 得到 Ready。
    //
    // 用一个"递增计数"的 Waker：wake 时 fetch_add 一个共享 AtomicUsize。
    let wake_count = Arc::new(AtomicUsize::new(0));

    // 构造计数 Waker（wake 时 fetch_add）。
    let counter = wake_count.clone();
    let waker = {
        fn wake(data: *const ()) {
            unsafe {
                (*(data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
            }
        }
        fn wake_by_ref(data: *const ()) {
            unsafe {
                (*(data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
            }
        }
        fn clone_w(data: *const ()) -> RawWaker {
            RawWaker::new(data, &VTABLE)
        }
        fn drop_w(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_w, wake, wake_by_ref, drop_w);
        unsafe { Waker::from_raw(RawWaker::new(counter.as_ptr() as *const (), &VTABLE)) }
    };

    let t = Arc::new(CancellationToken::new());
    let mut fut = std::pin::pin!(t.cancelled());

    // 步骤 1：主线程 poll，注册 waker。
    let mut cx = Context::from_waker(&waker);
    let p1 = Pin::as_mut(&mut fut).poll(&mut cx);
    assert!(matches!(p1, Poll::Pending), "未取消时应 Pending");

    // 步骤 2：在另一个线程 cancel。
    let t2 = t.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        t2.cancel();
    });
    h.join().unwrap();

    // 步骤 3：waker 应被调用过。
    assert!(
        wake_count.load(Ordering::SeqCst) >= 1,
        "cancel 应该触发已注册的 waker"
    );

    // 步骤 4：再 poll 应 Ready。
    let p2 = Pin::as_mut(&mut fut).poll(&mut cx);
    assert!(matches!(p2, Poll::Ready(())));
}
