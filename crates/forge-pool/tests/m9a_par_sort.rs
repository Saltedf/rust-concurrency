//! M9a-par:并行快排测试。
//!
//! 覆盖:
//! - 小数组(走串行 cutoff 分支)
//! - 大随机数组(走并行分治)
//! - 已排序数组(测 pivot 选择是否避免平方退化)
//! - 全相等等重复元素(测三路 partition)
//! - 奇数长度 / 偶数长度边界
//! - 结果与 std::sort 完全一致(逐元素比对)

use forge_pool::{par::par_sort, StealingPool};
use std::sync::Arc;

#[test]
fn par_sort_small_array() {
    // 小于 PAR_SORT_CUTOFF(1024),走串行基线分支——但仍然要正确。
    let pool = Arc::new(StealingPool::new(4));
    let mut a = vec![5, 3, 8, 1, 9, 2, 7, 4, 6, 0];
    let mut expected = a.clone();
    expected.sort();
    par_sort(&pool, &mut a);
    assert_eq!(a, expected);
}

#[test]
fn par_sort_large_random() {
    let pool = Arc::new(StealingPool::new(4));
    // 大数组:64k 元素,远超 cutoff,走完整并行分治。
    let mut a: Vec<i64> = (0..65_536i64)
        .map(|i| (i.wrapping_mul(2654435761)) ^ 0x9E3779B9)
        .collect();
    let mut expected = a.clone();
    expected.sort();
    par_sort(&pool, &mut a);
    assert_eq!(a, expected);
}

#[test]
fn par_sort_already_sorted() {
    // 已排序数组:naive pivot(取首元素)会让 partition 完全失衡、退化到 O(n²)。
    // 三数取中 pivot 应避免这种退化。
    let pool = Arc::new(StealingPool::new(4));
    let mut a: Vec<i32> = (0..8_000).collect();
    let expected = a.clone();
    par_sort(&pool, &mut a);
    assert_eq!(a, expected);
}

#[test]
fn par_sort_all_equal() {
    // 全相等:三路 partition 应一次性把所有元素归位为 == pivot,
    // 左右子段为空,递归立刻终止。
    let pool = Arc::new(StealingPool::new(4));
    let mut a = vec![7i32; 10_000];
    let expected = a.clone();
    par_sort(&pool, &mut a);
    assert_eq!(a, expected);
}

#[test]
fn par_sort_empty_and_single() {
    let pool = Arc::new(StealingPool::new(2));
    let mut empty: Vec<i32> = vec![];
    par_sort(&pool, &mut empty);
    assert!(empty.is_empty());

    let mut single = vec![42];
    par_sort(&pool, &mut single);
    assert_eq!(single, vec![42]);
}

#[test]
fn par_sort_two_elements() {
    let pool = Arc::new(StealingPool::new(2));
    let mut a = vec![2, 1];
    par_sort(&pool, &mut a);
    assert_eq!(a, vec![1, 2]);

    let mut b = vec![1, 2];
    par_sort(&pool, &mut b);
    assert_eq!(b, vec![1, 2]);
}

#[test]
fn par_sort_stress_many_runs() {
    // 跑多次,增加撞上 race / 死锁 / 数据竞争的概率。
    let pool = Arc::new(StealingPool::new(8));
    for seed in 0..20u64 {
        let mut a: Vec<i64> = (0..2000)
            .map(|i| {
                let x = (seed << 32) ^ i;
                (x.wrapping_mul(2654435761) as i64).abs()
            })
            .collect();
        let mut expected = a.clone();
        expected.sort();
        par_sort(&pool, &mut a);
        assert_eq!(a, expected, "seed {seed} mismatched");
    }
}

#[test]
fn par_sort_does_not_deadlock_on_deep_recursion() {
    // 深度递归 + 嵌套 spawn:测 V3 的"recv 不 park、边等边跑"。
    // 如果死锁,测试会在默认 60s 超时挂掉——这是我们要的失败信号。
    let pool = Arc::new(StealingPool::new(2));
    let mut a: Vec<i64> = (0..50_000).map(|i| (i as i64).wrapping_mul(31)).collect();
    let mut expected = a.clone();
    expected.sort();
    par_sort(&pool, &mut a);
    assert_eq!(a, expected);
}
