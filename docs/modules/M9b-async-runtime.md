# M9b — 异步执行器 + mio Reactor：把 M9a 改造成 `tokio` 的雏形

> 模块：`crates/forge-rt::{task, executor, reactor, lib}`
> 测试：`crates/forge-rt/tests/m9b_{delay_block_on,runtime_spawn,delay_concurrent,select}.rs`
> 跑：`cargo test -p forge-rt`

---

## 〇、开战之前：你已经有什么、还差什么

你已经走完了 M1–M9a。回顾你手里能用的零件：

- **原子变量与内存序**（M1）：`AtomicU32`、Acquire/Release/SeqCst。
- **互斥锁、Condvar**（M2/M7）：标准锁、用 futex 拼的真锁。
- **自旋锁**（M3）：低延迟、单核内极快。
- **`Arc`/`Weak`**（M4）：跨线程的共享所有权。
- **通道**（M5）：oneshot、mpsc。
- **`atomic-wait` 即 futex**（M6）：`wait(addr, val)` + `wake_one(addr)`。
- **无锁结构动物园**（M8）：信号量、RCU、Treiber 栈、MCS 锁、parking-lot、SeqLock、Chase-Lev 双端队列。
- **同步工作窃取池**（M9a）：N worker 各有本地队列 + 偷；任务等待时帮忙跑别的任务；嵌套 spawn 不死锁。

但你的池子只能跑**闭包**。`pool.spawn(|| { let x = fetch_url(); parse(x) })` 这一行里，`fetch_url()` 一旦真的发起 HTTP 请求，整个 worker 线程会被 syscall 阻塞——内核让它睡在 socket 的等待队列上，直到网卡收完数据包、内核再把它唤醒。**那 100 毫秒里**，这个 worker 不做任何事，却占着一个 OS 线程、占着一个核。如果你要写一个 web 服务器，同时处理 10000 个连接——**10000 个 OS 线程**直接把内核调度器压垮（线程栈 + 上下文切换开销爆炸）。

我们要在这一章做一件事：让"等待"变得几乎免费。

不是用更多线程，不是用更细的锁——而是把"任务"这个概念从**OS 线程**上摘下来，让它变成一种**可以暂停、可以恢复、可以放弃**的纯内存对象。一个 task 等待 socket 数据时，它不占用任何线程——它把"我该被叫醒"这个信息登记到一个叫 **reactor** 的东西上，然后**主动交出执行权**，回到执行器的就绪队列里等下一个能跑的任务。socket 数据到了，reactor 把 task 重新叫醒，task 重新进队列，被某个 worker 拿起继续跑。

这套机制的官方名字叫 **async/await**。Rust 把它做成了一个**零成本抽象**：你写出来的 `async fn` 在编译期被翻译成一个**状态机**，状态机本身就是个普通的 struct，不绑任何线程。

这套机制的脊梁有两个零件：

1. **Future + Waker**（语言层）：一个 future 是"可以问'你好了吗'的东西"；一个 waker 是"可以告诉执行器'请再问一次'的东西"。
2. **Executor + Reactor**（运行时层）：executor 反复从队列里拿 future 问"好了吗"；reactor 监听操作系统（epoll/kqueue/IOCP）说"该醒一下这些 future 了"。

这一章我们三个都造，最后用它们拼出一台能跑真实并发 task 的运行时——`tokio` 的雏形。读完之后，你应当能回答：

> 为什么 `tokio::spawn(async {}).await` 不会占住一个线程？为什么 `Delay::new(50ms)` 比起 `std::thread::sleep(50ms)` 在一万个并发任务里快几十倍？

让我们从一个看似离题、其实是整个故事起点的问题开始：**怎么写一个"问好不好"的函数？**

### 一个具体的"为什么我需要异步"的例子

为了让你**真切**感到需要异步，让我们对比三种 web 服务器写法。

**写法 A：每连接一个线程**

```rust
fn main() {
    let listener = std::net::TcpListener::bind("0.0.0.0:8080").unwrap();
    for stream in listener.incoming() {
        let stream = stream.unwrap();
        std::thread::spawn(move || handle(stream));
    }
}
```

每个连接开一个线程。10000 个并发连接 = 10000 个 OS 线程。每个线程栈默认 8MB——10000 × 8MB = 80GB 虚拟内存（虽然不真分配，但页表项爆炸）。内核调度器要管 10000 个线程，每秒几万次上下文切换。你的服务器还没处理一个字节就先死在调度上。

**写法 B：异步 + 单线程**

```rust
#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:80800").await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::spawn(async move { handle(stream).await });
    }
}
```

1 个线程。10000 个并发连接 = 10000 个 task。每个 task 大概 200 字节（future 状态机 + Arc + schedule 闭包）。10000 × 200B = 2MB——一个 Java app 一个对象的零头。调度器一个线程自己轮询就绪队列，0 上下文切换开销。

**写法 C：异步 + 多 worker**

```rust
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() { /* 同上 */ }
```

4 个 worker 线程，共享 10000 个 task。这就是并行 + 并发——4 个核同时推进任务，I/O 等待全部走 reactor 路径。

这一章我们造的就是写法 C 的内核——forge-rt。读完之后你应当能完整说出写法 C 的每一行背后发生了什么。

---

## 一、Future/Poll/Context：把"等"变成"问"

### ENEMY：`sleep` 是个坏 citizen

```rust
fn fetch_and_parse() -> String {
    let raw = std::thread::sleep_ms(100); // 假装是网络请求
    parse(raw)
}
```

`std::thread::sleep` 把当前 OS 线程**完全睡死**。睡着的 100ms 里，这个线程什么都不能做。M9a 的池子也救不了——`StealingPool` 的 worker 在跑这个任务时，确实会一直阻塞在 sleep 上，别的任务要靠别的 worker 偷。**如果你有 10000 个这样的任务**，你需要 10000 个 worker 线程才能并发跑它们。

我们想要的不是"睡"，是**"告诉调用方：我还没好，你先去干别的，过会儿再来问我"**。

### ANCHOR：餐厅服务员类比

把你自己想象成一个快餐店服务员。客人 A 点了一个汉堡。你下单给后厨。**然后呢？**

- **同步做法**：你站在出餐口等，一直等到汉堡做好，端给客人 A，再去服务客人 B。100ms 内你什么都干不了。这就是 `thread::sleep`。
- **异步做法**：你给后厨一张小票，上面写着"客人 A 的汉堡"。然后你**走开**，去服务客人 B、C、D。每过一会儿你回到后厨看一眼"小票对应的汉堡好了吗"。后厨说"还没"，你再去干别的；后厨说"好了"，你端给客人。

异步做法的关键是：**你不会被动地等**。你是**主动地、定期地"问"**。问一次没好，你就走开。

这个"问一次"的动作，Rust 起名叫 **poll**。

### LOW-FI：手写第一个 future

我们用一个最简单的例子：一个 `Delay` future——"等到某个时刻，就完成"。先**不考虑 reactor**，纯用 `Instant::now()` 自己判断。

```rust
// crates/forge-rt/src/lib.rs（节选）
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

pub struct Delay {
    deadline: Instant,
}

impl Delay {
    pub fn new(after: std::time::Duration) -> Self {
        Self { deadline: Instant::now() + after }
    }
}

impl Future for Delay {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}
```

逐字段拆解：

- `type Output = ()`：这个 future 完成时不返回任何值（就是一个"等一会儿"的信号）。`type Output = String` 就是"等到完返回一个 String"。
- `fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>`：**问一次**。
  - 返回 `Poll::Ready(value)` 表示"我好了，结果在这里"。
  - 返回 `Poll::Pending` 表示"还没好"。
- `self: Pin<&mut Self>`：参数类型，先**当 `&mut self` 看就行**——`Pin` 的存在理由我们第二章讲透。
- `cx: &mut Context<'_>`：暂时当"传进来一个东西，让我能拿到 Waker"。第三章讲透。

每次有人调 `delay.poll(cx)`，Delay 内部就是看一眼表："到了？没到？"。这是真正的**"问"**——而不是"睡"。

### WRITE：把 poll 跑起来

但 poll 谁来调？future 自己不会跑。你需要**主动**调它：

```rust
let mut delay = Delay::new(std::time::Duration::from_millis(50));
let waker = noop_waker(); // 第三章解释
let mut cx = std::task::Context::from_waker(&waker);

// 这是世界上最简陋的"执行器"：忙等到 Ready。
let start = Instant::now();
loop {
    if let Poll::Ready(v) = std::pin::Pin::new(&mut delay).poll(&mut cx) {
        break v;
    }
    // 没好——让出 CPU 一会儿。
    std::thread::yield_now();
}
```

这就是一个"忙等执行器"。它**不睡**——`yield_now` 只是告诉 OS"我这时间片不要了"，OS 会让别的线程跑，但 future 这个对象本身一直在内存里。这跟 `thread::sleep` 区别很大：`sleep` 把整个线程送进内核等待队列，谁来叫醒它只能靠内核；这里 future 是**用户态的纯内存对象**，谁来 poll 它都行——任何线程都行。

让我们细化这个差别，因为它**是整个异步故事的根**：

- `thread::sleep(50ms)`：你的线程向内核说"把我从运行队列里摘下来，50ms 后再放回去"。这 50ms 里你的线程不占 CPU，但**它也什么都干不了**——它被深度冻结了，栈还在、TLS 还在、所有寄存器状态都在内存里躺着。如果同一时间有 10000 个线程在 sleep，内核的调度队列有 10000 项；每秒的 timer tick 要扫一遍把它们叫醒——光是调度开销就吃掉几个核。
- `Delay + 忙等执行器`：你的线程**在用户态**反复看表。每次看表是几纳秒的纯内存操作。线程没有进内核，调度器看不见它。50ms 的 delay 期间它会被 yield_now 让出去几百次，但每次"回来"都不需要任何内核参与。10000 个这样的 future 在同一个线程上排队看表——内核完全感知不到。

但这版还有个大问题：**忙等浪费 CPU**。50ms 的 delay 你 poll 了几千次。即便 yield_now 让出时间片，CPU 仍在被反复唤醒。我们后面会把它修成"被叫醒再 poll"——这就是 Waker 的全部作用：让"看表"从**主动反复看**变成**被动被叫醒后看**，把 CPU 占用降到接近零。

### ISO·ZOOM：为什么是 `Pin<&mut Self>`？

现在你心里有一个疑问：为什么 `poll` 的 self 类型是 `Pin<&mut Self>` 而不是 `&mut Self`？

`Pin` 是个 newtype：`struct Pin<P> { pointer: P }`。当 `P = &mut Self`，它就是个普通的可变引用——但带了一个**契约**：**被 pin 的对象从此不能再被 move**。

为什么 future 需要"不能 move"？答案藏在第二章：**自引用**。先记下这个钩子，下面立刻展开。

---

## 二、Pin：自引用 future 与"为什么不能 move"

### ENEMY：一个会编译失败的 future

设想你写一个 future，它需要"先记下某个局部变量的地址，等一会儿，再用这个地址"。听起来很自然——这就是 C 程序员天天干的事。在 Rust 里，async/await 会**自动**生成这样的 future（编译器把 `let r = &x; ...; use(r)` 编进状态机里）。我们先**手写**一个简化版，看清问题：

```rust
struct SelfRef {
    data: u64,
    // 一个指针，指向自己里面的 data 字段。
    ptr: *const u64,
}

impl SelfRef {
    fn new(data: u64) -> Self {
        let mut s = Self { data, ptr: std::ptr::null() };
        s.ptr = &s.data as *const _;
        s
    }
}
```

构造完，`s.ptr` 指向 `s.data` 的当前地址。这看起来没问题——**只要 `s` 不动**。

### 逐拍手算 #1：自引用被 move 后悬空

让我们**逐拍**画出 `s` 在栈上被构造、然后被 move 的过程。假设 `s` 一开始在栈地址 `0x1000`：

```
拍 0（构造前）：
栈地址        内容
0x1000       (未初始化)
0x1008       (未初始化)

拍 1（let mut s = Self { data: 42, ptr: null() }）：
0x1000       data = 42
0x1008       ptr  = 0x0000_0000_0000_0000   (还没指)

拍 2（s.ptr = &s.data）：
0x1000       data = 42
0x1008       ptr  = 0x0000_0000_0000_1000   ← 指向自己 data 字段
                                                现在自引用关系成立。

拍 3（let s2 = s; —— move！）：
现在 s 被搬到另一个地址 0x5000。
0x5000       data = 42
0x5008       ptr  = 0x0000_0000_0000_1000   ← 仍然指向旧地址！
0x1000       (栈帧已退，内容是垃圾/复用)

s2.ptr 现在指向的 0x1000 已经不是 s2.data 了——可能是栈上的垃圾、
可能是别人写的脏数据。读 *s2.ptr 就是未定义行为（UB）。
```

这就是 C/C++ 程序员几十年都在踩的坑：**自引用对象不能 move**。一旦 move，内部的指针还指着旧地址。

async/await 编译出来的状态机正好就是这样：编译器把 `async fn` 里所有的局部变量塞进一个 struct，再在 `await` 点上让 `poll` 检查"该往下一状态走吗"。**这个 struct 内部经常自引用**——比如 `let r = &some_local; some_future.await; use(r)` 中，`r` 是 struct 内部的指针，指向 struct 内部的另一个字段。

所以 Rust 在 `Future::poll` 的签名里**强制** `self: Pin<&mut Self>`——意思是：调用方必须保证 `self` 这块内存**从此不能被 move**。这样一来，自引用就永远不会失效。

### ANCHOR：把"不能 move"做成类型

`Pin<P>` 是怎么"保证不能 move"的？靠的是 Rust 的所有权规则：

