//! M2.7 —— 锁中毒（lock poisoning）：持锁线程 panic 后，锁被标记为"中毒"
//!
//! 注意：scoped 线程 panic 会让 `thread::scope` 在结束时重新抛出 panic，
//! 所以这里用 `catch_unwind` 接住，才能在事后检查 `m` 是否中毒。
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::thread;

#[test]
fn panic_while_locked_poisons_the_mutex() {
    let m = Mutex::new(0);
    // 一个 scoped 线程持锁后 panic —— 这会让 m 中毒
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        thread::scope(|s| {
            s.spawn(|| {
                let _g = m.lock().unwrap();
                panic!("boom");
            });
        });
    }));

    // 现在 m 已中毒：lock() 返回 Err（Err 里内含 guard，可借此恢复不一致状态）
    let result = Mutex::lock(&m);
    assert!(result.is_err(), "持锁线程 panic 后，锁应当中毒");
    // 中毒状态下仍能拿到数据：PoisonError::into_inner 取出 guard，再解引用
    let guard = result.unwrap_err().into_inner();
    assert_eq!(*guard, 0);
}
