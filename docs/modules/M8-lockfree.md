# M8 — 无锁结构动物园：把第 1–7 章的工具集装成 7 台机器

> 模块：`forge-lockfree::{semaphore, rcu, stack, mcs, parking_lot, seqlock, deque}`
> 测试：`crates/forge-lockfree/tests/m8_*.rs`
> 跑：`cargo test -p forge-lockfree`

---

## 〇、开战之前：这一章到底在干什么

回想你到目前为止手里的家伙：原子变量（M1）、互斥锁（M2/M7）、自旋锁（M3）、`Arc`（M4）、通道（M5）、`atomic-wait` 即 futex（M6）、用 futex 拼的真 `Mutex`/`Condvar`/`RwLock`（M7）。**这些是零件**。M8 这一章是一个**装配车间**——我们把零件拼成 7 台真正在工程上能见到的机器：

1. **信号量 Semaphore** —— 一个原子计数器，把"锁/条件变量/停止位/就绪位"全统一成同一样东西。
2. **RCU（Read-Copy-Update）** —— 多读偶改的一大块数据，怎么换？答案是：**别动原的，造个新的，原子换指针**。
3. **Treiber 无锁栈 + ABA** —— 全靠 CAS，看似漂亮，但藏着一个能把程序烧成灰的 ABA bug。我们用 loom 把它抓出来。
4. **MCS 队列锁** —— 把"谁在等锁"这条队列从内核搬到用户态，每个 spinner 只 ping 自己的缓存行。
5. **parking-lot 式锁** —— 把锁压缩到 1 字节，等待者塞进全局 `HashMap`。WebKit 的 JavaScript 锁就是这么干的。
6. **SeqLock（序列锁）** —— 让读者**完全不阻塞**写者、写者也**完全不阻塞**读者，靠一个奇偶翻转的计数器。
7. **Chase-Lev 工作窃取双端队列** —— rayon、Go 调度器、crossbeam-deque 的心脏。我们按 Le et al. 论文放 fence。

每台机器都至少打**一个敌人**——一个用朴素工具会失败的并发场景。每个机器都有它存在的**唯一理由**。我们的目标不是让你背 7 个名词，是让你看完后能对自己说：**"看到这种并发问题，我知道该抄哪一台机器。"**

本章里我们要做 **4 次逐拍手算**（Treiber 栈 ABA、Chase-Lev fence 放置、SeqLock 读者重试、MCS 队列交接）。手算是核心——并发算法的正确性**只能**靠把每个拍子上的状态画出来证明，光读代码不行。

一个总纲性的提醒，贯穿全章：**"无锁"不是"无害"**。无锁代码是最容易写错、最难调试的代码类型之一。它换来的是"不阻塞"和"低延迟"，代价是"内存回收难"、"内存序难"、"ABA 难"。我们后面会一一撞上这三座墙。

---

## 一、M8a Semaphore：把"停止位/延迟位/就绪位/锁"统一成一个东西

### ENEMY：你有 4 个看起来毫无关系的同步装置

回顾一下你到目前为止见过的"两个状态"装置：

- **SpinLock / Mutex**：一个 `AtomicBool`，0 = 未锁、1 = 已锁。lock 把它从 0 翻到 1，unlock 翻回 0。
- **M5 oneshot 通道的"停止位"**：sender drop 后把它置 1，receiver 看到它就停止接收。
- **M5 oneshot 通道的"延迟初始化位"**：值还没塞进去时 = 0，塞进去翻成 1。
- **M7 Condvar 的 `notify` 信号**：counter 从 0 涨到 1 表示"该醒的人快醒"。

这 4 个东西写法各不相同。读者大概在想：**它们差别很大吧？锁要抢、停止位只是个观察标记、信号要唤醒人……**

可你仔细看看：**它们本质上都是"一个只能取 0 或 1 的计数器，0 = 没许可、1 = 有许可"**。差别只是"有人等的时候怎么办"。

我们要在这一节做一件狠事：**把这 4 件东西全部统一成同一个数据结构——二元信号量（binary semaphore）**。二元信号量就是初始许可数 = 1 的信号量；它的 `acquire` = `wait`（拿走一个许可，没有就睡）、`release` = `signal`（放回一个许可，唤醒一个等的人）。一旦你接受了这个同构，你就拥有了**一个工具同时表达锁、停止位、延迟位、就绪位、Condvar 信号**——这就是为什么 Mara Bos 在第 10 章第一节就讲它。

更深一层：许可是计数器，**计数器可以被推广到任意正整数**。把许可数从 1 推到 N，信号量就变成了"限制最多 N 个并发"的工具——线程池限制最多 N 个 worker 同时干活、连接池限制最多 N 个数据库连接、rate limiter 限制每秒最多 N 个请求。这些都是同一个 N-元信号量的不同用法。**从 1 到 N 是量的推广，从 N 到 1 是质的回归**——所有同步装置的本质都是计数器。

### ANCHOR：餐厅的桌位计数器

锚定一个画面：一家餐厅有 N 张桌子。门口放一块翻牌，上面写当前**可用桌位数**。客人来了看一眼：

- 数字 > 0：把牌子翻小一格（数字减 1），进去吃。
- 数字 = 0：等。等到有人吃完出门把牌子翻大（数字加 1），并叫一个等的人。

这就是信号量。两个原语：

- **`acquire` / `wait` / `P` / `down`**：拿一个许可。许可不够就阻塞。
- **`release` / `signal` / `V` / `up`**：还一个许可。从 0 涨到 1 时叫醒一个等待者。

二元信号量就是 N = 1 的特例：同一时刻只能有 1 个人持有许可。**这不就是 mutex 吗？** 是的。把二元信号量初始化成 1，`acquire` = lock，`release` = unlock，它就是 mutex。

那初始化成 0 呢？它就变成了"等信号"：A 线程 `acquire` 会立刻睡（许可 0），B 线程 `release` 把许可加到 1 并唤醒 A。这就是 Condvar、就是 `thread::park`/`unpark`、就是 oneshot 通道的"就绪位"。**终极同构骨架**——M1/M5/M7 里那些看似不同的"停止位/就绪位"全是同一个东西。

### LOW-FI：用 Mutex + Condvar 造一个朴素信号量

最朴素的实现就是一个 `Mutex<u32>` + `Condvar`：

```rust
use std::sync::{Condvar, Mutex};
use std::collections::VecDeque;

pub struct NaiveSemaphore {
    state: Mutex<u32>,
    has_permit: Condvar,
}

impl NaiveSemaphore {
    pub fn new(permits: u32) -> Self {
        Self { state: Mutex::new(permits), has_permit: Condvar::new() }
    }
    pub fn acquire(&self) {
        let mut s = self.state.lock().unwrap();
        while *s == 0 {
            s = self.has_permit.wait(s).unwrap();   // 睡到有人 release
        }
        *s -= 1;
    }
    pub fn release(&self) {
        let mut s = self.state.lock().unwrap();
        *s += 1;
        self.has_permit.notify_one();
    }
}
```

它**对**。它也**慢**。每次 `acquire`/`release` 都要锁 `Mutex`——**即便没有竞争**也要进一次锁。语义上一个信号量不过是一个原子计数器，凭啥要拖一个 Mutex？我们要把锁砍掉。

### WRITE：单个 `AtomicU32` + futex 版

回忆 M6/M7：`atomic_wait::wait(addr, expected)` 仅当 `*addr == expected` 时才睡；`wake_one(addr)` 唤醒一个等待者。这正好够我们造一个**不带 Mutex 的**信号量。看 `crates/forge-lockfree/src/semaphore.rs`：

### WRITE v1（看似正确的天真版）：单个 `AtomicU32` + futex

回忆 M6/M7：`atomic_wait::wait(addr, expected)` 仅当 `*addr == expected` 时才睡；`wake_one(addr)` 唤醒一个等待者。这看似够造一个**不带 Mutex 的**信号量。先写一版看起来无懈可击的：

```rust
use atomic_wait::{wake_one, wait};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct SemaphoreV1 {
    permits: AtomicU32,
}

impl SemaphoreV1 {
    pub fn new(permits: u32) -> Self { Self { permits: AtomicU32::new(permits) } }

    pub fn acquire(&self) {
        loop {
            let n = self.permits.load(Ordering::Relaxed);
            if n > 0 {
                if self.permits
                    .compare_exchange_weak(n, n - 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                { return; }
            } else {
                wait(&self.permits, 0);
            }
        }
    }

    pub fn release(&self) {
        // ⚠ 看似合理：旧值是 0 ⇒ "可能有人在等" ⇒ wake_one。
        if self.permits.fetch_add(1, Ordering::Release) == 0 {
            wake_one(&self.permits);
        }
    }
}
```

非竞争路径（最常见的"大家都不抢"）只有**一次 CAS**，没有任何 syscall——这版快得漂亮。`release` 的逻辑看上去也无懈可击："旧值是 0 就是'从无到有'，这一刻最该叫醒等待者"。

**可惜这版在多等待者场景下会死锁。** 这不是我们臆想的 bug——`tests/m8a_semaphore_regression.rs` 真能复现它（5 秒超时失败）。下面手算给你看。

### 故意打破：v1 的多等待者丢唤醒（手算复现）

设 permits=0，两个消费者 T1、T2 都想 acquire，一个生产者做两次 release。用 barrier 保证 T1、T2 都**已经进入 futex wait 队列**之后生产者才开始 release（把"两个 waiter 都在等"这个前置做实）：

| 拍 | 动作 | permits | futex 队列 | 说明 |
|---|---|---|---|---|
| 1 | T1 acquire：load=0，wait(permits,0) | 0 | [T1] | T1 确认 permits==0，入睡 |
| 2 | T2 acquire：load=0，wait(permits,0) | 0 | [T1,T2] | T2 也入睡 |
| 3 | release：fetch_add → 0→1，旧值==0 ⇒ **wake_one** | 1 | [T2] | T1 被唤醒 |
| 4 | release：fetch_add → 1→2，**旧值==1≠0 ⇒ 不 wake** | 2 | [T2] | 🔥 唤醒丢了 |
| 5 | T1 醒来：load=2，CAS 2→1 ⇒ 拿到许可 | 1 | [T2] | T1 完成 |
| 6 | …… | 1 | [T2] | permits=1，但 T2 永远睡 |

第 4 拍是罪魁：第二次 release 看到 permits 从 1 变 2（旧值非 0），于是判定"没人等"，不 wake。可 **T2 还在队列里**。T1 拿走一个许可后，permits 剩 1，T2 却再也等不到唤醒。

**根因**：`permits` 这一个数**无法同时**表达"当前许可数"和"是否有等待者"两件事。当许可从一个被唤醒但还没消费的 waiter 手里"经过"时（第 3 拍 wake 了 T1，但 T1 还没 CAS），permits 已经 ≥1，下一次 release 就误判"没人等"。`permits=0 ⇒ 可能有 waiter` 这条"复用状态位"的捷径，在多等待者下是**错的**。

这正好是 musk.md 说的"故意打破再重建"——v1 的心智模型（"许可数兼任等待者标志"）在一个具体场景下失效，我们必须扩模型。

### WRITE v2（修复版）：再加一个等待者计数器

破局：用一个**独立的** `num_waiters` 原子显式记录"已入睡或正要入睡"的等待者数。release 只要看它 >0，就 wake_one。看 `crates/forge-lockfree/src/semaphore.rs` 的真实代码：

```rust
pub struct Semaphore {
    permits: AtomicU32,
    /// 当前"已入睡或正要入睡"的等待者数。
    num_waiters: AtomicU32,
}

impl Semaphore {
    pub const fn new(permits: u32) -> Self {
        Self { permits: AtomicU32::new(permits), num_waiters: AtomicU32::new(0) }
    }

    pub fn acquire(&self) {
        loop {
            let n = self.permits.load(Ordering::Relaxed);
            if n > 0 {
                if self.permits
                    .compare_exchange_weak(n, n - 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                { return; }
            } else {
                // 关键顺序：先登记，再复查 permits，最后才 wait。
                self.num_waiters.fetch_add(1, Ordering::Release);
                if self.permits.load(Ordering::Acquire) == 0 {
                    wait(&self.permits, 0);
                }
                self.num_waiters.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    pub fn release(&self) {
        self.permits.fetch_add(1, Ordering::Release);
        if self.num_waiters.load(Ordering::Acquire) > 0 {
            wake_one(&self.permits);
        }
    }
}
```

把刚才那张 bug 时序表照 v2 重画一遍：

| 拍 | 动作 | permits | num_waiters | futex 队列 | 说明 |
|---|---|---|---|---|---|
| 1 | T1：num_waiters 0→1，复查 permits==0，wait | 0 | 1 | [T1] | 登记 + 入睡 |
| 2 | T2：num_waiters 1→2，复查 permits==0，wait | 0 | 2 | [T1,T2] | 登记 + 入睡 |
| 3 | release：fetch_add 0→1；load num_waiters=2>0 ⇒ wake_one | 1 | 2 | [T2] | T1 醒 |
| 4 | release：fetch_add 1→2；load num_waiters=2>0 ⇒ **wake_one** | 2 | 2 | [] | ✅ T2 也醒 |
| 5 | T1：CAS 2→1，num_waiters 2→1 | 1 | 1 | [] | T1 完成 |
| 6 | T2：CAS 1→0，num_waiters 1→0 | 0 | 0 | [] | T2 完成 |

第 4 拍不再丢唤醒——num_waiters=2>0，release 总会叫醒一个。两次 release 叫醒两个 waiter，正好。

**为什么 `acquire` 里是"先登记、再复查、最后 wait"这个顺序？** 这是在堵另一个缝：假如 release 夹在"load 看到 0"和"fetch_add 登记"之间，没登记就 wait，release 的 wake_one 就找不到人。登记之后**再读一次 permits**：若这期间有人 release 把 permits 抬上去，复查就看到了，根本不睡；若仍是 0 才 wait。这条"登记→复查→睡"的顺序和 M6 futex 的 `expected` 机制双重保险——即使 release 的 wake_one 在我们登记之后、wait 之前到达（队列里没人），wait 进内核时会发现 permits≠0 立即返回，绝不误睡。

逐项审内存序：

- `acquire` 的 `compare_exchange_weak(.., Acquire, ..)`：成功建立 happens-before，让我们看见 release 之前的所有改动（信号量当"等数据就绪"用时这条序很关键）。失败用 Relaxed——失败只是重试，没消费什么。
- `num_waiters.fetch_add(1, Release)` + release 端 `num_waiters.load(Acquire)`：这对 Release/Acquire 建立跨线程可见——release 看到 ≥1 个登记的等待者，就能放心 wake_one。`fetch_sub` 用 Relaxed 即可：那时我们已经在重试 CAS，不需要它再同步什么。
- `release` 的 `fetch_add(1, Release)`：发布"许可+1"给 acquire 的 Acquire。
- 没有用 SeqCst。原书也指出：信号量不需要全局全序，Acquire/Release 已够。

### ISO·ZOOM：为什么 `wait(permits, 0)` 必须带那个 0

新人最常写错的地方是去掉 `wait` 的第二个参数，让它无脑睡。会出什么 bug？

- T1 `acquire`：load = 0，准备去 `wait`。
- T2 `release`：`fetch_add(1)` 把 permits 0→1，num_waiters>0 ⇒ `wake_one`。**此刻 T1 还没进 wait**。
- T1 进 `wait`——若没带 expected，它真的睡了，**永远**。唤醒丢了。

带上 `wait(&permits, 0)`：T1 进 wait 时内核**原子地**再读一次 permits，发现它**已经不是 0**（被 T2 加成 1 了），立刻返回不睡——T1 重试 CAS 拿到。这正是 M6 讲的"expected 是 futex 防丢失唤醒的核心"。**整条 futex 路径的正确性都押在这个 0 上**——它和"登记→复查"的顺序是同一副机制的两面：内核的 atomic check-and-sleep 补上了用户态"复查→wait"之间的最后一道缝。

### 深一步：虚假唤醒（spurious wakeup）与循环重试

注意 `acquire` 的整个逻辑是 `loop { ... }`——为什么必须循环？因为 `wait` 在 Linux 上是 `futex(FUTEX_WAIT)` 系统调用，**系统调用允许虚假唤醒**——也就是 permits 还是 0 的情况下 wait 也可能"无缘无故"返回。这是 POSIX 明文允许的行为，许多 syscall 实现为了性能会这么做。

我们的代码循环重试，每次重试都重新评估"有 permit 吗？没有就再 wait"。这种"loop + 重新检查条件"的模式叫**条件变量标准用法**——M7 的 Condvar.wait 也是同样套路。**任何 wait/wake 原语都要配合循环检查条件**，因为底层可能虚假唤醒。

虚假唤醒不是 bug，是设计——内核为了实现简单允许这么做。**程序员必须假设 wait 随时可能返回**，把"条件是否满足"的判断留给应用层。

### 测试

- `tests/m8_01_semaphore.rs`：8 个线程、许可数 = 2、各自 acquire/yield/release 200 轮，验证"同一时刻活跃线程数永远 ≤ 2"。钉死"限制并发数"的语义。
- `tests/m8a_semaphore_regression.rs`：**专门复现 v1 的多等待者丢唤醒**（就是上面那张表）。Semaphore(0)，两线程 barrier 后同时 acquire，主线程 release 两次，带 5 秒超时。v1 必卡死失败；v2 秒过。这个测试是 v1→v2 演化的"老师"——先红后绿，bug 才算真被关上。

### 进阶：从信号量到 Mutex、Condvar、oneshot 通道

讲完同构骨架还不够，我们要把这个同构**做出代码**——你能用同一个 `Semaphore` 当 mutex、当 condvar、当通道就绪位。

```rust
use forge_lockfree::semaphore::Semaphore;

// 当 mutex 用：初始许可 1。
let mutex_proxy = Semaphore::new(1);
mutex_proxy.acquire();   // == lock
// ... 临界区 ...
mutex_proxy.release();   // == unlock

// 当"事件信号"用（初始 0）：A 等 B 完成
let done = Semaphore::new(0);
let d = done.clone_safe();   // 假设我们包了一层 Arc
thread::spawn(move || { /* 工作 */ d.release(); });
done.acquire();   // 阻塞到 spawn 的线程 release

// 当 oneshot 通道的"值就绪位"
let ready = Semaphore::new(0);
// 发送方塞值进 slot，然后 ready.release()
// 接收方 ready.acquire() 后读 slot —— 值必然已就绪
```

这三段代码用的是**同一个数据结构**——这就是 Mara Bos 第一节讲信号量的真正用意：**它是元 primitive，其它同步装置都可以从它派生**。Williams 在《C++ Concurrency in Action》里也强调过这点：标准库的 mutex/condvar 都可以被信号量实现，反之亦然——选哪一种作为基础纯粹是工程取舍。

### 一节小结

信号量教会我们：**"许可" 是一个比"锁"更基础的概念**。锁是许可 = 1 的特例，条件变量是许可 = 0 的特例。"减少许可 / 增加许可"这一对原子操作，足以表达任何"等什么 / 通知什么"的同步模式。当你下次设计一个新同步装置，先问自己：**它本质上是不是某种许可计数？**

### 真实工程里的信号量

`tokio::sync::Semaphore` 是异步生态里的标准实现，限制异步任务并发数（典型用法：限制 HTTP 客户端最多 N 个并发请求）。它内部就是一个 permit 计数器 + 等待者 waker 队列——和我们的实现是同一骨架，只是把 `thread::park` 换成 `task::yield`。

`std::sync::Semaphore` 在 Rust 1.0 之前曾存在过，后来被移除——因为信号量能表达的同步模式太多，标准库不想选边（用 mutex 还是 condvar 表达？）。这把决策留给了 crate 生态，于是有了 `parking_lot::Semaphore`、`async_std::sync::Semaphore`、`tokio::sync::Semaphore` 等多种实现。

历史上信号量比 mutex 早——Dijkstra 在 1965 年提出 P/V 操作，是并发编程的祖宗级概念。mutex 是后来简化出来的"二元信号量"。所以**从历史看，mutex 是信号量的特例**；从工程看，**信号量是 mutex 的泛化**。哪个作为基础 primitive 都行，看你的工程需要。

### 这一节的礼物：一个抽象，四个装置

记住这一节唯一一件事：**"二元信号量" = "mutex" = "Condvar 信号" = "就绪位/停止位"**——它们语义上是同一个原子计数器。这就是为什么 std 的 `thread::park`/`unpark` 在内部其实就是一个绑定到线程句柄的二元信号量（初始为 0），unpark = release，park = acquire。

接下来我们换一个完全不同的敌人：**数据太大，装不进任何原子类型**。怎么办？

---

## 二、M8b RCU：装不进原子的"大块数据"怎么换？

### ENEMY：32 字节的结构体你没法原子替换

