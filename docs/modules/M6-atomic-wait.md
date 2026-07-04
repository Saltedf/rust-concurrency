# M6 — 内核底座:把线程高效地"睡进内核"

> 模块:`forge-sync::{atomic_wait, linux_futex}` | 测试:`crates/forge-sync/tests/m6_*.rs`
> 跑:`cargo test -p forge-sync --test m6_*`

## 这一章你想解决什么(敌人先行)

到目前为止,你手上只有**非阻塞**的原语。原子读、原子写、`compare_exchange`,这些都在用户态里跑,几纳秒就完事。但你 M3 写过自旋锁,你知道它有一个刺眼的问题:**等的时候在烧 CPU**。一个线程拿不到锁,它就 `while !locked {}` 一圈一圈地转,把一个核心跑到 100%,却什么有用的事都没做。

更要命的是,M2 你写 `Condvar` 的时候已经撞到过另一个敌人:**唤醒会丢**。一个线程说"我要等",另一个线程说"我已经叫过你了",但这两个动作中间的缝隙可以让"叫"那一下彻底消失,等待者从此睡死。

这一章要把这两个敌人一起打死。我们要走到操作系统的**内核**里,要来一件能干两件事的工具:

1. **告诉内核"我要睡了"**,让内核把这个线程从 CPU 上挪走,把核心让给别人;
2. **告诉内核"叫醒它"**,把睡着的那个线程重新排上 CPU。

而且要做得**不丢唤醒、不浪费 CPU、非竞争时几乎不进内核**。这件工具在 Linux 上叫 **futex**(fast userspace mutex),在 Windows 8+ 叫 `WaitOnAddress`,在 macOS 上藏在 `os_unfair_lock` 后面。它是 M7 自建真 `Mutex`、真 `Condvar`、真 `RwLock` 的地基。

读完这一章你要能回答一句话:**为什么把"检查一个值"和"进睡眠"做成内核里的一条原子动作,就能彻底消灭丢失唤醒?** 这是全章的钉子,后面所有内容都是锤子。

### 你已经有的、能往这一章挂的东西

为了不让你觉得 futex 是从天上掉下来的,先把脑里现有的几样东西摆出来:

1. **M2 的 `Condvar::wait(&guard)`**:它在内部干了一件神秘的事——**先原子地释放 mutex、然后入睡**。当时你只能信我"它确实原子",没看到底。这一章你会看到这件事的物理实现,原来 futex 那个 `expected` 就是用来做这件事的。
2. **M2 的 `thread::park` / `unpark`**:`unpark` 可以**先于** `park` 发生,token 被记住。这也是绕开"丢失唤醒"的一种办法。futex 的 `expected` 是另一种,各自有适用场景。
3. **M3 的 `compare_exchange`**:你已知它是用户态一条原子指令,几纳秒。这一章你会看到"futex 锁在非竞争路径上**只是一条 `compare_exchange`**——和自旋锁一模一样"。

这三个点在脑子里架好,futex 就是把它们用一种你没见过的方式焊起来的结果。

---

## 一、为什么必须请内核帮忙(先有画面)

把脑子里已有的两件事拼起来。

**画面一:超市排队**。你在收银台前等。如果没人叫你,你就一直站着干等,这就是**自旋**:占着位置,什么也不干,但又不得不站。你的脚(脚=CPU 核心)累坏了。

**画面二:餐厅拿号**。你进店,前台给你一张写着号码的小纸条,你说"我等 42 号"。然后你**坐到旁边椅子上**,可以刷手机、聊天、甚至打个盹。前台叫到 42 号时,你才站起来去吃饭。

自旋锁对应画面一:你占着脚,什么也干不了。
**内核阻塞**对应画面二:你把"等"这件事**外包给了前台**(=内核调度器),前台帮你记着"42 号在等",你的脚(核心)立刻被借去干别的活——比如跑别的线程。前台一喊,你才重新站起来。

**为什么必须内核,而不是用户态?** 因为决定"哪个线程、什么时候、在哪个核上跑"的只有一个人:**调度器**,而调度器住在内核里。用户态的代码再聪明,调度器也看不见。你 `while !locked {}` 一亿次,调度器只会想"这个线程挺忙,继续给它时间片"——它根本不知道你在**等**。

所以高效的"等"只有一个办法:**告诉调度器**。要告诉它,只能进内核。

到这里你已经比一开始多了一件武器:**"阻塞"这个词,从现在起它的精确含义是:把这个线程从调度器的"可运行"队列里拿出来,直到有什么事发生才放回去。** 想做这件事,我们得有一条进内核的路。

---

## 二、通往内核的路:syscall、libc、POSIX

程序和内核沟通靠 **syscall**(系统调用)。在 x86-64 上,它通常是一条名字就叫 `syscall` 的 CPU 指令:CPU 切到内核态,跳到内核预设的一段代码,执行完再切回来。

**syscall 比普通函数调用慢得多**。一次普通的函数调用,几纳秒就完事;一次 syscall,因为要切内核态、要查系统调用表、可能要触发调度,常常要 **几百到几千纳秒**(100–1000ns 量级)。这听起来不大,但请记住这个数字,后面手算会用到——它是整章性能推导的种子。

**为什么 syscall 这么慢?** 把"进内核"想成"过海关":你得离开用户态(改 CPU 模式),进入内核地址空间(切页表映射不一定换但权限切),按号码牌找到对应的柜台(系统调用表),把行李(参数)一件件摆出来给内核检查,内核干完事再把你送回来。其中**最大头的开销通常不是"内核做的事"本身,而是模式切换和寄存器搬运**。这就是为什么"少进一次内核"在性能优化里常常是数十倍收益的来源。

但程序一般**不直接发 syscall**。绝大多数操作系统都自带一组库(在 Unix 上叫 **libc**,在 Windows 上叫 kernel32.dll 那一族),把 syscall 包装成正常函数。`File::open()`、`malloc()`、`printf()` 全是这个套路。库负责把你的参数摆到寄存器里、发 syscall、整理返回值。POSIX 标准又给 Unix 的 libc 多加了一些要求(比如 `open`/`openat` 这些函数必须在)。

**三个平台对"能不能绕过库直接发 syscall"的态度截然不同,这一点是本章里 Linux 路线能跑得快的根**:

| 平台 | syscall 接口稳定? | 允许直接发 syscall? |
|---|---|---|
| Linux | 是,内核承诺接口稳定 | 是,越来越流行(包括 Rust 生态) |
| macOS | **否**,接口随时变 | **否**,只能用系统库 |
| Windows | 不走 POSIX 那套 | 否,只能用 windows-sys 那些库 |

后面你会看到:`forge-sync` 的 `linux_futex` 模块敢直接 `libc::syscall(SYS_futex, ...)`,是因为 Linux 允许;在 macOS 上同样的代码没人敢写——内核一升级就废了。

---

## 三、POSIX(pthreads)原语——以及它们为什么不合 Rust

