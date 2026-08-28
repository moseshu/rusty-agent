//! What a finished run hands back (R3-7).
//!
//! # Three histories, three names
//!
//! A run produces three different sequences and they are deliberately not one array:
//!
//! | Sequence | Question it answers | Scope |
//! | --- | --- | --- |
//! | [`RunResult::original_input`] | this segment's continuation base | the segment |
//! | [`RunResult::new_items`] | what this segment produced, in order | the segment |
//! | [`RunResult::continuation_input`] | what the *next* call should send | the run |
//!
//! Collapsing them is the failure the reference implementation's `to_input_list()` exists to avoid:
//! a display history replayed as model input duplicates records, and a model input stored as
//! session history loses the ones a filter dropped. The third is a projection with an explicit
//! policy rather than a stored field, so it cannot drift from the second.
//!
//! [`RunResult::turn_records`] is not a fourth one. It answers what each turn *decided* and borrows
//! that turn's slice of the second rather than holding records of its own.

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    budget::BudgetSnapshot,
    error::{Error, Result},
    finish::FinishReason,
    item::{
        AgentId, InputItemNormalizer, Message, MessageRole, ModelInputItem, ModelResponse,
        OutputPhase, RunItem, RunItemKind,
    },
    state::{RunState, ToolUseTracker},
    step::NextStep,
    usage::Usage,
};
use std::{ops::Range, sync::Arc};

/// Read-only facts supplied to a terminal error handler.
///
/// The handler receives a snapshot rather than the live runner, so it can produce a user-facing
/// closeout without gaining a second way to mutate accounting or session history.
#[non_exhaustive]
pub struct RunErrorData<'a> {
    last_agent: &'a Arc<AgentSpec>,
    original_input: &'a [ModelInputItem],
    new_items: &'a [RunItem],
    model_responses: &'a [ModelResponse],
    turns: u32,
    budget: BudgetSnapshot,
    usage: Usage,
}

impl<'a> RunErrorData<'a> {
    pub(super) fn new(
        last_agent: &'a Arc<AgentSpec>,
        original_input: &'a [ModelInputItem],
        new_items: &'a [RunItem],
        model_responses: &'a [ModelResponse],
        turns: u32,
        budget: BudgetSnapshot,
        usage: Usage,
    ) -> Self {
        Self {
            last_agent,
            original_input,
            new_items,
            model_responses,
            turns,
            budget,
            usage,
        }
    }

    /// The agent that spoke last, as the **public** declaration.
    ///
    /// A closeout speaks in that agent's place, so a handler needs to know whose place it is —
    /// its name, its instructions, its declared tools. It is the public view for the same reason
    /// every other host-facing surface gets one: a prepared execution instance is framework
    /// internals, and a message attributed to it would name an agent the user never configured.
    #[must_use]
    pub const fn last_agent(&self) -> &'a Arc<AgentSpec> {
        self.last_agent
    }

    /// The continuation base this segment began from.
    ///
    /// This is either caller-supplied input or an automatic projection from the checkpoint.
    #[must_use]
    pub fn original_input(&self) -> &'a [ModelInputItem] {
        self.original_input
    }

    /// Records this segment produced before the terminal condition.
    #[must_use]
    pub fn new_items(&self) -> &'a [RunItem] {
        self.new_items
    }

    /// Completed model calls this segment made before the terminal condition.
    #[must_use]
    pub fn model_responses(&self) -> &'a [ModelResponse] {
        self.model_responses
    }

    /// Number of turns in this run segment.
    #[must_use]
    pub const fn turns(&self) -> u32 {
        self.turns
    }

    /// Turn counters as of the terminal condition.
    #[must_use]
    pub fn budget(&self) -> BudgetSnapshot {
        self.budget.clone()
    }

    /// The run's usage ledger as of the terminal condition, per request and in total.
    ///
    /// Cumulative across segments, which is what a handler writing "the budget ran out" needs: the
    /// responses above cover this segment only, and a closeout that quoted them on a resumed run
    /// would name a smaller number than the ceiling that just stopped it.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }
}

/// Input to a terminal error handler.
#[non_exhaustive]
pub struct RunErrorHandlerInput<'a> {
    error: &'a Error,
    data: RunErrorData<'a>,
}

impl<'a> RunErrorHandlerInput<'a> {
    pub(super) const fn new(error: &'a Error, data: RunErrorData<'a>) -> Self {
        Self { error, data }
    }

