# M2 — 共享与加锁:标准库原语,以及"工作者等工作"的循环

> 模块:`forge-sync::std_locks` | 测试:`crates/forge-sync/tests/m2_*.rs`
> 跑:`cargo test -p forge-sync`

## ENEMY:一个数装不下,两个线程要抢

M1 我们学会了用**原子**让多个线程安全地碰一个标志、一个指针、一个计数器。一句话复盘:`AtomicBool`/`AtomicUsize` 让"读-改-写"这条本该被打断的操作变成不可分割的一步,所以两个线程同时 `fetch_add` 不会丢更新。

可是现实里的状态不是一个 `usize` 装得下的。

想象一个 Web 服务器在维护"在线用户列表":那是一个 `Vec<User>`,有 `push`、有 `iter`、有按 id 删一个用户的逻辑。它有几十个字段、几百行不变量(比如"每个 user.id 不重复")。你**没法**把它塞进一个 `AtomicUsize`。你**也没法**用 M1 那招"compare_exchange 自己重试"——因为修改要好几条指令,中间状态是脏的(比如刚 `swap` 掉一个用户、还没修完索引)。

更糟的是,有时候你根本**不知道要等多久**。一个 worker 线程该处理任务,可任务队列是空的。它该干什么?

- 选项 A:用一个原子标志 `has_task: AtomicBool`,空了就 `while !has_task.load(Relaxed) {}`。这叫**忙等**(busy-wait)。CPU 一秒几十亿次地在转圈,把隔壁真正该跑的线程挤掉。我们立刻能想到:这不行。
- 选项 B:**睡过去,等别人把我叫醒**。这就是 `thread::park`、`Condvar` 要解决的问题。可"睡"和"叫醒"之间会出一种**致命的竞态**——叫醒发生在你"决定要睡"之后、"真正进内核睡"之前。通知就这么飘走了,你睡死了,任务永远不处理。这个 bug 我们**逐拍手算**给你看,然后告诉你标准库怎么把它堵上。

M2 这一章的终点,是一个能跑的小调度器种子——`TaskQueue`:4 个 worker 睡在条件变量上,生产者 `push` 一个任务就 `notify_one` 唤醒一个 worker。代码一共 30 行,但它**汇集了本章所有要点**:怎么让多个线程安全共享一个 `VecDeque`、怎么让等待者睡到条件成立、为什么"原子地解锁+睡"是命门。这个 30 行的种子,M9a 会把它变成一个工作窃取线程池。

我们不预先讲 API。每个工具都是**因为撞墙**被逼出来的。

---

## ANCHOR:卫生间门锁——这一章的物理类比

在我们写任何代码之前,先在脑子里建一个画面,这个画面会贯穿整章:

> 一家公司的办公室只有一个卫生间。门上有锁,锁有两种状态:**没人(开)** 和 **有人(锁着,里面有人)**。
>
> - 你想用卫生间,过去推门。门开着,你进去、把门锁上。别人再来,推不开,就**在门外站着等**。
> - 你用完出来,把门解锁。下一个等的人就能进去。
> - **所有人约定**:想用卫生间,必须先推门进去并锁上;用完必须解锁出来。**只要所有人都守这个约定,卫生间里永远最多一个人**,卫生纸不会被两个人同时扯。

这就是 **mutex**(mutual exclusion,互斥锁)。门 = mutex,推门进并锁上 = `lock()`,出来解锁 = `drop(guard)`,在门外等 = `lock()` 阻塞。

Rust 把这件事做得比 C++ 更聪明:**mutex 不只是门,mutex 还是卫生间本身**——你把"用户列表"这个数据**装进** `Mutex<Vec<User>>`,想碰数据就必须先 lock。这等于把"约定"焊进了类型系统:你想拿 `&mut Vec<User>`,唯一的办法就是先 `lock()`,编译器不给你别的路。C++ 的 `std::mutex` 是孤零零一扇门,卫生间在哪全靠程序员记,忘记锁就是 UB。Rust 这一步把"约定"变成"语法上唯一能做的事"。

但这扇门只解决"独占"。我们还要回答四个问题:

1. **门后面装的数据归谁所有?** 两个线程谁都不能比对方活得短(谁先退出,数据归谁 free?)。→ §3 `Arc`。
2. **门外排队的人,凭什么知道轮到自己?** → §6 `park`/`Condvar`。
3. **万一拿门钥匙的人在里面 panic(昏过去了),门外永远等下去?** → §5 锁中毒。
4. **多人只想"看一眼"数据(读),凭什么也要排独占队?** → §5 `RwLock`。

逐个逼出来。

---

## §1 线程:同进程内不隔离的执行流

操作系统隔离**进程**(process):进程 A 不能(在正常情况下)碰进程 B 的内存。但一个进程能**派生额外线程**(thread)——这些线程**同一个进程**,共享同一片内存。这正是并发的便利,也是并发 bug 的温床:一个线程刚把 `Vec` 的长度从 3 改成 4,还没写完第 4 个元素,另一个线程就读走了那个长度——它会以为有 4 个元素,然后读到一半垃圾。

最小可运行的程序:

```rust
use std::thread;

fn main() {
    thread::spawn(f); // 派生一个新线程跑 f
    thread::spawn(f); // 再派生一个
    println!("Hello from the main thread.");
}

fn f() {
    println!("Hello from another thread!");
    println!("This is my thread id: {:?}", thread::current().id());
}
```

跑几次,输出**会变**,甚至**会缺一行**。为什么?因为 `main` 一返回,整个程序就退出,操作系统把所有线程一起干掉——管你 f 跑到一半没跑完。`ThreadId` 唯一(每次 `spawn` 都不同),但不保证连续,你能拿它做的只有"复制一份、比比是否相等"。

要让子线程跑完再退,得 **join**(等它):

```rust
let t1 = thread::spawn(f);
let t2 = thread::spawn(f);
t1.join().unwrap(); // 阻塞到 t1 结束;若 t1 panic,这里返回 Err
t2.join().unwrap();
```

`join()` 返回 `thread::Result<T>`:`Ok(v)` 是闭包返回值,`Err(e)` 是 panic 信息。`.unwrap()` 把 panic 往外抛。

几个细节,顺便说清楚(后面要用):

- **`println!` 自带锁**。它内部用 `Stdout::lock()` 保证一条 `println!` 的输出**不会被另一条 `println!` 的字符切碎**。如果没有这个锁,你会看到 `Hello fromHello from another threa...` 这种交错。但这个锁**只在单条 `println!` 内有效**——两条 `println!` 之间依然可能交错。
- **`spawn` 要求 `'static`**。线程可能活到程序结束,所以它捕获的东西必须能活到程序结束。捕获局部变量要 `move`(把所有权搬进去)。
- **`thread::Builder`** 能设栈大小和线程名(出现在 panic 信息和监控工具里)。`spawn` 只是 `Builder::new().spawn().unwrap()` 的简写。

### 一段真实历史:The Leakpocalypse

Rust 1.0 之前有个 `thread::scoped`,返回一个 `JoinGuard`——它在 drop 时自动 join。看似天衣无缝:guard 一离开作用域就 drop,线程就 join,所以线程不可能活过作用域,所以可以借用局部变量,不需要 `'static`。

可 1.0 发布前几天,人们意识到一件恐怖的事:**drop 不保证发生**。

用 `Rc` 造一个环(`a.next = b.clone(); b.next = a.clone();`),整个环的引用计数永远 ≥ 1,这些 `Rc` **永远不会 drop**。如果 `JoinGuard` 被卷进这种环(或用 `mem::forget` 直接扔掉),它就不 drop,线程就不 join——而线程借用的那个局部变量可能已经被释放了。线程还在跑、访问已释放的内存 → UB。

