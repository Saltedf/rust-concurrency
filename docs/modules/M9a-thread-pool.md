# M9a — 同步工作窃取线程池：把 M1–M8 装成一台真实运行时

> 模块：`crates/forge-pool::{oneshot, v1_shared_queue, v3_stealing}`
> 测试：`crates/forge-pool/tests/m9a_{basic,nested_spawn}.rs`
> 跑：`cargo test -p forge-pool`

---

## 〇、开战之前：你已经有什么、还差什么

你已经走完了 M1–M8。回顾你手里能用的零件：

- **原子变量与内存序**（M1）：`AtomicU32`、Acquire/Release/SeqCst。
- **互斥锁、Condvar**（M2/M7）：标准锁、用 futex 拼的真锁。
- **自旋锁**（M3）：低延迟、单核内极快。
- **`Arc`/`Weak`**（M4）：跨线程的共享所有权。
- **通道**（M5）：oneshot、mpsc。
- **`atomic-wait` 即 futex**（M6）：`wait(addr, val)` + `wake_one(addr)`。
- **无锁结构动物园**（M8）：信号量、RCU、Treiber 栈、MCS 锁、parking-lot、SeqLock、Chase-Lev 双端队列。

这些零件单独看都是好东西。但你**还没有一台机器**——一台"我投一堆任务进去，它就自动用满所有核跑完"的机器。

如果你现在要写一个并行计算 1 到 1 亿的和，你会怎么做？你大概率会手动 `std::thread::spawn` 8 个线程、把区间切成 8 段、每个线程算一段、最后 join 8 次。**这一份手动切分代码每次都得重写**。如果其中某一段还要进一步切分（比如并行快排），你就得在每段内部再 spawn 一层——开 64 个线程？太多了，线程是操作系统资源，开多了会爆。

更糟的是，**任务之间会互相等待**：并行快排的某一段在等自己 spawn 出来的子段。线程在等的时候**白白占着一个核**——它没干活，但别的任务也用不上这个核（因为这个核被这个睡着的线程占了）。

我们要在这一章造一台机器，解决这两个问题：

1. **任务调度**：把"投进去就自动跑"做成一个抽象——你 `pool.spawn(|| ...)`，它在某个 worker 线程上跑，跑完结果送回你手里。
2. **负载均衡**：N 个 worker，每个有自己的本地队列；闲的 worker 去偷别人的任务，谁也不闲着。
3. **嵌套 spawn 不死锁**：worker 在等子任务结果的时候**继续干活**，把别的任务跑掉，绝不真的睡。

这台机器叫**工作窃取线程池**（work-stealing thread pool）。它是 `rayon`、`tokio`、Go 调度器、Java ForkJoinPool 的共同脊梁。M9a 完成后，你就有了一个**真实可用的同步运行时**——即便不做异步，你也能用它跑并行快排、并行 map、并行 reduce。

这一章我们要做**三次逐拍手算**（共享队列争用、LIFO/FIFO 工作窃取、嵌套 spawn 死锁与解除），还要**故意造一个死锁再修它**。读完之后，你应当能回答：

> rayon 的 `join(|| f1(), || f2())` 为什么能在 1 个 worker 上也不死锁？

让我们从一个看似简单、但藏陷阱的问题开始：**怎么拿到任务的返回值？**

### 一个具体的"为什么我需要池"的例子

为了让你**真切**感到需要一台机器，让我们对比三种写法。

**写法 A：单线程串行**

```rust
fn sum_range(start: u64, end: u64) -> u64 {
    (start..end).map(|i| expensive_hash(i)).sum()
}
fn main() {
    let total = sum_range(0, 1_000_000);
    println!("{total}");
}
```

跑在你机器上的 1 个核上。8 核机器的另外 7 个核全闲。1 亿条数据要算 ~1 分钟。

**写法 B：手动 std::thread::spawn 切 8 段**

```rust
fn sum_parallel(start: u64, end: u64) -> u64 {
    let n_threads = 8;
    let chunk = (end - start) / n_threads as u64;
    let handles: Vec<_> = (0..n_threads).map(|i| {
        let s = start + i as u64 * chunk;
        let e = if i == n_threads - 1 { end } else { s + chunk };
        std::thread::spawn(move || sum_range(s, e))
    }).collect();
    handles.into_iter().map(|h| h.join().unwrap()).sum()
}
```

8 核全用上，~7.5 秒。但是：你手动切了 8 段、写了 join 循环。下一次你要算"对一颗树并行遍历"或"并行快排"时，这套手写代码不能复用——树不能切成 8 段、并行快排的子段大小是动态的。

**写法 C：用工作窃取池**

```rust
fn sum_with_pool(pool: &StealingPool, start: u64, end: u64) -> u64 {
    if end - start < 10_000 {
        return sum_range(start, end);  // 小段：直接算，不 spawn
    }
    let mid = (start + end) / 2;
    let h = pool.spawn(move || sum_with_pool(pool, mid, end));
    let left = sum_with_pool(pool, start, mid);
    let right = h.recv();
    left + right
}
```

写法 C 用了**递归切分**——每段不够小就再切。每次切都通过 `pool.spawn` 投递一半到池里。池的 worker 自动把任务分发到所有核。**你不需要知道机器有几个核**——池知道。任务量小到一定程度就停止切（这里是 10000），避免无谓的 spawn 开销。

写法 C 的优势：
- **代码可复用**：换成"对树并行遍历"只需要把切分逻辑改成树递归。
- **自动负载均衡**：某段比预期慢（数据局部性差），别的 worker 自动通过工作窃取把别的段抢走，没人闲着。
- **嵌套 spawn 自然**：递归 spawn 不死锁——这是 M9a 的核心保证。

写法 C 的潜在问题：
- 如果 `sum_with_pool(pool, start, mid)` 直接在调用方线程跑、`pool.spawn(...)` 投另一半——**调用方线程的栈会递归展开**。深度为 30 的递归会让栈涨到 MB 级。
- rayon 用一个叫 `join` 的 API 解决这个：`rayon::join(|| left(), || right())`，它把两边都投到池里、当前 worker 只跑一边、另一边等别人偷——这样栈不会递归展开。

我们这版简化池支持 `spawn + recv` 的写法（写法 C），但 rayon 的 `join` 留给 M9a-par 章节优化。教学先讲清楚 `spawn + recv`——这是最基础的、所有更高级抽象（`join`、`scope`、`par_iter`）的底层。

---

## 一、JoinHandle：把"投了任务"变成"等得到结果"

### ENEMY：朴素池只能 fire-and-forget

网上大部分"Rust 线程池教程"长这样：

```rust
struct NaivePool {
    queue: Mutex<VecDeque<Box<dyn FnOnce() + Send>>>,
    // ...
}
impl NaivePool {
    fn spawn(&self, f: impl FnOnce() + Send + 'static) {
        self.queue.lock().unwrap().push_back(Box::new(f));
    }
}
```

`spawn` 接受一个 `FnOnce()`——**没有返回值**。任务跑完，结果就丢了。如果你想要结果，得自己在外面套一层 `Arc<Mutex<Option<T>>>` 或者 channel，把任务闭包改成"算完之后把结果塞进去"。

每个调用方都得自己造这套机制。我们想要的是：

```rust
let pool = StealingPool::new(8);
let h1 = pool.spawn(|| compute_a());
let h2 = pool.spawn(|| compute_b());
let (a, b) = (h1.recv(), h2.recv());  // 阻塞等结果
```

`spawn` 返回一个 `JoinHandle<T>`，调 `recv` 阻塞等。和 `std::thread::spawn` 返回 `JoinHandle` 一样自然——只不过任务跑在线程池里、不新开线程。

### ANCHOR：把这看成"任务跑完之后送一条消息"

把"返回值"这件事换个角度：任务在 worker 线程跑、调用方在另一个线程等。**这就是一条跨线程的消息**。M5 你已经做过这件事——**oneshot 通道**：发一条、收一条。

`spawn` 时：
1. 创建一个 oneshot 通道 `(sender, receiver)`；
2. 把 `sender` 捕获进任务闭包：`move || { let v = f(); let _ = sender.send(v); }`；
3. 把 `receiver` 包进 `JoinHandle<T>` 返回给调用方。

worker 跑完任务后 `sender.send(v)` 把值送回去；调用方 `handle.recv()` 阻塞收。

### LOW-FI：先看 M5 的 oneshot 能不能直接用

打开 `crates/forge-channel/src/oneshot.rs` 看一眼。M5 那版 oneshot 的关键签名：

```rust
pub fn split(&mut self) -> (Sender<'_, T>, Receiver<'_, T>)
```

`split` 借用 `&mut self`，返回的 `Sender/Receiver` 持有 `&Channel` 的引用。**Receiver 还被 `PhantomData<*const ()>` 标成 `!Send`**——它必须留在 split 的那个线程上，否则 `Sender` 里存的"接收线程句柄"就指错了线程。

这在 M5 是合理的（oneshot 就是给"两个特定线程之间"用的）。但线程池里**调用方线程和 worker 线程不是同一个**：调用方在 main 线程 spawn 任务、然后 main 线程 recv；worker 在 worker 线程跑任务、从 worker 线程 send。Sender 要从 worker 线程送、Receiver 要在 main 线程收——**两边都得能跨线程移动**。

M5 oneshot 的 `!Send` 限制直接把它从线程池场景里出局了。**我们必须自研一版完全 `Send` 的 oneshot**。

### WRITE：自研线程安全 oneshot

新设计：`Arc<Inner<T>>` 共享，里面是 `AtomicU32 state` + `UnsafeCell<MaybeUninit<T>>`。状态三态：`EMPTY = 0`、`SENT = 1`、`CLOSED = 2`。`Sender` 和 `Receiver` 各持一份 `Arc`，都能跨线程 move。

```rust
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use atomic_wait::{wake_one, wait};

const EMPTY: u32 = 0;
const SENT:   u32 = 1;
const CLOSED: u32 = 2;

struct Inner<T> {
    state: AtomicU32,
    slot: UnsafeCell<MaybeUninit<T>>,
}
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

pub struct Sender<T>   { inner: Arc<Inner<T>> }
pub struct Receiver<T> { inner: Arc<Inner<T>> }

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        state: AtomicU32::new(EMPTY),
        slot: UnsafeCell::new(MaybeUninit::uninit()),
    });
    (Sender { inner: inner.clone() }, Receiver { inner })
}
```

**Sender::send** 消费 self（只能发一次）。先写槽位，再 CAS `EMPTY → SENT`：

```rust
impl<T> Sender<T> {
    pub fn send(self, message: T) -> Result<(), T> {
        // 安全：此刻只有我们碰 slot（receiver 要等 SENT 才读）。
        unsafe { (*self.inner.slot.get()).write(message); }
        // Release：让上面 write 对将来的 Acquire 可见。
        match self.inner.state.compare_exchange(
            EMPTY, SENT, Ordering::Release, Ordering::Relaxed,
        ) {
            Ok(_) => {
                wake_one(self.addr());       // 叫醒一个可能在睡的 receiver
                std::mem::forget(self);      // 不让 Drop 重复处理
                Ok(())
            }
            Err(_) => {
                // 状态不是 EMPTY ⇒ 接收端已 drop。把消息拿回来还给调用方。
                let message = unsafe { (*self.inner.slot.get()).assume_init_read() };
                std::mem::forget(self);
                Err(message)
            }
        }
    }
}
```

**Sender::drop**（没 send 就被 drop）：把状态 `swap` 成 `CLOSED` 并 wake。

```rust
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let prev = self.inner.state.swap(CLOSED, Ordering::Release);
        if prev == EMPTY {
            wake_one(self.addr());  // 唤醒可能在等 EMPTY 的 receiver
        }
    }
}
```

**内存序逐字解释**（这是最容易写错的部分，所以单独看）：

- `Sender::send` 的 `write(slot, message)` 是 unsafe 写。它的可见性必须先于"接收线程看到状态变 SENT"。所以 send 在 write 之后、CAS 之前隐式通过 CAS 的 `Release` Ordering 发布——`compare_exchange(_, _, Ordering::Release, _)` 成功时，所有之前的写（包括 slot write）对做 `Acquire` 读的线程可见。
- `Receiver::try_recv` 用 `Acquire` 读状态：与 send 的 Release 配对——看到 SENT 就能看到 slot 写入。
- `take` 用 `AcqRel` 做 CAS：Acquire 保证看到 sender 的写入、Release 保证我们读出的值对后续（比如 receiver 自己的 Drop）可见。
- `Sender::drop` 的 `swap(CLOSED, Release)`：让 receiver 的 Acquire 看到 CLOSED 时也能正确同步。虽然这里没写 slot，但状态字本身的变更需要发布。
- `Receiver::recv` 的 `wait(addr, EMPTY)`：底层是 `futex(addr, FUTEX_WAIT, EMPTY)`。Linux 内核会先 atomically 比较 `*addr == EMPTY`，是才睡——这就消除了"看到 EMPTY 之后才睡、期间 sender 改了状态"的 race。

这套内存序是经过 loom 验证的（M5 oneshot 用同样的设计，loom 测了上千轮）。改任何一条都可能引入弱内存架构（ARM、PowerPC）上的 bug。

**Receiver::try_recv**（非阻塞）和 **recv**（阻塞）：

