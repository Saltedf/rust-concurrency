# M1 — 原子、内存序，以及"谁能跨线程"

> 模块：`forge-core::atomics`　|　测试：`crates/forge-core/tests/m1_*.rs`
> 跑：`cargo test -p forge-core`　|　查 UB：`cargo +nightly miri test -p forge-core`

这一章我们做一件具体的事：**让多个线程安全地碰同一个数、同一个标志、同一个指针，而且大多数时候不必上锁。** 我们不预先下定义，而是从一个个能跑的小程序出发，撞上问题，再**因为需要**而把概念一个一个逼出来。跟着敲、跟着跑，你会真的懂。

整章按 10 个小步推进，每一步都先撞墙、再补一件新武器。两段重心是 **M1.6（用 Relaxed 传 Box 会怎么崩）** 和 **M1.9（为什么必须有 SeqCst）**——这两段我们会逐拍手算硬件行为，是全课程的同构骨架，后面 M3/M4/M5/M8 都会复用它。

---

## 先认识唯一的敌人：数据竞争

我们要并发，本质上就是想让多个线程**碰同一片内存**。Rust 用"借用规则"在编译期就挡住了绝大多数灾难。先把这两条规则在脑子里擦亮——而且要用更准确的说法：

- **共享引用 `&T`**（别再叫"不可变引用"了）：可以复制、可以同时存在很多份。
- **独占引用 `&mut T`**：保证此时**只有它一个**在借用这片数据。

这两条加起来，理论上就能杜绝**数据竞争**——"一个线程在写、另一个线程同时在读/写同一片数据"。

为什么数据竞争这么致命？看这段（来自原书的核心例子）：

```rust
fn f(a: &i32, b: &mut i32) {
    let before = *a;
    *b += 1;
    let after = *a;
    if before != after {
        x(); // 永远不会发生
    }
}
```

编译器被允许假设借用规则永远成立，所以它认定 `b` 绝不可能和 `a` 指同一个 `i32`——于是 `*a` 在两次读取之间不可能变，`if` 永远不成立，**整段 `x()` 会被优化掉**。这套逻辑之所以成立，前提是"没有数据竞争"。

**而数据竞争是未定义行为（UB）**——不是"偶尔算错"，是"编译器从这一刻起可以对你做任何事"。更阴险的是，UB 能"穿越回过去"。看原书这个例子：

```rust
match index {           // ①
    0 => x(),
    1 => y(),
    _ => z(index),
}
let a = [123, 456, 789];
let b = unsafe { a.get_unchecked(index) };   // ② 假设 index < 3
```

因为 ② 用了 `get_unchecked`，编译器**被允许假设 `index` 只能是 0/1/2**。于是它在优化 ① 时，认定 `_` 分支只可能匹配 `2`，进而把 `z` 当成"只会被 `z(2)` 调用"来优化——**这一切发生在 ② 之前**。如果你真的传了 `index = 3`，程序可能在还没走到 ② 的时候就崩了。UB 会向前后两个方向污染整个程序。

> 教训：**只要可能发生数据竞争，整个程序都不可信。** Rust 的所有并发原语，本质都是把"否则会数据竞争"变成"有同步、合法、定义良好"。

但借用规则有个尴尬：它太严格了——"多线程下任何能被多线程碰的数据都不能被改"，那线程之间几乎没法通信。我们需要一个逃生舱。

---

## 内部可变性：通过共享引用去改

**内部可变性**就是那个逃生舱：某些类型"轻微地弯曲了借用规则"，让你**通过 `&T`（共享引用）去改值**，却不引发 UB。

为了不再混淆，从现在起我们用**共享 / 独占**这两个词，而不是"不可变 / 可变"。因为内部可变类型出现后，"`&T` 不能改"这句话就不成立了。

Rust 提供一族内部可变类型，按"能在多大范围用"排序：

- **`Cell<T>`**：单线程用。只能整体拷进拷出（`get`/`set`/`take`），不让你借出内部引用。因为它靠"单线程"这个前提来避免 UB，所以它 `Send` 但**不 `Sync`**。
- **`RefCell<T>`**：单线程用。靠一个运行时计数器跟踪借用，冲突就 panic。
- **`Mutex<T>` / `RwLock<T>`**：多线程版。冲突时不是 panic，而是**让线程睡觉等**（M2 详讲）。
- **原子类型（`AtomicU32` 等）**：多线程版的 `Cell`——只能整体读写，不能借出内部。这是本章主角。
- **`UnsafeCell<T>`**：上面所有人的地基。它只给你一个裸指针，不提供任何安全保证——所有内部可变类型最终都建在它上面（M3 的自旋锁、M4 的 Arc 都会直接碰它）。

> 注意：内部可变只是放宽了"共享借用下能否改"，**没有**放宽独占借用。独占引用仍然保证"此刻没有别的借用"。如果你用 `unsafe` 造出两个同时存在的独占引用，无论有没有内部可变，都是 UB。

好了，逃生舱有了。但还有一个根本问题没解决：**到底哪些类型"有资格"被搬到另一个线程、或被多个线程共享？** 这就是 `Send` 和 `Sync`。

---

## Send 与 Sync：谁来把守线程边界

一个值要从线程 A 进线程 B 的世界，物理上只有两条路：

1. **整个搬过去**（`move`）：A 把所有权交给 B。
2. **借一份引用过去**：A 还持有值，把 `&T` 复制给 B（可能 C、D 也各一份）。

Rust 用两个标记 trait 各管一条：

- **`Send`**：这个类型的所有权**可以搬到**另一个线程。`Arc<i32>` 是 `Send`，`Rc<i32>` 不是。
- **`Sync`**：这个类型**可以被多个线程共享**。准确地说：**`T: Sync` 当且仅当 `&T: Send`**。比如 `i32` 是 `Sync`，`Cell<i32>` 不是（但 `Cell<i32>` 是 `Send`）。

那句等价关系值得多看一眼，因为它把两个 trait 焊在了一起：**"共享 `T`"在 Rust 里就是"把 `&T` 复制到各线程"，而"能复制到别的线程"正是 `Send` 的定义——所以 `T: Sync` ⟺ `&T: Send`。** 记住它，你以后判断任何类型都很快。

这两个是 **auto trait（自动 trait）**：编译器看你的结构体字段——所有字段都 `Send`，整个类型就自动 `Send`；`Sync` 同理。所有基本类型（`i32`、`bool`、`str`）都 `Send + Sync`。这正是 auto trait 的妙处：你**不需要手写**任何东西，编译器替你做完了推理。代价是你得理解它的规则——比如 `Rc<T>` 内部藏了个非原子的引用计数（不 `Send` 也不 `Sync`），于是任何包含 `Rc` 字段的结构体都自动丧失跨线程资格；你想把它送进 `thread::spawn` 时，编译器会用一句冷冰冰的 "`Rc` cannot be sent between threads safely" 把你挡住。

那怎么**剔除**一个不该跨线程的类型？塞一个"不是 `Send`/`Sync`"的字段进去。常用 `PhantomData`：

```rust
use std::marker::PhantomData;
struct X {
    handle: i32,
    _not_sync: PhantomData<Cell<()>>,   // 零大小，但编译器把它当 Cell<()>
}
// handle 单独会让 X 既 Send 又 Sync；但加了 PhantomData<Cell<()>>，
// 而 Cell<()> 不 Sync，于是 X 也不 Sync（仍 Send）。
```

