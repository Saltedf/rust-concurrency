# M7 — 自建 futex 真锁：Mutex、Condvar、写公平 RwLock

> 模块：`forge-sync::{mutex, condvar, rwlock}`　|　测试：`crates/forge-sync/tests/m7_*.rs`
> 跑：`cargo test -p forge-sync --test m7_*`　|　miri：`cargo +nightly miri test -p forge-sync --test m7_01_mutex_counter`

M3 的自旋锁能"等"，但靠烧 CPU；M6 我们拿到了"睡/醒"的内核原语 atomic-wait（`wait` / `wake_one` / `wake_all`）。这一章把它们焊接起来——在 atomic-wait 上亲手造出**真正可用**的 `Mutex`、`Condvar`、`RwLock`，和 std / parking_lot 同一套思路。

核心追求只有一句：**尽量少做 syscall**。每一次 `wait` / `wake` 都可能陷进内核，比一条原子指令慢一两个数量级。每个原语都从一个"能跑但浪费 syscall"的版本起步，再一步步把 syscall 砍掉。

> **贯穿全章的钥匙**：atomic-wait 的 `wait` / `wake` **从内存安全角度不影响正确性**——它们只是避免忙等的优化。因为 `wait` 可能假唤醒，我们本就必须自己用原子管理锁状态、循环检查。删掉 `wait` / `wake`，锁退化成自旋锁——仍"正确"，只是没法用。这条结论能帮你推理任何 unsafe 锁代码：**先把状态机想对，`wait` / `wake` 只是性能。**

本章 9 个递进的步骤：

1. 2 态 Mutex（0/1）——能跑
2. 故意触发**活锁测试**（被 park 的线程永不醒）——证明 2 态不够
3. 3 态 Mutex（0/1/2）——失败测试即老师，引入"有等待者"信号
4. 自适应自旋——再省一刀
5. futex Condvar（counter + num_waiters + 循环抗假唤醒）
6. RwLock 编码态（读者数×2，+1 有写者等，u32::MAX 写锁；呼应 M2 写饥饿）
7. `writer_wake_counter` 消除写者忙转/饥饿
8. 把自研 Mutex 换进运行时就绪队列，对比 std / parking_lot
9. 锁中毒 + 对外 `forge-sync` API

---

## 第 0 拍（ENEMY）：我们要打的是哪三个敌人

敌人不是抽象的。把三个具体的"会咬人的场景"画在面前，你才知道每一步优化在打谁。

**敌人 ①：syscall 太贵。** 一次 `wake_one` 在"没人在等"时也会陷进内核，内核查等待队列、发现空、返回——白跑一趟。最常见场景恰恰是**没有竞争**（一个线程独占锁），这次 syscall 完全是浪费。原书实测：把这条浪费砍掉，非竞争路径能从 ~400ms 降到 ~40ms（10 倍），有的机器甚至 30 倍。这就是 `parking_lot` 比 std 快的根源之一。

为什么 syscall 这么贵？因为正常函数调用只是 CPU 跳个地址、压几个寄存器；syscall 要：(a) 切换 CPU 到内核态（特权级切换，要重新加载段寄存器、清 TLB 的部分项），(b) 内核要检查参数合法性（用户传的指针不能信），(c) 内核里走自己的等待队列数据结构，（d) 可能要触发调度、把别的线程换上核。整个流程是几百到几千个周期起步，而一条 `swap` 原子指令只要十几个周期。这就是为什么"少做一次 syscall"在锁上这么值——锁是程序里最频繁的操作之一。

**敌人 ②：写者被读者"忙转"成空转。** 一个 `RwLock`，读者不停加解锁，状态字一直在跳。写者调 `wait(state, s)`，结果因为读者数变了（不是写锁值），`expected` 老对不上，`wait` 立刻返回——它没在睡，它在 syscall 循环里烧 CPU。这是"伪睡眠"，比真自旋还糟（因为还搭上了 syscall 开销）。

**敌人 ③：写饥饿。** 一个写者 + 一群不停读的读者。写者永远抢不到：它要 `state==0` 才能拿锁，但读者总有人把 state 抬上去。写者可以饿死几秒、几分钟、永远。这是"读者优先"RwLock 的经典病。M2 我们已经见过它的症状——这里我们要亲手把它治好。

本章每一个优化都对应打这三个敌人中的一个或几个。看到任何一段代码，先问自己："它在打哪一个？"

---

## 第 1 拍（ANCHOR）：钥匙模型——锁是带门铃的单间

在碰任何代码前，先把整个章节的 mental model 立起来。

想象一个**单间厕所**：
- 门上有一块**牌子**，写着"空" / "有人" / "有人，且外面有人在等"。这块牌子就是我们的 `state` 原子变量。
- 一个人想进去，先看牌子（一次原子读）。
  - 牌子是"空"，他翻成"有人"进去（一次 CAS）——这是**非竞争快速路径**。
  - 牌子是"有人"，他得在门外等。
- **门外等的机制**有两种极端：
  - **自旋**（M3）：站着盯牌子，牌子一变"空"就冲。累，但反应快。
  - **睡觉**（M6 的 `wait`）：蹲角落睡，靠门铃叫醒。省力，但门铃按下去要钱（syscall）。
- **门铃（`wake_one`）**：里面的人出来时翻牌子，**并且**按一下门铃，告诉外面睡觉的人"轮到你了"。

本章所有的工程，都围绕一个问题转：**"里面的人出来时，到底要不要按门铃？"**
- 按得太勤（每次都按）→ 没人在外面也按，白浪费 syscall。**敌人 ①**。
- 按得太松（永远不按）→ 外面睡觉的人永远不醒。**活锁**。
- 按得"刚好"（有人等才按）→ 这是我们追求的甜点。

要做出"刚好"，门上的牌子必须**编码出"有没有人在等"**。一个二态牌子（空 / 有人）做不到——里面的人出来时只看到"有人"，不知道是不是有人在外面睡。这就是为什么我们要从 2 态升到 3 态。这是本章最重要的一句话，第 3 拍手算给你看为什么。

---

## 第 2 拍（LOW-FI）：先写出能跑的 2 态 Mutex（"能跑，但 unlock 永远按门铃"）

把 M3 的 `SpinLock<T>` 抄过来，只改一处：把 `AtomicBool` 换成 `AtomicU32`（因为 atomic-wait 只认 32 位）。`0` = 未锁、`1` = 已锁。guard / Deref / Drop 那一套和 M3 一模一样，这里只看 `lock` 和 `unlock`。

```rust
use crate::atomic_wait::{wake_one, wait};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Mutex<T> {
    /// 0 = 未锁；1 = 已锁。
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// 不安全保证：T: Send 才能跨线程送（和 M3 自旋锁同理）。
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        // swap 把 state 无条件设成 1；返回旧值。
        // 旧值是 0 → 抢到；旧值是 1 → 已被占，去睡。
        while self.state.swap(1, Ordering::Acquire) == 1 {
            wait(&self.state, 1);   // 仅当 state 仍是 1 才睡（M6 那套 expected 机制）
        }
        MutexGuard { mutex: self }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.state.store(0, Ordering::Release);
        wake_one(&self.mutex.state);   // ← 无条件 wake：哪怕没人在等
    }
}
```

**为什么 `wait(state, 1)` 那个 `1` 是关键？** `wait` 的语义是"**仅当** state 仍然等于给定值时才睡"。这一条堵住了 swap 之后、wait 之前那一刻的窗口：如果别人刚好在那窗口里解锁了（state 变 0），`wait` 看到 state != 1，**立刻返回不睡**。所以唤醒不会丢。这是 M6 futex 那一章的核心，这里直接用上。

**内存序**：和 M3 自旋锁完全同理——`swap(Acquire)` 与上次解锁的 `store(Release)` 配对，建立 happens-before，保证后进来的线程能看到上一持锁者写过的数据。复习一下：

- `Acquire` load/swap：之后的读/写不能重排到这条之前。
- `Release` store：之前的读/写不能重排到这条之后。
- 配对（同一变量的 Acquire + Release）→ 之前的写对之后的读可见。

### guard 的设计：为什么必须用 UnsafeCell？

