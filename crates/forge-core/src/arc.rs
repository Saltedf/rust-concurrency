//! # 模块 M4：自建 `Arc<T>` / `Weak<T>`（优化版，接近标准库实现）
//!
//! M2 里 `Arc` 是黑盒，这里我们把它拆开。这是本课程**最微妙**的 unsafe 之一——
//! 两个原子计数器协同，外加一个用 `usize::MAX` 当"锁"的临时自旋锁来让
//! `get_mut`/`downgrade` 不会漏掉并发操作。内存序几乎每一条都有理由。
//!
//! 我们实现的是原书第 6 章最终的**优化版**：所有 Arc 合起来算"一个隐式 Weak"，
//! 于是克隆/释放一个 Arc **只碰一个计数器**（不再为弱指针付费）。
//! 详见 `docs/modules/M4-arc.md`。

use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::fence;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 内部分配块。不公开——是 Arc/Weak 的实现细节。
struct ArcData<T> {
    /// `Arc` 的数量。
    data_ref_count: AtomicUsize,
    /// `Weak` 的数量，**外加 1**（只要还有任何 `Arc` 在）。
    /// 这个"+1"代表"所有 Arc 合起来算的一个隐式 Weak"，
    /// 让克隆 Arc 时不必碰这个计数器。
    alloc_ref_count: AtomicUsize,
    /// 数据本身。只剩弱指针时会被手动 drop，所以用 `ManuallyDrop`。
    data: UnsafeCell<ManuallyDrop<T>>,
}

use std::cell::UnsafeCell;

/// 强引用：共享所有权，只要还有一个 `Arc`，数据就还在。
pub struct Arc<T> {
    ptr: NonNull<ArcData<T>>,
}

/// 弱引用：不阻止数据被释放；想用得先 `upgrade` 成 `Arc`。
pub struct Weak<T> {
    ptr: NonNull<ArcData<T>>,
}

// 安全性：把 Arc/Weak 送到别的线程，等于让 T 被多线程共享（要 Sync）
// 也等于让 T 可能被另一个线程 drop（要 Send）。所以 T: Send + Sync 才行。
unsafe impl<T: Send + Sync> Send for Arc<T> {}
unsafe impl<T: Send + Sync> Sync for Arc<T> {}
unsafe impl<T: Send + Sync> Send for Weak<T> {}
unsafe impl<T: Send + Sync> Sync for Weak<T> {}

impl<T> Arc<T> {
    pub fn new(data: T) -> Arc<T> {
        Arc {
            ptr: NonNull::from(Box::leak(Box::new(ArcData {
                alloc_ref_count: AtomicUsize::new(1), // 这 1 = "所有 Arc 合起来的隐式 Weak"
                data_ref_count: AtomicUsize::new(1),
                data: UnsafeCell::new(ManuallyDrop::new(data)),
            }))),
        }
    }

    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    /// 仅当这是**唯一**的 Arc、且没有 Weak 时，给 `&mut T`；否则 `None`。
    pub fn get_mut(arc: &mut Self) -> Option<&mut T> {
        // 关键：两个计数器是两次独立的读，必须防"读一个、另一个人动手脚"的缝隙。
        // 办法：把 alloc_ref_count 从 1 "锁"成 usize::MAX（自旋锁），读完另一个再解锁。
        // Acquire：与 Weak::drop 的 Release 减量同步，保证随后的 data_ref_count.load
        //          能看到一个刚 upgrade 上来的新 Arc。
        if arc
            .data()
            .alloc_ref_count
            .compare_exchange(1, usize::MAX, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        let is_unique = arc.data().data_ref_count.load(Ordering::Relaxed) == 1;
        // Release：与 downgrade 的 Acquire 同步，保证上面那次 load 不会被
        //          "一个未来的 downgrade 之后的 Arc::drop" 倒灌影响。
        arc.data().alloc_ref_count.store(1, Ordering::Release);
        if !is_unique {
            return None;
        }
        // Acquire 栅栏：与 Arc::drop 的 Release 减量同步，保证此前所有通过旧 Arc 的访问都已结束。
        fence(Ordering::Acquire);
        unsafe { Some(&mut *arc.data().data.get()) }
    }

    /// 从 `&Arc` 降级出一个 `Weak`。
    pub fn downgrade(arc: &Self) -> Weak<T> {
        let mut n = arc.data().alloc_ref_count.load(Ordering::Relaxed);
        loop {
            if n == usize::MAX {
                // get_mut 正持锁，自旋等它解锁
                std::hint::spin_loop();
                n = arc.data().alloc_ref_count.load(Ordering::Relaxed);
                continue;
            }
            assert!(n < usize::MAX - 1);
            // Acquire：与 get_mut 的 Release-store 同步，防止"get_mut 之后的一个 Arc::drop"
            //          的效果在 get_mut 解锁前就被本线程看到（那会让 get_mut 漏判）。
            match arc.data().alloc_ref_count.compare_exchange_weak(
                n,
                n + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Weak { ptr: arc.ptr },
                Err(e) => n = e,
            }
        }
    }
}

impl<T> Deref for Arc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // 安全：存在 Arc ⇒ 数据还在 ⇒ 可共享读。
        unsafe { &*self.data().data.get() }
    }
}

