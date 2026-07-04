//! M4.1 —— 基础：两个 Arc 共享，最后一个 drop 时数据才被释放（原书测试）
use forge_core::arc::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc as StdArc;
use std::thread;

#[test]
fn shared_arc_drops_data_once() {
    static NUM_DROPS: AtomicUsize = AtomicUsize::new(0);
    struct DetectDrop;
    impl Drop for DetectDrop {
        fn drop(&mut self) {
            NUM_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    let x = Arc::new(("hello", DetectDrop));
    let y = x.clone();

    let t = thread::spawn(move || {
        assert_eq!(x.0, "hello");
    });
    assert_eq!(y.0, "hello");
    t.join().unwrap();

    // x 已随线程结束被 drop，但 y 还在 → 数据不该被释放
    assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 0);
    drop(y);
    // 现在 y 也没了 → 数据释放
    assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 1);

    let _ = StdArc::<u8>::new(0); // 仅占位，避免 unused import 误判
}