    /// Structured terminal error. Route on its type or code, never its display text.
    #[must_use]
    pub const fn error(&self) -> &'a Error {
        self.error
    }

    /// Read-only run snapshot.
    #[must_use]
    pub const fn data(&self) -> &RunErrorData<'a> {
        &self.data
    }
}

/// A final message generated by an error handler.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunErrorHandlerResult {
    message: Message,
    write_to_history: bool,
}

impl RunErrorHandlerResult {
    /// Creates a result that delivers a final assistant message without recording it.
    #[must_use]
    pub fn new(message: Message) -> Self {
        Self {
            message,
            write_to_history: false,
        }
    }

    /// Chooses whether the generated message is appended to session history.
    #[must_use]
    pub const fn with_write_to_history(mut self, write_to_history: bool) -> Self {
        self.write_to_history = write_to_history;
        self
    }

    /// Final message to deliver.
    #[must_use]
    pub const fn message(&self) -> &Message {
        &self.message
    }

    /// Whether the final message is also part of the continuation history.
    #[must_use]
    pub const fn write_to_history(&self) -> bool {
        self.write_to_history
    }
}

/// Produces a final message for an otherwise terminal run condition.
///
/// One handler serves every terminal condition, and [`RunErrorHandlerInput::error`] says which one
/// this is. That is only workable because a handler may decline — see [`Self::handle`].
#[async_trait]
pub trait RunErrorHandler: Send + Sync {
    /// Turns a structured error and read-only run snapshot into a delivery result.
    ///
    /// **`None` declines**: the run ends exactly as it would have with no handler installed. This
    /// is what keeps one handler from becoming an obligation to answer for everything. A host that
    /// installs a closeout for an exhausted budget should not thereby be responsible for provider
    /// refusals and invalid structured output when those conditions start arriving here too —
    /// silently speaking for a condition it never considered is worse than not speaking.
    ///
    /// Declining is not error handling. Returning `Err` still fails the run.
    async fn handle(
        &self,
        input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>>;
}

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

/// Folds the usage of a sequence of model calls into one ledger.
///
/// One implementation, two readers: [`RunResult::usage`] answers a finished run, and the agent
/// span has to answer a run that failed after paying for calls, where no [`RunResult`] exists. A
/// second copy of this fold would drift the day [`Usage`] grows a dimension, and the two totals
/// would disagree about the same calls.
///
/// Every call's per-request entries come along, so the fold answers "what did this run cost" and
/// "which call cost it" from the same value.
pub(crate) fn aggregate_usage(responses: &[ModelResponse]) -> Usage {
    responses.iter().fold(Usage::default(), |total, response| {
        total.accumulate(response.usage())
    })
}

/// What one settled turn decided.
///
/// The loop's control flow is one `match` on [`NextStep`], and until this existed that decision was
/// observable only through its consequences: a run's turn count says another turn happened, not
/// that the turn asked for one. Anything reconstructing the sequence from item counts is guessing,
/// and it guesses wrong the moment a state is added — a handoff continues the loop exactly like a
/// `RunAgain` does from the outside.
///
/// **Not a fourth history.** [`RunResult::turn_items`] borrows the run's own records rather than
/// this holding a copy, for the reason the three sequences above are separate names: a second array
/// of the same records is a second thing to keep in step.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TurnRecord {
    owner: Arc<TurnRecordOwner>,
    turn: u32,
    agent: AgentId,
    next_step: NextStep,
    items: Range<usize>,
}

impl TurnRecord {
    pub(super) const fn new(
        owner: Arc<TurnRecordOwner>,
        turn: u32,
        agent: AgentId,
        next_step: NextStep,
        items: Range<usize>,
    ) -> Self {
        Self {
            owner,
            turn,
            agent,
            next_step,
            items,
        }
    }

    /// Which turn this was, counting from 1 — the same number
    /// [`RunStreamEvent::TurnStarted`](super::RunStreamEvent::TurnStarted) carries.
    #[must_use]
    pub const fn turn(&self) -> u32 {
        self.turn
    }

    /// The **public** agent that ran the turn.
    ///
    /// Per turn rather than per run, because that is the granularity a handoff changes it at:
    /// [`RunResult::last_agent`] answers who finished, this answers who spoke when.
    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// Stable code of the [`NextStep`] the turn settled on: `run_again`, `handoff`, `final_output`
    /// or `interruption`.
    ///
    /// A code rather than the state itself, because [`NextStep`] is settlement's own control-flow
    /// type — see [`NextStep::code`] for why it does not leave the framework.
    #[must_use]
    pub const fn next_step_code(&self) -> &'static str {
        self.next_step.code()
    }

    /// Why the loop settled, when this turn is the one that ended it.
    ///
    /// `None` for every turn that did not: the run continued, or it stopped to ask the host
    /// something, which is not a finish.
    #[must_use]
    pub const fn finish_reason(&self) -> Option<FinishReason> {
        match &self.next_step {
            NextStep::FinalOutput { reason } => Some(*reason),
            NextStep::RunAgain | NextStep::Handoff { .. } | NextStep::Interruption { .. } => None,
        }
    }

    /// Where this turn's records sit in [`RunResult::new_items`].
    pub(super) const fn range(&self) -> &Range<usize> {
        &self.items
    }
}

