# M10 —— 把所有原语拼成两个真实应用

> 模块定位：M1 到 M9b 你造了原子、自旋锁、Mutex、Condvar、mpsc、Semaphore、Arc、线程池、异步运行时。但它们就像散落一地的零件——这一章把它们装成两台能跑的机器：一台并发网页爬虫，一台 mini-Redis。两台机器里**每一个并发决策**都要回到前面学过的原语上：为什么是这把锁、为什么是这个通道、为什么是这种结束判定。学完本章你能看着任意一个真实并发服务，把它拆回"哪些原语、为什么是这些"。

---

## 敌人先行（ENEMY）：两台机器最可能死在哪

先不要看代码。闭上眼睛想象你写一个爬虫。你给它一个种子 URL，它抓页面、解析链接、继续抓。这看似是一段顺序逻辑，但只要你想"快"，把"抓"这件事并发起来，立刻会撞上三堵墙：

1. **打爆对方**：你瞬间开 500 条连接去抓同一个网站。对方的 accept 队列满了，丢包；对方的 WAF 看着你像 DDoS，封你 IP。你成功"快"了一秒，然后被关在门外 24 小时。
2. **重复抓**：两个 worker 同时解析出同一个新链接，都把它丢进队列，都去抓——同一个页面被抓了两次，对方以为你恶意，又封你。
3. **不知道什么时候停**：你抓够了想要的页数，想关掉 workers——可是有个 worker 正抓着一个页面，马上就要吐出 20 个新链接；你这一关，那 20 个全没了；你不关，workers 又永远在 `pop()` 上空转。

mini-Redis 也一样有它的三堵墙：

1. **早到的 PUBLISH 被吞**：客户端 A `SUBSCRIBE news`，客户端 B 紧接着 `PUBLISH news hi`。如果服务端"注册 A 的订阅"和"B 的发布"用的是两把不同的锁、顺序还乱了，B 看到的订阅者列表里就没有 A，A 收不到这条消息——协议正确性破功。
2. **扇出漏人**：3 个订阅者同时在线，一条 PUBLISH 进来，你给"订阅者列表"逐个 `send`——列表在你读的那一刻和 send 的那一刻中间，有人退订了，有人新订阅了，你 send 到了一个已死的连接，或者漏 send 给新来的人。
3. **死锁 / 丢唤醒**：KV 存储和订阅表是两块共享状态，你用一把大锁包住它们，所有 SET 都阻塞所有 SUBSCRIBE；你拆成两把锁，又得想清楚加锁顺序，不然两把锁互相等。

这一章的目标，就是把这三对墙一堵一堵拆掉。

---

## 锚点（ANCHOR）：一句话记住这一章

> **真实并发服务 = 共享状态（锁） + 流量塑形（信号量 / 限速器） + 流水线（通道） + 结束判定（计数器）**。爬虫和 mini-Redis 只是这四块的不同组合。

把这句话记下来，下面每一节都在反复印证它。

---

## 第一拍：并发网页爬虫的"骨架"

### 画面先于代码

想象一个流水线车间。车间里有四类工位：

- **投料口**：把种子 URL 一份份投进传送带。
- **抓取工**：从传送带上取一份 URL，去网上把页面拽回来。每个抓取工对每个域名都有一张**限量通行证**——同一时刻对同一域名最多几个人出去。
- **拆解工**（其实和抓取工是同一批人）：把拽回来的页面拆开，找出里面的链接，没见过的扔回传送带。
- **写盘工**（只有一个）：站在车间末端，把所有抓回来的页面按顺序写进磁盘。

传送带不是无限的——它只能装 64 份。满了投料就得停下等。这叫**背压**。

这个画面里所有的"限量通行证"、"没见过的"、"传送带容量"、"唯一写盘工"，每一个都对应一个并发原语。下面我们一个一个把它们点亮。

### 代码骨架（`crates/forge-app/src/crawler.rs`）

```rust
pub trait Fetcher {
    fn fetch(&self, url: &str) -> Result<Page, String>;
}

pub struct Crawler<F: Fetcher> { /* fetcher, 限速, max_pages, queue_bound, n_workers */ }

impl<F: Fetcher + Send + Sync + 'static> Crawler<F> {
    pub fn run(self, seed_urls: Vec<String>) -> Vec<CrawledPage> { /* ... */ }
}
```

`Fetcher` 是一个 trait。为什么要 trait？因为我们要**在测试里替换它**：生产用 `ureq` 真去联网，测试用一张静态表 `HashMap<url, body>` 当假服务器。这一层抽象不是为了"未来扩展"，是为了今天就能让测试不依赖网络——CI 里跑测试不能慢、不能不稳、不能因为对方网站改版就红。

每一节我们都会回到这份骨架，往里加一块新原语。

---

## 第二拍：按域名限速 —— 一个 DomainLimiter

### 敌人：瞬间打爆对方

把 `per_domain = ∞` 想象成"每个 worker 都同时去同一个网站发请求"。8 个 worker 全开，对方服务器同一时刻看到 8 条来自你的连接。你抓 100 个页面就是 100 条连接（如果串行）或 8 条并发（如果限到 8）。对方不会封你。但如果你**瞬间开 500 条**呢？对方的 SYN 队列爆了，TCP 层就丢你的包，你看起来在飞速重传，对方看起来你在 DDoS。结局是被拉黑。

正解：**每个域名一张"限量通行证"**，规定同一时刻对这个域名最多 N 个并发请求。

### 画面：通行证窗口

想象 a.test 这个域名面前有一个小窗口，里面只摆 2 张通行证。worker 想抓 a.test 的 URL，得先去窗口领一张通行证。领到了才能发请求。发完请求把通行证还回窗口。窗口里没通行证时，worker 就在窗口排队。

这就是信号量（M8a）。但在我们的实际场景里，多个 worker 会**同时排队**，并且**频繁归还**——这正好是信号量最容易暴露 wake 丢失问题的工况。我们在写教程版本时实测到：用单原子 + atomic-wait 实现的 `Semaphore`，在高并发 + 多等待者时偶尔会丢一次 `wake_one`，导致 worker 永远卡在 `acquire`。

> 深究这件事的根因要回到 atomic-wait / futex 的"等待者计数"语义——它不维护"有多少人在等"这个数，只维护"有没有人 wake 过"。在 wake-one 模型下，一次 release 永远只唤醒一个；如果那一个醒来后又在 CAS 上输给别人、重新进 wait，它的"被唤醒额度"就用完了，需要再来一次 release 才能再次被唤醒。这就是教学版 Semaphore 的脆弱之处。M11 的"并发 bug 调试"小节会专门讲怎么用 `strace` 抓 futex 调用来定位它。

为了教程能稳定跑，我们用一个更朴素、但**绝不会丢唤醒**的实现：`Mutex + Condvar + 计数器`。

```rust
pub struct DomainLimiter {
    inner: std::sync::Mutex<LimInner>,
    cv: Condvar,
}
struct LimInner { available: usize }

impl DomainLimiter {
    pub fn new(permits: usize) -> Arc<Self> { /* ... */ }

    pub fn acquire(&self) -> Permit<'_> {
        let mut g = self.inner.lock().unwrap();
        while g.available == 0 {
            g = self.cv.wait(g).unwrap();   // 释放锁、睡、被唤醒后重新拿锁
        }
        g.available -= 1;
        Permit { limiter: self }
    }

    fn release(&self) {
        let mut g = self.inner.lock().unwrap();
        g.available += 1;
        drop(g);                  // 先放锁再 notify，避免唤醒者立刻又被锁挡住
        self.cv.notify_one();
    }
}
```

**为什么这样不会丢唤醒？** 关键在 `while + wait` 这一对。`Condvar::wait` 是一个**原子动作**：它把"释放 Mutex"和"把自己挂到 Condvar 的等待队列上"打包成不可分割的一步。也就是说，**不存在"我已经决定要等，但还没挂上去"这种中间状态**。release 端的 `notify_one` 要么在 wait 之前发生（这时 wait 一进去就看到 available 已经涨回来了，while 条件不成立，直接退出），要么在 wait 已经把人挂上之后发生（这时 notify 会精准击中等待队列里的某一个人）。两种情况都不会漏。

对比裸 atomic：你必须自己用 CAS 模拟"决定要等 → 挂上去"的原子性，错一步就是丢唤醒。Condvar 用一把锁把这件事兜住了——慢一点（每次都过锁），但稳。

`acquire` 返回一个 `Permit<'_>`，drop 时自动 `release`。这样 worker 写成：

```rust
let _permit = limiter.acquire();   // 领证
let page = fetcher.fetch(&url);    // 抓
// _permit 在这里 drop，自动还证
```

即使 `fetch` 出错早返回，`_permit` 也会被 drop——你不可能"忘记 release"。这是 RAII 给我们的免费正确性。

### 按域名"发证"：一张 HashMap

每个域名要一张独立的通行证。我们用：

```rust
per_domain: Mutex<HashMap<String, Arc<DomainLimiter>>>,
```

外层这把 `Mutex` 保护 HashMap 本身；内层每个 `Arc<DomainLimiter>` 才是真正的限速器。worker 来了：

```rust
fn limiter_for(&self, domain: &str) -> Arc<DomainLimiter> {
    // 快速路径：先读
    {
        let map = self.per_domain.lock();
        if let Some(lim) = map.get(domain) {
            return lim.clone();
        }
    }
    // 慢速路径：拿写权限再检查一次（double-checked）
    let mut map = self.per_domain.lock();
    map.entry(domain.to_string())
        .or_insert_with(|| DomainLimiter::new(self.per_domain_permits))
        .clone()
}
```

为什么慢速路径要"再 lock 一次再检查"？想象 a.test 第一次出现，两个 worker 同时走到快速路径，都发现"没有"，都释放读锁。如果直接进慢速路径、不检查就插入，你会插出**两个不同的 DomainLimiter**——一个 worker 拿到 A，另一个拿到 B，它们各算各的 2 张通行证，结果对 a.test 实际开了 4 条并发。第二次 lock 后 `or_insert_with` 会保证"键已存在就不覆盖"，所以哪怕两个 worker 都进慢速路径，最终也只会建一个 limiter，两人都拿到它的同一份 Arc clone。

### 手算例 1：按域名限速的逐拍时序

设 a.test 允许 2 并发，瞬间来 5 个 a.test 的 URL（U1..U5），有 5 个 worker（W1..W5）。

| 拍 | W1 | W2 | W3 | W4 | W5 | available | 等待队列 |
|----|-----|-----|-----|-----|-----|-----------|----------|
| 0  | acquire U1 → 拿到，去 fetch | acquire U2 → 拿到，去 fetch | acquire U3 → available=0，进 cv.wait | acquire U4 → 进 cv.wait | acquire U5 → 进 cv.wait | 0 | W3 W4 W5 |
| 1  | fetch 完成，drop permit → release → notify_one | 仍在 fetch | 被 notify 唤醒 → while 看 available=1 → 拿到 → 去 fetch | 仍在等 | 仍在等 | 0 | W4 W5 |
| 2  | 处理 U1 的链接 | fetch 完成 → release → notify_one | 仍在 fetch | 唤醒 → available=1 → 拿到 → fetch | 仍在等 | 0 | W5 |
| 3  | ... | 处理 U2 的链接 | fetch 完成 → release → notify_one | 仍在 fetch | 唤醒 → 拿到 → fetch | 0 | （空） |
| 4  | ... | ... | 处理 U3 的链接 | fetch 完成 → release（available=1，没人等，notify_one 空打） | 仍在 fetch | 1 | （空） |
| 5  | ... | ... | ... | 处理 U4 的链接 | fetch 完成 → release（available=2） | 2 | （空） |

注意拍 4 那个"notify_one 空打"——这是允许的，没人等的时候 notify 是 no-op，不消耗任何东西。这正是 Condvar 的容错性。

**反例：如果不限速会怎样？** 拍 0 就是 W1..W5 全都拿到"许可"（其实没许可这回事），5 条连接同时打到 a.test。如果 a.test 的 accept 队列只有 128，单次没事；但如果你跑 1000 个 worker × 10000 个 URL，瞬间 10000 条 SYN——对方的内核栈回 SYN-cookie 都来不及，干脆 blackhole 你。**限速不是"礼貌"，是"能持续抓下去"的硬条件。**

### 把它装回 Crawler

```rust
// worker 主循环里：
let domain = domain_of(&url);
let limiter = state.limiter_for(&domain);
let _permit = limiter.acquire();
let page_result = state.fetcher.fetch(&url);
// _permit drop
```

测试里我们用一个 `ConcurrencyCountingFetcher`，它会记录"每个域名在途的请求数峰值"，断言这个峰值永远 ≤ per_domain 设的值。这就把"限速真的生效了"这件事变成了一个可机器验证的不变式。

### 深挖：为什么 release 要"先 drop 锁，再 notify"

```rust
fn release(&self) {
    let mut g = self.inner.lock().unwrap();
    g.available += 1;
    drop(g);                  // 关键：先放锁
    self.cv.notify_one();     // 再 notify
}
```

看起来只是两行顺序，但顺序错了有性能问题。考虑反过来：

```rust
// 反例：
let mut g = self.inner.lock().unwrap();
g.available += 1;
self.cv.notify_one();    // notify 时还持有锁
drop(g);
```

`notify_one` 会唤醒一个等待者。那个等待者醒来第一件事是 `cv.wait` 内部要重新拿回 Mutex。但你此刻还持着锁——刚 notify 完，等待者立刻又睡回去了（拿不到锁）。等你 `drop(g)` 之后，等待者才能再被操作系统调度、再尝试拿锁。这中间多了一次"虚假唤醒 + 重新睡眠"的来回，浪费 CPU。

正确顺序是"先放锁再 notify"——等待者被唤醒的瞬间锁已经可用了，它能一次性拿到锁、跑下去。这是 std::sync::Condvar 的惯用法，几乎所有正确实现都这么写。

> 这不会丢唤醒吗？不会。`notify_one` 把等待者从"挂起"状态挪到"可运行"状态，这个动作本身不要求 notify 调用方持有锁。即使 notify 之后等待者还没来得及运行，操作系统也已经把它登记为"可调度"了。它迟早会拿到锁、看到 available 已经涨回来、跳出 while 循环。

### 深挖：available 为什么用 while 不用 if

```rust
pub fn acquire(&self) -> Permit<'_> {
    let mut g = self.inner.lock().unwrap();
    while g.available == 0 {           // while 不是 if
        g = self.cv.wait(g).unwrap();
    }
    g.available -= 1;
    Permit { limiter: self }
}
```

