//! Single-turn settlement.
//!
//! [`settle_turn`] is the whole of R3-4: one model response goes in, one [`SingleStepResult`] comes
//! out, and the stages in between run in a fixed order — classify, answer, decide, record. Each
//! stage lives in its own module so the milestones that replace one of them (R3-4b the batch shape,
//! R3-5 the tool-stop policy, R7 the guardrails) have one insertion point rather than a scattering
//! of call sites. A transfer of control is the one stage that does not fit that shape: it is
//! decided in `resolve`, recorded in `batch`, and projected here, because what it hands over is
//! assembled from three lists only this function holds at once.
//!
//! The streaming path (R3-7) consumes this same function. Two settlement paths would be two loops,
//! and the second one drifts.

use std::{collections::HashSet, sync::Arc};

use ra_core::{
    agent::{HandoffInputData, HandoffInputFilter},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{CallId, ItemId, ModelInputItem, ModelResponse, RunItem, RunItemKind},
    state::{ToolFailureTracker, ToolUseTracker},
    step::{NextStep, ProcessedResponse, SingleStepResult, resolve_output_phases},
    tool::ToolServices,
};

use crate::agent::AgentBinding;

#[doc(hidden)]
pub mod batch;
pub(crate) mod interrupt;
mod mcp_metadata;
#[doc(hidden)]
pub mod prepare;
#[doc(hidden)]
pub mod process;
#[doc(hidden)]
pub mod resolve;

use crate::hook::UserHooks;
use crate::lifecycle::LifecycleHooks;
use crate::permission::PermissionEngine;
use crate::tool::guardrail::ToolGuardrails;
use batch::{
    DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY, StreamedFunctionDispatches, TurnExecutionRequest,
    TurnRecovery, execute_actions,
};
use prepare::TurnActionSurface;
use process::process_model_response_with_history;
use resolve::{rebind_interruption, resolve_next_step_with_output, step_items};

/// Inputs for settling one turn.
///
/// The two trackers are run-scoped state and the only `&mut` here, which is deliberate: they are
/// what make the counts a fact about the run rather than about one function call. Both are
/// **required rather than optional** — a caller that could omit either would get a run whose
/// streaks silently never advance, and the loop breakers would read zero forever while the model
/// looped. [`RunState::trackers_mut`](ra_core::state::RunState::trackers_mut) hands over both at
/// once, because they are two fields of the same state.
#[must_use]
#[non_exhaustive]
pub struct TurnSettlementRequest<'a> {
    agent: &'a AgentBinding,
    response: &'a ModelResponse,
    surface: &'a TurnActionSurface,
    run: Arc<RunContext>,
    cancel: &'a CancelScope,
    tool_use: &'a mut ToolUseTracker,
    tool_failure: &'a mut ToolFailureTracker,
    services: ToolServices,
    max_function_tool_concurrency: Option<usize>,
    permission: PermissionEngine,
    guardrails: ToolGuardrails,
    user_hooks: UserHooks,
    lifecycle: LifecycleHooks,
    streamed_dispatches: Option<StreamedFunctionDispatches>,
    original_input: Vec<ModelInputItem>,
    pre_step_items: Vec<RunItem>,
    handoff_input_filter: Option<Arc<dyn HandoffInputFilter>>,
    server_manages_conversation: bool,
    recovery: Option<&'a mut TurnRecovery>,
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
    ///
    /// `run` is the run's live context, which this turn's tools read. It carries the public agent
    /// for the same reason `agent` is a binding: what a tool sees named is what the user configured,
    /// never a prepared instance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: &'a AgentBinding,
        response: &'a ModelResponse,
        surface: &'a TurnActionSurface,
        run: Arc<RunContext>,
        cancel: &'a CancelScope,
        tool_use: &'a mut ToolUseTracker,
        tool_failure: &'a mut ToolFailureTracker,
        permission: PermissionEngine,
    ) -> Self {
        Self {
            agent,
            response,
            surface,
            run,
            cancel,
            tool_use,
            tool_failure,
            services: ToolServices::new(),
            max_function_tool_concurrency: DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY,
            permission,
            guardrails: ToolGuardrails::default(),
            user_hooks: UserHooks::default(),
            lifecycle: LifecycleHooks::new(),
            streamed_dispatches: None,
            original_input: Vec::new(),
            pre_step_items: Vec::new(),
            handoff_input_filter: None,
            server_manages_conversation: false,
            recovery: None,
        }
    }

    /// Sets the framework ports the tools this turn calls are handed.
    pub fn with_services(mut self, services: ToolServices) -> Self {
        self.services = services;
        self
    }

    /// Installs the host hooks consulted by this execution path.
    pub fn with_user_hooks(mut self, hooks: UserHooks) -> Self {
        self.user_hooks = hooks;
        self
    }

    /// Sets the lifecycle narration this run and its running agent installed.
    pub fn with_lifecycle_hooks(mut self, lifecycle: LifecycleHooks) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    /// Sets the tool guardrails this turn's calls resolve their declarations against.
    pub fn with_tool_guardrails(mut self, guardrails: ToolGuardrails) -> Self {
        self.guardrails = guardrails;
        self
    }

    /// Sets the per-turn cap for concurrently dispatched function tools.
    pub fn with_max_function_tool_concurrency(mut self, max: impl Into<Option<usize>>) -> Self {
        self.max_function_tool_concurrency = max.into();
        self
    }

    /// Supplies function calls that began from completed model-stream items.
    pub(crate) fn with_streamed_dispatches(
        mut self,
        streamed_dispatches: StreamedFunctionDispatches,
    ) -> Self {
        self.streamed_dispatches = Some(streamed_dispatches);
        self
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

    /// Sets the transform applied to a transfer that declares none of its own.
    pub fn with_handoff_input_filter(mut self, filter: Arc<dyn HandoffInputFilter>) -> Self {
        self.handoff_input_filter = Some(filter);
        self
    }

    /// Records whether the provider owns conversation history for this turn.
    pub const fn with_server_managed_conversation(mut self, enabled: bool) -> Self {
        self.server_manages_conversation = enabled;
        self
    }

    pub(crate) fn with_recovery(mut self, recovery: &'a mut TurnRecovery) -> Self {
        self.recovery = Some(recovery);
        self
    }
}

