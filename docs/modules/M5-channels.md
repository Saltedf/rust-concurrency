# M5 — 自建 Channel：把错误从"运行时崩溃"逐步挪到"编译期"

> 模块：`forge-channel::{oneshot, mpsc}`　|　测试：`crates/forge-channel/tests/m5_*.rs`
> 跑：`cargo test -p forge-channel`　|　miri：`cargo +nightly miri test -p forge-channel --test m5_*`

---

## 第零拍 · ENEMY：让"在线程间塞一个值"这件事先变难

请你先停下，闭眼想一个画面：你坐在自己的房间里。隔壁房间坐着你的朋友。你们俩中间隔着一堵墙，墙上有一个**只容一张纸条通过**的窄缝。你想给他一个数：`42`。

你把"42"写在纸条上，塞进缝里。纸条落到他那一侧的地板上。

到这一步为止，一切都很容易。真正的麻烦从他**伸手去拿**那张纸条才开始：

1. 他怎么知道纸条已经**到了**？他不可能一刻不眨地盯着缝——人会累，CPU 也是。
2. 如果他**还没到**缝口，你已经又塞了第二张纸条进去，**第一张就被覆盖了**——他永远看不到 42，只看到你后来塞的那张。
3. 如果你**根本没塞**任何东西，他却伸手去读，他会从地上捡起**上一次某个人残留的废纸**——那是上一个程序留在那块内存里的垃圾。
4. 如果你**塞了两次**，他**也读了两次**——而纸条上画的不是数字、是一个不可复制的物件（例如一栋房子的钥匙），那这把钥匙就被**复制**了，但世界上只有一栋对应的房子，第二把钥匙是假的。

把这堵墙换成"两个线程共享的一块内存"，把纸条换成"一个 `T` 类型的值"，把"伸手去拿"换成"读这块内存"——你面对的就是这一整章要解决的问题。它有个名字，叫**通道（channel）**。

但请你**先别**记住"通道"这个词。先把上面那四个麻烦记住。这一章不是教你"通道是什么"，而是教你**怎样一步一步把那四个麻烦堵死**，并且在最后告诉你：堵这些洞所用的招数，**几乎全部能从"运行时"挪到"编译时"**——也就是让错误的代码**根本编译不过**，而不是"运行时崩一下"。

这是 Rust 这门语言最值钱的一种思维，Mara Bos 把它叫"通过类型保证安全"（safety through types）。这一章就是这句话最完整的展开。

**一个贯穿全章的提醒**：不要把"安全"和"便利"混为一谈。安全是"不会 UB"，便利是"用户写起来舒服"。`unsafe fn` 不安全但很便利（一句话就能调用）；一个被严格类型约束的接口可能很安全但用起来很别扭（要写生命周期）。这五个目标——安全、便利、灵活、简单、性能——经常**互相打架**。这一章每一步演化，都是在它们之间**重新分配**取舍。请你每读完一节都问自己："这一版的取舍是什么？换来的是什么？" 这是工程师视角的训练。

**还有一个贯穿全章的练习**：每读完一个版本，**自己合上书**在脑子里把代码默写一遍。写不出来的部分就是你还没真正理解的部分。读十遍不如默写一遍——这是 tsoding 风格的"用手指思考"。教程不是给你"读完"的，是给你"做出来"的。这一章的代码加起来不到 200 行，完全在你脑子里装得下。

> **类比贯穿声明**：从现在起到本章中段，我都用"窄缝 + 纸条"这堵墙作类比。等到第五拍我们引入"阻塞"时，墙会变成一道**有门铃的墙**——我会明确指出这次类比在哪儿断掉、为什么必须断。请你对每一次类比的断裂保持警觉，那通常是最值得理解的地方。

我们这一章要造的不是一种通道，而是**六种**——每一种都修前一种的洞。它们最终演化成两个可以生产用的成品：`oneshot`（只发一条、收一条）和 `mpsc`（多个发送者、一个接收者）。前者是 M9 异步运行时 `JoinHandle<T>` 的内核；后者是 M10 爬虫把 N 个 worker 的结果汇聚到一个 writer 的工具。**先把这两个名字放一边**，我们从最朴素的版本起步。

---

## 第一拍 · ANCHOR：最朴素的通道 = 一个被锁保护的队列

### 1.1 先承认一个事实：你已经会做一个通道了

回到 M2 我们做过的 `TaskQueue`：一个 `Mutex<VecDeque<Task>>` 加一个 `Condvar`。**那就是一个通道**——只不过传的不是"消息"而是"任务"。把 `Task` 换成任意类型 `T`，它就成了一个**通用的、能多生产多消费**的通道。

我们把它原样抄过来，给个新名字：

```rust
// crates/forge-channel/src/mpsc.rs（精简版）
use std::collections::VecDeque;
use std::sync::{Mutex, Condvar};

struct Shared<T> {
    queue: Mutex<VecDeque<T>>,   // 一个被锁保护的先进先出队列
    not_empty: Condvar,          // "队列非空"的通知铃
}

impl<T> Shared<T> {
    fn send(&self, message: T) {
        // 加锁 → 把消息塞到队尾 → 摇一下铃 → 放锁
        self.queue.lock().unwrap().push_back(message);
        self.not_empty.notify_one();
    }
    fn recv(&self) -> T {
        let mut guard = self.queue.lock().unwrap();
        loop {
            if let Some(m) = guard.pop_front() {   // 队列里有，就拿走
                return m;
            }
            // 没有就等铃响；wait 会在等待时放锁、被唤醒时重新加锁
            guard = self.not_empty.wait(guard).unwrap();
        }
    }
}
```

如果你还没在 M2 里碰过 `Condvar::wait`，请回头先读 M2 那一节。这一章不再重讲 `wait` 内部"放锁 → 睡 → 被唤醒 → 重加锁"的机制，但**会反复用它**。

这一版的优点很明显：**任意多个发送者、任意多个接收者、不用一句 `unsafe`、不用操心 `Send/Sync`**——编译器看见 `Mutex<T>` 和 `Condvar` 都是 `Sync`，就自动认为我们的 `Shared<T>` 也是 `Sync`，无须手写 `unsafe impl`。

它也很明显地**不够好**：

- 即便队列里已经躺着 100 条消息等着被取，**任何一次 send / recv 都得抢同一把锁**。一个线程在 push，所有人等它。
- 如果某次 `push` 触发了 `VecDeque` 扩容（要 realloc + 复制所有元素），**所有其他线程都得卡在等锁上**。
- 这个队列**没有上限**。生产者比消费者快，队列就一路涨——可能涨到 OOM。

第一、二两个问题我们要等到自建 `oneshot` 才有办法；第三个问题（无界）放到本章最后一节"有界 mpsc 与背压"专门处理。**先把注意力收窄到一个最特别的用例**——只发一条、收一条。这个用例值得为它专门重做。

### 1.2 为什么"只发一条"值得单独造一个通道

想象一下"任务结果回传"这个场景：你 spawn 一个线程去算一个数，算完它要把这个数送回主线程。**这一辈子只发一次、收一次**。用上面那个 `Mutex<VecDeque>` 来传？太奢侈了——你要为一整条队列的数据结构付费，却只用到其中一个槽位。

于是问题变成：**能不能不分配、不上锁、不用队列，就把一个值从 A 线程送到 B 线程？**

答案：能。但**前提是**你得绕开四个洞——也就是第零拍列出的那四个麻烦。这一拍我们开始打补丁。

---

