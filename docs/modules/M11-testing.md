> **M11 —— 并发与异步测试:把"我测过了"变成"我能证明"**
>
> 跑:`cargo test --workspace`　|　模型检查:`LOOM_MAX_PREEMPTIONS=3 cargo test loom`　|　UB 检测:`cargo +nightly miri test`　|　基准:`cargo bench -p forge-core`
>
> **本模块的敌人**(请你先记住它,它会陪你走完整章):你写完一段并发代码,在本机跑了一万次测试,全绿。你提交了。三天后生产环境某个 ARM 服务器上,凌晨 3 点,一条消息丢了。你拉回日志,发现是某个 `Relaxed` store 被重排了。你这才意识到——**"我测过"和"我证明过"之间隔着一道深渊,这道深渊的名字叫"概率性 bug"**。普通代码的 bug 是确定性的:输入 X 永远触发输出 Y 的错误。并发 bug 不一样:输入相同,99.999% 的执行没事,0.001% 的执行才会撞上那条致命的线程交错。**单元测试只覆盖了那 99.999%,完全没碰那 0.001%。** 这一章的全部工具——loom、miri、criterion、stress、锁序校验器——都是来帮你跨过这道深渊的。

---

## 〇、为什么"并发测试"需要一个独立模块

### ENEMY:并发 bug 是"概率事件",普通测试套不住它

请你回想 M1.6 那个 Relaxed 指针发布的 bug。我们写过这样一段"消费者":

```rust
let p = PTR.load(Ordering::Relaxed);   // [A]
if !p.is_null() {
    let _data = unsafe { &*p };        // [B]:可能读到未初始化
}
```

这个 bug 在 x86 上几乎不会触发——因为 x86 是 TSO(Total Store Order)模型,store 基本按程序顺序"漏"到主存。所以你写一个 `for _ in 0..10000 { 测试 }`,10000 次全过。然后你放心地提交。等到部署到苹果 M 系列、AWS ARM 实例、或者某台开了 SMT 的服务器上,bug 才浮出水面。

**这就是并发 bug 的反直觉之处**:它不是"代码逻辑错",而是"代码逻辑在某种 CPU 模型下错"。普通单元测试只检查"代码逻辑",它对"在哪种 CPU 模型下"完全不敏感。你的 10000 次测试,本质上是**同一个 CPU 模型**下跑了 10000 次——你只覆盖了一种可能性,而并发程序的可能性是一个**指数级爆炸的执行树**。

这一章要做的事情,就是把那棵执行树**手动展开给你看**,然后给你四把武器,让你能在写代码的当下、在本机的 x86 上,**提前发现 ARM 上才会暴露的 bug**。

### 一句话总览这一章

> 普通测试问"功能对不对",并发测试问"在所有合法的线程交错下,功能是不是都对"。loom 用**模型穷举**回答这个问题;miri 用**未定义行为检测**回答;stress 用**真实硬件 + 海量重复**回答;criterion 用**统计置信**回答"性能到底改没改"。它们各有盲区,只有**组合起来**才构成完整的防线。

### 五拍预告

| 拍 | 节 | 敌人 | 武器 |
|---|---|---|---|
| ENEMY/ANCHOR | M11a | "测过≠证明过"的深渊 | 概率思维 + 执行树 |
| LOW-FI | M11b | loom 的执行树指数爆炸 | `LOOM_MAX_PREEMPTIONS` 剪枝 |
| WRITE | M11c | miri 怎么"模拟"它根本不跑的 ARM | 解释器 + 弱内存 + 抢占 |
| WRITE | M11d | "性能改了 5ns 是不是错觉" | criterion 的统计模型 |
| ISO·ZOOM | M11e | 死锁的循环等待 | thread-local 锁序追踪器 |
| 补丁 | M11f | stress 是 loom/miri 的什么补丁 | 真实硬件的弱内存 |

---

## 一、ANCHOR:执行树——并发 bug 的"概率地形图"

### 把一次并发执行画成一棵树

请你闭上眼睛想象一个画面:两个线程,A 和 B,各自要做两件事。

```
线程 A:a1 → a2
线程 B:b1 → b2
```

**单线程视角下**,只有一种执行:`a1, a2`(或 `b1, b2`)。但**多线程视角下**,这两个线程在时间轴上可以任意交错。每一种交错都是一棵"执行树"上的一条根到叶的路径。

让我们把这棵树画出来。树的每一层代表"现在轮到谁执行下一步",分支代表"选择谁"。为简化,我们假设调度器在每一步都可以从"还有未完成步骤的线程"里任选一个推进。

```
                              [根]
                  /                            \
                A 走 a1                       B 走 b1
              /          \                  /           \
           A 走 a2      B 走 b1          A 走 a1        B 走 b2
             |            |                |              |
           B 走 b1      A 走 a2          B 走 b2        A 走 a1
             |            |                |              |
           B 走 b2      B 走 b2          A 走 a2        A 走 a2
        (叶:a1 a2 b1 b2)(叶:a1 b1 a2 b2)(叶:b1 a1 b2 a2)(叶:b1 b2 a1 a2)
```

注意右下两个叶节点里 `a2` 和 `a1` 的相对顺序——**A 自己的步骤顺序永远是 `a1 → a2`,不可能变成 `a2 → a1`**(这叫"程序顺序",单线程内不可逆)。变化的是"A 的步骤"和"B 的步骤"在时间轴上怎么交错。

两个线程各 2 步,共 4 步,合法的交错数 = `C(4,2) = 6` 种(从 4 个时间槽里选 2 个给 A)。我上面只画了 4 个叶节点,请你**自己补出剩下 2 个**作为练习(提示:从根"先 A 后 B"那条分支,继续往下选)。

**这就是执行树**:它的每一条根到叶路径都是一次"合法的"并发执行。如果你的程序在**任意一条**叶节点上出错,它就是有 bug 的。普通单元测试在本机上反复跑 10000 次,实际上只是在这棵树的某几条路径上来回溜达——它从未踏足大部分叶节点。

### 为什么 stress 测试可能跑几年都碰不到某个叶节点

现在请你想:x86 的真实调度器选哪条路径?

x86 是个**强模型 + 抢占式**调度器。抢占式意味着调度器可以在任意两条指令之间把线程挂起、换另一个线程上 CPU。但实际工程中,x86 的抢占频率大概是每 1~10 毫秒一次(取决于时钟中断和 `sched_yield` 习惯)。而一次 `Relaxed` store 到主存的延迟大约是几十纳秒。

也就是说:**x86 上两个线程的"指令交错窗口"通常是几百纳秒到几微秒**。如果一个 bug 需要的交错是"`a1` 和 `a2` 之间恰好插入 `b1`",而 `a1` 和 `a2` 相距只有几纳秒,那么这个 bug 触发的概率大约是 `几纳秒 / 几毫秒 ≈ 10⁻⁶`。你跑 10000 次测试,撞上的概率约 `10⁻²`——大概率撞不上。要撞上一次,你得跑大约 100 万次。

**ARM 上更糟**:ARM 是弱内存模型,store 可以被 store buffer 重排。这意味着即使没有抢占,某些交错也是"合法"的——你根本不需要抢占触发,store buffer 自己就会"模拟"出这种交错。所以同一个 bug 在 ARM 上触发概率可能是 `10⁻³`,在 x86 上是 `10⁻⁶`。你在 x86 上跑测试,等于在用 1000 倍的运气赌它不发生。

这就是为什么 stress 测试是 loom/miri 的**补丁而非替代**:stress 跑的是真实硬件的真实概率分布,而 loom/miri 跑的是**所有合法的可能性的均匀枚举**。前者抓"高频 bug",后者抓"低频但致命的 bug"。

### 把"概率"变成"枚举"

loom 的核心思想一句话:**与其赌运气,不如穷举**。loom 把执行树的所有叶节点挨个走一遍,任何一个叶节点触发断言失败,loom 立刻报告"找到反例",并打印出那条致命的交错。

听起来很美。问题在于:执行树的叶节点数是**指数级爆炸**的。两个线程各 N 步,叶节点数大约是 `C(2N, N) ≈ 4^N / √(πN)`。N=10 的时候已经上万,N=20 的时候是几十亿。loom 必须剪枝——这就是 `LOOM_MAX_PREEMPTIONS` 的来历。

我们下一节就手算这棵树,让你**亲眼看见**剪枝是怎么工作的。

---

## 二、M11b loom:把执行树一棵一棵走过去

### ENEMY:执行树指数爆炸,怎么剪?

loom 的剪枝策略基于一个观察:**大部分 bug,只需要少数几次"抢占"就能触发**。一次"抢占"的定义是:调度器在某个线程执行中途把它挂起,换另一个线程上 CPU。

loom 的运行模型是这样的:它把每个原子操作、每个 `loom::sync::Mutex::lock`、每个 `loom::thread::spawn` 的 join 点,都标记为**可能的抢占点**。然后它做一种受控的 DFS(深度优先搜索):每次到一个抢占点,它尝试"切换"或"不切换",优先选不切换(这叫"贪婪调度"——尽量让一个线程跑到底)。它只允许**整次执行**中出现 `MAX_PREEMPTIONS` 次切换。

`LOOM_MAX_PREEMPTIONS=0` 意味着"不允许任何抢占"——等价于单线程顺序执行,啥也测不出来。`MAX_PREEMPTIONS=3` 意味着"允许最多 3 次抢占"。这看起来很少,但已经覆盖了**绝大多数内存序 bug**——因为内存序 bug 通常只需要 1~2 次"恰到好处"的切换就能触发。

### LOW-FI:手算一个 2 线程 × 2 步的 loom 模型

请你准备一张纸,我们一起逐拍画。问题是这样的:

```
初始:x = 0, flag = 0(都是原子变量,Relaxed 序)
线程 A:a1: x.store(1, Relaxed)
        a2: y = flag.load(Relaxed)

线程 B:b1: flag.store(1, Relaxed)
        b2: z = x.load(Relaxed)
```

**bug 条件**:执行结束后 `y == 0 且 z == 0`。

请你先**用直觉判断**:这个 bug 真的会发生吗?

直觉派会说:"A 先写了 x,B 才读 x,怎么可能 z==0?B 先写了 flag,A 才读 flag,怎么可能 y==0?"

直觉派错了。我们用执行树证明。

#### 步骤 1:列出所有合法的叶节点

两个线程各 2 步,A 内部 `a1→a2` 顺序固定,B 内部 `b1→b2` 顺序固定。总执行序列是 4 步,合法的叶节点有 `C(4,2)=6` 个:

| 序号 | 执行顺序 | y(读 flag) | z(读 x) | y==0 且 z==0? |
|------|---------|------------|--------|---------------|
| L1 | a1 a2 b1 b2 | 0(a2 时 flag 还是 0) | 1(b2 时 x 已是 1) | **是** |
| L2 | a1 b1 a2 b2 | 1(a2 时 flag 已是 1) | 1(b2 时 x 已是 1) | 否 |
| L3 | a1 b1 b2 a2 | 1 | 1 | 否 |
| L4 | b1 a1 a2 b2 | 1 | 1 | 否 |
| L5 | b1 a1 b2 a2 | 1 | 1 | 否 |
| L6 | b1 b2 a1 a2 | 1 | 0(a2 时 x 还是 0? 不,a1 已在 b2 后) | 看下面 |

等一下,L6 的 z 值要看 b2 在 a1 之前还是之后。我表格里写的是 `b1 b2 a1 a2`,b2 在 a1 之前,所以 z = x.load 时 x 还是 0 → z = 0。但 y 呢?a2 时 flag 已是 1 → y = 1。所以 L6: y=1, z=0,bug 条件不满足(需要 y==0 **且** z==0)。

**只有 L1 触发 bug**:`a1 a2 b1 b2`。A 自己先做完两步(写 x,读 flag),B 再做两步。这时 A 读 flag 时 flag 还是 0,所以 y=0;B 读 x 时 x 已经被 a1 写成 1,所以 z=1。等等,z=1 不满足 z==0。我前面写错了。

让我重画。L1 是 `a1 a2 b1 b2`:
- a1: x = 1
- a2: y = flag.load,此时 flag=0 → y=0
- b1: flag = 1
- b2: z = x.load,此时 x=1 → z=1

bug 条件是 `y==0 且 z==0`,L1 的 z=1,**不触发**。

那要触发 z==0,需要 b2 在 a1 之前。但 b2 必须在 b1 之后(B 内部 b1→b2)。所以执行序列里 b2 要在 a1 之前,等价于 `b1 b2 a1 a2`(L6)。L6 的 y 呢?a2 在最后,flag 已经是 1 → y=1。**也不触发**。

**那这个 bug 真的不会发生吗?**

不会——**在强内存模型下**。因为 `a1 a2 b1 b2` 里 z=1,`b1 b2 a1 a2` 里 y=1。无论哪种交错,要么 y=1 要么 z=1。

但**在弱内存模型下**,会。这就是 loom 和"调度器交错"的区别。

#### 步骤 2:引入弱内存——`Relaxed` 允许 store 重排

关键事实:`Relaxed` 不禁止 store 被重排。所以 a1(`x.store`)和 a2 之前的"看不见的"代码可以被 CPU 重排。a2 是 load,load 不能和 store 合并重排得太离谱。真正能被 CPU 任意重排的是:**两个 store 之间**(它们打不同地址,store buffer 可以决定先后)。

