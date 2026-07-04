# M3 — 自建 SpinLock，以及 CPU 的真相

> 模块：`forge-core::spin`　|　测试：`crates/forge-core/tests/m3_*.rs`
> 跑：`cargo test -p forge-core --test m3_*`　|　miri：`cargo +nightly miri test -p forge-core --test m3_*`

M2 我们用现成的 `std::sync::Mutex`，黑盒一个，能用就行。这一模块我们**第一次自己写 `unsafe`**——从零造一个自旋锁 `SpinLock<T>`，把 M1 学的 Acquire/Release 落到一个能跑、能被 miri 验证的真实锁上。然后顺带掀开 CPU 的盖子：为什么"内存序在 x86 上几乎不花钱，到 ARM 上就要专门指令"，为什么"两个不相干的原子变量挨在一起会慢三倍"，为什么"我机器上能跑"这句话根本不算数。

这一模块不短。慢一点读。每一节只推一件事。

---

## 敌人先行：睡-醒一次太贵

先把痛苦造出来，让你**身体里**感到那个不合理。

回忆 M2 的 `Mutex`：你 `lock()` 一次，拿不到锁的线程**不烧 CPU**——它被操作系统"哄睡"，等锁释放时再被叫醒。这在锁持有一会儿的场景里很合理：睡觉期间这个核可以让给别人，电费也省。

但你现在要写的不是那种场景。你要写**性能敏感的热路径**：调度器里某个统计计数、无锁队列的尾指针、网卡收包的快速通道——锁只被持**几十纳秒**一瞬，线程们又跑在不同物理核上。这时"哄睡"的代价出来了：

- 操作系统把你从用户态切到内核态（一次系统调用）；
- 内核把你挂到等待队列；
- 调度器挑另一个线程来跑；
- 上下文切换：把你的寄存器、TLB、缓存状态都换一遍；
- 锁释放时，内核再把你叫醒：又是一次系统调用、又一次上下文切换。

**整个睡-醒周期大概要 1–10 微秒**（具体看 OS 和硬件），其中绝大部分是"什么有用的事都没干"。而你的锁本来只会被持 50 纳秒。你算一下这个荒谬：**为了不空转 50 纳秒，你让线程白白睡了 5000 纳秒**。线程被叫醒时锁早就空了，可它还在穿睡衣、找拖鞋。

你能感到这个荒谬吗：**为了让线程"不空转"，我们让它白白睡了一觉，醒来发现事情早就该自己干。**

这就是自旋锁（spin lock）存在的理由。锁不上时，**不睡**——原地反复试着锁（这叫**忙等 / 自旋**），直到抢到。代价：自旋**烧 CPU**，所以自旋锁只适合"持锁极短、且大概率马上能抢到"的场景。它不是 Mutex 的替代品，是另一种工具。

### 一个具体的数字对比

让数字说话。假设你的锁持有时间是 50 ns（比如就改一个计数器），有 4 个线程在 4 个核上跑：

- **std::sync::Mutex 路径**：抢不到锁 → park（一次系统调用约 1 µs）→ 被唤醒（又一次系统调用约 1 µs）→ 上下文切换（约 1 µs）。**总计约 3 µs = 3000 ns**，是持锁时间的 60 倍。而且这 3 µs 期间，你的线程啥都没干。
- **SpinLock 路径**：抢不到锁 → 自旋若干次（每次约 10 ns）→ 别人解锁时立刻抢到。如果别人持锁 50 ns，你最多自旋 5 到 10 次 = **总计约 50 到 100 ns**，和持锁时间同一量级。

这个 30 倍的差距，就是自旋锁存在的全部理由。当然反过来——如果锁持有 1 ms，自旋锁要白白烧 1 ms 的 CPU，而 Mutex 让你睡过这段。**所以自旋锁的适用条件很苛刻：持锁极短、worker 数不超过物理核、争用可预测**。脱离这个场景用自旋锁，就是性能自残。

> 现实里很多 `Mutex`（包括某些平台的 `std::sync::Mutex`）在叫 OS 睡线程之前会**先自旋一小会儿**——取两者之长。我们 M7 自建 futex 锁时也会做这个"自适应自旋"。这一模块把自旋这块单独剥出来讲透。

---

## 锚点：自旋锁就是"反复按门铃"

想象你站在朋友家门口，门没锁但你不确定他人在不在。你**按一下门铃、等两秒、再按一下**——这就是自旋。每次按是一次原子"试探 + 抢占"。门一开你就进去；不开就一直按。

把这张图刻进脑子：

- **门的状态**：开 / 关。一个布尔。
- **按门铃**：原子地"看门是不是开，同时把门关上"（防止别人和你同时挤进去）。这是关键——"看"和"关上"必须**原子**完成，否则你和别人可能同时看到门开、同时挤进去。
- **进门**：拿到了对屋里东西的独占访问。
- **出门**：把门重新打开（让别人能进）。

这一整节后面所有的代码，都是把这张图翻译成 Rust。"门"是一个 `AtomicBool`，"按门铃"是一次 `swap`，"出门"是一次 `store`。就这样。整件事的复杂度不在概念——概念就是上面这四句话——复杂度全在"**怎么让 Rust 的类型系统相信我们没撒谎**"和"**怎么让 CPU 的硬件机制保证别的核能看到我们的修改**"。前者要三版演化，后者要掀开 CPU 盖子。

---

## 第一版：最小自旋锁（安全但没保护任何数据）

先把"门"造出来。一个布尔、一个 `swap`、一个 `store`，就完事了。

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::hint::spin_loop;

pub struct SpinLock {
    locked: AtomicBool,   // 门：false=开，true=关
}

impl SpinLock {
    pub const fn new() -> Self {
        Self { locked: AtomicBool::new(false) }
    }

    pub fn lock(&self) {
        // 反复按门铃：原子地"读旧值、写 true"。
        //   返回 false（门原本开着）→ 我抢到了，跳出循环。
        //   返回 true（门已关）       → 别人在里面，自旋重试。
        while self.locked.swap(true, Ordering::Acquire) {
            spin_loop();   // 提示 CPU："我在自旋"，让它优化流水线（不会让线程睡觉）
        }
    }

