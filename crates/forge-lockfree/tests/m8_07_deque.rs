//! M8.7 —— Chase-Lev 工作窃取双端队列：push N，owner pop + stealer steal，
//! 取出的集合恰好是 0..N 的一个排列（无丢失、无重复）——这是 Chase-Lev 的核心不变量。
use forge_lockfree::deque::{Deque, Steal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

#[test]
fn deque_no_loss_no_duplication() {
    let deq = Arc::new(Deque::<u32>::new());
    let stealer = deq.stealer();
    const N: u32 = 2000;

    for i in 0..N {
        deq.push(i);
    }

    let collected = Arc::new(Mutex::new(Vec::<u32>::new()));
    let stop = Arc::new(AtomicBool::new(false));

    // 两个 stealer 偷
    let hs: Vec<_> = (0..2)
        .map(|_| {
            let stealer = stealer.clone();
            let collected = collected.clone();
            let deq = deq.clone();
            let stop = stop.clone();
            thread::spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match stealer.steal() {
                    Steal::Success(v) => collected.lock().unwrap().push(v),
                    Steal::Empty => {
                        if deq.pop().is_none() && stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                    Steal::Retry => {}
                }
            })
        })
        .collect();

    // owner 也 pop 余下的
    while let Some(v) = deq.pop() {
        collected.lock().unwrap().push(v);
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }

    let mut v = collected.lock().unwrap().clone();
    v.sort_unstable();
    v.dedup();
    assert_eq!(
        v.len() as u32,
        N,
        "应有 {N} 个不重复值，实际 {}",
        collected.lock().unwrap().len()
    );
    assert_eq!(v, (0..N).collect::<Vec<_>>());
}