你有一个 `struct Config { retries: u32, timeout_ms: u64, endpoint: String, ... }`，几十个字段。一堆线程经常读它、偶尔有人改它。`RwLock<Config>` 行不行？行，但每次读都要 `lock().read()`——拿不到就走 syscall。`AtomicUsize` 行不行？不行，结构体太大，最大原子类型只到 64 位（部分平台 128 位）。

朴素思路：**给每个字段单独一个原子**。可读的时候你要读多个字段，它们可能来自不同的时刻——你看到 `retries=3`（已更新）、`timeout_ms=5000`（旧值）的撕裂快照。这是个新敌人：**撕裂读（torn read）**。

撕裂读的恐怖之处在于它**不导致 crash，只导致错误结果**。配置读出 `retries=3, timeout_ms=5000` 看着合理，程序继续跑，但行为已经偏离正确配置——这种 bug 在生产里极难定位，因为它"看起来没坏"。日志里你看到超时设置成了 5000ms（旧值），重启后又变回 1000ms（新值），任何死板的排查手段都抓不到——因为它压根不抛错误。

这就是为什么"多字段一致快照"这件事在并发里值得专门起个名字（RCU）和专门写一节——它的敌人不是性能、不是死锁，是**悄无声息的逻辑错误**。

### ANCHOR：图书馆换目录卡

锚个画面：图书馆有一张目录卡，列出当前所有书的清单。读者来了看一眼清单找书。馆长每周要更新清单（加新书、下旧书）。如果馆长**当场涂改**那张卡，读者在他涂改时刚好来看，就会看到一半新一半旧的混乱清单。

正确做法：馆长**复印一份**，在复印件上涂改，涂完之后**整张替换**——把目录台上那张卡一瞬间换成新卡。读者要么看到旧卡、要么看到新卡，**永远不会看到一半**。

这就是 RCU——**R**ead-**C**opy-**U**pdate：读（旧版本）、复制（一份）、改（副本）、更新（原子换指针）。它的本质是**加一层间接**：

```
struct Config { ... }                  // 大数据
let ptr: AtomicPtr<Config> = ...;      // 用一个指针指它
```

指针本身是机器字大小，**可以原子替换**。读路径只读指针，拿到一份快照；写路径复制一份、改副本、CAS 把指针换到副本。读者要么拿到旧指针、要么拿到新指针，**永远看不到撕裂**。

### LOW-FI：先写出"读-复制-更新"骨架

```rust
use std::sync::atomic::{AtomicPtr, Ordering};
use std::ptr;

pub struct Rcu<T> {
    current: AtomicPtr<T>,
}

impl<T> Rcu<T> {
    pub fn new(value: T) -> Self {
        Self { current: AtomicPtr::new(Box::into_raw(Box::new(value))) }
    }

    pub fn read(&self) -> *const T {                       // 读：拿指针
        self.current.load(Ordering::Acquire)
    }
    pub fn update(&self, f: impl FnOnce(&T) -> T) {        // 复制 + 改 + 换
        let old = self.current.load(Ordering::Acquire);
        let new = Box::into_raw(Box::new(f(unsafe { &*old })));
        let prev = self.current.swap(new, Ordering::AcqRel);
        // ...prev 指向旧值，但别的线程可能还在读它，怎么办？
    }
}
```

读到 `swap(new, AcqRel)` 这一步，旧值 `prev` 还活着——别的线程可能正握着 `prev` 在读它的字段。我们**不能**立刻 `drop(Box::from_raw(prev))`——那等于在别人脚下抽走地毯。这就是 RCU 名字里没写出来的第 5 个字母：**D（Deallocation）**，回收。回收才是 RCU 最难的部分。

### ENEMY·二：回收的难题

Mara Bos 在第 10 章那幅图里把第 5 步画成灰色加问号——故意提醒你"这里没标准答案"。她列出 5 种策略：

1. **引用计数（`Arc`）**：每个读者 `clone` 一份 Arc，强引用计数保证旧值不死到所有读者放手。简单 sound，但每次读要 `fetch_add`——一条原子写，**争用**。
2. **泄漏**：永不回收。永不出错，但内存只增不减——只能用于"启动后改几次"的配置。
3. **GC**：让运行时跟踪。Rust 没有 GC。
4. **Hazard pointer**：每个线程在自己线程局部存"我正在读哪些指针"的清单，回收者扫一遍所有线程的清单，确认没人指向 `prev` 才回收。M8c 我们会细讲。
5. **Quiescent state（静止态）**：等所有线程都"走过某个安全点"——比如 epoch 切换——就保证没人在读旧值。这就是 crossbeam-epoch 的思路，下面我们就仿它写一个简化版。

### WRITE：用 Arc 回收的 RCU（forge-lockfree 的版本）

我们的 crate `forge-lockfree/src/rcu.rs` 选了最简单也最 sound 的策略——**Arc 回收 + Mutex 串行写**：

```rust
use std::sync::{Arc, Mutex};

pub struct Rcu<T> {
    current: Mutex<Arc<T>>,
}

impl<T> Rcu<T> {
    pub fn new(value: T) -> Self {
        Self { current: Mutex::new(Arc::new(value)) }
    }
    pub fn read(&self) -> Arc<T> {
        Arc::clone(&self.current.lock().unwrap())
    }
    pub fn update(&self, f: impl FnOnce(&T) -> T) {
        let mut g = self.current.lock().unwrap();
        *g = Arc::new(f(&g));
    }
}
```

读路径用 `Mutex` 拿到 `Arc` 的 clone——这条 Mutex 只在 `read`/`update` 各持有一瞬（纳秒级），实际工程里 contention 极低。写路径靠 Mutex 串行化（原书那条 tip：用 mutex 避免并发修改的复杂性）。**旧 `Arc` 在所有读者放手前不会被 drop**——这就是 Arc 替我们做的回收工作。

代价：读路径有一次锁。下面我们看 crossbeam-epoch 怎么把这个锁也去掉。

### ISO·ZOOM：从零实现 epoch-based reclamation（crossbeam-epoch 思路）

#### 敌人先行：和 hazard 同一个敌人，换一种打法

epoch-based reclamation（以下称 epoch）和 hazard pointer 解决的是**同一个敌人**——无锁结构里"指针刚被 free、别的线程还拿着它在读"。但两者打法截然不同。

hazard 的思路是**逐指针公告**：每个读者告诉回收者"我在用哪个指针"，回收者**精确**地避开这些指针。这种方式精细、回收快（攒够量就能扫一次），但读者要做两次原子写（store hazard 槽 + 清空）+ 一条 SeqCst fence。

epoch 反过来：**完全不关心具体指针，只关心"代次"**。回收者只问一件事——"所有读者都已经进入了新代次吗？"只要所有读者都进了新代次，那旧代次里退役的指针就**整体**可回收——不需要扫每个指针、不需要二分查找。

听起来抽象，先看为什么这条路成立。

#### LOW-FI 心智模型：餐厅换菜单

锚个画面。一家餐厅每周一换菜单。服务员们手上拿着的菜单可能是这一周的、也可能是上一周的（旧菜单还没收）。经理要回收旧菜单，要怎么做？

朴素做法（hazard 风格）：每个服务员在自己的工牌上写"我手上拿的是哪份菜单"，经理回收每份旧菜单前必须扫所有工牌。这就是上一节的方案。

epoch 风格：经理在门口挂一块牌子"现在是第几周"（global_epoch）。服务员进餐厅时**看一眼牌子**，在自己工牌上写下"我服务的周次"（pin）。经理每周一换菜单时**只看一件事**：所有服务员工牌上的周次都跟上了吗？只要还有一个服务员工牌上是上周，就等。等所有人都进到新周次——上周的菜单可以**整体下架**，不需要逐份核对。

差别在哪？hazard 是**每个指针单独决策**（"这份菜单有没有人在用？"），epoch 是**所有指针批量决策**（"老的一批整体回收，因为我确信没人还停在旧代次"）。代价：epoch 的回收有延迟（要等"所有人跟上"），但读路径开销极低——只一次原子写（写自己的代次）。

#### 代码要点：与 `epoch.rs` 的真实实现对应

`crates/forge-lockfree/src/epoch.rs` 把全局 epoch 和每线程 local epoch 都放进静态结构：

```rust
struct EpochRegistry {
    global_epoch: AtomicUsize,
    locals: [AtomicUsize; MAX_EPOCH_THREADS],   // 每线程一个 local_epoch
    next_slot: AtomicUsize,
}
static REGISTRY: EpochRegistry = EpochRegistry::new();
```

每线程的 GARBAGE 是**三袋**垃圾——这是 epoch 算法的关键工程实现：

```rust
static GARBAGE: RefCell<[Vec<(*mut (), fn(*mut ()))>; 3]> = ...;
```

为什么三袋？因为 epoch 推进是循环复用的——`garbage[0]`、`garbage[1]`、`garbage[2]` 轮流装"当前 epoch 退役的指针"。每推进一次 epoch，回收最老那袋。下面手算会看到这个三袋循环。

读者用 RAII guard 进出临界区：

```rust
pub fn pin() {
    // ...
    let g = REGISTRY.global_epoch.load(Ordering::Acquire);
    REGISTRY.locals[slot].store(g + 1, Ordering::Release);   // +1 偏移，区分 UNPINNED=0
    fence(Ordering::SeqCst);
}

pub fn unpin() {
    // ...
    REGISTRY.locals[slot].store(UNPINNED, Ordering::Release);
    try_advance();    // 顺便尝试推进
}
```

写者退役指针：

```rust
pub unsafe fn defer_destroy(ptr: *mut (), destructor: fn(*mut ())) {
    let g = REGISTRY.global_epoch.load(Ordering::Relaxed);
    GARBAGE.with(|bag| bag.borrow_mut()[g % 3].push((ptr, destructor)));
}
```

推进 epoch 在 `try_advance`：

```rust
let cur = REGISTRY.global_epoch.load(Ordering::Acquire);
for i in 0..max {
    let l = REGISTRY.locals[i].load(Ordering::Acquire);
    if l != UNPINNED && l - 1 < cur {
        return;   // 还有线程 pin 在比 cur 旧的 epoch，不能推进
    }
}
fence(Ordering::SeqCst);
if REGISTRY.global_epoch.compare_exchange(cur, cur + 1, AcqRel, Relaxed).is_err() {
    return;
}
// 回收 garbage[(cur-1) % 3]——出生在 cur-1，现在到了 cur+1，已经过 2 个 epoch
```

#### 必选手算：T1 pin epoch 0、T2 替换 X、try_advance 推进的逐拍时刻表

设栈 `head → X → Y`，X 出生在 epoch 0。三线程：T1（读者，pin）、T2（写者，retire X）、T3（读者，pin/unpin）。

**拍 0（初始）**：

```
global_epoch = 0
T1.local = UNPINNED, T2.local = UNPINNED, T3.local = UNPINNED
garbage[0] = [], garbage[1] = [], garbage[2] = []
head = X (alive)
```

**拍 1：T1 pin**。读 global_epoch=0，写自己 local = 0 + 1 = 1（偏移 1），fence(SeqCst)。

```
global_epoch = 0
T1.local = 1 (pin 在 epoch 0)
T1 读 X 的内容（"我在用 X"）
```

**拍 2：T2 是写者，它替换 head：X → Y**，把 X 从栈摘下。然后调 `defer_destroy(X, dtor)`。

```
let g = global_epoch.load(Relaxed) = 0
GARBAGE[T2][0 % 3 = 0].push(X)
```

```
global_epoch = 0
T2.garbage[0] = [X]   ← X 出生在 epoch 0
head = Y
```

**拍 3：T2 调 `try_advance()`**。读 cur=0，扫所有线程 local：T1.local=1（=epoch 0），**1 - 1 = 0 不 < cur=0**——OK，T1 跟得上。其它线程都是 UNPINNED。fence(SeqCst)。CAS global_epoch: 0 → 1 成功。

现在到回收步骤：`if cur >= 1`——cur=0，**跳过回收**。这一拍没有回收任何东西。

```
global_epoch = 1
T2.garbage[0] = [X]   ← 还在！为什么？
```

为什么 X 没被回收？这就是"两 epoch 宽限期"的精髓。X 出生在 epoch 0。try_advance 把 global 推到 1，回收的应该是 `garbage[(cur-1) % 3]` = `garbage[(0-1) % 3]` = `garbage[2]`——但 X 在 `garbage[0]`，没动。**X 要等到 global_epoch 推到 2 时才被回收**（那时回收的是 `garbage[(1-1) % 3]` = `garbage[0]`）。

为什么不能在 global=1 时就回收 garbage[0]（出生在 epoch 0 的指针）？因为**T1 还 pin 在 epoch 0**！T1 可能在拍 1 和拍 3 之间的任何时刻仍在读 X。如果 global 推到 1 就回收 X，T1 立刻 use-after-free。

**拍 4：T2 再次 try_advance**。读 cur=1，扫所有 local：T1.local=1（epoch 0），**1 - 1 = 0 < cur=1**——**T1 还 pin 在旧 epoch**，try_advance **失败**，return。

```
global_epoch = 1   ← 没推进
T1.local = 1 (pin 在 epoch 0)
T2.garbage[0] = [X]   ← 还在
```

这就是 epoch 的关键阻塞点：**只要还有任何线程 pin 在旧 epoch，全局 epoch 就推不动**。T1 不放手，整个系统只能等。

**拍 5：T1 unpin**。`locals[slot].store(UNPINNED)`，然后顺手 try_advance。读 cur=1，扫所有 local：T1=UNPINNED，T2/T3=UNPINNED。所有活跃线程都已 ≥ cur=1（实际是"无活跃"——无人 pin 即"无人落后"）。fence(SeqCst)。CAS global_epoch: 1 → 2 成功。

回收 `garbage[(cur-1) % 3]` = `garbage[(1-1) % 3]` = `garbage[0]`——**这里就是 X**！安全回收 X。

```
global_epoch = 2
T2.garbage[0] = []   ← X 被 dtor 回收
```

为什么这次安全？因为 T1 已 unpin——T1 不再持有 X。T3 还没进入，没有别的线程持有 X。X 此时回收不会 use-after-free。

**拍 6：T3 pin**。读 global_epoch=2，写 local=3（=epoch 2 + 1）。读 Y 的内容。unpin——local=UNPINNED。

```
global_epoch = 2
T3.local = UNPINNED
```

三袋垃圾的状态机：`garbage[0]` 现在空，可以装未来 epoch 3 的退役指针（3 % 3 = 0）；`garbage[1]` 装 epoch 4 的；`garbage[2]` 装 epoch 5 的——三袋循环。

#### 为什么是"两 epoch 宽限期"？一个 epoch 不够吗？

musk.md 要求打破再重建。考虑"一 epoch 宽限期"的朴素方案——global 推到 N+1 就回收所有 epoch ≤ N 的指针。画出会出 bug 的拍子：

| 拍 | T1（读者） | T2（try_advance） |
|---|---|---|
| 1 | T1 准备 pin。load global_epoch = N | |
| 2 | | T2 try_advance，扫所有 local——T1 还没写，T1.local=UNPINNED，OK。fence，CAS global: N → N+1。**回收 garbage[N%3]**。 |
| 3 | T1 store local = N+1（注意！T1 在拍 1 读到的是 N，但 store 时已经写 N+1 也合理——T1 不知道自己迟到） | |
| 4 | T1 解引用 X（出生在 epoch N，已被 T2 回收） | |

T1 在拍 1 load 到 global=N，但还没 store 自己的 local——T2 的 scan 看到 T1 是 UNPINNED，认为"T1 没参与"，推进 epoch 并回收。T1 在拍 2 之后才 store local——这时 T1 的 store 已经晚了，T2 已经走完。T1 此后读 X 就是 use-after-free。

**两 epoch 宽限期**堵的就是这个：T1 在拍 1 load 到 N，T2 把 global 推到 N+1——但 garbage[N%3] 此时**不**被回收，要等到 global 推到 N+2 时才回收。global 从 N+1 推到 N+2 又要 try_advance，而 try_advance 要求所有 local_epoch ≥ N+1。T1 既然 pin 在 N（local=N+1 偏移后），local_epoch=N<N+1，**第二次 try_advance 失败**——T2 无法把 global 推到 N+2，无法回收 garbage[N%3]。直到 T1 unpin。

这就是"两 epoch 宽限期"的精确含义：**多出一个 epoch 的缓冲，让"正在 pin 但还没写 local"的读者被下一次 try_advance 拦住**。一个 epoch 不够——会出现"load 后 store 前"的缝隙；两个 epoch 够——把缝隙堵在"无法推进第二次"。

#### ISO·ZOOM：epoch vs hazard 是回收问题的两极

epoch 和 hazard 在回收问题上几乎是**镜像**的两种解法：

| 维度 | hazard pointer | epoch |
|---|---|---|
| 公告内容 | 具体指针 | 代次 |
| 读路径开销 | 2 原子写 + SeqCst fence | 1 原子写 + SeqCst fence |
| 回收判定 | 逐指针查公告栏（O(N×M)） | 整代次批量（O(N) 扫所有 local） |
| 回收延迟 | 短（攒 SCAN_THRESHOLD=32 就扫） | 长（等 2 个 epoch 推进） |
| 内存峰值 | 低 | 中（垃圾滞留 2 epoch） |
| 工程实现复杂度 | 中（公告栏管理 + 扫描） | 高（三袋循环 + 推进协调） |

crossbeam 生态（crossbeam-epoch、crossbeam_skiplist、crossbeam_queue::SegQueue）选 epoch 的原因：① 读路径最便宜（pin 只一次 fetch_add）；② 批量回收摊薄扫描；③ 高并发下吞吐最高。代价是回收延迟——但绝大多数读多写少场景能容忍"几百纳秒后回收"。

crossbeam-epoch 的 pin 实现甚至比我们这版还快——它用 thread-local 句柄 + 单次原子 fetch_add 完成进入，连 fence 都推迟到第一次 retire 时。这种极致优化让 epoch 的读路径几乎零成本——这是它成为 crossbeam 默认回收策略的核心原因。

读者最难懂的点：**为什么 hazard 和 epoch 都需要 SeqCst fence？两者是同一个原因吗？** 同一个。它们都解决"读者公告 / 回收者扫描"之间的跨变量全序问题。差别是"公告内容"——hazard 公告指针、epoch 公告代次——但 fence 的作用是同构的：保证"读者写了公告"和"回收者读了公告"之间不存在"擦肩而过"的窗口。理解了这一节，你就理解了 crossbeam-epoch 那条 `fence(SeqCst)` 的真正含义。

> **这一节不只是"散文讲解"——`crates/forge-lockfree/src/epoch.rs` 已经从零把上面
> 这套骨架实现成可编译、可测试的真实代码**。`EpochGuard::new()` 进 pin、`Drop` 自动
> unpin、`defer_destroy(ptr, dtor)` 把指针塞进当前 epoch 的 garbage 袋，
> `try_advance()` 在所有活跃线程都已 ≥ 当前 epoch 时推进全局 epoch 并回收最老那袋。
> `epoch_gc_waits_for_unpin` 测试（`tests/m8_10_epoch.rs`）钉死了"pin 期间不回收、
> unpin 后 try_advance 触发回收"的核心时序。`rcu.rs` 的 Arc 版保留作为更简单、更 sound
> 的对照（教学版默认它）；要看"真 epoch 回收"，打开 `epoch.rs`。

> RCU 真正难点不是换指针，是回收。选哪种回收策略取决于工作负载：极少改 → 泄漏；偶尔改 → Arc；高频改、低延迟 → epoch；极端延迟敏感 → hazard pointer。每个选择都是一次工程权衡。

### 深一步：copy-update 为什么要 CAS 而不是 swap？

读者可能要问：上面 LOW-FI 里我们用 `swap`，但 WRITE 版（rcu.rs）用的是 Mutex + 直接赋值。差别在哪？

考虑两个写者并发改同一个 RCU。如果都用 `swap(new)`：

- W1 读 old，clone 出 c1，改 c1；
- W2 同时读 old，clone 出 c2，改 c2；
- W1 `swap(c1)`，head 现在指向 c1；
- W2 `swap(c2)`，head 现在指向 c2。

结果：W1 的修改**被 W2 覆盖丢失了**——c1 还活着但没人指它，它就是个孤儿。两个写者都以为自己的修改生效了，实际只剩一个。

要修这个 bug，并发写需要 **CAS**：W2 swap 之前先确认 head 还是它当初读到的 old——如果不是就重读重做。但 CAS 加上"先读 old 再 clone 再 CAS"这条路引入了"重试开销"。原书给出的 tip 简单粗暴：**用 mutex 串行化写路径**——读路径仍无写阻塞（mutex 只在写时持有），但避免并发写的复杂性。我们的 crate 就走这条路，于是 `update` 函数只是 `Mutex::lock` → clone 改 → 替换。

