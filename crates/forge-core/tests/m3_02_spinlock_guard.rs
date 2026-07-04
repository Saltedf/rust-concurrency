//! M3.2 —— SpinLock 守卫像 &mut T：直接调用 Vec::push，drop 自动解锁
//! （原书第 4 章末尾的示例）
use forge_core::spin::SpinLock;
use std::thread;

#[test]
fn spinlock_vec_push_via_guard() {
    let x = SpinLock::new(Vec::<i32>::new());
    thread::scope(|s| {
        s.spawn(|| x.lock().push(1)); // 守卫是临时量，语句结束即 drop → 解锁
        s.spawn(|| {
            let mut g = x.lock();
            g.push(2);
            g.push(2);
        });
    });
    let g = x.lock();
    let slice = g.as_slice();
    // 两个线程谁先谁后不确定，但内容必须是 {1,2,2} 的某种排列
    assert!(slice == [1, 2, 2] || slice == [2, 2, 1], "got {slice:?}");
}
