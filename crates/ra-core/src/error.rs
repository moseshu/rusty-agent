//! 错误分类学。跨层通用变体只放这里；各 crate 有自己的 Error。
//!
//! # 两个正交维度
//!
//! | 维度 | 类型 | 回答 |
//! | --- | --- | --- |
//! | 子系统 | [`Error`] 的变体 | 错误**发生在哪一层** |
//! | 可恢复性 | [`Recoverability`] | 拿到错误之后**该怎么办** |
//!
//! 可恢复性是从子系统与 kind **推导出来的投影**，不是存储的字段——因此两者
//! 不可能出现不一致。这也是不把两个维度做成两个并列枚举的原因。
//!
//! # 与各 crate 自有错误的关系
//!
//! `ra-model` / `ra-exec` / `ra-session` 等有各自更详细的错误类型，它们在跨越
//! crate 边界时收敛成本模块的 [`Error`]。因此这里的变体只需携带**判定可恢复性
//! 与生成用户消息所必需的信息**，不复刻下层的全部细节；细节走 `source`。
//!
//! # 扩展安全
//!
//! 所有对外枚举都是 `#[non_exhaustive]`（扩展安全第 1 条）：新增变体不破坏下游。
//! 结构体变体同样标注，因此外部只能通过本模块的构造函数创建（扩展安全第 2 条）。

use core::fmt;
use std::error::Error as StdError;

/// 装箱的底层错误源。
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// 框架通用结果类型。
pub type Result<T, E = Error> = core::result::Result<T, E>;

// ---------------------------------------------------------------------------
// 维度二：可恢复性
// ---------------------------------------------------------------------------

/// 拿到错误之后该怎么办。与 [`Error`] 的子系统维度正交。
///
/// 这是控制流的判据：重试策略（R1-9b）、模型回退（R1-12）、错误处理器
/// （R3-8）都只读这个投影，不解析错误文本。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recoverability {
    /// 原样重试可能成功：网络抖动、429、5xx、瞬时超时。
    Retryable,
    /// 原样重试无用，**换模型 / 参数 / 策略**可能成功：模型拒答、输出不合协议、
    /// 上下文超限。R1-12 的模型回退只对这一档生效。
    RetryableWithChange,
    /// 需要人介入：凭据无效、配置缺失、沙箱不可用、权限被拒。
    NeedsIntervention,
    /// 重试永远无用：调用方用错 API、状态已不可恢复。
    Fatal,
    /// 主动取消。**不是失败**，不计入失败率，不触发重试。
    Cancelled,
}

impl Recoverability {
    /// 是否可以原样重试同一个请求。
    ///
    /// 注意这**不等于**「值得再试一次」：[`Self::RetryableWithChange`] 也值得再试，
    /// 但必须先改变某个输入，否则只是重复烧钱。
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Retryable)
    }

    /// 是否值得在改变输入后再试（含原样重试）。
    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(self, Self::Retryable | Self::RetryableWithChange)
    }

    /// 是否算一次真正的失败。取消不算。
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
// 各子系统的失败原因
// ---------------------------------------------------------------------------

/// provider 调用的失败原因。决定 [`Recoverability`] 与重试策略。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    /// 网络不可达、连接重置、DNS 失败。
    Network,
    /// 触发限流（HTTP 429）。
    RateLimit,
    /// 请求超时。
    Timeout,
    /// 服务端错误（HTTP 5xx）。
    ServerError,
    /// 凭据无效或权限不足（HTTP 401 / 403）。
    Auth,
    /// 请求不合法（HTTP 400）。通常是本地构造错误，重试无用。
    BadRequest,
    /// 模型拒答。触发 R1-12 的模型回退。
    Refusal,
    /// 模型输出不符合协议：工具调用参数非法 JSON、结构化输出不匹配 schema。
    Behavior,
    /// 输入超出模型上下文窗口。需要先压缩再重试（R5）。
    ContextOverflow,
}

/// 工具执行的失败原因。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolErrorKind {
    /// 模型调用了不存在的工具，或 `ToolOrigin` 反查不到实现。
    NotFound,
    /// 入参不合 schema。
    InvalidInput,
    /// 单工具超时（R2-7）。
    Timeout,
    /// 工具自身执行失败。
    ExecutionFailed,
    /// 工具被取消（含 MCP 侧取消）。
    Cancelled,
}

/// 沙箱与隔离的失败原因。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxErrorKind {
    /// 操作越界被拒：写工作区之外、访问敏感路径、网络策略拦截。
    Denied,
    /// 后端不可用：缺 `bwrap` / `sandbox-exec` / docker daemon 未启动。
    Unavailable,
    /// 触及资源上限：内存、进程数、输出体量。
    ResourceLimit,
    /// 沙箱环境准备失败。
    Setup,
}