裸指针 `*const T` / `*mut T` 既不 `Send` 也不 `Sync`——编译器对它们一无所知。

> **为什么 `Rc` 不是 `Send`？** 这正是原子要登场的理由。`Rc` 的引用计数是个**普通整数**，不是原子的。两个线程各 clone、各 drop 一个 `Rc`，计数会像我们马上要见的"坏计数器"一样丢失更新——可能提前 free、造成 use-after-free。而 `Arc`（**A**tomic `Rc`）把那个计数换成原子计数，于是它就成了 `Send + Sync`。**原子的全部意义，就是让"本该被共享的状态"获得跨线程的资格。** 后面 M4 我们会亲手造一个 `Arc`，你会亲眼看到这一步。

> **一个能受用整本书的判断框架**：看到一个类型，先问"它要跨线程吗？"——要整包搬，看 `Send`；要共享引用，看 `Sync`（= `&T: Send`）。再问"它内部有没有没保护好的共享状态？"——有，就不合格。后面每个自建原语的设计，都在回答这两个问题；每次写 `unsafe impl Sync`，都是在说"我论证过，在某某前提下它是合格的"。

地基打好了，可以见主角了。

---

## 原子：一条不可分割的指令

**atomic（原子）** 这个词来自希腊语 ἄτομος——**不可分割**。一条原子操作：要么完整发生，要么根本没发生，任何线程都不会看到"半截"。因为不可分割，两条原子操作只可能"一条在另一条之前"或"之后"，绝不会交错——这就避开了数据竞争。

想象一台银行取号机：你按一下按钮，它**既要打印号条、又要让屏幕上的"当前号码"加一**。如果这两步之间有人插队，就会出现"两张 42 号"的灾难。取号机必须保证"按下按钮 → 出条 + 加一"是一个不可分割的动作——这就是原子的本意。原子类型就是这种"取号机版的整数"。

在 Rust 里，原子类型住在 `std::sync::atomic`，名字都以 `Atomic` 开头：`AtomicI32`、`AtomicUsize`、`AtomicBool`、`AtomicPtr<T>`……它们都是**内部可变**的——你能通过 `&AtomicU8`（共享引用）去改值。和 `Cell` 不同的是，它们能跨线程用（因为底层的硬件指令保证了原子性，M7 详讲）。

每个原子操作都吃一个 `Ordering` 参数。这一章前 5 节我们**全部用 `Ordering::Relaxed`**，先别管别的。`Relaxed` 只保证一件事：**对同一个原子变量的所有操作，大家看到的修改顺序是一致的（total modification order）**；但它**不承诺**不同变量之间的先后顺序，也**不承诺**这条操作前后的其它读写能不能被重排。本章前 5 节都是"单变量就够"的场景，所以 `Relaxed` 足矣。M1.6 会亲手打破这个够用——那正是全章最难、也最重要的一刻。

下面我们跟着原书的经典例子，一个个把原子操作逼出来。

---

## M1.1 停止位：第一个 AtomicBool

【敌人】一个后台线程在死循环里干活，用户敲 `stop` 时它该停下。最朴素的办法是共享一个 `&mut bool`——但两个线程同时一读一写同一个 `bool` 是数据竞争，会撕裂、会被优化掉。

【武器】`AtomicBool`：读和写都是原子的，绝不会看到半截值。

```rust
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
static STOP: AtomicBool = AtomicBool::new(false);

let bg = thread::spawn(|| {
    while !STOP.load(Relaxed) {        // 每轮开头瞄一眼
        some_work();
    }
});
// …用户交互…
STOP.store(true, Relaxed);             // 叫停
bg.join().unwrap();
```

就这么几行。后台线程每轮检查标志，主线程想停就把它写成 `true`。我们把它做成了 `forge_core::atomics::StopFlag`（`tests/m1_01_stop_flag.rs`）。**只要后台线程会定期检查标志，这个方案就完美**；如果它卡在 `some_work()` 里很久，停止就会有延迟——这是它的局限。

【为什么这里 `Relaxed` 就够】这个标志位**不承载任何需要同步的其它数据**——我们只是想知道"该停了吗"。没有"先写数据、再立标志"的发布关系，所以不需要任何排序承诺。`Relaxed` 只保证"这次读/写是原子的"，对一个纯标志位这就是全部。

**请你现在记住这个判断**："这个原子操作要不要发布别的数据？"——M1.6 会用同一个问题打你脸，到那时答案是"要"，于是 `Relaxed` 就不够了。

---

## M1.2 进度上报：单线程 `store`

后台线程处理 100 个任务，主线程每秒报一次进度：

```rust
let num_done = AtomicUsize::new(0);
thread::scope(|s| {
    s.spawn(|| {
        for i in 0..100 {
            process_item(i);
            num_done.store(i + 1, Relaxed);   // 写进度
        }
    });
    loop {
        let n = num_done.load(Relaxed);       // 读进度
        if n == 100 { break; }
        println!("Working.. {n}/100 done");
        thread::sleep(Duration::from_secs(1));
    }
});
```

这里用 `thread::scope`（M2 详讲）——它能在作用域结束时自动 join，还能让线程借用局部变量。

> 小升级：最后一条进度可能要等满一秒才被主线程看到。把 `thread::sleep` 换成 `thread::park_timeout`，并在后台线程每次 `store` 后 `main_thread.unpark()` 唤醒主线程，就能做到"一有新进度立刻报"。这就是 M2 要讲的"线程停靠（parking）"的雏形。

---

## M1.3 故意写错的多线程计数：丢失更新

把工作**分给 4 个线程**各做 25 个——现在 `store` 会**互相覆盖**进度。一个天真的写法是：

```rust
// 错！这是丢失更新的教科书示例
let cur = num_done.load(Relaxed);
num_done.store(cur + 1, Relaxed);
```

为什么错？因为 `load → 算 +1 → store` 是**三条独立指令**，中间可以被人插队。我们**逐拍手算**给你看为什么 8 线程各跑 10 万次、结果会远小于 80 万：

```
初始 num_done = 0
时刻 t1:  线程A load → 看到 0
时刻 t2:  线程B load → 看到 0        ← B 也看到 0，因为 A 还没 store
时刻 t3:  线程A 算出 1，store(1)
时刻 t4:  线程B 算出 1，store(1)      ← B 把 A 的更新覆盖了
最终：本该是 2，实际是 1。一次更新丢了。
```

两个线程各加一次，最终只加了 1。这就是**丢失更新（lost update）**。8 线程各 10 万次、`store` 版会得到几十万；运气差点甚至可能更糟（因为 `Relaxed` 不阻止编译器把循环合并、把 `load` 提到循环外——它甚至可能得到 1）。

【武器】我们需要"在当前值上加 1"这个动作是原子的——这就是 **fetch-and-modify** 家族：

```rust
let a = AtomicI32::new(100);
let b = a.fetch_add(23, Relaxed);   // 加 23，返回旧值
let c = a.load(Relaxed);
assert_eq!(b, 100);   // 旧值
assert_eq!(c, 123);   // 新值
```

`fetch_add` 在一条不可分割的指令里完成"读旧值、加、写回"，并**返回旧值**。同族还有 `fetch_sub / fetch_or / fetch_and / fetch_xor / fetch_max / fetch_min / swap`。它们的形状**完全一样**——"原子读旧值、算新值、写回、返回旧值"，唯一区别是中间那个二元运算。学会 `fetch_add`，全家都会。

