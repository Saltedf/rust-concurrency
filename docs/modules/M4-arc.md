# M4 — 自建 Arc / Weak

> 模块：`forge-core::arc`　|　测试：`crates/forge-core/tests/m4_*.rs`
> 跑：`cargo test -p forge-core --test m4_*`　|　miri：`cargo +nightly miri test -p forge-core --test m4_04_stress`

---

## ENEMY · 你身体里要先感到的那个痛

先做一个**梦**。

你住在一栋合租房里，三个人合租：你、小明、小红。客厅有一盏灯。规则是这
样：**最后一个离开客厅的人关灯**。

这听起来是个简单的规则。但请你**真的**在脑子里走一遍这个噩梦般的晚上：

- 你 22:00 起身去厨房倒水，路过客厅门口瞥了一眼——小明还在沙发上看书。
  你想："我不关灯，反正小明在。"
- 你 22:01 端着水回卧室。**就在这 30 秒里**，小明突然起身、关灯、回房间
  了——你以为他还在。
- 你 22:05 又出来上厕所，**默认客厅有灯**，一脚迈进去——绊倒在黑暗里，
  撞到茶几。

这栋合租房出错的方式不是"谁忘了关灯"。出错的方式是：**每个人都在用自己
30 秒前看到的信息做决定，而事情在 30 秒的缝隙里变了**。

把"客厅的灯"换成"一块堆上的内存"，把"三个室友"换成"三个线程"，你就站在
`Arc<T>` 真正要解决的问题门口了。

`Arc` 想干的事情只有一句：**让好几个变量共同拥有一块堆内存，并且保证只有
当最后一个所有者离开时，这块内存才被释放**。"最后一个"这三个字，看着朴
素，在多核上却是一个**深渊**。因为多核处理器**没有**一个统一的"现在"：每
个核都有自己的小本本（store buffer），每个核写下去的东西，要过一小会儿才
会被别的核看见。所以你看到的"现在是 1"，可能已经是别人眼里的"0"了。

这个模块我们就要亲手把 `Arc` 和它的弱表亲 `Weak` 从零搭起来，把每一处内
存序都钉死在它**解决的那个具体噩梦**上。这是本课程里**最微妙**的一段
unsafe 代码——两个原子计数器协同，外加一个用 `usize::MAX` 当"锁"的临时自
旋锁。原书把这章放在第 6 章，不是因为它的 API 重要，而是因为它**逼你在最
小的代码块里把所有内存序的功夫用完**。它是个**靶场**。

读完这一节，请记住那个梦——室友、灯、30 秒的缝隙。整个 `Arc` 的实现，就
是工程师们为了堵死这个缝隙而发明出的一套**拍子**。

还有一件事要说在前头：这一章会**用尽**前几章关于原子操作和内存序的全套工
具——`Relaxed`/`Release`/`Acquire`/`fence`、`compare_exchange`、CAS 循
环。如果你对任何一个还不熟，回去翻 M3 的对应小节再来。**这一章不会重新
解释这些原语的定义，只会解释它们在 Arc 里各自挡下的是哪一颗子弹**。把它
当作 M3 的综合应用题。

---

## 这一章的"一个东西"

每一章我都只让你带走**一个东西**。这一章的那个东西是：

> **末次 drop 不是"减完就 free"，而是"减完之后跨所有核立一道栅栏，确
> 认此前每一个人都放下手里的活，再去 free"。**

这一句话里每个词都是后面几节要展开的。如果你读完这一章只能记住一句，记
这句。

---

## ANCHOR · 把你已经会的拼上来

在 M2 里你已经用过 `std::sync::Arc`。你知道它是个**共享所有权**的智能指
针：`Arc::new(x)` 在堆上分配一块内存放 `x`，外加一个隐藏的"计数器"；克隆
一个 `Arc` 不复制 `x`，只把计数器 `+1`；释放一个 `Arc` 把计数器 `-1`；当
计数器**从 1 减到 0** 的那一次，负责把堆上的内存 free 掉。

你脑子里大概有这么张图：

```
      arc_a ───┐
               ├──► [ 计数器=2 | 数据: String="hi" ]   ← 一块堆内存
      arc_b ───┘
```

这张图里有一个东西是**撒谎**的：它把"计数器"和"数据"画在一块连续的内存
上。其实它确实是——但我们一会儿要拆开看，这两件事的**生命周期是不一样
的**。计数器必须活到"最后一个指针被释放"，而数据在"最后一个**强**指针"
被释放时就可以死了。这个差别，是后面整章的引擎。

另外你已经会的两件事，我再钉一下：

1. **`Box<T>` 是独占所有权**。`Arc` 不是独占——所以它**不能**用一个 `Box`
   当字段。`Box` 在类型层面就承诺"我是唯一的拥有者"，编译器会拒绝你把它
   复制成两个。
2. **Rust 的引用 `&T` 是借用的**，它的生命周期必须由某个拥有者担保。而
   `Arc` 想表达的拥有关系是"直到最后一个克隆被释放"——这个持续时间**无
   法**用 Rust 的生命周期参数写出来（生命周期描述的是"我借多久"，不是"我
   跟其它几个克隆共存亡"）。

所以 `Arc` 在底层用的既不是 `Box` 也不是 `&`，而是**一根裸指针**，外加程
序员手动承诺"这根指针永远指向活的内存，直到计数归零"。这是 unsafe 的领
地。下面我们从这里开始。

---

## LOW-FI · 第一版：就一个计数器

我们的第一版故意做得**最简单**——只支持 `Arc`，不支持 `Weak`，只数强指针。
等这个能跑了，再让现实进来打脸。

### 数据布局

把"计数器 + 数据"打包成一个**内部分配块**。注意：这个结构体**对外不公
开**。它是 `Arc` 的实现细节，用户根本不需要知道它存在。

```rust
struct ArcData<T> {
    ref_count: AtomicUsize,   // 当前有几个 Arc 指向我
    data: T,                  // 真正的数据
}
```

然后 `Arc<T>` 本身**只是一根指针**。它不是 `Box`，不是 `&`，是
`NonNull<ArcData<T>>`——一根**永不为 null** 的裸指针。为什么不用 `*mut`？
两个原因：

1. `*mut` 允许 null，而我们承诺这根指针永远指向一块活的 `ArcData`。用
   `NonNull` 把这个承诺编码进类型。
2. 顺带的好事：`Option<Arc<T>>` 会和 `Arc<T>` **一样大**，因为 null 那个
   比特模式被用来表示 `None`，不需要额外的 tag。

```rust
use std::ptr::NonNull;

pub struct Arc<T> {
    ptr: NonNull<ArcData<T>>,
}
```

`NonNull` 是个裸指针的包装。**编译器一看裸指针就保守地认为它 `!Send +
!Sync`**——它假设你不知道自己在干什么。所以这一节我们干的**第一件 unsafe**
不是读写内存，而是**承诺 `Arc<T>` 可以跨线程**：

```rust
unsafe impl<T: Send + Sync> Send for Arc<T> {}
unsafe impl<T: Send + Sync> Sync for Arc<T> {}
```

这两行的意思你得停下来想清楚，因为它**正是 `Arc` 安全性论证的根**。把一个
`Arc<T>` 发到别的线程，等于让那个线程拿到 `T` 的引用并**共享读**——所以
`T` 必须 `Sync`。也等于让那个线程**可能成为最后 drop 这个 `T` 的人**——
所以 `T` 必须 `Send`（能被搬到另一个线程去销毁）。两个条件都要，所以签名
是 `T: Send + Sync`。这就是**铁律**：`Arc<T>: Send + Sync ⟺ T: Send +
Sync`。`Weak<T>` 同理，下面不再重复。

### 把这个论证掰开揉碎

这一段论证足够重要，值得再花一拍把它掰到分子级别。`Send` 和 `Sync` 是
Rust 用类型系统**静态拒绝数据竞争**的工具：