为什么是 `while`？因为 `cv.wait` 可能有**虚假唤醒**——操作系统允许它在没有 notify 的情况下返回（某些架构上这是性能优化）。如果用 `if`，虚假唤醒一发生你就往下走，结果 `available` 还是 0，你 `-= 1` 溢出成 usize::MAX，整个限速器崩溃。`while` 保证每次唤醒后都**重新检查条件**——条件没满足就再睡。

这个 while 模式是 Condvar 编程的"铁律"：**永远用 while 守卫条件，永远不要相信唤醒的真实性**。

---

## 第三拍：去重集 —— 检查 + 插入必须原子

### 敌人：同一个页面被抓两次

worker W1 抓页面 P，从 P 里解析出链接 L。worker W2 同时抓页面 Q，从 Q 里也解析出链接 L（P 和 Q 都引用了 L，这太常见了——比如 L 是首页 /）。两人都把 L 丢进队列，都去抓 L。L 被抓了两次。

你可能会想："那我在丢进队列前查一下 visited 集合不就行了？"

```rust
if !visited.contains(&L) {
    visited.insert(L.clone());
    queue.push(L);
}
```

这看起来对，但**这是错的**。下面手算给你看。

### 手算例 2：检查和插入分两步的竞争

设 `visited` 当前是空的。两个 worker 同时解析出 L。

| 拍 | W1 | W2 | visited | 队列 |
|----|-----|-----|---------|------|
| 0  | `contains(L)` → false | | {} | [] |
| 1  | | `contains(L)` → false（W1 还没 insert！） | {} | [] |
| 2  | `insert(L)` → 真，L 入队 | | {L} | [L] |
| 3  | | `insert(L)` → 假（已存在），但 W2 已经决定要入队了 | {L} | [L, L] |

W2 在拍 1 拿到的 `false` 是当时的事实，但拍 1 和拍 2 之间 W1 改了状态。W2 的判断基于过期信息。这就是 TOCTOU（time-of-check-to-time-of-use）竞争。

正解：把"检查 + 插入"塞进**同一把锁的同一份临界区**，让它在逻辑上变成一个原子操作。

```rust
fn claim(&self, url: &str) -> bool {
    let mut set = self.visited.lock();
    set.insert(url.to_string())   // HashSet::insert 返回 true 表示"是新插入的"
}
```

`HashSet::insert` 返回 `bool`：`true` 表示这次真插进去了（之前没有），`false` 表示已经存在。一把锁把"看 + 改"封死，外人看不到中间状态。worker 用：

```rust
if state.claim(&link) {        // 真新链接，归我了
    queue.push(link);
}
```

**重建后的时序**：

| 拍 | W1 | W2 | visited | 队列 |
|----|-----|-----|---------|------|
| 0  | `claim(L)` → 拿到锁 → insert 返回 true → 释放锁 | （在锁外等） | {L} | [L] |
| 1  | | `claim(L)` → 拿到锁 → insert 返回 false → 释放锁 → 不入队 | {L} | [L] |

L 只入队一次。两步被一把锁压成了一步。

### 为什么不能"用一把更细粒度的锁"或者"分片"？

可以，但代价是复杂度。`HashSet` 用一把 `Mutex` 包，写起来 3 行；分片锁要 hash 分桶、每桶一把锁、删除时要协调——这不是教学版该做的事。M11 会讲到分片锁。这一拍记住一件事：**只要"判断"和"动作"之间存在窗口，就一定有竞争。锁的全部意义就是把那个窗口压成零。**

### 关于"种子"的预 claim

种子 URL 不能走 `claim` 路径——它们是入队的起点，不是从某个页面解析出来的。但它们也必须被标记成"已访问"，否则 worker 弹出种子、抓完、解析出的链接里如果有指向种子的，就会把种子再抓一次。所以 `run` 一开始就：

```rust
{
    let mut v = state.visited.lock();
    for u in &seed_urls {
        v.insert(u.clone());
    }
}
```

这是一个"批量预占"——一次锁、N 次插入，比 N 次单独 claim 高效得多。

---

## 第四拍：有界队列 —— 把"快"转成"慢"

### 敌人：内存爆掉

爬虫的"生产者"是解析链接的 worker，"消费者"也是同一个 worker（它从队列里 pop URL 去抓）。乍看没有生产消费不匹配。但是：抓一个页面要 50ms，而它解析出的链接可能有 20 个——一进 20 出。如果队列无界，几轮之后队列里就堆了几万个 URL，每个 URL 几十字节，加起来几百 MB，再涨就 OOM。

正解：**有界队列**。满了，push 就阻塞。生产者被消费者的速度拖住——这叫**背压**。

### 画面：传送带只能装 64 份

传送带的物理长度对应队列容量。抓取工扔一个 URL 上传送带，传送带满了他就得等。这种"等"不是浪费——它是把"我现在太快"翻译成"我自动慢下来"，避免你写一堆限流代码。

### 代码：一个最小的有界队列

```rust
struct BoundedQueue {
    inner: std::sync::Mutex<Inner>,
    not_full: Condvar,
    not_empty: Condvar,
}
struct Inner { buf: VecDeque<String>, cap: usize, closed: bool }

fn push(&self, url: String) {
    let mut g = self.inner.lock().unwrap();
    while g.buf.len() >= g.cap && !g.closed {
        g = self.not_full.wait(g).unwrap();    // 满了，等 not_full
    }
    if g.closed { return; }                     // 关了就丢弃这次 push
    g.buf.push_back(url);
    self.not_empty.notify_one();
}

fn pop(&self) -> Option<String> {
    let mut g = self.inner.lock().unwrap();
    loop {
        if let Some(u) = g.buf.pop_front() {
            self.not_full.notify_one();
            return Some(u);
        }
        if g.closed { return None; }
        g = self.not_empty.wait(g).unwrap();
    }
}

fn close(&self) {
    let mut g = self.inner.lock().unwrap();
    g.closed = true;
    self.not_empty.notify_all();   // 唤醒所有在 pop 上等的 worker
    self.not_full.notify_all();    // 唤醒所有在 push 上等的 worker
}
```

两把 Condvar：`not_full` 喂给 push、`not_empty` 喂给 pop。close 时两把都 notify_all，避免有人卡死。

对比 M5 的 `forge_channel::mpsc`：那个是无界的，没有 `not_full`。本拍我们看到，**有界只是多了"满了就 wait"这一行**，但这一行带来了背压。这是通道设计的一个连续谱：无界（最高吞吐、最高内存风险）↔ 有界阻塞（背压）↔ 有界丢弃（限速）↔ 有界 + 拒绝（拒绝服务）。本章我们选第二种。

### 手算例 3：背压下的队列长度演化

设 queue_bound = 3，生产者 W_p 每拍生产 2 个 URL，消费者 W_c 每拍消费 1 个。

| 拍 | 生产 | 消费 | 队列长度（拍末） | W_p 状态 |
|----|------|------|------------------|----------|
| 0  | 2（U1 U2） | 0 | 2 | 自由 |
| 1  | 2（U3 U4） | 1（U1） | 3 | **第 2 个 push 时满了，阻塞** |
| 2  | 0（被阻塞） | 1（U2） | 2 | 被 not_full 唤醒，push U4 |
| 3  | 1（U5） | 1（U3） | 2 | 自由 |

拍 1 的关键时刻：W_p 想推 U4，发现 len=3=cap，进 `not_full.wait`。它在那里**睡觉**，直到 W_c 消费一个、notify_one。这是背压的物理体现：**W_p 不再以自己的速度跑，而是被 W_c 的速度拽住**。

这就是为什么有界队列天然防 OOM：队列长度的上限就是 `cap`，超出部分全部翻译成"生产者睡眠"。

---

## 第五拍：结束判定 —— 一个被严重低估的难点

### 敌人：怎么知道"没活干了"

这是这一章里**最容易写错**的地方。先看几个错误版本。

**错误版本 A**："队列空了就停。" 反驳：队列空了，可能正有一个 worker 在 fetch 一个即将吐出 20 个链接的页面。你这一关，20 个链接全没了。

**错误版本 B**："收够 max_pages 条结果就关。" 反驳（弱）：这其实是对的，但只对"我就要 N 个页面"这种诉求对。如果你想"把整个站抓完"呢？没有 max_pages 这种东西。你需要另一种结束判定。

**错误版本 C**："worker 都空了就停。" 怎么知道 worker 都空了？你得问每个 worker"你在干活吗"。worker 之间的状态是分布的，问不实时。

正解：**显式追踪"还有多少条结果在路上"**。这个计数器是"未送达结果数"（in_flight_results）。它的语义是：**每一条会被送进结果通道的结果，都对应一次 +1**；**主线程每收一条结果，对应一次 -1**。

- 种子数 = 初始的 in_flight_results（因为每个种子最终会变成一条 CrawledPage）。
- worker 解析出新链接 L 并入队时：in_flight_results += 1（因为 L 早晚会被某个 worker 抓、变成一条 CrawledPage）。
- worker 抓 URL 失败时：in_flight_results -= 1（这条 URL 当初占了 +1 的额度，但不会送结果，得把额度还回去）。
- 主线程每 recv 一条结果：in_flight_results -= 1。

主循环：

```rust
while collected.len() < max_pages {
    if in_flight_results.load(SeqCst) == 0 {
        break;                  // 真没活了
    }
    let page = result_rx.recv();
    in_flight_results.fetch_sub(1, SeqCst);
    collected.push(page);
}
pending.close();                 // 通知 workers 别再 pop 了
```

为什么这个判定**绝对正确**？关键在 +1 的时机：**worker 必须先 +1 再 push，先 +1 再 send**。这样在主线程看来：

- 只要 in_flight > 0，就说明要么队列里有未消费的 URL（push 过、+1 过、还没被 worker 弹出），要么有 worker 正在 fetch（pop 过、还没 send）。
- in_flight == 0 当且仅当所有 push 过的 URL 都已经被 send 出来（或被 fetch 失败 -1 抵消）。

所以"看到 0"这一刻，可以保证**不会再有新结果到达**——主线程可以放心 break。

### 反例：把 -1 放错位置会怎样

假设你把"主线程 recv 后 -1"改成"worker send 后 -1"：

```rust
// worker:
result_tx.send(...);
in_flight_results.fetch_sub(1, SeqCst);   // 错误位置

// main:
while ... {
    if in_flight == 0 { break; }
    let page = result_rx.recv();
    // 这里不再 -1
}
```

竞态：worker send 完，但还没 -1。in_flight 仍 > 0。主线程 recv 到这条结果。下一次循环看 in_flight——还是 > 0（worker 还没来得及 -1）。主线程继续 recv，但没有新结果了，**永远阻塞在 recv**。

正确的位置是"主线程消费时 -1"——把"消费"和"记账"绑死在同一个执行流里。

### 推论：in_flight 必须用 SeqCst

```rust
in_flight_results.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
```

为什么不用 Relaxed？因为这个计数器跨线程传递"还有多少结果会来"这个信息——主线程读它来做控制流决策（要不要退出循环），worker 写它来表达"我又发了一条"。如果用 Relaxed，理论上主线程可能看到旧值（CPU 缓存延迟同步），导致 break 走在 worker 的 +1 之前，丢结果。SeqCst 强制全局顺序，保证主线程看到的 in_flight 是某一时刻的全局真相。代价是一次 SeqCst 比较慢（x86 上是 mfence），但这里每条结果才用一次，影响可忽略。

### 结束判定的另一种正确写法：sentinel

教学版的 in_flight 计数器优雅但微妙。另一种常见的写法是**哨兵值（sentinel）**：worker 退出时不直接 break，而是给结果通道发一个 `None`，主线程数 None 的个数等于 worker 数时停。这要求通道传 `Option<CrawledPage>`，并且每个 worker 退出前各发一个 None。这种写法的优点是"主线程不需要原子计数器"，缺点是**主线程必须等所有 worker 都退出才能结束**——如果你只想要 max_pages 条结果就提前停，哨兵法就不灵了，因为还有 worker 在跑、不会发 None。

我们的场景正好是"我可能想要提前停"——max_pages 触发后立刻关队列——所以选了 in_flight 计数器。每种结束判定都有自己的甜蜜点，没有银弹。

---

## 第六拍：把爬虫装起来

```rust
pub fn run(self, seed_urls: Vec<String>) -> Vec<CrawledPage> {
    let pending = Arc::new(BoundedQueue::new(self.queue_bound));
    let in_flight_results = Arc::new(AtomicUsize::new(seed_urls.len()));
    for u in &seed_urls { pending.push(u.clone()); }

    let (result_tx, result_rx) = mpsc::channel();
    let state = Arc::new(CrawlState { /* fetcher, visited, per_domain, ... */ });

    // 预占种子
    { let mut v = state.visited.lock(); for u in &seed_urls { v.insert(u.clone()); } }

    std::thread::scope(|s| {
        // workers
        for _ in 0..self.n_workers {
            let (state, pending, in_flight, result_tx) = (
                state.clone(), pending.clone(), in_flight_results.clone(), result_tx.clone(),
            );
            s.spawn(move || loop {
                let url = match pending.pop() { Some(u) => u, None => break };
                let limiter = state.limiter_for(&domain_of(&url));
                let _permit = limiter.acquire();
                let page_result = state.fetcher.fetch(&url);
                // _permit drop

                match page_result {
                    Ok(page) => {
                        let mut new_links = 0;
                        for link in extract_links(&page.body) {
                            if state.claim(&link) {
                                new_links += 1;
                                in_flight.fetch_add(1, SeqCst);  // 这条新链接早晚变一条结果
                                pending.push(link);
                            }
                        }
                        result_tx.send(CrawledPage { url: page.url, body: page.body, new_links });
                    }
                    Err(_) => {
                        in_flight.fetch_sub(1, SeqCst);  // 占了额度但不会发结果，还回去
                    }
                }
            });
        }

        // main：收结果 + 关队列
        let mut collected = Vec::new();
        while collected.len() < self.max_pages {
            if in_flight_results.load(SeqCst) == 0 { break; }
            let page = result_rx.recv();
            in_flight_results.fetch_sub(1, SeqCst);
            collected.push(page);
        }
        pending.close();
        collected
    })
}
```

每一行都能回到前面某一拍。这就是"原语拼装"——你看到的是 `BoundedQueue`、`DomainLimiter`、`claim`、`in_flight_results`、`mpsc` 五块积木咬在一起，没有别的。

