//! Error taxonomy. Only the cross-layer variants live here; each crate has its own Error.
//!
//! # Two orthogonal dimensions
//!
//! | Dimension | Type | Answers |
//! | --- | --- | --- |
//! | Subsystem | the [`Error`] variants | **which layer** the error happened in |
//! | Recoverability | [`Recoverability`] | **what to do** once you hold the error |
//!
//! Recoverability is a **projection derived** from the subsystem and kind, not a stored field, so
//! the two can never disagree. That is also why the two dimensions are not two parallel enums.
//!
//! # Relationship to each crate's own errors
//!
//! `ra-model`, `ra-exec`, `ra-session` and others have their own, more detailed error types, which
//! converge into this module's [`Error`] when they cross a crate boundary. A variant here
//! therefore only needs to carry **what is required to decide recoverability and to produce a user
//! message**; it does not reproduce every detail of the layer below, which travels in `source`.
//!
//! # Extension safety
//!
//! Every public enum is `#[non_exhaustive]` (extension-safety rule 1): adding a variant does not
//! break downstream code. Struct variants carry the same attribute, so outside code can only
//! construct them through this module's constructors (extension-safety rule 2).

use core::fmt;
use std::error::Error as StdError;

/// Boxed underlying error source.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// The framework's common result type.
pub type Result<T, E = Error> = core::result::Result<T, E>;

// ---------------------------------------------------------------------------
// dimension two: recoverability
// ---------------------------------------------------------------------------

/// What to do once you hold the error. Orthogonal to the subsystem dimension of [`Error`].
///
/// This is the control-flow criterion: the retry policy, model fallback, and the error handler
/// all read this projection only and never parse error text.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recoverability {
    /// Retrying unchanged may succeed: network jitter, 429, 5xx, a transient timeout.
    Retryable,
    /// Retrying unchanged is useless, but **a different model, parameters, or strategy** may
    /// succeed: a refusal, output that violates the protocol, an exceeded context window. Model
    /// fallback applies to this tier only.
    RetryableWithChange,
    /// A human has to step in: invalid credentials, missing configuration, an unavailable
    /// sandbox, a denied permission.
    NeedsIntervention,
    /// Retrying never helps: the caller misused the API, or the state is unrecoverable.
    Fatal,
    /// A deliberate cancellation. **Not a failure**: it does not count toward the failure rate
    /// and triggers no retry.
    Cancelled,
}

impl Recoverability {
    /// Whether the same request may be retried unchanged.
    ///
    /// Note this is **not** the same as "worth another attempt": [`Self::RetryableWithChange`] is
    /// also worth retrying, but only after some input changes — otherwise it just burns money
    /// twice.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Retryable)
    }

    /// Whether it is worth retrying after changing an input (this includes retrying unchanged).
    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(self, Self::Retryable | Self::RetryableWithChange)
    }

    /// Whether this counts as a real failure. Cancellation does not.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        !matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for Recoverability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Retryable => "retryable",
            Self::RetryableWithChange => "retryable_with_change",
            Self::NeedsIntervention => "needs_intervention",
            Self::Fatal => "fatal",
            Self::Cancelled => "cancelled",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// per-subsystem failure reasons
// ---------------------------------------------------------------------------

/// Why a provider call failed. Determines [`Recoverability`] and the retry policy.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    /// Network unreachable, connection reset, DNS failure.
    Network,
    /// Rate limited (HTTP 429).
    RateLimit,
    /// The request timed out.
    Timeout,
    /// Server error (HTTP 5xx).
    ServerError,
    /// Invalid credentials or insufficient permission (HTTP 401 / 403).
    Auth,
    /// Malformed request (HTTP 400). Usually a local construction bug, so retrying is useless.
    BadRequest,
    /// The model refused. Triggers model fallback.
    Refusal,
    /// Model output violates the protocol: invalid JSON tool arguments, structured output that
    /// does not match the schema.
    Behavior,
    /// Input exceeds the model context window. Compact first, then retry.
    ContextOverflow,
}

/// Why a tool execution failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolErrorKind {
    /// The model called a tool that does not exist, or `ToolOrigin` resolved to no implementation.
    NotFound,
    /// Arguments do not match the schema.
    InvalidInput,
    /// A single tool timed out.
    Timeout,
    /// The tool itself failed while executing.
    ExecutionFailed,
    /// The tool was cancelled (including a cancellation on the MCP side).
    Cancelled,
}