    pub fn unlock(&self) {
        // 出门：把门重新设为开。
        self.locked.store(false, Ordering::Release);
    }
}
```

先把每一行讲透，再回头讲为什么是这两个 Ordering。

`swap(true, Acquire)` 是一次**原子读-改-写**（read-modify-write，RMW）。它在一个不可打断的指令里干两件事：把 `locked` 的当前值读出来返回，同时把 `true` 写回去。因为原子，所以不可能出现"两个线程同时看到 false、同时以为自己抢到了"。要么 T1 先到（看到 false、写成 true），T2 后到（看到 true、自旋）；反之亦然。这就是"互斥"的来源——不在数据上加锁，而在**门**上加锁。

为什么是 `swap` 而不是 `load` 看一眼再 `store`？因为后两步中间有缝隙：T1 load 看到 false，正准备 store true，此刻 T2 也 load 看到 false——两个都以为自己抢到了。`swap` 把"看 + 写"焊成一步，缝隙消失了。这是所有原子 RMW 操作的核心价值。

`spin_loop()` 是一个**提示指令**（x86 上编成 `pause`，ARM 上编成 `yield`）。它告诉 CPU"我正在自旋等某个变量变化"，CPU 据此优化流水线——比如让出执行资源给同核的超线程伙伴、避免流水线中错误的分支预测惩罚、降低功耗。但它**绝不**调用操作系统把线程睡掉——这正是自旋和 park/sleep 的根本区别。一般包含它都是好主意；具体效益视硬件而定，某些场景甚至可以连按几次 `spin_loop` 再去试 `swap`，减少缓存行争用。

### 为什么是 Acquire 和 Release？

这里是这一模块的**核心一段**，慢慢读。**严禁跳过。**

我们 M1 讲过：`Acquire` 的 load 只允许"读"它**之后**的操作；`Release` 的 store 只允许"读"它**之前**的操作。合起来，`Release` 之前的所有写、对 `Acquire` 之后的读**全部可见**——这就是 happens-before 关系。

锁的语义刚好就是这样：

- 临界区里你写了一堆共享数据（`data = 42`、`vec.push(x)`……）。
- 解锁的 `store(false, Release)` 把这些写**打包发布**。
- 下一个线程 `swap(true, Acquire)` 拿到锁时，**上一个临界区里的所有写都对它可见**。

少了任一个 Ordering，这个保证就破了。下面 ISO·ZOOM 1 那节我们会停下来手算，破了之后会发生什么——你会亲眼看到 T2 拿到锁却读到旧值。

> 一个常见的初学者误区：以为"原子"就意味着"别的核马上能看到"。**错**。原子只保证"读-改-写不可打断"，**不**保证"写完后立刻对其它核可见"。可见性是**内存序**的事，原子和内存序是两件事。`Relaxed` 的原子操作仍然是原子的，但**不保证可见性顺序**。这一区分我们在手算那一节会看得清清楚楚。

**第一版的现状**：接口完全安全（误用不致 UB——顶多死锁或活锁），但**没用**——它不保护任何数据。用户想用它保护共享变量，还得自己写 `unsafe`。我们得让 `lock()` 直接返回被保护的数据。

### compare_exchange_weak 是另一条路

顺带提一下：`swap` 不是唯一选择。也可以写：

```rust
while self.locked.compare_exchange_weak(
    false, true, Ordering::Acquire, Ordering::Relaxed
).is_err() {
    spin_loop();
}
```

`compare_exchange_weak(expected, new, success, failure)` 干的事：如果 `locked == expected`（false），就写成 `new`（true），返回 Ok；否则返回 Err，并把当前值塞给你。它和 `swap` 的差别在语义层面：`swap` 是"无脑换成 true"，`compare_exchange` 是"如果是 false 才换"。两者在这把锁里效果相同。

但 ARM 上 `compare_exchange_weak` 编成 LL/SC 循环（详见下一节），允许**假失败**——明明是 false，也可能 Err。所以 `weak` 版**只适合放在循环里**用；想"试一次就走"必须用 `compare_exchange`（strong 版）。M7 我们会更细地对比它们。

---

## 第二版：让 lock 返回 `&mut T`（开始要 `unsafe` 了）

我们想让 `SpinLock<T>` 内部存一个 `T`，`lock()` 直接返回 `&mut T`。麻烦在于：`lock()` 拿的是 `&self`（共享引用），却要返回 `&mut T`（独占引用）。这在 Rust 的别名规则里**默认是非法的**——你不能从一个共享引用变出可变引用，否则编译器关于"共享即不可变"的优化前提就崩了。

Rust 给这种"逻辑上独占、语法上共享"的需求准备了**内部可变性**原语：`UnsafeCell<T>`。它是所有内部可变类型的底层发动机（`Cell`、`RefCell`、`Mutex` 全靠它）。它干的事很简单：给你一个 `*mut T`（裸指针），告诉你"我不管了，你自己保证用得对"。它**不是**魔法——它只是 Rust 借口"我看不见你"的一个逃生舱。

### UnsafeCell 到底做了什么

读者可能会问：`UnsafeCell<T>` 在内存里和 `T` 有什么区别？答案：**布局上完全一样**，零开销。它只是给编译器一个**信号**——"这个字段可能被别名改写，不要做'共享即不可变'的优化"。具体来说：

- 没有 `UnsafeCell` 的 `&T`，编译器可以假设"只要我持有这个引用，对应的内存就不会变"。它可以缓存读结果、可以做常量传播。
- 有 `UnsafeCell` 的 `&T`，编译器**不敢**这么假设——它必须每次重新读内存，因为可能有别的线程（或同一个线程的另一次借用）通过 `UnsafeCell::get()` 改写了它。

这就是为什么 `Cell`、`RefCell`、`Mutex` 内部都必须用 `UnsafeCell` 包裹可变字段——否则编译器会"优化"掉本该发生的重新读取，产生 bug。我们自建 SpinLock 同理：`value` 字段必须用 `UnsafeCell<T>`，否则编译器看到 `&self` 就会以为 `value` 不会变。

```rust
use std::cell::UnsafeCell;

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}
```

但 `UnsafeCell` 有个"传染"属性：它**故意不实现 `Sync`**。这是 Rust 的安全网——它知道你接下来要做的事可能有并发风险，所以先**禁止**这个类型跨线程共享，逼你**主动**承诺"我保证安全"。我们要把这个承诺写成代码。

### unsafe impl：向编译器承诺

```rust
// 安全性论证（写 unsafe impl 时必须能说清这段话）：
//   1. SpinLock 同一时刻只让**一个**线程碰 T（靠 lock 的原子互斥）。
//      所以不需要 T: Sync——我们从不让两个线程并发共享 &T。
//   2. 但 T 会从一个线程"送"到另一个（A 锁-改-解锁；B 后续锁能看到 A 的改）。
//      所以要求 T: Send——值要能跨线程搬。
unsafe impl<T: Send> Sync for SpinLock<T> {}
```

这段论证就是 M1 的 Send/Sync 框架在实战。把它背下来——后面所有自建锁都长这样。

读者可能要问：那 `SpinLock<T>: Send` 呢？要 `T: Send` 时 `SpinLock<T>` 才能 Send（拿到锁的所有权搬到另一个线程，相当于把 T 搬过去）。但这个 auto-trait 编译器**自动推导**——只要所有字段都是 Send，结构体就 Send。我们的 `AtomicBool` 是 Send、`UnsafeCell<T>` 在 `T: Send` 时是 Send，所以 `SpinLock<T>: Send` 在 `T: Send` 时自动成立，不用手写 `unsafe impl`。

### 为什么不要求 `T: Sync`？

这是初学者最容易卡的一步。**为什么 RwLock 的读端要求 `T: Sync`、SpinLock/Mutex 不要求？**

答：因为 SpinLock/Mutex **同一时刻只让一个线程**碰 T。这个线程要么独占 `&mut T`（写），要么独占 `&T`（读，但仍然不让别人同时读）——总之**不并发**。所以 T 只需要能"搬"过去（Send），不需要能"被多个线程同时持有共享引用"（Sync）。

RwLock 不一样：它允许多个 reader 同时持有 `&T`。多个线程同时碰 `&T`，这就要求 `T: Sync`。

把这条规则刻进脑子：**独占访问要求 Send，并发共享访问额外要求 Sync**。所有锁的设计都围绕这条规则。

### 写 lock 和 unlock

```rust
impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> &mut T {
        while self.locked.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        // 安全：我们刚刚抢到了锁，此刻没有别的线程能碰 T。
        unsafe { &mut *self.value.get() }
    }
}
```

`UnsafeCell::get()` 返回 `*mut T`（裸指针），把它转成 `&mut T` 必须 `unsafe`——这正是 `unsafe` 的意义："编译器没法证明这里没数据竞争，但我（程序员）用锁保证了"。

注意返回类型的生命周期。`fn lock(&self) -> &mut T` 经生命周期省略等价于 `fn lock<'a>(&'a self) -> &'a mut T`——返回的 `&mut T` 的生命周期**绑死在 `&self` 上**，只要 `SpinLock` 还活着，这个引用就"看起来"一直有效。这是我们后面要解决的麻烦。