- `T: Send` 意思是"把 `T` 的所有权搬到另一个线程是安全的"。
- `T: Sync` 意思是"让多个线程**同时**通过 `&T` 访问 `T` 是安全的"。

那 `Arc<T>: Send` 为什么需要 `T: Send` **和** `T: Sync` 两条？因为
`Arc<T>` 跨线程的方式**比 `Box<T>` 更复杂**：

1. 你把 `Arc<T>` 发到线程 B，线程 B 可能**永远持有**它（比如存到一个全局
   表里）。最终当所有 `Arc` 释放完，**drop 这个 `T` 的可能是 B 线程**——
   不是当初造它的那个线程。所以 `T` 必须 `Send`（能跨线程搬走所有权去销
   毁）。这一条 `Box<T>: Send` 也要求，并不新鲜。
2. 但 `Arc` 比 `Box` 多一件事：发到 B 的同时，**A 线程可能还持有一个克
   隆**。于是 A 和 B **同时**通过各自的 `&Arc<T>` 解引用到 `&T` 来读它。
   两个线程同时 `&T`——这就是 `Sync` 的定义。所以 `T` 必须 `Sync`。这一
   条是 `Arc` **独有**的，`Box` 没有这个要求（`Box` 是独占，不会有两人同
   时读）。

把两条放在一起：`Arc<T>: Send ⟺ T: Send + Sync`。`Sync` 那条因为
`&Arc<T>: Clone`——任何人拿到 `&Arc<T>` 都能造出新的 `Arc<T>`，从而产生
共享 `&T`，所以 `Arc<T>: Sync` 也要求一样的条件。

**反例检验**：假设 `T = Cell<u32>`。`Cell` 是 `!Sync`（它允许通过 `&T` 改
内部值，多线程同时改就是数据竞争）。如果我们的 `unsafe impl` 不要求
`T: Sync`，那 `Arc<Cell<u32>>` 就能跨线程，两个线程同时 `.set()`，数据竞
争——Rust 的类型系统承诺破产。所以这个约束**不能漏**。

> 这就是为什么 unsafe Rust 里 **`Send`/`Sync` 的 impl 是 unsafe 的**——编
> 译器把论证责任完全交给你。你写错一行，整个程序的内存安全就崩了。这一
> 章里我们写四次（`Arc` 和 `Weak` 各 `Send`+`Sync`），每一次都是同一个论
> 证。

> **类比小检验**：把 `T` 想成一个微波炉。`Send` 是"微波炉能搬去别人家"。
> `Sync` 是"几个人可以同时看这台微波炉"。`Arc<T>: Send` 要求这台微波炉能
> 搬去别人家，因为**有可能**最后一个释放它的人就是别人。`Arc<T>: Sync`
> 要求多人能一起看，因为 `Arc<T>` 一旦 `&self` 借出去，谁都能 `clone` 出
> 一个新的强引用，从而**共享**读到这台微波炉。两件事缺一不可。

### new：分配 + 放弃所有权

`Arc::new` 干三件事：

1. `Box::new` 在堆上分配一块 `ArcData`，初始化计数为 1（现在只有我一个是
   拥有者）。
2. `Box::leak` **故意忘记** `Box` 持有的独占所有权——我们不再让 `Box` 管
   这块内存了，所有权完全交给 `Arc` 的引用计数机制。
3. `NonNull::from` 把它转成永不为 null 的指针。

```rust
impl<T> Arc<T> {
    pub fn new(data: T) -> Arc<T> {
        Arc {
            ptr: NonNull::from(Box::leak(Box::new(ArcData {
                ref_count: AtomicUsize::new(1),
                data,
            }))),
        }
    }

    // 私有助手：从 Arc 取到 ArcData 的共享引用。
    // 这一步要 unsafe，因为编译器不知道这根指针还活着。
    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }
}
```

### Deref：让 Arc<T> 像个 &T

我们想让 `arc.foo()` 直接调到 `T` 的方法，所以实现 `Deref`：

```rust
impl<T> Deref for Arc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data().data
    }
}
```

注意我们**故意不实现 `DerefMut`**。`Arc` 是共享所有权，共享意味着"可能有
别人也拿着它"——给 `&mut T` 等于违反 Rust 的别名规则（aliasing rule）：
同一时间最多只能有一个 `&mut`，且不能有任何 `&` 同时存在。所以无条件给
`&mut T` 是**直接的未定义行为**。后面我们会做一个**条件**版本叫
`get_mut`。

### Clone：Relaxed 加一

克隆一个 `Arc` 不复制 `T`，只让计数器 `+1`：

```rust
impl<T> Clone for Arc<T> {
    fn clone(&self) -> Self {
        // 防御：逼近 usize::MAX/2 就整体 abort。
        // 用 MAX/2 而不是 MAX-1，因为 abort 不是瞬时的——
        // 但 MAX/2 个线程不可能同时存在（每个线程占至少几字节内存）。
        if self.data().ref_count.fetch_add(1, Ordering::Relaxed) > usize::MAX / 2 {
            std::process::abort();
        }
        Arc { ptr: self.ptr }
    }
}
```

**为什么这里 `Relaxed` 够？** 这是一个真问题，停下来想。

`Relaxed` 的意思是：这一次原子操作**只保证自己是原子的**，不保证和其它任
何变量的读写有任何先后顺序。换句话说，编译器和硬件**可以**把这次
`fetch_add` 重排到附近任何位置。

那么"克隆计数 +1"为什么不需要任何同步？因为**它本来就没什么要同步的**。
克隆前我已经能通过原 `Arc` 访问到 `T`，克隆后我多了一个 `Arc` 也能访问
`T`——`T` 本身没变，没有任何"先写数据、再发布给别的线程看"的发布关系。
这里只是一个数字 `+1`，能保证不丢就行。`Relaxed` 给你的"原子性 + 不丢
减"已经够了。

> 把它和后面 drop 的 `Release` 对比着记：**clone 是"我也要看了"，没有同
> 步语义；drop 是"我不再看了"，必须告诉最后一个人"我放下了"。前者
> Relaxed，后者 Release。**

### 防御性的 `abort`：为什么是 `MAX/2` 而不是 `MAX-1`

那一行 `if ... > usize::MAX / 2 { abort() }` 看着多余——`fetch_add` 加到
`MAX` 才会溢出，不是吗？问题在于：`abort` **不是瞬时操作**。一个线程检测
到计数过大、调 `process::abort`，到进程真的死去，中间有时间——这段时
间里另一个线程可能又调了一次 `clone`，把计数再加一。如果你用 `MAX-1` 当
阈值，到 `MAX-1` 时 abort，但 abort 期间另一个 clone 把它加到 `MAX`，再
一次 `fetch_add` 就**回绕到 0**——计数看起来归零，触发**错误的 free**。

`MAX/2` 这个阈值来自一个朴素的事实：每个线程占至少几字节的栈空间，所以
`usize::MAX/2` 个线程**物理上不可能同时存在**（在 64 位系统上是 2^63 个
线程，比宇宙里的原子还多）。所以一旦看到计数过 `MAX/2`，**它一定是被
`mem::forget` 之类的手段灌爆的 bug**——abort 是合理的应对。这是 Mara Bos
强调的一个工程细节：**unsafe 代码的防御要考虑 abort 的非瞬时性**。

### Drop：这一节的高潮，请慢一点读

释放一个 `Arc` 把计数器 `-1`。**只有最后一个释放者**——也就是看到计数从
1 减到 0 的那个人——负责 free。

```rust
impl<T> Drop for Arc<T> {
    fn drop(&mut self) {
        if self.data().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            // 我看到减完是 0 —— 我是最后一个。
            fence(Ordering::Acquire);   // ← 这一行的存在感是整章的灵魂
            unsafe {
                drop(Box::from_raw(self.ptr.as_ptr()));
            }
        }
    }
}
```

