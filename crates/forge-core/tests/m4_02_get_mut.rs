//! M4.2 —— get_mut：唯一 Arc 时拿到 &mut T；有克隆时返回 None
use forge_core::arc::Arc;

#[test]
fn get_mut_only_when_unique() {
    let mut a = Arc::new(42);
    // 唯一 → 能拿到 &mut
    *Arc::get_mut(&mut a).unwrap() = 99;
    assert_eq!(*a, 99);

    // 有了克隆 → 不再唯一
    let _b = a.clone();
    assert!(Arc::get_mut(&mut a).is_none());
    drop(_b);
    // 克隆没了 → 又能了
    assert!(Arc::get_mut(&mut a).is_some());
}
