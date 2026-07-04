//! # 信号量（Semaphore）—— 一个原子计数器
//!
//! 信号量就是一个计数器：`acquire`（wait/P/down）减一、为零则阻塞；
//! `release`（signal/V/up）加一（有上限）并唤醒等待者。极灵活——
//! **二元信号量（max=1）初始化为 1 就是 mutex，初始化为 0 就是 `park`/`unpark` 那样的信号**。
//! 这正是"停止位 = 延迟初始化位 = 通道就绪位 = 一元信号量"这副**终极同构骨架**。
//!
//! 这里用单个 `AtomicU32` + atomic-wait（futex）实现：非竞争路径只有一次 CAS。

use atomic_wait::{wait, wake_one};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Semaphore {
    /// 当前可用许可数（≥0）。
    permits: AtomicU32,
    /// 当前"已入睡或正要入睡"的等待者数。release 据此决定要不要 wake_one。
    ///
    /// 为什么需要它:旧版 release 只在 `fetch_add` 看到 old==0 时唤醒,
    /// 多等待者场景下会丢唤醒——
    ///   permits=0,T1/T2 都睡;release(0→1,wake T1);release(1→2,old≠0,**不 wake**);
    ///   T1 拿走许可(permits=1),T2 却永远睡。详见 tests/m8a_semaphore_regression.rs。
    /// 用 num_waiters 计数器后:只要还有登记的等待者,release 总会 wake_one。
    num_waiters: AtomicU32,
}

impl Semaphore {
    /// 创建一个有 `permits` 个初始许可的信号量。
    pub const fn new(permits: u32) -> Self {
        Self {
            permits: AtomicU32::new(permits),
            num_waiters: AtomicU32::new(0),
        }
    }

    /// 获取一个许可；若当前为 0 则阻塞，直到有人 release。
    pub fn acquire(&self) {
        loop {
            let n = self.permits.load(Ordering::Relaxed);
            if n > 0 {
                // 有许可：CAS 减一。失败（别人抢了）就重试。
                if self
                    .permits
                    .compare_exchange_weak(n, n - 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
            } else {
                // 没许可：准备入睡。**先登记**(Release),再复查 permits,再 wait。
                // 顺序很关键:登记在复查之前,保证这期间任何 release 都能从
                // num_waiters 看到我们在等 → wake_one;而我们随后复查 permits,
                // 若 release 已经把 permits 抬上去,就根本不睡(atomic-wait 的
                // expected 机制兜底:permits≠0 时 wait 立即返回)。
                self.num_waiters.fetch_add(1, Ordering::Release);
                if self.permits.load(Ordering::Acquire) == 0 {
                    // 仅当仍是 0 才睡（futex expected 机制防丢失唤醒）。
                    wait(&self.permits, 0);
                }
                // 醒来(或根本没睡)→ 注销等待者,回到循环顶部重试 CAS。
                self.num_waiters.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// 归还一个许可。只要有登记的等待者，就唤醒一个。
    pub fn release(&self) {
        self.permits.fetch_add(1, Ordering::Release);
        // 用 Acquire 读 num_waiters:与 acquire 登记时的 Release 配对,确保我们
        // 看到等待者登记后,他的等待状态是真实的。若 >0,必有(或将有)人在
        // futex 队列里;wake_one 把其中一个(若有)叫醒,其余靠后续 release。
        if self.num_waiters.load(Ordering::Acquire) > 0 {
            wake_one(&self.permits);
        }
    }
}
