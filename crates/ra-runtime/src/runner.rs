//! `Runner::run` and `run_streamed`: the body of the agent loop (R3-7).
//!
//! # One loop, not two
//!
//! Both entry points call [`run_loop`]. The streaming one differs by exactly one thing — it is
//! handed a channel to announce into — and that is the whole of the difference the reference
//! implementation spreads across two code paths. Two loops drift: the streamed one grows a fix the
//! other never gets, and the bug reproduces only when the host happens to subscribe.
//!
//! # What one turn is
//!
//! ```text
//! prepare_turn  ->  model call  ->  settle_turn  ->  NextStep
//!  (R3-0)                            (R3-4)           (R3-1)
//! ```
//!
//! The loop itself decides nothing about *whether* to continue: it matches [`NextStep`], which is
//! the single point R3-1 made control flow converge on. There is no `_` arm, so a fifth state is a
//! compile error here rather than a silently ignored one.
//!
//! # Deliberately absent
//!
//! **Budget policy beyond the shared value types.** Turns, tokens, and a wall-clock deadline are
//! enforced here through [`BudgetLimit`](ra_core::budget::BudgetLimit); spend is not a dimension,
//! because pricing is the host's. Provider refusal fallback and structured-output validation remain
//! at their provider and output-contract seams; when either produces a terminal [`Error`], they use
//! the same [`RunErrorHandler`] contract.
//!
//! **Session persistence and resume.** R6-6 turns a run into a `RunState`; R9 stores the items.
//! This produces the values both will read.

use std::{sync::Arc, time::Instant};

use ra_core::{
    budget::BudgetLimit,
    cancel::{CancelReason, CancelScope, Deadline, ScopeKind},
    error::{BudgetKind, Error, Result},
    finish::FinishReason,
    item::{
        ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind,
    },
    model::{ModelResolver, ModelSettings, ModelTracing},
    state::{RunState, WorkStateHandle},
    step::NextStep,
    tool::ToolRuntimeContext,
    trace::SpanKind,
};
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

pub mod result;
pub mod stream;

use result::aggregate_usage;
pub use result::{
    ContinuationInput, RunErrorData, RunErrorHandler, RunErrorHandlerInput, RunErrorHandlerResult,
    RunOutcome, RunResult,
};
pub use stream::{RunStream, RunStreamEvent};

use crate::{
    agent::AgentBinding,
    turn::{
        TurnSettlementRequest,
        batch::DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY,
        prepare::{PreparedTurn, TurnActionSurface, TurnPreparationRequest, prepare_turn},
        settle_turn,
    },
};

use crate::budget::budget_reminder;

/// Default turn cap.
///
/// It exists to stop a loop, not to size a task: a model that keeps calling the same tool has to
/// hit something. Sizing the work is what the rest of [`BudgetLimit`] is for.
pub const DEFAULT_MAX_TURNS: u32 = 32;

/// Run-level settings applied to every turn.
///
/// These are the outermost layer of the four-layer model-settings merge and the run-level model
/// override — the same two knobs [`prepare_turn`] already accepts, hoisted to run scope so a caller
/// does not re-supply them per turn and cannot supply them inconsistently.
#[must_use]
#[non_exhaustive]
#[derive(Clone)]
pub struct RunConfig {
    budget: BudgetLimit,
    max_function_tool_concurrency: usize,
    model: Option<String>,
    model_settings: ModelSettings,
    tracing: ModelTracing,
    error_handler: Option<Arc<dyn RunErrorHandler>>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl RunConfig {
    /// Creates the default configuration.
    pub fn new() -> Self {
        Self {
            budget: BudgetLimit::new().with_max_turns(DEFAULT_MAX_TURNS),
            max_function_tool_concurrency: DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY,
            model: None,
            model_settings: ModelSettings::new(),
            tracing: ModelTracing::Disabled,
            error_handler: None,
        }
    }

    /// Sets the turn cap. Zero is rejected when the run starts rather than silently meaning
    /// "unlimited".
    pub const fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.budget = self.budget.with_max_turns(max_turns);
        self
    }

    /// Replaces every budget dimension at once.
    ///
    /// The supplied limit must carry a turn cap. This is the one entry point that can drop the
    /// default one, and a loop with no cap of its own can only be stopped from outside — so a
    /// budget without one is rejected when the run starts rather than turning a scripted test or a
    /// looping model into a hang.
    pub const fn with_budget(mut self, budget: BudgetLimit) -> Self {
        self.budget = budget;
        self
    }