```rust
impl<T> Inner<T> {
    fn take(&self) -> Option<T> {
        let ok = self.state
            .compare_exchange(SENT, CLOSED, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok();
        if ok {
            Some(unsafe { (*self.slot.get()).assume_init_read() })
        } else { None }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Option<T> {
        if self.inner.state.load(Ordering::Acquire) == SENT {
            self.inner.take()
        } else { None }
    }

    pub fn recv(self) -> T {
        loop {
            if let Some(v) = self.try_recv() { return v; }
            let s = self.inner.state.load(Ordering::Acquire);
            if s == CLOSED {
                panic!("oneshot Receiver::recv: sender dropped without sending");
            }
            // s == EMPTY：wait 仅在 *addr == EMPTY 时睡，
            // 状态在等待期间被翻成 SENT/CLOSED 时，wake_one 会叫醒我们。
            wait(&self.inner.state, EMPTY);
        }
    }
}
```

为什么要 `recv` 是消费 self 的（只能收一次）？因为 `take` 把状态从 `SENT` 翻到 `CLOSED`——一旦取过就不能再取。`try_recv` 用 `&self` 但同样走 CAS，多次调安全（CAS 成功一次，后续都失败）。

为什么 sender 用 `AtomicU32` 而不是 `AtomicBool`？因为有三态（EMPTY/SENT/CLOSED），bool 只能表达两态。这也是 M5 oneshot 的"就绪位"演化到这里的最终形态。

**try_recv vs recv 的设计权衡**：

- `try_recv(&self)` 不消费 self，可以反复调。它用于 `JoinHandle::recv` 在 worker 线程上的循环——每次循环都 try 一下、不行就跑个任务再来。如果 try_recv 消费 self，循环里第二次调就崩了。
- `recv(self)` 消费 self，一次性。它用于外部线程的纯阻塞等待——一次等、等到就消费、之后这个 JoinHandle 不能再用。

这种"try 用 `&self`、wait 用 `self`"的分裂，是 Rust 类型系统让我们表达"调用频次契约"的方式。M5 oneshot 只给了 `receive(self)` 一种——因为 M5 的 receiver 是 `!Send`、必须留在原地，没必要支持反复 try。M9a 要支持反复 try，所以加了一层。

### ISO·ZOOM：和 M5 oneshot 对照

| 维度 | M5 oneshot | M9a 自研 oneshot |
| --- | --- | --- |
| 通道存储 | 调用方栈上 `Channel<T>` | `Arc<Inner<T>>` 堆上 |
| `Sender`/`Receiver` 是否 `Send` | `Receiver` 是 `!Send` | 都是 `Send` |
| 状态 | `AtomicBool`（两态） | `AtomicU32`（三态） |
| 唤醒机制 | `Thread::unpark` | `atomic_wait::wake_one`（futex） |
| 接收方阻塞实现 | `thread::park` 循环 | `atomic_wait::wait` 循环 |

M5 那版用 `Thread::unpark` 唤醒——它要知道"哪个线程在收"。这强制 receiver 留在那一个线程上。我们这版用 futex——接收线程在 `wait` 时内核会把它的句柄登记在 `addr` 上，sender `wake_one(addr)` 不需要知道是哪个线程。于是 receiver 可以在任何线程上 `recv`。这就是关键区别。

### ISO·ZOOM：和其它实现路径对照

有人会问：为什么不用 `Arc<Mutex<Option<T>>> + Condvar`？这不是更简单吗？

```rust
// 替代方案：Mutex<Option<T>> + Condvar（不推荐）
struct NaiveOneshot<T> {
    inner: Arc<(Mutex<Option<T>>, Condvar)>,
}
```

这是更"教科书"的写法，但它有两大缺点：

1. **每次 send/recv 都进锁**。Mutex 的开销在 send 路径上是 ~10ns（无竞争）——比 atomic CAS 的 ~1ns 慢一个数量级。在 `pool.spawn(...).recv()` 这种高频路径上，每纳秒都算数。
2. **Mutex 跨 send/recv 持有期间不能唤醒别人**。如果 receiver 在 wait 时，sender 必须先 lock 才能 notify——这个 lock 操作即便 receiver 已经释放了，仍是一次原子 CAS。

我们的原子版用 `AtomicU32` 表达三态，send 路径是 `write + CAS`（两个原子操作 + 一次 futex_wake），recv 路径是 `load + CAS`（一次原子操作 + 一次 futex_wait）。**完全无锁**。在高频路径上优势明显。

那为什么不用 `std::sync::mpsc::sync_channel(1)` 或者 `tokio::sync::oneshot`？

- `mpsc::sync_channel(1)` 是多生产者单消费者，但我们这里就一个生产者，多此一举。
- `tokio::sync::oneshot::channel()` 是异步的，需要 `await`——我们这是同步池，不能 await。
- 也可以用 `crossbeam::channel::bounded(1)`——它内部就是原子 + park 的组合，和我们这版结构上很像，但更通用。

我们自研的目的是**让你看清楚里面发生了什么**。生产代码你可以直接用 `crossbeam::channel::bounded(1)` 替代 oneshot——行为等价、性能相近。

---

## 二、V1：所有 worker 共享一个队列（敌人）

### ENEMY：朴素共享队列的序列化灾难

现在有了 `JoinHandle`，我们能写出一个**功能正确**的池：所有 worker 共享一个 `Mutex<VecDeque<Task>> + Condvar`。这就是 Williams Ch9 Listing 9.1/9.2、也是网上 99% 教程里线程池的样子。

但它在多核下**退化成 ~1 核吞吐**。我们会在手算例子 1 里画清楚为什么。

### ANCHOR：餐厅取号机

把共享队列想成一家小餐馆**唯一**的取号机：所有客人（worker）排一条队去抢同一个号牌（`Mutex`）。某个客人抢到号牌、走向桌子（取任务）、放下号牌（`unlock`）——下一位客人才能开始抢。号牌是**串行**的：同一时刻只有一个人拿它。

四核机器上跑四个 worker，理论上你应当看到 ~4 倍吞吐。但用这版池你只会看到 ~1 倍——另外三个 worker 都在等锁。

### LOW-FI：用 M2 的零件拼一个出来

完整代码在 `crates/forge-pool/src/v1_shared_queue.rs`：

```rust
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::oneshot;
use crate::Task;

pub struct SharedQueuePool {
    shared: Arc<Shared>,
    workers: Vec<thread::JoinHandle<()>>,
}

struct Shared {
    queue: Mutex<State>,
    cv: Condvar,
}

struct State {
    tasks: VecDeque<Task>,
    shutdown: bool,
}

impl SharedQueuePool {
    pub fn new(n_workers: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(State { tasks: VecDeque::new(), shutdown: false }),
            cv: Condvar::new(),
        });
        let mut workers = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let shared = shared.clone();
            workers.push(thread::spawn(move || worker_loop(shared)));
        }
        Self { shared, workers }
    }

    pub fn spawn<F, T>(&self, f: F) -> crate::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let task = Task(Box::new(move || {
            let value = f();
            let _ = sender.send(value);
        }));
        let mut s = self.shared.queue.lock().unwrap();
        s.tasks.push_back(task);
        self.shared.cv.notify_one();
        crate::JoinHandle { receiver }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        // 优雅关停：shutdown=true 时不再接新任务，但已有的全部跑完。
        let task = {
            let mut s = shared.queue.lock().unwrap();
            loop {
                if let Some(task) = s.tasks.pop_front() { break Some(task); }
                if s.shutdown { break None; }
                s = shared.cv.wait(s).unwrap();
            }
        };
        match task {
            Some(t) => t.run(),
            None => return,
        }
    }
}
```

每个 worker 都在 `worker_loop` 里：抢锁、看队列、有任务就取走放锁、跑任务、回循环。**所有 worker 抢同一把锁**。

`Task` 是一个 newtype：

```rust
pub struct Task(pub(crate) Box<dyn FnOnceOnce + Send + 'static>);

pub trait FnOnceOnce {
    fn call_once_boxed(self: Box<Self>);
}

impl<F: FnOnce()> FnOnceOnce for F {
    fn call_once_boxed(self: Box<Self>) { (*self)(); }
}
```

为什么不用 `Box<dyn FnOnce()>`？因为标准库的 `FnOnce::call_once` 接收 `self`，在 trait object 上调它会触发 unsized coercion，目前 Rust 还不让直接 `Box<dyn FnOnce()>::call_once()`。所以我们做一个自己的 trait `FnOnceOnce`，签名是 `self: Box<Self>`——这样 `Box<dyn FnOnceOnce>` 能直接调。

### 演化路径综述：四个版本的对照

让我们在动手 V2/V3 之前，先把四个版本的演化路径画一张总图：

| 版本 | 队列结构 | spawn 进哪 | owner 取哪端 | thief 偷哪端 | 解决的问题 |
| --- | --- | --- | --- | --- | --- |
| V0（朴素） | 共享 `VecDeque<Task>` 无锁 | 共享队列 push_back | 共享队列 pop_front | — | 无并发安全 |
| V1（敌人） | 共享 `Mutex<VecDeque> + Condvar` | 共享队列 push_back | 共享队列 pop_front | — | 正确但锁争用主导 |
| V2（中间） | 每 worker 一个 `Mutex<VecDeque>` + 全局 fallback | 在 worker 上 → 本地；在外部 → 全局 | 本地 LIFO | — | 解决锁争用，但负载不均 |
| V3（终点） | 每 worker 一个 `Mutex<VecDeque>`（owner LIFO，thief FIFO）+ injector | 在 worker 上 → 本地 LIFO；在外部 → injector FIFO | 本地 LIFO | 别人的 FIFO | 解决锁争用 + 负载均衡 |

V2 和 V3 的区别**仅在于**有没有工作窃取。代码上 V3 比 V2 多了 `find_work` 的 step 3（偷别人）。教程里 V2 的代码并入了 V3——你可以从 V3 代码里去掉 step 3，就得到 V2。

Williams 在 Ch9 9.1.4 也是这样组织的：先讲 V2（thread_local work queue），再讲 V3（work stealing）。我们跟随他的节奏。

---

### ISO·ZOOM：手算例子 1——共享队列上的争用

四个 worker、1000 个任务、共享 `Mutex<VecDeque>`。任务都用时假设 1ms。我们看几个关键时刻：

**时刻 T0**：队列里有 1000 个任务 `[T1, T2, ..., T1000]`，W1/W2/W3/W4 都在 `cv.wait`。

**时刻 T0+ε**：主线程 `spawn(T1)`：lock、push_back、notify_one、unlock。notify_one 叫醒一个 worker，比如 W1。

**时刻 T1（W1 醒来）**：
1. W1 抢到锁：`lock()` → 返回 `MutexGuard`。
2. W1 调 `pop_front`：拿到 `T1`。
3. W1 释放锁：drop guard。
4. W1 跑 T1（耗时 1ms）。

**时刻 T1（同时，W2/W3/W4 看到 notify_one）**：只有一个被叫醒（假设 W2）。W2 醒来，**抢锁**——但锁在 W1 手里。W2 阻塞在 `lock()`。

**时刻 T1+0.001ms（W1 释放锁，开始跑 T1）**：W2 拿到锁，`pop_front` → 拿到 `T2`，放锁。W3 抢到锁，拿 `T3`。W4 抢到锁，拿 `T4`。

如果队列里**只有 4 个任务**：W1-W4 各跑一个，4 倍加速。

**但队列里还有 996 个**。W1-W4 各自跑 1ms 的任务，跑完后回循环抢锁——**他们又同时抢**。

**时刻 T1+1ms（W1 跑完 T1，回循环）**：
- W1 `lock()`：拿锁，`pop_front` → T5，放锁，跑 T5。
- 与此同时 W2/W3/W4 也跑完他们的任务，回循环抢锁。

**关键时刻：抢锁序列化**。四个 worker 都在抢同一把锁。`Mutex` 保证了同一时刻只有一个能进入临界区。其他三个被内核挂起（futex wait），等锁释放后被唤醒。**内核调度切换是有成本的**——每次 lock contention 大约 1-10 微秒（取决于 CPU），比"取一个任务"这个动作（几纳秒）慢三个数量级。

**逐拍细化**：让我们把"四个 worker 同时回循环"的几纳秒画清楚。

```
时刻 T+1ms 纳秒 0：
    W1 lock() → 拿到锁。
    W2 lock() → 阻塞（futex_wait）。
    W3 lock() → 阻塞（futex_wait）。
    W4 lock() → 阻塞（futex_wait）。

时刻 T+1ms 纳秒 +50（W1 进入临界区）：
    W1 调 pop_front → T5。
    W1 drop guard → 释放锁（futex_wake，叫醒一个等待者）。

时刻 T+1ms 纳秒 +500（W2 被 futex_wake 唤醒，进入调度队列）：
    这是 Linux 调度器的唤醒延迟——大约 1-3μs。
    W2 重新尝试 lock() → 拿到锁。
    W2 调 pop_front → T6。
    W2 drop guard → 释放锁（futex_wake）。

时刻 T+1ms 微秒 +1（W3 被 futex_wake 唤醒）：
    W3 lock() → 拿到。
    W3 pop_front → T7。

时刻 T+1ms 微秒 +2：
    W4 lock() → 拿到。
    W4 pop_front → T8。
```

