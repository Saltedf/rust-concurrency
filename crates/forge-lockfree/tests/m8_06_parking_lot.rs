//! M8.6 —— parking-lot 式锁：互斥（8 线程各 1000 → 8000）
use forge_lockfree::parking_lot::ParkingLotMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn parking_lot_mutex_mutual_exclusion() {
    let m = Arc::new(ParkingLotMutex::new());
    let counter = AtomicI64::new(0);
    thread::scope(|s| {
        for _ in 0..8 {
            let m = m.clone();
            let counter = &counter;
            s.spawn(move || {
                for _ in 0..1000 {
                    m.lock();
                    counter.fetch_add(1, Ordering::Relaxed);
                    m.unlock();
                }
            });
        }
    });
    assert_eq!(counter.load(Ordering::Relaxed), 8000);
}