这个权衡体现了 RCU 的精髓：**读路径越快越好（无锁），写路径慢一点没关系**——因为本来写就少。如果你反过来：写多读少，那 RCU 是错误选择，应该用 `RwLock` 或原子类型。

### 用 Rust 借用规则保护 RCU

Rust 的 `Arc` 在这里特别合适：它提供了**自动回收**（最后一个 clone drop 时回收），且**完全 sound**（编译器保证不可能 use-after-free）。代价是每次 read 要 `fetch_add` 引用计数——但这是原子的、无锁的，比 mutex 快得多。这就是为什么 forge-lockfree 选 Arc 而不是 epoch——**sound 优先于极致性能**，教学版尤其如此。

真实工程（crossbeam-epoch、crossbeam_skiplist）走 epoch 路线是因为：① 引用计数有缓存争用（多核反复原子写同一个 `AtomicUsize`）；② epoch 用 thread-local，读路径零原子写——只在自己 CPU 上写自己的 epoch 槽。代价是回收延迟（两 epoch 窗口）、实现复杂度高。**没有银弹，永远是权衡**。

下一节我们撞上 RCU 的"小弟"——链表/栈——并第一次直面那个能烧光一切的敌人：**ABA**。

---

## 三、M8c Treiber 无锁栈 + ABA：CAS 看起来赢，其实被命运调包了

### ENEMY：ABA，无锁算法的终极反派

为什么 ABA 这么可怕？因为它**只在特定线程交错下才暴露**——单线程测试永远抓不到，集成测试跑一万次可能只失败一次，CI 上常绿、生产线上偶发 crash。Williams 在《C++ Concurrency in Action》第 7 章把 ABA 列为无锁数据结构的"头号公敌"，许多工业级无锁栈/队列论文的开篇都先讲 ABA——因为它**不是一个具体 bug，是一类 bug 的总称**。任何"基于 CAS 比较值"的算法，只要满足"旧值可能被回收且地址可能复用"，就有 ABA。这意味着：ABA 不是 Treiber 栈特有的，而是所有无锁 CAS 算法的共同敌人。理解了 ABA 你就能举一反三——后面看 MS 队列、看 HashMap、看任何无锁结构时，第一个问题就该是"它怎么解 ABA？"

考虑最简单的"无锁栈"：用 `AtomicPtr<Node>` 当 head。push：建节点，把 head 从 old CAS 到新节点。pop：读 head、读 head.next、把 head 从 old CAS 到 head.next。

这套结构叫 **Treiber 栈**，1986 年 R. Kent Treiber 发明，是历史上第一个被广泛使用的无锁算法。它看起来美：**没有锁、没有锁的 syscall、所有冲突都用 CAS 解决**。但藏着一个 bug——**ABA**——能让一个看似正确的 CAS 在另一线程脚下偷偷换走数据，造成 use-after-free。

我们先看代码，再**逐拍手算 ABA 怎么发生**，然后用三种办法（标记指针、epoch、hazard pointer）把它修好。

### ANCHOR：餐厅叫号机

锚一个画面：餐厅门口有一台叫号机，屏幕上显示**当前正在服务的号**。服务员 A 看一眼"现在叫到 5 号"，准备好"5 号叫完换成 6 号"的操作，**手伸到机器前**——这时另一个服务员 B 抢先操作，把号从 5 改成 6 又改成 7（5→6→5，号又回到 5 但中间已经经过了 6）——A 回头一看，屏幕还是"5"，于是放心地执行"5→6"。但屏幕上那个 5 **已经不是 A 当初看到的 5**——是 B 改回去的 5。A 的操作成功，但语义上**已经错**。

ABA 就是这个：**CAS 比较的是值（"是 5 吗？"），不是身份（"是不是我看到过的那个 5？"）**。中间只要有人把值改成 B 再改回 A，CAS 就被骗过。

### LOW-FI：先看朴素 Treiber 栈代码

`crates/forge-lockfree/src/stack.rs`：

```rust
struct Node<T> {
    value: ManuallyDrop<T>,
    next: *mut Node<T>,
}

pub struct Stack<T> {
    head: AtomicPtr<Node<T>>,
}

pub fn push(&self, value: T) {
    let node = Box::into_raw(Box::new(Node {
        value: ManuallyDrop::new(value),
        next: ptr::null_mut(),
    }));
    let mut old = self.head.load(Ordering::Relaxed);
    loop {
        unsafe { (*node).next = old };
        match self.head.compare_exchange_weak(
            old, node,
            Ordering::Release, Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => old = actual,
        }
    }
}

pub fn pop(&self) -> Option<T> {
    let mut old = self.head.load(Ordering::Acquire);
    loop {
        if old.is_null() { return None; }
        let next = unsafe { (*old).next };
        if self.head
            .compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            let value = unsafe { ManuallyDrop::take(&mut (*old).value) };
            // ⚠️ 故意不释放 old（规避 ABA；教学取舍，会泄漏）。
            return Some(value);
        }
        old = self.head.load(Ordering::Acquire);
    }
}
```

注意那条注释"故意不释放 old"——**这就是我们当前的 ABA 规避办法：永不回收节点**。代价是内存泄漏——程序跑得越久越胖。我们要在概念上证明为什么"一旦释放，立刻出 ABA"。

### 必选手算 1：Treiber 栈 ABA 逐拍重演

设栈里有两个节点 A（top）→ B。地址分别是 `A=0x1000`、`B=0x2000`。两线程 T1、T2。

```
初始：head = 0x1000（指向 A）
        A(0x1000).next = 0x2000
        B(0x2000).next = NULL
```

**T1 想要 pop**。它执行 pop 的第一行：`old = head.load()` 读到 `old = 0x1000`。第二行：`next = (*old).next` 读到 `next = 0x2000`。**T1 准备执行 CAS(head, 0x1000 → 0x2000)**——但还没执行就被抢占。

**T2 现在干了一连串事**：

1. T2 pop：head 从 0x1000 CAS 到 0x2000。**A 出栈，T2 拿走了 A 的值。**
2. T2 再 pop：head 从 0x2000 CAS 到 NULL。**B 出栈，T2 拿走了 B 的值。** 现在栈空。
3. T2 把 A 节点（地址 0x1000）**释放回分配器**。
4. T2 push 一个新值 X。分配器**复用了刚才释放的 0x1000 地址**——新节点恰好又落在 0x1000，它的 `.next` 在 T2 写入时为了配合当前栈空而被设为 NULL。

栈现在长这样：

```
head = 0x1000（T2 新分配的、装着 X 的节点，复用了 A 的地址）
        (0x1000).next = NULL
```

**T1 醒来**。它继续执行那条还没跑的 CAS：`compare_exchange(head, expected=0x1000, new=0x2000)`。head 当前**正是** 0x1000——CAS **成功**！head 被换成 0x2000。

可是 0x2000 是什么？**是 B 的地址，B 早就被 T2 pop 走、释放掉了**！栈头现在指向一个**已释放的、内容不明的内存**。下一次 pop 会读 `(*0x2000).next`——UB。如果分配器又把 0x2000 分给别人存别的东西，你读到的是别人写的随机字节，可能是任意指针，跳过去读 → 立刻 segfault 或更糟。

这就是 ABA。它的精髓：**T1 的 CAS 只看到"head 还是 0x1000"这个值，无法分辨"0x1000 还是 A"和"0x1000 已经是别人"**。值没变、身份已变。

### 用 loom 把它抓出来

光说不够。`loom` 是 Cargo 提到的并发模型检查器，能枚举所有可能的线程交错，让"只在特定拍子下才暴露"的 bug 在 x86 上也能复现。下面这段 loom 模型（**不在 crate 里跑**，因为 loom 模型和真实代码用的 Atomic 不同；这里只演示思想）能找到 ABA：

```rust
// 概念性 loom 模型，不在 crate 里编译
loom::model(move || {
    let s = Arc::new(loom::sync::atomic::AtomicPtr::<Node>::new(ptr::null_mut()));
    // ... 设置两线程：T1 pop 到 CAS 前一步、T2 干那一连串、T1 继续
    // 在 LOOM_MAX_PREEMPTIONS=4 时，loom 会枚举到 T1 的 CAS 在 T2 改回 0x1000 后才执行的那条路径。
    // 然后断言栈的不变量——会被违反。
});
```

实战经验：把允许的抢占次数（`LOOM_MAX_PREEMPTIONS`）调到 4–5，能在几秒内复现 ABA。这就是 loom 的威力——**把你以为的"理论 bug"变成一条可复现的失败测试**。

### WRITE：三种 ABA 解法

**解法 1：标记指针（tagged pointer）。** 64 位系统上指针只有低 48 位有效，高 16 位空闲。把"代次计数器"塞进高位：每次成功 CAS 把代次 +1。比较时不仅比指针、也比代次。这样"地址相同但代次不同"的 CAS 会失败，ABA 被堵住。

```rust
// 概念：
const PTR_MASK: usize = 0x0000_FFFF_FFFF_FFFF;
const TAG_SHIFT: usize = 48;

fn pack(ptr: *mut Node, tag: u16) -> usize {
    (tag as usize) << TAG_SHIFT | (ptr as usize & PTR_MASK)
}
fn unpack(packed: usize) -> (*mut Node, u16) {
    let ptr = (packed & PTR_MASK) as *mut Node;
    let tag = (packed >> TAG_SHIFT) as u16;
    (ptr, tag)
}
```

ABA 时地址回到 A，但代次已经从 0 涨到 1，CAS 比较完整 packed 值——失败。代价：代次 16 位，回绕到 65536 后理论上仍可能 ABA（实际工程里几乎不可能撞）。

**解法 2：epoch 回收（M8b 讲过的）。** 在"所有读者都已离开旧 epoch"之前**绝不回收** A 节点。T2 想释放 A？扔进垃圾袋，标上当前 epoch + 2。在 epoch 推进前，A 一直活着——T1 的 CAS 即便值巧合相同，读 `(*old).next` 也读到的是合法数据。直到所有当时在临界区的读者都退出了，才回收。**这是 crossbeam-epoch / crossbeam_queue 走的路。**

**解法 3：hazard pointer。** 每个线程在 thread-local 里维护一个"我正在用的指针"清单。T1 进 pop 时把 `old`（0x1000）写进自己的 hazard 槽；T2 想回收 A 前必须扫所有线程的 hazard 槽，发现 0x1000 在 T1 槽里——**不能回收**，等下次。这就堵死了"地址复用"。

```rust
// 简化：
static HAZARDS: [AtomicPtr<()>; NTHREADS] = [...]; // 每线程一个槽

fn pop(&self) -> Option<T> {
    loop {
        let old = self.head.load(Acquire);
        HAZARDS[my_slot].store(old as *mut (), Release);   // 标记"我正在用 old"
        fence(SeqCst);                                      // 与回收者的扫描互锁
        if self.head.load(Acquire) != old { continue; }    // 重读，防止 store 之前 head 已变
        // ... 此后回收者绝不会回收 old，安全做 CAS
        if self.head.compare_exchange_weak(old, ...).is_ok() {
            // ... 回收 old 之前要确认 old 不在任何 hazard 槽里
            return ...;
        }
    }
}
```

hazard pointer 的代价是每次读路径多两条原子写。优点是延迟极低、不需要全局 epoch 推进。

我们的 crate 当前选了**最朴素的第 4 种**：**永不回收**（pop 后节点泄漏）。这显然只能用于教学——真实工程必选上面三种之一。教程把这个抉择明明白白写进源码注释。

### ABA 在不同的数据结构里有不同的脸

ABA 不只在栈里出现。它出现在**所有依赖 CAS + 回收 + 地址复用**的算法里：

- **MS（Michael-Scott）无锁队列**：dequeue 时 CAS head → next，head 节点被回收、地址复用 → 同样 ABA。这是为什么 `crossbeam_queue::ArrayQueue` 用了固定 slot 数组、`SegQueue` 用了 epoch 回收。
- **无锁链表**：删除节点 A→B 中的 A 时，把前驱的 next 从 A CAS 到 B。如果 A 被回收、地址复用塞了新节点 C，前驱的 CAS 看到"还是 A"成功——结果前驱的 next 还指向 A（=C），链表断了。
- **链表迭代器**：某个线程持着指向节点 A 的迭代器，A 被回收、地址复用——迭代器解引用立刻 UB。

**统一规律**：只要满足"① CAS 比较值；② 旧值会被回收；③ 分配器可能复用地址"——ABA 就存在。**没有回收就没有 ABA**——这是为什么我们的 stack.rs 选了"泄漏"作为教学策略。

### hazard pointer 的完整实现（`hazard.rs`）

#### 敌人先行：pop 出来的指针，能不能立刻 free？

先把这一节真正的痛点装进身体里。回忆 Treiber 栈的 pop：

```rust
let old = self.head.load(Acquire);
let next = unsafe { (*old).next };
if self.head.compare_exchange_weak(old, next, ...).is_ok() {
    // CAS 成功：old 已被弹出栈。能不能立刻 drop(Box::from_raw(old)) ？
}
```

这条 CAS 成功意味着 old 这一刻**只有我一个线程**通过栈结构能到达它——所以拿走它合法。但"只有我能从栈到达它"不等于"只有我能读它"。考虑这一拍序列：

| 拍 | T1（pop） | T2（pop） |
|---|---|---|
| 1 | `old = head.load()` → 读到 X | |
| 2 | | `old = head.load()` → **也读到 X** |
| 3 | CAS 成功，X 出栈。T1 想 `free(X)` | |
| 4 | **T1 执行 `free(X)`** | |
| 5 | | `(*X).next` ← **X 已被释放，use-after-free** |

T2 在拍 2 把 X 读进了自己的寄存器，准备用它做 CAS。T1 在拍 4 把 X free 掉。T2 在拍 5 解引用 X 读 next——读到的是已被分配器改写的字节，可能是任意值，跳到野指针 segfault，更糟：分配器把这块地址又还给了别人存别的内容，T2 拿到的是别人写的数据，看起来"成功"但语义已错。

这就是 hazard pointer 要解决的敌人。**朴素回答"pop 后立刻 free"是错的**——必须等所有可能持有 X 指针的线程都放手了才能 free。问题：回收者怎么知道"谁还持有 X"？

#### LOW-FI 心智模型：图书馆公告栏

把整个机制压成一个画面。一家图书馆，每本书借出前读者必须做一件事——**在门口的公告栏上写一行**："我（读者 ID 7）正在读《指针 X》"。读完擦掉自己那行。

管理员要回收（下架）一本书时，必须扫一遍公告栏——只要公告栏上**有任何一行**写着这本书的名字，**就不能下架**。下架前管理员还要再贴一张通知"我要下架这些书，正在扫公告栏"，让正在贴公告的读者和扫描的管理员之间的操作**有先后**——这就是 fence 的作用。

精髓两条：

1. **读者公告**——"我在用哪个指针"。
2. **回收者扫描**——任何指针只要被任一线程公告，就推迟回收。

公告栏本身是**全局共享数组**，每个读者**只写自己的槽**（互不争用），回收者**只读所有槽**（一次扫描）。这就是为什么 hazard pointer 的读路径几乎不引起缓存争用。

#### 代码要点：与 `hazard.rs` 的真实实现对应

`crates/forge-lockfree/src/hazard.rs` 把公告栏做成全局静态结构：

```rust
const MAX_HAZARD_THREADS: usize = 128;

struct HazardRegistry {
    slots: [AtomicPtr<()>; MAX_HAZARD_THREADS],   // 128 个公告槽
    next_slot: AtomicUsize,                        // 下一个待认领的下标
}
static REGISTRY: HazardRegistry = HazardRegistry::new();
```

每个线程第一次用时 `ensure_slot` 调 `next_slot.fetch_add(1, Relaxed)` 认领一个下标，**终生持有**。thread_local 缓存下标本身，之后取槽零原子操作。

读者的三步公告在 `HazardGuard::protect`：

```rust
pub fn protect(ptr: *mut ()) -> Self {
    let slot = ensure_slot();
    REGISTRY.slots[slot].store(ptr, Ordering::Release);   // ① 公告"我在用 ptr"
    fence(Ordering::SeqCst);                                // ② 与回收者扫描互锁
    HazardGuard { slot }
}
```

注意 `protect` **只做了公告 + fence 这两步**——重读 head 确认 ptr 仍有效是调用者的责任（HazardStack::pop 里下一行的 `if self.head.load(Acquire) != old { continue; }`）。这条重读是 ABA 防护的真正位置，下面手算会画清它为什么必不可少。

回收者的扫描在 `scan_and_reclaim`：

```rust
let max = REGISTRY.next_slot.load(Ordering::Acquire);
let mut hazards = Vec::with_capacity(max);
for i in 0..max {
    let p = REGISTRY.slots[i].load(Ordering::Acquire);
    if !p.is_null() { hazards.push(p); }
}
hazards.sort_unstable_by(|a, b| a.cmp(b));
hazards.dedup();
fence(Ordering::SeqCst);                                  // 与读者端的 SeqCst fence 互锁
GARBAGE.with(|g| {
    for (ptr, dtor) in g.borrow_mut().drain(..) {
        if hazards.binary_search(&ptr).is_ok() { /* 还有人用，留到下次 */ }
        else { dtor(ptr); }                              // 安全回收
    }
});
```

排序 + 二分查找是为了在 128 个槽里快速判定一个指针是否被 hazard——回收是批量操作，每攒 32 个指针才扫一次（`SCAN_THRESHOLD = 32`），扫描成本被摊薄。

#### 必选手算：T1 pop 节点 X、T2 同时读 X 的 hazard 协议

设栈 `head → X → Y → NULL`，地址 `X=0x1000, Y=0x2000`。两线程并发 pop。

**拍 0（初始）**：

```
head = 0x1000 (X)
X.next = 0x2000 (Y)
T1.hazard = null, T2.hazard = null
T1 的垃圾袋 = [], T2 的垃圾袋 = []
```

**拍 1：T1 进入 pop**。它先建一个 HazardGuard（protect(null)），然后 `old = head.load(Acquire)` 读到 `old = 0x1000`。这一拍还没公告 X。

```
head = 0x1000
T1.old = 0x1000 (X)
T1.hazard = null   ← 还没写
```

**拍 2：T2 进入 pop**，也 `old = head.load(Acquire)` 读到 0x1000。

```
head = 0x1000
T2.old = 0x1000
T1.hazard = null, T2.hazard = null
```

**拍 3：T1 调 `guard.set(old)`**——把 X 写进自己的 hazard 槽，然后 fence(SeqCst)。

```
T1.hazard = 0x1000 (X)
[SeqCst fence 已发]
```

**拍 4：T1 重读 head 确认 X 仍在**——`if self.head.load(Acquire) != old { continue; }`。head 仍是 0x1000，重读通过。**只有通过重读才能继续**。

```
T1 验证 head == old == 0x1000 ✓
```

**拍 5：T2 调 `guard.set(old)`**，把 X 写进自己 hazard 槽，fence(SeqCst)。

```
T2.hazard = 0x1000 (X)
[SeqCst fence 已发]
```

**拍 6：T2 重读 head**，head 仍是 0x1000，通过。

**拍 7：T1 读 X.next = 0x2000，CAS head: 0x1000 → 0x2000 成功**。X 出栈。T1 取走 value。然后调 `retire(0x1000, destroy_node)`——把 X 塞进 T1 的垃圾袋（不立刻 free）。

```
head = 0x2000 (Y)
T1.garbage = [0x1000]
T2.hazard = 0x1000   ← T2 仍持有 X
```

**拍 8：假设此时垃圾袋长度触达阈值（教学版假设它达到 32）**，T1 调 `scan_and_reclaim`：

1. 扫所有槽，构建 hazards 集合 = { 0x1000 }（来自 T2 的槽）。
2. fence(SeqCst)。
3. 遍历垃圾袋 [0x1000]：binary_search(0x1000) **命中**——有人在用，**推迟回收**。

```
T1.garbage = [0x1000]   ← 留到下次
```

X **没被 free**。T2 仍可安全地用它做 CAS。

**拍 9：T2 读 X.next = 0x2000，CAS head: 0x1000 → 0x2000**——**失败**！head 当前是 0x2000，不是 0x1000。回到循环顶部重读 head=0x2000，重新走"set hazard(0x2000) → fence → 重读 → CAS"流程。最终 T2 pop 出 Y。

**拍 10：T2 的 guard drop**，清空自己 hazard 槽：

```
T2.hazard = null
```

**拍 11：T1 下次 `scan_and_reclaim`**——扫所有槽得到 hazards = {}（T2 已放手），binary_search(0x1000) 未命中，调 `destroy_node(0x1000)` → `Box::from_raw(0x1000)` 安全 drop。X 终于回收。

这就是 hazard 协议的完整时刻表。关键不变量：**只要 T2 在公告栏上写着 X，T1 的 scan 就一定看到 X**。这是被两条 SeqCst fence 保证的——T2 的 store + fence 与 T1 的 scan fence 之间有一个全序，二者不能"擦肩而过"。

