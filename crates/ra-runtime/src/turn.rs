//! Single-turn settlement.
//!
//! [`settle_turn`] is the whole of R3-4: one model response goes in, one [`SingleStepResult`] comes
//! out, and the stages in between run in a fixed order — classify, answer, decide, record. Each
//! stage lives in its own module so the milestones that replace one of them (R3-4b the batch shape,
//! R3-5 the tool-stop policy, R7 the guardrails, R17 handoffs) have one insertion point rather than
//! a scattering of call sites.
//!
//! The streaming path (R3-7) consumes this same function. Two settlement paths would be two loops,
//! and the second one drifts.

use ra_core::{
    cancel::CancelScope,
    error::Result,
    item::{ModelInputItem, ModelResponse, RunItem},
    state::ToolUseTracker,
    step::SingleStepResult,
    tool::ToolRuntimeContext,
};

use crate::agent::AgentBinding;

#[doc(hidden)]
pub mod batch;
pub(crate) mod interrupt;
#[doc(hidden)]
pub mod prepare;
#[doc(hidden)]
pub mod process;
#[doc(hidden)]
pub mod resolve;

use batch::{TurnExecutionRequest, execute_actions};
use prepare::TurnActionSurface;
use process::process_model_response;
use resolve::{resolve_next_step, step_items};

/// Inputs for settling one turn.
///
/// [`ToolUseTracker`] is run-scoped state and the only `&mut` here, which is deliberate: it is what
/// makes the per-turn counts a fact about the run rather than about one function call. It is also
/// **required rather than optional** — a caller that could omit it would get a run whose repeat
/// streaks silently never advance, and R3-6's loop breaker would read zero forever while the model
/// looped.
#[must_use]
#[non_exhaustive]
pub struct TurnSettlementRequest<'a> {
    agent: &'a AgentBinding,
    response: &'a ModelResponse,
    surface: &'a TurnActionSurface,
    context: &'a dyn ToolRuntimeContext,
    cancel: &'a CancelScope,
    tool_use: &'a mut ToolUseTracker,
    original_input: Vec<ModelInputItem>,
    pre_step_items: Vec<RunItem>,
}

impl<'a> TurnSettlementRequest<'a> {
    /// Creates a request for one turn.
    ///
    /// `surface` has to be the one this turn advertised — take it from the preparation with
    /// [`PreparedTurn::into_call`](prepare::PreparedTurn::into_call) rather than rebuilding it, or
    /// settlement resolves names against tools the turn never offered.
    ///
    /// `agent` is a binding rather than an ID because settlement is the stage that *files* things —
    /// records, counts, and later hooks and spans. It reads
    /// [`public_id`](AgentBinding::public_id) and nothing else, so no caller is in a position to
    /// hand it a sandbox-prepared clone's identity by mistake (R3-12).
    pub fn new(
        agent: &'a AgentBinding,
        response: &'a ModelResponse,
        surface: &'a TurnActionSurface,
        context: &'a dyn ToolRuntimeContext,
        cancel: &'a CancelScope,
        tool_use: &'a mut ToolUseTracker,
    ) -> Self {
        Self {
            agent,
            response,
            surface,
            context,
            cancel,
            tool_use,
            original_input: Vec::new(),
            pre_step_items: Vec::new(),
        }
    }

    /// Sets the input the run started from.
    pub fn with_original_input(mut self, input: Vec<ModelInputItem>) -> Self {
        self.original_input = input;
        self
    }

    /// Sets what earlier turns generated.
    pub fn with_pre_step_items(mut self, items: Vec<RunItem>) -> Self {
        self.pre_step_items = items;
        self
    }
}

/// Settles one turn in the only permitted stage order.
pub async fn settle_turn(request: TurnSettlementRequest<'_>) -> Result<SingleStepResult> {
    // 1. Classify. Nothing runs until every call the model made is bound to the thing that will
    // answer it, so no later stage has to re-read a provider payload to find out what it is.
    let processed = process_model_response(request.response, request.surface)?;

    // 1b. Record what the model asked for, before anything acts on it. This is not a fifth stage:
    // it produces no decision and nothing branches on it here. Its position is the point — R3-6's
    // loop breaker lives inside `dispatch_tool`, which runs below, so this turn's attempts have to
    // already be in the tracker when it looks. Recording after execution would show the breaker
    // every turn but the one it is being asked about.
    //
    // Attempts are recorded, not results: a call that is refused, times out, or resolves to nothing
    // is still the model asking for the same thing again, and that is exactly what `reset_tool_choice`
    // and the breaker react to.
    let public_id = request.agent.public_id();
    request
        .tool_use
        .record_turn(public_id, processed.attempts());

    // 2. Answer every bound action. Interruptions come back rather than blocking: a pending
    // approval is a state the run can be saved in, not an `await` somebody is stuck on.
    let execution = execute_actions(TurnExecutionRequest::new(
        &processed,
        public_id,
        request.tool_use,
        request.context,
        request.cancel,
    ))
    .await?;

    // 3. Decide. One function, four states, priority written down once.
    let next_step = resolve_next_step(&processed, &execution)?;

    // 4. Record. `new_step_items` and `session_step_items` are the same list this turn: nothing
    // filters the model-facing view yet, and R5's budgeting is what will make them diverge. The
    // builder's checks are what keeps that future divergence from quietly dropping history.
    let items = step_items(&processed, &execution, request.agent.public());
    SingleStepResult::builder()
        .original_input(request.original_input)
        .model_response(request.response.clone())
        .pre_step_items(request.pre_step_items)
        .new_step_items(items.clone())
        .session_step_items(items)
        .processed_response(processed)
        .next_step(next_step)
        .build()
}