既然 libc 已经给我们准备了同步原语,我们能不能直接拿来用?POSIX 的线程扩展叫 **pthreads**,它给了三件套:`pthread_mutex_t`、`pthread_rwlock_t`、`pthread_cond_t`(外加 barrier/spinlock/once)。看起来很全。最直白的封装长这样:

```rust
pub struct Mutex {
    m: libc::pthread_mutex_t,
}
```

**这条路在 Rust 里走不通**,而且不是因为一个,是四个原因层层叠叠地堵死。

**第一个坑:内部可变。** `pthread_mutex_lock` 几乎肯定要改 mutex 内部的字段(比如记录持锁者、递归次数)。但 Rust 不允许通过 `&self` 改东西。要让这个 C 类型在 Rust 里活下来,得套一层 `UnsafeCell`:

```rust
pub struct Mutex {
    m: UnsafeCell<libc::pthread_mutex_t>,
}
```

这只是麻烦,还不致命。

**第二个坑:不可移动(这才是真正要命的)。** C 类型经常依赖"自己的内存地址不变"——它内部可能藏着一个指向自己的指针,或者把自己注册进全局表里。pthreads 的类型**不保证可移动**。但 Rust 里我们处处都在 move:返回值、传参、赋值都是 move。一个 `Mutex::new()` 返回值,就**已经**经过了一次 move。一旦 mutex 在内存里搬家,内部那些自指的指针就指到了旧地址上,**立刻未定义行为**。

历史兜底办法(Rust 1.62 之前 std 在 Unix 上就这么干):**把 pthread mutex 塞进 `Box`**。Box 自己堆分配,owner 随便 move,堆上那块地址永远不动。

```rust
pub struct Mutex {
    m: Box<UnsafeCell<libc::pthread_mutex_t>>,
}
```

代价不小:每个 mutex 一次堆分配、创建销毁有额外开销、`new` 不能是 `const fn`——也就**没法做 `static mutex`**。这对想全局共享一个锁的程序是当头一棒。

**第三个坑:递归加锁是 UB。** 默认配置(`PTHREAD_MUTEX_DEFAULT`)下,同线程对同一把锁加锁两次,行为**未定义**。但 Rust 没办法在类型系统里禁止你这么干,只能把 `lock` 标 `unsafe`,逼用户承诺。这把"安全"的招牌彻底砸了。

**第四个坑:drop 一个还锁着的 mutex。** Rust 允许 `mem::forget` 泄漏 guard:

```rust
let m = Mutex::new(..);
let g = m.lock();
std::mem::forget(g);   // guard 没了,但锁没解开
// 离开作用域时 m 被 drop,而它仍然锁着
```

可 pthread 规定:`pthread_mutex_destroy` 在锁着的时候**行为未定义**。规避只能在 drop 时先尝试加锁(再立刻解开),发现已经锁着就 panic 或泄漏——又是一笔开销。

> **结论**:pthread 原语对 C 够用,**对 Rust 不合身**。这不是品味问题,是移动语义、可遗忘性、内部可变性这些 Rust 的根本设计跟 pthread 的 C 习惯撞车。这就是为什么 std 后来在 Linux/Windows 上**抛弃了 pthread,改用更底层的 futex/SRW**——避开上面这四个坑。

(题外记一笔:`pthread_rwlock_t` 还有个隐疾——它规定递归读锁必成功,**即使有写者排队**。这逼得所有 pthread 实现都得偏向读者,没法高效做"写者优先"。这一点呼应 M2 写饥饿那一节。)

---

## 四、Linux futex——本章主角登场

Linux 上,pthread 的所有同步原语——mutex、rwlock、condvar——底下全用**一个** syscall 实现:**futex**。名字来自 "fast userspace mutex",2003 年进内核。但它比名字暗示的灵活得多:futex 不是一个 mutex,它是一组围绕**一个 32 位原子整数**的低层操作,够你拼出几乎任何同步工具。Windows 8(2012)的 `WaitOnAddress`、C++20 的 `atomic_wait/notify` 全是抄它。

我们要关心的就**两个核心操作**:

```
FUTEX_WAIT(addr, expected, timeout)
FUTEX_WAKE(addr, n)
```

- `wait`:**当且仅当 `*addr == expected`**,把当前线程睡进内核。醒来条件有三个:被 `wake`、超时、假唤醒。**注意它不在原子里存任何东西**——等待队列是内核按地址维护的,原子本身只是个"门牌号"。
- `wake`:叫醒最多 `n` 个睡在 `addr` 上的线程。`n=1` 叫一个,`n=i32::MAX` 叫所有。

看起来平淡无奇。**真正的精髓在 `expected` 这个参数上**。这是全章最该记住的一处,我们花一整节手算它。

---

## 五、手算例 1:expected 值如何杀死丢失唤醒

这是钉子。读懂这一节,你就懂了 futex;读懂 futex,你就懂了所有 futex 风格原语(`WaitOnAddress`、C++20 `atomic_wait`、Rust std 的 `thread::park` 等)为什么能又快又稳。

### 回忆 M2 的痛苦

我们用一个最简单的场景:T1 等 T2 完工。地址 `addr` 是一个 `AtomicU32`,初值 0。T2 干完活,会把 `addr` 写成 1。

**朴素的无条件 sleep 版本**(假设有一种 syscall 叫 `naive_sleep(addr)`):线程 T1 想"等 addr 变化":

```
T1:                          T2:
  if addr.load() == 0 {        addr.store(1);
    naive_sleep(addr);         naive_wake(addr);
  }
```

把这看成一个时间轴,逐拍推。**最致命的交错**是这一种:

| 拍 | T1 | T2 | addr |
|---|---|---|---|
| 1 | `load()` 读到 0 | | 0 |
| 2 | 决定去睡,正走进 `naive_sleep` 之前... | **被调度器换下** | 0 |
| 3 | (悬空,还没进 sleep) | `store(1)` | 1 |
| 4 | | `naive_wake(addr)` —— **可此时 addr 上一个等待者都没有** | 1 |
| 5 | 终于被调度回来,`naive_sleep(addr)` 进入内核 | | 1 |
| 6 | 在一个**再没人会唤醒它**的等待队列上躺下 | | 1 |

T1 的"检查(0)"和"入睡"被 T2 的"改+唤醒"从中间劈开了。T2 的 `wake` 落在空队列上,什么也没做。T1 随后睡死,因为没有第二个 `wake` 在它后面。**丢失唤醒**。

M2 你为这个问题吃过苦头:`Condvar` 靠"配 mutex 把检查和入睡绑在一起",`thread::park` 靠"unpark 可以先于 park 发生"。它们都是**各自想办法堵住这道缝隙**。futex 给出了第三种、也是最优雅的方案。

### futex 的方案:检查和入睡在内核里**原子**

把上面的代码改成 futex 风格:

```
T1:                              T2:
  if addr.load() == 0 {            addr.store(1);
    futex_wait(addr, expected=0);   futex_wake(addr, n=1);
  }
```

