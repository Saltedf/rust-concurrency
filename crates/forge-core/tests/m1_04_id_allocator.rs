//! M1.4 —— ID 分配器：fetch_add 与带溢出保护的 fetch_update
use forge_core::atomics::IdAllocator;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

#[test]
fn fetch_add_ids_are_unique() {
    let alloc = Arc::new(IdAllocator::new());
    let ids = Arc::new(std::sync::Mutex::new(HashSet::new()));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let alloc = alloc.clone();
            let ids = ids.clone();
            thread::spawn(move || {
                for _ in 0..1000 {
                    ids.lock().unwrap().insert(alloc.next_id());
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // 8000 个 ID 必须两两不同。
    assert_eq!(ids.lock().unwrap().len(), 8 * 1000);
}

#[test]
fn capped_allocator_respects_limit() {
    let alloc = IdAllocator::new();
    let max = 100;
    let mut got = Vec::new();
    for _ in 0..max {
        got.push(alloc.next_id_capped(max).unwrap());
    }
    // 用尽之后应返回 None，绝不回绕。
    assert!(alloc.next_id_capped(max).is_none());
    // 分配出的值在 1..=max 范围内（fetch_update 返回的是旧值，所以是 0..max）。
    assert!(got.iter().all(|&v| v < max));
}