## 第二拍 · LOW-FI：第一版 one-shot 通道，能用但到处是刀

### 2.1 只用一个槽位、一个标志位

one-shot 通道的最小数据结构只有两个字段：

```rust
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Channel<T> {
    message: UnsafeCell<MaybeUninit<T>>,   // 一个"可能还没初始化"的槽位
    ready: AtomicBool,                      // "槽位里有没有东西"
}
```

`UnsafeCell<MaybeUninit<T>>` 读起来很吓人，请你**别被名字吓住**。把它拆开看：

- `MaybeUninit<T>` 表示"一块大小恰好够放一个 `T` 的内存，至于这块内存里**当前是不是已经写过一个有效的 `T`**，我（编译器）不知道，由使用者负责跟踪"。它就是 `Option<T>` 去掉那个 `Some/None` 标签后的"裸"版本——标签没了，省了一个字节的空间，代价是**安全性全靠你自己**。
- `UnsafeCell<T>` 是 Rust 给编译器的"提示"：这块内存可能会被"看起来不可变"的引用 `&T` 在背后偷偷改。它是所有"内部可变性"（`Mutex`、`RefCell`、`AtomicXxx`）的底层原料。在这里我们用它，是因为同一个 `&Channel`（共享引用）下，发送方要写、接收方要读——这是 Rust 默认不允许的"通过共享引用改值"，必须用 `UnsafeCell` 才能合法表达。

**为什么不直接用 `Option<T>`？** 因为 `ready` 这个 `AtomicBool` 已经表达了"有没有消息"，再用 `Option` 的 `Some/None` 就重复占空间。Mara Bos 在原书里说得很直白：`MaybeUninit<T>` 是 `Option<T>` 的"裸奔版"——一切检查责任都在你。

接着我们告诉编译器：这个 `Channel<T>` 在 `T: Send` 时可以跨线程共享（`Sync`）：

```rust
// 安全性：T 要能"送到另一个线程"（T: Send）。
// 同一时刻只有一方碰 message（靠 ready 协调），所以不需要 T: Sync。
unsafe impl<T: Send> Sync for Channel<T> {}
```

记住这两条约束：
- **`T: Send`** 是因为我们真的要把 `T` 从发送线程"送到"接收线程——值的所有权发生了转移。
- **不需要 `T: Sync`**——因为发送方写完之后，**接收方是独占地**读它（不是两方同时看同一个 `T`）。`Sync` 关心的是"多个线程能不能**同时**读同一个 `T`"，这里没有这个需求。

### 2.2 send 和 receive 的雏形：全是 `unsafe`

```rust
impl<T> Channel<T> {
    pub const fn new() -> Self {
        Self {
            message: UnsafeCell::new(MaybeUninit::uninit()),
            ready: AtomicBool::new(false),
        }
    }

    /// 安全性：只能调用一次！
    pub unsafe fn send(&self, message: T) {
        // (*self.message.get()) 拿到 &mut MaybeUninit<T>，
        // .write(message) 把 message 写进槽位（覆盖原来的"垃圾"）。
        (*self.message.get()).write(message);
        // Release：把上面这次 write 发布给接收方的 Acquire 读。
        self.ready.store(true, Ordering::Release);
    }

    /// 安全性：只能调用一次，且只能在 is_ready() 返回 true 之后！
    pub unsafe fn receive(&self) -> T {
        // assume_init_read 假设槽位已被初始化，把它当 T 读出来（按位复制）。
        (*self.message.get()).assume_init_read()
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}
```

注意 `send` 里 `Release` 和 `receive` / `is_ready` 里 `Acquire` 的配对——这正是 M1 反复练过的"Release/Acquire 建立同步"。发送方写完 message 后 `store(true, Release)`，接收方 `load(Acquire)` 看到 true 的那一刻，**之前的 write 在它眼里已经完成**——这就是"消息发布"。如果你忘了为什么，请回到 M1 重读 Release/Acquire 那一节；这里完全是一个骨架的复用。

### 2.3 这个版本"能用"，但**到处是刀**

请你停一秒，自己列举一下：用户能以哪些方式把这套 `unsafe` 接口用炸？我列几个最致命的：

1. **`send` 两次。** 第二次 `write` 会和接收方正在 `assume_init_read` 形成**数据竞争**——UB（未定义行为）。即便没人同时收，两次 `write` 也可能覆盖一个还没被读走的消息，造成内存泄漏或更糟。
2. **`receive` 两次。** 第二次 `assume_init_read` 会把同一个非 `Copy` 的 `T`（比如 `Vec`、`String`、`Box`）按位复制一份——两个所有者，drop 两次，UB。
3. **不 `is_ready` 就 `receive`。** 读到的是上一轮程序残留的"垃圾"内存——UB。
4. **`Channel` drop 时，发了没收的消息不会被 drop。** `MaybeUninit` 不跟踪初始化状态，drop 它的时候不会 drop 它的内容。结果就是**内存泄漏**——虽然 Rust 里"泄漏"在 soundness 上不算 UB（这是 Rust 有名的"泄漏是 safe"立场的后果），但绝对不是好行为。

这些错误的共同特点：**全是运行时错误**，编译器一个都不会拦。你只要写错了，程序可能看起来跑得好好的，也可能在你最不期望的时刻崩成奇怪的样子。

"只要拿得稳，它没问题；但拿不稳的方式太多了。" 这就是 `unsafe` 接口的宿命。下面我们开始**逐步把刀拿掉**。

---

## 第三拍 · WRITE：把 UB 变成 panic，把 panic 变成编译错误

这一拍是全章最重要的部分。**它教你怎么把"运行时的炸药"逐步拆除成"编译期的红绿灯"**。每一步都要付一点代价，我们要把代价记清楚。

### 3.1 第二版：加运行时检查，把 UB 降级成 panic

UB 是"死刑"——可能什么都不发生，也可能格式化你的硬盘。panic 至少是"明确失败、立刻停止"。我们先想办法让前面那四个洞**不至于变成 UB**。

**洞 3（不 ready 就 receive）** 改起来最直接：在 `receive` 开头加一句检查：

```rust
pub unsafe fn receive(&self) -> T {
    if !self.ready.load(Ordering::Acquire) {
        panic!("no message available!");
    }
    (*self.message.get()).assume_init_read()
}
```

**洞 2（receive 两次）** 也很巧妙：把 `receive` 里的 `load` 换成 `swap(false, Acquire)`——读的同时**顺手把 ready 复位成 false**。于是第二次 `receive` 会读到 false、panic：

```rust
pub fn receive(&self) -> T {
    if !self.ready.swap(false, Ordering::Acquire) {
        panic!("no message available!");
    }
    unsafe { (*self.message.get()).assume_init_read() }
}
```

注意这一步的副作用：`receive` 函数**不再需要标 `unsafe`**——我们替用户承担了"不 ready 就读"的责任。`receive` 还能继续标 `unsafe` 吗？继续标也行，但既然现在调用它**在任何情况下都不会 UB**，把它降级成 safe 函数是诚实的。

**洞 4（发了没收的消息泄漏）**：给 `Channel` 实现 `Drop`。drop 时是 `&mut self`，独占，不需要原子：

```rust
impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        if *self.ready.get_mut() {                       // ready=true 说明有未读消息
            unsafe { self.message.get_mut().assume_init_drop() }
        }
    }
}
```

`AtomicBool::get_mut` 拿到 `&mut bool`，绕过原子操作（因为独占，不需要原子）；`UnsafeCell::get_mut` 同理。这种"`get_mut` 免原子"的模式以后会反复见到——只要你能证明"此刻独占"，原子就是多余的。