    /// Sets the total token ceiling while retaining the other configured dimensions.
    pub const fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.budget = self.budget.with_max_tokens(max_tokens);
        self
    }

    /// Sets the wall-clock deadline while retaining the other configured dimensions.
    ///
    /// **Requires a Tokio runtime with the time driver enabled.** The run arms a timer from it and
    /// cancels itself when it fires; without a time driver the deadline degrades to the scope's own
    /// checkpoint handling and takes effect later.
    pub const fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.budget = self.budget.with_deadline(deadline);
        self
    }

    /// Sets the maximum number of function-tool dispatch chains one turn may run at once.
    ///
    /// This caps the whole dispatch chain, rather than only a particular tool implementation, so
    /// a model response cannot exhaust file descriptors, child-process slots, or MCP connections
    /// by naming an unbounded number of `Parallel` tools. Zero is rejected when the run starts.
    pub const fn with_max_function_tool_concurrency(mut self, max: usize) -> Self {
        self.max_function_tool_concurrency = max;
        self
    }

    /// Overrides the agent's model selector for this run.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the run-override layer of model settings.
    pub fn with_model_settings(mut self, model_settings: ModelSettings) -> Self {
        self.model_settings = model_settings;
        self
    }

    /// Sets provider-side tracing visibility.
    pub const fn with_tracing(mut self, tracing: ModelTracing) -> Self {
        self.tracing = tracing;
        self
    }

    /// Installs the handler used to turn terminal budget conditions into a final delivery.
    pub fn with_error_handler(mut self, error_handler: Arc<dyn RunErrorHandler>) -> Self {
        self.error_handler = Some(error_handler);
        self
    }

    /// Budget dimensions governing this run.
    #[must_use]
    pub const fn budget(&self) -> &BudgetLimit {
        &self.budget
    }

    /// The configured turn cap.
    ///
    /// This compatibility accessor retains the original public signature. `0` means a caller
    /// replaced the budget with one that has no turn cap; [`Runner::run`] rejects that invalid
    /// configuration before making a model call.
    #[must_use]
    pub const fn max_turns(&self) -> u32 {
        match self.budget.max_turns() {
            Some(max_turns) => max_turns,
            None => 0,
        }
    }

    /// The per-turn function-tool dispatch cap.
    #[must_use]
    pub const fn max_function_tool_concurrency(&self) -> usize {
        self.max_function_tool_concurrency
    }
}

impl std::fmt::Debug for RunConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunConfig")
            .field("budget", &self.budget)
            .field(
                "max_function_tool_concurrency",
                &self.max_function_tool_concurrency,
            )
            .field("model", &self.model)
            .field("model_settings", &self.model_settings)
            .field("tracing", &self.tracing)
            .field("has_error_handler", &self.error_handler.is_some())
            .finish()
    }
}

/// Everything one run needs.
///
/// The fields are owned rather than borrowed because a run has to be **spawnable**: the streaming
/// entry drives it on a background task, and R12's sub-agents and R17's graph nodes will each want
/// a run they can hold. A borrowed request would make those callers restructure around a lifetime
/// that exists only to save one `Arc`.
#[must_use]
#[non_exhaustive]
pub struct RunRequest {
    agent: AgentBinding,
    model_resolver: Arc<dyn ModelResolver>,
    tool_context: Arc<dyn ToolRuntimeContext>,
    cancel: CancelScope,
    input: Vec<ModelInputItem>,
    config: RunConfig,
    state: RunState,
    work_state: Option<Arc<dyn WorkStateHandle>>,
}