这就是 **"The Leakpocalypse"**(泄漏末日)。结论是一个让所有 Rust 设计者都难受的原则:

> **安全接口的正确性,绝不能建立在"对象一定会被 drop"这个假设上。**

泄漏**永远可能**(`mem::forget` 因此从 `unsafe` 升级为 `safe`,强调"忘掉是合法的")。旧 `thread::scoped` 被移除。直到 Rust 1.63,标准库才用**不依赖 Drop** 的新设计重新加回 `thread::scope`——它用闭包的**词法边界**强制 join:作用域闭包 return 之前,所有内部 spawn 的线程**已经被 join** 了,这是函数签名保证的,不靠任何析构。

这个故事我们会再撞见第二次:M5 自建通道时,如果你想说"发送端 drop 时自动关闭接收端",你必须**显式地、不依赖 Drop** 地关闭——否则一个 `mem::forget` 就能打穿你。

---

## §2 scoped threads:让线程借用局部变量

`thread::spawn` 要求 `'static`,因为它不知道线程什么时候结束。但**如果我们能保证**线程绝不活过某个作用域,它就可以安全地借用局部变量。`std::thread::scope` 就是这个保证:

```rust
let numbers = vec![1, 2, 3]; // 局部变量
thread::scope(|s| {
    s.spawn(|| println!("len: {}", numbers.len()));     // 借用 numbers(只读)
    s.spawn(|| for n in &numbers { println!("{n}"); }); // 也借用(只读)
});
// 作用域到此:scope 保证两个线程已 join,所以 numbers 可以安全释放
```

`scope` 的契约:**作用域闭包 return 之前,所有 spawn 的线程已 join**。所以没有 `'static` 约束,可以引用任何比作用域长寿的东西(局部变量、函数参数,都行)。

**借用规则照常工作**:两个线程都只读 `numbers`,编译器允许。如果你想让一个线程改:

```rust
let mut numbers = vec![1, 2, 3];
thread::scope(|s| {
    s.spawn(|| numbers.push(1));  // 第一个可变借用
    s.spawn(|| numbers.push(2));  // 第二个可变借用 → 编译错误!
});
```

编译器拦下你:`cannot borrow numbers as mutable more than once at a time`。这正是 M1 讲的数据竞争防护——**借用规则天然阻止"两个线程同时改同一份数据"**。但这也意味着:**想多线程改同一个 `Vec`,你必须借助某个"内部可变性"工具**(下一节开始逼出 `Arc`,§5 逼出 `Mutex`)。

测试 `m2_01_scoped_threads` 把这个写成了可运行示例:两个线程各算 `numbers` 的长度/求和,加到一个 `AtomicUsize` 上。

---

## §3 共享所有权:谁都不比谁活得短

问题来了。两个 `thread::spawn`(不是 scope)的线程,谁也不保证比对方先结束。它们都想用一个 `Vec<User>`——这个 `Vec` 该归谁所有?

- 如果归线程 1,线程 1 一退出就 free 了,线程 2 use-after-free。
- 如果归线程 2,同理。
- 谁都不能归。

三种解决,逐个升级:

### 3.1 static:整个程序"拥有"

```rust
static X: [i32; 3] = [1, 2, 3];
thread::spawn(|| dbg!(&X));
thread::spawn(|| dbg!(&X));
```

`static` 在程序启动前就存在、永不释放,任何线程都能借。但只能装**常量初始化**的东西——你没法在运行时构造一个 `static Vec<User>`(用户是动态来的)。

### 3.2 `Box::leak`:放弃所有权

```rust
let x: &'static [i32; 3] = Box::leak(Box::new([1, 2, 3]));
thread::spawn(move || dbg!(x));
thread::spawn(move || dbg!(x));
```

`Box::leak` 把堆上的 `[1,2,3]` 标记为"永不释放",换回一个 `&'static [i32;3]`。任何线程都能借。代价是**内存泄漏**——这次是故意的,但如果你在循环里这么干,程序迟早 OOM。

注意 `move` 看起来像"搬走所有权",其实引用是 `Copy`(`&'static` 是共享引用),`move` 只是把副本搬进闭包。原变量在外面也还能用。

### 3.3 引用计数:共享所有权

我们需要"最后一个人退出时关灯"的语义。这就是**引用计数**:在每个副本旁边记一个数字(有几个主人),clone 时 +1,drop 时 -1,降到 0 才 free。

`Rc`(reference counted)是单线程的引用计数。试试把它送到别的线程:

```rust
use std::rc::Rc;
let a = Rc::new(123);
thread::spawn(move || dbg!(a)); // 编译错误
```

编译器拦下:

```
error[E0277]: `Rc` cannot be sent between threads safely
```

为什么?`Rc` 的计数器是个**普通整数**。两个线程同时 clone——`count += 1` 是"读、加、写"三步,中间会被打断——会丢更新。**计数器算错,可能提前降到 0、提前 free,而另一个线程还持着指针读**——use-after-free。M1 我们讲过原子怎么解决"两个线程同时改一个数"——可是 `Rc` 没用原子,它就是为了**单线程**的廉价而设计的。

`Arc`(**A**tomic Rc)把那个计数换成**原子**计数,问题立刻解决:

```rust
use std::sync::Arc;
let a = Arc::new([1, 2, 3]); // 计数 = 1
let b = a.clone();            // 原子地 ++ 计数 = 2
thread::spawn(move || dbg!(a)); // 计数 = 2(各持一份,指向同一分配)
thread::spawn(move || dbg!(b)); // 计数 = 2
// 两个线程结束时各自 drop,计数 2→1→0,最后一个负责 free
```

> **这正是 M1 原子的意义回响**:原子让"本该被共享的状态"获得跨线程资格。`Arc` 的定义可以一句话说清——**"用原子计数替代普通计数的 `Rc`"**。M4 我们会亲手造一个 `Arc`,你会看到那个原子计数怎么写、用什么内存序。

#### 命名小技巧:shadow clone

`Arc` 每次 clone 都得起新名,代码会很乱。Rust 允许在新作用域里 **shadow** 同名变量:

```rust
let a = Arc::new([1, 2, 3]);
thread::spawn({
    let a = a.clone();   // 新作用域里 shadow
    move || dbg!(a)
});
dbg!(a); // 外面 a 还在(计数没被消耗)
```

每个线程内部都还能叫 `a`,代码清爽。

#### `Arc<T>` **不**给可变访问

`Arc<T>` 和 `&T` 一样,**只读**——因为值可能正被别的代码借。想 `a.sort()`?编译器:`cannot borrow data in Arc as mutable`。这是设计:多个主人不能你改你的、我改我的。想改 `Arc` 里的东西,得配合下一节的 `Mutex`/`RwLock`——把"想改"和"独占"绑定。

---

## §4 数据竞态:不是"出错",是"未定义行为"

在逼出 `Mutex` 之前,我们必须把"数据竞态"这个词钉死。

**数据竞态(data race)** 的精确定义:一个线程写、另一个线程同时读或写**同一块内存**,而且没有同步保证顺序。注意"同时"——单核时代你可以假装没有同时,但多核 CPU 上真的有两条核同时碰一个字节。

数据竞态在 Rust 里不是"运行时可能出错",是**未定义行为(undefined behavior, UB)**。这两件事天差地别:

- **运行时可能出错**:程序按规范跑,只是结果不对(比如除以零 panic)。编译器不会基于"它不会发生"做优化。
- **未定义行为**:编译器**可以假设它绝不发生**,并基于这个假设乱改你的程序。一旦你真的触发了,程序的行为变得**完全不可预测**——可能崩、可能给出离谱结果、可能"穿越时间"破坏前面的代码。

