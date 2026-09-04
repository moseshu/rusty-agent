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

use std::{any::Any, collections::BTreeSet, sync::Arc, time::Instant};

use futures::StreamExt;
use ra_core::{
    agent::AgentSpec,
    budget::BudgetLimit,
    cancel::{CancelReason, CancelScope, Deadline, ScopeKind},
    capability::{
        Capability, ContextProcessor, ContextProcessorRequest, ContextSummarizer,
        ContextSummaryRequest, ContextSummaryResponse, LoadSignal,
    },
    context::RunContext,
    error::{BudgetKind, Error, ProviderErrorKind, Result},
    filter::{
        ContextFilter, ContextFilterChain, ContextFilterReport, ContextFilterRequest,
        ModelInputData,
    },
    finish::FinishReason,
    item::{
        ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolApproval, ToolCallOutput,
    },
    model::{
        Model, ModelRequest, ModelResolver, ModelRetryAdviceRequest, ModelSettings,
        ModelStreamEvent, ModelTracing, ReplaySafety, RetryAdvice, RetryBackoff, RetryDecision,
        RetryPolicyContext, replay_safety_of, stamp_replay_safety,
    },
    permission::{PermissionMode, PermissionRule},
    state::{EventSeqAllocator, InterruptionResolution, RunId, RunState, ToolOutcome, ToolUse},
    step::NextStep,
    tool::{ToolOutputReferenceExtractor, ToolServices},
    trace::SpanKind,
    usage::{RequestUsage, Usage},
};
use tokio::sync::mpsc;
use tracing::{Instrument, info_span, warn};

pub mod result;
pub mod stream;

pub use crate::turn::prepare::ActionSurfaceBudget;

pub use result::{
    ContinuationInput, RunErrorData, RunErrorHandler, RunErrorHandlerInput, RunErrorHandlerResult,
    RunOutcome, RunResult, TurnRecord,
};
use result::{TurnRecordOwner, aggregate_usage};
pub use stream::{RunStream, RunStreamEvent};

use crate::{
    agent::AgentBinding,
    capability::{CapabilityPlan, DeferredPrompt},
    permission::PermissionEngine,
    tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
    turn::{
        TurnSettlementRequest,
        batch::{DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY, StreamedFunctionDispatches, StreamedStart},
        prepare::{
            PreparedTurn, ToolNameCollisionPolicy, TurnActionSurface, TurnPreparationRequest,
            prepare_turn,
        },
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
    partial_messages: bool,
    tool_name_collision_policy: ToolNameCollisionPolicy,
    action_surface_budget: ActionSurfaceBudget,
    context_filters: ContextFilterChain,
    tool_output_reference_extractor: Option<Arc<dyn ToolOutputReferenceExtractor>>,
    context_processors: Vec<Arc<dyn ContextProcessor>>,
    capabilities: Vec<Arc<dyn Capability>>,
    permission: PermissionEngine,
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
            partial_messages: false,
            tool_name_collision_policy: ToolNameCollisionPolicy::Warn,
            action_surface_budget: ActionSurfaceBudget::default(),
            context_filters: ContextFilterChain::new(),
            tool_output_reference_extractor: None,
            context_processors: Vec::new(),
            capabilities: Vec::new(),
            permission: PermissionEngine::default(),
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

    /// Forwards provider events while each model call streams, so a host can render a turn as it
    /// is produced.
    ///
    /// Function calls are always read from the model stream: a completed call starts immediately,
    /// while the provider is still generating later items. This switch controls only whether raw
    /// provider narration is exposed to a [`RunStream`]; it has no effect on tool dispatch or on
    /// the terminal response that settles the turn.
    ///
    /// Off by default, and not out of timidity. Narration that reaches a subscriber cannot be
    /// unsent, so forwarding it closes this call's retry window — a run that renders nothing gives
    /// up replayable failures for output nobody reads.
    ///
    /// **It takes effect only on [`Runner::run_streamed`].** [`Runner::run`] has no subscriber to
    /// forward to.
    pub const fn with_partial_messages(mut self, partial_messages: bool) -> Self {
        self.partial_messages = partial_messages;
        self
    }

    /// Selects whether an ambiguous model-facing action name is retained with a warning or
    /// rejected before the model call.
    pub const fn with_tool_name_collision_policy(
        mut self,
        tool_name_collision_policy: ToolNameCollisionPolicy,
    ) -> Self {
        self.tool_name_collision_policy = tool_name_collision_policy;
        self
    }

    /// Sets the per-turn budget for the complete model-facing action table.
    ///
    /// This budget is applied after dynamic availability and includes function tools and
    /// handoffs. It is distinct from a tool profile's selection budget because a profile does not
    /// own handoff declarations.
    pub const fn with_action_surface_budget(
        mut self,
        action_surface_budget: ActionSurfaceBudget,
    ) -> Self {
        self.action_surface_budget = action_surface_budget;
        self
    }

    /// Appends a pure projection applied to the model input before every request.
    ///
    /// A filter sees a persisted tool-output reference ledger and affects only the provider
    /// request; `RunState` retains the complete session records for resume and storage.
    ///
    /// Filters run in install order, after every context processor, and each one is measured
    /// separately — see [`ContextFilterChain`] for what that order decides.
    pub fn with_context_filter(mut self, context_filter: Arc<dyn ContextFilter>) -> Self {
        self.context_filters.push(context_filter);
        self
    }

    /// Installs the product's typed extractor for tool-output references in model responses.
    ///
    /// A filter does not parse narration to infer retention. When no extractor is installed,
    /// produced outputs are tracked but no response is considered an explicit reference.
    pub fn with_tool_output_reference_extractor(
        mut self,
        tool_output_reference_extractor: Arc<dyn ToolOutputReferenceExtractor>,
    ) -> Self {
        self.tool_output_reference_extractor = Some(tool_output_reference_extractor);
        self
    }

    /// Appends a context processor run before each ordinary model request.
    ///
    /// Processors receive the authoritative generated history as a read-only value and return a
    /// model-input projection plus any records the runtime must append. This lets services such as
    /// context compaction run without the loop kernel depending on their concrete crate.
    pub fn with_context_processor(mut self, context_processor: Arc<dyn ContextProcessor>) -> Self {
        self.context_processors.push(context_processor);
        self
    }

    /// Installs one capability for this run.
    ///
    /// A capability is assembled once, when the run starts: its declared dependencies are checked
    /// against the rest of the installed set, its tools and prompt fragment join the agent instance
    /// that executes, its settings fold onto that instance's settings layer, and its context
    /// transform is appended to the processors installed above. Installation order is the assembly
    /// order, except where a declared dependency moves a capability after the family it names — see
    /// [`CapabilityPlan`](crate::capability::CapabilityPlan) for the whole rule.
    ///
    /// A set that cannot be assembled — a missing dependency, two capabilities claiming one family,
    /// a dependency circle — fails the run before its first model call rather than reaching a model
    /// half-assembled.
    pub fn with_capability(mut self, capability: Arc<dyn Capability>) -> Self {
        self.capabilities.push(capability);
        self
    }

    /// Installs several capabilities, in iteration order.
    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = Arc<dyn Capability>>,
    ) -> Self {
        self.capabilities.extend(capabilities);
        self
    }

    /// Selects the base permission mode used for every tool call in this run.
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission = self.permission.with_mode(mode);
        self
    }

    /// Replaces the ordered permission-rule exceptions for this run.
    pub fn with_permission_rules(
        mut self,
        rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        self.permission = PermissionEngine::new(self.permission.mode()).with_rules(rules);
        self
    }

    /// Policy applied to the final tool-and-handoff table for each turn.
    #[must_use]
    pub const fn tool_name_collision_policy(&self) -> ToolNameCollisionPolicy {
        self.tool_name_collision_policy
    }

    /// Per-turn budget for the final tool-and-handoff action table.
    #[must_use]
    pub const fn action_surface_budget(&self) -> ActionSurfaceBudget {
        self.action_surface_budget
    }

    /// Permission evaluator shared by every turn of this run.
    #[must_use]
    pub const fn permission(&self) -> &PermissionEngine {
        &self.permission
    }

    /// Whether provider events are forwarded to the run subscriber.
    #[must_use]
    pub const fn partial_messages(&self) -> bool {
        self.partial_messages
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

    /// The model-input filters installed for this run, in the order they run.
    #[must_use]
    pub const fn context_filters(&self) -> &ContextFilterChain {
        &self.context_filters
    }

    /// Typed tool-output reference extractor, when configured.
    #[must_use]
    pub fn tool_output_reference_extractor(
        &self,
    ) -> Option<&Arc<dyn ToolOutputReferenceExtractor>> {
        self.tool_output_reference_extractor.as_ref()
    }

    /// Ordered context processors installed for this run.
    #[must_use]
    pub fn context_processors(&self) -> &[Arc<dyn ContextProcessor>] {
        &self.context_processors
    }

    /// Capabilities installed for this run, in installation order.
    ///
    /// Installation order, not assembly order: the two differ wherever a declared dependency moves
    /// a capability, and this accessor reports what the host asked for.
    #[must_use]
    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.capabilities
    }

    /// Appends the context transforms an assembled capability set contributed.
    ///
    /// Appended rather than merged in front: a processor installed through
    /// [`Self::with_context_processor`] was put there by the host directly, before any capability
    /// was resolved, and running the capability chain ahead of it would change what that host
    /// already had working.
    pub(crate) fn extend_context_processors(
        &mut self,
        context_processors: impl IntoIterator<Item = Arc<dyn ContextProcessor>>,
    ) {
        self.context_processors.extend(context_processors);
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
            .field("partial_messages", &self.partial_messages)
            .field(
                "tool_name_collision_policy",
                &self.tool_name_collision_policy,
            )
            .field("action_surface_budget", &self.action_surface_budget)
            .field("context_filters", &self.context_filters)
            .field(
                "has_tool_output_reference_extractor",
                &self.tool_output_reference_extractor.is_some(),
            )
            .field("context_processor_count", &self.context_processors.len())
            .field(
                "capabilities",
                &self
                    .capabilities
                    .iter()
                    .map(|capability| capability.kind())
                    .collect::<Vec<_>>(),
            )
            .field("permission", &self.permission)
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
    run_id: RunId,
    app_context: Option<Arc<dyn Any + Send + Sync>>,
    cancel: CancelScope,
    input: Vec<ModelInputItem>,
    config: RunConfig,
    state: RunState,
    services: ToolServices,
    event_seqs: EventSeqAllocator,
}