impl RunRequest {
    /// Creates a request.
    ///
    /// `cancel` is required for the same reason it is in turn preparation and tool dispatch: every
    /// stage of the loop awaits third-party code, and a run with no scope is a run nobody can
    /// interrupt.
    pub fn new(
        agent: AgentBinding,
        model_resolver: Arc<dyn ModelResolver>,
        tool_context: Arc<dyn ToolRuntimeContext>,
        cancel: CancelScope,
        input: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            agent,
            model_resolver,
            tool_context,
            cancel,
            input,
            config: RunConfig::new(),
            state: RunState::new(),
            work_state: None,
        }
    }

    /// Sets the run-level configuration.
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = config;
        self
    }

    /// Continues from an earlier segment's [`RunState`].
    ///
    /// **The whole state, not a field of it.** Resuming with a fresh tracker would reset every
    /// repeat streak, which turns "pause and continue" into a way to defeat R3-6's loop breaker
    /// (R3-6b) — and a per-field entry point has that same failure mode waiting for every fact
    /// R3-8 and R6-6 add, since a caller who carried the fields they knew about would silently
    /// drop the rest. Set one field with
    /// [`RunState::with_tool_use`](ra_core::state::RunState::with_tool_use) and pass the result.
    pub fn with_state(mut self, state: RunState) -> Self {
        self.state = state;
        self
    }

    /// Attaches the task state this run participates in (R3-13).
    ///
    /// Held as a handle rather than a value: the task spans runs and, at R17, nodes, so a run that
    /// owned a copy would checkpoint a snapshot that goes stale as soon as anything else advances
    /// it. Every tool this run dispatches is handed the same handle, and R7's guards will be.
    pub fn with_work_state(mut self, work_state: Arc<dyn WorkStateHandle>) -> Self {
        self.work_state = Some(work_state);
        self
    }
}

/// The agent loop.
#[derive(Debug)]
pub struct Runner;

impl Runner {
    /// Runs the agent to completion, to an interruption, or to the turn cap.
    ///
    /// An interruption comes back as [`RunOutcome::Interrupted`], not as an error: the host is
    /// being asked a question, and a run that reported that as a failure would have no way to be
    /// answered and resumed.
    pub async fn run(request: RunRequest) -> Result<RunResult> {
        run_loop(request, None).await
    }

    /// Runs the agent, announcing each turn's records as they are produced.
    ///
    /// Same loop, same settlement. The run starts immediately on a background task and is
    /// cancelled if the returned [`RunStream`] is dropped — or if the [`RunStream::finish`] future
    /// is dropped part-way — rather than being left detached and still paying a provider.
    ///
    /// **Requires a Tokio runtime with the time driver enabled.** Cleanup gives a cancelled run
    /// [`DRAIN_GRACE`](ra_core::cancel::DRAIN_GRACE) to reach a terminal state before aborting it,
    /// and that grace period is a timer. Without one the abort backstop is lost; cancellation
    /// itself still reaches the run.
    #[must_use]
    pub fn run_streamed(mut request: RunRequest) -> RunStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        // The run gets its own child scope so dropping the stream cancels this run without
        // touching the caller's scope, while a cancellation from above still propagates down.
        let scope = request.cancel.child(ScopeKind::Run);
        request.cancel = scope.clone();
        let guard = scope.cancel_on_drop(CancelReason::UserInterrupt);
        let task = tokio::spawn(async move { run_loop(request, Some(sender)).await });
        RunStream::new(receiver, task, guard)
    }
}

/// Everything one turn reads but never changes.
///
/// It exists so the loop can live in its own function: the alternative is a dozen parameters, and
/// the alternative to *that* is one function long enough that the budget checks and the settlement
/// hand-off stop being visible together.
struct TurnLoopContext<'a> {
    model_resolver: &'a Arc<dyn ModelResolver>,
    tool_context: &'a Arc<dyn ToolRuntimeContext>,
    work_state: Option<&'a Arc<dyn WorkStateHandle>>,
    cancel: &'a CancelScope,
    closeout_cancel: &'a CancelScope,
    config: &'a RunConfig,
    original_input: &'a [ModelInputItem],
    events: Option<&'a mpsc::UnboundedSender<RunStreamEvent>>,
}

/// What the loop produces, whichever way it ends.
struct TurnLoopProgress {
    generated: Vec<RunItem>,
    model_responses: Vec<ModelResponse>,
    turns: u32,
    budget_stop: Option<BudgetKind>,
}

/// The loop both entry points share.
///
/// `events` is the only difference between them.
async fn run_loop(
    request: RunRequest,
    events: Option<mpsc::UnboundedSender<RunStreamEvent>>,
) -> Result<RunResult> {
    // The name is the one the run starts with. A handoff replaces the running agent mid-loop, and
    // this span keeps the original name because it is the whole run's span — per-agent attribution
    // is what a handoff span and the agent span its target opens are for, and neither exists while
    // settlement still refuses handoffs.
    let agent_name = request.agent.public().name().to_owned();
    let agent_span = info_span!(
        "agent",
        span.kind = SpanKind::Agent.label(),
        agent.name = %agent_name,
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        cancel.reason = tracing::field::Empty,
        cancel.scope = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
        finish.reason = tracing::field::Empty,
        budget.kind = tracing::field::Empty,
        usage.input_tokens = tracing::field::Empty,
        usage.cached_input_tokens = tracing::field::Empty,
        usage.cache_write_tokens = tracing::field::Empty,
        usage.output_tokens = tracing::field::Empty,
        usage.reasoning_tokens = tracing::field::Empty,
    );
    let started = Instant::now();
    let result = run_loop_inner(request, events, &agent_span)
        .instrument(agent_span.clone())
        .await;
    agent_span.record(
        ra_core::trace::field::DURATION_MS,
        duration_ms(started.elapsed()),
    );
    result
}

