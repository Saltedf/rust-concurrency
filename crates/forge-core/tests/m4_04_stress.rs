//! M4.4 —— 压力测试 + miri 目标：多线程并发 clone/drop，最终正确释放
use forge_core::arc::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Barrier;
use std::thread;

#[test]
fn concurrent_clone_drop_deallocates_exactly_once() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct D;
    impl Drop for D {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    const N: usize = 8;
    let arc = Arc::new(D);
    let start = Barrier::new(N + 1);

    thread::scope(|s| {
        let start = &start; // 共享引用（Copy），让每个 move 闭包各取一份
        for _ in 0..N {
            let c = arc.clone();
            s.spawn(move || {
                let _g = start.wait();
                for _ in 0..1000 {
                    let _cc = c.clone(); // 反复 clone/drop 制造竞争
                }
                drop(c); // 线程那份在此 drop
            });
        }
        let _g = start.wait();
    });
    drop(arc); // 主线程那份；scope 结束后所有线程已 join

    assert_eq!(DROPS.load(Ordering::Relaxed), 1, "恰好释放一次");
}
