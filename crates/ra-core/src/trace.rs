//! span 分类与字段词表。所有 crate 打日志时共用这一套名字。
//!
//! # 三条边界
//!
//! 1. **这里只定词表**：span 名、字段名、级别约定。不实现任何 subscriber，更不
//!    接任何外部上报后端——装配 subscriber 是 `ra-cli` 的事，接不接后端是用户
//!    的事。
//! 2. **tracing 通道面向开发者与 eval，不是 UI 事件流。** 用户可见的事件走 R9
//!    rollout 的 `event_msg` 双通道，两者不可互相替代：日志可以随时调级别、可以
//!    丢，事件流不能。
//! 3. **span 字段只放标识与计数，不放内容。** 模型输入输出、工具入参、文件内容
//!    一律不进 span——它们体量大、含敏感数据，且已经在 rollout 通道里有权威副本。
//!    这条让「关掉敏感数据」不至于把 span 拓扑一起关掉（R14-2）。
//!
//! # 为什么要有词表
//!
//! 字段名是**契约**：eval 归因（R14-2）、成本报表、指标聚合都按名字取值。散落在
//! 各 crate 里手写字符串，迟早出现 `tool.name` 与 `tool_name` 并存，聚合出来的
//! 数字就是错的。所以名字在这里定一次，[`field::ALL`] 是全集。
//!
//! # 级别约定
//!
//! span 的级别由 [`SpanKind::level`] 定，错误事件的级别由 [`level_for`] 从可恢复性
//! 投影。其余事件按下表，判据是**默认级别下的日志量应当与任务规模成正比，而不是
//! 与 token 数成正比**：
//!
//! | 级别 | 放什么 | 量级 |
//! | --- | --- | --- |
//! | ERROR | 不干预就过不去的事 | 一次 run 零到一条 |
//! | WARN | 框架自己能处理但值得知道：重试、模型回退、drain 超时强杀、护栏拦截 | 每次发生一条 |
//! | INFO | run 的骨架：agent / turn / generation / function / handoff | 每 turn 个位数 |
//! | DEBUG | 逐工具、逐请求的细节；guardrail 与 `mcp_list_tools` | 每 turn 几十条 |
//! | TRACE | 逐 SSE 事件、逐 chunk、逐 token | 不设上限 |
//!
//! # 与 `tracing` 宏的配合
//!
//! `tracing` 的宏要求字段在**创建 span 时**就声明，之后才能 `record`。因此终态
//! 类字段（[`field::OUTCOME`] / [`field::ERROR_CODE`] / usage 那几项）必须在
//! callsite 用 [`tracing::field::Empty`] 占位：
//!
//! ```ignore
//! let span = tracing::info_span!(
//!     "generation",
//!     span.kind = SpanKind::Generation.label(),
//!     model.name = %model,
//!     outcome = tracing::field::Empty,         // ← 不占位，record 就是静默失败
//!     usage.cached_input_tokens = tracing::field::Empty,
//! );
//! ```
//!
//! 宏的字段名只能写成点分标识符的字面形式，**没法把常量插进去**；常量服务于
//! [`record_outcome`] 这类 helper 与 eval 侧断言。两边靠
//! `tests/it-core/tests/span_taxonomy.rs` 的最后一条断言对齐。

use core::fmt;
use std::borrow::Cow;

use tracing::{Level, Span};

use crate::cancel::{CancelReason, ScopeKind};
use crate::error::{Error, Recoverability};

/// 字段名词表。**改名等同破坏性变更**——下游的报表与断言都按名字取值。
pub mod field {
    // -- 每个 span 都有 ----------------------------------------------------

    /// span 分类，取值为 [`super::SpanKind::label`]。
    ///
    /// 与 span 名的区别见 [`super::SpanKind::span_name`]：**归因以本字段为准**。
    pub const SPAN_KIND: &str = "span.kind";
    /// 自定义分类的标签，仅 [`super::SpanKind::Custom`] 有。
    pub const SPAN_LABEL: &str = "span.label";
    /// 终态，取值为 [`super::SpanOutcome::as_str`]。span 关闭前 record。
    pub const OUTCOME: &str = "outcome";