- 你拿到一个 `Pin<&mut T>`，**不能**用 `&mut T` 把里面的 T 替换或拿走（`Pin` 不暴露这个能力）。
- 唯一能"消费" `Pin<&mut T>` 的合法方式是 `Pin::into_inner`（unsafe）、或者用 `Pin::as_mut` 拿 `Pin<&mut T>`（仍然 pinned）。
- 如果你想 **构造** 一个 `Pin<P>`，最常用的方法是 `Box::pin(x)`：把 x 放到堆上、拿到 `Pin<Box<T>>`。**堆地址从此固定**——`Pin<Box<T>>` 也只暴露能"deref 到 `Pin<&mut T>`"的能力，不允许你把 Box 抢出来再 move。

```rust
let s = SelfRef::new(42);
let pinned: std::pin::Pin<Box<SelfRef>> = Box::pin(s);
// 现在 pinned 引用的 SelfRef 永远在那块堆内存上——你拿不到它，
// 自然没法 move 它，自引用永远有效。
```

**栈上 pin** 也是合法的：`Pin::new_unchecked(&mut s)` 把栈上变量的地址也 pin 住——但要求调用方保证 `s` 在被 pin 期间不被 move（通常靠 shadowing：`let mut s = ...; let mut s = unsafe { Pin::new_unchecked(&mut s) };` 之后只用 pinned 的那个 `s`）。

### WRITE：Pin 在我们 forge-rt 里的样子

回到 task.rs：

```rust
// crates/forge-rt/src/task.rs（节选）
type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

pub(crate) struct Task {
    state: AtomicU8,
    future: std::sync::Mutex<Option<BoxFuture>>,
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
}
```

注意 `future` 字段：`Pin<Box<dyn Future>>`。这个类型同时干了两件事：

1. **`Box<...>`**：堆分配，地址在堆上固定。
2. **`Pin<...>`**：从此**类型系统层面**禁止 move。

task 一旦被 `spawn` 进来，它的 future 就被 `Box::pin` 装箱——之后**整个 task 的生命周期里**，future 都在那块堆内存上，自引用永远有效。这是 Pin 存在的全部理由。

### ISO·ZOOM：`Unpin` 这个逃生口

但你会问：`u32` 这种**没有自引用**的类型也必须被 pin 才能调 `poll` 吗？难道 `&mut u32` 不够？

答案是：**所有 `&mut T` 都能被自动"升级"成 `Pin<&mut T>`**，只要 `T: Unpin`。`Unpin` 是一个 marker trait，绝大多数类型（`u32`、`String`、`Vec`，所有非 async-fn 生成的 future）都自动 `Unpin`——它们可以被随意 move，pin 它们是免费的、毫无意义。

**只有 `!Unpin` 的类型**——主要是 `async fn` 编译出来的 future——才真正依赖 pin。这些类型是少数。所以日常你写普通代码根本看不见 `Pin`，但写异步运行时必须直面它。

### 深入：栈 pin vs 堆 pin

我们上面讲了 `Box::pin`——堆 pin。但还有一种叫**栈 pin**，它把"不能 move"这个保证放在栈帧上：

```rust
// 不安全的栈 pin 示范（不要照抄，仅教学）：
fn drive_unsafe_future<F: Future>(f: F) -> F::Output {
    let mut f = f; // f 在栈上
    // SAFETY: 调用方必须保证 f 在被 pin 后不被 move——也就是后续代码不再
    // 使用 mut f（除了通过 pinned 句柄）。这个保证靠"shadowing"维持：
    // 把 f 这个名字让渡给 pinned，原 f 就再也访问不到。
    let mut pinned = unsafe { std::pin::Pin::new_unchecked(&mut f) };
    let waker = noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    loop {
        match pinned.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => return v,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}
```

栈 pin 的代价：零分配。它的安全性靠"shadowing"——`let mut f` 之后 `let pinned = ... &mut f`，从这一行起 `f` 这个名字不再被使用，编译器保证你拿不到原始的 `&mut f` 去 move 它。一旦 `f` 这一行代码所在的函数返回（栈帧消失），pinned 引用就失效——所以 `Pin<&mut T>` 不能跨 await 点（栈帧会变）。

堆 pin 的代价：一次堆分配（约 30ns）。换来"地址在堆上稳定，跨任意函数返回/任意线程都能用"。我们 task.rs 用 `Pin<Box<dyn Future>>`，因为 task 的 future 必须**跨越 worker 线程的边界**——它被 worker A 构造、可能被 worker B poll、被 reactor 在第三个线程 wake。栈 pin 完全不适用这种场景。

### 一个常见误解：Pin 不防止 drop

很多人第一次学 Pin 会以为"被 pin 的对象永远不能被销毁"。**不是**。Pin 防止的是 **move**（地址改变），不防止 **drop**（销毁）。被 pin 的对象正常 `Drop::drop`，只是 drop 之前它的地址一直固定。`Pin<Box<T>>` 的 `Drop` 会先 drop T（在原地址 drop），再释放堆内存——两步分开，安全。

为什么这点重要？因为 task 完成时 future 会被 drop（task.rs 里 `Poll::Ready(())` 之后 `drop(future)`）。这个 drop 是在**原来的堆地址**上跑的，正好能让 future 释放它持有的资源（reactor 反注册、mutex 解锁、socket 关闭……）。Pin 保证的是"drop 之前地址不变"，不是"永不 drop"。

---

## 三、Waker：被叫醒再 poll

### ENEMY：忙等的浪费

回到第一章末尾那个忙等执行器。它每秒 poll 几千次，CPU 满载。问题在哪？**执行器不知道 future 什么时候才会变 Ready**——只能反复问。

如果我们能给 future 一个"按钮"，它按下按钮就**通知执行器"该再问我了"**，执行器就不用反复问了——只在按钮被按下时才 poll。这个"按钮"就是 **Waker**。

### ANCHOR：Waker 是什么

`std::task::Waker` 的本质非常简单——**它是一个"被 clone、被 wake 都很便宜"的句柄**。一个 Waker 内部藏着：

1. 一个**指针**（指向某个 task 的状态）；
2. 一个 **vtable**（一组函数指针：clone、wake、wake_by_ref、drop）。

`waker.wake()` 干什么？**调 vtable 里的 wake 函数**，把 task 重新入队。具体怎么入队由提供 vtable 的那一方决定——通常是执行器。

所以 `Waker` 是个**完全类型擦除**的对象：你拿到一个 Waker，完全不知道它是给哪个 task 的、它的 wake 会做什么——你只能 clone 它、wake 它。

### LOW-FI：手写一个最简 Waker（RawWaker）

为了让你看清 Waker 的内部，我们手写一个最简版：用 `RawWaker` + `RawWakerVTable`。这一段是教学性参考，真实代码用 `std::task::Wake` trait 更安全。

```rust
// crates/forge-rt/src/lib.rs（节选）
use std::task::{RawWaker, RawWakerVTable, Waker};

pub fn noop_waker() -> Waker {
    unsafe fn no_op(_: *const ()) {}
    unsafe fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    // SAFETY: vtable 的函数都满足"被任意指针调用都不 UB"——我们这里全是 no_op。
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
```

逐字段解释：

- `RawWaker::new(data, vtable)`：data 是一个 `*const ()`——任意类型擦除指针。vtable 是四个函数指针（clone、wake、wake_by_ref、drop_data）。
- `Waker::from_raw(...)`：把 RawWaker 包成一个类型安全的 Waker。unsafe 因为：如果你给的 vtable 是错的（比如 wake 时访问了已经释放的内存），就是 UB。
- `clone(p)`：必须返回一个新的 RawWaker（Waker 可以被任意 clone）。我们的 noop 实现直接返回同样的 RawWaker。
- `wake(ptr)` / `wake_by_ref(ptr)`：被叫醒——这里什么都不做（noop）。
- `drop_data(ptr)`：释放 data——这里 data 是 null，noop。

这个 waker 被 `wake()` 时什么都不发生，适合**不需要调度**的场合（比如 `select!` 内部反复 poll，见第九章）。

### WRITE：真实的 Waker = schedule 入队

但真实运行时里 Waker 必须**真的把 task 重新入队**。Rust 给了一个安全的捷径：实现 `std::task::Wake` trait。

```rust
// crates/forge-rt/src/task.rs（节选）
use std::task::Wake;

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // 状态机去重 wake：只有 IDLE → QUEUED 才需要真入队。
        let prev = self.state.swap(QUEUED, Ordering::AcqRel);
        if prev == IDLE {
            (self.schedule)(self.clone());
        }
    }
}
```

`Arc<Task>: Wake` ⇒ 你可以用 `Waker::from(arc_task.clone())` 造一个 Waker，它的 vtable **自动**由 std 生成，wake 时调 `Task::wake`。

注意 **`schedule`**：这是 task 在被构造时存进来的闭包——它知道"这个 task 该被 push 到哪个队列"。reactor 拿到 waker、调 `wake()`，就触发 `schedule(task)`，task 回到执行器就绪队列里。这就是整个异步运行时**唯一的一条回路**。

### 逐拍手算 #2：Waker 驱动的 reactor→exec 时序

让我们**逐拍**画一个 `Delay(1ms)` 的完整生命周期，看看 Waker 是怎么把"睡眠"变成"事件"的。涉及的几方：

- **执行器**（worker 线程）：从就绪队列里取 task 跑 poll。
- **reactor**（独立后台线程）：维护一个"deadline → Waker"的表；用 `mio::Poll::poll(timeout)` 等到点。
- **task A**：一个 await 了 `Delay(1ms)` 的 task。

初始状态：

```
执行器就绪队列：[ A ]
reactor 表（Token → Waker）：{ }
A 的状态：QUEUED
```

**拍 1**：worker 取 A，poll A。A 的 future 走到 `Delay::poll`：

```rust
fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if Instant::now() >= self.deadline {
        return Poll::Ready(());
    }
    // 没到期——注册 waker 到 reactor。
    self.reactor.register_timer(self.deadline, cx.waker().clone());
    Poll::Pending
}
```

`cx.waker()` 是 worker 在调 poll 前给 A 套上的 waker（= `Arc<A>::waker()`）。`register_timer` 把它（连同 deadline）存进 reactor 表。

拍 1 结束状态：

```
执行器就绪队列：[ ]
reactor 表：{ (id=42, deadline=T0+1ms, waker=W_A) }
A 的状态：IDLE（poll 完，没被 wake）
worker：开始找下一个 task——队列空，去 condvar 上 wait。
```

**拍 2**（1ms 后）：reactor 线程在 `mio::Poll::poll(Some(1ms))` 里等到了 timeout，醒来扫表，发现 id=42 的 deadline 已到。

```rust
// crates/forge-rt/src/reactor.rs（节选）
let now = Instant::now();
for w in inner.drain_expired(now) {
    w.wake(); // ← W_A.wake()
}
```

`W_A.wake()` 触发 `Task::wake_by_ref(&arc_A)`：

```rust
let prev = self.state.swap(QUEUED, Ordering::AcqRel);  // IDLE → QUEUED
if prev == IDLE {
    (self.schedule)(self.clone());  // schedule(A) → push A 回就绪队列 + notify
}
```

拍 2 结束状态：

```
执行器就绪队列：[ A ]
reactor 表：{ }   ← 42 被 drain 掉
A 的状态：QUEUED
worker：被 condvar notify 叫醒。
```

**拍 3**：worker 醒来，从队列里取 A，再 poll A。这次 `Delay::poll` 检查 `Instant::now() >= deadline`——已过——返回 `Poll::Ready(())`。task A 完成，从队列里消失。

```
执行器就绪队列：[ ]
A 的状态：IDLE（完成）
worker：继续找下一个 task。
```

这就是一次完整的"等 1ms"——但**整个过程中没有任何线程在睡**：worker 真正睡的时间是被 condvar 控制的（有活干就 poll，没活干才 wait），1ms 的"等待"完全靠 reactor 的 timeout + Waker 路径在后台完成。如果同一时刻有 10000 个 task 在 await Delay，reactor 只需要**一张表**记录所有 waker，**一个线程**扫表——10000 个 timer 共用 1 个内核线程。

这就是异步的胜利：**等待的成本从"一个 OS 线程"降到"一行 HashMap entry"**。

### 深化：Waker 的"clone 友好"为什么这么重要

你可能注意到我们在 `register_timer` 时调了 `cx.waker().clone()`——把 waker **clone 一份**存到 reactor 表里。reactor 拿到这份 clone，到点调它的 `wake()`，原 waker（被 worker 持有的那份）什么都不发生。

这种"clone 后存到别处"的模式在异步代码里**到处都是**：

- task A 注册 waker 到 reactor 表（reactor 持有一份）。
- task A spawn 子 task B，把 A 的 waker 传给 B，B 完成时 wake 父 A（B 持有一份）。
- channel 的 sender 持有 receiver 注册过来的 waker，sender send 时 wake（sender 持有一份）。

所以 waker 必须能**廉价 clone**。`Waker::clone` 的代价 ≈ 一次 `Arc::clone`（增加引用计数 + 内存序 fence），大约 5–15ns。如果 waker 内部用了某种重分配——比如每 clone 一次复制一段大内存——那异步代码立刻会被这种隐式开销吃掉一半性能。这就是为什么 Rust 的 `RawWaker` 设计成"一个指针 + 一个 vtable"——clone 操作就是 vtable 里的一个函数指针，对应实现通常是"add ref count"。

### 深化：`wake` vs `wake_by_ref`

`std::task::Wake` 有两个方法：`wake(self: Arc<Self>)` 消费 self；`wake_by_ref(self: &Arc<Self>)` 借用 self。

为什么两个？因为 waker 的常见调用模式是"从一个 `&Waker` 出发去 wake"——你拿到的是引用，不是所有权。`Waker::wake(&self)` 内部就是 `self.waker.wake_by_ref()`——它**不能**消费 self（Waker 还可能被别处持有）。

我们 task.rs 的实现：

```rust
impl Wake for Task {
    fn wake(self: Arc<Self>) { self.wake_by_ref() }
    fn wake_by_ref(self: &Arc<Self>) {
        let prev = self.state.swap(QUEUED, Ordering::AcqRel);
        if prev == IDLE {
            (self.schedule)(self.clone());
        }
    }
}
```