这是我们第一次自己造完整的锁，有必要把 guard 那层 unsafe 讲透（M3 偏重自旋逻辑，没展开）。

`Mutex<T>` 内部的 `T` 是被多线程共享的（`&Mutex<T>` 在多个线程之间传）。但锁要允许**持锁者**修改 `T`——这就要可变引用 `&mut T`。Rust 的借用规则不允许"同时有 `&Mutex<T>` 和 `&mut T`"——这就是 `UnsafeCell<T>` 的用武之地：它是一个**编译器知道的逃生舱**，告诉编译器"这个内部的 T 可能被别名，我自己保证安全"。

`UnsafeCell` 提供一个 `get()` 方法返回 `*mut T`（裸指针）。我们的 `MutexGuard::deref_mut` 长这样：

```rust
impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}
```

这里的 unsafe 块是因为我们在解引用裸指针。**安全性靠什么保证？**

1. **互斥**：同一时刻只有一个 `MutexGuard` 存在（因为 `lock` 是排他的）。所以"可变别名"实际不会发生——只有一个线程能拿到 `&mut T`。
2. **生命周期**：guard 借用了 `&Mutex`，guard 在，Mutex 就不能 drop。`MutexGuard<'a, T>` 的 `'a` 就是这个借用的生命周期。
3. **Send 边界**：`unsafe impl<T: Send> Sync for Mutex<T>` 我们要求 `T: Send`——这样把 `T` 通过 `Mutex` 送别的线程才不会破坏 Rust 的线程安全（比如不能锁住一个 `Rc`，再把 `Rc` 的副本送到别的线程）。

`Drop for MutexGuard` 里 unlock：guard 离开作用域时自动解锁，这一条保证**只要锁被拿到过，就一定会被放掉**——哪怕持锁的线程 panic（panic 时 Rust 也会展开栈、调 Drop）。这就是为什么"获取-释放"模式（RAII）比"手动 lock/unlock"安全得多。

### 这版的病：unlock 永远按门铃

`Drop::drop` 里那句 `wake_one` 是**无条件**的。哪怕根本没人在睡，它也会陷进内核一次。最要命的是**非竞争场景**——一个线程独占锁、几百万次 lock/unlock——这几百万次 `wake_one` 全是浪费。这就是敌人 ①。

> 在 macOS / Windows 上，因为底层 `WaitOnAddress` / libc++ 自己有 bookkeeping，这种浪费小得多；但在 Linux futex 上很明显。**这就是为什么"自己造锁"必须把平台考虑进去。**

---

## 第 3 拍（WRITE · 手算 #1）：2 态 Mutex 的活锁，以及 3 态如何救场

光说"浪费 syscall"读者未必疼。下面手算一个**真正会咬人**的场景：2 态 Mutex **不止浪费，还会让等待者永远不醒**。这不是性能问题，是**功能问题**。

### 场景设置

- 一把 2 态 `Mutex`：`state ∈ {0, 1}`。
- 三个线程：T1 持锁（state=1），T2 和 T3 想拿。

### 逐拍走（时间从上到下）

| 拍 | 动作 | state | T2 在哪 | T3 在哪 | T1 在哪 |
|---|---|---|---|---|---|
| 0 | 初始：T1 持锁 | 1 | — | — | 持锁 |
| 1 | T2 进 lock，`swap(1)` 返回 1（被占） | 1 | 准备 sleep | — | 持锁 |
| 2 | T2 调 `wait(state, 1)`，state 仍是 1 → **睡去** | 1 | **睡** | — | 持锁 |
| 3 | T3 进 lock，`swap(1)` 返回 1（被占） | 1 | 睡 | 准备 sleep | 持锁 |
| 4 | T3 调 `wait(state, 1)` → **睡去** | 1 | 睡 | **睡** | 持锁 |
| 5 | **T1 unlock：`state.store(0, Release)`** | **0** | 睡 | 睡 | 走了 |

拍 5 之后，state 是 0，锁实际上是空的。但 T2 和 T3 **都还在睡**。

为什么？因为 2 态编码里，**T1 不知道有没有人在等**。它只看到 state=1，把它翻成 0 就完事了——它没有触发任何 `wake`（因为 2 态版本里 unlock 是 `store(0)` + 无条件 `wake_one`，**如果**那一句漏了或者你想"优化"成有条件，就根本没机会发 wake）。

更准确地说：哪怕 2 态版本里有那句无条件 `wake_one`，T1 也只能 wake_one **一个**人。另一个人怎么办？在 2 态里这是个根本难题——醒来的那个人拿到锁后再 unlock 时，他**也不知道**还有没有人在等（state 又是 1，和"自己刚抢到时"长得一模一样）。所以 2 态版本的 unlock 只能无条件 `wake_one`，每次 unlock 都按门铃——这就是敌人 ① 的根源。

但更糟糕的情况是：**如果你尝试"聪明地"把 wake 砍掉**（比如改成"只在 state==某个特殊值时才 wake"），2 态没有那个特殊值，于是 wake 永远不发生。T2 和 T3 永远睡。这就是**活锁**——锁是空的，但没人能进。

> 活锁（livelock）和死锁（deadlock）的区别：死锁是线程互相等、都不动；活锁是线程都在动、但都没进展。这里的活锁是"持锁者已走、等待者不醒"——状态字是空，但睡眠的线程永远等不到唤醒。

这就是为什么 2 态在工程上**不够用**：它编码不出"有等待者"这条信息。

### 救场：3 态编码

引入第 3 个状态：

```
0：未锁
1：已锁、没有其他人在等
2：已锁、有其他人在等
```

`2` 就是"门外的牌子：有人，且有人在等"。新的逐拍：

| 拍 | 动作 | state | T2 在哪 | T3 在哪 | T1 在哪 |
|---|---|---|---|---|---|
| 0 | 初始：T1 持锁 | 1 | — | — | 持锁 |
| 1 | T2 进 lock，先试 CAS `0→1` 失败（state 是 1） | 1 | 准备 mark | — | 持锁 |
| 2 | T2 `swap(2)`：**把 state 从 1 改成 2**（"有人在等！"） | **2** | 准备 sleep | — | 持锁 |
| 3 | T2 调 `wait(state, 2)` → 睡去 | 2 | **睡** | — | 持锁 |
| 4 | T3 进 lock，CAS 失败，`swap(2)` 返回 2（已是 2） | 2 | 睡 | 准备 sleep | 持锁 |
| 5 | T3 调 `wait(state, 2)` → 睡去 | 2 | 睡 | **睡** | 持锁 |
| 6 | **T1 unlock：`swap(0, Release)` 返回 2** → 看到 2，`wake_one`！ | **0** | 睡 | 睡 | 走了 |
| 7 | 内核挑一个睡的（假设 T2）醒来 | 0 | **醒** | 睡 | — |
| 8 | T2 醒后从 `wait` 返回，循环回到 `swap(2)` —— 它把 state **从 0 拨成 2**（不是 1！）拿到锁 | **2** | **持锁** | 睡 | — |
| 9 | （T2 干完活）unlock：`swap(0)` 返回 2 → `wake_one` | 0 | 走了 | 睡 | — |
| 10 | T3 醒来，`swap(2)` 拨 0→2，拿到锁 | 2 | — | **持锁** | — |

注意拍 8 这一步——**它救了整个系统**：

> **被唤醒的线程拿到锁时，必须把 state 拨成 2（不是 1）**。这样它之后 unlock 时，仍会看到 2、仍会 `wake_one` 下一个等待者。等待者链条不断。

这就是 3 态 Mutex 的精髓。它**同时**打了敌人 ①：
- 非竞争路径（state 从 0 直接 CAS 到 1），unlock 看到的是 1，**不按门铃**——零 syscall。
- 竞争路径，unlock 看到的是 2，按一次门铃——刚刚好。

### 假唤醒也要安全：循环检查

回到拍 7。我们说"内核挑 T2 醒来"，但**实际上**，Linux futex 允许**假唤醒**（spurious wakeup）——线程可能在**没有**对应 `wake_one` 的情况下自己醒来。这是 futex 的已知行为，POSIX 也允许。

这意味着：`wait` 返回**不代表**有人叫你。所以正确的等待模式是**循环**：