#### 故意打破：如果删掉重读 head 那一步

musk.md 要求"打破再重建"。把上面拍 4 那条 `if self.head.load(Acquire) != old { continue; }` 删掉，会发生什么？

| 拍 | T1 | T2 |
|---|---|---|
| 1 | `old = head.load()` → 0x1000 | |
| 2 | | T3 把 X 出栈、retire、扫描时 T1 还没公告 X，**X 被 free** |
| 3 | `guard.set(0x1000)`：把 X 写进 hazard 槽 | |
| 4 | `(*0x1000).next` ← **use-after-free** |

T1 在拍 1 读到 X，还没来得及公告，X 就被 T3 回收了。T1 此时再公告已经太晚——X 早已不在。**重读 head 这一步就是堵这个缝隙**：公告完之后必须确认 head 仍是 X，才能保证"我在公告栏写的指针此刻仍有效"。这条重读是 hazard 协议正确性的脊柱，删掉它整个算法立刻 unsound。

#### 为什么要 SeqCst fence？Acquire/Release 不够吗？

考虑删掉 fence 改成 store(Release) + load(Acquire) 配对。读者 store 完指针后立刻读数据结构；回收者 load 完所有槽后立刻判定。问题：store 和 load 之间没有任何"屏障"——处理器可以把读者的 store 重排到它后续的读之后，或把回收者的 load 重排到它后续的 free 之后。于是出现"读者以为自己公告了，回收者却没看到"的窗口。

SeqCst fence 强制：**读者的 hazard-store 与回收者的 hazard-load 之间存在一个全局顺序**。要么读者先 store 后回收者 load（回收者一定看到），要么回收者先 load 后读者 store（读者重读 head 会发现指针已被换走、重试）。两者不能"重叠"。这就是 SeqCst 不可替代的位置——所有 Acquire/Release 都做不到这种跨变量的全序。

#### ISO·ZOOM：hazard vs epoch vs RCU-Arc 的三角权衡

到这一节末尾，我们见过了三种回收策略，它们在三个维度上各有取舍：

| 策略 | 读路径开销 | 回收延迟 | 内存峰值 |
|---|---|---|---|
| **hazard pointer** | 2 条原子写 + 1 条 SeqCst fence | 短（攒 32 个就扫） | 低（精确回收） |
| **epoch** | 1 条原子写（pin）+ 偶尔 fence | 长（等 2 个 epoch 推进） | 中（垃圾滞留 2 epoch） |
| **RCU + Arc** | 1 条原子 fetch_add（强引用计数） | 实时（最后一个放手就回收） | 最低 |
| **泄漏**（stack.rs） | 零 | 永不 | 无限增长 |

- **hazard**：读延迟最低（只动 thread-local 槽），但回收频率受扫描成本限制。适合"读极多、写极少、读延迟极敏感"——Linux 内核 RCU 的 userspace 替代品、高频读的 lock-free queue。
- **epoch**：读路径最便宜（一次 fetch_add），但回收有"2 epoch 宽限期"延迟。适合"读极多 + 写也较多、能容忍回收延迟"——crossbeam 的 Skiplist、SegQueue。
- **RCU + Arc**：实现最简单、最 sound，但 `fetch_add` 在所有读核之间争用同一缓存行——读核数高时性能崩塌。适合"读多、但读核数有限（数十以内）"。
- **泄漏**：教学专用，绝对 sound，但只能跑短时间。

读者最难懂的点：**为什么 hazard 的 fence 是 SeqCst，而 epoch 的 fence 也是 SeqCst——两者本质是不是一样？** 一样——它们都解决"读者公告 / 回收者扫描"之间的全序问题，差别只是"公告的内容"（hazard 公告具体指针、epoch 公告自己处在哪个代）和"扫描的频率"（hazard 每攒一批扫、epoch 每 epoch 推进时扫）。理解了一种，另一种就是平移。

### Michael-Scott 队列：完整实现（`queue.rs`）

我们的 crate 里 `queue.rs` 已经从零实现了 MS 队列——两个原子指针 `head` / `tail`，
初始都指向一个**哑节点**（dummy），节点用 `Option<T>` 表示（哑节点是 `None`）。
核心思路与论文一致：

- `enqueue(value)`：建节点 N（`Some(value)`），两步 CAS：
  1. CAS `tail.next: null → N`（Release，发布 N 的内容给 dequeue 的 Acquire）；
  2. CAS `tail: old → N`（推进 tail，允许失败——别人可能已替我推进；叫"helping"）。
  若发现 tail 落后于 tail.next（有人接上节点但还没推进 tail），路过线程顺手把 tail
  推到 tail.next。这保证 tail 长期落后不可能。
- `dequeue()`：读 head、读 tail、读 head.next；若 head==tail 且 head.next==null 则
  队列空（只剩哑节点）；否则 CAS `head: old → head.next`（AcqRel），CAS 成功后才
  `take()` 出新哑节点的 value 返回。旧 head（旧哑节点）的内存**故意不释放**
  （与 stack.rs 同样的教学取舍，规避 ABA）。

完整代码见 `crates/forge-lockfree/src/queue.rs`，测试 `tests/m8_08_ms_queue.rs`
覆盖 SPSC 顺序保持、MPMC 不丢不重、空队列语义。

#### 必选手算：MS 队列入队的两步 CAS 在并发 dequeue 下的交错

这是 ABA 之外 MS 队列独有的另一个微妙点。设队列里有一个真实节点 A，tail 指向 A，
A.next = null。两线程 T1（要 enqueue B）、T2（要 dequeue）。head 指向哑节点 D，
D.next = A。

```
拍 0：
  head → D(哑)，D.next = A
  tail → A，A.next = null
  T1 想把 B 接在 A 后；T2 想 dequeue。
```

```
拍 1：T1 执行 enqueue 第一步——load tail=A，load A.next=null，CAS A.next: null→B（成功）。
  A.next = B
  tail 还是 A（T1 还没执行第二步 CAS）
```

```
拍 2：T1 还没执行第二步 CAS（"推进 tail"），被抢占。
       T2 来 dequeue：load head=D，load tail=A，load D.next=A。
       head(D) != tail(A) → 不空。CAS head: D → A（成功）。
       T2 take A.value 返回。D（旧哑节点）成为新哑节点，但不回收（教学版）。
```

```
拍 3：T1 醒来，执行第二步 CAS tail: A → B（成功）。
  tail → B
  现在队列：head → A(新哑，value=None)，A.next = B；tail → B。
```

注意拍 2：T2 的 dequeue **没有**等 T1 把 tail 推进。它只看 `head != tail` 就判定
非空——这条判断不依赖 tail 是否被推进。所以"tail 落后于 tail.next"是 MS 队列的
合法中间态，dequeue 仍能正确推进。这是 MS 论文的关键洞察：**tail 的推进是一个
"best effort"操作**，慢一点没关系——任何路过线程都能 helping 推进。

如果 T1 在拍 1 之后挂很久呢？另一个 enqueue T3（想接 C）来时，T3 看到 tail=A、
A.next=B（非空）→ T3 走"helping"分支：CAS tail: A → B，把 tail 推到 B。
然后 T3 重读 tail=B、B.next=null，CAS B.next: null → C。所以队列永远不会卡在
"tail 落后"状态——这就是 helping 的威力。

MS 队列同样面临 ABA（dequeue CAS head 时地址复用）、同样需要回收策略。它是 Treiber
栈的"队列版"，理解了栈的 ABA 修复方案（hazard pointer / epoch，见 hazard.rs / epoch.rs），
队列的修复方案就是平移过来。

### ISO·ZOOM：内存序的取舍

回头审 stack.rs 的内存序。push 用 `Release`：发布新节点的内容（`.value` 和 `.next`）给 pop 的 Acquire。pop 第一次 load 用 Acquire、CAS 用 Relaxed——为什么 pop 的 CAS 不用 AcqRel？因为 pop 不会发布任何东西给下一个 pop 的读者（next 节点的内容是它自己 push 时已经 Release 发布过的，本线程的 CAS 只是把 head 指过去）。我们只要保证 pop 读到 `(*old).value` 之前先 Acquire 看到 head 的状态——这一点 load(Acquire) 已经做到了。**内存序不是越强越好，是越精确越好**——这条原则贯穿整章。

读者最难懂的点之一：**为什么 pop 后 `ManuallyDrop::take`？** 因为节点本身在泄漏（不被 drop），但节点里装的 `T` 是要交还给调用者的；`ManuallyDrop` 让我们能"取走 T 的所有权"而**不让 Box 在 drop 节点时顺手 drop T**——节点泄漏、T 顺利被取走。这是 Rust 里写无锁数据结构的标准手法。

下一节：把"等待者队列"从内核搬到用户态，造一个比 SpinLock 缓存友好的队列锁。

---

## 四、M8d MCS 队列锁：每个 spinner 只 ping 自己的缓存行

### ENEMY：SpinLock 的缓存 ping-pong

回忆 M3 SpinLock：N 个线程抢同一个 `AtomicBool`，未抢到的都 `load(Relaxed)` 死循环盯着它。这条死循环代价巨大——**所有 N 个核都在抢这同一个缓存行**。每核的缓存控制器都要"我也要这一行的最新值"，于是这一行在 N 个 L1 缓存之间**来回弹跳**（ping-pong），每次弹跳几十纳秒。N 越大越惨——这就是为什么 SpinLock 在高争用下性能崩塌。

**缓存 ping-pong 有多贵？** 直观感受一下：一次 L1 命中约 1ns，一次跨 socket 缓存协调（MESI 协议的 invalidate + transfer）约 40–100ns——差 40 到 100 倍。8 核抢一个锁字节，每秒能做几千万次无效的缓存协调，CPU 利用率拉满但实际进度极慢。这就是为什么"自旋锁在 4 核以上争用场景下不如睡眠锁"——睡眠锁让等待者真的睡、不抢缓存，自旋锁则把 CPU 烧在缓存协调上。

更糟糕的是**公平性**：SpinLock 不保证 FIFO，同一个核可能反复拿到锁（它就在自己 L1 里），其他核饿死。这就是为什么 Linux 内核的 spinlock 在高争用下会退化成"实际上睡眠的锁"。

敌人找到了：**所有 spinner 抢同一个字节**。

### ANCHOR：排队取餐

锚个画面：餐厅取餐。一种办法（SpinLock 模式）是所有人在出餐口挤成一堆，谁够得着谁拿——混乱且互相挤。另一种办法（MCS 模式）是**发号排队**：每个人拿到一个号，**每个人手里的号上有一个小灯**，前面那个号取完餐会**拍一下你手里的灯**让你去取——每个人都只盯**自己手里**的灯，不跟别人挤。

精髓：**每个 spinner 盯自己的变量，不抢同一个变量**。这就是 MCS（Mellor-Crummey & Scott, 1991）队列锁。

### LOW-FI：结构骨架

每个等待线程在**自己的栈上**（实际工程常放堆）放一个节点。锁本身只有一个 `AtomicPtr<Node>` 指向队列尾。每个节点里有：

- `granted: AtomicBool` —— 前驱是否已把锁交给我。false = 还在等。
- `next: AtomicPtr<Node>` —— 后继节点（前驱解锁时填）。
- `thread: Thread` —— 我自己（前驱解锁时 unpark 我）。

`crates/forge-lockfree/src/mcs.rs`：

```rust
pub struct McsLock {
    tail: AtomicPtr<Node>,
}
struct Node {
    granted: AtomicBool,
    next: AtomicPtr<Node>,
    thread: Thread,
}
```

### 必选手算 4：MCS 队列锁交接逐拍（4 线程）

设 4 个线程 T1/T2/T3/T4，各在自己的节点 N1/N2/N3/N4 上排队。每个 `granted` 初值 false。锁 `tail = NULL`。

**拍 0（初始）**：

```
tail = NULL
N1.granted=F  N1.next=NULL
N2.granted=F  N2.next=NULL
N3.granted=F  N3.next=NULL
N4.granted=F  N4.next=NULL
```

**拍 1：T1 调用 lock**。它把 N1 swap 接到队尾：`tail.swap(N1, AcqRel)`。换出的旧值是 NULL——说明前面没人，**T1 直接拿到锁**，不需要等。

```
tail = N1
N1.granted=F（但 T1 不查它，因为没前驱就跳过了等待循环）
T1 持锁
```

**拍 2：T2 调用 lock**。`tail.swap(N2, AcqRel)` 换出 N1。N1 不是 NULL——有前驱。T2 执行：

```
(*N1).next.store(N2, Release)    // 告诉 T1：你是我的前驱，我是后继
while !N2.granted.load(Acquire) { thread::park(); }  // 睡在自己
```

```
tail = N2
N1.next = N2
T2 睡在 N2.granted=F 上
```

**拍 3：T3 调用 lock**。

```
tail = N3
N2.next = N3
T3 睡在 N3.granted=F 上
```

**拍 4：T4 调用 lock**。

```
tail = N4
N3.next = N4
T4 睡在 N4.granted=F 上
```

队列形如：`tail → N4 → N3 → N2 → N1（持锁）`。**每个 spinner 只盯自己节点的 granted 字段**——没有任何核抢同一个缓存行！这是 MCS 相比 SpinLock 的核心收益。

**拍 5：T1 unlock（drop guard）**。看自己 `next`：

- T1 读 `N1.next`：是 N2（非 NULL），有后继。
- T1 不需要 CAS tail——因为 tail 此时是 N4，不是 N1。直接把后继唤醒：
  ```
  N2.granted.store(true, Release)
  N2.thread.unpark()
  ```

```
N2.granted = T   ← T2 的 park 返回，跳出 while，拿到锁
T1 已 drop（同时 free N1）
```

**拍 6：T2 持锁，工作，unlock**。同理：N2.next = N3，唤醒 T3。

```
N3.granted = T
T3 持锁
```

依次类推到 T4。FIFO 公平、每个 spinner 不抢同一个字节、锁本身只占一个 `AtomicPtr`（指针大小）。

**corner case**：如果 T1 unlock 时 N1.next 是 NULL（没有后继），它要 CAS `tail: N1 → NULL`。CAS 可能失败——失败说明就在这一刻有人正 swap 进来当我的后继、但还没来得及把 next 写好。这时 T1 要**等 next 写好**：

```rust
let mut next = self.node.next.load(Acquire);
if next.is_null() {
    if self.tail.compare_exchange(self.node, NULL, Release, Relaxed).is_ok() {
        // 真没后继，回收节点走人
        return;
    }
    // CAS 失败：有人正接上来，等他写好 next
    while next.is_null() {
        next = self.node.next.load(Acquire);
    }
}
// 有后继，交接
(*next).granted.store(true, Release);
(*next).thread.unpark();
```

这就是 mcs.rs 的 Drop 实现。逐行对照就能理解每个分支为什么必要。

### ISO·ZOOM：为什么这是"队列锁"的精髓

对比一下 SpinLock 的行为：N 个核都盯着同一个 `lock` 字节，那个字节每解锁一次就在 N 个核之间弹一次。MCS 让"等待"这一行为发生在**每个 spinner 自己的缓存行**上——锁字节本身在被持有时**不变化**（持锁者不写它），所以不引起任何缓存流量。解锁时**只写一次**——写到后继节点的 granted 字段，那个字段本来就在后继核的 L1 里（它一直 spin 在那里读），写入后直接命中后继核的缓存——一次定向写，没有 ping-pong。

代价：每次 lock/unlock 都要分配一个 Node（Box）。可以优化成 thread-local 复用——Mara Bos 第 10 章提到的"元素可以不是堆分配，而是等待线程的局部变量"。

我们的 mcs.rs 还把每线程的 `thread::current()` 存进节点，前驱用 `unpark` 唤醒后继——这把 MCS 的"等自己的变量"和 parking 的"睡得省 CPU"两者优点合一。Windows SRW 锁就是这套模式。

测试 `tests/m8_04_mcs.rs`：8 线程各加 1000 → 计数器最终 8000，验证互斥成立。FIFO 公平性测试留给读者（提示：用时间戳记录每线程获锁顺序，断言顺序单调）。

### 深一步：MCS vs CLH，一对孪生兄弟

MCS 不是唯一的队列锁。它的"前身"是 CLH（Craig, Landin, Hagersten, 1993）队列锁。两者差别在一个微妙的地方：

- **CLH**：每个节点 spin 在**前驱节点**的 `granted` 字段上。锁本身只需要一个 tail 指针。
- **MCS**：每个节点 spin 在**自己节点**的 `granted` 字段上。前驱解锁时主动写后继的 `granted`。

差别看似小事，实则在 NUMA（非一致内存访问）机器上影响巨大：

- CLH 的 spinner 反复读前驱节点的缓存行——前驱可能在另一个 NUMA 节点上，每次读要跨 NUMA 走慢线。
- MCS 的 spinner 只读自己节点的缓存行——它一定在自己 NUMA 节点的 L1 里，最快。

所以 MCS 在 NUMA 上比 CLH 快。代价是 MCS 的 unlock 稍复杂（要写后继节点的字段、可能要等 next 写好）。现代多 socket 服务器基本都选 MCS 变种。Williams 在《C++ Concurrency in Action》第 5 章详细对比了这两种队列锁。

### 一个工程细节：节点的存储位置

mcs.rs 注释里写"实际工程常放堆"——为什么不是栈？看 lock 函数：

```rust
let node = Box::into_raw(Box::new(Node { ... }));
```

我们用 `Box::into_raw` 把节点放堆上。理论上更高效的做法是放栈上：节点是 lock 函数的局部变量，函数返回（unlock）时自动回收，免一次 allocation。但 Rust 的借用检查器会让"返回一个指向栈上节点的 guard"变得难写——guard 的生命周期不能超过持有它的栈帧，而 lock guard 通常要被 return 出函数。**这是 Rust 写队列锁的一个常见痛点**。

真实工程（crossbeam-utils 的 `HardwareCell`、`parking_lot` 的内部）有几种解法：thread-local 节点池、pin 节点直到 guard drop、或者干脆用 unsafe 绕过借用检查。教学版用 Box 最直白、最 sound——读者可以据此理解为什么生产代码会做更复杂的优化。

### 一个细思极恐的细节：granted 的 Release/Acquire

unlock 写 `(*next).granted.store(true, Release)`，后继的 lock 在 `while !granted.load(Acquire)`。这对 Acquire/Release 配对建立了 happens-before：**前驱解锁前对共享数据的所有写，对后继获锁后可见**。如果漏掉这对（比如两边都用 Relaxed），后继可能看到锁保护的共享数据是旧值——互斥语义还在（granted 还是从 F 到 T），但**临界区的内容没同步过去**——典型的"锁住了但数据是脏的"bug。**锁的内存序错误往往不会让锁失灵，而是让"锁保护的数据"撕裂**——这是最难调的并发 bug 之一。

读者最难懂的 3 处之一就在这里：**"CAS 失败后等 next 写好"那段自旋为什么不阻塞？** 答案：后继线程 swap 进来之后**马上**就会 store next（它的下一行代码），这个 store 在几纳秒内必然发生。我们只需极短自旋——比 park/unpark 的成本（一次 syscall）低得多。

### 一个更阴险的细节：grant 之后读 `(*next).thread` 是 use-after-free（miri 抓到的真实 bug）

这条是本课程里最值得讲的 bug 之一——它不是我们故意留的"教学 bug"，而是**真实写在 `mcs.rs` 里、被 miri 抓出来的**。讲清楚它，你就理解了为什么"原子操作的内存序"和"对象的生命周期"是两回事。

最初的 unlock 交接路径长这样（**有 bug**）：

```rust
unsafe {
    (*next).granted.store(true, Ordering::Release);  // ① 把锁给后继
    (*next).thread.unpark();                          // ② 读后继的 Thread 句柄并唤醒
    drop(Box::from_raw(self.node));                   // ③ free 自己的节点
}
```

看起来无懈可击：① 用 Release 发布"锁给你了"，② 唤醒后继，③ 回收自己的节点。可 miri 在 8 线程压测下报了 **Undefined Behavior: data race between non-atomic read on thread A and retag write of type Node on thread B**。问题出在 ① 和 ② **之间**。

**逐拍看这个 UAF（手算）**：T1（持锁，节点 N）unlock，后继是 T2（节点 M）。

| 拍 | 动作 | M 状态 | 说明 |
|---|---|---|---|
| 1 | T1：`(*M).granted.store(true, Release)` | granted=true | T2 **此刻可以被唤醒** |
| 2 | T2：被调度上 CPU，`granted.load(Acquire)`→true，`lock()` 返回 | T2 持锁 | T2 进临界区 |
| 3 | T2：干完活，`_g` drop → T2 的 unlock | M 的 next（若有后继 T3）→ granted T3 | T2 handoff 给 T3 |
| 4 | T2：`drop(Box::from_raw(M))` | **M 被 free** | T2 回收自己的节点 M |
| 5 | T1：`(*M).thread.unpark()` | **读已 free 的 M** | 🔥 **use-after-free** |