impl<T> Clone for Arc<T> {
    fn clone(&self) -> Self {
        // 只碰 data_ref_count。Relaxed：克隆计数不涉及其它变量的同步。
        if self.data().data_ref_count.fetch_add(1, Ordering::Relaxed) > usize::MAX / 2 {
            std::process::abort(); // 防御性：计数逼近上限就整体 abort
        }
        Arc { ptr: self.ptr }
    }
}

impl<T> Drop for Arc<T> {
    fn drop(&mut self) {
        // Release 减量；只有"最后一个"（从 1 减到 0）才需要 Acquire。
        if self.data().data_ref_count.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire); // 与之前所有 Arc::drop 的 Release 同步
                                      // 安全：data_ref_count 已为 0，没人再碰数据。
            unsafe {
                ManuallyDrop::drop(&mut *self.data().data.get());
            }
            // 没有 Arc 了：把"所有 Arc 合起来的那个隐式 Weak"也释放掉。
            // 造一个 Weak 再 drop 它，正好减一次 alloc_ref_count。
            drop(Weak { ptr: self.ptr });
        }
    }
}

impl<T> Weak<T> {
    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    /// 尝试升级成 `Arc`。若数据已被释放（只剩弱指针），返回 `None`。
    pub fn upgrade(&self) -> Option<Arc<T>> {
        let mut n = self.data().data_ref_count.load(Ordering::Relaxed);
        loop {
            if n == 0 {
                return None; // 没有 Arc 了 ⇒ 数据已 drop
            }
            assert!(n < usize::MAX);
            // CAS 把 data_ref_count 从 n 加到 n+1。Relaxed：只是计数。
            match self.data().data_ref_count.compare_exchange_weak(
                n,
                n + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Arc { ptr: self.ptr }),
                Err(e) => n = e,
            }
        }
    }
}

impl<T> Clone for Weak<T> {
    fn clone(&self) -> Self {
        if self.data().alloc_ref_count.fetch_add(1, Ordering::Relaxed) > usize::MAX / 2 {
            std::process::abort();
        }
        Weak { ptr: self.ptr }
    }
}

impl<T> Drop for Weak<T> {
    fn drop(&mut self) {
        // Release 减量；最后一个（1→0）才 Acquire 并释放整个分配块。
        if self.data().alloc_ref_count.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            unsafe {
                drop(Box::from_raw(self.ptr.as_ptr()));
            }
        }
    }
}

impl<T> Arc<T> {
    /// 拿一个 `Weak`（便利方法）。
    pub fn weak(&self) -> Weak<T> {
        Self::downgrade(self)
    }
}