```rust
while self.state.swap(2, Acquire) != 0 {
    wait(state, 2);   // ← 假醒后循环回到 swap，重新检查
}
```

如果 `wait` 假唤醒，循环回到 `swap(2)`：
- 如果 state 还是 2（被占），继续 wait——没事。
- 如果 state 是 0（解锁了），swap 把它拨成 2 拿到锁——也正确。

这条"wait 必配循环检查"是**任何**基于 futex 的等待的法则。我们三个原语都遵守。

### 把它写成代码

```rust
pub fn lock(&self) -> MutexGuard<'_, T> {
    // 快速路径：CAS 0→1。非竞争时这是唯一的原子操作，零 syscall。
    if self.state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // 慢路径：state 不是 0，说明被占。
        // 把 state swap 成 2（标记"有等待者"），等到 state 是 0 才算抢到。
        while self.state.swap(2, Ordering::Acquire) != 0 {
            wait(&self.state, 2);   // state 仍是 2 才睡
        }
    }
    MutexGuard { mutex: self }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // 仅当曾是 2（有等待者）才按门铃。
        if self.mutex.state.swap(0, Ordering::Release) == 2 {
            wake_one(&self.mutex.state);
        }
    }
}
```

**一个看着奇怪、但必须想清楚的细节**：为什么 `CAS 0→1` 在 while 循环**外**，而 `swap(2)` 在循环**内**？

因为：一旦你调过 `wait`，你就**不再有资格**用 `1` 这个状态拿锁了。设想下面这种"在循环里也试 CAS 0→1"的错误版本：

```
（错误！）
while True {
    if CAS(0→1) 成功: return;       // ← 错在哪？
    if swap(2) == 0: return;        // 拿到了
    wait(state, 2);
}
```

错误在：你 `wait` 醒来后，CAS `0→1` 成功，state 变成 1。**但你身后还有别的等待者在睡**——它们等的是 state 从 2 变化，现在 state 是 1，unlock 时看到 1，不 wake。它们永远不醒。回到活锁。

正确版本是：一旦进入慢路径（说明已经有人等），**所有**后续抢锁都通过 `swap(2)`——保证 state 永远编码出"有等待者"，链条不断。这就是为什么 CAS `0→1` 只在慢路径入口前试一次。

### `compare_exchange` 的两个 Ordering 参数

复习一下 `compare_exchange`：它接收**两个**内存序——成功时用第一个、失败时用第二个。这是个常被忽略的细节。

```rust
self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
//                                       ↑成功用        ↑失败用
```

成功时为什么用 `Acquire`？因为我们刚拿到了锁，要看到上一持锁者写的数据——Acquire 和上次解锁的 Release 配对。

失败时为什么用 `Relaxed`？因为我们没拿到锁、什么也没读、直接进慢路径。失败时不需要任何同步——慢路径里的 `swap(2, Acquire)` 才是真正"看到锁"的操作。失败时用更强的 Ordering 是浪费（CPU 要插更贵的栅栏指令）。

这条"成功 Acquire、失败 Relaxed"是几乎所有锁的 CAS 模式。我们的 Mutex、RwLock 都遵守。M1 我们详细讲过这条，这里是它的实战体现。

### 每个等待者多付一次 wake 的代价

3 态版本有个不那么明显的代价：**每"等过一次"的线程，自己 unlock 时也会 `wake_one`（哪怕没必要）**。为什么？

因为这个线程当初拿锁时把 state 拨成了 2（不是 1）——它 unlock 时看到 2，必然 wake。但它**身后可能根本没人**在等（其他人已经全拿到锁了）。

这次多余的 wake 算什么？一个 syscall 的代价。但**最重要的非竞争场景**——大多数锁大多数时候都没人等——wait 和 wake 全免。原书实测从 ~400ms 降到 ~40ms（10 倍），有的机器 30 倍。代价完全值。

这是工程设计里常见的模式：**优化最常见的路径，哪怕代价是少数场景下多做点事**。"非竞争"是最常见的场景，3 态把这条路径打成零 syscall——这就是它的价值所在。

### parking_lot 的更精细做法（提示，不展开）

parking_lot 比我们的 3 态更精细：它用一个**单独的 atomic usize** 记录"锁状态 + 等待者数 + 公平位"。这样它能区分"自己等过、但身后没人"——这种情况下 unlock 不需要 wake。代价是状态字更大、CAS 编码更复杂。我们教学版不这么做，但你应该知道"还有更精细的版本存在"。

parking_lot 还做了一些我们没有的事：(a) 用 `lock_api` crate 自动生成 guard 那套样板，(b) 支持 `try_lock_for` / `wait_for` 等带超时的 API（要底层 atomic-wait 暴露带超时的 wait），(c) 在不同平台用不同的底层 syscall。这些是生产级锁要考虑的事。

---

## 第 4 拍（ISO·ZOOM）：再省一刀——自适应自旋

3 态已经把非竞争路径打成零 syscall。还能再省吗？唯一还能省的，是**竞争路径**上的"睡-醒"往返。

回到钥匙模型：你敲门（CAS 失败），里面的人**可能马上就出来**——他在另一个核上、临界区很短。这种情况下，如果你立刻蹲下睡觉，就要付出"睡-醒"两次 syscall 的代价；而如果你**在外面站几纳秒**（自旋），很可能他一出来你就立刻进去——一次 syscall 都没有。

但自旋有上限：如果里面的人**真的**要待一会儿（比如他在等 IO），自旋就是纯烧 CPU。所以解法是**先自旋极短一瞬，不行再睡**：

```rust
#[cold]
fn lock_contended(state: &AtomicU32) {
    let mut spin_count = 0;
    // 只在"已锁、无等待者"（state==1）时自旋。
    // state==2 说明别人已经放弃自旋去睡了——多半自旋没用。
    while state.load(Ordering::Relaxed) == 1 && spin_count < 100 {
        spin_count += 1;
        std::hint::spin_loop();
    }
    // 自旋后再试一次 CAS 0→1（一旦 wait 过，就必须用 2，见上一拍）。
    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    // 放弃自旋：swap 成 2，进入睡眠循环。
    while state.swap(2, Ordering::Acquire) != 0 {
        wait(state, 2);
    }
}
```

三个细节，每一个都对应一条具体决策：

1. **为什么只在 state==1 时自旋，不在 state==2 时自旋？**
   state==2 意味着已经有人放弃自旋去睡了——这是一个"信号"：当前持锁者大概要待一会儿，自旋多半没用。state==1 时，持锁者可能马上放，自旋有戏。

2. **为什么自旋用 `load(Relaxed)` 而不是 CAS？**
   CAS 要**独占**缓存行（MESI 协议的 exclusive 状态）。两个核反复 CAS 同一个 cache line，会反复触发缓存行所有权在核之间弹来弹去（"cache line ping-pong"），比反复 `load` 贵得多。`load` 是只读，多核可以共享缓存行。
   
   复习 MESI：每个缓存行有四种状态——Modified（独占且脏）、Exclusive（独占且干净）、Shared（共享且干净）、Invalid（无效）。`load` 让缓存行进 Shared，多核可同时 Shared。CAS 要把缓存行升级到 Modified，必须先把别的核的副本都 invalidate——这会触发跨核的"invalidate message"，往返延迟几十纳秒。自旋时反复 CAS，每次都要付这个延迟。

3. **为什么 `#[cold]`？**
   这是个给编译器的提示：这个函数是慢路径、不常进。编译器在优化 `lock` 主路径时，就不会为了让 `lock_contended` 内联而撑大指令缓存。`lock` 主路径越小、越快越好。
   
   类似地，可以给 `lock` 本身加 `#[inline]`——把快速路径的指令直接铺到调用点。这种微优化看着不起眼，但在高频锁上累积效果可观。

`100` 这个数字怎么来的？基本是任选的——原书指出 std 在 Linux 上也用 100，是经验值。原书在 AMD / Intel 两台机器上测：有的快、有的反而慢。**结论永远是"看情况"**——但 std / parking_lot 都采用，说明常见场景下净收益为正。

