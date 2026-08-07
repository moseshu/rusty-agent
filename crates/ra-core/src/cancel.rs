//! 取消契约：`run -> turn -> tool -> 子进程` 的 [`CancellationToken`] 树。
//!
//! # 为什么取消需要一份契约
//!
//! Rust 里「取消一个 future」就是把它 drop 掉，看上去不需要任何机制。真正的问题
//! 在于**不是所有在途工作都被 future 拥有**：`tokio::spawn` 出去的任务、
//! `Command::spawn` 出去的子进程、MCP 的远端调用，drop 掉句柄只断开引用，活儿还在
//! 跑。所以取消必须是一个**显式的、可传播的信号**，而不是隐式的 drop。
//!
//! # 三个不变量
//!
//! | 不变量 | 由谁保证 |
//! | --- | --- |
//! | 取消**只向下**传播：取消一个工具不会杀掉整个 run | [`CancelScope::child`] 的 token 树 |
//! | 根因**先到先得**：传播不改写子作用域自己的取消原因 | [`CancelScope::cancel`] 与 [`CancelScope::reason`] |
//! | 取消**不是失败**：不计失败率、不触发重试 | [`Error::recoverability`] 投影为 `Cancelled` |
//!
//! 第二条是刻意的设计：**不设 `ParentCancelled` 这种原因**。工具因超时被取消、
//! 随后整个 run 因用户中断被取消，工具那一层仍然报 [`CancelReason::Timeout`]——
//! 否则归因在传播的第一跳就丢了。
//!
//! # 本模块的边界
//!
//! `ra-core` 不持有运行时，因此这里**不 arm 任何定时器**。[`Deadline`] 是纯数据，
//! 到点触发取消由持有 runtime 的一方（`ra-runtime`）负责；作为兜底，任何检查点
//! （[`CancelScope::ensure_not_cancelled`] / [`CancelScope::run`]）观察到 deadline
//! 过期都会就地把它转成一次真正的取消。
//!
//! 完整规则、分层责任与反例见 `Docs/Cancellation_Contract.md`。

use core::fmt;
use core::future::Future;
use core::pin::pin;
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::future::{Either, select};
use tokio_util::sync::CancellationToken;

use crate::error::Error;

/// 取消信号发出后，等待在途任务 drain 到终态的宽限期；超过则强制终止。
///
/// 这是契约的一部分而不是调优参数：**取消后直接 drop `JoinHandle` 会在 Rust 里
/// 留下正在跑的子进程**（R3-4c ③）。任何 spawn 了任务或子进程的一层，都必须在
/// 取消后等到终态再返回，等不到就在宽限期结束时强杀。
pub const DRAIN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// 取消原因
// ---------------------------------------------------------------------------

/// 取消的根因。进 trace 标签与 eval 归因，因此**必须可枚举**，不能只是一句话。
///
/// 与 [`Error`] 的关系：`Error::Cancelled` 只携带面向人的文本，机器归因走
/// [`CancelReason::code`]——**不要去解析错误文本**反推原因。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// 用户主动中断：Ctrl-C、UI 上的停止按钮。
    UserInterrupt,
    /// 进程收到终止信号，整体收摊。
    Shutdown,
    /// 墙钟预算耗尽（`Budget::deadline`）。整个 run 或某一层的时限到了。
    Deadline,
    /// 单个操作自己的超时：工具超时（R2-7）、在途协议请求超时（R13）。
    ///
    /// 与 [`Self::Deadline`] 的区别是**谁的时限**：`Deadline` 是上层预算到点，
    /// `Timeout` 是这个操作本身跑太久。两者的处置不同——前者该结束任务，后者
    /// 通常只该放弃这一个操作。
    Timeout,
    /// 结果已不再被需要：`any` / `quorum` join 的落败分支（R17-4）、被新输入
    /// 取代的在途请求。**不是错误，也不是超时**。
    Superseded,
    /// 同批次的另一个任务失败，整批继续下去已无意义（R3-4c 的批量结算）。
    PeerFailure,
    /// 未记录根因。
    ///
    /// 只应出现在**绕过本模块、直接 cancel 裸 [`CancellationToken`]** 的路径上
    /// （第三方库持有 token 时无法避免）。框架内部禁止显式构造它——出现即说明
    /// 有一条取消路径没走 [`CancelScope::cancel`]，归因会断。
    Unspecified,
    /// 扩展点：产品或第三方自定义的原因（扩展安全第 5 条）。
    ///
    /// 标签用 `snake_case`，建议带自有前缀（如 `myapp_quota`），避免与内置
    /// [`Self::code`] 撞名。
    Custom(Cow<'static, str>),
}