> ⚠️ 一个坑：`fetch_add` / `fetch_sub` 在溢出时**回绕**（wrapping），不像普通整数在 debug 下会 panic。想加溢出检查，得用后面的 CAS。

把进度上报改成 4 线程：

```rust
let num_done = &AtomicUsize::new(0);
thread::scope(|s| {
    for t in 0..4 {
        s.spawn(move || {
            for i in 0..25 {
                process_item(t * 25 + i);
                num_done.fetch_add(1, Relaxed);   // 原子加，绝不丢
            }
        });
    }
    // …主线程上报，同前…
});
```

我们在 `tests/m1_03_counter_multithread.rs` 里留了一个 `#[ignore]` 的 `broken_read_modify_write`——你可以亲眼看到它怎么失败。`forge_core::atomics::Counter` 是正确版本。

> 教训（请刻进脑子）：**只要状态会被多线程"读-改-写"，就必须用原子的 `fetch_*` 或 CAS 循环，绝不能拆成 load→算→store。**

再升级一下：除了 `num_done`，再加 `total_time`（`fetch_add`）和 `max_time`（`fetch_max`），就能报平均耗时和峰值。注意三个原子是**分别更新**的，主线程可能读到"已加 num_done、还没加 total_time"的中间态，平均值会短暂不准——但因为只是给人看的统计，这点误差无所谓。**这正好说明 `Relaxed` 的边界：单变量一致，跨变量无序。** 如果要严格一致，就把三个数塞进一个 `Mutex`（M2）一起更新。

---

## M1.4 ID 分配器与 `fetch_update`（CAS）

【敌人】现在用到 `fetch_add` 的返回值了：每调一次 `allocate_new_id` 要返回一个**唯一**的 ID。

```rust
fn allocate_new_id() -> u32 {
    static NEXT_ID: AtomicU32 = AtomicU32::new(0);
    NEXT_ID.fetch_add(1, Relaxed)   // 返回旧值，所以第一个 ID 是 0
}
```

第一次调用得 0，第二次 1……唯一的麻烦是**溢出回绕**：第 4294967296 次调用会回到 0。如果"唯一性"关乎内存安全，这就不可接受。

原书给了三种解法（都值得想一遍）：
1. **溢出就 `process::abort()`**：标准库 `Arc::clone` 的计数溢出就是这么干的。
2. **溢出就 `fetch_sub` 回退再 panic**：标准库 `thread::scope` 的线程计数这么干。短暂超限被"活跃线程数"这个上限封顶。
3. **根本不让加法发生**：这才是真正正确的——但需要 CAS。

### compare-and-exchange（CAS）：最灵活的原子操作

CAS 的语义："**如果此刻值等于 `expected`，就把它改成 `new`**，并告诉我到底改没改成。" 它等价于（但全部在一条原子指令里完成）：

```rust
// 伪代码：load、比较、store 三步合一，不可分割
let v = self.load();
if v == expected { self.store(new); Ok(v) }
else { Err(v) }
```

`compare_exchange` 的签名有两个 ordering 参数：`compare_exchange(expected, new, success_order, failure_order)`。**为什么失败也要 ordering？** 因为 CAS 失败时它仍然做了一次 load——这次 load 可能被重排，也可能需要同步。M1.6 的 `LazyBox` 就是个失败序也要 `Acquire` 的例子。在你只想"失败就重试"的纯计数场景，失败序用 `Relaxed` 即可；但只要失败时你要读别人发布的数据，就必须像成功那样认真选 ordering。这是初学者最容易写错的地方。

把它放进循环重试，就能**实现任何**原子读-改-写。用 CAS 重写"自增 1"：

```rust
fn increment(a: &AtomicU32) {
    let mut current = a.load(Relaxed);
    loop {
        let new = current + 1;
        match a.compare_exchange(current, new, Relaxed, Relaxed) {
            Ok(_) => return,              // 没人抢，成功
            Err(v) => current = v,        // 有人抢先了，拿真实值重来
        }
    }
}
```

我们**逐拍手算**一下这个循环在两个线程并发时的行为，让你看清"乐观锁"为什么对：

```
初始 a = 0
线程 A: load → current = 0；算出 new = 1
线程 B: load → current = 0；算出 new = 1
时刻 t1:  A 执行 compare_exchange(expected=0, new=1)
          → 此时 a 仍是 0，匹配，store(1)，返回 Ok
          → A 返回，a 现在是 1
时刻 t2:  B 执行 compare_exchange(expected=0, new=1)
          → 此时 a 已经是 1，不匹配，返回 Err(1)，**不修改 a**
          → B 把 current 更新成 1，回到循环开头
时刻 t3:  B 重新算出 new = 2
时刻 t4:  B 再执行 compare_exchange(expected=1, new=2)
          → 此时 a 是 1，匹配，store(2)，返回 Ok
          → B 返回，a 现在是 2
─────────────────────────────────────────────────
最终 a = 2，没有丢失更新。B 的"乐观假设"被打破了一次，重试一次就成功了。
```

**这就是"乐观锁"的全部**：乐观地假设没人和我抢，干完活再核对；失败就重新读、重算、再试。这套"读-算-CAS-失败重试"的循环，是所有原子 RMW 的通用底座——M5 的通道、M8 的无锁栈/队列，全是它的变体。注意它的复杂度上界：在高争用下，多个线程可能反复重试（"活锁"），最坏情况下退化为串行甚至更慢。这就是为什么争用激烈的计数器常用 `fetch_add`（永远不需要重试）而不是 CAS 循环。

⚠️ **ABA 问题**：如果值从 A→B→A，CAS 仍会成功（它只看值相等与否，不知道"中间变过"）。我们用一张逐拍图来理解它为什么致命。假设有个无锁栈用 CAS 实现 push/pop：线程 T1 想把节点 X 压栈，步骤是 load head → 算出 new = X(指向旧 head) → CAS(head, 旧head, X)。

```
时刻 t1:  T1 load head → 看到 head = A（栈顶是 A）
时刻 t2:  T2 把 A 弹出、把 B 压入、又把 A 压回（栈顶又是 A 了）
          → 内存中：head 经历了 A → B → A 的变化
          → 而且 A 这个节点可能已经被释放又被重新分配了（"野指针复活"）
时刻 t3:  T1 执行 CAS(head, expected=A, new=X)
          → head 此刻确实是 A，CAS 成功！
          → 但 T1 不知道 head 中间变过，它以为栈还是 t1 时的样子
          → T1 把 X 压进去，X.next 指向"A 现在的下一个"——可能已经错了
─────────────────────────────────────────────────
结果：栈结构被悄悄破坏。CAS 看到值相等就以为没变，但中间的 B→A 变化逃过了它的检查。
```

对纯计数（数值只是数值）ABA 无害；但对**原子指针**算法它可能致命——节点可能被 free 后又重新分配，地址相同但内容全非。M8 的无锁栈会用"带版本号的指针"（tagged pointer）或 hazard pointer 来对付它。

用 CAS 写"绝不溢出"的 ID 分配——加之前先检查上限：

```rust
fn allocate_new_id() -> u32 {
    static NEXT_ID: AtomicU32 = AtomicU32::new(0);
    let mut id = NEXT_ID.load(Relaxed);
    loop {
        assert!(id < 1000, "too many IDs!");
        match NEXT_ID.compare_exchange_weak(id, id + 1, Relaxed, Relaxed) {
            Ok(_) => return id,
            Err(v) => id = v,
        }
    }
}
```