    // -- 失败与取消 --------------------------------------------------------

    /// 错误的机器可读标识，取值为 `Error::code()`。**不记错误文本**。
    pub const ERROR_CODE: &str = "error.code";
    /// 取消根因，取值为 `CancelReason::code()`。
    pub const CANCEL_REASON: &str = "cancel.reason";
    /// 发起取消的作用域层级，取值为 `ScopeKind::label()`。
    pub const CANCEL_SCOPE: &str = "cancel.scope";

    // -- agent -------------------------------------------------------------

    /// agent 名。
    pub const AGENT_NAME: &str = "agent.name";

    // -- turn --------------------------------------------------------------

    /// turn 序号，从 0 起。
    pub const TURN_INDEX: &str = "turn.index";

    // -- generation --------------------------------------------------------

    /// 模型名（最终解析出来的那个，不是用户写的别名）。
    pub const MODEL_NAME: &str = "model.name";
    /// provider 标识。
    pub const MODEL_PROVIDER: &str = "model.provider";
    /// 协议路径：`responses` / `chat` / `messages` / `compat`（R1）。
    pub const GEN_PROTOCOL: &str = "gen.protocol";
    /// 本次请求的输入 token 数。
    pub const USAGE_INPUT_TOKENS: &str = "usage.input_tokens";
    /// 其中命中缓存的部分。
    ///
    /// **这一项单列不是为了好看**：缓存命中率是成本主因，把它折进
    /// [`USAGE_INPUT_TOKENS`] 就再也算不出命中率，也就看不见成本问题。
    pub const USAGE_CACHED_INPUT_TOKENS: &str = "usage.cached_input_tokens";
    /// 输出 token 数。
    pub const USAGE_OUTPUT_TOKENS: &str = "usage.output_tokens";
    /// 其中的推理 token（若 provider 单列）。
    pub const USAGE_REASONING_TOKENS: &str = "usage.reasoning_tokens";

    // -- function（工具调用）-----------------------------------------------

    /// 工具的限定名，来自 `ToolOrigin`。
    pub const TOOL_NAME: &str = "tool.name";
    /// 模型侧的调用 id，用于和结果配对（不靠顺序配对）。
    pub const TOOL_CALL_ID: &str = "tool.call_id";

    // -- handoff -----------------------------------------------------------

    /// 交接的来源 agent。
    pub const HANDOFF_FROM: &str = "handoff.from";
    /// 交接的目标 agent。
    pub const HANDOFF_TO: &str = "handoff.to";

    // -- guardrail ---------------------------------------------------------

    /// 护栏标识，来自 guard 登记表（R7-0）。
    pub const GUARDRAIL_ID: &str = "guardrail.id";
    /// 触发位置，取值为 `GuardrailStage` 的小写名。
    pub const GUARDRAIL_STAGE: &str = "guardrail.stage";
    /// 是否真的拦下了（tripwire 是否触发）。
    pub const GUARDRAIL_TRIGGERED: &str = "guardrail.triggered";

    // -- mcp ---------------------------------------------------------------

    /// MCP server 标识。
    pub const MCP_SERVER: &str = "mcp.server";

    /// 全集。eval 与门禁用它校验「字段名必须在词表内」。
    pub const ALL: &[&str] = &[
        SPAN_KIND,
        SPAN_LABEL,
        OUTCOME,
        ERROR_CODE,
        CANCEL_REASON,
        CANCEL_SCOPE,
        AGENT_NAME,
        TURN_INDEX,
        MODEL_NAME,
        MODEL_PROVIDER,
        GEN_PROTOCOL,
        USAGE_INPUT_TOKENS,
        USAGE_CACHED_INPUT_TOKENS,
        USAGE_OUTPUT_TOKENS,
        USAGE_REASONING_TOKENS,
        TOOL_NAME,
        TOOL_CALL_ID,
        HANDOFF_FROM,
        HANDOFF_TO,
        GUARDRAIL_ID,
        GUARDRAIL_STAGE,
        GUARDRAIL_TRIGGERED,
        MCP_SERVER,
    ];
}