这三行（`fetch_sub(Release)` → 检查是不是 1 → `fence(Acquire)` → free）
是**整个 Arc 设计里最值钱的三行**。Mara Bos 在书里用了大半章在解释它，这
里我们用一个**逐拍**的例子讲透。

---

## WRITE · 手算例子 #1：末次 drop 的 Acquire 栅栏

### 没有这道 fence，会发生什么？

我们演一场戏。角色：

- **T0** 是主线程。它造了一个 `Arc<String>`，内容是 `"hello"`。
- T0 把这个 `Arc` **克隆**给两个工作线程 T1、T2（每个拿到一个克隆）。这时
  候计数器 `ref_count = 3`。
- T0 自己 drop 了手上的 `Arc`。`ref_count` 从 3 减到 2。它不是最后一个，
  所以它什么也不做，正常返回。
- **T1** 在它的线程里**还在读**这个 `String` 的字节（比如在调
  `s.as_bytes()` 拿指针解引用）。
- **T2** 也 drop 了。它看到计数从 2 减到 1——它也不是最后一个。
- 然后 T1 也 drop 了。**T1 的 drop** 看到计数从 1 减到 0——它是最后一个。

听起来一切都好：T1 在 drop 之前，已经读完了 `String`，然后才把计数减到
0，然后 free。问题在哪？

**问题在硬件实现层面**。真实 CPU 上，"读 String 的字节"和"减计数"是**两
条独立的指令**，而**它们之间没有同步关系**——除非我们建立。具体地，看
x86 这种"强模型"CPU 还好；但 C++/Rust 的内存模型是**机器无关**的，它承
诺的同步关系不能依赖具体 CPU。所以模型允许下面这种交错：

为了让你**真切**地看到，我把两个核的 **store buffer**（每个核的私有"待发
件箱"）画出来。store buffer 是真实硬件的概念：核执行一条 `store`，**不
是立刻**写到主内存，而是先扔进自己的 store buffer，过几十个时钟周期才"刷
新"到主内存。其它核在那之前**看不到**这次写。

我们要关心两个值：

- `ref_count` 这个原子变量在主内存里的值。
- T1 私有的几次"读 `String` 的字节"的 load 指令。

**坏版本**（假设我们错把 `fence(Acquire)` 删掉）：

| 拍 | T1 在干什么 | T2 在干什么 | ref_count 主内存值 | 备注 |
|---|---|---|---|---|
| t1 | 读 `s.as_bytes()[0]`（拿到 'h'） | — | 2 | T1 还在读字符串 |
| t2 | 读 `s.as_bytes()[1]`（拿到 'e'） | — | 2 | |
| t3 | 调 `fetch_sub(1, Release)`：ref_count 2→1 | — | 1 | T1 不是最后，不 free |
| t4 | — | 调 `fetch_sub(1, Release)`：ref_count 1→0 | 0 | T2 **是**最后！要 free |
| t5 | — | T2 开始 `Box::from_raw`→ free 那块堆内存 | — | 内存还给分配器 |
| t6 | 读 `s.as_bytes()[2]`（拿 'l'）… | — | — | **USE-AFTER-FREE**：T1 还在读已被 free 的字节 |

注意 t6：T1 在 t3 之前已经**开始**读字符串的字节（t1, t2），但它**还没读
完**——它打算读完 5 个字节 `'h','e','l','l','o'`。它在 t3 才减计数。看上
去它"减之前就读了两个字节"，应该没问题——**但模型不保证**。

具体地：t3 的 `Release`-store（"我把 ref_count 从 2 写到 1"）和 t4 的另
一次 `Release`-store（"我把 ref_count 从 1 写到 0"）**之间**，模型并没
有强制"T1 此前所有的 load（读字符串字节）必须在 T2 的 free 之前完
成"。`Release` 的语义只承诺：**我这次 store 之前的所有读写**，不会被我重
排到 store 之后。但 T1 的"读字符串字节"是**T1 自己的内存操作**，它和 T2
的 free 之间**没有任何 happens-before 关系**——除非末次 drop 加一道
`fence(Acquire)`。

> 这正是合租房噩梦的硬件化身：T1 在 t3 那一刻减了计数，但 T1 仍然**有权
> 继续访问**字符串（毕竟它持有一个 `Arc`，只是即将 drop）。T2 在 t4 看到
> 计数为 0，以为"我是最后一个"，于是 free。**T2 没法知道 T1 此刻还在
> 读**——它只看到计数。

### 加上 `fence(Acquire)` 之后重画同一交错

```rust
if self.data().ref_count.fetch_sub(1, Release) == 1 {
    fence(Acquire);
    unsafe { drop(Box::from_raw(self.ptr.as_ptr())); }
}
```

`Acquire` 栅栏的语义是：**这一次 fence 之后的所有读写，不会被我重排到
fence 之前；并且，它"看到"了所有此前对同一原子变量的 Release-store**。这
句话的两个分量都要。

把它应用到我们的例子：T2 在 t4 做的是 `fetch_sub(Release)`，看到值为 1，
然后**执行 `fence(Acquire)`**。这道 fence **建立了和此前每一次
`Release`-store 的 happens-before 关系**——包括 T1 在 t3 那次 `Release`。
而 T1 的 t3 `Release` 又带着"T1 在 t3 之前的所有内存操作（包括读字符串
的字节）"作为它的"行囊"。所以 fence 之后，T2 看到了 T1 在 t1, t2 读字节
这件事——这就保证了**T1 读完之后**，T2 才被允许开始 free。

重画一遍：

| 拍 | T1 | T2 | 关键约束 |
|---|---|---|---|
| t1 | 读 'h' | — | |
| t2 | 读 'e' | — | |
| t3 | `fetch_sub(Release)` ref_count 2→1 | — | T1 的"读字节"被这次 Release 打包带走 |
| t4 | — | `fetch_sub(Release)` ref_count 1→0 | |
| t4.5 | — | **`fence(Acquire)`** | **happens-before 链条建立**：t1/t2 的读 → T1 的 Release → T2 的 Acquire fence → free |
| t5 | （T1 还想读 'l'？— 但 T1 在 t3 之后已经从 `drop` 返回，它**不再持有 Arc**，不能再访问字符串） | — | T1 在 t3 之后已经没有合法的 `&String` 了 |
| t6 | — | free | 安全 |

注意 t5：T1 在 t3 进入 `drop` 之后，它的 `&mut self`（也就是它对 `Arc` 自
身的所有权）已经在 drop 里了——它**合法地**不能再访问底层数据。这就是
Rust 的借用规则替我们做的另一半工作。所以 T1 在 t3 之后**没有理由**再读
'l'。

那 T1 在 t1/t2 读字节呢？那两次读是**合法的**——T1 当时持有 `Arc`。而
T2 的 fence 把这两次读"等到了"——T1 的 Release-store 把它们打包，T2 的
Acquire-fence 接到包。这就是这套经典配方——

> **"Release 减量 + 末次 Acquire fence"**：每一次 `fetch_sub(Release)` 都
> 是"我放下了对数据的访问"；最后一次 `fetch_sub(Release)` 之后再立一道
> `fence(Acquire)`，等于"我现在确认此前所有人的放下都已经发生了，我才开
> 始 free"。

为什么不是每次都用 `AcqRel`？因为 `Acquire` 这部分同步代价**不便宜**——
它要求 CPU 把自己的 store buffer 和 invalidate queue 都排空。绝大多数
drop 都不是最后一个，没必要付这个钱。**只在末次付**，这是优化的核心。

### 一个常被忽略的细节：为什么是 `fence` 而不是 `load(Acquire)`

你可能在想：既然末次 drop 需要一个 `Acquire` 操作来同步，为什么不直接把
`fetch_sub` 写成 `AcqRel`（一举两得）？