/// Runs the agent loop after the outer agent span has been installed.
///
/// The span is passed in as well as installed: the terminal facts are recorded here, where the run
/// scope that explains a cancellation is still alive, rather than at the caller, which sees only
/// an `Error` and would have to guess the level a cancellation came from.
async fn run_loop_inner(
    request: RunRequest,
    events: Option<mpsc::UnboundedSender<RunStreamEvent>>,
    span: &tracing::Span,
) -> Result<RunResult> {
    let RunRequest {
        mut agent,
        model_resolver,
        tool_context,
        cancel,
        input: original_input,
        config,
        mut state,
        work_state,
    } = request;

    if let Err(error) = validate_config(&config) {
        // Ahead of the run scope, so there is no cancellation to attribute and no aggregate to
        // report — but the span still has to say the run ended and why.
        ra_core::trace::record_error(span, &error);
        return Err(error);
    }

    // The run gets its own scope, so either its configured deadline or an inherited caller
    // deadline stops this run without cancelling the caller's tree. An armed timer turns the
    // effective deadline — pure data in `ra-core` — into a real cancellation. Every descendant
    // sees it at the same instant: the model call in flight, the tools running under settlement,
    // and the loop's own checkpoint alike.
    let budget_deadline = config.budget.deadline();
    let closeout_cancel = cancel.child(ScopeKind::Run);
    let cancel = cancel.child(ScopeKind::Run);
    let cancel = match budget_deadline {
        Some(deadline) => cancel.with_deadline(deadline),
        None => cancel,
    };
    let _deadline = arm_deadline(&cancel);

    let context = TurnLoopContext {
        model_resolver: &model_resolver,
        tool_context: &tool_context,
        work_state: work_state.as_ref(),
        cancel: &cancel,
        closeout_cancel: &closeout_cancel,
        config: &config,
        original_input: &original_input,
        events: events.as_ref(),
    };
    let mut progress = TurnLoopProgress {
        generated: Vec::new(),
        model_responses: Vec::new(),
        turns: 0,
        budget_stop: None,
    };

    // The one place an expired wall clock is read back as a budget stop. Everything under the run
    // scope reports expiry the same way any other cancellation is reported, which is what lets the
    // loop stay free of deadline special cases; translating it here — rather than at each of the
    // four `?` inside — is why exactly one kind of stop can be a soft one.
    let outcome = match run_turns(&context, &mut agent, &mut state, &mut progress).await {
        Ok(outcome) => outcome,
        Err(error) if is_wall_clock_expiry(&error, &cancel, budget_deadline) => {
            progress.budget_stop = Some(BudgetKind::WallClock);
            RunOutcome::Completed {
                reason: FinishReason::BudgetExhausted,
            }
        }
        Err(error) => {
            record_progress_usage(span, &progress);
            record_terminal_error(span, &error, &cancel);
            return Err(error);
        }
    };

    // Which allowance ran out, recorded next to the finish reason rather than folded into it:
    // `FinishReason` maps tokens, cost and wall clock all onto `budget_exhausted`, so without this
    // a cost report cannot tell an expensive run from a slow one.
    if let Some(kind) = progress.budget_stop {
        span.record(ra_core::trace::field::BUDGET_KIND, kind.code());
    }

    let final_message = match deliver_budget_closeout(&context, &agent, &mut progress, &state).await
    {
        Ok(message) => message,
        Err(error) => {
            record_progress_usage(span, &progress);
            record_terminal_error(span, &error, &closeout_cancel);
            return Err(error);
        }
    };

    let mut result = RunResult::new(
        outcome.clone(),
        Arc::clone(agent.public()),
        original_input,
        progress.generated,
        progress.model_responses,
        progress.turns,
        state,
    );
    if let Some(message) = final_message {
        result = result.with_final_message(message);
    }
    record_run_outcome(span, &result);
    emit(events.as_ref(), RunStreamEvent::Finished(outcome));
    Ok(result)
}

