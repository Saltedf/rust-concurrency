//! # async 组合子 —— 从零写 `Race` / `Join` / `Then` / `Timeout`。
//!
//! 这些是 async 编程里最常用的"组合子"（combinator）：它们把几个 future 拼成
//! 一个新的 future，控制"谁先谁后"、"是否全部完成"、"超时怎么办"。
//!
//! `lib.rs` 里已经有一个 spin 版的 `select`（同步循环 + `yield_now`），它教学
//! 够用但**不真异步**。这一章我们把这些组合子写成**真正的 `Future`**——它们
//! 自己也实现 `poll`，由执行器推进。这才能在真实并发里和 reactor 协作。
//!
//! 教程第十六章逐拍画过 `Join` 的 poll 交错：两个子 future 各自异步，Join 用
//! 两个 `Option<...>` 槽"攒"住先 Ready 的那一边，等另一边也 Ready 才整体返回。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::reactor::Reactor;
use crate::Delay;

// =========================================================================
// Race —— 两 future 谁先 Ready 谁赢，另一个 drop
// =========================================================================

/// `Race<F1, F2>` 的结果：哪一边赢，带它的输出 + 另一边的（未完成的）future。
///
/// 和 `lib::SelectOutput` 同构——但 `Race` 是 `Future`（能 await）。
pub enum RaceOutput<A: Future, B: Future> {
    /// A 先 Ready。带 A 的结果 + 没跑完的 B（调用方决定 drop 还是接着跑）。
    Left(<A as Future>::Output, B),
    /// B 先 Ready。带 B 的结果 + 没跑完的 A。
    Right(<B as Future>::Output, A),
}

/// 一个能 await 的"竞争"future：poll 两个子 future，谁先 Ready 谁赢。
///
/// 不像 `lib::select` 的 spin 版（每次 poll 都轮询两边直到一方 Ready），
/// `Race::poll` 只 poll **一次**两边就返回——交给执行器在 waker 触发时再来。
/// 这才是正确的 Future 实现：poll 永远不能阻塞执行器。
///
/// 教程要点：A 和 B 都返回 Pending 时，我们用 **同一个** `cx.waker()` poll 两边
/// ——这样任一边就绪 wake 时，Race 自己也会被叫醒。如果给两边传不同 waker，
/// 就要小心"两个 waker 都得 wake"才保险；统一一个 waker 是最简洁的写法。
pub struct Race<A, B> {
    a: Option<A>,
    b: Option<B>,
}

impl<A, B> Race<A, B>
where
    A: Future,
    B: Future,
{
    pub fn new(a: A, b: B) -> Self {
        Self { a: Some(a), b: Some(b) }
    }
}

impl<A, B> Future for Race<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    type Output = RaceOutput<A, B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: 我们要求 A, B: Unpin，所以把 &mut Self 解 Pin 是安全的——
        // 我们不会把 &mut A / &mut B 暴露成 Pin<&mut A>（那需要 A: Unpin 才 sound）。
        let this = self.get_mut();

        // 先 poll A：如果 Ready，吃掉 a、把 b 还给调用方。
        if let Some(mut a) = this.a.take() {
            if let Poll::Ready(va) = Pin::new(&mut a).poll(cx) {
                let b = this.b.take().expect("b 已被 take 过，Race 内部状态出错");
                return Poll::Ready(RaceOutput::Left(va, b));
            }
            // A 还没好，放回去。
            this.a = Some(a);
        }
        // 再 poll B。
        if let Some(mut b) = this.b.take() {
            if let Poll::Ready(vb) = Pin::new(&mut b).poll(cx) {
                let a = this.a.take().expect("a 已被 take 过，Race 内部状态出错");
                return Poll::Ready(RaceOutput::Right(vb, a));
            }
            this.b = Some(b);
        }
        // 两边都 Pending：等任意一边的 waker 触发再 poll 一次。
        Poll::Pending
    }
}

/// 工厂函数：`race(f1, f2).await`。
pub fn race<A, B>(a: A, b: B) -> Race<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    Race::new(a, b)
}

// =========================================================================
// Join —— 两 future 都要完成，攒结果
// =========================================================================