`AcqRel` 的问题在于它**对每一次 drop 都生效**——你为 99% 不是末次的那些
drop 也付了 `Acquire` 的钱。但 `Acquire` 在弱模型 CPU（ARM、POWER）上代价
真的不低：它要求 CPU 排空 invalidate queue，确保随后的 load 看到的是"全
局最新的"值。在 `Arc::clone` 这种高频路径上，每个非末次的 drop 都付这个
钱是**完全浪费**——你不需要看到任何东西，你只是减一走人。

`fence(Acquire)` 写在分支里面，**只在末次执行**。这就是这个优化的全部价
值。把它和"分支预测几乎不出错"的硬件事实结合起来——99% 的情况下分支不
进，CPU 几乎为零成本地跳过这道 fence。

### 把它和 Waker 联起来

记不住为什么这道 fence 重要？等下你读 M9/M10 的时候，会看到一个异步任务
的 `Wake` 实现——它从 `Arc<Shared<Task>>` 里 `upgrade` 一个 `Weak` 出来
触发调度。如果 `Arc` 的末次 drop 缺了这道 fence，**任务可能已经 free
了**，Waker 还在调它的方法——崩溃。这一章不是抽象练习，它是真实运行时的
脊椎骨。

### 这道 fence 不能省——为什么不能用 Relaxed？

如果你硬把整段都写成 `Relaxed`，单线程测试照样过——你的压力测试甚至可能
跑一万次都不出问题。但这是**典型的"灾难藏在低概率事件里"**：在弱内存模
型 CPU（比如 ARM、POWER）上，或者当编译器激进重排时，上述 t6 的
use-after-free **真的会发生**。

这就是为什么 Mara Bos 反复强调：**单元测试通过不能证明正确**。要写**压力
测试**（`m4_04_stress.rs` 让 8 个线程各做 1000 次克隆/释放），还要用
**miri**（Rust 的 UB 检测器，能模拟弱内存模型）跑。我们的 `m4_04` miri
跑过，干净。

### ISO·ZOOM：第一版的限制

第一版能跑，但是它**不会过期**——只要还有一个 `Arc`，数据就死赖着不释
放。这听起来是好事，直到你想造一个**双向**的结构：父节点持有子节点的
`Arc`，子节点也持有父节点的 `Arc`。两条引用构成一个环，谁也不让谁先死，
整个环**永远**泄漏。

这是这一节的"故意打破"：你以为 `Arc` 是万能的共享所有权工具——现在给你一
个它**注定搞不定**的常见场景。我们需要一个**不阻止数据被释放**的引用类
型。它叫 `Weak`。

---

## 第二版：弱指针——两个计数器

### 它解决的敌人

`Weak<T>` 是 `Arc<T>` 的"看门狗表亲"。它指向同一块 `ArcData`，但**不**让
计数器 `ref_count` 加一。所以哪怕有一堆 `Weak`，只要 `Arc` 全没了，数据
该死还得死。`Weak` 想用数据时，要调 `upgrade()`——如果数据还活着，升级
成 `Arc`；如果已经死了，返回 `None`。

回到合租房的类比：`Arc` 是"在租约上的室友"——只要还有一个，房子就归你
们共住。`Weak` 是"以前住过、现在搬走了但还留了把备用钥匙的前室友"——他
偶尔想回来看看（`upgrade`），如果你们还在，他能进来；如果所有人都搬走
了、房子退租了（数据 drop 了），他的钥匙打不开门（`upgrade` 返回
`None`）。但**他的备用钥匙本身是合法的**——不会因为房子退租了就变成野
指针，因为我们**保留了房子的门牌**（`ArcData` 这块分配块）让他能查"房子
还在吗"。

### 环泄漏的真实场景

为什么我们需要 `Weak`？最典型的场景是**双向数据结构**：

- 一个 DOM 树：每个节点有 `Arc` 指向子节点，子节点想反向指父节点。
- 一个图：节点 A 和 B 互指。
- 一个观察者模式：被观察对象持 `Arc` 观察者，观察者反过来引用被观察对
  象。

如果两边都用 `Arc`，**两边互相加一**，谁也不让谁归零——这就是经典的**环
泄漏**。Rust 的 `Arc` 没有垃圾回收器去检测环，所以这个 bug 完全是程序员
的责任。把其中一端（通常是"反向"那端，比如子指父、被观察者持观察者）换
成 `Weak`，环就断了——`Weak` 不阻止释放，所以一侧的归零会触发链式释放。

但要实现 `Weak`，我们需要把"分配块"和"数据"分开管理：

- **数据**（`T`）在最后一个 `Arc` 被释放时就 drop。
- **分配块**（`ArcData<T>` 本身）要活到**最后一个 `Weak` 也被释放**——因
  为 `Weak` 还指着这块内存，你 free 了 `Weak` 就成了悬垂指针。

所以我们要**两个**计数器：

```rust
struct ArcData<T> {
    data_ref_count: AtomicUsize,   // Arc 的数量
    alloc_ref_count: AtomicUsize,  // Arc + Weak 的总数量
    data: UnsafeCell<Option<T>>,   // 用 Option：数据 drop 后换成 None
}
```

`Option<T>` 是用来表示"数据还活着吗"的——`Some(t)` 表示活着，`None` 表示
已被 drop。注意它包在 `UnsafeCell` 里——因为我们想在**独占的瞬间**（引用
计数为 0、没有别人在看）把 `Some` 换成 `None`，这是**内部可变性**。

### Arc::drop 现在

```rust
impl<T> Drop for Arc<T> {
    fn drop(&mut self) {
        if self.data().data_ref_count.fetch_sub(1, Release) == 1 {
            fence(Acquire);
            // 把数据从 Some 换成 None —— 真正 drop 这个 T
            unsafe { *self.data().data.get() = None; }
        }
    }
}
```

注意：这次我们没有 free 整个 `ArcData`——因为可能还有 `Weak` 指着它。我
们只把数据 drop 掉。分配块本身由 `Weak::drop` 在 alloc 计数归零时负责
free（公式跟 `Arc::drop` 一模一样：Release 减量 + 末次 Acquire fence +
`Box::from_raw`）。

`Weak::upgrade` 用一个 CAS 循环：把 `data_ref_count` 从 `n` 加到 `n+1`，
但如果它已经是 0（数据已死），就返回 `None`。这部分是教科书式的 CAS 循
环，没有新的内存序技巧，但有两点值得停一下：

**为什么 `upgrade` 的 CAS 也用 `Relaxed`？** 你可能担心：如果
`upgrade` 和 `Arc::drop` 在并发，会不会有"我加一了，但别人已经看到 0
然后 free 了"的尴尬？答案是不会——`fetch_sub` 和 `compare_exchange` 都
是**原子**的，硬件保证它们按某个全局顺序排队。要么我的 CAS 在 `drop`
之前（看到 n≥1，加成 n+1，`drop` 接着看到至少 1，不会去 free）；要么
`drop` 在我之前（它已经把计数减到 0 并 free 了数据，**但 `ArcData` 还没
被 free**——因为 `Weak` 还在；我的 CAS 看到的是 0，直接返回 `None`）。
两种情况都安全。`Relaxed` 就够了，因为这个原子操作本身**就是**我们要的
全部同步——它没有附带任何"先写 X 再 ++"的发布关系。

**`upgrade` 失败时的 `n = e` 这一行**——把 CAS 失败时返回的**当前值**赋
给 `n`，然后重试。这是 CAS 循环的标准写法：失败意味着"你想像中的那个值
已经过时了，这是真的当前值，拿这个再试一次"。在并发激烈时可能重试很多
次，但每次都是 `Relaxed` load + CAS，开销很小。

### 这版的代价：每次克隆 Arc 要碰两个计数器

第二版能用，**但每个 `Arc::clone` 要 `fetch_add` 两次**（`alloc_ref_count`
和 `data_ref_count` 各一次）。即使你**根本没用** `Weak`，每个克隆都得为
它的存在付钱。这在标准库里是不可接受的——`Arc` 是 Rust 性能敏感代码里的
常客。