### 第二版的失败：unlock 没法表达

第二版的 `lock()` 返回 `&mut T`，其生命周期绑死在 `&self` 上——只要锁还活着，这个 `&mut T` 就"看起来"一直有效。那 `unlock` 怎么办？我们想说的是"`&mut T` 在下一次 `unlock()` 调用时失效"。

**Rust 表达不了这种生命周期。** Rust 的生命周期只能依附于**作用域和引用**，不能依附于"某次方法调用"。原书有一段调侃：

> 如果编译器能听懂英语，我们大概想这样写：
> ```
> pub fn lock<'a>(&self) -> &'a mut T
> where
>     'a ends at the next call to unlock() on self,
>     even if that's done by another thread.
>     Oh, and it also ends when self is dropped, of course.
>     (Thanks!)
> ```

听不懂。我们只能**把责任甩给用户**——把 `unlock` 标成 `unsafe`：

```rust
/// Safety: 调用此函数前，来自 lock() 的 &mut T 必须已经不再被使用。
///         （包括不能保留任何从它派生出来的引用。）
pub unsafe fn unlock(&self) {
    self.locked.store(false, Ordering::Release);
}
```

能用，但用户每次都要写 `unsafe`，且忘了"先丢弃 `&mut T` 再 `unlock`"就是 UB：

```rust
let lock = SpinLock::new(0);
let r = lock.lock();          // r: &mut i32
unsafe { lock.unlock(); }     // 锁开了，但 r 还在
*r = 42;                      // UB！别的线程可能同时也在写
```

编译器拦不住——`r` 的生命周期绑死在 `&lock` 上，只要 `lock` 活着 `r` 就合法。**这版接口把锅留给了用户**，每用一次都像踩雷。我们得想办法把 `unsafe` 关回笼子里。

### miri 会怎么抓这种 bug

如果你以为上面那段代码"看起来能跑就行"，那就低估了工具。miri 是 Rust 的未定义行为检测器（M5 详讲它的原理），它能识别"对同一块内存同时存在活的 `&mut` 和别的访问"这种别名违规。把第二版的代码喂给 miri，一旦你在 `unlock` 之后还碰 `r`，miri 会立刻报错：

```
error: Undefined Behavior: trying to access <alloc> with associativity0,
       but that memory does not exist in the borrow stack
```

miri 的"借用栈"模型会在 `unlock`（Release store）那一刻清空对 `value` 的借用——因为逻辑上你承诺了"不再用"。之后再碰 `r` 就越界。这就是为什么我们的测试都跑 miri：它能抓住类型系统抓不到的 `unsafe` 错误。

---

## 第三版：Guard 把 unsafe 关进笼子

问题的核心：要把"解锁"绑到"`&mut T` 的末尾"。Rust 里有一个机制恰好能把"某段代码"绑到"一个值的末尾"——`Drop` trait。值离开作用域时，`Drop::drop` 自动调用。

我们造一个**守卫类型** `Guard`：它包着一个对锁的引用，**行为像 `&mut T`**（靠 `Deref`/`DerefMut`），并在 `drop` 时**自动解锁**（靠 `Drop`）。守卫的**存在本身**就是"我已独占该锁"的证明——拿到守卫的唯一途径是 `lock()`，守卫没消失就说明锁还没释放。

```rust
use std::ops::{Deref, DerefMut};

pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,   // 生命周期保证守卫不比锁长寿
}

impl<T> SpinLock<T> {
    pub fn lock(&self) -> Guard<'_, T> {
        while self.locked.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        Guard { lock: self }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // 安全：守卫的存在本身就证明我们独占了锁，没有别的线程在碰 T。
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // 安全：同上，独占访问。
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // 守卫消失 → 自动解锁：把本临界区里的所有修改"发布"给下一个 lock 的 Acquire。
        self.lock.locked.store(false, Ordering::Release);
    }
}
```

逐条解释这套设计的妙处。

**`Guard<'a, T>` 没有公开构造函数、字段私有**——拿到它的**唯一**途径是 `lock()`。所以"存在守卫 = 已上锁"这个不变量成立，`unsafe` 块的安全性论证就靠它。**`unsafe` 被关进了实现里，用户面对的是完全安全的接口。**

**`Deref` / `DerefMut`** 是 Rust 的"自动解引用"机制。给 `Guard` 实现这两个 trait 后，`g.push(2)` 会自动变成 `(*g).push(2)`——`Guard` 行为就像 `&mut T` 一样，可以直接调 `T` 的方法、读写字段。这就是为什么 `x.lock().push(1)` 写起来和 `&mut Vec` 一模一样。

**`Drop`** 是关键：守卫离开作用域时自动调用 `drop`，里面写 `store(false, Release)`。用户**无法忘记解锁**——守卫一消失就解了，没有"忘记调 unlock"这种 bug。

跑一下原书那个例子：

```rust
let x = SpinLock::new(Vec::new());
thread::scope(|s| {
    s.spawn(|| x.lock().push(1));                  // 守卫是临时量，语句结束即 drop → 解锁
    s.spawn(|| { let mut g = x.lock(); g.push(2); g.push(2); });
});
let g = x.lock();
assert!(g.as_slice() == [1, 2, 2] || g.as_slice() == [2, 2, 1]);
```

如果手贱提早 `drop(g)`，再 `g.push(2)` 会直接编译失败：

```
error[E0382]: borrow of moved value: `g`
   |     drop(g);
   |          - value moved here
   |     g.push(2);
   |     ^^^^^^^^^ value borrowed here after move
```

**类型系统替我们抓住了"用已解锁的引用"这类错误。** 这就是 `forge-core::spin`（`tests/m3_01..04`），miri 确认无 UB。

### Drop 的微妙之处：panic 时怎么办

`Guard` 的 `Drop` 实现里只有一行 `store(false, Release)`。读者可能担心：如果临界区里的代码 panic 了，守卫还会被 drop 吗？

**会。** Rust 的 `Drop` 是"无条件展开"（unwinding）的一部分——即便当前线程 panic，所有活着的局部变量（包括守卫）都会按逆序 drop。所以 panic 期间锁也会被正确释放，不会死锁。这是 `Drop` 比"手写 unlock"巨大的安全优势之一——手写 unlock 在 panic 路径上很容易漏。