T1 的 ① 和 ② 之间，T2 完整地走了一遍"醒→拿锁→干活→unlock→free 自己的节点 M"。等到 T1 执行 ② 读 `(*M).thread` 时，M 已经被 T2 释放了——读的是野指针。

为什么这道内存序拦不住？granted 的 Release/Acquire 配对保证了"锁保护的共享数据同步过去"，但**它管不了 M 本身的生命周期**。M 的 free 是 T2 在自己的 unlock 里做的，跟 T1 的 ① 没有 happens-before 关系（T1 的 ① 是"通知 T2 可以走了"，恰恰是**催促 T2 去 free M** 的信号）。内存序越对，T2 醒得越快，race 越容易触发——讽刺吧。

**修复**：在 grant **之前**把后继的 Thread 句柄 **clone** 出来；grant 之后就只对 clone 来的句柄 unpark，**不再解引用 `next`**。看 `crates/forge-lockfree/src/mcs.rs` 的真实代码：

```rust
unsafe {
    let successor_thread = (*next).thread.clone();    // ①' 先 clone（M 此时还活着：T2 还 park 着）
    (*next).granted.store(true, Ordering::Release);  // ②' grant——T2 此刻才能醒
    successor_thread.unpark();                         // ③' 用 clone 来的句柄唤醒，绝不碰 M
    drop(Box::from_raw(self.node));                    // ④' free 自己的节点 N（不是 M）
}
```

为什么 ①′ 读 `(*next).thread` 安全？因为此刻 T2 还 **park 在 `lock()` 里**（它的 `while !granted.load()` 还没看到 true），T2 持有自己的 `node: *mut Node M`，M 不会被 free（T2 的 `lock()` 栈帧还在，M 至少活到 T2 的 unlock）。clone 出来的 `Thread` 是 `Arc` 引用——unpark 用它，跟 M 的生死彻底解耦。

修复后 miri 干净通过（8 线程 × 1000 轮，38 秒，0 error）；之前的 `#[ignore]` 测试 `mcs_lock_contended_under_miri` 已恢复，转绿。

> **教训**（这条值得记住）：原子操作的 Ordering 解决的是"**可见性**"和"**顺序**"，不解决"**对象何时被释放**"。后者是**生命周期**问题，得靠"先拿住引用/句柄、再触发对方可能释放它"这个通用模式来兜底——和 M4 里 Arc 末次 drop 加 Acquire fence 是同一类思路：**在"可能导致对方动手"的那一步之前，把自己需要的东西攥到手里**。

---

## 四·补、M8d' CLH 队列锁：把"交接"做成一行 store

### ENEMY：和 MCS 同一个敌人，但 MCS 的 unlock 还要"找后继"

上一节 MCS 解决了 SpinLock 缓存 ping-pong 的敌人——每个 spinner 只盯自己节点的 granted 字段。但 MCS 的 unlock 路径藏着一个复杂度：解锁前要 `next.load(Acquire)` 找自己的后继节点，**如果没有后继**还要 CAS `tail: myself → NULL`，CAS 失败说明有人正接上来但还没写 next——必须自旋等 next 写好（见上一节最后那段 corner case）。这条 corner case 是 MCS 实现里最容易写错的地方。

能不能把 unlock **简化成一行 store**？这就是 CLH（Craig, Landin, Hagersten, 1993）队列锁的出发点。它的核心洞察：**让 spinner 盯前驱的节点而不是自己的节点**——这样解锁时不需要"找后继"，因为后继**本来就在盯我的节点**，我只要把自己节点的状态翻转，后继立刻看见。

CLH 的命名来自三位作者 Craig、Landin、Hagersten 的姓氏首字母。它比 MCS 早 2 年（1993 vs 1991，注意 MCS 论文实际发表于 1991 但常被引用为 1991 年 QEST 研讨会）。两种锁在学术上并称"队列锁双璧"，工业上 MCS 因 NUMA 友好而更常用，CLH 因实现简洁而出现在大量教学和早期 Linux 内核 spinlock 实现里。

### LOW-FI 心智模型：接力棒

把 CLH 压成一个画面。一场接力赛，跑道上一排运动员排队等候。每人手上都没有棒——棒在跑的人手里。规则是：

- **当前跑的人**（持锁者）拿着棒（= 他的节点的 locked=true）。
- **下一个等的人**死死盯着**前一个人**的手（= spin 在前驱节点的 locked 字段上）。
- 当前跑的人跑完时，**把棒递给后继**——但"递"这个动作在 CLH 里不是真的传递棒子，而是**自己放手**（= 把自己节点的 locked 置为 false）。后继一直盯着这只手，看到手张开就立刻上场。
- 上场时后继拿到的是**前驱的节点**——这个节点现在归他管了，他在它上面 spin（其实它已经被前驱设为 false，他立刻通过），然后持锁工作。

精髓：**棒（节点）在线程间传递，不是 spin 在自己的节点上**。每个 spinner 盯的是**前驱给他的那个节点**——解锁就是把"自己当时拿到的那个节点"翻转状态。不需要找后继，因为后继正在盯**我现在持有的节点**。

这条"节点跨线程传递"是 CLH 与 MCS 的根本差别。MCS 节点不传递——每人一个，前驱解锁时主动写后继节点的 granted；CLH 节点传递——前驱的节点会被后继接管，后继成为新的"持棒者"。

### 代码要点：与 `clh.rs` 的真实实现对应

`crates/forge-lockfree/src/clh.rs` 的数据结构极简：

```rust
pub struct ClhLock {
    tail: AtomicPtr<Node>,   // 队列尾
}

struct Node {
    locked: AtomicBool,       // 唯一的状态位：true = 想锁/持有；false = 已放手
}
```

注意：**节点里没有 next 指针**！这是 CLH 比 MCS 简洁的核心——不需要后继指针，因为后继是"从前驱手里接过节点的人"，他持有着前驱的节点，自然知道前驱是谁。

lock 路径：

```rust
pub fn lock(&self) -> ClhGuard<'_> {
    let node = Box::into_raw(Box::new(Node {
        locked: AtomicBool::new(true),     // "我要锁"
    }));
    let predecessor = self.tail.swap(node, Ordering::AcqRel);
    if !predecessor.is_null() {
        while unsafe { (*predecessor).locked.load(Ordering::Acquire) } {
            std::hint::spin_loop();
        }
    }
    ClhGuard { _lock: self, my_node: node, predecessor }
}
```

`tail.swap(node, AcqRel)` 一行完成了"接上队列 + 拿到前驱节点"两件事。swap 返回的是被换出的旧 tail——那就是前驱节点。如果旧 tail 是 null，说明队列原本空，我是队首，直接持锁、跳过 spin。

unlock 路径（在 ClhGuard::drop 里）：

```rust
impl Drop for ClhGuard<'_> {
    fn drop(&mut self) {
        unsafe { (*self.my_node).locked.store(false, Ordering::Release) };   // ← 解锁的全部！
        if !self.predecessor.is_null() {
            unsafe { drop(Box::from_raw(self.predecessor)) };   // 回收前驱节点
        }
        // 注意：my_node 不在这里回收——后继正在 spin 它
    }
}
```

**一行 store 就完成解锁**——这是 CLH 相比 MCS 的核心简洁点。没有"找后继"、没有"CAS tail → NULL"、没有"等 next 写好"的 corner case。后继一直在 spin 我的 my_node.locked，我把它翻 false，后继下次 load 立刻看到，跳出循环。

### 必选手算：T1/T2/T3 三线程排队的逐拍时刻表

设 ClhLock 初始 `tail = NULL`。三线程 T1、T2、T3 依次 lock。每线程的节点地址分别是 N1=0x1000、N2=0x2000、N3=0x3000。

**拍 0（初始）**：

```
tail = NULL
T1.predecessor = ?, T1.my_node = ?
T2.predecessor = ?, T2.my_node = ?
T3.predecessor = ?, T3.my_node = ?
N1.locked = ? (未分配), N2.locked = ?, N3.locked = ?
```

**拍 1：T1 调 lock**。`node = Box::into_raw(N1{locked=true})`。`predecessor = tail.swap(N1, AcqRel)`——swap 返回 NULL。predecessor 是 null，跳过 spin。T1 直接持锁。

```
tail = 0x1000 (N1)
N1.locked = true
T1.predecessor = NULL
T1.my_node = 0x1000 (N1)
T1 持锁，工作
```

**拍 2：T2 调 lock**。`node = N2{locked=true}`。`predecessor = tail.swap(N2, AcqRel)`——swap 返回 N1=0x1000。predecessor 非 null，**T2 spin 在 (*N1).locked 上**。

```
tail = 0x2000 (N2)
N1.locked = true   ← T2 在 spin 它
N2.locked = true
T2.predecessor = 0x1000 (N1)
T2.my_node = 0x2000 (N2)
T2 状态：while N1.locked == true, spin
```

**拍 3：T3 调 lock**。`node = N3{locked=true}`。`predecessor = tail.swap(N3, AcqRel)`——swap 返回 N2=0x2000。T3 spin 在 (*N2).locked 上。

```
tail = 0x3000 (N3)
N1.locked = true   ← T2 在 spin
N2.locked = true   ← T3 在 spin
N3.locked = true
T2.predecessor = N1, T3.predecessor = N2
```

队列形如：`tail → N3(T3) → N2(T2) → N1(T1, 持锁)`。每个 spinner 盯的是**前驱线程的节点**——T2 盯 T1 的 N1、T3 盯 T2 的 N2。

**拍 4：T1 unlock（drop guard）**。

```rust
(*N1).locked.store(false, Release);   // ← 解锁的全部
// N1.predecessor 是 NULL，跳过 Box::from_raw
// N1（my_node）不回收——T2 正在 spin 它
```

```
tail = 0x3000 (N3)
N1.locked = false   ← T2 的 spin 看到这个，跳出循环
T1 已 drop，但 N1 仍活着（T2 接管）
```

**拍 5：T2 的 spin 循环 load 到 N1.locked = false**，跳出循环。T2 持锁。

```
T2 持锁，工作
T2.my_node = N2, T2.predecessor = N1（接管 N1 的所有权）
```

注意：**T2 现在拥有 N1**——它在自己 guard 的 predecessor 字段里。N1 的状态是 false（被 T1 翻过）。T2 此后做啥都不影响 N1——N1 是它的"前驱节点"。

**拍 6：T2 unlock**。

```rust
(*N2).locked.store(false, Release);   // T3 在 spin N2，看到 false 跳出
// T2.predecessor = N1，回收 N1：
drop(Box::from_raw(0x1000));   // N1 此时无人 spin（T1 已走、T2 不再 spin N1），安全
// T2.my_node = N2 不回收——T3 正在 spin 它
```

```
tail = 0x3000 (N3)
N2.locked = false   ← T3 跳出 spin
N1 已被 Box::from_raw 回收
```

**拍 7：T3 持锁、工作**。

```
T3.my_node = N3, T3.predecessor = N2（接管 N2 所有权）
```

**拍 8：T3 unlock**。

```rust
(*N3).locked.store(false, Release);
// T3.predecessor = N2，回收 N2：
drop(Box::from_raw(0x2000));
// T3.my_node = N3——但 T3 是最后一个，没有后继来接管它
// N3 泄漏！
```

```
tail = 0x3000 (N3)   ← tail 仍指向 N3，没人来换它
N3.locked = false，但 N3 永远活着
```

这就是 clh.rs 顶层注释提到的"最后一个线程的节点会泄漏"——教学取舍。生产实现用 thread-local 节点池解决：每个线程把"自己用过的前驱节点"放进池子，下次 lock 时复用。

### CLH vs MCS：节点所有权的根本差别

逐项对比两者的差别，**每一项差别都源自"节点是否在线程间传递"**：

| 维度 | MCS | CLH |
|---|---|---|
| spin 目标 | 自己节点的 granted | 前驱节点的 locked |
| 节点所有权 | 每人一个，前驱 unlock 时主动写后继的 granted | 前驱的节点被后继接管，后继成为新持有人 |
| unlock 复杂度 | 中（找 next / CAS tail / 等 next 写好） | **低（一行 store + 回收前驱）** |
| 节点字段 | granted + next + thread（三个字段） | locked（一个字段） |
| NUMA 表现 | **好**（spin 在自己节点，必然在自己 NUMA 节点的 L1） | 差（spin 在前驱节点，前驱可能在另一 socket） |
| 内存回收 | 简单（每人 drop 自己节点） | 复杂（节点跨线程，最后那个会泄漏） |

NUMA 表现差别是工程取舍的核心。CLH 的 spinner 反复读**前驱线程**节点的缓存行——前驱可能在另一个 CPU socket 上，每次 load 要走 cross-socket interconnect，比读自己 L1 慢一个数量级（约 40-100ns vs 1ns）。MCS 的 spinner 读自己节点——一定在自己 L1 里。所以在多 socket 服务器上 MCS 比 CLH 快得多，现代生产 spinlock 几乎都选 MCS 变种（Linux 内核的 `osq_lock` 就是 MCS）。

但 CLH 也有它适合的场景：**单 socket 多核**（NUMA 罚款不存在）、**实现简洁性优先**（教学、嵌入式）、** unlock 必须极短**（实时系统）。Williams 在《C++ Concurrency in Action》第 5 章详细讨论这种取舍。

### 节点内存的"线程间迁移"

CLH 最反直觉的点是节点所有权。普通 spinlock 的状态位在锁本身上，CLH 的状态位在**节点**上——而节点会从前驱迁移到后继。具体追踪 N1（T1 的节点）的生命周期：

| 拍 | N1 的位置 | N1 的状态 | 谁负责 N1 |
|---|---|---|---|
| 1 | T1 创建并 swap 进 tail | locked=true | T1（持锁） |
| 2 | T1 持有；T2 把它换出 tail 作为 predecessor | locked=true | T1（持锁）；T2 引用它 spin |
| 4 | T1 解锁，N1.locked=false | locked=false | T1 已放手，但 T2 还在引用 |
| 5 | T2 接管 N1，作为它的 predecessor | locked=false | T2 持有引用 |
| 6 | T2 解锁时回收 N1 | 已 drop | 无人持有 |

注意拍 4-5：T1 已经 unlock 走人，但**不**回收 N1——因为 T2 还在 spin 它。T1 把 N1 的所有权"委托"给 T2（通过 ClhGuard 的 predecessor 字段）。T2 在自己 unlock 时才回收 N1。这种"前驱的节点由后继负责回收"是 CLH 的精髓——也解释了为什么节点不能放在栈上（栈上节点的所有权不能跨函数传递），必须堆分配。

读者最难懂的点：**为什么 T2 在拍 5 拿到 N1 后，自己 unlock 时是回收 N1 而不是 N2？** 因为 N2 是 T2 自己的 my_node——它**仍然活着**，被 T3 spin。T2 此时拥有的"可回收"节点是 N1（前驱），N1 在拍 4 被 T1 翻成 false 后**就再没人 spin 它了**（T2 在拍 5 已通过 spin，不再 spin N1）。所以 N1 此时无人引用，可回收。**回收的目标永远是 predecessor，不是 my_node**——这是 CLH 的内存管理铁律。

---

## 五、M8e parking-lot 式锁：把锁压缩到 1 字节

### ENEMY：锁本身占太多空间

你有一个 HashMap，里面一百万个 entry，每个 entry 想要自己的锁做细粒度同步。每把 std::Mutex 至少占 8 字节（许多实现是 32 字节以上），一百万个 entry 就要几十 MB——光锁本身就把内存吃光。

更狠的需求：你想给 JavaScript 对象（WebKit 的 V8 / JavaScriptCore）每个对象一把锁。一个 JS 对象可能只有 16 字节，加 40 字节的锁直接翻倍——WebKit 工程师们不能接受。

### ANCHOR：把"等待队列"搬到全局停车场

灵感：**锁本身只存"锁住没"+"有没有人在等"两个 bit**，等待者真正的队列塞进**一个全局 HashMap**，键是锁的内存地址。读者大概能想到画面：城市里有几个大停车场（每个 CPU 一个或全局一个），每辆车（等待线程）进去登记"我在等地址 0xABCD 那把锁"。锁本身只挂一个"有人在停车场等我吗"的小灯。

这就叫 **parking lot 模式**：锁极小（1 字节够用），等待者全局集中。WebKit 2015 年发明，`parking_lot` crate 沿用。**附带好处：在没有 futex 的平台上，这套全局 HashMap 自己就实现了 futex**——任意 atomic 地址都能 wait/wake。

### LOW-FI：1 字节锁 + 全局 HashMap

`crates/forge-lockfree/src/parking_lot.rs`：

```rust
const UNLOCKED: u8 = 0;
const LOCKED: u8 = 1;
// 注：原书和真实 parking_lot 还用了一个 "has_queue" 位，
// 我们教学版只用一位（locked），等待者有无由 HashMap 是否有该地址条目隐式表达。

fn global_lot() -> &'static Mutex<HashMap<usize, Vec<Thread>>> {
    static LOT: OnceLock<Mutex<HashMap<usize, Vec<Thread>>>> = OnceLock::new();
    LOT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct ParkingLotMutex {
    state: AtomicU8,
}
```

### WRITE：完整 lock/unlock

```rust
impl ParkingLotMutex {
    pub fn lock(&self) {
        // 快速路径：UNLOCKED→LOCKED。
        if self.state
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_ok()
        { return; }
        self.lock_slow();
    }

    #[cold]
    fn lock_slow(&self) {
        let addr = self.addr();
        let me = thread::current();
        loop {
            // 每轮先试快速路径
            if self.state
                .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
                .is_ok()
            { return; }
            // ① 先把自己登记进停车场
            {
                let mut lot = global_lot().lock().unwrap();
                lot.entry(addr).or_default().push(me.clone());
            }
            // ② 二次检查：若已解锁，摘掉自己、回去重抢（避免错过解锁）
            if self.state.load(Acquire) == UNLOCKED {
                let mut lot = global_lot().lock().unwrap();
                if let Some(q) = lot.get_mut(&addr) {
                    q.retain(|t| t.id() != me.id());
                    if q.is_empty() { lot.remove(&addr); }
                }
                continue;
            }
            // ③ 睡。park 的"unpark 不丢失"兜住"登记后、park 前"被唤醒的情形。
            thread::park();
        }
    }

    pub fn unlock(&self) {
        self.state.store(UNLOCKED, Release);
        let woken = {
            let mut lot = global_lot().lock().unwrap();
            lot.get_mut(&self.addr()).and_then(|q| q.pop())
        };
        if let Some(t) = woken { t.unpark(); }
    }
}
```

### ISO·ZOOM：为什么必须"先登记、再二次检查"

新人最容易把"先登记、再二次检查"这两步反过来或省掉，立刻 bug。考虑只做"检查 → 登记 → park"顺序会发生什么：

- T1 `state.load()` = LOCKED，决定要去 park。
- T2 `unlock()`：`state.store(UNLOCKED)`，查停车场——**空**，没人唤醒。
- T1 登记进停车场，`state.load()` = UNLOCKED……哎，如果它在这一步再检查一次似乎也行？

但再想：T1 第二次检查到 UNLOCKED，准备回去抢锁——这一刻 T3 又把锁抢走了。T1 重抢失败，回头要 park——可它**已经登记了**，怎么 park？park 会阻塞，直到 unlock 才醒。unlock 会查停车场——T1 在停车场——OK，能唤醒。所以这个顺序似乎也对。

那为什么标准做法仍是"先登记、再检查"？关键在于**唤醒不丢失**：T1 登记后，T2 解锁时**必然能在停车场里找到 T1**——不依赖 T1 自己再次检查的时序。如果反过来"先检查、再登记"，T1 检查到 LOCKED，准备登记——T2 解锁查空——T1 登记然后 park——**永远睡死**（T2 已经走人了）。所以**登记必须先于检查**，因为解锁者一定会查停车场——只要 T1 已登记，就一定能被叫醒。

`thread::park` 的"unpark token 不丢"机制兜住"登记完成 → park 调用"之间被唤醒的窗口：unpark 给一个 token，park 进来一看有 token 立刻返回不睡。这就闭环了。

测试 `tests/m8_06_parking_lot.rs` 验证互斥（多线程加锁累加）。

读者最难懂的点之二在这里：**这把锁怎么"小到 1 字节"？** 因为等待者不在锁里——锁只是个"有没有人在停车场等我"的提示，真正数据在全局 HashMap。真实 parking_lot crate 进一步把这一位塞进指针的空闲位——例如 `Arc::into_raw` 出来的指针低 2 位总是 0（对齐），那两位就能当锁用，**锁本身零开销**。这就是 WebKit 在 JavaScript 对象上做到的极致。