---

## ISO·ZOOM：爬虫的并发决策清单

把本章爬虫做过的所有并发决策列一张表，每一条都写**为什么**：

| 决策 | 选择 | 为什么不是别的 |
|------|------|----------------|
| 限速器实现 | `Mutex+Condvar` | 不是 `Semaphore`：教学版 Semaphore 在多等待者下偶尔丢唤醒；M11 会讲怎么修 |
| 限速粒度 | 每域名一把 | 不是全局一把：不同域名互不阻塞；不是每 URL 一把：那等于不限速 |
| 去重集 | `Mutex<HashSet>` | 不是 `RwLock<HashSet>`：写多读少，RwLock 反而慢；不是无锁：写起来不值 |
| 检查+插入 | 一把锁内原子 | 不是"先 contains 后 insert"：TOCTOU 竞争，会重抓 |
| 队列 | 有界 `Mutex<VecDeque>+Condvar` | 不是无界：背压防 OOM；不是 lock-free：写起来不值、调试难 |
| 结果通道 | 无界 mpsc | 不是有界：结果数永远 ≤ 待抓数，不会爆；写盘只有一个消费者 |
| 结束判定 | `AtomicUsize` in_flight | 不是"队列空"：吐链接窗口会丢；不是"worker 空"：分布式状态问不实时 |
| worker 模型 | scoped thread | 不是 M9a 池：教学焦点在并发决策不在池；M9a 池可作练习换入 |

---

# 第二部分：mini-Redis

爬虫是"大量短任务 + 限速 + 汇聚"的形态。mini-Redis 是另一类典型形态：**长连接 + 协议解析 + 共享 KV + pub/sub 扇出**。它的并发决策完全不同。

---

## 敌人先行：mini-Redis 会死在哪

1. **协议歧义**：客户端发 `SET key value\r\n`，你按"空格切"——value 里有空格怎么办？value 里有 `\r\n` 怎么办？协议必须有**长度前缀**才能精确切片。
2. **共享状态锁粒度**：KV 和订阅表是两块不相关的状态。用一把锁，一条 SET 阻塞所有 SUBSCRIBE；用两把锁，加锁顺序错了就死锁。
3. **早到 PUBLISH 被吞**：客户端 A SUBSCRIBE，紧接着客户端 B PUBLISH。如果"注册 A"和"B 读订阅列表"没有 happens-before 关系，B 可能看不到 A。
4. **扇出漏人**：3 个订阅者，逐个 send——读到列表的时刻和 send 的时刻中间状态变了，漏人。

这一部分就把这四堵墙拆掉。

---

## 锚点：RESP 协议

RESP（REdis Serialization Protocol）是一种**带长度前缀**的行框架协议。命令长这样：

```
*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nhello\r\n
```

拆开：

- `*3\r\n` —— 数组，3 个元素。
- `$3\r\nSET\r\n` —— bulk string，长度 3，内容 "SET"。
- `$3\r\nkey\r\n` —— bulk string，长度 3，内容 "key"。
- `$5\r\nhello\r\n` —— bulk string，长度 5，内容 "hello"。

**为什么用长度前缀而不是分隔符？** 因为 value 可以是任意字节——包括 `\r\n`。分隔符协议（比如 HTTP headers）遇到 value 里有分隔符就傻了；长度前缀协议知道"下面 5 个字节就是内容，原样读"，不受内容影响。

解析代码（`mini_redis.rs`）：

```rust
pub fn read_command(reader: &mut impl BufRead) -> Result<Command, RespError> {
    // 第一行：*N
    let mut header = String::new();
    reader.read_line(&mut header)?;
    let count = header.trim_end().strip_prefix('*')?.parse::<usize>()?;

    // N 个 bulk string
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        let mut len_line = String::new();
        reader.read_line(&mut len_line)?;
        let len = len_line.trim_end().strip_prefix('$')?.parse::<usize>()?;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;       // 吃掉结尾 \r\n
        args.push(String::from_utf8(buf)?);
    }

    parse_command(&args)   // GET/SET/DEL/PING/PUBLISH/SUBSCRIBE/UNSUBSCRIBE
}
```

注意 `reader.read_exact(&mut crlf)`—— RESP 规定每个 bulk string 后面跟 `\r\n`，我们读出来扔掉。这一步不能省：不读掉它们，下一次 `read_line` 会读到孤立的 `\r\n`，解析就崩了。

> **一个真实 bug 的故事**：最初这版的 `Command` 枚举里**没有 `Ping` 变体**，可 bin 的启动横幅却写着"支持 PING"。`PING` 命令落到 `parse_command` 的 `_ =>` 未知命令分支，连接收到空回复。这条 bug 没被单元测试抓到（因为没人测 PING），是**手动用裸 TCP 跑 `*1\r\n$4\r\nPING\r\n`、`recv` 回空字节**才暴露的。修法很轻：枚举加 `Ping`、解析加 `("PING",1) => Ok(Command::Ping)`、处理加 `Command::Ping => write_simple("PONG")`；并加一条回归测试 `bin_ping_returns_pong` 守住。教训：**bin 横幅宣称支持的命令，每一条都得有测试**——否则"广告"和"实现"会悄悄分叉。

---

## 共享状态：两把锁，不是一把

```rust
pub struct ServerState {
    pub kv: Mutex<HashMap<String, String>>,
    pub subs: Mutex<HashMap<String, Vec<mpsc::Sender<SubMessage>>>>,
}
```

**为什么两把锁？** 这两块状态的访问模式完全不同：

- `kv` 是"短锁、高频"：每条 GET/SET 都拿一下，立刻放。
- `subs` 是"长持有、低频"：只在 SUBSCRIBE/UNSUBSCRIBE/PUBLISH 时动。

如果用一把大锁：一条 SUBSCRIBE 注册（要遍历当前 channel 的 sender 列表）会阻塞所有 SET。QPS 直接掉一半。拆成两把锁后，GET/SET 和 SUBSCRIBE/PUBLISH 完全并行——除了它们各自内部。

**那会不会死锁？** 会不会出现"线程 A 拿了 kv 锁等 subs 锁，线程 B 拿了 subs 锁等 kv 锁"？看代码：我们的命令处理**从不**同时持有两把锁。SET 只动 kv，SUBSCRIBE 只动 subs，PUBLISH 只动 subs。没有"先拿 A 再拿 B"的路径，自然没有死锁。

> **加锁顺序原则**：如果你必须同时拿多把锁，永远按**全局统一的顺序**拿。这是避免死锁的金科玉律。本章我们的命令恰好不需要同时拿，所以天然安全。M11 会讲"必须同时拿两把锁时怎么办"。

---

## 手算例 4：PUBLISH/SUBSCRIBE 扇出

3 个客户端 SUBSCRIBE news；某客户端 PUBLISH news hello。逐拍画服务端怎么把一条消息扇出到 3 个订阅者。

服务端的 subs 表（概念）：

```
subs["news"] = [ tx_A, tx_B, tx_C ]
```

每个 `tx` 是一个 `mpsc::Sender<SubMessage>`，对应一个订阅客户端连接里那个隐式的 receiver。

PUBLISH news hello 的处理（简化）：

```rust
Command::Publish(channel, message) => {
    let senders: Vec<_> = {
        let subs = state.subs.lock();
        subs.get(&channel).cloned().unwrap_or_default()
    };
    let mut delivered = 0;
    for tx in &senders {
        tx.send((channel.clone(), message.clone()));
        delivered += 1;
    }
    write_int(&mut writer, delivered)?;
}
```

逐拍：

| 拍 | 服务端 | tx_A 的 receiver | tx_B 的 receiver | tx_C 的 receiver | publisher |
|----|--------|------------------|------------------|------------------|-----------|
| 0  | lock subs → clone 出 [tx_A, tx_B, tx_C] → unlock | 空 | 空 | 空 | 等回应 |
| 1  | tx_A.send(("news","hello")) | 收到 | 空 | 空 | 等回应 |
| 2  | tx_B.send(("news","hello")) | 收到 | 收到 | 空 | 等回应 |
| 3  | tx_C.send(("news","hello")) → delivered=3 | 收到 | 收到 | 收到 | 等回应 |
| 4  | write `:3\r\n` 给 publisher | 收到 | 收到 | 收到 | 收到 `:3` |

关键点：

1. **锁内只做"读列表"**，不 send。如果在锁内逐个 send，一个慢订阅者会让所有人卡住（send 是无界的所以本章不会，但生产实现里 send 可能阻塞）。**复制列表 → 出锁 → 锁外 send** 是惯用法。
2. **delivered 是 send 次数，不是"成功送达次数"**。这是教学简化。真实 Redis 也是这样——PUBLISH 返回的是"接收客户端数"，不保证对方真的读到。
3. **每个订阅者一个独立的 sender**——这就是为什么不能用 mpsc 单消费者模型：mpsc 的 Receiver 只能有一个，多订阅者要广播。每个订阅者一个 mpsc（或多 Sender 共享一个 receiver）就够。

**反例：用 mpsc 单消费者会怎样？** 假设你只有一对 (tx, rx)。3 个客户端都来 SUBSCRIBE，你都给他们同一个 tx 的 clone——但他们共享同一个 rx。谁去读 rx？没人能"独占"那个唯一的 receiver。消息只会被其中一个客户端随机读到，另两个永远收不到。所以**扇出必须每订阅者一通道**——这是协议语义强制的。

---

## 早到 PUBLISH：happens-before 在哪

### 敌人：客户端 A SUBSCRIBE，客户端 B 紧接着 PUBLISH，A 收不到

这是分布式系统经典问题："读"和"写"之间要有 happens-before。我们怎么保证 B 的 PUBLISH 一定能看到 A 的 SUBSCRIBE？

答案藏在 SUBSCRIBE 命令的**回执**里。服务端这样处理 SUBSCRIBE：

```rust
Command::Subscribe(channel) => {
    {
        let mut subs = state.subs.lock();
        subs.entry(channel.clone()).or_insert_with(Vec::new).push(tx.clone());
    }   // 注册完成
    write_simple(&mut writer, &format!("subscribed {channel}"))?;   // 回执
    writer.flush()?;
}
```

关键顺序：**先注册（写 subs），再回 `+subscribed`**。客户端 A 只有在收到 `+subscribed` 之后才会"认为自己订阅成功了"。客户端 B 在 PUBLISH 时，看到的 subs 表里有没有 A？

如果 A 还没收到 `+subscribed`：服务端要么还没注册 A（subs 没 A），要么注册了 A 但还没发回执。前者 B 看不到 A 是正确的（A 还没订阅完）；后者 A 已经在 subs 里，B 能看到——也对。

如果 A 收到了 `+subscribed`：那服务端**一定先**注册了 A（因为回执在注册之后）。TCP 保证 B 的 PUBLISH 到达服务端的时刻，A 的 SUBSCRIBE 处理（包括注册）一定已经完成——因为 A 的回执已经发回了，说明服务端走完了注册→回执的全流程。

所以 happens-before 链是：**A 注册到 subs → A 收到 +subscribed → A 知道自己订阅好了 →（同时其它客户端能看到 A）**。这一切由"注册在回执之前"这一行代码 + TCP 的顺序保证兜底。

测试里我们这样验证：客户端 A SUBSCRIBE，**等收到 +subscribed 才**让 B PUBLISH。这是协议契约：回执就是"我准备好了"的承诺。

---

## 长连接与"thread-per-connection"

```rust
pub fn serve<A: ToSocketAddrs>(addr: A, state: Arc<ServerState>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for incoming in listener.incoming() {
        let stream = incoming?;
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = handle_client(stream, state);
        });
    }
    Ok(())
}
```

**每条连接一个 OS 线程**。这是教学版的取舍。理由：

- 教学焦点是"并发决策"，不是"扛 10 万连接"。thread-per-connection 让每条 TCP 流、每一拍 read/write、每一次锁获取都**肉眼可见**——没有异步运行时把执行流切碎后藏起来的复杂性。
- 真实 Redis 也是单线程模型（核心命令串行）；它不是"每连接一线程"，但也不是异步——它用事件循环。M11 会讲"把 mini-Redis 搬到 M9b 异步运行时上"，那时你会看到为什么异步适合"高并发长连接"。

thread-per-connection 的代价：1 万连接 = 1 万线程 = 80 GB 栈（默认 8 MB/线程）。所以**只适合连接数中等**（几百到几千）的场景。

---

## 真实 bug 故事：测试里的 BufReader 陷阱

写本章的测试时，我（作者）踩了一个非常教育人的坑，值得专门讲。

最初的测试客户端长这样（简化）：

```rust
fn read_line(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());   // 每次新建！
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    line
}
```

每次调 `read_line` 都**新建一个 BufReader**。看起来无害——`BufReader` 不就是个带 8 KB 缓冲的 reader 吗？

问题是：`BufReader` 会**预读**。一次 `read_line` 调用，底层 `read` 实际从 socket 读进来的可能不是一行，而是好几 KB（TCP 是字节流，OS 一次性给你多少都行）。BufReader 把这些字节缓存到自己的内部缓冲区，只暴露第一行给你；剩下的留在缓冲区里等下次 `read_line` 用。

但下次 `read_line` 创建了**新的** BufReader！新 BufReader 的缓冲区是空的。它去 socket 读，但 socket 里那些字节**已经被上一个 BufReader 读走了**——它们躺在旧 BufReader 的内部缓冲里，旧 BufReader 一销毁（函数返回时）就被丢了。

**结果**：SET 命令的回执 "+OK\r\n" 和 GET 命令的 bulk 回复 "$5\r\nforge\r\n" 可能被服务端一次性发回来。第一次 read_line 创建 BufReader1，BufReader1 一次性读进 8KB（包含 "+OK\r\n$5\r\nforge\r\n"），返回 "+OK\r\n"。剩下的 "$5\r\nforge\r\n" 留在 BufReader1 的缓冲区。函数返回，BufReader1 销毁，缓冲区里的字节**永久丢失**。下次 read_line 创建 BufReader2，去 socket 读——socket 是空的（数据已经被读走了），阻塞，超时。

测试表现为：每次跑 5 次会失败 1 次，超时 2 秒。失败率取决于"服务端是把两条响应分两个 TCP 包发，还是一个包发"——这是操作系统 TCP stack 的决定，时序敏感，所以 flaky。

修复：让一个连接**持有一把 BufReader，贯穿它的整个生命周期**：