impl RunRequest {
    /// Creates a request.
    ///
    /// `cancel` is required for the same reason it is in turn preparation and tool dispatch: every
    /// stage of the loop awaits third-party code, and a run with no scope is a run nobody can
    /// interrupt.
    ///
    /// `run_id` is required and comes from the caller, never from a default. Persisted events,
    /// stored items and replay all attribute by it, and an identity a constructor mints on its own
    /// is one a resumed segment cannot be given back — [`RunId::generate`] makes minting a fresh one
    /// something the caller *says*. A continuation passes the identity its first segment used.
    pub fn new(
        agent: AgentBinding,
        model_resolver: Arc<dyn ModelResolver>,
        run_id: RunId,
        cancel: CancelScope,
        input: Vec<ModelInputItem>,
    ) -> Self {
        let state = RunState::start(run_id.clone());
        let event_seqs = state.restore_event_seq_allocator(None);
        Self {
            agent,
            model_resolver,
            run_id,
            app_context: None,
            cancel,
            input,
            config: RunConfig::new(),
            state,
            services: ToolServices::new(),
            event_seqs,
        }
    }

    /// Sets the run-level configuration.
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = config;
        self
    }

    /// Attaches the host's own state object.
    ///
    /// It reaches running code through
    /// [`RunContext::app_context`](ra_core::context::RunContext::app_context) and nowhere else, so a
    /// tool reads it by naming the type it expects rather than by casting whatever it was handed.
    pub fn with_app_context(mut self, app_context: Arc<dyn Any + Send + Sync>) -> Self {
        self.app_context = Some(app_context);
        self
    }

    /// Installs the framework ports every tool in this run is handed.
    ///
    /// One bag rather than one setter per port: the ports travel from here through settlement,
    /// batch execution and dispatch into [`Tool::call`](ra_core::tool::Tool::call), so a port added
    /// as its own parameter would change four signatures and every third-party tool.
    pub fn with_services(mut self, services: ToolServices) -> Self {
        self.services = services;
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
    ///
    /// **The state also carries the identity.** The continuation is attributed to the run it
    /// continues, so the `run_id` given to [`Self::new`] is replaced by the state's — an identity
    /// the caller passed separately could disagree with the one every already-persisted event was
    /// written under, and the state is the side that survived the restart.
    ///
    /// **Input selects the continuation base.** Supplying input preserves the caller-managed
    /// continuation behavior: that input is the base for this segment's model calls. Passing an
    /// empty input asks the runner to project the checkpoint's recorded history automatically.
    /// To add a new user turn, start from the projection [`RunResult::continuation_input`] builds
    /// and append that turn before constructing this request.
    ///
    /// **Choosing the first is a one-way door.** Input a caller supplies is a base, not a record,
    /// so the checkpoint never learns the new turn it carried; from then on the state says so and
    /// refuses to project, because a projection that silently dropped that turn is the failure
    /// this refusal exists to prevent. A run continued that way stays caller-managed, and the host
    /// keeps owning the transcript it built.
    ///
    /// Use [`Self::with_state_and_persisted_max_seq`] instead when a rollout log exists.
    pub fn with_state(self, state: RunState) -> Self {
        self.with_state_and_persisted_max_seq(state, None)
    }

    /// Continues from an earlier segment, reconciling the sequence bound against a persisted log.
    ///
    /// `persisted_run_max_seq` is the largest sequence number the rollout writer has actually
    /// written for this run. A checkpoint can lag behind the log — numbers are allocated before the
    /// write that persists them — so resuming from the checkpoint alone would re-issue numbers that
    /// are already on disk. Passing `None` states that no log exists to reconcile against.
    pub fn with_state_and_persisted_max_seq(
        mut self,
        state: RunState,
        persisted_run_max_seq: Option<u64>,
    ) -> Self {
        self.run_id = state.run_id().clone();
        self.event_seqs = state.restore_event_seq_allocator(persisted_run_max_seq);
        self.state = state;
        self
    }

    /// The run-scoped allocator host events draw their sequence numbers from.
    ///
    /// Read-only on purpose, and there is deliberately no setter. The allocator and the state are
    /// two carriers of the same run identity; letting one be replaced independently would let a run
    /// report one identity to its tools and checkpoint another, and would leave the bound this
    /// allocator advances snapshotted into nothing.
    #[must_use]
    pub const fn event_seq_allocator(&self) -> &EventSeqAllocator {
        &self.event_seqs
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
    ///
    /// Boxed because the loop's future carries a whole turn — preparation, the model call, and the
    /// dispatcher its stream starts tools through — and a caller that composes runs should not
    /// have to hold all of it inline on the stack.
    pub async fn run(request: RunRequest) -> Result<RunResult> {
        Box::pin(run_loop(request, None)).await
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
        let task = tokio::spawn(async move { Box::pin(run_loop(request, Some(sender))).await });
        RunStream::new(receiver, task, guard)
    }
}

/// Everything one turn reads but never changes.
///
/// It exists so the loop can live in its own function: the alternative is a dozen parameters, and
/// the alternative to *that* is one function long enough that the budget checks and the settlement
/// hand-off stop being visible together.
struct TurnLoopContext<'a> {
    /// The model-input base for this segment.
    ///
    /// A caller-supplied continuation keeps control of this projection. Empty input instead uses
    /// the checkpoint's recorded history, which lets a host resume without rebuilding it.
    input_base: &'a [ModelInputItem],
    /// Whether this segment's model input is fully reconstructible from `RunState`.
    ///
    /// A caller-managed continuation can carry an arbitrary projection the checkpoint does not
    /// own. Context processing must not replace a partial reconstruction and silently discard
    /// that input, so processors run only when this is true.
    authoritative_history_complete: bool,
    model_resolver: &'a Arc<dyn ModelResolver>,
    run_id: &'a RunId,
    app_context: Option<&'a Arc<dyn Any + Send + Sync>>,
    services: &'a ToolServices,
    cancel: &'a CancelScope,
    closeout_cancel: &'a CancelScope,
    config: &'a RunConfig,
    permission: &'a PermissionEngine,
    events: Option<&'a mpsc::UnboundedSender<RunStreamEvent>>,
    event_seqs: &'a EventSeqAllocator,
    /// Capability fragments resolved at assembly and still waiting for the signal that earns them.
    deferred_prompts: &'a [DeferredPrompt],
}

/// What the loop produces, whichever way it ends.
///
/// **It does not hold the records themselves.** The run's history lives in [`RunState`], which is
/// what a checkpoint carries and what the next model request is built from; a second copy here
/// would be a second thing to keep in step, and the two would first disagree on the resume path
/// where only one of them spans earlier segments. What this holds instead is where *this* segment
/// starts in that history, because a [`RunResult`] reports the segment it ran while the checkpoint
/// reports the whole run.
struct TurnLoopProgress {
    first_item: usize,
    first_response: usize,
    turn_record_owner: Arc<TurnRecordOwner>,
    turn_records: Vec<TurnRecord>,
    turns: u32,
    /// Turns this run completed before the current segment began.
    ///
    /// `turns` counts the segment, because that is what `RunResult`, `TurnRecord`, and the stream
    /// events describe. The tool-output reference ledger measures staleness across the whole run
    /// instead, so its two call sites add this base. Captured once, from the restored ledger, and
    /// never recomputed: reading the high-water mark again mid-segment would count the turns this
    /// segment has already recorded a second time.
    reference_turn_base: u64,
    budget_stop: Option<BudgetKind>,
}

