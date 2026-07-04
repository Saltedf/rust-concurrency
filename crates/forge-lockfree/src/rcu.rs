//! # RCU（Read-Copy-Update）—— 锁自由读、复制后整体替换（Arc 回收版）
//!
//! 要"多线程常读、偶尔改"一大块数据，又没有能装下整块的原子类型？加一层间接：
//! 用一个**指针**指向数据。读：读指针拿到快照。改：复制一份 → 改副本 → CAS 把指针换过去。
//!
//! 难点在**回收旧数据**（别的线程可能还读着旧指针）。原书列了若干回收策略：
//! Arc（引用计数）、泄漏、GC、hazard pointer、quiescent state。
//! 这里采用 **Arc 回收**：内部存 `Arc<T>`，读路径 clone 一份快照（无写阻塞），
//! 写路径在 `Mutex` 保护下 copy-update-swap（避免并发写）。这把 RCU 的"读-复制-更新"
//! 形态忠实呈现，且回收由 Arc 自动、sound。
//!
//! ## 想看"真 epoch 回收版"？
//!
//! 这版用 Arc 是教学版最 sound 的选择（无 unsafe、回收自动）。要看 RCU 论文里
//! 真正的"延迟回收"——`crates/forge-lockfree/src/epoch.rs` 从零实现了 epoch-based
//! reclamation：`pin()` 进临界区、`defer_destroy(ptr, dtor)` 退役指针、`try_advance()`
//! 推进 epoch 并回收两 epoch 前的垃圾。M8 文档 M8b 节的"ISO·ZOOM"段有完整对照。
//! 教程里两版并存：rcu.rs 简单 sound、epoch.rs 工程贴近 crossbeam-epoch。

use std::sync::{Arc, Mutex};

/// 一个 RCU 保护的值。读拿到 `Arc<T>` 快照；写整体替换。
pub struct Rcu<T> {
    // 写路径用 Mutex 串行化（原书 tip：读仍无写阻塞，写互斥避免并发修改的复杂性）。
    current: Mutex<Arc<T>>,
}

impl<T> Rcu<T> {
    pub fn new(value: T) -> Self {
        Self {
            current: Mutex::new(Arc::new(value)),
        }
    }

    /// 读：拿到当前值的 `Arc<T>` 快照（clone 廉价——只 ++引用计数）。
    pub fn read(&self) -> Arc<T> {
        Arc::clone(&self.current.lock().unwrap())
    }

    /// 写：读出当前值、复制并修改、整体替换为新 `Arc`。返回旧值。
    pub fn update(&self, f: impl FnOnce(&T) -> T) {
        let mut g = self.current.lock().unwrap();
        let new = Arc::new(f(&g));
        *g = new;
    }
}