```rust
struct RespClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,   // 一直留着，不销毁
}
```

每次 `read_line` 用同一个 `self.reader`——预读到的字节留在它的缓冲区，下次接着用。这就修复了 flaky。

**教训**：`BufReader` 不是"无状态工具函数"，它**有状态**（缓冲区）。你不能把它当临时变量随手建随手扔。任何"持久的字节流 + 预读"的抽象都必须有一个长寿命的 reader 实例。这是 Rust IO 编程的一个常见陷阱——Rust 的类型系统不会帮你抓住它（每次新建 BufReader 编译器不报错），只有理解了"预读 + 状态"才会避开它。

> 顺带：这就是为什么 `std::io::BufReader::new` 接受任何 `Read`，但**没有**一个"无状态的 read_line 函数"标准库——`std::io::BufRead::read_line` 是 trait 方法，必须作用在某个**具体的 BufReader 实例**上，强制你持有它。设计上就在提示"我是有状态的"。

这个 bug 在生产代码里也会出现——只要你"图方便"在循环里临时建 BufReader。它不致命（超时），但极难定位（flaky、看起来像网络问题）。M11 会讲怎么用日志和包捕获抓它。



一个连接一旦 SUBSCRIBE 了某个 channel，它就进入"半订阅态"——服务端要给它推消息（来自 PUBLISH），它也能继续发命令。我们的教学版做了一个简化：**SUBSCRIBE 之后的连接不再主动 drain 它自己的 receiver**——也就是 PUBLISH 出来的消息会被 send 到这个连接的 sender，但本连接的 handle_client 循环还阻塞在 `read_command` 等下一条客户端命令，不会去读 receiver 把消息写到 socket。

这是教学简化。真实 Redis 在 SUBSCRIBE 后会进入一个专门的循环：既 poll 客户端命令，又 poll 自己的 receiver，谁先来处理谁。要实现这个需要非阻塞 IO 或 select——这是 M11 的内容。本章我们的测试**只用 PUBLISH 的返回值（投递数）验证扇出**，不验证订阅者真的收到字节流。这条边界我们在注释里讲清楚，留给练习。

> **练习**：把 handle_client 改成"SUBSCRIBE 后切到双轮询模式"。提示：把 socket 设成 nonblocking，用一个循环同时 `read_command`（非阻塞）和 `sub_rx.recv`（mpsc 的 recv 是阻塞的，你可以用 try_recv 或把 receiver 包成 channel + select）。

---

## 命令的串行处理：一条连接一个 BufReader

服务端的 `handle_client` 也持有一把长寿命的 `BufReader`：

```rust
fn handle_client(stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut subscribed: HashSet<String> = HashSet::new();
    let (sub_tx, sub_rx) = mpsc::channel::<SubMessage>();

    loop {
        let cmd = match read_command(&mut reader) {
            Ok(c) => c,
            Err(_) => break,
        };
        match cmd { /* GET/SET/DEL/PUBLISH/SUBSCRIBE/UNSUBSCRIBE */ }
    }
    // 客户端断开：清理所有订阅
    Ok(())
}
```

这一段隐含了**串行协议模型**：一条连接上的命令**按到达顺序串行处理**，前一条处理完（包括把响应写回 socket 并 flush）才读下一条。这是教学版简化，对应"客户端发一条命令、等回应、再发下一条"的使用模式。

但真实 Redis 客户端会用**管道（pipelining）**：连续发好几条命令，不等回应。服务端这边 `read_command` 会从 BufReader 里连续读出多条命令，按顺序处理，把响应逐条写回。我们的实现其实已经支持这种模式——`read_command` 从 reader 读一条，循环里再读一条，BufReader 的缓冲区把"管道里堆着的命令"都留住了，不需要额外代码。

注意一个细节：`writer.flush()` 必须显式调。`TcpStream` 的 `Write` 是直接写、不需要 flush，但我们用的是 `write!` 宏，它会调 `write_fmt`——在 BufWriter 才会缓冲。这里我们没有给 writer 包 BufWriter，所以每条 `write!` 都是直接 syscall，flush 是 no-op。但**如果你给 writer 加了 BufWriter**（生产里推荐，省 syscall），就必须 flush——否则响应憋在缓冲区里，客户端读不到，看起来像服务端卡死。

> **小练习**：给 writer 包一层 BufWriter，跑测试看看会不会失败。然后加 `writer.flush()` 修复。这会让你亲身感受"缓冲 = 必须显式 flush"。

---

## 客户端断开的清理

```rust
// 客户端断开后：
{
    let mut subs = state.subs.lock();
    for ch in &subscribed {
        if let Some(vec) = subs.get_mut(ch) {
            if !vec.is_empty() {
                vec.swap_remove(0);
            }
        }
    }
}
```

这段在 `handle_client` 退出时跑。它把这条连接订阅过的每个 channel 的 sender 列表里删掉一个。

**为什么是必要的？** 不删的话，subs 表里会留下指向这条已死连接的 sender。下次有人 PUBLISH，会往这个 sender 发消息——消息堆在 mpsc 的内部队列里，永远没人读，**内存泄漏**。一个跑了一周的服务，断了几万次连接，subs 表里堆了几万个死 sender，每次 PUBLISH 给它们都发一份——内存涨、CPU 涨。

**为什么 swap_remove(0) 是个简化？** 真实实现应该删"指向本连接的 sender"。但 mpsc::Sender 没有 `==`（不能直接比较两个 Sender 是不是同一个）。简化版假设"每条连接在每个 channel 上只订阅一次"，所以列表里第一个就是它的。这是一个**有 bug 的简化**——如果同一条连接对同一 channel 订阅了两次（我们的协议允许），就会删错。

教学版我们承认这个简化，并在注释里标出来。生产实现要么用"每个 sender 一个唯一 ID 配合删除"，要么用 `Arc<Mutex<Vec<Sender>>>` + 容忍偶尔多删（消息多投不致命）。M11 会讲怎么测这种"连接断开 + 状态泄漏"的 bug。

---

## 两个应用的形态对比

把爬虫和 mini-Redis 放在一张表里，看它们各自的"并发形态"：

| 维度 | 爬虫 | mini-Redis |
|------|------|------------|
| 任务形态 | 大量短任务（抓一个页面 50ms） | 少量长任务（一条连接活几小时） |
| 共享状态 | visited 集合、per_domain 限速表 | kv 表、subs 表 |
| 流量塑形 | 按域名限速（per-domain limiter） | 无（信任客户端） |
| 流水线 | 待抓队列（有界背压）+ 结果通道（无界汇聚） | TCP 连接本身就是流水线 |
| 结束判定 | in_flight 计数器 | 每条连接独立，连接断 = 任务完 |
| 失败模式 | fetch 失败 → 跳过这条 URL | 命令解析失败 → 关这条连接 |
| 主要敌人 | 重复抓、打爆对方、提前停 | 协议歧义、状态泄漏、早到消息丢失 |

这张表的价值在于：**当你下次遇到一个新需求**（比如"做一个并发日志收集器"），你可以问自己——它形态更接近爬虫（短任务、限速、汇聚），还是更接近 mini-Redis（长连接、协议、共享状态）？答案是前者，你照搬爬虫的骨架；是后者，照搬 mini-Redis 的骨架。这就是"原语拼装"的复用方式——不是复用代码，是复用**决策模板**。

## 本章踩过的真实 bug（编年史）

写本章代码时，我前后撞上四个 bug，把它们记下来——每一个都是教学金矿：

1. **结束判定提前退出**：第一版用"队列空 + 没新链接"判断结束。结果跑测试时**只抓到 1 个页面**就停了。原因：worker A 弹出种子 URL 后队列瞬间为空，但 A 正在 fetch、马上要吐 5 个新链接——主线程这一瞬间看到"空"就 break 了，那 5 个链接全丢。修法：用 in_flight_results 显式追踪"还有多少条结果会来"。

2. **结束判定计数器放错位置**：第二版把 in_flight 的 -1 放在 worker send 之后。结果**间歇性死锁**——主线程 recv 到结果，再看 in_flight 还是 > 0（worker 还没 -1），继续 recv，永远阻塞。修法：把 -1 移到 main recv 之后，让"消费"和"记账"绑死。

3. **限速器丢唤醒**：第三版用 `forge_lockfree::Semaphore`。压力测试下**75% 概率死锁**——多个 worker 在 `acquire` 上睡死，没人 release 给它们。原因是教学版 Semaphore 在多等待者工况下偶发丢 `wake_one`。修法：换成 `Mutex+Condvar` 版（DomainLimiter），用一把锁把"读计数 + 改计数 + 等待"压成原子，绝不丢唤醒。

4. **测试客户端 BufReader 陷阱**：写 mini-Redis 测试时**5 次跑 1 次超时**。原因是每次 `read_line` 新建一个 BufReader，旧 BufReader 销毁时把预读到的字节（下一条命令的响应）一起丢了。修法：让连接持有一把长寿命的 BufReader。

**这四个 bug 都有一个共同点**：它们不是"逻辑错"，是"时序错"。代码读起来都对，单线程跑也对，多线程压一下就崩。这就是并发编程的本质难度——你不仅要写对逻辑，还要写对**逻辑之间的 happens-before 关系**。M11 会专门讲怎么用 loom、strace、tsan 把这种时序 bug 抓出来。



---

## ISO·ZOOM：mini-Redis 的并发决策清单

| 决策 | 选择 | 为什么不是别的 |
|------|------|----------------|
| 协议 | RESP（长度前缀） | 不是空格分隔：value 含空格 / CRLF 会歧义 |
| 共享状态 | 两把锁（kv / subs） | 不是一把大锁：SET 不应阻塞 SUBSCRIBE；不是无锁：不值 |
| 锁顺序 | 单命令只持一把锁 | 不跨锁：天然无死锁；必须跨锁时全局统一顺序 |
| PUBLISH 扇出 | 锁内 clone 列表、锁外 send | 不是锁内 send：慢订阅者会拖所有人 |
| 每订阅者一通道 | 每个连接一个 mpsc | 不是共享 receiver：mpsc 单消费者无法广播 |
| happens-before | "注册 → 回执"顺序 + TCP | 不是"靠 sleep 等待"：那是 flaky |
| 连接模型 | thread-per-connection | 不是异步：教学焦点在并发决策；M11 换异步 |
| SUBSCRIBE 后 drain | 教学版不 drain | 真实版要双轮询；M11 / 练习 |

---

## 三个手算例子回顾

把本章的三个手算例子摆在一起，看它们的共通结构：

1. **按域名限速**（第二拍）：available 计数器 + cv 等待队列。逐拍画 worker 拿证 / 归证 / 排队。
2. **去重集竞争**（第三拍）：两步分锁 vs 一锁原子。逐拍画 W1 / W2 同时 contains 的 TOCTOU。
3. **pub/sub 扇出**（第四拍）：subs[channel] → clone senders → 锁外逐个 send。逐拍画服务端、3 个 receiver、publisher 的状态演化。

三个例子共同教会一件事：**并发正确性的关键不是"快"，而是"在什么时候、用什么原语、把哪些动作压成原子"**。锁、信号量、Condvar、通道——它们都是"压动作"的工具，区别只在压什么、压多紧。

---

## 五拍拆解（回顾）

- **ENEMY**：每台机器都有三堵墙（爬虫：打爆对方 / 重复抓 / 不知道停；mini-Redis：协议歧义 / 锁粒度 / 早到 PUBLISH）。
- **ANCHOR**：共享状态 + 流量塑形 + 流水线 + 结束判定。
- **LOW-FI**：先用最朴素的 `Mutex+Condvar+HashSet+VecDeque` 把机器跑起来——能跑的丑代码胜过跑不起来的漂亮抽象。
- **WRITE**：每一行都回到前面某一拍的原语，没有黑盒。
- **ISO·ZOOM**：把所有并发决策列成清单，每一条都写"为什么不是别的"。

---

## L1 – L5 自检

**L1（认得零件）**：本章用到了 M1（原子）、M5（mpsc）、M7（Mutex）、M8a（Semaphore 概念，但实际用了 Condvar 版）、M4（Arc）。指出每章对应 crawler.rs / mini_redis.rs 的哪一行。

**L2（理解为什么）**：为什么限速器用 `Mutex+Condvar` 而不是 `forge_lockfree::Semaphore`？为什么去重集是 `Mutex<HashSet>` 而不是 `RwLock<HashSet>`？为什么结束判定用 `AtomicUsize` 而不是"队列空"？

**L3（能手算）**：拿一张纸，画出"per_domain=2，瞬间来 5 个同域名 URL"的前 5 拍 available 和等待队列演化。画出"两 worker 同时 claim 同一链接"在错误版本和正确版本下的 visited / 队列演化。画出 PUBLISH news hello 扇出给 3 个订阅者的逐拍。

**L4（能预测）**：如果把 `in_flight_results` 的 -1 从 main 挪到 worker（send 后），会发生什么？如果把 BoundedQueue 改成无界，长跑会怎样？如果 SUBSCRIBE 的"注册"和"回执"调换顺序，会出什么 bug？

**L5（能迁移）**：给你一个"并发下载 1000 个文件、每个域名限速、写到一个汇总文件"的需求，你能复用本章的哪几块？哪些决策要改？

---

## 自检题

1. 为什么 `DomainLimiter::acquire` 用 `while available == 0` 而不是 `if`？
2. `claim` 函数返回 `bool`——true 和 false 分别意味着什么？为什么用 `HashSet::insert` 的返回值就够了，不用先 `contains`？
3. `BoundedQueue::push` 里 `while g.buf.len() >= g.cap && !g.closed` 这个 `!g.closed` 条件去掉会怎样？
4. `in_flight_results` 在 worker 解析出新链接时 +1，在主线程 recv 时 -1。如果两边都改成在 worker 里 +1/-1（worker send 时 -1），main 还能不能正确判定结束？为什么？
5. RESP 协议为什么用 `$LEN\r\nBYTES\r\n` 而不是 `BYTES\r\n`？
6. `serve` 函数为什么用 `thread::spawn` 而不是 M9a 的线程池？这个选择在什么场景下会反过来？
7. SUBSCRIBE 命令为什么"先注册后回执"而不是反过来？反过来会出什么 bug？

---

## 动手清单