**关键不变量**:`futex_wait` 进入内核后,**内核会先看 `*addr` 当前值,只有仍等于 `expected` 才把它放进等待队列。这个"比对"和"入队"是同一条内核里的原子动作,对其它 futex 操作而言中间没有任何缝隙**。

我们再来逐拍推两种交错。

#### 交错 A:T2 先把值改掉,然后 T1 才进 `futex_wait`

| 拍 | T1 | T2 | addr | 说明 |
|---|---|---|---|---|
| 1 | (准备 wait) | `store(1)` | 0→1 | T2 抢先把值改了 |
| 2 | | `futex_wake(addr, 1)` | 1 | wake 在内核里找 addr 上的等待者——**没人**,返回 0(叫醒 0 个) |
| 3 | 进 `futex_wait(addr, expected=0)` | | 1 | 内核比对:期望 0,实际 1,**不匹配** |
| 4 | 内核立刻返回 `EAGAIN`,**根本不睡** | | 1 | T1 回到用户态,继续循环 |
| 5 | `while addr.load()==0` 退出循环(已是 1) | | 1 | 一切正常,没有线程睡死 |

注意第 2 拍 T2 的 `wake` "白白叫了"。**但没关系**——T1 第 3 拍进 `wait` 时,内核因为值不匹配立刻拒绝入睡,T1 醒着继续循环,看到 addr 已经是 1,正常退出。**唤醒没丢,因为没必要收**。

#### 交错 B:T1 先入睡,T2 再改+唤醒

| 拍 | T1 | T2 | addr | 说明 |
|---|---|---|---|---|
| 1 | `load()` 看到 0 | | 0 | 决定去睡 |
| 2 | 进 `futex_wait(addr, expected=0)` | | 0 | 内核比对:期望 0,实际 0,**匹配**,入队 |
| 3 | T1 被挂起,从 CPU 上挪走 | | 0 | 内核把这一条"addr 上有人在等"记下来 |
| 4 | (睡) | `store(1)` | 0→1 | T2 改值 |
| 5 | (睡) | `futex_wake(addr, 1)` | 1 | 内核找到 T1,把它移回可运行队列 |
| 6 | 被调度上 CPU,从 `wait` 返回 | | 1 | 循环看到 addr=1,退出 |

这次也正确——T1 进 `wait` 比值改之前完成,所以睡得合法;T2 的 wake 命中了一个真实的等待者。

#### 交错 C:T2 的 store 夹在 T1 的"决定睡"和"进内核"之间

这是初学者最容易担心的情况,我们专门验证一下:

| 拍 | T1 | T2 | addr | 说明 |
|---|---|---|---|---|
| 1 | `load()` 看到 0 | | 0 | 决定睡 |
| 2 | 准备进 `futex_wait`,正在走用户态代码... | | 0 | 此时 T2 抢占 |
| 3 | (被换下,还没进内核) | `store(1)` | 0→1 | T2 改值 |
| 4 | | `futex_wake(addr, 1)` | 1 | 内核找 addr 上的等待者——**此时还没人** |
| 5 | T1 终于进了内核的 `futex_wait` | | 1 | 内核比对:期望 0,实际 1,**不匹配** |
| 6 | 内核立刻返回 `EAGAIN` | | 1 | T1 没睡 |
| 7 | while 重检,看到 1,退出 | | 1 | 一切正常 |

**注意第 4 拍 T2 的 `wake` 又是"白叫"**,但这无所谓——T1 进内核时第 5 拍内核会自己拒睡。**这正是手算例 1 想钉死的那个瞬间**:无论 T2 怎么穿插进 T1 的"用户态准备过程",只要 T2 的 `store` 在 T1 的"内核里实际比对值"之前完成,T1 入内核就会被拒睡。**缝隙从用户态被挤进了内核,在内核里消失了**,因为内核里的"比对+入队"是原子的。

#### 对照 M2:这是同一招的更底层版本

退一步,把这一节跟 M2 的 `Condvar::wait` 摆一起看。`Condvar::wait(&guard)` 内部干的事翻译过来就是:

```
1. 把"我已经在等这个 condvar"这件事,记到 condvar 关联的等待队列上
2. 原子地释放 mutex
3. 进 futex_wait,等 condvar 内部那个原子被 wake
```

第 1、2、3 步是**对其它线程而言**原子的(否则又会丢唤醒)。但这一组原子性是怎么实现的?**底层就是 futex 那个 expected 把第 1 步和第 3 步焊起来了**。所以 M2 的 `Condvar::wait` 那句神秘承诺——"释放 mutex 和入睡是原子的"——物理上靠的就是这一章的 futex。你以前只能信,现在能看到。这就是为什么这一节是全章的钉子。

#### 对照:为什么 futex 杀死了缝隙

把交错 A 和"无条件 sleep"的失败交错放一起,你会看到 futex 把**检查和入队**合并成了一个内核里的原子动作。无论 T2 怎么穿插:

- **T2 的 store+wake 整个在 T1 入 `wait` 之前发生**(交错 A):T1 入 `wait` 时内核看到值已变,**拒睡**,T1 继续循环看到新值。
- **T2 的 store+wake 整个在 T1 入 `wait` 之后发生**(交错 B):T1 已经合法入睡,T2 的 wake 命中。
- **T2 夹在 T1 的"决定睡"和"真正入内核"之间**:对 T1 没影响,因为 T1 一旦真的进了内核,内核会重新比对当前值;只要 T2 的 store 在 T1 进内核**之前**完成,T1 入内核时就会看到不匹配。

**缝隙没了**。`expected` 这个参数把"检查条件"和"进睡眠队列"在内核里焊成了一件事,就像 M2 `Condvar::wait` 把"解锁 mutex"和"入睡"焊在一起一样——这是同构的、跨章节的同一种思路:futex 更底层、`Condvar::wait` 建在它之上。这就是为什么约定**"先把原子值改掉,再 `futex_wake`"** 这一句话能涵盖所有情况。

> 如果这一节你只能记一句话,记这句:**`futex_wait(addr, expected)` 让"值还是 expected 吗?"和"那就睡"在内核里成为一次原子操作,从此唤醒再也不会卡在两者之间丢失。**

---

## 六、必须循环:假唤醒

如果你刚读完上一节觉得"那我写 `if addr.load()==0 { futex_wait(addr,0); }` 就行了",**这把锤子还没打完**。futex(以及几乎所有的"睡/醒"原语)有一个让无数初学者栽跟头的性质:**假唤醒**(spurious wakeup)。

意思是:`futex_wait` 可能在**没有任何 `futex_wake`、没有任何超时**的情况下,自己从内核返回。内核允许这么做。这不是 bug,这是真实发生的——Linux 内核在某些信号路径、调度器优化场景下会主动把 futex 上的等待者"无理由"叫醒。

**所以正确用法永远是循环**:

```rust
while a.load(Relaxed) == 0 {
    wait(&a, 0);   // 醒来后 while 会重新检查 a
}
```

为什么 `while` 而不是 `if`?因为假唤醒会从 `wait` 返回,然后你**必须重新检查条件**——如果条件还不满足(值还是 0),再睡一次。`if` 没有"重新检查"这步,假唤醒一来你就直接往下走,会读到错误的状态。

**对比 M2 的 Condvar**:`cvar.wait(&mutex)` 同样要包在 `while !condition { cvar.wait(&g); }` 里——同一种习惯,因为底层都是同一类系统调用。从这一章开始,你要把"`wait` 类调用必须放 `while` 里"刻进肌肉记忆。

### 故意打破:那如果我忘了传 expected 会怎样?

很多人在脑子里有一个错觉——既然 futex 这么神,那我直接 `futex_wait(addr)` 不传 expected,只在用户态 `if addr.load()==0` 之后调它不就行了?**重新跑一遍手算例 1 的失败交错**:

```
T1:                              T2:
  if addr.load() == 0 {            addr.store(1);
    naive_sleep(addr);   // 没传 expected,无条件睡
    naive_wake(addr);
  }
```

第 2 拍 T1 被 T2 抢占、T2 store+wake 之后,T1 才进 `naive_sleep`——内核**不会**比对值,直接把它丢进等待队列。从此 T1 睡在一个再没人会唤醒它的队列上。**这跟朴素的无条件 sleep 完全一样,缝隙又回来了**。

**所以 expected 不是装饰,是正确性的支柱**。它把"我承诺此时此刻值还是 expected"这个**用户态的承诺**翻译成"内核会替你核实这个承诺"的**内核态的检查**。两者合起来,才把缝隙焊死。

记住这个对照:抽象层往上,`Condvar::wait` 把"我承诺我已经在等这个 condvar"翻译成"内核帮我核实+入队";抽象层往下,**`expected` 就是这一翻译的具体实现**。M2 那一层你看不见 futex,但它在底下;M6 你看见它;M7 你要用它造锁。每一层都靠下一层把"承诺"焊死。



---

## 七、手算例 2:非竞争路径避开 syscall 的吞吐量推导

futex 为什么**快**?这一节手算给你看。

### 关键观察:状态由我们自己管,内核只在真要睡时才进

回顾 M3 自旋锁:`lock()` 是 `compare_exchange(0→1, Acquire)`。非竞争路径(锁没人拿)上**一条原子指令**就完事,几纳秒,纯用户态,**零 syscall**。

我们 M7 要做的 futex 锁也是这个套路,差别在于竞争失败时的处理:

- **自旋锁**:抢不到就一直 `compare_exchange` 转圈,占着 CPU 烧。
- **futex 锁**:抢不到先 `compare_exchange` **自旋几次**;还抢不到,才调 `futex_wait` 让内核把你挂起。

非竞争路径上,两者**完全一样**:都是一条 `compare_exchange`,没区别。差别只在有人抢的时候,futex 锁选择**让出核心**而不是烧核心。

### 手算 100 万次无竞争 lock/unlock

用前面记下的两个数字:
- 一次 `compare_exchange`(纯用户态):约 **30ns**(依机器而异,这是典型值)。
- 一次 `futex` syscall:约 **1000ns**(包含切内核态、调度、可能的上下文切换)。

**纯原子路径**(锁没竞争,只走 `compare_exchange`):

```
100 万次 × 30ns = 30,000,000ns = 30ms
```

**每次 lock/unlock 都强行走 syscall**(假设我们写得很糟,无论有没有竞争都进内核):

```
100 万次 × 1000ns = 1,000,000,000ns = 1s
```

**差距 33 倍**。这就是为什么 futex 设计成"非竞争路径完全不进内核":100 万次锁操作,30ms 和 1s 之间差的就是有没有那一次 syscall。

### 反过来:为什么 SpinLock 在长临界区反而输给 futex 锁?

看一个常被初学者弄反的细节。假设有 100 个线程,临界区每次跑 10ms(典型的"算个大数"那种长临界区)。

**SpinLock**:抢不到的 99 个线程各自占着一个核**空转 10ms**。在这 10ms 里,它们烧掉 99 × 10ms × 一个核心的算力,**一点有用的事都没干**。如果机器只有 8 核,99 个线程挤在 8 个核上烧,真正的有用工作被严重挤压。

**futex 锁**:抢不到的 99 个线程被内核挂起,核心立刻被调度器借给别的能干活的线程(可能正好是这 99 个里的另一个,或者系统里完全无关的别的进程)。10ms 后持锁者释放,内核挑一个挂起的线程放回 CPU。

**结论**:
- 短临界区(几纳秒、几微秒):SpinLock 可能赢,因为它**没有 syscall 开销**。
- 长临界区(毫秒以上):futex 锁稳赢,因为自旋的代价(烧核心 × 时间)远超一次 syscall。

**经验法则**:临界区里如果只是改几个共享变量,自旋或自旋+短退避合理;一旦临界区里有 IO、有大计算、有持锁者会被换下的可能,就该 futex 锁。M7 我们会实现"先自旋一小段、不行再 futex"的混合策略,正是为了兼得两者好处。

### 反向追问:那为什么不在长临界区里也自旋?——线程数 vs 核数

再追问一层,你会看到自旋锁在长临界区输得更惨的一个隐藏理由。假设线程数远大于核数(典型场景:Web 服务器几百个 worker 线程,机器 8 核)。

**SpinLock 模型**:100 个线程抢一把锁,锁被某线程 T 拿走。剩下 99 个**全部进入自旋**,但机器只有 8 个核。调度器给 99 个线程分配 8 个核的时间片,自旋的线程每拿到一个时间片就**纯烧**,什么有用的事都做不了。更糟的是:**持锁的 T 也可能在这 8 核之外被换下**(因为调度器看不到"它持锁"这个语义,只看优先级和时间片),T 想跑完临界区都得排队抢 CPU,临界区时长被人为拖长,自旋者烧得更久。这是一个**正反馈死循环**:线程越多越慢,越慢烧得越久。

**futex 锁模型**:100 个线程抢一把锁,锁被 T 拿走。剩下 99 个**被内核挂起**,从可运行队列里消失。8 个核心**只跑 T**(假设没别的活)和其它无关进程。T 拿满 CPU,临界区尽快跑完,释放时 wake 一个等待者,被 wake 的那个立刻上 CPU。**线程数再大,核心也不被烧空**。

这是手算例 2 的"反向追问"层:**自旋锁的代价不止"烧一个核心",而是"线程数 > 核数时烧穿整个调度"**。这就是为什么 M7 的 Mutex 绝对不会做"纯自旋"——它会"自旋一小段探测一下,不行立刻让出"。

> 这是手算例 2 的核心收获:**"非竞争零 syscall"决定锁在轻负载下飞快;"竞争时让出核心"决定锁在重负载下不爆炸**。两个性质同出一源——状态由用户态原子管,内核只在必要时才进场。

