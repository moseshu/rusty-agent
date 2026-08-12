//! The cancellation contract: a [`CancellationToken`] tree over `run -> turn -> tool -> child
//! process`.
//!
//! # Why cancellation needs a contract
//!
//! In Rust, "cancelling a future" means dropping it, which looks like it needs no machinery at
//! all. The real problem is that **not all in-flight work is owned by a future**: a task from
//! `tokio::spawn`, a child process from `Command::spawn`, a remote MCP call — dropping the handle
//! only severs a reference while the work keeps running. Cancellation therefore has to be an
//! **explicit, propagable signal** rather than an implicit drop.
//!
//! # Three invariants
//!
//! | Invariant | Guaranteed by |
//! | --- | --- |
//! | Cancellation propagates **downward only**: cancelling a tool does not kill the run | the token tree of [`CancelScope::child`] |
//! | The root cause is **first-writer-wins**: propagation never rewrites a child scope's own reason | [`CancelScope::cancel`] and [`CancelScope::reason`] |
//! | Cancellation is **not failure**: no failure rate, no retry | [`Error::recoverability`] projects to `Cancelled` |
//!
//! The second invariant is deliberate: **there is no `ParentCancelled` reason**. When a tool is
//! cancelled by a timeout and the whole run is then cancelled by the user, that tool still reports
//! [`CancelReason::Timeout`] — otherwise attribution is lost on the very first propagation hop.
//!
//! # This module's boundary
//!
//! `ra-core` owns no runtime, so it **arms no timer**. [`Deadline`] is pure data; firing a
//! cancellation when it expires belongs to whoever holds the runtime (`ra-runtime`). As a
//! backstop, any checkpoint ([`CancelScope::ensure_not_cancelled`], [`CancelScope::run`]) that
//! observes an expired deadline converts it into a real cancellation on the spot.
//!
//! The full rules, the per-layer responsibilities, and the counterexamples are in
//! `Docs/Cancellation_Contract.md`.

use core::fmt;
use core::future::Future;
use core::pin::pin;
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::future::{Either, select};
use tokio_util::sync::CancellationToken;

use crate::error::Error;

/// Grace period for in-flight work to drain to a terminal state after a cancellation signal;
/// past it, work is killed.
///
/// This is part of the contract, not a tuning knob: **dropping a `JoinHandle` after cancelling
/// leaves a running child process behind in Rust**. Any layer that spawned a task or a child
/// process must wait for a terminal state before returning, and kill it when the grace period
/// ends.
pub const DRAIN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// cancellation reasons
// ---------------------------------------------------------------------------

/// Root cause of a cancellation. It becomes a trace label and an eval attribution, so it **has to
/// be enumerable** rather than a sentence.
///
/// How it relates to [`Error`]: `Error::Cancelled` carries human-facing text only, while machine
/// attribution goes through [`CancelReason::code`] — **do not parse the error text** to recover
/// the reason.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// The user interrupted deliberately: Ctrl-C, or a stop button in the UI.
    UserInterrupt,
    /// The process received a termination signal and is shutting down.
    Shutdown,
    /// The wall-clock budget ran out (`Budget::deadline`): a whole run, or one layer, hit its
    /// deadline.
    Deadline,
    /// One operation's own timeout: a tool call's own timeout, or an in-flight protocol request.
    ///
    /// It differs from [`Self::Deadline`] in **whose limit** expired: `Deadline` is an upper
    /// layer's budget, `Timeout` is this operation running too long. The responses differ — the
    /// first should end the task, the second usually only abandons this one operation.
    Timeout,
    /// The result is no longer wanted: a losing branch of an `any` or `quorum` join, or an
    /// in-flight request superseded by new input. **Neither an error nor a timeout.**
    Superseded,
    /// Another task in the same batch failed and continuing the batch is pointless.
    PeerFailure,
    /// No root cause was recorded.
    ///
    /// It should only appear on a path that **bypasses this module and cancels a bare
    /// [`CancellationToken`] directly** (unavoidable when a third-party library holds the token).
    /// Constructing it explicitly inside the framework is forbidden: seeing it means some
    /// cancellation path skipped [`CancelScope::cancel`], and attribution is broken there.
    Unspecified,
    /// Extension point: a reason defined by a product or third party (extension-safety rule 5).
    ///
    /// Use `snake_case` for the label and prefer an own prefix (such as `myapp_quota`) so it
    /// cannot collide with a built-in [`Self::code`].
    Custom(Cow<'static, str>),
}

impl CancelReason {
    /// Creates a [custom reason](Self::Custom).
    #[must_use]
    pub fn custom(label: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(label.into())
    }

    /// Stable machine-readable identity, used as a trace label and a metric dimension value.
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