`wake` 直接调 `wake_by_ref`——因为 schedule 闭包需要一个 `Arc<Task>`，我们无论如何都要 clone 一份。`wake_by_ref` 里调 `self.clone()`（Arc clone），把这份 clone 喂给 schedule。这样 waker 持有的那份 Arc 一直不被动用，可以反复被 wake（虽然我们的状态机会去重，第二次 wake 看到 QUEUED 不会再入队）。

---

## 四、单线程 `block_on`：把零件拼起来

### ENEMY：还差一个主循环

我们有了 Future、Pin、Waker——但还缺一个把它们组合起来的**主循环**。这个循环干两件事：

1. 从就绪队列里取 task，poll 它。
2. 队列空了但主 future 还没好——等被 wake。

### LOW-FI：`block_on` 草图

```rust
// crates/forge-rt/src/executor.rs（节选）
pub fn block_on<F, T>(future: F, reactor: &Reactor) -> T
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // 1) 就绪队列 + 一个"待处理计数 + Condvar"用来不丢唤醒。
    let queue: Arc<Mutex<VecDeque<Arc<Task>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let woken: Arc<(Mutex<usize>, Condvar)> = Arc::new((Mutex::new(0), Condvar::new()));

    // 2) schedule 回调：被 wake 时 push task + notify。
    let schedule = Arc::new(move |task: Arc<Task>| {
        queue.lock().unwrap().push_back(task);
        let mut c = woken.0.lock().unwrap();
        *c += 1;
        woken.1.notify_one();
    });

    // 3) 把主 future 包成 task，塞进队列。
    let (sender, receiver) = oneshot::channel::<T>();
    let main_task = Task::spawn(future, schedule, sender);
    queue.lock().unwrap().push_back(main_task);

    // 4) 主循环。
    loop {
        // 取一个 task，poll 它。
        if let Some(t) = queue.lock().unwrap().pop_front() {
            let w = t.waker();
            t.poll(&w);
            continue;
        }
        // 队列空：看主 future 完没。
        if let Some(v) = receiver.try_recv() {
            return v;
        }
        // 都没好——等 condvar（reactor 的 wake 会 notify_one）。
        let mut c = woken.0.lock().unwrap();
        while *c == 0 {
            c = woken.1.wait(c).unwrap();
        }
        *c -= 1;
    }
}
```

逐行讲：

- **`schedule` 闭包**就是 task 在被 `wake()` 时调的入队回调。它 push task、增加 `woken` 计数、`notify_one`。这一行**连接了 reactor 和 executor**——reactor 调 wake → wake 调 schedule → schedule 把 task 入队 + notify → 主循环醒。
- **`main_task`**：我们 spawn 一个 task，但**不**起 worker 线程——直接在当前线程跑它。task 的 future 跑完时通过 oneshot 把结果送出来。
- **主循环**：pop → poll → 再 pop → ...。队列空就检查主 task 完没；还没完就 condvar wait。这就是整个 event loop。

为什么用 Condvar 而不是原子 spin？因为 spin 会 100% 占满 CPU；Condvar 让线程**真的睡**——直到 wake 来 notify。注意：**这里线程睡觉是没问题的**——睡觉的代价（一次 syscall + 一次唤醒）远小于"poll 几千次空 future"的代价。

### ISO·ZOOM：`Task::poll` 内部的状态机

`block_on` 调 `t.poll(&w)` 时，task 内部跑了一个 CAS 状态机。我们看一眼：

```rust
// crates/forge-rt/src/task.rs（节选）
pub(crate) fn poll(self: Arc<Task>, waker: &Waker) {
    // CAS QUEUED → RUNNING。失败说明被人抢先了，return。
    if self.state.compare_exchange(QUEUED, RUNNING, AcqRel, Relaxed).is_err() {
        return;
    }
    // 取 future 独占 poll（不持锁）。
    let future = self.future.lock().unwrap().take().unwrap();
    let result = future.as_mut().poll(&mut cx);
    match result {
        Poll::Ready(()) => { /* 完成 */ self.state.store(IDLE, Release); }
        Poll::Pending => {
            // 放回 future。
            *self.future.lock().unwrap() = Some(future);
            // CAS RUNNING → IDLE。
            if self.state.compare_exchange(RUNNING, IDLE, AcqRel, Relaxed).is_err() {
                // 失败说明 poll 期间被 wake 写成了 QUEUED。
                // 把状态写回 IDLE，重新入队。
                self.state.store(IDLE, Release);
                (self.schedule)(self);
            }
        }
    }
}
```

最微妙的是 **`compare_exchange(RUNNING, IDLE)` 失败**这一支——它处理"poll 进行中时被 wake"的并发情况。设想：

- worker 正在 poll A；
- 同时 reactor 线程发现 A 注册的 timer 到了，调 `A.wake_by_ref()`，把 state 写成 QUEUED（因为 prev 是 RUNNING，不会重新入队）；
- worker 的 poll 返回 Pending，想 CAS RUNNING → IDLE——失败（state 是 QUEUED 不是 RUNNING）；
- worker 知道"poll 期间有人想叫醒我"——所以手动重新入队一次。

这个机制保证：**poll 期间到达的 wake 永远不会丢**。它是异步运行时里**最常见的并发 bug 源**——一个稍微不注意的写法就会让 task 永远不被再 poll（"stalled task"）。

### 深化：内存序为什么是 AcqRel

注意上面 CAS 用的是 `Ordering::AcqRel`。为什么？

- **Acquire**：保证这个 CAS **之后**的所有读写不会被重排到 CAS 之前。这对 wake 路径关键——`schedule(task)` 之前必须看到 task 状态被改成了 QUEUED。
- **Release**：保证这个 CAS **之前**的所有读写不会被重排到 CAS 之后。这对其他线程读 state 时看到一致状态关键——reactor 写完 deadline 后做 Release，executor 读 state 时做 Acquire，两者配对。

我们这块代码里：
- `state.swap(QUEUED, AcqRel)` 在 wake 里——swap 是 read-modify-write，AcqRel 既获取又释放，保证 wake 看到的 self 是"最新版本"，并把自己的写入发布给后续读者。
- `state.store(IDLE, Release)` 在 poll 结束——把"我已经 poll 完"这件事发布出去，让接下来的 wake 看到 IDLE 状态。
- CAS 失败的 fallback 里我们 `store(IDLE, Release)` 然后调 `schedule`——也是 Release，让并发 wake 看到状态变化。

错误的内存序（比如全用 Relaxed）会让 task 偶发性丢失 wake——在某些架构（ARM、PowerPC）上立刻能复现 bug，x86 因为是 strong memory model 大概率没事但仍然是 UB。M1 内存序那一章详细讲过这些——M9b 这里只是应用。

### 深化：为什么不直接持锁 poll

很多新手会想：`future` 字段为什么是 `Mutex<Option<...>>`？直接 `Option<...>` 不行吗？答案是：**poll 期间不能持锁**。设想如果我们用 `self.future.lock()` 持着锁去 poll：

1. worker A 在 poll task X 的 future；
2. X 的 future 内部 spawn 了子 task Y；
3. Y 内部又 await 一个 `oneshot::Receiver<X>` 等 X 完成；
4. 但 X 现在被锁住了——Y 想访问 X 的什么字段都得等 A 释放锁；
5. A 在等 X 的 future 返回，X 的 future 在等 Y 完成，Y 在等 A 释放 X 的锁——死锁。

虽然这个具体场景依赖具体 API 设计，但**一般原则**是：异步代码不要持锁 await。我们的 task.rs 用 take+drop lock+poll+put back 的模式，把锁的范围缩到最小——只保护"取/存 future"两个动作，poll 期间不持任何锁。这避免了"poll 期间触发别的锁竞争"的连锁问题。

---

## 五、Task = `Arc<Task>`：spawn 的真实形状

### ANCHOR：一个 task 的内部

```rust
// crates/forge-rt/src/task.rs（节选）
pub(crate) struct Task {
    state: AtomicU8,
    future: Mutex<Option<Pin<Box<dyn Future<Output = ()> + Send>>>>,
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
}
```

为什么是 `Arc`？因为 task 同时被**两处**持有：

1. **执行器就绪队列**里有一份（准备 poll）。
2. **Waker** 里持有一份（被 reactor 或别的 task clone 出来再 wake）。

Waker 是 `Clone` 的——你可以把它复制好几份（reactor 一份、别的 task 一份），任何一份被 `wake()` 都会把 task 入队。所以 Waker 必须用引用计数管理 task 的生命周期——这就是 `Arc`。

`spawn` 长这样：

```rust
// crates/forge-rt/src/task.rs（节选）
pub(crate) fn spawn<F, T>(
    future: F,
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
    sender: oneshot::Sender<T>,
) -> Arc<Task>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // 把 F::Output 包成 ()，跑完送 oneshot。
    let wrapped: BoxFuture = Box::pin(async move {
        let value = future.await;
        let _ = sender.send(value);
    });
    Arc::new(Task {
        state: AtomicU8::new(QUEUED),
        future: Mutex::new(Some(wrapped)),
        schedule,
    })
}
```

注意 `Box::pin(async move { ... })`：spawn 进来的任意 future 都被装箱 + pin。`async move` 块把"跑原 future + 送结果"两件事合成一个 `Output = ()` 的 future——这样所有 task 的 future 类型统一成 `dyn Future<Output = ()>`，**就绪队列里所有 task 都是同一种类型**。

### WRITE：spawn 返回 JoinHandle

`spawn` 不光返回 task——它**还**返回一个 `JoinHandle<T>` 给调用方，让调用方能拿到 future 的结果。

```rust
// crates/forge-rt/src/executor.rs（节选）
pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel::<T>();
    let task = Task::spawn(future, self.schedule.clone(), sender);
    (self.schedule)(task); // 入队
    JoinHandle { receiver, _rt: self.clone() }
}
```

`oneshot` 是 forge-pool 的线程安全 oneshot：task 跑完时 `sender.send(value)`，调用方 `JoinHandle::recv()` 拿到 value。这个 oneshot 是**同步阻塞**的（M5 风格），调用方调 `.recv()` 会真的 park 等待。

**为什么 JoinHandle 不是 async 的？** 因为我们的 oneshot 不支持"注册 waker"。真实 tokio 用一个**异步 oneshot**——它的 Receiver 实现了 Future，poll 时如果没就绪就把 task 的 waker 存进 sender 那一侧，sender send 时 wake。我们这里简化，只做同步 `recv`——这够教学用了，留作练习让读者把 oneshot 改成异步。

---

## 六、多线程执行器：M9a 的 worker 模型原样复用

### ANCHOR：和 M9a 的同构

`Runtime` 的 worker 循环**结构上**和 M9a 的 `StealingPool` 一模一样：

- N worker 线程，每个有 `Arc<LocalQueue>`（owner LIFO push/pop，thief FIFO steal）；
- 一个外部 injector（`Vec<Mutex<VecDeque>>`）；
- 一个 pending 计数 + Condvar，保证不丢唤醒；
- worker_loop：找活 → 干活；没活 → condvar wait。

唯一的差别：**任务类型从"闭包 `Box<dyn FnOnce()>`"换成"`Arc<Task>`"**。worker 取到 task 后不是 `task.run()`，而是 `task.waker()` + `task.poll(&waker)`。

```rust
// crates/forge-rt/src/executor.rs（节选）
fn worker_loop(
    state: Arc<PoolState>,
    schedule: Arc<dyn Fn(Arc<Task>) + Send + Sync + 'static>,
    _reactor: Reactor,
) {
    loop {
        if let Some(task) = find_work(&state) {
            let w = task.waker();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.clone().poll(&w);
            }));
            continue;
        }
        if state.shutdown.load(Ordering::Acquire) {
            if find_work(&state).is_none() {
                return;
            }
            continue;
        }
        let mut p = state.pending.lock().unwrap();
        while *p == 0 && !state.shutdown.load(Ordering::Acquire) {
            p = state.pending_cv.wait(p).unwrap();
        }
    }
}
```

`find_work` 的优先级：本地 LIFO → injector FIFO → 偷别人的 FIFO 端。和 M9a 完全一致——这一节不再重复，复习请回 M9a 的 [`v3_stealing`](../../crates/forge-pool/src/v3_stealing.rs)。

### ISO·ZOOM：worker loop 的"reactor 路径"

reactor 是一个**独立的 OS 线程**（`forge-reactor`），它和 worker loop 之间**唯一的通信**是通过 task 的 Waker。worker 不直接调 reactor，reactor 不直接调 worker——它们通过 task 这一层间接耦合：

- worker poll `Delay` → `Delay` 调 `reactor.register_timer(deadline, waker)`；
- reactor 到期 → `waker.wake()` → 触发 `schedule(task)` → push 到某 worker 的队列（或 injector 槽）。

这就是**关注点分离**：executor 关心"调度"，reactor 关心"操作系统说谁该醒"。两者通过 Waker 这个**值类型**解耦——任何执行器都能和任何 reactor 配对，只要它们都用 `std::task::Waker`。

---

## 七、最小 Reactor：把 epoll/kqueue/IOCP 包起来

### ENEMY：怎么"等到期"？

reactor 的核心难题：**它得知道"什么时候 timer 到了"**。

最笨的办法是 reactor 自己 sleep 一个最小粒度（比如 1ms）、醒来扫一遍表。但 1ms 的精度意味着每秒 1000 次唤醒——CPU 一直在被骚扰。

真实办法：**用操作系统的"等待多个事件"接口**——Linux 上是 `epoll`、macOS 上是 `kqueue`、Windows 上是 `IOCP`。这三个 API 共同的特点是：**你给它们一个事件列表 + 一个 timeout，它们能睡到"任何一个事件发生"或"timeout 到"**——一次 syscall 等多个事件。

`mio` crate 把这三个 API 抽象成一个统一的 Rust 接口：

