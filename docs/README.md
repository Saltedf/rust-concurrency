# 《Forge》—— 从第一性原理，手把手构建一个真实的 Rust 并发运行时

> 一套**做中学**的中文分步教程。你将从零写出一个真实、可运行、长得像
> `crossbeam` + `parking_lot` + `tokio` 的并发工具链与运行时。
> 学完它，你不再需要任何参考书就能理解 Rust 并发的每一个底层细节。

---

## 为什么有这套教程

市面上的 Rust 并发教程，几乎都掉进两个坑：

1. **玩具实现，学完无用**——给你一个 30 行的 `SpinLock` 例子，讲完就和真实开源代码脱节了。
2. **零散罗列**——东讲一个原子操作、西讲一个通道，没有一个把它们串起来的真实系统。

《Forge》换个做法：**我们就去造那个真实的系统**。这个系统叫 **Forge**，是一组可以发布的 crate。
它的每一个组件，**就是** Mara Bos《Rust Atomics and Locks》教的那些原语本身——不是孤立的演示，而是被一个真实运行时**消费**的零件：

```
Channel  → JoinHandle 的结果投递、爬虫的结果汇聚
Arc      → 每个异步任务共享的 Arc<Shared<Task>>
原子     → 任务 ID、停止位、运行时指标
futex 锁 → 调度器的就绪队列锁
Chase-Lev 工作窃取双端队列 → 多核调度器
SeqLock  → Reactor 的就绪快照
Semaphore → 爬虫的按域名限速
```

Mara Bos 全书 1–10 章的每一个概念，都被**深度应用**到 Forge 的某个真实组件里。最终你会拥有**两个顶峰**——一个同步工作窃取线程池（`rayon` 的脊梁）和一个异步执行器 + Reactor（`tokio` 的雏形）——以及两个跑在它们之上的真实应用：并发网页爬虫与 mini-Redis。

## 这套教程的"自包含"承诺

> **你只需要会基础 Rust**（所有权、借用、trait、闭包、泛型）。**不需要**任何并发基础，**不需要**翻任何参考书。
> 教程里每出现一个概念，都在**教程内**把它讲透：它为了对付什么"敌人"而被发明 → 输入输出是什么 → 一个能成像的心智模型 → 自底向上的推导 → 可跑的代码 → 常见陷阱 → 与其它问题的同构关系。
> `books/` 目录下的四本原书，是**编写者**的素材，不是**你**的前置条件。每个模块结尾有"深入阅读（可选）"，想钻得更深再去翻。

## 教学方法

遵循 `teaching-method.md`（《Dual Cognition 双向认知框架》）和第一性原理：

- **先有"敌人"再讲武器**：每个原语都先说清它解决了哪种让人抓狂的痛苦。
- **I/O 锚点先行**：先回答"这东西能干什么、喂它什么、它吐出什么"，再谈机制。
- **低保真心智模型**：先用一个不精确但能成像的类比建立直觉，再深入。
- **自底向上拼图**：把陌生概念拆成你已经掌握的小组块（"一个 `for` 循环 + 一个累加器"）。
- **识别同构**：反复点明——停止位 = 延迟初始化位 = 信号量 = 通道就绪位，本质都是 `AtomicBool` + 一个 `Ordering`。
- **L1–L5 缩放**：每个主题结束时，给出"一句话描述 / 类比 / 跟踪一个例子 / 解释内部设计 / 推导与边界"五个层级的自检。

## 依赖链（自底向上的 DAG）

```
forge-core       原子 · 自旋锁 · Arc          (M1, M3, M4)
   ↑
forge-sync       std 锁 · atomic-wait · futex 锁   (M2, M6, M7)
   ↑
forge-channel    oneshot · mpsc                (M5)
forge-lockfree   信号量 · RCU · 无锁栈/队列 · MCS · parking-lot · SeqLock · Chase-Lev   (M8)
   ↑
forge-pool       同步工作窃取线程池（顶峰#1）   (M9a)
   ↑
forge-rt         异步执行器 + mio Reactor（顶峰#2）   (M9b)
   ↑
forge-app        并发网页爬虫 · mini-Redis      (M10)
```

