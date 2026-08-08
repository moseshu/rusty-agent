//! Span taxonomy and the field vocabulary. Every crate logs through these names.
//!
//! # Three boundaries
//!
//! 1. **This module defines vocabulary only**: span names, field names, level conventions. It
//!    implements no subscriber and wires up no external reporting backend — assembling a
//!    subscriber is `ra-cli`'s job, and choosing a backend is the user's.
//! 2. **The tracing channel serves developers and eval, not the UI event stream.** User-visible
//!    events go through the `event_msg` channel of the R9 rollout. The two are not
//!    interchangeable: logs can change level or be dropped at any time, the event stream cannot.
//! 3. **Span fields carry identifiers and counts, never content.** Model input and output, tool
//!    arguments, and file contents never enter a span: they are large, they contain sensitive
//!    data, and the rollout channel already holds an authoritative copy. This is what keeps
//!    "turn off sensitive data" from also turning off the span topology (R14-2).
//!
//! # Why a vocabulary
//!
//! Field names are a **contract**: eval attribution (R14-2), cost reports, and metric aggregation
//! all look values up by name. Hand-written strings scattered across crates eventually produce
//! both `tool.name` and `tool_name`, and the aggregated numbers are simply wrong. So the names are
//! fixed once here, and [`field::ALL`] is the complete set.
//!
//! # Level conventions
//!
//! A span's level comes from [`SpanKind::level`] and an error event's level is projected from
//! recoverability by [`level_for`]. Everything else follows the table below, whose criterion is
//! that **at the default level the log volume should scale with task size, not with token
//! count**:
//!
//! | Level | Contents | Volume |
//! | --- | --- | --- |
//! | ERROR | Something that cannot proceed without intervention | zero or one per run |
//! | WARN | The framework handles it but you should know: retry, model fallback, drain-timeout kill, guard block | one per occurrence |
//! | INFO | The skeleton of a run: agent / turn / generation / function / handoff | single digits per turn |
//! | DEBUG | Per-tool and per-request detail; guardrail and `mcp_list_tools` | tens per turn |
//! | TRACE | Per SSE event, per chunk, per token | no ceiling |
//!
//! # Working with the `tracing` macros
//!
//! The `tracing` macros require a field to be declared **when the span is created** before it can
//! be `record`ed. Terminal fields ([`field::OUTCOME`], [`field::ERROR_CODE`], the usage entries)
//! therefore have to be reserved at the callsite with [`tracing::field::Empty`]:
//!
//! ```ignore
//! let span = tracing::info_span!(
//!     "generation",
//!     span.kind = SpanKind::Generation.label(),
//!     model.name = %model,
//!     outcome = tracing::field::Empty,         // <- without this, `record` silently does nothing
//!     usage.cached_input_tokens = tracing::field::Empty,
//! );
//! ```
//!
//! A macro field name can only be written as a literal dotted identifier, so **a constant cannot
//! be interpolated there**; the constants serve helpers such as [`record_outcome`] and the
//! assertions on the eval side. The last assertion in `tests/it-core/tests/span_taxonomy.rs` keeps
//! the two in sync.

use core::fmt;
use std::borrow::Cow;

use tracing::{Level, Span};

use crate::cancel::{CancelReason, ScopeKind};
use crate::error::{Error, Recoverability};

/// The field-name vocabulary. **Renaming one is a breaking change** — downstream reports and
/// assertions look values up by name.
pub mod field {
    // -- present on every span ---------------------------------------------

    /// Span kind, valued by [`super::SpanKind::label`].
    ///
    /// See [`super::SpanKind::span_name`] for how this differs from the span name:
    /// **attribution goes by this field**.
    pub const SPAN_KIND: &str = "span.kind";
    /// Label of a custom kind; only [`super::SpanKind::Custom`] carries it.
    pub const SPAN_LABEL: &str = "span.label";
    /// Terminal state, valued by [`super::SpanOutcome::as_str`]. Recorded before the span closes.
    pub const OUTCOME: &str = "outcome";

    // -- failure and cancellation ------------------------------------------