    /// Whether time ran out.
    ///
    /// These two tiers belong in eval's "timeout rate" and not in its "user interrupt rate";
    /// keeping those apart is the only reason this projection exists.
    #[must_use]
    pub const fn is_expiry(&self) -> bool {
        matches!(self, Self::Deadline | Self::Timeout)
    }

    /// Whether a human initiated it (a user interrupt or process termination) rather than an
    /// internal scheduling decision.
    ///
    /// The UI should show "stopped" for these two tiers and **show nothing at all** for the likes
    /// of [`Self::Superseded`] — that is internal framework orchestration the user need not know
    /// about.
    #[must_use]
    pub const fn is_user_initiated(&self) -> bool {
        matches!(self, Self::UserInterrupt | Self::Shutdown)
    }

    /// User-facing phrase. It becomes the `reason` field of `Error::Cancelled`.
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
    /// Converges into [`Error::Cancelled`], whose recoverability projects to `Cancelled` (not a
    /// failure).
    ///
    /// **The machine-readable reason is lost at this step**, deliberately: `Error` answers "what
    /// to do about it" while attribution answers "why it happened", and the latter travels as
    /// [`CancelReason::code`] in the trace.
    fn from(reason: CancelReason) -> Self {
        Self::cancelled(reason.user_message())
    }
}

// ---------------------------------------------------------------------------
// scope levels
// ---------------------------------------------------------------------------

/// A scope's level in the cancellation tree. Used for diagnostics and trace labels only; it does
/// not affect propagation semantics.
///
/// The canonical nesting is `Run -> Turn -> Tool -> Process`. A sub-agent's run is another `Run`
/// hanging under a `Tool`, so **levels are not required to decrease monotonically**.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// An entire run. A sub-agent's run is also this level.
    Run,
    /// One turn: a model round trip plus the tool batch that follows it.
    Turn,
    /// One tool call.
    Tool,
    /// One child process or PTY session.
    Process,
    /// Extension point: a custom level such as a graph node (extension-safety rule 5).
    Custom(Cow<'static, str>),
}

impl ScopeKind {
    /// Creates a [custom level](Self::Custom).
    #[must_use]
    pub fn custom(label: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(label.into())
    }

    /// Stable machine-readable identity.
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
// wall-clock deadlines
// ---------------------------------------------------------------------------

/// A deadline expressed as an absolute instant.
///
/// An instant rather than a `Duration`, because deadlines are inherited across layers: a child
/// scope must receive the same endpoint, not a fresh duration measured from its own start — the
/// latter would hand out a full duration again at every level of nesting.
///
/// **Not serializable**: [`Instant`] is a point on a monotonic clock and means nothing across
/// processes. `Deadline` therefore stays out of `RunState`; a deadline that must be persisted is
/// stored as absolute wall-clock time and converted back on load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Deadline(Instant);

impl Deadline {
    /// Creates one from an absolute instant.
    #[must_use]
    pub const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    /// Expires `after` from now.
    #[must_use]
    pub fn after(after: Duration) -> Self {
        Self(Instant::now() + after)
    }

    /// The expiry instant. `ra-runtime` arms its timer from it
    /// (`tokio::time::Instant::from_std`).
    #[must_use]
    pub const fn instant(self) -> Instant {
        self.0
    }

    /// Time left before expiry; zero once expired.
    #[must_use]
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    /// Whether it has expired.
    ///
    /// Expired **is not the same as cancelled**; see [`CancelScope::ensure_not_cancelled`].
    #[must_use]
    pub fn is_expired(self) -> bool {
        self.remaining().is_zero()
    }
}

// ---------------------------------------------------------------------------
// cancellation scopes
// ---------------------------------------------------------------------------

/// Storage slot for a cancellation reason: one cell of its own plus a link to the parent slot.
///
/// A child scope without its own reason walks up the chain, so "propagation preserves the root
/// cause" requires writing nothing downward at cancellation time — **the root cause is a looked-up
/// projection, not a copied duplicate**, the same approach as `Recoverability`.
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

/// One scope in the cancellation tree: a [`CancellationToken`] plus its root cause and deadline.
///
/// [`Clone`] yields **another handle to the same scope** (sharing the token and the root cause),
/// which is how a scope is handed to a spawned task. To derive a new level, use [`Self::child`].
///
/// # Usage
///
/// ```ignore
/// let run = CancelScope::root().with_deadline(Deadline::after(TEN_MINUTES));
/// let turn = run.child(ScopeKind::Turn);
/// let tool = turn.child(ScopeKind::Tool).with_deadline(Deadline::after(THIRTY_SECONDS));
///
/// // Every await point is cancellable:
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
    /// Creates the root of a new cancellation tree, at level [`ScopeKind::Run`].
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

    /// Derives a child scope: a parent cancellation propagates into it, its own does not
    /// propagate back up.
    ///
    /// The deadline is inherited as a **snapshot taken at creation**. Tightening the parent
    /// later does not reach back into children that already exist; a tightening that must take
    /// effect immediately goes through [`Self::cancel`].
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