我们把它做成了 `IdAllocator::next_id_capped`（`tests/m1_04_id_allocator.rs`）：分配满上限后下一次返回 `None`，绝不回绕。

> `fetch_update` 是这套 CAS 循环的语法糖，一行搞定：`NEXT_ID.fetch_update(Relaxed, Relaxed, |n| n.checked_add(1)).expect("too many!")`。

---

## M1.5 CAS 延迟一次性初始化：happens-before 的雏形

【敌人】一段初始化代码在多线程下必须**恰好执行一次**，而且所有线程必须看到**同一个**结果。M1.2 那种"无害竞争"已经不够了——比如进程级随机密钥，每次运行都该不同、但同一进程内必须一致。

【武器】用 CAS：只有第一个线程能把 `0` 换成自己的密钥，其余线程的 CAS 失败，改用赢家那份：

```rust
fn get_key() -> u64 {
    static KEY: AtomicU64 = AtomicU64::new(0);
    let key = KEY.load(Relaxed);
    if key == 0 {
        let new_key = generate_random_key();
        match KEY.compare_exchange(0, new_key, Relaxed, Relaxed) {
            Ok(_) => new_key,   // 我赢了，用我的
            Err(k) => k,        // 别人赢了，用别人的
        }
    } else { key }
}
```

注意这里**不在循环里**，所以用强 `compare_exchange`（不要假失败）。我们把它做成了 `OnceFlag`（`tests/m1_05_once_flag.rs`）：100 个线程并发 `call_once`，闭包恰好执行一次。

> ⚠️ **但请记住 `OnceFlag` 的边界**：`Relaxed` 保证"恰好执行一次"（CAS 在 `done` 上的全局修改顺序决定了唯一的赢家），却**不保证**执行的副作用被其它线程看见。它是"恰好一次"的保证，**不是**"安全发布数据"的保证。这个裂缝，正是下一节要补上的——而且补的方式会直接导向我们要造的 `LazyBox`，以及全课程最重要的同构骨架。

---

## M1.6 ⭐ AtomicPtr + Relaxed 传 Box：全课程最重要的同构骨架

> 从这里开始，本章从"原子的舒适区"走进"内存序的雷区"。这一段我们会**逐拍手算**硬件行为，是后面 M3/M4/M5/M8 全部要复用的同构骨架。请你慢一点读。

【敌人】当初始化的产物是一个**堆上的大对象**，塞不进单个 `AtomicU64`——它可能是一个 `String`、一个 `Vec`、一个有几十个字段的结构体。"软件工程基本定理"出场——**加一层间接**：用 `AtomicPtr<T>` 存指针，`null` 当"未初始化"。生产者分配 `Box<T>`、把数据写到堆，再用 `ptr.store(...)` 发布指针；消费者 `ptr.load(...)` 拿到指针后读 `*ptr`。

最直觉（也最致命）的写法是全用 `Relaxed`：

```rust
// ⚠️ 这段代码是错的，但错在哪你看不出来——这正是它的可怕
fn get_data_broken() -> &'static Data {
    static PTR: AtomicPtr<Data> = AtomicPtr::new(std::ptr::null_mut());
    let p = PTR.load(Relaxed);              // [A]
    if !p.is_null() {
        return unsafe { &*p };              // [B]：读 *p
    }
    let b = Box::new(make_data());          // [C]：分配并初始化（多条 store）
    let raw = Box::into_raw(b);
    PTR.store(raw, Relaxed);                // [D]：发布指针
    unsafe { &*raw }
}
```

在单线程下它毫无问题。在 x86（强内存模型）下大概率也没问题——所以你会"测不出来"。但它在 ARM/POWER 这类**弱内存模型**上是错的，编译器和 CPU 都被允许把它玩坏。

### 逐拍手算：弱内存模型下 Relaxed 怎么把数据搞坏

我们把对象简化成最朴素的 `String`：堆上有一块内存，前 8 字节是 `len/cap`（叫"元数据"），后面是字符数据。`Box::new(String::from("hi"))` 实际做了**多条 store**：

- store #1：把 `len = 2` 写到堆的偏移 0
- store #2：把 `cap = 2` 写到堆的偏移 8
- store #3：把 `'h'` 写到堆的偏移 16
- store #4：把 `'i'` 写到堆的偏移 17
- store #5（[D]）：把"堆地址 0x1000"写到 `PTR` 这个原子变量里

线程 T1（生产者）按 1→2→3→4→5 的**程序顺序**写了 5 条 store。但**关键事实**：在弱内存模型的 CPU 上，`Relaxed` **不禁止这 5 条 store 被重排**——尤其因为它们打的是**不同的内存地址**，CPU 的 store buffer 完全可以让它们以任意顺序"漏"到主存。

现在 T2（消费者）登场，它循环 `PTR.load(Relaxed)` 等指针出现。我们列出**一个合法的、可能真实发生的**执行轨迹（这就是 loom 要枚举的那种交错）：

```
T1 的 store buffer 里有 5 条 store：[s1:s=len=2, s2:cap=2, s3:'h', s4:'i', s5:PTR=0x1000]
T1 的 CPU 因为分支预测/缓存命中等原因，决定让 s5 先离开 store buffer → 主存。
       ↓
时刻 t1:  主存 PTR = 0x1000  ← 指针发布了！
          堆 0x1000 处的内存仍是未初始化的垃圾（s1..s4 还在 store buffer 里）
       ↓
时刻 t2:  T2 的 PTR.load(Relaxed) 看到 0x1000（非空！）→ 它认为数据 ready 了
       ↓
时刻 t3:  T2 解引用 0x1000，读 len → 读到垃圾值（比如 0xDEADBEEF）
          T2 读字符数据 → 段错误，或读到一堆乱码
       ↓
时刻 t4:  s1..s4 终于从 T1 的 store buffer 漏到主存——但已经晚了
```

**这套交错在 ARM 上是完全合法的**——因为 `Relaxed` 没有任何"先写堆、再写 PTR"的排序承诺。CPU 只关心"单线程跑起来结果一样"，对 T1 单线程而言，先写 PTR 还是先写字符串没区别（它自己不会立刻去读）。但对 T2 而言，这就是灾难：**指针非空，但所指对象尚未初始化**。

这是数据竞争级别的灾难——`cargo +nightly miri test` 会直接报 UB；更可怕的是 x86 因为强模型（TSO）碰巧不容易触发，让你以为"我测过没事"，然后在某个 ARM 服务器或苹果 M 系列上偶发性崩溃。

### 用 loom 抓这个 bug

loom 是 Rust 的并发模型检查器，它把"所有可能的线程交错"挨个枚举一遍。一段模拟上面 bug 的 loom 测试大概长这样（简化版，省略 loom 的 `model` 装饰）：

```rust
// LOOM_MAX_PREEMPTIONS=3 cargo test --test m1_07_lazy_box loom
use loom::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

let ptr = Arc::new(AtomicPtr::<Data>::new(std::ptr::null_mut()));
let p2 = ptr.clone();

let h = loom::thread::spawn(move || {
    // 故意用 Relaxed（错的版本）
    let raw = Box::into_raw(Box::new(make_data()));
    p2.store(raw, Ordering::Relaxed);   // [D]
});

// 消费者
let p = ptr.load(Ordering::Relaxed);    // [A]
if !p.is_null() {
    let _data = unsafe { &*p };         // [B]：loom 这里不会真的 segv，
                                        // 但 miri 会立刻报"访问未初始化"
}
h.join().unwrap();
```