/// Records the run-level aggregate without creating a second accounting source.
///
/// These are the same field names a `generation` uses one level down, and the scale is the
/// difference: this is the whole run, that is one request. Reports group by `span.kind` before
/// summing — see the note in `ra_core::trace::field`.
fn record_run_outcome(span: &tracing::Span, result: &RunResult) {
    record_usage(span, &result.usage());
    if let Some(reason) = result.outcome().finish_reason() {
        span.record(ra_core::trace::field::FINISH_REASON, reason.code());
    }
    ra_core::trace::record_outcome(span, ra_core::trace::SpanOutcome::Ok);
}

/// Records usage already paid by model calls when no [`RunResult`] will be returned.
///
/// Through the same summation [`RunResult::usage`] uses, so a run that failed and a run that
/// finished report the calls they made the same way.
fn record_progress_usage(span: &tracing::Span, progress: &TurnLoopProgress) {
    record_usage(span, &aggregate_usage(&progress.model_responses));
}

/// Records normalized usage on any span whose scale is defined by its kind.
fn record_usage(span: &tracing::Span, usage: &ra_core::usage::Usage) {
    span.record(
        ra_core::trace::field::USAGE_INPUT_TOKENS,
        usage.input_tokens(),
    );
    span.record(
        ra_core::trace::field::USAGE_CACHED_INPUT_TOKENS,
        usage.cached_input_tokens(),
    );
    span.record(
        ra_core::trace::field::USAGE_CACHE_WRITE_TOKENS,
        usage.cache_write_tokens(),
    );
    span.record(
        ra_core::trace::field::USAGE_OUTPUT_TOKENS,
        usage.output_tokens(),
    );
    span.record(
        ra_core::trace::field::USAGE_REASONING_TOKENS,
        usage.reasoning_tokens(),
    );
}

/// Records a failed span, attributing a cancellation to the level that raised it.
///
/// The scope is asked rather than the error: `Error::Cancelled` carries a display string, while
/// the root cause and the initiating level are the machine-readable pair the cancellation contract
/// designates. Without the level, a run stopped by its own deadline and one stopped by the
/// caller's interrupt are the same line in a report.
fn record_terminal_error(span: &tracing::Span, error: &Error, scope: &CancelScope) {
    match (
        error.is_cancelled(),
        scope.reason(),
        scope.cancelled_scope(),
    ) {
        (true, Some(reason), Some(kind)) => {
            span.record(ra_core::trace::field::ERROR_CODE, error.code());
            ra_core::trace::record_cancel(span, &reason, &kind);
        }
        _ => ra_core::trace::record_error(span, error),
    }
}

/// Runs turns until something says to stop.
async fn run_turns(
    context: &TurnLoopContext<'_>,
    agent: &mut AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
) -> Result<RunOutcome> {
    let config = context.config;
    let outcome = loop {
        // Checked before the budget so a cancelled run reports as cancelled rather than as having
        // exhausted its turns — the reason a host reacts to is different for each.
        context.cancel.ensure_not_cancelled()?;

        if let Some(kind) = state.budget().exhausted_kind(config.budget()) {
            progress.budget_stop = Some(kind);
            break RunOutcome::Completed {
                reason: FinishReason::from_budget_kind(kind),
            };
        }
        progress.turns += 1;
        state.budget_mut().record_turn();
        emit(
            context.events,
            RunStreamEvent::TurnStarted {
                turn: progress.turns,
                agent: agent.public_id().clone(),
            },
        );

        // One scope per turn, so anything that must not outlive this turn has somewhere to attach
        // without the run's own scope inheriting it.
        let turn_scope = context.cancel.child(ScopeKind::Turn);

        // The level that answers "what did this turn cost". A turn is not one model call: it can
        // hold retries, a model fallback, and a whole batch of tools, so the generation and
        // function spans underneath have to roll up somewhere before the run total. Opened only
        // once the turn is certain to run — the budget break above ends the loop without one.
        //
        // The stream event counts turns from 1 for a host to display; the trace field is defined
        // from 0, and the conversion is here rather than at either definition.
        let turn_span = info_span!(
            "turn",
            span.kind = SpanKind::Turn.label(),
            turn.index = progress.turns - 1,
            outcome = tracing::field::Empty,
            error.code = tracing::field::Empty,
            cancel.reason = tracing::field::Empty,
            cancel.scope = tracing::field::Empty,
            duration.ms = tracing::field::Empty,
            usage.input_tokens = tracing::field::Empty,
            usage.cached_input_tokens = tracing::field::Empty,
            usage.cache_write_tokens = tracing::field::Empty,
            usage.output_tokens = tracing::field::Empty,
            usage.reasoning_tokens = tracing::field::Empty,
        );
        let turn_started = Instant::now();
        let step = run_one_turn(context, agent, state, progress, &turn_scope, &turn_span)
            .instrument(turn_span.clone())
            .await;
        turn_span.record(
            ra_core::trace::field::DURATION_MS,
            duration_ms(turn_started.elapsed()),
        );
        match &step {
            Ok(_) => ra_core::trace::record_outcome(&turn_span, ra_core::trace::SpanOutcome::Ok),
            Err(error) => record_terminal_error(&turn_span, error, &turn_scope),
        }

        // A turn that reached a conclusion ends the loop with it; anything else means another
        // turn. The two states stay `Option` rather than becoming a second control-flow enum:
        // `NextStep` is the one that names what a turn decided, and it is answered inside.
        match step? {
            None => {}
            Some(outcome) => break outcome,
        }
    };
    Ok(outcome)
}

