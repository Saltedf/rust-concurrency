//! M4.3 —— Weak：upgrade / downgrade，弱指针不阻止数据释放（原书第二个测试）
use forge_core::arc::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

#[test]
fn weak_pointer_upgrade_and_expiry() {
    static NUM_DROPS: AtomicUsize = AtomicUsize::new(0);
    struct DetectDrop;
    impl Drop for DetectDrop {
        fn drop(&mut self) {
            NUM_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    let x = Arc::new(("hello", DetectDrop));
    let y = Arc::downgrade(&x);
    let z = Arc::downgrade(&x);

    let t = thread::spawn(move || {
        let y = y.upgrade().unwrap(); // 还有 Arc ⇒ 能升级
        assert_eq!(y.0, "hello");
    });
    assert_eq!(x.0, "hello");
    t.join().unwrap();

    assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 0);
    assert!(z.upgrade().is_some());

    drop(x); // 最后一个 Arc 没了

    assert_eq!(NUM_DROPS.load(Ordering::Relaxed), 1); // 数据已释放
    assert!(z.upgrade().is_none()); // 弱指针升级失败
}