但要注意：panic 之后锁虽然释放了，可被保护的数据可能处于"半修改"状态（比如 Vec push 到一半）。下一个拿到锁的线程看到的是不一致的数据。这是所有互斥锁的共同特性，不是自旋锁的问题——**panic 安全要靠 `MutexGuard::poison` 机制**（M2 提过），我们的 `SpinLock` 没实现 poison，留给读者作为练习。

具体来说，`std::sync::Mutex` 在守卫 drop 时检查当前线程是否正在 panic——如果是，就把锁标记为"中毒"（poisoned），后续 `lock()` 返回 `Err(PoisonError)`。用户必须显式决定"继续用这个不一致的数据"还是"放弃"，不能稀里糊涂读到一个半改的状态。这是个很贴心的设计，但代价是每次 drop 多一次线程局部状态查询。我们的 SpinLock 为了简洁没做这个，但**生产环境的锁应该做**。

### 为什么 Guard 不持 `&mut T` 字段？

一个微妙的点：`Guard` 内部**只存 `&'a SpinLock<T>`**，不存 `&mut T`。为什么？因为如果存 `&mut T`，那它的生命周期就来自 `unsafe { &mut *self.value.get() }`——而我们刚说过，这种裸指针转出来的引用生命周期会绑死在 `&self` 上，退化回第二版的困境。让 `Guard` 不持 `&mut T`、每次 `Deref`/`DerefMut` 都**重新**从 `UnsafeCell::get()` 拿，把"对 T 的访问"和"Guard 的生命周期"解耦——这是关键的设计技巧。

### 同构：所有自建锁都长这样

刻进脑子——"内部可变（`UnsafeCell`）+ `unsafe impl Sync where T: Send` + 守卫 `Deref/DerefMut/Drop`"是 Rust 里**所有自建锁**的统一骨架。M7 的 futex `Mutex`/`RwLock`、M8 的 MCS 锁、parking-lot 锁，全是这个模子的变体。把 M3 这副骨架刻进脑子，后面省一半力气。

`std::sync::Mutex` 也是这个骨架，只是它的 `Guard` 类型叫 `MutexGuard`，且 `lock` 内部多了"先自旋、再 park"的自适应逻辑。

---

## ISO·ZOOM 1：手算 Acquire/Release 的必要性

我们一直说"Acquire/Release 是锁的标准内存序"，但**不写它会怎样**？这一节我们停下来，**逐拍**走一遍两个核的执行，让你**亲眼看到** bug 怎么诞生。

### 场景设置

两个核，T1 和 T2。共享两块内存：

- `data: i32`——被锁保护的普通变量（**非原子**）。初值 `0`。
- `locked: AtomicBool`——锁。初值 `false`。

T1 持锁，写共享数据，然后解锁。T2 抢锁，然后读 `data`。逻辑上 T2 应该看到 T1 写的值——这是锁的全部意义。

```
T1:                              T2:
data = 42;                       (等锁)
locked.store(false, Release);    if !locked.swap(true, Acquire) {
                                     print(data);   // 应该看到 42
                                 }
```

### 实验 A：把两个 Ordering 都换成 Relaxed

把解锁的 `Release` 改成 `Relaxed`，把抢锁的 `Acquire` 也改成 `Relaxed`。**逻辑上的"锁"还在**（`swap` 仍然是原子，互斥仍然成立），只是不再有 happens-before。我们来看 T2 会读到什么。

要画这张图，我们得先认识两件事：**store buffer** 和 **MESI 缓存行状态**。

#### store buffer：每个核私有的"写暂存区"

写内存其实很慢——L1 缓存命中也要 1–3 个周期，没命中要几十上百周期。CPU 不愿意为了一条 store 干等。所以每个核都有一个**私有的写缓冲队列**叫 store buffer：一条 store 指令不是直接打到缓存上，而是先进 buffer、CPU 立刻去干下一条指令，buffer 在后台慢慢把写排进缓存。

代价：**"我写了"和"别的核能看到这次写"之间有延迟**。对单线程无影响（buffer 排空前，本核读这个地址会先看 buffer，叫 store-to-load forwarding）；但对多线程致命——别的核不知道你 buffer 里有啥。

#### MESI：缓存行的四种状态

缓存以**缓存行**（cache line，通常 64 字节）为单位。每行有四种状态：

- **M**odified（修改）：本核独占此行，且内容已被改、和内存不一致。
- **E**xclusive（独占）：本核独占此行，但内容和内存一致（没改过）。
- **S**hared（共享）：多个核都有此行的副本，内容一致、都是只读。
- **I**nvalid（失效）：本核此行作废，要读得重新去别的核或内存拉。

一个核要写某行，必须先把它**独占**（升到 M 或 E）；这会触发 MESI 协议给其它核发 Invalidate 消息，让它们同地址的行从 S 或 M 落到 I。这是**缓存一致性**的来源。

#### 逐拍（Relaxed 版）

| 拍 | T1 核 | T2 核 | locked 行状态 | data 行状态 | 说明 |
|---|---|---|---|---|---|
| 0 | — | — | S（false）| S（0）| 初态：两核都持两行的 S 副本 |
| 1 | `data = 42`：先进 T1 的 store buffer | — | S（false）| S（0）| **data 还没真到缓存！** 只在 T1 的 buffer 里 |
| 2 | `locked.store(false, Relaxed)`：也先进 T1 的 store buffer | — | S（false）| S（0）| **locked 也没真到缓存！** 注意 `locked` 本来就是 false，这条 store 是 no-op |
| 3 | T1 的 buffer 还在排空（要等 MESI 的 Invalidate Ack）| T2 开始 `locked.swap(true, Relaxed)`：原子 RMW，要求独占 locked 行 | T1: I / T2: M（true）| S（0）| T2 拿到锁！因为它读到的 `locked=false`（T1 那条 store 还赖在 buffer 里没下来，但内容也是 false，所以无所谓——重要的是 T2 看不到 T1 那条 `data=42`） |
| 4 | — | T2 读 `data`：**从自己缓存读到 0！** | M（true）| S（0）| **bug：T2 拿到了锁，却读到旧值 0。** |
| 5 | T1 的 buffer 终于排空：`data=42`、`locked=false` 才真正进缓存 | — | … | M（42）| 太晚了 |

第 4 拍那一刻，**互斥成立**（T2 确实拿到了锁），**但可见性不成立**（T1 写的 42 还没排空到缓存，T2 看不到）。这正是 happens-before 缺失的直接后果。锁"看着对"，数据却错了。

读者要问：那为什么 T2 的 swap 能"绕过"T1 的 buffer？因为 store buffer 是**每个核私有**的，T2 看不到 T1 的 buffer；T2 的 swap 操作的是缓存（已经反映的值是 false）。Relaxed **没有任何屏障指令**，T1 的 buffer 没被强制排空，T2 的读也没被强制重新拉缓存。两个核各自看各自的视图，没人强制它们对齐。

#### 把它改成 Release/Acquire，再走一遍

同样的设置，把 Ordering 改回来。`Release` 在 T1 这一侧会把 store buffer **排空**再 store；`Acquire` 在 T2 这一侧会让后续的读**不去用旧缓存**（具体到 x86 是免费的，具体到 ARM 是 `ldar` 指令）。