    /// Machine-readable error identity, valued by `Error::code()`. **The error text is not
    /// recorded.**
    pub const ERROR_CODE: &str = "error.code";
    /// Cancellation root cause, valued by `CancelReason::code()`.
    pub const CANCEL_REASON: &str = "cancel.reason";
    /// Scope level that initiated the cancellation, valued by `ScopeKind::label()`.
    pub const CANCEL_SCOPE: &str = "cancel.scope";

    // -- agent -------------------------------------------------------------

    /// Agent name.
    pub const AGENT_NAME: &str = "agent.name";

    // -- turn --------------------------------------------------------------

    /// Turn index, starting at 0.
    pub const TURN_INDEX: &str = "turn.index";

    // -- generation --------------------------------------------------------

    /// Model name: the one finally resolved, not the alias the user wrote.
    pub const MODEL_NAME: &str = "model.name";
    /// Provider identity.
    pub const MODEL_PROVIDER: &str = "model.provider";
    /// Protocol path: `responses` / `chat` / `messages` / `compat` (R1).
    pub const GEN_PROTOCOL: &str = "gen.protocol";
    /// Input tokens for this request.
    pub const USAGE_INPUT_TOKENS: &str = "usage.input_tokens";
    /// The cached portion of those input tokens.
    ///
    /// **This is not broken out for tidiness**: cache hit rate is the dominant cost driver, and
    /// folding it into [`USAGE_INPUT_TOKENS`] makes the hit rate — and therefore the cost problem
    /// — impossible to compute.
    pub const USAGE_CACHED_INPUT_TOKENS: &str = "usage.cached_input_tokens";
    /// Output tokens.
    pub const USAGE_OUTPUT_TOKENS: &str = "usage.output_tokens";
    /// The reasoning tokens among them, when the provider reports them separately.
    pub const USAGE_REASONING_TOKENS: &str = "usage.reasoning_tokens";

    // -- function (tool call) ----------------------------------------------

    /// Qualified tool name, from `ToolOrigin`.
    pub const TOOL_NAME: &str = "tool.name";
    /// Model-side call id, used to pair with the result (never positional pairing).
    pub const TOOL_CALL_ID: &str = "tool.call_id";

    // -- handoff -----------------------------------------------------------

    /// Source agent of the handoff.
    pub const HANDOFF_FROM: &str = "handoff.from";
    /// Target agent of the handoff.
    pub const HANDOFF_TO: &str = "handoff.to";

    // -- guardrail ---------------------------------------------------------

    /// Guard identity, from the guard registry (R7-0).
    pub const GUARDRAIL_ID: &str = "guardrail.id";
    /// Trigger point, valued by the lowercase name of `GuardrailStage`.
    pub const GUARDRAIL_STAGE: &str = "guardrail.stage";
    /// Whether it actually blocked (whether the tripwire fired).
    pub const GUARDRAIL_TRIGGERED: &str = "guardrail.triggered";

    // -- mcp ---------------------------------------------------------------

    /// MCP server identity.
    pub const MCP_SERVER: &str = "mcp.server";

    /// The complete set. Eval and the gates use it to check that every field name is in the
    /// vocabulary.
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
// span taxonomy
// ---------------------------------------------------------------------------

/// Span kind. Aligned with openai `tracing/span_data.py`, dropping the three voice kinds and
/// adding `turn`.
///
/// `turn` is ours: openai uses `response` for one model round trip, but **a single turn can
/// contain retries, a model fallback, and a whole batch of tools**. Without this level there is no
/// way to answer "what did this turn cost".
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SpanKind {
    /// One agent's entire execution. Each sub-agent (R12) gets its own.
    Agent,
    /// One turn: a model round trip plus the tool batch that follows it.
    Turn,
    /// One model call. Retries and fallbacks each get their own span.
    Generation,
    /// One tool call. The name follows openai's `function` and covers every `Tool` implementation.
    Function,
    /// One agent handoff.
    Handoff,
    /// One guard check (R7).
    Guardrail,
    /// One MCP tool-list fetch. It is a common source of startup latency and cache invalidation,
    /// which earns it its own kind.
    McpListTools,
    /// Extension point: a kind defined by a product or a third party (extension-safety rule 5).
    Custom(Cow<'static, str>),
}

impl SpanKind {
    /// Creates a [custom kind](Self::Custom).
    #[must_use]
    pub fn custom(label: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(label.into())
    }

    /// Used as the `tracing` span name.
    ///
    /// `tracing` requires a span name to be a `&'static str`, which a custom label cannot
    /// provide, so [`Self::Custom`] is always called `custom` and the real label goes into
    /// [`field::SPAN_LABEL`]. **Do not group attribution by span name**; group by
    /// [`field::SPAN_KIND`].
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

    /// The value of [`field::SPAN_KIND`]. A custom kind returns its own label.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Custom(label) => label.as_ref(),
            other => other.span_name(),
        }
    }