### 深一步：为什么这一节是 futex 的"用户态实现"

parking-lot 模式有个隐藏价值，原书也指出了：**在没有原生 futex 的平台上，这套全局 HashMap 自己就实现了 futex**。

回忆 M6/M7：futex 的核心能力是"在某个原子地址上 wait / wake"。Linux 有 `futex(2)` syscall 直接支持。但 macOS、Windows（老版本）没有等价的 syscall。怎么办？用 parking-lot 模式：

- 用全局 HashMap<地址, 等待队列> 替代内核的"地址 → 等待线程列表"。
- `wait(addr, expected)`：检查 `*addr == expected`，是则把自己 push 进 `HashMap[addr]`、`thread::park`。
- `wake_one(addr)`：从 `HashMap[addr]` pop 一个线程 `unpark`。

这就是 std 在不支持 futex 的平台上提供 `thread::park`/`unpark` 的办法，也是 `atomic-wait` crate 在跨平台时的实现路径。**parking-lot 不只是"小锁"，它是"用户态 futex"**——这两件事是同一个数据结构。

### 与 MCS 的关系

回看上一节 MCS：每个 spinner 睡在自己节点的 `granted` 上。parking-lot 是另一种睡觉方式——spinner 睡在全局 HashMap 里。两者都解决了"SpinLock 缓存 ping-pong"，但角度不同：

- MCS 把"谁在等"这条信息**分散**到每个等待者自己的节点上。优点：睡觉完全不进任何全局结构。缺点：锁本身要存一个 `AtomicPtr`（指针大小 8 字节）。
- parking-lot 把"谁在等"这条信息**集中**到一个全局 HashMap。优点：锁本身压到 1 字节甚至 0 字节。缺点：所有睡觉都过同一个 HashMap，HashMap 自己有锁（虽然争用极低）。

工程取舍：要"锁极小"（细粒度、海量锁）选 parking-lot；要"无全局结构"（嵌入式、内核）选 MCS。两者都是工业级答案。

---

## 六、M8f Sequence Lock：让读者完全不阻塞写者

### ENEMY：读者阻塞写者，写者阻塞读者

考虑 Linux 内核需要给每个进程提供"当前系统时间"。时间是几百字节的 `struct timespec`，频繁被进程读、偶尔被时钟中断更新。如果用 `RwLock`：每个读者要 `read()`——拿不到就阻塞；写者要 `write()`——有读者在就要等。**时间更新被读者的读延迟拖累，读者的读被写者阻塞**——内核不能接受这种延迟。

而且——这是 SeqLock 的真正动机——**读者完全不可信**。内核不信任读者进程（用户态），不能用 `RwLock`：恶意读者可以故意 `read()` 后永不放手，把整个时钟更新卡死。内核需要一个**读者完全无法阻塞写者**的机制。

### ANCHOR：版本号 + 双重比对

锚个画面：图书馆的目录册每次更新都印一个**版本号**，**奇数 = 正在更新中、偶数 = 稳定**。读者打开目录册时记下版本号 v1，慢慢翻完，记下结尾版本号 v2。如果 v1 = v2 且都是偶数——**你读到的是一致快照**。如果 v1 ≠ v2 或其中一个是奇数——目录在你眼皮底下被改了，重读一遍。

写者的纪律：开始更新前 `version += 1`（偶 → 奇），改完 `version += 1`（奇 → 偶）。任何读者看到奇数说明写者正在改，**重读**。

这就是 SeqLock（sequence lock），Linux 内核 2.6 起 用它做时间戳、网络统计计数器等"读极多写极少"的数据。

### 必选手算 3：SeqLock 读者重试逐拍

设 `seq = 0`（偶，稳定），数据是 `(a=10, b=10)`。读者 R、写者 W。

**拍 0（初始）**：

```
seq = 0（偶）
data = (10, 10)
```

**拍 1：R 想读**。它执行 read：

```
s1 = seq.load(Acquire)  → 0（偶）
读 data → snapshot = (10, 10)
```

**拍 2：就在 R 读 `seq.load(s2)` 之前，W 进来了**：

```
seq.fetch_add(1, AcqRel)   → seq 从 0 变 1（奇）
改 data.a = 11
改 data.b = 11
seq.fetch_add(1, Release)  → seq 从 1 变 2（偶，新值）
```

注意：W 改 data 时，R 已经读完了 snapshot = (10, 10)，但 R 还没做 s2 比对。

**拍 3：R 继续执行**：

```
s2 = seq.load(Acquire)  → 2
s1 (0) != s2 (2)  → 重试！
```

R 重试：

```
s1' = seq.load(Acquire) → 2（偶）
读 data → snapshot = (11, 11)
s2' = seq.load(Acquire) → 2（W 已离场）
s1' == s2'，返回 (11, 11) ✓
```

如果 W 在 R 第一次读的过程中**没有**进入会怎样？s1 = 0、读 = (10, 10)、s2 = 0，相等返回，**一次读路径无任何重试**——这就是 SeqLock 的快速路径。

更狡猾的情况：W 进入（奇）→ W 离开（偶）→ W 再进入（奇）→ W 离开（偶）…… seq 走 0→1→2→3→4。如果 R 在 s1=0、s2=4——还是不相等，重试。**只要 seq 变过就重试**，这是 SeqLock 的不变量。

但有个**陷阱**：如果 seq 从 0→1→2→3→4→5→……→回到 0（回绕），s1=0=s2 但中间已被改多次！这就是为什么 SeqLock 的 seq 通常用 `usize`（64 位）——回绕到原值需要 2^63 次写操作，宇宙寿命内不可能。

### WRITE：完整实现

`crates/forge-lockfree/src/seqlock.rs`：

```rust
pub struct SeqLock<T> {
    seq: AtomicUsize,
    data: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    pub fn new(value: T) -> Self {
        Self { seq: AtomicUsize::new(0), data: UnsafeCell::new(value) }
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel); // 偶→奇
        WriteGuard { lock: self, _start_seq: seq }
    }

    pub fn read(&self) -> T {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {                       // 奇：写者正在改，自旋等
                std::hint::spin_loop();
                continue;
            }
            let snapshot = unsafe { self.data.get().read() };
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 { return snapshot; }       // 没被改过，OK
            // 否则重试
        }
    }
}

pub struct WriteGuard<'a, T> { lock: &'a SeqLock<T>, _start_seq: usize }

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.seq.fetch_add(1, Ordering::Release); // 奇→偶
    }
}
```

### ISO·ZOOM：内存序与一个**严酷**的妥协

逐项审内存序：

- 写者 `fetch_add(1, AcqRel)`：偶→奇这一步用 Acquire（看到之前读者的状态、保护开始改数据）+ Release（标记"我开始改了"）；奇→偶用 Release（把对 data 的修改发布给读者的下次 Acquire）。
- 读者两次 `seq.load(Acquire)`：第一次建立"我看到 seq = s1 这个时刻 data 的状态"；第二次确认"在我读完之前没人改"。
- 读者读 `data` 用裸 `ptr::read`——**非原子**。

这里就藏着一个**严酷妥协**：**严格说，这是数据竞争（UB）**。Rust 内存模型不允许两个线程非原子地并发读写同一块内存，**即便读到的值被丢弃**。Mara Bos 在第 10 章原书就明确指出这点——"both reading and writing the data should be done using only atomic operations, even though the entire read or write does not have to be a single atomic operation."

我们的 crate 把这点写进模块顶层注释，**明确告诉你不要在 miri 下跑数据竞争路径**。要做到完全 sound 需要 RFC 3301 的 `AtomicPerByte`——按字节原子地读写整块数据。在那之前，所有 SeqLock 实现都背着这个 UB 妥协。Linux 内核敢用是因为内核不信任读者、不在乎读者读到撕裂值——读者最多读到错误数据，不会破坏内核本身。**Rust 用户态没有这种"读者不可信"的特权**，所以 SeqLock 在 Rust 里要慎用。

读者最难懂的点之三：**为什么读者用 Relaxed 也安全？** 因为读者**只比对 seq**——seq 的两次读都是原子 Acquire，建立了 happens-before；而读 data 这个动作"安全不安全"完全由 seq 的两次比对来兜底。**比对失败就丢掉这次读到的 data**，比对成功就采纳。data 本身的"读"不需要保证什么内存序——它只是一堆字节被拷贝出来。这种"用 Acquire 计数器保护非原子数据"的模式是 SeqLock 的精髓。

测试 `tests/m8_02_seqlock.rs`：写者把 `(i, i)` 写 20 万次（i 从 1 涨到 200000），读者不停读，断言 `a == b`（无撕裂）且 `a ≤ 200000`。这把"读者永远看到一致快照"这条不变量钉死。

### 深一步：SeqLock vs RwLock vs RCU 的取舍

到这里你可能会问：RwLock、RCU、SeqLock 都是"多读偶改"，区别在哪？

- **RwLock**：读者**真持有读锁**——多个读者可同时持有，但写者必须等所有读者放手。**写者会被读者阻塞**，反之亦然。每次 read/write 都过锁。
- **RCU**：读者**完全不过锁**——只读指针拿快照。写者**完全不等读者**——换指针就走，旧值由回收策略延迟处理。代价是写者要做副本、回收复杂。
- **SeqLock**：读者**完全不过锁**——读 seq、读 data、再读 seq 比对。写者**完全不等读者**——只把 seq 拨奇、写 data、再拨偶。代价是读者要重试、且写路径必须串行。

简单说：**RCU 适合"读者拿到完整副本就走"、SeqLock 适合"读者只是临时看一眼字段"**。RCU 适合数据通过指针访问（`Arc<T>`），SeqLock 适合数据是值类型、放在 inline（`UnsafeCell<T>`）。两者读路径都无锁，但回收/重试机制不同。

Linux 内核用 SeqLock 给进程提供时间戳——时间戳是个几十字节的 struct，每个进程经常读、时钟中断偶尔写。用 RwLock 的话恶意读者能卡死时钟中断；用 SeqLock 读者最多读到旧值或重试一次，**无法阻塞内核**。这就是 SeqLock 的存在理由。

### 一个使用陷阱：写者的唯一性

SeqLock **要求单写者**——多个写者并发会破坏 seq 的奇偶语义。如果实在要多写者，要在 SeqLock 外面套一层 mutex（rcu.rs 那样）。这一限制在 Mara Bos 原书没明说，但实现里隐含——我们的 `write()` 返回 WriteGuard 而不消费 SeqLock，意味着多个线程理论上能同时拿 guard。教学版默认单写者，留个 TODO 给读者：用 Mutex 包 write。

### `UnsafeCell` 与 unsafe impl Sync

看 seqlock.rs 这两行：

```rust
pub struct SeqLock<T> {
    seq: AtomicUsize,
    data: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for SeqLock<T> {}
```

`UnsafeCell<T>` 是 Rust 内部类型，告诉编译器"这块内存可能被并发改、别做假设"。Rust 默认所有 `!Sync` 的类型不能跨线程共享——`UnsafeCell` 是 `!Sync` 的，所以我们要 `unsafe impl Sync` 告诉编译器"我知道我在干什么"。这个 unsafe 的承诺是：**所有访问 data 的代码都经过 seq 保护**。写者持有 guard 时独占；读者通过 seq 比对保证读到一致快照。我们写的 read 函数和 WriteGuard 的 Deref 实现承担了这个承诺。

**这就是 unsafe 的本质**：不是"危险"，是"程序员对编译器承诺它无法验证的不变量"。SeqLock 的承诺是"seq 的奇偶和 Acquire/Release 配对正确"——如果某个 bug 让 seq 比对失效，data 立刻撕裂，UB 立刻发生。所以测试 `m8_02` 在循环里跑 20 万次写、读者断言"永远不撕裂"——这是对承诺的运行时验证。

---

## 七、M8g Chase-Lev 工作窃取双端队列：调度器的心脏

### ENEMY：线程池任务分配的瓶颈

你做了一个线程池，16 个 worker 线程并行干活。最朴素的办法：所有 worker 共享一个全局任务队列（`Mutex<VecDeque<Task>>`）。每来一个任务 push 进去，每个 worker 抢锁 pop 一个。问题：**16 个 worker 全抢这一把锁**——锁成了吞吐量瓶颈。任务越多越堵，堵到任务执行时间远小于等锁时间。

这就是为什么 std 的 `mpsc` 不能当工作池的任务队列——它是单消费者，多消费者会立刻把 channel 内部锁打成瓶颈。`crossbeam_channel` 的多消费者模式也面临同样问题：所有消费者抢一个队列头。**全局共享队列是工作池的反模式**。

更糟糕的是**缓存争用的雪崩**：16 个核都 spin 在同一个队列头的缓存行上，每来一个任务这一行就在 16 个 L1 之间弹一遍。任务派发延迟随核数线性增长——核越多反而越慢。这是"反可扩展性"（anti-scalability），是工作池设计的头号大敌。

这就是为什么 rayon、Go 调度器、crossbeam-deque 都不用全局队列——它们用**工作窃取（work stealing）**：每个 worker 有**自己的本地队列**，自己 push/pop 不抢锁；空闲了再去**偷**别的 worker 队列里的任务。

### ANCHOR：每人一个栈，偷别人的

锚个画面：16 个厨师各自有自己的小推车（本地任务栈）。新任务来了厨师先 push 进自己的推车（不跟别人挤）；自己没活了就从**别人推车底部**偷偷拿一份（偷的人少、被偷的人也少察觉）。这就是 Chase-Lev（David Chase, Yossi Lev, 2005）双端队列。

工作窃取的核心洞察：**争用来源于集中，分散就能消除**。每个 worker 自己的本地队列只有自己 push/pop——零争用。只有"偷"这一动作跨 worker，且偷的是对方队列的另一端（最不冲突的位置），所以偷的概率本就低、偷到时的影响也小。实测：rayon 在 16 核机器上工作窃取的争用开销不到 5%——其余 95% 时间每个核都在干实事。

这套思想不止用于线程池。Go 的 GMP 调度器（goroutine / machine / processor）、Java 的 ForkJoinPool、TBB 的 task_scheduler——所有现代工作窃取调度器都基于 Chase-Lev 或其变种。**它是并行编程基础设施的标准件**。

- **owner**（推车主人）在 **bottom** 端 push/pop —— LIFO，最近的任务先做（缓存友好、有局部性）。
- **stealer**（别的厨师）从 **top** 端偷 —— FIFO，最早的、最不冲突的任务被偷走。

精髓：owner 操作 bottom 端**不抢任何锁**（单线程访问），多个 stealer 间用 CAS 协调 top 端。**几乎消除了所有锁争用。**

### LOW-FI：环形缓冲 + 两个游标

固定容量环形数组 `buffer[CAP]`，两个游标 `bottom`、`top`（都是有符号整数，差 `bottom - top` = 当前元素数）。

- `push(v)`：写 `buffer[bottom % CAP] = v`；`bottom += 1`。
- `pop()`：`bottom -= 1`；读 `buffer[bottom % CAP]`；如果 `bottom ≤ top` 说明空，恢复 bottom 返回 None。
- `steal()`：读 `top`、读 `bottom`；若 `top ≥ bottom` 返回 Empty；否则读 `buffer[top % CAP]`，CAS `top → top + 1`。

### 必选手算 2：Chase-Lev fence 放置逐拍

这是本章的第二个必选手算。Chase-Lev 在弱内存模型（ARM、POWER）上**极易写错**——Rust 内存模型论文《Correct and Efficient Work-Stealing for Weak Memory Models》（Le, Nguyen 等）专门研究这件事。我们逐拍看为什么朴素 Relaxed 出错、论文怎么放 fence 修。

**场景**：owner 刚 push 完一个值进 `buffer[5]`，正准备 `bottom.store(6, Relaxed)`。一个 stealer 同时来偷。

设 buffer[5] 初始未初始化（garbage），bottom=5、top=5（空）。

```
拍 0：
bottom = 5
top = 5
buffer[5] = (未初始化)
```

**owner push 流程（朴素 Relaxed 版）**：

```
拍 1：owner 写 buffer[5] = 42（Relaxed store）
拍 2：owner.store(bottom=6, Relaxed)
```

**stealer 流程**：

```
拍 3：stealer.load(top=5, Acquire)
拍 4：stealer.load(bottom, ...) —— 想知道有没有东西可偷
拍 5：若 bottom=6，stealer 读 buffer[5]
```

**问题来了**：在弱内存模型下，owner 的两条 store（拍 1 写 buffer、拍 2 写 bottom）**没有顺序保证**！它们可能被**重排**成"先 store bottom=6，再 store buffer[5]=42"。或者从 stealer 的视角看，bottom 的更新可能**先于** buffer[5] 的更新被看到。

重排后的 stealer 视角：

```
拍 4：stealer.load(bottom) = 6（已经看到 owner 把 bottom 加了）
拍 5：stealer 读 buffer[5] → (未初始化的 garbage)！
```

stealer 拿到 garbage 当 Task 用 → UB（最常见症状：函数指针乱跳，立即 segfault）。

**Le et al. 论文的解法**：owner 在两条 store 之间放一条 `fence(Release)`；stealer 在 load top 和 load bottom 之间放 `fence(Acquire)`；pop 的特定位置放 `fence(SeqCst)`。重画一遍：

**owner push（正确版）**：

```
拍 1：owner 写 buffer[5] = 42（Relaxed store）
拍 1.5：fence(Release)   ← 保证拍 1 的写在拍 2 之前对所有 Acquire 可见
拍 2：owner.store(bottom=6, Relaxed)
```

**stealer（正确版）**：

```
拍 3：stealer.load(top=5, Acquire)
拍 3.5：fence(Acquire)   ← 与 owner 的 Release 配对
拍 4：stealer.load(bottom, Acquire)
拍 5：若 bottom=6，读 buffer[5] = 42（保证不会读到 garbage）
```

`fence(Release)` 保证 owner 写 buffer 在写 bottom 之前对所有采取 Acquire 序的线程可见。`fence(Acquire)` 让 stealer 看到 bottom 的更新后**也能看到** buffer 的内容。两条 fence 配对，把"先 buffer 后 bottom"这条顺序锁死。这就是为什么我们的 crate `deque.rs` 在 push 里写：

```rust
(*self.slot(b)).write(value);              // Relaxed store buffer
fence(Ordering::Release);                  // Le et al.
self.bottom.store(b + 1, Ordering::Relaxed);
```

pop 的最后一项竞争还要 `fence(SeqCst)` 协调"最后一个元素到底归 owner 还是归 stealer"——这是 SeqCst 在整个算法里唯一不可替代的位置（详情见论文）。loom 在 `LOOM_MAX_PREEMPTIONS=4` 下能枚举出"不放 fence 时的失败交错"，是验证 Chase-Lev 正确性的标准手段。

### WRITE：完整 push / pop / steal

`crates/forge-lockfree/src/deque.rs`（节选）：

```rust
pub struct Deque<T> {
    bottom: AtomicIsize,
    top: AtomicIsize,
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

pub enum Steal<T> { Success(T), Empty, Retry }

pub fn push(&self, value: T) {
    let b = self.bottom.load(Relaxed);
    let t = self.top.load(Acquire);
    assert!((b - t) < CAP as isize, "deque overflow");
    unsafe { (*self.slot(b)).write(value); }
    fence(Release);                       // ← Le et al.
    self.bottom.store(b + 1, Relaxed);
}

pub fn pop(&self) -> Option<T> {
    let b = self.bottom.load(Relaxed) - 1;
    self.bottom.store(b, Relaxed);
    fence(SeqCst);                        // ← 协调与 steal 的竞争
    let t = self.top.load(Relaxed);
    if t <= b {
        let v = unsafe { (*self.slot(b)).assume_init_read() };
        if t == b {
            // 最后一个：与 steal 竞争
            let won = self.top
                .compare_exchange(t, t + 1, SeqCst, Relaxed)
                .is_ok();
            self.bottom.store(b + 1, Relaxed);
            if won { Some(v) } else {
                std::mem::forget(v);    // 被 steal 拿走，v 不是合法 T
                None
            }
        } else { Some(v) }
    } else {
        self.bottom.store(b + 1, Relaxed);
        None
    }
}

fn steal_inner(&self) -> Steal<T> {
    let t = self.top.load(Acquire);
    fence(Acquire);                       // ← Le et al.
    let b = self.bottom.load(Acquire);
    if t >= b { return Steal::Empty; }
    let v = unsafe { (*self.slot(t)).assume_init_read() };
    if self.top
        .compare_exchange(t, t + 1, SeqCst, Relaxed)
        .is_ok()
    { Steal::Success(v) }
    else {
        std::mem::forget(v);
        Steal::Retry
    }
}
```

### ISO·ZOOM：为什么"最后一个元素"需要 SeqCst