| 拍 | T1 核 | T2 核 | locked | data | 说明 |
|---|---|---|---|---|---|
| 1 | `data = 42`：进 store buffer | — | S（false）| S（0）| 同上 |
| 2 | `locked.store(false, Release)`：**先排空 store buffer！** `data=42` 真正进缓存（要先独占 data 行，再写） | — | E（false）| M（42）| Release 把"前面的写"全部推到缓存里，再执行本条 store |
| 3 | — | T2 `swap(true, Acquire)`：先 invalidate locked 行、独占读 | T2: M（true）| S（42）| T2 在拿锁的瞬间，连带地把 data 也"刷新"了（因为 Acquire 不允许后续读被重排到 swap 之前） |
| 4 | — | T2 读 `data`：**42！** | M（true）| S（42）| 正确。 |

第 2 拍是关键。`Release` 不是"加一条屏障指令"那么简单——它的**语义**是"把我之前的所有写都对其它核可见，再执行这条 store"。在 x86 上这条语义本来就满足（详见下一节），所以编译出来啥都不加；在 ARM 上必须 `stlr`。但**抽象模型层面**，Acquire/Release 给的就是上面这个保证。

> **一句话总结这个手算**：**互斥靠原子，可见性靠内存序**。两者是不同的东西，缺一不可。`swap(true, Relaxed)` 给互斥不给可见性，所以不够。

### 反过来：Release 也能"漏"

读者可能想问：那 Acquire/Release 就一定够吗？答案是**对"一对一传递"够，对"多变量不变量"可能不够**。考虑两个变量 `x` 和 `y`，T1 在临界区里写 `x=1; y=1`，T2 读到 `x=1` 后想断言 `y==1`。Release/Acquire 保证 T2 看到临界区里所有写，但**如果 T2 是从另一个线程（没拿过锁）读 y**，就没有任何保证。这种跨线程的多变量不变量需要 `SeqCst`（M1.9 详讲）。本模块的 SpinLock 是"一对一传递"（一个临界区到下一个临界区），所以 Acquire/Release 够用。

### loom 怎么抓这个 bug

loom 是一个**模拟器**，它枚举所有"由 Rust 抽象内存模型允许的"线程交错。即便你只在 x86 上跑测试，loom 也会在内部尝试"T1 的两条 store 还没排空就被 T2 抢到"这种交错（这是 ARM 上真实发生的事情，loom 把它当作可能性枚举出来），断言失败。这正是 loom 的价值：**让你在 x86 上提前发现 ARM 上才会暴露的 bug**。

把上面的 Relaxed 版用 loom 跑，几百次迭代内必然抓到 `data != 42` 的反例。loom 模型的写法超出本模块范围（M11 详讲），但你此刻要记住：**loom 模拟的是抽象模型，不是任何具体 CPU**。所以你在 x86 上跑 loom，也能抓到只在 ARM 上才会触发的内存序 bug。

---

## ISO·ZOOM 2：x86 免费午餐 vs ARM 专门指令

上一节我们说 `Release` 会"排空 store buffer"。那它到底编译成什么指令？为什么 M1 我们说"在 x86 上 Acquire/Release 几乎免费"？这一节把 CPU 的盖子彻底掀开。

### 处理器重排：单线程看不见的"不算事"

先建立一个心智模型：处理器和编译器**唯一**的承诺是"不改变**单线程**结果"。多核下每个核都有自己的 store buffer、失效队列、流水线，**对外可见的顺序完全可以和代码顺序不同**——只要单线程看不出来。这就是为什么我们必须**显式声明内存序**：用屏障指令**禁止**某些重排。

每个 CPU 架构对"允许什么重排"规定不同。我们只关心两个：

- **x86-64（强内存模型，TSO = Total Store Order）**：几乎不重排。唯一允许的是"store 被推迟到后面的 load 之后"（因为 store buffer）。
- **ARM64（弱内存模型）**：几乎所有 load/store 都可能被重排。

读者要小心一个常见误解：**编译器也能重排**。`Relaxed`、`Acquire`、`Release` 同时约束处理器**和编译器**。即使你的 x86 CPU 不重排，编译器也可能在 `Relaxed` 操作之间挪代码（虽然实际编译器对原子操作很保守，但你不能依赖这点）。所以"`Relaxed` 在 x86 上没事"这句话只在"加上 `compiler_fence` 挡住编译器"时才成立。原书第 7 章末那个 ARM M1 实验就是用 `compiler_fence` 单独挡编译器、只测处理器重排。

### x86：Acquire/Release 免费，SeqCst store 要 xchg

在 x86 上，因为 TSO 已经禁止了"acquire-load 之后的 load 被提前"和"release-store 之前的 store 被推后"，所以**普通 `mov` 就满足 Acquire/Release 语义**，编译出来啥都不加。这就是"免费"的真正含义——你写 `Acquire` 和写 `Relaxed` 编出的机器码**一模一样**。

```
Release store (x86-64):          Relaxed store (x86-64):
    mov dword ptr [rdi], 0           mov dword ptr [rdi], 0
    ret                              ret
```

唯一例外是 `SeqCst` 的 **store**。SeqCst 要求"全局单一顺序"——它不能被重排到任何后面的内存操作之后（包括后面的 load）。普通 `mov` 满足不了，所以 x86 把 SeqCst store 编成 `xchg`（自带隐式 lock 前缀，相当于一次原子 swap，把 store buffer 顺手排空）：

```
SeqCst store (x86-64):
    xor eax, eax            // 把 eax 清零（xor 自身是经典写法，比 mov eax,0 短一字节）
    xchg dword ptr [rdi], eax  // 交换：把 0 写进内存，把内存原值放进 eax（我们不在乎 eax）
    ret
```

这就是 SeqCst store 比 Release store 贵的来源——一次 `xchg` 比一次 `mov` 慢，因为它要走缓存一致性协议独占缓存行、还要排空 store buffer。其它操作（load、fetch_add）的代码 SeqCst 和 Release 完全一样：

```
SeqCst load (x86-64):            SeqCst fetch_add (x86-64):
    mov eax, dword ptr [rdi]        lock add dword ptr [rdi], 10
    ret                             ret
```

注意 SeqCst load 仍然是普通 `mov`——因为 SeqCst 的"全局顺序"靠 store 升级到 xchg 来保证，load 不需要额外代价。

所以"用 Acquire/Release 替代 SeqCst"在 x86 上**省的就是 SeqCst store 那条 xchg**。其它操作（load、fetch_add）的代码完全一样。如果你的热路径里没有 SeqCst store，换 Acquire/Release 几乎不省钱；如果有，能省一些。

### ARM：ldar / stlr / dmb ish

ARM 是弱内存模型，必须显式指令。`Acquire` 的 load 编成 `ldar`（load-acquire），`Release` 的 store 编成 `stlr`（store-release）。这两条指令各自带屏障语义，比 `ldr`/`str` 稍贵。`SeqCst` 因为要求全局顺序，需要 `dmb ish`（数据内存屏障）：

```
Release store (ARM64):           Acquire load (ARM64):
    stlr wzr, [x0]                   ldar w0, [x0]
    ret                             ret

SeqCst fence (ARM64):             SeqCst store (ARM64):
    dmb ish                          stlr wzr, [x0]   // 和 Release 一样
                                    ret
```