```rust
let mut poll = mio::Poll::new()?;
let mut events = mio::Events::with_capacity(64);
// 注册一些事件源（fd）。
poll.registry().register(&mut source, token, interest)?;
// 等：直到任何一个 source 就绪或 timeout 到。
poll.poll(&mut events, Some(timeout))?;
// events 里现在是被触发的事件列表。
```

### LOW-FI：我们的最小 reactor

```rust
// crates/forge-rt/src/reactor.rs（节选）
pub struct Reactor {
    pub(crate) inner: Arc<ReactorInner>,
}

pub(crate) struct ReactorInner {
    slots: Mutex<HashMap<u64, Slot>>,       // Token → Waker + deadline
    timers: Mutex<BTreeMap<Instant, HashSet<u64>>>, // deadline → 一组 id
    next_id: AtomicU64,
    poll_waker: MioWaker,                    // 用来打断 poll
}
```

reactor 内部维护两张表：

- **`slots`**：每个注册项的"id → (Waker, deadline)"。
- **`timers`**：一张"deadline → 一组 id"的 BTreeMap，按 deadline 排序——`timers.keys().next()` 就是最近一个 deadline。

后台线程的循环：

```rust
// crates/forge-rt/src/reactor.rs（节选）
fn reactor_thread(inner: Arc<ReactorInner>, mut poll: Poll) {
    let mut events = Events::with_capacity(64);
    loop {
        // 1) 算 timeout：下一个 deadline 距 now 多久。
        let timeout = inner.next_deadline().map(|dl| {
            let now = Instant::now();
            if dl <= now { Duration::ZERO } else { dl - now }
        });
        // 2) 等：要么 timeout，要么被外部 wake_poll 打断。
        let _ = poll.poll(&mut events, timeout);
        events.clear();
        // 3) 把"已到期"的 timer 全部唤醒。
        let now = Instant::now();
        for w in inner.drain_expired(now) {
            w.wake();
        }
    }
}
```

这个循环干三件事：

1. **算 timeout**：扫 `timers` 找最小 deadline，算距 now 多久。
2. **mio::Poll::poll 等待**：这是 reactor 线程**唯一**的 sleep 路径——内核把它真的睡在 epoll_wait 上，要么 timeout 触发、要么 `poll_waker.wake()` 触发。这一睡可能睡几十毫秒、也可能睡几小时——只要没 timer 也没 I/O，reactor 一点 CPU 都不烧。
3. **drain_expired**：扫一遍 `timers`，把所有 deadline ≤ now 的 waker 取出来 wake。

`poll_waker` 是关键：外部线程注册新 timer 时（`register_timer`），可能新 timer 比当前 reactor 在等的 timer **更早**到期——这时必须打断 reactor 的 poll，让它重新算 timeout。`MioWaker::wake()` 就是干这个的：它往 reactor 注册的 token 0 上发个事件，`poll.poll` 立刻返回。

### WRITE：注册路径

```rust
// crates/forge-rt/src/reactor.rs（节选）
fn register_timer(&self, id: u64, deadline: Instant, waker: Waker) {
    {
        let mut slots = self.slots.lock().unwrap();
        slots.insert(id, Slot { waker: Some(waker), deadline });
    }
    self.timers.lock().unwrap()
        .entry(deadline).or_default().insert(id);
    // 叫醒 reactor 线程重算 timeout。
    let _ = self.poll_waker.wake();
}
```

注册路径做四件事：

1. 加锁 slots，插入新条目。
2. 加锁 timers，按 deadline 插入 BTreeMap。
3. wake reactor 线程（防止它还在等旧 timeout）。
4. reactor 线程醒，重新算 timeout，下次 poll 用新 timeout 等。

注意：**两个锁分别加**，不嵌套——避免死锁。这是并发编程的常识，M2 已经反复讲过。

### ISO·ZOOM：socket 也能这么干

当前我们的 reactor 只支持 timer。要支持 TCP socket，做两件事：

1. 在 mio::Poll 里 `registry.register(&mut tcp_stream, token, Interest::READABLE)`；
2. reactor 表里加一个 `Slot { kind: SourceKind::Tcp(token), waker }`；
3. reactor 线程 poll 后遍历 events，每个 event 的 token 反查到 waker，wake 它。

**结构完全不变**——只是多几种 SourceKind。tokio 的 reactor 就是这么干的。我们这版只实现 timer，是因为 timer 自包含、不依赖网络——教学更纯粹。把 socket 留给 M10。

### 深化：epoll 的"水平触发"和"边缘触发"

reactor 调 `mio::Poll::poll` 时它最终会调 Linux 的 `epoll_wait`。epoll 有两种触发模式：

- **水平触发（Level Triggered, LT）**：只要 fd 上**还有未读数据**，每次 epoll_wait 都会返回它。这是 mio 的默认。优点：容易使用，不怕"漏事件"；缺点：如果你没读完，epoll 会反复叫醒你——容易"惊群"。
- **边缘触发（Edge Triggered, LT）**：只在 fd **状态变化**时返回一次（从"无数据"变"有数据"那一刻）。后续即使还有数据，也不再来叫你。优点：每个事件只叫一次，CPU 占用低；缺点：你必须**一次性读完所有数据**（读到 `EAGAIN`），否则漏掉。

异步运行时几乎全用 ET（边缘触发），因为它和异步模型配合好——一次 wake 之后，task 自己 read 到 EAGAIN 才会重新 await。我们的 timer 实现其实没用到 fd（mio::Waker 是 epoll 内部特殊处理），所以这个区别暂时不影响我们。但理解它对 M10 写网络代码至关重要。

### 深化：为什么 reactor 是一个独立线程

reactor 在我们的实现里是**一个独立 OS 线程**。它**不**在 worker 池里——它的唯一职责是 `mio::Poll::poll` + wake wakers。这种设计有几个好处：

1. **专注**：reactor 线程不被任何 user task 占用，永远在等 epoll。如果它去跑 user task，会延迟"应该被立刻 wake 的 waker"——增加延迟。
2. **公平性**：所有 task 共用一个 reactor，没人能"独占"reactor 线程。
3. **简单**：单线程 reactor 不需要锁竞争它的内部表（虽然我们仍然用 Mutex 保护，是为了让外部线程能注册）。

tokio 的 multi-thread runtime 也是这种"独立 reactor 线程"模型。某些运行时（如 smol 的 async-io）用更轻量的"按需起 reactor 线程"，但本质一样——reactor 和 executor 解耦。

### 深化：reactor 表的内存成本

我们 reactor 用 `HashMap<u64, Slot>` 存 waker。每个 Slot 大约：Waker（24–32 字节）+ deadline（Instant 16 字节）+ HashMap overhead（一个 entry 至少 32 字节）≈ 80 字节。10 万个 timer = 8MB 内存。这是 reactor 的内存代价——非常便宜。

相比之下，10 万个 OS 线程的栈空间 = 10 万 × 8MB = 800GB——你根本开不出这么多。这是异步的另一面胜利：**内存成本也降了三个数量级**。

reactor 表的"键"（u64 token）只是个不透明 ID。tokio 用一个更紧凑的"SlotMap"——key 复用、版本号防 ABA——能进一步降低内存。我们这版用 HashMap 已经够教学用。

### 深化：为什么我们用 BTreeMap 排 deadline

`timers: Mutex<BTreeMap<Instant, HashSet<u64>>>` 这张表是按 deadline 排序的。为什么用 BTreeMap 而不是 HashMap？

因为 reactor 线程每轮都需要**找最小 deadline**——`timers.keys().next()`。BTreeMap 的最小 key 查询是 O(log n)；HashMap 找最小值是 O(n)。在 10 万个 timer 的场景，每次扫表都 O(n) 会吃掉不少 CPU。

BTreeMap 的代价是插入也是 O(log n)——但插入发生在"future 被 poll 时注册"，频率远低于"reactor 找最近 deadline"。trade-off 是对的。

更激进的实现用 `BinaryHeap`（堆）—— O(log n) 插入 + O(1) 查最小。但堆不支持"任意删除"——我们 unregister 时要能删任意 token，BTreeMap 用 `entry(deadline)` + `HashSet<id>` 能干净删，堆得自己写。教学上 BTreeMap 更直观，我们用它。tokio 实际用一个分层的"时间轮"——比堆更快（O(1) 摊销），但实现复杂得多。

---

## 八、`spawn().await`：返 JoinHandle

我们已经看过了 `spawn` 返回 `JoinHandle`，但 JoinHandle 当前是同步的——`recv()` 阻塞。真实异步代码里你想 `let v = rt.spawn(async {...}).await;`——这就需要 JoinHandle 实现 Future。

### 当前简化的样子

```rust
// crates/forge-rt/src/executor.rs（节选）
pub struct JoinHandle<T> {
    pub(crate) receiver: oneshot::Receiver<T>,
    pub(crate) _rt: Runtime,
}

impl<T> JoinHandle<T> {
    pub fn recv(self) -> T {
        self.receiver.recv()  // 同步阻塞
    }
}
```

`oneshot::Receiver::recv` 用 futex 等到 sender send——这一步是真阻塞。在异步上下文里调它会**阻塞 worker 线程**——这是异步里的大忌（"async 里同步阻塞"会拖慢整个 worker）。

### 改造方向（留作练习）

要把它变成异步，需要：

1. 一个**异步 oneshot**——它的 Receiver 实现 Future：poll 时 if 未就绪就把当前 waker 注册进 sender 那侧，sender send 时 wake。
2. 然后 `JoinHandle` 的 `Future` impl 直接 delegate 给 `receiver.poll()`。

我们这版用同步 oneshot（forge-pool::oneshot）是因为它在 M5 已经实现成熟、稳定。异步 oneshot 留作 M9b-patterns 章节的练习。教学完整性不受影响——教程的核心是讲清"executor + reactor 怎么工作"，不是讲清"所有异步原语怎么实现"。

---

## 九、`select!`：两个 future 谁先 Ready 谁赢

### ENEMY：等"任意一个"

设想你在写一个 web server。客户端发了一个请求，你想：要么 100ms 内收到完整请求，要么超时关连接。这是一个"两个 future 谁先 Ready 谁赢"的场景——`request_future` 和 `timeout_future`。

如果两个 future 都跑完，结果 OK——但你**多等了一次**（如果 timeout 先 Ready，request 还在傻等）。更糟的是，**你已经放弃了 request**，但 request 内部可能还持有 mutex、socket buffer、子任务——这些资源得释放。

`select!` 就是解决这个的：它**等任意一个 Ready**，返回那个结果，**同时 drop 掉输掉的另一个**。

### 逐拍手算 #3：select! 的分支 drop

让我们**逐拍**画一个例子：F1 是 `Delay(50ms)`，F2 持有一个假想的 Mutex guard。select 等谁先 Ready。

```
拍 0（select 开始）：
状态：
  F1：未就绪
  F2：未就绪，但**持有 Mutex guard**（lock_guard alive）
  Mutex：被 F2 锁住，别人想 lock 会被阻塞。

拍 1（poll F1）：未到 50ms → Pending。
拍 2（poll F2）：未 Ready → Pending。

...

拍 N（50ms 到）：
  F1 poll → Ready(())。
  select 返回 SelectOutput::Left((), F2)。
```

注意：返回 `Left(v, F2)` 时，**F2 还活着**——它被作为返回值交给调用方。这是我们这个 select 的设计：**输掉的 future 不被自动 drop，而是还给调用方**。让调用方决定要不要继续跑、还是 drop。

```
拍 N+1（调用方拿到 F2，决定 drop）：
  drop(F2) →
    F2 的 Drop 跑：
      - Mutex guard 的 Drop 跑：unlock Mutex。
      - F2 里可能还注册了 reactor 的 timer：反注册。
      - 如果 F2 spawn 了子任务，那些子任务……（这个比较复杂，留给 actor 模型章节）

拍 N+2（Mutex 被 unlock 后）：
  别的线程 lock 这个 Mutex 现在能拿到了——F2 的资源释放完成。
```

### "忘记 drop 输掉的分支"是常见 bug

如果我们的 select 把 F2 悄悄泄漏（比如把它存到一个永不清理的 cache 里），那么：

- F2 的 Mutex guard 永远不释放 → 别的线程拿不到锁 → **死锁**。
- F2 注册的 socket buffer 不释放 → **fd 泄漏**（最终 process 撑爆 fd 表）。
- F2 内部的内存不释放 → **内存泄漏**。

`select!` 的安全保证就在这里：**它返回输掉的 future 给你，让你必须显式决定它的命运**——你不写 `drop(loser)` 就直接丢弃，编译器警告 unused；你写 `drop(loser)` 就清干净。

### LOW-FI：手写一个 select

```rust
// crates/forge-rt/src/lib.rs（节选）
pub enum SelectOutput<A: Future, B: Future> {
    Left(<A as Future>::Output, B),
    Right(<B as Future>::Output, A),
}

pub fn select<A, B>(mut a: A, mut b: B) -> SelectOutput<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = Pin::new(&mut a).poll(&mut cx) {
            return SelectOutput::Left(v, b);
        }
        if let Poll::Ready(v) = Pin::new(&mut b).poll(&mut cx) {
            return SelectOutput::Right(v, a);
        }
        std::thread::yield_now();
    }
}
```

这是个**同步函数**——它在循环里反复 poll 两个 future，谁先 Ready 就返回谁、把另一个作为返回值带出来。生产环境的 `tokio::select!` 是个**宏**，它把"等任意分支 Ready"编译成异步代码（每次 poll 都 yield 回执行器，而不是 spin）。但**核心思想完全一样**：

1. 顺序 poll 每个分支；
2. 谁先 Ready 谁赢；
3. 输的分支被 drop（宏版本是隐式 drop，函数版本是显式返回给调用方）。

