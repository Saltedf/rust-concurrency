//! M8h Barrier 测试：可重用屏障、多轮汇合、generation 不混淆。

use forge_lockfree::latch::Barrier;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[test]
fn barrier_single_thread() {
    let b = Barrier::new(1);
    let leader = b.wait();
    assert!(leader, "n=1 时唯一线程应是 leader");
}

#[test]
fn barrier_releases_all() {
    let b = Arc::new(Barrier::new(4));
    let crossed = Arc::new(AtomicUsize::new(0));
    let leader_count = Arc::new(AtomicUsize::new(0));

    let mut hs = vec![];
    for _ in 0..4 {
        let b = b.clone();
        let crossed = crossed.clone();
        let leader_count = leader_count.clone();
        hs.push(std::thread::spawn(move || {
            let is_leader = b.wait();
            if is_leader {
                leader_count.fetch_add(1, Ordering::SeqCst);
            }
            crossed.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(crossed.load(Ordering::SeqCst), 4);
    assert_eq!(leader_count.load(Ordering::SeqCst), 1, "每轮恰好一个 leader");
}

#[test]
fn barrier_multiple_rounds() {
    // 4 个线程跑 3 轮，每轮全员到齐才放行。
    let b = Arc::new(Barrier::new(4));
    let rounds = Arc::new(AtomicUsize::new(0));

    let mut hs = vec![];
    for _ in 0..4 {
        let b = b.clone();
        let rounds = rounds.clone();
        hs.push(std::thread::spawn(move || {
            for _ in 0..3 {
                b.wait();
                // 到齐后才能各自 +1
                rounds.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    // 4 线程 × 3 轮 = 12 次通过
    assert_eq!(rounds.load(Ordering::SeqCst), 12);
}

#[test]
fn barrier_phase_ordering() {
    // 验证屏障的"汇合"语义：phase1 全部完成之前，没人能开始 phase2。
    let b = Arc::new(Barrier::new(3));
    let phase1_done = Arc::new(AtomicUsize::new(0));
    let in_phase2 = Arc::new(AtomicUsize::new(0));

    let mut hs = vec![];
    for _ in 0..3 {
        let b = b.clone();
        let p1 = phase1_done.clone();
        let p2 = in_phase2.clone();
        hs.push(std::thread::spawn(move || {
            // phase 1
            std::thread::sleep(std::time::Duration::from_millis(10));
            p1.fetch_add(1, Ordering::SeqCst);
            b.wait();
            // phase 2 开始——此时所有线程的 phase1 都完成了
            // (因为 barrier 保证全员到齐才放行)
            assert_eq!(
                p1.load(Ordering::SeqCst),
                3,
                "进入 phase2 时 phase1 必须全部完成"
            );
            p2.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(in_phase2.load(Ordering::SeqCst), 3);
}

#[test]
fn barrier_stress_fast_slow() {
    // 制造快慢线程交错，验证 generation 不混淆。
    // 3 个线程 × 5 轮，其中线程 0 每轮故意慢一拍。
    let b = Arc::new(Barrier::new(3));
    let ok = Arc::new(AtomicUsize::new(0));

    let mut hs = vec![];
    for tid in 0..3 {
        let b = b.clone();
        let ok = ok.clone();
        hs.push(std::thread::spawn(move || {
            for r in 0..5 {
                if tid == 0 {
                    // 慢线程
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                b.wait();
                ok.fetch_add(1, Ordering::SeqCst);
                let _ = r;
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(ok.load(Ordering::SeqCst), 3 * 5);
}