这就是"Relaxed 在 x86 上看起来没事、到 ARM 上就炸"的根源。x86 的强模型替你挡住了很多重排，让你以为 `Relaxed` 够用；换到 ARM，那些重排真实发生，bug 才暴露。

原书有个实验：把自旋锁的所有内存序改成 Relaxed、加 `compiler_fence` 仅挡编译器。在 x86 上跑 4 线程各加 100 万次，结果永远是 4000000；到 Apple M1（ARM）上结果是 3988255、3982153——错 0.4%。

**所以：永远按抽象内存模型（Acquire/Release/SeqCst）推理，别依赖"我这台机器上能跑"。** 这正是 loom/miri 的价值——它们帮你在 x86 上也能提前发现这类 bug。

### CAS 与 LL/SC：为什么有 `compare_exchange_weak`

`compare_exchange` 在 x86 上是 `lock cmpxchg`（一条指令、自带 lock 前缀）；在 ARM 上是一对 **load-linked / store-conditional** 指令：`ldxr`（load exclusive）读出来、改、`stxr`（store exclusive）写回去——但 `stxr` 可能**假失败**：如果这期间这个缓存行（其实是粒度更大的"监控区"）被任何核写过，就失败。这是 ARM 实现"原子 RMW"的方式：靠一对指令 + 重试。

假失败的根源是监控粒度粗糙——硬件为了省事，不精确到字节，而是一个缓存行甚至更大区域。所以任何"刚好"落到同行的无关写都可能让 LL/SC 假失败。这就是 `compare_exchange_weak` 存在的理由：它允许假失败，所以**适合放在循环里**用；`compare_exchange`（strong）会自己重试，**适合只想试一次**的场景。

```
compare_exchange_weak(5, 6, Relaxed, Relaxed) on ARM64:
    ldxr w8, [x0]       // 独占读
    cmp w8, #5          // 是 5 吗？
    b.ne .L1            // 不是 → 跳到 clrex
    mov w8, #6
    stxr w9, w8, [x0]   // 独占写：可能失败（w9=1）即使值确实是 5
    ret
.L1:
    clrex               // 放弃独占
    ret
```

**重要副产物**：失败的 CAS 在大多数架构上**和一次 store 一样贵**——它要独占缓存行（触发其它核失效）。所以自旋锁**别**在循环里猛 CAS——先用便宜的 `load` 看"锁开了没"，看着开了再 CAS，能省一堆无效的缓存失效。我们的实现用 `swap`（一次 RMW），是合理选择；更精细的实现会先 `load` 自旋、再 `swap`。

### 为什么"失败的 CAS 也独占缓存行"是个反直觉的坑

这点要展开讲，因为它是个常被忽略的性能陷阱。直觉上你会想："CAS 失败 = 没改数据 = 没代价"。**错**。在 x86 上 `lock cmpxchg` 不管成不成功，都会通过 lock 前缀独占这条缓存行（让其它核的副本失效）。原因是硬件必须保证原子性——它得先独占、再比较、再决定写不写，整个过程缓存行都在它手里。

原书第 7 章有个实验：背景线程死循环跑一个**注定失败**的 `compare_exchange(10, 20)`（A 永远不是 10），主线程跑 10 亿次 `load`。结果主线程从 300 ms 暴涨到 3 秒——10 倍。哪怕 CAS 没改数据，它的"独占"动作也把主线程的缓存行副本赶走了，主线程每次 load 都 miss。

这个发现对自旋锁设计有直接影响：**在循环里猛 `swap` 或猛 CAS 是糟糕的**——每次失败都让持锁者的缓存行失效，持锁者下次碰自己的锁字段又 miss，双方互相捣乱。正确做法：

```rust
// 糟糕：每次 swap 都独占缓存行
while self.locked.swap(true, Acquire) {
    spin_loop();
}

// 更好：先 load（便宜，不独占）看到开了再 swap
while {
    while self.locked.load(Relaxed) {
        spin_loop();
    }
    self.locked.compare_exchange(false, true, Acquire, Relaxed).is_err()
} {
    spin_loop();
}
```

第二种写法看似啰嗦，但 load 不要求独占（只读），不会捣乱持锁者的缓存。等看到锁开了再去 swap，绝大多数情况一次就成。Linux 内核的自旋锁、Java 的 `AbstractQueuedSynchronizer` 都用了这个"先 load 后 CAS"的模式。我们的 `forge-core` 实现用 `swap` 是为了讲解清晰，**不是最优**——读者进阶时可以改成 load+CAS 版。

### ARMv8.1：CISC 风格的原子指令

ARM 也没闲着。ARMv8.1 引入了一批 CISC 风格的原子指令：`ldadd`（load and add，等价于 `fetch_add`）、`cas`（compare and swap）等。它们不用 LL/SC 循环，单条指令完成。还各自带 acquire/release 变体：`ldadda`（acquire）、`ldaddl`（release）、`ldaddal`（acquire+release，等价于 SeqCst）。

ARMv8.1 之前的 ARM 上，`compare_exchange` 和 `compare_exchange_weak` 编出的代码不同（weak 不带重试分支）；ARMv8.1 之后用 `cas` 指令，两者一样。这就是为什么文档里总是说"weak 在循环里用"——它是为 LL/SC 架构准备的，在 CAS 架构上 weak 和 strong 没区别。

---

## ISO·ZOOM 3：手算 false sharing 的缓存行 ping-pong

最后一个手算，把"伪共享"算给你看。这一节算完，你就理解为什么 M1.10 反复强调 `#[repr(align(64))]`。

### 场景设置

两个线程、两个计数器。计数器结构有两种布局：

```rust
// 布局 A：相邻字段（伪共享重灾区）
struct Counters {
    a: AtomicU64,
    b: AtomicU64,
}

// 布局 B：每个字段独占一条缓存行（消除伪共享）
#[repr(align(64))]
struct CacheLine<T>(pub T);

struct CountersPadded {
    a: CacheLine<AtomicU64>,
    b: CacheLine<AtomicU64>,
}
```

T1 死磕 `a`，T2 死磕 `b`，各做 `fetch_add(1)` 一千万次。两个计数器**逻辑上互不相干**——你不读我的、我不读你的。理论上两个核应该完全并行，总时间和单线程差不多。

可现实不是。来看为什么。

### 手算 MESI ping-pong（布局 A）

`AtomicU64` 是 8 字节。`Counters` 的 `a` 和 `b` 紧挨着，**总尺寸 16 字节，远小于一条缓存行（64 字节）**——所以两者必然**落在同一条缓存行**上。这就是"伪共享"（false sharing）的温床：两个不相干的变量被缓存机制强行绑定。

设 T1 跑在核 1、T2 跑在核 2。逐拍看一行缓存行的状态机：

| 拍 | 核 1（写 a）| 核 2（写 b）| 缓存行（含 a+b）状态 |
|---|---|---|---|
| 0 | 想写 a：要独占 → 发 Invalidate 给核 2 | — | 核 1: E / 核 2: I |
| 1 | `fetch_add`：把行读到核 1 的 L1，写 a → M | — | 核 1: M（a=1,b=0）/ 核 2: I |
| 2 | — | 想写 b：要独占 → 发 Invalidate 给核 1 | 核 1 收到请求，必须把 M 的行写回（或转发）、降级 |
| 3 | — | `fetch_add`：行读到核 2 的 L1，写 b → M | 核 1: I / 核 2: M（a=1,b=1）|
| 4 | 想写 a：又要独占 → Invalidate 核 2 … | — | （回到拍 0 的对称）|