// ---------------------------------------------------------------------------
// span 分类
// ---------------------------------------------------------------------------

/// span 分类。对齐 openai `tracing/span_data.py`，去掉语音那三类，加上 `turn`。
///
/// `turn` 是我们自己的：openai 用 `response` 表示一次模型往返，但**一个 turn 可能
/// 包含重试、模型回退和一整批工具**，没有这一层就没法回答「这一轮花了多少钱」。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SpanKind {
    /// 一个 agent 的整段执行。子 agent（R12）各自一个。
    Agent,
    /// 一轮：模型往返 + 其后的工具批次。
    Turn,
    /// 一次模型调用（含重试与回退，各自一个 span）。
    Generation,
    /// 一次工具调用。名字沿用 openai 的 `function`，涵盖所有 `Tool` 实现。
    Function,
    /// 一次 agent 交接。
    Handoff,
    /// 一次护栏检查（R7）。
    Guardrail,
    /// 一次 MCP 工具列表拉取——它是启动延迟与缓存失效的常见源头，值得单列。
    McpListTools,
    /// 扩展点：产品或第三方自定义的分类（扩展安全第 5 条）。
    Custom(Cow<'static, str>),
}

impl SpanKind {
    /// 构造[自定义分类](Self::Custom)。
    #[must_use]
    pub fn custom(label: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(label.into())
    }

    /// 用作 `tracing` span 名。
    ///
    /// `tracing` 要求 span 名是 `&'static str`，自定义标签给不了，因此
    /// [`Self::Custom`] 一律叫 `custom`，真实标签进 [`field::SPAN_LABEL`]。
    /// **归因不要按 span 名分组**，按 [`field::SPAN_KIND`] 分。
    #[must_use]
    pub const fn span_name(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Turn => "turn",
            Self::Generation => "generation",
            Self::Function => "function",
            Self::Handoff => "handoff",
            Self::Guardrail => "guardrail",
            Self::McpListTools => "mcp_list_tools",
            Self::Custom(_) => "custom",
        }
    }

    /// [`field::SPAN_KIND`] 的取值。自定义分类返回它自己的标签。
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Custom(label) => label.as_ref(),
            other => other.span_name(),
        }
    }

    /// 该分类的 span 应该打在哪一级。
    ///
    /// 分档的判据是**一次正常的 run 在 INFO 下应当读得完**：run 骨架（agent /
    /// turn / generation / function / handoff）进 INFO；高频且多数时候无事发生的
    /// （guardrail / `mcp_list_tools`）进 DEBUG——护栏真拦下来的时候另发一条 WARN
    /// 事件，那才是要看的东西。
    #[must_use]
    pub const fn level(&self) -> Level {
        match self {
            Self::Agent
            | Self::Turn
            | Self::Generation
            | Self::Function
            | Self::Handoff
            | Self::Custom(_) => Level::INFO,
            Self::Guardrail | Self::McpListTools => Level::DEBUG,
        }
    }

    /// 创建该分类的 span 时**必须**带上的字段。
    ///
    /// 只列创建时就知道的标识类字段；终态字段（[`field::OUTCOME`]、usage 等）用
    /// [`tracing::field::Empty`] 占位后再 record，不在这里要求。
    #[must_use]
    pub const fn required_fields(&self) -> &'static [&'static str] {
        match self {
            Self::Agent => &[field::SPAN_KIND, field::AGENT_NAME],
            Self::Turn => &[field::SPAN_KIND, field::TURN_INDEX],
            Self::Generation => &[field::SPAN_KIND, field::MODEL_NAME, field::MODEL_PROVIDER],
            Self::Function => &[field::SPAN_KIND, field::TOOL_NAME, field::TOOL_CALL_ID],
            Self::Handoff => &[field::SPAN_KIND, field::HANDOFF_FROM, field::HANDOFF_TO],
            Self::Guardrail => &[
                field::SPAN_KIND,
                field::GUARDRAIL_ID,
                field::GUARDRAIL_STAGE,
            ],
            Self::McpListTools => &[field::SPAN_KIND, field::MCP_SERVER],
            Self::Custom(_) => &[field::SPAN_KIND, field::SPAN_LABEL],
        }
    }
}