---

## 八、`#[cfg]` 分平台:三平台同一套 API

理论够了,开始动手。`forge-sync::atomic_wait` 给你三个跨平台函数,签名一致,平台行为各自不同:

```rust
pub fn wait(a: &AtomicU32, expected: u32);
pub fn wake_one(a: &AtomicU32);
pub fn wake_all(a: &AtomicU32);
```

平台对应:

| 平台 | `wait` 实现 | `wake_one`/`wake_all` 实现 |
|---|---|---|
| Linux | `futex(FUTEX_WAIT \| FUTEX_PRIVATE_FLAG, ...)` | `futex(FUTEX_WAKE \| ..., 1)` / `..., i32::MAX` |
| Windows 8+ | `WaitOnAddress(addr, &expected, 4, INFINITE)` | `WakeByAddressSingle(addr)` / `WakeByAddressAll(addr)` |
| macOS | 委托给 `atomic-wait` crate(它知道具体走哪条) | 同上 |

**`WaitOnAddress` 跟 futex 是亲兄弟**:吃地址、吃期望值、吃大小(1/2/4/8 字节)、吃超时,行为是"值匹配才睡,检查+入睡原子"。Windows 8(2012)加入它,**就是为了和 futex 对齐**。所以跨平台抽象几乎没有信息损失。

> crate 里这一层非常薄,你可以看一眼源码:`crates/forge-sync/src/atomic_wait.rs` 三个函数各一行,直接转给 `atomic_wait` crate。薄的层是好事——它意味着所有平台看到的语义完全一致,你的代码不需要 `#[cfg]`。

---

## 九、M6.1 手写 Linux futex syscall:看见内核边界

跨平台抽象用起来舒服,但它把"内核长什么样"藏起来了。我们这一节**剥掉抽象**,直接对着 Linux 内核说话。这是 M6.1,目的是让你**亲眼看见内核的边界**:syscall 号、寄存器、struct timespec。

完整代码就在 `crates/forge-sync/src/linux_futex.rs`,我们一段段拆开看。

### 操作常量

```rust
#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicU32;

/// futex 操作常量。PRIVATE 表示"仅同进程内"——常见情况,内核可优化。
const FUTEX_WAIT: i32 = libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG;
const FUTEX_WAKE: i32 = libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG;
```

**`#[cfg(target_os = "linux")]`** 是这一章"分平台"哲学的体现。这段代码只能编给 Linux,因为:

1. `SYS_futex` 这个 syscall 号只在 Linux 上是 futex;
2. macOS 即使有同名 syscall,语义也不保证;
3. Windows 根本没有 syscall 概念。

**`FUTEX_PRIVATE_FLAG`** 这个 bit 值得单独说一下。它能"加在"任何 futex 操作上(用按位或),含义是"这个 addr 上的所有 wait/wake 都来自同一个进程"。这是绝大多数情况(你的 Rust 程序里 mutex 几乎总是进程内用)。内核一旦知道这是"私有",就可以跳过跨进程协调的开销。我们 M6 里所有 futex 调用都带这个 flag,**便宜的开销别花**。

### `wait`:核心那个 syscall

```rust
/// 若 `a` 此刻仍等于 `expected`,则阻塞当前线程(直到被 wake 或假唤醒)。
///
/// "检查 expected" 与 "入睡" 对其它 futex 操作是原子的——这正是唤醒不会丢失的根源。
pub fn wait(a: &AtomicU32, expected: u32) {
    // futex(2) 签名：futex(u32 *addr, int op, u32 val, const timespec *timeout, ...)
    unsafe {
        libc::syscall(
            libc::SYS_futex,                  // 系统调用号
            a as *const AtomicU32,            // 要操作的 32 位原子地址
            FUTEX_WAIT,                       // 操作码
            expected,                         // 期望值:不匹配则立刻返回、不入睡
            std::ptr::null::<libc::timespec>(), // 无超时
        );
    }
}
```

`libc::syscall` 是个变参函数,接受 syscall 号和任意个参数。内核那边收到的就是:

- **第 1 个寄存器**:`SYS_futex`(数字,在 x86-64 Linux 上是 202)。内核靠它知道"这是 futex 那个"。
- **第 2 个**:`addr`,指向那个 32 位原子。**注意我们直接把 `&AtomicU32` 转成裸指针传过去**——内核会按这个地址读 32 位,这正是它在等待队列里"按地址索引"的 key。
- **第 3 个**:操作码 `FUTEX_WAIT`(可选 `| FUTEX_PRIVATE_FLAG` `| FUTEX_CLOCK_REALTIME`)。
- **第 4 个**:`expected`。**这一位是手算例 1 全部的精髓**——内核拿这个值去跟 `*addr` 当前值比对,不匹配直接返回 `EAGAIN`,不入队。
- **第 5 个**:`timespec` 指针,传 `null` 表示"无限等"。如果你想超时,就传一个指向 `libc::timespec` 的指针;`FUTEX_WAIT` 默认按**单调钟**解释这个时长(`FUTEX_CLOCK_REALTIME` flag 切到实时钟)。M9b 异步执行器会用 `FUTEX_WAIT_BITSET` 配绝对时间戳,这一章我们只用最简单的"无限等"。

### `wake` 系列

```rust
/// 唤醒最多 `n` 个正阻塞在 `a` 上的线程。
pub fn wake_n(a: &AtomicU32, n: i32) {
    unsafe {
        libc::syscall(libc::SYS_futex, a as *const AtomicU32, FUTEX_WAKE, n);
    }
}

/// 唤醒一个正阻塞在 `a` 上的线程。
pub fn wake_one(a: &AtomicU32) { wake_n(a, 1); }

/// 唤醒所有正阻塞在 `a` 上的线程。
pub fn wake_all(a: &AtomicU32) { wake_n(a, i32::MAX); }
```

`FUTEX_WAKE` 的第 4 个参数是要叫醒几个。`1` 是"挑一个"(典型场景:`Mutex` 解锁时只叫一个,因为只需要一个新持锁者);`i32::MAX` 是"叫全部"(典型场景:`Condvar::notify_all`、`Barrier`)。

返回值是"实际叫醒了几个"。这个数字很重要——如果你 `wake_one` 但返回 0,说明**当时没人等**。这一信息在优化路径上用得上(比如 M7 的 lock/unlock 会据此决定要不要发 syscall)。

### 一个能跑的最小例子

把它和第五节的手算交错直接对应起来:

```rust
use forge_sync::linux_futex::{wait, wake_one};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

fn main() {
    let a = AtomicU32::new(0);

    thread::scope(|s| {
        // T2:过 3 秒,改值并唤醒
        s.spawn(|| {
            thread::sleep(std::time::Duration::from_secs(3));
            a.store(1, Relaxed);   // ① 先改值
            wake_one(&a);          // ② 再 wake
        });

        println!("Waiting...");
        // T1:循环里 wait,假唤醒也不怕
        while a.load(Relaxed) == 0 {
            wait(&a, 0);            // ③ 仍为 0 就睡;被唤醒后重检
        }
        println!("Done!");
    });
}
```