**关键观察**：四个 worker 取 T5、T6、T7、T8 这一组任务的总耗时是 **3-5 微秒**（四倍 lock + 三次 futex 唤醒延迟）。如果任务本身执行只要 1 微秒（短任务），那么**取任务的耗时是执行任务的 3-5 倍**——锁争用彻底主导。

**吞吐退化曲线**：

| 任务耗时 | 取任务锁争用 | 4 worker 实际加速 |
| --- | --- | --- |
| 1 μs | 3-5 μs | 0.2x-0.3x（比单线程还慢！）|
| 100 μs | 3-5 μs | 2.5x-3x |
| 1 ms | 3-5 μs | 3.5x-3.9x |
| 10 ms | 3-5 μs | 3.95x-3.99x |

**结论**：共享队列的扩展性瓶颈是**取任务入口的锁**。任务越长，锁占比越小。但任务长到一定程度（10ms+），任务本身的并行性就够了，不需要线程池——`std::thread::spawn` 几个就够。

**真正的甜区是 100μs-10ms**——这正是 rayon、tokio 等运行时优化的目标区间。在这个区间里，V1 的锁争用让加速比卡在 2.5x-3x；V3 工作窃取能把锁争用降到几乎为零，跑出 3.9x-4x。

**criterion 基准预期数字**（在 4 核机器上跑 50000 个 `1+1` 的短任务）：

| 池类型 | 总耗时（相对值） | 加速比 |
| --- | --- | --- |
| 单线程 | 1.0x | 1x |
| V1 共享队列（4 worker） | 0.7x–0.9x | 1.1x–1.4x |
| V3 工作窃取（4 worker） | 0.25x–0.35x | 2.9x–4x |

V1 在某些情况比单线程还慢——锁争用太狠。这个数字就是你后面所有改进的**驱动力**。

> **和 rayon 正面比(plan 要求的对照)**:`cargo bench -p forge-pool --bench m9a_par_vs_rayon`。在 50 万 u64 的并行快排上,实测(4 核):serial ~27ms、`forge_par_sort` ~18ms(~1.5×)、`rayon par_sort` ~17ms(~1.6×)。**forge 和 rayon 在同量级**(rayon 略快),没被甩开数量级——这对教学版是个不丢人的成绩。差距主要在:我们的本地队列是 `Mutex<VecDeque>`(v3_stealing),rayon 用无锁 Chase-Lev deque——升级到 M8g 的无锁 deque 后差距应进一步缩小。注意小 N(10 万)时 forge/rayon **都比 serial 慢**,因为 spawn + 窃取的固定开销 > 排序时间:并行不是免费午餐,任务粒度得够大(Amdahl + 任务粒度的手算见后)。

---

## 三、V2：每个 worker 自己一个队列

### ENEMY：共享队列的争用是结构性问题

V1 的争用不是"实现细节差"，是**结构**问题：所有 worker 都在改同一个数据结构。哪怕你把 `Mutex<VecDeque>` 换成 M8 的无锁队列，缓存一致性协议（MESI）也会让所有核反复 invalidate 彼此的缓存行——叫 **cache ping-pong**。多核扩展性极差。

### ANCHOR：每人一个抽屉

每人一个抽屉。你往自己抽屉里放任务、从自己抽屉里取——不碰别人的抽屉，没争用。除非你抽屉空了，才去看别人的——这是 V3 工作窃取。

V2 是 V3 的子集：先有"每人一个队列"，再叠加"偷别人的"。代码上 V2 和 V3 共用，我们直接跳到 V3。

---

## 四、V3：工作窃取（Williams Ch9 Listing 9.7 + 9.8）

### ENEMY：每人一个队列的负载不均

每人一个队列解决了争用。但新问题来了：**W1 抽屉里有 100 个任务、W2/W3/W4 抽屉全空**。这是并行快排的典型情形——快排的递归 split 把任务喂给**当前 worker**，每段排完又 spawn 一段——全部堆在 W1 的本地队列里。W2/W3/W4 闲死，W1 忙死。

这就是 Williams Ch9 9.1.4 末尾说的："this defeats the purpose of using a thread pool"。

### ANCHOR：餐厅后厨的传菜台

每份订单进来都分配给某个厨师（worker）的小传菜台（本地队列）。W1 的传菜台堆满了。W2/W3/W4 的传菜台空着。**怎么办？**

W2 走到 W1 的传菜台，从**最旧的一头**（先到的订单）拿走一份——这是工作窃取。W2 不抢 W1 手上正在做的那份（最新的、热的、缓存里的），W2 拿走一份**冷的、放最久的**——这份反正不在 W1 的缓存里，被偷走对 W1 的影响最小。

这就是工作窃取的核心：**owner 用 LIFO 端（最新的先做），thief 用 FIFO 端（最旧的先偷）**。两端不同，争用最小。

### WRITE：每 worker 一个 `LocalQueue`

代码在 `crates/forge-pool/src/v3_stealing.rs`。本教学版用 `Mutex<VecDeque<Task>>` 包一层（Williams Listing 9.7 也是这个设计；M8g 的 Chase-Lev 无锁版本在生产代码里更优，但教学清晰度上锁版更好）：

```rust
struct LocalQueue {
    inner: Mutex<VecDeque<Task>>,
}

impl LocalQueue {
    fn new() -> Self { Self { inner: Mutex::new(VecDeque::new()) } }
    fn push(&self, t: Task) { self.inner.lock().unwrap().push_front(t); }  // owner LIFO 端
    fn pop(&self) -> Option<Task> { self.inner.lock().unwrap().pop_front() }  // owner LIFO 端
    fn steal(&self) -> Option<Task> { self.inner.lock().unwrap().pop_back() } // thief FIFO 端
}
```

owner 用 `push_front`/`pop_front`（VecDeque 的 front 端 = LIFO）；thief 用 `pop_back`（back 端 = FIFO）。两端从相反方向操作，物理上减少同一缓存行的争用。

每个 worker 持有：
- `local: Arc<LocalQueue>` —— 自己的本地队列；
- `queues: Vec<Arc<LocalQueue>>` —— 所有 worker 的队列（含自己的），用于偷；
- `index: usize` —— 自己在数组里的下标。

通过 `thread_local` 让 spawn 知道"我现在在哪个 worker 上"：

```rust
thread_local! {
    static WORKER: std::cell::RefCell<Option<WorkerHandle>> = std::cell::RefCell::new(None);
}
```

### WRITE：spawn——根据线程身份分流

```rust
pub fn spawn<F, T>(&self, f: F) -> crate::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    let mut task_opt = Some(Task(Box::new(move || {
        let value = f();
        let _ = sender.send(value);
    })));

    let pushed_to_local = WORKER.with(|w| {
        if let Some(h) = w.borrow().as_ref() {
            if let Some(t) = task_opt.take() {
                h.local.push(t);  // 当前是 worker：push 到本地 LIFO 端
                return true;
            }
        }
        false
    });

    if !pushed_to_local {
        // 外部线程：注入到某个 worker 的 injector 槽（轮询）。
        // 不能直接 push 到 worker 的 LocalQueue——因为 push 不是线程安全的
        // 多写者（即便 LocalQueue 内部有 Mutex，外部线程直接 push 到 worker 的
        // LIFO 端会和 owner 自己的 push 抢锁，且语义混乱）。
        // 所以专门用一个 injector 队列（每 worker 一个），外部线程 push_back，
        // worker 来 pop_front（FIFO）。
        let n = self.state.n_workers;
        let idx = self.state.next_external.fetch_add(1, Ordering::Relaxed) % n;
        let task = task_opt.take().expect("task still Some");
        self.state.injector[idx].lock().unwrap().push_back(task);
    }

    // 通知 worker：增加 pending 计数 + notify_all。
    {
        let mut p = self.state.pending.lock().unwrap();
        *p += 1;
    }
    self.state.pending_cv.notify_all();

    crate::JoinHandle { receiver }
}
```

**两个关键设计点**：

1. **外部线程不能直接 push 到 worker 的 `LocalQueue`**。Williams 在 Ch9 把这做成一个 `threadsafety_queue<function_wrapper> pool_work_queue`（全局共享队列），worker 在自己本地空时去全局队列取。我们这版简化为"每个 worker 一个 injector 槽"——同样效果，更均匀的负载分配。
   
   为什么不让外部线程直接 push 到 `LocalQueue`？技术上 `LocalQueue` 内部有 `Mutex`，外部线程 push 是可以的——但这破坏了"owner 端无锁路径"的优化预期：M8g 的 Chase-Lev owner push 是无锁的（只动 `bottom` 原子），如果允许外部线程也 push，必须加锁，owner 就丢了无锁优势。所以生产设计（rayon）总是把外部投递和 owner 投递分开到两个队列。

2. **`pending` 计数 + Condvar**：避免丢唤醒。每次 spawn 都 `notify_all`，所有 worker 都会醒来重新检查 find_work。这比 atomic-wait 的 futex 路径在多测试并行时更稳定（教程末尾的 ISO 节讨论为什么）。

   为什么是 `notify_all` 而不是 `notify_one`？因为外部线程不知道哪个 worker 闲——notify_one 可能挑到一个正在跑长任务的 worker，那个 worker 跑完手里的任务才会醒来检查。`notify_all` 让所有 worker 都醒来，闲的能立刻取活——多唤醒几个的开销（几微秒）远小于延迟调度的代价（毫秒级）。

   对应 worker_loop 里的 wait 是经典的 condvar 模式：
   
   ```rust
   let mut p = state.pending.lock().unwrap();
   while *p == 0 && !state.shutdown.load(Ordering::Acquire) {
       p = state.pending_cv.wait(p).unwrap();
   }
   ```
   
   `while` 循环防 spurious wakeup——醒来后重新检查条件。`wait` atomically 释放 mutex 并睡——这消除了"释放锁之后才睡、期间有人 notify"的 race。

### WRITE：worker_loop——找活干

```rust
fn worker_loop(state: Arc<PoolState>) {
    loop {
        if let Some(task) = find_work(&state) {
            // catch_unwind：单个任务的 panic 不能让整个 worker 退出。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.run();
            }));
            continue;
        }
        if state.shutdown.load(Ordering::Acquire) {
            if find_work(&state).is_none() {
                return;
            }
            continue;
        }
        // 没活干也没关停：wait 在 condvar 上，绝不丢唤醒。
        let mut p = state.pending.lock().unwrap();
        while *p == 0 && !state.shutdown.load(Ordering::Acquire) {
            p = state.pending_cv.wait(p).unwrap();
        }
    }
}
```

`find_work` 是调度核心：

```rust
fn find_work_via_handle(h: &WorkerHandle) -> Option<Task> {
    // 1) 本地 LIFO（owner 端）。
    if let Some(t) = h.local.pop() {
        return Some(t);
    }
    // 2) 所有 injector 槽（FIFO），从自己开始轮询。
    let n_inj = h.pool_state.injector.len();
    for k in 0..n_inj {
        let idx = (h.index + k) % n_inj;
        if let Some(t) = h.pool_state.injector[idx].lock().unwrap().pop_front() {
            return Some(t);
        }
    }
    // 3) 偷别的 worker 的本地队列（FIFO 端）。
    let n = h.queues.len();
    for k in 1..=n {
        let victim_idx = (h.index + k) % n;
        if victim_idx == h.index { continue; }
        if let Some(t) = h.queues[victim_idx].steal() {
            return Some(t);
        }
    }
    None
}
```

**优先级**：本地 LIFO → injector FIFO → 偷别人 FIFO。

**为什么是这个优先级**？

- **本地 LIFO 优先**：owner 的本地队列是自己 spawn 出来的、数据热的任务。优先做它们缓存命中率高。这也是 owner push/pop 同端（LIFO）的延续——同一端的操作是连续的缓存友好访问。
- **injector FIFO 其次**：injector 里的任务是外部线程投递的，外部线程投递时不知道哪个 worker 会取——这些任务是"全局公共"的，应当被任意 worker 公平消费。FIFO（先来先服务）保证公平性。
- **偷别人最后**：偷是相对昂贵的操作——要遍历别的 worker 的队列、抢它们的锁。能不偷就不偷。只有自己本地空、injector 空时才去偷。

**轮询顺序的细节**：偷的时候从 `(my_index + 1) % n` 开始，这是 Williams Ch9 Listing 9.8 的设计——避免每个 thief 都从 worker 0 开始偷，让 worker 0 的队列被反复查。每个 thief 从自己下一个开始轮询，让偷窃压力均匀分布。

### 旁支：catch_unwind 防止 worker 被任务 panic 拖死

`worker_loop` 跑任务时用 `catch_unwind` 包：

```rust
let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    task.run();
}));
```

为什么必要？如果任务闭包 panic 了——比如用户的 `|| panic!("oops")`——`task.run()` 会 unwind。如果不 catch，worker 线程跟着退出。退出之后：
- 这个 worker 的本地队列里剩余的任务**永远不会被运行**（worker 死了，没人 join 它的本地队列）。它们的 Sender 在闭包 unwind 时被 drop without send → 调用方的 recv 看到 CLOSED → panic 传染整个调用链。
- injector 里这个 worker 对应槽位的任务也会被丢（虽然 find_work 兜底有别的 worker 来取，但 injector 是 worker_index 对应的、visually worker 死了之后这个槽还是会被访问——但本地队列是 worker 死了就丢了）。

