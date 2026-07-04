//! # parking-lot 式锁 —— 1 字节锁 + 全局"停车场"
//!
//! 把"等待队列"从锁里挪到一个**全局 HashMap**（地址 → 等待线程队列），锁本身极小。
//! WebKit 2015 年首创，`parking_lot` crate 沿用；它也等于在"没有 futex 的平台"上
//! 自己实现了 futex。
//!
//! 教学版（全局 `Mutex<HashMap>`）。正确性靠三件事配合（与 M2 的 park 模式同构）：
//! ① 等待者**先登记进停车场，再二次检查锁状态**；② 解锁后**总是查停车场唤醒一个**；
//! ③ `thread::park` 的"unpark 不丢失"。三者合起来保证唤醒不丢。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, Thread};

const UNLOCKED: u8 = 0;
const LOCKED: u8 = 1;

fn global_lot() -> &'static Mutex<HashMap<usize, Vec<Thread>>> {
    static LOT: OnceLock<Mutex<HashMap<usize, Vec<Thread>>>> = OnceLock::new();
    LOT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct ParkingLotMutex {
    state: AtomicU8,
}

impl ParkingLotMutex {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNLOCKED),
        }
    }

    fn addr(&self) -> usize {
        self as *const Self as usize
    }

    pub fn lock(&self) {
        // 快速路径：UNLOCKED→LOCKED。
        if self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        self.lock_slow();
    }

    #[cold]
    fn lock_slow(&self) {
        let addr = self.addr();
        let me = thread::current();
        loop {
            // 每轮先试快速路径
            if self
                .state
                .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            // ① 先把自己登记进停车场
            {
                let mut lot = global_lot().lock().unwrap();
                lot.entry(addr).or_default().push(me.clone());
            }
            // ② 二次检查：若已解锁，摘掉自己、回去重抢（避免错过解锁）
            if self.state.load(Ordering::Acquire) == UNLOCKED {
                let mut lot = global_lot().lock().unwrap();
                if let Some(q) = lot.get_mut(&addr) {
                    q.retain(|t| t.id() != me.id());
                    if q.is_empty() {
                        lot.remove(&addr);
                    }
                }
                continue;
            }
            // ③ 睡。park 的"unpark 不丢失"兜住"登记后、park 前"被唤醒的情形。
            thread::park();
        }
    }

    pub fn unlock(&self) {
        // 先解锁（Release 发布临界区），再总是查停车场唤醒一个等待者。
        self.state.store(UNLOCKED, Ordering::Release);
        let woken = {
            let mut lot = global_lot().lock().unwrap();
            lot.get_mut(&self.addr()).and_then(|q| q.pop())
        };
        if let Some(t) = woken {
            t.unpark();
        }
    }
}
