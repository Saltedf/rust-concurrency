//! M8.2 —— SeqLock：读者永远读到一致的 (a,a)，不会撕裂成 (a, a-1)
use forge_lockfree::seqlock::SeqLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn seqlock_no_torn_reads() {
    let sl = Arc::new(SeqLock::<(u64, u64)>::new((0, 0)));
    let stop = Arc::new(AtomicBool::new(false));

    let w = {
        let sl = sl.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            for i in 1..=200_000u64 {
                let mut g = sl.write();
                g.0 = i;
                g.1 = i;
            }
            stop.store(true, Ordering::Release);
        })
    };

    // 读者：不断读，必须永远 a==b（无撕裂）
    while !stop.load(Ordering::Acquire) {
        let (a, b) = sl.read();
        assert_eq!(a, b, "撕裂读！seqlock 失效");
        assert!(a <= 200_000);
    }
    w.join().unwrap();
    let (a, b) = sl.read();
    assert_eq!((a, b), (200_000, 200_000));
}