**洞 1（send 两次）** 最难。`ready` 只能告诉你"是否发完了"，不能告诉你"是否**正在**发"。要堵这个洞，得加第二个标志 `in_use`：

```rust
pub struct Channel<T> {
    message: UnsafeCell<MaybeUninit<T>>,
    in_use: AtomicBool,   // 新增！
    ready: AtomicBool,
}

pub fn send(&self, message: T) {
    if self.in_use.swap(true, Ordering::Relaxed) {
        panic!("can't send more than one message!");
    }
    unsafe { (*self.message.get()).write(message) };
    self.ready.store(true, Ordering::Release);
}
```

`in_use.swap(true, Relaxed)` 返回 true 就 panic。**为什么 `Relaxed` 就够？** 因为 `in_use` 唯一的作用是"在所有 send 之间排他"——只有 `swap` 返回 false 的那个 send 才会去碰 `message`，其他 send 都被 panic 拦在外面。`in_use` 的**全修改顺序**（M1 讲过）已经保证了"只有一个 swap 返回 false"，它不需要和别的变量建立任何同步关系，所以 `Relaxed` 即可。

顺便说一句：现在 `is_ready` 里的 `load` 也可以从 `Acquire` 降到 `Relaxed`——因为 `receive` 内部那个 `Acquire` 的 `swap` 已经承担了同步责任，`is_ready` 只起"指示"作用，准确性靠"全修改顺序保证它看到 true 之后 `receive` 也必看到 true"。

> **省 1 字节的变体**：用一个 `AtomicU8` 把通道状态编码成 `EMPTY / WRITING / READY / READING` 四态，每次状态转移用 `compare_exchange` 完成。功能完全等价，少一个字节的 `in_use`。教程这里不展开，思路和上面那两个 bool 完全一样——状态机嘛。

第二版到这里：**接口全部 safe**（用户怎么误用最多 panic），**不泄漏未读消息**。但还是有几个让人不舒服的地方：

- `is_ready` + `receive` 是**两个分开的调用**。理论上 `is_ready` 返回 true 之后、`receive` 之前，可能有别人 `receive` 把它取走——这就是 TOCTOU（time-of-check-to-time-of-use）的影子。`swap` 已经堵住了"两个 receiver"的情况，但**心理上的"两次调用"仍然存在**。
- panic 本身不是好体验。能不能让用户**根本写不出**"send 两次"的代码？

可以。下一步我们让编译器替我们挡。

### 3.2 第三版：类型级保证，把 panic 也挪走

**这一节是全章的核心招数**，请你慢一点读。

Rust 有个语言级的事实：**非 `Copy` 类型被 move 之后，原变量就没了**。你不能再用它，编译器会拦：

```rust
let s = String::from("hi");
let t = s;            // s 被 move
println!("{}", s);    // 编译错误：use of moved value `s`
```

利用这一点，我们可以**让"只能做一次"的操作消费 self**——把 `self` 吃掉。吃完之后那个变量就没了，没法再用，"两次"自然不可能：

```rust
pub fn channel<T>() -> (Sender<T>, Receiver<T>) { /* ... */ }

pub struct Sender<T> { /* ... */ }
pub struct Receiver<T> { /* ... */ }

impl<T> Sender<T> {
    pub fn send(self, message: T) { /* ... */ }     // 注意：self by value
}
impl<T> Receiver<T> {
    pub fn receive(self) -> T { /* ... */ }
}
```

`sender.send(...)` 一次之后，`sender` 就被 move 走了。再来一次 `sender.send(...)`？编译器直接报：

```
error[E0382]: use of moved value: `sender`
```

**没编译通过**。不是"运行时 panic"，是"根本编译不过"。这两个的差距极大——前者可能要等到产品上线才发现，后者你按下编译键的那一刻就发现了。

但这个改动有一个**结构性代价**：原来只有一个 `Channel` 类型，现在变成两个类型 `Sender` / `Receiver`，**它们共享同一块内存**（同一个 `message` 和 `ready`）。怎么办？用 `Arc`（M4 讲过）：

```rust
struct Channel<T> {                // 不再 pub：实现细节
    message: UnsafeCell<MaybeUninit<T>>,
    ready: AtomicBool,
}

pub struct Sender<T>   { channel: Arc<Channel<T>> }
pub struct Receiver<T> { channel: Arc<Channel<T>> }

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let a = Arc::new(Channel {
        message: UnsafeCell::new(MaybeUninit::uninit()),
        ready: AtomicBool::new(false),
    });
    (Sender { channel: a.clone() }, Receiver { channel: a })
}
```

Arc 让两个类型"共享同一份堆分配"，引用计数归零时自动 drop。**注意一个副作用**：现在 `in_use` 标志**不需要了**——"只发一次"已经被类型系统静态保证，运行时检查可以删。`send` 再也不会 panic。

> **这就是"用类型换运行时检查"的范例**：把不变量编码进类型，编译器替你守，省掉一次原子 swap、一次 panic 路径。代价是**一次 `Arc` 堆分配**。这是 Rust 里最常见的取舍之一：把"动态检查"换成"静态保证"，几乎总要付一点运行时代价（这里是分配），但换来的是更安全、更快（少了检查）的代码。

### 3.3 类型级技巧的拆解：`self by value`、`PhantomData`、`!Send`

第三版的精髓集中在一招：**让"只能做一次"的操作消费 self**。请允许我多花几段把这一招讲透，因为它会在你以后写的所有 Rust 代码里反复出现。

**`fn send(self, ...)` 和 `fn send(&self, ...)` 的差别是什么？** 字面上一个 `&`。语义上天差地别：

- `&self` 借用——调用之后 `sender` 还在，可以再用。如果你想表达"只能调一次"，`&self` **做不到**——编译器只会检查借用是否冲突，不会检查"调了几次"。
- `self` 消费——调用之后 `sender` 被 move 走了，**变量不存在了**。再调一次就是"use of moved value"，编译器拦。

这一招的本质是：**把"次数"这个**运行时**的概念，翻译成"所有权"这个**编译时**的概念。** 一次 = 一个所有权 = 一个 `self`。两次 = 两个所有权 = 编译错误。

`Copy` 类型是这条路的"漏洞"——`Copy` 类型被"消费"后还能继续用（因为它被复制了，没被搬走）。所以 `Sender` 绝不能 derive `Copy`。这是为什么我们没给 `Sender` 写 `impl Copy`，也没 derive——结构体里包含 `Arc`（不 `Copy`），自动就 `!Copy` 了。**如果你不小心给 Sender 加了 `#[derive(Copy, Clone)]`，整个"只能 send 一次"的保证就崩了**——编译能过，但 send 两次不再报错。这是 `Copy` 这条规则的一个微妙陷阱：它会让"消费 self"这招失效。

**`PhantomData<*const ()>` 令 `Receiver: !Send`** 是第二招。原理：

- Rust 的 `Send` 是个 unsafe trait，但绝大多数类型靠"自动推导"获得它——结构体的所有字段都 `Send`，结构体就 `Send`；
- `*const T`（裸指针）**不实现 `Send`**（哪怕 `T: Send`）——这是 Rust 的保守选择，因为裸指针没有生命周期/借用检查，跨线程乱用容易出 UB；
- `PhantomData<X>` 在编译期被当成 `X` 来检查 trait 实现（虽然它运行时不占空间）；
- 所以 `PhantomData<*const ()>` 让 `Receiver` "看起来"包了一个裸指针，于是它也不 `Send`。