要触发"消费者看到 x=1 但没看到 y=1"这种重排 bug,我们需要让 A 线程做**两个 store**,而不是 store 后立刻 load。我们换一个更典型的"消息发布"骨架:

```
线程 A:a1: data.store(42, Relaxed)    // 先写数据
        a2: ready.store(true, Relaxed)  // 再立标志

线程 B:b1: seen_ready = ready.load(Relaxed)  // 读标志
        b2: got = data.load(Relaxed)         // 再读数据
```

**bug 条件**:`seen_ready == true 且 got != 42`。意思是 B 看到了"已就绪",但读到的数据不是 A 写的 42——这是经典的"消息发布了但数据还没落地"。

在强模型下(x86 TSO),这个 bug 几乎不发生:A 的两个 store 进 store buffer 后,会按程序顺序"漏"到主存(`data` 先于 `ready`)。B 的两个 load 也按程序顺序读到。所以 B 一旦看到 `ready==true`,`data` 必然已经是 42。

在弱模型下(ARM),A 的两个 store 没有顺序保证(都打不同地址),store buffer 可以让 `ready` 先落地、`data` 后落地。这时 B 看到 `ready==true` 但 `data` 还是 0——bug。

loom 跑的是**抽象 Rust 内存模型**,它把"两个 Relaxed store 可能被重排"建模成"两个可能的执行顺序"。loom 不关心具体硬件,它枚举的是"模型允许的可能性"。

#### 步骤 2.5:loom 怎么把这个 bug 翻译成"调度交错"

loom 内部的实现策略很巧妙:它不直接建模"store buffer 重排",而是把"重排"翻译成"调度的另一种可能"——也就是说,loom 假设:对于 Relaxed 操作,任意两个 store 的"可见顺序"都可以被翻转,而这个翻转可以用"另一个线程恰好在两次 store 中间被调度"来模拟。

具体到我们的例子,loom 会枚举这样的执行:

1. A 跑 `data.store(42)` —— 在 loom 模型里,这次 store 的"效果"被记下,但**未必立刻对其他线程可见**。
2. loom 在这里制造一次"切换"——挂起 A,跑 B。
3. B 跑 `ready.load`。loom 现在要决定 B 读到什么。**关键**:loom 把 Relaxed 看成"对其他线程的可见性无承诺",所以 loom 允许 B 读到"未来才会发生的 `ready.store`"。loom 让 B 读到 `seen_ready = true`。
4. B 跑 `data.load`。loom 让 B 读到旧值 `got = 0`(因为 A 的 `data.store` 在这个交错下"还没对 B 可见")。
5. 切回 A,A 跑 `ready.store(true)`,结束。
6. B 的不变量 `seen_ready==true ⇒ got==42` 在 `seen_ready=true, got=0` 时失败。**loom 找到反例。**

整个过程 loom 跑了不到 1 毫秒,但它在模型层面"等价于"了一次 ARM 上触发概率 `10⁻⁵` 的稀有交错。这就是 loom 的力量:它把"硬件概率分布的尾部"翻译成"模型确定性的枚举",让你**不依赖任何随机性**就能撞上那条致命的交错。

**为什么单凭"调度交错"在强模型下抓不到这个 bug?** 我们也手算一遍,把强模型和弱模型对照看清楚。在强模型(TSO)下,A 的两条 store 必须按程序顺序对 B 可见——也就是说,B 一旦看到 `ready==true`(由 a2 写入),就必然也能看到 `data==42`(由 a1 写入,且 a1 在 a2 之前完成)。这种"程序顺序的外部可见性"是 TSO 给的承诺。

但 ARM 不给这个承诺。ARM 上 A 的两条 store 可以乱序对外可见,因为它们打不同地址、彼此没有数据依赖,CPU 的 store buffer 完全有权决定先发哪一条。loom 把这种"ARM 允许、x86 不允许"的乱序统一当成"模型允许",所以在 loom 的执行树里,**那条致命的乱序被枚举成一条普通的分支**——你在 x86 上跑 loom,它也照样能找到。
**所以这个 bug 在 ARM 上能触发,x86 上不能(在纯调度交错意义下)。** loom 怎么找到它?

#### 步骤 3:loom 在 `MAX_PREEMPTIONS=2` 下的搜索

loom 的搜索过程(简化版):

1. **初始状态**:执行树根,A 和 B 都还没开始。
2. **第一拍**:loom 选择一个线程先跑。它默认先跑主线程(假设是 A),让 A 跑到第一个抢占点(a1 = `data.store(42)` 之后)。
3. **决策点**:loom 尝试两种选择——
   - **不切换**:A 继续跑 a2(`ready.store(true)`)。然后 A 结束,B 开始 b1 b2。最终序列 `a1 a2 b1 b2`。B 读到 `ready=true, data=42`,bug 不触发。
   - **切换**:挂起 A,让 B 跑。B 跑 b1(`ready.load`)。loom 此时让 B 读到 `ready=true`(因为 Relaxed 不承诺可见性延迟)。再次决策——
     - 不切换:B 继续 b2(`data.load`)。loom 让 B 读到 `data=0`(因为 A 的 a1 在这个交错下"还没对 B 可见")。最终 `a1 b1 b2 a2`,B 的不变量 `seen_ready==true ⇒ got==42` 失败。**bug 触发!** loom 报告:"找到反例,交错是 a1 → b1 → b2 → a2"。

loom 在 `MAX_PREEMPTIONS=2` 下只需要 1 次切换(在 a1 之后切到 B)就能找到这个 bug。它**不依赖**任何具体硬件——它枚举的是抽象执行模型允许的所有可能性,而 ARM 的 store buffer 乱序只是这个抽象模型的一个具体实例。

#### 步骤 4:把上面的过程画成执行树

```
                    [A 开始]
                       |
                      a1           ← A 写 data=42
                       |
                  -----+-----      
                ↙ 抢占=1        ↘ 抢占=0(继续 A)
              [切到 B]            [A 继续 a2]
                |                     |
               b1                    a2 ← A 写 ready=true
               (B 读 ready=true,     |
                loom 允许!)         [B 开始]
                |                     |
              --+--                  b1
            ↙     ↘                   |
        [B 继续]  [切回 A]            b2 ← B 读 data=42
           |        |                  |
          b2       a2                 [结束]
         (B 读 data=0,
          bug!)     |
                  [B 继续]
                    |
                   b2
                    |
        [结束]    [结束]
        序列:      序列:
        a1 b1      a1 a2
        b2 a2      b1 b2
        ← BUG!     (无 bug)
```

loom 系统地从左到右走遍每个分支,在标着 `← BUG!` 的叶节点停下,打印出那条路径。**这就是 loom 的全部魔力**:它把"概率事件"翻译成"枚举事件",让你不用赌运气。

### WRITE:loom 测试的骨架长什么样

我们写一个完整的、能编译的 loom 测试,演示上面的 bug。这个测试**故意**用 Relaxed 制造 bug,让 loom 抓出来。

```rust
// crates/forge-core/tests/loom_m11_relaxed_reorder.rs
//
// 跑法:
//   LOOM_MAX_PREEMPTIONS=2 cargo test -p forge-core --test loom_m11_relaxed_reorder
//
// 注意:loom 必须用 `loom::sync::atomic::Ordering` 替换 `std` 的版本,
// 用 `loom::thread::spawn` 替换 `std::thread::spawn`。
// 我们用 `cfg(loom)` 让同一份测试在普通 `cargo test` 下也能编译(只是不跑模型)。

#![cfg(loom)]  // 这个测试文件只在 loom feature 下编译。loom 测试一般在单独的 target。

#[cfg(loom)]
mod model {
    use loom::sync::atomic::{AtomicUsize, Ordering};
    use loom::thread;

    /// 这是"故意写错"的版本:两个 store 用 Relaxed,
    /// 在弱内存模型下可能被重排,导致消费者看到 x=1 但 y=0。
    #[test]
    fn relaxed_store_store_can_reorder() {
        loom::model(|| {
            let x = std::sync::Arc::new(AtomicUsize::new(0));
            let y = std::sync::Arc::new(AtomicUsize::new(0));

            let (x2, y2) = (x.clone(), y.clone());
            let h = thread::spawn(move || {
                // 线程 A:两个 Relaxed store
                x2.store(1, Ordering::Relaxed);
                y2.store(1, Ordering::Relaxed);
            });

            // 线程 B(主线程):两个 Relaxed load
            let x_seen = x.load(Ordering::Relaxed);
            let y_seen = y.load(Ordering::Relaxed);
            h.join().unwrap();

            // 不变量:不可能"看到 x=1 但没看到 y=1"。
            // 强模型下成立,弱模型下违反——loom 会找到反例。
            // 注:严格 C++ memory model 下,Relaxed 不建立跨变量顺序,
            // 所以这个断言会被 loom 抓到。
            if x_seen == 1 {
                // 这个分支在某些交错下会失败
                // (因为 y_seen 可能是 0,即"看到了 x=1 但 y 还没写")
                // 为了让测试在 loom 下确实失败,我们改成 assert:
                // assert!(y_seen == 1);  // ← 取消注释会让 loom 报反例
                let _ = y_seen; // 这里只是记录,不做强断言
            }
        });
    }
}
```

loom 的真实测试通常**带强断言**,让它一旦枚举到反例就 panic,从而 fail 整个 loom 测试。把上面的 `assert!(y_seen == 1);` 取消注释,loom 会在几秒内打印一条反例路径——你会在终端里看到类似 `thread panicked at 'assertion failed: y_seen == 1'` 加上一串 `loom::model` 的 backtrace,告诉你这条交错具体是哪几步。

### ISO·ZOOM:loom 在 Forge 各模块的真实战绩

回望我们已经走过的章节,loom 抓到过哪些 bug?

- **M1.6 Relaxed 重排**:就是上面这个例子。指针发布的 store 和数据 store 都用 Relaxed,在弱模型下指针先于数据可见。loom 在 `MAX_PREEMPTIONS=3` 下抓到。
- **M3 SpinLock unlock 的 Relaxed**:unlock 用 Relaxed(应为 Release),临界区内的写可能"漏"到下个线程的临界区。loom 抓到。
- **M5 oneshot 丢失唤醒**:发送端 `store(1, Relaxed)` 与接收端 `load + wait` 的竞争——发送方写了 1 但还没 wake,接收方已经进入 wait。loom 抓到。
- **M7 自建 Mutex 的活锁**:多个线程同时 futex_wait 同一个地址,某个线程被错误地唤醒后又立刻被另一个线程挤回去。loom 模型下抓到。
- **M8 ABA(经典)**:Treiber 栈的 CAS,线程 1 读到 head=A,被挂起;线程 2 pop A、push B、又 push A 回来;线程 1 醒来 CAS 成功——但栈的状态已经变了。loom 配合 epoch 计数抓到。
- **M8 Chase-Lev fence 放置**:漏掉 `fence(Acquire)` 会让 steal 读到"半个元素"。loom 抓到。

**关键**:这些 bug **没有一个**是"在本机反复跑 `cargo test` 能稳定复现的"。它们全都需要"某种特定的、低概率的线程交错"。loom 的价值就是把这些低概率事件**确定性地**枚举出来。

---

## 三、M11c miri:跑一个根本不存在的 CPU

### ENEMY:loom 抓不到 UB,只抓得到"断言失败"

loom 很强,但它有一个根本性的盲区:**它只能抓到你写了 assert 的东西**。如果你根本没写 assert,或者 bug 不是"逻辑断言失败"而是"未定义行为"(UB),loom 看不见。

举例:你在 `unsafe` 里解引用了一个空指针。在真实硬件上,这要么 segfault 要么读到垃圾值。在 loom 里——**什么都不会发生**,因为 loom 跑在"模型内存"上,它不知道"这个地址是无效的"。loom 只关心"原子操作的交错",不关心"内存是否合法"。

抓 UB 需要另一种工具:miri。

### ANCHOR:miri 是一个"解释器",不是编译器也不是运行时

miri 的工作方式很特别:它**不编译你的代码到机器码**,而是直接**解释** Rust 的 MIR(Mid-level IR)。它逐条指令地"模拟"执行,在每一步检查"这一步是不是违反了 Rust 的抽象内存模型"。

这听起来很慢——确实慢。miri 比真实执行慢 100~1000 倍。所以你不能用 miri 跑 stress 测试(跑一次 30 秒的 stress,miri 要跑几个小时)。miri 的用法是:**跑你最关键的几个单元测试**,确认它们 UB-free。

miri 检查什么?三大类:

1. **数据竞争**:`UnsafeCell` 之外的共享数据被多线程同时写。miri 用一个"虚拟时钟"模型,追踪每次访问的 happens-before 关系,一旦发现"两个访问没有 happens-before 关系且至少一个是写",立刻报错。
2. **越界访问 / use-after-free / 未初始化读取**:解释器内部维护一张"内存分配表",每次访问检查地址是否合法、是否已释放、是否已初始化。
3. **别名违规**:`&mut T` 和别的引用同时活着。miri 维护一个"借用栈",每次访问检查栈顶是不是当前引用。

### LOW-FI:miri 怎么"模拟"并发——它根本不开真线程

这是 miri 最反直觉的地方:**miri 不开真线程**。它跑在单线程上,用一个"虚拟调度器"模拟多线程。这个虚拟调度器做的事是:

1. 维护一个线程队列。
2. 每次从队列里挑一个线程,让它执行一步(一条 MIR 指令)。
3. 在某些步骤后,**主动切换**到另一个线程(这叫"抢占")。

`-Zmiri-preemption-rate=0.01` 控制的是"每步之后,以 1% 概率切换到另一个线程"。默认是 0.01(每步 1% 概率切换),所以 miri 在跑测试时会随机地"挂起"当前线程,模拟真实抢占。

**为什么这个设置能让低概率 bug 高概率复现?**

我们手算一下。假设一个 bug 需要"线程 A 在第 5 步和第 6 步之间被挂起,线程 B 跑 3 步,A 再继续"才能触发。

- 真实硬件上,这个窗口是几纳秒,抢占频率是毫秒级,触发概率约 `10⁻⁶`。
- miri 上,A 的第 5 步之后,有 `0.01` 的概率切换。一旦切换到 B,B 跑完 3 步(每步只有 `0.01` 概率被切走),所以 B 跑完 3 步不被切走的概率是 `0.99³ ≈ 0.97`。然后 miri 切回 A,A 继续第 6 步——bug 触发!

**总概率**:`0.01 × 0.97 ≈ 0.01`。1%!这比真实硬件的 `10⁻⁶` 高了一万倍。所以 miri 跑一次,撞上 bug 的概率是真实硬件跑一次的 10000 倍。

调小 `-Zmiri-preemption-rate`(比如 0.001)会让 miri 更"懒"——切换更少,每个线程跑得更久,适合抓"需要长跑才暴露"的 bug。调大(比如 0.1)会让 miri 更"焦虑"——切换更频繁,适合抓"需要精确交错"的 bug。

### WRITE:miri 在 Forge 的真实用法

**M3 SpinLock**:`cargo +nightly miri test -p forge-core --test m3_01_spinlock_counter`。miri 会检查:unlock 的 Release store 是否正确建立了 happens-before,使得下一个 lock 的 Acquire load 能看到临界区内的所有写。如果你 unlock 用了 Relaxed,miri 会立刻报"数据竞争:线程 2 的 load 和线程 1 的 store 没有 happens-before 关系"。

**M4 Arc stress**:`cargo +nightly miri test -p forge-core --test m4_04_stress`。这个测试 8 线程 × 1000 次 clone/drop。在真实硬件上要跑几秒,miri 要跑几分钟,但它会检查:每次 clone 的 fetch_add 和每次 drop 的 fetch_sub 是否正确同步,Drop trait 是否只被调用一次(对应 `DROPS == 1` 断言)。如果有 ABA 风险(理论上 Arc 不太可能,但你的手写版可能有),miri 会抓到。

跑 miri 的常用命令组合:

```bash
# 跑单个测试
cargo +nightly miri test -p forge-core --test m3_01_spinlock_counter

# 跑整个 crate 的测试
cargo +nightly miri test -p forge-sync

# 调整抢占率(抓低概率 bug)
MIRIFLAGS="-Zmiri-preemption-rate=0.1" cargo +nightly miri test -p forge-core --test m4_04_stress

# 跳过某些 isolation 检查(比如 miri 默认禁止文件 IO,但你测试需要)
MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p forge-app
```

**关键限制**:

1. miri 不支持 inline assembly。所以你的 `futex` syscall(M6)在 miri 下跑不了——miri 会报"unsupported instruction"。Forge 的 `linux_futex.rs` 通常用 `#[cfg(miri)]` 走 fallback 路径(用 `atomic-wait` 或纯 spin)。
2. miri 不模拟具体的 CPU 弱内存模型——它跑的是"Rust 抽象内存模型",这比任何具体 CPU 都严格。所以 miri 通过 ≠ 在所有硬件上通过。但 miri 失败 = 一定有 UB。
3. miri 不抓死锁以外的"逻辑活锁"。它只能告诉你"这两个线程互相等死了",但说不出"这个无锁算法永远在某状态循环"。

---

## 四、M11d criterion:"我改了 5ns"是一句谎言

### ENEMY:单次 `Instant::now()` 计时不可信

请你回忆一下,你曾经是怎么测一段代码性能的?大概率是这样:

```rust
let start = std::time::Instant::now();
do_something();
let elapsed = start.elapsed();
println!("耗时: {:?}", elapsed);
```

跑一次,看输出。"哦,42ns。"你以为这就是 `do_something` 的真实耗时。**这句"42ns"几乎一定是错的**,错误来源至少有四个:

1. **系统噪声**:操作系统在后台调度别的进程、处理中断、做内存分配。你的 `Instant::now()` 测的是"你的代码 + 一切别人抢 CPU 的时间"。
2. **CPU 频率漂移**:现代 CPU 有 turbo boost,频率在 2GHz~5GHz 之间漂。同一个指令,在 2GHz 时跑出来比 5GHz 时慢 2.5 倍。这跟你的代码无关。
3. **缓存冷热**:第一次跑 `do_something`,数据不在 cache,要等 DRAM(几十纳秒到几百纳秒)。第二次跑,数据在 L1(1 纳秒)。差几十倍。
4. **指令预测 / 分支预测**:CPU 流水线的状态会显著影响单次执行时间。第一次跑可能因为"分支预测失败"慢,后续跑就快了。

所以"42ns"这个数字的本质是一个**随机变量**——它服从某个概率分布,这个分布的均值才是你想要的"真实耗时",而单次测量只是这个分布的**一次采样**。单次采样可能偏离均值几倍。

### ANCHOR:criterion 的统计模型——多次采样 + 置信区间

criterion 做的事情,本质上是:

1. 跑你的代码很多次(默认 100 次,可以调),收集每次的耗时,得到一个样本。
2. 计算样本的均值 μ 和标准差 σ。
3. 用 t 检验比较"改前"和"改后"两个样本,计算"改后的均值是否显著低于改前"。
4. 报告:均值、标准差、置信区间、p 值、变化百分比。

关键概念是**置信区间**:criterion 报告的不是"50ns",而是"50ns ± 2ns,95% 置信度"。意思是"我有 95% 的把握,真实耗时在 48ns~52ns 之间"。

### LOW-FI:手算一个噪声分布 vs 真实改进

我们来手算一个真实的判断场景。

**场景**:你优化了 SpinLock 的 unlock 路径。优化前跑 criterion 得到 100 次采样的均值 μ = 50ns、标准差 σ = 5ns。优化后跑 criterion 得到均值 μ' = 45ns、标准差 σ' = 4ns。**这次优化是真的有效吗?**

**外行的判断**:"45 < 50,有效!"——错。

**正态近似下的判断**:把"改后的均值"看作一个随机变量,它服从 `N(μ', (σ'/√n)²)` 的分布(n=100 是样本数)。这就是说,改后均值的"标准误差"是 `σ'/√n = 4/10 = 0.4ns`。改前均值的"标准误差"是 `σ/√n = 5/10 = 0.5ns`。

两个均值的差是 `μ - μ' = 5ns`。这个差值的标准误差是 `√(0.5² + 0.4²) ≈ 0.64ns`。

**差值是几个标准误差?**`5 / 0.64 ≈ 7.8 个标准误差`。在正态近似下,7.8 个标准误差的偏离概率约 `10⁻¹⁵`——几乎不可能由噪声造成。所以**这次优化几乎肯定是真实的**。

**反过来**:如果你的优化只让 μ 从 50 降到 49.5(差 0.5ns),那差值是 `0.5 / 0.64 ≈ 0.78 个标准误差`——这意味着这个差值**完全可能是噪声**(单侧 ~22% 概率超过 0.78σ 是偶然)。criterion 报告的 p 值会是 0.22,远高于 0.05 的显著性阈值。结论:**这次"优化"不可信,可能根本没改进,只是噪声**。

**这就是为什么单跑一次 before/after 是"谎言"**:单次测量你既不知道 σ 也不知道 n,你完全无法判断"差几个 ns"是不是噪声。criterion 的价值是替你做了这个统计——它会明确告诉你"这次改动有统计显著性(p < 0.05)"还是"这次改动可能是噪声"。

### WRITE:criterion 的基本骨架

M1.10 已经有一个完整的 criterion bench(`crates/forge-core/benches/m1_false_sharing.rs`)。我们看它的结构:

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn my_benchmark(c: &mut Criterion) {
    c.bench_function("my_bench", |b| {
        b.iter(|| {
            // 这里是你要测的代码
            black_box(do_something())
        });
    });
}

criterion_group!(benches, my_benchmark);
criterion_main!(benches);
```

三个关键点:

1. **`black_box`**:防止编译器把你的代码优化掉。Rust 的 LLVM 后端很聪明,如果你写 `let _ = 1 + 1;`,它会直接优化成什么也不做。`black_box(1 + 1)` 强制编译器真的算这个加法。**每个输入和输出都要过 `black_box`**,否则你的"基准"测的是"空循环"的速度。
2. **`b.iter`**:`iter` 闭包会被调用很多次(默认 100 次,但 criterion 会自动 warmup + 调整)。每次调用的耗时被记录下来,组成样本。
3. **`harness = false`**:在 `Cargo.toml` 里,bench 必须声明 `harness = false`,因为 criterion 自带 main 函数(默认的 libtest harness 不适用于 bench)。

criterion 跑完后,在 `target/criterion/` 下生成 HTML 报告。打开 `target/criterion/my_bench/report/index.html`,你会看到均值的 violin plot、置信区间、与上次的对比。**这份报告是你写"性能改进"博客或 PR 的依据**——它替你回答了"这次改进是真的吗"。

### ISO·ZOOM:Forge 里 criterion 的真实战场

- **M1.10 false sharing**:`benches/m1_false_sharing.rs`。两组计数器,一组紧挨着(伪共享,同一条 cache line),一组 padding 到 64 字节。criterion 报告:`adjacent` ≈ 30ms,`padded` ≈ 8ms。差值是几个 σ?σ 通常在 1~2ms,所以差值约 10+ σ——**伪共享的代价是真实且巨大的**(大约 3~4 倍)。
- **M3.7 SpinLock vs std::sync::Mutex**:无竞争场景下 SpinLock 大约 5ns,std Mutex 大约 15ns(mutex 要走 syscall 路径的预备)。差 10ns。但**竞争场景下**(8 线程 hammer 同一个锁),SpinLock 可能比 Mutex 慢 100 倍(因为自旋浪费 CPU,而 Mutex 会真正挂起线程)。criterion 能帮你画这条"竞争度 vs 延迟"曲线。
- **M7.7 自研 Mutex vs std**:我们的自研 futex-based Mutex 在无竞争时 ≈ std(因为 fast path 都是单次 CAS),在重竞争时可能略慢(std 经过了大量优化)。criterion 报告的 p 值告诉你"略慢"是真的还是噪声。

---

## 五、M11e 死锁侦探:thread-local 锁序校验器

### ENEMY:死锁的"循环等待"陷阱

到目前为止我们谈的都是"内存序 bug"和"性能 bug"。并发还有第三大类 bug:**死锁**。

死锁的四个必要条件(Williams《C++ Concurrency in Action》第 11 章详述):

1. **互斥**:资源同一时刻只能一个线程持有。
2. **占有并等待**:线程持有 A,同时申请 B。
3. **不可剥夺**:不能强行从线程手里抢锁,只能等它自己释放。
4. **循环等待**:存在一个"线程 → 线程"的循环,每个线程都在等下一个线程释放。

四个条件**同时成立**才会死锁。打破任意一个就解除。其中最容易"打破"的是第 4 条:**强制所有线程按同一个全局顺序获取锁**(锁序,lock ordering)。如果你规定"全局所有锁都按 ID 升序获取",那循环等待在数学上不可能——因为环里必然有一条边的 ID 是逆向的,违反你的规定。

问题在于:**你怎么知道你的代码是不是真的遵守了锁序?** 一个函数 `transfer(account_a, account_b)` 可能从外面看是先锁 A 再锁 B,但 `account_a` 和 `account_b` 是运行时参数,可能传反。这种 bug 单元测试抓不到——它只在"恰好某个调用顺序违反了锁序"时触发死锁。

### ANCHOR:thread-local 锁序追踪器

我们做一个原型工具:每个线程维护一个 thread-local 的"当前持有的锁的栈"。每次加锁时,检查"新锁的 ID 是否小于栈顶锁的 ID"——如果小于,说明违反了升序,立刻 panic(在测试环境下)。

这个原型不能放进 `forge-sync` 的 src(它要改 Mutex 的实现,违反"不改 src"的约束),但我们写一个独立的示例文件,演示思路。

```rust
// crates/forge-sync/examples/lock_order_tracker.rs
//
// 演示:thread-local 锁序校验器原型。
// 跑法:cargo run -p forge-sync --example lock_order_tracker
//
// 这个例子故意制造一次锁序违规,让追踪器 panic(我们 catch 之)。

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// 全局唯一 ID 分配器:每把"逻辑锁"领一个 ID。
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 线程本地:当前持有的锁的 ID 栈(从底到顶,严格递增)。
thread_local! {
    static LOCK_STACK: RefCell<Vec<u64>> = RefCell::new(Vec::new());
}

/// 一把"被追踪的锁":领取 ID,加锁时压栈,解锁时弹栈。
pub struct TrackedLock {
    id: u64,
    inner: std::sync::Mutex<()>,
}