把 `LOOM_MAX_PREEMPTIONS` 调到 3，loom 会枚举到我们上面画的那种交错——T1 的 [D] 先于 [C] 可见，T2 的 [A] 拿到非空指针后立刻解引用 [B]。loom 自己不会 segv（它跑在模型内存上），但它能让你**看到**这次执行是合法的；miri 则会直接判 UB。

### Release / Acquire：跨线程拉起一根因果线

【武器】**Release 配 store，Acquire 配 load。** 规则只有一句，请你把它念三遍：

> **当一个 Acquire-load 读到了一个 Release-store 写入的值，一根 happens-before 关系就建立起来了：那次 store 及其之前的所有操作，happens-before 那次 load 及其之后的所有操作。**

（`AcqRel` 用于读-改-写，等于"load 用 Acquire、store 用 Release"。）

**类比（请记住它，它会陪你走完整个 Forge）**：Release 是 `git push`——你把本地所有 commit 推上去，那一次 push 隐含了"我之前所有的工作都已经在远端"。Acquire 是 `git pull`——只要你 pull 到了某次提交，你就一定能看到它之前的全部历史。`Relaxed` 则像是把改动随手塞进一个抽屉，没有任何承诺"什么时候、以什么顺序被别人看见"。

用 Release/Acquire 重写我们的 `get_data`：

```rust
use std::sync::atomic::Ordering::{Acquire, Release};
fn get_data_fixed() -> &'static Data {
    static PTR: AtomicPtr<Data> = AtomicPtr::new(std::ptr::null_mut());
    let p = PTR.load(Acquire);                  // [A']：订阅
    if !p.is_null() {
        return unsafe { &*p };                  // [B']：安全，能看到完整对象
    }
    let b = Box::new(make_data());              // [C']：分配并初始化
    let raw = Box::into_raw(b);
    PTR.store(raw, Release);                    // [D']：发布
    unsafe { &*raw }
}
```

### 逐拍手算：Release/Acquire 版为什么对

我们重画一遍刚才那个最阴险的交错，看 Release/Acquire 是怎么挡住它的：

```
T1 程序顺序：[C' 分配+初始化（写堆）] → [D' store PTR, Release]
T2 程序顺序：[A' load PTR, Acquire] → [B' 读 *p]
```