**每一拍都在 ping-pong**：行在两个核之间来回搬运。每次"独占请求 → 对方写回 → 我读 → 我写"是一个**缓存往返**，大约 **5–15 纳秒**（具体看架构和缓存层级）。这叫**缓存行乒乓**（cache line ping-pong）。

注意第 1 拍的微妙之处：核 1 写 a 时，它**连带地把整条缓存行**（含 b）独占了——哪怕它根本不碰 b。MESI 的粒度是缓存行，不是字节。这是伪共享的硬件根源：**a 和 b 在同一条行上，所以"碰 a"和"碰 b"在硬件眼里是同一件事**。

### 为什么缓存行的粒度是 64 字节，不是 1 字节

读者可能要问：硬件为什么这么"笨"，不能精细到字节吗？答：**空间和复杂度的权衡**。MESI 协议要为每一块"可单独失效的内存单位"维护状态——状态记录在缓存里、要随 Invalidate 消息在线缆上传输。粒度越细，状态记录越多、消息越多。64 字节是工业界长期实践下来的甜点：大到能让一次 cache miss 顺便把邻居也拉进来（空间局部性），小到不至于让一次写捣乱太多无关数据。某些服务器 CPU 用 128 字节，所以严格的 benchmark 会用 `#[repr(align(128))]` 试一遍。

这也解释了为什么 `AtomicBool` 虽然只有 1 字节，却照样能引发伪共享——它和邻居挤在同一条 64 字节行里。同理，`AtomicU64`（8 字节）如果两个紧挨着，差距 8 字节，必然同行。要拉开它们，必须让两者起始地址至少差 64 字节——这就是 `#[repr(align(64))]` 的作用。

### `#[repr(align(64))]` 到底干了什么

`#[repr(align(N))]` 是告诉编译器：这个类型的每个值都必须放在 N 字节对齐的地址上。对一个 8 字节的 `AtomicU64` 包上 `align(64)`，结果是：结构体大小变成 64 字节（开头 8 字节是数据，后面 56 字节是 padding），并且起始地址必须是 64 的倍数。两个这样的结构体相邻，第二个必然在新的一条缓存行上。

`CacheLine<T>` 就是这个包装的封装：

```rust
#[repr(align(64))]
pub struct CacheLine<T>(pub T);
```

它零运行时开销（padding 占内存但不耗 CPU），只是让结构体更"胖"。M1.10 已经介绍过它，这里只是复习——后面 M8、M9 都会用到。

### 算一下总代价

T1 做 10⁷ 次 `fetch_add`，每次都触发一次乒乓（~10 ns）：

```
10 ns × 10,000,000 = 100,000,000 ns = 0.1 秒
```

T2 同理，又 0.1 秒。两个核**串行**地抢这条缓存行，**总耗时 ≈ 0.2 秒**（保守估计，实际还更糟，因为流水线被 stall、L2/L3 也要参与）。

对照布局 B（padded）：a 和 b 各自独占一条缓存行，互不干扰。两个核完全并行，每次 `fetch_add` 只动自己那条行（始终 M），**没有乒乓**。每次只要 ~1 ns（L1 命中）：

```
1 ns × 10,000,000 = 10 ms = 0.01 秒
```

**比值：0.2 / 0.01 = 20×**。当然这是理论上限——实际还有 `fetch_add` 自己的开销、流水线、超线程干扰——原书实测的典型倍数是 **3–5×**（具体看硬件，可能更高）。无论精确值多少，**这条规律不变：两个不相干的热点变量挤在一行 = 性能自残**。

### criterion 怎么测

```rust
use criterion::{criterion_group, criterion_main, Criterion};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[repr(align(64))]
struct CacheLine<T>(pub T);

fn bad(c: &mut Criterion) {
    c.bench_function("false_sharing", |b| {
        let cnt = std::sync::Arc::new((
            AtomicU64::new(0),
            AtomicU64::new(0),
        ));
        b.iter(|| {
            let cnt = cnt.clone();
            std::thread::scope(|s| {
                let cnt = &*cnt;
                s.spawn(move || { for _ in 0..10_000_000 { cnt.0.fetch_add(1, Relaxed); } });
                s.spawn(move || { for _ in 0..10_000_000 { cnt.1.fetch_add(1, Relaxed); } });
            });
        });
    });
}

fn good(c: &mut Criterion) {
    c.bench_function("padded", |b| {
        let cnt = std::sync::Arc::new((
            CacheLine(AtomicU64::new(0)),
            CacheLine(AtomicU64::new(0)),
        ));
        b.iter(|| {
            let cnt = cnt.clone();
            std::thread::scope(|s| {
                let cnt = &*cnt;
                s.spawn(move || { for _ in 0..10_000_000 { cnt.0.0.fetch_add(1, Relaxed); } });
                s.spawn(move || { for _ in 0..10_000_000 { cnt.1.0.fetch_add(1, Relaxed); } });
            });
        });
    });
}

criterion_group!(benches, bad, good);
criterion_main!(benches);
```

预期待：`padded` 明显快于 `false_sharing`，典型 3–5×，某些机器上更多。这条经验法则后面所有模块都要用到——M8 的无锁队列节点、M9 的 per-worker 统计、任何"高并发 + 紧凑结构"的场景都得警惕伪共享。

### 一个反直觉的副产物：紧凑有时也对

伪共享反过来不成立——**不是"永远要对齐到 64"**。如果两个变量**总是一起读**（比如一对 `head`/`tail` 索引），把它们放同一条缓存行反而是好事——一次 cache miss 把两个都拉过来。我们的 `SpinLock<T>` 把 `AtomicBool` 和 `T` 放在一起：拿锁的那一下同时把 `T` 拉进缓存，省一次 miss。**对齐是工具，不是教条。** 关键是分清"逻辑相关、一起访问"（紧凑）vs"逻辑无关、各自访问"（散开）。

---

## 把 SpinLock 当 per-worker 热计数器：和 std::Mutex 实测对比

讲到这里，我们来一个落地实验：把 SpinLock 和 `std::sync::Mutex` 在"per-worker 各自加自己的计数"这个场景下对比。这是个**自旋锁理想场景**——锁极短、跑在不同核上。

```rust
use forge_core::spin::SpinLock;
use std::sync::Mutex;
use std::time::Instant;

fn bench_spin(n_workers: usize, iters: u64) -> std::time::Duration {
    let counter = SpinLock::new(0u64);
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..n_workers {
            s.spawn(|| {
                for _ in 0..iters {
                    let mut g = counter.lock();
                    *g += 1;
                }
            });
        }
    });
    start.elapsed()
}

fn bench_std(n_workers: usize, iters: u64) -> std::time::Duration {
    let counter = Mutex::new(0u64);
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..n_workers {
            s.spawn(|| {
                for _ in 0..iters {
                    let mut g = counter.lock().unwrap();
                    *g += 1;
                }
            });
        }
    });
    start.elapsed()
}
```

预期待：在锁极短、worker 数 ≤ 物理核数的场景，SpinLock 通常更快——它不接入运行时（不 park、不系统调用）。但要注意几个陷阱：