/// Runs one turn: prepare, call the model, settle, and say whether the run continues.
///
/// `None` means another turn; `Some` carries the outcome the run ends with.
async fn run_one_turn(
    context: &TurnLoopContext<'_>,
    agent: &mut AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
    turn_scope: &CancelScope,
    turn_span: &tracing::Span,
) -> Result<Option<RunOutcome>> {
    let config = context.config;
    let input = next_input(
        context.original_input,
        &progress.generated,
        budget_reminder(state.budget(), config.budget()),
    );
    let mut preparation = TurnPreparationRequest::new(
        agent,
        context.model_resolver.as_ref(),
        context.tool_context.as_ref(),
        turn_scope,
        state.tool_use(),
        input,
    )
    .with_model_settings(config.model_settings.clone())
    .with_tracing(config.tracing);
    if let Some(model) = &config.model {
        preparation = preparation.with_model(model.clone());
    }
    let prepared = prepare_turn(preparation).await?;

    let (surface, response) = call_model(turn_scope, prepared).await?;

    // Both facts about a completed call are recorded here, before settlement, and the stop
    // either may cause is *not* taken here. The response has already been paid for, so its
    // items belong in history and its tool calls belong to the turn that requested them; the
    // check at the top of the next iteration is what ends the run, one turn later and with the
    // work intact.
    //
    // Recording before settling is also what keeps the two facts agreeing when a deadline
    // interrupts settlement below: the session projection may then be incomplete, but usage
    // accounting, provider diagnostics, and an error handler's snapshot must not deny that the
    // call ran. The copy is what that costs, next to the two history copies this turn already
    // makes for settlement.
    record_usage(turn_span, response.usage());
    state.budget_mut().record_usage(response.usage());
    progress.model_responses.push(response.clone());

    let (tool_use, tool_failure) = state.trackers_mut();
    let mut settlement = TurnSettlementRequest::new(
        agent,
        &response,
        &surface,
        Arc::clone(context.tool_context),
        turn_scope,
        tool_use,
        tool_failure,
    )
    .with_original_input(context.original_input.to_vec())
    .with_pre_step_items(progress.generated.clone());
    if let Some(work_state) = context.work_state {
        settlement = settlement.with_work_state(Arc::clone(work_state));
    }
    settlement =
        settlement.with_max_function_tool_concurrency(config.max_function_tool_concurrency);
    let settled = settle_turn(settlement).await?;

    for item in settled.session_step_items() {
        emit(context.events, RunStreamEvent::Item(item.clone()));
    }
    progress
        .generated
        .extend(settled.session_step_items().iter().cloned());

    // No `_` arm, deliberately. R3-1 made this the one place control flow converges, and a
    // fifth state has to be answered here rather than fall through to "keep going".
    match settled.next_step() {
        NextStep::RunAgain => Ok(None),
        NextStep::FinalOutput { reason } => Ok(Some(RunOutcome::Completed { reason: *reason })),
        NextStep::Interruption { items } => Ok(Some(RunOutcome::Interrupted {
            items: items.clone(),
        })),
        // Control transfers to another agent, which speaks next. The new agent arrives as a
        // public declaration, so it binds directly: whatever prepared *this* turn's execution
        // instance has no say over who runs the next one. Unreachable until R17 — settlement
        // refuses handoffs — but the state machine has to say what it does about it.
        NextStep::Handoff { new_agent } => {
            *agent = AgentBinding::direct(Arc::clone(new_agent));
            Ok(None)
        }
    }
}

