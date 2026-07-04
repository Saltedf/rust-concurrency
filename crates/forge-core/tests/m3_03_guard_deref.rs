//! M3.3 —— 守卫的 Deref/DerefMut + Drop 自动解锁
use forge_core::spin::SpinLock;

#[test]
fn guard_deref_mut_and_auto_unlock() {
    let lock = SpinLock::new(5);
    {
        let mut g = lock.lock();
        *g = 42; // DerefMut：像 &mut T 一样写
        assert_eq!(*g, 42); // Deref：像 &T 一样读
    } // g drop → 自动解锁
    assert_eq!(*lock.lock(), 42); // 再次上锁能看到修改
}