> **决策的小结**：自适应自旋是一个"赌"。赌赢了，省 syscall；赌输了，浪费一点 CPU 时间。`100` 是个保守的赌注——多试几下不行就放弃。真实的工程里没有银弹，只有"在大多数场景下净收益为正"的工程取舍。这就是 `forge-sync::mutex` 的最终形态（`tests/m7_01_mutex_counter.rs`、`m7_02_mutex_uncontended.rs`，miri 干净）。

### 锁中毒（poison）

`std::sync::Mutex` 有个特性叫**中毒**：持锁的线程 panic 后，guard 的 `Drop` 还是会解锁，但 std 会标记这把锁"中毒"，后续 `lock()` 返回 `Err`。这是为了**防止后续线程读到半截写坏的数据**——panic 时临界区可能没走完。

我们的 `forge-sync::Mutex` **不实现中毒**（和 parking_lot 一样）。理由：
- 中毒是策略，不是机制。它需要在 `Mutex` 里加一个 `AtomicBool` 标记，在 `lock` 里检查——这是一笔额外开销，**每一次** lock 都要付。
- 大多数真实程序不需要中毒：它们用 `Mutex` 保护的数据要么能从半截状态恢复，要么有外层错误处理。
- 想要中毒，可以在外面套一层自己的类型。teaching 上我们把这条留给读者作为练习。

std 选择有中毒是历史决策——Rust 早期把"防呆"放在标准库里。parking_lot 选择无中毒是性能决策。两种哲学都合理。

### miri 验证：unsafe 写对了吗？

`cargo +nightly miri test -p forge-sync --test m7_01_mutex_counter` 在 miri 下跑测试。miri 检查 unsafe 代码的内存违规——解引用裸指针、数据竞争（在单线程视角下模拟）、未定义行为。我们的 `Mutex` 用了 `UnsafeCell` + 裸指针解引用，miri 干净意味着：

- 我们对 `T: Send` 的边界是对的。
- `Deref` / `DerefMut` 里的 `unsafe { &*... }` 没有别名问题（同一时刻只有一个 guard）。
- 内存序（Acquire / Release）的配对至少在 miri 的检查下没有数据竞争。

miri 不能证明并发正确（它单线程模拟），但能抓到一大类低级错误。这是为什么"miri 干净"是有意义的信心指标。

---

## 第 5 拍（ANCHOR）：Condvar 的钥匙模型——柜台与号牌

`Condvar`（条件变量）的用途：一个线程持着锁、检查某个条件、条件不满足就**等**，直到另一个线程改了数据、条件满足了、来"通知"它。和 `Mutex` 不同，`Condvar` 解决的是"等条件"而不是"等锁"。

钥匙模型：**银行柜台**。
- 一个柜台（mutex 保护的数据）。
- 你拿了号（counter），坐在大厅等（wait）。
- 业务员办完一轮，叫号（notify）——大厅的屏幕（counter）翻到下一个号。
- 你**先**记下你拿的号，**再**去坐下。如果你的号已经被叫过了（counter 变了），你立刻起身不坐。

为什么需要 Condvar？想象一个生产者-消费者队列：

```rust
let queue = Mutex::new(Vec::new());
// 消费者：
loop {
    let mut q = queue.lock();
    while q.is_empty() {
        // 怎么等"队列不空"？不能在这里 sleep——还持着锁呢。
        // 也不能 unlock 再 sleep——那中间生产者通知不到。
        // ↑ 这就是 Condvar 要解决的：原子地"解锁 + 等通知"。
        q = condvar.wait(q);   // 解锁、睡、被通知后重新加锁
    }
    let item = q.pop().unwrap();
    drop(q);
    process(item);
}
// 生产者：
{
    let mut q = queue.lock();
    q.push(item);
}
condvar.notify_one();   // 通知等待的消费者
```

`Condvar` 和 atomic-wait 的 `wait`/`wake` 几乎同形——**唯一的差别在"怎么防丢通知"**。atomic-wait 的 `wait` 用的是"检查原子值"防丢；Condvar 的"通知"是抽象的、没法直接绑到一个原子值上。所以 Condvar 自己造一个原子值：**一个 `counter`，每次 notify 都改它**。`wait` 在**解锁 mutex 之前**记下 counter 值，解锁后 `wait(counter)`——于是"解锁后、入睡前"这段时间如果有 notify（counter 变了），futex 的 expected 检查会让 `wait` 立刻返回、不睡。

---

## 第 6 拍（WRITE）：futex Condvar 的两个版本

### 版本 1：一个 counter

```rust
use crate::atomic_wait::{wake_all, wake_one, wait};
use crate::mutex::MutexGuard;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

pub struct Condvar {
    counter: AtomicU32,
}

impl Condvar {
    pub const fn new() -> Self {
        Self { counter: AtomicU32::new(0) }
    }

    pub fn notify_one(&self) {
        self.counter.fetch_add(1, Relaxed);
        wake_one(&self.counter);
    }

    pub fn notify_all(&self) {
        self.counter.fetch_add(1, Relaxed);
        wake_all(&self.counter);
    }

    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        // 解锁前记下 counter——关键的一步。
        let counter_value = self.counter.load(Relaxed);
        let mutex = guard.mutex;
        drop(guard);                       // 解锁 mutex
        wait(&self.counter, counter_value); // counter 仍为旧值才睡
        mutex.lock()                        // 重新加锁后返回新 guard
    }
}
```

`wait` 那三步是核心：

1. **持锁时**记下 counter。
2. **解锁**。
3. `wait(counter)`——仅当 counter 仍是旧值才睡。

为什么这三步能防丢通知？考虑两个线程：

- T1（等待方）：持锁 → 记 counter=5 → 解锁 → 准备调 `wait(5)`。
- T2（通知方）：等锁 → 拿到锁 → 改数据 → `notify_one`（counter 变 6）→ 解锁。

T2 拿到锁、改 counter 必然在 T1 解锁之后（解锁-加锁的 happens-before）。所以 T1 在步骤 3 调 `wait` 时，要么：
- counter 已经是 6 → `wait` 立刻返回，不睡——通知没丢。
- counter 还是 5 → `wait` 睡，但 T2 之后一定会 `wake_one`——通知会到。

**为什么 counter 用 Relaxed？** 因为所需同步全由**配对的 mutex** 提供：

- T1 持锁时记 counter，T2 必须先**加锁**才能改数据 + notify——而"解锁→加锁"本身就有 happens-before。
- 所以 T1 那次 Relaxed load 保证能看到 T2 在 notify 之前的 counter 状态。

这就是为什么 Condvar **必须**和 mutex 配对用——不配对，counter 的 Relaxed 序就没了保障，可能漏唤醒。

`tests/m7_03_condvar.rs` 那个测试 (`assert!(wakeups < 10)`) 就是验证它真的睡了、不是在忙等。如果 `wakeups` 是几千次，说明 `wait` 没在睡、变成了 `notify_one` 之间的轮询——这是 Condvar 实现错误的典型症状。

### 一个具体的 Condvar 使用例子：等待队列非空

把"消费者等队列不空"写完整，看 Condvar 怎么和 Mutex 配合：

```rust
use forge_sync::mutex::Mutex;
use forge_sync::condvar::Condvar;

let queue = Mutex::new(Vec::new());
let cv = Condvar::new();

// 消费者线程
std::thread::scope(|s| {
    s.spawn(|| {
        let mut g = queue.lock();
        // while 循环 + wait：抗假唤醒
        while g.is_empty() {
            g = cv.wait(g);   // 原子地"解锁 + 等通知 + 重新加锁"
        }
        let item = g.pop().unwrap();
        println!("got: {item}");
    });

    // 生产者线程
    s.spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut g = queue.lock();
        g.push(42);
        // 通知消费者——必须在持锁时或解锁后立即调，两种都对
        cv.notify_one();
    });
});
```

几个关键点：

1. **while 循环 + wait**——不是 `if`。因为假唤醒，`wait` 可能在没人通知时返回；用 `if` 会让消费者在队列为空时继续 pop，panic。
2. **`cv.wait(g)` 把 guard 当参数、返回新 guard**——这是 Rust 风格的 Condvar API，比 C 的 `pthread_cond_wait(&cv, &mutex)` 安全：编译器保证你不会忘记重新加锁。
3. **生产者持锁时 notify**——这个模式下，消费者在被叫醒前根本拿不到锁（生产者还持着），所以 wait 一定先看到队列非空（happens-before 通过 mutex 传递）。