impl CancelReason {
    /// 构造[自定义原因](Self::Custom)。
    #[must_use]
    pub fn custom(label: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(label.into())
    }

    /// 稳定的机器可读标识。用于 trace 标签与指标维度值。
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::UserInterrupt => "user_interrupt",
            Self::Shutdown => "shutdown",
            Self::Deadline => "deadline",
            Self::Timeout => "timeout",
            Self::Superseded => "superseded",
            Self::PeerFailure => "peer_failure",
            Self::Unspecified => "unspecified",
            Self::Custom(label) => label.as_ref(),
        }
    }

    /// 是否因为时间到了。
    ///
    /// 这两档该进 eval 的「超时率」，不该混进「用户中断率」——把它们分开统计是
    /// 这个投影存在的唯一理由。
    #[must_use]
    pub const fn is_expiry(&self) -> bool {
        matches!(self, Self::Deadline | Self::Timeout)
    }

    /// 是否由人发起（用户中断或进程被终止），而非系统内部的调度决定。
    ///
    /// UI 对这两档要显示「已停止」，对 [`Self::Superseded`] 之类则**什么都不该
    /// 显示**——那是框架的内部编排，用户不需要知道。
    #[must_use]
    pub const fn is_user_initiated(&self) -> bool {
        matches!(self, Self::UserInterrupt | Self::Shutdown)
    }

    /// 面向用户的短语。会成为 `Error::Cancelled` 的 `reason` 字段。
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::UserInterrupt => "用户中断".to_owned(),
            Self::Shutdown => "程序正在退出".to_owned(),
            Self::Deadline => "已超过时间上限".to_owned(),
            Self::Timeout => "操作超时".to_owned(),
            Self::Superseded => "结果已不再需要".to_owned(),
            Self::PeerFailure => "同批次的其它任务失败".to_owned(),
            Self::Unspecified => "未记录原因".to_owned(),
            Self::Custom(label) => label.as_ref().to_owned(),
        }
    }
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl From<CancelReason> for Error {
    /// 收敛成 [`Error::Cancelled`]，可恢复性投影为 `Cancelled`（不是失败）。
    ///
    /// **机器可读的原因在这一步丢失**，这是刻意的：`Error` 面向「怎么处置」，
    /// 归因面向「为什么发生」，后者走 trace 里的 [`CancelReason::code`]。
    fn from(reason: CancelReason) -> Self {
        Self::cancelled(reason.user_message())
    }
}

// ---------------------------------------------------------------------------
// 作用域层级
// ---------------------------------------------------------------------------

/// 作用域在取消树里的层级。只用于诊断与 trace 标注，不影响传播语义。
///
/// 规范嵌套是 `Run -> Turn -> Tool -> Process`；子 agent（R12）是挂在 `Tool`
/// 下面的又一个 `Run`，因此**不强制层级单调递减**。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// 一整个 run。子 agent 的 run 也是这一档。
    Run,
    /// 一个 turn（一次模型往返 + 其后的工具批次）。
    Turn,
    /// 一次工具调用。
    Tool,
    /// 一个子进程 / PTY 会话。
    Process,
    /// 扩展点：图节点等自定义层级（扩展安全第 5 条）。
    Custom(Cow<'static, str>),
}

impl ScopeKind {
    /// 构造[自定义层级](Self::Custom)。
    #[must_use]
    pub fn custom(label: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(label.into())
    }

    /// 稳定的机器可读标识。
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Run => "run",
            Self::Turn => "turn",
            Self::Tool => "tool",
            Self::Process => "process",
            Self::Custom(label) => label.as_ref(),
        }
    }
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// 墙钟时限
// ---------------------------------------------------------------------------

/// 绝对时间点形式的时限。
///
/// 用绝对时间而不是 `Duration`，是因为时限要跨层继承：子作用域拿到的必须是「还
/// 剩多久」的同一个终点，而不是从自己开始重新计时的一段时长——后者会让每嵌套一
/// 层就白送一次完整时长。
///
/// **不可序列化**：[`Instant`] 是单调时钟上的点，跨进程无意义。因此 `Deadline`
/// 不进 `RunState`；需要持久化的时限存绝对墙钟时间，加载时再换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Deadline(Instant);

impl Deadline {
    /// 以一个绝对时间点构造。
    #[must_use]
    pub const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    /// 从现在起 `after` 之后到期。
    #[must_use]
    pub fn after(after: Duration) -> Self {
        Self(Instant::now() + after)
    }

