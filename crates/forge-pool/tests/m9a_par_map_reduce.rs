//! M9a-par:par_map / par_reduce 测试。

use forge_pool::par::{par_map, par_reduce};
use forge_pool::StealingPool;
use std::sync::Arc;

#[test]
fn par_map_basic() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..10_000).collect();
    let out = par_map(&pool, &input, |x| x * x);
    let expected: Vec<i64> = input.iter().map(|x| x * x).collect();
    assert_eq!(out, expected);
}

#[test]
fn par_map_empty() {
    let pool = Arc::new(StealingPool::new(2));
    let out: Vec<i64> = par_map(&pool, &[], |x: &i64| x * 2);
    assert!(out.is_empty());
}

#[test]
fn par_map_small() {
    // 小于 chunk cutoff:整段一个任务,但结果仍正确。
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i32> = vec![1, 2, 3, 4, 5];
    let out = par_map(&pool, &input, |x| x + 100);
    assert_eq!(out, vec![101, 102, 103, 104, 105]);
}

#[test]
fn par_map_preserves_order() {
    // 不同段在不同 worker 上跑、以不同时间完成——但写入位置由 start 决定,
    // 顺序必须保留。这一测试专门检查"位置正确性"。
    let pool = Arc::new(StealingPool::new(8));
    let input: Vec<i64> = (0..20_000).collect();
    let out = par_map(&pool, &input, |&x| x);
    assert_eq!(out, input);
}

#[test]
fn par_reduce_sum() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (1..=10_000).collect();
    let total = par_reduce(&pool, &input, || 0i64, |acc, x| acc + x, |a, b| a + b);
    // 1+2+...+10000 = 10000*10001/2 = 50005000
    assert_eq!(total, 5_000_5_000);
}

#[test]
fn par_reduce_max() {
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<i64> = (0..10_000)
        .map(|i| (i as i64).wrapping_mul(2654435761))
        .collect();
    let expected = input.iter().copied().max().unwrap();
    let got = par_reduce(
        &pool,
        &input,
        || i64::MIN,
        |acc, x| if *x > acc { *x } else { acc },
        |a, b| if a > b { a } else { b },
    );
    assert_eq!(got, expected);
}

#[test]
fn par_reduce_empty() {
    let pool = Arc::new(StealingPool::new(2));
    let empty: Vec<i64> = vec![];
    let got = par_reduce(&pool, &empty, || 0i64, |acc, x| acc + x, |a, b| a + b);
    assert_eq!(got, 0);
}

#[test]
fn par_reduce_string_concat() {
    // 非数值 U,验证 reduce 的通用性(只要 init/step/merge 类型对得上)。
    let pool = Arc::new(StealingPool::new(4));
    let input: Vec<String> = (0..2000).map(|i| format!("{i},")).collect();
    let got = par_reduce(
        &pool,
        &input,
        || String::new(),
        |acc, x| acc + x,
        |a, b| a + &b,
    );
    // 拼接长度 = 2000 段 × 每段长度
    let expected_len: usize = (0..2000).map(|i| format!("{i},").len()).sum();
    assert_eq!(got.len(), expected_len);
}