/// Executes one prepared model call and records its provider-neutral terminal facts.
async fn call_model(
    turn_scope: &CancelScope,
    prepared: PreparedTurn,
) -> Result<(TurnActionSurface, ModelResponse)> {
    let model = Arc::clone(prepared.model());
    let selector = prepared.selector().clone();
    let (surface, model_request) = prepared.into_call();
    let model_name = selector.model().unwrap_or("<provider_default>");
    let generation_span = info_span!(
        "generation",
        span.kind = SpanKind::Generation.label(),
        model.name = %model_name,
        model.provider = %selector.provider(),
        gen.protocol = %selector.protocol(),
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        cancel.reason = tracing::field::Empty,
        cancel.scope = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
        usage.input_tokens = tracing::field::Empty,
        usage.cached_input_tokens = tracing::field::Empty,
        usage.cache_write_tokens = tracing::field::Empty,
        usage.output_tokens = tracing::field::Empty,
        usage.reasoning_tokens = tracing::field::Empty,
    );
    let started = Instant::now();
    let response = async { turn_scope.run(model.get_response(model_request)).await }
        .instrument(generation_span.clone())
        .await
        .and_then(|response| response);
    generation_span.record(
        ra_core::trace::field::DURATION_MS,
        duration_ms(started.elapsed()),
    );
    match response {
        Ok(response) => {
            record_generation_usage(&generation_span, response.usage());
            ra_core::trace::record_outcome(&generation_span, ra_core::trace::SpanOutcome::Ok);
            Ok((surface, response))
        }
        Err(error) => {
            record_terminal_error(&generation_span, &error, turn_scope);
            Err(error)
        }
    }
}

/// Records the normalized per-request usage preserved by the model response.
fn record_generation_usage(span: &tracing::Span, usage: &ra_core::usage::Usage) {
    record_usage(span, usage);
}

/// Asks the configured handler to turn an exhausted budget into something deliverable.
async fn deliver_budget_closeout(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    progress: &mut TurnLoopProgress,
    state: &RunState,
) -> Result<Option<Message>> {
    let (Some(kind), Some(handler)) = (progress.budget_stop, context.config.error_handler.as_ref())
    else {
        return Ok(None);
    };

    let error = Error::budget(kind, "the configured run budget was exhausted");
    let data = RunErrorData::new(
        agent.public(),
        context.original_input,
        &progress.generated,
        &progress.model_responses,
        progress.turns,
        state.budget().clone(),
    );
    // A deadline cancels the run scope, so this uses its sibling: budget exhaustion still gets a
    // chance to produce a closeout, while an interrupt on the caller's scope cancels both paths.
    // A deadline the caller put on its *own* scope reaches this sibling too, so a run that exhausts
    // its budget at the moment the caller's clock also runs out ends as a cancellation with nothing
    // delivered. That is the caller's limit winning, which is the same order every other path uses.
    let Some(closeout) = context
        .closeout_cancel
        .run(handler.handle(RunErrorHandlerInput::new(&error, data)))
        .await??
    else {
        // Declined. The run ends the way it would have with no handler at all.
        return Ok(None);
    };
    validate_error_handler_message(closeout.message())?;

    let message = closeout.message().clone();
    if closeout.write_to_history() {
        let item = RunItem::new(
            next_error_item_id(&progress.generated, progress.turns),
            RunItemKind::Message(message.clone()),
        );
        emit(context.events, RunStreamEvent::Item(item.clone()));
        progress.generated.push(item);
    } else {
        emit(
            context.events,
            RunStreamEvent::FinalMessage(message.clone()),
        );
    }
    Ok(Some(message))
}

