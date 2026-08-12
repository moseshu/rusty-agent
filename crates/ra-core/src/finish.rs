//! Why a run stopped, as a structured value rather than prose.
//!
//! Three consumers need this answer and none of them can read sentences: graph-edge routing
//! branches on it, closeout logic decides whether a step is still owed, and the host protocol
//! renders it. Every one of those would otherwise have to re-derive the reason from the last
//! message, which is the text-driven control flow this framework forbids.
//!
//! **Stability**: `Stable` — [`FinishReason`] is on the Stable API list and downstream code matches
//! on it directly.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::BudgetKind;

/// Why a run reached its end.
///
/// Before this type existed, one `NextStep::FinalOutput` variant carried four different meanings:
/// the model concluded, a budget ran out, `tool_use_behavior` stopped the loop, or an error
/// handler produced the output. They demand different responses, so they are different values.
///
/// # Two shapes this deliberately does not have
///
/// **No per-variant payload.** Which guard tripped, which budget dimension ran out, and which
/// cancellation cause fired are all recorded by the types that own them — [`Error::Guardrail`],
/// [`Error::Budget`], and [`CancelReason::code`]. Copying them here would create a second source of
/// truth that can disagree with the first, and the cancellation contract already names its own
/// projection as the machine-readable one.
///
/// **No `Custom(Cow<'static, str>)`.** Open labels are right for classification axes a third party
/// must be able to extend (`CapabilityFamily`, `ToolNamespace`, `PromptRole` — extension safety
/// rule 5). This is not one: graph edges route on these values, so a free-form reason would put
/// control flow back into strings. `#[non_exhaustive]` is how this enum grows instead.
///
/// [`Error::Guardrail`]: crate::error::Error::Guardrail
/// [`Error::Budget`]: crate::error::Error::Budget
/// [`CancelReason::code`]: crate::cancel::CancelReason::code
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model produced a final answer and asked for nothing further.
    Final,
    /// A tool result became the final output under `tool_use_behavior`.
    ToolStop,
    /// The turn limit was reached before the model concluded.
    MaxTurns,
    /// A token, cost, or wall-clock budget ran out and the run ended softly.
    BudgetExhausted,
    /// The run was cancelled: a user interrupt, a shutdown, or an expired deadline.
    Cancelled,
    /// An error handler produced the outcome in place of the model.
    ErrorHandled,
    /// A guardrail tripped and stopped the run.
    GuardrailTripped,
}

impl FinishReason {
    /// Classifies an exhausted budget.
    ///
    /// The two enums are different axes and do not line up one to one: [`BudgetKind`] says *which
    /// allowance* ran out, while this says *how the run ended*. Reaching `max_turns` is the one
    /// case a host reacts to differently — it usually means the agent is looping rather than that
    /// the work was too large — so it keeps its own reason and the other three converge.
    ///
    /// Whatever ends a run softly on an exhausted budget has to make exactly this call. Pinning
    /// the mapping here keeps every caller from re-deriving it, and differently.
    #[must_use]
    pub const fn from_budget_kind(kind: BudgetKind) -> Self {
        match kind {
            BudgetKind::MaxTurns => Self::MaxTurns,
            BudgetKind::Tokens | BudgetKind::Cost | BudgetKind::WallClock => Self::BudgetExhausted,
            // `BudgetKind` is `#[non_exhaustive]` but lives in this crate, so a new allowance is a
            // compile error here rather than a silent fall-through to the wrong reason.
        }
    }

    /// Stable machine-readable slug for traces, rollout lines, and the host protocol.
    ///
    /// It mirrors [`CancelReason::code`](crate::cancel::CancelReason::code) and matches the serde
    /// wire form, so a value written to a trace field and one written to `RunState` read the same.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Final => "final",
            Self::ToolStop => "tool_stop",
            Self::MaxTurns => "max_turns",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Cancelled => "cancelled",
            Self::ErrorHandled => "error_handled",
            Self::GuardrailTripped => "guardrail_tripped",
        }
    }

    /// Whether the loop reached its own conclusion rather than being stopped from outside.
    ///
    /// Closeout logic asks this to decide whether a step is still owed, and graph-edge routing
    /// asks it to pick between a success edge and a fallback edge. The distinction is **who ended
    /// the run**, not whether the answer was good: a model that concludes wrongly still finished.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        match self {
            Self::Final | Self::ToolStop => true,
            Self::MaxTurns
            | Self::BudgetExhausted
            | Self::Cancelled
            | Self::ErrorHandled
            | Self::GuardrailTripped => false,
        }
    }

    /// Whether continuing from the saved state with a fresh allowance is meaningful.
    ///
    /// Resume and the host's "continue" affordance ask this. It is narrower than
    /// `!is_complete()`: a tripped guardrail or a handled error would reproduce the same stop,
    /// while an exhausted allowance or an interrupt would not.
    #[must_use]
    pub const fn is_resumable(self) -> bool {
        match self {
            Self::MaxTurns | Self::BudgetExhausted | Self::Cancelled => true,
            Self::Final | Self::ToolStop | Self::ErrorHandled | Self::GuardrailTripped => false,
        }
    }
}

impl fmt::Display for FinishReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}
