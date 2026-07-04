# 素材 → 模块 追溯表（编写者用）

> 这张表是**给编写者（我）**看的对照清单，确保四本书 + 网络资料的每一个主题都被**深度应用**到某个模块里。
> **读者无需看这张表**——教程成品自包含，不依赖任何原书。

| 书 / 章节 | 落点模块 | 如何"深"（非浅尝） |
|---|---|---|
| Mara Bos Ch1（线程/scoped/Arc/Send-Sync/内部可变/Mutex/RwLock/park/Condvar） | M2 全 + 调度器种子 | Condvar/park 构建真实"worker 等工作"循环 |
| Mara Bos Ch2（原子：load/store/fetch_*/CAS/lazy init） | M1.1–1.5 + 后续各模块复用 | 每个原语追溯到某个 fetch_*/CAS |
| Mara Bos Ch3（内存序：happens-before/Relaxed/Release-Acquire/SeqCst/Fence） | M1.6–1.9（主）+ M3.4/M5.8/M8g 各以新 bug 形态重讲 | 排序随每个原语重教，非一次性 |
| Mara Bos Ch4（自建 SpinLock） | M3 全 + M3.7 接入运行时 | Guard + 排序 + 基准，真实热路径 |
| Mara Bos Ch5（自建 Channel 六版本） | M5 全 → M9 JoinHandle / M10 结果通道 | Channel 即运行时结果投递机制 |
| Mara Bos Ch6（自建 Arc） | M4 全 → M9 `Arc<Shared<Task>>` | Arc 即任务句柄 |
| Mara Bos Ch7（处理器：缓存/MESI/false sharing/x86 TSO vs ARM） | M3.5/3.6 + M1.10 + M8g | 每个概念配一个可测基准 |
| Mara Bos Ch8（OS 原语：futex/os_unfair_lock/WaitOnAddress） | M6 全 + M7 底座 | 产出 M7 用的安全 atomic-wait |
| Mara Bos Ch9（自建 futex 锁：3态Mutex/Condvar/写公平RwLock） | M7 全，换进运行时 | 三者全建全基准 |
| Mara Bos Ch10（信号量/RCU/无锁链表/MCS/parking-lot/SeqLock） | **M8 全部从零**（含 RCU epoch 回收、hazard ptr、MCS/CLH、parking-lot） | 用户选"全部从零"，无仅演示项 |
| C++ Concurrency Ch6/7（锁基/无锁数据结构、ABA、hazard ptr） | M9a（池=锁基结构）/ M8c（ABA+hazard ptr） | |
| C++ Concurrency Ch9（线程池/可等待任务/避死锁/本地队列/工作窃取） | M9a 全 | 每个概念映射到一步 |
| C++ Concurrency Ch11（测试调试并发代码） | M11 贯穿全程 | loom + miri + criterion + stress 从第一天起 |
| Async Rust（O'Reilly）全书（Future/Pin/Context/Waker/async队列/任务窃取/mio reactor） | M9b 全 | 异步运行时从零 |
| The Linux Programming Interface（futex / epoll 章） | M6 / M9b 底层对照 | 手写 syscall |

## 网络/论文参考（"深入阅读可选"页脚会引用，正文已自洽）

- Tokio "Async in depth"：<https://tokio.rs/tokio/tutorial/async>
- Rust async book "Build an Executor"：<https://rust-lang.github.io/async-book/02_execution/04_executor.html>
- crossbeam-deque 文档：<https://docs.rs/crossbeam/latest/crossbeam/deque/index.html>
- Chase & Lev 原论文《Dynamic Circular Work-Stealing Deque》：<https://www.dre.vanderbilt.edu/~schmidt/PDF/work-stealing-dequeue.pdf>
- Le, Ngyen 等《Correct and Efficient Work-Stealing for Weak Memory Models》：<https://inria.hal.science/hal-00802885/document>
- parking_lot crate：<https://docs.rs/parking_lot>