/// 会话持久化的失败原因。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionErrorKind {
    /// 目标会话不存在。
    NotFound,
    /// 记录损坏、JSONL 行不可解析、`call_id` 配不上对。
    Corrupted,
    /// 底层读写失败。
    Io,
    /// `schema_version` 不兼容且无法迁移（R6-6）。
    VersionMismatch,
}

/// 控制协议的失败原因。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolErrorKind {
    /// 帧不可解析，或超出 `max_buffer_size`。
    Frame,
    /// 传输层断开。
    Transport,
    /// 握手失败：版本不兼容、能力协商失败。
    Handshake,
    /// 在途请求超时未收到响应。
    Timeout,
}

/// 预算耗尽的种类。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetKind {
    /// 达到 `max_turns`。
    MaxTurns,
    /// 达到 token 预算。
    Tokens,
    /// 达到费用预算。
    Cost,
    /// 达到墙钟 deadline。
    WallClock,
}

/// 护栏触发的位置。对应 R7-1 / R7-3 的四类 tripwire。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardrailStage {
    /// 首个 agent 运行前的输入护栏。
    Input,
    /// 最终输出后的输出护栏。
    Output,
    /// 工具入参护栏。
    ToolInput,
    /// 工具结果护栏。
    ToolOutput,
}

// ---------------------------------------------------------------------------
// 维度一：按子系统
// ---------------------------------------------------------------------------