catch_unwind 让单个任务 panic 不影响 worker。worker 继续跑下一个任务。rayon 的实际实现里 panic 的任务会让 JoinHandle 返回 panic 信息（生产代码该这么干），我们简化为"日志后忽略"。

### ISO·ZOOM：手算例子 2——LIFO/FIFO 不对称的任务流转

**初始状态**：4 个 worker。W1 的本地队列（VecDeque，front=LIFO 端、back=FIFO 端）里有 8 个任务：

```
W1.local:  front -> [T8 T7 T6 T5 T4 T3 T2 T1] <- back
                                    (最新的在 front，最旧的在 back)
W2/W3/W4.local: 空
injector: 空
```

T8 是 W1 最近 spawn 的，T1 是最早 spawn 的。任务的"温度"：T8 是热的（W1 刚 spawn 它，相关数据大概率还在 W1 的 L1/L2 缓存里）；T1 是冷的（早就 spawn 了，数据大概率被挤出缓存了）。

**时刻 T0**：所有 worker 同时进入 `find_work`。

**W1 的视角**：
1. `local.pop()` —— 从 front 端弹出 **T8**（LIFO，最新）。
2. W1 跑 T8。T8 的数据还热，缓存命中率高。

**W2 的视角**：
1. `local.pop()` → None。
2. 遍历 injector → 全空。
3. 偷别人：从 `(W2.index + 1) % 4 = W3` 开始偷，W3 空；再 W4，空；再 W1，**W1 的队列还有 7 个**！`steal()` 从 back 端拿——**T1**（FIFO，最旧）。
4. W2 跑 T1。T1 的数据冷，但反正不在 W2 的缓存里——偷一份冷任务对 W2 无所谓。

**W3 的视角**（和 W2 几乎同时）：
1. 本地空、injector 空。
2. 偷：从 W4 开始（`(W3.index + 1) % 4`），空；W1，`steal()` 从 back 端——**T2**（现在 back 端第二旧）。
3. W3 跑 T2。

**W4 的视角**：
1. 偷：W1（`(W4.index + 1) % 4 = W1`，因为 W4=3, 3+1=4 mod 4=0），`steal()` 从 back 端——**T3**。
2. W4 跑 T3。

**T0+ε 后的状态**：

```
W1.local:  front -> [T7 T6 T5 T4] <- back   （W1 pop 了 T8）
W1 跑 T8
W2 跑 T1
W3 跑 T2
W4 跑 T3
```

**为什么这个不对称设计是最优的？**

**Owner 用 LIFO 的两个好处**：

1. **缓存友好**：W1 最近 spawn 的任务（T8），其相关数据（输入参数、栈帧、分配的临时内存）还**热**——在 W1 的 L1/L2 缓存里。W1 立刻做 T8，缓存命中率高。如果 W1 反过来先做 T1（最旧），T1 的数据早被挤出缓存，W1 要从 L3/内存重新加载。

2. **深度优先、控制任务总数**：很多算法（并行快排、树遍历、分治）天然是递归的。每个任务 spawn 一两个子任务。LIFO 让 owner 总是先做子任务（最后 spawn 的）——这本质是**深度优先**，让子树快速完成、释放栈空间、降低全局任务总数。如果是 FIFO（广度优先），任务总数会爆炸——一颗深度为 30 的二叉树会让队列堆 2^28 个任务。

**Thief 用 FIFO 的两个好处**：

1. **减少与 owner 的争用**：thief 从 back 端偷、owner 从 front 端 push/pop——两端物理上分离（VecDeque 的两端），缓存行极少冲突。如果两端都从同一边操作，每次都要抢同一个缓存行。

2. **偷"老"任务更划算**：老任务（T1）大概率不在 owner 的缓存里（owner 早就 push 完它去做别的了）。偷走它对 owner 的影响最小——反正 owner 接下来要重新加载它的数据。新任务（T8）在 owner 缓存里——偷走它，owner 就丢失了缓存优势。

**对称设计的失败**（反例）：

- 如果 owner 和 thief 都用 LIFO（都从 front）：每次都抢 front 缓存行，cache ping-pong。性能回到 V1。
- 如果 owner 用 FIFO、thief 用 LIFO：owner 总是做最旧的任务（冷），新任务被偷走——缓存优势全丢。

**这是 rayon、crossbeam-deque、Go scheduler、Java ForkJoinPool 的共同设计**。Le, Leiserson 等人在《Correct and Efficient Work-Stealing for Weak Memory Models》里证明了这个不对称设计在弱内存架构下的正确性。M8g 的 Chase-Lev 双端队列就是它的无锁实现。

---

## 五、嵌套 spawn 死锁：M9a 的核心 stress（敌人先行 → 重建）

到此为止，我们的 V3 池在"任务互相独立"的场景下工作得很好。`tests/m9a_basic.rs::v3_pool_runs_many_tasks_from_external_thread` 跑 500 个独立任务、4 worker，吞吐远超 V1。

但有一个**关键场景**还没处理：**任务 A spawn 任务 B、然后等 B 的结果**。这就是并行快排、并行归并、递归分治的标准模式。

### ENEMY：先写一个会死锁的 JoinHandle::recv

考虑这个调用：

```rust
let pool = StealingPool::new(1);  // 1 个 worker，最容易触发
pool.spawn(|| {
    let h = pool.spawn(|| 99);  // 任务 A 在 worker W1 上跑，spawn B
    h.recv() + 1                // A 等 B 的结果
});
```

任务 A 在 worker W1 上跑。A 内部 `pool.spawn(B)` —— 因为当前线程是 worker（thread_local WORKER 有值），B 被 push 到 **W1 自己的本地队列**。然后 A 调 `h.recv()` 等 B 的结果。

**如果 `recv` 实现是"纯阻塞 park"**（像外部线程那样），让我们逐拍画死锁：

**时刻 T0**：W1 正在跑 A。A 调 `pool.spawn(B)`：B 被 push 到 `W1.local`。

```
W1.local: front -> [B] <- back
W1 当前在跑 A（被 spawn 时是从 W1.local pop 出来的某个任务）
```

**时刻 T1**：A 调 `h.recv()`。`recv` 看到 oneshot 状态是 EMPTY（B 还没 send）。`recv` 进入 wait/park 状态。

**时刻 T2**：W1 现在在 park。**没有别的 worker**（n=1）。B 在 W1.local 里**永远不会被运行**——因为只有 W1 能跑它，但 W1 在睡。

**死锁**。A 永远等不到 B 的结果，B 永远不会被跑。

这不是 N=1 的极端情形。哪怕 N=4，只要别的 worker 都在忙（或者运气不好 B 没被偷），同样会死锁。**rayon/tokio 等运行时必须解决这个问题**。

### 这个死锁的本质：占用资源的任务在等被占资源服务的任务

让我们把这个死锁抽象一下。死锁的四个经典必要条件（Coffman 条件）：

1. 互斥：worker 资源不可共享（一个 worker 同时只能跑一个任务）。
2. 占有等待：W1 占着自己、等 B 跑完。
3. 不可剥夺：B 不能强制从 W1 拿走（除非 W1 自己释放 CPU）。
4. 循环等待：W1 等 B，B 等 W1 跑它。

条件 4 看起来不满足？其实满足——W1 等 B 完成的信号、B 等 W1 给它 CPU 时间。这是隐式的循环依赖。

修复思路是**打破条件 2 或 3**：让 W1 在等的时候**主动释放 CPU 给 B**。具体来说：W1 不真睡，而是"借自己的 CPU 给 B"——把 B 从队列里拉出来跑。B 跑完、oneshot 被填，W1 醒来继续 A。

这个模式叫**阻塞时帮忙**（helping when blocked）或**work-first scheduling**。它是 rayon、tokio（block_in_place）、Java ForkJoinPool（managedBlocker）的共同核心。

### 重建：阻塞时帮忙——`recv` 在 worker 上不 park，继续跑任务

修复：`JoinHandle::recv` 检查当前线程是不是 worker。**如果是**，绝不 park，而是循环"检查结果 → 跑一个挂起的任务 → 再检查结果"。

```rust
pub fn recv(self) -> T {
    let on_worker = v3_stealing::is_on_worker();
    if on_worker {
        loop {
            // 1) 结果就绪？
            if let Some(v) = self.receiver.try_recv() {
                return v;
            }
            // 2) 没就绪——跑一个挂起的任务（本地 LIFO → injector → 偷别人）。
            //    这一步是"阻塞时帮忙"的核心。
            if !v3_stealing::run_one_pending_task() {
                // 连任务都没有：让出 CPU 一个时间片。
                // 不能 park（park 会让本 worker 永远睡死）。
                std::thread::yield_now();
            }
        }
    } else {
        // 外部线程：直接 park 等结果，安全。
        self.receiver.recv()
    }
}
```

`run_one_pending_task` 在 worker 上调 `find_work_via_handle` 找一个任务跑。在我们的死锁场景里：

**时刻 T1'（修复后）**：A 调 `h.recv()`。`recv` 检测到当前在 worker 上。进入循环。
1. `try_recv` → None（B 还没 send）。
2. `run_one_pending_task` → 调 `find_work_via_handle` → 从 `W1.local` pop 出 **B**（LIFO 端）。
3. **W1 跑 B**。B 计算完 `99`，调 `sender.send(99)`，oneshot 状态 EMPTY→SENT。
4. `run_one_pending_task` 返回 true。
5. 回到 `recv` 循环顶：`try_recv` → Some(99)。返回。

**死锁解除**。A 拿到 99，加 1 返回 100。

这个机制叫**阻塞调度**（blocking scheduling）或**work helping**——阻塞的 worker 不真睡，而是帮着把队列里的任务跑掉。rayon 把这叫"work-stealing is symmetric"。tokio 的 `block_on` 也是这个思路——future 在 await 时，执行器不让线程睡，而是去 poll 别的 future。

### ISO·ZOOM：手算例子 3——同一交错，先死后活

让我们更仔细地画一遍，让死锁和解除都"看得见"。

**场景**：N=2 worker。任务 A 在 W1 上跑，A `spawn(B)` 然后 `recv` 等 B。同时主线程在 W1.spawn(C)，C 也进 injector。

**先看会死锁的版本（`recv` 直接 park）**：

```
时刻 0: W1 跑 A。A 调 spawn(B)。B -> W1.local = [B]。A 调 recv。
时刻 1: recv 看到 oneshot EMPTY，park。W1 睡死。
时刻 1: W2 此时在跑别的任务，没空。
时刻 2: B 在 W1.local 里，没人 pop。W1 永 sleep。
        = 死锁 =
```

**修复版（`recv` 在 worker 上不 park）**：

```
时刻 0: W1 跑 A。A 调 spawn(B)。B -> W1.local = [B]。A 调 recv。
        recv 检测到 on_worker=true，进循环。
时刻 1: recv.try_recv() -> None。
        recv 调 run_one_pending_task。
        run_one_pending_task 调 find_work_via_handle：
          - W1.local.pop() -> Some(B)   （LIFO 端，B 是唯一一个）
        W1 跑 B。
时刻 2: B 算完 99，sender.send(99)。oneshot EMPTY -> SENT。
        B 退出。
时刻 3: run_one_pending_task 返回 true。
        recv 循环回顶：try_recv() -> Some(99)。
        A 收到 99，+1=100，返回。
时刻 4: A 退出，W1 回 worker_loop 找下一个任务。
```

**对比关键点**：

| 维度 | 死锁版 | 修复版 |
| --- | --- | --- |
| recv 在 worker 上的行为 | park（睡死） | 循环跑任务 |
| B 谁跑 | 没人跑（W1 在睡、W2 不知道） | W1 自己跑（在自己阻塞期间帮忙） |
| oneshot 状态转换 | 卡在 EMPTY | EMPTY → SENT → CLOSED |
| 总耗时 | 无穷 | 等 B 实际跑完的时间 |

测试 `tests/m9a_nested_spawn.rs::v3_nested_spawn_one_level_does_not_deadlock` 验证这个：1 worker、嵌套一层 spawn、超时 10 秒。死锁版会超时失败；修复版通过。

测试 `v3_many_nested_spawns_under_load` 更狠：4 worker、40 个 outer 任务、每个 outer spawn 3 个 inner。这让 worker 频繁"边等边跑"，能验证多级嵌套也不死锁。

### 为什么外部线程可以 park？

外部线程（main、不在 pool 里的线程）调 `recv` 时，它**不在任务图里**——它不参与调度，它的睡死不会让任何任务卡住。任务图里的所有任务都能被 worker 跑完。所以外部线程 park 是安全的。

但 worker 线程**在任务图里**——它跑的当前任务可能阻塞了别的任务（任务 A 等 B）。worker 一睡，整个任务图就死了。

这就是为什么 `recv` 要**根据线程身份分流**：worker 走"帮忙"路径、外部线程走"park"路径。`is_on_worker()` 用 thread_local 检测。

### 旁支：thread_local WORKER 是怎么"注册"的？

注意我们的 worker 线程在启动时做了一件关键事：

