# Summary

[引言](./README.md)

# 基石原语

- [M1 — 原子、内存序，以及"谁能跨线程"](./modules/M1-atomics-and-ordering.md)
- [M2 — 共享与加锁：标准库原语](./modules/M2-sharing-and-locking.md)

# 自建原语

- [M3 — 自建 SpinLock，以及 CPU 的真相](./modules/M3-spinlock.md)
- [M4 — 自建 Arc / Weak](./modules/M4-arc.md)
- [M5 — 自建 Channel：把错误从"运行时 UB"挪到"编译期"](./modules/M5-channels.md)
- [M6 — 内核底座：把线程高效地"睡进内核"](./modules/M6-atomic-wait.md)
- [M7 — 自建 futex 真锁：Mutex、Condvar、写公平 RwLock](./modules/M7-real-locks.md)

# 无锁结构

- [M8 — 全部无锁结构（第 10 章从零）+ Latch/Barrier/Cancel](./modules/M8-lockfree.md)

# 两个顶峰

- [M9a — 同步工作窃取线程池 + 并行算法](./modules/M9a-thread-pool.md)
- [M9b — 异步执行器 + mio Reactor + 协程/组合子](./modules/M9b-async-runtime.md)

# 真实应用与测试

- [M10 — 爬虫 + mini-Redis + 事件总线/Actor/零依赖服务器](./modules/M10-applications.md)
- [M11 — 并发测试 + 异步测试（loom / miri / criterion / stress）](./modules/M11-testing.md)

# 附录

- [素材 → 模块 追溯表](./map-book-to-module.md)