Mara Bos 的例子很直观:假设你 `unsafe` 地绕过借用检查,写出 `fn f(a: &i32, b: &mut i32)` 让 `a` 和 `b` 指向同一个 `i32`,然后:

```rust
let before = *a;
*b += 1;
let after = *a;
if before != after {
    x(); // 看起来"应该"被调用?
}
```

编译器看到 `a` 是共享引用,根据"无数据竞态"规则,它**假设** `*a` 在借用期间不变——所以 `before == after` 一定成立——所以 `if` 体可以**整个删掉**。你的代码就这么消失了。如果你 unsafe 写错了导致 `a` `b` 真的别名,触发 UB,编译器基于错误假设做的所有优化都会反过来咬你——可能 `x()` 永远不调,也可能更早的某段代码莫名其妙地崩。

**安全 Rust 不可能直接写出数据竞态**——借用规则拦住了。但安全 Rust **可以**通过"内部可变性"类型(`Cell`、`RefCell`、`Mutex`、`Arc`)间接地碰共享可变状态,这些类型的设计者用 `unsafe` 把关,保证不会数据竞态。下面几节我们会看到每种是怎么把关的。

### 4.1 UB 怎么"穿越时间"——一个具体的优化例子

为了让"UB 不可预测"这件事真正落地,我们把 Mara Bos 的例子再展开一拍。设想你 unsafe 写了:

```rust
let a = [123, 456, 789];
let index = 3; // 越界!a 只有 3 个元素
match index {
    0 => x(),
    1 => y(),
    _ => z(index),
}
let b = unsafe { a.get_unchecked(index) }; // 假设 index < 3
```

编译器看到 `get_unchecked`,被允许假设 `index < 3`。它回头推理:`match` 的第三支 `_ => z(index)` 只有 `index >= 2` 才会命中,结合 `index < 3` → 这一支只能 `index == 2` → 所以 `z` 只可能被 `z(2)` 调用 → 进一步优化 `z` 时可以把参数硬编码成 2。如果你实际跑 `index = 3`,编译器已经基于"index == 2"删了/改了 `z` 里某些代码,你跑的可能是**残缺版**的 `z`——结果完全不可预测。

更恐怖的是,这个优化发生在 `get_unchecked` **之前**的代码(`match` 和 `z`)。**UB 像时间旅行者一样,影响了它发生之前的代码**。这不是编译器的 bug,这是"基于错误前提的推理可以得出任意结论"的逻辑必然——garbage in, garbage out,只是 garbage 在时间上往前流。

这就是为什么 §1 的 Leakpocalypse 那段我们说"安全接口不能依赖 drop 一定发生"——任何 UB 都是这样的"时间旅行者",一个看似无害的假设被打破,可能让程序任意位置的代码崩溃。Rust 把"无数据竞态"作为铁律,正是为了堵住这个时间旅行者。

### 4.2 借用规则如何天然防数据竞态

回头看借用规则:"一个对象,要么有任意多个 `&T`,要么有唯一一个 `&mut T`,不能同时有"。这恰恰是数据竞态定义里"一个线程写、另一个线程同时读或写"的反面。

为什么?设想两个线程同时操作一个 `Vec`:

- 如果两个线程都只读——它们各持 `&Vec`,允许(多个共享借用)。没数据竞态。
- 如果一个线程要写——它要 `&mut Vec`。借用规则要求"没有别的借用",所以另一个线程此刻**不能**有 `&Vec` 或 `&mut Vec`。要么另一个线程的借用已经结束,要么根本没开始。**没法"同时"**——天然无竞态。

所以安全 Rust 里,两个线程同时 `&mut Vec` 是写不出来的——编译器在你 spawn 时就拦下(`move` 闭包只能搬走所有权,不能两个闭包同时持 `&mut`)。这就是 M1 的"无畏并发"在类型系统层面的兑现:**数据竞态在编译期就消失,不是运行时检查**。

代价是:**严格的借用规则让"多线程共享可变状态"变得几乎写不出来**——你需要内部可变性(§5)绕过它,这就是 `Mutex`/`RwLock`/`Arc` 存在的理由。

---

## §5 内部可变性:绕过"只读"的合法手段

借用规则说:`&T` 不能改东西。但有时候我们**需要**通过 `&T` 改——比如 `Arc` 内部的计数器,clone 时要 +1,可 `Arc::clone(&self)` 拿的是 `&self`。这就需要一个"合法绕过借用规则"的机制,叫**内部可变性**(interior mutability)。

> 借用规则不破,是**外延的规则**变了:`&T` 还是 `&T`,但某些类型承诺"我内部有机制保证安全,你可以通过 `&T` 改我的内容"。机制是什么,因类型而异。

### 5.1 `Cell<T>`:整存整取

最简单的内部可变性。`Cell<T>` 把 `T` 装进去,允许你通过 `&Cell<T>` **整体替换**它的内容(`set`)或**整体取出**(`get`,需 `T: Copy`)。它的安全保证是:**绝不给你 `&mut T`**,所以不可能"一边持着引用一边被改"。代价是只能整体换,不能拿引用进去改一个字段。

`Cell` 还有一个铁律:**只能单线程用**。下一节会看到为什么。

### 5.2 `RefCell<T>`:运行时借用检查

`Cell` 不能拿引用,有时候太憋屈。`RefCell<T>` 允许你在**运行时**借用——`borrow()` 给 `&T`,`borrow_mut()` 给 `&mut T`。它内部有个计数器记录"当前有几个共享借用、有没有独占借用"。如果你违规(比如已经 `borrow_mut` 了还试图再 `borrow_mut`),**运行时 panic**——不是 UB,是 panic。代价是运行时开销 + 只能单线程。

`Mutex` 和 `RwLock` 就是 `RefCell` 的**多线程版**——借用检查放到运行时,违规不是 panic 而是**阻塞等**(等别人放了你再上)。下一节正式展开。

### 5.3 `UnsafeCell<T>`:一切的根

所有"内部可变性"类型(`Cell`、`RefCell`、`Mutex`、`RwLock`、`AtomicXxx`)**底层都是 `UnsafeCell`**。它是个空壳:`UnsafeCell<T>` 内部就是个 `T`,但 `get()` 返回 `*mut T`(裸指针),只能在 `unsafe` 块里用。它告诉编译器:"这块内存可能通过 `&T` 被改,你别瞎假设。"

为什么需要它?回顾 §4:编译器假设"共享引用 `&T` 指向的值在借用期间不变",基于此做优化(比如把两次 `*a` 读合并成一次)。如果你绕过这个假设(`unsafe` 直接改了),编译器基于错误前提做的优化会反过来咬你。`UnsafeCell` 是个**标记**:它告诉编译器"这块内存除外,它可能通过 `&T` 被改,你别针对它做'值不变'的优化"。所以 `UnsafeCell` 本身不提供任何同步,它只让编译器**别瞎假设**——同步(锁、原子)由上层类型(`Mutex` 等)自己保证。

换句话说,`UnsafeCell` 解决的是"**编译器优化**层面的可见性"问题,**不**解决"多核 CPU 缓存一致性"问题(那是 M1 的内存序 + M3 的 CPU 真相)。两件事是独立的,缺一不可。

普通用户**不应该**直接用 `UnsafeCell`。它是给库作者造安全抽象用的。我们 M3 自建 SpinLock 时会正面撞上它——你会看到 SpinLock 内部就是 `AtomicBool`(锁状态) + `UnsafeCell<T>`(被保护的数据),前者保证"同一时刻只有一个线程进",后者保证"编译器别把我当只读"。两者合起来,才是安全的可变共享。