接下来我们要做**优化**——让没人在用 `Weak` 的常见情况下，克隆 `Arc` **只
碰一个计数器**。这一步会引出本章最硬核的一段内存序推理。

---

## WRITE · 手算例子 #2：get_mut 和 downgrade 的缝隙

### 优化的核心想法：把"所有 Arc"算作"一个隐式 Weak"

第二版里，`alloc_ref_count = (#Arc) + (#Weak)`。每次克隆 `Arc` 都得让它
加一。但如果你仔细想：只要还有**任何一个** `Arc` 在，分配块就**绝对不
会**被 free（因为 `Arc::drop` 在最后会 drop 数据，但**不 free 分配块**——
是 `Weak::drop` 在 `alloc_ref_count → 0` 时 free）。

所以可以换一个**约定**：`alloc_ref_count = (#Weak) + (1 如果还有任何
Arc)`。也就是说，**所有 `Arc` 合起来算作一个"隐式 Weak"**。

这样：

- 克隆 `Arc` **只动** `data_ref_count`，**不碰** `alloc_ref_count`。
- `Arc::drop` 通常也只动 `data_ref_count`。**只有**最后一个 `Arc` 被释放
  时，才把那个"隐式 Weak"也减掉（造一个 `Weak` 立刻 drop）。
- `Weak::clone`/`Weak::drop` 只动 `alloc_ref_count`。

这就是第三版（最终版）的结构，也是标准库真实使用的设计。`ArcData` 现在长
这样：

```rust
struct ArcData<T> {
    data_ref_count: AtomicUsize,
    alloc_ref_count: AtomicUsize,   // = #Weak + (1 if any Arc)
    data: UnsafeCell<ManuallyDrop<T>>,
}
```

> 这里把 `Option<T>` 换成了 `ManuallyDrop<T>`——两者都能让我们"先 drop 数
> 据，留下空壳"，但 `ManuallyDrop` 大小和 `T` 完全一样，省了一个 tag 字
> 节。代价是要 `unsafe` 手动调 `ManuallyDrop::drop`。

### 但是 get_mut 现在麻烦了

`get_mut` 想给一个 `&mut T` 出去，条件是**"我是唯一的"**。第一版里"唯一"
只看 `ref_count == 1`。现在"唯一"要同时满足两件事：

1. 只有一个 `Arc`（`data_ref_count == 1`）。
2. **没有任何** `Weak`（否则 `Weak::upgrade` 可能随时造出一个新的 `Arc`，
   别名出现）。

由于"所有 Arc 算一个隐式 Weak"，没有任何 `Weak` 等价于
`alloc_ref_count == 1`（这 1 就是隐式 Weak 的贡献）。

所以 `get_mut` 要检查：

```
data_ref_count == 1  AND  alloc_ref_count == 1
```

但**这是两次独立的读**。在它们之间，**别的线程可以动手脚**。

### annoying 函数：让两次检查"各看一个旧值"

我们演一场戏。三个线程：

- **A 线程** 在循环里跑一个叫 `annoying` 的函数：它**不停**地
  `downgrade`→`drop(arc)`→（短暂持有 Weak）→`upgrade`→`drop(weak)`。
- **B 线程** 调 `Arc::get_mut(&mut arc)`，想确认唯一性。

`annoying` 长这样（注意它是**合法的**用户代码，我们没有理由拒绝它）：

```rust
fn annoying(mut arc: Arc<Something>) {
    loop {
        let weak = Arc::downgrade(&arc);   // 造一个 Weak：alloc +1
        drop(arc);                          // 让 Arc 暂时归零（但 weak 还在）
        // ↑ 此刻：data_ref_count=0, alloc_ref_count=1（只有 weak）
        arc = weak.upgrade().unwrap();      // 升级回 Arc：data_ref_count 0→1
        drop(weak);                         // alloc -1
        // ↑ 此刻：data_ref_count=1, alloc_ref_count=1（只有隐式 weak）
    }
}
```

注意 `annoying` 循环过程中**会经过**两个状态：

- 状态 X：`data_ref_count=0, alloc_ref_count=1`（只持 weak）。
- 状态 Y：`data_ref_count=1, alloc_ref_count=1`（只持 arc）。

`get_mut` 想看到的是 Y。但它是**两次独立读**：先读一个计数器，再读另一
个。如果 `annoying` 正好在两次读之间完成一次状态切换，`get_mut` 可能"各
看一个旧值"——比如先看到 Y 时刻的 `alloc_ref_count=1`，**再**看到 X 时刻
的 `data_ref_count=0`（这其实是 0，但加上 annoying 紧接着的 upgrade，
**马上**就变 1 了）。

我们精确地把这个缝隙画出来。**坏版本**（假设 `get_mut` 简单地写
成"先 load alloc_ref_count，再 load data_ref_count"）：

| 拍 | A 线程（annoying） | B 线程（get_mut） | data_ref_count | alloc_ref_count | B 看到的 |
|---|---|---|---|---|---|
| t1 | （处于 Y：持 arc，无 weak）`downgrade` 开始：load 看到 1 | load `alloc_ref_count` | 1 | 1 | alloc=1 ✓ |
| t2 | `downgrade` 的 CAS：alloc 1→2 ✓ | — | 1 | 2 | |
| t3 | `drop(arc)`：data_ref_count 1→0 | — | 0 | 2 | |
| t4 | — | load `data_ref_count` | 0 | 2 | data=0 ✓（恰好） |
| t5 | `weak.upgrade()`：CAS data 0→1 ✓ | — | 1 | 2 | |
| t6 | `drop(weak)`：alloc 2→1 | — | 1 | 1 | |

仔细看 t4：B 看到 `data_ref_count=0`。但 `0 ≠ 1`，按"两个都等于 1"的判
据，B 应该返回 `None`——好像没事？

但**真正的危险**在另一个交错。再演一次：

| 拍 | A 线程 | B 线程 | data_ref_count | alloc_ref_count | B 看到的 |
|---|---|---|---|---|---|
| t1 | （处于 X：持 weak，无 arc） | load `alloc_ref_count` | 0 | 1 | alloc=1 ✓ |
| t2 | `weak.upgrade()` 的 CAS：data 0→1 ✓ | — | 1 | 1 | |
| t3 | `downgrade`：CAS alloc 1→2 ✓ | — | 1 | 2 | |
| t4 | — | load `data_ref_count` | 1 | 2 | data=1 ✓ |

t4 之后 B 看到 `data_ref_count=1`，加上 t1 看到的 `alloc_ref_count=1`，
B **判断**："我是唯一的！"，返回 `&mut T`。

但**实际**此刻 `alloc_ref_count=2`——A 线程刚造出了一个 `Weak`，**马上**
要 `upgrade` 成新的 `Arc`。一旦 A 调 `upgrade`，世界上就有**两个** `Arc`
同时指向同一个 `T`——其中一个还被 B 当成 `&mut T` 借出去。**别名 UB。**

这就是 Mara Bos 说的**annoying 函数**的真实威力：它不需要做任何"坏
事"，只是不停地切换状态，就能让两次独立读的 `get_mut` 看到一个**从未真
实存在过**的组合（`alloc=1 ∧ data=1` 在 t4 那一刻实际**不**成立——
`alloc` 已经是 2）。

### 解药：用 `usize::MAX` 当临时自旋锁

破局的洞察是：**只要让 `downgrade` 不能在我两次读之间发生就行**。我们不
需要真正的 mutex——缝隙只有几条指令——只要让 `downgrade` **自旋等一
下**。

