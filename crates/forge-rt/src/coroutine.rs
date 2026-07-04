//! # 协程 / 生成器 —— 用 Future + 状态机手写一个"能暂停、能吐值、能恢复"的函数。
//!
//! Rust 稳定版没有 `yield` 关键字（nightly 才有 `Coroutine` trait）。但
//! `Future` 本身就是一个"能暂停（`Poll::Pending`）、能恢复（再 poll）、能结束
//! （`Poll::Ready(v)`）"的东西——它和"协程"在结构上是一回事，只是 Future 的
//! "输出"只有**一个**（结束时吐的那一个值），而生成器想吐**多个**值。
//!
//! 这一章我们手写一个 `Gen<T>`：内部一个 `Future`，每次 poll 把要吐的值塞进
//! 一个 `Option<T>` 槽位、返回 `Pending`（= 暂停）；调用方 `resume()` 从槽位里
//! 把值取走，再 poll 一次推进状态机。当 future 终于返回 `Ready(fin)` 时，生成
//! 器进入终结态，之后任何 `resume()` 都返回 `None`（像 `Iterator`）。
//!
//! 教程 M9b 第十五章逐拍画过状态转换：`gen { yield 1; yield 2; }` 编译成
//! `state = 0 / 1 / Done` 三态；每次 `resume()` 推进一拍。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::noop_waker;

/// 生成器吐出的"半个值"——要么是"中间产物"（`Yielded(v)`，还能再 resume），
/// 要么是"终结"（`Complete(fin)`，再 resume 返回 `None`）。
///
/// 这个名字直接对标 nightly 的 `std::ops::CoroutineState`，让读者将来无缝过渡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenState<Y, R> {
    /// 暂停时吐出的中间值。生成器内部状态已经"卡在"下一个 yield 点之前。
    Yielded(Y),
    /// 生成器跑完了，带最终返回值。之后再 `resume()` 会得到 `None`。
    Complete(R),
}

/// 生成器对外 trait——和 nightly 的 `Coroutine` 同构，只是名字换成教学版。
///
/// `resume()` 推进一次：让内部状态机从"上一次暂停的地方"跑到"下一个暂停点"
/// （吐 `Yielded`）或"终点"（吐 `Complete`）。已经跑完的生成器再 resume，行为
/// 是返回 `None`（像 `Iterator::next`）。
pub trait Generator {
    /// 每次 yield 吐出的中间值类型。
    type Yield;
    /// 跑完时返回的最终值类型。
    type Return;

    /// 推进一次。`None` 表示生成器已经跑完（之前返回过 `Complete`）。
    fn resume(&mut self) -> Option<GenState<Self::Yield, Self::Return>>;
}

/// "yield 槽"的对外句柄——future 拿到它，就能往里写一个 yield 值。
///
/// 内部是一个 `Rc<RefCell<Option<Y>>>`（或 `Arc<Mutex<...>>`），所以 future 和
/// `Gen::yielded` 共享同一块内存的"所有权"。future 通过 `set` 写入一个值，然后
/// 返回 `Poll::Pending`——这就模拟了一次 `yield v`。
///
/// 我们用 `Rc<RefCell<...>>` 而不是裸指针：单线程生成器用 Rc 就够，避免 unsafe。
/// 教程的"自引用结构 + Pin"那一节会讲清"为什么裸指针需要 Pin"——这里用 Rc 是
/// 让"槽位"从借用检查里逃逸出来，但仍保持内存安全。
pub struct YieldSlot<Y> {
    inner: std::rc::Rc<std::cell::RefCell<Option<Y>>>,
}

impl<Y> YieldSlot<Y> {
    /// 写入一个 yield 值。future 在 poll 时调 `slot.set(v)` 然后 `return Pending`，
    /// 等价于 `yield v`。
    pub fn set(&self, value: Y) {
        *self.inner.borrow_mut() = Some(value);
    }

    /// 从槽里取走值（由 `Gen::resume` 在 poll 后调用）。
    pub(crate) fn take(&self) -> Option<Y> {
        self.inner.borrow_mut().take()
    }