- **worker 数超过物理核数时** SpinLock 优势消失甚至变差——多个线程在同一核上互相自旋，纯烧 CPU。
- **持锁时间变长时** SpinLock 优势消失——自旋的"等待成本"线性增长，而 park 的成本是固定的。
- `std::sync::Mutex` 的实现质量很高（很多平台先自旋再 park），所以"自建自旋锁完胜 std Mutex"这种结论不能下太满。**永远 benchmark 你自己的场景**。

这一节的意义不在数字本身，而在让你**养成"看到锁就问三个问题"的习惯**：锁持多久？worker 数？跑在多少核上？答案决定自旋 vs park 的取舍。

### 一个常被忽略的陷阱：自旋锁不公平

我们的 `SpinLock` 是**不公平**的——谁抢到 `swap` 全凭运气，某个线程可能一直抢不到（这叫**饥饿**）。`std::sync::Mutex` 在大多数平台上是公平的（FIFO 等待队列），不会让某个线程永远等。

不公平的好处是吞吐高（不维护队列、不上下文切换），坏处是延迟尾部长（最坏情况可能极差）。M8 我们会讲 MCS 锁——一个**公平**的自旋锁，靠链表排队。本模块的 SpinLock 是**最朴素**的自旋锁，只适合"争用低、持锁短"的场景。

### 另一个陷阱：单核上的自旋是灾难

如果 worker 数超过物理核数（或者 OS 把多个 worker 调度到同一核），自旋锁会变成灾难。考虑：核 1 上跑着 T1（持锁）和 T2（想抢锁，被 OS 时间片切到）。T2 在自旋——可它自旋 100% 占着核 1，T1 没机会被调度回去释放锁。T2 死循环烧 CPU，T1 在 runqueue 里干等。这叫**优先级反转**或**自旋死锁**（其实不是死锁，但表现类似）。

`std::sync::Mutex` 不怕这个——它 park 后让出核，T1 能跑、能释放锁。所以**单核场景或过订阅（oversubscription）场景下，绝不要用自旋锁**。这就是为什么"自旋 vs park"的选择要先看你的部署：多核、worker 数等于核数、争用短——自旋；其它情况——park。

### 内存序选择的一个具体决策树

把决策过程写出来，避免你下次犹豫：

1. **保护临界区的锁**（我们的 SpinLock）：lock 用 `Acquire`，unlock 用 `Release`。这是黄金法则，**别用 SeqCst**——白白多花一条 xchg，没收益。
2. **简单的"通知"语义**（M1 的 stop flag）：写者 `Release`，读者 `Acquire`。
3. **需要全局顺序的多次操作**（比如多变量的不变量）：考虑 `SeqCst`。
4. **纯计数器、不在意顺序**（比如统计点击数）：`Relaxed` 够用。
5. **不确定时**：用 `SeqCst`，最坏只是慢一点，至少不会错。

我们的 SpinLock 选 1。M1 的 counter 选 4。M4 的 Arc 计数会选 1 + 一道栅栏。每个选择都由"敌人是什么"决定——不是套公式。

---

## 把这一模块刻进脑子

### 一句话总结

**自旋锁 = 反复试着锁（不睡）+ Acquire/Release（保证可见性）+ Guard 把 unsafe 关进笼子**。Acquire/Release 不是装饰，是"T2 看到 T1 的写"的唯一保证；在 x86 上这条保证免费，在 ARM 上要专门指令，但**抽象模型层面永远是同一条规则**。

### 三件事刻进脑子

1. **互斥靠原子，可见性靠内存序**——两件事，缺一不可。`swap(Relaxed)` 给互斥不给可见性。
2. **`UnsafeCell` + `unsafe impl Sync where T: Send` + `Guard(Deref/DerefMut/Drop)`** 是所有自建锁的统一骨架。
3. **永远按抽象模型推理，别依赖"我这台机器上能跑"**——x86 替你挡的 bug，到 ARM 上就炸。

### L1–L5 缩放回顾

| 层 | 你能…… |
|---|---|
| L1 | 一句话：自旋锁 = 忙等的 Mutex；Acquire/Release 是它的标准内存序。 |
| L2 | 类比：自旋="反复按门铃"、Guard="拿着钥匙的人，走了才锁门"。 |
| L3 | 跟踪三版演化：最小（保不了数据）→ UnsafeCell+unsafe unlock（要用户写 unsafe）→ Guard+Drop（完全安全）。 |
| L4 | 解释 `unsafe impl Sync where T: Send` 的论证、Acquire/Release 如何建立 happens-before、为什么 store buffer 会让 Relaxed 解锁出 bug。 |
| L5 | 知道自旋何时优于睡眠；x86 TSO 下 Acquire/Release 免费、SeqCst store 要 xchg；ARM 上要 ldar/stlr/dmb；CAS 在 ARM 是 LL/SC、可能假失败；false sharing 让无关变量慢 3–5×；何时该 align(64)。 |

### 自检

- [x] 先讲敌人（睡-醒太贵、要保护数据、要把 unsafe 藏起来）再演化。
- [x] 忠于原书第 4 章的三版结构与第 7 章的 CPU 真相。
- [x] 手算 1：Acquire/Release 必要性，逐拍画 store buffer + MESI，Relaxed 版 T2 读到 0。
- [x] 手算 2：false sharing 缓存行 ping-pong，10⁷ × 10ns ≈ 0.1s，padded 后快 3–5×。
- [x] 识别同构：Guard + UnsafeCell + unsafe impl Sync 是所有自建锁的统一骨架。
- [x] 变速：三版厚写、CPU 真相点出关键（x86 免费、ARM 要屏障、CAS 代价、伪共享）。

### 动手清单

- [ ] `cargo test -p forge-core --test m3_*`：跑通 4 个测试。
- [ ] `cargo +nightly miri test -p forge-core --test m3_04_protects_across_threads`：用 miri 验证 happens-before。
- [ ] **故意把** `spin.rs` 里的 `Release` 改成 `Relaxed`，用 loom 跑（参考原书第 7 章末的实验），观察错率。改回来。
- [ ] 写上面那份 criterion benchmark，在你自己的机器上测 false sharing 的真实倍数。换 `#[repr(align(128))]` 看是否更高（某些机器缓存行是 128B）。
- [ ] 把 `bench_spin` 和 `bench_std` 在 2/4/8/16 worker 下对比，画出"自旋优势随 worker 数变化"的曲线。找出"自旋开始变差"的拐点。
- [ ] **思考题**：如果 `T: !Send`（比如 `Rc<T>`），我们标 `unsafe impl Sync for SpinLock<T>` 会怎样？写出 UB 的具体场景。（提示：Rc 的引用计数不是原子的。）
- [ ] **进阶思考题**：我们的 SpinLock 不公平。怎么改才能保证 FIFO？需要哪些数据结构？（提示：链表排队，M8 详讲。）

---

下一站 → [M4 自建 Arc / Weak](./M4-arc.md)：M2 里 `Arc` 是黑盒，现在我们把它拆开——亲手实现原子引用计数、`Weak` 指针、以及那个防 use-after-free 的 `Release` 计数 + `Acquire` 栅栏。M3 学的"unsafe impl Sync where T: Send"骨架会再次出现。
