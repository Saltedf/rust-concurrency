//! # M8h：倒计数门闩（Latch）与可重用屏障（Barrier）
//!
//! 这两个原语来自 C++ Concurrency in Action 第 4 章，专门解决"**让多个线程
//! 在某个汇合点对齐**"这一类问题。它们都比 mutex 更轻——没有"所有权"概念，
//! 只用一两个原子加 atomic-wait 的 `wait`/`wake_all`。
//!
//! - [`Latch`]：**一次性**倒计数门闩。初始化为 N，N 次 `count_down` 之后打开，
//!   打开后永远打开（直到被 drop）。适合"等所有 worker 把数据生成完"。
//! - [`Barrier`]：**可重用**屏障。N 个线程到齐后一起放行，下一轮还能继续用。
//!   比 Latch 多一个"代次（generation）"计数，防止"上一轮慢到的线程被误算进下一轮"。
//!
//! 两者配方上的同构点：**先改原子值，再 wake_all**——这与 M6 / M7 一脉相承。

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

/// 复用 forge-sync 已统一封装好的"地址等待"三件套。
use forge_sync::atomic_wait::{wait, wake_all};

// =====================================================================
//                              Latch
// =====================================================================

/// 倒计数门闩。一次性——计数到 0 后永远打开，不再重置。
///
/// 内部只有一个 `AtomicU32`：剩余计数。它一身二职：
/// - 数值本身就是"还差几次 count_down"的答案；
/// - 它的地址是 atomic-wait 的"床位"，`wait` 的线程睡在这里。
pub struct Latch {
    count: AtomicU32,
}

impl Latch {
    /// 新建一个需要 `n` 次 `count_down` 才能打开的门闩。
    /// `n == 0` 表示一开始就打开。
    pub fn new(n: u32) -> Self {
        Latch {
            count: AtomicU32::new(n),
        }
    }

    /// 剩余计数。0 表示门闩已打开。
    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }

    /// 若门闩已打开返回 true；否则 false。不阻塞。
    pub fn is_open(&self) -> bool {
        self.count.load(Ordering::Acquire) == 0
    }

    /// 把计数减一。若减到 0，唤醒所有睡在 `wait` 上的线程。
    ///
    /// # 内存序配方（与 M4 Arc drop 同构）
    ///
    /// - `fetch_sub` 用 **Release**：count_down 之前写下的所有数据（worker 的产出）
    ///   必须先对其它核心可见，然后"我减完了"这件事才能被看到。
    /// - `wait` 一侧用 **Acquire** load：看到 0 的线程，也就同步看到了所有 worker
    ///   在 count_down 之前的全部写入。
    ///
    /// 这跟 Arc 最后一次 `fetch_sub` 配 fence(Acquire) 是同一个骨架：**"释放方
    /// 用 Release 减计数，获取方用 Acquire 看到计数归零"**，建立 happens-before。
    pub fn count_down(&self) {
        // Release：把我之前的写入"打包"塞进这条减法。
        let prev = self.count.fetch_sub(1, Ordering::Release);
        // prev 是减之前的值。prev == 1 表示我是把计数从 1 减到 0 的那个人。
        if prev == 1 {
            // 由我来广播唤醒。醒来者会重新 load（在 wait 里），它们 Acquire load
            // 看到 0，就同步到了我这条 Release——和 M4 Arc 末次 drop 完全同构。
            wake_all(&self.count);
        }
        // prev == 0 不应发生（不能 count_down 到负数）。与 std::latch 一致，
        // 这里不做饱和检查——滥用是调用方的事，保持实现最薄。
    }

    /// 阻塞当前线程，直到门闩打开（计数归 0）。
    ///
    /// 必须循环：atomic-wait 的 `wait` 可能假唤醒。
    pub fn wait(&self) {
        // Acquire：和 count_down 的 Release 配对。
        loop {
            let cur = self.count.load(Ordering::Acquire);
            if cur == 0 {
                return;
            }
            // 把"刚读到的值"传给 wait：atomic-wait 内部会原子地检查值是否仍是 cur。
            //   - 如果仍是 cur，则睡进内核；
            //   - 如果已被改过，则立刻返回。
            // 于是 wake 永远不会卡在"已检查但还没睡"的缝隙里丢失（M6 的核心不变量）。
            wait(&self.count, cur);
        }
    }
}

impl std::fmt::Debug for Latch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Latch")
            .field("count", &self.count.load(Ordering::Relaxed))
            .finish()
    }
}

// =====================================================================
//                            Barrier
// =====================================================================

