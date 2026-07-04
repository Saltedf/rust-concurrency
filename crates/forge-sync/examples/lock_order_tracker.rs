//! M11e —— thread-local 锁序校验器原型。
//!
//! 演示 Williams《C++ Concurrency in Action》Ch11 的"运行时锁层级"思路:
//! 强制每把锁有唯一 ID,加锁时检查"新锁 ID 严格大于栈顶 ID",从而禁止循环等待。
//!
//! 这个原型**不改动** forge-sync 的 src 代码,只是一个独立示例。
//! 跑法:cargo run -p forge-sync --example lock_order_tracker
//!
//! 详见 docs/modules/M11-testing.md 第五节。

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// 全局唯一 ID 分配器:每把"逻辑锁"领一个 ID。
/// fetch_add 保证多线程并发 new 也不会撞号。
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// 线程本地:当前持有的锁的 ID 栈(从底到顶,严格递增)。
// 一旦发现新锁 ID 不大于栈顶,立刻 panic——锁序被打破。
thread_local! {
    static LOCK_STACK: RefCell<Vec<u64>> = RefCell::new(Vec::new());
}

/// 一把"被追踪的锁":领取 ID,加锁时压栈,解锁时弹栈。
pub struct TrackedLock {
    id: u64,
    inner: std::sync::Mutex<()>,
}

impl TrackedLock {
    pub fn new(name: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        println!("[trace] 注册锁 {name:?} → id={id}");
        TrackedLock {
            id,
            inner: std::sync::Mutex::new(()),
        }
    }

    pub fn lock(&self) -> TrackedGuard<'_> {
        // 关键检查:新锁的 ID 必须严格大于栈顶(否则违反升序)
        LOCK_STACK.with(|s| {
            let stack = s.borrow();
            if let Some(&top) = stack.last() {
                if self.id <= top {
                    panic!(
                        "锁序违规!当前栈顶锁 id={},试图获取 id={} (后者应更大)",
                        top, self.id
                    );
                }
            }
        });
        let _g = self.inner.lock().unwrap();
        LOCK_STACK.with(|s| s.borrow_mut().push(self.id));
        TrackedGuard {
            id: self.id,
            _inner: _g,
        }
    }
}

/// Guard:Drop 时弹栈,验证栈的一致性。
pub struct TrackedGuard<'a> {
    id: u64,
    _inner: std::sync::MutexGuard<'a, ()>,
}

impl<'a> Drop for TrackedGuard<'a> {
    fn drop(&mut self) {
        LOCK_STACK.with(|s| {
            let mut stack = s.borrow_mut();
            let popped = stack.pop();
            debug_assert_eq!(popped, Some(self.id), "锁栈被破坏");
        });
    }
}

fn main() {
    // 先注册两把锁:id 会按调用顺序分配——先 new 的 id 小。
    let lock_low = TrackedLock::new("db_row");
    let lock_high = TrackedLock::new("file_handle");

    // 场景 1:正确的顺序(低 ID → 高 ID)
    println!("--- 场景 1:正确顺序(低 → 高) ---");
    {
        let _g1 = lock_low.lock();
        let _g2 = lock_high.lock();
        println!("  两把锁都拿到了(顺序正确,无 panic)");
    }

    // 场景 2:错误的顺序(高 → 低,违规!) —— 会 panic,我们 catch 之
    println!("--- 场景 2:违规顺序(高 → 低) ---");
    let result = std::panic::catch_unwind(|| {
        let _g1 = lock_high.lock(); // id 大,先拿到
        let _g2 = lock_low.lock(); // id 小,违反升序 → panic!
    });
    assert!(result.is_err(), "应当 panic");
    println!("  (预期内的 panic:锁序校验器抓到了违规)");

    println!("\n=== 演示结束 ===");
    println!("局限:这个原型只追踪单线程的栈,看不到\"线程1持A等B,线程2持B等A\"这种跨线程死锁。");
    println!("      要抓跨线程死锁,需要全局维护一张\"等待图\"并检测环(Williams Ch11 提到的算法)。");
}