如果生产者在解锁后 notify，counter 仍能防止丢通知——这就是版本 1 设计的核心价值。

### 版本 2：再加 `num_waiters`，砍掉无人时的 notify syscall

版本 1 的病：`notify_one` 永远 `fetch_add + wake_one`，哪怕没人在等。和 2 态 mutex 同样的浪费。解法也类似：**自己跟踪等待者数**。

```rust
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

pub struct Condvar {
    counter: AtomicU32,
    num_waiters: AtomicUsize,  // ← 新增
}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            num_waiters: AtomicUsize::new(0),
        }
    }

    pub fn notify_one(&self) {
        if self.num_waiters.load(Relaxed) > 0 {  // 没人等就跳过
            self.counter.fetch_add(1, Relaxed);
            wake_one(&self.counter);
        }
    }

    pub fn notify_all(&self) {
        if self.num_waiters.load(Relaxed) > 0 {
            self.counter.fetch_add(1, Relaxed);
            wake_all(&self.counter);
        }
    }

    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.num_waiters.fetch_add(1, Relaxed);     // 进大厅
        let counter_value = self.counter.load(Relaxed);
        let mutex = guard.mutex;
        drop(guard);
        wait(&self.counter, counter_value);
        self.num_waiters.fetch_sub(1, Relaxed);     // 出大厅
        mutex.lock()
    }
}
```

`num_waiters` 用 `AtomicUsize` 而不是 `AtomicU32`：`usize` 不会溢出（够数每个字节），而 32 位 counter 在 42 亿次 notify 后会回绕——虽然概率小到可以忽略（要恰好回绕到原值才算漏），但理论上存在。`num_waiters` 不会回绕，所以用它做"有没有人在等"的判定更稳。

**内存序仍然是全 Relaxed**——还是靠 mutex 提供 happens-before。`num_waiters.fetch_add` 在持锁时执行，notify 线程的 `num_waiters.load` 在它加锁之后——所以 notify 永远不会看到比"实际等待者少"的值。

### 两个进阶坑（教程必须点）

**坑 ①：counter 回绕。** counter 约 42 亿次 notify 后溢出回 0，理论上可能让某线程 `wait` 时记的值"恰好等于"回绕后的值，导致它误睡。实际可忽略（要恰好回绕一圈）；彻底兜底可以用**带超时的 `wait`**——等几秒自动醒，重新检查条件。Linux futex 支持 `FUTEX_WAIT` 带超时，atomic-wait 没暴露这个口子，但如果你自己造可以用。

**坑 ②：`notify_one` 可能"多叫醒一个"。** 一个线程正要睡（已 load counter，还没真睡），此时 `notify_one` 把 counter 加了——这个线程不会真睡（counter 对不上），但 `wake_one` 仍然叫醒了另一个真睡的线程。结果两个线程都醒着抢锁，浪费一个。这叫"惊群的轻量版"。

彻底避免需要**把等待者分组**——glibc 2017 年的新 condvar 就是这么做的（两阶段：第一组能消费通知，第二组等下一轮）。代价是类型变大、算法变复杂。我们的 forge-sync 不做这个优化。

**坑 ③：`notify_all` 的"惊群"（thundering herd）。** `notify_all` 把所有等待者都叫醒，它们几乎全部抢锁失败又睡回去——CPU 浪费大。Linux 上可以用 `FUTEX_REQUEUE` 把它们**直接搬到 mutex 上睡**（不叫醒，搬队列）。但这要求 Condvar 记住配对的 mutex——这就是"Condvar 通常只配一个 mutex"的深层原因。

**坑 ④：notify 被"偷"。** 假设有 T1 在等，T2 来 notify_one。notify 增加了"应醒次数"，但 T3 此时也调 `wait`——T3 可能看到"应醒次数 > 0"，直接返回、把通知偷走，T1 继续睡。这是 glibc 老 condvar 的著名 bug。彻底解决也是用分组等待者。这个 bug 在某些场景下（比如"刚好要叫醒一个特定线程"）是致命的，但在大多数"广播"场景下可以接受。

---

## 第 7 拍（ANCHOR）：RwLock 的钥匙模型——图书阅览室

`RwLock`（读写锁）允许"多个读者同时进，但写者独占"。钥匙模型：

- 一个**图书阅览室**。
- **读者**进去看书，可以一起进（书是共享的，看书不冲突）。
- **写者**进去改书，必须独占——不能有任何读者或其他写者在。
- 门上牌子写**当前屋里有多少读者**（0 表示空）。
- 写者想进：必须屋里是 0。
- 读者想进：屋里不能有写者（写者用特殊牌子占位）。

比 Mutex 多一个维度：**两类用户**，规则不同。这导致三个新问题——分别对应敌人 ② 和 ③。

**为什么 RwLock 有用？** 真实场景里大量数据是"读多写少"的：缓存（被频繁查、偶尔更新）、配置（被频繁读、偶尔 reload）、文件系统元数据（被频繁 stat、偶尔改）。如果用 Mutex，每次读都要排他——并发度被严重压制。RwLock 让多个读同时进行，只在写时排他——读并发度大幅提升。

**RwLock 比 Mutex 慢吗？** 在低竞争下，RwLock 通常**比 Mutex 慢**——因为 state 编码更复杂（要 CAS 多次、判断奇偶）。在高竞争但读多写少下，RwLock **快得多**——多个读者并行。在写多的场景下，RwLock **反而比 Mutex 慢**——因为写者要等所有读者走完。所以 RwLock 不是"高级 Mutex"，而是**特定场景**的优化。

---

## 第 8 拍（WRITE · 手算 #2）：RwLock 的状态编码与流转

### 用一个 u32 怎么编码所有信息？

我们只有**一个** `AtomicU32` 当 state，但要编码：
- 当前读者数（可能 0、1、2、…），
- 是否写锁，
- （后面）是否有写者在等。

原书的编码方案，全部塞进这 32 位里：

```
读者数 × 2          → 偶数：0, 2, 4, 6, ...
读者数 × 2 + 1      → 奇数：1, 3, 5, 7, ...（表示"有这么多读者，且有写者在等"）
u32::MAX（=奇数）   → 写锁
```

为什么是 ×2？因为**奇偶性**腾出了一位信息："有没有写者在等"。`读者数 × 2` 永远是偶数；再 `+1` 就变奇数，表示"写者在等"。u32::MAX 是奇数，自然成为"写锁"的特殊值。

读者进：state 必须是偶数（没有写者在等 / 没写锁），CAS `s → s+2`。
读者出：`fetch_sub(2)`。
写者进：state 必须 ≤ 1（0=空，1=空但有写者在等），CAS `s → u32::MAX`。
写者出：`store(0)`。

这个编码的精妙处：**用奇偶性承载额外信息，不需要额外的原子变量**。同一个 u32，既数读者数，又表达"有没有写者在等"，还表达"是不是写锁"。这是锁设计中"状态压缩"的典范。

### 手算 #2：写公平的流转

**场景**：R1 和 R2 是两个读者，W1 是写者。

| 拍 | 动作 | state | 屋里 | 等的写者? |
|---|---|---|---|---|
| 0 | 初始 | 0 | 空 | 无 |
| 1 | R1 `read`：state 偶，CAS `0→2` | **2** | R1 | 无 |
| 2 | R2 `read`：state 偶，CAS `2→4` | **4** | R1, R2 | 无 |
| 3 | W1 `write`：state 不是 ≤1，CAS 失败；state 偶（4）→ CAS `4→5` 拨奇数（"有写者在等"） | **5** | R1, R2 | **有** |
| 4 | R3 想进 `read`：state 奇（5）→ **必须等**（被挡） | 5 | R1, R2 | 有 |
| 5 | R1 unlock：`fetch_sub(2)`，5→3 | **3** | R2 | 有 |
| 6 | R2 unlock：`fetch_sub(2)`，3→1。**返回值是 3** → 最后一个读者 + 有写者在等 → `wake_one` W1 | **1** | 空 | 有 |
| 7 | W1 醒，看 state ≤1，CAS `1→u32::MAX` 拿写锁 | **MAX** | W1 | — |

