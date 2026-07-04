//! M1.1 —— 停止位 StopFlag
//!
//! 教程目标：一个后台线程能被另一个线程礼貌叫停，且共享标志不会数据竞争。
use forge_core::atomics::StopFlag;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn background_loop_stops_when_flagged() {
    let flag = Arc::new(StopFlag::new());
    let ticks = Arc::new(AtomicUsize::new(0));

    let f = flag.clone();
    let t = ticks.clone();
    let handle = thread::spawn(move || {
        while !f.is_stopped() {
            t.fetch_add(1, Ordering::Relaxed);
        }
    });

    // 让后台线程跑一会儿。
    thread::sleep(std::time::Duration::from_millis(20));
    flag.stop();
    handle.join().unwrap();

    assert!(
        ticks.load(Ordering::Relaxed) > 0,
        "后台线程应当已经跑过若干轮"
    );
    assert!(flag.is_stopped());
}