> 类比的延伸：还记得开篇那个合租房的客厅吗？这个 `usize::MAX` 就是**在客
> 厅门口装了一个"更衣室"**——更衣室一次只能进一个人。B 想确认"现在客厅里
> 究竟有没有人"（即 `get_mut` 想确认唯一性），它进更衣室（把
> `alloc_ref_count` 从 1 换成 MAX）。这时候 A 想做 `downgrade`（想往客厅
> 里加一个 Weak）——A 看到更衣室被占了（值是 MAX），**只能在门口转圈等**
> （`spin_loop`）。B 在更衣室里慢慢看另一个计数器，看完了出来（store 把
> MAX 换回 1），A 才被允许进去办自己的事。代价是 A 转了几圈，但**B 看到
> 的状态是真实的**——没有人能在它看的瞬间偷溜进去。

办法：**把 `alloc_ref_count` 临时换成一个特殊值 `usize::MAX`**，约定
"这个值表示 get_mut 正在持锁"。`downgrade` 看到这个值就自旋。

```rust
pub fn get_mut(arc: &mut Self) -> Option<&mut T> {
    // 第 1 步：尝试把 alloc_ref_count 从 1 CAS 成 usize::MAX（持锁）。
    // Acquire：要和 Weak::drop 的 Release-store 同步，
    // 保证随后的 data_ref_count.load 能看到一个刚 upgrade 上来的新 Arc。
    if arc.data().alloc_ref_count
        .compare_exchange(1, usize::MAX, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return None; // 不是 1 → 肯定不唯一，直接失败
    }

    // 第 2 步：持锁期间读另一个计数器。这时 downgrade 进不来。
    let is_unique = arc.data().data_ref_count.load(Ordering::Relaxed) == 1;

    // 第 3 步：解锁——把 MAX 换回 1。
    // Release：要和 downgrade 的 Acquire-CAS 同步，防止"未来的一次
    // Arc::drop"的副作用倒灌进刚才那次 load（见下文解释）。
    arc.data().alloc_ref_count.store(1, Ordering::Release);

    if !is_unique {
        return None;
    }

    // 第 4 步：和第一版一样，加 Acquire fence 与 Arc::drop 的 Release 同步。
    fence(Ordering::Acquire);
    unsafe { Some(&mut *arc.data().data.get()) }
}
```

而 `downgrade` 改成在 CAS 循环里**检查特殊值**：

```rust
pub fn downgrade(arc: &Self) -> Weak<T> {
    let mut n = arc.data().alloc_ref_count.load(Ordering::Relaxed);
    loop {
        if n == usize::MAX {
            // get_mut 在持锁，让一让
            std::hint::spin_loop();
            n = arc.data().alloc_ref_count.load(Ordering::Relaxed);
            continue;
        }
        assert!(n < usize::MAX - 1, "weak ref overflow");
        // Acquire：与 get_mut 的 Release-store 同步，防止 get_mut 的"未来
        // 影响"提前生效。
        match arc.data().alloc_ref_count
            .compare_exchange_weak(n, n + 1, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => return Weak { ptr: arc.ptr },
            Err(e) => n = e,
        }
    }
}
```

### 重画 annoying，证明缝隙被堵住

我们再演一次刚才那个**危险交错**，这次带上锁：

| 拍 | A 线程 | B 线程（get_mut） | alloc_ref_count | data_ref_count |
|---|---|---|---|---|
| t1 | （X：持 weak，无 arc） | CAS：alloc 1→**MAX**（持锁）✓ | MAX | 0 |
| t2 | `weak.upgrade`：data 0→1 ✓ | — | MAX | 1 |
| t3 | `downgrade` 开始：load 看到 **MAX** | — | MAX | 1 |
| t4 | **`spin_loop()`，等** | `data_ref_count.load`：看到 1 | MAX | 1 |
| t5 | **还在等** | store：alloc **MAX→1**（解锁） | 1 | 1 |
| t6 | load 重试，看到 1 | — | 1 | 1 |
| t7 | CAS：alloc 1→2 ✓ | — | 2 | 1 |

关键看 t3-t4：A 想做 `downgrade`，但 `alloc_ref_count` 是 MAX，**它只能
spin**。B 在 t4 安心地 load `data_ref_count`——这次**没有人能在中间插
入** downgrade。B 看到的 `alloc=1` 和 `data=1` 是**真实同时成立**的。B
返回 `&mut`，A 还在自旋——A 必须等 B 解锁后才能继续。**缝隙被堵住。**

> 把这个锁和我们第一讲的"室友-灯"类比连起来：第一版的噩梦是"我看到的
> 客厅状态过时了"。这个锁的发明等于在客厅门口装了一个**只能一人进的更
> 衣室**——你进去之后，外面的人**必须**等你出来才能改客厅的状态。代价
> 是 spin 几个时钟周期，但**消灭了所有交错风险**。

### 这里的内存序几乎每一条都有理由

我们把每一条钉一遍——这是这一章最值钱的一段记忆：

1. **`get_mut` 的 CAS 用 `Acquire`**：和 `Weak::drop`（`Release`-减量）同
   步。为什么？考虑一个线程刚刚 `upgrade` 出一个新 `Arc`——它会先
   `fetch_add(data_ref_count)`，然后 `Weak::drop` 把 `alloc_ref_count`
   减回 1。`Weak::drop` 的 `Release` 带着"我已经把 data_ref_count 加好
   了"这个信息。`get_mut` 的 CAS `Acquire` **接到**这个信息——保证接下来
   的 `data_ref_count.load` **看到那个加好的值**。否则，`load` 可能读到旧
   值（比如还以为是 1，但实际刚刚有人 upgrade 了，已经 2）。
2. **`get_mut` 的解锁 `store(Release)`**：和 `downgrade` 的 `Acquire`-CAS
   同步。考虑一个**未来的** `Arc::drop`——它会把 `data_ref_count` 减 1。
   我们要保证 `get_mut` 的 `data_ref_count.load` **不会**被这个未来的减"
   倒灌"影响（即看到"未来的 0"）。`store(Release)` 把"我那次 load 已经做
   完了"打包带走，`downgrade` 的下一次 `Acquire`-CAS 才能接到——这就阻止
   了重排把未来的减"提前"到 load 之前。
3. **`get_mut` 末尾的 `fence(Acquire)`**：和 `Arc::drop`（`Release`-减
   量）同步。这和第一版的语义一样——保证此前所有通过旧 `Arc` 的访问都已
   结束。
4. **`downgrade` 的 CAS 用 `Acquire`**：和 `get_mut` 的 `Release`-store
   同步。这"反向"地**锁住了 `get_mut`**——使 `get_mut` 之后的一次
   `Arc::drop` 的副作用，不会在 `get_mut` 解锁前就被本线程看到。

这四条像一张蜘蛛网，每根丝都连着一只具体的"坏结果"。如果你将来要写自己
的多原子变量数据结构，**回到这四条**——它们是模板。

### ISO·ZOOM：为什么用 `usize::MAX` 而不是真 mutex

`usize::MAX` 这个"锁"是**自己造的极简自旋锁**。它有几个特点：

- **临界区极短**——就两次原子操作（CAS + store）。spin 几乎不会发生。
- **不需要系统调用**——mutex 涉及内核态切换，对这么短的临界区是杀鸡用牛
  刀。
- **不处理公平性、不处理死锁**——因为它**绝不可能**在持锁期间被阻塞（持
  锁后只有两次本地操作）。所以不会死锁。

这是 Mara Bos 这章给你的**元课程**：当你需要"原子地操作多个原子变量"
时，**临时把其中一个换成哨兵值**是一种便宜而强大的技术。它不是 mutex——
它是**用原子变量自己当锁**。

---

## 把它接进运行时：Arc 是异步运行时的脊椎

学到这里你可能想："这玩意儿除了造数据结构还有什么用？"——答案让你
意外：**整个异步运行时就是建在 `Arc` 上的**。

考虑一个 `async fn`。它会被编译成一个**状态机**——一个 `enum`，每个变体
对应一个 `.await` 点之间的小段。这个状态机要被**扔到堆上**（因为它的生命
周期不固定），然后由调度器在不同线程之间搬来搬去（work-stealing 调度
器）。多个组件要共享它：调度器、唤醒器（Waker）、可能还有定时器。

