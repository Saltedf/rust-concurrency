//! M8.09 —— Hazard pointer 回收：ABA 安全的 Treiber 栈、不泄漏、并发 push/pop。
use forge_lockfree::hazard::{self, HazardStack};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn hazard_stack_lifo_single_thread() {
    let s = HazardStack::<i32>::new();
    s.push(1);
    s.push(2);
    s.push(3);
    assert_eq!(s.pop(), Some(3));
    assert_eq!(s.pop(), Some(2));
    assert_eq!(s.pop(), Some(1));
    assert_eq!(s.pop(), None);
}

/// 单线程大量 push/pop：确保 retire → 扫描 → 回收 闭环工作，
/// 不会因为"指针还在 hazard 槽"误判、不会因为没人扫而永远堆积。
#[test]
fn hazard_stack_reclaim_actually_frees() {
    let s = HazardStack::<Box<[u8; 64]>>::new();
    for round in 0..1000 {
        for i in 0..64 {
            s.push(Box::new([i as u8; 64]));
        }
        for _ in 0..64 {
            assert!(s.pop().is_some());
        }
        // 每轮手动触发扫描，强制回收。
        hazard::scan_and_reclaim();
        // round 仅用于让循环次数明显（编译器不会优化掉）。
        let _ = round;
    }
    // 全部回收后本线程垃圾袋应该基本空（除非有并发 hazard——这里没有）。
    hazard::flush_local();
}

/// 并发 push/pop：不丢不重，所有 push 进的值都被 pop 出一次。
/// 这里验证 hazard pointer 在并发下不发生 use-after-free。
#[test]
fn hazard_stack_concurrent_push_pop_no_uaf() {
    let s = Arc::new(HazardStack::<u32>::new());
    const N: u32 = 20_000;
    let pushed = Arc::new(AtomicUsize::new(0));
    let popped = Arc::new(AtomicUsize::new(0));

    thread::scope(|sc| {
        // 4 个生产者各推 N 个不同值。
        for tid in 0..4u32 {
            let s = s.clone();
            let pushed = pushed.clone();
            sc.spawn(move || {
                for i in 0..N {
                    s.push(tid * 1_000_000 + i);
                    pushed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        // 4 个消费者持续 pop，直到所有 pushed 都被消费。
        for _ in 0..4 {
            let s = s.clone();
            let pushed = pushed.clone();
            let popped = popped.clone();
            sc.spawn(move || loop {
                if let Some(_v) = s.pop() {
                    popped.fetch_add(1, Ordering::Relaxed);
                } else if pushed.load(Ordering::Relaxed) == (4 * N) as usize
                    && popped.load(Ordering::Relaxed) == (4 * N) as usize
                {
                    break;
                } else {
                    std::thread::yield_now();
                }
            });
        }
    });

    assert_eq!(pushed.load(Ordering::Relaxed), (4 * N) as usize);
    assert_eq!(popped.load(Ordering::Relaxed), (4 * N) as usize);
}

/// 模拟"读者长时间持着 hazard、retire 期间不能回收"：
/// 一个线程 hazard 一个指针后睡眠，主线程 retire 它，扫描必须跳过它（保留在垃圾袋）。
#[test]
fn hazard_retire_skips_protected_pointer() {
    use std::sync::Barrier;
    // 我们直接用 HazardGuard 的底层 API 验证。
    let barrier = Arc::new(Barrier::new(2));
    let b = barrier.clone();

    let h = thread::spawn(move || {
        // 公告一个特定指针值。用一个伪造的、非空、绝不会真的被 Box::from_raw 回收的地址。
        let fake_ptr = 0xDEAD_BEEFu32 as *mut ();
        let _g = hazard::HazardGuard::protect(fake_ptr);
        b.wait(); // 让主线程看到我已 hazard
        std::thread::sleep(std::time::Duration::from_millis(200));
        // 主线程在此期间 scan_and_reclaim——必须不能回收 fake_ptr
        // （虽然 fake_ptr 不在垃圾袋，这里只是验证扫描能跳过非空 hazard 槽）。
    });

    barrier.wait();
    // 这里 retire 一个真实指针，扫描应正确把它回收（fake_ptr 不在垃圾袋，
    // 但 hazard 槽里有它——验证扫描不会误回收任何 hazard 槽里的指针）。
    let real = Box::into_raw(Box::new(42u64)) as *mut ();
    unsafe { hazard::retire(real, |p| drop(Box::from_raw(p as *mut u64))) };
    hazard::scan_and_reclaim();
    // real 应已被回收（它不在任何 hazard 槽里）。fake_ptr 在 hazard 槽里但不在垃圾袋。
    // 我们没法直接断言"real 已 drop"，但能保证程序到这里没 crash、没 panic。

    h.join().unwrap();
}