/// 一个能 await 的"汇合"future：poll 两个子 future，**都要**完成。
///
/// **核心机制**：两个 `Option` 槽存住"先 Ready 的那一边的结果"。一边 Ready 就
/// 塞进槽里、把对应的子 future drop（释放资源），下次 poll 只 poll 还没好的
/// 那一边。两边都 Ready 才返回 `(a, b)`。
///
/// **必须存结果**：因为子 future 一旦返回 `Ready`，再 poll 它就是逻辑 bug
/// （它的内部状态可能已经无效）。教程第十六章逐拍画过这个坑。
pub struct Join<A, B>
where
    A: Future,
    B: Future,
{
    a: Option<A>,
    b: Option<B>,
    /// A 已经 Ready 后的结果（如果 B 还没好）。B Ready 时取出打包返回。
    a_out: Option<A::Output>,
    /// B 已经 Ready 后的结果。
    b_out: Option<B::Output>,
}

impl<A, B> Join<A, B>
where
    A: Future,
    B: Future,
{
    pub fn new(a: A, b: B) -> Self {
        Self {
            a: Some(a),
            b: Some(b),
            a_out: None,
            b_out: None,
        }
    }
}

impl<A, B> Future for Join<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
    A::Output: Unpin,
    B::Output: Unpin,
{
    type Output = (A::Output, B::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // poll A（如果还没好）。
        if let Some(mut a) = this.a.take() {
            match Pin::new(&mut a).poll(cx) {
                Poll::Ready(va) => {
                    this.a_out = Some(va);
                    // a 在这里被 drop（出作用域）。
                }
                Poll::Pending => {
                    this.a = Some(a);
                }
            }
        }
        // poll B（如果还没好）。
        if let Some(mut b) = this.b.take() {
            match Pin::new(&mut b).poll(cx) {
                Poll::Ready(vb) => {
                    this.b_out = Some(vb);
                }
                Poll::Pending => {
                    this.b = Some(b);
                }
            }
        }

        // 两边都好了？
        if this.a_out.is_some() && this.b_out.is_some() {
            let va = this.a_out.take().expect("刚检查过 is_some");
            let vb = this.b_out.take().expect("刚检查过 is_some");
            Poll::Ready((va, vb))
        } else {
            Poll::Pending
        }
    }
}

pub fn join<A, B>(a: A, b: B) -> Join<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
    A::Output: Unpin,
    B::Output: Unpin,
{
    Join::new(a, b)
}

// =========================================================================
// Then —— future 完成后接一个转换函数，链式
// =========================================================================

/// 一个"链式" future：先 await `F`，完成后用 `Fn(F::Output) -> G` 接出 `G`，再 await `G`。
///
/// 这就是 `FutureExt::then` 的最简版本。状态机两态：`First`（在跑 F + 持有 chain）、
/// `Second`（在跑 G）。chain 用 `Option` 包装以便 `take`（FnOnce 不能被多次调用）。
pub struct Then<F, G, Fn>
where
    F: Future,
    G: Future,
{
    inner: ThenState<F, G, Fn>,
}

enum ThenState<F, G, Fn>
where
    F: Future,
    G: Future,
{
    /// 还在跑第一个 future。`chain` 包在 Option 里，因为它是 `FnOnce` —— 只能被调一次，
    /// poll 时 `take` 走。
    First { first: F, chain: Option<Fn> },
    /// 第一个 future 完成了，正在跑第二个。
    Second { second: G },
    /// 都完成了（终止态，再 poll 会 panic）。
    Done,
}

impl<F, G, Fn> Then<F, G, Fn>
where
    F: Future,
    G: Future,
    Fn: FnOnce(F::Output) -> G,
{
    pub fn new(first: F, chain: Fn) -> Self {
        Self {
            inner: ThenState::First { first, chain: Some(chain) },
        }
    }
}

impl<F, G, Fn> Future for Then<F, G, Fn>
where
    F: Future + Unpin,
    G: Future + Unpin,
    Fn: FnOnce(F::Output) -> G + Unpin,
{
    type Output = G::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            match &mut this.inner {
                ThenState::First { first, chain } => {
                    let v = match Pin::new(first).poll(cx) {
                        Poll::Ready(v) => v,
                        Poll::Pending => return Poll::Pending,
                    };
                    // take 走 chain（FnOnce），用它构造第二个 future。
                    let chain = chain
                        .take()
                        .expect("ThenState::First 的 chain 应当还在（状态机只走一次 First → Second）");
                    let second = chain(v);
                    this.inner = ThenState::Second { second };
                }
                ThenState::Second { second } => match Pin::new(second).poll(cx) {
                    Poll::Ready(v) => {
                        this.inner = ThenState::Done;
                        return Poll::Ready(v);
                    }
                    Poll::Pending => return Poll::Pending,
                },
                ThenState::Done => panic!("Then::poll 被在一个已完成的 future 上调用"),
            }
        }
    }
}

