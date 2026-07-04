//! M1.9 —— SeqCst 存-载（Dekker 风格）
//!
//! 直觉上"两个线程各存 1 再读对方，不可能都读到 0"。但在 Acquire/Release 下这是**允许**的；
//! 只有 SeqCst 强制一个全局总顺序，才能保证"至少一个线程看到对方的 1"。
//!
//! 本测试跑许多轮 SeqCst，断言这个不变式始终成立。
//! （要观察 AcqRel 下"都看到 0"的违规，需用 loom 枚举交错——见教程。）
use forge_core::atomics::dekker_store_load;
use std::sync::atomic::Ordering;

#[test]
fn seqcst_forbids_both_seeing_zero() {
    for _ in 0..20_000 {
        let (a_saw_b, b_saw_a) = dekker_store_load(Ordering::SeqCst);
        // SeqCst：至少一个线程看到了对方的 1。
        assert!(
            a_saw_b || b_saw_a,
            "SeqCst 下不应两个线程都看到对方为 0"
        );
    }
}