/// Settles one turn in the only permitted stage order.
pub async fn settle_turn(mut request: TurnSettlementRequest<'_>) -> Result<SingleStepResult> {
    // 1. Classify. Nothing runs until every call the model made is bound to the thing that will
    // answer it, so no later stage has to re-read a provider payload to find out what it is.
    let processed = match process_model_response_with_history(
        request.response,
        request.surface,
        &request.pre_step_items,
    ) {
        Ok(processed) => processed,
        Err(error) => {
            // A completed stream item can already have spawned tool work. Classification normally
            // precedes execution, but this path owns an exception to that ordering, so it must
            // give the same cancellation and drain guarantee as every later failure path.
            if let Some(dispatches) = request.streamed_dispatches.take() {
                dispatches.cancel_and_drain().await;
            }
            return Err(error);
        }
    };

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

    if !processed.handoffs().is_empty()
        && let Some(recovery) = request.recovery.as_deref_mut()
    {
        recovery.handoff = Some((
            processed.clone(),
            HandoffInputData::new(
                request.original_input.clone(),
                request.pre_step_items.clone(),
                Vec::new(),
            ),
        ));
    }

    // 2. Answer every bound action. Interruptions come back rather than blocking: a pending
    // approval is a state the run can be saved in, not an `await` somebody is stuck on.
    let execution_request = TurnExecutionRequest::new(
        &processed,
        public_id,
        request.tool_use,
        request.tool_failure,
        Arc::clone(&request.run),
        request.cancel,
        request.permission.clone(),
    )
    .with_services(request.services.clone())
    .with_tool_guardrails(request.guardrails.clone())
    .with_user_hooks(request.user_hooks.clone())
    .with_lifecycle_hooks(request.lifecycle.clone())
    .with_max_function_tool_concurrency(request.max_function_tool_concurrency)
    .with_recovery(request.recovery.as_deref_mut());
    let execution_request = match request.streamed_dispatches.take() {
        Some(streamed_dispatches) => {
            execution_request.with_streamed_dispatches(streamed_dispatches)
        }
        None => execution_request,
    };
    let execution = execute_actions(execution_request).await?;

    // 2b. Record how it turned out, the mirror of 1b and for the mirror-image reason. Attempts have
    // to be filed before execution so the repeat breaker sees the turn it is being asked about;
    // outcomes cannot be, because they do not exist until the calls have run. Filing them here
    // rather than inside the batch keeps one settled turn producing one set of records.
    //
    // What is recorded is what came back, classified: a call that failed and a call that succeeded
    // are the difference between a streak advancing and a streak clearing, and a call that never
    // ran is neither.
    request
        .tool_failure
        .record_turn(public_id, execution.outcomes().to_vec());

    // 3. Decide. One function, four states, priority written down once.
    let (next_step, final_output) = resolve_next_step_with_output(
        &processed,
        &execution,
        request.agent.public().tool_use_behavior(),
        request.cancel,
        &request.run,
    )
    .await?;

    // 4. Record. `new_step_items` and `session_step_items` start as the same list; only a transfer
    // of control below makes them diverge, through its projection and its filter.
    //
    // Recording reads the decision above, because R3-10's two channels are a fact about how the
    // turn settled rather than about what the provider wrote. Doing it here instead of in the
    // runner is what keeps one record from having two phases — a settled turn is what R9 stores
    // and what R12 attributes, and a copy the runner corrected afterwards would leave the
    // authoritative one saying something else.
    let items = step_items(&processed, &execution, request.agent.public(), &next_step);

    // The decision above was made before these records existed, so a pending decision it carries is
    // still the pre-settlement copy of one of them. Re-pointing it now is what makes this one
    // settled turn internally consistent: every consumer of `next_step` sees the same record the
    // session stores, instead of each having to know to go look it up.
    let next_step = rebind_interruption(next_step, &items, execution.nested_interruptions())?;

    // 5. Project, and only on the turn that transfers control. Everything above produced the
    // turn's records; this decides how much of them the *next* agent is shown and, when a filter
    // runs, which of them the turn stores. A declared projection only narrows the model view. A
    // filter's `new_items` are what the reference persists for the turn, while its `input_items`
    // narrow the model view alone.
    let projected = project_handoff_input(&request, &processed, &next_step, &items).await?;
    let items = match projected.session {
        Some((handed, kept)) => {
            // A filter's own messages land on the channel this turn settled on, like the rest.
            let stored =
                resolve_output_phases(filtered_turn_records(&items, 0, &handed, kept), &next_step);
            if let Some(recovery) = request.recovery.as_deref_mut() {
                recovery.unfiltered_records = Some(items);
            }
            stored
        }
        None => items,
    };
    let (input_history, pre_handoff_items, new_items) = projected.input.into_parts();

    SingleStepResult::builder()
        .original_input(input_history)
        .model_response(request.response.clone())
        .pre_step_items(pre_handoff_items)
        .new_step_items(new_items)
        .session_step_items(items)
        // Carried out of the turn that produced them rather than recorded here: the two trackers
        // above are run state this function was handed mutably, while these are a product of the
        // settlement, and the loop records them beside everything else the turn returned.
        .tool_input_guardrail_results(execution.tool_input_guardrail_results().to_vec())
        .tool_output_guardrail_results(execution.tool_output_guardrail_results().to_vec())
        .function_results(execution.function_results().to_vec())
        .final_output(final_output)
        .processed_response(processed)
        .next_step(next_step)
        .build()
}

