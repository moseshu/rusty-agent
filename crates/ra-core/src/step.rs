//! The four `NextStep` states, `ProcessedResponse`, and `SingleStepResult`.
//!
//! One turn moves through the three in order: [`ProcessedResponse`] (R3-2) says what the model
//! asked for, [`NextStep`] (R3-1) is where the loop's control flow converges, and
//! [`SingleStepResult`] (R3-3) is the single product that carries both plus everything the turn
//! generated.
//!
//! **Stability**: `Internal` — these are turn-settlement intermediates. Once they leak into
//! downstream code, R1 and R3 can no longer be refactored.

use std::sync::Arc;

use crate::{
    agent::AgentSpec,
    error::{Error, Result},
    finish::FinishReason,
    item::RunItem,
};

pub mod processed;
pub mod single;

pub use processed::{
    ProcessedResponse, ProcessedResponseBuilder, ToolNotFound, ToolRunApproval, ToolRunFunction,
    ToolRunHandoff, ToolUse,
};
pub use single::{SingleStepResult, SingleStepResultBuilder};

/// What the loop does after one settled turn.
///
/// **Every "should this keep going?" decision has to become one of these four values.** Scattering
/// early returns through the runner is what this type exists to prevent: with one convergence point
/// a reader finds the whole control flow by finding the `match`, and a new state becomes a compile
/// error at every site that has to handle it.
///
/// # Why this enum is exhaustive
///
/// It carries no `#[non_exhaustive]`, alone among the framework's public enums (extension safety
/// rule 1, and it is named as the sole exception in the CI gate). The reasoning inverts for
/// outward-facing data: a `ToolOutput` variant added to a closed enum breaks every third-party
/// `match`, so those must stay open. `NextStep` is the opposite — the framework is the only thing
/// that matches on it, and **adding a control-flow state must break every match until each one
/// says what it does about it**. R3's acceptance criterion states the same rule from the other
/// side: no `_ =>` arm when matching this.
///
/// For the same reason there is no `is_terminal()` or `should_continue()` helper. A boolean
/// projection is exactly the early-return shortcut this type replaces: it would let a call site
/// branch without saying what it does about handoffs or interruptions, and it would keep compiling
/// when a fifth state arrives.
#[derive(Debug, Clone)]
pub enum NextStep {
    /// Run another turn with the same agent.
    RunAgain,
    /// Transfer control to another agent, which runs the next turn.
    Handoff {
        /// The agent taking over. R3-12 binds the public and execution identities around it; this
        /// is the public one, so events and results stay attributed where the user expects.
        new_agent: Arc<AgentSpec>,
    },
    /// Stop. [`FinishReason`] says why, because the ways a run can settle demand different
    /// responses from the host.
    ///
    /// [`FinishReason::Cancelled`] and [`FinishReason::GuardrailTripped`] usually reach the run
    /// result through the error path rather than this variant, but the type does not forbid them:
    /// an R3-8 error handler can turn either into a settled final output.
    FinalOutput {
        /// Why the loop settled.
        reason: FinishReason,
    },
    /// Stop and wait for the host to answer. The run resumes once every item has a decision.
    Interruption {
        /// Approval-shaped items only; build them through [`NextStep::interruption`].
        items: Vec<RunItem>,
    },
}

impl NextStep {
    /// Builds a [`NextStep::Interruption`] after checking every item is one a host can answer.
    ///
    /// A non-approval item in this list produces a run that waits forever for a decision nobody
    /// was asked to make, and the symptom — a hung run — points nowhere near the cause. The variant
    /// stays directly constructible because settlement is inside the framework, but this is the
    /// path that says the invariant out loud.
    pub fn interruption(items: Vec<RunItem>) -> Result<Self> {
        if items.is_empty() {
            return Err(Error::caller(
                "an interruption must carry at least one item for the host to answer",
            ));
        }
        for item in &items {
            if !item.kind().is_interruption() {
                return Err(Error::caller(format!(
                    "run item kind `{}` is not something a host can approve",
                    item.kind().label()
                )));
            }
        }
        Ok(Self::Interruption { items })
    }
}