注意 `A: Unpin` 约束——我们的 select 用 `Pin::new(&mut a)` 把 `&mut A` 升级成 `Pin<&mut A>`。这只有 `A: Unpin` 才合法（因为 `Pin::new` 要求 inside 不被 pin 后 move——`Unpin` 类型本来就能 move，所以 pin 它没意义、也就没风险）。

### 深化：tokio::select! 长什么样

真实的 `tokio::select!` 是一个**过程宏**，它把"等任意分支 Ready"展开成异步代码。结构骨架（简化版）：

```rust
// 等价于：
// tokio::select! {
//     v = fut1 => println!("fut1 won: {v}"),
//     v = fut2 => println!("fut2 won: {v}"),
// }
//
// 展开后大致是：
let mut fut1 = fut1;
let mut fut2 = fut2;
loop {
    let poll1 = Pin::new(&mut fut1).poll(&mut cx);
    if let Poll::Ready(v) = poll1 {
        drop(fut2); // ← 显式 drop 输掉的分支！
        println!("fut1 won: {v}");
        break;
    }
    let poll2 = Pin::new(&mut fut2).poll(&mut cx);
    if let Poll::Ready(v) = poll2 {
        drop(fut1);
        println!("fut2 won: {v}");
        break;
    }
    // 都没 Ready——yield 回执行器。
    std::future::poll_fn(|cx| Poll::Pending).await;
}
```

注意那个 `drop(fut2)`——宏自动生成的 cleanup 代码。这正是 select! 区别于"忘记 drop"的安全保证。tokio 的实现还更细致：它处理了"分支用 `&mut` 引用 future 而不是消费它"的情况（用 `FutureExt::change_context` 之类的高级技巧），但**核心是同一个**：谁先 Ready 谁赢，输的那个被显式 drop。

### 为什么"忘记 drop 输掉的分支"在异步里特别危险

在同步代码里，"忘了 drop 一个对象"通常是性能问题（内存泄漏）——不会立刻爆炸。在异步里，**它会直接死锁**。原因：

1. **mutex**：输掉的 future 如果持有 async mutex（如 tokio::sync::Mutex 的 guard），它的 drop 是"unlock"——如果它没被 drop，mutex 永远不解锁，所有想 lock 它的 task 都死等。
2. **channel permit**：tokio 的 mpsc 有"permit"概念——sender 拿到 permit 才能发。如果输掉的 future 持有 permit 没 drop，channel 的容量永远少 1。
3. **reactor 注册**：输掉的 future 如果注册了 timer 或 socket 到 reactor，没 drop 它就不会反注册——reactor 表里留着一个永远 wake 不出的 ghost waker，wake 它时把一个早已完成的 task 重新入队（task 状态机兜底会拒绝再 poll，但表项仍在，长期看是内存泄漏）。

这就是为什么 Rust 的 select 设计成"显式 drop"——它强迫你想清楚资源怎么释放。其他语言（Go 的 select）靠 GC 兜底——但 GC 兜底意味着 mutex unlock 可能延迟到下一个 GC 周期，那段时间里 mutex 是锁住的。Rust 的 RAII 让"丢一个值就立刻释放它的资源"——这让 select! 的 cleanup 行为**可预测**。

---

## 十、async/await 是状态机语法糖（轻点带过）

你写：

```rust
async fn fetch_user(id: u64) -> User {
    let raw = fetch_url(format!("/users/{id}")).await;
    parse_user(raw).await
}
```

编译器把它**脱糖**成一个状态机：

```rust
enum FetchUserFut {
    Start { id: u64 },
    AwaitingFetch { fut: FetchUrlFut, id: u64 },
    AwaitingParse { fut: ParseUserFut },
    Done,
}

impl Future for FetchUserFut {
    type Output = User;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<User> {
        loop {
            match &mut *self {
                FetchUserFut::Start { id } => {
                    let fut = fetch_url(format!("/users/{id}"));
                    *self = FetchUserFut::AwaitingFetch { fut, id: *id };
                }
                FetchUserFut::AwaitingFetch { fut, .. } => {
                    let raw = ready!(Pin::new(fut).poll(cx));
                    let fut = parse_user(raw);
                    *self = FetchUserFut::AwaitingParse { fut };
                }
                FetchUserFut::AwaitingParse { fut } => {
                    let user = ready!(Pin::new(fut).poll(cx));
                    *self = FetchUserFut::Done;
                    return Poll::Ready(user);
                }
                FetchUserFut::Done => panic!("poll after Ready"),
            }
        }
    }
}
```

每个 `await` 点是一个**状态切换**。状态之间转换靠 `poll` 驱动——每次 poll 走零或多步状态转换，直到撞上一个"子 future 返回 Pending"（`ready!` 宏会 early return Poll::Pending）或自己返回 Ready。

**关键**：这个状态机 struct 内部**自引用**——`AwaitingFetch { fut, id }` 里 fut 可能引用了 `id`（或者更准确地说，编译器生成的代码里 fut 内部存了 `&id`）。这就是为什么 async fn 生成的 future 是 `!Unpin`，必须被 pin 才能被 poll——回到第二章的伏笔。

**`async` 是这个状态机的 `enum` 定义；`await` 是这个状态机的状态切换**。你写 `async fn` 时写的代码不是函数体——它是**状态机的转换逻辑**。函数从来不被直接调用；它被实例化成 enum，然后被某个执行器 poll。

### 深化：`async fn` 的零成本细节

编译器在生成这个 enum 时做了几件"零成本"的优化：

1. **借用的局部变量直接进 enum**：原 async fn 里 `let raw = ...await;` 的 `raw` 是 enum 的一个字段（在 `AwaitingParse` 状态里）。它**不**存在栈帧上——因为这个 enum 不在栈帧上（它在堆上，被 task 持有）。
2. **每个 await 点对应一个 enum 变体**：3 个 await 点 → 至少 3 个变体（不算 Start/Done）。这导致 async fn 的 enum 大小 = max(各变体大小) + tag。如果你 async fn 里有大的局部变量，enum 会变大——这是为什么"async 函数应该尽量小"的实践依据。
3. **`!Unpin` 自动派生**：enum 含有自引用字段（编译器检测到），编译器自动给它加 `!Unpin` 标记（通过 `PhantomPinned`）。

为什么 enum 不能被自动 `Unpin`？因为 enum 里某个字段可能引用另一个字段的地址（编译器为了让 await 之间共享 local 变量而插入的引用）。一旦 enum 被 move，那些引用就悬空。`!Unpin` 让"必须 pin 才能 poll"成为编译期强制——你拿不到 `&mut Self`，只有 `Pin<&mut Self>`。

### 深化：嵌套 await 的状态机叠加

```rust
async fn outer() -> u32 {
    let a = inner1().await;  // ← await 1
    let b = inner2(a).await; // ← await 2
    a + b
}
```

outer 编译成 outer_fut，它内部"持有" inner1_fut 和 inner2_fut。poll outer_fut 时：

1. outer_fut 状态机走 Start → 内部 poll inner1_fut。
2. inner1_fut 自己又是个状态机，走它自己的转换。
3. inner1_fut 返回 Pending → outer_fut 透传 Pending。
4. inner1_fut 返回 Ready(v) → outer_fut 把状态推进到 AwaitingInner2，内部开始持有 inner2_fut。
5. poll inner2_fut，循环。

整个"嵌套"是 enum 的字段嵌套——`OuterFut { inner: InnerFut }`。每次 poll 都是一层层往下递归 poll。这就是为什么 await 是"零成本"的——它**不创建线程、不分配新栈**，只是在一个栈帧里递归调 poll。

但代价是：**栈深度可能爆炸**。一个深度 100 的 await 链意味着 100 层 poll 递归——如果每层 future 都 poll 它的子 future，栈涨 100 帧。tokio 等运行时用 `boxing` 在 await 链不深的边界把 future 装箱（`Box::pin`），打断递归——把"无限递归"变成"堆上的指针跳转"。

---

## 十一·补、并发 vs 并行：再厘清一次

这两个词被混用得很厉害，让我们最后再厘一次：

- **并发（Concurrency）**：多个任务**在同一个时间段内**被推进——可能交替推进（A 跑 10ms、B 跑 10ms、A 跑 10ms ……）。强调"逻辑上的同时"。
- **并行（Parallelism）**：多个任务**在同一个时刻**被推进——需要多核。强调"物理上的同时"。

异步是**并发**工具，不是并行工具。`async fn` 让你在 1 个线程上并发处理 10000 个连接——但这 10000 个连接**不是同时**跑，而是被 worker 线程**轮流** poll。要并行，你需要多 worker 线程（我们 Runtime 的 4 个 worker），它们各自 poll 自己队列里的 task——这就是并行 + 并发。

一个常见误解：把异步代码放到多核机器上不一定更快。如果你的 async fn 全是 CPU 计算（没有 await 点），它在 1 个 worker 上跑就占满 1 个核——加更多 worker 没用，因为 task 没有让出点。**异步解决 I/O 等待问题，不解决 CPU 密集计算问题**。后者要靠 M9a 的并行池（rayon 风格），或者 M9b + 多 worker 把 CPU 密集 task 切成多份 spawn 到不同 worker。

实际上 tokio 的最佳实践是：**异步池（reactor + executor）处理 I/O，rayon 池（work-stealing 闭包池）处理 CPU**。两套池子并存，各干各的。我们 forge-rt + forge-pool 加起来正好对应这个组合。

### 一个常被忽略的事实：异步的"零成本"是有代价的

Rust 文档反复强调 async/await 是"零成本抽象"——意思是它**不引入运行时开销**（不像 Go 的 goroutine 有调度器栈、不像 Java 的虚拟线程有 JVM 帮你管）。但你仍然付出了一些代价：

- **编译器生成的状态机比手写循环大**：一个 async fn 编译出的 enum 通常比等价的同步代码体积大几倍。如果你的 hot loop 里 future 太多，instruction cache 命中率下降。
- **每个 task 一次堆分配**（`Arc<Task>` + `Pin<Box<Future>>`）：高频 spawn 的场景这成本不小。tokio 有 `LocalSet` 用栈分配优化单线程场景。
- **await 点的额外状态切换**：每个 await 是一次 poll return + 一次 wake 重入——纯计算路径绕这些是有开销的。

这些代价在 I/O 重的场景里完全值得（你省下了线程切换），但在 CPU 重的场景里反而是负担。所以**别一股脑把所有函数改成 async**——只在有 I/O 的边界上 async。

---

## 十一、把 M9b 装回去：完整运行时

现在我们手里有：

- `Task`（第五章）：被调度的单位。
- `block_on`（第四章）：单线程主循环。
- `Runtime`（第六章）：多线程工作窃取执行器。
- `Reactor`（第七章）：操作系统事件翻译。
- `Delay`（第一章）：最简 future，驱动 reactor 路径。
- `select`（第九章）：多 future 竞争。

跑一个完整例子：

```rust
// tests/m9b_delay_concurrent.rs（节选）
let reactor = Reactor::new().expect("reactor");
let rt = Runtime::new(4, reactor.clone()).expect("runtime");

let counter = Arc::new(AtomicUsize::new(0));
let mut handles = Vec::new();
for _ in 0..8 {
    let c = counter.clone();
    let r = reactor.clone();
    handles.push(rt.spawn(async move {
        Delay::new(r, Duration::from_millis(60)).await;
        c.fetch_add(1, Ordering::Relaxed);
    }));
}
for h in handles { h.recv(); }
```

8 个 task，每个 await 60ms。如果是同步代码（每 task 一个 thread + `thread::sleep`），总耗时 = 8 × 60ms = 480ms（串行）或 60ms（8 核并行但占 8 个 OS 线程）。我们这版用 4 个 worker 线程，**总耗时远小于 480ms**——因为 8 个 task 在 4 个线程上并发 await，每个 Delay 在 reactor 表里只是一行 entry，不占线程。

实测：

```
running 2 tests
test runtime_drives_many_concurrent_delays_via_workstealing ... ok
test many_concurrent_delays_under_block_on ... ok
```

——通过。这就是异步的胜利：**并发度不再受限于线程数**。

---

## 十二、L1–L5：五层理解

- **L1（字面）**：`Future::poll` 返回 `Poll::Ready` 或 `Poll::Pending`；`Waker::wake` 把 task 重新入队。
- **L2（机制）**：executor 反复 poll；reactor 监听 OS 事件；两者通过 Waker 协作。
- **L3（设计）**：Pin 让自引用 future 安全；Task 用 Arc 共享给 executor + Waker；schedule 闭包解耦 executor 和 reactor。
- **L4（边界）**：当前 reactor 只支持 timer；JoinHandle 是同步的；select 是 spin 版（生产用宏版）。
- **L5（直觉）**：异步不是"魔法让等待变快"，是"让等待的成本从线程降到内存 entry"。10000 个 task 同时 await，共用 1 个 reactor 线程 + 1 张表。

---

## 十三、自检清单

读完这一章，你应该能：

- [ ] 解释 `Future::poll` 的两个返回值各自意味着什么。
- [ ] 用餐厅服务员类比解释同步 vs 异步的区别。
- [ ] 画出"自引用 struct 被 move 后 ptr 悬空"的逐拍过程。
- [ ] 解释 `Pin<&mut Self>` 为什么能让自引用 future 安全。
- [ ] 手写一个 noop Waker（用 `RawWaker + RawWakerVTable`）。
- [ ] 解释 `Task::wake_by_ref` 里状态机三态（IDLE/QUEUED/RUNNING）的转换。
- [ ] 画出 reactor → wake → schedule → executor 的完整时序（拍 1/2/3）。
- [ ] 解释 `mio::Poll::poll` 怎么让 reactor 线程不烧 CPU。
- [ ] 解释 select 为什么必须 drop 输掉的分支。
- [ ] 把 `async fn` 脱糖成一个 enum 状态机（至少说清"await = 状态切换"）。
- [ ] 说清"并发不是并行"——分别给出一个"并发但不并行"和一个"并行但不并发"的例子。
- [ ] 解释 `task.rs` 里 `compare_exchange(RUNNING, IDLE)` 失败的那一支为什么必须重新入队——它处理的并发场景是什么。
- [ ] 解释为什么 reactor 表用 BTreeMap 排序 deadline 而不是 HashMap。
- [ ] 解释 `Box::pin` 和 `Pin::new_unchecked` 各自的安全条件，以及在什么场景下用哪个。