    /// Sets the deadline. **It can only tighten, never loosen**: an input later than the
    /// inherited deadline is ignored.
    ///
    /// Otherwise a single tool could grant itself more time than the whole run, and the run-level
    /// budget would mean nothing.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(
            self.deadline
                .map_or(deadline, |current| current.min(deadline)),
        );
        self
    }

    /// This scope's level.
    #[must_use]
    pub const fn kind(&self) -> &ScopeKind {
        &self.kind
    }

    /// The deadline in effect, including one inherited from a parent.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// The underlying token, for handing cancellation to a third-party library that only speaks
    /// [`CancellationToken`].
    ///
    /// **Cancelling through it loses the root cause** (it degrades to
    /// [`CancelReason::Unspecified`]). Use it only when an interface leaves no choice.
    #[must_use]
    pub const fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// Whether it has been cancelled.
    ///
    /// It reads the cancellation signal only, **not the deadline**: when a deadline has expired
    /// but nobody armed a timer, this still returns `false` until some checkpoint converts it into
    /// a real cancellation. That keeps `is_cancelled()` and [`Self::token`] permanently in
    /// agreement.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The cancellation root cause; `None` while not cancelled.
    ///
    /// Once cancelled it **always** returns `Some`: a lookup that finds nothing on the chain
    /// degrades to [`CancelReason::Unspecified`], so "cancelled but reasonless" is not a
    /// reachable state.
    #[must_use]
    pub fn reason(&self) -> Option<CancelReason> {
        if !self.is_cancelled() {
            return None;
        }
        Some(self.slot.lookup().unwrap_or(CancelReason::Unspecified))
    }

    /// Cancels this scope and every descendant. The parent is **unaffected**.
    ///
    /// A no-op once already cancelled: **the first root cause is never overwritten by a later
    /// one**. So when a tool times out and the user then interrupts the whole run, that tool still
    /// reports `Timeout`.
    pub fn cancel(&self, reason: CancelReason) {
        if self.token.is_cancelled() {
            return;
        }
        // Record the reason before signalling: the other order lets a woken waiter read an empty
        // slot. Under concurrency the `OnceLock` first-writer wins.
        let _ = self.slot.own.set(reason);
        self.token.cancel();
    }

    /// Waits until this scope is cancelled.
    ///
    /// Note a deadline **does not** wake it on its own: with no timer armed, an expired deadline
    /// is only noticed at a checkpoint. Waking on a deadline requires whoever holds the runtime to
    /// arm one.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Checkpoint: returns an error carrying the root cause when already cancelled.
    ///
    /// It also backstops deadlines: observing an expired one **converts it into a real
    /// cancellation on the spot** (with reason [`CancelReason::Deadline`], propagated to
    /// descendants). So even with no timer at all, a timeout still takes effect at the next
    /// checkpoint, just less promptly.
    ///
    /// Insert one in a long loop and in any synchronous stretch between two awaits.
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

    /// Runs a future inside this scope: cancellation returns an `Err` carrying the root cause
    /// immediately.
    ///
    /// This is the default way to make every await point cancellable. An already-cancelled scope
    /// **starts no new work**: it returns `Err` even when the future is long since ready.
    ///
    /// The deadline is checked **once, at entry**: expiring mid-wait does not wake anything, so a
    /// timer armed by `ra-runtime` has to cancel the tree. Without a timer, the timeout is
    /// deferred to the next checkpoint.
    ///
    /// # When not to use it
    ///
    /// On cancellation `fut` is **dropped**. That is safe for a pure future, but if the future
    /// owns a spawned task or a child process behind it, dropping only lets go while the process
    /// keeps running — that case must follow the drain protocol ([`DRAIN_GRACE`]) instead of this
    /// helper.
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

    /// Binds an RAII guard that cancels this scope with `reason` when the guard drops.
    ///
    /// It addresses **forgetting to cancel**: dropping a scope does not cancel its token, so a
    /// descendant waiting on [`Self::cancelled`] waits forever. Anywhere a scope is handed to a
    /// spawned task, use the guard rather than remembering to call [`Self::cancel`] on every exit
    /// path.
    #[must_use]
    pub fn cancel_on_drop(self, reason: CancelReason) -> CancelOnDrop {
        CancelOnDrop {
            scope: self,
            reason: Some(reason),
        }
    }
}

/// The RAII guard from [`CancelScope::cancel_on_drop`].
#[derive(Debug)]
pub struct CancelOnDrop {
    scope: CancelScope,
    reason: Option<CancelReason>,
}

impl CancelOnDrop {
    /// The guarded scope.
    #[must_use]
    pub const fn scope(&self) -> &CancelScope {
        &self.scope
    }

    /// Disarms the guard: the work finished normally, so dropping it cancels nothing.
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