impl fmt::Display for SpanKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// 终态
// ---------------------------------------------------------------------------

/// span 的终态。**取消单列一档**，不并进 `Error`——理由同 R0-2：取消不是失败，
/// 混在一起会让失败率指标在用户按停止键时飙升。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpanOutcome {
    /// 正常完成。
    Ok,
    /// 以失败告终。
    Error,
    /// 被取消。
    Cancelled,
}

impl SpanOutcome {
    /// [`field::OUTCOME`] 的取值。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// 是否算一次失败。取消不算。
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Error)
    }
}

impl fmt::Display for SpanOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&Error> for SpanOutcome {
    /// 走 `Error::is_cancelled()` 的投影，不匹配变体、不看文本。
    fn from(err: &Error) -> Self {
        if err.is_cancelled() {
            Self::Cancelled
        } else {
            Self::Error
        }
    }
}

// ---------------------------------------------------------------------------
// 级别投影
// ---------------------------------------------------------------------------

/// 一个错误该打在哪一级。**从可恢复性投影得到**，不由 callsite 自己拍。
///
/// | 可恢复性 | 级别 | 理由 |
/// | --- | --- | --- |
/// | `Retryable` / `RetryableWithChange` | WARN | 框架会自己处理，人不需要动手 |
/// | `NeedsIntervention` / `Fatal` | ERROR | 不干预就过不去 |
/// | `Cancelled` | INFO | 取消是正常终态，刷 ERROR 会淹掉真错误 |
///
/// 让 callsite 自己选级别的后果是可预见的：重试前的每次失败都打 ERROR，日志里
/// 全是红的，真正需要人看的那条反而被淹掉。
#[must_use]
pub const fn level_for(recoverability: Recoverability) -> Level {
    match recoverability {
        Recoverability::Retryable | Recoverability::RetryableWithChange => Level::WARN,
        Recoverability::NeedsIntervention | Recoverability::Fatal => Level::ERROR,
        Recoverability::Cancelled => Level::INFO,
    }
}

// ---------------------------------------------------------------------------
// record helper
// ---------------------------------------------------------------------------

/// 记录终态。
///
/// 字段必须在创建 span 时用 [`tracing::field::Empty`] 占过位，否则 `record` 静默
/// 无效——这是 `tracing` 的语义，不是本函数的疏漏。
pub fn record_outcome(span: &Span, outcome: SpanOutcome) {
    span.record(field::OUTCOME, outcome.as_str());
}

/// 记录一个错误的终态：[`field::ERROR_CODE`] + [`field::OUTCOME`]。
///
/// 只落 `code()`，**不落错误文本**：文本会变、会含路径与密钥，且按文本聚合就是
/// R7-10 禁止的词表式判断。取消会被记成 [`SpanOutcome::Cancelled`] 而不是
/// `Error`。
pub fn record_error(span: &Span, err: &Error) {
    span.record(field::ERROR_CODE, err.code());
    record_outcome(span, SpanOutcome::from(err));
}

/// 记录一次取消：根因、发起层级与终态。
///
/// 这是取消契约（R0-4）里承诺的两个字段的唯一写入口。
pub fn record_cancel(span: &Span, reason: &CancelReason, scope: &ScopeKind) {
    span.record(field::CANCEL_REASON, reason.code());
    span.record(field::CANCEL_SCOPE, scope.label());
    record_outcome(span, SpanOutcome::Cancelled);
}