---

## 十四、动手清单

- [ ] 把 forge-rt 的 reactor 加上 TCP 支持：用 `mio::net::TcpListener`，注册到 registry，accept 后把 stream 注册成 READABLE，wake 对应 task。
- [ ] 把 JoinHandle 改成真异步：实现一个 async oneshot（Receiver 实现 Future，poll 时注册 waker）。
- [ ] 实现一个 `interval(ms)` future：每 ms 触发一次，永不结束（用 mio 的 timerfd 或自己的 deadline 链）。
- [ ] 写一个 `join_all(vec_of_futures)` future：等所有 future 都 Ready。
- [ ] 把 forge-rt 跑成一个 mini HTTP server：accept 一个连接就 spawn 一个 task，task 里读完请求、写响应。
- [ ] 用 `perf top` 对比"1000 个 thread + sleep"vs"1000 个 task + Delay"在 1 秒里的 ctx-switch 次数——异步应当是 ~0。
- [ ] **加一个"task 计数器"**：Runtime 内部追踪当前 alive 的 task 数量；spawn 加 1，task 完成减 1。在 main 里每秒 print 一次。这能帮你看出 task 是否泄漏（数量持续增长）。
- [ ] **加 task cancellation**：在 JoinHandle 上加一个 `cancel()` 方法——drop future 立刻返回（Drop 跑 cleanup）。这能让你看见 task 被取消时它持有的 reactor 注册如何被反注册。
- [ ] **比较 `Delay + spin poll` vs `Delay + reactor` 的 CPU 占用**：写两个版本，分别跑 1000 个 Delay(1s)，用 `top` 看 CPU——前者应该接近 100%，后者应该接近 0%。
- [ ] **写一个 `race!` 宏**：参考 `tokio::select!`，写一个能 race 任意数量 future 的宏。关键挑战：drop 输掉的分支时要按正确顺序 drop（逆序，避免悬空引用）。

---

## 附：模块对照表

| forge-rt 文件 | 对应概念 | 教程章节 |
|---|---|---|
| `src/lib.rs` (`Delay`, `select`, `noop_waker`) | 最简 future / 竞争 / 手写 waker | 一、三、九 |
| `src/task.rs` | Task = Arc + state machine | 五 |
| `src/executor.rs` | block_on + 多线程 Runtime | 四、六、八 |
| `src/reactor.rs` | mio-based reactor | 七 |
| `tests/m9b_delay_block_on.rs` | block_on + Delay | 一、四 |
| `tests/m9b_runtime_spawn.rs` | 多线程 spawn | 五、六 |
| `tests/m9b_delay_concurrent.rs` | 并发 Delay 验证 | 七、十一 |
| `tests/m9b_select.rs` | select + 输家 drop | 九 |

---

# 第十五部分：协程 / 生成器 —— 把"暂停 + 吐值 + 恢复"做成一个类型

> 对应代码：`crates/forge-rt/src/coroutine.rs`
> 对应测试：`crates/forge-rt/tests/m9b_coroutine_basic.rs`、`tests/m9b_coroutine_file.rs`

## 一、先在身体里制造一个问题

你已经能写 `async fn`，能 `spawn` 任务，能用 `Delay` + reactor 让"等待"几乎不占线程。现在停下来，看一段不可能写出来的代码：

```rust
fn two_values() -> ? {
    yield 1;
    yield 2;
}
```

你想要什么？你想要一个函数，它**执行到一半**把 `1` 塞出去给调用方，然后**停住**；调用方处理完 `1`，再回来喊一声"继续"，函数从 `yield 1;` 这一行**接着跑**，把 `2` 吐出去。普通函数做不到——普通函数一调用就从头跑到尾，只 return 一次。

这种"能暂停、能吐值、能恢复"的函数，名字叫**生成器**（generator），它是协程（coroutine）的一个特例。Python 里写作：

```python
def two_values():
    yield 1
    yield 2
```

Python 用 `yield` 关键字支持。Rust 稳定版没有 `yield`，要等 nightly 的 `Coroutine` trait 才有语法。但**你已经会写了**——你前几章一直在用一种叫 `Future` 的东西。Future 不就是"能暂停（`Poll::Pending`）、能恢复（再 poll 一次）、能结束（`Poll::Ready(v)`）"的东西吗？

唯一差别：Future 只在**结束时**吐一个值，而生成器想吐**多个**。这个差别是这一章要消除的全部内容。

## 二、画面先：生成器是台"按一下出一张牌"的自动发牌机

把函数想象成一台机器：你按一下按钮（= resume），它**发一张牌出来**然后停住；你**再按一下**，它发下一张；按到没牌可发，机器"咔哒"一声显示"已空"。

普通函数不是这样的。普通函数是你一喊它，它一口气把所有事干完、把所有牌撒在你头上、然后消失。你没法"喊一半"。

Future 呢？Future 是台"按一下问你好了没"的机器：你按一下（= poll），它说"没好"或"好了，结果在这"。它最多**只在最后一次按**的时候给你一个值。

生成器 = 把 Future 的"最后一次才出值"改成"每一次按都出一个值、最后一次出终结信号"。机械结构完全一致。

## 三、手算：`gen { yield 1; yield 2; }` 的状态转换

这是这一部分最重要的一节。我们手算一遍，让你**亲眼看见**生成器怎么从一段源码变成一个状态机。

源码（假想语法）：

```
gen {
    yield 1;
    yield 2;
}
```

编译器把它脱糖成一个**带状态字段的 struct** + 一个 `resume` 方法。状态字段有四态：`Start`、`State1`（= yield 1 之后）、`State2`（= yield 2 之后）、`Done`。每次调 `resume` 推进一拍：

| 调用前状态 | resume 干了什么                                       | 吐出       | 调用后状态 |
|------------|-------------------------------------------------------|------------|------------|
| Start      | 跑到第一个 `yield`，把 1 塞出去                       | Yielded(1) | State1     |
| State1     | 从 `yield 1;` 之后恢复，跑到 `yield 2;`，把 2 塞出去  | Yielded(2) | State2     |
| State2     | 从 `yield 2;` 之后恢复，遇到函数结尾，返回            | Complete(()) | Done     |
| Done       | 已经跑完，没东西可推进                                | None       | Done       |

这就是 `coroutine.rs::HandGen` 里那个 enum：

```rust
pub enum HandGen {
    Start,
    State1,
    State2,
    Done,
}
```

`resume` 用 `match self` 把"当前状态"映射到"该做的事 + 下一个状态"。`coroutine.rs` 里的实现只有 20 行，但这 20 行就是这个状态机的全部——你能用纸笔把表 1 默出来，就理解了生成器。

### 把这个状态机再走一遍——但这次有中间变量

很多读者看完表 1 觉得"懂了"，但一遇到 `let total = 0; yield 1; total += 1; yield 2; total += 2;` 这种**中间有变量**的生成器就懵了。中间变量存在哪里？答案：存在**状态机的字段里**。

让我们把生成器源码改成：

```
gen {
    let mut total = 0;
    total += 1; yield total;        // yield 1
    total += 2; yield total;        // yield 3
    yield total;                     // yield 3 (没改)
}
```

编译器脱糖时，`total` **不再是栈上的局部变量**——它必须被存进状态机的 struct 里，否则下次 resume 时栈帧已经没了，`total` 就丢了。脱糖后大致：

```rust
struct GenWithState {
    state: State,
    total: i32,          // ← 局部变量升级为字段!
}

enum State { Start, AfterFirstYield, AfterSecondYield, AfterThirdYield, Done }

impl Generator for GenWithState {
    type Yield = i32;
    type Return = ();

    fn resume(&mut self) -> Option<GenState<i32, ()>> {
        match self.state {
            Start => {
                self.total = 0;          // let mut total = 0;
                self.total += 1;         // total += 1;
                self.state = AfterFirstYield;
                Some(GenState::Yielded(self.total))   // yield total (=1)
            }
            AfterFirstYield => {
                self.total += 2;         // total += 2; (total = 3)
                self.state = AfterSecondYield;
                Some(GenState::Yielded(self.total))   // yield 3
            }
            AfterSecondYield => {
                // yield total; (total 还是 3,没改)
                self.state = AfterThirdYield;
                Some(GenState::Yielded(self.total))   // yield 3
            }
            AfterThirdYield => {
                self.state = Done;
                Some(GenState::Complete(()))
            }
            Done => None,
        }
    }
}
```

**核心洞察**：生成器的"暂停"不是真的"让 CPU 睡着"——它只是把当前所有局部变量**存进 struct 字段**，然后 return。下次 resume 时从 struct 字段**恢复**这些变量，接着跑。"暂停 = 把栈上的东西挪到堆上；恢复 = 把堆上的东西挪回栈上"。这就是协程比线程便宜几十倍的原因：线程的"暂停"要存整套寄存器 + 切换栈（~微秒级），协程的"暂停"只是 return（~纳秒级）。

理解了这一点，`async fn` 里的所有局部变量（`let a = ...; let b = ...;`）你都该重新理解了——它们**不是**栈帧上的局部变量，而是编译器生成的状态机 struct 的字段。这些字段在 future 被 `Box::pin` 时落到堆上，在 task 被 spawn 时进入执行器的队列。`async fn` 的"栈"其实是堆上的 struct——这正是它便宜的原因。

## 四、对照：`async fn` 的脱糖

为什么我们要在异步运行时这一章讲生成器？因为 `async fn` **就是**编译器生成的状态机。你看：

```rust
async fn fetch_two() -> u32 {
    let a = fetch_x().await;  // 第一个 await 点
    let b = fetch_y().await;  // 第二个 await 点
    a + b
}
```

编译器把它脱糖成大致这样：

```rust
enum FetchTwo {
    Start,
    AfterX { a: u32, fut_x: FetchX },
    AfterY { a: u32, b: u32, fut_y: FetchY },
    Done,
}

impl Future for FetchTwo {
    type Output = u32;
    fn poll(...) -> Poll<u32> {
        match self {
            Start => { /* poll fut_x, Ready 就切到 AfterX */ }
            AfterX { .. } => { /* poll fut_y, Ready 就切到 AfterY */ }
            AfterY { a, b, .. } => { self = Done; Ready(*a + *b) }
            Done => panic!("poll after ready"),
        }
    }
}
```

每一个 `.await` 点 = 一个状态。`Poll::Pending` = "暂停在当前状态"，`Poll::Ready` = "切到下一个状态或终结"。这跟生成器的"每次 resume 推进一个 yield"是**同一种机械结构**，只是术语不同：

| Future 术语            | 生成器术语             |
|------------------------|------------------------|
| `Poll::Pending`        | Yielded (yield 一次)   |
| `Poll::Ready(v)`       | Complete (终结)        |
| `poll(cx)`             | `resume()`             |
| 每个 `.await` 点        | 每个 `yield` 点        |

**唯一剩下的事**是让生成器能"在中间吐值"。Future 的中间状态没法吐值——`poll` 的签名只允许最后吐一个 `Output`。解决：**给生成器一个外置槽位**。生成器内部想 yield 一个值时，往槽里写、然后返回 `Pending`；调用方 `resume` 看到 `Pending` 就去槽里取值。这就是 `Gen::yielded: Option<T>` 这个字段的来历。

## 五、`Gen<T>` 的实现骨架

把上面这套想法翻译成代码（简化版，省去 Rc/RefCell）：

```rust
pub struct Gen<Y, R, F: Future<Output = R>> {
    future: Pin<Box<F>>,        // 内部 future,每次 poll 推进一拍
    yielded: Option<Y>,         // yield 槽:future 写,resume 读
    done: bool,                 // 是否已 Complete
}

pub trait Generator {
    type Yield;
    type Return;
    fn resume(&mut self) -> Option<GenState<Self::Yield, Self::Return>>;
}

impl<Y, R, F: Future<Output = R>> Generator for Gen<Y, R, F> {
    type Yield = Y;
    type Return = R;

    fn resume(&mut self) -> Option<GenState<Y, R>> {
        if self.done { return None; }
        // 用一个 noop waker 构造 Context(resume 是同步的,wake 无意义)。
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match self.future.as_mut().poll(&mut cx) {
            Poll::Ready(r) => {
                self.done = true;
                Some(GenState::Complete(r))
            }
            Poll::Pending => {
                // future 暂停了 —— 看它有没有往槽里写值。
                let y = self.yielded.take().expect(
                    "future 返回 Pending 但没 yield,这是 future 的 bug"
                );
                Some(GenState::Yielded(y))
            }
        }
    }
}
```

关键点：**槽位必须在 future 写之前就准备好**。`Gen::new` 的签名是：

```rust
pub fn new<E>(future_factory: E) -> Self
where E: FnOnce(YieldSlot<Y>) -> F
```

调用方拿到 `YieldSlot<Y>` 句柄，**在构造 future 时**就把它"缝"进 future 内部。future 想 yield 时调 `slot.set(v)`，等价于 `yield v`。

为什么是 `factory` 而不是直接传 future？因为 future 需要在自己的字段里**持有** YieldSlot，这个句柄必须在 future 构造之前就存在。`Gen::new` 先建一个空槽，把句柄 clone 一份给 factory，factory 用这份句柄和它自己的状态字段（step、计数器等）一起构造 future。`Gen` 留另一份句柄，poll 之后用 `slot.take()` 把值取走。

## 六、为什么用 `Rc<RefCell<Option<Y>>>` 而不是裸指针

读 `coroutine.rs` 你会看到 `YieldSlot<Y>` 内部是 `Rc<RefCell<Option<Y>>>`——一份给 future，一份给 Gen。这是有意的安全选择：