/// Identity shared by one result and the records it produced.
///
/// This is deliberately non-zero-sized: pointer identity must distinguish two independently
/// completed runs, including when their record ranges happen to be identical.
#[derive(Debug)]
pub(super) struct TurnRecordOwner {
    _marker: u8,
}

impl TurnRecordOwner {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self { _marker: 0 })
    }
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
    turn_record_owner: Arc<TurnRecordOwner>,
    turn_records: Vec<TurnRecord>,
    turns: u32,
    state: RunState,
    final_message: Option<Message>,
}

impl RunResult {
    // One argument per field the loop fills, and the loop is the only caller. A parameter struct
    // here would be the same list with a name in front of it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        outcome: RunOutcome,
        last_agent: Arc<AgentSpec>,
        original_input: Vec<ModelInputItem>,
        new_items: Vec<RunItem>,
        model_responses: Vec<ModelResponse>,
        turn_record_owner: Arc<TurnRecordOwner>,
        turn_records: Vec<TurnRecord>,
        turns: u32,
        state: RunState,
    ) -> Self {
        let final_message = find_final_message(&new_items).cloned();
        Self {
            outcome,
            last_agent,
            original_input,
            new_items,
            model_responses,
            turn_record_owner,
            turn_records,
            turns,
            state,
            final_message,
        }
    }

    /// Overrides the delivered message with one a [`RunErrorHandler`] produced.
    ///
    /// A closeout speaks for a run that stopped without answering, so it outranks whatever the
    /// model last said — including nothing at all.
    pub(super) fn with_final_message(mut self, message: Message) -> Self {
        self.final_message = Some(message);
        self
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

    /// The continuation base this segment started from.
    ///
    /// This is the caller-provided input when one was supplied. For an empty checkpoint resume,
    /// it is the checkpoint's projected history.
    #[must_use]
    pub fn original_input(&self) -> &[ModelInputItem] {
        &self.original_input
    }

    /// Everything **this segment** generated, in the order it happened.
    ///
    /// A resumed run's earlier records are not here: they are in
    /// [`RunState::generated_items`](ra_core::state::RunState::generated_items), reachable through
    /// [`Self::state`]. The split is deliberate — [`Self::turn_records`] index into this one, and
    /// a host that streamed the earlier segments has already shown their records once.
    #[must_use]
    pub fn new_items(&self) -> &[RunItem] {
        &self.new_items
    }

    /// One entry per model call **this segment** made, kept for usage accounting, provider
    /// continuation IDs, and replay.
    ///
    /// Scoped like [`Self::new_items`], and for the same reason; the run's own list is
    /// [`RunState::model_responses`](ra_core::state::RunState::model_responses).
    #[must_use]
    pub fn model_responses(&self) -> &[ModelResponse] {
        &self.model_responses
    }

    /// How many turns ran in this segment.
    #[must_use]
    pub const fn turns(&self) -> u32 {
        self.turns
    }

    /// One record per **settled** turn, in the order they ran.
    ///
    /// Shorter than [`Self::turns`] when the last turn never settled — an error or a cancellation
    /// mid-turn leaves a turn that was started and paid for but decided nothing, and reporting a
    /// decision it did not make is worse than reporting one fewer record.
    ///
    /// **The last record is not always the one that ended the run.** A turn cap or an exhausted
    /// budget fires *between* turns, so every record can say `run_again` while the run stopped
    /// anyway; [`Self::outcome`] is what always answers how it ended.
    #[must_use]
    pub fn turn_records(&self) -> &[TurnRecord] {
        &self.turn_records
    }

    /// The records one settled turn produced.
    ///
    /// Empty for a record that belongs to a different run. Records share a private owner identity
    /// with their result, then their range is looked up rather than indexed, so either kind of
    /// mismatch answers nothing instead of panicking. Records produced outside any turn — a
    /// closeout written to history after the loop ended — belong to no record here and are reached
    /// through [`Self::new_items`].
    #[must_use]
    pub fn turn_items(&self, record: &TurnRecord) -> &[RunItem] {
        if !Arc::ptr_eq(&self.turn_record_owner, &record.owner) {
            return &[];
        }
        self.new_items
            .get(record.range().clone())
            .unwrap_or_default()
    }

    /// Tool-use history as of the last turn (R3-6b).
    ///
    /// A read-only projection of [`Self::state`], for callers that only want to inspect the
    /// streaks. Continuing a run carries the whole [`RunState`], not this field.
    #[must_use]
    pub const fn tool_use(&self) -> &ToolUseTracker {
        self.state.tool_use()
    }

    /// The run's own state as of the last settled turn.
    ///
    /// Handed back rather than consumed, because a run that continues from an interruption has to
    /// carry it forward: starting the next segment with fresh state resets every repeat streak,
    /// which turns "pause and resume" into a way to defeat the loop breaker. Feed it to
    /// [`RunRequest::with_state`](super::RunRequest::with_state).
    #[must_use]
    pub const fn state(&self) -> &RunState {
        &self.state
    }

    /// Token usage across every call **this segment** made, per request and in total.
    ///
    /// Summed from [`Self::model_responses`] rather than accumulated into a field: a stored total
    /// is a second source of truth that a dropped or retried response can put out of step with the
    /// calls it claims to summarise.
    ///
    /// # This is not the same number as the run's ledger
    ///
    /// [`RunState::usage_totals`](ra_core::state::RunState::usage_totals), reachable through
    /// [`Self::state`], is cumulative across every segment of the run and is what the token budget
    /// is measured against. This is what the segment that just finished spent. On a run that was
    /// never resumed the two agree; on a resumed one they are *supposed* to differ, and reading
    /// this one as the run's total would under-report every continuation.
    #[must_use]
    pub fn usage(&self) -> Usage {
        aggregate_usage(&self.model_responses)
    }

    /// The message that delivered the run, if it produced one.
    ///
    /// Structural: the assistant message settlement put on
    /// [`OutputPhase::Final`](ra_core::item::OutputPhase::Final) (R3-10). It does not parse the
    /// text, which is R1-16's structured-output contract.
    ///
    /// **`None` is a real answer, not just "the model said nothing".** A run that stopped for an
    /// approval has not delivered anything yet, and neither has one that hit
    /// [`FinishReason::MaxTurns`] — the cap fires between turns, so the last thing the model said
    /// was work in progress. A configured [`RunErrorHandler`] can turn that condition into an
    /// explicit delivery; otherwise a host that needs to show *something* reads
    /// [`Self::new_items`].
    #[must_use]
    pub fn final_message(&self) -> Option<&Message> {
        self.final_message.as_ref()
    }

    /// The text of [`Self::final_message`], empty when the run delivered none.
    ///
    /// **It reads that one field and nothing else.** The temptation this method exists to remove is
    /// the other implementation — walking [`Self::new_items`] for the record settlement stamped
    /// [`OutputPhase::Final`] on. That walk is already done once, in [`Self::new`], and a second
    /// copy would be a second definition of "which record was the delivery" for the two of them to
    /// disagree about: a closeout from a [`RunErrorHandler`] replaces the field without touching
    /// the items, so the two answers differ on exactly the runs a host most wants to render.
    ///
    /// Empty is meaningful in the same way [`Self::final_message`]'s `None` is: a run that stopped
    /// for an approval or ran out of turns delivered nothing. A host that wants to show *something*
    /// regardless reads [`Self::new_items`].
    ///
    /// Choosing among several candidate answers, shortening one, or rendering it for a particular
    /// audience is presentation policy and belongs to R15, not here.
    #[must_use]
    pub fn final_text(&self) -> String {
        self.final_message()
            .map(Message::text_content)
            .unwrap_or_default()
    }

    /// Builds the input for a call that continues from this run.
    ///
    /// A projection, not a stored list. Storing it would let the "what to send next" copy drift
    /// from the "what happened" one, and the drift shows up as a duplicated or missing turn a
    /// session later.
    ///
    /// It extends this result's continuation base with the records this segment produced. For an
    /// automatic checkpoint resume that base already contains the prior recorded history; for an
    /// explicit resume it remains the caller-provided input.
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

fn find_final_message(items: &[RunItem]) -> Option<&Message> {
    items.iter().rev().find_map(|item| match item.kind() {
        RunItemKind::Message(message)
            if matches!(message.role(), MessageRole::Assistant)
                && message.phase() == Some(OutputPhase::Final) =>
        {
            Some(message)
        }
        _ => None,
    })
}