/// 工厂：`then(f, |v| g(v)).await`。
pub fn then<F, G, Fn>(first: F, chain: Fn) -> Then<F, G, Fn>
where
    F: Future + Unpin,
    G: Future + Unpin,
    Fn: FnOnce(F::Output) -> G + Unpin,
{
    Then::new(first, chain)
}

// =========================================================================
// Timeout —— 套一个 deadline，超时返回 Err
// =========================================================================

/// `Timeout<F>` 的输出：要么 inner 完成（`Ok(v)`），要么 deadline 到了（`Err(inner)`）。
///
/// 设计上我们把 inner future 还给调用方（`Elapsed(inner)`）——这样调用方决定
/// 要 drop 取消，还是再 try 一次（虽然 inner 通常已经跑了一部分，再 await 没意义）。
pub enum TimeoutOutput<F: Future> {
    /// inner future 在 deadline 前完成。
    Ok(F::Output),
    /// deadline 先到。带 inner future 还给调用方（决定要不要 drop 取消）。
    Elapsed(F),
}

/// 把一个 future 套一层 deadline。
///
/// 实现：内部维护一个 `Delay`（来自 `lib`，注册到 reactor）。每次 poll 先看 inner
/// 完没完，再看 Delay 完没完——哪个先 Ready 就走哪一支。两边都用同一个 waker。
///
/// 注意 `Timeout::poll` 的顺序：**先 poll inner**，这样如果 inner 在 deadline 之前
/// 恰好就绪，我们不会因为 Delay 早一拍 Ready 而误判超时。
pub struct Timeout<F: Future> {
    inner: Option<F>,
    delay: Option<Delay>,
}

impl<F: Future> Timeout<F> {
    /// 构造：inner future + 一个 deadline（after 距今多久）+ 一个 reactor 引用。
    pub fn new(inner: F, reactor: Reactor, after: Duration) -> Self {
        Self {
            inner: Some(inner),
            delay: Some(Delay::new(reactor, after)),
        }
    }
}

impl<F> Future for Timeout<F>
where
    F: Future + Unpin,
{
    type Output = TimeoutOutput<F>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // 1) 先 poll inner。
        if let Some(mut inner) = this.inner.take() {
            match Pin::new(&mut inner).poll(cx) {
                Poll::Ready(v) => {
                    // inner 完了——把 Delay drop（反注册 timer）。
                    this.delay = None;
                    return Poll::Ready(TimeoutOutput::Ok(v));
                }
                Poll::Pending => {
                    this.inner = Some(inner);
                }
            }
        }

        // 2) 再 poll Delay。
        if let Some(mut delay) = this.delay.take() {
            match Pin::new(&mut delay).poll(cx) {
                Poll::Ready(()) => {
                    // 超时——把 inner 还给调用方。
                    let inner = this
                        .inner
                        .take()
                        .expect("Timeout 内部状态错：inner 已被 take");
                    return Poll::Ready(TimeoutOutput::Elapsed(inner));
                }
                Poll::Pending => {
                    this.delay = Some(delay);
                }
            }
        }

        // 两边都没好。等任意一边的 waker 触发再来一次。
        Poll::Pending
    }
}

/// 工厂：`timeout(f, reactor, 100_ms).await`。返回 `TimeoutOutput<F>`。
pub fn timeout<F: Future + Unpin>(inner: F, reactor: Reactor, after: Duration) -> Timeout<F> {
    Timeout::new(inner, reactor, after)
}

// =========================================================================
// 小工具：把任意 T 包成立即完成的 future（教学辅助）
// =========================================================================

/// 一个"立即就绪"的 future——构造时就带好值，第一次 poll 返回 Ready。
/// 教程用它在测试里"喂" Race/Join 的某一支，让另一支慢慢就绪。
pub struct Ready<T>(pub Option<T>);

impl<T> Ready<T> {
    pub fn new(v: T) -> Self {
        Ready(Some(v))
    }
}

impl<T> Future for Ready<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        self.0
            .take()
            .map(Poll::Ready)
            .expect("Ready::poll 被在一个已完成的 future 上调用")
    }
}

impl<T> Unpin for Ready<T> {}