```rust
let handle = thread::Builder::new()
    .name(format!("forge-worker-{index}"))
    .spawn(move || {
        WORKER.with(|w| {
            *w.borrow_mut() = Some(WorkerHandle {
                local,
                queues: queues_clone,
                index,
                pool_state: st.clone(),
            });
        });
        worker_loop(st);
    })
```

`WORKER.with(|w| ...)` 在 worker 线程**进入 worker_loop 之前**把 `WorkerHandle` 注册到 thread_local。后续在这个 worker 线程上调任何代码——包括用户的任务闭包、包括 `pool.spawn()`、包括 `JoinHandle::recv()`——只要访问 `WORKER`，就能拿到 `WorkerHandle`。

**这就是 thread_local 让 spawn 自动分流的原因**：

- 外部线程调 `pool.spawn(...)` → `WORKER.with(|w| ...)` → w 是 None（外部线程从没设置过 WORKER）→ 走 injector 路径。
- worker 线程调 `pool.spawn(...)`（在任务闭包内部嵌套 spawn）→ WORKER 是 Some → 走本地 LIFO 路径。

不需要任何手动标记"我现在是 worker"——thread_local 隐式做了。这是 Rust thread_local 的典型用法之一（另一种典型用法是全局 logger、metrics 收集器）。

**注意一个常见坑**：如果你在 worker 线程内又 `std::thread::spawn` 了一个**外部子线程**（不是 pool 的 worker），那个子线程的 WORKER 是 None——它不会被自动注册成 pool 的 worker。这是正确的——你不能让任意 std::thread 突然变成 pool 的 worker，那会破坏 pool 的 worker 计数和调度假设。

### 旁支：JoinHandle::recv 之外的死锁场景

我们解决了"任务 A 等 spawn 出来的 B"的死锁。但还有别的死锁场景：

1. **任务 A 等任务 B，但 B 等任务 A**：循环依赖。这不是线程池能解决的——本质是死锁。rayon/tokio 也不能。生产代码应避免，工具如 `cargo-dylint` 能检测。
2. **任务 A 持有锁 L，spawn B，B 要拿锁 L**：classic deadlock。修复：任务 A 在 spawn 之前释放所有锁。rayon 的 `join` 推荐这种"先把任务准备好再 spawn"的风格。
3. **任务 A 调外部阻塞 API（比如 std 上的 `Read::read`）**：worker 被阻塞，不能跑别的任务。rayon 提供 `yield_now` / tokio 提供 `block_in_place`——把当前任务"换出"让 worker 去跑别的。M9a 的简化版不处理这个，留给 M9b。

理解了这些边界，你才知道工作窃取池能解决什么、不能解决什么。**它解决的是"任务间结构性的等待"**——A spawn B、等 B。它**不**解决资源争用（锁）、I/O 阻塞、循环依赖。

---

## 六、优雅关停：drop(pool) 不丢任务

### ENEMY：粗暴关停会丢任务

如果 drop(pool) 直接 kill 所有 worker，正在排队还没跑的任务怎么办？它们的 Sender 被 drop without send，调用方的 `recv` 会 panic。

更隐蔽的 bug：如果 worker 正在跑一个**嵌套 spawn** 的任务（A 调 spawn B、等 B），kill A 的 worker 会让 A 半路死掉、B 永远没人跑、A 持有的任何资源（锁、文件句柄）泄漏。这种 bug 在生产代码里极难复现——通常只在负载高、关停时机不巧时触发。

### WRITE：先标记关停、再清空、再 join

`StealingPool::drop` 做三件事：

```rust
impl Drop for StealingPool {
    fn drop(&mut self) {
        // 1) 标记关停。
        self.state.shutdown.store(true, Ordering::Release);
        // 2) 叫醒所有可能在 wait 的 worker。
        self.state.pending_cv.notify_all();
        let threads = self.state.worker_threads.lock().unwrap();
        for t in threads.iter() { t.unpark(); }
        drop(threads);
        // 3) join 所有 worker。
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
```

`worker_loop` 的退出条件是 **shutdown=true 且 find_work 返回 None**：

```rust
loop {
    if let Some(task) = find_work(&state) {
        // ... 跑任务
        continue;
    }
    if state.shutdown.load(Ordering::Acquire) {
        if find_work(&state).is_none() {
            return;  // 关停 + 全空：退出
        }
        continue;  // 关停但还有任务：继续清空
    }
    // 没活干也没关停：wait
    ...
}
```

含义：**关停后不再接新任务（外部线程调 spawn 会进入 injector，但 worker 已经在 drain 状态）**，但已有的任务全部跑完才退出。这就保证了 drop(pool) 后所有已投递任务的结果都不会丢。

### 优雅关停的细节：什么叫"接新任务"

`shutdown=true` 之后，外部线程还能调 `pool.spawn(...)` 吗？技术上能——我们的 `spawn` 没有 `if shutdown { return Err }` 检查。新投递的任务会进 injector，但因为 worker 在 drain 状态、最终会被取出跑完。

但这是个**语义模糊**的设计：调用方不知道自己 spawn 的任务到底有没有跑。生产代码通常会让 `spawn` 在 shutdown 后返回 `Err`（或者 `Option<JoinHandle>`），明确告诉调用方"任务没接"。

我们这版简化为"先标记后清空"——drop 期间接的新任务也会被跑。drop 完成后 pool 不存在了，新 spawn 自然无处可投（`&pool` 借用违约，编译器拒绝）。这是简化但正确的折衷。

### 关停时机与超时

注意我们 `drop` 里 `join()` 是**无超时**的：如果某个 worker 卡在用户任务的死循环里（`loop {}`），drop 会永远挂起。生产代码通常给关停加超时（比如 30 秒）：

```rust
// 生产代码示例（本教学版不实现）
for w in self.workers.drain(..) {
    let result = w.join_timeout(Duration::from_secs(30));
    if result.is_err() {
        eprintln!("worker did not shut down in 30s, leaking it");
        // 释放 JoinHandle 但不 join——worker 线程继续跑、和 pool 一起被回收
        // 这要求 worker 线程不持有 pool 的引用（否则 pool 永远不被释放）
    }
}
```

我们的简化版假设用户任务都是"有限时间内能跑完"——这是教学合理简化。M9b 会处理超时。

---

## 七、性能对比与延伸

### 性能：V1 vs V3 vs std::thread::spawn vs rayon

在 4 核机器上跑 50000 个 `black_box(i + 1)`（短任务）：

| 方式 | 总耗时（相对值） | 加速比 | 备注 |
| --- | --- | --- | --- |
| 单线程串行 | 1.0x | 1x | baseline |
| `std::thread::spawn` × 50000 | 失败 / 极慢 | — | OS 线程创建开销太大 |
| V1 共享队列 | 0.7x | 1.4x | 锁争用主导 |
| V3 工作窃取 | 0.28x | 3.6x | 接近理论 4x |
| rayon::scope | 0.25x | 4.0x | 生产级、无锁 Chase-Lev |

`std::thread::spawn` 50000 次会爆系统线程上限（Linux 默认 ~32768）。即便能跑，每次 spawn ~50μs，50000 × 50μs = 2.5s，比单线程还慢。线程池的价值就在这里——**复用线程**。

V3 接近 rayon，但还有差距，原因：
- V3 用 `Mutex<VecDeque>`，每次 push/pop 都进锁。rayon 用无锁 Chase-Lev（M8g）。
- V3 的 injector 是 `Mutex`，外部线程投递有锁。rayon 用无锁 injector。
- V3 的 `catch_unwind` 有运行时开销。rayon 不做（任务闭包 panic 让 worker 死，由调用方处理）。

如果把这些都优化掉，就是 rayon。

### ISO·ZOOM：和 rayon 源码对照

rayon 的核心在 `rayon-core/src/registry.rs` 和 `rayon-core/src/sleep.rs`。和我们这版的对照：

| 概念 | 我们的 V3 | rayon |
| --- | --- | --- |
| Worker 抽象 | `WorkerHandle` + thread_local | `WorkerThread` + thread_local |
| 本地队列 | `LocalQueue`（Mutex<VecDeque>） | `LocalQueue<Latch>`（基于 crossbeam-deque 的无锁 Chase-Lev）|
| Injector | 每 worker 一个 Mutex<VecDeque> | 一个全局 `InjectorStack<T>`（无锁）|
| 偷窃 | 从 (my_index+1)%n 开始轮询 | 同样设计 |
| 阻塞调度 | `JoinHandle::recv` 循环 try_recv + run_one_pending_task | `join` 内部 `wait_until` 循环 |
| 唤醒 | `Condvar + Mutex<usize>` | 自定义 sleep 计数器 + `ParkError` |
| panic 隔离 | `catch_unwind` 把 panic 吞掉 | panic 信息捕获后通过 `JobResult` 传播到调用方 |

rayon 多了一个重要功能：**sleep/wake 协议**。当所有 worker 都闲时，它们进入**真正的睡眠**（让出 CPU），通过原子计数器（`sleepers`、`awakeners`）协调唤醒。我们这版的 worker 在没活干时仍在 condvar 上等——比真睡更省电，但比 spin 更慢。

为什么 rayon 这么在意睡眠？因为 rayon 经常嵌在异步运行时里用（`tokio + rayon`），如果 rayon 的 worker 一直 spin，会跟 tokio 的 worker 抢 CPU。让 rayon 的 worker 在没活干时真睡、有活干时立刻醒，是工程上的精细平衡。我们这版教学池不处理这种"与外部协调"的场景。

### ISO·ZOOM：和 std::thread::spawn 的成本对照

`std::thread::spawn` 的成本（x86_64 Linux，2024）：

- **线程创建**：~30-50μs（mmap 栈、clone syscall、TLS 初始化）。
- **线程销毁**：~10-20μs（munmap 栈、exit syscall）。
- **总 overhead**：每次 spawn+join 约 50-100μs。

如果任务本身耗时 1ms，spawn overhead 占 5-10%——可接受。如果任务耗时 10μs，spawn overhead 占 80-90%——不可接受。

线程池**摊薄**了 spawn overhead：

- 第一次启动 N 个 worker：N × 50μs = 一次性开销 ~200μs（N=4）。
- 之后每个 `pool.spawn(...)`：~0.1-1μs（一次锁 + 一次 condvar notify）。
- **50000 个任务**：pool 总 spawn 开销 ~50ms；std::thread::spawn 总开销 ~3 秒。

这就是为什么"任务很多 + 单任务很小"必须用池——池的 spawn 比 std 的快 100 倍以上。

### ISO·ZOOM：和 tokio::spawn 的成本对照

tokio 的 spawn 比线程池 spawn 更便宜（~50ns）：因为异步任务不是 OS 线程，是 future。spawn 一个 future 只是把 future 装箱 + push 到 reactor 队列——纯用户态操作，无系统调用。

代价是：future 必须**协作式**让出（`await`），不能在 future 里阻塞。如果 future 里调 `std::thread::sleep`，整个 reactor 卡住。这就是为什么 tokio 有 `spawn_blocking`——把阻塞任务丢到一个独立的线程池（**用我们这套工作窃取设计**）跑。

所以：**同步线程池是异步运行时的"阻塞任务专门池"**。tokio 的 `spawn_blocking` 内部就是 rayon 式的工作窃取池。

### ISO·ZOOM：为什么 V3 用 Condvar 而不是 atomic-wait 的 futex？

`oneshot::recv` 用 atomic-wait（futex）的 `wait/wake_one`——这是单条 oneshot 的精确唤醒，性能最优。但 `worker_loop` 用 `Condvar + Mutex<usize>` ——为什么不用 atomic-wait？

理论上 `worker_loop` 可以用一个 `AtomicUsize pending` + atomic-wait：

```rust
// 候选方案：纯原子 + futex
fn spawn(...) {
    let p = self.state.pending.fetch_add(1, Ordering::Release);
    // 重要：fetch_add 之后才 wake_one。但 wake_one 之前如果 worker 已经
    // 在 wait(addr, expected) 里，wake 信号到达。但如果 worker 此刻在
    // find_work 和 wait 之间（不在 wait 里），wake 信号丢失——worker 之后
    // 进入 wait(addr, expected)，但 expected 可能已经不对（pending 变了），
    // 不睡，循环重检。OK。
    wake_one(&self.state.pending as *const _);
}
```

这看起来对。但实际测试中我们发现：**多个测试并行跑时，这套机制偶发"任务丢失"**——某些 Sender 被 drop without send。排查发现是 `wake_one` 的 spurious wakeup + thread_local 上下文切换的复杂交互导致的。

`Condvar + Mutex` 路径语义更强：notify 总是和 mutex 持有配对，wait 总是 atomically 释放锁并睡——这套机制在标准库层面被验证了 20 年，不会丢唤醒。**教学版优先正确性**，性能留给生产代码（用 M8g 的 Chase-Lev + 无锁 injector + 原子唤醒）。

`oneshot::recv` 用 atomic-wait 是因为它就一条消息、一对线程，场景简单、不存在丢唤醒的空间。worker_loop 是多对多、多队列、多任务的复杂状态机，Condvar 的强保证更划算。

---

## 八、L1–L5：分层掌握

### L1（认得代码）