- [ ] 把 `DomainLimiter` 换回 `forge_lockfree::Semaphore`，写一个压力测试（100 个 worker × 1000 次 acquire/release），看它会不会偶发死锁。用 `strace -e futex` 观察 syscall，定位丢唤醒的现场。（M11 预习）
- [ ] 给 BoundedQueue 加一个"溢出计数器"——push 时如果满了不阻塞，而是丢弃并计数。这种"有界丢弃"策略在什么场景下比"有界阻塞"更合适？
- [ ] 把 `handle_client` 改成 SUBSCRIBE 后双轮询（非阻塞 socket + try_recv）。验证订阅者真的能收到 PUBLISH 的消息字节流。
- [ ] 加一个 EXPIRE 命令：`EXPIRE key 10` 让 key 10 秒后过期。提示：用一个后台线程定期扫 kv 表，或者每条 GET 时惰性检查时间戳。两种做法的取舍是什么？
- [ ] 给 crawler 加一个 robots.txt 解析：每个域名抓之前先抓 `/robots.txt`，遵守 Disallow。这要在 `Fetcher` trait 上加方法吗？还是另起一个抽象？
- [ ] 把 mini-Redis 的 KV 从 `Mutex<HashMap>` 换成 M8 的无锁结构（比如 lock-free hash map，或者 SeqLock 包装的 HashMap）。基准测试对比 QPS。
- [ ] 用 M9b 的异步运行时重写 mini-Redis 的 `serve`：每条连接是一个 task，Reactor 负责 epoll。对比 thread-per-connection 版本在 1000 连接下的内存占用。

---

## 本章学到的原语对应表

| 原语 | 出自 | 在本章的角色 |
|------|------|--------------|
| `Arc<T>` | M4 | 共享 CrawlState / ServerState / DomainLimiter |
| `Mutex<T>` | M7 | visited 去重集、kv 存储、subs 订阅表、per_domain 表 |
| `Condvar` | M7 | DomainLimiter 的 cv、BoundedQueue 的 not_full/not_empty |
| `mpsc::channel` | M5 | worker→main 结果汇聚；pub/sub 每订阅者一通道 |
| `Semaphore`（概念） | M8a | DomainLimiter 的语义来源（实现换成了 Condvar 版） |
| `AtomicUsize` | M1 | in_flight_results 结束判定计数器 |
| scoped thread | std | worker 池、连接处理 |
| `thread::spawn` | std | mini-Redis 的 thread-per-connection |

---

# 第三部分：三个补缺子应用

主干里的爬虫和 mini-Redis 已经把"怎么用前面九章的原语拼一台机器"讲透了。但真实世界的并发还有三种**反复出现的形状**，原书把它们单独拎出来讲：**响应式事件总线**（Async Rust 第 6 章）、**Actor 模型**（第 8 章）、**零依赖 epoll 服务器**（第 10 章）。

这一部分给这三个形状各补一个**自包含的最小实现**——每一台都和前面 mini-Redis / crawler 共享同一套原语（`Arc`、`Mutex`、`Condvar`、`mpsc`），但每一台揭示一个**前面没单独讲透的并发决策点**。

- **事件总线**揭示：当一条消息要送给很多收件人时，"广播扇出"和 mpsc 的"单消费者"语义有何不同，以及"慢订阅者会不会拖垮整体"这个背压问题。
- **Actor** 揭示：除了"加锁保护共享状态"，还有"把状态封进单线程、外部只发消息"这一条路——它是 M2 那条"数据竞争公式"的另一种解。
- **epoll 服务器** 揭示：mini-Redis 那个"thread-per-connection"在 1000 连接时为什么会崩——以及 Linux 内核给的多路复用接口长什么样。

每节仍然按"敌人先行 → 画面先于代码 → 手算逐拍 → 代码 → 深挖"的顺序走。三节都独立成篇，但都呼应主干里讲过的东西——读完你会看到这三台机器其实是 mini-Redis 的三种"切片"。

---

# 子应用之一：响应式事件总线（呼应 Async Rust 第 6 章）

## 敌人先行：同一条消息要送给很多人，谁来抄副本？

想象你在写一个聊天室服务。Alice 发了一条"hello"——服务端要让 Bob、Carol、Dave **每人都收到一份完整副本**。这听上去是天经地义的需求，但如果你不假思索地用我们 M5 自研的 `forge_channel::mpsc`，你会撞上一堵墙。

`mpsc` 是**多生产者单消费者**：N 个 sender 把消息推进**同一条**队列，只有一个 receiver 在另一端取。如果你让 PUBLISH 端用一条 mpsc 队列推送"hello"，第一个 receiver 取走之后——**消息就从队列里消失了**。第二个 receiver 永远看不见它。

这就是敌人：**一条消息的"广播"和 mpsc 的"消费"语义是冲突的**。mpsc 是"读完即销毁"，广播要的是"每人一份完整副本"。如果你直接套 mpsc，你做出来的不是聊天室，是抢答器。

## 画面先于代码：抄写员与一摞信封

闭上眼睛想一个画面：你是图书馆前台。一份新到的杂志（消息）放在柜台上。规则是：**每个登记过的读者都要拿到这份杂志的一份复印本**。你不是把杂志交给第一个伸手的人就完事——你身后站着一个抄写员，每来一份杂志他抄 N 份（N = 登记的读者数），然后每人信箱里各塞一份。

那个"登记表"就是 **Subject**（被观察的对象）。每个登记过的读者就是一个 **Observer**（观察者）。抄写员的工作——"抄 N 份，各塞一份"——就是事件总线的**publish**。

把它翻译成代码层面的对应物：

| 图书馆类比 | 代码对应 |
|----------|---------|
| 登记表 | `subs: Mutex<Vec<SubEntry>>` |
| 一个读者的信箱 | 一条独立的 `mpsc::Sender<T>`（注意：是**每订阅者一条独立队列**，不是所有人共用一条） |
| 抄写员抄 N 份 | `for entry in subs.iter_mut() { entry.sender.send(msg.clone()) }` |
| 新杂志 | `publish(&msg)` 的入参 |

关键洞察：**广播不是"让 N 个 receiver 共享一条队列"，而是"给每个 receiver 发一条独立的队列"**。每个订阅者 `subscribe()` 时，事件总线给他**新建一条** mpsc 通道，把 sender 留在登记表里，receiver 还给他。publish 时遍历登记表里所有 sender，**每个 send 一份 `msg.clone()`**。这就是"广播扇出"。

注意这条洞察和我们 mini-Redis 那张 `subs: HashMap<String, Vec<Sender>>` **完全同构**——mini-Redis 是"按 channel 分桶"的事件总线，本节做的是**单主题**的版本。把多个 `EventBus` 放进一张 `HashMap<String, EventBus<T>>`，就重新组装出了 mini-Redis 的 pub/sub。

## 手算例 1：3 个订阅者的扇出逐拍

这一段是本节的"高潮"，必须能在脑子里逐拍过一遍。设：topic "news"，订阅者 A、B、C，他们已经各自调过 `subscribe()` 拿到了 receiver `rA / rB / rC`。现在 publisher 调 `publish(&"hello".to_string())`。

**第 0 拍（publish 入口）**：

```
登记表 subs（一个 Vec）:
  [ { sender: sA, queued: 0 },
    { sender: sB, queued: 0 },
    { sender: sC, queued: 0 } ]
队列实际内容：
  rA 的队列: []     ← 空
  rB 的队列: []     ← 空
  rC 的队列: []     ← 空
```

**第 1 拍（拿到锁，开始遍历 subs）**：进入循环，先看 A 那一条。策略是 DropOldest，A 的 queued (0) < cap (16)，**决定 send**。调 `sA.send(msg.clone())`——**第一次 clone** 发生在这里。`msg.clone()` 是 `"hello".to_string()` 的克隆，堆上多一份字符串。

```
rA 的队列: ["hello"]   ← sA 推了一份进去
queued(A) := 1
delivered := 1
```

**第 2 拍（看 B 那一条）**：同样 send，**第二次 clone**。

```
rB 的队列: ["hello"]
queued(B) := 1
delivered := 2
```

**第 3 拍（看 C 那一条）**：同样 send，**第三次 clone**。

```
rC 的队列: ["hello"]
queued(C) := 1
delivered := 3
```

**第 4 拍（遍历结束，释放锁，返回 delivered = 3）**。

注意 clone 的次数 = 实际 send 的订阅者数 = 3。如果我们当时策略决定不发（比如 DropNewest 且满了），就不 clone——避免无谓的堆分配。这条对性能很关键：当 `T` 很大时（比如一份 1MB 的 JSON），广播给 100 个订阅者要 clone 100 次。教程末尾的练习里我们会让 `T` 包成 `Arc<T>`，让 clone 只是引用计数加 1。

**第 5 拍之后**：A、B、C 三个消费方各自在自己的线程里 `rA.recv() / rB.recv() / rC.recv()`，分别拿到 `"hello"`。注意：**这三方互相完全独立**——A 慢一点不影响 B 拿到自己的副本（只要 B 的队列没爆）。这就是广播相对于"消息总线单消费者"的核心好处。

把它对照 mini-Redis 的 PUBLISH 流程：那条 `for tx in &senders { tx.send(...) }` 和本节的 publish **逐拍一一对应**。mini-Redis 的 subs 表是一个 `Vec<Sender>`，本节的 `Vec<SubEntry>` 多了一个 `queued` 计数——这是为了下一节的"背压"。

## 背压：一个慢订阅者会不会拖垮整体？

到这里你可能已经嗅到敌人了。想象 C 是个慢订阅者——他每秒只能处理 1 条消息，但 publisher 每秒推 10000 条。C 的队列会无限堆积。10000 条 × 1KB = 10MB / 秒，几小时后 OOM（Out Of Memory），整个进程被 kill。

这就是**背压（backpressure）**问题。它的本质是：**生产者和消费者的速度不匹配**，系统里有个"无界缓冲"在中间当垃圾桶。我们的 mpsc 是无界的，所以天然就是个背压炸弹。

工程界给的三种标准应对：

1. **`Block`（阻塞）**：publisher 在慢订阅者上阻塞，等它消费。**好处**：自然反压——慢订阅者拖慢 publisher，publisher 拖慢上游。**坏处**：一个慢订阅者把整个总线拖住，其它快订阅者也跟着饿死。这个策略只适合"所有订阅者都重要、宁可慢也不能丢"的场景，比如分布式事务里的状态广播。
2. **`DropOldest`（丢最旧）**：满了之后丢队列里最老的消息，给新消息腾位置。**适合**：实时股价显示——旧的没看见也无所谓，要的是最新值。**`tokio::sync::broadcast` 用的就是这个**——它内部是个环形缓冲，滞后订阅者的旧消息被新消息覆盖。
3. **`DropNewest`（丢最新）**：满了之后丢新消息，保留旧的。**适合**：审计日志——宁可漏新也不能漏旧。

我们在 `event_bus.rs` 里给一个 `OverflowPolicy` 枚举，把决策点摆在台面上。注意：**真实实现**里"丢最旧"需要一个环形缓冲（ring buffer），我们的 mpsc 是 `VecDeque` 无界的——所以本教学版的 DropOldest 实际上是"满了继续 send，但计数不增"的近似。真正的环形缓冲留给练习。

这条决策不止出现在事件总线里。回到 crawler：那个 `BoundedQueue` 就是 `Block` 策略——push 满了就阻塞。回到 mini-Redis：那条 `for tx in &senders { tx.send(...) }` 是无界 send，所以**慢订阅者会把它的 sender 队列撑爆**——这是 mini-Redis 留给我们的一个未解决的坑，教程末尾的"动手清单"里有相关的练习。

## 代码：一个最小的 EventBus

`crates/forge-app/src/event_bus.rs` 实现了一台自包含的事件总线。核心结构：

```rust
struct SubEntry<T> {
    sender: mpsc::Sender<T>,
    queued: usize,  // 当前堆积计数，给背压用
}

pub struct EventBus<T: Clone + Send> {
    subs: Arc<Mutex<Vec<SubEntry<T>>>>,
    cap: usize,
    policy: OverflowPolicy,
}
```

`subscribe()` 每次调都新建一条 mpsc 通道，sender 入表、receiver 返回给订阅者：

```rust
pub fn subscribe(&self) -> Subscription<T> {
    let (tx, rx) = mpsc::channel::<T>();
    self.subs.lock().push(SubEntry { sender: tx, queued: 0 });
    rx
}
```

`publish(&msg)` 就是手算例 1 的代码版：

```rust
pub fn publish(&self, msg: &T) -> usize {
    let mut delivered = 0;
    let mut subs = self.subs.lock();
    for entry in subs.iter_mut() {
        let should_send = match self.policy {
            OverflowPolicy::DropNewest => entry.queued < self.cap,
            OverflowPolicy::DropOldest => true, // 满了仍 send，计数封顶
            OverflowPolicy::Block      => entry.queued < self.cap, // 教学版无法真正阻塞
        };
        if should_send {
            entry.sender.send(msg.clone());   // ← 这里就是抄写员
            if entry.queued < self.cap { entry.queued += 1; }
            delivered += 1;
        }
    }
    delivered
}
```

注意**返回值是 `usize`**——"成功送出"的订阅者数。这给调用方一个信号：如果 publish 返回值 < subscriber_count()，说明背压策略让一些订阅者被丢掉了。这条信号在监控里很有用——"publish 0 次 / 总 100 次"意味着订阅者普遍跟不上。

## 多主题路由器：把多个 EventBus 装进 HashMap

mini-Redis 是"按 channel 分桶"的事件总线。本节给一个 `TopicBus<T>`——把若干个 `EventBus` 装进 `HashMap<String, EventBus<T>>`，每个 topic 一条独立总线：

```rust
pub struct TopicBus<T: Clone + Send + 'static> {
    topics: Arc<Mutex<HashMap<String, EventBus<T>>>>,
}
```

`subscribe(topic)` 在 topic 不存在时新建一条总线，`publish(topic, msg)` 查表分发。这一层和 mini-Redis 的 `ServerState.subs` **结构对称**——区别是 mini-Redis 直接用 `Vec<Sender>`，本节用 `EventBus`（多了 cap + policy）。把 `TopicBus` 当 mini-Redis 的 pub/sub 的"加强版"看就行。

## 深挖：为什么"广播"不能用 RwLock 加速？

读者会问：subs 表只读不写时（比如 publish 期间我们改的是 entry.queued 而不是 Vec 本身），能不能用 `RwLock` 让多个 publish 并行？答案是**不能**——因为 publish 在遍历时会写 `entry.queued`，这不是"读 subs 表"，是"写 entry 内部字段"。哪怕用 RwLock，写 queued 也要拿 write lock。所以这里 Mutex 比 RwLock 更直接，没有性能损失。