**等价的写法还有**：`PhantomData<Rc<()>>`（`Rc` 不 `Send`）、`PhantomData<Cell<()>>`（`Cell` 不 `Sync`，但 `Send`——这个不行，方向不对）。`PhantomData<*const ()>` 是社区约定的"令类型 `!Send`"的标准写法。它的好处是**只挡 Send**，不挡其他 trait。

这一招不只是 oneshot 在用——`std::thread::LocalKey` 的内部、很多"线程亲和"的类型（比如绑定到某个事件循环的 future）都用类似的技巧把"必须在某线程用"编进类型。**记住模式：要让一个类型 `!Send`，给它加一个 `PhantomData<*const ()>` 字段。**

### 3.4 第四版：用借用省掉那次 Arc 分配

Arc 方便，但分配本身有成本——一次 `malloc` 加上初始化、引用计数。如果我们愿意让用户自己**把 `Channel` 放在局部变量里**（而不是让 `channel()` 函数替我们分配），就可以让 `Sender` / `Receiver` **借用**它，零分配：

```rust
pub struct Channel<T> {
    message: UnsafeCell<MaybeUninit<T>>,
    ready: AtomicBool,
}

pub struct Sender<'a, T>   { channel: &'a Channel<T> }
pub struct Receiver<'a, T> { channel: &'a Channel<T> }

impl<T> Channel<T> {
    pub fn split(&mut self) -> (Sender<'_, T>, Receiver<'_, T>) {
        *self = Self::new();   // 重置：drop 上次未读的消息、清状态
        (
            Sender   { channel: self },
            Receiver { channel: self },
        )
    }
}
```

`split` 的签名很值得品味。它要求 `&mut self`（独占借用），却返回两个 `Sender / Receiver`，里面装的是 `&Channel`（共享借用）。这是借用检查器允许的——把**一个独占借用降级成多个共享借用**是合法的，只要这些共享借用还活着，原来的 `&mut self` 就不能再用。

含义是：**只要 `Sender` 或 `Receiver` 还在，`Channel` 就被借住了**——你不能再 `split` 第二次，也不能动它。**两者都消失之后**，借用结束，你可以再次 `split`。借用检查器替我们保证了"同一时刻只有一对 Sender/Receiver"。

`*self = Self::new()` 这一句也不能小看。它的作用是**重置**：如果上一次 `split` 之后留下了"发了没收"的消息，赋值会先 drop 旧的 `self`（触发我们写的 `Drop`），再换上一个干净的 `Channel`。否则第二次 `split` 之后 `ready` 可能还是 true，**破坏了不变量**。这是一种常见的"重新进入不变量"模式——每次重新暴露接口前，先确认状态干净。

到这里我们已经把"send/receive 各只一次"压进了类型系统。还剩最后一个洞：**接收方应该能阻塞**。前面四个版本测试时，我们都在外面**手写** `thread::park` 循环——很丑。下一拍把它内置进通道。

---

## 第四拍 · ISO·ZOOM：阻塞、Thread 句柄、丢失唤醒

### 4.1 把类比换一下：从"窄缝"到"有门铃的墙"

窄缝的比喻到此为止失效了。窄缝模型里，发送方塞完纸条就走了，接收方什么时候来取他不知道——这恰恰是我们要解决的问题。新的比喻是：**墙上装一个门铃**。发送方塞完纸条，**按一下门铃**；接收方听不见铃响就不去取。这个门铃在 Rust 里的名字，叫 `thread::park` / `unpark`。

请记住：**类比的换档本身就是一个信号**。窄缝失效，是因为我们需要表达"通知"这件事；而通知必须有"接收方先准备好接收通知"这层语义。窄缝太被动了，门铃才有。

### 4.2 入门陷阱：丢失唤醒（lost wakeup）

这一节是本章**两个手算例子中的第一个**，请你逐拍跟着走。

设想我们要让 one-shot 通道的接收方在没消息时**睡过去**，等发送方来叫醒它。最朴素的写法是这样：

```rust
// 接收方 T2
while !ready.load(Ordering::Relaxed) {
    thread::park();    // 没准备好就睡
}
// 发送方 T1
message.write(data);
ready.store(true, Ordering::Release);
thread::current_t2().unpark();   // 假设 T1 知道要叫醒 T2
```

看起来没毛病。但**有竞态**。我们来逐拍画一个真实的交错。假设一开始 `ready=false`，"许可 token"=0（park/unpark 用一个 token 计数，每次 unpark 给一个，最多累计 1 个，park 时如果有 token 就消费一个并立刻返回，否则睡）：

| 拍 | T1（发送方） | T2（接收方） | ready | unpark token | T2 状态 |
|----|--------------|--------------|-------|--------------|---------|
| t1 |              | 进入 while，`load(Relaxed)` 看到 false | false | 0 | 运行 |
| t2 |              | 决定要 park，**还没真的调用 park()**（CPU 正在执行 `call`） | false | 0 | 运行 |
| t3 | `message.write(data)` | （还在那个 call 里） | false | 0 | 运行 |
| t4 | `ready.store(true, Release)` | （还在那个 call 里） | true  | 0 | 运行 |
| t5 | `unpark(T2)`：因为 T2 此刻**没在 park**，token 累加到 1 | （终于进入 park 内部） | true | 1 | 运行 |
| t6 |              | park 检查 token：发现有 1 个 → 消费 → **立刻返回** | true | 0 | 运行 |
| t7 |              | 回到 while 顶，`load(Relaxed)` 看到 true → 退出循环 | true | 0 | 运行 |

哎，这条交错**恰好走对了**——T2 没有睡死。是因为 unpark 提前给的 token 被 T2 后来调用 park 时**捡了起来**。这是 `park/unpark` 设计上特意保留的"许可"语义：unpark 提前调用不丢，token 留着。

**那"丢失唤醒"哪儿来的？** 麻烦出在另一条交错上。再画一次，这次 T2 在 t2 那一拍**真的进了 park**（系统调用已生效）。

> **停一下，做一个对比**：你或许听说过"自旋锁"——它不睡，一直在那转圈 `while !locked {}`。它没有"丢失唤醒"的问题，因为它根本没"睡"。但代价是**烧 CPU**：内核仍然给它分配时间片，它就把时间片全用来空转。`park/unpark` 的设计目的就是"该睡的时候睡、该醒的时候醒"——一旦睡，就要解决"如何不漏掉叫醒"。这个 token 机制就是它的回答。

| 拍 | T1（发送方） | T2（接收方） | ready | token | T2 状态 |
|----|--------------|--------------|-------|-------|---------|
| t1 |              | load(Relaxed) 看到 false | false | 0 | 运行 |
| t2 |              | **真的调用 park()**：检查 token=0，没有 → 让出 CPU，进入睡眠 | false | 0 | **睡眠** |
| t3 | `message.write(data)` | （睡） | false | 0 | 睡眠 |
| t4 | `ready.store(true, Release)` | （睡） | true | 0 | 睡眠 |
| t5 | `unpark(T2)`：T2 此刻在 park → 唤醒它 | （被唤醒） | true | 0 | 运行 |
| t6 |              | 回到 while 顶，load 看到 true → 退出 | true | 0 | 运行 |

这条也对了。**那 bug 在哪儿？** Bug 在第三种交错上——T2 在 t2 进 park 之前**还没真的进**，但**已经过了最后一次 `load` 检查**：