- **裸指针 + unsafe** 也行（M9b 教学版的"自引用 struct + Pin"那一节就是这么做的）。但裸指针要求 Gen 自己**永远不能 move**，否则指针悬空。`Gen` 没 Pin，move 它（比如从一个 Vec 挪到另一个）就会破坏指针。
- **Rc<RefCell<...>>** 让"槽位的所有权"从 Gen 这块内存里逃出来——future 和 Gen 各持一份 `Rc`，指向同一块堆上的 `Option<Y>`。Gen 怎么 move 都行，Rc 内部指针不变。

代价：`Gen` 不是 `Send`——Rc/RefCell 不是线程安全的。教学库的取舍：单线程生成器够用，把"跨线程生成器"留给课后题（提示：换成 `Arc<Mutex<Option<Y>>>`）。

## 七、用 Gen 跑一遍：协程逐行读"大文件"

Maxwell《Async Rust》Ch5 用生成器"逐行读文件、不一次性载入内存"做例子。我们用 `Gen` 实现一个最小版：

```rust
struct LineReader {
    lines: Vec<String>,
    pos: usize,
    slot: YieldSlot<String>,
}

impl Future for LineReader {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.pos < self.lines.len() {
            let line = self.lines[self.pos].clone();
            self.pos += 1;
            self.slot.set(line);    // = yield line
            Poll::Pending
        } else {
            Poll::Ready(())          // = return;
        }
    }
}

let mut gen = Gen::<String, (), _>::new(|slot| LineReader {
    lines: vec!["first".into(), "second".into(), "third".into()],
    pos: 0,
    slot,
});

assert_eq!(gen.resume(), Some(GenState::Yielded("first".into())));
assert_eq!(gen.resume(), Some(GenState::Yielded("second".into())));
assert_eq!(gen.resume(), Some(GenState::Yielded("third".into())));
assert_eq!(gen.resume(), Some(GenState::Complete(())));
assert_eq!(gen.resume(), None);
```

生成器只持有 `pos`（一个 usize）和当前行的 String——不论文件多大，内存占用恒定。这就是协程的核心胜利：**让"流式处理"不需要把整批数据装进内存**。

## 八、Gen 还实现了 Iterator

`gen.next()` 等价于"resume 一次，如果是 Yielded 就返回 `Some(y)`，如果是 Complete 或 None 就返回 `None`"。所以你可以：

```rust
let collected: Vec<u32> = gen.collect();
```

`coroutine.rs` 给 Gen 实现了 `Iterator`，让这两套 API 无缝衔接。注意 `Iterator::Item = Y`——终结返回值 `R` 被 `Complete(_)` 这一支丢弃了（如果调用方关心 R，就用 `Generator::resume` 而不是 Iterator 接口）。

### Generator vs Iterator 的本质差别

你可能问：生成器能像 Iterator 一样用，那它和 Iterator 有什么差别？为什么 Rust 既有 `Iterator` 又要有 nightly 的 `Coroutine`？

差别在**谁驱动**。Iterator 的 `next()` 是**调用方驱动**的——调用方喊"下一个"，iterator 同步地算出下一个返回。Iterator 内部**没有"暂停在某个状态等外部事件"**的概念——它跑完一次 `next()` 就把控制权完全交还。

生成器不一样。生成器可以"暂停在某个状态**等外部输入**"——nightly 的 `Coroutine::resume(self: Pin<&mut Self>, arg: Input)` 接受一个参数，调用方每次 resume 可以传不同的值进去。这让生成器能做 Iterator 做不到的事：**双向通信**。一个例子：

```
// 假想 nightly 语法
let mut adder = gen { |mut acc: i32|
    loop {
        let x: i32 = yield acc;   // 吐出当前累加值,等调用方喂下一个数
        acc += x;
    }
};
adder.resume(0);    // → Yielded(0)  初始累加值
adder.resume(5);    // → Yielded(5)  加 5
adder.resume(3);    // → Yielded(8)  再加 3
```

每次 resume 喂进一个值，生成器内部 `yield` 表达式的"返回值"就是喂进来的那个数。这种"调用方 ↔ 生成器双向数据流"是 Iterator 的 `next()` 做不到的（next 不接受参数）。

我们这一章的 `Gen` 是单向的（只 yield 不收 resume 参数）——这对应 Python 的"简单生成器"。但即便如此，它已经足够展示"暂停 + 恢复"的机制。等 nightly 稳定后，你只要把 `resume(&mut self)` 改成 `resume(&mut self, arg: Input)`，就升级成双向协程了。

## 九、读者最容易踩的坑：忘了 yield 就返回 Pending

测试里有一条专门验证这个 bug：

```rust
#[test]
fn gen_panic_on_pending_without_yield() {
    struct BadSlot;
    impl Future for BadSlot {
        type Output = ();
        fn poll(...) -> Poll<()> { Poll::Pending }  // 不写槽!
    }
    let mut gen = Gen::<u32, (), _>::new(|_| BadSlot);
    // resume 应当 panic
}
```

为什么 panic 而不是无限循环？因为 noop waker 永远不会被叫醒——如果 future 返回 Pending 又没 yield，Gen 既没法推进（future 永远 Pending）也没法吐值给调用方（槽里空），唯一的"出路"是死循环。教学版选择**立刻 panic**把 bug 暴露出来，比悄悄卡死强。

生产版（nightly 的 `Coroutine`）在编译期就保证你"yield 或 return 二选一"，不会有这个运行时 panic。这是 DSL（领域专用语法）比库 API 强的地方。

## 十、协程小结

- 生成器 = Future + 外置 yield 槽。`Poll::Pending` = yield 一次，`Poll::Ready` = 终结。
- `async fn` 是编译器自动生成的生成器：每个 `.await` 点是一个状态。
- `Gen<Y, R, F>` 用 `YieldSlot<Y>` 让 future 内部能往槽里写值；`resume` poll 一次，从槽取值。
- 用 `Rc<RefCell<...>>` 让槽从借用检查里逃逸，Gen 自身可以 move（不是 Pin）。
- 协程的最大用处：流式处理大文件 / 大数据流，内存恒定。

---

# 第十六部分：async 设计模式 —— Race / Join / Then / Timeout

> 对应代码：`crates/forge-rt/src/combinators.rs`
> 对应测试：`crates/forge-rt/tests/m9b_combinators_race_join.rs`、`tests/m9b_combinators_then_timeout.rs`

## 一、敌人先行：lib.rs 的 spin 版 select 有什么问题

M9b 第九章你已经写过 `select`：

```rust
pub fn select<A, B>(mut a: A, mut b: B) -> SelectOutput<A, B> {
    loop {
        if let Poll::Ready(v) = Pin::new(&mut a).poll(&mut cx) { return Left(v, b); }
        if let Poll::Ready(v) = Pin::new(&mut b).poll(&mut cx) { return Right(v, a); }
        std::thread::yield_now();   // ← 这里!
    }
}
```

它**自旋**。两个 future 都 Pending 时，它不交出执行权——它在 `yield_now` 上空转，烧一个核到死。在单线程 `block_on` 里勉强能用（因为 `Delay` 的 reactor 是另一个线程），但在多线程 `Runtime` 上：**如果一个 worker 跑着这个 select，它没空 poll 别的任务**。1000 个 select 同时存在 = 1000 个 worker 同时自旋 = 100% CPU。

真正的解法是把 select 写成一个 **Future**：它的 `poll` 只 poll 一次两边就返回，把"再 poll 一次"的责任交给执行器。两个子 future 都 Pending 时，select 自己也 Pending，执行器就能去跑别的任务。等到某个子 future 的 reactor 唤醒它，select 的 waker 也被一起唤醒，重新入队，下一拍再 poll。

这一部分我们写四个这样的"组合子 Future"：Race、Join、Then、Timeout。它们都是普通 Future，能 spawn、能 await、能和 reactor 协作。

## 二、Race：谁先 Ready 谁赢

### 画面

两个选手在跑道上同时起跑。裁判（执行器）按一下秒表（= poll）看一眼，谁先撞线谁赢，另一个被喊停（drop 还是接着跑由调用方决定）。

### 状态

`Race<A, B>` 内部两个字段：`a: Option<A>`、`b: Option<B>`。用 `Option` 是因为某一边 Ready 时要把它**取走**（消费）返回给调用方。

### `poll` 的实现思路

```rust
impl<A: Future + Unpin, B: Future + Unpin> Future for Race<A, B> {
    type Output = RaceOutput<A, B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // poll A —— Ready 就 Left 赢。
        if let Some(mut a) = this.a.take() {
            if let Poll::Ready(va) = Pin::new(&mut a).poll(cx) {
                let b = this.b.take().unwrap();
                return Poll::Ready(RaceOutput::Left(va, b));
            }
            this.a = Some(a);  // 没 Ready 放回去
        }
        // poll B —— Ready 就 Right 赢。
        if let Some(mut b) = this.b.take() {
            if let Poll::Ready(vb) = Pin::new(&mut b).poll(cx) {
                let a = this.a.take().unwrap();
                return Poll::Ready(RaceOutput::Right(vb, a));
            }
            this.b = Some(b);
        }
        Poll::Pending
    }
}
```

**关键设计**：两边都 poll 时用的是**同一个** `cx.waker()`。这样无论哪一边的 reactor 调 `cx.waker().wake()`，Race 自己也会被唤醒。如果给两边传不同 waker，就要保证两个 waker 都 wake 才稳——更复杂、更慢。统一一个 waker 是 async Rust 的标准做法。

### 逐拍示例

- 拍 1：`poll A` → Pending（A 的 reactor 注册了 waker）；`poll B` → Pending。Race 整体返回 Pending。执行器去跑别的任务。
- 拍 N：A 的 reactor 检测到事件就绪，调 `waker.wake()`。Race 重新入队。`poll A` → Ready(va)。Race 返回 `Left(va, B)`。B 还在 `b` 字段里没被消费，作为 loser 还给调用方。

## 三、Join：两边都完成，攒结果

### 画面

两个工人在装配同一台机器的两个零件。先装好的人**不能走**——他要把零件放进桌上的格子（= 槽位），然后**等**另一个工人也装好。两个格子都满了，机器才能出货。

### 状态

```rust
pub struct Join<A: Future, B: Future> {
    a: Option<A>,
    b: Option<B>,
    a_out: Option<A::Output>,   // ← 攒 A 的结果
    b_out: Option<B::Output>,   // ← 攒 B 的结果
}
```

### 为什么必须"攒"

这是 Join 最容易踩的坑，也是这一节的核心。看下面这个错误版本（不攒结果，直接两拍内拿）：

```rust
// 错误版本
fn poll(...) -> Poll<(A::Output, B::Output)> {
    let va = poll_a();   // 假设它返回 Ready
    let vb = poll_b();   // B 还没好 → Pending
    // 现在 va 拿在手上了,但 B 还没好,怎么办?
    // 如果返回 Pending,下一拍再 poll_a —— 但 a 已经 Ready 过了!
    // 再 poll 一个已 Ready 的 future 是逻辑 bug(它的内部状态可能已经无效)。
}
```

**问题**：第一次拿到 va 之后，**不能丢**也**不能再 poll a**——必须**存起来**。下一拍只 poll b，等 b 也 Ready，把存着的 va 一起返回。这就是 `a_out: Option<A::Output>` 的全部意义。

### 逐拍手算：B 先 Ready、A 后 Ready

设定：A 是 `Delay(100ms)`，B 是 `Delay(50ms)`。B 应当先 Ready。下表**逐拍**画 Join 的状态：

| 拍 | poll A 结果      | poll B 结果      | a_out | b_out | 整体返回 | 解读                                            |
|----|------------------|------------------|-------|-------|----------|-------------------------------------------------|
| 1  | Pending          | Pending          | None  | None  | Pending  | 两边都没好,等                                    |
| 2  | Pending          | **Ready(vb)**    | None  | Some  | Pending  | B 好了,存进 b_out;A 还没好,整体仍 Pending        |
| 3  | **Ready(va)**    | (不再 poll B)    | Some  | Some  | Ready    | A 也好了,两槽满,返回 `(va, vb)`,Join 终结       |

**拍 2 的关键**：B 返回 `Ready(vb)` 后，我们**不再 poll B**——下一拍 b 字段是空的（取走、drop 了），只 poll a。如果错误地继续 poll B（已经 Ready 的 future），轻则 panic（"poll after ready"），重则 UB（future 的内部状态被消费了）。

测试 `join_returns_both_when_ready_in_opposite_order` 验证这个场景：

```rust
// 拍 1: 都没好。
assert!(poll_once(&mut j).is_pending());
// 让 B 先 ready。
ready_b.store(true, Ordering::Release);
// 拍 2: B 好了但 A 没,整体仍 Pending。
assert!(poll_once(&mut j).is_pending());
// 让 A 也 ready。
ready_a.store(true, Ordering::Release);
// 拍 3: 两边都 ready,整体 Ready(("a", "b"))。
match poll_once(&mut j) {
    Poll::Ready((va, vb)) => { assert_eq!(va, "a"); assert_eq!((vb, "b")); }
    _ => panic!(),
}
```

注意每拍之后都断言了"poll 计数器"：拍 2 之后 A 被 poll 了 2 次（第一次没好、第二次还没好），B 也被 poll 了 2 次（第一次没好、第二次好了被消费）。拍 3 A 被 poll 第 3 次（这次 ready），B **不再被 poll**——因为 b 字段已经空了。

### 第二手算：A 先 Ready、B 后 Ready（对称交错）

把场景反过来：A 是 `Delay(50ms)`，B 是 `Delay(100ms)`。这次 A 先 Ready。**机制完全对称**，但要确认 a_out 这一支也能正常工作——很多实现会"偏心"一边的 Option 槽。

| 拍 | poll A 结果      | poll B 结果      | a_out | b_out | 整体返回 |
|----|------------------|------------------|-------|-------|----------|
| 1  | Pending          | Pending          | None  | None  | Pending  |
| 2  | **Ready(va)**    | Pending          | Some  | None  | Pending  | ← A 先 ready,存进 a_out
| 3  | (不再 poll A)    | **Ready(vb)**    | Some  | Some  | Ready    | ← B 也 ready,两槽满,返回