### 5.4 `Send` 和 `Sync`:谁能跨线程?

到这里必须把"谁能跨线程"讲清楚——因为下一节要解释为什么 `Cell`/`RefCell` 不能跨线程、而 `Mutex`/`RwLock` 可以。

Rust 用两个 auto trait 标记类型的线程安全:

- **`Send`**:`T` 是 `Send`,意味着 `T` 的所有权**可以搬到另一个线程**。例如 `Arc<i32>: Send`,`Rc<i32>: !Send`。
- **`Sync`**:`T` 是 `Sync`,意味着 `&T` **可以共享给另一个线程**(可以同时被多个线程持)。等价定义是 **`T: Sync ⟺ &T: Send`**。

为什么是这两个等价定义?想清楚:

- `T: Sync` 意思是"多个线程可以同时持 `&T`"。
- 一个 `&T` 跨线程传递,本质是把 `&T` 这个**引用**的所有权搬过去。`&T` 自己也是个值,它能搬过去当且仅当 `&T: Send`。
- 所以"`T` 允许 `&T` 跨线程共享" ⟺ "`&T` 是 `Send`" ⟺ "`T: Sync`"。

这是定义,不是定理——记住"`T: Sync ⟺ &T: Send`"这一行,后面遇到任何 Sync/Send 困惑都拿它推。

**`Cell` 和 `RefCell` 是 `Send` 但 `!Sync`**。为什么?

- 它们是 `Send`:把一个 `Cell<i32>` 整个 move 到另一个线程——只有那一个线程持,没有"同时",安全。
- 它们**不**是 `Sync`:多个线程同时持 `&Cell<i32>`——可以一个 `set`、一个 `get`,**没有任何同步**,纯数据竞态。所以禁止。

`Rc` 更严,既不 `Send` 也不 `Sync`:连搬过去都不行(计数不是原子)。

`Mutex`/`RwLock`/`AtomicXxx`/`Arc` 是 `Send + Sync`(对合适的 `T`)——它们**内部**有同步机制(锁、原子指令),保证不会有数据竞态,所以允许跨线程共享 `&Self`。

`Send`/`Sync` 都是 **auto trait**:你不需要手动 impl,编译器看到你的 struct 字段都 `Send` 就自动给你 `Send`;有字段不 `Send` 就不 `Send`。要**禁止**(比如自造类型不让别人跨线程用),加个 `PhantomData<Cell<()>>` 字段。要**强行允许**(你用 unsafe 自己保证了安全),写 `unsafe impl Sync for MyType {}`——`unsafe` 标记是"编译器查不了,我自己担保"。

`*const T` / `*mut T` 裸指针默认既不 `Send` 也不 `Sync`——编译器对它们一无所知。

### 5.5 三个常见误解,逐个打掉

学到这里,聪明的读者会冒出三个直觉错误。我们先认领它们,再一个个打破。

**误解一:"`Arc<T>` 既然能跨线程,那它就是 `Sync` 的"——错。**

`Arc<T>: Sync` 当且仅当 `T: Send + Sync`。设想 `Arc<Cell<i32>>`:`Arc` 的计数是原子的、`Arc` 自己能跨线程共享,但 `Cell` 是 `!Sync` 的(见 5.4)——两个线程同时拿 `&Arc<Cell<i32>>`,各 `clone` 一份 `Arc`、解引用拿到 `&Cell`,然后**同时 `set`**——纯数据竞态。所以编译器把 `Arc<Cell<i32>>` 标成 `!Sync`,你根本拿不到 `&Arc<Cell<i32>>` 跨线程。这正是 auto trait 的妙处:它**自动**顺着字段传染,"你装的东西不 Sync,你也就不 Sync"。

**误解二:"`Rc` 不能跨线程,那 `Rc<&'static T>` 总能吧,反正引用是 'static"——错。**

`Rc<T>: Send` 当且仅当 `T: Send`。`&'static T` 是 `Send`(只要 `T: Sync`),但 `Rc` 自己不 `Send`——它内部的非原子计数器是问题,跟它装的东西是否 'static 无关。auto trait 的逻辑是"看所有字段",`RcInner { strong: Cell<usize>, weak: Cell<usize>, value: T }`——`Cell` 不 `Sync`、计数会被并发改,所以 `Rc` 既不 `Send` 也不 `Sync`,跟 `T` 是什么无关。

**误解三:"既然 `Mutex<T>` 给独占访问,那我可以用 `Mutex<Cell<i32>>` 吗"——能,但没必要。**

`Cell` 只允许整存整取,`Mutex` 给独占 `&mut T`,两者一起用——你拿锁、然后只能 `set`/`get` 整个 `Cell`,不能 `+= 1`(因为 `Cell` 不给 `&mut`)。等价于 `Mutex<i32>` 但更难用。教训:**内部可变性是层层叠加的**,有时叠两层是有意义的(`Mutex<Vec<User>>` 是合理的——`Vec` 给你 `&mut Vec`,你能 `push`、`iter` 全套操作),有时叠两层是退化(`Mutex<Cell<T>>`)。判断标准:外层工具能不能给你想要的访问粒度?能,就别叠;不能(比如 `Arc` 只读、想改),再叠一层。

### 5.6 整理:单线程 vs 多线程,只读 vs 可变

把本章前半的工具列成一张矩阵:

| | 单线程 | 多线程 |
|---|---|---|
| **只读共享** | `&T` | `&T`(需 `T: Sync`) |
| **可变,整体换** | `Cell<T>` | `AtomicXxx`(M1) |
| **可变,持引用改** | `RefCell<T>` | `Mutex<T>` / `RwLock<T>` |
| **共享所有权** | `Rc<T>` | `Arc<T>` |

每一列从上到下能力递增、开销递增;每一行从左到右同步开销递增。选型的依据:**先看你需要什么粒度的访问**(整体换 / 持引用改),再看**是否跨线程**。`Cell` 和原子是"整存整取"档;`RefCell` 和锁是"持引用改"档。**别跨档选**——比如需要 `&mut Vec` 你却用了原子,那你每次操作都得把整个 `Vec` swap 出来改完再 swap 回去,灾难。

这张矩阵是本章最重要的"地图"。后面 M3 自建 SpinLock、M4 自建 Arc、M7 自建 Mutex/RwLock,都是把矩阵右列的格子**自己造一遍**——你会看到每个格子背后的 `unsafe` 是怎么把关的。

---

## §6 `Mutex<T>`:把数据装进锁里

终于到正题。

回到 §2 的悬而未决:两个线程想同时改一个 `Vec`,借用规则不让。怎么破?**用 `Mutex`**。

`Mutex<T>` 是这样设计的:它**装住**一个 `T`,你想碰 `T` 必须 `lock()`。`lock()` 返回一个 `MutexGuard<T>`——guard 就是"我持有了锁"的证据,它通过 `DerefMut` 让你拿到 `&mut T`。`drop(guard)` 时自动解锁。

最小例子——10 个线程各把一个数加 100 次:

```rust
use std::sync::Mutex;
use std::thread;

fn main() {
    let n = Mutex::new(0);
    thread::scope(|s| {
        for _ in 0..10 {
            s.spawn(|| {
                let mut guard = n.lock().unwrap(); // 加锁
                for _ in 0..100 {
                    *guard += 1;                    // 通过 guard 改 n
                }
                // guard 在此 drop → 解锁
            });
        }
    });
    assert_eq!(n.into_inner().unwrap(), 1000); // into_inner 拆掉 Mutex,拿回内部的 0
}
```