如果真要并行 publish，得给每个 SubEntry 单独加锁——粒度细到每条订阅者一把。这个练习留给动手清单。

## 与 mini-Redis pub/sub 的对应

把 `TopicBus` 摆在 mini-Redis 旁边：

| mini-Redis | TopicBus |
|-----------|----------|
| `ServerState.subs: Mutex<HashMap<String, Vec<Sender>>>` | `TopicBus.topics: Mutex<HashMap<String, EventBus<T>>>` |
| `subs.entry(channel).push(sender)` | `topics.entry(topic).subscribe()` |
| `for tx in senders { tx.send(...) }` | `bus.publish(&msg)` |
| 无背压（无界 send） | cap + OverflowPolicy |

两台机器**思想完全相同**。TopicBus 把"每条 topic 的扇出 + 背压"封进 EventBus，把"topic 路由"留在 TopicBus 这一层——这是更清晰的分层。

---

# 子应用之二：Actor 模型（呼应 Async Rust 第 8 章）

## 敌人先行：共享可变状态的两种死法

回到 M2 给的"数据竞争公式"：

```
数据竞争 = 共享可变状态 + 并发执行
```

M7 给的解法是**加锁**：把共享状态包进 `Mutex<T>`，谁要改谁拿锁。这是crawler 和 mini-Redis 走的路。但这条路有两个老问题：

1. **死锁**：两个线程各拿一把锁，互相等对方那把锁，永远卡住。M7 教过避免死锁的"锁顺序"原则，但只要项目一大，违反顺序的临界区总会冒出来。
2. **思维负担**：每一处临界区都要小心翼翼地想"我拿的这把锁会不会被别人持着？我持锁期间能不能调外部函数？外部函数会不会反过来再拿这把锁（重入死锁）？"——这些都不是单看代码就能看出的。

这一节讲**第二条路**：**根本不让任何人直接碰状态**。把状态封进一个**单线程**里，那个线程就是 actor；其他人想改它，**只能往它的信箱里投消息**。actor 自己一条条处理消息、更新状态。因为只有一个线程碰状态，**死锁和数据竞争在物理上不可能发生**。

这条路的代价是：**响应要靠"消息里塞回复通道"才能拿回来**（见下方 Get 例子）。代价换来的好处是：你写 actor 内部的 handler 时，**完全不需要锁**——它是纯单线程思维。

## 画面先于代码：邮局信箱和专属办事员

闭上眼睛想：一个公务员坐在小房间里，桌上有一个信箱。外面的人不能直接进房间改文件——所有改动必须**写信**扔进信箱。办事员一条条拆信、按信里说的办事、改桌上的文件。一天工作结束时信箱空了，办事员下班。

翻译：

| 邮局类比 | 代码对应 |
|---------|---------|
| 小房间 + 办事员 | actor 的后台线程 + 它持有的 state |
| 信箱 | inbox（一条 `mpsc::Receiver<M>`） |
| 信 | 一条 `M` 类型消息 |
| 外面的人 | 调 `handle.send(msg)` 的使用方 |
| 投信口 | `Handle<M>`（包了 `mpsc::Sender<M>`） |

关键性质：**文件（state）只活在办事员那一个房间里**。外面的人拿不到文件的引用——他们只能写信投递。这意味着**多线程并发投信是安全的**——投信是 mpsc 的"多生产者"，mpsc 内部自己保证并发 send 不出问题（M5 讲过）。办事员拆信是**严格串行**的，所以 state 永远只被一个线程碰——无锁、无竞态。

这就是 actor 的核心：**用消息传递代替共享状态**。它不是"避免"数据竞争，是"从物理上消除"数据竞争发生的可能性。

## 手算例 2：两个 Handle 同时发 Inc 的逐拍

设：一个 Counter actor，初始值 5。两个线程 T1 和 T2 同时拿到 Handle（实际上 Handle 可以 Clone，两个线程各持一份）。两人都发一条 `Inc(1)`。我们要画**消息从投递到处理的完整流转**。

**第 0 拍（初始）**：actor 线程正阻塞在 `rx.recv()` 上等消息。state.value = 5。

```
inbox 队列: []           ← 空
T1: 即将 send(Inc(1))
T2: 即将 send(Inc(1))
```

**第 1 拍（T1 send）**：T1 调 `handle.send(Inc(1))`。mpsc::send 把消息推进队列尾部，然后 `notify_one` 唤醒一个等待的接收者（actor 线程）。

```
inbox 队列: [Inc(1)]     ← T1 的消息入队
```

**第 2 拍（T2 send，几乎同时）**：T2 也调 `handle.send(Inc(1))`。第二条消息入队。注意：T1 和 T2 在 send 时**互相不加锁**——mpsc 内部的 Mutex 只在 push_back 那一瞬短锁，不影响并发吞吐。

```
inbox 队列: [Inc(1), Inc(1)]   ← T1 和 T2 的两条都在
```

**第 3 拍（actor 线程被唤醒，recv 返回第一条）**：`rx.recv()` 从队列头 pop 出 `Inc(1)`。队列里还剩一条。

```
inbox 队列: [Inc(1)]     ← T2 的那条还在
handler 被调用: state.value 从 5 → 6
```

**第 4 拍（handler 返回，循环继续 recv）**：再 pop 一条。

```
inbox 队列: []
handler 被调用: state.value 从 6 → 7
```

**第 5 拍（handler 返回，循环继续 recv）**：队列空了。actor 线程阻塞在 `rx.recv()` 上，等下一条消息。state.value = 7。

注意整个过程**完全无锁**——actor 内部没有一处 `Mutex::lock`。两个并发 send（T1 和 T2）是 mpsc 的"多生产者"语义保证安全的；state 只被 actor 那一个线程碰，所以更新 state 不需要任何同步。这就是"用消息传递代替共享状态"的物理保证。

对比 Mutex 方案：如果 Counter 是 `Arc<Mutex<i64>>`，T1 和 T2 各 `lock()`、改值、`unlock()`。两次 lock/unlock 的 syscall 不便宜，而且**思维上**每处临界区都要担心"我持锁期间能不能调用其它可能也拿这把锁的函数"。actor 方案把这些心智负担全消除了。

## 代码：Counter actor

`crates/forge-app/src/actor.rs` 给了完整的实现。最关键的是那个 actor 循环——它就是手算例 2 第 3-5 拍的代码版：

```rust
pub fn spawn<M, S, F>(initial_state: S, handler: F) -> (Actor<M>, Handle<M>)
where
    M: Send + 'static, S: Send + 'static,
    F: Fn(&mut S, M) + Send + 'static,
{
    let (tx, rx) = inbox_channel::<M>();           // 自包含 inbox（支持关闭语义）
    let handle = Handle { tx: tx.clone() };
    let shutdown_tx = Some(tx);

    let join = thread::Builder::new()
        .name("forge-actor".into())
        .spawn(move || {
            let mut state = initial_state;
            // —— 核心 actor 循环：一条条处理 inbox 里的消息 ——
            while let Some(msg) = rx.recv() {       // ← 第 3-5 拍在这里发生
                handler(&mut state, msg);
            }
            // 所有 sender drop 后 recv 返回 None，循环退出
        })
        .expect("failed to spawn actor thread");

    let actor = Actor { handle: Some(join), shutdown_tx };
    (actor, handle)
}
```

注意 `while let Some(msg) = rx.recv()`——当所有 Handle 都 drop 之后，inbox 检测到"sender 都没了"，recv 返回 None，循环退出，线程正常结束。这就是 actor 的"信箱关了才下班"语义。

Counter 的消息和 handler：

```rust
pub enum CounterMsg {
    Inc(i64),
    Get(Reply<i64>),       // ← 回复通道塞在消息里
}

pub fn counter_handler(state: &mut CounterState, msg: CounterMsg) {
    match msg {
        CounterMsg::Inc(n) => state.value += n,
        CounterMsg::Get(resp) => { let _ = resp.send(state.value); }
    }
}
```

`Get(Reply<i64>)` 是 actor 模式的精髓：**回复通道塞在消息里**送到 actor 线程，actor 处理完用 `resp.send` 把答案送回。这条 Reply 通道是基于 `Arc<Mutex<Option<T>> + Condvar>` 写的（详见 actor.rs 顶部）——我们没用 `forge_channel::oneshot`，因为那一版是**借用版**（`Sender<'a, T>`），生命周期 `'a` 让它没法塞进消息跨线程移动。所以这里写了一个 `'static` 的等价物。

## 第二个示例：KV actor

```rust
pub enum KvMsg {
    Set(String, String),
    Get(String, Reply<Option<String>>),
    Del(String),
}

pub fn kv_handler(state: &mut KvState, msg: KvMsg) {
    match msg {
        KvMsg::Set(k, v) => { state.map.insert(k, v); }
        KvMsg::Get(k, resp) => { let _ = resp.send(state.map.get(&k).cloned()); }
        KvMsg::Del(k)     => { state.map.remove(&k); }
    }
}
```

注意 `state.map` 是个普通 `HashMap`——**没有任何锁包裹**。因为只有 actor 线程碰它，外部只能发消息。对比 mini-Redis 那个 `kv: Mutex<HashMap<String, String>>`：mini-Redis 走 Mutex 路线，每条 GET/SET 都 lock/unlock；KV actor 走消息路线，外部发 Set/Get 消息，actor 内部串行处理。**两种方案在语义上等价，性能也接近**，但 actor 方案让"什么操作会和什么操作互斥"更清晰——所有操作都串行，不存在锁粒度问题。

## Actor vs Mutex：什么时候选哪个？

这是初学者最常问的问题。给一个判定清单：

**选 Mutex**：
- 状态访问是**短临界区**（几条原子操作就能完成）。
- 多个操作之间**不需要复杂编排**——只是简单的 read / write。
- 想要**低延迟**——一次 lock/unlock 比一次 send/recv 快 10 倍以上。
- 例子：mini-Redis 的 kv 表、crawler 的 visited 集合。

**选 Actor**：
- 状态有**复杂的内部不变量**——多步操作必须原子完成，且这些操作之间有依赖。
- 想把**并发简化成串行**——actor 内部不需要考虑锁顺序、重入、粒度。
- 需要**状态机语义**——比如"连接处于 CONNECTING 状态时拒绝 Send"。
- 例子：数据库的连接池（每个连接是一个 actor）、TCP 连接状态机、带状态的协议解析器。

Async Rust 第 8 章用一个"路由 actor"展示更高级的用法——actor 之间互相发消息（actor A 把消息转发给 actor B），形成一个**消息驱动的拓扑**。这种结构在分布式系统里特别常见（Erlang/OTP 整个就是 actor 模型）。

## 深挖：为什么"消息传递"能避免死锁？

Mutex 方案的死锁来自"两把锁循环等待"。Actor 方案里**根本没有锁**——每个人只往信箱里投消息。投消息（mpsc::send）是无界的、立即返回的，不会阻塞等待别人。

但 actor 不是没有死锁——它换了一种形式：**消息循环依赖**。Actor A 发消息给 B、等回复；B 处理时发消息给 A、等回复。两个 actor 都在等对方的回复，谁也不会处理下一条消息——这就是 actor 版的"死锁"。规避它的方法和 Mutex 版类似：**不要让 actor 在等回复时阻塞自己处理消息的循环**。Async Rust 第 8 章后半段的"supervision"和"rebind"机制就是为了处理这种问题。

## 第三个示例：把 actor 和事件总线连起来

actor 抽象是可组合的。`actor.rs` 给一个 broadcast actor——它的状态持有一个 `EventBus<T>`，每收到一条消息就 publish 到总线上：

```rust
pub fn broadcast_handler<T: Clone + Send + 'static>(state: &mut BroadcastState<T>, msg: T) {
    state.bus.publish(&msg);
}
```

这就是"actor 嵌进事件总线"的最小例子。你可以想象一个聊天室：每来一个客户端，给它起一个 actor 持有自己的 EventBus；客户端发消息 = 给 actor 投信；actor 把消息广播给所有订阅者。整个系统**没有任何一把锁**——并发安全完全靠"消息传递 + 单线程 state"两个原语保证。

---

# 子应用之三：零依赖 epoll 服务器（呼应 Async Rust 第 10 章）

## 敌人先行：mini-Redis 在 1000 连接时会死在哪？

回到 mini-Redis 那个 `serve` 函数：每来一条连接 `thread::spawn` 一个线程处理。1000 连接 = 1000 线程。听起来好像没问题，但实际跑起来会撞上三堵墙：

1. **栈内存**：默认每个线程 8MB 栈，1000 个就是 8GB——单机内存装不下。
2. **调度抖动**：操作系统调度 1000 个线程时，上下文切换开销指数增长。每个线程获得 CPU 时间片的间隔变长，响应延迟恶化。
3. **cache miss**：1000 个线程的栈和热点数据在 L1/L2 cache 里互相挤兑，cache 命中率暴跌。

C10K 问题（一台机器同时管 10000 连接）就是在讨论这个。解决办法叫 **I/O 多路复用**：**一个线程同时监视几千条 socket**，操作系统在 socket 上有事件（可读 / 可写 / 出错）时叫醒这个线程。

在 Linux 上，多路复用的现代接口叫 **epoll**。这一节我们**直接调 libc 的 syscall**——不依赖 mio、不依赖 tokio——把"一台机器扛 10 万连接"的运行时内核从底层扒开给你看。

## 画面先于代码：一个门卫和一摞名牌

闭上眼睛想：你是一栋大楼的门卫。大楼里有 1000 个房间（1000 条 socket 连接），每个房间住着一个客户。你的工作是：**任意一个客户的房间灯亮了（有数据来了），你要立刻去那个房间处理**。

最笨的做法（thread-per-connection）：**雇 1000 个门卫**，每人盯一个房间。开销爆炸。

聪明的做法（epoll）：**只雇 1 个门卫**。门卫桌上有一摞名牌——每个名牌写着一个房间号。门卫**每 5 分钟看一次"哪些房间的灯亮了"**——操作系统会告诉他"今天 3 号、17 号、42 号三个房间的灯亮了"。门卫就依次去那三个房间处理。

那"操作系统怎么知道哪些灯亮了"？答案是内核维护了一棵"我监视的房间"表。门卫每隔一段时间问内核："我监视的这些房间里，哪些有动静？"——这就是 `epoll_wait`。