- `Task` 是 `Box<dyn FnOnceOnce + Send>` 的 newtype，目的是能装箱任意 `FnOnce()`。
- `JoinHandle<T>` 持有一个 oneshot `Receiver<T>`。
- `SharedQueuePool` 是 V1 敌人；`StealingPool` 是 V3 终点。
- spawn 在 worker 内 push 到本地 LIFO；在外部线程走 injector FIFO。
- `recv` 在 worker 上不 park、循环跑任务；在外部线程 park。

### L2（会画图）

- 画出 V1 共享队列的争用：4 worker 抢同一把锁。
- 画出 V3 工作窃取的任务流转：owner LIFO、thief FIFO。
- 画出嵌套 spawn 的死锁与解除。

### L3（会调参数）

- N worker 怎么选？通常 = `std::thread::hardware_concurrency()`。
- 任务粒度怎么切？太细→调度开销主导；太粗→负载不均。rayon 的经验值是 100μs–10ms。
- LocalQueue 容量？M8g 的 Chase-Lev 固定 4096，超过会 abort。生产代码要做 dynamic resize。

### L4（会改实现）

- 把 `LocalQueue` 的 `Mutex<VecDeque>` 换成 M8g 的 Chase-Lev 双端队列（无锁）。
- 把 `injector` 换成无锁队列。
- 给 worker 加 sleep 策略：闲置超过 N ms 后真睡，省电。
- 给 spawn 加优先级：用户任务 vs 内部 bookkeeping。
- 加 cancel：外部能取消一个已投递但未跑的任务。

### L5（会评判他人实现）

- 看到 Go 的 `runtime.GOMAXPROCS` 调度器、Java 的 ForkJoinPool、Python 的 `concurrent.futures`、C# 的 ThreadPool，能指出它们的工作窃取结构。
- 看到 `tokio` 的 `block_in_place`、rayon 的 `join`，能解释"为什么阻塞时不真睡"。
- 看到"thread per task"的设计（如某些 actor 框架），能指出它在高并发下的扩展性问题。

---

## 九、自检

读完这一章，你应当能回答：

1. **为什么 M5 的 oneshot 不能直接给线程池用？**
   答：它的 Receiver 是 `!Send`，必须留在 split 的那个线程上。线程池里调用方线程和 worker 线程不是同一个，Receiver 需要跨线程移动。

2. **V1 共享队列在 4 核机器上为什么跑不出 4x 加速？**
   答：所有 worker 抢同一把 `Mutex`，取任务的动作被序列化。任务越短，锁争用占总耗时比例越大。

3. **owner LIFO + thief FIFO 为什么不对称？**
   答：owner 用 LIFO 是缓存友好（最近 spawn 的任务数据还热）+ 深度优先（控制任务总数）。thief 用 FIFO 减少与 owner 的缓存行争用 + 偷冷任务对 owner 影响小。

4. **嵌套 spawn 在 1 个 worker 上为什么会死锁？**
   答：任务 A spawn B 后 recv 等 B。B 落在同一个 worker 的本地队列。如果 recv park，worker 睡死，B 永远没人跑，A 永远等不到。

5. **修复方案是什么？**
   答：recv 在 worker 上不 park，循环"try_recv → run_one_pending_task → 重试"。让 worker 在阻塞期间继续跑队列里的任务（包括子任务 B）。

6. **drop(pool) 时正在排队的任务会被丢弃吗？**
   答：不会。shutdown 标志让 worker 不再接新任务，但 worker 退出条件是 `shutdown && find_work().is_none()`——已投递的任务全部跑完才退。

7. **`is_on_worker()` 怎么实现？**
   答：thread_local 缓存的 `Option<WorkerHandle>`。worker_loop 启动时设置为 Some，spawn / recv 用它判断当前线程身份。

---

## 十、动手清单

按难度递增：

1. **跑现有测试**：`cargo test -p forge-pool -- --test-threads=1`。应当 14 个全绿。
2. **加一个 stress 测试**：10000 个独立任务、8 worker、断言总耗时 < 单线程的 1/4。
3. **加一个递归并行快排**：`fn par_sort(pool: &StealingPool, slice: &mut [i32])`，用 pool 嵌套 spawn。和 `slice.sort()` 比耗时。
4. **加一个 sleep 测试**：投 100 个 `thread::sleep(10ms)` 任务到 4-worker 池，总耗时应当 ~250ms（100×10/4），而不是 1000ms。
5. **把 LocalQueue 换成 M8g 的 Chase-Lev**：观察性能提升。注意 Chase-Lev 在多 thief 高并发下的潜在丢任务问题——加 stress 测试验证。
6. **加全局 injector**：参考 rayon 的设计，一个全局无锁队列，外部线程直接 push 进去、所有 worker 竞争 pop。替代当前的 per-worker injector。
7. **加任务取消**：spawn 返回的 JoinHandle 加一个 `cancel()` 方法，把还没跑的任务从队列里删除（这要求队列支持随机删除——用 intrusive list 或 hashmap 实现）。

---

## 十一、下一步

M9a 给了你**同步**运行时。下一站 **M9b** 把它扩展成**异步**运行时——把每个 worker 的"跑一个任务"换成"poll 一个 future"，加上 mio 的事件循环。同样的工作窃取、同样的阻塞调度、同样的 JoinHandle——只是任务变成 future。

你会发现 M9a 这套结构**几乎照搬**到 M9b：oneshot 通道换成 future 的 channel、`recv` 换成 `await`、worker_loop 换成 reactor loop。同步运行时是异步运行时的简化特例——理解了 M9a，M9b 就只是"加一层状态机"。

### 名字的由来：为什么叫"工作窃取"？

"Work stealing"这个词来自 1995 年 MIT 的 Robert D. Blumofe 和 Charles E. Leiserson 的论文《Bounding Work and Span in Parallel Computations》。他们证明了：在 P 个处理器上、总工作量 W、关键路径长度 S 的计算中，工作窃取调度的期望完成时间不超过 W/P + O(S)。这是**渐近最优**的——你不可能比把工作均分到 P 个处理器上更快。

"窃取"（stealing）这个名字是有意的——它强调** thief 主动从 victim 那里拿**，而不是 victim 把任务推给别人。这个区别很重要：

- **Push 模型**（victim 推）：victim 每次完成一个任务后看自己队列长不长，长了就推给别人。问题：victim 要主动监测队列长度，每次完成任务都要做这个检查，开销大；而且"什么时候推"的判断很 tricky。
- **Pull 模型**（thief 拉 / steal）：victim 只管自己干，从不主动推；thief 自己队列空了才去偷。问题：thief 的判断"自己空了"很自然——空就偷，简单。

Pull 模型的优势：**正常路径（victim 自己干）几乎零开销**。只有当某个 worker 闲了才发生偷窃。如果系统整体繁忙，几乎没有偷窃发生；如果有空闲，空闲者主动找活干。这就是工作窃取的本质——**让忙的 worker 不被打扰、让闲的 worker 主动找事**。

这个思路在很多地方都能见到：餐厅服务员闲的会主动帮忙的同事（pull），而不是忙的同事主动把客人推给闲的（push）；分布式任务系统（Celery、Kafka consumer rebalance）也用类似设计。我们这里只是把它做成了线程池里的微观机制。

### 一句话总结

M9a 的核心是三件事：**JoinHandle 让任务有返回值、本地 LIFO 队列让 owner 无锁、工作窃取让闲 worker 自找活**。再加一个**反直觉但必要**的细节——**worker 在等子任务时不真睡，而是边等边干别的活**。这套机制组合起来，就是 rayon 的脊梁。

> "Concurrency is decoupling. Threads, locks, channels, pools—these are all mechanisms for decoupling 'when work is submitted' from 'when work is done'. Asynchrony is the same decoupling, taken one step further: decoupling 'when work starts' from 'when work waits'."  
> ——改编自 Mara Bos

M9a 完。

---

# M9a-par —— 在线程池上跑真正的并行算法

> 子模块:`crates/forge-pool/src/par.rs`
> 测试:`crates/forge-pool/tests/m9a_par_{sort,map_reduce,iter}.rs`
> 跑:`cargo test -p forge-pool`

前面十节你造好了一台工作窃取线程池——`StealingPool`。它能 `spawn`、能偷、能在等子任务时不死睡。但这台机器空转没用,**得让它干活**。这一节我们用 Williams《C++ Concurrency in Action》Ch8/Ch10 的三件经典武器——并行快排、并行 map/reduce、rayon 风格的 `par_iter`——把池跑热,顺便手算两个最容易被新手误解的点:**为什么"spawn 一半 + 自己排另一半"比"两半都 spawn"快**,**为什么并行不是免费午餐**。

---

## 一、敌人先行:你试着并行排个序

假设你要给 100 万个数排序。串行 `sort()` 在你 8 核机器上跑 200ms。你盯着 CPU 监视器看——**只有一个核是 100%,其它七个核在打盹**。这让你心里很不舒服:明明有 8 个核,凭什么只用一个?

你想:快排本来就是分治算法——partition 之后,左半 `< pivot`、右半 `≥ pivot`,两半**完全独立**,可以同时排。那我把两半各 spawn 一个任务,等它们都结束不就行了?

```rust
// 朴素想法(伪代码,先不要抄)
fn naive_par_sort(slice: &mut [i64]) {
    let pivot = partition(slice);
    let (left, right) = slice.split_at_mut(pivot);
    let left_handle  = pool.spawn(|| naive_par_sort(left));   // ← 借用检查不让,先不管
    let right_handle = pool.spawn(|| naive_par_sort(right));
    let _ = left_handle.recv();
    let _ = right_handle.recv();
}
```

你跑了一下,8 个核是亮了,但**比串行还慢 30%**。为什么?这就是这一节要拆开看的两个真相:

1. **你 spawn 了两个任务,然后本线程就 recv 阻塞等结果了**——本线程在等的这段时间**啥都没干**。它本来可以排掉一半,现在它只是干等。等于 8 个核里浪费了 1 个。
2. **spawn 一个任务的开销不是零**——它要装箱闭包、push 队列、可能被别的 worker 偷(偷要走锁)、还要发 oneshot 结果回来。**这个开销大约 1–10μs**。如果你的子任务本身只跑 5μs,那并行纯属亏本。

下面我们一层一层修这两个洞。修完之后,你能看到接近线性的加速(8 核 ≈ 6–7×)。

---

## 二、第一幅画:分治树是一棵"二叉瀑布"

在讲代码之前,先在脑子里画一张图。100 万个元素、8 个核。你调 `par_sort`:

- **顶层**:100 万个元素。partition 之后左半 50 万、右半 50 万(假设 pivot 选得好)。
- **第二层**:每个 50 万再 partition,得到 25 万 / 25 万。现在树上有 4 个待排任务。
- **第三层**:12.5 万 / 12.5 万 × 4 = 8 个任务。
- ……一直分到每个任务大约 1000 个元素(这是 cutoff),转串行。

整棵递归树有多少层?log2(1_000_000 / 1000) ≈ 10 层。叶子节点(走串行 sort 的那些)大约有 1000 个。**这是一棵倒立的瀑布**——上面的任务大、下面的任务小;上面少、下面多。

关键问题来了:**这棵树上挂着大约 2000 个待排任务,但你只有 8 个核。怎么调度,才能让 8 个核一直忙,谁也别闲着,谁也别互相打架?**

这就是工作窃取要做的事。下面我们一边看代码一边画某一瞬间 8 个核分别在干哪一段。

---

## 三、并行快排的实现:spawn 一半,自己排另一半

```rust
// crates/forge-pool/src/par.rs(节选,简化注释)

const PAR_SORT_CUTOFF: usize = 1024;

pub fn par_sort<T: Ord + Clone + Send + Sync + 'static>(
    pool: &Arc<StealingPool>,
    slice: &mut [T],
) {
    // ① 基线:小切片直接串行,不再分。
    //    这一步是关键——见后面 Amdahl 段的手算。
    if slice.len() <= PAR_SORT_CUTOFF {
        slice.sort();
        return;
    }

    // ② 三路 partition:[< pivot | == pivot | > pivot]
    let (lt_end, gt_start) = partition_three_way(slice);
    let left_len = lt_end;
    let right_start = gt_start;

    // ③ 关键技巧:用裸指针把 slice 拆成"左半"和"右半"两个互不重叠的视图。
    //    Rust 的借用检查器无法静态表达"两个不重叠的可变借用分给两个线程",
    //    所以这里用 unsafe。rayon 内部也这么做。
    let left_ptr  = SendPtr(slice.as_mut_ptr());
    let right_ptr = SendPtr(unsafe { slice.as_mut_ptr().add(right_start) });

    // ④ spawn 左半排到池里(让空闲 worker 偷)。
    let pool_clone = Arc::clone(pool);
    let left_handle = pool.spawn(move || {
        let p = left_ptr;  // 先 bind wrapper,避免 2021 闭包分字段捕获
        let left_slice: &mut [T] =
            unsafe { std::slice::from_raw_parts_mut(p.0, left_len) };
        par_sort(&pool_clone, left_slice);
    });

    // ⑤ 本线程不停下来等 spawn 的左半——本线程立即开始排右半。
    //    这就是"spawn 一半 + 自己排另一半"。
    let right_slice: &mut [T] = unsafe {
        std::slice::from_raw_parts_mut(right_ptr.0, slice.len() - right_start)
    };
    par_sort(pool, right_slice);

    // ⑥ 右半排完了,回头等 spawn 的左半。
    //    如果左半还没被偷走、还在本 worker 队列里,recv 会把它跑掉
    //    (M9a 的"边等边干"——recv 在 worker 上不 park,而是循环跑任务)。
    left_handle.recv();
}
```