## 课程地图

| 模块 | 主题 | 产出 | 状态 |
|---|---|---|---|
| **M1** | 原子与内存序 | `forge-core::atomics` | ✅ |
| **M2** | 共享与加锁（std） | 调度器种子：worker 等工作循环 | 🔨 |
| **M3** | 自建 SpinLock + CPU 真相 | `forge-core::spin` | 🔨 |
| **M4** | 自建 Arc / Weak | `forge-core::arc` | 🔨 |
| **M5** | 自建 Channel | `forge-channel` | 🔨 |
| **M6** | 内核底座 atomic-wait（futex） | `forge-sync::atomic_wait` | 🔨 |
| **M7** | 自建 futex 真锁 | `forge-sync::{mutex,condvar,rwlock}` | 🔨 |
| **M8** | 全部无锁结构（第10章从零） | `forge-lockfree` | 🔨 |
| **M9a** | 🏔 **顶峰#1** 同步工作窃取线程池 | `forge-pool` | 🔨 |
| **M9b** | 🏔 **顶峰#2** 异步执行器 + Reactor | `forge-rt` | 🔨 |
| **M10** | 真实应用：爬虫 + mini-Redis | `forge-app` | 🔨 |
| **M11** | 并发测试：loom + miri + criterion + stress | 贯穿全程 | 🔨 |

每个模块是一篇长文 `docs/modules/Mx-*.md`，按 codecrafters 风格拆成 5–30 个**小步**，每一步：写代码 → 跑测试看**红** → 补全 → 变**绿**。

## 如何跟做

```bash
# 1. 每一步都有一个对应的测试文件，例如 M1.6 对应
cargo test -p forge-core --test m1_06_relaxed_bug
# 一开始它是红的（你还没写对），按教程写代码，再跑，变绿即过关。

# 2. 还没学到的步骤用 #[ignore] 标记，去掉 ignore 表示"开始这一步"。
#    跑所有已达绿的：
cargo test --workspace
#    只跑当前正在做的（被 ignore 的）：
cargo test --workspace -- --ignored
```

### 四个验证工具（M11 会逐一介绍，但从 M1 起就用）

- **`cargo test`**：每一步的功能正确性。
- **loom**（nightly）：并发模型检查器，让"只在弱内存 ARM 上才暴露的 bug"在 x86 上也能复现。
  ```bash
  LOOM_MAX_PREEMPTIONS=3 cargo +nightly test --test 'loom_*'
  ```
- **miri**（nightly）：检查 `unsafe` 代码里的未定义行为。
  ```bash
  MIRIFLAGS="-Zmiri-preemption-rate=0.01" cargo +nightly miri test -p forge-core
  ```
- **criterion**：性能基准，量化"自研 vs 真实 crate"的差距。
  ```bash
  cargo bench
  ```
- **`scripts/stress.sh`**：高并发压力测试，把概率性 bug 逼近必现。

## 工具链

本课程需要 **nightly**（loom 与 miri 都依赖它）。仓库根目录 `rust-toolchain.toml` 已固定为 nightly 并带上 `miri`、`rust-src` 组件，首次 `cargo` 会自动准备。

## 目录结构

```
rust-concurrency/
├── Cargo.toml                 # workspace
├── rust-toolchain.toml        # nightly + miri
├── teaching-method.md         # 教学方法规范
├── books/                     # 四本原书（解包后的纯文本，编写者对照用）
├── docs/
│   ├── README.md              # 本文件
│   ├── map-book-to-module.md  # 素材→模块追溯（编写者用）
│   └── modules/               # 长篇自包含中文分步教程
├── crates/                    # 7 个 crate（见上方依赖链）
└── scripts/stress.sh          # 压力测试
```

---

准备好就从 [M1 原子与内存序](./modules/M1-atomics-and-ordering.md) 开始。每一步都先回答：**这东西是为了对付什么敌人，才被发明出来的？**