| 拍 | T1（发送方） | T2（接收方） | ready | token | T2 状态 |
|----|--------------|--------------|-------|-------|---------|
| t1 |              | load(Relaxed) 看到 false | false | 0 | 运行 |
| t2 |              | 决定 park，**正要调用**（指令还没发出） | false | 0 | 运行 |
| t3 | `ready.store(true, Release)` | （同上） | true | 0 | 运行 |
| t4 | `unpark(T2)`：T2 此刻**不在 park**，token 累到 1 | （同上） | true | 1 | 运行 |
| t5 |              | **真的调用 park()**：检查 token=1 → 消费 → 立刻返回 | true | 0 | 运行 |
| t6 |              | while 顶 load 看到 true → 退出 | true | 0 | 运行 |

哎，又对了——因为 token 提前给。**真正的"丢失唤醒"需要 unpark **早于** T2 上次"决定 park"那一刻——这才会出现"许可给错了人/被消费掉了"的情况。在 one-shot + `Relaxed` 读 + 单接收线程的设定下，**`park`/`unpark` 的 token 机制已经堵住了大部分情况**。但请你**别松口气**——下面这条**才是真正的死锁**：

让接收方循环里**两次** `load`（这是常见的写法，比如想避免无谓 park）。或更隐蔽地，T2 在 park 之后**先消费了 token 才回到 while 顶**——但**别的** `unpark`（不是我们的 send）叫醒了 T2：

| 拍 | T1 | T2 | ready | token | T2 状态 |
|----|----|----|-------|-------|---------|
| t1 |    | load 看到 false，决定 park | false | 0 | 运行 |
| t2 |    | 真的 park：检查 token=0，睡着 | false | 0 | 睡眠 |
| t3 | （还在写） | **第三方**线程 X 调用 `T2.unpark()`：token 累到 1 | false | 1 | 睡眠→运行 |
| t4 |    | 回到 while 顶，load 看到 false（消息还没来）→ 再次决定 park | false | 0 | 运行（**token 被消费了**）|
| t5 |    | 真的 park：检查 token=0，睡着 | false | 0 | 睡眠 |
| t6 | `ready.store(true, Release)` | （睡） | true | 0 | 睡眠 |
| t7 | `unpark(T2)`：T2 在 park，应该唤醒它……但 **token 已经被 T2 在 t4 消费过了**，而 t3 的那次 unpark 已经"用过" | （睡） | true | 0 | **睡眠（永远）** |

T2 死锁。`unpark` 不能累加超过 1 个 token，所以 t3 给的那个 token 已经在 t4 被消费了，t7 的 unpark 是**新的** token——**应该**能唤醒 T2。等一下，是真的能唤醒吗？能。`unpark` 每次调用：如果目标在 park，唤醒它（设 token=1 后消费）；如果不在 park，token 累加（上限 1）。t7 时 T2 在 park，t7 的 unpark 会**直接唤醒** T2。

