//! M8.5 —— RCU：读快照 + copy-update 整体替换
use forge_lockfree::rcu::Rcu;

#[test]
fn rcu_read_and_update() {
    let rcu = Rcu::new(10i32);
    assert_eq!(*rcu.read(), 10);
    rcu.update(|v| v + 5);
    assert_eq!(*rcu.read(), 15);
    rcu.update(|_| 99);
    assert_eq!(*rcu.read(), 99);
}