仔细看 pop 和 steal 都在抢同一件事：当 `top == bottom`（队列只剩一个元素）时，到底是 owner pop 拿走还是 stealer steal 拿走？两边都用 CAS `top → top + 1` 抢——**只有一边的 CAS 成功**。这一边的 CAS 必须用 SeqCst，另一边也是。为什么不能用 AcqRel？因为我们要保证**"pop 看到 top = b"和"steal 看到 bottom = t"这两件事不会同时成立**——这需要 pop 的 bottom-store 与 steal 的 top-load 之间有一个**全序**。SeqCst 是唯一能提供这个全序的内存序。这是 Chase-Lev 全算法里唯一非 SeqCst 不可的位置。

另一个细节：pop 在 `bottom -= 1` 之前不检查 top，而是**先减后查**。这是为了避免"先查 top，再减 bottom"的窗口里 stealer 看到错误的 bottom。Le et al. 论文证明这种顺序才是 sound 的——直觉解释：pop 先"占住"自己即将弹出的位置（bottom-=1），让 stealer 看到的 bottom 比实际能偷的小一格，宁可让 stealer 看到空、不让他偷到不属于他的东西。这种"宁可保守"的取向是无锁算法的常见取舍。

测试 `tests/m8_07_deque.rs` 是这一章最有说服力的：push 2000 个连续整数，2 个 stealer 偷 + owner pop，**收集到的所有数排序去重后必须恰好等于 0..2000**——无丢失、无重复。这条不变量是 Chase-Lev 正确性的硬指标。

### 深一步：为什么是 LIFO + FIFO，不是两端都 LIFO？

owner 在 bottom 端 LIFO（push/pop 同端），stealer 从 top 端 FIFO（只 steal、不 push）。这个不对称设计是有意为之：

**owner LIFO 的好处**：刚 push 的任务在缓存里还热，立刻 pop 出来执行——缓存命中率高、分支预测准、虚拟内存页表也在 TLB 里。这就是"栈式调度的局部性"，几乎所有递归算法（DFS、回溯）都受益。

**stealer FIFO 的好处**：偷最早 push 的任务（top 端）——这些任务在队列里待最久，owner 大概率不马上需要它们，偷走对 owner 影响小。如果反过来偷 bottom 端（最新 push 的），等于偷 owner 正准备 pop 的——立刻引起 owner 与 stealer 竞争 bottom 游标，争用骤增。

**两端都 LIFO 行不行**？stealer 偷 bottom 端会和 owner pop 抢同一游标——这把"无争用 push/pop"的优势抹平。两端都 FIFO 行不行？owner push/pop 在 bottom 端 FIFO 意味着 pop 时要拿最早的——那要遍历整个 buffer 找最旧的位置，O(n)。所以**只有"LIFO + FIFO"这个组合同时满足"owner 端 O(1) 不争用"和"stealer 端 O(1) 与 owner 不冲突"**。这是 Chase-Lev 的核心洞察。

### 大小、容量、回绕

我们的实现用固定容量 4096。`bottom` 和 `top` 都是 `isize`（i64 在 64 位上）——它们会一直增长，但 `% CAP` 取模定位槽位。两游标的差值是当前元素数，必须 < CAP（否则 pop 出的位置已经被新 push 覆盖）。

**为什么用 `isize` 而不是 `usize`？** 因为 `bottom - top` 在并发下可能出现负值（owner 把 bottom -= 1 之后，stealer 还没看到更新时）。`isize` 让减法自然处理负数，避免 `usize` 下溢 panic。

**回绕**：`isize` 最大约 9.2 × 10^18，每秒 push 一百万次要 29 万年才回绕。可以忽略。

**容量耗尽**：push 时 `assert!((b - t) < CAP)`。生产代码（crossbeam-deque）会动态扩容——但教学版为了简化用固定容量，溢出直接 panic。这是"教学取舍"的另一个例子。

### `MaybeUninit<T>` 的角色

`buffer` 类型是 `Box<[UnsafeCell<MaybeUninit<T>>]>`。`MaybeUninit<T>` 表示"这块内存可能装着一个 T、也可能没初始化"——Rust 编译器不会假设它有合法值。我们：

- `push` 写：`(*slot).write(value)` —— 初始化这一槽。
- `pop`/`steal` 读：`(*slot).assume_init_read()` —— 我们承诺这一槽在调用之前必然已被 push 写过。这个承诺由算法的不变量保证（top < bottom 时，所有 top..bottom 之间的槽都被写过）。
- `mem::forget(v)`：pop 与 steal 竞争失败时，那个值已经被"assume_init_read"出来——但实际它已经被另一边取走了，**不能让它 drop**（drop 会调用 T 的 Drop，可能释放资源），所以 forget 掉。

这是无锁数据结构里处理"竞争失败但已经 read 出来"的标准手法。`assume_init_read` + `mem::forget` 是 unsafe Rust 的常见组合，对应该多想三遍——一旦漏掉 forget，立刻 use-after-free 或 double-free。

### 与 crossbeam-deque 的对照

crossbeam-deque 是 Rust 生态里 Chase-Lev 的标准实现，被 rayon 调度器使用。我们的教学版相比它简化了：

- 固定容量，不动态扩容。
- 不支持多 owner（crossbeam 支持多个 Worker 互相偷）。
- fence 严格按 Le et al. 论文放置——这一点与 crossbeam 一致。

读者读完本章后，可以打开 crossbeam-deque 源码对照——你会发现它做的事和我们一样，只是多了工程包装（容量管理、句柄抽象、epoch 回收）。**核心算法是同一套**。

---

## 八、回到地面：7 台机器能解决什么问题？

回看一下我们装配出的 7 台机器和它们各自的敌人：

| 机器 | 敌人 | 精髓 |
|------|------|------|
| Semaphore | 4 个看起来不同的同步装置 | 全部统一成一个原子计数器 |
| RCU | 装不进原子的大块数据 | 加一层间接 + 整体换指针 |
| Treiber 栈 | 锁的 syscall 开销 | 全 CAS，但要解 ABA |
| MCS 锁 | SpinLock 的缓存 ping-pong | 每 spinner 盯自己的变量 |
| parking-lot 锁 | 锁占空间太大 | 锁压到 1 字节，等待者进全局 HashMap |
| SeqLock | 读者阻塞写者 / 写者阻塞读者 | 奇偶计数器双重比对，读者完全不阻塞 |
| Chase-Lev | 线程池全局队列锁争用 | 每人本地队列 + 互相偷 |

记住这一章唯一一件事：**每台无锁机器都对应一个具体的敌人，没有银弹。** 看到"数据太大没法原子"想 RCU；看到"读者远远多于写者且写者不能被阻塞"想 SeqLock；看到"线程池任务调度"想 Chase-Lev；看到"锁太多占内存"想 parking-lot；看到"高争用自旋"想 MCS。看到无锁栈/队列就要立刻问：**ABA 怎么解？回收怎么做？**——这两个问题答不上来，就别上无锁。

---

## 九、L1–L5 理解阶梯

- **L1（认得名词）**：能说出 Semaphore / RCU / Treiber / MCS / parking-lot / SeqLock / Chase-Lev 各对付什么敌人。
- **L2（读懂代码）**：能给每段代码标注内存序为什么这么选，能解释每条 `unsafe` 的前提。
- **L3（手算证明）**：能徒手画 Treiber ABA、Chase-Lev fence、SeqLock 重试、MCS 交接这 4 个拍子序列，并指出每条序的依据。
- **L4（识别敌人）**：拿到一个新并发问题，能判断该用哪台机器、为什么不用其它六台。
- **L5（自造机器）**：能在没有现成 crate 的情况下，针对自己的工作负载从零写一个无锁结构，并选对回收策略、内存序、ABA 防护。

---

## 十、自检问题

1. 二元信号量初始为 1 时等价于什么？初始为 0 时又等价于什么？为什么？
2. `wait(&permits, 0)` 那个 0 删掉会出什么 bug？画拍子证明。
3. RCU 的第 5 步（D，deallocation）有哪 5 种策略？各自代价是什么？
4. ABA 中"值相同身份不同"的根源是什么？标记指针 / epoch / hazard pointer 各怎么堵住它？
5. MCS 相比 SpinLock 缓存友好的根本原因是什么？用一句话说清。
6. parking-lot 为什么必须"先登记、再二次检查"？反过来会出什么 bug？
7. SeqLock 在 Rust 用户态有什么严酷妥协？为什么内核敢用、用户态慎用？
8. Chase-Lev 的 push 里那条 `fence(Release)` 删掉会在什么架构上炸？为什么？画拍子证明。
9. Chase-Lev 的 pop 在最后一项上为什么必须用 SeqCst？AcqRel 不行？
10. 7 台机器分别的"非竞争快速路径"是什么？哪几台在非竞争下零 syscall？

---

## 十一、动手清单

- **A1（必做）**：跑 `cargo test -p forge-lockfree`，7 个测试全绿。
- **A2**：把 `stack.rs` 改成"用 tagged pointer 防 ABA"——高位塞代次计数器，每成功 CAS +1。补一个测试：用 `loom`（`LOOM_MAX_PREEMPTIONS=4`）复现 ABA，再用你的 tagged 版本证明不再发生。
- **A3**：给 `rcu.rs` 写一个 epoch-based 回收版（不放进 crate 也行，作为练习）。提示：用 `AtomicUsize` global_epoch + thread-local pinned epoch + garbage bag。
- **A4**：给 `mcs.rs` 写一个 FIFO 公平性测试——记录每线程获锁的时间戳，断言顺序单调。
- **A5**：给 `parking_lot.rs` 写一个"恶意读者故意不放手"的测试——把它和 std::Mutex 对比，看 parking_lot 的 1 字节空间优势。
- **A6**：用 `criterion` 基准对比 `forge-lockfree::deque::Deque` 和 `crossbeam-deque::Worker` 在 push/pop/steal 上的吞吐，画图。
- **A7（高难）**：给 `seqlock.rs` 写一个完全 sound 版（用 `AtomicU64` 数组代替 struct 字段，每个字段单独原子访问），对比朴素版的数据竞争 UB 在 miri 下的报错。
- **A8**：用 `loom` 给 Chase-Lev 写一个 4 线程模型（owner + 3 stealer），跑 `LOOM_MAX_PREEMPTIONS=4`，验证不丢不重。

---

## 附录：本章 7 台机器的快速对比表

读到这里你大概有点信息过载——7 个数据结构、4 个手算、几十个内存序选择。把这一章压缩成一张表，方便你日后查阅：

| 机器 | 锁 / 无锁 | 等待方式 | 主要数据 | 主要内存序 | 适用场景 |
|------|-----------|----------|----------|------------|----------|
| Semaphore | 都不是，是计数器 | atomic-wait | AtomicU32 | Acquire / Release | 限并发、信号、二元=mutex |
| RCU (Arc 版) | 读无锁、写有 mutex | 不阻塞 | Mutex<Arc<T>> | Arc 内部 SeqCst 计数 | 多读偶改、大块数据 |
| Treiber 栈 | 无锁 | 不阻塞（CAS 循环） | AtomicPtr<Node> | Release / Acquire | LIFO、高频 push/pop |
| MCS 锁 | 队列锁 | thread::park | AtomicPtr<Node> | AcqRel / Release | 高争用、NUMA |
| parking-lot | 有锁（极小） | thread::park | AtomicU8 + HashMap | Acquire / Release | 海量细粒度锁 |
| SeqLock | 读者无锁、写者独占 | 自旋重试 | AtomicUsize + UnsafeCell | Acquire / Release / SeqCst | 多读偶改、读者不可信 |
| Chase-Lev | 无锁（owner 端单线程） | 自旋（stealer） | AtomicIsize × 2 + 环形 | fence(Release/Acquire/SeqCst) | 工作窃取调度 |

记住这张表的核心是"**每一列的选择都不是任意**"——它们由"对付什么敌人"决定。改一列就改了敌人，整台机器都可能失灵。这就是为什么无锁代码不能"看着差不多了就改改"——每个 fence、每个内存序、每个回收策略都是**对某个失败模式的精确反制**。

## 附录：4 个必选手算的索引

为了方便你回头查，本章 4 个必选手算位置：

1. **Treiber 栈 ABA**（M8c 第三节）—— 7 个拍子，证明地址复用导致 CAS 误判。
2. **Chase-Lev fence 放置**（M8g 第三节）—— 6 个拍子，证明不放 fence 时 stealer 读到未初始化 buffer。
3. **SeqLock 读者重试**（M8f 第三节）—— 3 个拍子，证明 seq 双重比对能检测到写者夹击。
4. **MCS 队列交接**（M8d 第三节）—— 6 个拍子，证明 FIFO 公平 + 每个 spinner 只盯自己缓存行。

每个手算都是"**把这个机器为什么必须这样设计**讲清楚的最直接方式"——比读 10 行代码更直观。如果回头你忘了某个机器怎么工作，**先回到对应的手算**。

---

> **本章的礼物**：你现在已经看完了 Mara Bos 第 10 章的全部 7 个灵感主题，并且**每一个都从零建过、每一个都画过拍子**。下一章 M9a 我们要把 Chase-Lev 双端队列装进一个真正的同步工作窃取线程池（forge-pool），让这套理论在调度器里跑起来。

---

# M8h Latch & Barrier —— 倒计数门闩与可重用屏障

到这里之前，本章的 7 台机器都在解决"**怎么安全地共享/传递数据**"。最后这两个小结构解决的是另一类问题：**怎么让多个线程在某个汇合点对齐**。它们不搬运数据，只搬运"信号"——"我到了"、"所有人都到了吗"。

这类原语在 C++ Concurrency in Action 第 4 章里被归为"等待事件"的工具（与 future、条件变量并列）。它们比 mutex 还轻——没有"谁持锁"的概念，只有"几个线程还差几个"。但正因为它薄，配方上的每一拍都必须精确。

## 第一节：Latch 是什么——"一次性闸门"

先建画面。想象一条河上有道闸门，水位代表"还没干完的工人数"。一开始水位 4 米——有 4 个工人在干活。每干完一个工人离开，水位降 1 米。**只有水位降到 0 米时，闸门才会打开**，让等着的人过去。

这就是 Latch（门闩）：一次性闸门，初始化为 N，N 次 `count_down` 之后永远打开，直到被丢弃。**打开后不能再关上**——这是它和 Barrier 最关键的差别。

什么时候用？最常见的场景是"等所有 worker 把数据生成完，然后主线程统一处理"。比如你启动 8 个线程各算一块傅里叶变换，主线程在 latch 上等，等它们都 count_down 完，主线程再读结果。这种"一次性汇合"用 Barrier 是杀鸡用牛刀——Barrier 的复杂度全花在"重置计数"上，而你只用一轮。

什么时候不该用？凡是"想用第二轮"的，都不是 Latch。多轮循环对齐请用 Barrier。

Latch 的数据极简：一个 `AtomicU32`。它一身二职——数值是"还差几次 count_down"，地址是 atomic-wait 的"床位"（wait 的线程睡在这里，wake_all 唤醒它们）。整套实现不到 30 行有效代码。

## 第二节：Latch 的内存序配方（核心，手算）

这是这一节唯一必须看懂的东西。下面我们逐拍画一个 5 线程的例子，你会看到它与 M4 Arc drop 的内存序骨架是同一个东西。

**设置**：线程 T1–T4 各算一个数组（写到自己的 `Vec<u64>`），算完调 `count_down()`。线程 T5 在 `latch.wait()` 上等，等打开后它要把四个数组拼起来用。

```
共享：
  results: [Mutex<Vec<u64>>; 4]   ← 每个 worker 写自己的格子
  latch:   Latch::new(4)
```

**配方**：
- `count_down` 里 `fetch_sub(1, Release)`
- 最后那一次（prev==1）额外 `wake_all`
- `wait` 里 `while load(Acquire) != 0 { wait(...) }`

现在逐拍走。为了清晰，把每个 worker 在 `count_down` **之前**的写入记作 `W_i`（写自己的 results[i]），把 `count_down` 这条指令记作 `D_i`。T5 的"拼数组"记作 `R`。

| 拍 | 事件 | 原子值 | 关键同步 |
|----|------|--------|----------|
| 0 | T1 写完 results[0]，正要 count_down | 4 | T1 的 W_1 已经发出（但其它核心未必看到） |
| 1 | T1 执行 `fetch_sub(1, Release)` | 3 | **W_1 ↔ D_1 之间建立 Release 屏障**：W_1 不可能被重排到 D_1 之后 |
| 2 | T2、T3 同样路径 | 2 → 1 | 各自的 W_2、W_3 也被各自的 D_2、D_3 圈住 |
| 3 | T4 写完 results[3]，count_down。prev=1 | 0 | T4 是"把它从 1 减到 0"的那一个——**触发 wake_all**。此时 T4 的 W_4 也被圈在它的 D_4 里 |
| 4 | T5 被 wake_all 叫醒，回到 wait 循环顶部，`load(Acquire)` | 0 | **Acquire 看到值 0** ——这一次 load 与上面四条 Release fetch_sub 中的**某一条**建立了 synchronizes-with |

关键点在第 4 拍：T5 的 Acquire load "钩住"的是**最后一个把计数减到 0 的那条 Release**（T4 的 D_4）。但 Release/Acquire 配方的承诺比这更强——它说："**在 D_4 之前所有对 T4 可见的写入**，都对 T5 可见"。而 D_4 之前，T4 至少看到了自己的 W_4。但 W_1/W_2/W_3 呢？

这里要用到 happens-before 的**传递性**。每条 D_i 都是 Release，它圈住了对应的 W_i。这些 Release 操作虽然是不同线程发的，但它们都作用于**同一个原子变量** `count`。当 T5 的 Acquire load 看到值 0 时，按内存模型规定，它**看到了把计数减到 0 的整条因果链**——也就是所有四条 fetch_sub，连同它们各自圈住的 W_1..W_4。于是 T5 拿到了完整的 results。

**这跟 M4 Arc drop 是同一个骨架。** 回想 M4：多个 Arc 各自 `fetch_sub(1, Release)`，**最后一个**（把 strong 计数从 1 减到 0 的那个）执行 `fence(Acquire)`，于是它能安全 drop 内部数据。这里 Latch 是**对称的镜像**：减的人用 Release，等的人用 Acquire；但骨子里都是"**释放方减计数 + 获取方看计数归零**"。Arc 的 fence 放在 drop 那边是因为只有一个释放者需要同步；Latch 的 Acquire 放在 wait 那边是因为所有等待者都需要同步。**配方的对偶性**——记下这个，你就抓住了 M1–M8 所有"计数 + 等待"原语的核心。

为什么 `count_down` 用 Release 而不是 Relaxed？因为如果用 Relaxed，W_1 可能"漏"在 D_1 之后才被其它核心看见——T5 看到 count=0 时，results[0] 可能还是空的。Release 把 W_1 钉死在 D_1 之前，杜绝这种漏写。

为什么 `wait` 里用 Acquire 而不是 Relaxed？同理——如果用 Relaxed，T5 看到 count=0 不能保证它也看到了 W_1..W_4，照样可能读到空数组。Acquire 把"看到 0"和"看到 W_i"绑成一个不可分割的事件。

到这里你应该能复述这条铁律：**"先 Release 改计数、后 Acquire 看计数"——这是 Latch、Arc drop、所有"减计数+等待"原语通用的配方。**

## 第三节：Latch 实现与一个 bug 风险

代码（`crates/forge-lockfree/src/latch.rs`）：

```rust
pub fn count_down(&self) {
    let prev = self.count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        // 我把它从 1 减到 0 —— 广播唤醒
        wake_all(&self.count);
    }
}

pub fn wait(&self) {
    loop {
        let cur = self.count.load(Ordering::Acquire);
        if cur == 0 { return; }
        wait(&self.count, cur);   // 内核原子地检查 + 入睡
    }
}
```

注意几个细节，都是前面章节踩过的坑：

1. **wake_all 必须在改值之后**。这是 atomic-wait 不丢唤醒的不变量（M6）。如果先 wake 再 fetch_sub，等的人可能在"已检查 0、还没睡"的缝隙里错过唤醒。这里顺序对。
2. **wait 必须循环**。atomic-wait 的 `wait` 可能假唤醒（M6 反复强调）。醒来必须重新 load 判断。
3. **`prev == 1` 判断 leader**。fetch_sub 返回减**之前**的值，所以 prev=1 意味着我是把它减到 0 的那一个。多个线程同时减到 0 不可能——count 是单调递减的。

**一个 bug 风险**（不是我们的实现里有，而是 std::latch 用户容易犯）：调用方多次 count_down 会让计数变负。我们没做饱和检查（与 std::experimental::latch 一致），因为加锁检查会破坏无锁性。滥用是调用方的事。但你要知道：**Latch 的契约假设"每方只减一次"**——这是它和 Barrier 的另一差别。

---

## 第四节：Barrier —— 可重用的汇合