怎么共享一个堆上的、生命周期不确定的状态机？`Arc<Shared<Task>>`。

我们这个模块的 `ArcData` 跟运行时里的 `Shared<Task>` 在结构上**几乎一
样**：

- 都有一个引用计数。
- 都有"数据被 drop 但分配块还活着"的瞬间（任务结束了，但 Waker 可能还指
  着它）。
- 都需要 `Weak`（Waker 通常持 `Weak<Task>`，避免阻止任务被回收）。

所以你在这里学的**每一拍**——两个计数器、`ManuallyDrop`、`upgrade` 的 CAS
循环、`Release + Acquire fence` 配方——在 M9（运行时）和 M10（执行器）里
会**原样复用**。这一章不是孤岛，它是后面三章的地基。

---

## Drop 顺序谜题：小心那道隐式 drop

`Arc::drop` 的末次分支里有一行容易被忽略：

```rust
if self.data().data_ref_count.fetch_sub(1, Release) == 1 {
    fence(Acquire);
    unsafe { ManuallyDrop::drop(&mut *self.data().data.get()); }
    drop(Weak { ptr: self.ptr });   // ← 这一行
}
```

这是在"造一个 `Weak` 立刻 drop 它"。为什么？因为优化版的约定是"所有
`Arc` 合起来算一个隐式 Weak"——最后那个 `Arc` 没了，这个隐式 Weak 也得消
失，否则 `alloc_ref_count` 永远到不了 0，分配块永不释放。

但它有个**副作用**：在 `Weak { ptr: self.ptr }` 被 drop 时，会跑 `Weak` 的
`Drop::drop`，那里面又一次 `fetch_sub(Release)`——如果这一次也归零，还会
再一次 `fence(Acquire)` 然后 `Box::from_raw` free 整块内存。

也就是说，**末次 `Arc::drop` 实际上触发了两次减量、可能两次 fence**。这
不是浪费——它把"drop 数据"和"drop 分配块"两件事解耦了，让我们能正确处
理"还有 Weak 在"的情况。

> **故意打破再重建**：你可能会问——"那为什么不直接在 `Arc::drop` 里减一
> 次 `alloc_ref_count`？为什么要造一个 `Weak` 再 drop？"答案是**可读性+
> 复用**：`Weak::drop` 的逻辑（Release 减量 + 末次 Acquire fence +
> `Box::from_raw`）已经被写对了一次，我们直接复用它，而不是把同一段不安全
> 代码贴两份。这是 unsafe 代码的**DRY 原则**——重复的不安全代码是 bug 的
> 温床。

### 一个真问的迷思：能不能把数据 drop 和分配块 free 颠倒？

设想你想"先 free 分配块，再 drop 数据"——这**不可能**，因为 drop 数据
需要读 `ArcData::data` 字段，而分配块已经 free 了。所以**数据 drop 必
须先于分配块 free**。我们的实现严格遵守这个顺序：

1. `ManuallyDrop::drop(&mut *self.data().data.get())` —— 先把 `T` 的
   `Drop::drop` 跑掉，调用 `T` 析构函数。
2. `drop(Weak { ptr: self.ptr })` —— 然后才减 `alloc_ref_count`，可能
   free 整块 `ArcData`。

注意第 1 步之后，**`ArcData` 里的 `data` 字段已经是"被 drop 过"的未初始化
内存**——但 `ArcData` 本身还活着（`alloc_ref_count` 还没到 0），所以读它
的其它字段（计数器）仍然合法。这就是 `ManuallyDrop` 的精髓：**它让我们可
以把"T 的析构"和"ArcData 的释放"在时间上分开**，从而正确处理"还有
`Weak` 指着 `ArcData` 但 `Arc` 全没了"的状态。

### 顺序违反的代价：假设你把第二步放到第一步之前

如果你写成：

```rust
// 错误示范！
drop(Weak { ptr: self.ptr });  // 可能 free 了 ArcData
unsafe { ManuallyDrop::drop(&mut *self.data().data.get()); } // 读已 free 的内存！
```

这会立刻 use-after-free——`self.data()` 解引用 `self.ptr`，但 `ptr` 指的
那块内存在上一行可能已经被 `Box::from_raw` free 还给分配器了。第二行的
`get()` 拿到的是悬垂指针。miri 会立刻抓到你。**顺序在 unsafe 代码里不是
风格问题，是正确性问题。**

---

## 完整的第三版实现

下面是 `crates/forge-core/src/arc.rs` 的完整代码，注释把上面讲的每一条内
存序理由都钉死在现场。**你阅读这一节时，把上面的两个手算例子放在手边，
逐行对照。**

```rust
//! # 模块 M4：自建 `Arc<T>` / `Weak<T>`（优化版，接近标准库实现）
//!
//! 两个原子计数器协同，外加一个用 `usize::MAX` 当"锁"的临时自旋锁，
//! 让 `get_mut`/`downgrade` 不会漏掉并发操作。内存序几乎每一条都有理由。

use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::fence;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 内部分配块。不公开——是 Arc/Weak 的实现细节。
struct ArcData<T> {
    /// `Arc` 的数量。
    data_ref_count: AtomicUsize,
    /// `Weak` 的数量，外加 1（只要还有任何 Arc 在）。
    /// 这个"+1"是"所有 Arc 合起来的隐式 Weak"，让克隆 Arc 时不必碰它。
    alloc_ref_count: AtomicUsize,
    /// 数据本身。只剩弱指针时会被手动 drop，所以用 ManuallyDrop。
    data: UnsafeCell<ManuallyDrop<T>>,
}

/// 强引用：共享所有权，只要还有一个 Arc，数据就还在。
pub struct Arc<T> {
    ptr: NonNull<ArcData<T>>,
}

/// 弱引用：不阻止数据被释放；想用得先 upgrade 成 Arc。
pub struct Weak<T> {
    ptr: NonNull<ArcData<T>>,
}

// 安全性论证：把 Arc/Weak 送到别的线程，等于让 T 被多线程共享（要 Sync），
// 也等于让 T 可能被另一个线程 drop（要 Send）。所以 T: Send + Sync 才行。
unsafe impl<T: Send + Sync> Send for Arc<T> {}
unsafe impl<T: Send + Sync> Sync for Arc<T> {}
unsafe impl<T: Send + Sync> Send for Weak<T> {}
unsafe impl<T: Send + Sync> Sync for Weak<T> {}

impl<T> Arc<T> {
    pub fn new(data: T) -> Arc<T> {
        Arc {
            ptr: NonNull::from(Box::leak(Box::new(ArcData {
                alloc_ref_count: AtomicUsize::new(1), // 1 = 隐式 Weak
                data_ref_count: AtomicUsize::new(1),
                data: UnsafeCell::new(ManuallyDrop::new(data)),
            }))),
        }
    }

    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    /// 仅当这是唯一的 Arc、且没有 Weak 时，给 &mut T；否则 None。
    pub fn get_mut(arc: &mut Self) -> Option<&mut T> {
        // 关键：两个计数器是两次独立的读，必须防"读一个、另一个动手脚"的缝隙。
        // 办法：把 alloc_ref_count 从 1 "锁"成 usize::MAX（自旋锁），读完另一个再解锁。
        // Acquire：与 Weak::drop 的 Release 减量同步，保证随后的 data_ref_count.load
        //          能看到一个刚 upgrade 上来的新 Arc。
        if arc.data().alloc_ref_count
            .compare_exchange(1, usize::MAX, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        let is_unique = arc.data().data_ref_count.load(Ordering::Relaxed) == 1;
        // Release：与 downgrade 的 Acquire 同步，防止"未来的一次 Arc::drop"
        //          的副作用倒灌进刚才那次 load。
        arc.data().alloc_ref_count.store(1, Ordering::Release);
        if !is_unique {
            return None;
        }
        // Acquire 栅栏：与 Arc::drop 的 Release 减量同步，
        // 保证此前所有通过旧 Arc 的访问都已结束。
        fence(Ordering::Acquire);
        unsafe { Some(&mut *arc.data().data.get()) }
    }

    /// 从 &Arc 降级出一个 Weak。
    pub fn downgrade(arc: &Self) -> Weak<T> {
        let mut n = arc.data().alloc_ref_count.load(Ordering::Relaxed);
        loop {
            if n == usize::MAX {
                // get_mut 正持锁，自旋等它解锁
                std::hint::spin_loop();
                n = arc.data().alloc_ref_count.load(Ordering::Relaxed);
                continue;
            }
            assert!(n < usize::MAX - 1, "weak ref overflow");
            // Acquire：与 get_mut 的 Release-store 同步，
            // 防止"get_mut 之后的一个 Arc::drop"在 get_mut 解锁前生效。
            match arc.data().alloc_ref_count
                .compare_exchange_weak(n, n + 1, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Weak { ptr: arc.ptr },
                Err(e) => n = e,
            }
        }
    }

    /// 拿一个 Weak（便利方法）。
    pub fn weak(&self) -> Weak<T> {
        Self::downgrade(self)
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
            std::process::abort();
        }
        Arc { ptr: self.ptr }
    }
}

impl<T> Drop for Arc<T> {
    fn drop(&mut self) {
        // Release 减量；只有"最后一个"（从 1 减到 0）才需要 Acquire。
        if self.data().data_ref_count.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire); // 与之前所有 Arc::drop 的 Release 同步
            unsafe {
                ManuallyDrop::drop(&mut *self.data().data.get());
            }
            // 造一个 Weak 再 drop 它，正好减一次 alloc_ref_count
            // （释放"所有 Arc 合起来的隐式 Weak"）。
            drop(Weak { ptr: self.ptr });
        }
    }
}

impl<T> Weak<T> {
    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    /// 尝试升级成 Arc。若数据已被释放（只剩弱指针），返回 None。
    pub fn upgrade(&self) -> Option<Arc<T>> {
        let mut n = self.data().data_ref_count.load(Ordering::Relaxed);
        loop {
            if n == 0 {
                return None; // 没有 Arc 了 ⇒ 数据已 drop
            }
            assert!(n < usize::MAX);
            // CAS 把 data_ref_count 从 n 加到 n+1。Relaxed：只是计数。
            match self.data().data_ref_count
                .compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed)
            {
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
```

