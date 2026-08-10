//! What a finished run hands back (R3-7).
//!
//! # Three histories, three names
//!
//! A run produces three different sequences and they are deliberately not one array:
//!
//! | Sequence | Question it answers |
//! | --- | --- |
//! | [`RunResult::original_input`] | what was asked |
//! | [`RunResult::new_items`] | what the run produced, in order |
//! | [`RunResult::continuation_input`] | what the *next* call should send |
//!
//! Collapsing them is the failure the reference implementation's `to_input_list()` exists to avoid:
//! a display history replayed as model input duplicates records, and a model input stored as
//! session history loses the ones a filter dropped. The third is a projection with an explicit
//! policy rather than a stored field, so it cannot drift from the second.

use ra_core::{
    agent::AgentSpec,
    finish::FinishReason,
    item::{InputItemNormalizer, Message, ModelInputItem, ModelResponse, RunItem, RunItemKind},
    state::ToolUseTracker,
    usage::Usage,
};
use std::sync::Arc;

/// How a run ended.
///
/// Interruption is a first-class outcome, not an error and not a [`FinishReason`]. A run waiting on
/// an approval has not *finished* — it is a state the host can answer and resume from — so folding
/// it into a finish reason would make "done" and "waiting" indistinguishable to every caller that
/// only checks whether the run returned `Ok`.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// The loop settled and will not continue.
    Completed {
        /// Why it stopped.
        reason: FinishReason,
    },
    /// The run stopped to ask the host something. Every item is approval-shaped.
    Interrupted {
        /// Decisions the host owes before the run can continue.
        items: Vec<RunItem>,
    },
}

impl RunOutcome {
    /// Why the run stopped, or `None` while it is waiting on a decision.
    #[must_use]
    pub const fn finish_reason(&self) -> Option<FinishReason> {
        match self {
            Self::Completed { reason } => Some(*reason),
            Self::Interrupted { .. } => None,
        }
    }

    /// Decisions the host owes, empty when the run completed.
    #[must_use]
    pub fn interruptions(&self) -> &[RunItem] {
        match self {
            Self::Completed { .. } => &[],
            Self::Interrupted { items } => items,
        }
    }
}

/// Which history to build the next call's input from.
///
/// The two policies differ in who is allowed to reinterpret the records, which is why the choice
/// belongs to the caller rather than to a default. R9-7 and R12-2 both consume this.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContinuationInput {
    /// Every record, projected verbatim.
    ///
    /// Use it when something downstream owns reconciliation and must see exactly what happened —
    /// a resume that has to line up with stored history, or a nested run whose parent will do the
    /// pruning.
    PreserveAll,
    /// Run through [`InputItemNormalizer`]: deduplicated, call/output paired, dangling reasoning
    /// removed.
    ///
    /// The default, because it is what a provider will accept. An unpaired tool call in the input
    /// is rejected by every provider, and the run cannot know a caller wanted to keep one.
    #[default]
    Normalized,
}

/// Everything one finished run produced.
///
/// `last_agent` is the **public** agent (R3-12): after a handoff the run is attributed to whoever
/// spoke last as the user knows them, not to a prepared execution instance.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunResult {
    outcome: RunOutcome,
    last_agent: Arc<AgentSpec>,
    original_input: Vec<ModelInputItem>,
    new_items: Vec<RunItem>,
    model_responses: Vec<ModelResponse>,
    turns: u32,
    tool_use: ToolUseTracker,
}

impl RunResult {
    pub(super) const fn new(
        outcome: RunOutcome,
        last_agent: Arc<AgentSpec>,
        original_input: Vec<ModelInputItem>,
        new_items: Vec<RunItem>,
        model_responses: Vec<ModelResponse>,
        turns: u32,
        tool_use: ToolUseTracker,
    ) -> Self {
        Self {
            outcome,
            last_agent,
            original_input,
            new_items,
            model_responses,
            turns,
            tool_use,
        }
    }

    /// How the run ended.
    #[must_use]
    pub const fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }

    /// The public agent that spoke last.
    #[must_use]
    pub const fn last_agent(&self) -> &Arc<AgentSpec> {
        &self.last_agent
    }

    /// The input the run started from.
    #[must_use]
    pub fn original_input(&self) -> &[ModelInputItem] {
        &self.original_input
    }

    /// Everything the run generated, in the order it happened.
    #[must_use]
    pub fn new_items(&self) -> &[RunItem] {
        &self.new_items
    }

    /// One entry per model call, kept for usage accounting, provider continuation IDs, and replay.
    #[must_use]
    pub fn model_responses(&self) -> &[ModelResponse] {
        &self.model_responses
    }

    /// How many turns ran.
    #[must_use]
    pub const fn turns(&self) -> u32 {
        self.turns
    }

    /// Tool-use history as of the last turn (R3-6b).
    ///
    /// Handed back rather than consumed, because the run that continues from an interruption
    /// has to carry it forward: starting the next segment with a fresh tracker resets every
    /// repeat streak, which turns "pause and resume" into a way to defeat the loop breaker.
    /// Feed it to [`RunRequest::with_tool_use`](super::RunRequest::with_tool_use).
    #[must_use]
    pub const fn tool_use(&self) -> &ToolUseTracker {
        &self.tool_use
    }

    /// Token usage across every call this run made.
    ///
    /// Summed from [`Self::model_responses`] rather than accumulated into a field: a stored total
    /// is a second source of truth that a dropped or retried response can put out of step with the
    /// calls it claims to summarise.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.model_responses
            .iter()
            .fold(Usage::default(), |total, response| {
                let usage = response.usage();
                Usage::new(
                    total.input_tokens() + usage.input_tokens(),
                    total.output_tokens() + usage.output_tokens(),
                )
                .with_cached_input_tokens(total.cached_input_tokens() + usage.cached_input_tokens())
                .with_reasoning_tokens(total.reasoning_tokens() + usage.reasoning_tokens())
            })
    }

    /// The last thing the model said, if it said anything.
    ///
    /// Structural: the final assistant message in generation order. It deliberately does **not**
    /// filter on [`OutputPhase`](ra_core::item::OutputPhase) yet — R3-10 is what makes the
    /// commentary/final split load bearing — and it does not parse the text, which is R1-16's
    /// structured-output contract.
    #[must_use]
    pub fn final_message(&self) -> Option<&Message> {
        self.new_items
            .iter()
            .rev()
            .find_map(|item| match item.kind() {
                RunItemKind::Message(message) => Some(message),
                _ => None,
            })
    }

    /// Builds the input for a call that continues from this run.
    ///
    /// A projection, not a stored list. Storing it would let the "what to send next" copy drift
    /// from the "what happened" one, and the drift shows up as a duplicated or missing turn a
    /// session later.
    #[must_use]
    pub fn continuation_input(&self, policy: ContinuationInput) -> Vec<ModelInputItem> {
        let mut items = self.original_input.clone();
        items.extend(self.new_items.iter().filter_map(RunItem::to_model_input));
        match policy {
            ContinuationInput::PreserveAll => items,
            // A normalization failure means an item could not be rendered as JSON, which cannot
            // happen for values that were already serialized on the way in. Falling back to the
            // verbatim list keeps a projection infallible rather than making every caller handle
            // an error that has no reachable cause.
            ContinuationInput::Normalized => InputItemNormalizer::new()
                .normalize_model_items(&items)
                .map_or(items, ra_core::item::NormalizedInput::into_items),
        }
    }
}