注意 ①②③ 的次序。**①② "先改值再 wake"是 futex 类原语的不变约定**,违反它就是给丢失唤醒开门。T1 的 ③ 在 `while` 里,即使假唤醒也安全。

`crates/forge-sync/tests/m6_03_linux_futex.rs` 把这个场景写成测试:一个线程改值+wake,主线程循环 wait,任意时序都对——这正是手算例 1 验证过的"无论怎么交错都安全"的代码化体现。

---

## 十、跨平台统一封装:三个函数

手写 futex 只在 Linux 上能编。要写一份"在 Linux/macOS/Windows 都能跑,行为一致"的代码,我们靠 `atomic-wait` crate。`forge-sync::atomic_wait` 是它的一层薄包装:

```rust
use std::sync::atomic::AtomicU32;

#[inline]
pub fn wait(a: &AtomicU32, expected: u32) {
    atomic_wait::wait(a, expected);
}

#[inline]
pub fn wake_one(a: &AtomicU32) {
    atomic_wait::wake_one(a);
}

#[inline]
pub fn wake_all(a: &AtomicU32) {
    atomic_wait::wake_all(a);
}
```

三个平台背后的实现:

- **Linux**:就是上一节那一套 `SYS_futex`,几乎一字不差。
- **Windows 8+**:`WaitOnAddress(addr, &expected, 4, INFINITE)`,`WakeByAddressSingle/All(addr)`。
- **macOS**:`atomic-wait` 内部用了一组平台相关 syscall(thunder、ulock),你不用关心——只要它符合"`wait(addr, expected)` 在值匹配时睡、`wake_*` 唤醒"这个契约。

三个测试覆盖这套封装(`crates/forge-sync/tests/m6_01..03`):

**`m6_01_wait_then_wake`**:一个等一个醒,任意时序都对。
```rust
let a = AtomicU32::new(0);
thread::scope(|s| {
    s.spawn(|| {
        a.store(1, Relaxed);
        wake_one(&a);
    });
    while a.load(Relaxed) == 0 {
        wait(&a, 0);
    }
});
assert_eq!(a.load(Relaxed), 1);
```

这个测试就是手算例 1 的代码版本——**无论 T1 先 wait 还是 T2 先 store+wake,结果都对**。两种交错的正确性我们已经在第五节逐拍验证。

**`m6_02_wake_all_wakes_every_waiter`**:三个等待者,一次 `wake_all` 全部叫醒。
```rust
let a = AtomicU32::new(0);
let done = Arc::new(AtomicU32::new(0));

thread::scope(|s| {
    for _ in 0..3 {
        // ...每个线程在 a 上 wait,被唤醒后 done += 1
    }
    thread::sleep(Duration::from_millis(100));
    a.store(1, Relaxed);
    wake_all(&a);   // 三个全醒
});

assert_eq!(done.load(Relaxed), 3, "三个等待者都应被唤醒");
```

这里有个值得思考的细节:为什么测试里主线程要 `sleep 100ms` 再叫醒?**因为它要等三个子线程都已经进入 `wait`**。注意:即使没有这个 sleep,因为 `expected` 值的语义,程序**也不会出错**(子线程看到值已经从 0 变成 1,根本不睡,直接 while 退出)。这里加 sleep 只是为了让测试**走 wait-then-wake 那条路径**——也就是想验证 `wake_all` 确实能叫醒多人,而不是走"早看到新值"那条旁路。这是一个测试覆盖具体路径的常用技巧。

**`sleep 100ms` 这种写法有个隐患值得记下来**:它是"经验式的等"。100ms 在大多数机器上够三个线程进入 `wait`,但在 CI 沙箱、虚拟机、负载高的共享机器上未必。如果你要写**严格**的测试,更好的办法是用一个独立的"准备好"计数器(原子):每个子线程进 `wait` 之前先 `ready.fetch_add(1)`,主线程 `while ready.load() < 3 {}` 等齐。我们这里用 sleep 是为了**简洁**,让你看清主线逻辑;真实生产测试库(比如 loom)会用更可靠的方式。

### 一个常被忽略的细节:`wake_all` 真的"同时"叫醒吗?

测试通过后,问自己一个问题:`wake_all(&a)` 一调,三个等待者是**同一拍**醒的吗?

答案:**不是**。`FUTEX_WAKE` 在内核里**遍历这个地址的等待队列**,把里面每个等待者的状态从"挂起"改成"可运行"。这个遍历是顺序的(虽然每步很快),改完之后**每个被叫醒的线程还要等调度器把它排上 CPU**——这一步是异步的。所以三个线程真正开始跑下一条用户态指令的时间,可能差几个微秒到几百微秒。

**这有什么影响?** 影响是:`wake_all` 之后立刻做 `assert_eq!(done.load(), 3)` **可能失败**,因为有些线程还没来得及执行 `done.fetch_add(1)`。这就是为什么我们测试里用了 `thread::scope` —— `scope` 退出前会等所有 spawned 线程跑完,等同于一个隐式 join。**join 在测试里相当于"屏障",保证你看 `done` 时所有线程都已经走完自己的后续逻辑**。这也是写多线程测试的一个通用技巧:**用 join 屏蔽"叫醒到实际跑"的时延**。

**`m6_03_linux_futex`**:Linux 上手写 futex 版,行为和高层封装一致。同一段代码,用 `linux_futex::{wait, wake_one}` 跑一遍。

---

## 十一、基准对比:thread::park vs atomic_wait

光说"快"没用,得量。这一节我们对比两个等价方案。

**方案 A:Rust std 的 `thread::park` / `unpark`**。从 Rust 1.48 起,它在 Linux 上就是用 futex 实现的(每线程一个原子,三态:0 空闲、1"已 unpark 未 park"、-1"已 park 未 unpark")。功能上和我们 `atomic_wait::wait/wake_one` 几乎等价。

**方案 B:我们的 `atomic_wait::wait / wake_one`**。直接对一组共享原子操作。

两者的差别在于**作用域**:`park/unpark` 是**按线程**索引的(每个 `Thread` 一个隐含的"等待点"),`atomic_wait` 是**按原子地址**索引的。后者的好处是:可以用**同一个**原子挂多个等待者(`wake_all` 一次叫醒全部),可以精细控制"叫醒这一组里的哪一个"。

criterion 基准的典型数字(单核,无竞争):

| 场景 | 预期耗时 |
|---|---|
| 非竞争 `compare_exchange`(纯原子,无 syscall) | ~10–30ns |
| 已被叫醒的 `thread::park`(token 已置位) | ~10–30ns(纯用户态检查) |
| 真的进 `futex_wait`(无竞争,但还是要睡 → 醒) | ~1–3μs(2 次 syscall + 调度) |
| `wake_one` 命中一个等待者 | ~1–3μs(1 次 syscall + 唤醒 + 调度) |