/// Checks that a closeout can stand where the model's own answer would have stood.
///
/// Shape is all this can check today, because an agent cannot yet declare an output schema.
/// **Once one can, a closeout has to satisfy it here too** — otherwise a run whose contract
/// promises structured output can end with free text that every caller parsing
/// [`RunResult::final_message`] chokes on, and it will have arrived through the framework's own
/// delivery path rather than the model's.
fn validate_error_handler_message(message: &Message) -> Result<()> {
    if message.role() != MessageRole::Assistant || message.phase() != Some(OutputPhase::Final) {
        return Err(Error::caller(
            "a run error handler must return an assistant message with final output phase",
        ));
    }
    Ok(())
}

fn next_error_item_id(items: &[RunItem], turns: u32) -> ItemId {
    let mut suffix = 0_u32;
    loop {
        let id = ItemId::new(format!("run-error-{turns}-{suffix}"));
        if !items.iter().any(|item| item.id() == &id) {
            return id;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn validate_config(config: &RunConfig) -> Result<()> {
    config.budget.validate()?;
    if config.budget.max_turns().is_none() {
        return Err(Error::config(
            "`max_turns` must be set; it is the loop's own termination condition, and a run that \
             can only be stopped from outside is a hang whenever the model keeps calling tools",
        ));
    }
    if config.max_function_tool_concurrency == 0 {
        return Err(Error::config(
            "`max_function_tool_concurrency` must be at least 1; zero would permanently queue \
             every tool call",
        ));
    }
    Ok(())
}

/// Cancels its scope when the run's wall clock expires; aborts the timer when dropped.
///
/// Dropping matters as much as firing: a finished run must not go on to cancel anything, and the
/// scope it holds outlives this task by design.
struct DeadlineTimer(tokio::task::JoinHandle<()>);

impl Drop for DeadlineTimer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Arms the timer that converts `scope`'s deadline into a real cancellation.
///
/// `ra-core` owns no runtime and so arms no timer; this is the one place that obligation is met.
/// Without it a deadline still takes effect, but only at the next checkpoint that happens to look —
/// which a model call or a long tool can postpone indefinitely.
fn arm_deadline(scope: &CancelScope) -> Option<DeadlineTimer> {
    let deadline = scope.deadline()?;
    let scope = scope.clone();
    Some(DeadlineTimer(tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.instant())).await;
        scope.cancel(CancelReason::Deadline);
    })))
}

/// Whether a failure is this run's wall clock expiring rather than an interruption.
///
/// The reason is read from the scope, never parsed out of the error text: `CancelReason` is the
/// machine-readable attribution the cancellation contract designates, and `Error::Cancelled`
/// carries only a display string. A tool's own timeout cancels the tool's scope, not this one, so
/// it stays a failure — as it should.
///
/// The configured budget deadline is checked separately from the effective scope deadline.
/// `CancelScope::deadline` includes an inherited caller deadline, which must remain cancellation
/// rather than being relabelled as this run's exhausted budget.
fn is_wall_clock_expiry(
    error: &Error,
    scope: &CancelScope,
    budget_deadline: Option<Deadline>,
) -> bool {
    error.is_cancelled()
        && budget_deadline.is_some_and(Deadline::is_expired)
        && scope
            .reason()
            .is_some_and(|reason| matches!(reason, CancelReason::Deadline))
}

/// Converts an elapsed duration to the stable trace unit without truncating a very long run.
fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Builds the next call's input: what was asked, then everything produced since, then `reminder`.
///
/// Projection happens here rather than being accumulated as the run goes, so the model-facing view
/// is always derived from the authoritative records. Adapters normalize it again before sending
/// (R1-17); this stage only decides *which* records go.
///
/// A reminder goes **last and is not a record**: it never enters `generated`, so it is regenerated
/// from the current allowance each turn and leaves no trace in session history. Last is also the
/// only position that costs nothing, since everything ahead of it stays byte-identical between
/// turns and remains eligible for the provider's prefix cache.
fn next_input(
    original_input: &[ModelInputItem],
    generated: &[RunItem],
    reminder: Option<Message>,
) -> Vec<ModelInputItem> {
    let mut input = original_input.to_vec();
    input.extend(generated.iter().filter_map(RunItem::to_model_input));
    input.extend(reminder.map(ModelInputItem::Message));
    input
}

/// Announces one event, ignoring a receiver that has gone away.
///
/// A dropped receiver is not a run failure. The host stopping listening is handled by cancelling
/// the run when the stream drops, which is a decision about the run rather than about one send.
fn emit(events: Option<&mpsc::UnboundedSender<RunStreamEvent>>, event: RunStreamEvent) {
    if let Some(sender) = events {
        let _ = sender.send(event);
    }
}
