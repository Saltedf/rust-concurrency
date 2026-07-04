//! M8.3 —— Treiber 无锁栈：LIFO + 多线程 push 后全部 pop
use forge_lockfree::stack::Stack;
use std::thread;

#[test]
fn stack_lifo_single_thread() {
    let s = Stack::<i32>::new();
    s.push(1);
    s.push(2);
    s.push(3);
    assert_eq!(s.pop(), Some(3));
    assert_eq!(s.pop(), Some(2));
    assert_eq!(s.pop(), Some(1));
    assert_eq!(s.pop(), None);
}

#[test]
fn stack_concurrent_push_then_drain() {
    let s = Stack::<u32>::new();
    thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for i in 0..1000u32 {
                    s.push(i);
                }
            });
        }
    });
    let mut count = 0;
    while s.pop().is_some() {
        count += 1;
    }
    assert_eq!(count, 4000);
}