漂亮的地方:每个线程的 100 次自增**合起来**变成了一次不可分割的"原子"操作——因为别的线程只能在锁未锁时看到 `n`,而那段时间没人改它。**对外可见的 `n` 永远是 0, 100, 200, ... 的整数倍,中间状态(比如自增到一半)被锁挡住,看不见。**

这就是 mutex 真正的意义:它不只是"防止两个线程同时改",它让**一整段包含多条指令的操作**对别的线程看起来像**一步**。它把"操作的边界"扩大到了你 `lock()` 到 `drop(guard)` 之间的一切。

### 6.1 保持锁定时间尽可能短

锁是独占的。你拿得久,别人就等得久。原书这个对比非常直观——加上 `sleep`:

```rust
// 版本 A:持锁状态 sleep —— 10 线程串行,程序跑 ~10 秒
s.spawn(|| {
    let mut guard = n.lock().unwrap();
    for _ in 0..100 { *guard += 1; }
    thread::sleep(Duration::from_secs(1)); // 持锁睡!
});

// 版本 B:先解锁再 sleep —— 10 线程并行,程序跑 ~1 秒
s.spawn(|| {
    let mut guard = n.lock().unwrap();
    for _ in 0..100 { *guard += 1; }
    drop(guard);                             // 先解锁!
    thread::sleep(Duration::from_secs(1));   // 不持锁睡
});
```

差别巨大。"持锁太久"会把并行性完全抵消——10 个核的机器退化成 1 核,白买。

### 6.2 锁中毒:持锁线程死了怎么办

每个 `.lock().unwrap()` 的 `unwrap` 在干什么?在处理**中毒(poisoning)**。

设想:一个线程拿到锁,改了 50 次,然后 panic 了——锁会被自动释放(panic 的 unwind 机制会 drop guard,触发 unlock),但**数据处于不一致状态**(只改了 50 次,不是预期的 100 次,违反了"对外可见值是 100 的倍数"这个不变量)。别的线程后续 lock 时拿到这个脏数据,基于它做决策——很可能引发连锁 bug。

Rust 的设计:**持锁时 panic 的 mutex 被标记为"中毒"**,后续 `lock()` 返回 `Err(PoisonError)`。`PoisonError` **里头带着 guard**——你必要时可借此修复不一致状态(比如回滚),再让别的线程继续。多数代码要么忽略中毒(`.unwrap()` 把 panic 往外传)、要么 `err.into_inner()` 接受脏数据继续跑。

测试 `m2_07_poison` 演示了完整的链路:scoped 线程持锁 panic → `thread::scope` 在结束时把 panic 重新抛出 → 用 `catch_unwind` 接住 → 事后 `Mutex::lock(&m)` 拿到 `Err` → `into_inner()` 取出脏数据(仍是 `0`,因为 panic 发生在第一次自增前)。

#### 为什么 Rust 选了"中毒"而不是"自动恢复"

你可能会问:为什么不让 mutex 自动忽略 panic、让别的线程继续用?答案分两层。

**第一层:语义层面,中毒后的数据可能不再满足不变量。** 比如一个 `Mutex<Bank>` 保护着"所有账户余额之和 = 总发行量"这个不变量。持锁线程在两条 `transfer` 之间 panic——余额已经从 A 扣了、还没加到 B——不变量被打破。别的线程拿到锁,基于"不变量成立"做决策(比如允许提现),会连锁出错。中毒机制逼你**显式**面对这个可能性,而不是默默继续。

**第二层:工程层面,Rust 的"无畏"哲学。** Rust 宁愿让你程序**显式失败**,也不愿让你程序**默默错误地继续跑**。后者是 C/C++ 长期的痛——一个线程死了,全局状态处于半破坏,别的线程继续跑出离谱结果,debug 半天才发现根因。中毒把"有线程死了"这件事**广播**给所有用这把锁的人,即便你不修复、只是 `.unwrap()` 重新抛 panic,也比默默继续强——至少 bug 会**显式炸**,而不是默默错。

实战中,大多数代码用 `.unwrap()`(把 panic 往外传)或 `err.into_inner()`(接受脏数据)。前者适合"任何不一致都该让整个进程死"的场景(比如无状态服务);后者适合"数据本身是 best-effort 的"(比如缓存)。**几乎没人**真的去修复不一致状态——那要求你对被保护数据的内部结构了如指掌。这就是为什么 Mara Bos 说"恢复不一致状态在实践中不常见"。

### 6.3 阴险的坑:`MutexGuard` 的临时生命周期

"drop guard 即解锁"很方便,但有个**反直觉**的坑。看这段**意图错误**的代码:

```rust
if let Some(item) = list.lock().unwrap().pop() {
    process_item(item); // ← 此刻 guard 还没 drop!还在持锁!
}
```

直觉是"`list.lock().unwrap().pop()` 这条语句结束时就解锁了"。**错**。`lock()` 返回的临时 guard 在 `if let` **整条语句结束时**(也就是 `process_item` 跑完、`}` 走到时)才 drop——于是 `process_item` 期间不必要地占着锁,别的线程进不来。

为什么?这是 Rust 临时量生命周期规则:`if let PATTERN = EXPR { BODY }` 中,EXPR 里的临时量(包括 `lock()` 返回的 guard)要活到**整条 `if let` 结束**,因为 PATTERN 可能从 EXPR 借东西(`front()` 就会借),编译器保守地延长临时量的寿命——即便你用 `pop()` 拿到 owned 值、本不必延长,也一样延长。

**反例**:普通 `if` 没这问题:

```rust
if list.lock().unwrap().pop() == Some(1) {
    do_something(); // 这里已经不持锁了
}
```

因为 `if` 的条件是纯 `bool`,不可能借东西,临时量在条件求值完就 drop 了。

**正确写法**:把 pop 拆到单独的 `let`,让 guard 在那条 `let` 结束时就 drop:

```rust
let item = list.lock().unwrap().pop(); // guard 在此 drop(语句结束)
if let Some(item) = item {
    process_item(item);                  // 不持锁
}
```

测试 `m2_03_guard_lifetime` 就是这个正确写法——它甚至在线程外 pop 完后,新起线程立刻能 `push`(证明锁已释放)。

**记住**:拿不准就显式 `drop(guard);`,或把锁操作单独成句。临时 guard 别和 `if let`/`while let`/`match` 混写。

---

## §7 `RwLock<T>`:读和写分得清

`Mutex` 只管独占——哪怕你只想看一眼数据(一个 `&T` 就够),它也给你 `&mut T`,挡住所有别的线程。读多写少的场景下(比如配置表),这浪费得太离谱:**8 个线程同时只读,本来可以 8 核并行,变成 8 个排队**。

**读写锁**(reader-writer lock)更聪明。三种状态:

1. **未锁**:任何人都能上读锁或写锁。
2. **被一个写者锁**(独占):其他写者、读者都等。
3. **被 N 个读者锁**(共享):其他读者可以继续进,写者等。

适合"多读少写"。Rust 的 `RwLock<T>`:

```rust
use std::sync::RwLock;
let config = RwLock::new(vec![1, 2, 3]);

let g = config.read().unwrap();   // 共享读锁,多个读者可并行
assert_eq!(&*g, &[1,2,3]);
drop(g);

let mut g = config.write().unwrap(); // 独占写锁
*g = vec![10];
```

`RwLockReadGuard` 只 `Deref`(像 `&T`),`RwLockWriteGuard` 还 `DerefMut`(像 `&mut T`)。它是 `RefCell` 的多线程版——`RefCell` 是"运行时借用检查、违规 panic",`RwLock` 是"运行时借用检查、违规阻塞等"。