/// Why sandboxing or isolation failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxErrorKind {
    /// An out-of-bounds operation was denied: a write outside the workspace, access to a
    /// sensitive path, a network policy block.
    Denied,
    /// The backend is unavailable: no `bwrap`, no `sandbox-exec`, or the docker daemon is not
    /// running.
    Unavailable,
    /// A resource ceiling was hit: memory, process count, output size.
    ResourceLimit,
    /// Preparing the sandbox environment failed.
    Setup,
}

/// Why session persistence failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionErrorKind {
    /// The target session does not exist.
    NotFound,
    /// A corrupt record, an unparsable JSONL line, or a `call_id` that pairs with nothing.
    Corrupted,
    /// The underlying read or write failed.
    Io,
    /// The `schema_version` is incompatible and cannot be migrated.
    VersionMismatch,
}

/// Why the control protocol failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolErrorKind {
    /// An unparsable frame, or one beyond `max_buffer_size`.
    Frame,
    /// The transport disconnected.
    Transport,
    /// Handshake failure: incompatible versions or failed capability negotiation.
    Handshake,
    /// An in-flight request timed out without a response.
    Timeout,
}

/// Which budget was exhausted.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetKind {
    /// Reached `max_turns`.
    MaxTurns,
    /// Reached the token budget.
    Tokens,
    /// Reached the spend budget.
    Cost,
    /// Reached the wall-clock deadline.
    WallClock,
}

/// Where a guard fired. Matches the four tripwire classes the guardrail contract defines.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardrailStage {
    /// The input guard, before the first agent runs.
    Input,
    /// The output guard, after the final output.
    Output,
    /// The tool-argument guard.
    ToolInput,
    /// The tool-result guard.
    ToolOutput,
}

// ---------------------------------------------------------------------------
// dimension one: by subsystem
// ---------------------------------------------------------------------------