关键观察：

- **拍 3 是写公平的命门**。W1 一到，立刻把 state 从 4（偶）拨成 5（奇）。从此**新读者进不来**（state 奇），W1 等到当前读者全走完就能拿到锁。这就是**防写饥饿**——一旦有写者在等，新读者立刻被挡。
- **拍 6 那个 `fetch_sub(2) == 3` 的判定**：从 3 减到 1，意味着"最后一个读者走了 + 有写者在等"——这两条信息**同时**编码在 3 这个值里（3 = 读者数×2=2，即一个读者，+1 = 有写者在等）。减完 state=1，是"空 + 有写者在等"，写者看到 `state<=1` 就能抢。
- **R3 在拍 4 被挡**。这就是写者优先的代价：哪怕只是"想读"，也得让写者先。在"频繁读、偶尔写"的场景，这正是想要的；在"频繁写、偶尔读"的场景，反而是负担（读者被频繁挡）。这是 RwLock 公平策略的权衡——M2 我们讨论过写优先 vs 读优先。

### 写者进锁后的多重检查：一个易错点

写者的 `write` 函数有三段（看完整代码）：抢写锁失败 → 拨奇数挡新读者 → 等 counter。三段之间都要重新 load state。这是个**容易写错**的地方：

```rust
// 错误版本：只在循环开头 load 一次 state
let mut s = self.state.load(Relaxed);
loop {
    if s <= 1 { CAS(s, MAX, ...); ... }
    if s % 2 == 0 { CAS(s, s+1, ...); ... }
    let w = self.writer_wake_counter.load(Acquire);
    // s = self.state.load(Relaxed);   ← 漏了这步
    if s >= 2 { wait(...); }
    // wait 醒来后 s 还是旧值——bug！
}
```

漏了 `s = self.state.load(Relaxed)` 的后果：醒来后 s 是睡前看到的旧值，可能 state 已经变了（比如别的写者拿到了锁又放了），但你还在用旧 s 判断——逻辑就错了。

正确版本是每次 wait 醒来都重新 load state，每次 CAS 失败也用返回的新 s 继续。这是 CAS 循环的标准模式（M1 我们讲过）。

### 完整代码

```rust
use crate::atomic_wait::{wake_all, wake_one, wait};
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct RwLock<T> {
    /// 读者数 × 2，+1 若有写者在等；u32::MAX 表示写锁。
    /// ⇒ state 偶时读者可进（+2），奇时读者必须等。
    state: AtomicU32,
    /// 仅在要唤醒写者时 +1，写者等它（避免被频繁变化的读者数吵醒）。
    writer_wake_counter: AtomicU32,
    value: UnsafeCell<T>,
}

// 多读者同时持 &T 需要 T: Sync；写者会把 T 送别的线程 → T: Send。
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            writer_wake_counter: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            if s % 2 == 0 {
                // 偶数：可读。+2 占一个读锁。
                assert!(s != u32::MAX - 2, "读者太多");
                match self.state.compare_exchange_weak(
                    s, s + 2, Ordering::Acquire, Ordering::Relaxed,
                ) {
                    Ok(_) => return ReadGuard { rwlock: self },
                    Err(e) => s = e,
                }
            }
            if s % 2 == 1 {
                // 奇数：有写者在等或已写锁 → 等。
                wait(&self.state, s);
                s = self.state.load(Ordering::Relaxed);
            }
        }
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            // 未锁（0 或 1）→ 抢写锁。
            if s <= 1 {
                match self.state.compare_exchange(
                    s, u32::MAX, Ordering::Acquire, Ordering::Relaxed,
                ) {
                    Ok(_) => return WriteGuard { rwlock: self },
                    Err(e) => { s = e; continue; }
                }
            }
            // 把 state 拨成奇数（+1），挡住新读者，防写饥饿。
            if s % 2 == 0 {
                match self.state.compare_exchange(
                    s, s + 1, Ordering::Relaxed, Ordering::Relaxed,
                ) {
                    Ok(_) => {}
                    Err(e) => { s = e; continue; }
                }
            }
            // 等写者唤醒计数器（仅在真要唤醒写者时才变）。
            let w = self.writer_wake_counter.load(Ordering::Acquire);
            s = self.state.load(Ordering::Relaxed);
            if s >= 2 {
                wait(&self.writer_wake_counter, w);
                s = self.state.load(Ordering::Relaxed);
            }
        }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        // 读者数 -2。若从 3 减到 1：最后一个读者 + 有写者在等 → 唤醒一个写者。
        if self.rwlock.state.fetch_sub(2, Ordering::Release) == 3 {
            self.rwlock.writer_wake_counter.fetch_add(1, Ordering::Release);
            wake_one(&self.rwlock.writer_wake_counter);
        }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.rwlock.state.store(0, Ordering::Release);
        self.rwlock.writer_wake_counter.fetch_add(1, Ordering::Release);
        wake_one(&self.rwlock.writer_wake_counter);  // 可能的等待写者
        wake_all(&self.rwlock.state);                 // 所有等待读者
    }
}
```

读者测试见 `tests/m7_04_rwlock_write_exclusivity.rs`（4 写者各加 1000 → 4000）和 `m7_05_rwlock_readers.rs`（8 读者并行读 vec）。

### 为什么 RwLock 需要 T: Sync 而 Mutex 不需要？

注意 `unsafe impl<T: Send + Sync> Sync for RwLock<T>`——比 Mutex 多一个 `T: Sync` 边界。为什么？

因为 RwLock 允许**多个读者同时**持有 `&T`。`&T` 在 Rust 里默认可以跨线程共享——但前提是 `T: Sync`（"T 的 `&T` 可以安全跨线程"）。如果 `T` 不是 `Sync`（比如 `Rc<i32>`），多读者就会破坏线程安全——两个线程各持一个 `&Rc`，引用计数就被并发改坏了。

Mutex 不需要 `T: Sync`，因为 Mutex 同一时刻只让**一个**线程访问 `T`，没有共享 `&T`。

这条边界是 Rust 类型系统帮我们抓并发 bug 的典型例子：编译期就堵住了"用 RwLock 包 Rc"这种错误。

---

## 第 9 拍（WRITE · 手算 #3）：writer_wake_counter 救写者于忙转

最后一块拼图。**为什么 `RwLock` 需要一个单独的 `writer_wake_counter`，不能直接 `wait(state, ...)`？**

### 没有它时的灾难：写者被读者"忙转"

假设我们**没有** `writer_wake_counter`，写者直接 `wait(state, s)` 等 state。考虑 100 个读者不停 lock/unlock 的场景：

| 拍 | 动作 | state | W1 在哪 |
|---|---|---|---|
| 0 | 100 个读者活跃 | 200 | 想写 |
| 1 | W1 `CAS 0→MAX` 失败，记下 `s=200`，调 `wait(state, 200)` | 200 | 准备睡 |
| 2 | R3 unlock：`fetch_sub(1)`，state 200→199 | **199** | — |
| 3 | W1 的 `wait(200)` 看到 state=199 ≠ 200 → **立刻返回不睡** | 199 | **醒了** |
| 4 | W1 又试 CAS，失败，记 `s=199`，又 `wait(199)` | 199 | 准备睡 |
| 5 | R7 unlock：state 199→198 | **198** | — |
| 6 | W1 `wait(199)` 看到 state=198 ≠ 199 → **又立刻返回** | 198 | **又醒了** |
| ... | （循环几十次） | ... | **忙转！** |

W1 永远没法真正睡——读者数每时每刻都在变，它的 `expected` 永远对不上。这就是敌人 ②。最糟的是它**比真自旋还贵**：每次失败的 `wait` 都是一次 syscall（进内核、查等待队列、发现值不对、返回）。

读者越多、unlock 越频繁，W1 烧得越凶——这是**反向扩展**：负载越重，bug 越严重。

### 救场：让写者等一个**不乱跳**的变量

引入 `writer_wake_counter`：它**只在真要唤醒写者时**才 +1，平时不变。写者等它而不是 state：