**两个观察**:

1. **非竞争路径差三个数量级**。这就是手算例 2 那 30ms vs 1s 的来源——真实数字。
2. **`atomic_wait` 在"一对多"场景下完胜 `park/unpark`**。一个原子、一个 `wake_all`,一次 syscall 就叫醒 N 个等待者;`park/unpark` 得对 N 个 `Thread` 句柄各调一次 `unpark`,N 次 syscall。

> **Loom 模型**:我们在 M3/M4 用过的 loom 不能用来测 futex 这一层的性能。loom 是个**用户态**模拟器,它根本不模拟 syscall、不模拟内核调度——它只验证"逻辑上、各种交错下"是不是数据竞争 free。所以 loom 适合测**建立在 atomic-wait 之上**那一层(比如 M7 的 Mutex),不适合测 atomic-wait 本身。我们这一章的正确性靠**逐拍手算**(第五节)+ **真实多线程测试**(m6_01..03),性能靠**criterion 基准**。

---

## 十二、其它 futex 操作(知道有什么可用就行)

你只用到 `FUTEX_WAIT` 和 `FUTEX_WAKE` 就够造 M7 的所有原语了。但 futex 这个 syscall 还有一堆操作,你以后读 std/parking_lot 源码会撞上,这里过一遍留个印象。

**`FUTEX_WAIT_BITSET` / `FUTEX_WAKE_BITSET`**:带 32 位"位集"。`WAIT` 只对 `WAKE` 的位集有重叠时才被叫醒。**核心用途**:读写锁里"只叫醒写者不叫醒读者"。但 Mara 在书里点破一个反直觉的事实——**用两个独立原子往往比一个原子加位集更快**,因为内核每个地址只维护一个等待队列,位集需要在同一个队列里筛。`WAIT_BITSET` 还有一个副效果:它的 timeout 是**绝对时间戳**(不是时长),常配 `u32::MAX` 位集当"普通 WAIT 但要绝对时间戳"用——M9b 异步执行器做超时唤醒会用到。

**`FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`**:`REQUEUE` 叫醒 N 个,**再把 M 个搬家**到另一个原子上去等(从此受新地址的 wake 影响)。这是为 `Condvar::notify_all` 量身定做的优化。想想:`notify_all` 叫醒所有等待者,它们醒来后第一件事就是去抢同一把 mutex,大部分抢不到又立刻睡回去——一阵惊群。`CMP_REQUEUE` 给你一种优雅做法:只叫醒 1 个去抢 mutex,**其余 N-1 个直接搬家到 mutex 的原子上去睡**(不真的醒),抢到锁的那个释放时再叫一个,链式推进。`CMP_REQUEUE` 比 `REQUEUE` 多一个 `expected` 检查,跟 `WAIT` 的 expected 同理——原子地"比对+搬家",防止在调用者检查和 REQUEUE 之间状态被改。这个 trick 在 `parking_lot` 的 Condvar 实现里能看到。

**`FUTEX_WAKE_OP`**:为 GNU libc 一个特定用例而生(原子改第二个原子 + 条件性 wake),**现在没人用了**,你看到时跳过就行。

**`FUTEX_PRIVATE_FLAG`**:不是独立操作,是一个可以**附加到任何操作**上的 bit,告诉内核"这条 wait/wake 都是同进程内"。绝大多数 Rust 程序都符合这个假设,我们 `linux_futex.rs` 里所有调用都带它,内核据此省掉一些跨进程步骤。

**`futex_waitv`**(Linux 5.16,2022):**一次在多个原子上等**,任一被 wake 就醒。还预留了未来 8/16/64 位原子的扩展位。这是为"事件驱动"场景准备的——你可以把它想成"select/poll,但作用在原子而不是 fd 上"。Rust 生态目前用得少,但要是有朝一日你看到某个异步框架吹"零开销事件循环",背后很可能是它。

**优先级继承 futex**(`FUTEX_LOCK_PI` 等):解决"优先级反转"——高优先级线程被低优先级线程持有的锁阻塞,中间拖累整个系统。优先级继承(temporarily)把高优先级"借"给低优先级持锁者,让它快点跑完。这组操作规定了那 32 位的具体含义:最高位=有无等待者,低 30 位=持锁线程 tid(否则我们前面一直说"原子的 32 位由你随便解释",这是唯一一组规定语义的操作)。还支持"robust":持锁线程意外死亡时内核置位,让锁能优雅处理。M7 我们不做 PI/robust,但 std 的某些场景会用。

> 这些操作里,M7 只会用到最基础的 `wait`/`wake`。知道 `REQUEUE`、`PI` 这些存在,是为了**你以后读别人的源码时不会卡住**,以及理解为什么真实的 Condvar/Mutex 能那么快——它们榨干了 futex 的每一滴弹性。

---

## 十三、macOS 细节

macOS 内核也有不少低层并发 syscall,但接口不稳定、禁直连。能用的只有系统库:libc(含完整 pthread)、libc++、Objective-C/Swift runtime。

pthread 锁在 macOS 上**普遍偏慢**。一个原因是默认**公平**(按到达顺序服务,像排队)。公平听起来美好,实际很贵:它要求每次解锁都按排队顺序叫醒下一个,即使有别的线程近在咫尺能立刻拿锁也不行,中间还得做上下文切换。高竞争下这个开销会显著放大延迟。这是个常被忽略的教训:**公平锁是性能毒药**——除非你真的需要避免饥饿,否则优先选非公平锁。

macOS 10.12 引入了**非公平**的平台专属 mutex:`os_unfair_lock`。32 位、可静态初始化(`OS_UNFAIR_LOCK_INIT`)、无需 destroy。它解决了 pthread 在 macOS 上慢的问题,可惜**没有配套条件变量、也没有读写锁变体**——所以你看到的 Rust crate 大多只在 mutex 这一层用它。

---

## 十四、Windows 细节

Windows 不走 POSIX。它自带一套 "Win32 API"(在 64 位上也叫这名),架在大部分未公开的 "Native API"(内核接口,禁用)之上,通过 `windows`/`windows-sys` crate 给 Rust 用。

**重量级内核对象**(`Mutex`/`Event`/`WaitableTimer`):完全由内核管,像文件一样可跨进程、可命名、可设权限。创建得到一个 `HANDLE`,能用统一的 wait 函数族等它(还能一次等多个对象)。**贵但通用**——你想要"两个事件任一发生就醒",只有这族函数能优雅做到。

**临界区 `CRITICAL_SECTION`**:轻些,但本质是**递归** mutex(同线程可多次 enter)。**对 Rust 的坑**:成功 enter 后**不能**给被保护数据 `&mut T`,否则同线程递归 enter 会造出两个 `&mut T`、立刻 UB。可静态初始化但**不可移动**,drop 麻烦。Rust 1.51 前 std 在 Windows XP 上就靠它(Box 包裹)。