把它翻译成代码对应：

| 门卫类比 | 代码对应 |
|---------|---------|
| 摞名牌（被监视的房间列表） | 一棵 epoll 实例（内核对象） |
| 把一个新名牌放上桌 | `epoll_ctl(EPOLL_CTL_ADD, fd, ...)` |
| 把一个名牌拿走 | `epoll_ctl(EPOLL_CTL_DEL, fd, ...)` |
| 问"哪些灯亮了" | `epoll_wait(epfd, &events, ...)` |
| 内核告诉你的"亮灯房间号列表" | 返回的 events 数组 |

## epoll 三步走

具体的 API 三步：

1. **建一棵 epoll 实例**：`int epfd = epoll_create1(EPOLL_CLOEXEC);` 返回一个 fd。这个 fd 是一个内核对象，你接下来"想监视哪些 fd"都登记到它身上。
2. **登记关心的 fd**：`epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &event)`。`event.events` 字段写"这条 fd 上发生什么事件我才被叫醒"——比如 `EPOLLIN`（可读）。`event.u64` 字段写"叫醒我时怎么认出这条 fd"——我们直接把 fd 本身塞进去。
3. **等事件发生**：`int n = epoll_wait(epfd, &mut events, max, timeout_ms);`。阻塞直到至少有一个事件就绪，或者超时。返回值 n 告诉你"这次叫醒你有几条 fd 就绪了"，你遍历 `events[0..n]` 处理。

## edge-triggered vs level-triggered：本节最容易翻车的点

epoll 注册 fd 时可以加一个标志 `EPOLLET`——它把触发模式从默认的 **level-triggered** 切换到 **edge-triggered**。两种模式的行为完全不同，**翻车就翻在这里**。

**level-triggered（默认）**：只要 fd 上**还有数据可读**，每次 `epoll_wait` 都会把它返回。你读了一半？下次 wait 它还在。你不读？它会一直出现在每次 wait 的返回里。

**edge-triggered（`EPOLLET`）**：只在 fd **状态变化时**通知**一次**——从"无数据"变成"有数据"的那一刻通知一次。如果你只读了一半就回去 wait，**不会再被叫醒**——内核认为"已经通知过你了，剩下的你自己的事"。

听起来 edge-triggered 坑更大，为什么还要用？因为它**系统调用次数更少**——level-triggered 每次都重复通知同一个 fd，浪费 CPU 在"重复唤醒同一个 fd"上。代价是 edge-triggered **必须读到 EAGAIN**（read 返回 WouldBlock），把 fd 里的数据彻底抽干——否则剩下的数据会被饿死。

## 手算例 3：edge-triggered 下两条连接同时就绪

设：echo 服务器有两条已建立的连接 C1（fd=5）和 C2（fd=6）。客户端同时分别发了 "hello"（5 字节）和 "world"（5 字节）。我们用 edge-triggered 模式。epfd 上登记了 fd=5 和 fd=6 都监听 EPOLLIN|EPOLLET。

**第 0 拍（两个客户端各发一段数据，几乎同时到达内核）**：

```
内核 socket 接收缓冲：
  fd=5: [hello]   ← 5 字节
  fd=6: [world]   ← 5 字节
epoll_wait 阻塞中，等待事件
```

**第 1 拍（epoll_wait 返回 n=2）**：内核告诉服务器"5 和 6 两条 fd 都就绪了"。`events[0]` = {u64=5}，`events[1]` = {u64=6}。

```
events 数组: [{u64:5}, {u64:6}]
服务器开始遍历
```

**第 2 拍（处理 events[0]，fd=5）**：调 `read(5, buf)`。读到 "hello"。**关键：edge-triggered 必须循环 read 到 EAGAIN**——再调一次 `read(5, buf)`，返回 -1，errno=EAGAIN（WouldBlock）。表示 fd=5 的数据抽干了。把 "hello" `write(5, ...)` 回去（echo）。

```
fd=5 接收缓冲: []   ← 抽干了
fd=5 发送缓冲: [hello]   ← 写回去了
```

**第 3 拍（处理 events[1]，fd=6）**：调 `read(6, buf)`。读到 "world"。再调一次 `read(6, buf)`，EAGAIN。write 回去。

```
fd=6 接收缓冲: []
fd=6 发送缓冲: [world]
```

**第 4 拍（events 数组处理完，回到 epoll_wait 阻塞）**：等待下一批事件。

**第 5 拍（如果 fd=5 那次只读了一次，没读到 EAGAIN，会发生什么？）**：假设 C1 紧接着又发了 "again"（5 字节），但服务器在第 2 拍只 `read(5)` 一次就回去处理 events[1] 了。此时 fd=5 的缓冲里还剩 [again]。**因为 edge-triggered 只在"从无到有"那一瞬间通知一次**，而第 1 拍已经通知过了，这次 "again" 的到达**不会触发新的 epoll_wait 唤醒**——fd=5 的数据被饿死，永远不会被 echo 回去。

这就是 edge-triggered 的核心陷阱：**必须读到 EAGAIN**。我们的 `handle_conn` 函数里就是 `loop { read ... if WouldBlock break }` 的写法，把这条规则在代码里固化下来。

## 为什么必须配合非阻塞 socket？

这个细节很多教程一笔带过，但它是 edge-triggered 能工作的**物理前提**。

想象 edge-triggered 模式下，一个客户端发了 100 字节，你被叫醒，调 `read` 读到 50 字节——但客户端**还在慢慢发**剩下的 50 字节。如果你的 socket 是**阻塞**的，下一次 `read` 会**卡住**——等到那 50 字节真的到了才返回。这期间你**整个事件循环被冻住**——其它几千条连接全晾着。这就是为什么 edge-triggered **必须配非阻塞 socket**：read 没数据时立即返回 EAGAIN，让你回去 wait 等下一条 fd。

我们的 listening socket 和 accept 出来的每条新连接都设了 `SOCK_NONBLOCK`——一上来就非阻塞。这一条是 epoll + edge-triggered 工作的硬要求。

## 代码：一台 epoll echo 服务器

`crates/forge-app/src/bare_server.rs` 给了完整实现。注意它在非 Linux 平台上**整模块为空**（`#![cfg(target_os = "linux")]`）——epoll 是 Linux 专属。

`bind` 函数把三步走的前两步做了：

```rust
let epfd = unsafe { epoll_create1(EPOLL_CLOEXEC) };   // step 1
// ... 建 listening socket，setnonblock，bind，listen ...
let mut ev = epoll_event {
    events: (EPOLLIN | EPOLLET) as u32,   // ← edge-triggered
    u64: listen_fd as u64,                 // ← 叫醒时这样认出它
};
epoll_ctl(epfd, EPOLL_CTL_ADD, listen_fd, &mut ev);   // step 2
```

`serve` 是事件循环——手算例 3 第 1-4 拍的代码版：

```rust
loop {
    let n = epoll_wait(self.epfd, events.as_mut_ptr(), max, timeout);
    for i in 0..n {
        let ev = &events[i];
        let fd = ev.u64 as RawFd;
        if fd == self.listen_fd {
            // listening fd 就绪：accept 所有排队新连接（edge-triggered 必须循环 accept 到 EAGAIN）
            loop {
                let new_fd = accept4(self.listen_fd, ..., SOCK_NONBLOCK);
                if new_fd < 0 && WouldBlock { break; }
                self.add_conn(new_fd)?;
            }
        } else {
            // 已建立连接就绪：read 直到 EAGAIN，把读到的写回
            self.handle_conn(fd)?;
        }
    }
}
```

`handle_conn` 是手算例 3 第 2-3 拍的代码版——**循环 read 到 EAGAIN**：

```rust
fn handle_conn(&self, fd: RawFd) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd, ...) };
        if n < 0 {
            if err.kind() == WouldBlock { return Ok(()); }  // ← 抽干了
            self.remove_conn(fd); return Ok(());
        }
        if n == 0 { self.remove_conn(fd); return Ok(()); }  // EOF
        // echo 回去
        libc::write(fd, &buf[..n], ...);
    }
}
```

注意 `if n == 0`——read 返回 0 表示对端正常关闭（EOF）。这时候要 `remove_conn`（从 epoll 摘除并 close fd），否则那个 fd 会一直留在 epoll 实例里，每次 wait 都返回它（level-triggered）或者再也不返回但占着内核资源（edge-triggered）。

## 深挖：accept 也要循环到 EAGAIN

一条容易被忽略的细节：listening socket 在 edge-triggered 模式下，**accept 也要循环**。如果一次 `epoll_wait` 返回 listening fd 就绪，内核可能在那个时刻往 accept 队列里塞了 5 条新连接。你 `accept` 一次只取出 1 条，剩下的 4 条**永远不会再触发 listen fd 的 edge 通知**——因为"从无到有"已经发生过一次了。所以代码里是 `loop { accept4 ... if WouldBlock break }`，和 read 一样的模式。

## 深挖：write 也会 EAGAIN

我们简化版的 `handle_conn` 没处理 write 端的 EAGAIN——假设 echo 数据少，写缓冲不会满。但**真实服务器必须处理**：write 一次写不完，要缓存剩余字节，给 fd 注册 `EPOLLOUT`（可写）事件，等下次 epoll_wait 通知 fd 可写时续写。这一块留给练习。我们的简化版在 echo 数据量小（< 64KB）时工作正常，测试覆盖到 50KB 的大消息也能通过——因为写缓冲通常有 64KB-256KB。

## 这台机器就是 M9b Reactor 的内核

`bare_server` 是 M9b 自研异步运行时 Reactor 的**直接前驱**。M9b 的 Reactor 内部就是一棵 epoll 实例 + 一张 `fd → Waker` 的映射表。Future 被 await 时，它的 Waker 注册到 epoll；事件就绪时 Reactor 调用 Waker 唤醒对应的 Future。bare_server 把那层"Future + Waker"的抽象剥掉，让你看见裸 syscall——这一节学完，M9b 的 Reactor 就不再神秘。

## 与 mini-Redis 的对照

把 bare_server 和 mini-Redis 的 `serve` 摆在一起：

| mini-Redis | bare_server |
|-----------|-------------|
| thread-per-connection（每连接一线程） | 单线程 + epoll（一线程管所有连接） |
| `thread::spawn(move || handle_client(stream))` | `for ev in events { handle_conn(fd) }` |
| 阻塞 `read_line` | 非阻塞 `read` + edge-triggered |
| 1000 连接 = 1000 线程 = 8GB 栈 | 1000 连接 = 1 线程 = 几 MB |
| 简单直接，调试容易 | 复杂，但能扛 C10K |

两台机器**做的事情一样**（echo / RESP），但**并发模型完全不同**。mini-Redis 教你"并发决策"，bare_server 教你"一台机器怎么扛量"。完整的生产服务器是**两者的结合**：用 epoll 当 Reactor，每个就绪事件调度一个 Future（async fn），由多线程的执行器并行跑 Future——这就是 Tokio / async-std 的架构。

---

## 这三个子应用和主干的关系

把三个子应用摆在一起，你会看到它们其实**都是 mini-Redis 的切片**：

- **事件总线** = mini-Redis 的 `subs: HashMap<String, Vec<Sender>>` 单独拎出来，加了背压策略。
- **Actor** = mini-Redis 的 `Mutex<HashMap>` 状态从"加锁保护"换成"封进单线程 + 消息传递"。
- **epoll 服务器** = mini-Redis 的 `thread::spawn(move || handle_client)` 从"每连接一线程"换成"一线程管所有连接"。

它们揭示的是**三种独立的并发决策维度**：

1. **扇出语义**（广播 vs 单消费）——事件总线讲透。
2. **共享状态的形态**（锁 vs 消息传递）——Actor 讲透。
3. **I/O 模型**（thread-per-connection vs 多路复用）——epoll 服务器讲透。

一个真实的并发服务（数据库、消息队列、Web 框架）在每一个维度上都要做选择。学完这一节，你能在脑子里把任意一台真实服务拆回"它在每个维度选了什么、为什么"——这就是这一节给并发直觉补的最后一块拼图。

---

## L1–L5 缩放回顾

把整章缩成不同颗粒度，每退一级都更接近"为什么这些原语非有不可"：

| 层 | 你能…… |
|---|---|
| **L1（一句话）** | 真实并发服务 = "限速 + 去重 + 汇聚 + 状态共享 + I/O 模型"五个决策的组合；前九章每个原语都是其中一项决策的工具。 |
| **L2（类比）** | 爬虫是"按店面发牌子（Semaphore）+ 已访问登记本（Mutex<HashSet>）+ 传送带汇总（mpsc）"；mini-Redis 是"带锁的共享账本 + 大喇叭广播（pub/sub）"；Actor 是"每个员工一个信箱，谁要改状态就写信"；epoll 服务器是"一个前台同时盯所有对讲机"。 |
| **L3（跟踪）** | 能逐拍走：① 爬虫按域名发许可的时序；② 去重集 contains+insert 必须锁内原子；③ PUBLISH 扇出到 N 订阅者；④ epoll edge-triggered 读到 EAGAIN。 |
| **L4（解释决策）** | 说清每个并发决策"为什么"：为什么限速用 Semaphore 不用 Mutex、为什么去重必须锁内、为什么 pub/sub 不能用单消费者 mpsc、为什么 epoll 必须非阻塞 + 读到 EAGAIN。 |
| **L5（迁移）** | 看一台真实服务（Redis/Nginx/PostgreSQL），能在脑子里把它拆回"它在扇出/状态/I/O 三个维度各选了什么、为什么、代价是什么"——并能指出我们这版的简化在哪、真实版还多了什么（持久化、集群、协议协商）。 |

> **自检**：合上文档，你能——① 画出爬虫的 worker + DomainLimiter + visited + mpsc 的数据流图？② 解释 mini-Redis 的 PUBLISH 为什么必须 clone 出 sender 列表再锁外 send？③ 说清 Actor 模型怎么消灭数据竞争（状态封进单线程）？④ 解释 epoll 边沿触发下"没读到 EAGAIN 会怎样"？
>
> **动手清单**：① 给爬虫加一个"深度限制"（max-depth）；② 给 mini-Redis 加 EXPIRE（过期的 key 怎么主动清理？）；③ 把 Actor 的 Counter 换成有状态的"银行账户"（存/取/查余额，取款超额要拒绝）；④ 给 echo-server 加"最大连接数限制"（超过拒绝，练习 epoll 上 listener 的 ET 处理）。

---