impl TurnLoopProgress {
    /// The whole run's number for the turn now running, as the reference ledger counts turns.
    fn reference_turn(&self) -> u64 {
        self.reference_turn_base
            .saturating_add(u64::from(self.turns))
    }

    /// Records this segment generated, as a window into the run's own history.
    fn segment_items<'a>(&self, state: &'a RunState) -> &'a [RunItem] {
        state
            .generated_items()
            .get(self.first_item..)
            .unwrap_or_default()
    }

    /// Model calls this segment made, as a window into the run's own history.
    fn segment_responses<'a>(&self, state: &'a RunState) -> &'a [ModelResponse] {
        state
            .model_responses()
            .get(self.first_response..)
            .unwrap_or_default()
    }
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
        usage.requests = tracing::field::Empty,
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
#[allow(clippy::too_many_lines)]
async fn run_loop_inner(
    request: RunRequest,
    events: Option<mpsc::UnboundedSender<RunStreamEvent>>,
    span: &tracing::Span,
) -> Result<RunResult> {
    let RunRequest {
        mut agent,
        model_resolver,
        run_id,
        app_context,
        cancel,
        input: requested_input,
        mut config,
        mut state,
        services,
        event_seqs,
    } = request;

    if let Err(error) = validate_config(&config) {
        // Ahead of the run scope, so there is no cancellation to attribute and no aggregate to
        // report — but the span still has to say the run ended and why.
        ra_core::trace::record_error(span, &error);
        return Err(error);
    }

    // Checked here, beside the rest of the configuration and ahead of the segment: whether the
    // installed capabilities can be assembled at all is a property of the configuration, and a
    // missing dependency reported after a model has been paid to read a half-assembled surface is
    // reported too late. Binding them is a separate step below, because that needs the run.
    let capability_plan =
        match CapabilityPlan::resolve(config.capabilities().iter().map(Arc::clone)) {
            Ok(plan) => plan,
            Err(error) => {
                ra_core::trace::record_error(span, &error);
                return Err(error);
            }
        };

    // An explicit input remains the caller's continuation base. An empty resumed request chooses
    // the checkpoint projection, so hosts that persist only state do not have to rebuild it.
    let resuming = state.current_agent().is_some();
    if let Err(error) = state.begin_segment(agent.public_id().clone(), requested_input.clone()) {
        ra_core::trace::record_error(span, &error);
        return Err(error);
    }
    // Read from the state that owns it rather than re-derived from this segment's arguments.
    // `begin_segment` above is what decides it, it survives a checkpoint, and a second encoding
    // here would have to be kept true by hand across resume paths that never meet.
    let authoritative_history_complete = state.input_history_is_complete();
    let input_base = segment_input_base(&state, resuming, requested_input);
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

    let mut deferred_prompts: Vec<DeferredPrompt> = Vec::new();
    // Under the run scope rather than ahead of it: a capability resolves its prompt fragment with
    // third-party asynchronous code, and a run whose assembly reads a slow source is one the
    // deadline and the caller's interrupt still have to reach.
    if !capability_plan.is_empty() {
        let assembly = assemble_capabilities(
            &capability_plan,
            &agent,
            &run_id,
            app_context.as_ref(),
            &state,
            &event_seqs,
        );
        match cancel.run(assembly).await.and_then(|result| result) {
            Ok((execution, context_processors, deferred)) => {
                agent = AgentBinding::prepared(Arc::clone(agent.public()), execution);
                config.extend_context_processors(context_processors);
                deferred_prompts = deferred;
            }
            Err(error) => {
                record_terminal_error(span, &error, &cancel);
                return Err(error);
            }
        }
    }

    let mut progress = TurnLoopProgress {
        first_item: state.generated_items().len(),
        first_response: state.model_responses().len(),
        turn_record_owner: TurnRecordOwner::new(),
        turn_records: Vec::new(),
        turns: 0,
        reference_turn_base: state
            .tool_output_references()
            .last_completed_turn()
            .unwrap_or(0),
        budget_stop: None,
    };
    let permission = config.permission().clone().with_rules(
        config
            .permission()
            .rules()
            .iter()
            .cloned()
            .chain(state.permission_rules().iter().cloned()),
    );
    let context = TurnLoopContext {
        input_base: &input_base,
        authoritative_history_complete,
        model_resolver: &model_resolver,
        run_id: &run_id,
        app_context: app_context.as_ref(),
        services: &services,
        cancel: &cancel,
        closeout_cancel: &closeout_cancel,
        config: &config,
        permission: &permission,
        events: events.as_ref(),
        event_seqs: &event_seqs,
        deferred_prompts: &deferred_prompts,
    };
    let interrupted_turn = resolve_interrupted_turn(&context, &agent, &mut state).await;

    // Settling checkpointed answers and running turns are one stage as far as stopping is
    // concerned: both execute tools under the run scope, so both can be the thing a deadline
    // interrupts. Joining them into a single `Result` before the match below is what keeps the
    // translation underneath a single statement of the rule rather than two copies to keep in step.
    let stepped = match interrupted_turn {
        Ok(()) => run_turns(&context, &mut agent, &mut state, &mut progress).await,
        Err(error) => Err(error),
    };

    // The one place an expired wall clock is read back as a budget stop. Everything under the run
    // scope reports expiry the same way any other cancellation is reported, which is what lets the
    // loop stay free of deadline special cases; translating it here — rather than at each of the
    // four `?` inside — is why exactly one kind of stop can be a soft one.
    let outcome = match stepped {
        Ok(outcome) => outcome,
        Err(error) if is_wall_clock_expiry(&error, &cancel, budget_deadline) => {
            progress.budget_stop = Some(BudgetKind::WallClock);
            RunOutcome::Completed {
                reason: FinishReason::BudgetExhausted,
            }
        }
        Err(error) => {
            record_progress_usage(span, progress.segment_responses(&state));
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

    let final_message =
        match deliver_budget_closeout(&context, &agent, &mut progress, &mut state).await {
            Ok(message) => message,
            Err(error) => {
                record_progress_usage(span, progress.segment_responses(&state));
                record_terminal_error(span, &error, &closeout_cancel);
                return Err(error);
            }
        };

    if let Some(reason) = outcome.finish_reason() {
        state = state.with_finish_reason(reason);
    }
    state.snapshot_event_seq(&event_seqs);

    // Cut out of the run's history rather than accumulated alongside it: a result reports the
    // segment it ran, and the checkpoint it carries reports every segment.
    let (new_items, model_responses) = segment_records(&progress, &state);
    let mut result = RunResult::new(
        outcome.clone(),
        Arc::clone(agent.public()),
        input_base,
        new_items,
        model_responses,
        progress.turn_record_owner,
        progress.turn_records,
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

/// Binds the run's capabilities and splits what they contribute between the two places it goes.
///
/// The agent instance comes back paired with the context processors and the deferred prompts
/// deliberately. Tools, prompt text, and sampling settings describe the thing that executes, while
/// a context transform is a property of the run and a deferred fragment belongs to the turn loop
/// that waits for its signal — and a single "prepared" value carrying all three would be a home for
/// a group that has no other reason to be one object.
///
/// The instance is assembled onto [`AgentBinding::execution`], not the public agent: a host that
/// already prepared its own execution instance keeps it, and the identity every record is filed
/// under stays the one the user configured.
async fn assemble_capabilities(
    plan: &CapabilityPlan,
    agent: &AgentBinding,
    run_id: &RunId,
    app_context: Option<&Arc<dyn Any + Send + Sync>>,
    state: &RunState,
    event_seqs: &EventSeqAllocator,
) -> Result<(
    Arc<AgentSpec>,
    Vec<Arc<dyn ContextProcessor>>,
    Vec<DeferredPrompt>,
)> {
    // The public agent, because binding is told who is running rather than what was assembled —
    // and what was assembled is precisely what does not exist yet at this point.
    let mut context = RunContext::new(run_id.clone(), agent.public())
        .with_budget(state.budget().clone())
        .with_usage_totals(state.usage_totals().clone())
        .with_pending_control_requests(state.pending_control_requests().to_vec())
        .with_event_seq_allocator(event_seqs.clone());
    if let Some(app_context) = app_context {
        context = context.with_app_context(Arc::clone(app_context));
    }

    let assembled = plan.assemble(&context).await?;
    let execution = assembled.prepare_agent(agent.execution())?;
    Ok((
        execution,
        assembled.context_processors().to_vec(),
        assembled.deferred_prompts().to_vec(),
    ))
}

/// Settles host answers that were checkpointed with an interrupted run before another model call.
///
/// The approval record remains in history as the control-plane question; this stage appends the
/// paired tool output (or rejection) and only then clears its pending ID. That ordering makes a
/// checkpoint taken between the click and the tool invocation resumable instead of losing work.
async fn resolve_interrupted_turn(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
) -> Result<()> {
    let answers = state.pending_interruption_resolutions().to_vec();
    // Filed once for the whole stage rather than once per answer, because these are the tail of a
    // single interrupted turn. `ToolFailureTracker::record_turn` fingerprints the ordered turn it
    // is given to recognise a re-settle, so splitting one turn into several one-outcome calls
    // would leave that guard describing a turn nothing will ever settle again.
    let mut outcomes = Vec::new();
    for answer in answers {
        let item = state
            .generated_items()
            .iter()
            .find(|item| item.id() == answer.item_id())
            .ok_or_else(|| {
                Error::caller(format!(
                    "answered interruption `{}` is absent from generated items",
                    answer.item_id()
                ))
            })?;
        let RunItemKind::ToolApproval(approval) = item.kind() else {
            return Err(Error::caller(format!(
                "answered interruption `{}` is not a local tool approval",
                answer.item_id()
            )));
        };
        let provenance = item.provenance().cloned();
        let approval = approval.clone();
        let (output, outcome) = match answer.resolution() {
            InterruptionResolution::Reject { .. } => {
                let output = ToolCallOutput::new(
                    approval.call_id().clone(),
                    serde_json::json!({"code": "approval_rejected", "tool": approval.tool_name()}),
                )
                .with_error(true);
                // Filed as a refusal, the same as a call the permission stage declines below: both
                // are "the runtime answered without running the tool", which is what
                // `ToolOutcome::refused` names. Recording nothing instead would leave an earlier
                // failure streak standing behind a call that never ran, so the next genuine
                // attempt would be judged on evidence this one did not produce. An approval
                // written before routing identities were stored has nothing to file it under.
                let outcome = approval.lookup_key().map(|key| {
                    ToolOutcome::refused(
                        ToolUse::Tool(key.clone()),
                        approval.call_id().clone(),
                        approval.arguments(),
                        output.output(),
                    )
                });
                (output, outcome)
            }
            InterruptionResolution::Approve { .. } => {
                let (output, outcome) =
                    execute_approved_call(context, agent, state, &approval, answer.item_id())
                        .await?;
                (output, Some(outcome))
            }
            _ => return Err(Error::caller("unsupported interruption resolution")),
        };
        outcomes.extend(outcome);
        let mut output_item = RunItem::new(
            ItemId::new(format!("{}.output", approval.call_id())),
            RunItemKind::ToolCallOutput(output),
        );
        if let Some(provenance) = provenance {
            output_item = output_item.with_provenance(provenance);
        }
        emit(context.events, RunStreamEvent::Item(output_item.clone()));
        state.record_generated_items([output_item]);
        state.settle_interruption_resolution(answer.item_id())?;
    }
    if !outcomes.is_empty() {
        let (_, failure) = state.trackers_mut();
        failure.record_turn(agent.public_id(), outcomes);
    }
    Ok(())
}

/// Runs one call the host approved before the checkpoint was taken.
///
/// The identity handed to the breaker is rebuilt from the approval's lookup key rather than asked
/// of a bound action, which is safe only because the tool below is *selected* by that same key:
/// the two cannot drift the way [`ProcessedResponse`](ra_core::step::ProcessedResponse) warns
/// about, because one is the search term for the other.
async fn execute_approved_call(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &RunState,
    approval: &ToolApproval,
    item_id: &ItemId,
) -> Result<(ToolCallOutput, ToolOutcome)> {
    let key = approval.lookup_key().ok_or_else(|| {
        Error::caller(format!(
            "approval `{item_id}` cannot resume because it has no serialized tool lookup key"
        ))
    })?;
    let tool = agent
        .public()
        .tools()
        .iter()
        .find(|tool| tool.origin().lookup_key() == key)
        .ok_or_else(|| {
            Error::caller(format!(
                "approval `{item_id}` names lookup key `{key:?}`, which the resumed agent no \
                 longer provides"
            ))
        })?;

    let mut run = RunContext::new(context.run_id.clone(), agent.public())
        .with_budget(state.budget().clone())
        .with_usage_totals(state.usage_totals().clone())
        .with_pending_control_requests(state.pending_control_requests().to_vec())
        .with_event_seq_allocator(context.event_seqs.clone());
    if let Some(app_context) = context.app_context {
        run = run.with_app_context(Arc::clone(app_context));
    }

    // The streak already counts this call: settlement recorded the attempt before it interrupted,
    // so the breaker sees the same history here that it would have seen had the host answered
    // without a restart in between.
    let identity = ToolUse::Tool(key.clone());
    let history = CallHistory::new(
        state.tool_use().repeat_streak(agent.public_id(), &identity),
        state
            .tool_failure()
            .no_progress_streak(agent.public_id(), &identity),
    );
    let dispatch = dispatch_tool(
        ToolDispatchRequest::new(
            Arc::clone(tool),
            approval.call_id().clone(),
            approval.arguments().clone(),
            Arc::new(run),
            context.cancel.child(ScopeKind::Tool),
            history,
            context.permission.clone(),
        )
        .with_services(context.services.clone())
        .with_approval_granted(),
    )
    .await?;

    let call_id = approval.call_id().clone();
    match dispatch {
        ToolDispatch::Observed(observation) => {
            let failure_code = observation.failure_code();
            let output = observation.output().clone();
            let outcome = match failure_code {
                Some(code) => ToolOutcome::failed(
                    identity,
                    call_id,
                    approval.arguments(),
                    output.output(),
                    code,
                ),
                None => {
                    ToolOutcome::succeeded(identity, call_id, approval.arguments(), output.output())
                }
            };
            Ok((output, outcome))
        }
        ToolDispatch::Refused(refusal) => {
            let output = refusal.into_output();
            let outcome =
                ToolOutcome::refused(identity, call_id, approval.arguments(), output.output());
            Ok((output, outcome))
        }
        ToolDispatch::AwaitingApproval(_) => Err(Error::caller(
            "an approved tool call requested approval again",
        )),
    }
}

/// Selects the input base for one segment after its state accepted the agent identity.
fn segment_input_base(
    state: &RunState,
    resuming: bool,
    requested_input: Vec<ModelInputItem>,
) -> Vec<ModelInputItem> {
    if !resuming || !requested_input.is_empty() {
        return requested_input;
    }

    let mut input = state.original_input().to_vec();
    input.extend(
        state
            .generated_items()
            .iter()
            .filter_map(RunItem::to_model_input),
    );
    input
}

/// Copies the checkpoint records created by one segment for its result.
fn segment_records(
    progress: &TurnLoopProgress,
    state: &RunState,
) -> (Vec<RunItem>, Vec<ModelResponse>) {
    (
        progress.segment_items(state).to_vec(),
        progress.segment_responses(state).to_vec(),
    )
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
fn record_progress_usage(span: &tracing::Span, responses: &[ModelResponse]) {
    record_usage(span, &aggregate_usage(responses));
}

/// Records normalized usage on any span whose scale is defined by its kind.
fn record_usage(span: &tracing::Span, usage: &ra_core::usage::Usage) {
    span.record(ra_core::trace::field::USAGE_REQUESTS, usage.requests());
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

        if let Some(kind) = state.exhausted_budget_kind(config.budget()) {
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
            usage.requests = tracing::field::Empty,
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
        state.snapshot_event_seq(context.event_seqs);
    };
    Ok(outcome)
}

/// Runs one turn: prepare, call the model, settle, and say whether the run continues.
///
/// `None` means another turn; `Some` carries the outcome the run ends with.
///
/// This is the lifecycle coordinator for one turn. Keeping preparation, context processing,
/// dispatch, accounting, and settlement together makes their ordering auditable.
#[allow(clippy::too_many_lines)]
async fn run_one_turn(
    context: &TurnLoopContext<'_>,
    agent: &mut AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
    turn_scope: &CancelScope,
    turn_span: &tracing::Span,
) -> Result<Option<RunOutcome>> {
    let config = context.config;
    // Ahead of everything that reads history, so a fragment earned by the previous turn is in the
    // request that also carries the tool result which earned it.
    deliver_deferred_prompts(context, agent, state, progress.reference_turn());
    let reminder = budget_reminder(state, config.budget());
    // Where the authoritative history sits inside the request, so a context processor can be told
    // which span of items it owns without counting them again. Preparation appends its own tail
    // items after this point, and everything past the history is the processor's suffix.
    let history_span = context.authoritative_history_complete.then(|| HistorySpan {
        history_len: state
            .generated_items()
            .iter()
            .filter(|item| item.is_model_input())
            .count(),
    });
    let input = match history_span {
        Some(_) => next_input(state.original_input(), state.generated_items(), reminder),
        None => next_input(context.input_base, progress.segment_items(state), reminder),
    };
    let preparation_context = live_context(context, agent, state);
    let mut preparation = TurnPreparationRequest::new(
        agent,
        context.model_resolver.as_ref(),
        &preparation_context,
        turn_scope,
        state.tool_use(),
        input,
    )
    .with_model_settings(config.model_settings.clone())
    .with_tracing(config.tracing)
    .with_tool_name_collision_policy(config.tool_name_collision_policy())
    .with_action_surface_budget(config.action_surface_budget());
    if let Some(model) = &config.model {
        preparation = preparation.with_model(model.clone());
    }
    let prepared = prepare_turn(preparation).await?;
    let (prepared, context_records, context_responses) =
        process_context_processors(context, state, progress, turn_scope, prepared, history_span)
            .await?;
    // The filter chain runs last, on whatever the coarse transforms produced. A context processor
    // reprojects whole regions of history; running a filter before it would only trim items the
    // processor's own output then replaced with the untouched originals.
    let (prepared, context_filter_reports) =
        apply_context_filters(config, state, progress.reference_turn(), prepared)?;

    let first_item = progress.segment_items(state).len();
    let context_usage = context_responses
        .iter()
        .fold(Usage::default(), |total, response| {
            total.accumulate(response.usage())
        });
    if context_usage.requests() > 0 {
        state.record_usage(&context_usage);
    }
    for response in context_responses {
        state.record_model_response(response);
    }
    if !context_records.is_empty() {
        for item in &context_records {
            emit(context.events, RunStreamEvent::Item(item.clone()));
        }
        state.record_generated_items(context_records);
    }

    let streaming_dispatch = StreamedDispatchInput {
        agent_id: agent.public_id().clone(),
        tool_use: state.tool_use().clone(),
        tool_failure: state.tool_failure().clone(),
        run: Arc::new(live_context(context, agent, state)),
        cancel: turn_scope.clone(),
        services: context.services.clone(),
        max_function_tool_concurrency: config.max_function_tool_concurrency,
        permission: context.permission.clone(),
    };
    let (surface, response, streamed_dispatches) =
        call_model(turn_scope, prepared, context, streaming_dispatch).await?;

    // Both facts about a completed call are recorded here, before settlement, and the stop
    // either may cause is *not* taken here. The response has already been paid for, so its
    // items belong in history and its tool calls belong to the turn that requested them; the
    // check at the top of the next iteration is what ends the run, one turn later and with the
    // work intact.
    //
    // Recording before settling is also what keeps the two facts agreeing when a deadline
    // interrupts settlement below: the session projection may then be incomplete, but usage
    // accounting, provider diagnostics, and an error handler's snapshot must not deny that the
    // call ran. The copy is what that costs, next to the history copy this turn already makes for
    // settlement.
    record_usage(turn_span, response.usage());
    state.record_usage(response.usage());
    state.record_model_response(response.clone());
    let referenced_outputs = referenced_tool_outputs(config, &response)?;

    // Settlement sees the same input base the model did and only this segment's preceding items.
    // The base may itself be a full caller-supplied or checkpoint-projected transcript.
    let segment_original_input = context.input_base.to_vec();
    let pre_step_items = progress.segment_items(state).to_vec();

    // Built again rather than reused from preparation: the call above has been paid for, and the
    // spend a tool reads has to include it. The two contexts are the same run and the same agent —
    // what differs is only how much of the budget each stage can truthfully report.
    //
    // A tool the stream already started holds the earlier one instead, for the reason given on
    // [`StreamedDispatchInput`]: its response had not been paid for when it was handed over.
    let settlement_context = Arc::new(live_context(context, agent, state));
    let (tool_use, tool_failure) = state.trackers_mut();
    let settlement = TurnSettlementRequest::new(
        agent,
        &response,
        &surface,
        settlement_context,
        turn_scope,
        tool_use,
        tool_failure,
        context.permission.clone(),
    )
    .with_original_input(segment_original_input)
    .with_pre_step_items(pre_step_items)
    .with_services(context.services.clone())
    .with_max_function_tool_concurrency(config.max_function_tool_concurrency)
    .with_streamed_dispatches(streamed_dispatches);
    let settled = settle_turn(settlement).await?;

    record_tool_output_references(
        config,
        state,
        progress.reference_turn(),
        settled.session_step_items(),
        referenced_outputs,
    )?;

    for item in settled.session_step_items() {
        emit(context.events, RunStreamEvent::Item(item.clone()));
    }
    // The range is relative to the segment, because that is what `RunResult::turn_items` indexes.
    state.record_generated_items(settled.session_step_items().iter().cloned());
    let last_item = progress.segment_items(state).len();

    // Recorded before the `match` below, which is where a handoff replaces the running agent: the
    // record says who ran *this* turn, and taking the agent afterwards would attribute the turn to
    // whoever it handed off to.
    progress.turn_records.push(TurnRecord::new(
        Arc::clone(&progress.turn_record_owner),
        progress.turns,
        agent.public_id().clone(),
        settled.next_step().clone(),
        first_item..last_item,
        context_filter_reports,
    ));

    // No `_` arm, deliberately. R3-1 made this the one place control flow converges, and a
    // fifth state has to be answered here rather than fall through to "keep going".
    match settled.next_step() {
        NextStep::RunAgain => Ok(None),
        NextStep::FinalOutput { reason } => Ok(Some(RunOutcome::Completed { reason: *reason })),
        // The items are the session's own records: settlement re-points a pending decision at what
        // it stores before handing the decision over, so this outcome and the stream carry one copy
        // of each question rather than two that disagree about who produced it.
        NextStep::Interruption { items } => {
            // The checkpoint keeps their IDs, which resolve against the records recorded just
            // above. Settlement guarantees they are among them, so a failure here is this loop
            // breaking its own contract rather than anything the host did.
            state.set_pending_interruptions(items)?;
            Ok(Some(RunOutcome::Interrupted {
                items: items.clone(),
            }))
        }
        // Control transfers to another agent, which speaks next. The new agent arrives as a
        // public declaration, so it binds directly: whatever prepared *this* turn's execution
        // instance has no say over who runs the next one. Unreachable until R17 — settlement
        // refuses handoffs — but the state machine has to say what it does about it.
        NextStep::Handoff { new_agent } => {
            *agent = AgentBinding::direct(Arc::clone(new_agent));
            state.set_current_agent(agent.public_id().clone());
            Ok(None)
        }
    }
}

/// Delivers every deferred capability fragment this turn has earned into authoritative history.
///
/// # Why history rather than the turn's tail
///
/// A fragment is delivered once and then stays. The alternative — rebuilding it into the tail of
/// every subsequent turn — puts it after the cached span on every call, which costs more per turn
/// than leaving the text resident in the prefix would have; deferring would then be a way to make
/// long runs more expensive rather than less. Written into history it is paid for once, and every
/// later turn carries it inside the span a cache read already covers.
///
/// # Why "delivered already" is read back from history
///
/// The record ID comes from the family, so a history that holds it is the record of the delivery.
/// Keeping a flag beside the prompts instead would be a second copy of that fact, and the two would
/// first disagree on a resume: the history survives a checkpoint, an in-memory flag does not, and
/// the run would re-deliver a fragment it can see in its own transcript.
fn deliver_deferred_prompts(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    turn: u64,
) {
    if context.deferred_prompts.is_empty() {
        return;
    }

    let delivered: BTreeSet<&ItemId> = state.generated_items().iter().map(RunItem::id).collect();
    let signal = LoadSignal::new(turn, state.tool_use().agent(agent.public_id()));
    let items: Vec<RunItem> = context
        .deferred_prompts
        .iter()
        .filter(|prompt| !delivered.contains(prompt.record_id()))
        .filter(|prompt| prompt.is_signalled(&signal))
        .map(DeferredPrompt::to_run_item)
        .collect();

    if items.is_empty() {
        return;
    }
    for item in &items {
        emit(context.events, RunStreamEvent::Item(item.clone()));
    }
    state.record_generated_items(items);
}

/// Runs every installed context processor and rebuilds the ordinary request from its projection.
///
/// Processors are deliberately generic here. The loop knows how to request a summary and append
/// returned records, but it does not know whether a processor is compaction, redaction, or a
/// product-specific retention policy.
async fn process_context_processors(
    context: &TurnLoopContext<'_>,
    state: &RunState,
    progress: &TurnLoopProgress,
    turn_scope: &CancelScope,
    prepared: PreparedTurn,
    history_span: Option<HistorySpan>,
) -> Result<(PreparedTurn, Vec<RunItem>, Vec<ModelResponse>)> {
    let processors = context.config.context_processors();
    if processors.is_empty() {
        return Ok((prepared, Vec::new(), Vec::new()));
    }
    // Only a request this loop assembled can be split into prefix, history, and tail. A segment
    // resuming on a caller's own projection is skipped rather than guessed at.
    let Some(span) = history_span else {
        return Ok((prepared, Vec::new(), Vec::new()));
    };
    let Some((prefix, suffix)) = span.split(prepared.request().input(), state.original_input())
    else {
        return Ok((prepared, Vec::new(), Vec::new()));
    };

    let mut input = prepared.request().input().to_vec();
    let mut history = state.generated_items().to_vec();
    let mut generated_items: Vec<RunItem> = Vec::new();
    let mut taken_ids: BTreeSet<ItemId> = history.iter().map(|item| item.id().clone()).collect();
    let mut model_responses = Vec::new();
    let summarizer = RunnerContextSummarizer::new(&prepared, turn_scope);

    for (index, processor) in processors.iter().enumerate() {
        let record_id = ItemId::new(format!("context-{}.{index}", progress.reference_turn()));
        let request = ContextProcessorRequest::new(
            state.run_id().clone(),
            progress.reference_turn(),
            record_id,
            prepared.selector().model().map(str::to_owned),
            prefix.clone(),
            history.clone(),
            suffix.clone(),
            input,
        );
        let result = processor.process_context(request, &summarizer).await?;
        input = result.input().to_vec();
        for item in result.generated_items() {
            // Reconciliation names records by ID, so a duplicate is not a detail: it silently
            // overwrites or double-counts a record instead of failing.
            if !taken_ids.insert(item.id().clone()) {
                return Err(Error::caller(format!(
                    "a context processor emitted record `{}`, which the run already generated",
                    item.id()
                )));
            }
            history.push(item.clone());
            generated_items.push(item.clone());
        }
        model_responses.extend(result.model_responses().iter().cloned());
    }

    let prepared = prepared.map_request(|request| request.with_input(input));
    Ok((prepared, generated_items, model_responses))
}

/// Where the authoritative history sits inside one assembled request.
#[derive(Debug, Clone, Copy)]
struct HistorySpan {
    history_len: usize,
}

impl HistorySpan {
    /// Splits an assembled request into the parts a context processor does not own.
    ///
    /// The tail is whatever follows the history, which is more than this loop appended: turn
    /// preparation adds its own reminder items after the history, and a processor that rebuilt the
    /// request from the loop's suffix alone would drop them.
    ///
    /// The prefix is compared rather than merely counted, so this is a positional claim the
    /// request has to still satisfy rather than one it is assumed to. Preparation only appends
    /// today, and a transform that rewrote or dropped an item there would keep every length here
    /// correct while making the boundaries wrong — which is one of the reasons the filter chain
    /// runs after this split rather than before it. `None` then means the request no longer has
    /// the shape this span describes, and context processing is skipped rather than applied to the
    /// wrong region.
    fn split(
        self,
        input: &[ModelInputItem],
        expected_prefix: &[ModelInputItem],
    ) -> Option<(Vec<ModelInputItem>, Vec<ModelInputItem>)> {
        let history_end = expected_prefix.len().checked_add(self.history_len)?;
        let prefix = input.get(..expected_prefix.len())?;
        if prefix != expected_prefix {
            return None;
        }
        Some((prefix.to_vec(), input.get(history_end..)?.to_vec()))
    }
}

/// Executes a context-summary request with the resolved model and stable instructions of a turn.
struct RunnerContextSummarizer<'a> {
    model: &'a Arc<dyn Model>,
    template: &'a ModelRequest,
    cancel: &'a CancelScope,
}

impl<'a> RunnerContextSummarizer<'a> {
    const fn new(prepared: &'a PreparedTurn, cancel: &'a CancelScope) -> Self {
        Self {
            model: prepared.model(),
            template: prepared.request(),
            cancel,
        }
    }
}

#[async_trait::async_trait]
impl ContextSummarizer for RunnerContextSummarizer<'_> {
    async fn summarize(&self, request: ContextSummaryRequest) -> Result<ContextSummaryResponse> {
        let mut input = request.input().to_vec();
        input.push(ModelInputItem::Message(Message::user(
            request.instructions(),
        )));
        let settings = self
            .template
            .model_settings()
            .clone()
            .reconcile_tool_surface(std::iter::empty::<&str>());
        let mut model_request =
            ModelRequest::new(input, settings).with_tracing(self.template.tracing());
        if let Some(instructions) = self.template.system_instructions() {
            model_request = model_request.with_system_instructions(instructions);
        }
        if let Some(cache_plan) = self.template.cache_plan() {
            model_request = model_request.with_cache_plan(cache_plan.clone());
        }
        if let Some(output_schema) = request.output_schema() {
            model_request = model_request.with_output_schema(output_schema.clone());
        }
        model_request.validate_cache_plan()?;
        let response = self
            .cancel
            .run(self.model.get_response(model_request))
            .await??;
        let text = response
            .output()
            .iter()
            .filter_map(|item| match item.kind() {
                RunItemKind::Message(message) if message.role() == MessageRole::Assistant => {
                    Some(message.text_content())
                }
                _ => None,
            })
            .collect::<String>();
        if text.trim().is_empty() {
            return Err(Error::caller(
                "the context summary response did not contain assistant text",
            ));
        }
        Ok(ContextSummaryResponse::new(text, response))
    }
}

/// Projects the request input through the installed filters and records what each one did.
///
/// `turn` spans the whole run, not this segment: the ledger a filter consults was restored with the
/// checkpoint, and a result last referenced before a resume has to stay comparable with the turn
/// now being prepared.
///
/// The system instructions are handed over so a filter can measure the whole request, and written
/// back from the prepared request rather than from the chain — the chain refuses a filter that
/// changed them, so there is nothing here to write back.
fn apply_context_filters(
    config: &RunConfig,
    state: &RunState,
    turn: u64,
    prepared: PreparedTurn,
) -> Result<(PreparedTurn, Vec<ContextFilterReport>)> {
    let filters = config.context_filters();
    if filters.is_empty() {
        return Ok((prepared, Vec::new()));
    }

    let request = ContextFilterRequest::new(state.run_id(), turn, state.tool_output_references());
    let data = ModelInputData::new(
        prepared.request().input().to_vec(),
        prepared.request().system_instructions().map(str::to_owned),
    );
    let (data, reports) = filters.apply(&request, data)?.into_parts();
    Ok((
        prepared.map_request(|request| request.with_input(data.into_input())),
        reports,
    ))
}

fn referenced_tool_outputs(
    config: &RunConfig,
    response: &ModelResponse,
) -> Result<Vec<ra_core::item::CallId>> {
    match config.tool_output_reference_extractor() {
        Some(extractor) if !config.context_filters().is_empty() => {
            extractor.referenced_tool_outputs(response)
        }
        Some(_) | None => Ok(Vec::new()),
    }
}

/// Settles this turn's retention facts, on the same whole-run turn axis the projection used.
///
/// Kept for the whole chain rather than for the one filter that consults it. The ledger costs a
/// list of call IDs per turn, while deciding per filter would mean asking each one whether it reads
/// retention — a question whose wrong answer is a filter silently seeing an empty ledger.
fn record_tool_output_references(
    config: &RunConfig,
    state: &mut RunState,
    turn: u64,
    items: &[RunItem],
    referenced_outputs: Vec<ra_core::item::CallId>,
) -> Result<()> {
    if config.context_filters().is_empty() {
        return Ok(());
    }
    let new_outputs = items.iter().filter_map(|item| {
        let RunItemKind::ToolCallOutput(output) = item.kind() else {
            return None;
        };
        Some(output.call_id().clone())
    });
    state
        .tool_output_references_mut()
        .record_turn(turn, new_outputs, referenced_outputs)
}

/// Builds the live context the stage about to run hands to third-party code.
///
/// One function so every stage projects the same facts. Two of them matter enough to name:
///
/// - **The public agent, never the execution instance.** A dynamic availability check, a tool and
///   later a guard all report and branch on the agent the user configured; a prepared clone that
///   reached them would make a run describe something nobody wrote down.
/// - **The spend counters as copies taken from [`RunState`], not handles into it.** The context is
///   a read view; the state stays the one thing that accumulates and the one thing a checkpoint
///   carries.
fn live_context(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &RunState,
) -> RunContext {
    let run = RunContext::new(context.run_id.clone(), agent.public())
        .with_budget(state.budget().clone())
        .with_usage_totals(state.usage_totals().clone())
        .with_pending_control_requests(state.pending_control_requests().to_vec())
        .with_event_seq_allocator(context.event_seqs.clone());
    match context.app_context {
        Some(app_context) => run.with_app_context(Arc::clone(app_context)),
        None => run,
    }
}

struct ModelCallAttempt<'a> {
    turn_scope: &'a CancelScope,
    model: &'a Arc<dyn Model>,
    selector: &'a ra_core::model::ModelSelector,
    events: Option<&'a mpsc::UnboundedSender<RunStreamEvent>>,
    attempt: u32,
    max_retries: u32,
    retry_trace: Option<RetryTrace>,
    streaming_dispatch: StreamedDispatchInput,
}

/// What one attempt let out of the runtime before it failed.
///
/// This is the runner's own record, not the adapter's opinion, and it is the reason a failed call
/// may or may not be replayable. Both fields mean the same thing at different costs: something
/// happened that a second attempt would repeat.
#[derive(Debug, Clone, Copy, Default)]
struct CallConsumption {
    /// Provider narration reached a [`RunStream`] subscriber, which cannot be unsent.
    published: bool,
    /// A tool started from a completed stream item and may have had side effects.
    dispatched: bool,
}

impl CallConsumption {
    /// Whether anything happened that replaying this call would duplicate.
    const fn any(self) -> bool {
        self.published || self.dispatched
    }
}

/// Everything needed to start a tool from a completed item in a model stream.
///
/// # Early tools see the run as it was before the call
///
/// The response's usage does not exist while the stream is still open, so the context here is the
/// one from immediately before the model call, and settlement builds a second one that includes
/// the call's spend. In a turn where the adapter narrated items, both are therefore live at once:
/// tools started early read the pre-call spend, tools the terminal response spawns read the
/// post-call spend. Sharing one context instead would mean choosing which group to lie to, and the
/// pre-call figure is the only one an early tool could ever have been given.
///
/// Settlement remains the only place that records the response and its attempts, so nothing about
/// the turn's own records depends on which context a tool held.
#[derive(Clone)]
struct StreamedDispatchInput {
    agent_id: ra_core::item::AgentId,
    tool_use: ra_core::state::ToolUseTracker,
    tool_failure: ra_core::state::ToolFailureTracker,
    run: Arc<RunContext>,
    cancel: CancelScope,
    services: ToolServices,
    max_function_tool_concurrency: usize,
    permission: PermissionEngine,
}

impl StreamedDispatchInput {
    fn start(&self) -> StreamedFunctionDispatches {
        StreamedFunctionDispatches::new(
            self.agent_id.clone(),
            self.tool_use.clone(),
            self.tool_failure.clone(),
            Arc::clone(&self.run),
            self.cancel.clone(),
            self.services.clone(),
            self.max_function_tool_concurrency,
            self.permission.clone(),
        )
    }
}

/// Facts selected after a failed attempt and recorded on the next physical request's span.
struct RetryTrace {
    delay: std::time::Duration,
    reason: Option<String>,
}

struct RetryEvaluation<'a> {
    turn_scope: &'a CancelScope,
    model: &'a Arc<dyn Model>,
    request: &'a ModelRequest,
    error: &'a Error,
    retry_settings: Option<&'a ra_core::model::ModelRetrySettings>,
    attempt: u32,
    max_retries: u32,
    consumed: CallConsumption,
}

/// Executes one prepared model call and records its provider-neutral terminal facts.
///
/// Every call streams, because a completed function call in the stream is what lets a tool overlap
/// the rest of generation. What a subscriber sees is a separate switch; both settle one
/// [`ModelResponse`] through the same settlement. That is deliberate — a second settlement path
/// for streamed turns is how two views of the same run start to disagree about what happened.
async fn call_model(
    turn_scope: &CancelScope,
    prepared: PreparedTurn,
    context: &TurnLoopContext<'_>,
    streaming_dispatch: StreamedDispatchInput,
) -> Result<(TurnActionSurface, ModelResponse, StreamedFunctionDispatches)> {
    let model = Arc::clone(prepared.model());
    let selector = prepared.selector().clone();
    let (surface, model_request) = prepared.into_call();

    // Checked here rather than left to each adapter. A cache plan names a prefix by hash, and an
    // adapter that forgets to check would send one naming text the request no longer carries —
    // silently, and only visible later as a cache that never hits. One check on the single
    // dispatch path covers every protocol, including the ones whose adapters do not exist yet.
    model_request.validate_cache_plan()?;

    // Tools start from completed stream items even when no host subscribes to raw narration, so
    // every call streams. `partial_messages` decides one separate thing: whether that narration
    // leaves the runtime. A run that forwards nothing has consumed nothing, which is what keeps
    // its failures replayable.
    let narration = context
        .config
        .partial_messages
        .then_some(context.events)
        .flatten();
    let retry_settings = model_request.model_settings().retry().cloned();
    let max_retries = retry_settings
        .as_ref()
        .and_then(ra_core::model::ModelRetrySettings::max_retries)
        .unwrap_or(0);
    let mut attempt = 0;
    let mut failed_attempts = 0;
    let mut retry_trace = None;

    loop {
        let mut consumed = CallConsumption::default();
        let response = call_model_attempt(
            ModelCallAttempt {
                turn_scope,
                model: &model,
                selector: &selector,
                events: narration,
                attempt,
                max_retries,
                retry_trace: retry_trace.take(),
                streaming_dispatch: streaming_dispatch.clone(),
            },
            model_request.clone(),
            &surface,
            &mut consumed,
        )
        .await;
        match response {
            Ok((mut response, streamed_dispatches)) => {
                if failed_attempts > 0 {
                    let usage = prepend_failed_attempts(response.usage(), failed_attempts);
                    response = response.with_usage(usage);
                }
                return Ok((surface, response, streamed_dispatches));
            }
            Err(error) => {
                let Some((decision, delay)) = evaluate_retry(RetryEvaluation {
                    turn_scope,
                    model: &model,
                    request: &model_request,
                    error: &error,
                    retry_settings: retry_settings.as_ref(),
                    attempt,
                    max_retries,
                    consumed,
                })
                .await?
                else {
                    return Err(error);
                };

                warn!(
                    error.code = error.code(),
                    retry.attempt = attempt + 1,
                    retry.max = max_retries,
                    retry.delay_ms = duration_ms(delay),
                    retry.reason = decision.reason().unwrap_or(""),
                    "retrying failed model request"
                );
                turn_scope.run(tokio::time::sleep(delay)).await?;
                attempt = attempt.saturating_add(1);
                failed_attempts = failed_attempts.saturating_add(1);
                retry_trace = Some(RetryTrace {
                    delay,
                    reason: decision.reason().map(str::to_owned),
                });
            }
        }
    }
}

/// Executes one physical model request and gives it its own generation span.
async fn call_model_attempt(
    attempt: ModelCallAttempt<'_>,
    model_request: ModelRequest,
    surface: &TurnActionSurface,
    consumed: &mut CallConsumption,
) -> Result<(ModelResponse, StreamedFunctionDispatches)> {
    let model_name = attempt.selector.model().unwrap_or("<provider_default>");
    let generation_span = info_span!(
        "generation",
        span.kind = SpanKind::Generation.label(),
        model.name = %model_name,
        model.provider = %attempt.selector.provider(),
        gen.protocol = %attempt.selector.protocol(),
        retry.attempt = attempt.attempt,
        retry.max = attempt.max_retries,
        retry.delay_ms = tracing::field::Empty,
        retry.reason = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        cancel.reason = tracing::field::Empty,
        cancel.scope = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
        usage.requests = tracing::field::Empty,
        usage.input_tokens = tracing::field::Empty,
        usage.cached_input_tokens = tracing::field::Empty,
        usage.cache_write_tokens = tracing::field::Empty,
        usage.output_tokens = tracing::field::Empty,
        usage.reasoning_tokens = tracing::field::Empty,
    );
    if let Some(retry_trace) = &attempt.retry_trace {
        generation_span.record(
            ra_core::trace::field::RETRY_DELAY_MS,
            duration_ms(retry_trace.delay),
        );
        if let Some(reason) = &retry_trace.reason {
            generation_span.record(ra_core::trace::field::RETRY_REASON, reason.as_str());
        }
    }
    let started = Instant::now();
    let response = stream_model_call(
        attempt.model,
        model_request,
        attempt.events,
        surface,
        attempt.turn_scope,
        attempt.streaming_dispatch.start(),
        consumed,
    )
    .instrument(generation_span.clone())
    .await;
    generation_span.record(
        ra_core::trace::field::DURATION_MS,
        duration_ms(started.elapsed()),
    );
    match response {
        Ok((response, streamed_dispatches)) => {
            record_generation_usage(&generation_span, response.usage());
            ra_core::trace::record_outcome(&generation_span, ra_core::trace::SpanOutcome::Ok);
            Ok((response, streamed_dispatches))
        }
        Err(error) => {
            record_terminal_error(&generation_span, &error, attempt.turn_scope);
            Err(error)
        }
    }
}

/// Applies the runner-owned retry limits and replay boundary to a policy decision.
async fn evaluate_retry(
    input: RetryEvaluation<'_>,
) -> Result<Option<(RetryDecision, std::time::Duration)>> {
    // Every model call streams, so this is a constant rather than a mode. It stays in the two
    // provider-facing values because they describe the transport a policy is reasoning about.
    const STREAMED: bool = true;

    if input.attempt >= input.max_retries
        || input.error.is_cancelled()
        || !input.error.is_retryable()
    {
        return Ok(None);
    }
    let Some(settings) = input.retry_settings else {
        return Ok(None);
    };
    let Some(policy) = settings.policy() else {
        return Ok(None);
    };

    // Asked before the adapter is, because this is not the adapter's question. The runtime is the
    // only party that knows a raw frame reached a subscriber or that a tool already ran on the
    // strength of this call, and no provider advice can make repeating either of those safe. An
    // adapter reporting `Safe` for a mid-stream drop is answering truthfully about the *request*
    // while knowing nothing about the file a tool has already written.
    if input.consumed.any() {
        return Ok(None);
    }

    let advice = input.model.get_retry_advice(&ModelRetryAdviceRequest::new(
        input.error,
        input.attempt,
        STREAMED,
        input.request.continuation(),
    ));

    // `Unsafe` means the adapter knows replay would duplicate accepted output or state. It is a
    // hard boundary, not a provider preference a policy may overrule.
    if matches!(
        advice
            .as_ref()
            .map_or_else(|| replay_safety_of(input.error), RetryAdvice::replay_safety),
        ReplaySafety::Unsafe
    ) {
        return Ok(None);
    }
    let decision = input
        .turn_scope
        .run(policy.evaluate(&RetryPolicyContext::new(
            input.error,
            input.attempt,
            input.max_retries,
            STREAMED,
            advice.as_ref(),
        )))
        .await?;
    if !decision.should_retry() {
        return Ok(None);
    }

    // Nothing left this runtime — the check above established that — so the remaining question is
    // whether the *provider* is holding state this request would be replayed against. A stateless
    // continuation is holding none, which makes the replay free regardless of what the adapter was
    // able to determine about the failure itself.
    let safety = advice
        .as_ref()
        .map_or_else(|| replay_safety_of(input.error), RetryAdvice::replay_safety);
    let nothing_to_duplicate = !input.request.continuation().is_server_managed();
    if !matches!(safety, ReplaySafety::Safe) && !nothing_to_duplicate && !decision.replay_approved()
    {
        return Ok(None);
    }

    let delay = decision.delay().unwrap_or_else(|| {
        RetryBackoff::from_settings(settings.backoff()).delay(
            input.attempt,
            advice
                .as_ref()
                .and_then(RetryAdvice::retry_after)
                .or_else(|| {
                    ra_core::model::NormalizedProviderError::from_error(input.error)
                        .and_then(ra_core::model::NormalizedProviderError::retry_after)
                }),
            ra_core::model::JitterSample::new(rand::random()),
        )
    });
    Ok(Some((decision, delay)))
}

/// Adds one zero-cost ledger entry for each request that failed before reporting usage.
fn prepend_failed_attempts(usage: &Usage, failed_attempts: u32) -> Usage {
    let mut augmented = Usage::default();
    for _ in 0..failed_attempts {
        augmented = augmented.accumulate(&Usage::from_request(RequestUsage::default()));
    }
    augmented.accumulate(usage)
}

/// Drives one streamed model call, forwarding its provider events and returning what it settled to.
///
/// # Two kinds of event arrive and only one is forwarded
///
/// Provider events are narration and go straight through. The adapter's normalized items are
/// dropped here: this turn's records reach a subscriber from settlement, attributed and with their
/// output phase decided, and forwarding the adapter's copy as well would publish each of them
/// twice — the first time as something the run has not yet judged.
///
/// # A stream that never settles is a failed call
///
/// The terminal response is the only place usage, the transport identifier and the output ordering
/// exist, so a stream that ends without one has not produced a turn no matter how much narration it
/// delivered. Rebuilding the turn from the deltas instead would mean re-deriving all three from a
/// provider-shaped vocabulary that carries no stability promise, once per protocol — and reporting
/// a turn the provider never said it finished.
async fn stream_model_call(
    model: &Arc<dyn Model>,
    request: ModelRequest,
    events: Option<&mpsc::UnboundedSender<RunStreamEvent>>,
    surface: &TurnActionSurface,
    cancel: &CancelScope,
    mut dispatches: StreamedFunctionDispatches,
    consumed: &mut CallConsumption,
) -> Result<(ModelResponse, StreamedFunctionDispatches)> {
    // `CancelScope::run` cannot wrap this call: the dispatcher below owns spawned tasks that have
    // to be drained rather than dropped when the scope fires. Its entry checkpoint is taken here
    // instead, and it earns its place twice — an already-cancelled scope opens no provider
    // request, and a deadline that expired without a timer to fire it becomes a real cancellation.
    cancel.ensure_not_cancelled()?;

    let settled = read_model_stream(
        model,
        request,
        events,
        surface,
        cancel,
        &mut dispatches,
        consumed,
    )
    .await;
    match settled {
        Ok(response) => Ok((response, dispatches)),
        Err(error) => {
            dispatches.cancel_and_drain().await;
            // Stamped from what this runtime did, not from what the provider thinks. `Unsafe`
            // travels with the error so a host reads the same boundary the retry gate applied.
            //
            // The other branch deliberately does *not* stamp `Safe`. Nothing observable left the
            // runtime, but whether the provider accepted and charged the request is its own
            // question that only an adapter can answer — and answering it here with `Safe` is
            // precisely the failure `ReplaySafety::Unknown` exists to keep available.
            Err(if consumed.any() {
                stamp_replay_safety(error, ReplaySafety::Unsafe)
            } else {
                error
            })
        }
    }
}

/// Reads the stream to its terminal response, publishing and dispatching as events arrive.
///
/// Split from [`stream_model_call`] so that every failure exit reaches the one place that drains
/// early tool work, rather than repeating the teardown at each `return`.
async fn read_model_stream(
    model: &Arc<dyn Model>,
    request: ModelRequest,
    events: Option<&mpsc::UnboundedSender<RunStreamEvent>>,
    surface: &TurnActionSurface,
    cancel: &CancelScope,
    dispatches: &mut StreamedFunctionDispatches,
    consumed: &mut CallConsumption,
) -> Result<ModelResponse> {
    let mut stream = model.stream_response(request);
    let mut settled: Option<ModelResponse> = None;
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => {
                // The fallback mirrors `CancelScope::run`: a scope that signalled is cancelled
                // whether or not a root cause was recorded, and continuing to read would be the
                // one outcome that is certainly wrong.
                return Err(cancel.reason().unwrap_or(CancelReason::Unspecified).into());
            }
            event = stream.next() => event,
        };
        let Some(event) = event else { break };

        // Resolved before the ordering check on purpose. A stream that fails after settling has
        // broken the contract *and* hit something; reporting only the broken contract would name
        // the consequence and lose the cause, which is the one thing this frame carried.
        let event = event?;
        if settled.is_some() {
            return Err(Error::provider(
                ProviderErrorKind::Behavior,
                "the model stream emitted an event after its terminal response",
            ));
        }
        match event {
            ModelStreamEvent::RawResponse(raw) => {
                // Once this leaves the model boundary it is visible to the run subscriber. A
                // replay would duplicate even a harmless-looking `response.created` frame, so
                // every published raw event closes the retry window for this call. With narration
                // switched off there is no subscriber, and nothing is published.
                if events.is_some() {
                    consumed.published = true;
                }
                emit(events, RunStreamEvent::RawResponse(raw));
            }
            ModelStreamEvent::Completed(response) => settled = Some(*response),
            ModelStreamEvent::RunItem(item) => {
                start_streamed_call(item.item(), surface, dispatches, consumed)?;
            }
            // Every other model event is the adapter's own view of items this run publishes itself.
            _ => {}
        }
    }
    settled.ok_or_else(|| {
        Error::provider(
            ProviderErrorKind::Behavior,
            "the model stream ended without a terminal response, so the turn has no usage, no \
             request identifier and no settled output order",
        )
    })
}