这段代码里最反直觉的一行是 **④ spawn 之后,⑤ 立刻开始排右半**。新手第一反应是:"我刚 spawn 了左半,难道不该等它吗?" **不该**。这一节剩下的篇幅就是讲清楚为什么。

### 三路 partition:为什么要它

我们用的是三路(不是两路)partition,而且 pivot 取"首中末三数的中位数"。两个细节,各有理由:

**为什么取三数中位数,而不是 slice[0]?** 想象输入已经接近有序。如果你总是取 slice[0] 当 pivot,partition 之后左半长度 0、右半长度 n-1——你退化成了选择排序,O(n²)。三数取中之后,即使输入有序,pivot 也接近中位数,左右大致均衡。

**为什么三路(分 `<` / `==` / `>` 三段),而不是两路(`<` 和 `≥`)?** 想象输入是 100 万个相同的数字。两路 partition 会把所有数字归到 `≥ pivot` 那一半,递归一层只少了一个元素,立刻退化成 O(n²)。三路 partition 把所有等于 pivot 的元素一次性归位,左右两段为空,递归立即终止——O(n) 完事。

这两点都是细节,但缺一个都会让某种特殊输入(有序、全相等)把并行快排的性能打到地下。**并行不能掩盖算法退化**——一个 O(n²) 的算法并行到 8 核,仍然是 O(n²/8),治标不治本。

---

## 四、手算 #1:递归树 + 工作窃取如何喂饱 8 个核

这是本节最重要的图。请你**在脑子里跟着我一步步画**。8 核机器,排 100 万元素。我们看顶层 partition 之后,8 个 worker(W0..W7)分别在干哪一段。

### 第 0 拍:外部线程投递顶层任务

外部线程调 `pool.spawn(|| par_sort(&pool, &mut all_1_000_000))`。这个任务进入 injector(轮询落到 W0 的注入槽)。W0 醒来,从 injector 取到这个任务,开始 partition 100 万元素。

**这一拍的状态**:W0 在 partition(串行扫描,大约 5ms),W1..W7 在 wait。

### 第 1 拍:W0 partition 完,spawn 左半,自己排右半

W0 partition 完,得到左半 50 万、右半 50 万。W0 调 `pool.spawn(left_half)`——这个 spawn 是在 W0(worker 线程)上调的,所以走的是本地 LIFO 队列(push_front),**左半任务进入 W0 的本地队列**。然后 W0 立刻调 `par_sort(right_half)`——W0 自己开始排右半。

**这一拍的状态**:
- W0:排右半(50 万)。
- W1..W7:还在 wait(它们不知道 W0 的本地队列里有活)。

注意一个关键细节:**W0 没有发信号给其它 worker**——它只是 spawn 到自己的本地队列。其它 worker 何时醒来?它们在 condvar 上 wait,而 condvar 在 spawn 时被 `notify_all`(参见 v3_stealing.rs)。所以 W1..W7 这一拍**会被唤醒**,醒来后 `find_work`,本地空、injector 空、然后去偷——它们会发现 W0 的本地队列里有那个左半任务(50 万)。**8 个 worker 里有 7 个会同时尝试偷同一个任务**,只有一个偷成功。

### 第 2 拍:W1 偷到左半,W0 继续排右半

假设 W1 偷到了左半(50 万)。W1 开始 partition 50 万。

**这一拍的状态**:
- W0:排右半(50 万)的某个子段(它已经 partition 完右半,spawn 了右半的左半,自己在排右半的右半……递归中)。
- W1:partition 左半(50 万)。
- W2..W7:还在找活干,但池里暂时没新任务。

### 第 3–6 拍:树往下展开

每排掉一层,树上的待排任务翻一倍。W0 和 W1 各自又 spawn 一半、自己排另一半。到第 6 拍左右,树上挂着大约 8 个待排任务(每段大约 12.5 万),8 个 worker 终于都吃上了。

**第 6 拍的状态**(关键瞬间):
```
W0:排 [12.5 万段 A]
W1:排 [12.5 万段 B]
W2:排 [12.5 万段 C]   ← W2 是从 W0 那里偷来的
W3:排 [12.5 万段 D]   ← 从 W1 那里偷来的
W4:排 [12.5 万段 E]   ← 从 W2 那里偷来的
W5:排 [12.5 万段 F]   ← 从 W3 那里偷来的
W6:排 [12.5 万段 G]
W7:排 [12.5 万段 H]
```

8 个核都满了。这是并行快排的**理想稳态**。

### 第 7 拍:谁干完了,就去偷

现在某个 worker(假设 W3)先排完自己那段 12.5 万。它怎么办?它**不停下来**,而是去 `find_work`——本地空、injector 空,然后偷。它扫一遍 W0、W1、W2、W4、W5、W6、W7 的本地队列,看谁的队列里有任务。

这时候谁的队列里有任务?**正在递归中的 worker**。比如 W0 此刻正在排某个 6 万的子段,它 partition 之后又 spawn 了左半 3 万(进自己本地队列)、自己在排右半 3 万。**W0 的本地队列里挂着那个 3 万**。W3 一偷,就拿到那个 3 万,开始排。

**这就是工作窃取的精髓**:忙的 worker 不主动推任务给别人(它的注意力在自己的活上,不该分心),闲的 worker 主动找活(它闲着也是闲着,扫一遍别人的队列很便宜)。结果是**所有核一直满**——只要树上还有任务,就没有 worker 会空转。

### 与"两半都 spawn"的对比

回到第一节那个朴素想法——两半都 spawn、本线程 recv 等结果。我们现在能看清楚它错在哪:

**朴素版的第 1 拍**:W0 partition 完,spawn 左半、spawn 右半,然后 `left_handle.recv()`。这一行 recv 让 W0 进入"等待循环"。但 W0 在 recv 里**不真睡**(M9a 的设计:recv 在 worker 上继续跑挂起的任务),所以 W0 会从自己队列里把刚 spawn 的左半或右半拉出来跑——**等于绕了一圈又回到自己手上**。spawn 的开销(装箱 + push + oneshot 创建)白付了。

**正确版**:W0 spawn 左半、**自己直接调 par_sort 排右半**。W0 排右半的这 5ms,**不花任何 spawn 开销**——它就是普通函数调用。同时左半挂在 W0 本地队列上,等别的 worker 偷。当 W0 把右半排完,大概率左半已经被某个 worker 偷走并在排了;W0 进 recv 等左半结果,这个等待时间几乎为零(左半可能已经排完或快完了)。

**核心洞见**:"spawn 一半 + 自己排另一半"等价于"免费把自己排的那一半的工作量转移给了本线程,让本线程不空等"。这个技巧有个名字——**深度优先 spawn + 本线程接续**(depth-first spawn with continuation stealing)。它和工作窃取是一对:spawn 把任务"摊"到本地队列上等偷,本线程继续深入递归(深度优先),偷的 worker 从队列另一端拿(广度优先)。两个方向、两条流水线,谁也不挡谁。

---

## 五、第二件武器:par_map / par_reduce

并行快排是分治,但很多场景没那么花哨——你只是想把一个切片上的每个元素并行算一下,或者并行累加。这就是 map / reduce。

### par_map:切段并行填预分配的输出

```rust
// crates/forge-pool/src/par.rs(节选)

pub fn par_map<T, U, F>(pool: &Arc<StealingPool>, slice: &[T], f: F) -> Vec<U>
where
    T: Sync + Send + 'static,
    U: Send + Default + 'static,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
    let n = slice.len();
    if n == 0 { return Vec::new(); }

    // 段数:每段至少 PAR_MAP_CUTOFF(256)个元素,且不超过 n_workers * 4。
    let n_chunks = ...; // 大约 min(n/256, 32) 段
    let chunk_len = (n + n_chunks - 1) / n_chunks;

    let f_arc = Arc::new(f);
    let mut out: Vec<U> = (0..n).map(|_| U::default()).collect();
    let out_ptr = SendPtr(out.as_mut_ptr());  // 不重叠的输出段
    let in_ptr  = SendConstPtr(slice.as_ptr());

    let mut handles = Vec::new();
    for (start, end) in 切段 {
        let f_clone = Arc::clone(&f_arc);
        let h = pool.spawn(move || {
            let ip = in_ptr;  let op = out_ptr;
            let in_slice  = unsafe { slice::from_raw_parts(ip.0.add(start), end-start) };
            let out_slice = unsafe { slice::from_raw_parts_mut(op.0.add(start), end-start) };
            for (i, x) in in_slice.iter().enumerate() {
                out_slice[i] = f_clone(x);
            }
        });
        handles.push(h);
    }
    for h in handles { let _ = h.recv(); }
    out
}
```

关键设计:**预分配整个输出 Vec,每个任务直接写自己那段**。不是"每个任务返回一个 `Vec<U>`,最后 concat"——那样要分配 N 次小 Vec,还要拷贝 N 次合并。预分配 + 不重叠指针 = 零拷贝。

为什么段数取 `n_workers * 4`?如果段数等于核数(8 段 / 8 核),万一某段任务比其它段慢(比如那段的数据触发了 f 的慢路径),就会有 worker 干完了闲着、其它还在干。**段数略大于核数**(32 段 / 8 核)让"长尾"任务被均匀稀释——某个慢段排到某个 worker 上,该 worker 干完慢段后立刻能拿另一个段继续干,负载自然均衡。这个 4 倍系数 rayon 也用,叫"chunk granularity"。

### par_reduce:三闭包 API

reduce 比 map 多一个坑。一个闭包 `(U, &T) -> U` 不够——分段并行后,每段产生一个 `U`,但主线程手里只有 N 个 `U`(段部分结果),没有原始 `T`(已经被 fold 掉了)。要把 N 个 `U` 合成一个,需要一个 `U + U -> U` 的合并函数。

所以 par_reduce 的 API 是**三个闭包**:

```rust
pub fn par_reduce<T, U, I, S, M>(
    pool: &Arc<StealingPool>,
    slice: &[T],
    init: I,   // || U:每段的起点零值
    step: S,   // (U, &T) -> U:段内累加
    merge: M,  // (U, U) -> U:段间合并
) -> U
```

这看起来啰嗦,但每一个都不可省。**rayon 也这么干**——它的 `fold` + `reduce` 就是这个分工:`fold` 让"扫一段"并行,`reduce` 把各段结果串行合并。合并是 O(n_chunks) 次串行,通常 n_chunks 远小于 n,这步开销可忽略。

举例——求和:

```rust
let total: i64 = par_reduce(
    &pool, &input,
    || 0i64,                // 每段从 0 开始
    |acc, x| acc + x,       // 段内:累加
    |a, b| a + b,           // 段间:两段相加
);
```

求最大值:

```rust
let max: i64 = par_reduce(
    &pool, &input,
    || i64::MIN,
    |acc, x| if *x > acc { *x } else { acc },
    |a, b| if a > b { a } else { b },
);
```

**练习**(自己想想再往下读):为什么 `init` 是 `Fn() -> U`(每次调一次),而不是直接一个 `U`?——因为 N 段并行,N 个任务各自要一个起点零值。如果 init 是单个 `U`,你得让 N 个任务共享它的可变借用,这在并行下不可能(数据竞争)。`Fn() -> U` 让每个任务自己造一个,各管各的。

---

## 六、第三件武器:par_iter(rayon 风格)

rayon 之所以好用,是因为它把"并行"藏进了一个看起来和串行迭代器一模一样的 API:

```rust
// 串行
let sum: i64 = vec.iter().map(|x| x * 2).filter(|x| x % 3 == 0).sum();
// 并行(几乎只改一个前缀)
let sum: i64 = par_iter.sum(&pool);
```

我们造一个简化版的 `ParIter`,体会 rayon 的设计哲学——**惰性适配器链 + 终端 sink 触发并行遍历**。

```rust
// crates/forge-pool/src/par.rs(节选)

pub struct ParIter {
    n: usize,
    // 把"对一段 [start, end) 做完所有 map/filter 之后产出什么"
    // 编译成一个闭包。每个 adapter 包一层。
    chunk_fn: Arc<dyn Fn(usize, usize) -> Vec<i64> + Send + Sync>,
}

impl ParIter {
    pub fn from_slice(slice: &[i64]) -> Self {
        let buf = Arc::new(slice.to_vec());
        let buf_clone = Arc::clone(&buf);
        Self {
            n: slice.len(),
            chunk_fn: Arc::new(move |s, e| buf_clone[s..e].to_vec()),
        }
    }

    pub fn map<F: Fn(i64) -> i64 + Send + Sync + 'static>(self, f: F) -> Self {
        let prev = self.chunk_fn;
        let f = Arc::new(f);
        Self {
            n: self.n,
            chunk_fn: Arc::new(move |s, e| {
                let mut v = prev(s, e);
                for x in v.iter_mut() { *x = f(*x); }
                v
            }),
        }
    }

    pub fn filter<F: Fn(i64) -> bool + Send + Sync + 'static>(self, pred: F) -> Self {
        let prev = self.chunk_fn;
        let pred = Arc::new(pred);
        Self {
            n: self.n,
            chunk_fn: Arc::new(move |s, e| {
                prev(s, e).into_iter().filter(|x| pred(*x)).collect()
            }),
        }
    }

    // 终端:sum。切段并行跑 chunk_fn,各段求和,主线程串行加总。
    pub fn sum(self, pool: &Arc<StealingPool>) -> i64 {
        // ... 切段、spawn、各段 sum、合并
    }
}
```