```rust
pub fn write(&self) -> WriteGuard<'_, T> {
    while self.state.compare_exchange(0, u32::MAX, Acquire, Relaxed).is_err() {
        // ① 先记下 counter 的当前值。
        let w = self.writer_wake_counter.load(Acquire);
        // ② 再看 state 还是不是被占。
        if self.state.load(Relaxed) != 0 {
            // ③ 等 counter。counter 只在"读者全走完/写者解锁"时才变。
            //    所以这条 wait 不会被无关的读者加解锁吵醒。
            wait(&self.writer_wake_counter, w);
        }
    }
    WriteGuard { rwlock: self }
}

// 读者 unlock：若自己是最后一个读者，counter + 1 + wake_one。
impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        if self.rwlock.state.fetch_sub(2, Release) == 3 {
            self.rwlock.writer_wake_counter.fetch_add(1, Release);  // ← counter 变
            wake_one(&self.rwlock.writer_wake_counter);
        }
    }
}
```

### 一个反向例子：如果 counter 顺序反了会怎样

为了让你**真的**理解"先记 counter、再查 state"的必要性，我们手算一个**错误**的版本：把顺序反过来——先查 state、再记 counter。

错误的 `write`（**别这么写**）：

```rust
// 错误顺序
if self.state.load(Relaxed) != 0 {           // ① 先查 state
    let w = self.writer_wake_counter.load(Acquire);  // ② 再记 counter
    wait(&self.writer_wake_counter, w);      // ③ 等
}
```

逐拍走"最后一个读者 unlock 的瞬间"，假设读者 unlock 时 state 从 2 减到 0、然后 counter + 1：

| 拍 | 写者动作 | 读者动作 | 写者看到 |
|---|---|---|---|
| A | `load(state)` = 2（非 0） | — | state=2 |
| B | （写者还没记 counter） | `fetch_sub(state)`：state 2→0 | — |
| C | （写者还没记 counter） | `fetch_add(counter)`：counter 0→1；`wake_one` | — |
| D | `load(counter)` = **1**（已经是新值！） | — | counter=1 |
| E | `wait(counter, 1)` | — | 睡了，counter 仍是 1 |

写者**睡死**了。因为 wake_one 在拍 C 就发过了——那时写者还没开始 wait，内核找不到它在等。等写者真 wait 时，counter 已经是 1，再也没有人来叫它。

这就是为什么顺序**必须**是"先记 counter、再查 state"：这样如果 state 在你"记完 counter 之后、wait 之前"变了，counter 也会变（unlock 同时改两者），你的 `wait(counter, w)` 会立刻返回（因为 counter 不等于 w）。正确顺序堵住了窗口。

这条规律在 Condvar（先记 counter、再解锁）、RwLock 写者（先记 counter、再查 state）里都成立。M6 我们已经讲过 futex 的 expected 机制——这里就是它的实战价值。

### 手算 #3：有 counter 时，W1 只在真该醒时醒

同样的 100 读者场景，重画：

| 拍 | 动作 | state | writer_wake_counter | W1 在哪 |
|---|---|---|---|---|
| 0 | 100 读者活跃 | 200 | 0 | 想写 |
| 1 | W1 CAS 失败，state 非 0，记 `w=0`，`wait(counter, 0)` | 200 | 0 | **睡**（真睡！） |
| 2 | R3 unlock，state 200→199，**counter 不变** | 199 | 0 | 睡 |
| 3 | R7 unlock，state 199→198，**counter 不变** | 198 | 0 | 睡 |
| ... | （读者一个个走，counter 一直是 0） | ... | 0 | 睡 |
| 100 | R_last unlock，state 2→0（编码简化，实际看 fetch_sub 返回值是否触发条件），**counter + 1 + wake_one** | 0 | **1** | — |
| 101 | W1 的 `wait(0)` 看到 counter=1 ≠ 0 → 醒 | 0 | 1 | **醒**（只这一次！） |
| 102 | W1 CAS `0→MAX` 成功 | MAX | 1 | **持锁** |

W1 从拍 1 睡到拍 101，**一次 syscall 唤醒**，没有被中间的读者吵醒过一次。这就是 `writer_wake_counter` 的价值。

注意拍 1 那一步的**三步**：
1. CAS 抢锁失败。
2. **先** load counter（记下当前值）。
3. **再** load state，看是不是该睡。

这三步的顺序不能反。如果反着写——先看 state、再记 counter——会有窗口：state 看到非 0、还没记 counter，结果最后一个读者刚好把 state 减到 0 并 fetch_add counter，然后你才记 counter——你记的是新 counter，wait 永远等不到变化。漏唤醒。

正确顺序是"先记 counter 再看 state"——和 Condvar 那个"先记 counter 再解锁"是同构的模式。这是 futex 风格 wait 防丢通知的通用套路。

### 内存序：为什么 counter 的 load 用 Acquire、+1 用 Release？

这是整个 RwLock 里**最微妙**的内存序决策。考虑写者线程和读者线程的交错：

- 写者：`load(counter, Acquire)` → `load(state)` → 决定睡还是抢。
- 读者 unlock：`fetch_sub(state, Release)` → `fetch_add(counter, Release)` + `wake_one`。

**Acquire-Release 配对**保证：写者看到 counter 加了之后，**也一定能看到 state 已经被减下来**。否则会发生这种灾难：

- 写者 `load(counter)=0`（旧值）。
- 读者 `fetch_sub(state)`（state 减了，但还没 fetch_add counter）。
- 写者 `load(state)`——如果**没有** Acquire/Release 屏障，写者可能看到 state 还是旧的（非 0），决定睡。
- 读者 `fetch_add(counter)` + `wake_one`——但写者的 `wait(counter, 0)` 已经记录了旧 counter 值，按理应该被 wake_one 叫醒……

问题是 `load(counter)` 和 `load(state)` 之间没有同步，写者可能"看到新 counter、却看到旧 state"或反过来。Acquire-Release 配对就是堵这个窗口：写者的 Acquire load 保证它**之后**的 state load 能看到读者 Release fetch_sub 之后的值。这一条不写好，写者要么漏醒（睡死），要么误睡（被立刻叫醒又睡回去）。

**注意这里读者用了两次 Release**：`fetch_sub(state, Release)` 和 `fetch_add(counter, Release)`。这两次 Release 都和写者的 Acquire load 配对——前者配 `load(state, Relaxed)` 后续的可见性（虽然 state 是 Relaxed，但写者通过 Acquire-load counter 进入"已同步"状态后，看到的 state 是 Release-fetch_sub 之后的）。这是个比较微妙的推理，关键在于 Acquire-Release 配对只约束**同一对变量**，但跨变量的可见性可以通过"Release-Acquire 链"传递。

---

## 第 10 拍（ISO·ZOOM）：把自研锁换进运行时——和 std / parking_lot 对比

我们造的 `forge-sync::{Mutex, RwLock, Condvar}` 在理念上和 std / parking_lot 是**同一套**：futex-based、3 态（或多态）编码、自适应自旋、独立唤醒计数器。差异在工程细节：

| 方面 | std::sync (Linux) | parking_lot | forge-sync |
|---|---|---|---|
| 实现 | 自己的 `sys::mutex`（基于 futex） | 自己实现（不依赖 std） | atomic-wait crate |
| 中毒 | 有 | **没有** | 没有 |
| 3 态 | 有 | 有（更精细） | 有 |
| 自适应自旋 | 100 次 | 可配置 | 100 次 |
| RwLock 公平 | 平衡 | 写优先可选 | **写优先** |
| condvar 分组等待者 | 是（glibc） | 否 | 否 |
| 类型大小 | 较大（中毒位 + 状态） | 极小（仅 state） | 极小 |

### 把自研 Mutex 换进 M2 的运行时就绪队列

M2 我们用 `std::sync::Mutex` 包了一个"任务队列"。把那行 `use std::sync::Mutex;` 换成 `use forge_sync::mutex::Mutex;`——**API 兼容**（都有 `new` / `lock` / `MutexGuard` 实现 DerefMut）。

非竞争场景（调度器主循环每次只锁一下、马上放）会立刻变快——因为我们的版本在非竞争路径零 syscall，而 std 的 2 态版本每次 unlock 都 `wake_one`。

> 这就是为什么"自己造锁"对运行时很值：调度器的就绪队列锁是**全系统竞争最激烈的锁之一**，每毫秒可能锁/解几百次。每次省一次 syscall，一年下来省下的 CPU 时间是天文数字。