    /// 到期时间点。`ra-runtime` 用它 arm 定时器（`tokio::time::Instant::from_std`）。
    #[must_use]
    pub const fn instant(self) -> Instant {
        self.0
    }

    /// 距离到期还剩多久；已过期则为零。
    #[must_use]
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    /// 是否已过期。
    ///
    /// 过期**不等于已取消**——见 [`CancelScope::ensure_not_cancelled`]。
    #[must_use]
    pub fn is_expired(self) -> bool {
        self.remaining().is_zero()
    }
}

// ---------------------------------------------------------------------------
// 取消作用域
// ---------------------------------------------------------------------------

/// 取消原因的存储槽：自己一格，外加指向父槽的链。
///
/// 子作用域没有自己的原因时沿链上溯，于是「传播保留根因」不需要在取消时向下写
/// 任何东西——**根因是查出来的投影，不是复制出来的副本**，与 `Recoverability`
/// 同一个路子。
#[derive(Debug)]
struct ReasonSlot {
    own: OnceLock<CancelReason>,
    parent: Option<Arc<ReasonSlot>>,
}

impl ReasonSlot {
    fn lookup(&self) -> Option<CancelReason> {
        if let Some(reason) = self.own.get() {
            return Some(reason.clone());
        }
        self.parent.as_ref()?.lookup()
    }
}

/// 取消树上的一个作用域：一个 [`CancellationToken`] 加上它的根因与时限。
///
/// [`Clone`] 得到的是**同一个作用域的另一个句柄**（共享 token 与根因），用于把
/// 作用域交给 spawn 出去的任务；要派生新层级用 [`Self::child`]。
///
/// # 用法
///
/// ```ignore
/// let run = CancelScope::root().with_deadline(Deadline::after(TEN_MINUTES));
/// let turn = run.child(ScopeKind::Turn);
/// let tool = turn.child(ScopeKind::Tool).with_deadline(Deadline::after(THIRTY_SECONDS));
///
/// // 每个 await 点都可取消：
/// let output = tool.run(call_the_tool()).await?;
/// ```
#[derive(Debug, Clone)]
pub struct CancelScope {
    kind: ScopeKind,
    token: CancellationToken,
    slot: Arc<ReasonSlot>,
    deadline: Option<Deadline>,
}

impl CancelScope {
    /// 新建一棵取消树的根，层级为 [`ScopeKind::Run`]。
    #[must_use]
    pub fn root() -> Self {
        Self {
            kind: ScopeKind::Run,
            token: CancellationToken::new(),
            slot: Arc::new(ReasonSlot {
                own: OnceLock::new(),
                parent: None,
            }),
            deadline: None,
        }
    }

    /// 派生一个子作用域：父取消会传播到它，它取消不影响父。
    ///
    /// 时限按**创建时快照**继承。之后再收紧父作用域不会追溯到已创建的子作用域；
    /// 需要立刻生效的收紧走 [`Self::cancel`]。
    #[must_use]
    pub fn child(&self, kind: ScopeKind) -> Self {
        Self {
            kind,
            token: self.token.child_token(),
            slot: Arc::new(ReasonSlot {
                own: OnceLock::new(),
                parent: Some(Arc::clone(&self.slot)),
            }),
            deadline: self.deadline,
        }
    }