/// Starts a tool from one completed stream item, when the item is a function call this turn
/// advertised and the call is one that may overlap the rest of generation.
fn start_streamed_call(
    item: &RunItem,
    surface: &TurnActionSurface,
    dispatches: &mut StreamedFunctionDispatches,
    consumed: &mut CallConsumption,
) -> Result<()> {
    if !matches!(item.kind(), RunItemKind::ToolCall(_)) {
        return Ok(());
    }

    // Classified by the function settlement uses, against the same surface, so an early start can
    // never bind a name differently from the record that will answer it.
    let processed = crate::turn::process::process_model_response(
        &ModelResponse::new(vec![item.clone()]),
        surface,
    )?;

    // A handoff, a name this turn did not advertise, or a hosted call: settlement answers all of
    // them, and none of them is work that overlapping would speed up.
    let Some(action) = processed.functions().first() else {
        return Ok(());
    };
    if dispatches.start(action)? == StreamedStart::Started {
        consumed.dispatched = true;
    }
    Ok(())
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
    state: &mut RunState,
) -> Result<Option<Message>> {
    let (Some(kind), Some(handler)) = (progress.budget_stop, context.config.error_handler.as_ref())
    else {
        return Ok(None);
    };

    let error = Error::budget(kind, "the configured run budget was exhausted");
    // The same base and segment window the model used. The base can already contain the complete
    // checkpoint projection, while an explicit caller continuation remains caller-controlled.
    let data = RunErrorData::new(
        agent.public(),
        context.input_base,
        progress.segment_items(state),
        progress.segment_responses(state),
        progress.turns,
        state.budget().clone(),
        state.usage_totals().clone(),
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
            next_error_item_id(state.generated_items(), progress.turns),
            RunItemKind::Message(message.clone()),
        );
        emit(context.events, RunStreamEvent::Item(item.clone()));
        state.record_generated_items([item]);
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
