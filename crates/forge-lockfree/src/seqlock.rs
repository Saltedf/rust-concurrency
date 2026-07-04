//! # 序列锁（SeqLock）—— 读不阻塞写、写不阻塞读
//!
//! 一个 `AtomicUsize` 计数器：**写者改数据前把它从偶变奇（进行中），改完再 +1 回到偶**。
//! 读者：读计数 → 读数据 → 再读计数；两次相等且为偶 ⇒ 读到的是一致快照；否则重来。
//!
//! 适合"多读少写、数据较大放不进单个原子"。Linux 内核用它给进程提供时间戳。
//!
//! ⚠️ **内存模型警告（原书亦如此）**：并发非原子地读/写数据**严格说是数据竞争（UB）**，
//! 即便读到的值被丢弃。完全合规需要按字节原子访问（RFC 3301 AtomicPerByte）。
//! 这里按经典 seqlock 模式实现并**明确标注此限制**；不要在 miri 下跑数据竞争路径。

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct SeqLock<T> {
    seq: AtomicUsize,
    data: UnsafeCell<T>,
}

// 安全性说明见上：多线程并发读/写 data 是 seqlock 模式固有的"事实数据竞争"，
// 在 AtomicPerByte 落地前属已知妥协。我们承诺单写者。
unsafe impl<T: Send> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            seq: AtomicUsize::new(0),
            data: UnsafeCell::new(value),
        }
    }

    /// 写者进入临界区：计数器从偶变奇。返回一个 guard，drop 时计数器 +1 回偶。
    pub fn write(&self) -> WriteGuard<'_, T> {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel); // 偶→奇
                                                           // AcqRel：之前的写都已发布，且我们看到之前读者的状态。
        WriteGuard {
            lock: self,
            _start_seq: seq,
        }
    }

    /// 读者：无阻塞地读一个一致快照；若写者正在改就重试。
    pub fn read(&self) -> T {
        loop {
            let s1 = self.seq.load(Ordering::Acquire); // 奇 ⇒ 正在写，先自旋等
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let snapshot = unsafe { self.data.get().read() };
            // 读完后 Acquire 再读计数器：若变了或变奇 ⇒ 写者插手过 ⇒ 重来。
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return snapshot;
            }
        }
    }
}

pub struct WriteGuard<'a, T> {
    lock: &'a SeqLock<T>,
    _start_seq: usize,
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        // 奇→偶：通知读者"写完了"。
        self.lock.seq.fetch_add(1, Ordering::Release);
    }
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}
impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}