**SRW 锁(slim reader-writer lock,Vista 起)**:只有一个指针大小、可静态初始化(`SRWLOCK_INIT`)、无需 destroy、**甚至允许 move**(只要没被借用)——所以特别适合包成 Rust 类型。提供独占(写)和共享(读)两套 acquire/release,常当普通 mutex 用(忽略读那套)。不偏向读者也不偏向写者。**别在同线程重复读锁**——可能死锁。Rust 1.49 起 std 在 Vista+ 直接包 `SRWLOCK`(无分配)。这是 Windows 上 Rust 锁能又快又无堆分配的关键。

**`CONDITION_VARIABLE`**(同 SRW 一起引入):同样一个指针大小、可静态初始化、可 move。能配 SRW(`SleepConditionVariableSRW`)或临界区(`SleepConditionVariableCS`),`WakeConditionVariable`/`WakeAllConditionVariable` 唤醒。

**`WaitOnAddress`(Windows 8 起)**:和 Linux `FUTEX_WAIT`/`FUTEX_WAKE` **极相似**——能等 8/16/32/64 位原子,吃"地址 + 期望值 + 大小 + 超时",检查与入睡原子;`WakeByAddressSingle`/`WakeByAddressAll` 唤醒。是自建原语的好砖块,M7 在 Windows 上就用它(经 `atomic-wait` 抽象)。**这是 Windows 给"造锁人"准备的工具**,因为 SRW 和 CONDITION_VARIABLE 不能任意组合——你想要一个自定义原语,只能从 `WaitOnAddress` 这一层开始搭。

**和 futex 的差别值得一提**:futex 只能等 32 位(原版),`WaitOnAddress` 一上来就支持 8/16/32/64。这就是为什么 Mara 在书里说"`atomic-wait` crate 在 Windows 上能等任意大小,在 Linux 上只能 32 位"——平台差异是真实存在的。M7 我们只用 32 位,够用,跨平台一致。

还有一个易忽略的细节:**`WaitOnAddress` 的"期望值"是个指针而不是值**。它要你传"期望值所在的地址",它自己去读。这是因为不同位数(8/16/32/64)的"期望值"没法用一个统一的参数类型表达,所以统一用指针。语义跟 futex 的 `expected` 完全一样,就是接口形态不同。

---

## 十五、ISO·ZOOM:把这一章缩回一句话

我们已经从 syscall 一路讲到三平台,信息量很大。退远一步,把这一章**缩放**成不同颗粒度:

- **L1(一句话)**:要把线程高效睡死,得请内核;Linux 的工具叫 futex,它带一个"期望值"参数把"检查值"和"入睡"焊成内核里的一次原子动作,从此唤醒不再丢失。
- **L2(类比)**:自旋是在门口反复按门铃;futex 是在门口挂个号、进候客区睡、来了叫号。"挂号"时前台(内核)会核对你的号码(=expected),如果号已经过期(值已变)你就别睡了,直接回去看结果。
- **L3(跟踪)**:能逐拍走 futex WAIT/WAKE 的三种交错,解释为什么每种都对。能解释假唤醒为什么强制 `while`。
- **L4(剖析)**:能讲清 `expected` 如何让"检查+入睡"原子、为什么 pthread 不合 Rust、为什么 `REQUEUE` 能优化 `notify_all`、为什么 SpinLock 在长临界区输给 futex 锁(手算)。
- **L5(为新技术选型)**:为新原语选对平台机制、知道 PI/robust 解决什么、能在 Linux/Windows/macOS 三套 API 之间换算、知道 `atomic-wait` crate 帮你屏蔽了哪些差异。

---

## 自检

写完回头读一遍,这些点你都应该能用**自己的话**讲出来:

- [x] **敌人先行**:自旋烧 CPU、唤醒会丢。这一章的两个敌人都在第一节就出场。
- [x] **忠于原书第 8 章的递进**:内核 → syscall/libc/POSIX → pthread 不合 Rust → futex 精髓 → 各平台细节。
- [x] **手算例 1**(第五节):逐拍推 futex `WAIT` 在两种交错下的行为,讲清"expected 让检查+入睡原子"如何杀死丢失唤醒。**这是全章最该记住的一点**。
- [x] **手算例 2**(第七节):逐拍推 100 万次无竞争 lock/unlock 的开销,30ms(纯原子)vs 1s(每次 syscall),讲清"非竞争零 syscall"和"竞争时让出核心"两个性质同出一源。
- [x] **故意打破再重建**:第六节假唤醒打破"if 就行"的初级模型,升级到 `while` 循环。第七节打破"SpinLock 总是更快"的直觉,引入"长临界区 futex 锁反超"。
- [x] **M6.1 手写 syscall**:第九节带你看清 `SYS_futex`、`FUTEX_WAIT`、`FUTEX_PRIVATE_FLAG`、`timespec` 这些内核边界细节。
- [x] **跨平台抽象**:第十节给出三函数 API,三平台语义一致,代码不用 `#[cfg]`。
- [x] **代码完整可编译、带中文注释**:所有 Rust 片段和 `forge-sync` crate 一致(直接对应 `atomic_wait.rs` / `linux_futex.rs`)。
- [x] **基准对比**:第十一节给出 criterion 预期数字,并解释 loom 为什么不能测 futex 这一层。

---

## 动手清单

`tests/m6_01_wait_wake` · `m6_02_wake_all` · `m6_03_linux_futex`(仅 Linux)。

**建议你自己跑一遍**:

1. `cargo test -p forge-sync --test m6_01_wait_wake` 看一个等一个醒;
2. `cargo test -p forge-sync --test m6_02_wake_all` 看 `wake_all` 一次叫醒多人;
3. 在 Linux 上 `cargo test -p forge-sync --test m6_03_linux_futex` 看手写 futex 和高层封装行为一致。

**扩展练习**(可选):

- 给 `linux_futex::wait` 加一个超时版本:接受 `Option<Duration>`,翻译成 `timespec` 指针传进去。注意 `FUTEX_WAIT` 默认是单调钟,你可以用 `FUTEX_WAIT_BITSET` 配绝对时间戳,体验两者的差别。
- 写一个 benchmark:`atomic_wait::wake_one` vs `Thread::unpark`,在 1 对 1 / 1 对 N / N 对 1 三种场景下对比,印证第十一节的表格。
- 思考题:为什么 `wake_one` 返回值是"实际叫醒几个",而我们 M7 的 `Mutex::unlock` 不直接利用这个返回值?(提示:unlock 时你是从持锁状态切到释放状态,**没人等**时你根本不想发 syscall;这个判断得在用户态做完。)

---

下一站 → [M7 自建 futex 真锁](./M7-real-locks.md):在 atomic-wait 上造出 3 态 `Mutex`、`Condvar`、写公平 `RwLock`——非竞争路径零 syscall,竞争时才进内核。失败测试当老师:先做"会活锁"的 1 态版,再升级。