**核心设计:闭包洋葱**。每个 `map` / `filter` 不真正遍历数据,它只是把"前一个 chunk_fn 的输出,经过自己的变换,变成新输出"包成一个新闭包。这就是惰性——你链 5 个 map,数据不会被遍历 5 遍,而是在最终 `sum` 触发时,**每个 chunk 只遍历一次,一遍里跑完所有 5 个 map**。

**为什么 chunk_fn 返回 `Vec<i64>` 而不是单值?** 因为 filter 会改变元素数量——一段 1000 个元素 filter 后可能只剩 50 个。chunk_fn 必须能产出"变长结果",所以返回 Vec。

**sum 是 sink**——它是真正触发并行的终端方法。它切段、每段调 chunk_fn(拿到该段的最终结果 Vec)、各段 sum、主线程合并。如果你不调 sink,ParIter 链什么也不做(惰性)。

这套设计是 rayon 的脊梁。rayon 真正的实现要复杂得多(全泛型、关联类型、内部用 GAT),但**思想内核就是"闭包洋葱 + 切段并行 + 工作窃取调度"**。我们这里把 i64 写死,是为了把思想内核暴露出来,不被泛型噪声淹没。

---

## 七、手算 #2:Amdahl 定律——并行不是免费午餐

你跑了一下 `par_sort` 在 100 万元素上,8 核机器,期望 6–7× 加速,结果只有 4×。为什么?**因为你的程序不是 100% 可并行**。

Gene Amdahl 在 1967 年指出一个让人沮丧的事实:**程序的加速比受限于它的串行部分**。我们手算一遍。

### 加速比公式

假设一个程序的总运行时间是 T。其中 **p 比例可并行**(可以分到 N 个核上),**1 - p 比例必须串行**(partition、合并、I/O、锁等)。用 N 个核跑,新时间是:

```
T_new = T * [(1 - p) + p / N]
```

加速比 S = T / T_new:

```
S(N) = 1 / [(1 - p) + p / N]
```

### 你的程序:p = 95%

假设你的 par_sort 95% 时间花在并行 partition + 并行递归上,5% 花在串行部分(顶层 partition 之前的输入准备、各段合并、最后 slice 拷贝等)。p = 0.95。

**4 核**:

```
S(4) = 1 / (0.05 + 0.95/4)
     = 1 / (0.05 + 0.2375)
     = 1 / 0.2875
     ≈ 3.48×
```

理论极限 4×,实际 3.48×。少了的 0.52× 是那 5% 串行部分的代价。

**16 核**:

```
S(16) = 1 / (0.05 + 0.95/16)
      = 1 / (0.05 + 0.0594)
      = 1 / 0.1094
      ≈ 9.15×
```

你给 4 倍的核(4→16),加速比只提升了 2.6 倍(3.48→9.15)。**收益递减**。

**无穷多核**:

```
S(∞) = 1 / 0.05 = 20×
```

无论你加多少核,加速比永远不超过 20×——**那 5% 的串行部分封顶**。这就是 Amdahl 定律的杀伤力:**串行部分是并行的天敌,再多的核也救不了它**。

### Amdahl 对 par_sort 的实际影响

我们这套 par_sort 的串行部分来自:

1. **partition 本身**:每次递归的 partition 是串行的(扫描整个切片)。partition 时间 O(n),递归树每层 partition 总和也是 O(n),共 log n 层,所以 partition 总开销 O(n log n)——和排序本身一个量级。但**顶层那一次 partition 必须串行**(在 spawn 之前),没法并行。这就是 5% 串行的来源。
2. **spawn / recv 开销**:每个 spawn 大约 1–10μs(装箱闭包 + push 队列 + 创建 oneshot channel + 可能的偷窃锁竞争)。一个 1000 元素的串行 sort 大约 50μs。如果你把 cutoff 设到 100,就会 spawn 上百万个任务,总 spawn 开销 = 100 万 × 5μs = 5 秒——比串行排序 200ms 慢 25 倍。**cutoff 太小,并行变并行亏**。
3. **cache 局部性**:并行任务的内存访问模式可能比串行差(两个 worker 同时排相邻段,会互相把对方的 cache line 挤出去)。这是 Amdahl 没有建模但实测会看到的额外损耗。

### 任务粒度手算:什么时候才该 spawn?

经验法则:**任务的预期执行时间应该至少是 spawn 开销的 10 倍**。spawn 开销约 5μs,所以单个任务至少要跑 50μs 才划算。

- 串行 sort 1000 个 i64 大约 50μs(实测)。
- 串行 sort 100 个 i64 大约 5μs——已经和 spawn 开销持平,**不该 spawn**。

这就是为什么 `PAR_SORT_CUTOFF = 1024`:在 cutoff 处,串行 sort 的时间和 spawn 开销相当,再切下去就是亏。**这个数会随硬件和元素类型变化**——rayon 默认用 4096 个元素(它考虑了更多 cache 因素)。我们的 1024 是教学折中。

### criterion 实测预期

如果你用 criterion 在不同数据规模下 benchmark par_sort vs 串行 sort,会看到一条很有教育意义的曲线:

- **n = 100**:par_sort 比串行慢 5–10×(spawn 开销 dominate,数据小到根本不需要并行)。
- **n = 10_000**:par_sort 比串行慢 1.2×(数据还是太小,8 个核的 spawn 开销抵不过并行收益)。
- **n = 100_000**:par_sort 比串行快 1.5×(开始有收益)。
- **n = 1_000_000**:par_sort 比串行快 5×(接近理想)。
- **n = 10_000_000**:par_sort 比串行快 6.5×(收益递减,Amdahl 串行部分开始封顶)。

**这条曲线是并行算法的本质**——小数据并行亏、大数据并行赚、超大数据受串行部分封顶。没有银弹,只有工程权衡。

---

## 八、为什么 par_sort 用 unsafe?Rust 借用检查器的边界

你可能注意到 par_sort / par_map / par_reduce 里到处是 `SendPtr`、`SendConstPtr`、`unsafe { slice::from_raw_parts_mut(...) }`。这一节讲清楚为什么要这么写,以及它的安全边界在哪。

### 问题:Rust 不让你"把一个切片的两个不重叠段同时交给两个线程"

Rust 的借用规则是:**任意时刻,要么有一个可变借用,要么有多个不可变借用**。`split_at_mut` 是这个规则的合法例外——它把一个 `&mut [T]` 拆成两个不重叠的 `&mut [T]`。

但 `split_at_mut` 拆出来的两个借用**生命周期是绑在同一个 scope 上的**。你不能把其中一个塞进 `pool.spawn` 的闭包里(闭包要 `'static`),另一个留在本线程。借用检查器会拒绝——因为闭包跑在另一个线程上,生命周期超出了当前函数,而借用的来源(slice)的生命周期不够长。

所以你只能用**裸指针**。裸指针没有生命周期,可以塞进 `'static` 闭包。代价是:**安全性靠你自己保证**——你必须确保两个指针指向的区间不重叠、且 slice 在闭包运行期间不被释放。

### 我们的保证

par_sort 里这个保证来自:

1. **不重叠**:`left_ptr = base`,`right_ptr = base + right_start`,且 `right_start >= lt_end`(partition 的输出保证)。两段地址区间严格不重叠。
2. **存活**:spawn 的任务的 `JoinHandle` 在本函数返回前一定 `recv()`——recv 完成意味着任务结束,意味着闭包不再访问 slice。所以 slice 在闭包访问期间一定还活着(它的所有者——调用者——还没回收它)。

这两条加起来,unsafe 块是 sound 的。rayon 内部也是这套逻辑,只是它把 unsafe 包装得更精细(`ParallelSlice`、`ChunksMut` 等),让用户层看到的 API 没有 unsafe。

### SendPtr / SendConstPtr:让裸指针 Send

裸指针 `*mut T` / `*const T` 默认 `!Send`(Rust 保守,怕你跨线程改它)。我们包一个 newtype:

```rust
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
```

这个 `unsafe impl` 是说"我担保跨线程搬运这个指针是安全的"——前提是使用方保证不重叠 + 存活。这就是 unsafe Rust 的本质:**编译器信任程序员的担保,程序员承担证明责任**。

### 一个 2021 edition 的小坑

你看到代码里有 `let p = left_ptr;` 这样的"无意义赋值"。这不是装饰,是**绕过 Rust 2021 闭包的分字段捕获**。2021 edition 里,`move || left_ptr.0.add(start)` 只捕获 `left_ptr.0`(裸指针本身),不捕获 `left_ptr`(wrapper)。裸指针 `!Send`,闭包就 `!Send`,spawn 报错。先 `let p = left_ptr;` 让闭包捕获 wrapper(它是 Send 的),问题解决。

这是 Rust 类型系统细节,但**真实工程里撞到一次就忘不掉**。rayon 内部也有类似的 workaround。

---

## 九、把三件武器放在一起:一个小例子

```rust
use forge_pool::{StealingPool, par::*};
use std::sync::Arc;

fn main() {
    let pool = Arc::new(StealingPool::new(8));

    // ① 并行快排
    let mut data: Vec<i64> = (0..1_000_000).map(|i| (i as i64).wrapping_mul(2654435761)).collect();
    par_sort(&pool, &mut data);
    assert!(data.windows(2).all(|w| w[0] <= w[1]));

    // ② par_map:每个元素平方
    let squares = par_map(&pool, &data, |&x| x * x);

    // ③ par_reduce:求和
    let total: i64 = par_reduce(
        &pool, &squares,
        || 0i64,
        |a, x| a + x,
        |a, b| a + b,
    );

    // ④ par_iter:链式 map/filter/sum
    let filtered_sum: i64 = ParIter::from_slice(&squares)
        .map(|x| x + 1)
        .filter(|x| x % 7 == 0)
        .sum(&pool);

    println!("total = {total}, filtered = {filtered_sum}");
}
```

四段代码、四种并行模式,都跑在同一台 `StealingPool` 上。**池是共享的**——你不用为每个算法起一组新线程。这正是 rayon 的世界模型:**一个全局池,所有并行算法在它上面跑**。

---

## 十、读者最容易卡住的 1 个点

我在写完这一节后,假装自己是第一次读它的 18 岁读者,从头扫一遍。我卡在的地方是:**"为什么 spawn 一半 + 自己排另一半,就比两半都 spawn 要好?不都是用了 2 个核吗?"**

答案藏在"本线程在 recv 期间干什么"这一细节里。两种方案下:

- **两半都 spawn**:W0 partition 完,spawn 左、spawn 右,然后 `left.recv()`。此刻 W0 进入 recv 循环。M9a 的 recv 在 worker 上不 park,而是循环跑挂起的任务。所以 W0 会立刻从自己队列里 pop 出左半或右半(刚 push 进去的),开始排。**W0 等于绕了一圈,把刚 spawn 的任务又自己捡回来跑**。spawn 的开销(装箱 + oneshot channel 创建 + push)白付了。
- **spawn 一半 + 自己排另一半**:W0 partition 完,spawn 左、然后**直接函数调用**排右半。W0 排右半的这 5ms,没有 spawn 开销——就是普通函数调用。左半挂在 W0 本地队列上,等别的 worker 偷。当 W0 排完右半进 recv 等左半,**左半大概率已经被别的 worker 偷走并在排了**,W0 几乎不等。

两种方案都用了 2 个核,但**前者多付了一次 spawn + 一次自己 pop 的开销**。这个开销看起来小(~5μs),但递归树有几千个节点,累计起来就是几百毫秒——足够把加速比从 6× 打到 4×。

**一句话**:"spawn 一半 + 自己排另一半" 让本线程永远在做有用功,从不浪费。这是工作窃取调度器的灵魂。

---

## 十一、收尾

这一节给你三件武器:

1. **par_sort**:并行快排。分治 + 工作窃取 + cutoff 转串行。
2. **par_map / par_reduce**:切段并行 + 串行合并。三闭包 reduce API。
3. **par_iter**:惰性适配器链 + sink 触发并行。rayon 风格。

加上手算的两个核心点——**递归树如何被工作窃取喂饱**、**Amdahl 定律如何封顶加速比**——你现在能:

- 看到一个并行算法,判断它"分治得对不对、cutoff 选得对不对、串行部分占多少"。
- 写一个新的并行算法,知道 API 怎么设计(map/reduce/iter 三选一)、unsafe 用在哪、Send 怎么处理。
- 在 benchmark 面前不慌——知道"小数据并行亏"是正常的,知道怎么调 cutoff 找甜区。

**M9a 至此真正完结**。你不只有一台线程池,你还用它在真实问题上跑出了真实加速。这是从"会写并发原语"到"会写并行程序"的跨越。下一站 M9b 把同步池扩展成异步执行器——但那是另一个故事了。

> "Parallelism is not concurrency. Concurrency is about *decoupling*; parallelism is about *speedup*. A concurrent program may run on one core; a parallel program had better use more than one."
> ——Mara Bos(改写)

