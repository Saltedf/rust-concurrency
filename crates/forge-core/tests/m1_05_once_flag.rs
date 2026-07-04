//! M1.5 —— OnceFlag：用 CAS 保证闭包在所有线程中恰好执行一次
use forge_core::atomics::OnceFlag;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn closure_runs_exactly_once() {
    let flag = Arc::new(OnceFlag::new());
    let calls = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..100)
        .map(|_| {
            let flag = flag.clone();
            let calls = calls.clone();
            thread::spawn(move || {
                flag.call_once(|| {
                    calls.fetch_add(1, Ordering::Relaxed);
                });
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(calls.load(Ordering::Relaxed), 1, "闭包必须恰好执行一次");
    assert!(flag.is_done());
}