/// What a transfer hands the receiving agent, and what its filter decided the turn stores.
pub(crate) struct ProjectedHandoff {
    /// The receiving agent's model input, before it is flattened.
    pub(crate) input: HandoffInputData,
    /// When a filter ran: the IDs of the turn records it was handed in `new_items`, and the
    /// `new_items` it returned.
    pub(crate) session: Option<(Vec<ItemId>, Vec<RunItem>)>,
}

/// Decides what the agent taking over is shown, and leaves every other turn untouched.
///
/// Two narrowings run here, in one order and never the other way round. The declared
/// [`HistoryProjection`](ra_core::agent::HistoryProjection) is applied first because it is the
/// ceiling the *declaration* wrote down; the filter then runs on whatever that produced, so a host
/// transform can narrow, reshape, or redact further but is never handed context the declaration
/// withheld — it does not receive it in the first place.
///
/// A per-handoff filter replaces the run-level one rather than running after it. Chaining them
/// would mean a host that configured a default for the run cannot exempt a single transfer from it,
/// which is the only reason to set one per handoff.
async fn project_handoff_input(
    request: &TurnSettlementRequest<'_>,
    processed: &ProcessedResponse,
    next_step: &NextStep,
    items: &[RunItem],
) -> Result<ProjectedHandoff> {
    let complete = HandoffInputData::new(
        request.original_input.clone(),
        request.pre_step_items.clone(),
        items.to_vec(),
    );
    if !matches!(next_step, NextStep::Handoff { .. }) {
        return Ok(ProjectedHandoff {
            input: complete,
            session: None,
        });
    }
    let Some(handoff) = processed.handoffs().first() else {
        return Err(Error::caller(
            "the turn settled as a transfer of control but bound no handoff to project history for",
        ));
    };

    filter_handoff_input(
        complete,
        handoff.spec(),
        handoff.call_id(),
        &request.run,
        request.handoff_input_filter.as_ref(),
        request.server_manages_conversation,
        request.cancel,
    )
    .await
}