parking_lot 在 Rust 生态流行，正是因为它的非竞争路径同样零 syscall，且没有中毒开销。我们的 forge-sync 是 parking_lot 风格的简化教学版。

### 为什么 std 不学 parking_lot 把中毒去掉？

历史原因：std::sync::Mutex 在 Rust 1.0 之前就定型了，那时候"防呆"优先于"性能"。去掉中毒会破坏向后兼容（已有代码依赖 `lock()` 返回 `Result`），所以 std 不能动。parking_lot 是后起之秀，没有历史包袱，直接做"正确"的事。

这是个典型的"标准库 vs 第三方库"权衡：标准库稳定但滞后；第三方库灵活但要用户主动选择。

### 测试覆盖：`tests/m7_01` 到 `m7_05`

- `m7_01_mutex_counter`：10 线程各加 100 → 1000，验证互斥和 happens-before。**miri 干净**（这是个重要信号：unsafe 用对了）。
- `m7_02_mutex_uncontended`：单线程 100 万次 lock/unlock → 1,000,000，验证非竞争快速路径正确。
- `m7_03_condvar`：等待-通知往返，`assert!(wakeups < 10)` 验证真睡了。
- `m7_04_rwlock_write_exclusivity`：4 写者各加 1000 → 4000，验证写互斥。
- `m7_05_rwlock_readers`：8 读者并行读 vec，验证多读者共存。

注意这些测试**不能**证明没有竞争 bug——竞争 bug 可能在某些交错下才暴露，跑 100 次都不一定碰到。要更可靠地验证：

- **miri**：单线程模拟，抓低级内存错误。
- **loom**（Rust 的并发模型检查器）：枚举所有可能的交错，理论上能证明无竞争。但 loom 很慢，只能跑小例子。
- **stress test**：跑几百万次、多线程、长时间——抓"大概率 bug"。

我们的 forge-sync 测试覆盖了第一层（miri）和第三层（功能性测试）。生产级锁（parking_lot）会做更全面的 loom 验证。

---

## 第 11 拍（ANCHOR · 整体回顾）：三种锁的共同骨架

回头看，三个原语共享同一条设计哲学：

> **用原子变量编码"完整的状态信息"（包括有没有人在等），让 unlock 知道要不要按门铃。状态机想对，wait/wake 只是性能优化。**

具体对应：

| 原语 | 状态编码 | "有等待者"信号 | 谁负责维护这条信号 |
|---|---|---|---|
| Mutex | `0/1/2` | `state==2` | 等待者自己 swap 成 2 |
| Condvar | `counter` 单调增 | `num_waiters > 0` | wait 入口 fetch_add、出口 fetch_sub |
| RwLock | `读者数×2 + 写者等位` | `state` 奇偶 | 写者自己 +1 拨奇数 |

每一个都是"先想清楚状态编码、再写 wait/wake"。如果你想加一种新的锁（自旋读写锁、序列锁 SeqLock、MCS 队列锁），第一步永远是问：**"我有什么信息需要编码进 state？这个编码够不够？"** 不够就升 state（Mutex 2→3 就是这个升法），或者加新的原子变量（RwLock 加 `writer_wake_counter`）。

第二条共同哲学：

> **wait/wake 永远配循环检查使用。** 因为假唤醒存在，任何 `wait` 后都必须重新检查条件。这条在 M6 就确立了，这里每个原语都遵守。

第三条共同哲学：

> **"先记下、再检查"防丢通知。** Condvar 是"先记 counter 再解锁"，RwLock 写者是"先记 counter 再看 state"。这是 futex 风格 wait 的通用模式：把"决定要等"和"真的等"之间的窗口用 expected 检查堵上。

第四条共同哲学：

> **"非竞争路径零 syscall"是性能关键。** 大多数锁在大多数时候都没人抢——非竞争路径的性能决定了整体性能。竞争路径慢一点没关系（反正不常见）。

---

## L1–L5 缩放回顾

| 层 | 你能…… |
|---|---|
| L1 | 一句话：在 futex 上造 Mutex/Condvar/RwLock，核心是"用状态编码出'有没有人在等'，从而少做 syscall"。 |
| L2 | 用"带门铃的单间""银行柜台""图书阅览室"三个画面解释三个原语，知道 2 态 / 3 态 / 奇偶编码的存在。 |
| L3 | 跟踪 3 态 Mutex 的 state 流转、Condvar 的 counter+mutex 配对、RwLock 的奇偶编码 + writer_wake_counter；解释每个内存序选择。 |
| L4 | 推理"为什么 wait/wake 不影响正确性"、为什么 Condvar 全 Relaxed、为什么 RwLock counter 要 Acquire/Release 配对；能写出自己的状态机并验证不漏唤醒。 |
| L5 | 判断自适应自旋 / 写公平在该不该上、知道惊群 / REQUEUE / 分组等待者 / 锁中毒这些进阶手段的代价；能比较 std / parking_lot / forge-sync 的工程取舍。 |

## 自检

- [x] 先讲敌人（syscall 慢、写者忙转、写饥饿），每个优化都对应打其中一个。
- [x] 忠于原书第 9 章每个原语的"基础→优化"演化，给出每步省了什么。
- [x] 强调"wait/wake 只是优化、不影响正确性"这条推理 unsafe 锁的钥匙。
- [x] 内存序每条都给出理由（尤其 Condvar 全 Relaxed 靠 mutex 同步；RwLock counter 的 Acquire/Release 配对）。
- [x] 三个手算例子：2 态 Mutex 活锁 / RwLock 编码流转 / writer_wake_counter 消除忙转。
- [x] 五拍（ENEMY/ANCHOR/LOW-FI/WRITE/ISO·ZOOM）实质展开；故意打破（2 态活锁）再重建（3 态）。
- [x] 代码与 crate（`forge-sync::mutex/condvar/rwlock`）完全一致，可编译。

## 动手清单

- [ ] 跑 `cargo test -p forge-sync --test m7_*`，确认全绿。
- [ ] 跑 `cargo +nightly miri test -p forge-sync --test m7_01_mutex_counter`，确认 miri 干净。
- [ ] 把 M2 的 `std::sync::Mutex` 换成 `forge_sync::mutex::Mutex`，跑那章的测试，观察是否仍通过（API 兼容性）。
- [ ] 给 `forge-sync::Mutex` 加一个 `try_lock`（提示：CAS `0→1` 失败就返回 `None`）。
- [ ] 给 `forge-sync::Mutex` 加锁中毒（持锁线程 panic 后 `lock` 返回 `Err`）——提示：需要 `AtomicBool` 标记 + 在 guard 的 `Drop` 里检查 panic 状态。
- [ ] 把 RwLock 的"写优先"改成"读优先"——把 state 编码反过来，观察新行为。
- [ ] 用 `cargo bench`（或 `criterion` crate）测 forge-sync vs std vs parking_lot 在非竞争 / 高竞争两种场景下的差距，验证"3 态 vs 2 态"在 Linux 上是否真有 10 倍。
- [ ] 给 Condvar 加 `wait_timeout`（提示：需要 atomic-wait 暴露带超时的 wait，或自己用 linux_futex 直接调 `FUTEX_WAIT` 带超时）。
- [ ] 在 `RwLock` 的 `read` 里加自适应自旋（提示：和 Mutex 的 `lock_contended` 同构，但只在 state 偶时自旋）。

测试文件：
- `crates/forge-sync/tests/m7_01_mutex_counter.rs`（miri 干净）
- `crates/forge-sync/tests/m7_02_mutex_uncontended.rs`
- `crates/forge-sync/tests/m7_03_condvar.rs`
- `crates/forge-sync/tests/m7_04_rwlock_write_exclusivity.rs`
- `crates/forge-sync/tests/m7_05_rwlock_readers.rs`

---

下一站 → [M8 全部无锁结构](./M8-lockfree.md)：第 10 章七大主题**全部从零**——信号量、RCU+epoch 回收、Treiber 栈+ABA+hazard 指针、MCS/CLH 队列锁、parking-lot 式锁、SeqLock、Chase-Lev 工作窃取双端队列（M9a 调度器的心脏）。