Release 的语义是："**D' 之前的所有写（包括 C' 对堆的所有 store）必须先于 D' 完成、对其它核可见**"。在硬件层面，T1 的 CPU 在执行 [D'] 这条 store 时，必须先把它的 store buffer **排空**（x86 用 `xchg`/`lock` 前缀；ARM 用 `stlr` 或 `dmb ish`）——这就把 s1..s4 强制"漏"到主存，然后才允许 s5（PTR 的发布）漏出去。

所以现在 T1 这边的可见顺序**保证**是 s1→s2→s3→s4→s5，没有重排空间了。

Acquire 的语义是："**A' 之后的所有读（包括 B' 对 `*p` 的读）必须真的在 A' 之后进行，并且能看到 A' 那一刻已经发布到主存的全部写**"。在硬件上，T2 的 CPU 在执行 [A'] 这条 load 时，会给它的 load buffer 上锁（x86 普通 load 就是 Acquire 语义，免费的；ARM 用 `ldar` 或 `dmb ish`），保证 [B'] 不会跑到 [A'] 之前。

现在 T2 拿到 PTR = 0x1000。因为 [D'] 是 Release，[A'] 是 Acquire，而且 [A'] 读到了 [D'] 写的值——**happens-before 接通**：[C']（以及它对堆的所有 store）happens-before [B']（对 `*p` 的读）。T2 读 `*p` 必然看到完整的 `'h','i',len=2,cap=2`，绝无可能读到垃圾。

**这就是 Release/Acquire 的全部魔法**：它不是让数据"传得更快"，而是**在两个线程之间拉起了一根因果线**——只要消费者 load 到了那次 store 写的值，生产者在 store 之前做的一切就必然对消费者可见。`Relaxed` 没有这根线，所以两个线程看到的"真实"可以是割裂的。

> **同构骨架**（请刻进脑子）：从现在起你看到任何"一个线程分配数据 → 用原子变量发布指针/标志 → 另一个线程读标志后访问数据"的代码，**第一反应必须是 Release/Acquire**。M3 的解锁/加锁、M4 的 `Arc::new`、M5 的通道 send/recv、M8 的无锁栈 push/pop——全是这个骨架的变体。能识别这个骨架，你就赢了并发的一半。

### 一个更现实的消息传递例子

把上面的"指针 → 数据"换成"标志 → 数据"，更贴近日常：

```rust
use std::sync::atomic::Ordering::{Acquire, Release};
static DATA: AtomicU64 = AtomicU64::new(0);
static READY: AtomicBool = AtomicBool::new(false);

thread::spawn(|| {
    DATA.store(123, Relaxed);
    READY.store(true, Release);   // "发布"：之前写 DATA 随此公开
});
while !READY.load(Acquire) {     // "订阅"：读到 true 后，能看见那次发布
    thread::sleep(Duration::from_millis(100));
    println!("waiting...");
}
println!("{}", DATA.load(Relaxed));   // 必定打印 123
```

主线程的 `Acquire` 读到了 `true`（即后台线程 `Release` 写的值），于是 happens-before 接通：后台线程 `Release` 之前的 `DATA.store(123)` 必然对主线程可见。**最后一行只有一种可能：123。** 如果全用 Relaxed，主线程可能看到 `READY` 变 true、却从 `DATA` 读到 0——这就是上一段那个"指针可见、数据未初始化"的整数版本。我们把它做成了 `tests/m1_06_release_acquire.rs`，跑 2000 轮，永远成立。

> `DATA` 自己用 `Relaxed` 就够了——同步全靠 `READY` 那对 Release/Acquire。**常见误区：不是每个原子都要 Acquire/Release，只有"拉因果线"的那一对需要。**

### 完整的 `LazyBox`：CAS 失败序为什么也要 Acquire

把上面的全部拼起来，得到 `forge_core::atomics::LazyBox`：

```rust
pub fn get(&self, init: impl FnOnce() -> T) -> &T {
    let mut p = self.ptr.load(Ordering::Acquire);    // [1]
    if p.is_null() {
        p = Box::into_raw(Box::new(init()));
        match self.ptr.compare_exchange(
            std::ptr::null_mut(), p,
            Ordering::Release,    // 成功：发布我们构造的对象
            Ordering::Acquire,    // 失败：读赢家发布的对象
        ) {
            Ok(_) => { /* 我们赢了 */ }
            Err(winner) => {
                unsafe { drop(Box::from_raw(p)); }   // 回收自己的
                p = winner;
            }
        }
    }
    unsafe { &*p }
}
```

**为什么 CAS 的失败序也是 `Acquire`？** 这是最容易漏的细节，但有了上面的同构骨架就好理解了：竞争失败时，我们要读**赢家写入的指针**——这同样是一次"通过指针发布数据"，赢家用了 `Release`，我们必须用 `Acquire` 才能看到赢家那份完整的数据。**失败序取决于"失败时你要不要看到别人发布的数据"**——要，就必须 Acquire；不要（比如 CAS 失败就重试、不读任何东西），才能用 Relaxed。

`tests/m1_07_lazy_box.rs` 用 16 线程并发 `get()`，最终所有人看到的都是赢家那份**同一个值**；`cargo +nightly miri test --test m1_07_lazy_box` 确认无 UB。

---

## M1.7 happens-before：因果关系的正式说法

M1.6 我们已经在用"happens-before"这个词了，现在给它一个精确的定义，因为后面所有章节都建立在它上面。

内存模型用 **happens-before（先于）关系**来定义顺序。它不谈指令、缓存、缓冲、时序，只定义"哪些事保证在另一些事之前发生"，其余一概不定。

基本规则：**同一线程内，前面的 happens-before 后面的。** 跨线程的 happens-before 只在少数几种情况发生：

- `thread::spawn`：父线程 spawn 之前的所有操作，happens-before 子线程的开头。
- `thread::join`：子线程的所有操作，happens-before 父线程 join 之后的代码。
- 解锁 / 加锁：解锁 happens-before 下次加锁（M2/M3）。
- **非 Relaxed 的原子操作**：Release-store 配 Acquire-load。Relaxed 自己**永远不会**产生跨线程 happens-before。

来看原书这个会让你怀疑人生的例子（`a`、`b` 在不同线程并发执行）：

```rust
static X: AtomicI32 = AtomicI32::new(0);
static Y: AtomicI32 = AtomicI32::new(0);

fn a() {
    X.store(10, Relaxed);   // [1]
    Y.store(20, Relaxed);   // [2]
}
fn b() {
    let y = Y.load(Relaxed);   // [3]
    let x = X.load(Relaxed);   // [4]
    println!("{x} {y}");
}
```

同线程顺序：[1]→[2]、[3]→[4]。但因为全是 Relaxed，**没有别的 happens-before**。所以输出可能是 `0 0`、`10 20`、`10 0`，都好理解。但**也可能是 `0 20`**——即便没有任何全局一致的执行顺序能产生它！因为 [3] 读 Y 时和 [2] 没有 happens-before，可能读到 20；[4] 读 X 时和 [1] 没有 happens-before，可能读到 0。

最反直觉的一点：**[3] 读到了 20，并不代表它和 [2] 之间建立了 happens-before**——即便 20 就是 [2] 写的。我们的"先后"直觉在这里失效了。一个更直觉（但不那么形式化）的说法是：**从 b 的视角看，[1] 和 [2] 就像反着发生了一样。**

这里值得停下来多想一秒，因为它是整章最反直觉的结论，也是后面所有陷阱的根源。你可能在想："我亲眼看到 [3] 读到了 20，而 20 这个值就是 [2] 写进去的——那 [2] 当然发生在 [3] 之前，怎么可能没有 happens-before？" 问题出在"之前"这个词上。在日常语言里，"X 在 Y 之前"是个时序概念——时间上更早。但在内存模型里，"happens-before"是个**因果**概念，不是时序概念：它说的是"X 的效果**保证**对 Y 可见"，而不是"X 在墙钟时间上更早"。

具体到上面：[3] 读到 20 是个**巧合**——硬件恰好在那个时刻把 [2] 的写传播到了 [3] 所在的核。但"巧合的可见性"不等于"保证的可见性"。内存模型不会把巧合升级成保证：因为 [3] 是 Relaxed load，模型只承诺它读到的是"全修改顺序上某个时刻的值"，不承诺"读到这个值就意味着能看到写这个值之前的一切"。**只有 Acquire 读到 Release 写的值，模型才把那次可见性"升级"成因果保证**——这就是 M1.6 那根因果线的形式化定义。

这就是为什么 M1.6 的同构骨架**必须**用 Release/Acquire——Relaxed 不能拉起那根因果线，于是"指针先到、数据后到"在模型层面是完全合法的。

**spawn 和 join 会建立 happens-before**：

```rust
static X: AtomicI32 = AtomicI32::new(0);
fn main() {
    X.store(1, Relaxed);
    let t = thread::spawn(f);   // spawn：之前的一切 happens-before 新线程
    X.store(2, Relaxed);
    t.join().unwrap();          // join：被 join 线程的一切 happens-before join 之后
    X.store(3, Relaxed);
}
fn f() {
    let x = X.load(Relaxed);
    assert!(x == 1 || x == 2);   // 不可能失败：不可能读到 0 或 3
}
```

spawn/join 替我们拉好了因果线，所以你平时用 scoped thread + join 的代码从不操心内存序。**麻烦只出在"线程之间既不 spawn 也不 join、却要传数据"的场景**——这时你得自己用 Release/Acquire 拉线。

### Relaxed 的保证：单变量的"全修改顺序"

Relaxed 虽不提供跨变量顺序，却保证**每个原子变量有自己的全修改顺序（total modification order）**——所有线程对该变量的修改，都认同同一个顺序。

```rust
static X: AtomicI32 = AtomicI32::new(0);
fn a() { X.fetch_add(5, Relaxed);  X.fetch_add(10, Relaxed); }
fn b() { let (a,b,c,d) = (X.load(R),X.load(R),X.load(R),X.load(R)); println!("{a} {b} {c} {d}"); }
```

只有一个线程改 X，修改顺序只能是 `0→5→15`。所以 `0 0 0 0`、`0 0 5 15`、`0 15 15 15` 都可能，但 `0 5 0 15` 或 `0 0 10 15` **不可能**。即便有多个线程改，所有线程也认同同一个顺序。

> 一个理论上的小怪物——**out-of-thin-air（凭空出现）**：两个线程互相 `Y.store(X.load())` / `X.store(Y.load())`，严格按模型，理论上可能两个都变成 37（循环因果）。这被普遍认为是**模型的 bug**，现实中不会发生。你可以 blissfully ignore。

---

## M1.8 `compare_exchange` vs `_weak`：ARM LL/SC 的假失败

【敌人】在 ARM 这类"弱内存"架构上，CAS 的底层指令是 **load-linked / store-conditional（LL/SC）**：先 linked-load 读取一个缓存行、再 store-conditional 写回，但 store-conditional **可能莫名其妙地失败**——只要中间这个缓存行被任何无关写触碰过（哪怕是别的核写了同一个缓存行上的另一个变量），它就拒绝执行。这是硬件设计上的妥协，用来避免 CAS 滥用缓存锁。

所以 Rust 标准库提供两个版本：

- `compare_exchange`（强）：失败**只**因为值真的不匹配。内部是个循环，会把假失败吃掉。
- `compare_exchange_weak`（弱）：还可能**假失败**（spurious failure）——值其实匹配却返回 `Err`。

经验法则：**CAS 循环里用 `_weak`**（假失败直接重试即可，省一条循环判断指令）；只重试一次或不重试的场景才用强版本。

`forge_core::atomics::cas_add` 是个范例——它用 `compare_exchange_weak` 的循环实现原子加法，返回**新值**：

```rust
pub fn cas_add(target: &AtomicU64, n: u64) -> u64 {
    let mut current = target.load(Ordering::Relaxed);
    loop {
        let new = current.wrapping_add(n);
        match target.compare_exchange_weak(
            current, new,
            Ordering::Relaxed, Ordering::Relaxed,
        ) {
            Ok(_old) => return new,
            Err(actual) => current = actual,   // 假失败/真失败都一样：拿真实值重试
        }
    }
}
```

注意 ARM 的 LL/SC 还有另一个后果——它就是 M1.10 要讲的"伪共享"会**让 CAS 假失败率飙升**的根因之一：如果你的原子变量和别人共享一个缓存行，那个别人的写就会让你的 LL/SC 频繁失败。

---

## M1.9 ⭐ SeqCst 存-载（Dekker）：为什么必须有最强内存序

> 这是本章第二个手算重心。和 M1.6 一样，我们会逐拍走多线程交错，看为什么 Release/Acquire 不够，必须请出 `SeqCst`。

【敌人】考虑这个"两个线程都想抢进临界区"的经典场景（Dekker 算法核心）：

```rust
// 线程 A:  flagA.store(true, ???);  seenA = flagB.load(???);  if !seenA { critical(); }
// 线程 B:  flagB.store(true, ???);  seenB = flagA.load(???);  if !seenB { critical(); }
```

两个线程都先"举手"（store true），再"看对方"（load）。**直觉**："不可能两个都看到对方是 false"——因为必有一个 store 先发生，另一个线程的 load 必看到 true。所以"两个线程都进临界区"应该不可能。

**但这个直觉是错的**——在 Release/Acquire（甚至 Relaxed）下，两个线程可以**都**看到 false，都进临界区。我们逐拍证明给你看。

### 逐拍手算：Acquire/Release 下 Dekker 为什么会失败

把 `???` 全填成 `AcqRel`（store 用 Release、load 用 Acquire）。线程 A 的代码序列是 `[A1: store(flagA, true, Release)] → [A2: load(flagB, Acquire)]`。线程 B 同理。

```
关键事实：A1 和 A2 在线程 A 内**是两条独立的指令**。
Release 只承诺"A1 之前的写 不被重排到 A1 之后"——
但 A1 本身和 A2 之间**没有任何排序约束**！A2 是个 Acquire load，
它只约束"A2 之后的读 不被重排到 A2 之前"，对 A1→A2 这个方向无能为力。

于是 CPU 完全可以执行 [A2, A1] 这个顺序——先 load flagB、再 store flagA。
对称地，B 那边也可以 [B2, B1]。
```

我们列出这个致命的交错：

```
时刻 t1:  A2 先执行：load flagB → false（B 还没举手）
时刻 t2:  B2 先执行：load flagA → false（A 还没举手）
时刻 t3:  A1 执行：store flagA = true
时刻 t4:  B1 执行：store flagB = true
─────────────────────────────────────────────────
结果：A 看到 B=false → A 进临界区
      B 看到 A=false → B 进临界区
      两个线程同时碰 S → 数据竞争 → UB
```

**这个交错在 ARM/POWER 上是完全合法的**——Acquire/Release 不提供"store→load"的排序，CPU 完全可以先执行 load。这叫 **Dekker 反例**，是 SeqCst 存在的直接理由。

### SeqCst：全局总顺序

`SeqCst`（顺序一致）包含 Acquire（对 load）和 Release（对 store）的全部保证，**再加一条**：所有 `SeqCst` 操作排成一个**全局总顺序**，所有线程都认同。这条总顺序与每个变量的全修改顺序一致。

把上面的 `???` 全换成 `SeqCst`，再走一遍那个致命交错：

```
SeqCst 承诺：所有 SeqCst 操作（A1, A2, B1, B2）排成一个全局总顺序，所有线程一致。
那么这个总顺序的第一个操作要么是 A1、要么是 B1（不可能是 load，因为初始值是 false）。
假设 A1 排第一：
   全局顺序：A1, …
   那么 B2（load flagA）在全局顺序中**排在 A1 之后**，
   所以 B2 必看到 flagA = true → B 看到 true → B 不进临界区。
对称地，如果 B1 排第一，A2 必看到 true，A 不进临界区。
─────────────────────────────────────────────────
结论：两个线程不可能都进临界区。Dekker 修好了。
```

**这就是 SeqCst 多出来的那一点点力量**：它阻止"store→load"被重排（在 SC 模型下，同一线程的 store 和 load 即便打不同地址也不能调换顺序——这正是 Dekker 需要的）。我们做成了 `dekker_store_load`（`tests/m1_09_seqcst.rs`）：用 `SeqCst` 跑，永远至少一个线程看到 true；换成 AcqRel/Relaxed 就有可能两个都看到 false。

### SeqCst 多花的代价

`SeqCst` 不是免费的，它的代价取决于硬件：

- **x86（强模型 TSO）**：普通的 `store` 不需要任何屏障就能达到 Release 语义，普通 `load` 免费 Acquire。但 SeqCst 的 store 要带 **`lock` 前缀**或用 **`xchg`** 指令——这会强制 CPU 排空 store buffer，比普通 store 贵几倍（几十纳秒级）。
- **ARM（弱模型）**：Acquire/Release 用 `ldar`/`stlr` 已经比较便宜（比 Relaxed 的 `ldr`/`str` 贵一点点），但 SeqCst 还要在 store 前后插 **`dmb ish`**（数据内存屏障）——这条指令强制 CPU 等所有之前的内存操作完成，开销在几十到上百纳秒。
- 对一个高并发的热点变量，把 Release 换成 SeqCst 可能直接让吞吐砍半。

我们**逐拍手算**这个代价在 x86 上的样子，让你对"砍半"有个具体感觉。假设有个原子 `flag`，线程 A 在死循环里 `flag.store(true, SeqCst)`：

```
普通 store（Relaxed/Release 在 x86 上）：
   CPU 指令：mov [flag], 1
   执行：直接把 1 写进 store buffer，立刻继续执行下一条指令。
   store buffer 异步地把写漏到 L1 cache，整个过程对 CPU 是"零等待"。
   → 大约 1-2 个时钟周期（几纳秒）

SeqCst store（xchg）：
   CPU 指令：xchg [flag], 1  （等价于 lock 前缀的 mov）
   执行：CPU 必须先**排空 store buffer**——把所有未决的写都漏到 L1 cache，
        确保它们对其它核可见，然后才允许这条 xchg 完成。
        这意味着 CPU 要等若干个 cache 周期（典型 20-40 纳秒）。
   → 大约 20-40 个时钟周期，慢了一个数量级。
```

ARM 的差距更悬殊：`dmb ish` 几乎是个全内存屏障，要等所有之前的 load 和 store 都完成，开销在 50-100 纳秒级别。这就是为什么"SeqCst 是好默认"是个危险的错觉——在高并发热点上，它会把你精心设计的并行算法活生生拖回串行。

**所以 SeqCst 的正确态度**：把它当**警告标志**。看到 `SeqCst`，要么这里真的需要 Dekker 风格的"store-then-load"全局顺序（极少数），要么作者偷懒没分析清楚。能用 Release/Acquire 解决就别上 SeqCst。

> **SeqCst 不能造出"release-load"或"acquire-store"**——这两个不存在。`SeqCst` 的 store 仍然是 Release 语义、load 仍然是 Acquire 语义，只是多了一个全局总顺序的承诺。如果你写 `a.store(1, SeqCst)` 然后期待它和 `b.load(SeqCst)` 形成"release-load"关系，那是没用的——`b.load` 不会因为 `a.store` 是 SeqCst 就看到 `a` 之前的写。

---

## M1.10 false sharing：缓存行 ping-pong

最后一个敌人，不是正确性，是性能。CPU 缓存以**缓存行**（通常 64 字节）为最小单位。两个**逻辑无关**的原子变量若挤在同一条缓存行里，一个核改它会让另一个核的**整条缓存行**失效（MESI 协议）——哪怕另一个核根本不读它。两个本可并行的线程被逼成串行，慢 3–5 倍。这叫**伪共享（false sharing）**。

### 逐拍手算：伪共享怎么让两个独立计数器互相拖慢

想象两个线程，T1 死命 `counterA.fetch_add(1)`，T2 死命 `counterB.fetch_add(1)`。两个计数器逻辑上完全独立，本该完美并行。但如果它们恰好落在**同一条 64 字节的缓存行**上（`AdjacentCounters` 那种布局）：

```
初始：缓存行 X 在 cache[L1-核0] 状态=S（共享）、cache[L1-核1] 状态=S
─────────────────────────────────────────────────
时刻 t1: 核0 执行 fetch_add(counterA)
   → 要改 counterA，必须把缓存行 X 升级到 M（修改）状态
   → MESI 协议：发一个 "invalidate" 给核1，核1 被迫把自己那份 X 标记为 I（无效）
   → 核0 现在独占 X，写完成
时刻 t2: 核1 执行 fetch_add(counterB)
   → 要改 counterB（它也在缓存行 X 上！），但核1 的 X 是 I 状态
   → 核1 必须重新从核0 那里把 X 拉过来（read-for-ownership）
   → 又发 invalidate 给核0，核0 的 X 变 I
   → 核1 独占，写完成
时刻 t3: 核0 又要写 counterA → 又要抢 X ……
─────────────────────────────────────────────────
结果：缓存行 X 在两个核之间像乒乓球一样来回弹，
      两个本该并行的线程被逼成完全串行，
      性能比单线程版本还差（因为多了 invalidate 的开销）。
```

**这就是伪共享**。修复办法：把每个"热点"变量用 `#[repr(align(64))]` 单独放到一条缓存行上，彼此互不打扰。

```rust
#[repr(align(64))]
pub struct CacheLine<T>(pub T);
// 紧挨着 → 伪共享；各自 CacheLine 包裹 → 各占一行
```

`forge_core::atomics` 里 `AdjacentCounters` 是坏例子、`PaddedCounters` 是好例子（`tests/m1_10_false_sharing.rs`），基准 `benches/m1_false_sharing.rs` 能直接量出 3–5 倍差距（`cargo bench -p forge-core`）。

**经验法则**：任何"常写、会被多核访问"的热点状态，都该独占一条缓存行——M8 的队列节点、M9 每个 worker 的统计都会用到。这是 Ch7"理解处理器"的预告，那里我们会深挖 x86 TSO vs ARM 弱序、store buffer、MESI 的全部细节。

---

## Fences：把"排序"从原子操作上剥离

`fence(Release)` / `fence(Acquire)` 让你把排序和具体原子操作分开。本质上：

- `a.store(1, Release)` ⟺ `fence(Release); a.store(1, Relaxed)`
- `a.load(Acquire)` ⟺ `a.load(Relaxed); fence(Acquire)`

代价是可能多一条指令。但好处是 **fence 不绑死单个变量**——一道 fence 能管多个变量。原书的例子：10 个线程各算各的、各 `READY[i].store(true, Release)`；主线程用 10 次 `Relaxed` load 检查，只要有任何一个 ready，就**一道** `fence(Acquire)` 把 10 个变量的同步一次性建立，再读数据。还可以**条件性**插 fence（指针非 null 时才插），省掉不必要的开销。M8 的 Chase-Lev 队列会用到。

`compiler_fence` 只挡编译器、不挡 CPU——几乎不够用，只在信号处理/中断这种"同核"场景才够。

---

## 几个必须打破的误解（原书的精华）

1. **"强内存序让改动'立刻'可见"——错。** 内存模型根本不谈时序，只谈顺序。强序不让你数据传得更快，反而可能更慢。
2. **"关掉优化就不用操心内存序"——错。** CPU 的重排还在。
3. **"用不重排指令的简单 CPU 就不用操心"——错。** 编译器仍会基于错误序做假设。
4. **"Relaxed 是免费的"——不一定。** 单线程几乎免费；但多线程争用同一变量时，缓存协调会显著变慢（M1.10、M7 详讲）。
5. **"SeqCst 是好默认、永远正确"——错。** 算法本身可能就是错的；而且 SeqCst 是个"远得离谱的声明"，让代码更难审。把它当警告。
6. **"SeqCst 能造 release-load / acquire-store"——错。** 这两个不存在。

---

## Consume：一个"本该更便宜"的 Acquire（但没人实现）

顺着 M1.6 的同构骨架想：Acquire 防止"数据在指针加载前被访问"。可这有意义吗——指针还没加载，你怎么访问它指向的数据？理论上一种更弱的序就够了：**只同步"依赖于所读值的操作"**，这就是 `Consume`。好消息：在所有现代 CPU 上，Consume 和 Relaxed 是同一条指令（"免费"）。坏消息：**没有编译器真正实现 Consume**（"依赖"在优化下太难保持，编译器把 Consume 升级成 Acquire 了事）。所以 Rust 不暴露 `Ordering::Consume`。了解一下就好。

---

## L1–L5 缩放回顾

| 层 | 你能…… |
|---|---|
| **L1 入口** | 一句话说清原子（不可分割）、内存序（排序契约）、Send/Sync（跨线程资格）。 |
| **L2 直觉** | "取号机"（原子）、"git push/pull"（Release/Acquire）、"邮局把关"（Send/Sync）。 |
| **L3 操作** | 跟踪丢失更新、CAS 循环、消息传递、LazyBox 的每一步；手算 Dekker 反例。 |
| **L4 机制** | 解释重排为何合法（优化只对单线程负责）、happens-before 如何接通、为何 CAS 失败序也要 Acquire、SeqCst 的全局总顺序。 |
| **L5 掌控** | 对任意新场景推出正确 ordering；知道每种 ordering 的硬件代价（xchg/dmb ish）；用 loom/miri 验证；识别"指针发布"同构骨架。 |

## 自检（teaching-method）

- [x] 先有 I/O 锚点和敌人（数据竞争 = UB）再谈机制。
- [x] 心智模型能成像（取号机 / git push-pull / Send-Sync 把关）。
- [x] 把陌生映射到已知（CAS=乐观锁；fetch_* 同构；Arc="原子的 Rc"）。
- [x] 识别同构（停止位=就绪位=OnceFlag=LazyBox 指针发布=…）。
- [x] **M1.6 和 M1.9 两个手算例子实质展开**（逐拍画 store buffer / 全局顺序）。
- [x] 变速：ordering/Send-Sync/数据竞争厚写，StopFlag 薄写。
- [x] 费曼测试：你能向同事讲清"为什么 M1.6 那段用 Release/Acquire 就修好了"吗？讲清"为什么 Dekker 必须 SeqCst"吗？

## 动手清单（对应测试）

`tests/m1_01_stop_flag` · `m1_02_counter` · `m1_03_counter_multithread`（含 `#[ignore]` 的坏计数器，跑 `-- --ignored` 看）· `m1_04_id_allocator` · `m1_05_once_flag` · `m1_06_release_acquire` · `m1_07_lazy_box`（miri 干净；loom 模型在 `LOOM_MAX_PREEMPTIONS=3` 下能枚举出 Relaxed 版的 bug）· `m1_08_cas_weak` · `m1_09_seqcst` · `m1_10_false_sharing` + `benches/m1_false_sharing`。

---

下一站 → [M2 共享与加锁（std）](./M2-sharing-and-locking.md)：当状态复杂到一个原子表达不了，就请出 `Mutex`/`RwLock`/`Condvar`，以及线程等待的两种姿势（park 与条件变量）。我们会用它们搭出"worker 等工作"的循环——那是未来调度器的种子。