/// Applies the selected declaration's projection and filter to ordinary or resumed input.
pub(crate) async fn filter_handoff_input(
    complete: HandoffInputData,
    spec: &ra_core::agent::HandoffSpec,
    call_id: &ra_core::item::CallId,
    run: &RunContext,
    default_filter: Option<&Arc<dyn HandoffInputFilter>>,
    server_manages_conversation: bool,
    cancel: &CancelScope,
) -> Result<ProjectedHandoff> {
    let projected = complete.project(spec.history_projection(), call_id)?;
    let filter = spec.input_filter().or(default_filter);
    if server_manages_conversation && filter.is_some() {
        return Err(Error::config(
            "server-managed conversations do not support handoff input filters",
        ));
    }
    match filter {
        None => Ok(ProjectedHandoff {
            input: projected,
            session: None,
        }),
        // Third-party `async` code, so it is never awaited bare — the cancellation contract's one
        // rule. The exit check is the same one the tool-stop policy makes for the same reason:
        // `CancelScope::run` polls the future before it looks at the cancellation, so a filter that
        // becomes ready in the wake-up that delivered the interrupt would otherwise hand a cancelled
        // run a fresh agent and a fresh input to spend on.
        Some(filter) => {
            let handed = projected
                .new_items()
                .iter()
                .map(|item| item.id().clone())
                .collect();
            let data = cancel.run(filter.filter(run, projected)).await??;
            cancel.ensure_not_cancelled()?;
            let kept = data.new_items().to_vec();
            Ok(ProjectedHandoff {
                input: data,
                session: Some((handed, kept)),
            })
        }
    }
}

/// The records a turn stores once a handoff filter has returned its `new_items`.
///
/// The reference persists the filter's `new_items` for the turn; its `input_items` only narrow the
/// model view. The filter governs the turn records it was handed that are model input and that the
/// session has not received yet — the first `locked` records are already stored and stay, as the
/// reference cannot unpersist an earlier write. Records a declared projection withheld from the
/// filter, and records that are not model input (such as approval questions), stay as they were.
/// The returned records replace the governed ones at the position of the first of them.
///
/// A governed call or output whose counterpart the filter removed is removed with it, as the
/// reference's held session writes keep each call paired. A call the session already holds stays
/// even when the filter removed its output: the reference leaves it unanswered in the session
/// rather than store the output the filter withheld, and model input drops unanswered calls.
pub(crate) fn filtered_turn_records(
    turn: &[RunItem],
    locked: usize,
    handed: &[ItemId],
    kept: Vec<RunItem>,
) -> Vec<RunItem> {
    let handed: HashSet<&ItemId> = handed.iter().collect();
    let governed = |index: usize, item: &RunItem| {
        index >= locked && item.is_model_input() && handed.contains(item.id())
    };
    // The filter's copy of a record it does not govern is ignored, and repeats are dropped.
    let protected: HashSet<ItemId> = turn
        .iter()
        .enumerate()
        .filter(|(index, item)| !governed(*index, item))
        .map(|(_, item)| item.id().clone())
        .collect();
    let mut present = protected.clone();
    let mut kept: Vec<RunItem> = kept
        .into_iter()
        .filter(|item| present.insert(item.id().clone()))
        .collect();
    let mut records = Vec::with_capacity(turn.len());
    let mut placed = false;
    for (index, item) in turn.iter().enumerate() {
        if !governed(index, item) {
            records.push(item.clone());
            continue;
        }
        if !placed {
            records.append(&mut kept);
            placed = true;
        }
    }
    records.append(&mut kept);

    let answered: HashSet<&CallId> = turn
        .iter()
        .filter_map(|item| {
            call_side(item)
                .filter(|(_, output)| *output)
                .map(|(id, _)| id)
        })
        .collect();
    let calls: HashSet<CallId> = records
        .iter()
        .filter_map(|item| call_side(item).filter(|(_, output)| !output))
        .map(|(id, _)| id.clone())
        .collect();
    let outputs: HashSet<CallId> = records
        .iter()
        .filter_map(|item| call_side(item).filter(|(_, output)| *output))
        .map(|(id, _)| id.clone())
        .collect();
    records
        .into_iter()
        .filter(|item| {
            protected.contains(item.id())
                || call_side(item).is_none_or(|(id, output)| {
                    if output {
                        calls.contains(id)
                    } else {
                        !answered.contains(id) || outputs.contains(id)
                    }
                })
        })
        .collect()
}

/// The call a record makes or answers, and whether it is the answer.
fn call_side(item: &RunItem) -> Option<(&CallId, bool)> {
    match item.kind() {
        RunItemKind::ToolCall(call) => Some((call.call_id(), false)),
        RunItemKind::HandoffCall(call) => Some((call.call_id(), false)),
        RunItemKind::ToolCallOutput(output) => Some((output.call_id(), true)),
        RunItemKind::HandoffOutput(output) => Some((output.call_id(), true)),
        _ => None,
    }
}
