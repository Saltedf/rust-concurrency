//! M1.8 —— compare_exchange_weak 的 CAS 循环等价于 fetch_add
use forge_core::atomics::cas_add;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn cas_loop_matches_fetch_add() {
    let v = Arc::new(AtomicU64::new(0));
    let n_threads = 8;
    let per = 50_000u64;

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let v = v.clone();
            thread::spawn(move || {
                for _ in 0..per {
                    cas_add(&v, 1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(v.load(Ordering::Relaxed), n_threads as u64 * per);
}
