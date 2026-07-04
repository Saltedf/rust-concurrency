//! M3.4 —— SpinLock 跨线程保护非 Copy 数据（验证 happens-before + miri 目标）
//!
//! Acquire/Release 保证：一个线程 push 的字符，另一个线程后续 lock 后一定看得到。
use forge_core::spin::SpinLock;
use std::thread;

#[test]
fn protects_non_copy_data_across_threads() {
    let lock = SpinLock::new(String::new());
    thread::scope(|s| {
        for c in ['a', 'b', 'c', 'd'] {
            s.spawn({
                let lock = &lock; // 借用（引用是 Copy，每次迭代各取一份）
                move || {
                    // move 只搬走 c 和那个引用；lock 本体仍归外层所有
                    let mut g = lock.lock();
                    g.push(c);
                }
            });
        }
    });
    let mut chars: Vec<char> = lock.lock().chars().collect();
    chars.sort();
    assert_eq!(chars, vec!['a', 'b', 'c', 'd']);
}