    /// 设置时限。**只能收紧不能放宽**——比已继承的时限更晚的输入会被忽略。
    ///
    /// 否则一个工具就能给自己批一个比整个 run 更长的时限，run 级预算形同虚设。
    #[must_use]
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(
            self.deadline
                .map_or(deadline, |current| current.min(deadline)),
        );
        self
    }

    /// 本作用域的层级。
    #[must_use]
    pub const fn kind(&self) -> &ScopeKind {
        &self.kind
    }

    /// 生效中的时限（含从父作用域继承的）。
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// 底层 token，用于把取消传给只认 [`CancellationToken`] 的第三方库。
    ///
    /// **通过它取消会丢失根因**（降级为 [`CancelReason::Unspecified`]）。只在
    /// 接口不给选择时才这么用。
    #[must_use]
    pub const fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// 是否已被取消。
    ///
    /// 只看取消信号，**不看时限**：时限过期但没人 arm 定时器时，这里仍返回
    /// `false`，直到某个检查点把它转成真取消。这样 `is_cancelled()` 与
    /// [`Self::token`] 的视图永远一致。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// 取消的根因；未取消时为 `None`。
    ///
    /// 已取消时**必定**返回 `Some`：链上查不到就降级为
    /// [`CancelReason::Unspecified`]，因此「已取消却没有原因」这种状态不存在。
    #[must_use]
    pub fn reason(&self) -> Option<CancelReason> {
        if !self.is_cancelled() {
            return None;
        }
        Some(self.slot.lookup().unwrap_or(CancelReason::Unspecified))
    }

    /// 取消本作用域及其全部后代。对父作用域**无影响**。
    ///
    /// 已取消时是无操作：**先到的根因不被后到的覆盖**。因此工具超时之后整个 run
    /// 又被用户中断，工具那一层仍然报 `Timeout`。
    pub fn cancel(&self, reason: CancelReason) {
        if self.token.is_cancelled() {
            return;
        }
        // 先记原因再发信号：反过来会让醒来的等待方读到空的槽。
        // 并发下以 OnceLock 的先到者为准。
        let _ = self.slot.own.set(reason);
        self.token.cancel();
    }

    /// 等待本作用域被取消。
    ///
    /// 注意时限**不会**自己唤醒它——没有 arm 定时器的话，过期的 deadline 只在
    /// 检查点被发现。要靠时限醒来，由持有 runtime 的一方 arm。
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// 检查点：已取消则返回带根因的错误。
    ///
    /// 顺带兜底时限——观察到 deadline 过期就**就地转成一次真正的取消**（原因为
    /// [`CancelReason::Deadline`]，并传播给后代）。所以即使没有任何定时器，超时
    /// 也一定会在下一个检查点生效，只是不那么及时。
    ///
    /// 长循环、两次 await 之间的同步计算段，都该插一次。
    pub fn ensure_not_cancelled(&self) -> Result<(), Error> {
        if let Some(deadline) = self.deadline
            && deadline.is_expired()
        {
            self.cancel(CancelReason::Deadline);
        }
        match self.reason() {
            Some(reason) => Err(reason.into()),
            None => Ok(()),
        }
    }

    /// 在本作用域内跑一个 future：取消即刻返回带根因的 `Err`。
    ///
    /// 这是「每个 await 点可取消」的默认写法。已取消的作用域**不会启动新工作**，
    /// 即便 future 早已就绪也直接返回 `Err`。
    ///
    /// 时限只在**入口**检查一次：等待期间到期不会自己醒来，得靠 `ra-runtime`
    /// arm 的定时器 cancel 这棵树。没有定时器时，超时推迟到下一个检查点才生效。
    ///
    /// # 什么时候不能用
    ///
    /// 取消时 `fut` 被 **drop**。纯 future 这样处理是安全的；但如果 future 背后
    /// 拥有 spawn 出去的任务或子进程，drop 只是撒手不管，进程还在跑——那种情况
    /// 必须走 drain 协议（[`DRAIN_GRACE`]），不能用这个 helper。
    pub async fn run<F>(&self, fut: F) -> Result<F::Output, Error>
    where
        F: Future,
    {
        self.ensure_not_cancelled()?;

        let fut = pin!(fut);
        let cancelled = pin!(self.token.cancelled());
        match select(fut, cancelled).await {
            Either::Left((output, _)) => Ok(output),
            Either::Right(((), _)) => {
                Err(self.reason().unwrap_or(CancelReason::Unspecified).into())
            }
        }
    }

    /// 绑定 RAII 守卫：守卫 drop 时用 `reason` 取消本作用域。
    ///
    /// 解决的是**忘记取消**：作用域被 drop 并不会取消它的 token，于是等在
    /// [`Self::cancelled`] 上的后代任务会永远等下去。凡是把作用域交给了 spawn
    /// 任务的地方，都该用守卫而不是靠记得在每条退出路径上调 [`Self::cancel`]。
    #[must_use]
    pub fn cancel_on_drop(self, reason: CancelReason) -> CancelOnDrop {
        CancelOnDrop {
            scope: self,
            reason: Some(reason),
        }
    }
}

/// [`CancelScope::cancel_on_drop`] 的 RAII 守卫。
#[derive(Debug)]
pub struct CancelOnDrop {
    scope: CancelScope,
    reason: Option<CancelReason>,
}

impl CancelOnDrop {
    /// 被守卫的作用域。
    #[must_use]
    pub const fn scope(&self) -> &CancelScope {
        &self.scope
    }

    /// 解除守卫：工作已正常收尾，drop 时不再取消。
    #[must_use]
    pub fn disarm(mut self) -> CancelScope {
        self.reason = None;
        self.scope.clone()
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(reason) = self.reason.take() {
            self.scope.cancel(reason);
        }
    }
}