Latch 用一轮就扔。但很多场景需要"**每轮都对齐**"——比如并行 Jacobi 迭代：N 个线程各算一行，每轮结束所有人到齐交换边界，再进下一轮。这种**周期性汇合**正是 Barrier 的领地。

画面：想象一个会议室，N 个人围坐。每个人算完自己那份工作就走到门口等着，但门不开——直到最后一个人到。门一开，所有人同时出门（这就是"放行"）。然后会议室**自动重置**，下一轮人又能进来。

关键差别（再强调一次）：Barrier 的计数会被 leader **自动重置回 N**，于是能反复用。Latch 不重置。

这种"自动重置"看似只是多一行代码，实际上引入了 Latch 没有的一个陷阱——**代次（generation）混淆**。下面手算一个真实例子。

## 第五节：手算——为什么 Barrier 必须有 generation

这是我们这一节的"敌人先行"。先看一个**没有 generation 的朴素 Barrier** 怎么在快慢线程交错下出 bug。

朴素实现（错误示范）：

```rust
// 错误版：只有 count，没有 generation
struct NaiveBarrier { n: u32, count: AtomicU32 }

fn wait(&self) {
    let prev = self.count.fetch_sub(1, Release);
    if prev == 1 {
        self.count.store(self.n, Release);  // 重置
        wake_all(&self.count);
    } else {
        while self.count.load(Acquire) != self.n {
            wait(&self.count, /* ??? */);
        }
    }
}
```

设置：N=3。三个线程 T1（快）、T2（快）、T3（慢）。它们要跑 2 轮。

| 拍 | T1 | T2 | T3 | count |
|----|------|------|------|-------|
| 0 | fetch_sub: 3→2 | | | 2 |
| 1 | (prev=2，不是 leader)进 wait 循环，load=2≠3，进 sleep | | | 2 |
| 2 | | fetch_sub: 2→1 | | 1 |
| 3 | | (prev=1，leader) **重置 count=3**，wake_all | | | **3** |
| 4 | 被 wake，load=3=N，**误以为放行**——出 wait。开始第二轮工作 | | | 3 |
| 5 | T1 已经在做第二轮的活，又 fetch_sub: 3→2 | | | 2 |
| 6 | | | T3 终于到第一轮的 wait。fetch_sub: 2→1 | | 1 |
| 7 | | | prev=1！**T3 被认成第二轮的 leader！** 它重置 count=3、wake_all | | 3 |
| 8 | | | T3 出 wait，开始第二轮工作 | | 3 |

看第 6–7 拍：T3 还在**第一轮**，但它看到的 count 是 3（被 T2 第 3 拍重置过，又被 T1 第 5 拍减了一次后变 2，T3 自己减一次变 1）。它误判自己是 leader，去重置 count、wake——**可根本没有人睡在 wait 上**（T1/T2 都已经跑掉了）。更糟的是 T3 自己也"以为放行"了，开始第二轮工作。但 T3 的第一轮工作其实还没跟 T1/T2 同步——T3 读到的"上一轮 T1 写的数据"可能根本不是上一轮的，是上上一轮的残留。

这就是**代次混淆**：一个慢到的线程带着旧轮次的认知，闯进了新轮次的计数。结果是同步关系彻底错乱——Barrier 的承诺（"放行时所有人都对齐过"）被打破。

**修复**：给每一轮打一个"代次"标签。计数器不光记录"还差几个人"，还记录"这是第几代"。慢到的线程发现自己的代次和当前的代次对不上，就知道自己迟到了——要么补一刀（罕见），要么直接放行（这里我们选这个）。

## 第六节：generation 编码——把两件事塞进一个原子

实现上最优雅的办法是**把"代次"和"剩余计数"编码进同一个 `AtomicU32`**——这样我们仍然只用一个原子、一个 atomic-wait 床位。方案：

```
高 2 位 = 代次 mod 4
低 30 位 = 剩余计数
```

为什么 2 位代次够？因为 atomic-wait 的"床位"是原子的**值**——值变了，wait 立刻返回。leader 推进代次时值必然变，所以旧代次的 sleeper 一定会被"值变化"触发醒来。模 4（而不是模 2）是为了留一个保险：理论上只要"旧代 sleeper 还在睡" 和 "leader 把代次推到下一档" 之间没有第三轮插进来，模 2 就够。模 4 给"两轮重叠"留了余量——实践中 barrier 几乎不会有这种交错，但模 4 几乎零成本（仍是同一个 u32），所以选它。

```rust
const GEN_SHIFT: u32 = 30;
const COUNT_MASK: u32 = (1u32 << GEN_SHIFT) - 1;

fn pack(gen: u32, count: u32) -> u32 {
    ((gen & 0b11) << GEN_SHIFT) | (count & COUNT_MASK)
}
```

`wait` 的核心：

```rust
pub fn wait(&self) -> bool {
    let s = self.state.load(Ordering::Acquire);   // 记住我进来的代次
    let (gen, _count) = unpack(s);

    // 把剩余计数减一（fetch_sub 只动计数位，不影响代次位）
    let prev = self.state.fetch_sub(1, Ordering::Release);
    let (prev_gen, prev_count) = unpack(prev);

    // 在我 load 和 fetch_sub 之间，有人推进了代次——我迟到了
    if prev_gen != gen { return false; }

    if prev_count == 1 {
        // 我是 leader：推进代次、重置计数、wake_all
        let next_gen = (gen + 1) & 0b11;
        self.state.store(pack(next_gen, self.n), Ordering::Release);
        wake_all(&self.state);
        return true;
    }

    // 不是 leader：睡在"当前 state 值"上，直到代次变了
    loop {
        let cur = self.state.load(Ordering::Acquire);
        let (cur_gen, _) = unpack(cur);
        if cur_gen != gen { return false; }   // 放行
        wait(&self.state, cur);
    }
}
```

现在重画上一节的崩溃场景，看 generation 怎么救场。

| 拍 | T1 | T2 | T3 | state (gen, count) |
|----|------|------|------|---------------------|
| 0 | load: (g=0, c=3)，fetch_sub | | | (0, 2) |
| 1 | 进 wait，load: (0, 2)≠leader，sleep 在 state=(0,2) 上 | | | (0, 2) |
| 2 | | load: (0, 3)，fetch_sub → (0, 1) | | (0, 1) |
| 3 | | prev_count=1，leader！**store (g=1, c=3)**，wake_all | | **(1, 3)** |
| 4 | T1 被 wake，load: **gen=1 ≠ 0**，放行 ✓ | | | (1, 3) |
| 5 | T1 第二轮工作，fetch_sub → (1, 2) | | | (1, 2) |
| 6 | | | T3 终于到。load: (g=1, c=2)——T3 的 gen=1，count=2 | | (1, 2) |
| 7 | | | T3 fetch_sub → (1, 1)。prev_gen=1=T3 的 gen ✓，不是混淆 | | (1, 1) |
| 8 | | | T3 不是 leader（prev_count=2，不是 1），进 wait，sleep 在 (1,1) 上 | | (1, 1) |
| 9 | | T2 进入第二轮，fetch_sub → (1, 0)... 等等，prev_count=1 | | (1, 0) → |
| 10 | | T2 是第二轮 leader，store (g=2, c=3)，wake_all | | (2, 3) |
| 11 | | | T3 被 wake，load: **gen=2 ≠ 1**，放行 ✓ | | (2, 3) |

注意第 6 拍的关键：T3 进来时读到的 gen=1（不是它"以为"的 0）。它**接受自己已经迟到第一轮**这一事实，直接当作"我处在第二轮"参与——这正确。它不再被错认成"第一轮的迟到 leader"，因为 fetch_sub 后它检查 `prev_gen == gen`，prev_gen 和它记住的 gen 都是 1，匹配，于是它走正常的"非 leader"路径。

**generation 的本质作用**：给每个 wait 一个"身份"（"我是哪一代进来的"），让迟到的线程能识别自己迟到、让睡着的线程能识别自己被哪一代唤醒。没有这个身份，所有线程都长得一样，于是谁先谁后全靠猜——猜错就是上一节的 bug。

---

## 第七节：读者最难懂的一处——`wait` 里为什么传 `cur` 而不是固定值

`wait(&self.state, cur)` 这一行，`cur` 是我刚 load 到的值。为什么不传一个常量，比如 `u32::MAX`？

回想 atomic-wait 的契约（M6）：`wait(addr, expected)` 的语义是"**如果 addr 此刻仍等于 expected，则睡进内核；否则立刻返回**"。这是"检查 + 入睡"的原子操作。

如果我传 `u32::MAX`（一个绝不可能等于 state 的值），wait 会立刻返回——因为它发现值不匹配。于是 `wait` 退化成"立即返回"，整个循环就变成忙等 spin——CPU 飙到 100%，这正是我们要避免的。

如果我传 `cur`（刚 load 到的值），有两种情况：
- **state 仍是 cur**：说明这一代还没人推进，我安详入睡。对的。
- **state 已经变了**（被 leader 推进代次了）：wait 立刻返回，我们回到循环顶部重新 load，看到新 gen，放行。也对。

所以传 `cur` 才是"真正睡进去"和"被推进及时叫醒"之间的正确平衡。**这是 atomic-wait 配方里最容易被写错的一行**——很多教程演示时传错值，于是看起来在等、其实在忙等。我们的实现里这一行不能改。

---

# M-cancel CancellationToken —— 协作式取消令牌

前面 M8 的所有原语都在解决"线程之间怎么同步"。这一节换个角度：**怎么让一个线程（或 future）停下来**。

听起来像是同一个问题，其实不是。"同步"是"等条件成熟"，"取消"是"通知对方别等了"。前者关心的是"什么时候继续"，后者关心的是"如何让一个正在跑的任务停下来，而且不留下烂摊子"。

## 第一节：敌人先行——为什么不能"杀"线程

先建画面。想象你在厨房做菜：切了一半洋葱、油刚下锅、肉还在化冻。这时有人从背后把你拽走——厨房什么样？洋葱切了一半留在案板、油锅在火上没人看、肉化冻水流了一桌。**外部强杀线程就是这种状态**：所有"半成品"留在进程里——mutex 被锁住再没人能解锁、堆上分配的内存没人释放、正在写的数据结构停在半写状态、不变量被破坏。

C 标准库的 `pthread_cancel` 提供两种模式：
- **异步取消**：随时可能杀。POSIX 自己都说"几乎不可能用对"——因为任何一句 C 代码都可能是取消点，你不能保证它处于"安全状态"。
- **推迟到取消点**：只在某些 libc 函数（sleep、read、write…）才检查取消标志。安全一些，但 Rust 不暴露这套机制——因为 Rust 的所有数据结构都不假设"我在某个点可以被中断"。

Rust 的答案：**根本不提供"外部杀线程"**。Rust 标准库没有 `Thread::kill`，这是有意为之的安全决策。取而代之的是**协作式取消**——被取消的任务**自己**在方便的时刻查标志，自己决定何时退出。

"协作"二字含义就在这里：取消者只设标志，被取消者配合地主动检查。**检查点由被取消者自己选**——这保证它只在"我现在处于一致状态、可以安全退出"的时刻才退出。Rust 的安全保证就这样保住了：被取消者知道自己退出时锁已释放、不变量已维护、内存已还。

## 第二节：CancellationToken 的 API 画面

`CancellationToken` 就是上面说的"标志"，外加一个让异步代码能"睡在标志上"的机制：

```rust
pub struct CancellationToken { /* Arc<Inner> */ }

impl CancellationToken {
    pub fn new() -> Self;                      // 未取消
    pub fn is_cancelled(&self) -> bool;        // 同步检查
    pub fn cancel(&self);                      // 设标志 + 唤醒等待者
    pub fn cancelled(&self) -> Cancelled;      // 返回一个 Future
}

impl Clone for CancellationToken { ... }       // 克隆共享状态
```

两种用法：

**同步用法**（在普通线程的循环里）：

```rust
while !token.is_cancelled() {
    干一批活;
}
println!("收到取消，清理后退出");
```

**异步用法**（在 `async fn` 里 await 一个 future）：

```rust
async fn worker(t: CancellationToken) {
    loop {
        tokio::select! {
            _ = do_one_unit() => continue,   // 正常路径
            _ = t.cancelled() => break,      // 被取消
        }
    }
    清理();
}
```

`cancelled()` 返回的 `Cancelled` 是一个 future：第一次 poll 时若已取消则立刻 Ready；否则把当前 `Waker` 注册到 token 内部，返回 Pending。`cancel()` 被调用时取出全部 waker，逐个 wake——于是所有 await 在它上的 future 都被调度器重新调度，下次 poll 时读到 flag=true，返回 Ready。

## 第三节：为什么内存序可以全 Relaxed

`cancel()` 里设 flag 用 `Ordering::Relaxed`，`is_cancelled()` 读 flag 也用 `Ordering::Relaxed`。这看起来很激进——M1 教过我们"通常要 Release/Acquire 才能同步数据"。为什么这里可以放任？

**关键观察**：flag 本身**不携带任何数据**需要同步。它只是个布尔值——true 或 false。线程间真正要同步的数据（比如 worker 之间共享的 results 数组）有它们自己的同步机制（mutex、channel、Arc 的内存序）。

具体说：取消者设 flag=true 时，并不打算通过这个 flag 让被取消者"看到某个数据"。被取消者看到 flag=true 后做什么？它只是**清理自己的资源然后退出**——清理的是自己持有的锁、自己分配的内存，跟取消者没关系。所以 flag 的传播不需要 happens-before 任何数据写入，Relaxed 就够了。

对比一下：Latch 不能全 Relaxed——因为 latch.wait 看到计数归零后**紧接着要读 results**，那个读必须和 worker 的写建立同步。而 CancellationToken 看到取消后**不读取消者的任何数据**，只是自己跑路。这就是差别。

`std::sync::atomic::AtomicBool::load(Relaxed)` 在 x86 上编译成普通 MOV，没有 fence、没有锁前缀——这是它快的来源。`is_cancelled()` 可以放在任何热路径循环顶部，几乎零开销。

## 第四节：Cancelled future 的 waker 队列——和 Cancelled::drop 的取舍

`Cancelled` 这个 future 有一个微妙的实现细节。第一次 poll（未取消时）必须把当前 Waker 推进一个队列；`cancel()` 时逐个 wake。看起来直接，但有几个坑：

**坑 1：去重。** 同一个 future 被多次 poll（每次被调度器重新调度），每次都 push 一个 waker 会让队列无限增长。我们用 `will_wake` 做末尾去重——如果队列末尾的 waker 能唤醒同一个人，就不重复 push。这不完美（`will_wake` 是 best-effort，可能 false negative），但实践上足够。

**坑 2：cancel 之后到达的 poll。** cancel 一旦发生，`pending` 字段被设为 None，之后任何新 poll 都直接 Ready。这是"一次性事件"的语义——Cancelled 不像条件变量那样可以被重新等待。

**坑 3：Cancelled::drop 时的清理。** 一个 `select!` 分支里如果另一边先 Ready，Cancelled 被 drop。此时它的 waker 还留在队列里——cancel 时会去 wake 一个已经死掉的 future，浪费一次。理想情况是 drop 时从队列精确移除自己，但 `Waker` 没有可靠的"身份"（`will_wake` 不满足等价关系）。所以实践中**不去精确移除**，依赖"队列在 cancel 时整体清空"作为上限。我们的实现走的就是这条简洁路径——这是工业级 token 库（如 tokio_util::sync::CancellationToken）的共同取舍。

## 第五节：实现里的关键代码

```rust
pub fn cancel(&self) {
    // 先设 flag（Relaxed 够，见第三节）
    self.inner.flag.store(true, Ordering::Relaxed);
    // 取出全部 waker 并永久关闭队列
    let pending = self.inner.waker_lock.lock().unwrap().pending.take();
    if let Some(list) = pending {
        for w in list { w.wake(); }
    }
}
```

注意"先 store flag、再 take 队列"的顺序——这跟 atomic-wait 的"先改值、再 wake"是同构配方。flag.store 保证之后任何 is_cancelled 都看到 true（无需 wake）；take 队列则把已注册的 waker 全部唤醒。如果在两者之间有新 poll 进来，它走快路径（flag 已 true → 立刻 Ready），不进队列——这正是我们要的。

```rust
fn poll(...) -> Poll<()> {
    if self.token.is_cancelled() { return Poll::Ready(()); }
    {
        let mut q = self.token.inner.waker_lock.lock().unwrap();
        match &mut q.pending {
            None => return Poll::Ready(()),  // 拿锁期间被 cancel
            Some(list) => {
                // 末尾去重
                if !list.back().map(|w| w.will_wake(cx.waker())).unwrap_or(false) {
                    list.push_back(cx.waker().clone());
                }
            }
        }
    }
    // 双检：拿锁期间 cancel 可能发生
    if self.token.is_cancelled() { return Poll::Ready(()); }
    Poll::Pending
}
```

**双检**是必备的——否则会丢唤醒。如果不双检，考虑这个交错：poll 锁内 push waker → 锁外、还没 return Pending → cancel() 触发 wake(队列里的 waker) → poll 返回 Pending。此时 future 已经被 wake 一次，但它还没"睡"过——这次 wake 浪费了。下次调度器再来 poll 才能发现 flag=true。双检让 poll 在拿锁外再查一次 flag，弥补这个缝隙。

---

## 第六节：手算——协作式取消的时序

设置：一个 worker 在循环里干活，主线程在某个时刻取消。

| 拍 | worker 线程 | 主线程 | flag | iter |
|----|------------|--------|------|------|
| 0 | 进入循环，检查 `is_cancelled` | | false | 0 |
| 1 | false，干一批活（耗时 1ms） | | false | 1 |
| 2 | 检查 `is_cancelled` | 主线程 `cancel()`：flag.store(true) | true | 1 |
| 3 | 看到 true，跳出循环 | | true | 1 |
| 4 | 清理：释放自己持有的 mutex、drop 自己的 Arc | | true | 1 |
| 5 | 退出 | | true | 1 |

关键看第 2 拍：worker 和主线程并发。worker 在干活的中间（不可能被打断——Rust 不允许），它做完一批才检查 flag。这正是"协作式"的含义——**取消不是即时的，它发生在 worker 自己选的检查点**。如果 worker 的活每批 1ms，那取消延迟最多 1ms；如果活每批 1 秒，取消延迟最多 1 秒。延迟由 worker 的检查频率决定，不是由取消者的急迫程度决定。

这是协作式的代价：**取消响应延迟 = 检查周期**。它换来的好处是绝对的内存安全——worker 保证在第 3–4 拍退出时自己处于一致状态，没有任何"半成品"。C++ `pthread_cancel` 异步模式做不到这一点。

如果业务需要快速取消响应，做法不是"换抢占式"，而是"**缩短检查周期**"——把长任务拆成小段，每段结束查一次 flag。这正是 async Rust 的 `select! { _ = work => ..., _ = token.cancelled() => ... }` 在做的事：在每个 await 点都给取消一次机会。**await 点就是协作式取消的检查点**。

---

## 本章最后的两个练习

- **A1（必做）**：跑 `cargo test -p forge-lockfree -p forge-core`，确认 m8h_* 和 cancel_* 全绿。
- **A2**：给 Latch 加一个 `count_down_and_wait()` 方法——原子地减一，然后等。注意它和"先 count_down 后 wait"在唤醒语义上有微妙差别（count_down_and_wait 永远会等，即使我是 leader；而 count_down+wait 会让 leader 在 wake_all 后立刻 wait 然后被自己 wake——但其实 wake_all 不 wake 自己，所以 leader 会真的睡死）。试着用代码验证这个坑，然后想清楚怎么避免。
- **A3**：给 Barrier 写一个 `wait_with_timeout(dur) -> Result<(), TimedOut>`。提示：timeout 要起一个辅助线程或用 condvar——纯 atomic-wait 不直接支持 timeout（Linux futex 有 FUTEX_WAIT_BITSET 可带时钟，但 atomic-wait crate 不暴露）。
- **A4**：给 CancellationToken 加一个 `select!`-友好的"取消后做某事"——比如 `cancelled().then(cleanup)`。提示：用 `async { token.cancelled().await; cleanup().await }`。
- **A5（高难）**：把 Cancelled 的 waker 队列改成 lock-free 的 intrusive 链表（每个等待者自己持有节点），对比 std::Mutex 版的延迟。

---

> **这两个子模块的礼物**：Latch 给你"一次性汇合"的最薄实现，Barrier 给你"可重置汇合"——后者通过 generation 编码解决了一个 Latch 没有的陷阱。CancellationToken 则把"如何让任务停下来"这个问题，从外部强杀的不安全路径，扭到协作式检查的安全路径——这是 Rust 并发安全性的最后一道防线。它们都极薄（Latch 30 行、Barrier 60 行、Token 100 行），但每一行都有理由——这正是无锁并发原语的常态。
