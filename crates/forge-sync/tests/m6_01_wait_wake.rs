//! M6.1 —— 地址等待：一个线程 wait，另一个改值 + wake
//!
//! 无论时序如何都正确：要么等待者还没 wait（load 到新值，不睡），
//! 要么它在睡（被 wake 叫醒）。这正是 futex "检查+入睡原子" 的威力。
use forge_sync::atomic_wait::{wait, wake_one};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

#[test]
fn wait_then_wake() {
    let a = AtomicU32::new(0);
    thread::scope(|s| {
        s.spawn(|| {
            a.store(1, Relaxed);
            wake_one(&a);
        });
        while a.load(Relaxed) == 0 {
            wait(&a, 0); // 仍为 0 就睡；被唤醒后重新检查
        }
    });
    assert_eq!(a.load(Relaxed), 1);
}