/// The framework's common error. Variants are split by **subsystem**: which layer it happened in.
///
/// "What to do" is projected by [`Error::recoverability`]. Do not decide it by matching variants
/// or parsing text — that is exactly the text-driven control flow this framework forbids.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration error: missing, invalid, or conflicting sources.
    #[error("配置错误：{message}")]
    #[non_exhaustive]
    Config {
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// The caller misused the API: an invalid argument combination, an illegal state-machine
    /// transition, a contract that was not honored.
    ///
    /// This class **must never be retried**: it is a code defect, not a runtime condition.
    #[error("调用方错误：{message}")]
    #[non_exhaustive]
    Caller {
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// A model provider call failed.
    #[error("provider 错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Provider {
        /// Failure reason, which determines recoverability.
        kind: ProviderErrorKind,
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// A tool execution failed.
    #[error("工具 `{tool}` 失败（{kind:?}）：{message}")]
    #[non_exhaustive]
    Tool {
        /// Failure reason.
        kind: ToolErrorKind,
        /// Qualified tool name, from `ToolOrigin`.
        tool: String,
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// Sandboxing or isolation failed.
    #[error("沙箱错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Sandbox {
        /// Failure reason.
        kind: SandboxErrorKind,
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// Session persistence failed.
    #[error("会话错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Session {
        /// Failure reason.
        kind: SessionErrorKind,
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// The control protocol failed.
    #[error("协议错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Protocol {
        /// Failure reason.
        kind: ProtocolErrorKind,
        /// Developer-facing description.
        message: String,
        /// Underlying error source.
        source: Option<BoxError>,
    },

    /// A budget was exhausted.
    ///
    /// Reaching the loop, it should take the **soft ending** of `NextStep::FinalOutput` rather
    /// than aborting, which is why its recoverability is
    /// [`Recoverability::NeedsIntervention`]: either the user raises the budget or accepts the
    /// current result.
    #[error("预算耗尽（{kind:?}）：{message}")]
    #[non_exhaustive]
    Budget {
        /// Which budget ran out.
        kind: BudgetKind,
        /// Developer-facing description.
        message: String,
    },

    /// A guard tripwire fired.
    #[error("护栏 `{guardrail}` 在 {stage:?} 阶段触发：{message}")]
    #[non_exhaustive]
    Guardrail {
        /// Where it fired.
        stage: GuardrailStage,
        /// Guard identity, from the guard registry.
        guardrail: String,
        /// Developer-facing description.
        message: String,
    },

    /// A deliberate cancellation. **Not a failure.**
    #[error("已取消：{reason}")]
    #[non_exhaustive]
    Cancelled {
        /// Cancellation reason, for the UI and the trace to display.
        reason: String,
    },
}

impl Error {
    // -- constructors (the variants are non_exhaustive, so this is the only way in) --------

    /// Creates a [configuration error](Error::Config).
    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [caller error](Error::Caller).
    #[must_use]
    pub fn caller(message: impl Into<String>) -> Self {
        Self::Caller {
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [provider error](Error::Provider).
    #[must_use]
    pub fn provider(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self::Provider {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [tool error](Error::Tool).
    #[must_use]
    pub fn tool(kind: ToolErrorKind, tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            kind,
            tool: tool.into(),
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [sandbox error](Error::Sandbox).
    #[must_use]
    pub fn sandbox(kind: SandboxErrorKind, message: impl Into<String>) -> Self {
        Self::Sandbox {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [session error](Error::Session).
    #[must_use]
    pub fn session(kind: SessionErrorKind, message: impl Into<String>) -> Self {
        Self::Session {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [protocol error](Error::Protocol).
    #[must_use]
    pub fn protocol(kind: ProtocolErrorKind, message: impl Into<String>) -> Self {
        Self::Protocol {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a [budget-exhausted error](Error::Budget).
    #[must_use]
    pub fn budget(kind: BudgetKind, message: impl Into<String>) -> Self {
        Self::Budget {
            kind,
            message: message.into(),
        }
    }

    /// Creates a [guard tripwire error](Error::Guardrail).
    #[must_use]
    pub fn guardrail(
        stage: GuardrailStage,
        guardrail: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Guardrail {
            stage,
            guardrail: guardrail.into(),
            message: message.into(),
        }
    }

    /// Creates a [cancellation](Error::Cancelled).
    #[must_use]
    pub fn cancelled(reason: impl Into<String>) -> Self {
        Self::Cancelled {
            reason: reason.into(),
        }
    }

    /// Attaches an underlying error source. Calling this on a variant that carries no source
    /// (`Budget`, `Guardrail`, `Cancelled`) is a no-op.
    #[must_use]
    pub fn with_source(mut self, src: impl Into<BoxError>) -> Self {
        match &mut self {
            Self::Config { source, .. }
            | Self::Caller { source, .. }
            | Self::Provider { source, .. }
            | Self::Tool { source, .. }
            | Self::Sandbox { source, .. }
            | Self::Session { source, .. }
            | Self::Protocol { source, .. } => *source = Some(src.into()),
            Self::Budget { .. } | Self::Guardrail { .. } | Self::Cancelled { .. } => {}
        }
        self
    }

    // -- projection: the second dimension ----------------------------------

    /// What to do once you hold this error.
    ///
    /// **This is the only control-flow criterion**: retry, model fallback, and the error handler
    /// all read it alone, matching no variant and parsing no text.
    // This match is a **decision table**: each variant's mapping is an independent decision with
    // its own stated reason. Two arms landing on the same tier today does not make them the same
    // decision — merging them would leave the reasoning with nowhere to live and would hide the
    // possibility that they diverge later. They are deliberately kept apart.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn recoverability(&self) -> Recoverability {
        match self {
            Self::Cancelled { .. } => Recoverability::Cancelled,

            // A caller defect: retrying never helps.
            Self::Caller { .. } => Recoverability::Fatal,

            // A configuration problem always needs a human to change the configuration.
            Self::Config { .. } => Recoverability::NeedsIntervention,

            // Budget exhausted: the user either raises it or accepts the current result.
            Self::Budget { .. } => Recoverability::NeedsIntervention,

            // A guard firing is a deliberate block, not a retryable fault.
            Self::Guardrail { .. } => Recoverability::Fatal,

            Self::Provider { kind, .. } => match kind {
                ProviderErrorKind::Network
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::ServerError => Recoverability::Retryable,
                // Refusal -> change model; protocol violation -> change prompt; context
                // overflow -> compact first.
                ProviderErrorKind::Refusal
                | ProviderErrorKind::Behavior
                | ProviderErrorKind::ContextOverflow => Recoverability::RetryableWithChange,
                ProviderErrorKind::Auth => Recoverability::NeedsIntervention,
                ProviderErrorKind::BadRequest => Recoverability::Fatal,
            },

            Self::Tool { kind, .. } => match kind {
                ToolErrorKind::Timeout => Recoverability::Retryable,
                // Different arguments or a different tool may succeed.
                ToolErrorKind::InvalidInput | ToolErrorKind::ExecutionFailed => {
                    Recoverability::RetryableWithChange
                }
                ToolErrorKind::NotFound => Recoverability::Fatal,
                ToolErrorKind::Cancelled => Recoverability::Cancelled,
            },

            Self::Sandbox { kind, .. } => match kind {
                SandboxErrorKind::ResourceLimit => Recoverability::RetryableWithChange,
                SandboxErrorKind::Denied
                | SandboxErrorKind::Unavailable
                | SandboxErrorKind::Setup => Recoverability::NeedsIntervention,
            },

            Self::Session { kind, .. } => match kind {
                SessionErrorKind::Io => Recoverability::Retryable,
                SessionErrorKind::NotFound
                | SessionErrorKind::Corrupted
                | SessionErrorKind::VersionMismatch => Recoverability::NeedsIntervention,
            },

            Self::Protocol { kind, .. } => match kind {
                ProtocolErrorKind::Transport | ProtocolErrorKind::Timeout => {
                    Recoverability::Retryable
                }
                ProtocolErrorKind::Frame => Recoverability::Fatal,
                ProtocolErrorKind::Handshake => Recoverability::NeedsIntervention,
            },
        }
    }

    /// Whether it may be retried unchanged. Equivalent to `self.recoverability().is_retryable()`.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.recoverability().is_retryable()
    }

    /// Whether this is a deliberate cancellation rather than a failure.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.recoverability(), Recoverability::Cancelled)
    }

    /// Stable machine-readable identity, shaped like `provider.rate_limit` or `tool.timeout`.
    ///
    /// Used for trace labels, eval attribution, and metric aggregation. **It is part of the
    /// contract**, so changing one is a breaking change (stability grade `Evolving`).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Config { .. } => "config",
            Self::Caller { .. } => "caller",
            Self::Cancelled { .. } => "cancelled",
            Self::Budget { kind, .. } => match kind {
                BudgetKind::MaxTurns => "budget.max_turns",
                BudgetKind::Tokens => "budget.tokens",
                BudgetKind::Cost => "budget.cost",
                BudgetKind::WallClock => "budget.wall_clock",
            },
            Self::Guardrail { stage, .. } => match stage {
                GuardrailStage::Input => "guardrail.input",
                GuardrailStage::Output => "guardrail.output",
                GuardrailStage::ToolInput => "guardrail.tool_input",
                GuardrailStage::ToolOutput => "guardrail.tool_output",
            },
            Self::Provider { kind, .. } => match kind {
                ProviderErrorKind::Network => "provider.network",
                ProviderErrorKind::RateLimit => "provider.rate_limit",
                ProviderErrorKind::Timeout => "provider.timeout",
                ProviderErrorKind::ServerError => "provider.server_error",
                ProviderErrorKind::Auth => "provider.auth",
                ProviderErrorKind::BadRequest => "provider.bad_request",
                ProviderErrorKind::Refusal => "provider.refusal",
                ProviderErrorKind::Behavior => "provider.behavior",
                ProviderErrorKind::ContextOverflow => "provider.context_overflow",
            },
            Self::Tool { kind, .. } => match kind {
                ToolErrorKind::NotFound => "tool.not_found",
                ToolErrorKind::InvalidInput => "tool.invalid_input",
                ToolErrorKind::Timeout => "tool.timeout",
                ToolErrorKind::ExecutionFailed => "tool.execution_failed",
                ToolErrorKind::Cancelled => "tool.cancelled",
            },
            Self::Sandbox { kind, .. } => match kind {
                SandboxErrorKind::Denied => "sandbox.denied",
                SandboxErrorKind::Unavailable => "sandbox.unavailable",
                SandboxErrorKind::ResourceLimit => "sandbox.resource_limit",
                SandboxErrorKind::Setup => "sandbox.setup",
            },
            Self::Session { kind, .. } => match kind {
                SessionErrorKind::NotFound => "session.not_found",
                SessionErrorKind::Corrupted => "session.corrupted",
                SessionErrorKind::Io => "session.io",
                SessionErrorKind::VersionMismatch => "session.version_mismatch",
            },
            Self::Protocol { kind, .. } => match kind {
                ProtocolErrorKind::Frame => "protocol.frame",
                ProtocolErrorKind::Transport => "protocol.transport",
                ProtocolErrorKind::Handshake => "protocol.handshake",
                ProtocolErrorKind::Timeout => "protocol.timeout",
            },
        }
    }

    /// The **user**-facing message: what happened, and what can be done next.
    ///
    /// How it differs from `Display`: `Display` targets developers and logs and keeps internal
    /// terminology, while this method targets the UI, exposes no internal detail, and always
    /// offers an actionable next step.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Config { message, .. } => {
                format!("配置有问题：{message}。请检查配置文件与环境变量。")
            }
            Self::Caller { .. } => "内部错误：调用方式不正确。这是程序缺陷，请反馈。".to_owned(),
            Self::Cancelled { reason } => format!("已取消：{reason}"),

            Self::Budget { kind, .. } => match kind {
                BudgetKind::MaxTurns => {
                    "已达到最大轮次上限，任务未完成。可以提高上限后继续。".to_owned()
                }
                BudgetKind::Tokens => "已达到 token 预算上限。可以提高预算后继续。".to_owned(),
                BudgetKind::Cost => "已达到费用上限。可以提高预算后继续。".to_owned(),
                BudgetKind::WallClock => "已超过时间上限，任务未完成。".to_owned(),
            },

            Self::Guardrail { guardrail, .. } => {
                format!("操作被安全护栏 `{guardrail}` 拦截。如确需执行，请调整权限配置。")
            }

            Self::Provider { kind, .. } => match kind {
                ProviderErrorKind::Network => "无法连接到模型服务，请检查网络。".to_owned(),
                ProviderErrorKind::RateLimit => "模型服务限流，稍后会自动重试。".to_owned(),
                ProviderErrorKind::Timeout => "模型服务响应超时，稍后会自动重试。".to_owned(),
                ProviderErrorKind::ServerError => "模型服务暂时不可用，稍后会自动重试。".to_owned(),
                ProviderErrorKind::Auth => "模型服务认证失败，请检查 API key 是否有效。".to_owned(),
                ProviderErrorKind::BadRequest => {
                    "请求被模型服务拒绝。这通常是程序缺陷，请反馈。".to_owned()
                }
                ProviderErrorKind::Refusal => "模型拒绝回答，正在尝试其它模型。".to_owned(),
                ProviderErrorKind::Behavior => "模型输出格式不正确，正在重试。".to_owned(),
                ProviderErrorKind::ContextOverflow => {
                    "上下文超出模型窗口，正在压缩后重试。".to_owned()
                }
            },

            Self::Tool { kind, tool, .. } => match kind {
                ToolErrorKind::NotFound => {
                    format!("工具 `{tool}` 不存在。这通常是程序缺陷，请反馈。")
                }
                ToolErrorKind::InvalidInput => format!("工具 `{tool}` 的参数不正确，正在重试。"),
                ToolErrorKind::Timeout => format!("工具 `{tool}` 执行超时。"),
                ToolErrorKind::ExecutionFailed => format!("工具 `{tool}` 执行失败。"),
                ToolErrorKind::Cancelled => format!("工具 `{tool}` 已取消。"),
            },

            Self::Sandbox { kind, .. } => match kind {
                SandboxErrorKind::Denied => "操作超出了沙箱允许的范围，已被拒绝。".to_owned(),
                SandboxErrorKind::Unavailable => {
                    "沙箱后端不可用，请运行 `ra doctor sandbox` 自检。".to_owned()
                }
                SandboxErrorKind::ResourceLimit => {
                    "操作触及资源上限（内存 / 进程数 / 输出体量）。".to_owned()
                }
                SandboxErrorKind::Setup => {
                    "沙箱环境准备失败，请运行 `ra doctor sandbox` 自检。".to_owned()
                }
            },

            Self::Session { kind, .. } => match kind {
                SessionErrorKind::NotFound => "找不到指定的会话。".to_owned(),
                SessionErrorKind::Corrupted => "会话记录已损坏，无法恢复。".to_owned(),
                SessionErrorKind::Io => "读写会话记录失败，稍后会自动重试。".to_owned(),
                SessionErrorKind::VersionMismatch => {
                    "会话记录来自不兼容的版本，无法加载。".to_owned()
                }
            },

            Self::Protocol { kind, .. } => match kind {
                ProtocolErrorKind::Frame => {
                    "与宿主的通信数据不合法。这通常是程序缺陷，请反馈。".to_owned()
                }
                ProtocolErrorKind::Transport => "与宿主的连接已断开，正在重连。".to_owned(),
                ProtocolErrorKind::Handshake => "与宿主的版本不兼容，请升级后重试。".to_owned(),
                ProtocolErrorKind::Timeout => "宿主响应超时。".to_owned(),
            },
        }
    }
}