### 7.1 写饥饿:埋 M7 的伏笔

`RwLock` 听起来比 `Mutex` 全面优秀。但有**陷阱**:大多数实现,当有写者在等时,会**挡住新读者**(哪怕此刻锁是读锁)——否则一群读者集体霸占锁、写者永远进不来,这叫**写饥饿**(writer starvation)。

写饥饿不容易立刻看出来——你跑测试好好的,上线后发现"配置偶尔几秒不更新"。M7 我们会自建一个**写公平**的 RwLock,正面解决这个敌人。现在你只需记住:`RwLock` 不是银弹,在写者频繁的场景下,它的"公平性"是个隐藏属性。

> **`cargo bench -p forge-sync --bench m2_rwlock_starvation`** 把这条伏笔用数字摆出来:7 个读者死循环 `read()`(hold 期间做活撑大 hold 时间),写者完成 5000 次 `write()`,比较 `std::sync::RwLock`(reader-preferring)和 `forge_sync::RwLock`(M7 写公平版)。实测有个反直觉但诚实的结论——**forge 的写者反而比 std 慢 2–3 倍**。原因:这条 bench 测的是**吞吐**,不是**公平性**;forge 的写公平保证来自每次操作都要维护的 3 态编码 + `writer_wake_counter` + Condvar,是活性保证的代价,而 std 被调优了十几年,per-op 开销低得多。"写公平"的真正价值在**最坏情况写延迟**(p99/p999),不在吞吐——要衡量它得换测尾延迟,留给练习。这条 bench 的教学价值:区分"吞吐"和"公平性",别混为一谈。

### 7.2 类型约束

`Mutex<T>` 要 `T: Send`(数据要能搬到持锁线程),`RwLock<T>` 还要 `T: Sync`(多个读者线程同时持 `&T`)。违反这些约束,你能造出锁,但锁本身不 `Sync`,送不到别的线程——等于没用。

测试 `m2_04_rwlock` 演示 8 个读者并行求和、写者独占重置。

---

## §8 等待:让线程睡到条件成立

到这里我们有了锁、有了共享可变数据。但还差最关键的一块——**等**。

设想一个任务队列。worker 线程要 pop 任务处理。队列空了怎么办?

- **忙等**:`while q.is_empty() {}`。CPU 转圈烧电,把别的线程挤掉。**绝对不行**。
- **睡过去,等别人把我叫醒**:这是 `thread::park` 和 `Condvar` 要解决的。

但"睡"和"叫醒"之间有一个**致命的竞态**。我们用两个手算例子把它钉死。

### 8.1 姿势一:`thread::park` / `unpark`

一个线程 `thread::park()` 把自己睡死(进内核、不占 CPU)。另一个线程拿它的 `Thread` 句柄,调 `unpark()` 把它唤醒。

最小例子:一个生产者、一个消费者。

```rust
use std::collections::VecDeque;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

fn main() {
    let queue = Mutex::new(VecDeque::new());

    thread::scope(|s| {
        let consumer = s.spawn(|| loop {
            let item = queue.lock().unwrap().pop_front();
            if let Some(item) = item {
                dbg!(item);
            } else {
                thread::park(); // 队列空,睡
            }
        });

        for i in 0.. {
            queue.lock().unwrap().push_back(i);
            consumer.thread().unpark(); // 唤醒消费者
            thread::sleep(Duration::from_secs(1));
        }
    });
}
```

两个关键性质,每个都是命门:

1. **`unpark` 的请求不会丢**——这就是设计来堵"丢失唤醒"竞态的。
2. **`unpark` 不累积**——两次 `unpark` 再两次 `park`,第二个 `park` 照样睡。

还有一个:`park` 可能有**假唤醒**(spurious wakeup)——没人 `unpark` 它也可能自己醒。我们的循环天然处理了它(醒来再 `lock` 看一眼,空就再 `park`)。

**`park` 的短板**:一旦有**多个**消费者,生产者**不知道该 `unpark` 哪个**。一对一还行,一对多就得换 `Condvar`。

#### 为什么 `park`/`unpark` 设计成 per-Thread 的许可

你可能会问:为什么 `unpark` 不直接是"广播给所有等的人"?为什么要 per-Thread 维护一个许可 token?答案是**开销**:广播需要一个全局的等待者队列,每次 `park`/`unpark` 都要竞争这个队列的锁;而 per-Thread 许可只是每个线程自己的一个 bit,无竞争。代价就是上面说的"一对一才合适"。`Condvar` 提供了那个"全局等待者队列"——多花点同步开销,换"一对多"的能力。**两者是不同档位的工具**,选哪个看你场景里等待者的数量。

#### 手算例子 A:`park`/`unpark` 为什么不丢通知

这是必手算的第一处。Mara Bos 的 8 步推演,我们逐拍画给你看。设消费者 C、生产者 P、共享队列 Q(初始空)、C 的"许可 token" T(初始 false——不存在)。

| 时刻 | 线程 | 动作 | Q 状态 | T(许可) | C 状态 |
|---|---|---|---|---|---|
| t1 | C | `lock(Q)` | [] | false | 运行 |
| t2 | C | `pop_front()` 返回 `None` | [] | false | 运行 |
| t3 | C | `unlock(Q)`(guard drop) | [] | false | 运行 |
| **t4** | **P** | **`lock(Q)`**(C 已 unlock,P 拿到) | [] | false | 运行 |
| **t5** | **P** | **`push_back(42)`** | **[42]** | false | 运行 |
| **t6** | **P** | **`unlock(Q)`** | [42] | false | 运行 |
| **t7** | **P** | **`unpark(C)`**——C 还没 park! | [42] | **true**(许可被记下) | 运行 |
| **t8** | **C** | **`park()`** | [42] | **false**(许可被消费) | **立刻返回,不睡!** |

注意 t7→t8:**`unpark` 在 C 真正 `park` 之前发生**。如果 `unpark` 只对"已经 park 的线程"有效,这个通知就丢了——C 永远睡死,Q 里有 42 也没人处理。

**`park`/`unpark` 的设计保证**:每个线程关联一个"许可 token"。`unpark(T)` 把 T 的 token 设为 true(如果已是 true 就保持 true,**不累加**)。`park()` 检查 token:若 true,**消费掉**(置 false)并立刻返回;若 false,进内核睡,直到下次 `unpark` 把 token 设 true 才被唤醒。

正因为"许可"这个机制,通知在 t7→t8 这种交错下**不丢**。C 的 `park` 立刻返回,继续循环,`lock(Q)` 看到 42,pop 出来处理。

**但 `unpark` 不累积**——这是另一面。设想 P 连 `unpark` 两次(t7a、t7b),C 接下来 `park` 两次(t8a、t8b):

| 时刻 | 动作 | T | C 状态 |
|---|---|---|---|
| t7a | P `unpark(C)` | true | 运行 |
| t7b | P `unpark(C)` | true(不变成 2) | 运行 |
| t8a | C `park()` → 消费 token → 立即返回 | false | 继续运行 |
| t8b | C `park()` → token 是 false → **进内核睡** | false | **睡死** |

第二次 `park` 没有许可可消费,**真的睡了**。所以原书强调"只在确认队列空之后才 `park`"——不要每处理一个就 `park`,否则两次 `unpark` 只能匹配一次 `park`,另一次 `park` 飘进虚空。

#### 手算例子 B:`park` 的真正竞态——`while` 内的检查缝隙

许可机制看似天衣无缝,但有**真正的死角**。把消费者代码改成更"严谨"的写法:

```rust
loop {
    let q = queue.lock().unwrap();
    while q.is_empty() {
        // 这里 unlock 后才能 park,否则持着锁睡,别人怎么 push?
        drop(q);                  // 解锁
        thread::park();           // 睡
        q = queue.lock().unwrap();// 醒来重锁
    }
    let item = q.pop_front().unwrap();
    drop(q);
    process(item);
}
```

**问题**:在 `drop(q)`(解锁)和 `thread::park()`(睡)之间,**有一个缝隙**。在这个缝隙里,生产者 P 完全可以跑过来 push 一个任务并 `unpark`。我们逐拍画:

| 时刻 | C 动作 | P 动作 | Q | T | C 状态 |
|---|---|---|---|---|---|
| s1 | 检查 `q.is_empty()` → true | | [] | false | 运行(持锁) |
| s2 | `drop(q)` 解锁 | | [] | false | 运行(不持锁) |
| **s3** | (准备调 `park`...) | **`lock(Q)` → 拿到** | [] | false | 运行 |
| **s4** | | **`push(42)`** | **[42]** | false | 运行 |
| **s5** | | **`unlock(Q)`** | [42] | false | 运行 |
| **s6** | | **`unpark(C)`** | [42] | **true**(许可给了) | 运行 |
| **s7** | **`park()`**——查 token:true → 消费 → 立即返回 | | [42] | false | 运行(没事!) |

等等,这没事啊?许可机制救命了。那问题在哪?

**问题在另一种交错**——P 的 `unpark` 在 s3 **之前**发生(也就是 C 还没决定要 park 时):

| 时刻 | C 动作 | P 动作 | Q | T | C 状态 |
|---|---|---|---|---|---|
| s1 | 检查 `q.is_empty()` → true | | [] | false | 运行(持锁) |
| s2 | `drop(q)` 解锁 | | [] | false | 运行 |
| **s3** | | **`lock(Q)`、push、unlock、`unpark(C)`** | [42] | true | 运行 |
| s4 | (准备 park) | | [42] | true | 运行 |
| s5 | **`park()`** → token true → 消费 → 立即返回 | | [42] | false | 运行 |

还是没事!`park` 立即返回,继续循环,`lock` 看到 42,pop 处理。

**真正的问题是别的**:许可**只能存一份**。设想两次循环都走到"准备 park"但还没真 park,中间生产者连 `unpark` 两次:

| 时刻 | C 动作 | P 动作 | T | C 状态 |
|---|---|---|---|---|
| s1 | 循环 1:`is_empty` true,drop q | | false | 运行 |
| s2 | | `unpark(C)`(为任务 1) | true | 运行 |
| s3 | | `unpark(C)`(为任务 2) | true(不累加!) | 运行 |
| s4 | `park()` → token true → 立即返回 | | false | 运行(只消化了一个!) |
| s5 | 循环 2:`lock`,`is_empty`?(其实有任务 2)| | false | 运行 |

仔细看 s5——C 醒来重新 lock 后会 pop 任务 1,然后下一轮 `is_empty` 会看队列还有任务 2,**不会**进 park 分支,所以这里其实**没 bug**。

**park 真正的死角**:它在"一对一"模型下被许可机制救回来,但**多消费者**下彻底失效——生产者不知道哪个消费者在 park、`unpark` 哪个都没用、许可可能给错人。我们需要一个**"广播电台"**:谁在等匿名、谁在通知也匿名。这就是 **Condvar**。

而且,**还有一种更深的死角**:park 的 token 是 per-thread 的——一个线程只有一个 token。如果你**同一个线程**在等**多个不同条件**(队列非空、或停止信号到了),你只有一个 token,这两种通知会**互相吃掉**对方的许可。Condvar 用"每个条件一个变量"解决了这个。

### 8.3 姿势二:Condvar(条件变量)

条件变量就是一个"广播电台"。它有 `wait` 和 `notify`:多个线程能 wait 在同一个 Condvar 上;`notify_one` 叫醒一个、`notify_all` 叫醒全部。**生产者无需知道谁在等**——这正是多消费者场景需要的。

Condvar 解决的核心问题:**"解锁 mutex"和"开始等"之间的缝隙会让通知丢失**。它的解法是——`wait` **原子地**完成"解锁 mutex + 进内核 park"。

`Condvar::wait(guard)` 的契约:

1. 吃一个 `MutexGuard`(证明你已锁)。
2. **原子地**:`unlock(mutex)` + 把当前线程挂到这个 Condvar 的等待队列上 + 进内核睡。
3. 被 `notify` 唤醒后,重新 `lock(mutex)`,返回新的 `MutexGuard`。

"原子"是命门——下面手算给你看为什么。

#### 手算例子 C:如果 `wait` 不是原子的,通知怎么丢

把消费者的循环写成这样(伪代码):

```rust
let mut q = queue.lock().unwrap();
while q.is_empty() {
    // 假想:"先解锁,再 park"——不是原子的
    drop(q);                    // 步骤 a:解锁
    not_empty.wait_no_lock();   // 步骤 b:开始等(假设的坏 API)
    q = queue.lock().unwrap();  // 步骤 c:重锁
}
```

设想 C 在 `drop(q)`(步骤 a)之后、`wait_no_lock()`(步骤 b)之前,**有一拍空隙**。生产者 P 完全可以挤进来:

| 时刻 | C 动作 | P 动作 | Q | C 状态 |
|---|---|---|---|---|
| c1 | `lock(Q)`,`is_empty` true | | [] | 运行(持锁) |
| c2 | **`drop(q)` 步骤 a**(解锁)| | [] | 运行(**不持锁**) |
| **c3** | (准备调 `wait_no_lock`) | **`lock(Q)`** | [] | 运行 |
| **c4** | | **`push(42)`** | [42] | 运行 |
| **c5** | | **`unlock(Q)`** | [42] | 运行 |
| **c6** | | **`notify_one()`——但 C 还没在等!** | [42] | 运行 |
| **c7** | **`wait_no_lock()`**——现在才开始等 | [42] | **睡死** |

c6 → c7:**`notify_one` 发生时 C 还没注册到等待队列**。`notify_one` 找不到等待者,通知**飘走、丢失**。C 进 `wait` 后永远没人叫它——它睡死了,而队列里有 42。

这就是为什么 `Condvar::wait` 必须把"解锁"和"开始等"做成**原子**:让 P 在 c3-c6 之间**根本拿不到 mutex**,P 的 `lock(Q)` 会阻塞——直到 C 真的进了内核 park(此时 C 已注册到等待队列),mutex 才被释放,P 才能进来。P 进来后 `notify`,**必然**命中已注册的 C。

#### Condvar 的真实写法

```rust
use std::sync::Condvar;

let queue = Mutex::new(VecDeque::new());
let not_empty = Condvar::new();

// 消费者
thread::scope(|s| {
    s.spawn(|| loop {
        let mut q = queue.lock().unwrap();
        let item = loop {
            if let Some(item) = q.pop_front() {
                break item;
            } else {
                // 原子地:解锁 + 睡,醒来后重新加锁并返回新 guard
                q = not_empty.wait(q).unwrap();
            }
        };
        drop(q);          // 处理前先解锁(让别的线程能用队列)
        dbg!(item);
    });

    // 生产者
    for i in 0.. {
        queue.lock().unwrap().push_back(i);
        not_empty.notify_one();
        thread::sleep(Duration::from_secs(1));
    }
});
```