    /// Which level a span of this kind belongs at.
    ///
    /// The criterion is that **a normal run should be readable end to end at INFO**: the run
    /// skeleton (agent / turn / generation / function / handoff) goes to INFO, while the frequent
    /// and usually uneventful ones (guardrail, `mcp_list_tools`) go to DEBUG — when a guard
    /// actually blocks, a separate WARN event carries the part worth reading.
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

    /// Fields that **must** be present when a span of this kind is created.
    ///
    /// Only identifying fields that are known at creation time are listed. Terminal fields
    /// ([`field::OUTCOME`], usage, and so on) are reserved with [`tracing::field::Empty`] and
    /// recorded later, so they are not required here.
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
// terminal state
// ---------------------------------------------------------------------------

/// Terminal state of a span. **Cancellation gets its own tier** rather than being folded into
/// `Error`, for the same reason as R0-2: cancellation is not failure, and merging them makes the
/// failure-rate metric spike every time a user presses stop.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpanOutcome {
    /// Completed normally.
    Ok,
    /// Ended in failure.
    Error,
    /// Was cancelled.
    Cancelled,
}

impl SpanOutcome {
    /// The value of [`field::OUTCOME`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether this counts as a failure. Cancellation does not.
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
    /// Projected through `Error::is_cancelled()`; it matches no variant and reads no text.
    fn from(err: &Error) -> Self {
        if err.is_cancelled() {
            Self::Cancelled
        } else {
            Self::Error
        }
    }
}

// ---------------------------------------------------------------------------
// level projection
// ---------------------------------------------------------------------------

/// Which level an error belongs at. **Projected from recoverability**, never chosen by the
/// callsite.
///
/// | Recoverability | Level | Reason |
/// | --- | --- | --- |
/// | `Retryable` / `RetryableWithChange` | WARN | the framework handles it; nobody has to act |
/// | `NeedsIntervention` / `Fatal` | ERROR | it cannot proceed without intervention |
/// | `Cancelled` | INFO | cancellation is a normal ending; logging it as ERROR drowns real errors |
///
/// Letting the callsite pick the level has a predictable outcome: every pre-retry failure logs as
/// ERROR, the log turns red end to end, and the one line that actually needs a human is buried.
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

/// Records the terminal state.
///
/// The field must have been reserved with [`tracing::field::Empty`] when the span was created,
/// or `record` silently does nothing — that is `tracing` semantics, not an oversight here.
pub fn record_outcome(span: &Span, outcome: SpanOutcome) {
    span.record(field::OUTCOME, outcome.as_str());
}

/// Records an error terminal state: [`field::ERROR_CODE`] plus [`field::OUTCOME`].
///
/// Only `code()` is stored, **never the error text**: text changes, it can contain paths and
/// secrets, and aggregating by text is exactly the vocabulary-matching that R7-10 forbids. A
/// cancellation is recorded as [`SpanOutcome::Cancelled`] rather than
/// `Error`。
pub fn record_error(span: &Span, err: &Error) {
    span.record(field::ERROR_CODE, err.code());
    record_outcome(span, SpanOutcome::from(err));
}

/// Records a cancellation: root cause, initiating level, and terminal state.
///
/// This is the only write path for the two fields promised by the cancellation contract (R0-4).
pub fn record_cancel(span: &Span, reason: &CancelReason, scope: &ScopeKind) {
    span.record(field::CANCEL_REASON, reason.code());
    span.record(field::CANCEL_SCOPE, scope.label());
    record_outcome(span, SpanOutcome::Cancelled);
}