那"丢失唤醒"到底是什么？**它发生在 T2 还没进 park、但已经过了最后一次 `load` 检查**的窗口——而且**`unpark` 的提前调用并没有把 token 留下来**，因为标准库的 `park` 默认会消费 token 后**继续睡**（这是 [`park_timeout` 文档](https://doc.rust-lang.org/std/thread/fn.park.html)里写得明明白白的"spurious wakeups"和 token 语义的微妙之处）。简而言之：

- 如果接收方的循环里 `load` 和 `park` 是**两条独立的语句**，T2 可能在 `load` 看到 false 之后、`park` 之前，被调度走；
- 此期间 T1 写 ready=true 并 `unpark(T2)`；
- T2 被重新调度，**继续**执行 park——`park` 看到 token=1，消费后**立刻返回**，T2 退出循环。**这次没问题。**

**那真的会丢失吗？** 标准库的 `park`/`unpark` 设计就是为了**不丢**——只要你用对。**真正的丢失唤醒发生在更复杂的代码里**：

- 接收方不**总是** park（比如它先做点别的）；
- 或者接收方在 park 之前**再次 load** 一次（这次 load 在 unpark **之后**，看到 true 就跳过 park——但这本来是对的）；
- 或者**用了 `Condvar`**，但 `notify` 在 `wait` 之前发——`Condvar` 的通知**会丢**（不像 unpark 有 token 缓存）。

**这就解释了**为什么 M2 的 `TaskQueue` 必须在 `Condvar::wait` 里**重新检查条件**、并且**用 `while` 而不是 `if`**——`Condvar` 没有 token 机制，通知是一次性的，错过就没了。**这是同一副骨架**：M2 的"`while` + `Condvar::wait`"和本节的"`while` + `park`"，结构完全同构，都是"先检查后睡，醒来再检查"——这就是**所有阻塞接收的通用模式**，请把它刻进脑子里。

> **同构声明**：本节的 `while !ready.swap(false, Acquire) { park() }` 和 M2 的 `while task_queue.is_empty() { condvar.wait(guard) }`，**是同一种结构**——循环检查"条件是否满足"，不满足就睡，醒来再检查。差别只在底层原语：`Condvar::wait` 配 `Mutex`（睡的同时放锁），`park` 不配锁（直接睡）。本章后面写的 `oneshot` 用 `park`，`mpsc` 用 `Condvar`——按"需不需要锁保护队列"来选。

### 4.3 把 park/unpark 内置进 one-shot 通道

要"按门铃"，发送方得知道**接收方在哪条线程上**。我们在 `Sender` 里加一个字段：

```rust
use std::thread::Thread;

pub struct Sender<'a, T> {
    channel: &'a Channel<T>,
    receiving_thread: Thread,    // 接收线程的句柄
}
```

`Thread` 是 `std::thread` 提供的"线程句柄"，可以拿去 `unpark`。`thread::current()` 返回当前线程的句柄——`split` 的时候我们存的就是它：

```rust
pub fn split(&mut self) -> (Sender<'_, T>, Receiver<'_, T>) {
    *self = Self::new();
    (
        Sender {
            channel: self,
            receiving_thread: std::thread::current(),
        },
        Receiver {
            channel: self,
            _no_send: PhantomData,
        },
    )
}
```

`receiving_thread: thread::current()`——**调用 `split` 的那条线程**就是接收线程。这一假设必须成立，否则 `Sender` 里的句柄指向了错的线程，叫不醒真正的接收者。

**怎么强制 Receiver 留在调用 `split` 的线程上？** 用 `PhantomData<*const ()>` 把它标成 `!Send`：

```rust
use std::marker::PhantomData;

pub struct Receiver<'a, T> {
    channel: &'a Channel<T>,
    _no_send: PhantomData<*const ()>,    // → Receiver: !Send
}
```

为什么这能让 `Receiver` 不能跨线程？因为裸指针 `*const ()` **不实现 `Send`**——`PhantomData` 在编译期被当成它包的类型来检查 trait 实现，于是 `Receiver` 自动也变成 `!Send`。**这个错误从"运行时唤醒错线程"挪到了"编译期无法 spawn"**：

```rust
let (sender, receiver) = channel.split();
std::thread::spawn(move || {
    receiver.receive();   // 编译错误：`*const ()` 不能跨线程 move
});
```

这是类型系统的另一个胜利：把"线程亲和性"（必须留在某条线程上）编进类型，让错误在编译期暴露。

最后，`send` 和 `receive` 的成品：

```rust
impl<T> Sender<'_, T> {
    pub fn send(self, message: T) {
        // 安全：此刻只有我们碰 message（Receiver 要等 ready=true 才读）。
        unsafe { (*self.channel.message.get()).write(message) };
        // Release：把 write 的内容发布给接收方的 Acquire。
        self.channel.ready.store(true, Ordering::Release);
        // 按门铃。如果接收方还没 park，token 留着；如果 park 了，叫醒它。
        self.receiving_thread.unpark();
    }
}

impl<T> Receiver<'_, T> {
    pub fn is_ready(&self) -> bool {
        self.channel.ready.load(Ordering::Relaxed)
    }

    pub fn receive(self) -> T {
        // 必须循环：park 可能有假唤醒，醒来要重新检查。
        while !self.channel.ready.swap(false, Ordering::Acquire) {
            std::thread::park();
        }
        // 安全：ready 此刻是 true（我们刚把它换回 false），message 已初始化。
        unsafe { (*self.channel.message.get()).assume_init_read() }
    }
}
```

`receive` 里的 `swap(false, Acquire)` 一石二鸟：既建立了同步（Acquire 配 send 的 Release），又把 ready 复位（防下次/防 receive 两次）。

**这就是 `crates/forge-channel/src/oneshot.rs` 的当前形态。** 我们已经走完了原书第 5 章的五个版本。还剩最后一个版本——排序——可以放到动手清单里自己练（用一个 `AtomicU8` 编码四态）。下一节我们转到另一种通道：**多生产者**。

### 4.4 一个值得品味的细节：`swap` vs `load` 的取舍

回头看 `receive` 里那句 `self.channel.ready.swap(false, Ordering::Acquire)`。换成 `load(Acquire)` 行不行？语义上"读 ready"是一样的。不行——少了"把 ready 复位成 false"这件事。

为什么复位这件事如此重要？因为 `Channel::drop` 靠 `ready` 判断"槽位里有没有未读的消息需要 drop"。如果 `receive` 不复位，那么消息被取走之后 `ready` 还是 true，drop 时会再去 drop 一次已经空了的槽位——UB。`swap` 一次完成"读 + 复位"，让 `ready` 始终准确地反映"槽位里此刻是否有未读消息"。**`swap` 是一石二鸟：既是 Acquire 同步、又是状态推进。**

请你养成一个习惯：每写一句原子操作，问自己——它**改变了什么状态**？副作用是什么？这些副作用是不是我想要的？把原子操作当函数调用看，不要当成"读一下"或"写一下"那么简单。M1 里我们练过：`fetch_add` 返回旧值、`compare_exchange` 同时是"读+条件写"。它们的副作用往往就是它们存在的全部理由。

### 4.5 drop 顺序与 Arc 引用计数：补一个细节

第三版用 `Arc` 共享 `Channel` 时，drop 的顺序很微妙：

- `Sender::drop` 把 `Arc` 的引用计数从 2 减到 1；
- `Receiver::drop` 把 `Arc` 的引用计数从 1 减到 0——**这是真正触发 `Channel::drop` 的那一刻**。

所以 `Channel::drop` 一定发生在**最后一个**（Sender 或 Receiver）drop 的时候。我们写的 `Drop for Channel` 假定"此刻独占"——这是对的，因为引用计数归零意味着没有任何 `Sender` 或 `Receiver` 还能访问它。`*self.ready.get_mut()` 那个 `&mut self` 是 borrow checker 给我们的"独占证明"。

第四版（借用版）的 drop 更直接：`Channel` 是用户局部变量，作用域结束就 drop，没有引用计数。两种实现最终都靠"`&mut self` 证明独占"来跳过原子操作——**这是"独占免原子"的通用模式**。请记住它，M7 自建锁时会再用。

---

## 第五拍 · ISO·ZOOM（续）：mpsc，从一个发送者到 N 个发送者

### 5.1 为什么 oneshot 不够

`oneshot` 把"只发一条"做到了极致：无锁、无分配、阻塞、编译期安全。但很多场景需要**多条**消息——比如爬虫里 N 个 worker 各自不停地往主线程发"爬完了 X 条 URL"的统计。这种场景下我们不能给每个 worker 发完一条就 split 一次。

`mpsc`（multiple producer, single consumer）就是为这种场景设计的：**多个发送者、一个接收者**，任意多的消息。结构上回到第一拍的 `Mutex<VecDeque> + Condvar`：

```rust
// crates/forge-channel/src/mpsc.rs
struct Shared<T> {
    queue: Mutex<VecDeque<T>>,
    not_empty: Condvar,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        not_empty: Condvar::new(),
    });
    (Sender { shared: shared.clone() }, Receiver { shared })
}

pub struct Sender<T>   { shared: Arc<Shared<T>> }
pub struct Receiver<T> { shared: Arc<Shared<T>> }
```

`Sender` 可克隆——`Arc` 让多个发送者共享同一份 `Shared`：

```rust
impl<T> Sender<T> {
    pub fn send(&self, message: T) {
        self.shared.queue.lock().unwrap().push_back(message);
        self.shared.not_empty.notify_one();
    }
}

impl<T> Receiver<T> {
    pub fn recv(&self) -> T {
        let mut guard = self.shared.queue.lock().unwrap();
        loop {
            if let Some(m) = guard.pop_front() { return m; }
            guard = self.shared.not_empty.wait(guard).unwrap();
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self { Sender { shared: self.shared.clone() } }
}
```

注意 `recv` 的循环——和 oneshot 的 `receive` 是**同一副骨架**：检查条件（队列非空），不满足就睡，醒来再检查。差别只是把 `park/unpark` 换成了 `Condvar::wait`——因为队列要靠 `Mutex` 保护，而 `Condvar::wait` 在睡的同时能放锁、醒来时重加锁，正好适合这种"被锁保护的状态"。

**为什么 mpsc 选 Condvar 而不是 park？** 因为 mpsc 的状态（`VecDeque` 长度、队列内容）需要锁保护——多个生产者并发 push、消费者并发 pop，没有锁就会数据竞争。既然已经有锁了，"睡+放锁+醒来+重加锁"是 `Condvar::wait` 的本职工作。oneshot 没有锁——只有一个 `AtomicBool` 和一个 `MaybeUninit`，靠 `Release/Acquire` 协调——没有锁可以放，所以用更轻量的 `park/unpark`。**选哪个原语，看你的状态用不用锁保护**。这是 Rust 并发工具箱的一条经验法则：锁保护的睡 `Condvar`，无锁的睡 `park`。后面 M6 我们会看到第三种"`futex`"——它把"原子变量+睡"绑在一起，是更底层的东西。

测试 `m5_04_mpsc_multi_producer` 验证了 4 个生产者各发 1000 条、最后能收到 4000 条。测试 `m5_05_mpsc_blocking` 验证了 recv 在没消息时阻塞、消息到达后被唤醒。

**一个值得品味的细节**：`m5_04` 在 `thread::scope` 结束后 `drop(tx)`——主线程保留的那个 Sender 必须 drop，否则引用计数永远不到 0。`thread::scope` 保证 spawned 线程都 join 完才返回，但**它不会帮你 drop 主线程的 Sender**——你要自己显式 drop。这是 mpsc 一个新手常踩的坑：忘了 drop 一个 Sender，导致 `recv` 永远阻塞（因为还"有生产者活着"）。在标准库 `std::sync::mpsc` 里，这种忘记 drop 会让"通道关闭检测"失效；在我们的实现里，因为没有关闭语义，这只是意味着 Receiver 永远在等。

### 5.2 手算例子 #2：有界 mpsc 的背压时序

`mpsc` 的当前实现是**无界**的——队列想涨多大就涨多大。这听起来很方便，实际上很危险。考虑一个爬虫：

- 主线程源源不断地把 URL 喂给 10 个 worker；
- 每个 worker 抓完一个 URL，把抓到的页面塞给一个 writer 线程；
- writer 线程**写磁盘很慢**（10 ms 一条），worker 抓页面很快（1 ms 一条）。

无界通道下会发生什么？**队列爆炸**。10 个 worker 每毫秒各塞一条，writer 每 10 ms 才消费一条——队列每毫秒涨 10 条、消费 0.1 条，净涨 9.9 条。一千万条 URL 进来，队列堆到几千万条，**内存爆掉**。

**有界通道**给队列加一个上限 `N`。满了之后，**生产者 send 会被阻塞**——直到消费者取走一条、腾出位置。这叫**背压**（backpressure）：上游快、下游慢，背压把"慢"沿流水线**反传**给上游，强迫它也变慢。

下面逐拍画一个**容量 2、3 个生产者 P1/P2/P3、1 个消费者 C** 的有界通道。设两个数据结构：

- `queue`：长度最多 2；
- `waiters`：一个等待队列，记录"想 send 但队列满了"的生产者。

从空开始：

| 拍 | 事件 | queue 长度 | queue 内容 | P1 状态 | P2 状态 | P3 状态 | C 状态 | waiters |
|----|------|-----------|-----------|---------|---------|---------|--------|---------|
| t0 | 初始 | 0 | [] | 就绪 | 就绪 | 就绪 | 阻塞（空） | [] |
| t1 | P1 send(a) | 1 | [a] | 完成（就绪） | 就绪 | 就绪 | **被 notify 唤醒** | [] |
| t2 | P2 send(b) | 2 | [a, b] | 就绪 | 完成 | 就绪 | 运行中 | [] |
| t3 | P3 send(c)：**满了** | 2 | [a, b] | 就绪 | 就绪 | **阻塞**（加入 waiters） | 运行中 | [P3] |
| t4 | C recv()：拿走 a | 1 | [b] | 就绪 | 就绪 | 在 waiters 里 | 准备处理 a | [P3] |
| t5 | C 唤醒 waiters 队首 P3：把它的 c 塞进队列 | 2 | [b, c] | 就绪 | 就绪 | **被唤醒**，完成 | 处理 a | [] |
| t6 | P1 send(d)：**满了** | 2 | [b, c] | **阻塞** | 就绪 | 就绪 | 处理 a | [P1] |
| t7 | C recv()：拿走 b | 1 | [c] | 在 waiters 里 | 就绪 | 就绪 | 处理 b | [P1] |
| t8 | C 唤醒 P1：塞 d | 2 | [c, d] | 完成 | 就绪 | 就绪 | 处理 b | [] |

看 t2→t3：P3 想 send 但队列已满，它**不能**返回（消息丢了就糟了），也**不能**自旋（烧 CPU），它必须**睡过去**——把自己的 `Thread` 句柄塞进 `waiters` 队列，然后 `park`。消费者在 `recv` 之后看到 `waiters` 非空，就把队首的 `Thread` 拿出来 `unpark`，让它完成塞入操作。

**有界背压怎么防 OOM？** 在爬虫场景下：10 个 worker 各塞一条就让队列满（容量 2），第 3 个 worker 一塞就被阻塞；writer 处理掉一条，才放一个 worker 进来。**worker 的吞吐量被强行降到 writer 的吞吐量**——磁盘慢，整条流水线就慢，但**内存永远只占几条**。这就是背压的本质：**让快的等慢的，而不是让慢的被快的淹没**。

> **更现实一点的容量**：生产代码里容量不会是 2，而是几百到几千——大到能吸收短暂的突发（"快的一段暂时比慢的快"），小到不会让内存爆炸。容量选多少是个工程问题，要看内存预算和延迟预算。Go 语言标准库的 channel 默认就是有界的，鼓励"小容量或无缓冲"——这是一种**显式背压文化**。

**无界真的总是错吗？** 不一定。如果生产者**有自然上限**（比如只有 10 个 worker，每个 worker 同时只处理一个 URL），那无界队列也不会无限涨——最多堆 10 条。无界通道的麻烦在于它**把限制藏在实现里**：你以为它能扛一千万，实际上 OOM 在百万级别就来了。有界通道把限制**写在接口上**（`bounded(1000)`），让上下游都看得见。**显式的边界，比隐式的"应该没问题"可靠得多。**

`forge-channel::mpsc` 当前实现**还没有**有界版本。这是一个练习——动手清单里有。它的实现思路：在 `Shared` 里加 `capacity: usize` 和 `not_full: Condvar`，`send` 里如果 `queue.len() >= capacity` 就 `not_full.wait`。结构和 `recv` 完全对称。

### 5.3 mpsc 的关闭语义：所有 Sender drop 之后

`mpsc` 有一个 oneshot 不存在的问题：**接收方怎么知道"再也收不到消息了"？** 在 oneshot 里这一点没有歧义——只发一条，收完就完。但 mpsc 可能发任意多条，接收方循环 `recv()` 何时该停？

答案：**当所有 Sender 都 drop 了**。这是 Rust 标准库 `std::sync::mpsc` 的语义——最后一个 Sender drop 时，Receiver 的 `recv` 会收到一个"通道关闭"信号（在标准库里表现为 `Err`）。我们当前的 `forge-channel::mpsc` 还没实现这个——`recv` 会永远阻塞下去。这是一个明确的待办：

- `Shared` 里加一个 `senders: AtomicUsize`，记录活的 Sender 数；
- `Sender::clone` 时 `fetch_add(1)`；
- `Sender::drop` 时 `fetch_sub(1, Release)`，如果减到 0，`notify_one` 唤醒接收方；
- `recv` 里检查 `senders == 0 && queue.is_empty()`，是的话就返回"关闭"。

注意这里又一次出现"`fetch_sub(Release)` + 最后一次 `Acquire`"的模式——和 M4 的 `Arc` 引用计数**完全同构**。`Arc::drop` 用 `fetch_sub(Release)` 减引用计数，最后一个减到 0 的人用 `Acquire` fence 保证看到所有之前的修改；这里的 Sender 计数也是同一回事。**M4 的 Arc 计数 = mpsc 的 Sender 计数 = 同一个骨架**。

这个观察请你也记住：**所有"引用计数式的协调"都长得一样**。无论数的是 Arc 引用、Sender 句柄、还是 worker 任务——只要"最后一个走的人负责善后"，结构就一定包含 `fetch_sub` + 条件 + fence。这是并发编程里最常复用的一块代码骨架。

---

## 一个统一视角：就绪位 = 停止位 = once flag

请你回头看本章里出现过的所有"原子标志位"：

- oneshot v2 的 `in_use`（"是否已开始 send"）
- oneshot v2/v3 的 `ready`（"消息是否就绪"）
- 自旋锁的 `M1.6`"locked"标志（"锁是否被持有"）
- M1.6 的 `Once`/`once_flag`（"初始化是否已完成"）

它们**结构上完全同构**。都是一个原子布尔/整数，用 `swap` 或 `compare_exchange` 从一个状态原子地转到另一个状态，**只有那个成功转换的线程能继续**——其他线程要么看到"已占用"就 panic / spin / park，要么看到"已就绪"就配 Acquire 读到对方写的内容。**这就是 M1.6 里讲的"Release/Acquire 契约"在通道场景下的复用**：

- 自旋锁：`swap(true, Acquire)` 抢锁成功的人，看到上一个持有者 Release 的内容；
- oneshot：`swap(false, Acquire)` 拿到 ready=true 的接收方，看到发送方 Release 写的消息；
- Once：`compare_exchange(UNINIT, INITIALIZING, Acquire)` 抢到的人执行初始化，其他人等到 `INITIALIZED`（Release）后 `load(Acquire)` 看到初始化的数据。

**就绪位、停止位、once flag——同一个骨架。** 这就是同构的力量：学一个，会一片。

**为什么这种同构重要？** 因为你以后看到任何"用原子布尔做一次性协调"的代码，都可以**直接套这套骨架**——不用重新发明。比如：

- 你要做一个"全局只初始化一次的 logger"——`Once` 的模式；
- 你要做一个"任务跑完发一个完成信号"——`oneshot::ready` 的模式；
- 你要做一个"互斥访问某资源"——`spinlock::locked` 的模式。

写第一遍的时候你是"发明者"，吃力；写第二遍的时候你是"搬运工"，轻松；第三遍开始你是"老师"，能给别人讲。**这一章希望你从发明者直接跳到老师**，因为我们把骨架抽出来了。**这是抽象的回报**：一次学会，反复套用。

---

## 接入：oneshot 是 JoinHandle，mpsc 是爬虫汇聚

请把这两个名字记下来，因为后面会反复用到：

- **`oneshot` 就是 M9 异步运行时的 `JoinHandle<T>` 的内核**。你 spawn 一个 future，它跑完之后要把结果送回调用方；调用方或者阻塞等（同步版 `JoinHandle`），或者 await（异步版）。背后都是一个 one-shot 通道——发一次、收一次。"任务句柄"就是 `Receiver<T>`。
- **`mpsc` 就是 M10 爬虫的"N 个 worker 把结果汇聚到一个 writer"的工具**。每个 worker 拿一个克隆的 `Sender`，往里 send 抓到的页面；writer 线程拿唯一的 `Receiver`，循环 recv，写到磁盘。N→1 的扇入，正是 mpsc 的形状。

M5 这两个通道会在 M9 / M10 里被反复用到。这一章不是孤岛，是**运行时的地基**。

---

## 全章复盘：权衡的桌

原书第 5 章真正的礼物不在"通道怎么写"，而在它把**安全 / 便利 / 灵活 / 简单 / 性能**五个目标摆上桌，让我们看它们如何互相拉扯：

- 想要**编译期安全**（消费 self）→ 丢了"可多次发送"，得引入 `Arc` 付一次分配；
- 想要**省分配**（借用）→ 得和生命周期、`!Send` 限制打交道，丢了灵活性；
- 想要**阻塞便利**（内建 park）→ 得把 Receiver 钉在一个线程上；
- 想要**灵活（多生产者、无界）** → 得回到 `Mutex + Condvar`，付每次 send/recv 的锁开销。

**没有免费午餐**。但 Rust 让你能"用一点 A 换多一点 B"，并让编译器替你守住换来的不变量。`forge-channel` 同时给了 `oneshot`（极致性能、编译期保证、借用、阻塞）和 `mpsc`（最大灵活、`Mutex+Condvar`、多生产者）——按用例选。这正是工程意义上的"知道每种工具适合什么"。

---

## L1–L5 缩放回顾

| 层 | 你能…… |
|---|---|
| **L1** | 一句话：通道在线程间传消息；one-shot 只传一条。 |
| **L2** | 类比："一次性信封"（oneshot）、"有门铃的墙"（park/unpark）、"信箱队列"（mpsc）、"流水线被卡住的反压力"（背压）。 |
| **L3** | 跟踪六版本：unsafe 雏形 → 运行时检查（in_use/swap）→ 类型级（send(self) 消费 + Arc）→ 借用（split + 生命周期）→ 阻塞（Thread 句柄 + park/unpark + PhantomData 令 Receiver !Send）→ 单 AtomicU8 编码四态（练习）。 |
| **L4** | 解释：(a) `MaybeUninit<T>` 为何比 `Option<T>` 省空间；(b) `send(self)` 如何把"send 两次"从运行时 panic 变成编译期错误；(c) `PhantomData<*const ()>` 如何让 `Receiver: !Send`；(d) `swap(false, Acquire)` 为何一石二鸟；(e) 为什么 `in_use` 用 Relaxed 足够，而 `ready.store` 必须 Release。 |
| **L5** | 为给定用例在安全/便利/性能间做权衡：何时选 oneshot、何时选 mpsc、何时该上背压；并知道每一步取舍省了什么、赔了什么。 |

---

## 自检

- [x] 先让读者感到痛苦（第零拍列了四个洞）。
- [x] 类比贯穿：窄缝 → 门铃；明确指出换档。
- [x] 手算例子 #1：丢失唤醒的三种交错时刻表（ready 值 / token / T2 状态）。
- [x] 手算例子 #2：有界 mpsc 的逐拍队列长度 / 生产者状态 / waiters 队列。
- [x] 一节一个新概念（MaybeUninit → 运行时检查 → 类型级 → 借用 → 阻塞 → mpsc → 背压，每节只一个）。
- [x] 故意打破：第二版"完全 safe"之后指出 panic 仍可能、TOCTOU 影子、窄缝类比失效。
- [x] 同构点明：就绪位 = 停止位 = once flag，复用 M1.6 的 Release/Acquire 契约；oneshot 的 `while+swap+park` 与 M2 的 `while+condvar.wait` 是同构。
- [x] 接入：oneshot→JoinHandle，mpsc→爬虫汇聚。
- [x] 禁用"显然/就是/简单/众所周知"（已检查）。

---

## 动手清单

- [ ] `tests/m5_01_oneshot_blocking`：跑通，对照本文章节确认每个版本的演化。
- [ ] `tests/m5_02_oneshot_non_copy`：验证 `Vec` 被 move 而不是 copy（注释掉 `send(self)` 改成 `send(&self)`，看编译错误）。
- [ ] `tests/m5_03_oneshot_drop_unread`：用 miri 跑 `cargo +nightly miri test -p forge-channel --test m5_03_oneshot_drop_unread`，确认不泄漏。
- [ ] `tests/m5_04_mpsc_multi_producer`：把生产者数和 PER 改大（比如 8 × 10000），观察行为不变。
- [ ] `tests/m5_05_mpsc_blocking`：把 `Duration::from_millis(50)` 改成 5，看测试是否还稳；思考为什么。
- [ ] **练习**：实现第六版（单 `AtomicU8` 编码 EMPTY/WRITING/READY/READING 四态），对照原书。
- [ ] **练习**：给 `forge-channel::mpsc` 加一个**有界**版本 `bounded(capacity)`，用 `not_full: Condvar` 在 send 时阻塞。验证容量 2、3 生产者的逐拍行为是否与本文章节 5.2 一致。
- [ ] **练习**：在 `oneshot` 里把 `Receiver` 的 `_no_send` 改成 `PhantomData<Rc<()>>`（也是 `!Send`），看测试是否还编译。比较两种"令类型 !Send"的写法。

---

下一站 → [M6 内核底座 atomic-wait](./M6-atomic-wait.md)：自旋锁烧 CPU、`Condvar` 要配 Mutex——有没有更底层的"睡/醒"原语？我们下到内核，包一个跨平台的 `futex`（Linux）/ `os_unfair_lock`（macOS）/ `WaitOnAddress`（Windows），它是 M7 自建真锁的地基。