注意拍 2 之后**两个槽都没满**——只有 a_out 有值。整体仍 Pending。如果某实现误把"a_out 有值"当成"完成了"，就会错误地提前返回 `(Some(va), None)`——这在编译期会被类型系统挡住（返回类型是 `(A::Output, B::Output)`，不是 `(Option, Option)`），但**运行期**很容易写出"忘了检查 b_out 也得 Some"的 bug。我们的实现用 `if this.a_out.is_some() && this.b_out.is_some()` 双重检查，两个都满才返回。

### 为什么用 `Option<Output>` 而不用 `OnceCell` 或直接值

读到这里的读者可能想：用 `OnceCell<A::Output>`（std 的写一次 cell）是不是更优雅？语义上确实更贴——OnceCell 表达的就是"先空、后填一次、再不空"。但教学库选择 `Option` 是因为：

1. **`Option` 是最基础概念**，读者从前几章就在用，不需要再引入 OnceCell。
2. **`Option::take` 语义清晰**：取出值、原位变 None。这正是 Join 终结时想要的——拿走结果、槽位重新变空（虽然 Join 终结后整个 future 会被 drop，槽位是否空已无关）。
3. **教学重点是"先 ready 的结果必须存起来"这个 idea**，不是 OnceCell 这种具体容器。一旦 idea 建立起来，用 Option / OnceCell / Mutex / 任何容器都行——它们都只是"保管中间结果"的容器。



### 为什么 a_out 和 b_out 是 Option 而不是直接值

因为 Join 的生命周期里有"一边好、一边没好"的中间态。这个中间态下，好的那一边的值**必须存在某个地方**——Option 是最自然的容器。`take()` 在最后返回时把它取出来打包成 `(va, vb)`。

## 四、Then：future 完成后接一个转换函数

### 画面

工厂流水线：第一段工序的产出**直接喂给**第二段工序。第一段没完，第二段不动；第一段完，第二段开跑。

### 状态机

两态：

```rust
enum ThenState<F, G, Fn> {
    First { first: F, chain: Option<Fn> },   // 在跑 first
    Second { second: G },                     // 在跑 second
    Done,
}
```

`chain` 是 `Option<Fn>` 因为 `Fn: FnOnce`——只能调一次。poll 时 `take()` 走它，用它把 `first` 的结果变成 `second` future。

### `poll` 的循环

```rust
fn poll(...) -> Poll<G::Output> {
    loop {
        match &mut self.inner {
            First { first, chain } => {
                let v = ready!(Pin::new(first).poll(cx));  // first 没 ready 就 return Pending
                let chain = chain.take().unwrap();
                self.inner = Second { second: chain(v) };
                // loop 继续,接着 poll second
            }
            Second { second } => {
                let v = ready!(Pin::new(second).poll(cx));
                self.inner = Done;
                return Ready(v);
            }
            Done => panic!(),
        }
    }
}
```

（实际代码没用 `ready!` 宏，是显式的 `match` + `return Poll::Pending`，因为这是教学库。）`loop` 让"first 刚好首拍就 Ready + second 也立即 Ready"能在一拍内完成。

### 用法

```rust
let f = then(Ready::new(3), |v: u32| Ready::new(v * 2));
// f.await == 6
```

链式拼接：

```rust
then(then(first, g), h)   // first → g → h
```

这就是 async Rust 里 `FutureExt::then` 的雏形。真实库还提供 `and_then`（Result 友好版）、`or_else`（错误恢复版）等变体。

## 五、Timeout：套一层 deadline

### 画面

运动员在跑步（inner future），旁边有计时员（Delay future）。计时员的闹钟响了（deadline 到）运动员还没跑完，比赛判超时——但运动员本人还在跑（inner future 还没 drop），是把他叫停（drop）还是让他跑完由裁判（调用方）决定。

### 状态

```rust
pub struct Timeout<F: Future> {
    inner: Option<F>,
    delay: Option<Delay>,
}
```

两个 Option：inner Ready 时把 delay drop（反注册 reactor 的 timer）；delay Ready（= 超时）时把 inner 还给调用方（调用方决定 drop 还是续跑）。

### `poll` 的顺序很关键

```rust
fn poll(...) -> Poll<TimeoutOutput<F>> {
    // 1) 先 poll inner。
    if let Some(mut inner) = this.inner.take() {
        match Pin::new(&mut inner).poll(cx) {
            Ready(v) => { this.delay = None; return Ready(Ok(v)); }
            Pending => { this.inner = Some(inner); }
        }
    }
    // 2) 再 poll delay。
    if let Some(mut delay) = this.delay.take() {
        match Pin::new(&mut delay).poll(cx) {
            Ready(()) => {
                let inner = this.inner.take().unwrap();
                return Ready(Elapsed(inner));
            }
            Pending => { this.delay = Some(delay); }
        }
    }
    Pending
}
```

**为什么先 poll inner**？因为如果 inner 在 deadline **之前**恰好就绪（比如 deadline 是 100ms，inner 在 99.9ms 就绪），我们希望判 Ok 而不是 Elapsed。先 poll inner：如果它 Ready，立刻返回 Ok 并 drop delay（取消计时）；如果它 Pending，再看 delay。

如果反过来先 poll delay，就有可能"delay 在 100ms 时 Ready、inner 其实也在 99.9ms Ready 了但没 poll 到"——误判超时。这种竞争在 reactor 的事件循环里很常见。

### 逐拍示例

- 拍 1：poll inner → Pending；poll delay → Pending。整体 Pending。
- 拍 N：inner 的 reactor 唤醒。poll inner → Ready(v)。drop delay，返回 Ok(v)。
- 或者：拍 M：delay 的 reactor 唤醒（100ms 到了）。poll inner → Pending；poll delay → Ready(())。返回 Elapsed(inner)。

测试 `timeout_returns_elapsed_when_deadline_hits_first` 验证后一种路径：inner 是 `Delay(150ms)`，timeout 设 50ms，结果必须是 `Elapsed`，且耗时在 50ms 附近（不是 150ms）。

## 六、关于"取消"和"背压"

这两个模式不是单独的 Future，是**使用模式**——但它们如此常见，必须讲。

### 取消 = drop future

Rust 的 async 取消机制非常优雅：**取消一个 task 就是 drop 它的 future**。future 的 Drop 会自动清理：

- reactor 注册项（TimerRegistration、TCP socket 注册）被反注册；
- 持有的锁（async Mutex guard）被释放；
- 关联的 channel sender/receiver 被 drop；
- 中间状态的计算结果被丢弃。

不需要 `cancel()` 方法、不需要协作标志、不需要中断信号——Rust 的所有权模型自动处理。代价：调用方必须**主动 drop**。`Race` 的 loser、`Timeout` 的 inner 都是 drop 给调用方的——调用方决定 drop（取消）还是继续 await。

### 逐拍看取消的清理

假设你有一个 task 正在 `Delay(10s).await`。它的 reactor 表里有一行 `{ deadline: T+10s, waker: task_waker }`。你 drop 这个 task 的 future：

1. **future 的 Drop 跑**：Delay 字段被 drop。
2. **Delay 的 Drop**：它持有的 `TimerRegistration` 被 drop。
3. **TimerRegistration 的 Drop**：调 `reactor.unregister(id)`，reactor 表里那行被删。
4. **task 的 Arc 引用计数减 1**：如果 reactor 不再持引用（它在 unregister 时已经清了 waker，waker 内部的 Arc<Task> 释放），task 的内存被回收。

整条链路**自动**完成。和线程取消（要 `pthread_cancel` + 清理 handler + 可能死锁）相比，async 取消在 worst case 也只是"多跑几次 drop"——它是 RAII 的延伸。

### 取消的边界情况

不是所有 future 都能"安全 drop"。一些边界：

- **执行到一半的 critical section**：如果你在 `MutexGuard` 持有时 await，await 点的 future 被 drop 时 guard 也 drop（释放锁）——这通常 OK。但如果 guard 持有时跨过了 await 点，drop 后别的 task 能拿到锁，逻辑上要保证被 drop 的 task 没有把共享数据留在不一致状态。
- **数据库事务**：事务 future 被 drop 时，DB 驱动通常自动 rollback。但如果你的业务逻辑依赖"事务一定提交"，drop 会破坏这个假设。
- **socket 写一半**：TCP 写 future 被 drop 时，缓冲区里没发完的数据可能丢。TCP 协议层不丢（OS 缓冲区继续发），但用户态缓冲区可能丢。

经验法则：**把 await 点放在"可以中断"的地方**。具体来说，避免在持有 invariant 时 await——先把数据 flush 到一致状态再 await。这是 async 编程的纪律，和同步编程里"持锁时不调用未知代码"同理。

### 背压 = 有界 channel

"背压"（backpressure）：消费者来不及处理时，让生产者**慢下来**而不是堆积任务。同步世界里用有界队列（`mpsc::sync_channel(cap)` 满了就阻塞 send）；异步世界里用**有界 async channel**：channel 满时 `send().await` 返回 Pending，让生产者的 task 让出执行权。这样消费速度自然调节生产速度。

无界 channel（如 `tokio::mpsc::unbounded_channel`）**没有背压**——生产者会一直往里塞，内存可能爆掉。生产代码里几乎总是该用有界版。

### 背压的逐拍机制

设生产者 P、消费者 C、有界 channel 容量 4。C 处理一个消息要 10ms，P 每 1ms 产一个。

- 拍 1-4：P send → 立即 Ready（channel 有空位）；C recv → 处理。
- 拍 5：P send → channel 满了 → send future 返回 Pending。
- 拍 5+：P 的 task 被挂起，让出执行权。其他 task 跑。C 处理完一个消息 → channel 空一格 → 唤醒 P 的 task。
- P 的 task 重新入队 → send → Ready。继续产。

这就是"消费速度调节生产速度"。无界版没有"channel 满了"这一拍，P 一直跑，内存里堆积几千个未处理消息——一个慢消费者拖垮整个系统。

forge-channel 那一章（M5）我们写的 mpsc 是有界的（默认 cap = 128），用户可以调。生产代码里 cap 的选择是个工程问题：太小（如 1）会让 P 和 C 强制串行；太大（如 100000）等于没背压。常见值在 16–256 之间，视业务平滑度而定。



## 七、组合子的组合：把这些拼起来

```rust
// 等"请求"或"超时"中先到的那个,然后用 then 处理结果。
race(
    fetch_request(),
    timeout_after_5s(),
).then(|outcome| match outcome {
    Left(req)      => handle(req),
    Right(_timeout) => fallback(),
});
```

`race` 出来的 future 再 `then` 一层，类型上完全 OK——所有组合子都是普通 Future，可以无限嵌套。这就是 async Rust 表达力强的根源：**异步控制流是一等公民**，能像数据一样传来传去、套娃套下去。

## 八、读者最难懂的 1 处

这一部分最容易卡住的地方是 **Join 的"必须存结果"**。直观上"poll 一下两边、谁好谁不好"听起来很简单，但"先好的那一边的值要存在哪里"这个问题被大多数人忽略。如果你不存（直接 drop），等另一边也好时你已经丢了；如果你再 poll 它（想再拿一次），future 状态机已经无效。

记住：**Future 是单次消费的**。一旦它返回过 `Ready`，它就**永远不能再被 poll**。Join 的 `Option<Output>` 槽就是为了在"一边先好、另一边没好"的中间态里**保管好这个一次性的值**，等到时机成熟一起返回。理解了这一点，Join、`join_all`、`try_join`、`FuturesUnordered` 都是一回事——把"已经 ready 的结果"攒起来，等没 ready 的那些慢慢到齐。

## 九、设计模式部分小结

- **Race**：两 future 谁先 Ready 谁赢；loser 还给调用方。用同一个 waker 让两边都能唤醒 Race 自己。
- **Join**：两 future 都要完成；先 ready 的结果存进 `Option` 槽，等另一个也 ready 才整体返回。**绝对不要** poll 一个已经 Ready 过的 future。
- **Then**：链式 future；状态机 First → Second；`FnOnce` 用 `Option` 包装以便 `take`。
- **Timeout**：先 poll inner、再 poll delay；inner 先 Ready 就 Ok 并取消计时，delay 先 Ready 就 Elapsed 并把 inner 还给调用方。
- **取消** = drop future；**背压** = 有界 channel。

---

## 附：模块对照表（含 M9b 补缺）

| forge-rt 文件 | 对应概念 | 教程章节 |
|---|---|---|
| `src/lib.rs` (`Delay`, `select`, `noop_waker`) | 最简 future / 竞争 / 手写 waker | 一、三、九 |
| `src/task.rs` | Task = Arc + state machine | 五 |
| `src/executor.rs` | block_on + 多线程 Runtime | 四、六、八 |
| `src/reactor.rs` | mio-based reactor | 七 |
| `src/coroutine.rs` | Gen/Generator/HandGen,async fn 脱糖 | **十五** |
| `src/combinators.rs` | Race/Join/Then/Timeout | **十六** |
| `tests/m9b_delay_block_on.rs` | block_on + Delay | 一、四 |
| `tests/m9b_runtime_spawn.rs` | 多线程 spawn | 五、六 |
| `tests/m9b_delay_concurrent.rs` | 并发 Delay 验证 | 七、十一 |
| `tests/m9b_select.rs` | select + 输家 drop | 九 |
| `tests/m9b_coroutine_basic.rs` | Gen 状态机 + HandGen 对照 | **十五** |
| `tests/m9b_coroutine_file.rs` | 流式 LineReader | **十五** |
| `tests/m9b_combinators_race_join.rs` | Race/Join poll 交错 | **十六** |
| `tests/m9b_combinators_then_timeout.rs` | Then/Timeout | **十六** |

---

（完）
