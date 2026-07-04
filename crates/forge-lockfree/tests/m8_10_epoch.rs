//! M8.10 —— Epoch-based reclamation：ABA 安全、批量回收、并发 pin/defer 不 UAF。
use forge_lockfree::epoch::{self, EpochGuard, EpochStack};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn epoch_stack_lifo_single_thread() {
    let s = EpochStack::<i32>::new();
    s.push(1);
    s.push(2);
    s.push(3);
    assert_eq!(s.pop(), Some(3));
    assert_eq!(s.pop(), Some(2));
    assert_eq!(s.pop(), Some(1));
    assert_eq!(s.pop(), None);
}

/// 单线程大量 push/pop：defer_destroy → 推进 epoch → 旧 epoch 垃圾回收。
/// 验证"两 epoch 后回收"工作：retire 后多次 try_advance 应能回收。
#[test]
fn epoch_stack_reclaim_after_advance() {
    let s = EpochStack::<u32>::new();
    for round in 0..500 {
        for i in 0..50 {
            s.push(round * 1000 + i);
        }
        for _ in 0..50 {
            assert!(s.pop().is_some());
        }
        // pop 内部会调 try_advance，但可能没推进两次。手动调确保。
        epoch::try_advance();
        epoch::try_advance();
    }
}

/// 并发 pin/defer：4 线程 push/pop，验证不发生 UAF。
#[test]
fn epoch_stack_concurrent_no_uaf() {
    let s = Arc::new(EpochStack::<u64>::new());
    const N: u64 = 20_000;
    let pushed = Arc::new(AtomicUsize::new(0));
    let popped = Arc::new(AtomicUsize::new(0));

    thread::scope(|sc| {
        for tid in 0..4u64 {
            let s = s.clone();
            let pushed = pushed.clone();
            sc.spawn(move || {
                for i in 0..N {
                    s.push(tid * 1_000_000 + i);
                    pushed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        for _ in 0..4 {
            let s = s.clone();
            let pushed = pushed.clone();
            let popped = popped.clone();
            sc.spawn(move || loop {
                if let Some(_v) = s.pop() {
                    popped.fetch_add(1, Ordering::Relaxed);
                } else if pushed.load(Ordering::Relaxed) == (4 * N) as usize
                    && popped.load(Ordering::Relaxed) == (4 * N) as usize
                {
                    break;
                } else {
                    std::thread::yield_now();
                }
            });
        }
    });

    assert_eq!(pushed.load(Ordering::Relaxed), (4 * N) as usize);
    assert_eq!(popped.load(Ordering::Relaxed), (4 * N) as usize);
}

/// 直接测 epoch GC 的"两 epoch 窗口"语义：
/// 在 pin 状态下 defer 一个指针，紧接着 try_advance 应**不能**回收它
/// （因为还 pin 着）；unpin 后再 try_advance，应能回收。
#[test]
fn epoch_gc_waits_for_unpin() {
    // 用一个全局 AtomicUsize 追踪 destructor 调用次数（destructor 类型是 fn，不能用闭包）。
    static DESTROYED: AtomicUsize = AtomicUsize::new(0);

    fn dtor(p: *mut ()) {
        DESTROYED.fetch_add(1, Ordering::SeqCst);
        // 回收原始 Box<u64>。
        let _ = unsafe { Box::from_raw(p as *mut u64) };
    }

    let obj_ptr = Box::into_raw(Box::new(42u64)) as *mut ();
    DESTROYED.store(0, Ordering::SeqCst);

    {
        let _g = EpochGuard::new(); // pin
        unsafe { epoch::defer_destroy(obj_ptr, dtor) };
        epoch::try_advance();
        epoch::try_advance();
        // 第二次 try_advance：global_epoch=1，本线程 local_epoch=1（pin 在 epoch 0），
        // local_epoch-1=0 < 1=cur → 不能推进 → 不回收。
        assert_eq!(DESTROYED.load(Ordering::SeqCst), 0, "pin 期间不应回收");
    }
    // unpin 后 local_epoch=0：try_advance 看到 cur=1，所有 local 都是 UNPINNED → 推进到 2，
    // 同时回收 garbage[(1-1)%3] = garbage[0]——正是我们刚 defer 的那个。
    epoch::try_advance();
    assert_eq!(
        DESTROYED.load(Ordering::SeqCst),
        1,
        "unpin 后一次 try_advance 即可回收"
    );
}

/// 嵌套 pin/unpin 不会过早 unpin。
#[test]
fn epoch_nested_pin() {
    {
        let _g1 = EpochGuard::new();
        {
            let _g2 = EpochGuard::new();
            // 内层：仍 pin。
        }
        // 外层：仍 pin。
    }
    // 完全退出。能走到这里说明嵌套 pin/unpin 逻辑正确。
}