**铁律:`wait` 必须在循环里**。因为假唤醒——`wait` 可能没人 notify 也自己醒。醒来**必须**重新检查条件(这里就是"`q.pop_front()` 能 pop 出东西"),不成立就继续 wait。**绝不能假设"`wait` 返回 = 条件成立了"**。这就是为什么外层是 `loop` + 内层也是 `loop`——外层处理"不停消费",内层处理"假唤醒后重试"。

Condvar 还有些边角:

- 通常**只跟一个 Mutex 配对**——两个线程用不同 mutex 等同一个 Condvar 会 panic。
- `notify_one` 叫醒一个(具体哪个未指定),`notify_all` 叫醒所有。
- 有 `wait_timeout` 变体带超时。

---

## WRITE:把所有东西拧成一件实物——`TaskQueue`

M2 的终点产物。一个"生产者 push、工作者用 Condvar 阻塞 pop"的线程安全队列。代码极短,但它**汇集本章全部要点**:`Mutex` 保护数据、`Condvar` 让等待者睡到条件成立、`wait` 在循环里(消化假唤醒)、guard 一离开作用域就解锁(缩短临界区)。

源码在 `crates/forge-sync/src/std_locks.rs`:

```rust
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

pub struct TaskQueue<T> {
    queue: Mutex<VecDeque<T>>,
    /// "队列非空"这个条件。等待者睡在这上面,生产者 push 后 notify_one 唤醒一个。
    not_empty: Condvar,
}

impl<T> TaskQueue<T> {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            not_empty: Condvar::new(),
        }
    }

    /// 投入一个任务,并唤醒一个正在等待的工作者。
    pub fn push(&self, task: T) {
        // 先 push 再 notify;guard 在这条语句末 drop,释放锁。
        self.queue.lock().unwrap().push_back(task);
        self.not_empty.notify_one();
    }

    /// 阻塞地取出一个任务;空了就睡,直到被通知。
    /// 必须循环:Condvar::wait 可能假唤醒。
    pub fn pop_blocking(&self) -> T {
        let mut guard = self.queue.lock().unwrap();
        loop {
            if let Some(task) = guard.pop_front() {
                return task;
            }
            // wait 原子地"解锁 + 睡",醒来后重新加锁并返回新 guard。
            // 这一步消除"解锁"和"开始等"之间的缝隙——通知不可能丢。
            guard = self.not_empty.wait(guard).unwrap();
        }
    }

    /// 非阻塞地尝试取一个;没有就立刻返回 None。
    pub fn try_pop(&self) -> Option<T> {
        self.queue.lock().unwrap().pop_front()
    }
}
```

测试 `m2_06_condvar_task_queue` 把它跑成一个**真实的小调度器**:4 个 worker 各自循环 `pop_blocking`,生产者投 1000 个任务 + 4 个"毒丸"哨兵(`usize::MAX`)叫停。结果:1000 个任务全被处理、4 个 worker 各自收到毒丸退出。

```rust
const N_WORKERS: usize = 4;
const N_TASKS: usize = 1000;

thread::scope(|s| {
    for _ in 0..N_WORKERS {
        s.spawn(|| loop {
            let task = q.pop_blocking();
            if task == usize::MAX { break; }   // 毒丸:收摊
            done.fetch_add(1, Ordering::Relaxed);
        });
    }
    for i in 0..N_TASKS { q.push(i); }
    for _ in 0..N_WORKERS { q.push(usize::MAX); }
});

assert_eq!(done.load(Ordering::Relaxed), N_TASKS);
```

注意几个细节,都是本章学过的:

- **生产者先 push 完所有任务,再投毒丸**——不能边 push 边投,否则 worker 可能在任务还没 push 完时就收到毒丸退出。
- **毒丸数 = worker 数**——每个 worker 必须各自收到一个才能退出。
- **`notify_one` 而不是 `notify_all`**——push 一个任务只需唤醒一个 worker,唤醒所有只会让其他 worker 假唤醒浪费 CPU。

**这个 `TaskQueue` 就是 M9a 工作窃取线程池的起点**。到那时你会发现:单队列 + Condvar 在多核下会成瓶颈(所有 worker 抢同一把锁),于是我们给每个 worker 配一个本地无锁队列(Chase-Lev, M8),互相"偷"工作——但"pop 时若空就阻塞等"这套**骨架**,正是这里立起来的。

---

## ISO·ZOOM:回头看看你学到了什么

回到开头的 ENEMY:状态装不下原子、两个线程要抢、worker 要等任务。你现在有了一整套工具:

- **状态装不下原子** → 装进 `Mutex<T>`(独占)或 `RwLock<T>`(读写分离)。
- **谁都不比谁活得短** → `Arc` 共享所有权(原子计数)。
- **想跨线程共享 `&T`** → `T` 必须 `Sync`;想搬过去 → `T` 必须 `Send`。
- **worker 要等任务** → `park`/`unpark`(一对一)或 `Condvar`/`wait`/`notify`(多对多,原子解锁+睡,通知不丢)。
- **持锁者死了** → 锁中毒,后续 lock 返回 Err。
- **持锁太久** → 并行性归零。
- **`if let` + 临时 guard** → 锁意外延长,阴森陷阱。

这些不是 API 列表——它们每一个都是**被一个具体问题逼出来的解**。

### L1–L5 缩放回顾

| 层 | 你能…… |
|---|---|
| **L1** | 一句话:`Arc` 共享所有权,`Mutex`/`RwLock` 互斥/读写访问,`Condvar`/`park` 让线程睡到条件成立。 |
| **L2** | 用类比讲清楚:锁是卫生间门锁(Mutex)、读卡位 vs 写卡位(RwLock)、广播电台 vs 直拨电话(Condvar vs park)。 |
| **L3** | 跟踪 10×100 自增的可见值序列、park 8 步、Condvar wait 的"原子解锁+睡"、guard 生命周期陷阱。 |
| **L4** | 解释锁中毒为何存在、`unpark` 许可不累积、写饥饿为何发生、scoped threads 为何不依赖 Drop、`T: Sync ⟺ &T: Send`。 |
| **L5** | 判断何时该 Mutex vs RwLock vs park vs Condvar;知道每种在 M4/M7/M9a 里如何被自研版替代。 |

### 自检

- [x] 先讲敌人(状态装不下原子、worker 要等条件)再讲武器。
- [x] 至少 2 处手算例子(park 8 步不丢通知 + wait 原子性必要性)。
- [x] 故意打破:`unpark` 不累积、`if let` + 临时 guard、scoped threads 历史(Leakpocalypse)。
- [x] 用原书的真实例子(10×100、park 8 步、if-let 陷阱、写饥饿)。
- [x] 识别同构:scoped threads 复用借用规则;`Arc`="原子的 Rc";`RwLock`="多线程 RefCell";`Cell`=`AtomicXxx` 的单线程版。
- [x] 终点产出真实可用的 `TaskQueue`,并预告它是调度器种子。

### 动手清单

`tests/m2_01_scoped_threads` · `m2_02_mutex_counter` · `m2_03_guard_lifetime` · `m2_04_rwlock` · `m2_05_park_unpark` · `m2_06_condvar_task_queue` · `m2_07_poison`。

每个测试对应一节,跑通它,改一行看它怎么挂(比如把 `pop_blocking` 的循环去掉,看假唤醒怎么搞死你)。

---

下一站 → [M3 自建 SpinLock + CPU 真相](./M3-spinlock.md):我们第一次写 `unsafe`——用 `AtomicBool` + `UnsafeCell` 造一个真实的自旋锁,把 M1 的 Acquire/Release 理论落到一个能跑、能被 miri 验证的锁上。顺带揭开 CPU 的真相(x86 强序 vs ARM 弱序),你会明白为什么"原子的内存序"不是过度设计。
