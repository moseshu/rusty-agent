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
//! **Budgets other than the turn cap.** Cost, tokens, wall clock, and telling the model how much
//! allowance is left are R3-8's, along with the error handler that turns a refusal or an invalid
//! final output into a settled result. [`RunConfig::max_turns`] is here anyway because it is not a
//! budget — it is the loop's own termination condition, and shipping a loop that can only be
//! stopped from outside would make every other milestone's tests a hang risk.
//!
//! **Session persistence and resume.** R6-6 turns a run into a `RunState`; R9 stores the items.
//! This produces the values both will read.

use std::sync::Arc;

use ra_core::{
    cancel::{CancelReason, CancelScope, ScopeKind},
    error::{Error, Result},
    finish::FinishReason,
    item::{ModelInputItem, ModelResponse, RunItem},
    model::{ModelResolver, ModelSettings, ModelTracing},
    state::ToolUseTracker,
    step::NextStep,
    tool::ToolRuntimeContext,
};
use tokio::sync::mpsc;

pub mod result;
pub mod stream;

pub use result::{ContinuationInput, RunOutcome, RunResult};
pub use stream::{RunStream, RunStreamEvent};

use crate::{
    agent::AgentBinding,
    turn::{
        TurnSettlementRequest,
        prepare::{TurnPreparationRequest, prepare_turn},
        settle_turn,
    },
};

/// Default turn cap.
///
/// It exists to stop a loop, not to size a task: a model that keeps calling the same tool has to
/// hit something. Sizing the work is R3-8's job, with budgets the model can be told about.
pub const DEFAULT_MAX_TURNS: u32 = 32;

/// Run-level settings applied to every turn.
///
/// These are the outermost layer of the four-layer model-settings merge and the run-level model
/// override — the same two knobs [`prepare_turn`] already accepts, hoisted to run scope so a caller
/// does not re-supply them per turn and cannot supply them inconsistently.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunConfig {
    max_turns: u32,
    model: Option<String>,
    model_settings: ModelSettings,
    tracing: ModelTracing,
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
            max_turns: DEFAULT_MAX_TURNS,
            model: None,
            model_settings: ModelSettings::new(),
            tracing: ModelTracing::Disabled,
        }
    }

    /// Sets the turn cap. Zero is rejected when the run starts rather than silently meaning
    /// "unlimited".
    pub const fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
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

    /// The turn cap.
    #[must_use]
    pub const fn max_turns(&self) -> u32 {
        self.max_turns
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
    tool_use: ToolUseTracker,
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
            tool_use: ToolUseTracker::new(),
        }
    }

    /// Sets the run-level configuration.
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = config;
        self
    }

    /// Continues with tool-use history from an earlier run segment.
    ///
    /// Resuming with a fresh tracker would reset every repeat streak, which turns "pause and
    /// continue" into a way to defeat R3-6's loop breaker (R3-6b).
    pub fn with_tool_use(mut self, tool_use: ToolUseTracker) -> Self {
        self.tool_use = tool_use;
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

/// The loop both entry points share.
///
/// `events` is the only difference between them.
async fn run_loop(
    request: RunRequest,
    events: Option<mpsc::UnboundedSender<RunStreamEvent>>,
) -> Result<RunResult> {
    let RunRequest {
        mut agent,
        model_resolver,
        tool_context,
        cancel,
        input: original_input,
        config,
        mut tool_use,
    } = request;

    if config.max_turns == 0 {
        return Err(Error::config(
            "`max_turns` must be at least 1; a run that may not take a turn cannot produce \
             anything, and zero is too easy to reach by arithmetic on a caller's own budget",
        ));
    }

    let mut generated: Vec<RunItem> = Vec::new();
    let mut model_responses: Vec<ModelResponse> = Vec::new();
    let mut turns: u32 = 0;

    let outcome = loop {
        // Checked before the turn cap so a cancelled run reports as cancelled rather than as
        // having exhausted its turns — the reason a host reacts to is different for each.
        cancel.ensure_not_cancelled()?;

        if turns >= config.max_turns {
            break RunOutcome::Completed {
                reason: FinishReason::MaxTurns,
            };
        }
        turns += 1;
        emit(
            events.as_ref(),
            RunStreamEvent::TurnStarted {
                turn: turns,
                agent: agent.public_id().clone(),
            },
        );

        // One scope per turn, so a per-turn deadline (R3-8) has somewhere to attach without the
        // run's own scope inheriting it.
        let turn_scope = cancel.child(ScopeKind::Turn);

        let input = next_input(&original_input, &generated);
        let mut preparation = TurnPreparationRequest::new(
            &agent,
            model_resolver.as_ref(),
            tool_context.as_ref(),
            &turn_scope,
            input,
        )
        .with_model_settings(config.model_settings.clone())
        .with_tracing(config.tracing);
        if let Some(model) = &config.model {
            preparation = preparation.with_model(model.clone());
        }
        let prepared = prepare_turn(preparation).await?;

        let model = Arc::clone(prepared.model());
        let (surface, model_request) = prepared.into_call();

        // R1-7's insertion point: the streaming model call and its delta forwarding replace this
        // line without moving anything else, because settlement consumes a terminal
        // `ModelResponse` either way.
        let response = turn_scope.run(model.get_response(model_request)).await??;

        let settled = settle_turn(
            TurnSettlementRequest::new(
                &agent,
                &response,
                &surface,
                tool_context.as_ref(),
                &turn_scope,
                &mut tool_use,
            )
            .with_original_input(original_input.clone())
            .with_pre_step_items(generated.clone()),
        )
        .await?;

        model_responses.push(response);
        for item in settled.session_step_items() {
            emit(events.as_ref(), RunStreamEvent::Item(item.clone()));
        }
        generated.extend(settled.session_step_items().iter().cloned());

        // No `_` arm, deliberately. R3-1 made this the one place control flow converges, and a
        // fifth state has to be answered here rather than fall through to "keep going".
        match settled.next_step() {
            NextStep::RunAgain => {}
            NextStep::FinalOutput { reason } => break RunOutcome::Completed { reason: *reason },
            NextStep::Interruption { items } => {
                break RunOutcome::Interrupted {
                    items: items.clone(),
                };
            }
            // Control transfers to another agent, which speaks next. The new agent arrives as a
            // public declaration, so it binds directly: whatever prepared *this* turn's execution
            // instance has no say over who runs the next one. Unreachable until R17 — settlement
            // refuses handoffs — but the state machine has to say what it does about it.
            NextStep::Handoff { new_agent } => {
                agent = AgentBinding::direct(Arc::clone(new_agent));
            }
        }
    };

    let result = RunResult::new(
        outcome.clone(),
        Arc::clone(agent.public()),
        original_input,
        generated,
        model_responses,
        turns,
        tool_use,
    );
    emit(events.as_ref(), RunStreamEvent::Finished(outcome));
    Ok(result)
}

/// Builds the next call's input: what was asked, then everything produced since.
///
/// Projection happens here rather than being accumulated as the run goes, so the model-facing view
/// is always derived from the authoritative records. Adapters normalize it again before sending
/// (R1-17); this stage only decides *which* records go.
fn next_input(original_input: &[ModelInputItem], generated: &[RunItem]) -> Vec<ModelInputItem> {
    let mut input = original_input.to_vec();
    input.extend(generated.iter().filter_map(RunItem::to_model_input));
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
