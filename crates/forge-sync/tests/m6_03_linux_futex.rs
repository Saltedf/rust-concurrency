//! M6.3 —— Linux 手写 futex：和高层封装行为一致（仅 Linux）
#![cfg(target_os = "linux")]

use forge_sync::linux_futex::{wait, wake_one};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

#[test]
fn raw_futex_wait_wake() {
    let a = AtomicU32::new(0);
    thread::scope(|s| {
        s.spawn(|| {
            a.store(1, Relaxed);
            wake_one(&a);
        });
        while a.load(Relaxed) == 0 {
            wait(&a, 0);
        }
    });
    assert_eq!(a.load(Relaxed), 1);
}