/// 框架通用错误。变体按**子系统**划分，即错误发生在哪一层。
///
/// 「该怎么办」由 [`Error::recoverability`] 投影得到，不要靠匹配变体或解析
/// 文本来判断——那正是 R7-10 去词表化禁止的做法。
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 配置错误：缺失、非法、来源冲突。
    #[error("配置错误：{message}")]
    #[non_exhaustive]
    Config {
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 调用方用错 API：参数组合非法、状态机被违规驱动、契约未被遵守。
    ///
    /// 这类错误**永远不该重试**——它是代码缺陷，不是运行时状况。
    #[error("调用方错误：{message}")]
    #[non_exhaustive]
    Caller {
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 模型 provider 调用失败。
    #[error("provider 错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Provider {
        /// 失败原因，决定可恢复性。
        kind: ProviderErrorKind,
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 工具执行失败。
    #[error("工具 `{tool}` 失败（{kind:?}）：{message}")]
    #[non_exhaustive]
    Tool {
        /// 失败原因。
        kind: ToolErrorKind,
        /// 工具的限定名，来自 `ToolOrigin`。
        tool: String,
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 沙箱与隔离失败。
    #[error("沙箱错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Sandbox {
        /// 失败原因。
        kind: SandboxErrorKind,
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 会话持久化失败。
    #[error("会话错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Session {
        /// 失败原因。
        kind: SessionErrorKind,
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 控制协议失败。
    #[error("协议错误（{kind:?}）：{message}")]
    #[non_exhaustive]
    Protocol {
        /// 失败原因。
        kind: ProtocolErrorKind,
        /// 面向开发者的描述。
        message: String,
        /// 底层错误源。
        source: Option<BoxError>,
    },

    /// 预算耗尽。
    ///
    /// 它进入 loop 时应走 `NextStep::FinalOutput` 的**软结束**而非中止
    /// （R3-8），因此可恢复性是 [`Recoverability::NeedsIntervention`]：
    /// 要么用户提高预算，要么接受当前结果。
    #[error("预算耗尽（{kind:?}）：{message}")]
    #[non_exhaustive]
    Budget {
        /// 耗尽的是哪一项预算。
        kind: BudgetKind,
        /// 面向开发者的描述。
        message: String,
    },

    /// 护栏 tripwire 触发。
    #[error("护栏 `{guardrail}` 在 {stage:?} 阶段触发：{message}")]
    #[non_exhaustive]
    Guardrail {
        /// 触发位置。
        stage: GuardrailStage,
        /// 护栏标识，来自 guard 登记表（R7-0）。
        guardrail: String,
        /// 面向开发者的描述。
        message: String,
    },

    /// 主动取消。**不是失败**。
    #[error("已取消：{reason}")]
    #[non_exhaustive]
    Cancelled {
        /// 取消原因，供 UI 与 trace 显示。
        reason: String,
    },
}

impl Error {
    // -- 构造函数（变体是 non_exhaustive，外部只能走这里） -------------------

    /// 构造[配置错误](Error::Config)。
    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
            source: None,
        }
    }

    /// 构造[调用方错误](Error::Caller)。
    #[must_use]
    pub fn caller(message: impl Into<String>) -> Self {
        Self::Caller {
            message: message.into(),
            source: None,
        }
    }

    /// 构造 [provider 错误](Error::Provider)。
    #[must_use]
    pub fn provider(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self::Provider {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// 构造[工具错误](Error::Tool)。
    #[must_use]
    pub fn tool(kind: ToolErrorKind, tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            kind,
            tool: tool.into(),
            message: message.into(),
            source: None,
        }
    }

    /// 构造[沙箱错误](Error::Sandbox)。
    #[must_use]
    pub fn sandbox(kind: SandboxErrorKind, message: impl Into<String>) -> Self {
        Self::Sandbox {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// 构造[会话错误](Error::Session)。
    #[must_use]
    pub fn session(kind: SessionErrorKind, message: impl Into<String>) -> Self {
        Self::Session {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// 构造[协议错误](Error::Protocol)。
    #[must_use]
    pub fn protocol(kind: ProtocolErrorKind, message: impl Into<String>) -> Self {
        Self::Protocol {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// 构造[预算耗尽](Error::Budget)。
    #[must_use]
    pub fn budget(kind: BudgetKind, message: impl Into<String>) -> Self {
        Self::Budget {
            kind,
            message: message.into(),
        }
    }

    /// 构造[护栏触发](Error::Guardrail)。
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

    /// 构造[取消](Error::Cancelled)。
    #[must_use]
    pub fn cancelled(reason: impl Into<String>) -> Self {
        Self::Cancelled {
            reason: reason.into(),
        }
    }

    /// 附加底层错误源。在不携带 source 的变体（`Budget` / `Guardrail` /
    /// `Cancelled`）上调用是无操作。
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

    // -- 投影：第二个维度 ---------------------------------------------------

    /// 拿到这个错误之后该怎么办。
    ///
    /// **这是唯一的控制流判据**——重试（R1-9b）、模型回退（R1-12）、错误处理器
    /// （R3-8）都只读它，不匹配变体、不解析文本。
    // 这个 match 是一张**决策表**：每个变体的映射是一条独立的、单独说明理由的
    // 决定。两条今天恰好落在同一档，不代表它们是同一个决定——合并分支会让理由
    // 注释无处安放，也会掩盖将来分化的可能。因此刻意不合并。
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn recoverability(&self) -> Recoverability {
        match self {
            Self::Cancelled { .. } => Recoverability::Cancelled,

            // 调用方缺陷：重试永远无用。
            Self::Caller { .. } => Recoverability::Fatal,

            // 配置问题一律需要人改配置。
            Self::Config { .. } => Recoverability::NeedsIntervention,

            // 预算耗尽：要么用户提额，要么接受当前结果。
            Self::Budget { .. } => Recoverability::NeedsIntervention,

            // 护栏触发是刻意拦截，不是可重试的故障。
            Self::Guardrail { .. } => Recoverability::Fatal,

            Self::Provider { kind, .. } => match kind {
                ProviderErrorKind::Network
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::ServerError => Recoverability::Retryable,
                // 拒答换模型、输出不合协议换提示、上下文超限先压缩。
                ProviderErrorKind::Refusal
                | ProviderErrorKind::Behavior
                | ProviderErrorKind::ContextOverflow => Recoverability::RetryableWithChange,
                ProviderErrorKind::Auth => Recoverability::NeedsIntervention,
                ProviderErrorKind::BadRequest => Recoverability::Fatal,
            },

            Self::Tool { kind, .. } => match kind {
                ToolErrorKind::Timeout => Recoverability::Retryable,
                // 换参数或换工具可能成功。
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

    /// 是否可以原样重试。等价于 `self.recoverability().is_retryable()`。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.recoverability().is_retryable()
    }

    /// 是否是主动取消而非失败。
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.recoverability(), Recoverability::Cancelled)
    }

    /// 稳定的机器可读标识，形如 `provider.rate_limit`、`tool.timeout`。
    ///
    /// 用于 trace 标签、eval 归因与指标聚合。**它是契约的一部分**，改动等同于
    /// 破坏性变更（稳定性分级 `Evolving`）。
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

    /// 面向**用户**的消息：说清发生了什么、下一步能做什么。
    ///
    /// 与 `Display` 的区别：`Display` 面向开发者与日志，保留内部术语；本方法
    /// 面向 UI，不暴露内部细节，且总是给出可操作的下一步。
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