---

## 测试：单元测试不够，要压力 + miri

`crates/forge-core/tests/` 下有四个测试，逐级加压：

- **`m4_01_arc_drop.rs`** —— 两个 Arc 跨线程共享一个带 `Drop` 的类型，验证
  最后一个 drop 时数据恰好释放一次。这是单元测试，**只能证明基本功能**。
- **`m4_02_get_mut.rs`** —— 验证 `get_mut` 在唯一时返回 `Some`，有克隆时返
  回 `None`。
- **`m4_03_weak.rs`** —— 验证 `upgrade`/`downgrade`，以及 Arc 全没了之后
  `upgrade` 返回 `None`。
- **`m4_04_stress.rs`** —— **真正的考验**。8 个线程，每个线程把同一个
  `Arc` 克隆 1000 次再全部释放。最终断言全局 `DROPS` 计数**恰好是 1**——
  证明无论线程如何交错，数据**恰好释放一次**。

`m4_04` 还应该用 **miri** 跑一遍——miri 是 Rust 的 UB 检测器，能模拟弱内
存模型，揪出我们肉眼看不出的内存序 bug：

```
cargo +nightly miri test -p forge-core --test m4_04_stress
```

我们的实现 miri 跑过，干净。**这是我能给你的、关于这份代码正确性的最强
证据。**

---

## 安慰一下：这章是本课程最难的一段

如果你读到这一节觉得脑子要炸——正常。Mara Bos 在原书末尾特别写了这句：

> "如果你觉得这版优化的内存序推理很难，别担心。**很多并发数据结构都比这个
> 简单**。`Arc` 之所以放进这章，恰恰是因为它的这些微妙之处。"

换句话说——**过了 Arc 这关，后面 M5（channel）、M7（mutex）、M8
（条件变量）的内存序都不会更难**。这章是地板，不是天花板。

---

## L1–L5 缩放回顾

| 层 | 你能…… |
|---|---|
| **L1** | 一句话：`Arc` 是共享所有权的引用计数；`Weak` 不阻止释放。 |
| **L2** | 类比："合租房，最后一个走的人关灯"（引用计数）；"门牌号还在但人搬走了"（`Weak` 过期）；"客厅门口的更衣室"（`get_mut` 的临时锁）。 |
| **L3** | 跟踪 `clone`（`Relaxed` 加）、`drop`（`Release` 减 + 末次 `Acquire` fence）、`upgrade`（CAS）。知道 `Weak` 引入第二个计数器，优化版把所有 Arc 算一个隐式 Weak。 |
| **L4** | **手算**末次 drop 的 Acquire fence——画出 store buffer 交错，证明缺它会 use-after-free。**手算** `get_mut` vs `annoying` 的缝隙——画出两次独立读被夹击，证明 `usize::MAX` 自旋锁堵住它。 |
| **L5** | 独立推理任意引用计数场景的内存序；知道"Release 减量 + 末次 Acquire fence"是经典配方；能在自己的多原子变量数据结构里复用"`usize::MAX` 哨兵锁"技术；把 `Arc` 的设计与异步运行时的 `Arc<Shared<Task>>` 关联起来。 |

## 自检清单

读完这一节，**不回看**，看你能不能做到：

- [ ] 用自己的话讲清楚：为什么 `clone` 用 `Relaxed` 而 `drop` 用 `Release`。
- [ ] 画出末次 `Arc::drop` 时两个线程的 store buffer 交错，指出"没有 fence
      会发生什么"和"fence 怎么堵住它"。
- [ ] 解释 `usize::MAX` 这个哨兵值在 `get_mut` 和 `downgrade` 之间建立的
      "更衣室"——以及它**为什么不是 mutex**。
- [ ] 说出 `Arc<T>: Send + Sync` 要求 `T: Send + Sync` 的**两条独立理由**
      （共享读、可能被另一线程 drop）。
- [ ] 解释为什么 `m4_04_stress` 这种压力测试**加上 miri** 才能给出"高度可
      信的正确性"，而单元测试不行。

## 动手清单

按这个顺序做，每一步都跑 `cargo test -p forge-core --test <name>`：

1. `m4_01_arc_drop` —— 先确认基础引用计数能跑。**然后故意把
   `fence(Acquire)` 删掉**，重跑 miri，看 miri 是否报错（在弱模型上很可能
   会）。把它加回来。
2. `m4_02_get_mut` —— 验证 `get_mut`。
3. `m4_03_weak` —— 验证 `Weak::upgrade` 在数据还活着时返回 `Some`，在数据
   死了之后返回 `None`。
4. `m4_04_stress` —— 跑压力测试。**然后开 8 个线程，每个线程跑 100 万次
   `clone`/`drop`**（修改 N 和迭代次数），看会不会因为 ref_count 接近上限
   而 abort。

如果你做完这些还想加码：**自己实现一个不带 `Weak` 的极简 `Arc`**（回到第
一版），跑 miri——然后**故意把 `Release` 改成 `Relaxed`**，观察 miri 在哪
个测试里抓到你。这是你能给"为什么内存序重要"装上的最结实的肌肉记忆。

---

下一站 → [M5 自建 Channel](./M5-channels.md)：把 Mara Bos 第 5 章的六版本
one-shot 通道走一遍——从一个会出 UB 的雏形，一步步用类型系统把错误从运行
时挪到编译期，最后接上 park 阻塞。