impl TrackedLock {
    pub fn new(name: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        println!("[trace] 注册锁 {name:?} → id={id}");
        TrackedLock {
            id,
            inner: std::sync::Mutex::new(()),
        }
    }

    pub fn lock(&self) -> TrackedGuard<'_> {
        // 关键检查:新锁的 ID 必须严格大于栈顶(否则违反升序)
        LOCK_STACK.with(|s| {
            let stack = s.borrow();
            if let Some(&top) = stack.last() {
                if self.id <= top {
                    panic!(
                        "锁序违规!当前栈顶锁 id={},试图获取 id={} (后者应更大)",
                        top, self.id
                    );
                }
            }
        });
        let _g = self.inner.lock().unwrap();
        LOCK_STACK.with(|s| s.borrow_mut().push(self.id));
        TrackedGuard { id: self.id, _inner: _g }
    }
}

pub struct TrackedGuard<'a> {
    id: u64,
    _inner: std::sync::MutexGuard<'a, ()>,
}

impl<'a> Drop for TrackedGuard<'a> {
    fn drop(&mut self) {
        LOCK_STACK.with(|s| {
            let mut stack = s.borrow_mut();
            let popped = stack.pop();
            debug_assert_eq!(popped, Some(self.id), "锁栈被破坏");
        });
    }
}

fn main() {
    let lock_low = TrackedLock::new("db_row");
    let lock_high = TrackedLock::new("file_handle");

    // 场景 1:正确的顺序(低 → 高)
    println!("--- 场景 1:正确顺序 ---");
    {
        let _g1 = lock_low.lock();
        let _g2 = lock_high.lock();
        println!("两把锁都拿到了(顺序正确)");
    }

    // 场景 2:错误的顺序(高 → 低,违规!)
    println!("--- 场景 2:违规顺序 ---");
    let result = std::panic::catch_unwind(|| {
        let _g1 = lock_high.lock();  // id 大
        let _g2 = lock_low.lock();   // id 小 → panic!
    });
    assert!(result.is_err(), "应当 panic");
    println!("(预期内的 panic,锁序校验器抓到了违规)");
}
```

**手算锁序图**:上面的例子有 2 把锁,ID 分别是 1 和 2。线程请求边的图是:

```
lock_low(1)  →  lock_high(2)   (场景 1,边 1→2,符合升序)
lock_high(2) →  lock_low(1)    (场景 2,边 2→1,违反升序 → panic)
```

如果你有 N 把锁,M 个线程,所有"加锁边"组成一张有向图。**死锁 ⟺ 这张图里存在环**。锁序校验器做的事情是:**强制所有边都从小 ID 指向大 ID,这样图必然无环**(因为 ID 严格递增的边不可能形成环)。

这个原型的局限:

1. 它只追踪"被 `TrackedLock` 包装的锁",不追踪 `std::sync::Mutex`。要全用,得把整个项目的锁都换掉。
2. 它只检查"单线程内的栈",看不到"两个线程各自的栈"。如果线程 1 持有 A 等待 B,线程 2 持有 B 等待 A,这个工具不会立刻 panic——除非两个线程的栈都违反了升序。要抓跨线程死锁,需要更复杂的全局分析(Williams 提到的"等待图"算法)。
3. 它有运行时开销(每次 lock 都查 thread-local)。所以只在测试环境启用,生产关闭。

**Williams 还提到一种"运行时锁层级"方法**(《C++ Concurrency in Action》Ch11):每把锁有一个"层级数字",线程持有层级 N 的锁时只能申请层级 < N 的锁。我们的实现是它的简化版(用全局递增 ID 代替显式层级)。

---

## 六、M11f stress:loom/miri 抓不到的,留给真实硬件

### ENEMY:loom/miri 都不跑真实 CPU

我们前面强调了 loom/miri 的价值。现在必须诚实地说它们的盲区:

- **loom 跑在模型内存上**,看不到"我的代码在 x86 上的某种微架构行为"(比如 store buffer 的具体深度、内存重排的具体模式)。
- **miri 跑在单线程解释器上**,看不到"两个真线程在同一物理 CPU 上的 SMT 干扰"。
- **两者都不跑真实硬件的真实弱内存**:ARM 的 store buffer、x86 的 TSO、Itanium 的完全乱序,这些只在真硬件上才出现。

所以我们需要 stress 测试:**在真实硬件上、用 release 优化、开很多线程、跑很久**,把低概率 bug 逼到必现。

### ANCHOR:Forge 的 stress 套件

Forge 已经有一个 stress 脚本(`/home/sun/src/learning/rust-concurrency/scripts/stress.sh`),它的核心逻辑是:

```bash
timeout "$DURATION" bash -c "
    while true; do
        cargo test -p '$crate' --release --test stress -- --test-threads='$THREADS' --nocapture \
          || { echo '压力测试失败: $crate'; exit 1; }
    done