## 结语

这一章没有引入任何新原语。它做的是另一件事：**把前九章的原语像积木一样拼起来，拼出两台能跑的机器**。当你能在脑子里把任意一台真实并发服务（数据库、消息队列、Web 框架）拆回"哪些原语、为什么是这些"，你就真的拥有了并发直觉。

三个补缺子应用则把 mini-Redis / crawler **没单独讲透的三种并发决策**各做一个最小实现——响应式扇出、消息传递代替共享状态、I/O 多路复用——让你看见同一台机器在每个维度上能做出不同的选择。

---

## 把这个跑起来：三个真实 bin

前面几千字一直在"纸面"上讨论并发决策。这一节做另一件事：**把上面这些机器真的跑起来**。`crates/forge-app/src/bin/` 下有三个可执行文件——`mini-redis`、`crawler`、（仅 Linux）`echo-server`。你跑通它们需要的东西只有两条：`cargo run` 和一台能联网的机器（爬虫用）。这一节不引入任何新原语，它讲的是"纸面模型怎么落到一条会回字节的 TCP 流上"。

读者最容易在"协议"和"代码"之间塌掉的环节，是从敲下 `redis-cli set k v` 到屏幕上出现 `OK` 的那一秒里，究竟发生了什么。我们用 mini-redis 这条 bin 把它逐拍拆开。

### mini-redis：用 redis-cli 和它对话时，字节在走什么路

启动它：

```text
cargo run -p forge-app --bin mini-redis
```

终端会打印一行 `mini-redis 监听 127.0.0.1:6379（thread-per-connection）`。这台机器就在那里等着了。你换一个终端敲 `redis-cli -p 6379 set k v`，屏幕回 `OK`。看似平淡，但这一拍里有四件事同时发生。

第一，redis-cli 把你说的 `set k v` 翻成 **RESP 帧**。RESP 是 Redis 的 wire protocol（线路协议），它不是按空格切命令，而是把每条命令编码成一个"数组"，数组里每个元素是一段定长字符串。`set k v` 翻出来是这样一串字节：

```text
*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n
```

`*3` 是"接下来有 3 个字段"；`\r\n` 是 RESP 的换行约定，永远带一个回车加一个换行，不能用单 `\n`。每个字段前面 `$N` 表示"这个字段有 N 个字节"，紧接着是 N 字节的实际内容，再接 `\r\n`。所以 `$3\r\nSET\r\n` 就是"3 字节的字符串 SET"。读者可以自己数：`*3` 这条数组有 3 个 `$` 块，分别是 `SET`、`k`、`v`。这一拍的全部意义就是把"人眼里的命令"翻译成"长度前缀 + 原始字节"——长度前缀让接收方不用扫字符串找空格，直接按字节数读，二进制安全（key 里能放空格、换行、任何字节）。

第二，redis-cli 这串字节通过 TCP socket 发到 6379 端口。mini-redis 的主循环卡在 `TcpListener::accept()` 上——操作系统在内核里维护着一条"已完成三次握手的连接队列"，accept 就是从这条队列里取一条出来。取到之后 mini-redis 立刻 `thread::spawn`——这就是"thread-per-connection"模型字面意义上的样子：每来一条连接起一个新线程。读者要问：为什么这么奢侈？答案是教学清晰度。一台机器扛 1000 连接起 1000 线程在生产里是反模式（每个线程栈 8MB，1000 个就 8GB），但**你能看见每条 TCP 流、每一次锁获取**——异步运行时把这些细节藏起来了，调试时你都不知道哪条连接在哪条 task 上。M11 会专门讲怎么把它搬到 epoll/async 上。

第三，新线程里跑 `serve_one`。它把这串字节交给 `read_command`——本模块前面讲过的 RESP 解析器。解析器读完 `*3`，知道要读 3 个 `$` 块；读 `$3\r\nSET\r\n`、`$1\r\nk\r\n`、`$1\r\nv\r\n`，最后得到 `Command::Set("k", "v")`。这一步是纯协议层，没有锁、没有共享状态。

第四，`handle_client` match 到 `Command::Set`，拿 `state.kv.lock()`——这是前面讲过的自研 `forge_sync::Mutex`——插进 HashMap，回 `+OK\r\n`。redis-cli 收到 `+` 开头就知道这是"简单字符串回复"，直接把 `OK` 打到屏幕。从你敲命令到看见 OK，整条路径上**只发生了一次锁获取**，锁的粒度是"整个 HashMap"——这是 mini-Redis 故意选的最粗粒度，因为它最容易讲清楚；真实 Redis 用哈希桶分片（每个桶一把锁）来减小粒度，是 M8 的练习题。

读者现在拿一条 GET 走一遍：

```text
redis-cli -p 6379 get k
```

发出去的字节是 `*2\r\n$3\r\nGET\r\n$1\r\nk\r\n`。服务端 match 到 `Command::Get`，锁里 `kv.get("k")`，拿到 `Some("v")`，回 `$1\r\nv\r\n`——这是一条 **bulk string 回复**，和 `+OK` 不一样：bulk string 用 `$N\r\n` 起头，因为它的内容可能含 `\r\n`，必须用长度前缀。`+OK` 这种简单字符串不能含换行，所以用 `+` 前缀 + 第一个 `\r\n` 就结束。redis-cli 看见 `$1` 知道是 bulk，读 1 字节得 `v`，再吃 `\r\n`，屏幕打 `v`。

如果你 `get 不存在的key`，服务端回 `$-1\r\n`——这是 **nil bulk**，长度为 -1 表示"这条字符串不存在"。redis-cli 把它翻成 `(nil)`。读者要意识到协议层的细节：服务端没办法回 `+OK` 表示"没有"，因为 `+OK` 是成功语义；也没办法回空 bulk `$0\r\n\r\n`，因为那表示"空字符串值"，和"key 不存在"是两回事。所以协议专门留了一个 sentinel `$-1` 来区分这两种"无"。这就是为什么 RESP 把"长度"做成有符号——负数留给特殊语义。

我们手算一帧完整往返，把上面四拍压实成一行字节流。客户端发：`*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n`（27 字节）。服务端回：`+OK\r\n`（5 字节）。没有更多东西。这一拍里没有线程切换以外的并发——一把锁、一次 HashMap 插入、一次 flush。**复杂度全部集中在协议解析**，不在并发决策。这就是 mini-Redis 设计的取舍：把并发决策做到最朴素（thread-per-connection + 全局一把锁），让协议层成为读者练习解析器的地方。

SUBSCRIBE 和 PUBLISH 让事情变得有趣。你 `redis-cli -p 6379 subscribe ch`，redis-cli 会**阻塞**收消息。这条连接发完 `*2\r\n$9\r\nSUBSCRIBE\r\n$2\r\nch\r\n` 后，服务端在 `state.subs` 表里把"这条连接的 mpsc::Sender"加进 `subs["ch"]` 列表，回 `+subscribed ch\r\n`。注意这里并发决策的关键：**KV 用一把锁，订阅表用另一把锁**——为什么分开？因为 GET/SET 是"短锁"（微秒级），SUBSCRIBE 注册是"罕见但可能慢"（要修改 Vec）。如果共用一把锁，一条 SET 就会阻塞所有正在注册的 SUBSCRIBE。这就是"按访问模式分锁"——粒度不是越细越好，是按场景分。

现在另一条连接 `publish ch hello`。服务端 match 到 `Command::Publish`，**先在锁内把 senders 列表 clone 到本地 vec，再在锁外逐个 send**。这一步是 mini-Redis 最值得讲清楚的并发决策：如果在锁内 send，一个慢订阅者（receiver 阻塞没在读）会让所有其他订阅者等他——PUBLISH 退化为串行。锁外 send 让所有 send 并发进行（虽然 send 本身不慢，但无界 mpsc 也会在 receiver 满时阻塞——这是教学版故意留下的练习：换成有界 + 丢弃策略）。`write_int(w, delivered)` 回 `:1\r\n`——整数回复，`:` 前缀。

读者最难懂的点：**为什么订阅者那条连接没有主动 drain 自己的 receiver？** 因为 `handle_client` 的主循环卡在 `read_command` 上等下一条命令，没法同时读 sub_rx。教学版的简化是"让 PUBLISH 把消息推到每个订阅者的 sub_tx，订阅者下次发命令时消息还在 receiver 里"——这违背真实 Redis 的语义（订阅者应该立刻收到 push），但讲清楚并发决策就够。M11 把它搬到 async 时会专门解决这个问题。

### crawler：命令行怎么落到并发决策上

```text
cargo run -p forge-app --bin crawler -- https://example.com --max 10 --concurrency 4
```

启动后你看到一行行的 `抓到 https://... (XXX 字节, 解析出新链接 N)`。这一拍里发生了什么？

crawler 的 main 把"跑爬虫"分成三步：解析命令行、选 fetcher、调 `run_with_fetcher`。第一步无聊，第三步是前面 `Crawler::run` 的入口。值得讲的是第二步：fetcher 是个 trait，bin 默认接 `UreqFetcher`（feature `real-fetch`），测试接 `MockFetcher`。**为什么 main 把 fetch 抽成 trait？** 因为我们要让"测试"和"生产"走完全相同的并发路径——只是把"真的发 HTTP"换成"查 HashMap"。如果 fetch 写死在生产代码里，测试要么连真服务器（慢、不稳定、CI 没网），要么把 fetch 函数指针绕来绕去污染主路径。trait 把"什么算一次抓取"定义成一个洞，洞后面接什么都行。

`--concurrency 4` 决定 worker 线程数，`--domain-limit`（默认 2）决定每个域名同时能开几条连接。这两个数解耦是有道理的：你可能想用 8 个 worker 加速整体爬取，但对单个域名只允许 2 条并发（避免压垮对方服务器）。worker 池是"全局并发"，DomainLimiter 是"按目标的礼貌性（politeness）"——它们是**两个独立的并发维度**。DomainLimiter 用 `Mutex + Condvar + 计数器` 实现（前面讲过为什么不用 `forge_lockfree::Semaphore`：在大量等待者场景下可能丢唤醒，而爬虫限速器的正确性比每秒多打几次 CAS 重要）。

`--max 10` 是停止条件——抓够 10 页就关待抓队列。但"够 10 页"的判定不是简单地数 collected 长度，因为 collected 长度还在涨时，workers 可能又往待抓队列里塞了新链接。前面讲过 `in_flight_results` 计数器的语义：入队 +1、worker 完成一条 -1。这个计数器归零 + collected < max 才是真正没活干。如果只判"队列空就停"，正有一个 worker 在 fetch 即将吐 10 条链接的页面时，主线程关队列就把那 10 条全丢了——这是 `Crawler::run` 最棘手的并发 bug，留给读者拿掉计数器再复现。

`extract_links` 做最朴素的 HTML 解析——找所有 `href="` 后面到下一个 `"` 的子串。它**故意不引入真正的 HTML 解析器**——那会让教程注意力跑到 HTML 文法上去。代价是它会错过单引号 `href='...'`、相对 URL `/path`（被过滤掉）、带 `#` 锚点的链接。够抓 example.com，够讲清楚爬虫的并发决策。生产代码用 `scraper` crate，但那是工程选择，不是教学焦点。

### echo-server：epoll 流程在 bin 里就一条 main

```text
cargo run -p forge-app --bin echo-server
nc 127.0.0.1 7878
```

`echo_server.rs` 整个 bin 就一个 main：调 `BareEchoServer::bind`、`server.serve(64, None)`。所有 epoll 流程前面已经讲透，bin 只负责把这台机器跑起来。值得讲的是为什么这个 bin 整体 `#[cfg(target_os = "linux")]`——epoll 是 Linux 专属 syscall，macOS 用 kqueue、Windows 用 IOCP，接口完全不同。非 Linux 平台这个 bin 只打印一条"没东西可跑"退出，避免 cargo 在 macOS 上拉 libc 编译失败。

`nc` 连上之后你打一个字，它立刻回显。这一拍里发生的是：listening socket 在 epoll 实例里被登记成 `EPOLLIN | EPOLLET`（edge-triggered）；你的连接进来，listening fd 状态从"无连接"变"有连接"，触发一次 edge 通知；服务端 `accept4` 取出新连接 fd，登记到 epoll；你打字，新 fd 上有数据，触发 EPOLLIN；服务端 read 直到 EAGAIN，把读到的写回。整个过程**一个线程**就能同时管几千条连接——这就是 C10K 的本质。

读者最难懂的点：为什么 edge-triggered 必须"读到 EAGAIN"。你打 5 个字节，服务端读到 5 个字节，但你打字时又来了 3 个字节——edge-triggered 不会再通知第二次（"从无到有"已经发生过）。如果你只读 5 个回去 wait，那 3 个字节就被饿死。所以必须循环 read 到 EAGAIN——把 fd 抽干。level-triggered 没这个问题（每次 wait 都重复通知），但代价是系统调用次数多。M9b 的 Reactor 选 edge-triggered + 抽干，就是这台机器的延伸。

### 三个 bin 的共同形状

把三台机器摆在一起，你会发现它们都是"参数 → 选实现 → 跑循环"。mini-redis：参数是端口，实现是 thread-per-connection 循环；crawler：参数是 seed/max/concurrency，实现是 worker 池循环；echo-server：参数是端口，实现是 epoll 循环。**bin 的职责是把这些参数翻译成 lib 的入口**，所有并发决策留在 lib。这就是为什么 main 都很短——它们只是"启动器"，复杂度在 lib 里。

测试侧的做法也一致：mini-redis 的集成测试起一个后台 server 线程、用裸 TcpStream 写 RESP 字节、断言每个字段；crawler 的集成测试用 MockFetcher 注入 `run_with_fetcher`、断言抓到的 URL 列表。两种测法各自对应一种"可注入性"——mini-redis 抽不出 fetcher，但抽得出端口（bind 0 让操作系统随机分配）；crawler 抽得出 fetcher，所以连端口都不用起。这种"哪里能切一刀让测试注入"的设计，本身就是把并发原语做成可测的工程实践。


下一章（M11）我们换一个视角：**怎么测、怎么调试并发代码**。本章你看到了一个教学版 Semaphore 在压力下偶发死锁——M11 会教你用 loom、strace、tsan 把这种 bug 揪出来。bare_server 这一节我们直接调了 libc 的 epoll syscall——M11 还会教你用 `strace -e epoll_wait` 观察一个真实服务器的 syscall 序列，把"事件循环"在系统调用层可视化。