/// 可重用的线程汇合屏障。N 个线程到齐后一起放行；下一轮还能继续用。
///
/// 与 Latch 的关键差别：**Barrier 会自动重置**，于是可以被同一组线程反复使用。
/// 这带来一个 Latch 没有的陷阱——"代次（generation）混淆"：一个上一轮慢到的
/// 线程，可能看到计数已经被下一轮重置回 N，误以为自己到齐了，或在错误的轮次
/// 被唤醒。解决办法是把"代次"和"剩余计数"一起编码进同一个原子。
///
/// # 编码方案
///
/// 用一个 `AtomicU32`：低 30 位是"剩余计数"，高 2 位是"代次 mod 4"。
/// 2 位足够区分相邻两轮（leader 推进代次前，所有同伴都在 sleep 当前代的某个值上；
/// 推进到 mod 4 的下一档时，旧代与新代永远不等）。每完成一轮，代次 +1（绕回 0）。
///
/// `wait` 时记住"我进入时看到的代次"，然后 fetch_sub 减一。如果减后计数归 0，
/// 我是 leader，推进代次、重置计数、wake_all。否则睡在"当前 state 值"上，
/// 醒来重 load，若代次已变则放行，否则继续睡。
pub struct Barrier {
    n: u32,
    /// 高 2 位 = 代次（mod 4），低 30 位 = 还在等的线程数。
    state: AtomicU32,
}

const GEN_SHIFT: u32 = 30;
const COUNT_MASK: u32 = (1u32 << GEN_SHIFT) - 1; // 低 30 位全 1
const GEN_MASK: u32 = 0b11;

#[inline]
fn pack(gen: u32, count: u32) -> u32 {
    debug_assert!(count < (1u32 << GEN_SHIFT));
    ((gen & GEN_MASK) << GEN_SHIFT) | (count & COUNT_MASK)
}

#[inline]
fn unpack(s: u32) -> (u32, u32) {
    ((s >> GEN_SHIFT) & GEN_MASK, s & COUNT_MASK)
}

impl Barrier {
    /// 新建一个需要 `n` 个线程到齐才放行的屏障。`n` 必须 ≥ 1。
    pub fn new(n: u32) -> Self {
        assert!(n >= 1, "Barrier 至少需要 1 个线程");
        assert!(n < (1u32 << GEN_SHIFT), "Barrier n 过大");
        Barrier {
            n,
            state: AtomicU32::new(pack(0, n)),
        }
    }

    /// 当前代次已经到达的线程数（仅用于调试/断言）。
    pub fn arrived(&self) -> u32 {
        let (_, count) = unpack(self.state.load(Ordering::Acquire));
        self.n.saturating_sub(count)
    }

    /// 到达屏障，阻塞直到本批 N 个线程全部到齐，然后一起返回。
    ///
    /// 返回值：`true` 表示你是这一批里**最后到达**的那个线程（"leader"/代言人），
    /// 可用它做一些只需一个线程做的清理（与 std::sync::Barrier 同义）。
    pub fn wait(&self) -> bool {
        // Acquire：和上一轮 leader 的 Release store 配对——能看到上一轮所有线程
        // 在 wait 之前的写入。
        let s = self.state.load(Ordering::Acquire);
        let (gen, _count) = unpack(s);

        // 把剩余计数减一。fetch_sub 只动计数位，不动代次位——因为减的是
        // COUNT_MASK 范围内的数，且我们的 n < 2^30，不会进位到代次位。
        // Release：我在 wait 之前的写入，必须先于同伴醒来后被看到。
        let prev = self.state.fetch_sub(1, Ordering::Release);
        let (prev_gen, prev_count) = unpack(prev);

        // fetch_sub 之前先 load 过 gen。如果有人在这期间推进了代次（即 prev_gen != gen），
        // 说明我已经迟到，本轮已经被 leader 放行——直接返回（罕见但必须处理）。
        if prev_gen != gen {
            // 代次已变：本轮已放行。我不是 leader。
            return false;
        }

        // 我是 leader 吗？prev_count == 1 表示减之前只剩 1 个名额（就是我）。
        if prev_count == 1 {
            // 推进代次、重置计数为 N、wake_all。
            let next_gen = (gen + 1) & GEN_MASK;
            // Release：同伴醒来后用 Acquire load 读到这个新值，就同步到了
            // 我推进代次之前的所有写入（也含所有同伴 wait 之前的写入）。
            self.state.store(pack(next_gen, self.n), Ordering::Release);
            wake_all(&self.state);
            return true;
        }

        // 不是 leader：睡，直到代次被推进。
        // 把当前 state 重新 load 作为 expected：它编码了"我这一代的当前计数"。
        // atomic-wait 的契约保证：若 state 被改（包括 leader 推进代次），
        // wait 立刻返回去重 load——于是代次推进一定触发醒来。
        loop {
            let cur = self.state.load(Ordering::Acquire);
            let (cur_gen, _) = unpack(cur);
            if cur_gen != gen {
                // 代次变了——本轮已放行，我可以走了。
                return false;
            }
            wait(&self.state, cur);
        }
    }
}

impl std::fmt::Debug for Barrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (gen, count) = unpack(self.state.load(Ordering::Relaxed));
        f.debug_struct("Barrier")
            .field("n", &self.n)
            .field("gen", &gen)
            .field("remaining", &count)
            .finish()
    }
}