    /// clone 出一个新句柄——future 持一份，Gen 持一份。
    fn clone_handle(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// YieldSlot 不是 Send/Sync（Rc/RefCell 不是）——单线程生成器够用。
// 如果你要跨线程，把内部换成 Arc<Mutex<Option<Y>>>，并把 Gen 标 Send。

/// 一个手写的生成器。内部包一个 `Future<Output = R>`：
///
/// - 每次 `resume()` 拿一个 noop waker poll 一次这个 future；
/// - 如果 future 在内部把某个值塞进了 `yielded` 槽并返回 `Pending`，我们取出
///   槽里的值、返回 `GenState::Yielded(y)`；
/// - 如果 future 返回 `Ready(r)`，生成器进入终结态，返回 `GenState::Complete(r)`；
/// - 之后再 `resume()`，直接返回 `None`。
///
/// **关键约束**：future 的 `poll` 必须在"想吐值"时**先写 `yielded` 槽、再返回
/// `Pending`**。这就是"用 Poll::Pending + 槽位传值"模拟 yield 的全部秘密。
///
/// `Y` 是中间值类型；`R` 是终结返回类型；`F` 是包起来的 future 类型。
pub struct Gen<Y, R, F>
where
    F: Future<Output = R>,
{
    /// 被钉住的 future。`Pin<Box<...>>` 让自引用的状态机地址固定。
    future: Pin<Box<F>>,
    /// "yield 槽"——future 想吐值时往这里写，`resume` 从这里取。
    /// Rc<RefCell<Option<Y>>>：和 future 共享同一块内存。
    yielded: YieldSlot<Y>,
    /// 是否已经跑完（已返回过 `Complete`）。跑完后再 resume 一律返回 `None`。
    done: bool,
}

impl<Y, R, F> Gen<Y, R, F>
where
    F: Future<Output = R>,
{
    /// 构造一个生成器。`future_factory` 收到一个"指向 yield 槽的引用"，返回一个
    /// future；这个 future 内部想 yield 时，往槽里写值、返回 `Pending`。
    ///
    /// 为什么是 `factory` 而不是直接传 future？因为 future 要拿到 yield 槽的句柄
    /// 才能往里写值——这个句柄必须在 future 构造之前就准备好。所以让调用方
    /// "给我一个 yield 槽句柄，我还你一个 future"。
    pub fn new<E>(future_factory: E) -> Self
    where
        E: FnOnce(YieldSlot<Y>) -> F,
    {
        let yielded = YieldSlot {
            inner: std::rc::Rc::new(std::cell::RefCell::new(None)),
        };
        let slot_for_future = yielded.clone_handle();
        let future = future_factory(slot_for_future);
        Self {
            future: Box::pin(future),
            yielded,
            done: false,
        }
    }
}

impl<Y, R, F> Generator for Gen<Y, R, F>
where
    F: Future<Output = R>,
{
    type Yield = Y;
    type Return = R;

    fn resume(&mut self) -> Option<GenState<Y, R>> {
        if self.done {
            return None;
        }
        // 构造一个 noop Context：resume 是同步的，"被 wake"在这里没意义
        // （wake 也只能让 noop waker 啥都不做）；如果 future 返回 Pending，
        // 我们就当"生成器暂停了"，从槽里取值返回 Yielded。
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let poll_result = self.future.as_mut().poll(&mut cx);

        match poll_result {
            Poll::Ready(r) => {
                self.done = true;
                // 即使 future 终结时也写了一个 yield 值，按规范返回 Complete（丢弃 yield 值）。
                // 但通常 future 应当在最后一次 poll 里返回 Ready，不再写槽。
                Some(GenState::Complete(r))
            }
            Poll::Pending => {
                // future 还没跑完，看看它有没有往槽里写值。
                if let Some(y) = self.yielded.take() {
                    Some(GenState::Yielded(y))
                } else {
                    // 一个不太正常的中间态：future 返回 Pending 但没 yield。
                    // 教学版让这个错误立刻暴露——你的 future 写错了。
                    panic!(
                        "Gen::resume: future 返回 Pending 但没调用 YieldSlot::set —— \
                         生成器的 future 必须在返回 Pending 之前 yield 一个值，\
                         否则调用方永远拿不到推进信号"
                    );
                }
            }
        }
    }
}

impl<Y, R, F> Iterator for Gen<Y, R, F>
where
    F: Future<Output = R>,
{
    type Item = Y;

    fn next(&mut self) -> Option<Y> {
        match self.resume() {
            Some(GenState::Yielded(y)) => Some(y),
            Some(GenState::Complete(_)) => None,
            None => None,
        }
    }
}

// =========================================================================
// 手写的"等价于 async fn"状态机 —— 教学对照
// =========================================================================

/// 一个最小可读的"手写状态机生成器"——直接 enum + 状态字段，让读者看清
/// `async fn { yield 1; yield 2; }` 脱糖后的样子。
///
/// 它演示同样的 idea，但**不依赖 Future/Pin**，纯粹一个 enum 推进。
/// 教程第十五章用它和 `Gen` 互相印证。
pub enum HandGen {
    /// 初始态。下一次 resume 进入 State1。
    Start,
    /// "yield 1"之后的暂停点。
    State1,
    /// "yield 2"之后的暂停点。
    State2,
    /// 跑完了。
    Done,
}

impl HandGen {
    pub fn new() -> Self {
        HandGen::Start
    }
}

impl Default for HandGen {
    fn default() -> Self {
        Self::new()
    }
}

impl Generator for HandGen {
    type Yield = u32;
    type Return = ();

    fn resume(&mut self) -> Option<GenState<u32, ()>> {
        match self {
            HandGen::Start => {
                // 对应源码 `yield 1;`：吐出 1，状态推进到 State1。
                *self = HandGen::State1;
                Some(GenState::Yielded(1))
            }
            HandGen::State1 => {
                // 对应源码 `yield 2;`：吐出 2，状态推进到 State2。
                *self = HandGen::State2;
                Some(GenState::Yielded(2))
            }
            HandGen::State2 => {
                // 函数返回（隐式 `return;`）。
                *self = HandGen::Done;
                Some(GenState::Complete(()))
            }
            HandGen::Done => None,
        }
    }
}