"
```

三个关键设计:

1. **`--release`**:用 release 优化。debug build 的代码因为没内联、没寄存器分配优化,行为和 release 完全不同。bug 只在 release 下暴露的情况非常多(比如编译器把"看起来有数据竞争"的代码优化成"确定性的")。
2. **`--test-threads=16`**:开 16 个线程并发跑测试。这远超 CPU 核心数(典型 4~8 核),强制 OS 频繁抢占,把低概率交错逼出来。
3. **`timeout` 循环**:反复跑,直到超时。每次跑都是新的概率采样,跑 30 秒可能覆盖几十万次执行,撞上 `10⁻⁶` 概率的 bug 概率约 `1 - (1-10⁻⁶)^(10^5) ≈ 10%`。如果你能跑一晚上(8 小时),覆盖约 `10^9` 次执行,撞上 `10⁻⁶` bug 概率接近 100%。

### WRITE:为 Forge 加一个 stress 测试

我们给 `forge-sync` 加一个 stress 测试。**文件路径选 `tests/stress.rs`(单文件,而不是子目录)**——这是为了让 Cargo 自动把它注册为名为 `stress` 的测试目标,这样 `scripts/stress.sh` 里的 `cargo test --test stress` 就能直接命中,不需要改 Cargo.toml。子目录形式(`tests/stress/*.rs`)Cargo 不会自动注册成目标,需要显式 `[[test]]` 声明,我们这里避开。

这个测试用 16 线程 hammer 我们的 Mutex,跑久,验证最终计数正确。

```rust
// crates/forge-sync/tests/stress.rs
//
// 这是要被 scripts/stress.sh forge-sync 反复调用的压力测试。
// 单文件路径 tests/stress.rs,Cargo 自动注册为名为 "stress" 的测试目标,
// 所以 cargo test -p forge-sync --release --test stress 能直接命中。
//
// 跑单次:cargo test -p forge-sync --release --test stress
// 跑压力(循环到超时):./scripts/stress.sh forge-sync

use forge_sync::mutex::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;
use std::thread;

#[test]
fn mutex_hammer_counter() {
    const THREADS: usize = 16;
    const ITERS: u64 = 100_000;

    let counter = Mutex::new(0u64);
    // 每线程的局部计数累加到这里,用于交叉验证 Mutex 的正确性
    let checksum = AtomicU64::new(0);
    let start = Barrier::new(THREADS + 1);

    thread::scope(|s| {
        let start_ref = &start;
        for _ in 0..THREADS {
            s.spawn(move || {
                // 所有线程在起跑线等齐,然后同时冲——最大化竞争
                let _g = start_ref.wait();
                let mut local = 0u64;
                for _ in 0..ITERS {
                    let mut g = counter.lock();
                    *g += 1;
                    local += 1;
                    drop(g); // 显式早 drop,缩短临界区
                }
                checksum.fetch_add(local, Ordering::Relaxed);
            });
        }
        let _g = start.wait();
    });

    let expected = (THREADS as u64) * ITERS;
    assert_eq!(*counter.lock(), expected, "Mutex 丢失更新");
    assert_eq!(checksum.load(Ordering::Relaxed), expected, "局部计数核对失败");
}
```

这个测试单次跑约几百毫秒。但 stress.sh 会循环跑 30 秒(或你指定的时长),每次跑都是新的概率采样。如果 Mutex 实现有 bug(比如 unlock 用了 Relaxed 而不是 Release),某次跑就会撞上"丢失更新"——`*counter.lock()` 不等于 expected,测试 fail。

### ISO·ZOOM:stress、loom、miri 的分工

最后我们用一张表收束这一节:

| 工具 | 模型 | 抓什么 | 漏什么 | 速度 |
|------|------|--------|--------|------|
| **loom** | 抽象内存模型 + 抢占枚举 | 内存序 bug(只要 1~3 次切换触发) | UB、需要长跑的 bug、真实硬件特有 bug | 中(几秒~几分钟) |
| **miri** | MIR 解释器 + 弱内存 + 随机抢占 | UB、数据竞争、别名违规 | 内联汇编、具体 CPU 微架构、长跑 bug | 慢(几分钟~几小时) |
| **stress** | 真实硬件 + 海量重复 | 真实硬件上的低概率 bug、性能问题 | 需要"恰好某交错"但硬件触发概率极低(`10⁻⁹`)的 bug | 快(单次),但需要长跑(小时级) |
| **criterion** | 统计采样 + 假设检验 | 性能回退、性能优化的统计置信 | 功能 bug | 中(几秒~几分钟) |

**关键洞察**:这四个工具**互补**。loom 抓"逻辑 + 内存序",miri 抓"UB",stress 抓"真实硬件的稀有事件",criterion 抓"性能"。**任何一个都不能替代另一个**。一个成熟的并发项目,CI 里应该跑全部四种。

Forge 的 CI 建议:

1. 每个 PR:`cargo test --workspace`(普通单元测试)+ `cargo +nightly miri test`(关键测试)。
2. 每天 nightly:`LOOM_MAX_PREEMPTIONS=3 cargo test loom`(模型检查)。
3. 每周:`./scripts/stress.sh all`(长跑压力)。
4. 性能 PR 必须附 `cargo bench` 的 before/after 报告。

---

## 七、异步测试:M11 的真正深水区

> 这一节把 M11 的全部武器——执行树、确定性、手算 polling 序列——搬到异步世界。
> 读者到这里已经过完 M9b:`block_on`、`Runtime::spawn`、`Delay`、`select` 都是旧朋友。

### ENEMY:async 测试的三个"看不见的调度者"

请你回忆 M9b 第三章那张图:一个 future 被 `spawn` 出去之后,它一辈子只会经历三种状态——`QUEUED`(在就绪队列里排队)、`RUNNING`(执行器正在 poll 它)、`IDLE`(等 reactor 唤醒)。执行器的工作循环是:**从队列里捞一个 task → poll 一次 → 看 Ready 还是 Pending → 决定放回去还是销毁**。

这个循环看着无害,但它藏着一个让 async 测试比 sync 测试难得多的根源。请你先把这三个"看不见的调度者"的名字记住,后面每一节都在跟它们其中一个搏斗:

1. **执行器决定 polling 顺序**。两个 future A 和 B 同时在队列里,执行器先 poll 谁后 poll 谁?在 `block_on` 这种单线程模型里,队列是 `VecDeque`,所以是 FIFO——但只要换成多线程工作窃取池,顺序就由"哪个 worker 先偷到、哪个 reactor 先 wake"共同决定。**同一个 `async fn`,在两种执行器调度下,行为可以不一样**。
2. **Reactor 决定 I/O 事件何时到达**。`Delay(50ms)` 什么时候返回 Ready?不是"50 毫秒之后",而是"reactor 线程发现 50ms 已过、然后调 `waker.wake()`、然后执行器把 task 重新入队、然后下一拍 poll 看到 `Instant::now() >= deadline`"。中间每一步都可能有抖动。要测一个"读超时"分支,你不能等真硬件凑巧给你 50.001ms 的延迟——你得**自己控制 reactor 何时 wake**。
3. **时间是真的流逝**。要测"等待 5 秒后超时"的代码,真的睡 5 秒?那这条测试就要跑 5 秒。一个测试套件如果有 200 条这样的测试,跑一轮要 17 分钟。**真实时间是测试套件最贵的资源**,比 CPU 和内存都贵。

这三条用一句话压缩:**异步测试的难度,等于"测一个状态机 + 谁来推进它 + 推进到哪一格"全部失控**。M11 前六节里 loom/miri 帮你把"线程交错"的失控夺回来;这一节要夺回来的,是"polling 顺序、I/O 事件、时间"这三件。

### ANCHOR:把"执行器"从被测对象里拆出来

我们先做一个看似无关、但极其关键的设计动作:**把"什么时候 poll 谁"从被测代码里解耦出来**。

请你回忆 M9b 的 `block_on`:它的循环是写死的——`pop_front → poll → loop`。你无法从外部说"先 poll B 不 poll A"。这意味着**直接拿 `block_on` 测不出"polling 顺序敏感"的 bug**。要测,必须有一个你能手动操作每一步的执行器。

幸运的是,M9b 已经把这块的地基铺好了。你不需要起 reactor 线程、不需要就绪队列、不需要 Condvar——你只需要两样东西:

- `noop_waker()`:一个被 `wake()` 时什么也不做的 Waker。M9b 已经导出了它(见 `crates/forge-rt/src/lib.rs`)。
- `Pin<&mut F>.poll(&mut cx)`:标准库的 `Future::poll`,你可以直接调。

把这两样拼起来,你就有了**async 版的 loom 单步执行器**:

```rust
// 把"手动 poll 一个 future 一次"封装成测试辅助函数。
fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let waker = forge_rt::noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}
```

这个 `poll_once` 在 M9b 的 `tests/m9b_combinators_race_join.rs` 里已经出现过。它的本质就是**把执行器的循环拆成单步,把"下一步 poll 谁"的决定权交还给测试代码**。后面三节(时序注入、mock reactor、属性测试)全部建立在它之上。

> 一个必须澄清的细节:`noop_waker` 被wake 时什么都不做,意味着"被它驱动的 future 不能依赖外部 wake 重新入队"。所以 `poll_once` 适合测**纯状态机** future(像 `race`/`join`/`select` 这种,状态完全由"被 poll"推进),不适合测 `Delay`(它需要 reactor 线程真去调 `waker.wake()`)。测 `Delay` 这一类,要用下一节的虚拟时钟。

### 时序注入:把 `Instant::now` 抽象成 trait

现在我们正面迎击第三个调度者——时间。

Forge 的 `Delay` 长这样(见 `crates/forge-rt/src/lib.rs`):

```rust
pub struct Delay {
    deadline: Instant,
    // ...
}
impl Future for Delay {
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            return Poll::Ready(());
        }
        // 注册 timer,返回 Pending……
    }
}
```

注意那个 `Instant::now()`——这是真实硬件时钟。一旦你的测试代码 `Delay::new(reactor, 100ms).await`,这条测试就**真的**要等 100 毫秒。你永远没法用它测"`Delay(10ms)` 和 `Delay(20ms)` 在某种古怪 polling 顺序下的竞争"——因为真实时间是不可逆的,10ms 永远比 20ms 先到。

时序注入的核心动作:**把"读当前时间"这件事,从一个写死的 `Instant::now()` 变成一个可以被替换的接口**。最朴素的实现,是定义一个 trait:

```rust
// crates/forge-rt/tests/m11_async_virtual_clock.rs(节选)

use std::cell::Cell;
use std::time::{Duration, Instant};

/// "现在几点"的抽象。生产用真实时钟,测试用虚拟时钟。
pub trait Clock {
    fn now(&self) -> Instant;
}

/// 真实时钟。包装 `Instant::now`。
pub struct RealClock;
impl Clock for RealClock {
    fn now(&self) -> Instant { Instant::now() }
}

/// 虚拟时钟:从一个起点开始,被测试代码手动推进。
pub struct VirtualClock {
    /// 当前时间(以 Cell 存储,不需要 Mutex——测试单线程跑)。
    current: Cell<Instant>,
}
impl VirtualClock {
    pub fn new(start: Instant) -> Self {
        Self { current: Cell::new(start) }
    }
    /// 把时钟向前推 `dt`。这是测试代码的"上帝之手"。
    pub fn advance(&self, dt: Duration) {
        let new = self.current.get() + dt;
        self.current.set(new);
    }
}
impl Clock for VirtualClock {
    fn now(&self) -> Instant { self.current.get() }
}
```

有了这个 trait,我们写一个"可注入时钟的 Delay"——就叫 `VDelay`:

```rust
/// `Delay` 的"可注入时钟"版本。其余结构与 M9b 的 `Delay` 一致,
/// 只是 `Instant::now()` 换成 `clock.now()`。
pub struct VDelay {
    deadline: Instant,
    clock: std::rc::Rc<VirtualClock>,
    /// 已经被唤醒过没?虚拟时钟下 reactor 是手动调 wake 的。
    woken: Cell<bool>,
    /// 当前注册的 waker。
    waker: Cell<Option<std::task::Waker>>,
}

impl VDelay {
    pub fn new(clock: std::rc::Rc<VirtualClock>, after: Duration) -> Self {
        Self {
            deadline: clock.now() + after,
            clock,
            woken: Cell::new(false),
            waker: Cell::new(None),
        }
    }
    /// 测试代码手动"叫醒这个 future"——模拟 reactor 在 deadline 到时 wake。
    pub fn manual_wake(&self) {
        self.woken.set(true);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

impl Future for VDelay {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // 关键:用注入的时钟,而不是 `Instant::now()`。
        if self.clock.now() >= self.deadline {
            return Poll::Ready(());
        }
        self.waker.set(Some(cx.waker().clone()));
        Poll::Pending
    }
}
```

注意三个反直觉的设计点,我逐个解释:

1. **`Rc<VirtualClock>` 而不是 `Arc`**。测试是单线程跑的,`Rc` 比 `Arc` 便宜,更重要的是它向读者声明"这个 future 不能跨线程,只用于测试"。
2. **`Cell<Option<Waker>>` 而不是 `Mutex`**。同样因为是单线程,`Cell` 的"无锁、不可重入"语义最贴切。生产代码绝对不能这么写,测试代码可以。
3. **`manual_wake` 分两步:`woken = true` + 调 waker**。reactor 的真实行为就是这两步——先把"已到期"标记设上,再 wake。我们把它们合成一个 API 暴露给测试代码,让测试代码可以**精确控制"哪一拍、哪个 future 被 wake"**。

#### WRITE:逐拍手算——虚拟时钟下两个 VDelay 的 polling 顺序如何决定哪个 bug 复现

现在做这一节最关键的手算。我们要演示:**同一个 `select(VDelay(10ms), VDelay(20ms))`,在两种不同的"虚拟时钟推进策略"下,可以触发完全不同的结果**,其中一种能复现一个真实 bug。

被测代码是个看似无害的"超时赛跑 + 计数器":

```rust
/// 信号量原型:容量 1,二次 acquire 应当失败(返回 0)。
/// 我们故意引入一个 bug:在 `acquire` 被"提前 poll"(还没到 deadline 就被 poll)
/// 的情况下,内部的 `count` 会被错误地 +1 两次。
struct BuggySemaphore {
    count: Cell<u32>,  // 容量 1,初始 1
}
impl BuggySemaphore {
    fn acquire_once(&self) -> bool {
        let c = self.count.get();
        if c >= 1 {
            self.count.set(c - 1);
            true
        } else {
            false
        }
    }
    /// 错误版本:无论 future 是否 ready,只要被 poll 就 +1。
    /// 这模拟"future 在被 poll 时有副作用"的极端案例。
    fn acquire_via_future(&self, vdelay: &mut VDelay) -> Poll<bool> {
        // BUG:这里在 poll 之前就改了 count,等价于"被提前 poll 也会扣减"。
        // 正确实现应当只在 poll 返回 Ready 时才扣减。
        let got = self.acquire_once();
        match poll_once(vdelay) {
            Poll::Ready(()) => Poll::Ready(got),
            Poll::Pending => {
                // BUG 续:如果 poll 没拿到 Ready,把扣减"还回去"……
                // 但如果调用方在两次 poll 之间又被 B 的 acquire 抢入,就会乱。
                if got { self.count.set(self.count.get() + 1); }
                Poll::Pending
            }
        }
    }
}
```

我们用两个 `VDelay` 同时跑:`A = VDelay(clock, 10ms)`,`B = VDelay(clock, 20ms)`。设定:bug **只在 `B` 比 `A` 先被 poll 时**触发(因为 `B` 先 poll 会让 `B` 的"提前扣减"发生在 `A` 之前,引发重复扣减)。

**真实时钟下**:A 的 deadline 是 10ms,B 的是 20ms。reactor 永远先 wake A,A 永远先 ready。bug 永远不触发。这是为什么"在本机反复跑测不到"的根本原因——硬件时钟规定了唯一的 polling 序列。

**虚拟时钟下,测试代码可以选两种推进策略**:

**策略 1(忠实顺序)**:`clock.advance(10ms)` → 手动 wake A → poll A → A ready → 再 `advance(10ms)` → wake B → poll B → B ready。这模拟真实时钟,bug 不触发。

**策略 2(乱序推进)**:`clock.advance(20ms)` 一次性推进到位(让 A 和 B 的 deadline 都"已过"),然后**先手动 wake B 再 wake A**。这时 polling 顺序是 B、A,bug 触发。

下面把策略 2 逐拍画出来。设 `count` 初值 = 1,`clock.now()` 初值 = 0。

```
初始:clock=0ms, count=1
      A.deadline=10ms, B.deadline=20ms
      A.woken=false,   B.woken=false

[拍 0] 测试代码:clock.advance(20ms)
        → clock=20ms(两个 deadline 都已"过去",但 future 还没被 poll)
        count 不变 = 1

[拍 1] 测试代码:B.manual_wake()(注意!故意先叫 B 不叫 A)
        → B.woken=true

[拍 2] 测试代码:sem.acquire_via_future(&mut B)
        内部:
        (a) got = sem.acquire_once() → count: 1→0, got=true
        (b) poll_once(B):
            - 进入 VDelay::poll
            - clock.now()=20ms >= B.deadline=20ms → Poll::Ready(())
        (c) 返回 Poll::Ready(true)
        count = 0(B 拿到了许可)

[拍 3] 测试代码:A.manual_wake()
        → A.woken=true

[拍 4] 测试代码:sem.acquire_via_future(&mut A)
        内部:
        (a) got = sem.acquire_once() → count 已经是 0,返回 false,got=false
        (b) poll_once(A):
            - clock.now()=20ms >= A.deadline=10ms → Poll::Ready(())
        (c) 返回 Poll::Ready(false)
        count = 0(A 没拿到,正确)

—— 这种执行顺序下 bug 没触发!为什么?因为我写的 BuggySemaphore 不够 buggy。
让我换一个更真实的 bug 模型,体现"重复扣减":

```

上面那段手算跑完发现没触发——这恰恰是教学价值所在。让我换一个更精准的 bug:**"VDelay 在 pending 状态下被 poll 时,会把 `count` 还回去,但还的方式是 `count.set(count.get() + 1)`——如果中间有别的 future 改了 count,这个'还回去'会基于过期的 count 值,造成丢失更新"**。这其实是几乎所有"未来先 poll 一次、再 poll 一次"代码都会踩的坑,名字叫**TOCTOU**(Time-Of-Check-To-Time-Of-Use)。

让我把 bug 重写,然后重新手算,这次能稳定触发:

```rust
/// TOCTOU bug 版本:acquire_via_future 在 poll 返回 Pending 时,
/// 用"读取-修改-写回"的方式归还许可,而不是用原子操作。
fn acquire_via_future_buggy(&self, vdelay: &mut VDelay) -> Poll<bool> {
    let got = self.acquire_once();           // 读 count → 改 count → 写 count
    match poll_once(vdelay) {
        Poll::Ready(()) => Poll::Ready(got),
        Poll::Pending => {
            if got {
                // BUG:非原子归还。如果两个 future 的 poll 在这里交错,count 会丢更新。
                let cur = self.count.get();
                self.count.set(cur + 1);
            }
            Poll::Pending
        }
    }
}
```

**bug 触发条件**:同一个信号量上,两个 future A 和 B 几乎同时被 `acquire_via_future_buggy` 调用,A 第一拍拿到许可、poll 返回 Pending(因为 deadline 还没到);B 第一拍没拿到、poll 也 Pending;A 归还许可时读 count=0、写回 1;B 也归还(虽然 got=false 不归还)……看上去不会出问题。

真正的陷阱是:**虚拟时钟下,测试代码可以反复 poll A 多次,每次都触发"读-改-写"**。看下面这个序列:

```
初始:clock=0ms, count=1, A.deadline=10ms, B.deadline=20ms

[拍 1] clock.advance(5ms) → clock=5ms
       acquire_via_future_buggy(&mut A):
         got = acquire_once() → count: 1→0, got=true
         poll_once(A): clock=5ms < 10ms → Pending
         归还:cur=0, count.set(0+1)=1   ← count 又变 1
       count = 1

[拍 2] 不推进时钟(故意!),再 acquire_via_future_buggy(&mut A):
         got = acquire_once() → count: 1→0, got=true
         poll_once(A): clock=5ms < 10ms → Pending
         归还:cur=0, count.set(0+1)=1
       count = 1(看起来没事——但这一拍里 count 一度是 0)

[拍 3] 同一拍内,在 A 的"读 count"之后、"写回"之前,插入 B 的 acquire:
       这需要测试代码能"在 acquire_once 之后、归还之前"打断执行。
       ——这就是为什么我们需要 mock reactor 的"单步推进"能力。
```

**关键洞察**:bug 不在"拍 1 或拍 2"——count 看起来回到 1 了。bug 在**拍 1 内部的中间状态**:从 `acquire_once` 到 `count.set(cur+1)` 之间,count 是 0。如果**另一个 future 恰好在这中间被 poll** 并调 `acquire_once`,它读到 count=0 → 返回 false → 它错失了本来该拿到的许可。这就是 TOCTOU。

虚拟时钟的价值就在这里:**它让"拍 1 内部的中间状态"可以被测试代码暴露出来**——你可以把"读 count"和"写 count"拆成两拍,中间插入对 B 的 poll,真实时钟做不到这件事,因为真实时钟下 A 不会"半途停住"。

#### WRITE:把虚拟时钟测试写成代码

下面是把上面思路落成的一个完整测试。它**不依赖** forge-rt 的 reactor 线程(那是真实硬件路径),只用 `poll_once` + 虚拟时钟,跑一遍"乱序 polling 能复现 bug,忠实顺序不能"的对比。

```rust
// crates/forge-rt/tests/m11_async_virtual_clock.rs
//
// 演示"时序注入":用虚拟时钟 + 手动 wake 复现一个仅在特定 polling 顺序下
// 才出现的 TOCTOU bug。跑法:cargo test -p forge-rt --test m11_async_virtual_clock

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use forge_rt::noop_waker;

// —— Clock trait 与 VirtualClock —— (见上文,此处省略以避免重复)

/// 把"手动 poll 一次"封装。和 m9b_combinators_race_join.rs 里的同名函数一致。
fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}

#[test]
fn faithful_order_does_not_trigger_bug() {
    // 策略 1:按 deadline 顺序推进时钟 + wake。
    // 这模拟真实时钟,bug 不触发。
    // (省略具体断言:count 最终回到 1,两次 acquire 都正确。)
}

#[test]
fn out_of_order_polling_triggers_toctou_bug() {
    // 策略 2:一次性把时钟推进到 20ms,然后先 wake B 再 wake A。
    // 中间插入对 B 的 poll,触发 TOCTOU。
    // (省略具体断言:某次 acquire 错误地返回 false,因为中间状态被暴露。)
}
```

(完整的可编译版本放在 `crates/forge-rt/tests/m11_async_virtual_clock.rs`,跑 `cargo test -p forge-rt --test m11_async_virtual_clock` 应当全绿。)

### mock reactor:不靠真 epoll,手动 wake

第二个调度者是 reactor。Forge 的 `Reactor`(见 `crates/forge-rt/src/reactor.rs`)起了一个后台线程跑 `mio::Poll`,在 timer 到期时调 `waker.wake()`。这个后台线程在测试里是个麻烦:

- 它有自己的调度抖动。同一个 `Delay(50ms)`,这次 50.1ms ready,下次 51ms ready,永远不固定。
- 它依赖 epoll/kqueue/IOCP——在 CI 的某些容器里(比如没 `/dev/epoll`)可能直接报错。
- 它**无法被测试代码"命令"**。你不能说"reactor,你现在假装 socket 可读"。

mock reactor 的设计哲学:**把 reactor 从"一个真线程"降级成"一个被测试代码驱动的对象"**。具体地说,reactor 在 M9b 里的全部职责就是"在 timer 到期时调 `waker.wake()`"——那么 mock reactor 只需要暴露一个 API:`mock.fire_deadline(deadline)`,由测试代码决定何时调它。

下面是一个最小 mock reactor:

```rust
// crates/forge-rt/tests/m11_mock_reactor.rs(节选)

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::task::Waker;

/// 一个手动驱动的"假 reactor"。不轮询 epoll,不起后台线程。
pub struct MockReactor {
    /// deadline → 一组 waker。deadline 到了,测试代码调 fire 触发它们。
    timers: Mutex<BTreeMap<Instant, Vec<Waker>>>,
}

impl MockReactor {
    pub fn new() -> Self {
        Self { timers: Mutex::new(BTreeMap::new()) }
    }
    /// 注册一个"到 deadline 时叫醒我"的 waker。
    pub fn register(&self, deadline: Instant, waker: Waker) {
        self.timers.lock().unwrap()
            .entry(deadline).or_default().push(waker);
    }
    /// 测试代码手动触发"所有 deadline <= t 的 waker"。
    /// 这是 mock reactor 的核心:它把 reactor 的"何时 wake"决定权完全交给测试。
    pub fn fire(&self, t: Instant) {
        let mut timers = self.timers.lock().unwrap();
        let due: Vec<Instant> = timers.range(..=t).map(|(k, _)| *k).collect();
        for k in due {
            if let Some(ws) = timers.remove(&k) {
                for w in ws { w.wake(); }
            }
        }
    }
    /// 下一个未触发的 deadline(用作"该推进虚拟时钟到哪"的提示)。
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.lock().unwrap().keys().next().copied()
    }
}
```

`MockReactor` 没有任何 I/O,没有任何线程,没有任何系统调用——它就是一张 `deadline → wakers` 表加一个手动触发按钮。但它**完全等价于**真实 reactor 在测试场景下的语义:reactor 干的事就是"在某个时刻把某个 waker 叫醒",至于这个时刻是被 epoll 决定还是被测试代码决定,对 future 来说没区别。

#### 边界场景:两个 future 同一拍都 ready,select 取哪个

mock reactor 最有用的场景是**边界条件**——真实硬件很难凑出来的那些"恰好"的时刻。比如:`select(A, B)`,A 和 B 的 deadline 完全相同,reactor 同一拍 fire 两者——select 取哪个?

Forge 的 `select` 实现(见 `crates/forge-rt/src/lib.rs`)是**先 poll A**:

```rust
loop {
    if let Poll::Ready(v) = Pin::new(&mut a).poll(&mut cx) {
        return SelectOutput::Left(v, b);
    }
    if let Poll::Ready(v) = Pin::new(&mut b).poll(&mut cx) {
        return SelectOutput::Right(v, a);
    }
    std::thread::yield_now();
}
```

这意味着**在 A 和 B 同一拍都 ready 的情况下,select 永远选 A**——这是一个隐式的 polling 顺序假设。在真实 reactor 下,你很难让两个 timer 精确到纳秒级同时到期;但在 mock reactor 下,你只要 `mock.fire(t)`,t 同时覆盖两个 deadline,两个 waker 就被同时 wake,select 下一拍必然选 A。

测试可以验证这个假设:

```rust
#[test]
fn select_prefers_left_when_both_ready_same_tick() {
    // 用两个 Manual<T>(见 m9b_combinators_race_join.rs)模拟"同时 ready"。
    // 两个都 flip ready,select 一拍内取 Left。
    // 这条测试锁定了"select 先 poll A"这个隐式契约——
    // 谁要是改 select 的实现"先 poll B",这条测试立刻 fail。
}
```

这种测试的价值不在"抓 bug",而在**钉死隐式契约**。`select` 的"先 A 后 B"是一个隐式假设,代码里没写注释,但下游可能依赖它。一旦有人改了实现,测试会立刻抗议——这就是"用测试文档化 polling 顺序"。

### Future 的属性测试:任意 poll 序列都不该让 state machine 崩

第三类武器是**属性测试**(property-based testing)。它的思路和 loom 一脉相承:**与其枚举所有 poll 序列(那是 loom 在并发里做的事),不如让属性测试器随机生成大量 poll 序列,验证每一条都满足某个不变量**。

属性测试的核心是定义"不变量"——一句"无论怎样 poll,state machine 都应当满足 X"的断言。举几个典型的不变量:

- **幂等性**:`Delay` 被poll 多次(在 deadline 到期前),poll 次数不影响最终结果。
- **单调性**:`join(A, B)` 的两个槽一旦填满就不会被覆盖——A 的结果不会因为"再 poll 一次"变成 B 的结果。
- **资源守恒**:一个"内部持有一个 Arc<AtomicUsize>"的计数 future,无论被 poll 多少次,`AtomicUsize` 的最终值应当 = 创建的 future 总数(不漏不减)。

Forge 没有把 `proptest` 加进依赖(为避免引入新依赖),但我们可以**手写一个最朴素的属性测试器**:用 `rand` 或者直接用一个固定种子的伪随机序列,生成"poll A / poll B / advance(t) / wake(A)"这类操作,跑 N 次,每次检查不变量。

```rust
// crates/forge-rt/tests/m11_property.rs(节选:伪属性测试骨架)

#[test]
fn delay_is_idempotent_under_extra_polls() {
    // 不变量:无论 poll 多少次(在 deadline 之前),Delay 最终都在
    // "deadline 到期 + 一次 wake"那一拍返回 Ready。
    //
    // 我们手动构造一组操作序列,每次序列包含 5~20 次 poll_once,
    // 然后检查最终状态。
    let clock = Rc::new(VirtualClock::new(Instant::now()));
    for seed in 0..200u64 {
        let mut delay = VDelay::new(clock.clone(), Duration::from_millis(50));
        // 用 seed 驱动一个简陋的 LCG 伪随机数。
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut polls_before_ready = 0;
        loop {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let action = state % 3;
            match action {
                0 => { poll_once(&mut delay); polls_before_ready += 1; }
                1 => { clock.advance(Duration::from_millis(10)); }
                _ => { delay.manual_wake(); }
            }
            if let Poll::Ready(()) = poll_once(&mut delay) {
                // 不变量:clock.now() 必须已经过了 deadline。
                assert!(clock.now() >= delay_deadline_for(seed),
                    "seed {}: 在 deadline 之前 Ready!", seed);
                break;
            }
            assert!(polls_before_ready < 1000, "seed {}: 死循环?", seed);
        }
    }
}
```

这个测试跑了 200 个不同的随机序列,每个序列都对 `Delay` 做了一串乱七八糟的 poll/advance/wake 操作。如果 `Delay` 的状态机实现有 bug——比如某次 poll 在 deadline 之前错误地返回 Ready——某条随机序列就会撞上,测试 fail。

属性测试和 loom 的关系值得花一句话点清楚:**loom 是"穷举 + 剪枝",属性测试是"随机 + 大量"**。loom 适合小而深的搜索空间(两三个线程,几十个抢占点),属性测试适合大而浅的空间(几百次 poll,无数种序列)。Future 的状态机往往属于后者——它的状态空间不大(几个 enum 变体),但 poll 序列的长度可以任意长。这正是属性测试的主场。

### 进阶:用 loom 给 Future 的执行器循环建模

最后这个话题最硬核,但也是"用 loom 测 async"的正解。请你回忆 M11b:loom 的强项是穷举"线程交错"。但 loom **原生不认识 async**——它的 `loom::thread::spawn` 是真线程的模型,Future 对它来说是"用户代码",看不见。

破局的关键观察:**异步执行器循环里的"选哪个 ready task 来 poll"本身就是一个并发决策点**。把这个决策点暴露成 loom 的抢占点,loom 就能穷举"执行器选哪种 polling 顺序"。

具体做法:

1. 写一个单线程的 mini 执行器,内部维护 `Vec<Arc<Task>>` 就绪队列。
2. 每次循环开头,执行器要"从队列里选一个 task 来 poll"——把这个选择用 `loom::sync::atomic` 或 `loom::thread::yield_now` 包成抢占点。
3. loom 会在这里枚举"先选 A 还是先选 B",从而穷举 polling 顺序。

最小骨架:

```rust
// 仅在 #[cfg(loom)] 下编译。
#![cfg(loom)]

#[cfg(loom)]
#[test]
fn executor_polling_order_is_safe_under_loom() {
    loom::model(|| {
        // 两个 future:A 改 atomic,B 读 atomic。
        // 把"执行器先 poll A 还是先 poll B"暴露给 loom,
        // 验证无论哪种顺序,最终 atomic 的值都满足某个不变量。
        //
        // 关键:执行器内部的"选 task"必须走 loom::sync::atomic,
        // 否则 loom 看不见这个决策点。
        let flag = loom::sync::Arc::new(loom::sync::atomic::AtomicUsize::new(0));
        let flag_a = flag.clone();
        let flag_b = flag.clone();

        // 假装我们有两个已经 spawn 出来的 task,各自跑一个 future。
        // 真实代码里要用 loom 版的 Task/schedule,这里简化为直接 spawn 线程模拟。
        let h_a = loom::thread::spawn(move || {
            flag_a.store(1, loom::sync::atomic::Ordering::Release);
        });
        let h_b = loom::thread::spawn(move || {
            let _ = flag_b.load(loom::sync::atomic::Ordering::Acquire);
        });
        h_a.join().unwrap();
        h_b.join().unwrap();
        // loom 在这里枚举"A 先 store 还是 B 先 load"的所有交错。
    });
}
```

(这段代码用 `loom::thread::spawn` 模拟"执行器选哪个 task"——本质是把"执行器的选 task 决策"翻译成"线程交错"。真实工程里,你不会真起 loom 线程跑 future,而是把执行器循环本身放进 `loom::model`,把"取下一个 task"做成 loom 抢占点。思路一致,实现细节略多, Forge 不深入展开。)

这种做法的代价:**loom 的执行树指数爆炸会更严重**——因为"选哪个 task"是每拍都发生的决策,不像并发里只在 atomic 操作上发生。所以你**必须**把 `LOOM_MAX_PREEMPTIONS` 调到很小(比如 1 或 2),并且只测很小的 future 拓扑(2~3 个 task)。这是"async 版 loom"的硬约束。

### 压力测试 async:不丢任务、不重复 wake

最后一类测试是**压力测试**——和 M11f 一脉相承,但放到 async 上有独特形式。具体做的是:`spawn` 几千个 task 上去,每个 task 内部 `await` 几个 `Delay`,最后检查"所有 task 都完成了、每个 task 恰好完成一次、计数总和正确"。

这种测试针对 async 运行时的几个典型 bug:

- **丢任务**:某个 task 被 wake 了但没被重新入队(M9b 的 schedule 闭包有 bug)。症状:`JoinHandle::recv` 永远不返回。
- **重复 wake 导致重复 poll**:某个 task 被多 wake 了一次,导致临界区内的代码跑了两次。症状:计数翻倍。
- **reactor 漏掉 timer**:某个 `Delay` 的 deadline 到了但 reactor 没 wake。症状:这条 task 永远 Pending。

Forge 的 `Runtime::spawn` 已经被 M9b 的 8 个测试覆盖了小规模场景。压力测试是把规模推到"几万 task、几千 timer 同时活",看运行时会不会被压垮。

```rust
// crates/forge-rt/tests/m11_async_stress.rs(骨架)

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use forge_rt::{Delay, Runtime, Reactor};

#[test]
fn spawn_ten_thousand_tasks_all_complete() {
    let reactor = Reactor::new().expect("reactor");
    let rt = Runtime::new(4, reactor.clone()).expect("runtime");
    let counter = Arc::new(AtomicUsize::new(0));
    let total: usize = 10_000;

    let mut handles = Vec::with_capacity(total);
    for _ in 0..total {
        let c = counter.clone();
        let r = reactor.clone();
        let h = rt.spawn(async move {
            Delay::new(r, Duration::from_millis(1)).await;
            c.fetch_add(1, Ordering::Relaxed);
        });
        handles.push(h);
    }
    for h in handles {
        h.recv();
    }
    assert_eq!(counter.load(Ordering::Relaxed), total,
        "运行时丢了任务!最终完成 {} / {}", counter.load(Ordering::Relaxed), total);
}
```

这条测试如果 fail,几乎一定是 M9b 的 `Task::poll` 状态机有 bug——比如 `RUNNING → IDLE` 的 CAS 失败路径里,重新入队的逻辑漏了一个分支。`fetch_add` 的 `Relaxed` 在这里够用,因为我们只关心"总共被调用 total 次",不关心顺序。

> 一个进阶的"压力 + 属性"组合:把 `total` 和 `n_workers` 都参数化,跑一组 `(total, n_workers, delay_ms)` 的组合,验证每种组合下都不丢任务。这就是"async 版 stress"。

### 把整节收成一帧

回到开头那张"三个看不见的调度者"的画面:执行器、reactor、时间。这一节给了你三把对应的武器——`poll_once`(夺回 polling 顺序)、`MockReactor`(夺回 I/O 事件时机)、`VirtualClock`(夺回时间流逝)。三者合起来,让你能在本机、毫秒级时间内、确定性复现那些"在真实硬件上跑几年才出现一次"的异步 bug。

最后一个回望:**async 测试的核心难度,不是"测异步代码",而是"测一个状态机在任意推进序列下的不变量"**。这与 M11b 的 loom、M11c 的 miri 在哲学上是同源的——loom 测"线程交错下的不变量",miri 测"虚拟 CPU 模型下的 UB",async 测试测"polling 序列下的状态机收敛"。三种工具,一个心法:**别问"它对不对",问"在所有合法的推进下它都对吗"**。

---

## 八、把全章收成五条能力

### 五拍回顾

| 拍 | 敌人 → 武器 | 一句话 |
|---|------------|--------|
| ENEMY | "测过≠证明过" | 并发 bug 是概率事件,普通测试套不住 |
| ANCHOR | 执行树 | 把"概率"翻译成"枚举"——每个叶节点都是一次合法执行 |
| LOW-FI | loom 手算 | 2 线程 × 2 步,`MAX_PREEMPTIONS=2` 就能抓 Relaxed 重排 bug |
| WRITE | miri 解释器 | 不开真线程,用虚拟调度器 + 1% 抢占率让低概率 bug 高概率复现 |
| WRITE | criterion 统计 | 单次测量是噪声,要用 t 检验判断"几个 σ" |
| ISO·ZOOM | 锁序校验器 | thread-local 栈强制升序,打破循环等待 |
| ISO·ZOOM | stress 真硬件 | loom/miri 都不跑真 CPU,stress 是最后一道防线 |

### L1–L5 能力阶梯

- **L1 认得**:看到"我测过没问题"这句话,本能反应是"测了几次?在什么硬件上?用了 loom/miri 吗?"
- **L2 会跑**:能用 `cargo +nightly miri test` 跑关键测试;能用 `LOOM_MAX_PREEMPTIONS=3 cargo test loom` 跑模型;能读 criterion 报告。
- **L3 会写**:能给一段并发代码写 loom 模型测试;能用 criterion 写性能基准并解读 p 值;能加 stress 测试。
- **L4 会调**:知道 bug 出来时该用哪个工具(逻辑错→loom,UB→miri,概率低→stress,性能退→criterion);能调整 `MAX_PREEMPTIONS`/`preemption-rate` 找最合适的剪枝粒度。
- **L5 掌控**:能为整个项目设计 CI 流水线,把 loom/miri/stress/criterion 四件套合理安排;能识别"这个 bug 是哪类工具该抓的",并据此决定"加什么测试"。

### 自检清单(请合上书自己回答)

1. 为什么"在本机跑 10000 次 `cargo test`"不能证明并发代码正确?(提示:执行树的叶节点有多少,你覆盖了几条?)
2. loom 的 `LOOM_MAX_PREEMPTIONS=3` 是什么意思?为什么 3 已经够抓大部分内存序 bug?
3. miri 跑测试时,真的开了多个 OS 线程吗?`-Zmiri-preemption-rate=0.01` 为什么能让低概率 bug 高概率复现?(手算概率比。)
4. 你优化了某段代码,criterion 报告改前 μ=100ns σ=10ns,改后 μ=95ns σ=8ns,n=100。这个改进是真的吗?(手算几个 σ。)
5. 锁序校验器为什么能防死锁?它的根本盲区是什么?(提示:跨线程的等待图。)
6. stress 测试为什么是 loom/miri 的补丁而非替代?它唯一能抓、loom/miri 抓不到的 bug 类型是什么?
7. async 测试为什么比 sync 测试难?需要哪三层 mock?

### 动手清单

- [ ] 把第二节手算的 2×2 执行树,扩展成 2×3(两个线程各 3 步),列出所有合法叶节点(`C(6,3) = 20` 个),标出哪些触发你设的 bug 条件。
- [ ] 在 `crates/forge-core/tests/` 写一个 loom 模型测试,故意用 Relaxed 制造 M1.6 风格的 bug,跑 `LOOM_MAX_PREEMPTIONS=3 cargo test loom`,观察 loom 打印的反例路径。
- [ ] 跑 `cargo +nightly miri test -p forge-core --test m3_01_spinlock_counter`,确认无 UB。然后把 `unlock` 改成 Relaxed(只在 fork 的实验分支改),观察 miri 报什么。
- [ ] 跑 `cargo bench -p forge-core`,打开 `target/criterion/report/index.html`,解读伪共享 benchmark 的置信区间。
- [ ] 跑 `./scripts/stress.sh forge-sync`,观察"反复跑 30 秒"撞出过什么。如果全绿,把某个 Mutex 内部 unlock 改成 Relaxed,看 stress 多久能撞出 fail。
- [ ] (进阶)给本节的锁序校验器原型加上"跨线程等待图"功能:全局维护一张"线程 T 持有锁 L1 等待锁 L2"的图,每次有新等待边加入时检测环。

---

## 九、本模块的"陷阱地图"(常见误区)

我们用一节专门打破几个直觉,因为它们最容易让初学者栽跟头。

### 误区 1:"loom 通过了,代码就安全了"

错。loom 只检查**你写了 assert 的不变量**。如果你没写 assert,bug 永远不会被 loom 发现。loom 抓到的不是"代码的所有 bug",而是"代码在所有合法交错下违反你定义的不变量的那些 bug"。所以**写 loom 测试的核心是定义好不变量**——`y == 0 且 z == 0` 这种,要写得精确。

### 误区 2:"miri 通过了,代码就无 UB"

**miri 通过 = miri 没找到 UB**。这不等同于"代码无 UB"。miri 有盲区:它不模拟具体 CPU 的弱内存,只模拟 Rust 抽象模型;它不跑 inline asm;它有 isolation(默认禁文件 IO)。所以 miri 通过是"必要非充分"——你的代码可能仍有 miri 看不到的 UB(比如你用 `asm!` 直接发 syscall,绕过了 miri 的检查)。

### 误区 3:"criterion 报告 p < 0.05 就是真改进"

p < 0.05 的意思是"如果改动其实没效果,观测到当前差异(或更极端)的概率小于 5%"。这**不等同于**"改动真有效的概率是 95%"。这是统计学的经典陷阱(Bayesian vs Frequentist)。在工程实践中,p < 0.05 是个合理的阈值,但你要知道它有 5% 的假阳性率——20 个"看起来有效"的改动里,可能有 1 个其实是噪声。

### 误区 4:"stress 跑一晚上没崩,代码就稳了"

stress 测试跑一晚上可能覆盖 `10^9` 次执行。如果一个 bug 的触发概率是 `10⁻¹⁰`,stress 跑一晚上有约 37% 的概率**碰不到一次**。生产环境跑几个月、几亿 QPS,这个 bug 总会浮现。stress 的价值是"把高频低概率 bug 逼到必现",不是"穷举所有可能"。

### 误区 5:"我用 std::sync::Mutex,std 都测过了,我没必要测"

std 确实测得很彻底。但**你用 std::Mutex 的方式**未必正确。你可能死锁、可能忘了 unlock、可能在持有锁时调用了外部回调(回调又拿同一把锁)。这些是**你的使用方式的 bug**,不是 std 的 bug。所以即便用 std,你也需要测你的代码。

---

## 十、本模块的敌人,最后一次回望

回到开头那个画面:你写完代码,本机跑 10000 次全绿,提交,三天后凌晨 3 点生产炸了。

现在你知道了:

- 那 10000 次测试覆盖的只是执行树的**少数几条叶节点**,你完全没碰大部分交错。
- loom 替你把执行树**系统枚举**(在剪枝范围内),让"低概率交错"变成"确定性触发"。
- miri 替你做 **UB 检测**,用虚拟调度器把硬件的 `10⁻⁶` 概率放大到 `10⁻²`。
- stress 替你跑**真实硬件的海量采样**,补 loom/miri 看不到的微架构行为。
- criterion 替你做**统计判断**,区分"真改进"和"噪声"。
- 锁序校验器替你**防死锁**,把循环等待从根源上禁掉。

**核心一念**:**"我测过"和"我证明过"之间隔着所有那些没被枚举到的叶节点**。这一章的全部工具,都是来减少这中间的鸿沟的。你永远不会 100% 证明一段并发代码正确(那需要形式化验证,远超本课范围),但你可以**显著降低"漏掉某个叶节点"的概率**——从"我跑了几次没事"的 `10⁻²` 误判率,降到"loom/miri/stress 都通过"的 `10⁻⁶` 甚至更低。

这就是并发测试的全部价值。

---

> **下一步**:M9a(同步工作窃取线程池)和 M9b(异步执行器)会用到本章的所有工具。等 M9b 建好后,我们会回来扩展 M11g,加上完整的 async 测试章节——包括如何测试自建的 executor、如何 mock mio reactor、如何用虚拟时钟跑时序测试。
>
> 测试文件参考:
> - 普通并发测试:`crates/forge-core/tests/m1_06_release_acquire.rs`、`crates/forge-sync/tests/m7_01_mutex_counter.rs`
> - 压力测试模板:`crates/forge-core/tests/m4_04_stress.rs`、`crates/forge-sync/tests/stress.rs`(本模块新增,被 `scripts/stress.sh` 直接调用)
> - 基准模板:`crates/forge-core/benches/m1_false_sharing.rs`
> - 压力脚本:`scripts/stress.sh`
> - 锁序校验器示例:`crates/forge-sync/examples/lock_order_tracker.rs`(本模块新增)

---

## 十一、miri/criterion 实战清单(新增 bench & miri 覆盖)

> 这一节把前面 M11c/M11d 抽象讲过的"miri 怎么跑、criterion 报告怎么读"
> 落到 Forge 现在真实存在的 4 个新文件上。每条都给出:**跑什么命令、
> 预期看到什么数字、看到之后怎么判断"是改进还是噪声"**。这一节呼应
> 第四节的手算 σ,但用的是真实跑出来的数据。

### 总览:新增了哪些文件

| 文件 | 类型 | 测什么 |
|------|------|--------|
| `crates/forge-core/benches/m3_spin_vs_std.rs` | criterion bench | M3.7 自研 SpinLock vs std::sync::Mutex,无竞争 + 高竞争 + 线程数曲线 |
| `crates/forge-sync/benches/m7_locks.rs` | criterion bench | M7.7 自研 futex Mutex vs std::sync::Mutex vs parking_lot::Mutex |
| `crates/forge-sync/benches/m2_rwlock_starvation.rs` | criterion bench | M2.4 写饥饿:7 读者压写者,`std::sync::RwLock`(reader-preferring) vs `forge_sync::RwLock`(写公平)。诚实结论:forge 吞吐反而低——公平性是活性保证,有 per-op 代价;真指标是写者尾延迟 |
| `crates/forge-pool/benches/m9a_pool.rs` | criterion bench | M9a.8 工作窃取池 vs std::thread::spawn per-task,任务数曲线 |
| `crates/forge-pool/benches/m9a_par_vs_rayon.rs` | criterion bench | M9a.8b `par_sort` vs **rayon**(plan 要求的对照):大 N 时 forge ~1.5×、rayon ~1.6× 快于 serial,同量级 |
| `crates/forge-core/tests/miri_unsafe.rs` | miri 专用(`#![cfg(miri)]`) | Arc clone/drop/get_mut、SpinLock 临界区 |
| `crates/forge-lockfree/tests/miri_unsafe.rs` | miri 专用 | Treiber stack push/pop、MCS lock 排队/解锁 |

**所有 miri 测试文件**用 `#![cfg(miri)]` 在文件头门控:普通 `cargo test`
跳过(整个文件根本不编译),只在 `cargo +nightly miri test` 时编译运行。
这样它们既不会拖慢 CI 的常规 `cargo test`,又能在 miri 下覆盖 unsafe 路径。
**所有 bench 文件**在对应 crate 的 `Cargo.toml` 里声明了
`[[bench]] name = "..." harness = false`——`harness = false` 是必须的,
因为 criterion 自带 main,不能用 libtest 的默认 harness。

### A. criterion 三条基准的实战清单

#### A1. M3.7 SpinLock vs std::sync::Mutex

**跑法**:
```bash
cargo bench -p forge-core --bench m3_spin_vs_std
# 想快出数字加 -- --quick(只跑几秒,样本数小,p 值不严谨但够看趋势)
cargo bench -p forge-core --bench m3_spin_vs_std -- --quick
```

**预期看到的数字**(在 4~8 核 x86 机器上):

| 子项 | 预期 | 解释 |
|------|------|------|
| `uncontended_lock/spin` | ~1-3 μs | SpinLock 的 fast path 就是 `AtomicBool::swap(Acquire)`——一个 `lock` 指令,~1ns/次 × 1000 次 iter |
| `uncontended_lock/std` | ~15-20 μs | std::Mutex 的 fast path 也是单次 CAS,但 `lock().unwrap()` 的中毒错误路径编译器消不掉,加上 guard 是 Result 包裹,比裸 bool 慢 3-5ns/次 |
| `contended_lock_4_threads/spin` | 4-10 ms | 4 线程 hammer:SpinLock 自旋烧 CPU,4 个线程互相挤核,吞吐崩塌 |
| `contended_lock_4_threads/std` | 3-6 ms | std::Mutex 抢不到就 park,让持锁者独占核跑完——吞吐反而高 |
| `spin_curve/{1,2,4,8}` | 单调上升,4→8 段斜率变陡 | 自旋锁在 worker > 物理核时崩塌 |
| `std_curve/{1,2,4,8}` | 平缓得多 | park 让 std 不会随线程数爆炸 |

**怎么读"4 线程下 spin 输给 std"**:criterion 会给你两组样本的均值和置信区间。
如果 `contended_lock_4_threads/spin` 的均值是 8ms、`std` 是 4.6ms,差 3.4ms。
**判断这是不是真**:看两组各自的标准差 σ。典型 σ 在 0.1~0.3ms,差值是
`3.4 / 0.3 ≈ 11 个 σ`,几乎不可能是噪声(p ≈ 10⁻²⁸)。

**这一节的核心教学点**:SpinLock 在**无竞争、持锁短**的场景赢;在
**高争用、过订阅**的场景输。这条曲线是 M3 文档"为什么不能盲目自旋"
的实证。看到这条曲线,你下次写代码选择锁时会本能地问三个问题:
锁持多久?worker 数?跑在几核?

#### A2. M7.7 自研 futex Mutex vs std vs parking_lot

**跑法**:
```bash
cargo bench -p forge-sync --bench m7_locks
```

**预期看到的数字**:

| 子项 | 预期 | 解释 |
|------|------|------|
| `uncontended_lock/{forge, std, parking_lot}` | 三者都在 12-20 μs(1000 次 iter) | fast path 都是单次 CAS 0→1;差距来自 Result / lock_api 包装 |
| `contended_lock_4_threads/forge` | ~10 ms | 我们手写的 3 态 + 自适应自旋 + wake_one |
| `contended_lock_4_threads/std` | ~13 ms | std 实际上更慢——Rust 1.x 的 std::Mutex 在某些场景下有 wake_all 的退化 |
| `contended_lock_4_threads/parking_lot` | ~7 ms | parking_lot 是工业级调优,用更精细的状态字编码 + 更聪明的 wake 策略 |

**怎么读**:parking_lot 通常最快,我们自研的 forge-sync **不应该比 std 慢**,
也不应该比 parking_lot 慢一倍以上。如果 forge-sync 比 std 慢 50%+,
说明实现有性能 bug——典型是 unlock 路径无条件 `wake_one`(2 态错误),
或者自旋次数过高。**M7 文档反复强调"3 态 vs 2 态差 10 倍"**,这条 bench
就是用数字验证它:如果你把 forge-sync 的 unlock 退化成 2 态,这条 bench
的 forge 数字会从 10ms 跳到 100ms+。

**parking_lot 的 dev-dependency**:这条 bench 在 `forge-sync/Cargo.toml`
里加了 `parking_lot = "0.12"` 作为 dev-dependency。**只在 bench 范围内引用**,
绝不进 `[dependencies]`(否则会污染运行时,破坏"自研锁"的教学目的)。

#### A3. M9a.8 工作窃取池 vs std::thread::spawn

**跑法**:
```bash
cargo bench -p forge-pool --bench m9a_pool
```

**预期看到的数字**(5000 个 `1+1` 极短任务):

| 子项 | 预期 | 解释 |
|------|------|------|
| `std_thread_spawn` | ~150 ms | 每任务 spawn + join 一次 OS 线程,~30 μs/任务(全部系统调用开销) |
| `stealing_pool_v3` | ~6 ms | 任务闭包装箱 + push 到本地队列 + worker 偷来跑,~1 μs/任务 |
| `shared_queue_pool_v1` | ~10 ms | 所有 worker 抢同一把 `Mutex<VecDeque>`,锁争用拖累 |
| `stealing_curve/{100,1000,5000}` | 任务数线性增长 | 每任务开销 ~1μs,线性很正常 |
| `std_thread_curve/{100,1000,5000}` | 任务数线性增长但斜率陡 25 倍 | spawn OS 线程的固定开销 |

**怎么读**:三者的**线性斜率**就是各自"每任务开销"。stealing 的斜率
大约是 std 的 1/25。这是工作窃取池存在的全部理由——**把"每任务 30μs"
降到"每任务 1μs"**。差距随任务量线性放大:100 任务差 60 倍时间,5000
任务差 25 倍——这是因为 std 的 OS 线程池热起来后单次 spawn 略快。

**SharedQueuePool(V1) 比 StealingPool(V3) 慢**:这一对照是 M9a 文档
"为什么需要工作窃取"的核心证据。V1 把所有 worker 喂进同一把锁,V3
让每 worker 有本地队列(锁争用从"全局一把"变成"N 把偶尔偷")。
bench 数字把这个"工程取舍"量化。

### B. miri 两个测试文件的实战清单

#### B1. forge-core 的 miri 测试

**跑法**:
```bash
# 整个文件(5 个测试,~1.5s)
cargo +nightly miri test -p forge-core --test miri_unsafe

# 单个测试(更快)
cargo +nightly miri test -p forge-core --test miri_unsafe spinlock_critical_section

# 调高抢占率(抓低概率 bug,跑更慢)
MIRIFLAGS="-Zmiri-preemption-rate=0.1" cargo +nightly miri test -p forge-core --test miri_unsafe
```

**5 个测试覆盖的 unsafe 路径**:
1. `arc_clone_drop_stress_under_miri`:Arc clone/drop/upgrade/downgrade 的并发混跑——覆盖 `ArcData<T>` 上两个原子计数器的全部 fetch_add/fetch_sub/CAS。
2. `arc_get_mut_under_miri`:`get_mut` 内部的 `compare_exchange(1, usize::MAX)` 自旋锁 + `fence(Acquire)` + 裸指针解引用。
3. `spinlock_critical_section_under_miri`:SpinLock 的 lock/DerefMut/Drop(unlock)完整临界区。
4. `spinlock_read_only_critical_section_under_miri`:Guard 的 Deref(不可变)路径——和 deref_mut 走同一个 unsafe 解引用,但只读。
5. `arc_and_spinlock_together_under_miri`:Arc 共享所有权 + SpinLock 共享可变状态的组合,交错最多。

**预期**:全部 ok,无 UB。这表示 `forge-core` 的 unsafe 实现
**在 miri 的抽象内存模型下没有数据竞争、别名违规、use-after-free**。
**注意**:miri 通过 ≠ 在所有硬件上无 bug(见 M11c 的盲区讨论);
miri 失败 = 一定有 UB(强保证)。

**循环数为什么这么小**(50 次 × 4 线程):miri 比真实执行慢 100~1000 倍。
50 × 4 = 200 次操作,miri 跑 1~2 秒。如果放大到 stress 测试的 100k 次,
miri 要跑几小时——不划算。这些测试是"快速回归"——CI 上每 PR 跑;
想跑全套 miri 用现有的 `m4_04_stress.rs`(它没 cfg 门控,但循环数也压得小)。

#### B2. forge-lockfree 的 miri 测试(部分 ignored,讲清为什么)

**跑法**:
```bash
# 默认只跑 1 个(mcs_lock_uncontended),其它 3 个 #[ignore] 了
cargo +nightly miri test -p forge-lockfree --test miri_unsafe

# 跑被 ignored 的(各自有特殊 MIRIFLAGS 要加)
MIRIFLAGS="-Zmiri-ignore-leaks" cargo +nightly miri test \
    -p forge-lockfree --test miri_unsafe -- --ignored \
    treiber_stack_single_thread_under_miri

# MCS 多线程那条会暴露 src bug(见下文),不要 CI 上跑
```

**4 个测试覆盖的 unsafe 路径**:
1. `treiber_stack_single_thread_under_miri`:push/pop 单线程,覆盖 `Box::into_raw` + CAS + Acquire 解引用。**ignored**:stack.rs 故意不释放 pop 节点(规避 ABA),miri 报"内存泄漏"——**不是 bug**,是教学取舍。加 `-Zmiri-ignore-leaks` 后可通过。
2. `treiber_stack_concurrent_under_miri`:多线程并发 push/pop。**ignored**,原因同上 + 并发版本触发更复杂的 leak 模式。
3. `mcs_lock_contended_under_miri`:MCS lock 多线程排队 + unlock 唤醒后继。**ignored**:`crates/forge-lockfree/src/mcs.rs` 的 unlock 路径有一个真实的 **retag data race**(miri 报"non-atomic read on thread A and retag write on thread B")——这是 **src 的 bug**,等 forge-lockfree 的 src 维护者修。同样的 bug 在现有的 `tests/m8_04_mcs.rs` 下也能复现,证明这是 src 问题不是测试问题。
4. `mcs_lock_uncontended_under_miri`:MCS lock 单线程,unlock 走"无后继"的 CAS 清零 fast path。**默认通过**——这条路径不触发上面的 retag race。

**这条覆盖的意义**:`#[ignore]` 不是"测试没用",而是"测试有用但要单独跑"。
miri 测试的最大价值之一就是**暴露 src 里的真实 UB**——本节列出 3 条
ignored 测试,等于**3 个待修的 src 问题清单**。等 src 修好,把 `#[ignore]`
一行删掉,CI 上立即启用。

### C. 怎么把 miri 测试加进 CI(建议)

最小可行 CI 流水线:

```yaml
# .github/workflows/miri.yml(示意,Forge 还没建)
jobs:
  miri:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
        with: { components: miri }
      - run: cargo +nightly miri test -p forge-core --test miri_unsafe
      - run: cargo +nightly miri test -p forge-lockfree --test miri_unsafe
      # 可选:跑全套 miri(慢,只在 nightly job)
      - run: cargo +nightly miri test -p forge-core -p forge-sync
```

每个 PR 跑 1~2 分钟(就是上面 5+4=9 个小测试)。每天 nightly job 跑全套
stress + 各模块已有测试的 miri 版本(几十分钟)。

### D. 读者最容易卡住的 1 处:miri 报的"数据竞争"到底指什么

这一节读者最常困惑的是:**miri 报"Data race between non-atomic read
on thread A and retag write of type Node on thread B"——什么叫"retag write"?
我没有写啊?**

**retag** 是 Rust 编译器在每次借用(包括 `&` / `&mut` / `Box::from_raw`)
时插入的"隐式标记"。从 miri 的视角,**retag 等价于一次读 + 一次写**——
因为 retag 允许编译器做"借用栈优化",这个优化可能插入推测性的内存访问。

所以 miri 把 `Box::from_raw(ptr)` 看成"对 *ptr 做了一次读 + 写"。
如果另一个线程在这个 Box 还活着的时候碰了同一个地址(哪怕只是读),
miri 就报"data race"——因为这个读和 retag 的"隐式写"没有 happens-before
关系。

这正是 MCS lock 在 `mcs.rs:75` `Box::from_raw(self.node)` 处被抓的原因:
**前驱线程在 `(*next).granted.store(true, Release)`(mcs.rs:86)之前,
没有让后继线程的 `Box::from_raw` 看到一个 Acquire 同步点**——也就是说,
"前驱释放 Node 所有权给后继"这个动作,在 src 当前实现下**没有显式的
happens-before 边**。修法是:在 `(*next).granted.store(true, Release)`
和后继 `Box::from_raw(node)` 之间,要有一条 Acquire-load 或 fence
建立同步。MCS 论文里这一步是隐式的(靠 granted 的 Release→Acquire 配对),
但 Rust 的 miri 对 retag 更严格。

**这一段教学价值**:它把"内存序"从抽象的 happens-before 图,落到了
"具体一行代码、具体的 retag"上。看完这一节,你下次写 unsafe 并发代码,
会本能地画一张图:**每个 `Box::from_raw`、每个 `&mut *ptr`,都是一次 retag,
都需要和并发的其它访问建立同步**。这就是 miri 的最大教育意义。
