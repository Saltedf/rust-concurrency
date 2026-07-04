//! M7.2 —— 自建 Mutex 非竞争快速路径（单线程大量 lock/unlock，功能正确）
use forge_sync::mutex::Mutex;

#[test]
fn mutex_uncontended_many_locks() {
    let m = Mutex::new(0u64);
    for _ in 0..1_000_000 {
        *m.lock() += 1;
    }
    assert_eq!(*m.lock(), 1_000_000);
}
