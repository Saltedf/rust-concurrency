//! M9a-par:ParIter(rayon 风格)测试。

use forge_pool::par::ParIter;
use forge_pool::StealingPool;
use std::sync::Arc;

#[test]
fn par_iter_sum_basic() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..10_000).collect();
    let total = ParIter::from_slice(&input).sum(&pool);
    assert_eq!(total, (0..10_000i64).sum::<i64>());
}

#[test]
fn par_iter_map_filter_sum() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..10_000).collect();
    let total = ParIter::from_slice(&input)
        .map(|x| x * 2)
        .filter(|x| x % 3 == 0)
        .sum(&pool);
    let expected: i64 = (0..10_000i64).map(|x| x * 2).filter(|x| x % 3 == 0).sum();
    assert_eq!(total, expected);
}

#[test]
fn par_iter_chained_maps() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..5_000).collect();
    let total = ParIter::from_slice(&input)
        .map(|x| x + 1)
        .map(|x| x * x)
        .map(|x| x - 1)
        .sum(&pool);
    let expected: i64 = (0..5_000i64)
        .map(|x| x + 1)
        .map(|x| x * x)
        .map(|x| x - 1)
        .sum();
    assert_eq!(total, expected);
}

#[test]
fn par_iter_empty() {
    let pool = Arc::new(StealingPool::new(2));
    let empty: Vec<i64> = vec![];
    let total = ParIter::from_slice(&empty).map(|x| x + 1).sum(&pool);
    assert_eq!(total, 0);
}

#[test]
fn par_iter_filter_drops_all() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..2_000).collect();
    let total = ParIter::from_slice(&input).filter(|_| false).sum(&pool);
    assert_eq!(total, 0);
}

#[test]
fn par_iter_for_each_counts() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..8_000).collect();
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = counter.clone();
    ParIter::from_slice(&input).for_each(&pool, move |_| {
        c2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 8_000);
}
