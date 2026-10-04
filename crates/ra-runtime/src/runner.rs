//! `Runner::run` and `run_streamed`: the body of the agent loop (R3-7).
//!
//! # One loop, not two
//!
//! Both entry points call the private `run_loop`. The streaming one differs by exactly one thing
//! — it is handed a channel to announce into — and that is the whole of the difference the
//! reference implementation spreads across two code paths. Two loops drift: the streamed one grows
//! a fix the other never gets, and the bug reproduces only when the host happens to subscribe.
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
//! enforced here through [`BudgetLimit`]; spend is not a dimension, because pricing is the host's.
//! Provider refusal fallback and structured-output validation remain at their provider and
//! output-contract seams; when either produces a terminal [`Error`], they use the same
//! [`RunErrorHandler`] contract.
//!
//! **Session persistence and resume.** R6-6 turns a run into a `RunState`; R9 stores the items.
//! This produces the values both will read.

use std::{any::Any, collections::BTreeSet, future::Future, ops::Range, sync::Arc, time::Instant};

use futures::StreamExt;
use ra_core::{
    agent::{AgentSpec, HandoffInputData, HandoffInputFilter, ToolUseResult},
    budget::BudgetLimit,
    cancel::{CancelReason, CancelScope, Deadline, ScopeKind},
    capability::{
        Capability, ContextProcessor, ContextProcessorRequest, ContextSummarizer,
        ContextSummaryRequest, ContextSummaryResponse, LoadSignal,
    },
    context::RunContext,
    error::{BudgetKind, Error, ProviderErrorKind, Result},
    event::{HostEvent, HostEventSink},
    filter::{
        ContextFilter, ContextFilterChain, ContextFilterReport, ContextFilterRequest,
        ModelInputData,
    },
    finish::FinishReason,
    guardrail::{
        GuardrailEvidence, GuardrailFinalOutput, InputGuardrail, InputGuardrailResult,
        OutputGuardrail, ToolInputGuardrail, ToolOutputGuardrail, merge_input_guardrails,
        merge_output_guardrails,
    },
    hook::{HookDecision, HookEvent, HookEventName, StopHookData},
    item::{
        CallId, ItemId, ItemProvenance, Message, MessageRole, ModelInputItem, ModelResponse,
        OutputPhase, RunItem, RunItemKind, ToolApproval, ToolCallOutput,
    },
    lifecycle::{
        AgentEndInput, AgentStartInput, HandoffInput, LifecycleHook, LlmEndInput, LlmStartInput,
    },
    model::{
        Model, ModelRequest, ModelResolver, ModelRetryAdviceRequest, ModelSettings,
        ModelStreamEvent, ModelTracing, ReplaySafety, RetryAdvice, RetryBackoff, RetryDecision,
        RetryPolicyContext, replay_safety_of, stamp_replay_safety,
    },
    permission::{PermissionMode, PermissionRule},
    prompt::CachePlan,
    session::{
        Session, SessionInputCallback, SessionSettings,
        rollout::{
            RolloutItem, RolloutModelUsage, RolloutRecorder, RolloutRunEnd, RolloutRunEnded,
            RolloutRunStarted, RolloutTurnContext,
        },
    },
    state::{
        EventSeqAllocator, HandoffProjection, InterruptionResolution, NestedRunRef, RunId,
        RunState, ToolOutcome, ToolUse,
    },
    step::NextStep,
    tool::{ToolLookupKey, ToolOutputReferenceExtractor, ToolServices},
    trace::SpanKind,
    usage::{RequestUsage, Usage},
};
use tokio::sync::mpsc;
use tracing::{Instrument, info_span, warn};

mod grouping;
pub mod result;
#[doc(hidden)]
pub mod session_persistence;
pub mod stream;

pub use crate::turn::prepare::ActionSurfaceBudget;

pub use result::{
    AgentToolInvocation, ContinuationInput, RunErrorData, RunErrorHandler, RunErrorHandlerInput,
    RunErrorHandlerResult, RunOutcome, RunResult, TurnRecord,
};
use result::{TurnRecordOwner, aggregate_usage, find_final_message};
pub use stream::{RunStream, RunStreamEvent};

use crate::{
    agent::{
        AgentBinding, AgentRegistry,
        control::{AgentHandle, AgentTreeRef},
        tool::ParentRun,
    },
    capability::{CapabilityPlan, DeferredPrompt},
    guardrail::{InputGuardrailCheck, StageOutcome, run_output_guardrails},
    hook::{UserHookRegistration, UserHooks},
    lifecycle::{LifecycleHooks, dispatch as lifecycle_dispatch},
    permission::PermissionEngine,
    tool::dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
    tool::guardrail::ToolGuardrails,
    turn::{
        TurnSettlementRequest,
        batch::{DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY, StreamedFunctionDispatches, StreamedStart},
        prepare::{
            PreparedTurn, ToolNameCollisionPolicy, TurnActionSurface, TurnPreparationRequest,
            prepare_turn,
        },
        resolve::check_for_final_output_from_tools,
        settle_turn,
    },
};

use crate::budget::{NestedSpend, RunSpend, budget_reminder, exhausted_budget_kind};
use crate::sandbox::{SandboxRunConfig, SandboxRuntime};

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
    agent_registry: AgentRegistry,
    handoff_input_filter: Option<Arc<dyn HandoffInputFilter>>,
    context_filters: ContextFilterChain,
    tool_output_reference_extractor: Option<Arc<dyn ToolOutputReferenceExtractor>>,
    memory_usage_sink: Option<Arc<dyn ra_core::memory::MemoryUsageSink>>,
    context_processors: Vec<Arc<dyn ContextProcessor>>,
    capabilities: Vec<Arc<dyn Capability>>,
    permission: PermissionEngine,
    input_guardrails: Vec<Arc<dyn InputGuardrail>>,
    output_guardrails: Vec<Arc<dyn OutputGuardrail>>,
    tool_input_guardrails: Vec<Arc<dyn ToolInputGuardrail>>,
    tool_output_guardrails: Vec<Arc<dyn ToolOutputGuardrail>>,
    pre_approval_tool_input_guardrails: bool,
    user_hooks: UserHooks,
    lifecycle_hooks: Vec<Arc<dyn LifecycleHook>>,
    sandbox: Option<SandboxRunConfig>,
    group_id: Option<String>,
    session_input_callback: Option<Arc<dyn SessionInputCallback>>,
    session_settings: Option<SessionSettings>,
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
            agent_registry: AgentRegistry::default(),
            handoff_input_filter: None,
            context_filters: ContextFilterChain::new(),
            tool_output_reference_extractor: None,
            memory_usage_sink: None,
            context_processors: Vec::new(),
            capabilities: Vec::new(),
            permission: PermissionEngine::default(),
            input_guardrails: Vec::new(),
            output_guardrails: Vec::new(),
            tool_input_guardrails: Vec::new(),
            tool_output_guardrails: Vec::new(),
            pre_approval_tool_input_guardrails: false,
            user_hooks: UserHooks::default(),
            lifecycle_hooks: Vec::new(),
            sandbox: None,
            group_id: None,
            session_input_callback: None,
            session_settings: None,
        }
    }

    /// Sets how a run with a session merges the session's history with its new input.
    ///
    /// Without one, the run starts from the history followed by the new input. Whatever the
    /// callback returns, the session is appended to only with the new turn's items; see
    /// [`SessionInputCallback`]. A run without a session ignores it.
    pub fn with_session_input_callback(mut self, callback: Arc<dyn SessionInputCallback>) -> Self {
        self.session_input_callback = Some(callback);
        self
    }

    /// The callback set by [`Self::with_session_input_callback`], if any.
    #[must_use]
    pub fn session_input_callback(&self) -> Option<&Arc<dyn SessionInputCallback>> {
        self.session_input_callback.as_ref()
    }

    /// Overrides the session's own settings for this run: values set here win, and values left
    /// unset keep the session's.
    pub const fn with_session_settings(mut self, settings: SessionSettings) -> Self {
        self.session_settings = Some(settings);
        self
    }

    /// The session settings this run overrides, if any.
    #[must_use]
    pub const fn session_settings(&self) -> Option<&SessionSettings> {
        self.session_settings.as_ref()
    }

    /// Configures how this run reaches the sandboxes its sandbox agents run in.
    ///
    /// Without one, a sandbox agent is refused when it is about to run rather than run with no
    /// workspace.
    pub fn with_sandbox(mut self, sandbox: SandboxRunConfig) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// How this run reaches its sandboxes, if it does.
    #[must_use]
    pub const fn sandbox(&self) -> Option<&SandboxRunConfig> {
        self.sandbox.as_ref()
    }

    /// Links this run with others the host considers one conversation or process — a chat
    /// thread's id, for instance.
    ///
    /// The reference's `RunConfig.group_id`. Sandbox memory appends the runs of one group to one
    /// rollout; a run with no group, and no conversation or session to take one from, is a
    /// rollout of its own.
    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = Some(group_id.into());
        self
    }

    /// The group this run belongs to, as the host named it.
    #[must_use]
    pub fn group_id(&self) -> Option<&str> {
        self.group_id.as_deref()
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

    /// Sets the declarations this run resolves handoff targets against.
    ///
    /// A run whose agents declare no handoff never consults it. One whose agents do has to supply
    /// it: a declaration names its target by identity so that two agents can transfer control to
    /// each other, and nothing in the running agent can turn that identity back into something
    /// runnable on its own.
    pub fn with_agent_registry(mut self, agent_registry: AgentRegistry) -> Self {
        self.agent_registry = agent_registry;
        self
    }

    /// Sets the transform applied to every transfer that declares none of its own.
    ///
    /// It is a default, not a stage: a handoff carrying its own filter uses that one instead of
    /// this, so a single transfer can opt out of a run-wide policy by declaring what it wants.
    pub fn with_handoff_input_filter(mut self, filter: Arc<dyn HandoffInputFilter>) -> Self {
        self.handoff_input_filter = Some(filter);
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

    /// Delivers validated final memory citations to a host-owned sink, with a 100 ms timeout.
    /// The sink should enqueue durably and deduplicate by run, final item, and citation token.
    pub fn with_memory_usage_sink(
        mut self,
        sink: Arc<dyn ra_core::memory::MemoryUsageSink>,
    ) -> Self {
        self.memory_usage_sink = Some(sink);
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
    /// order; dependencies check presence without reordering capabilities — see
    /// [`CapabilityPlan`] for the whole rule.
    ///
    /// A missing dependency, two capabilities claiming one family, or a capability requiring its
    /// own family fails the run before its first model call. Mutually dependent capabilities are
    /// accepted when every required family is installed.
    ///
    /// A sandbox agent's own capabilities may not claim a family installed here: the built-in
    /// sandbox capabilities share family names with the host-side ones while meaning something
    /// else, so the run fails before that agent's session is created.
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

    /// Declarations this run resolves handoff targets against.
    #[must_use]
    pub const fn agent_registry(&self) -> &AgentRegistry {
        &self.agent_registry
    }

    /// Transform applied to a transfer of control that declares none of its own.
    #[must_use]
    pub const fn handoff_input_filter(&self) -> Option<&Arc<dyn HandoffInputFilter>> {
        self.handoff_input_filter.as_ref()
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

    /// Adds one check applied to this run's input before an agent acts on it.
    ///
    /// Run-level rather than agent-level, and the two are added together: an agent's own guardrails
    /// travel with the declaration wherever it is used, while these belong to this execution of it.
    /// One identity may not appear in both — see [`merge_input_guardrails`].
    pub fn with_input_guardrail(mut self, guardrail: Arc<dyn InputGuardrail>) -> Self {
        self.input_guardrails.push(guardrail);
        self
    }

    /// Adds one check applied to this run's final output before it is delivered.
    pub fn with_output_guardrail(mut self, guardrail: Arc<dyn OutputGuardrail>) -> Self {
        self.output_guardrails.push(guardrail);
        self
    }

    /// Input guardrails this run adds to the starting agent's own.
    #[must_use]
    pub fn input_guardrails(&self) -> &[Arc<dyn InputGuardrail>] {
        &self.input_guardrails
    }

    /// Output guardrails this run adds to the finishing agent's own.
    #[must_use]
    pub fn output_guardrails(&self) -> &[Arc<dyn OutputGuardrail>] {
        &self.output_guardrails
    }

    /// Installs an optional host hook for its declared event.
    /// These are distinct from SDK lifecycle observers and tool guardrails.
    pub fn with_user_hook(mut self, registration: UserHookRegistration) -> Self {
        self.user_hooks = self.user_hooks.with_hook(registration);
        self
    }

    /// Installs a shared host hook registry, also usable for host-owned session events.
    pub fn with_user_hooks(mut self, hooks: UserHooks) -> Self {
        self.user_hooks = hooks;
        self
    }

    /// Host event hooks installed for this run.
    #[must_use]
    pub const fn user_hooks(&self) -> &UserHooks {
        &self.user_hooks
    }

    /// Installs one lifecycle narration over the whole run.
    ///
    /// The run-scoped half of [`ra_core::lifecycle`]; the agent-scoped half is declared on the
    /// agent, so that an agent reached by a handoff brings its own. This one decides nothing at any
    /// of its seven moments, which is what lets it be installed with no matcher and no
    /// declaration — there is no scope a mistake here could widen.
    ///
    /// Two hooks may share a display name, exactly as two guardrails or two host hooks may: nothing
    /// looks one up by it.
    pub fn with_lifecycle_hook(mut self, hook: Arc<dyn LifecycleHook>) -> Self {
        self.lifecycle_hooks.push(hook);
        self
    }

    /// Lifecycle narration installed on this run, in declaration order.
    #[must_use]
    pub fn lifecycle_hooks(&self) -> &[Arc<dyn LifecycleHook>] {
        &self.lifecycle_hooks
    }

    /// Installs one check a tool may declare over its arguments.
    ///
    /// Run-level, and there is no agent-level counterpart to merge with: a tool names the check it
    /// wants by [`ToolGuardrailId`](ra_core::tool::ToolGuardrailId) and this is where the object
    /// behind that name comes from. Installing something no tool declares is allowed and does
    /// nothing; declaring something nobody installed stops the run before its first model call.
    ///
    /// Unlike a run-level guardrail's name, this identity is a lookup key, so two installations
    /// under one ID are refused when the run starts.
    pub fn with_tool_input_guardrail(mut self, guardrail: Arc<dyn ToolInputGuardrail>) -> Self {
        self.tool_input_guardrails.push(guardrail);
        self
    }

    /// Installs several argument checks, in iteration order.
    pub fn with_tool_input_guardrails(
        mut self,
        guardrails: impl IntoIterator<Item = Arc<dyn ToolInputGuardrail>>,
    ) -> Self {
        self.tool_input_guardrails.extend(guardrails);
        self
    }

    /// Installs one check a tool may declare over its results.
    pub fn with_tool_output_guardrail(mut self, guardrail: Arc<dyn ToolOutputGuardrail>) -> Self {
        self.tool_output_guardrails.push(guardrail);
        self
    }

    /// Installs several result checks, in iteration order.
    pub fn with_tool_output_guardrails(
        mut self,
        guardrails: impl IntoIterator<Item = Arc<dyn ToolOutputGuardrail>>,
    ) -> Self {
        self.tool_output_guardrails.extend(guardrails);
        self
    }

    /// Also checks a call's arguments *before* the host is interrupted to approve it.
    ///
    /// Off by default, and it buys one thing: nobody is asked to approve a call that is going to be
    /// refused anyway. It costs the check twice on every approved call.
    ///
    /// **It does not replace the check after the answer comes back.** That one always runs, because
    /// an approval can return in a later process against a registry the host has since changed, and
    /// a decision reached before the interruption is not a decision about the run that resumes.
    pub const fn with_pre_approval_tool_input_guardrails(mut self, enabled: bool) -> Self {
        self.pre_approval_tool_input_guardrails = enabled;
        self
    }

    /// Argument checks this run installed, in installation order.
    #[must_use]
    pub fn tool_input_guardrails(&self) -> &[Arc<dyn ToolInputGuardrail>] {
        &self.tool_input_guardrails
    }

    /// Result checks this run installed, in installation order.
    #[must_use]
    pub fn tool_output_guardrails(&self) -> &[Arc<dyn ToolOutputGuardrail>] {
        &self.tool_output_guardrails
    }

    /// Whether a call awaiting approval has its arguments checked before the host is interrupted.
    #[must_use]
    pub const fn pre_approval_tool_input_guardrails(&self) -> bool {
        self.pre_approval_tool_input_guardrails
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
            .field("has_memory_usage_sink", &self.memory_usage_sink.is_some())
            .field("partial_messages", &self.partial_messages)
            .field(
                "tool_name_collision_policy",
                &self.tool_name_collision_policy,
            )
            .field("action_surface_budget", &self.action_surface_budget)
            .field("agent_registry", &self.agent_registry)
            .field("handoff_input_filter", &self.handoff_input_filter.is_some())
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
            .field("sandbox", &self.sandbox)
            .field("group_id", &self.group_id)
            .field(
                "has_session_input_callback",
                &self.session_input_callback.is_some(),
            )
            .field("session_settings", &self.session_settings)
            .field(
                "input_guardrails",
                &self
                    .input_guardrails
                    .iter()
                    .map(|guardrail| guardrail.name())
                    .collect::<Vec<_>>(),
            )
            .field(
                "output_guardrails",
                &self
                    .output_guardrails
                    .iter()
                    .map(|guardrail| guardrail.name())
                    .collect::<Vec<_>>(),
            )
            .field(
                "tool_input_guardrails",
                &self
                    .tool_input_guardrails
                    .iter()
                    .map(|guardrail| guardrail.id().as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "tool_output_guardrails",
                &self
                    .tool_output_guardrails
                    .iter()
                    .map(|guardrail| guardrail.id().as_str())
                    .collect::<Vec<_>>(),
            )
            .field("user_hooks", &self.user_hooks)
            .field(
                "lifecycle_hooks",
                &self
                    .lifecycle_hooks
                    .iter()
                    .map(|hook| hook.name())
                    .collect::<Vec<_>>(),
            )
            .field(
                "pre_approval_tool_input_guardrails",
                &self.pre_approval_tool_input_guardrails,
            )
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
    tool_input: Option<Arc<serde_json::Value>>,
    agent_handle: Option<AgentHandle>,
    /// The agent tree an unbound run executes in: a nested agent-tool run started inside it.
    agent_tree: Option<AgentTreeRef>,
    nested_spend: Option<NestedSpend>,
    rollout: Option<Arc<dyn RolloutRecorder>>,
    /// How many leading items of `input` the rollout already holds.
    recorded_input: usize,
    session: Option<Arc<dyn Session>>,
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
            tool_input: None,
            agent_handle: None,
            agent_tree: None,
            rollout: None,
            nested_spend: None,
            recorded_input: 0,
            session: None,
        }
    }

    /// Records the structured arguments of the agent-tool call this run answers.
    pub(crate) fn with_tool_input(mut self, tool_input: serde_json::Value) -> Self {
        self.tool_input = Some(Arc::new(tool_input));
        self
    }

    /// Records this run into a session rollout through `recorder`.
    ///
    /// The run records that it started and on what new input, the context of its first model call,
    /// every session record it produces, every host event attributed to it, the usage of every
    /// model call it pays for, and how it ended; it then waits for the recorder to flush, and logs
    /// rather than fails if that does not succeed. These are Codex's turn records: its turn is a
    /// run here.
    ///
    /// Only this run is recorded. A nested agent-tool run and a spawned agent are not given the
    /// recorder, and their events, though they reach the same event sink, are not recorded with
    /// this run's; a spawned agent records into a rollout of its own when its tree has a store for
    /// them (see [`AgentControl::with_rollout_store`](crate::agent::control::AgentControl::with_rollout_store)). Events are recorded whether or not [`Self::with_services`] installed an event
    /// sink; one that was installed receives them as before.
    pub fn with_rollout_recorder(mut self, recorder: Arc<dyn RolloutRecorder>) -> Self {
        self.rollout = Some(recorder);
        self
    }

    /// Reads this run's history from `session` and appends what the run adds to it.
    ///
    /// The reference's `session` argument. A fresh run starts from the session's history followed
    /// by its input — or whatever [`RunConfig::with_session_input_callback`] makes of the two —
    /// and the session is given the new input before the first model call and the run's records as
    /// each turn settles. A final answer is appended once the output guardrails have passed it, and
    /// a run whose input guardrail tripped appends its input and nothing else.
    ///
    /// A run continued from its checkpoint takes its history from the checkpoint and appends only
    /// what it has not appended yet, so the same session must be passed again; it must carry no
    /// input of its own, since the session already holds the conversation that input would be a
    /// projection of.
    ///
    /// If persistence fails, continue from [`Error::run_state`] rather than an earlier checkpoint.
    /// It retains completed tool work and reconciles uncertain appends before continuing. The host
    /// must serialize access to the original backend, including other copies of that checkpoint.
    pub fn with_session(mut self, session: Arc<dyn Session>) -> Self {
        self.session = Some(session);
        self
    }

    /// Records only the input after its first `count` items as new: the rollout already holds the
    /// rest. A spawned agent's run starts on the agent's history followed by its mail, and the
    /// history is in the agent's rollout from its earlier runs, as Codex's thread holds its history
    /// and records only a turn's new input.
    pub(crate) const fn with_recorded_input(mut self, count: usize) -> Self {
        self.recorded_input = count;
        self
    }

    /// Runs this request inside `tree` without binding it to an agent there, as a nested
    /// agent-tool run inherits the tree of the run that started it.
    pub(crate) fn with_agent_tree(mut self, tree: Option<AgentTreeRef>) -> Self {
        self.agent_tree = tree;
        self
    }

    /// Bills this run's model calls to the run whose agent-tool call started it as well.
    pub(crate) fn with_nested_spend(mut self, nested_spend: NestedSpend) -> Self {
        self.nested_spend = Some(nested_spend);
        self
    }

    /// Sets the run-level configuration.
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = config;
        self
    }

    /// Marks a fresh run as a child of another run. The parent is persisted in [`RunState`] so
    /// approval resume uses `SubagentStop` without relying on the host to repeat this setting.
    pub fn with_parent_run_id(mut self, parent: RunId) -> Result<Self> {
        self.state.assign_parent_run_id(parent)?;
        Ok(self)
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

    /// Runs this request as the agent `handle` names in its agent tree.
    ///
    /// The run's tools reach the tree through [`ToolServices::agent_control`], acting as that
    /// agent, whatever services [`Self::with_services`] installed. Mail sent to the agent is
    /// delivered into the run at model-call boundaries — from the second model call on when the
    /// request carries new input, so that input is answered first, as Codex drains pending input.
    /// The run marks the agent running when it starts and records how it ended; a spawned agent's
    /// run then reports its result to its parent. In a tree built
    /// [`with_close_descendants_on_cancel`](crate::agent::control::AgentControl::with_close_descendants_on_cancel),
    /// a cancelled run also closes the agents below its agent before it returns.
    pub fn with_agent_control(mut self, handle: AgentHandle) -> Self {
        self.agent_handle = Some(handle);
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
        let sandbox = sandbox_runtime(&request);
        let mut abandoned = ReleaseSandboxOnDrop(Some(Arc::clone(&sandbox)));
        let result = Box::pin(run_loop(request, None, sandbox)).await;
        abandoned.0 = None;
        result
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
    ///
    /// **An aborted run's sandboxes are still released.** The drain grace bounds the run, not the
    /// cleanup of the sessions it owns: after aborting a run that did not stop in time, the reaper
    /// waits for that cleanup — or starts it, if the run never got that far.
    #[must_use]
    pub fn run_streamed(request: RunRequest) -> RunStream {
        Self::run_streamed_in(request, tracing::Span::none())
    }

    /// [`Self::run_streamed`], with the background run's spans nested under `span`.
    ///
    /// A spawned task does not inherit the spawner's span. A nested agent run streamed from inside
    /// a tool call passes the call's span, so its spans sit where they would had it been awaited
    /// through [`Self::run`].
    pub(crate) fn run_streamed_in(mut request: RunRequest, span: tracing::Span) -> RunStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        // The run gets its own child scope so dropping the stream cancels this run without
        // touching the caller's scope, while a cancellation from above still propagates down.
        let scope = request.cancel.child(ScopeKind::Run);
        request.cancel = scope.clone();
        let guard = scope.cancel_on_drop(CancelReason::UserInterrupt);
        let sandbox = sandbox_runtime(&request);
        let supervised = sandbox.enabled().then(|| Arc::clone(&sandbox));
        let task = tokio::spawn(
            async move { Box::pin(run_loop(request, Some(sender), sandbox)).await }
                .instrument(span),
        );
        RunStream::new(receiver, task, guard, supervised)
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
    /// Structured agent-tool arguments this run answers, projected into every live context.
    tool_input: Option<&'a Arc<serde_json::Value>>,
    services: &'a ToolServices,
    cancel: &'a CancelScope,
    closeout_cancel: &'a CancelScope,
    config: &'a RunConfig,
    permission: &'a PermissionEngine,
    tool_guardrails: &'a ToolGuardrails,
    events: RunEvents<'a>,
    event_seqs: &'a EventSeqAllocator,
    /// Capability fragments resolved at assembly and still waiting for the signal that earns them.
    deferred_prompts: &'a [DeferredPrompt],
    /// Prepares sandbox agents before their turns.
    sandbox: &'a SandboxRuntime,
    /// The agent this run is bound to in its tree: the mailbox it delivers at model-call
    /// boundaries, and the budget reminders it is owed.
    agent_mail: Option<&'a AgentHandle>,
    /// Whether the first model call of this segment already takes pending mail.
    deliver_mail_first: bool,
    /// What the run has spent, as the runs it starts and its token ceiling see it.
    spend: &'a RunSpend,
    /// Whether this segment continues a turn that stopped for approval rather than starting one.
    continues_turn: bool,
    /// The session the run's history is read from and appended to.
    session: Option<&'a dyn Session>,
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
    /// The records a resumed segment settled before its first turn, when those settled the run.
    ///
    /// A resume that concludes on the stop policy has no turn of its own to read the delivery from,
    /// and these are what that delivery consists of.
    resumed_conclusion: Option<Range<usize>>,
    /// What the agent-tool runs this segment started spent, as moved into the run's ledger.
    nested_usage: Usage,
    /// Billed session compactions, which do not produce ordinary model responses.
    session_compaction_usage: std::sync::Mutex<Usage>,
    /// Whether this segment has recorded its turn context in the run's rollout.
    turn_context_recorded: bool,
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
    mut request: RunRequest,
    events: Option<mpsc::UnboundedSender<RunStreamEvent>>,
    sandbox: Arc<SandboxRuntime>,
) -> Result<RunResult> {
    // Before anything copies the allocator: in an agent tree, a run continued from its checkpoint
    // draws from the sequence an earlier segment of it registered there, which a completion it
    // asked to be told about may still be drawing from.
    let agent_tree = request
        .agent_handle
        .as_ref()
        .map(AgentHandle::tree_ref)
        .or_else(|| request.agent_tree.clone());
    if let Some(tree) = &agent_tree {
        request.event_seqs = tree.adopt_event_seqs(request.event_seqs.clone());
    }
    let rollout = start_recording(&mut request);
    let rollout_run_id = request.run_id.clone();
    // The name is the one the run starts with. A handoff replaces the running agent mid-loop, and
    // this span keeps the original name because it is the whole run's span. Per-agent attribution
    // is what the turn spans underneath carry, each recording the agent that ran it; a span that
    // renamed itself on a transfer would leave one run reported under two names.
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
    // Everything a cancellation notification needs, captured before the loop consumes the request
    // — and only when someone is listening, so the default run pays nothing for it.
    let interrupt = request
        .config
        .user_hooks()
        .has_event(HookEventName::Interrupt)
        .then(|| InterruptNotice::new(&request));
    // Settled after the loop, whichever way it ended: the sessions the run owns are cleaned up on
    // a failed or cancelled run exactly as on a finished one.
    //
    // What this run has spent as the runs it starts see it; see `RunSpend`. A run bound to an
    // agent tree with a rollout budget charges it here, once for every call its ledger receives.
    let agent_handle = request.agent_handle.clone();
    let spend = RunSpend::new(
        request.state.usage_totals(),
        request.nested_spend.clone(),
        agent_handle.as_ref().and_then(AgentHandle::rollout_budget),
    );
    // Installed around the loop so a tool this run dispatches can start a nested run from it; see
    // `agent::tool::parent` for why this is a task-local rather than a field of the tool context.
    let parent = ParentRun::new(
        Arc::clone(&request.model_resolver),
        request.config.clone(),
        request.app_context.clone(),
        request.run_id.clone(),
        Arc::clone(request.agent.public()),
        request.services.clone(),
        request.event_seqs.clone(),
        agent_tree,
        Arc::clone(&spend),
    );
    if let Some(handle) = &agent_handle {
        handle.run_started(request.agent.public_id());
    }
    let result = parent
        .scope(Box::pin(run_loop_inner(
            request,
            events,
            &agent_span,
            &sandbox,
            &spend,
        )))
        .instrument(agent_span.clone())
        .await;
    let result = settle_sandbox(&sandbox, result)
        .instrument(agent_span.clone())
        .await;
    // Written before the agent's status is published, so whoever that wakes — the host, or a
    // parent waiting on the agent — finds the run's end in its rollout.
    if let Some(rollout) = &rollout {
        record_run_end(rollout.as_ref(), &rollout_run_id, &result).await;
    }
    if let Some(handle) = &agent_handle {
        handle.run_ended(&result).await;
    }
    if let Some(interrupt) = interrupt
        && result
            .as_ref()
            .is_err_and(ra_core::error::Error::is_cancelled)
    {
        interrupt.notify().await;
    }
    agent_span.record(
        ra_core::trace::field::DURATION_MS,
        duration_ms(started.elapsed()),
    );
    result
}

/// Records that a run given a rollout recorder started, and routes the run's host events to the
/// recorder as well as to the sink the host installed. Returns the recorder, if there is one.
fn start_recording(request: &mut RunRequest) -> Option<Arc<dyn RolloutRecorder>> {
    let rollout = request.rollout.clone()?;
    let started = RolloutRunStarted::new(request.run_id.clone(), request.agent.public_id().clone());
    let mut started = if input_is_continuation_base(&request.state, &request.input) {
        started.with_continuation_base(request.input.clone())
    } else {
        let recorded = request.recorded_input.min(request.input.len());
        started.with_input(request.input[recorded..].to_vec())
    };
    if let Some(parent) = request.state.parent_run_id() {
        started = started.with_parent_run_id(parent.clone());
    }
    rollout.record(RolloutItem::RunStarted(started));
    request.services = request
        .services
        .clone()
        .with_event_sink(Arc::new(RecordingSink {
            run_id: request.run_id.clone(),
            recorder: Arc::clone(&rollout),
            inner: request.services.event_sink().cloned(),
        }));
    Some(rollout)
}

/// The event sink of a run that is recorded: every event reaches the sink the host installed, and
/// those attributed to the run are recorded as well.
///
/// The sink is handed on with the run's services — to its tools, to the nested runs its agent-tool
/// calls start, and to the agents it spawns — so it filters by run rather than recording whatever
/// passes through: a nested run or a spawned agent is not part of this run's rollout, while an
/// event recorded for this run after it returned, such as the completion of an agent it spawned, is.
struct RecordingSink {
    run_id: RunId,
    recorder: Arc<dyn RolloutRecorder>,
    inner: Option<Arc<dyn HostEventSink>>,
}

impl HostEventSink for RecordingSink {
    fn emit(&self, event: HostEvent) {
        if *event.run_id() == self.run_id {
            self.recorder.record(RolloutItem::Event(event.clone()));
        }
        if let Some(inner) = &self.inner {
            inner.emit(event);
        }
    }
}

/// Records how a recorded run ended and waits for the recorder to write it all.
///
/// A recorder that cannot is logged rather than turned into a failure of the run: the run's work
/// and its result do not depend on the log, as Codex's turn does not fail on a rollout flush.
async fn record_run_end(rollout: &dyn RolloutRecorder, run_id: &RunId, result: &Result<RunResult>) {
    let ended = match result {
        Ok(result) => match result.outcome() {
            RunOutcome::Interrupted { .. } => {
                RolloutRunEnded::new(run_id.clone(), RolloutRunEnd::Interrupted)
            }
            outcome => {
                let ended = RolloutRunEnded::new(run_id.clone(), RolloutRunEnd::Completed);
                match outcome.finish_reason() {
                    Some(reason) => ended.with_finish_reason(reason),
                    None => ended,
                }
            }
        },
        Err(error) if error.is_cancelled() => {
            RolloutRunEnded::new(run_id.clone(), RolloutRunEnd::Cancelled)
        }
        Err(error) => RolloutRunEnded::new(run_id.clone(), RolloutRunEnd::Failed)
            .with_error(error.to_string()),
    };
    rollout.record(RolloutItem::RunEnded(ended));
    if let Err(error) = rollout.flush().await {
        warn!(run_id = %run_id, %error, "the run's rollout could not be flushed");
    }
}

/// What it takes to tell a host that its run was cancelled, held across the loop that consumed it.
///
/// A cancelled run returns no [`RunResult`], so the callback is handed the entry checkpoint's read
/// view rather than accounting invented for a turn that never settled. It runs under a fresh scope
/// of its own — the run's is already cancelled, and a callback under it would be dropped before it
/// started — bounded by the same drain grace everything else gets on the way out. Its decision is
/// discarded: cancellation is a notification, and nothing said here revives the run.
struct InterruptNotice {
    hooks: UserHooks,
    cancel: CancelScope,
    run: Arc<RunContext>,
    services: ToolServices,
}

impl InterruptNotice {
    fn new(request: &RunRequest) -> Self {
        let mut run = RunContext::new(request.run_id.clone(), request.agent.public())
            .with_budget(request.state.budget().clone())
            .with_usage_totals(request.state.usage_totals().clone())
            .with_event_seq_allocator(request.event_seqs.clone());
        if let Some(app_context) = &request.app_context {
            run = run.with_app_context(Arc::clone(app_context));
        }
        if let Some(tool_input) = &request.tool_input {
            run = run.with_tool_input(Arc::clone(tool_input));
        }
        Self {
            hooks: request.config.user_hooks().clone(),
            cancel: request.cancel.clone(),
            run: Arc::new(run),
            services: request.services.clone(),
        }
    }

    async fn notify(self) {
        let Some(reason) = self.cancel.reason() else {
            return;
        };
        let cleanup =
            CancelScope::root().with_deadline(Deadline::after(ra_core::cancel::DRAIN_GRACE));
        let _cleanup_deadline = arm_deadline(&cleanup);
        let _ = self
            .hooks
            .bind(self.run, cleanup.clone(), self.services)
            .dispatch(HookEvent::Interrupt { reason: &reason })
            .await;
        cleanup.cancel(reason);
    }
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
    sandbox: &SandboxRuntime,
    spend: &RunSpend,
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
        tool_input,
        agent_handle,
        agent_tree: _,
        nested_spend: _,
        rollout,
        recorded_input: _,
        session,
    } = request;
    let services = match &agent_handle {
        Some(handle) => services.with_agent_control(Arc::new(handle.clone())),
        None => services,
    };
    // Codex drains pending input before the first model call only when the turn brought none of
    // its own, so new input is answered before anything that was waiting.
    let deliver_mail_first = requested_input.is_empty();

    if let Err(error) =
        validate_config(&config).and_then(|()| sandbox.assert_agent_supported(agent.public()))
    {
        // Ahead of the run scope, so there is no cancellation to attribute and no aggregate to
        // report — but the span still has to say the run ended and why.
        ra_core::trace::record_error(span, &error);
        return Err(error);
    }

    // Merged here, beside the rest of the configuration: two guardrails declared under one
    // identity is a configuration mistake, and the only thing worse than refusing it is refusing it
    // after the run has paid for a model call. The output list is merged now and used much later
    // for the same reason — a run that would have been refused at delivery should not be started.
    //
    // Merged against the **starting** agent, and the two lists mean different things by that. The
    // input checks examine the caller's input, which only the starting agent ever sees, so this is
    // their final form. The output list belongs to whoever's answer is delivered, and a handoff can
    // make that somebody else — so this merge is the early refusal for the ordinary run, and the
    // delivery site re-merges when control has moved. The starting agent's declaration is still
    // checked here, because a run that would be refused at delivery should not be started.
    let input_guardrails =
        merge_input_guardrails(agent.public().input_guardrails(), config.input_guardrails());
    let starting_agent = Arc::clone(agent.public());
    let output_guardrails = merge_output_guardrails(
        agent.public().output_guardrails(),
        config.output_guardrails(),
    );

    // Indexed against the agent the run **starts** with: the agent-scoped half moves to whoever
    // control is transferred to, which is a thing that happens later and only once. Nothing is
    // validated here, because there is nothing to validate — a lifecycle hook's name is a display
    // label two hooks may share, and no event has to be subscribed to.
    let mut lifecycle =
        LifecycleHooks::installed(config.lifecycle_hooks(), agent.public().lifecycle_hooks());

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
    if state.terminal_unrecoverable() {
        return Err(Error::caller(
            "this run completed terminal work whose Session append failed and cannot be resumed",
        )
        .with_run_state(state));
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

    let resuming = state.current_agent().is_some();
    // A segment that answers questions the last one stopped on finishes that turn rather than
    // starting a new one, which is what the agent tree's budget reminder is keyed to.
    let continues_turn =
        !state.pending_interruptions().is_empty() || !state.nested_runs().is_empty();
    // A fresh run with a session starts from the session's history and owes the session its new
    // input. A continuation takes its history from the checkpoint, as the reference's resumed
    // state does, and owes nothing for input.
    let (requested_input, session_input) = match session.as_deref() {
        None => (requested_input, Vec::new()),
        Some(_) if resuming && !requested_input.is_empty() => {
            let error = Error::caller(
                "a run continued from its checkpoint with a session takes its history from the \
                 checkpoint and the session; pass no input, and start a new run for a new turn",
            );
            ra_core::trace::record_error(span, &error);
            return Err(error);
        }
        Some(_) if resuming => (requested_input, Vec::new()),
        Some(session) => {
            let plan = cancel
                .run(session_persistence::prepare_input_with_session(
                    &run_id,
                    &requested_input,
                    session,
                    config.session_input_callback().map(AsRef::as_ref),
                    config.session_settings(),
                ))
                .await
                .and_then(|plan| plan);
            match plan {
                Ok(plan) => {
                    if session.compaction().is_some() {
                        state.set_session_compaction(
                            ra_core::session::SessionCompactionContext::new(
                                plan.generation(),
                                Vec::new(),
                                None,
                                None,
                            ),
                        );
                    }
                    plan.into_parts()
                }
                Err(error) => {
                    record_terminal_error(span, &error, &cancel);
                    return Err(error);
                }
            }
        }
    };
    if let Err(error) = state.begin_segment(agent.public_id().clone(), requested_input.clone()) {
        ra_core::trace::record_error(span, &error);
        return Err(error);
    }
    if session.is_some() {
        state.bind_session_persistence();
    }
    // Read from the state that owns it rather than re-derived from this segment's arguments.
    // `begin_segment` above is what decides it, it survives a checkpoint, and a second encoding
    // here would have to be kept true by hand across resume paths that never meet.
    let authoritative_history_complete = state.input_history_is_complete();
    let input_base = segment_input_base(&state, resuming, requested_input);
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
            tool_input.as_ref(),
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

    // Indexed and checked after assembly, because a capability contributes tools of its own and a
    // declaration on one of them has to resolve like any other. Before the first model call, which
    // is the point: a run that names a check nobody installed is misconfigured, and finding that
    // out after a model has been paid to choose a tool is finding it out too late.
    let tool_guardrails = match ToolGuardrails::install(
        config.tool_input_guardrails().iter().map(Arc::clone),
        config.tool_output_guardrails().iter().map(Arc::clone),
        config.pre_approval_tool_input_guardrails(),
    )
    .and_then(|guardrails| {
        guardrails
            .preflight(agent.execution().tools())
            .map(|()| guardrails)
    }) {
        Ok(guardrails) => guardrails,
        Err(error) => {
            record_terminal_error(span, &error, &cancel);
            return Err(error);
        }
    };

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
        resumed_conclusion: None,
        nested_usage: Usage::default(),
        session_compaction_usage: std::sync::Mutex::new(Usage::default()),
        turn_context_recorded: false,
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
        tool_input: tool_input.as_ref(),
        services: &services,
        cancel: &cancel,
        closeout_cancel: &closeout_cancel,
        config: &config,
        permission: &permission,
        tool_guardrails: &tool_guardrails,
        events: RunEvents {
            stream: events.as_ref(),
            rollout: rollout.as_deref(),
        },
        event_seqs: &event_seqs,
        deferred_prompts: &deferred_prompts,
        sandbox,
        agent_mail: agent_handle.as_ref(),
        deliver_mail_first,
        spend,
        continues_turn,
        session: session.as_deref(),
    };
    // The stage runs once per run, on the segment that opens it. A continuation does not repeat
    // the caller's opening input, so a check written against that input has nothing new to look
    // at — and the reference implementation gates the same way, on being the run's first turn.
    //
    // Decided by an explicit mark rather than by comparing the verdicts already recorded: two
    // checks may share a name, and a check cancelled before it finished leaves no verdict at all,
    // so neither the names nor the count of results can say whether the stage has happened.
    let input_check = (!state.input_guardrails_started())
        .then(|| {
            InputGuardrailCheck::prepare(
                input_guardrails,
                live_context(&context, &agent, &state),
                state.original_input().to_vec(),
            )
        })
        .flatten();
    // The blocking half of the input stage, the settlement of checkpointed answers, and the turns
    // themselves are one stage as far as stopping is concerned: all three run under the run scope,
    // so any of them can be what a deadline interrupts. Joining them into a single `Result` before
    // the match below is what keeps the translation underneath a single statement of the rule
    // rather than three copies to keep in step — a deadline that expired while a blocking check
    // was still thinking is the same budget stop it would have been one line later.
    let stepped = async {
        state.snapshot_event_seq(&event_seqs);
        session_persistence::resume_pending_session_write(
            context.session,
            &mut state,
            &cancel,
            &|usage| record_session_compaction_spend(&context, &progress, usage),
        )
        .await?;
        // Before anything that can stop the run, the input checks included: the reference appends
        // a run's input before its first turn, so a tripped check still leaves the question asked
        // in the session.
        if let Some(session) = context.session
            && !session_input.is_empty()
        {
            let count = state.session_persisted_item_count().unwrap_or(0);
            session_persistence::append_session_items(
                session,
                &mut state,
                session_input,
                count,
                &cancel,
                &|usage| record_session_compaction_spend(&context, &progress, usage),
            )
            .await?;
        }
        if !state.subagent_started()
            && let Some(parent) = state.parent_run_id().cloned()
        {
            config
                .user_hooks()
                .bind(
                    Arc::new(live_context(&context, &agent, &state)),
                    cancel.clone(),
                    services.clone(),
                )
                .dispatch(HookEvent::SubagentStart {
                    parent_run_id: &parent,
                })
                .await?;
            state.mark_subagent_started();
        }
        // The opening half of the agent bracket. An activation is every time the running agent
        // changes, and the start of a segment is one: a run continued in another process gets a
        // fresh set of hook objects, and a host that set something up on the first segment has
        // nothing set up on the continuation unless it is told again. `is_resumed` is what tells
        // the two apart.
        if !lifecycle.is_empty() {
            let run = live_context(&context, &agent, &state);
            let mut starting =
                AgentStartInput::new(&run, context.input_base).with_services(&services);
            if resuming {
                starting = starting.resumed();
            }
            lifecycle_dispatch::agent_start(&lifecycle, &starting, &cancel).await?;
        }
        let input_check = run_blocking_input_guardrails(input_check, &mut state, &cancel).await?;
        // Only when an interrupted turn has answers to settle: an approved call runs on the tools
        // the prepared agent carries. After the blocking checks either way, so a tripped one stops
        // the run before a sandbox is created, started or changed. Every other run is prepared at
        // the top of its first turn, as the reference prepares at the top of each loop.
        if !state.pending_interruption_resolutions().is_empty() || !state.nested_runs().is_empty() {
            agent = cancel
                .run(sandbox.prepare_agent(
                    &agent,
                    context.model_resolver.as_ref(),
                    context.config.model.as_deref(),
                ))
                .await
                .and_then(|prepared| prepared)?;
        }
        // Settled like the turn it finishes: a call still waiting on a nested run's question is asked
        // again straight away — the model has nothing new to read until that call has an output —
        // and results the stop policy promotes end the run as they would have ended that turn,
        // through the same stop hook, delivery and output checks.
        let resumed_from = progress.segment_items(&state).len();
        let resumed = resolve_interrupted_turn(&context, &agent, &mut state, &lifecycle).await?;
        // The turn this segment finishes was settled on a spent shared budget, or the budget was
        // spent while it waited: as at the end of a turn, it ends the run once its answers are in.
        if continues_turn
            && !matches!(resumed, ResumeStage::Interrupted(_))
            && spend.rollout_budget_exhausted()
        {
            progress.budget_stop = Some(BudgetKind::Tokens);
            return Ok(RunOutcome::Completed {
                reason: FinishReason::BudgetExhausted,
            });
        }
        match resumed {
            ResumeStage::Interrupted(items) => return Ok(RunOutcome::Interrupted { items }),
            ResumeStage::Concluded(reason) => {
                progress.resumed_conclusion =
                    Some(resumed_from..progress.segment_items(&state).len());
                let outcome = RunOutcome::Completed { reason };
                if !continue_from_stop_hook(&context, &agent, &mut state, &mut progress, &outcome)
                    .await?
                {
                    return Ok(outcome);
                }
                progress.resumed_conclusion = None;
            }
            ResumeStage::Continue => {
                if let Some(session) = context.session {
                    state.snapshot_event_seq(&event_seqs);
                    session_persistence::save_session_items(
                        session,
                        &mut state,
                        &cancel,
                        &|usage| record_session_compaction_spend(&context, &progress, usage),
                    )
                    .await?;
                }
            }
        }
        // Boxed for the reason `Runner::run` boxes the loop: this future carries a whole turn, and
        // the caller composing runs should not hold all of it inline.
        Box::pin(run_turns(
            &context,
            &mut agent,
            &mut state,
            &mut progress,
            &mut lifecycle,
            input_check,
        ))
        .await
    }
    .await;

    // Whatever the loop's agent-tool calls spent before it stopped belongs in the ledger the result,
    // the checkpoint and an error handler read, whichever way it stopped — including a stage that
    // failed after a nested run had already been paid for. Nothing inside the loop reads the ledger
    // for spend: the budget checks and the code the loop runs read the live spend instead.
    let nested = absorb_nested_spend(spend, &mut state, &mut progress);
    // Agent-tool calls are billed to this run, so the rollout's usage records add up to its ledger.
    if let (Some(rollout), Some(nested)) = (rollout.as_deref(), nested) {
        rollout.record(RolloutItem::ModelUsage(RolloutModelUsage::new(
            run_id.clone(),
            nested,
        )));
    }

    // The one place an expired wall clock is read back as a budget stop. Everything under the run
    // scope reports expiry the same way any other cancellation is reported, which is what lets the
    // loop stay free of deadline special cases; translating it here — rather than at each of the
    // four `?` inside — is why exactly one kind of stop can be a soft one.
    let outcome = match stepped {
        Ok(outcome) => outcome,
        Err(error)
            if !state.has_session_write_checkpoint()
                && is_wall_clock_expiry(&error, &cancel, budget_deadline) =>
        {
            progress.budget_stop = Some(BudgetKind::WallClock);
            RunOutcome::Completed {
                reason: FinishReason::BudgetExhausted,
            }
        }
        Err(error) => {
            return Err(terminal_failure(
                span,
                sandbox,
                &input_base,
                &progress,
                &state,
                error,
                &cancel,
            ));
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
                return Err(terminal_failure(
                    span,
                    sandbox,
                    &input_base,
                    &progress,
                    &state,
                    error,
                    &closeout_cancel,
                ));
            }
        };

    // Resolve delivery once for both output checks and the result. A blocked candidate stays in
    // history, but its turn no longer has a final-output decision.
    //
    // Only a run that concluded delivers its concluding turn's answer: a turn that answered on a
    // spent shared budget keeps the answer in history, and the run ends without handing it over.
    let final_message = final_message.or_else(|| {
        outcome
            .finish_reason()
            .filter(|reason| reason.is_complete())
            .and_then(|_| concluding_turn_message(&state, &progress).cloned())
    });

    // The closing half of the agent bracket, and ahead of the output guardrails: an observer is
    // told the answer the agent produced, not the one a check may go on to refuse. Only a run that
    // ended with an answer reaches here — a run handed back with approvals outstanding raises
    // nothing, and a run that failed or was cancelled returned long before this line.
    //
    // Only final answers close the agent lifecycle; resumable budget stops do not.
    if !lifecycle.is_empty()
        && let Some(reason) = outcome
            .finish_reason()
            .filter(|reason| reason.is_complete())
    {
        let run = live_context(&context, &agent, &state);
        let tool_outputs = concluding_turn_tool_outputs(&state, &progress);
        let mut ending = AgentEndInput::new(&run, reason)
            .with_services(&services)
            .with_tool_outputs(&tool_outputs);
        if let Some(message) = &final_message {
            ending = ending.with_message(message);
        }
        if let Err(error) =
            lifecycle_dispatch::agent_end(&lifecycle, &ending, &closeout_cancel).await
        {
            return Err(terminal_failure(
                span,
                sandbox,
                &input_base,
                &progress,
                &state,
                error,
                &closeout_cancel,
            ));
        }
    }

    // Only for a run that reached its own conclusion. A run stopped from outside — an exhausted
    // budget, the turn cap, an interrupt — has no answer the agent chose, and the closeout above is
    // the host's own text rather than something the host needs protecting from.
    //
    // Under the run scope, so a guardrail is bounded by the same deadline everything else is.
    //
    // **A deadline that expires here is deliberately not translated into a budget stop**, which is
    // the one place this stage differs from the two before it. Those run before there is an answer,
    // so an expiry there is a run that produced nothing and is resumable. Here the answer exists
    // and has not been cleared: reporting a completion would hand back exactly the output the check
    // was installed to look at, with nobody having looked. It stops as an error instead.
    if let Some(reason) = outcome
        .finish_reason()
        .filter(|reason| reason.is_complete())
    {
        // Re-merged only when control moved, because the checks belong to the agent whose answer is
        // being handed over. A refusal here is late — the model call has been paid for — but the
        // alternative is worse in both directions: running the starting agent's checks on another
        // agent's answer applies a promise nobody made about it, and skipping the receiving agent's
        // own checks delivers an answer it declared had to be examined.
        let output_guardrails = if Arc::ptr_eq(agent.public(), &starting_agent) {
            output_guardrails
        } else {
            merge_output_guardrails(
                agent.public().output_guardrails(),
                config.output_guardrails(),
            )
        };
        let verdicts = {
            // The delivery the guardrails examine is the one the result will report, resolved the
            // same way: a closeout outranks the model's own last word. A complete run has no
            // closeout today, and stating the rule here rather than relying on that keeps the two
            // answers from parting company if it ever does.
            let delivered = final_message.as_ref();
            // A run that stopped on a tool result has no assistant message carrying its answer, so
            // the concluding turn's outputs go with it. Without them a guardrail on such a run
            // examines an empty string and reports a pass.
            let tool_outputs = concluding_turn_tool_outputs(&state, &progress);
            let mut delivery = GuardrailFinalOutput::new(reason).with_tool_outputs(&tool_outputs);
            if let Some(message) = delivered {
                delivery = delivery.with_message(message);
            }
            let run = live_context(&context, &agent, &state);
            run_output_guardrails(&output_guardrails, &run, &delivery, &cancel).await
        };
        match verdicts {
            // Recorded before the stop is propagated, and recorded whether or not one fired: a
            // refusal whose evidence went out with it is a refusal nobody can audit.
            Ok(outcome) => {
                let (results, stop) = outcome.into_parts();
                state.record_output_guardrail_results(results);
                if let Some(error) = stop {
                    let error = error.with_guardrail_evidence(GuardrailEvidence::Output(
                        state.output_guardrail_results().to_vec(),
                    ));
                    return Err(terminal_failure(
                        span,
                        sandbox,
                        &input_base,
                        &progress,
                        &state,
                        error,
                        &cancel,
                    ));
                }
            }
            // A check that failed, or was cancelled, refused nothing: the answer stays in the
            // session as the reference keeps it. Failing to store it outranks the check's error,
            // as it does there, because it is the one that loses history.
            Err(error) => {
                state.snapshot_event_seq(&event_seqs);
                let cleanup = CancelScope::root()
                    .with_deadline(Deadline::after(ra_core::cancel::DRAIN_GRACE));
                let _cleanup_deadline = arm_deadline(&cleanup);
                let error = match context.session {
                    Some(session) => {
                        match session_persistence::save_session_items(
                            session,
                            &mut state,
                            &cleanup,
                            &|usage| record_session_compaction_spend(&context, &progress, usage),
                        )
                        .await
                        {
                            Ok(()) => error,
                            Err(write_error) => {
                                state.mark_terminal_unrecoverable();
                                write_error
                            }
                        }
                    }
                    None => error,
                };
                return Err(terminal_failure(
                    span,
                    sandbox,
                    &input_base,
                    &progress,
                    &state,
                    error,
                    &cancel,
                ));
            }
        }
    }

    // What the run settled since the last turn that continued: the final answer once nothing
    // refused it, a closeout, the turn a budget ended, or the turn that stopped for approval — the
    // last one unless the answers to it may become an output guardrail's to refuse, in which case
    // the resumed run appends it with them once it is cleared.
    state.snapshot_event_seq(&event_seqs);
    if let Some(session) = context.session
        && !defers_interrupted_session_items(&outcome, agent.public(), &config)
    {
        // Ordinary writes share the run's cancellation and configured deadline. Only a soft
        // wall-clock stop needs a fresh, bounded scope to flush the records it already produced.
        let cleanup = (progress.budget_stop == Some(BudgetKind::WallClock)).then(|| {
            closeout_cancel
                .child(ScopeKind::Run)
                .with_deadline(Deadline::after(ra_core::cancel::DRAIN_GRACE))
        });
        let _cleanup_deadline = cleanup.as_ref().map(arm_deadline);
        let write_scope = cleanup.as_ref().unwrap_or(&cancel);
        if let Err(error) =
            session_persistence::save_session_items(session, &mut state, write_scope, &|usage| {
                record_session_compaction_spend(&context, &progress, usage);
            })
            .await
        {
            if outcome
                .finish_reason()
                .is_some_and(FinishReason::is_complete)
            {
                state.mark_terminal_unrecoverable();
            }
            return Err(terminal_failure(
                span,
                sandbox,
                &input_base,
                &progress,
                &state,
                error,
                write_scope,
            ));
        }
    }

    if let Some(reason) = outcome.finish_reason() {
        state = state.with_finish_reason(reason);
    }
    state.snapshot_event_seq(&event_seqs);

    // Cut out of the run's history rather than accumulated alongside it: a result reports the
    // segment it ran, and the checkpoint it carries reports every segment.
    let (new_items, model_responses) = segment_records(&progress, &state);
    let result = RunResult::new(
        outcome.clone(),
        Arc::clone(agent.public()),
        input_base,
        new_items,
        model_responses,
        progress.turn_record_owner,
        progress.turn_records,
        progress.turns,
        state,
        final_message,
        progress.nested_usage,
        progress
            .session_compaction_usage
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    if let Some(sink) = &config.memory_usage_sink {
        crate::memory::report_final_citations(&result, sink, &run_id).await;
    }
    record_run_outcome(span, &result);
    emit(
        RunEvents {
            stream: events.as_ref(),
            rollout: None,
        },
        RunStreamEvent::Finished(outcome),
    );
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
    tool_input: Option<&Arc<serde_json::Value>>,
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
    if let Some(tool_input) = tool_input {
        context = context.with_tool_input(Arc::clone(tool_input));
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
///
/// A paused agent-tool call continues from its checkpoint once every question its nested run — and
/// any run nested in that — asked has an answer, or as soon as one of them was rejected. Otherwise
/// it stays paused with the answers it has, and nothing it asked about runs: the reference judges a
/// nested run with an unanswered approval and no rejection as pending and leaves it alone. A
/// rejection outranks that, so the refusal is settled into the nested run's history while the
/// questions still open stay open. Only the unanswered questions, of this run and of the paused
/// runs, are asked again before another model call.
///
/// What the stage ends on is decided the way a turn's is: a paused call is asked about first, and
/// otherwise the results the calls produced go to the stop policy, as the reference finalizes
/// from the tool results of the interrupted turn it resolves.
// Keep both settlement loops together so their failure exits restore the remaining children
// before returning the parent's checkpoint.
#[allow(clippy::too_many_lines)]
async fn resolve_interrupted_turn(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    lifecycle: &LifecycleHooks,
) -> Result<ResumeStage> {
    let answers = state.pending_interruption_resolutions().to_vec();
    let continued = state.take_nested_runs();
    let mut paused = Vec::new();
    let mut tool_results = Vec::new();
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
            InterruptionResolution::Reject { .. } => rejection(&approval),
            InterruptionResolution::Approve { .. } => {
                let resumed =
                    match run_approved_call(context, agent, state, lifecycle, &approval).await {
                        Ok(resumed) => resumed,
                        Err(error) => {
                            if let Some(nested) = error.nested_run() {
                                paused.push(nested.clone());
                                state.settle_interruption_resolution(answer.item_id())?;
                                paused.extend(continued);
                                state.set_nested_runs(paused)?;
                                state.snapshot_event_seq(context.event_seqs);
                            }
                            return Err(error);
                        }
                    };
                match resumed {
                    ResumedOutcome::Settled {
                        output,
                        outcome,
                        tool_result,
                    } => {
                        tool_results.extend(tool_result);
                        (output, Some(outcome))
                    }
                    // The host's answer is spent — the call ran — and what the call owes now is the
                    // nested run's to settle, so the approval is cleared without an output.
                    ResumedOutcome::Paused(nested) => {
                        paused.push(*nested);
                        state.settle_interruption_resolution(answer.item_id())?;
                        continue;
                    }
                }
            }
            _ => return Err(Error::caller("unsupported interruption resolution")),
        };
        outcomes.extend(outcome);
        let mut output_item = RunItem::new(
            ItemId::new(format!("{}.output", approval.call_id())),
            RunItemKind::ToolCallOutput(output.with_kind(approval.kind())),
        );
        if let Some(provenance) = provenance {
            output_item = output_item.with_provenance(provenance);
        }
        emit(context.events, RunStreamEvent::Item(output_item.clone()));
        state.record_generated_items([output_item]);
        state.settle_interruption_resolution(answer.item_id())?;
    }
    let mut continued = continued.into_iter();
    while let Some(nested) = continued.next() {
        if nested
            .state()
            .is_some_and(|state| nested_answer_status(state) == NestedAnswers::Pending)
        {
            paused.push(nested);
            continue;
        }
        let saved = nested.clone();
        let resumed = match continue_paused_run(context, agent, state, lifecycle, nested).await {
            Ok(resumed) => resumed,
            Err(error) => {
                paused.push(error.nested_run().cloned().unwrap_or(saved));
                paused.extend(continued);
                state.set_nested_runs(paused)?;
                state.snapshot_event_seq(context.event_seqs);
                return Err(error);
            }
        };
        let (output, outcome) = match resumed {
            ResumedOutcome::Settled {
                output,
                outcome,
                tool_result,
            } => {
                tool_results.extend(tool_result);
                (output, outcome)
            }
            ResumedOutcome::Paused(nested) => {
                paused.push(*nested);
                continue;
            }
        };
        outcomes.push(outcome);
        // Attributed as the batch attributes the output of a call it settles: to the public agent
        // that made the call, which is the agent this checkpoint resumes.
        let output_item = RunItem::new(
            ItemId::new(format!("{}.output", output.call_id())),
            RunItemKind::ToolCallOutput(output),
        )
        .with_provenance(
            ItemProvenance::new(agent.public_id().clone()).with_agent_name(agent.public().name()),
        );
        emit(context.events, RunStreamEvent::Item(output_item.clone()));
        state.record_generated_items([output_item]);
    }
    if !outcomes.is_empty() {
        let (_, failure) = state.trackers_mut();
        failure.record_turn(agent.public_id(), outcomes);
    }
    end_resume_stage(context, agent, state, paused, &tool_results).await
}

/// Where the host's answers leave a paused agent-tool run, judged over its whole nested tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NestedAnswers {
    /// Something was rejected. The run continues so the refusal is settled into its history, and
    /// whatever is still unanswered is asked again from there.
    Rejected,
    /// Nothing was rejected and something is unanswered: the run stays paused with what it has.
    Pending,
    /// Every question has an answer.
    Approved,
}

/// The reference's `_nested_interruptions_status`: a rejection anywhere outranks an unanswered
/// question, which outranks approval.
fn nested_answer_status(state: &RunState) -> NestedAnswers {
    if has_rejection(state) {
        NestedAnswers::Rejected
    } else if state.has_unanswered_interruptions() {
        NestedAnswers::Pending
    } else {
        NestedAnswers::Approved
    }
}

fn has_rejection(state: &RunState) -> bool {
    state
        .pending_interruption_resolutions()
        .iter()
        .any(|answer| matches!(answer.resolution(), InterruptionResolution::Reject { .. }))
        || state
            .nested_runs()
            .iter()
            .filter_map(NestedRunRef::state)
            .any(has_rejection)
}

/// Decides what the resume stage ends on, in the order a turn's settlement does.
async fn end_resume_stage(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    paused: Vec<NestedRunRef>,
    tool_results: &[ToolUseResult],
) -> Result<ResumeStage> {
    state.set_nested_runs(paused)?;
    let items: Vec<_> = state.unanswered_interruption_items().cloned().collect();
    if !items.is_empty() {
        return Ok(ResumeStage::Interrupted(items));
    }
    let concluded = check_for_final_output_from_tools(
        tool_results,
        agent.public().tool_use_behavior(),
        context.cancel,
    )
    .await?;
    Ok(if concluded {
        ResumeStage::Concluded(FinishReason::ToolStop)
    } else {
        ResumeStage::Continue
    })
}

/// What settling an interrupted turn on resume decided.
enum ResumeStage {
    /// Nothing concluded: the run asks the model for its next response.
    Continue,
    /// A call still waits on a nested run's questions, which the run asks again.
    Interrupted(Vec<RunItem>),
    /// The stop policy promoted a result settled here to the run's answer.
    Concluded(FinishReason),
}

/// The answer a rejected approval puts in history, and how the call is filed.
fn rejection(approval: &ToolApproval) -> (ToolCallOutput, Option<ToolOutcome>) {
    let output = ToolCallOutput::new(
        approval.call_id().clone(),
        serde_json::json!({"code": "approval_rejected", "tool": approval.tool_name()}),
    )
    .with_error(true)
    .with_kind(approval.kind());
    // Filed as a refusal, the same as a call the permission stage declines: both are "the runtime
    // answered without running the tool", which is what `ToolOutcome::refused` names. Recording
    // nothing instead would leave an earlier failure streak standing behind a call that never ran,
    // so the next genuine attempt would be judged on evidence this one did not produce. An approval
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

/// Runs a call the host approved before the checkpoint was taken.
async fn run_approved_call(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    lifecycle: &LifecycleHooks,
    approval: &ToolApproval,
) -> Result<ResumedOutcome> {
    let key = approval.lookup_key().ok_or_else(|| {
        Error::caller(format!(
            "approval for call `{}` cannot resume because it has no serialized tool lookup key",
            approval.call_id()
        ))
    })?;
    execute_resumed_call(
        context,
        agent,
        state,
        lifecycle,
        ResumedCall {
            key,
            call_id: approval.call_id(),
            arguments: approval.arguments(),
            nested: None,
        },
    )
    .await
}

/// Dispatches an agent-tool call again to continue the nested run it paused on.
async fn continue_paused_run(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    lifecycle: &LifecycleHooks,
    nested: NestedRunRef,
) -> Result<ResumedOutcome> {
    let key = nested.lookup_key().cloned().ok_or_else(|| {
        Error::caller(format!(
            "paused agent-tool run for call `{}` has no tool routing identity",
            nested.call_id()
        ))
    })?;
    let call_id = nested.call_id().clone();
    let arguments = nested.arguments().clone();
    let checkpoint = nested.into_state().ok_or_else(|| {
        Error::caller(format!(
            "paused agent-tool run for call `{call_id}` carries no checkpoint to continue"
        ))
    })?;
    execute_resumed_call(
        context,
        agent,
        state,
        lifecycle,
        ResumedCall {
            key: &key,
            call_id: &call_id,
            arguments: &arguments,
            nested: Some(checkpoint),
        },
    )
    .await
}

/// One call a resumed run dispatches again before its next model call.
struct ResumedCall<'a> {
    key: &'a ToolLookupKey,
    call_id: &'a CallId,
    arguments: &'a serde_json::Value,
    /// The paused nested run the call continues; `None` for a call the host approved.
    nested: Option<RunState>,
}

/// How a call dispatched on resume ended.
///
/// Short-lived — each is matched as soon as it is returned — so the settled variant stays inline
/// rather than boxed for the size of a value that never sits in a collection.
#[allow(clippy::large_enum_variant)]
enum ResumedOutcome {
    /// It has an output, and the record of how it went.
    Settled {
        output: ToolCallOutput,
        outcome: ToolOutcome,
        /// What the stop policy reads, present when the call produced a usable result — the same
        /// rule the batch applies, so a resumed call can end the run exactly when a call settled
        /// in its turn could have.
        tool_result: Option<ToolUseResult>,
    },
    /// Its nested run stopped on a question again.
    Paused(Box<NestedRunRef>),
}

/// Runs one call the host approved before the checkpoint was taken, or continues one whose nested
/// run paused.
///
/// The identity handed to the breaker is rebuilt from the recorded lookup key rather than asked
/// of a bound action, which is safe only because the tool below is *selected* by that same key:
/// the two cannot drift the way [`ProcessedResponse`](ra_core::step::ProcessedResponse) warns
/// about, because one is the search term for the other.
///
/// What the checks around the call decided is filed on `state` here. The post-approval check the
/// contract requires is inside the dispatch, and filing it lets a run resumed in a second process
/// leave the same evidence a run that never paused would have.
async fn execute_resumed_call(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    lifecycle: &LifecycleHooks,
    call: ResumedCall<'_>,
) -> Result<ResumedOutcome> {
    let ResumedCall {
        key,
        call_id,
        arguments,
        nested,
    } = call;
    // The execution instance's tools, because that is what runs: a capability contributes tools
    // to it alone, and a call the host approved on one of those must still find it on resume.
    let tool = agent
        .execution()
        .tools()
        .iter()
        .find(|tool| tool.origin().lookup_key() == key)
        .ok_or_else(|| {
            Error::caller(format!(
                "resumed call `{call_id}` names lookup key `{key:?}`, which the resumed agent no \
                 longer provides"
            ))
        })?;

    let mut run = RunContext::new(context.run_id.clone(), agent.public())
        .with_budget(state.budget().clone())
        .with_usage_totals(context.spend.shared_usage())
        .with_pending_control_requests(state.pending_control_requests().to_vec())
        .with_event_seq_allocator(context.event_seqs.clone());
    if let Some(app_context) = context.app_context {
        run = run.with_app_context(Arc::clone(app_context));
    }
    if let Some(tool_input) = context.tool_input {
        run = run.with_tool_input(Arc::clone(tool_input));
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
    let request = ToolDispatchRequest::new(
        Arc::clone(tool),
        call_id.clone(),
        arguments.clone(),
        Arc::new(run),
        context.cancel.child(ScopeKind::Tool),
        history,
        context.permission.clone(),
    )
    .with_services(context.services.clone())
    .with_tool_guardrails(context.tool_guardrails.clone())
    .with_user_hooks(context.config.user_hooks().clone())
    .with_lifecycle_hooks(lifecycle.clone());
    let request = match nested {
        Some(checkpoint) => request.with_nested_resume(checkpoint),
        None => request.with_approval_granted(),
    };
    let (dispatch, guardrails) = dispatch_tool(request).await?.into_parts();
    let (input_verdicts, output_verdicts) = guardrails.into_parts();
    state.record_tool_input_guardrail_results(input_verdicts);
    state.record_tool_output_guardrail_results(output_verdicts);

    let call_id = call_id.clone();
    match dispatch {
        ToolDispatch::Observed(observation) => {
            let failure_code = observation.failure_code();
            let output = observation.output().clone();
            let outcome = match failure_code {
                Some(code) => {
                    ToolOutcome::failed(identity, call_id, arguments, output.output(), code)
                }
                None => ToolOutcome::succeeded(identity, call_id, arguments, output.output()),
            };
            let tool_result = (!output.is_error())
                .then(|| ToolUseResult::new(tool.origin().clone(), output.clone()));
            Ok(ResumedOutcome::Settled {
                output,
                outcome,
                tool_result,
            })
        }
        ToolDispatch::Refused(refusal) => {
            let output = refusal.into_output();
            let outcome = ToolOutcome::refused(identity, call_id, arguments, output.output());
            Ok(ResumedOutcome::Settled {
                output,
                outcome,
                tool_result: None,
            })
        }
        ToolDispatch::AwaitingApproval(_) => Err(Error::caller(
            "an approved tool call requested approval again",
        )),
        ToolDispatch::AwaitingNestedApproval(nested) => Ok(ResumedOutcome::Paused(nested)),
    }
}

/// Whether a segment runs on the input its caller supplied in place of its checkpoint's history.
///
/// Read from the state before the segment begins. A continuation given input of its own runs on
/// that input, as [`segment_input_base`] selects it, so the input already holds the run's history.
fn input_is_continuation_base(state: &RunState, input: &[ModelInputItem]) -> bool {
    state.current_agent().is_some() && !input.is_empty()
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
/// Ends a run that failed after its turns started: records what the span reports for a failure,
/// and hands sandbox memory the segment as far as it got, so the failure is remembered with the
/// records that led to it.
fn terminal_failure(
    span: &tracing::Span,
    sandbox: &SandboxRuntime,
    input_base: &[ModelInputItem],
    progress: &TurnLoopProgress,
    state: &RunState,
    error: Error,
    scope: &CancelScope,
) -> Error {
    record_progress_usage(span, progress.segment_responses(state));
    record_terminal_error(span, &error, scope);
    sandbox.record_failed_segment(input_base, progress.segment_items(state));
    if state.has_session_write_checkpoint() {
        error.with_run_state(state.clone())
    } else {
        error
    }
}

fn segment_records(
    progress: &TurnLoopProgress,
    state: &RunState,
) -> (Vec<RunItem>, Vec<ModelResponse>) {
    (
        progress.segment_items(state).to_vec(),
        progress.segment_responses(state).to_vec(),
    )
}

/// Releases a run's sandbox sessions when the caller drops [`Runner::run`] before it finished.
///
/// A dropped future cannot await its own cleanup, and the reference's `finally` would have: so the
/// cleanup is handed to a task of its own, which starts it if the run never got that far and
/// otherwise waits for the one already under way. Disarmed once the run has settled itself.
struct ReleaseSandboxOnDrop(Option<Arc<SandboxRuntime>>);

impl Drop for ReleaseSandboxOnDrop {
    fn drop(&mut self) {
        let Some(sandbox) = self.0.take().filter(|sandbox| sandbox.enabled()) else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(async move { sandbox.finish_cleanup().await }));
        }
    }
}

/// The sandbox half of one run, built from the request before the loop consumes it.
///
/// A sandboxed run is also given the rollout its sandbox memory is recorded under: the run's
/// group, as the reference's `_sandbox_memory_rollout_id` resolves it. A run here has no
/// server-side conversation to take one from, so it is the run's session, the host's group id, or
/// a rollout of the run's own.
fn sandbox_runtime(request: &RunRequest) -> Arc<SandboxRuntime> {
    let rollout_id = request.config.sandbox().map(|_| {
        grouping::resolve_run_grouping_id(
            None,
            request.session.as_deref(),
            request.config.group_id(),
        )
    });
    Arc::new(SandboxRuntime::new(
        request.config.sandbox().cloned(),
        request.state.sandbox_resume_state().cloned(),
        request.config.capabilities(),
        Arc::clone(&request.model_resolver),
        rollout_id,
    ))
}

/// Cleans up the run's sandbox sessions and records what resumes them on the result.
///
/// A cleanup failure is logged rather than returned, as on the reference: the run's own outcome is
/// what the caller asked for, and a finished run is not turned into a failed one because releasing
/// its sandbox did not go cleanly. The result then carries no resume state, since one describing
/// sessions whose cleanup failed would resume a workspace nobody can vouch for. A run with no
/// sandbox configuration keeps whatever resume state it was continued with.
///
/// Before cleanup the run is recorded for sandbox memory — the finished run, or the segment a
/// failed one got through — and a failure to record it is logged in the same way.
async fn settle_sandbox(
    sandbox: &Arc<SandboxRuntime>,
    result: Result<RunResult>,
) -> Result<RunResult> {
    if !sandbox.enabled() {
        return result;
    }
    // Before cleanup, whose pre-stop callbacks are what extract and consolidate the segments.
    let enqueued = match &result {
        Ok(result) => sandbox.enqueue_memory_result(result).await,
        Err(error) => sandbox.enqueue_memory_failure(error).await,
    };
    if let Err(error) = enqueued {
        warn!(error = %error, "Failed to enqueue sandbox memory after run");
    }
    let cleanup = sandbox.cleanup().await;
    if let Err(error) = &cleanup {
        warn!(error = %error, "failed to clean up sandbox resources after run");
    }
    let mut result = result;
    if let Err(error) = &mut result
        && let Some(state) = error.run_state_mut()
        && let Err(error) = state.set_sandbox_resume_state(cleanup.as_ref().ok().cloned().flatten())
    {
        warn!(error = %error, "failed to record sandbox cleanup in a failed run checkpoint");
    }
    result.map(|mut result| {
        if let Err(error) = result.set_sandbox_resume_state(cleanup.unwrap_or(None)) {
            warn!(error = %error, "failed to record what resumes the run's sandbox sessions");
        }
        result
    })
}

/// Records the run-level aggregate without creating a second accounting source.
///
/// These are the same field names a `generation` uses one level down, and the scale is the
/// difference: this is the whole run, that is one request. Reports group by `span.kind` before
/// summing — see the note in `ra_core::trace::field`.
///
/// The run's own model calls only: an agent-tool run it started records its calls on its own span,
/// one level down, and counting them here as well would count them twice in any report that sums
/// agent spans.
fn record_run_outcome(span: &tracing::Span, result: &RunResult) {
    record_usage(span, &aggregate_usage(result.model_responses()));
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
///
/// `input_check`, when present, is raced against the **first** turn. See
/// [`race_input_guardrails`] for what that race decides and what it deliberately does not.
async fn run_turns(
    context: &TurnLoopContext<'_>,
    agent: &mut AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
    lifecycle: &mut LifecycleHooks,
    mut input_check: Option<InputGuardrailCheck>,
) -> Result<RunOutcome> {
    let config = context.config;
    let outcome = loop {
        // Checked before the budget so a cancelled run reports as cancelled rather than as having
        // exhausted its turns — the reason a host reacts to is different for each.
        context.cancel.ensure_not_cancelled()?;

        if let Some(kind) = exhausted_budget_kind(state, config.budget(), context.spend) {
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
        let turn = run_one_turn(
            context,
            agent,
            state,
            progress,
            lifecycle,
            &turn_scope,
            &turn_span,
        )
        .instrument(turn_span.clone());
        let (verdicts, step) = match input_check.take() {
            // Boxed because this arm holds the whole turn future *and* the guardrail stage, and
            // the loop's own future would otherwise carry both on every iteration — including the
            // ones after the first, where the stage is long since gone.
            Some(check) => {
                Box::pin(race_input_guardrails(
                    check,
                    turn,
                    &turn_scope,
                    context.cancel,
                ))
                .await
            }
            None => (Ok(StageOutcome::empty_stage()), turn.await),
        };
        turn_span.record(
            ra_core::trace::field::DURATION_MS,
            duration_ms(turn_started.elapsed()),
        );
        match &step {
            Ok(_) => ra_core::trace::record_outcome(&turn_span, ra_core::trace::SpanOutcome::Ok),
            Err(error) => record_terminal_error(&turn_span, error, &turn_scope),
        }

        // Ahead of the turn's own result, and that order is the decision: a refused input outranks
        // whatever the turn it was racing managed to produce, including the cancellation the
        // refusal itself caused.
        //
        // The verdicts are recorded before the refusal is raised, and recorded whether or not one
        // fired: the checks that had already passed are evidence the stage ran, and the tripping
        // one is the evidence the refusal is argued from.
        let (verdicts, refusal) = verdicts?.into_parts();
        state.record_input_guardrail_results(verdicts);
        if let Some(error) = refusal {
            // Re-attached from the run's own record rather than kept as the dispatcher left it.
            // The stage runs in two halves and each dispatcher call sees only its own verdicts, so
            // a refusal from the raced half would otherwise omit everything the blocking half had
            // already concluded — the checks that passed being exactly what says the stage ran.
            return Err(error.with_guardrail_evidence(GuardrailEvidence::Input(
                state.input_guardrail_results().to_vec(),
            )));
        }

        // A turn that reached a conclusion ends the loop with it; anything else means another
        // turn. The two states stay `Option` rather than becoming a second control-flow enum:
        // `NextStep` is the one that names what a turn decided, and it is answered inside.
        //
        // The one thing that can overrule a conclusion is a stop hook asking for more work, and
        // it is asked here rather than inside the turn because the question is about the run's
        // delivery: a turn does not know whether its own answer is the one being handed over.
        let step = step?;
        // A turn that continues is appended as soon as its input checks have cleared it, as the
        // reference saves each turn that runs again or hands off. A turn that ended the run is
        // appended after delivery has been decided, by the loop's caller.
        if step.is_none()
            && let Some(session) = context.session
        {
            state.snapshot_event_seq(context.event_seqs);
            session_persistence::save_session_items(session, state, context.cancel, &|usage| {
                record_session_compaction_spend(context, progress, usage);
            })
            .await?;
        }
        if let Some(outcome) = step
            && !continue_from_stop_hook(context, agent, state, progress, &outcome).await?
        {
            break outcome;
        }
        state.snapshot_event_seq(context.event_seqs);
    };
    Ok(outcome)
}

/// Whether a run that stopped for approval leaves that turn's records out of its session for now.
///
/// The reference's `_should_defer_interrupted_session_items`. When output guardrails are installed
/// and a tool result can end the run, the approved call's output may become the final answer —
/// and an answer an output guardrail refuses must not already be in the session. The resumed run
/// appends the whole turn once that is decided.
fn defers_interrupted_session_items(
    outcome: &RunOutcome,
    agent: &AgentSpec,
    config: &RunConfig,
) -> bool {
    matches!(outcome, RunOutcome::Interrupted { .. })
        && (!agent.output_guardrails().is_empty() || !config.output_guardrails().is_empty())
        && !matches!(
            agent.tool_use_behavior(),
            ra_core::agent::ToolUseBehavior::RunLlmAgain
        )
}

/// Checks the candidate delivery after input checks have passed and before output guardrails.
/// Accepted continuation is persisted as user-role history, including for tool-stop deliveries.
async fn continue_from_stop_hook(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
    outcome: &RunOutcome,
) -> Result<bool> {
    let Some(reason) = outcome
        .finish_reason()
        .filter(|reason| reason.is_complete())
    else {
        return Ok(false);
    };
    let event_name = if state.parent_run_id().is_some() {
        HookEventName::SubagentStop
    } else {
        HookEventName::Stop
    };
    if !context.config.user_hooks().has_event(event_name) {
        return Ok(false);
    }
    let decision = {
        let outputs = concluding_turn_tool_outputs(state, progress);
        let delivery = StopHookData::new(
            reason,
            concluding_turn_message(state, progress),
            &outputs,
            state.stop_hook_active(),
        );
        let event = match state.parent_run_id() {
            Some(parent) => HookEvent::SubagentStop {
                parent_run_id: parent,
                delivery,
            },
            None => HookEvent::Stop(delivery),
        };
        context
            .config
            .user_hooks()
            .bind(
                Arc::new(live_context(context, agent, state)),
                context.cancel.clone(),
                context.services.clone(),
            )
            .dispatch(event)
            .await?
    };
    let HookDecision::Block { prompt } = decision else {
        return Ok(false);
    };
    // Numbered by the whole run's turn, like the compaction record next door, and for the same
    // reason: at most one continuation follows any one turn, so the identity is unique without
    // consulting history — and a resumed segment does not restart the numbering and collide with
    // what the previous one wrote.
    let item = RunItem::new(
        ItemId::new(format!("hook-continuation-{}", progress.reference_turn())),
        RunItemKind::Message(Message::text(MessageRole::User, prompt)),
    );
    state.mark_stop_hook_active();
    state.record_generated_items([item.clone()]);
    emit(context.events, RunStreamEvent::Item(item));
    let end = progress.segment_items(state).len();
    if let Some(record) = progress.turn_records.last_mut() {
        record.continue_after_hook(end);
    }
    Ok(true)
}

/// Runs the half of the input stage that must finish before anything is sent.
///
/// A tripwire here refuses the run with no model call having happened, which is what a host gives
/// up the racing default to get. What comes back is the raced half, or `None` when the stage was
/// entirely blocking and has nothing left to race.
///
/// Errors are returned rather than handled, so the caller can put them through the same wall-clock
/// translation the turns go through: a deadline that expired during a blocking check is the same
/// budget stop it would be one line later, and reporting it as a bare cancellation here would make
/// the outcome depend on which half of the stage a check happened to be in.
async fn run_blocking_input_guardrails(
    input_check: Option<InputGuardrailCheck>,
    state: &mut RunState,
    cancel: &CancelScope,
) -> Result<Option<InputGuardrailCheck>> {
    let Some(mut check) = input_check else {
        return Ok(None);
    };
    // Recorded before the refusal is raised, and recorded whether or not one fired.
    let (verdicts, refusal) = check.run_blocking(cancel).await?.into_parts();
    state.record_input_guardrail_results(verdicts);
    if let Some(error) = refusal {
        return Err(error.with_guardrail_evidence(GuardrailEvidence::Input(
            state.input_guardrail_results().to_vec(),
        )));
    }
    // A checkpoint before this boundary must retry the blocking checks. Once they pass,
    // the raced half may be abandoned without re-entering it on a continuation.
    state.mark_input_guardrails_started();
    Ok((!check.is_empty()).then_some(check))
}

/// Runs the input guardrails alongside the first turn, and stops that turn when one trips.
///
/// # What the race buys, and what it costs
///
/// A guardrail is usually a model call, so running it before the first one would double the latency
/// of every run to protect against the fraction that are refused. Running it beside them costs
/// nothing on a run that passes.
///
/// What it costs on a run that is refused is stated rather than hidden: the turn is **cancelled**
/// the moment a tripwire fires, so a check that returns while the model call is still in flight
/// stops the run before any tool executes — the ordinary case, since the two calls are of
/// comparable length. A guardrail slower than the whole turn does not: it returns after that turn's
/// tools have already run, and the refusal then ends the run rather than preventing it. A check
/// that must be decided before anything executes belongs at the tool boundary, where approval and
/// the tool guardrails sit.
///
/// A successful turn still waits for its input verdict. A failed turn drops a pending guardrail
/// future because there is no answer left to approve. When the guardrail fails first, the turn is
/// cancelled and drained so its model call and tool execution unwind through the framework.
async fn race_input_guardrails(
    check: InputGuardrailCheck,
    turn: impl Future<Output = Result<Option<RunOutcome>>>,
    turn_scope: &CancelScope,
    run_scope: &CancelScope,
) -> (
    Result<StageOutcome<InputGuardrailResult>>,
    Result<Option<RunOutcome>>,
) {
    let guardrails = check.run(run_scope);
    tokio::pin!(guardrails, turn);

    let mut verdicts = None;
    let step = loop {
        tokio::select! {
            // Biased so the refusal is seen on the wake-up that produced it. Left to chance, a
            // turn that becomes ready in the same wake-up could settle first and run its tools
            // after the input had already been refused.
            biased;
            result = &mut guardrails, if verdicts.is_none() => {
                // Nothing this turn produces can be delivered now, and everything it is still
                // doing is being paid for. `PeerFailure` is the reason it is: another task in the
                // same batch failed and continuing is pointless.
                //
                // A cancellation is excluded, and not as a nicety. It means a scope above already
                // stopped this run and is propagating into the turn on its own; stamping
                // `PeerFailure` on the turn as well would relabel a user's interrupt or an expired
                // deadline as a guardrail's doing, in the one field attribution reads.
                //
                // A tripwire is one of the two ways this arm stops the turn, and it does not
                // arrive as an `Err`: the stage returns its verdicts either way and carries the
                // refusal beside them, so the stop has to be read out of the outcome rather than
                // off the `Result`.
                let stops_the_turn = match result.as_ref() {
                    Ok(outcome) => outcome.refuses(),
                    Err(error) => !error.is_cancelled(),
                };
                if stops_the_turn {
                    turn_scope.cancel(CancelReason::PeerFailure);
                }
                verdicts = Some(result);
            }
            step = &mut turn => break step,
        }
    };

    let verdicts = match verdicts {
        Some(verdicts) => verdicts,
        // Preserve the turn's error and drop the pending check. Guardrail futures own cleanup of
        // any spawned work, as specified by the InputGuardrail cancellation contract.
        None if step.is_err() => Ok(StageOutcome::empty_stage()),
        // The turn won the race. The verdict is still owed — a guardrail that has not answered has
        // not approved anything — so it is awaited rather than dropped.
        None => guardrails.await,
    };
    (verdicts, step)
}

/// Runs one turn: prepare, call the model, settle, and say whether the run continues.
///
/// `None` means another turn; `Some` carries the outcome the run ends with.
///
/// This is the lifecycle coordinator for one turn. Keeping preparation, context processing,
/// dispatch, accounting, and settlement together makes their ordering auditable.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_one_turn(
    context: &TurnLoopContext<'_>,
    agent: &mut AgentBinding,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
    lifecycle: &mut LifecycleHooks,
    turn_scope: &CancelScope,
    turn_span: &tracing::Span,
) -> Result<Option<RunOutcome>> {
    let config = context.config;
    // Every turn, as the reference prepares: a transfer of control may have brought a sandbox agent
    // in, and a session that stopped since the last turn is started again.
    *agent = turn_scope
        .run(context.sandbox.prepare_agent(
            agent,
            context.model_resolver.as_ref(),
            config.model.as_deref(),
        ))
        .await
        .and_then(|prepared| prepared)?;
    // Ahead of everything that reads history, so a fragment earned by the previous turn is in the
    // request that also carries the tool result which earned it.
    deliver_deferred_prompts(context, agent, state, progress.reference_turn());
    deliver_agent_mail(context, state, progress, turn_scope);
    deliver_rollout_budget_reminder(context, state, progress);
    let reminder = budget_reminder(context.spend, config.budget());
    // What this turn's request is built from: the base the run continues on, and the records
    // appended since. The two ways of decomposing that are equivalent until a transfer of control
    // narrows the base — after which only the state knows what the receiving agent may see.
    let turn_input = TurnInput::resolve(context, state, progress)?;
    let input = next_input(turn_input.base(), turn_input.carried(), reminder);
    // What an agent spawned during this turn starts from: the agent running it, and the history
    // its request is built from. Only a run that takes part in an agent tree pays for the copy.
    if context.services.agent_control().is_some()
        && let Some(parent) = ParentRun::current()
    {
        let mut history = turn_input.base().to_vec();
        history.extend(
            turn_input
                .carried()
                .iter()
                .filter_map(RunItem::to_model_input),
        );
        parent.enter_turn(agent.public(), history);
    }
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
    .with_action_surface_budget(config.action_surface_budget())
    .with_agent_registry(config.agent_registry())
    .with_services(context.services);
    if let Some(model) = &config.model {
        preparation = preparation.with_model(model.clone());
    }
    let prepared = prepare_turn(preparation).await?;
    let sandbox_processors = context.sandbox.context_processors(agent).await;
    let (prepared, context_records, context_responses) = process_context_processors(
        context,
        &sandbox_processors,
        progress,
        turn_scope,
        prepared,
        &turn_input,
        preparation_context,
    )
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
        record_spend(context, state, &context_usage, None, progress);
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
        guardrails: context.tool_guardrails.clone(),
        user_hooks: context.config.user_hooks().clone(),
        lifecycle: lifecycle.clone(),
    };
    // Only a configured sink can ever read these, and deriving them is not free: it deserializes
    // every tool output in the request and compares it against the authoritative record, once per
    // turn, and what it produces is persisted into `RunState` and therefore into every checkpoint.
    // A run that installed no sink would be paying that on every turn to accumulate evidence
    // nothing will read.
    let memory_exposures = if config.memory_usage_sink.is_some() {
        crate::memory::request_exposures(prepared.request(), state)
    } else {
        Vec::new()
    };
    // The model identity outlives the request it came from: `into_call` consumes the preparation,
    // and the closing half of this bracket has to name the same model the opening half did.
    let selector = prepared.selector().clone();
    if let Some(rollout) = context.events.rollout
        && !progress.turn_context_recorded
    {
        progress.turn_context_recorded = true;
        rollout.record(RolloutItem::TurnContext(turn_context(
            context.run_id,
            progress,
            &selector,
            prepared.request().model_settings().effort(),
        )));
    }
    // Announced once for the whole logical call rather than once per physical request. Retries and
    // a provider fallback happen inside it, and are already on the generation spans underneath; a
    // callback per attempt would make a host counting model calls count retries, and would leave
    // the pair unpaired on exactly the calls that had trouble.
    if !lifecycle.is_empty() {
        let run = live_context(context, agent, state);
        let calling = LlmStartInput::new(&run, &selector, prepared.request().input())
            .with_system_instructions(prepared.request().system_instructions())
            .with_services(context.services);
        lifecycle_dispatch::llm_start(lifecycle, &calling, turn_scope).await?;
    }
    let mut compaction_evidence = state.session_compaction().cloned().unwrap_or_default();
    compaction_evidence.reset_exchange();
    let compaction = context
        .session
        .and_then(ra_core::session::Session::compaction);
    let mut compaction_response_stored = None;
    if let Some(compaction) = compaction {
        state.set_session_compaction(compaction_evidence.clone());
        compaction_response_stored = compaction.response_stored(prepared.request());
        // Evidence is best-effort, as in the reference: an item that cannot be fingerprinted is
        // omitted, so stored history covering it cannot match and automatic compaction is
        // skipped. The optional compaction path must never fail the model turn itself.
        match turn_scope
            .run(compaction.model_request_digests(prepared.request()))
            .await?
        {
            Ok(digests) => {
                for digest in digests {
                    compaction_evidence.push_model_item_digest(digest);
                }
            }
            Err(error) => tracing::warn!(
                error.code = error.code(),
                "Session compaction evidence omitted a model request that could not be fingerprinted"
            ),
        }
    }
    let mut early = EarlyRecords::new(context.events.rollout, agent.public());
    let (surface, response, streamed_dispatches) = call_model(
        turn_scope,
        prepared,
        context,
        streaming_dispatch,
        &mut early,
    )
    .await?;
    state.record_memory_exposures(memory_exposures);

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
    record_spend(context, state, response.usage(), selector.model(), progress);
    state.record_model_response(response.clone());
    if let Some(compaction) = compaction {
        for item in response.output().iter().filter_map(RunItem::to_model_input) {
            match turn_scope.run(compaction.model_item_digest(&item)).await? {
                Ok(digest) => compaction_evidence.push_model_item_digest(digest),
                Err(error) => tracing::warn!(
                    error.code = error.code(),
                    "Session compaction evidence omitted a response item that could not be fingerprinted"
                ),
            }
        }
        compaction_evidence.set_response(
            response.response_id().map(str::to_owned),
            compaction_response_stored,
        );
        state.set_session_compaction(compaction_evidence);
    }

    // After the spend has been recorded, so a callback reads the run's totals with the call it is
    // being told about already in them. A call that produced no response never reaches here: it did
    // not end, it failed, and the run's own error is what says so.
    if !lifecycle.is_empty() {
        let run = live_context(context, agent, state);
        let answered = LlmEndInput::new(&run, &selector, &response).with_services(context.services);
        lifecycle_dispatch::llm_end(lifecycle, &answered, turn_scope).await?;
    }

    let referenced_outputs = referenced_tool_outputs(config, &response)?;

    // Settlement sees exactly the decomposition the request was built from, because it is what a
    // transfer of control projects: the base this run continues on, and the records added to it
    // since. Handing it a differently sliced but equivalent view would work until the day a handoff
    // narrowed one of them, at which point the projection and the request would describe two
    // different conversations.
    let (segment_original_input, pre_step_items) = turn_input.into_parts();

    // Built again rather than reused from preparation: the call above has been paid for, and the
    // spend a tool reads has to include it. The two contexts are the same run and the same agent —
    // what differs is only how much of the budget each stage can truthfully report.
    //
    // A tool the stream already started holds the earlier one instead, for the reason given on
    // [`StreamedDispatchInput`]: its response had not been paid for when it was handed over.
    let settlement_context = Arc::new(live_context(context, agent, state));
    let mut recovery = crate::turn::batch::TurnRecovery::default();
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
    .with_tool_guardrails(context.tool_guardrails.clone())
    .with_user_hooks(context.config.user_hooks().clone())
    .with_lifecycle_hooks(lifecycle.clone())
    .with_max_function_tool_concurrency(config.max_function_tool_concurrency)
    .with_streamed_dispatches(streamed_dispatches)
    .with_recovery(&mut recovery);
    let settlement = match config.handoff_input_filter() {
        Some(filter) => settlement.with_handoff_input_filter(Arc::clone(filter)),
        None => settlement,
    };
    // Settlement runs the calls that did not start during the stream; their records go first.
    early.record_before_settlement(&response);
    let settled = match settle_turn(settlement).await {
        Ok(settled) => settled,
        Err(error) => {
            if !recovery.nested_runs.is_empty() {
                // Retain paid-for calls, completed siblings and every failed child's routing.
                // This mirrors the reference's tool-output committer on a failed batch.
                state.record_generated_items(response.output().iter().cloned());
                state.record_generated_items(recovery.execution.new_items().iter().cloned());
                state.set_pending_interruptions(recovery.execution.interruptions())?;
                recovery.nested_runs.extend(
                    recovery
                        .execution
                        .function_results()
                        .iter()
                        .filter_map(|result| result.nested_run().cloned()),
                );
                state.set_nested_runs(recovery.nested_runs)?;
                state.record_tool_input_guardrail_results(
                    recovery.execution.tool_input_guardrail_results().to_vec(),
                );
                state.record_tool_output_guardrail_results(
                    recovery.execution.tool_output_guardrail_results().to_vec(),
                );
                state
                    .trackers_mut()
                    .1
                    .record_turn(agent.public_id(), recovery.execution.outcomes().to_vec());
                state.snapshot_event_seq(context.event_seqs);
            }
            return Err(error);
        }
    };

    // Recorded straight after settlement, alongside the items: these are decisions this turn's
    // calls produced, and a checkpoint taken from here on has to carry the evidence for what the
    // model was shown.
    state.record_tool_input_guardrail_results(settled.tool_input_guardrail_results().to_vec());
    state.record_tool_output_guardrail_results(settled.tool_output_guardrail_results().to_vec());

    record_tool_output_references(
        config,
        state,
        progress.reference_turn(),
        settled.session_step_items(),
        referenced_outputs,
    )?;

    for item in settled.session_step_items() {
        let events = if early.recorded(item) {
            context.events.stream_only()
        } else {
            context.events
        };
        emit(events, RunStreamEvent::Item(item.clone()));
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
    let decided: Result<Option<RunOutcome>> = match settled.next_step() {
        NextStep::RunAgain => Ok(None),
        NextStep::FinalOutput { reason } => Ok(Some(RunOutcome::Completed { reason: *reason })),
        // The items are the session's own records: settlement re-points a pending decision at what
        // it stores before handing the decision over, so this outcome and the stream carry one copy
        // of each question rather than two that disagree about who produced it.
        NextStep::Interruption { items } => {
            // The checkpoint keeps the IDs of this run's own questions, which resolve against the
            // records recorded just above. Settlement guarantees they are among them, so a failure
            // here is this loop breaking its own contract rather than anything the host did.
            //
            // A nested run's questions are not this run's records. The checkpoint keeps the paused
            // nested runs instead, and each question is answered — and read back — through the run
            // that raised it.
            let nested: Vec<&RunItem> = settled.nested_interruptions().collect();
            let own: Vec<RunItem> = items
                .iter()
                .filter(|item| !nested.contains(item))
                .cloned()
                .collect();
            state.set_pending_interruptions(&own)?;
            state.set_nested_runs(
                settled
                    .function_results()
                    .iter()
                    .filter_map(|result| result.nested_run().cloned())
                    .collect(),
            )?;
            Ok(Some(RunOutcome::Interrupted {
                items: items.clone(),
            }))
        }
        // Control transfers to another agent, which speaks next. The new agent arrives as a
        // public declaration, so it binds directly: whatever prepared *this* turn's execution
        // instance has no say over who runs the next one.
        NextStep::Handoff { new_agent } => {
            // What the receiving agent continues from, which is the settled turn's carried-forward
            // view rather than the session's. The session keeps every record either way; this is
            // the half a declared projection and a host filter were allowed to narrow, and building
            // it from the stored records instead would quietly undo both.
            //
            // The boundary is the last record this turn stored. Everything the run appends after it
            // — the next turns, a deferred fragment, an error handler's message — carries on behind
            // the projection instead of being folded into it.
            let projected = HandoffInputData::new(
                settled.original_input().to_vec(),
                settled.pre_step_items().to_vec(),
                settled.new_step_items().to_vec(),
            )
            .into_model_input();
            let boundary = state
                .generated_items()
                .last()
                .map(RunItem::id)
                .cloned()
                .ok_or_else(|| {
                    Error::caller(
                        "a transfer of control settled without the run having stored any record; \
                         there is nothing for the receiving agent's history to resume after",
                    )
                })?;
            state.install_handoff_projection(HandoffProjection::new(projected, boundary))?;

            // Handoff belongs to the receiving agent, while the input still names the source.
            let receiving = lifecycle.rebound(new_agent.lifecycle_hooks());
            if !receiving.is_empty() {
                let run = live_context(context, agent, state);
                let transfer = HandoffInput::new(&run, new_agent).with_services(context.services);
                lifecycle_dispatch::handoff(&receiving, &transfer, turn_scope).await?;
            }
            *agent = AgentBinding::direct(Arc::clone(new_agent));
            state.set_current_agent(agent.public_id().clone());
            // Narration follows control. The departing agent's hooks stop here — one that kept
            // reporting would attribute the next agent's model calls and tool runs to it — and the
            // arriving agent's take over, together with the run-scoped half that spans both.
            *lifecycle = receiving;
            if !lifecycle.is_empty() {
                // The input the arriving agent is starting on, not the one the run opened with:
                // an observer told otherwise would be shown a transcript this agent never receives.
                let (projected, _) = state.model_input_base()?;
                let projected = projected.to_vec();
                let run = live_context(context, agent, state);
                let starting =
                    AgentStartInput::new(&run, &projected).with_services(context.services);
                lifecycle_dispatch::agent_start(lifecycle, &starting, turn_scope).await?;
            }
            Ok(None)
        }
    };
    let decided = decided?;
    // Codex fails the request whose usage leaves the tree's shared budget spent — and, once it is
    // spent, every later one — after the response is recorded and the tool calls it issued have
    // run. So a turn settled on a spent budget ends the run here whatever it decided: an answer is
    // kept in history but not delivered, and no further model call is made. A turn that stopped
    // for approval still pauses; the segment that settles the answers ends the run instead.
    if !matches!(decided, Some(RunOutcome::Interrupted { .. }))
        && context.spend.rollout_budget_exhausted()
    {
        progress.budget_stop = Some(BudgetKind::Tokens);
        return Ok(Some(RunOutcome::Completed {
            reason: FinishReason::BudgetExhausted,
        }));
    }
    Ok(decided)
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

/// Records the mail waiting for this run's agent as input to the model call about to be made.
///
/// Mail that arrives while the model is answering waits for the next call; a run whose answer
/// concludes it leaves that mail queued for the agent's next run, as Codex defers mailbox delivery
/// once a turn has produced its final answer. A turn already cancelled takes nothing, so an
/// interrupt never swallows a follow-up into a run that is stopping.
fn deliver_agent_mail(
    context: &TurnLoopContext<'_>,
    state: &mut RunState,
    progress: &TurnLoopProgress,
    turn_scope: &CancelScope,
) {
    let Some(handle) = context.agent_mail else {
        return;
    };
    if (progress.turns <= 1 && !context.deliver_mail_first) || turn_scope.is_cancelled() {
        return;
    }
    let mail = handle.take_mail();
    if mail.is_empty() {
        return;
    }
    // Numbered by the whole run's turn, like the other records the loop writes itself, so a
    // resumed segment does not collide with what an earlier one recorded.
    let turn = progress.reference_turn();
    let items: Vec<RunItem> = mail
        .iter()
        .enumerate()
        .map(|(index, mail)| {
            RunItem::new(
                ItemId::new(format!("agent-message-{turn}-{index}")),
                RunItemKind::Message(mail.to_message()),
            )
        })
        .collect();
    for item in &items {
        emit(context.events, RunStreamEvent::Item(item.clone()));
    }
    state.record_generated_items(items);
}

/// Writes the agent tree's budget reminder into history, when the agent is owed one.
///
/// Once per turn of the agent's, at its first model call, as Codex records it when a turn starts:
/// a segment that only finishes a turn stopped for approval is not given one. Acknowledged after
/// it is recorded, so a run that stops before then is reminded next time.
fn deliver_rollout_budget_reminder(
    context: &TurnLoopContext<'_>,
    state: &mut RunState,
    progress: &TurnLoopProgress,
) {
    let Some(handle) = context.agent_mail else {
        return;
    };
    if progress.turns > 1 || context.continues_turn {
        return;
    }
    let Some(reminder) = handle.pending_budget_reminder() else {
        return;
    };
    let item = RunItem::new(
        ItemId::new(format!("rollout-budget-{}", progress.reference_turn())),
        RunItemKind::Message(Message::user(reminder.text())),
    );
    emit(context.events, RunStreamEvent::Item(item.clone()));
    state.record_generated_items([item]);
    handle.mark_budget_reminder_delivered(reminder);
}

/// Settles adapter compaction usage into the live shared budget and rollout channel.
fn record_session_compaction_spend(
    context: &TurnLoopContext<'_>,
    progress: &TurnLoopProgress,
    usage: &Usage,
) {
    if usage.requests() == 0 {
        return;
    }
    let mut accumulated = progress
        .session_compaction_usage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *accumulated = accumulated.accumulate(usage);
    drop(accumulated);
    context.spend.record_own(usage);
    if let Some(rollout) = context.events.rollout {
        let mut record = RolloutModelUsage::new(context.run_id.clone(), usage.clone());
        if let Ok(turn) = u32::try_from(progress.reference_turn()) {
            record = record.with_turn_index(turn);
        }
        rollout.record(RolloutItem::ModelUsage(record));
    }
}

/// Records a model call this run paid for, in its ledger and in the spend the runs it started and
/// its agent tree read.
fn record_spend(
    context: &TurnLoopContext<'_>,
    state: &mut RunState,
    usage: &Usage,
    model: Option<&str>,
    progress: &TurnLoopProgress,
) {
    state.record_usage(usage);
    context.spend.record_own(usage);
    if let Some(rollout) = context.events.rollout {
        let mut record = RolloutModelUsage::new(context.run_id.clone(), usage.clone());
        if let Ok(turn) = u32::try_from(progress.reference_turn()) {
            record = record.with_turn_index(turn);
        }
        if let Some(model) = model {
            record = record.with_model(model);
        }
        rollout.record(RolloutItem::ModelUsage(record));
    }
}

/// The context a recorded run's first model call in a segment runs in: Codex's turn context, with
/// the model and effort the call resolved to.
fn turn_context(
    run_id: &RunId,
    progress: &TurnLoopProgress,
    selector: &ra_core::model::ModelSelector,
    effort: Option<ra_core::model::Effort>,
) -> RolloutTurnContext {
    let turn = u32::try_from(progress.reference_turn()).unwrap_or(u32::MAX);
    let mut context = RolloutTurnContext::new(run_id.clone(), turn);
    if let Some(model) = selector.model() {
        context = context.with_model(model);
    }
    if let Some(effort) = effort {
        context = context.with_effort(effort.to_string());
    }
    context
}

/// Moves what this run's agent-tool calls have spent into its ledger.
///
/// The reference's nested run adds to the parent's usage directly; here it reaches this run's
/// [`RunSpend`] as it goes — which is what the budget checks and the run's live context read — and
/// the loop moves it into the ledger once, when the loop ends, whichever way it ended.
fn absorb_nested_spend(
    spend: &RunSpend,
    state: &mut RunState,
    progress: &mut TurnLoopProgress,
) -> Option<Usage> {
    let nested = spend.take_nested();
    if nested.requests() == 0 && nested.total_tokens() == 0 {
        return None;
    }
    state.record_usage(&nested);
    progress.nested_usage = progress.nested_usage.accumulate(&nested);
    Some(nested)
}

/// Runs every installed context processor and rebuilds the ordinary request from its projection.
///
/// Processors are deliberately generic here. The loop knows how to request a summary and append
/// returned records, but it does not know whether a processor is compaction, redaction, or a
/// product-specific retention policy.
async fn process_context_processors(
    context: &TurnLoopContext<'_>,
    sandbox_processors: &[Arc<dyn ContextProcessor>],
    progress: &TurnLoopProgress,
    turn_scope: &CancelScope,
    prepared: PreparedTurn,
    turn_input: &TurnInput,
    run: RunContext,
) -> Result<(PreparedTurn, Vec<RunItem>, Vec<ModelResponse>)> {
    // Sandbox capabilities transform the current input even on caller-managed continuations,
    // as the reference's prepare_sandbox_input does. Only run-level processors require an
    // authoritative history decomposition.
    let decomposition = turn_input
        .history_span
        .and_then(|span| span.split(prepared.request().input(), turn_input.base()));
    let processors: Vec<&Arc<dyn ContextProcessor>> = sandbox_processors
        .iter()
        .chain(
            context
                .config
                .context_processors()
                .iter()
                .filter(|_| decomposition.is_some()),
        )
        .collect();
    if processors.is_empty() {
        return Ok((prepared, Vec::new(), Vec::new()));
    }
    let mut input = prepared.request().input().to_vec();
    // Without a reconstructible history, expose the caller's input as a prefix, not as records
    // from the checkpoint that may no longer be part of the caller's projection.
    let (prefix, suffix, mut history) = match decomposition {
        Some((prefix, suffix)) => (prefix, suffix, turn_input.carried().to_vec()),
        None => (input.clone(), Vec::new(), Vec::new()),
    };
    let mut generated_items: Vec<RunItem> = Vec::new();
    let mut taken_ids: BTreeSet<ItemId> = history.iter().map(|item| item.id().clone()).collect();
    let mut model_responses = Vec::new();
    let summarizer = RunnerContextSummarizer::new(&prepared, turn_scope);
    let hooks = (context
        .config
        .user_hooks()
        .has_event(HookEventName::PreCompact)
        || context
            .config
            .user_hooks()
            .has_event(HookEventName::PostCompact))
    .then(|| {
        Arc::new(context.config.user_hooks().bind(
            Arc::new(run),
            turn_scope.clone(),
            context.services.clone(),
        ))
    });

    for (index, processor) in processors.iter().enumerate() {
        let record_id = ItemId::new(format!("context-{}.{index}", progress.reference_turn()));
        let request = ContextProcessorRequest::new(
            context.run_id.clone(),
            progress.reference_turn(),
            record_id,
            prepared.selector().model().map(str::to_owned),
            prefix.clone(),
            history.clone(),
            suffix.clone(),
            input,
        );
        let request = match &hooks {
            Some(hooks) => request.with_user_hook_dispatcher(hooks.clone()),
            None => request,
        };
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

/// What one turn's model input is built from: a base, and the records appended to it since.
///
/// Three sources can answer that, and which one does is a fact about how the run got here rather
/// than a preference. A transfer of control outranks both others: it installed a base precisely
/// because the receiving agent may not see everything the session holds, and rebuilding from the
/// history would hand over exactly what the projection withheld. Otherwise a run whose history is
/// wholly reconstructible uses the state's own decomposition, which is what lets context processing
/// own the whole run rather than one segment; and a continuation carrying the caller's projection
/// uses that, because the state never recorded it.
#[derive(Debug, Clone)]
struct TurnInput {
    base: Vec<ModelInputItem>,
    carried: Vec<RunItem>,
    /// `None` when this segment's input is not reconstructible from state, which is what makes a
    /// positional split of the assembled request unsafe — see [`HistorySpan`].
    history_span: Option<HistorySpan>,
}

impl TurnInput {
    fn resolve(
        context: &TurnLoopContext<'_>,
        state: &RunState,
        progress: &TurnLoopProgress,
    ) -> Result<Self> {
        let (base, carried) = if state.handoff_projection().is_some() {
            state.model_input_base()?
        } else if context.authoritative_history_complete {
            (state.original_input(), state.generated_items())
        } else {
            (context.input_base, progress.segment_items(state))
        };
        let history_span = context.authoritative_history_complete.then(|| HistorySpan {
            history_len: carried.iter().filter(|item| item.is_model_input()).count(),
        });
        Ok(Self {
            base: base.to_vec(),
            carried: carried.to_vec(),
            history_span,
        })
    }

    fn base(&self) -> &[ModelInputItem] {
        &self.base
    }

    fn carried(&self) -> &[RunItem] {
        &self.carried
    }

    fn into_parts(self) -> (Vec<ModelInputItem>, Vec<RunItem>) {
        (self.base, self.carried)
    }
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
/// Both halves of the projection are written back, and anything derived from the prefix is rebuilt
/// from the text that will actually be sent. A filter may replace the system instructions — the
/// chain measures that rather than refusing it — so a cache plan carried by the prepared request
/// would otherwise still hold the hash of a prefix this turn is no longer sending, and the
/// request's own validation would reject it at the provider boundary.
///
/// The cache **scope** is deliberately carried across unchanged. It identifies the conversation a
/// cache entry belongs to rather than the bytes in it, so a host that keeps one stable key per
/// session keeps it here too; only the prefix hash follows the text.
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
    let before_instructions = prepared.request().system_instructions().map(str::to_owned);
    let data = ModelInputData::new(
        prepared.request().input().to_vec(),
        before_instructions.clone(),
    );
    let (data, reports) = filters.apply(&request, data)?.into_parts();
    let (input, instructions) = data.into_parts();
    let instructions_changed = instructions != before_instructions;

    Ok((
        prepared.map_request(|request| {
            let request = request.with_input(input);
            if !instructions_changed {
                return request;
            }
            let scope = request
                .cache_plan()
                .and_then(|plan| plan.cache_scope())
                .map(str::to_owned);
            match instructions {
                Some(instructions) => {
                    let plan = CachePlan::for_prefix(&instructions, scope.as_deref());
                    request
                        .with_system_instructions(instructions)
                        .with_cache_plan(plan)
                }
                // A projection that removed the prefix leaves nothing for a plan to key on, so the
                // plan goes with it rather than being kept against text that is no longer there.
                None => request.without_system_instructions(),
            }
        }),
        reports,
    ))
}

/// Only a turn that still proposes final output can supply the delivered assistant message.
/// Earlier blocked candidates remain untouched in history.
fn concluding_turn_message<'a>(
    state: &'a RunState,
    progress: &TurnLoopProgress,
) -> Option<&'a Message> {
    let record = progress.turn_records.last()?;
    record.finish_reason()?;
    find_final_message(
        progress
            .segment_items(state)
            .get(record.item_range().clone())?,
    )
}

/// The tool outputs the **concluding** turn settled, for a delivery whose answer is one of them.
///
/// Scoped to that one turn, and the scope is the point. A run that looked something up on turn one
/// and finished on turn two settled results on both, but only the second turn's are candidates for
/// the answer — handing over the segment's would let an earlier turn's result decide the verdict
/// on a final output it is not part of. It would also make the answer depend on whether the run
/// had been resumed, since a resumed segment starts its history part-way through.
///
/// Which of the turn's results a policy promoted is still not re-derived: `stop_on_first_tool`, a
/// name list, and a host's handler each pick a different subset, and a second reading of that
/// decision would sooner or later disagree with the first.
fn concluding_turn_tool_outputs(
    state: &RunState,
    progress: &TurnLoopProgress,
) -> Vec<ToolCallOutput> {
    // A resume that concluded before its first turn delivers what it settled.
    let range = match (progress.turn_records.last(), &progress.resumed_conclusion) {
        (Some(record), _) => record.item_range().clone(),
        (None, Some(range)) => range.clone(),
        (None, None) => return Vec::new(),
    };
    let items = progress.segment_items(state);
    items
        .get(range)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output.clone()),
            _ => None,
        })
        .collect()
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
/// - **The spend counters as copies, not handles.** The context is a read view; the state stays the
///   one thing that accumulates and the one thing a checkpoint carries. The usage is the one the
///   run shares with its agent-tool chain (see `RunSpend::shared_usage`), as the reference hands a
///   nested run its parent's own usage object; a run no agent-tool call started reads its own
///   ledger, with what its agent-tool calls have spent so far.
fn live_context(
    context: &TurnLoopContext<'_>,
    agent: &AgentBinding,
    state: &RunState,
) -> RunContext {
    let mut run = RunContext::new(context.run_id.clone(), agent.public())
        .with_budget(state.budget().clone())
        .with_usage_totals(context.spend.shared_usage())
        .with_pending_control_requests(state.pending_control_requests().to_vec())
        .with_event_seq_allocator(context.event_seqs.clone());
    if let Some(app_context) = context.app_context {
        run = run.with_app_context(Arc::clone(app_context));
    }
    if let Some(tool_input) = context.tool_input {
        run = run.with_tool_input(Arc::clone(tool_input));
    }
    run
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
    guardrails: ToolGuardrails,
    user_hooks: UserHooks,
    lifecycle: LifecycleHooks,
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
            self.guardrails.clone(),
            self.user_hooks.clone(),
            self.lifecycle.clone(),
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
    early: &mut EarlyRecords<'_>,
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
        .then_some(context.events.stream)
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
            early,
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
    early: &mut EarlyRecords<'_>,
) -> Result<(ModelResponse, StreamedFunctionDispatches)> {
    // A retried attempt starts over: what the failed one streamed was never acted on.
    early.discard_pending();
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
        early,
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
#[allow(clippy::too_many_arguments)]
async fn stream_model_call(
    model: &Arc<dyn Model>,
    request: ModelRequest,
    events: Option<&mpsc::UnboundedSender<RunStreamEvent>>,
    surface: &TurnActionSurface,
    cancel: &CancelScope,
    mut dispatches: StreamedFunctionDispatches,
    consumed: &mut CallConsumption,
    early: &mut EarlyRecords<'_>,
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
        early,
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
#[allow(clippy::too_many_arguments)]
async fn read_model_stream(
    model: &Arc<dyn Model>,
    request: ModelRequest,
    events: Option<&mpsc::UnboundedSender<RunStreamEvent>>,
    surface: &TurnActionSurface,
    cancel: &CancelScope,
    dispatches: &mut StreamedFunctionDispatches,
    consumed: &mut CallConsumption,
    early: &mut EarlyRecords<'_>,
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
                // Narration only: it is not a session record, so it never reaches the rollout.
                if let Some(sender) = events {
                    let _ = sender.send(RunStreamEvent::RawResponse(raw));
                }
            }
            ModelStreamEvent::Completed(response) => settled = Some(*response),
            ModelStreamEvent::RunItem(item) => {
                early.completed(item.item());
                start_streamed_call(item.item(), surface, dispatches, consumed, early)?;
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
    early: &mut EarlyRecords<'_>,
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
    // Recorded before the tool exists, so nothing it reports can precede the call it answers.
    if !dispatches.defers(action)? {
        early.record_pending();
    }
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
    // A closeout speaks for the current agent and must respect the history its handoff allowed.
    // Ordinary runs retain the segment decomposition exposed by the error-handler contract.
    let (base, carried) = if state.handoff_projection().is_some() {
        state.model_input_base()?
    } else {
        (context.input_base, progress.segment_items(state))
    };
    let data = RunErrorData::new(
        agent.public(),
        base,
        carried,
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
/// Where what a run produces goes as it happens: the stream subscriber, if there is one, and the
/// rollout recorder, if the run was given one.
#[derive(Clone, Copy)]
struct RunEvents<'a> {
    stream: Option<&'a mpsc::UnboundedSender<RunStreamEvent>>,
    rollout: Option<&'a dyn RolloutRecorder>,
}

impl RunEvents<'_> {
    /// The same subscriber, without the rollout: for a record the rollout already holds.
    const fn stream_only(self) -> Self {
        Self {
            rollout: None,
            ..self
        }
    }
}

/// The records of a turn a recorded run writes before the turn settles.
///
/// Codex persists each completed item of a response before it runs the call it carries, so a turn
/// interrupted while its tools run still has the call every reported effect answers. Here a turn's
/// records are written when it settles, and settlement is exactly what an interruption skips. So
/// when a call is about to start — from the stream, or in settlement — every completed item of the
/// response up to it is recorded first, in response order and attributed as settlement attributes
/// it. Settlement then records only what is new: an item already recorded as it settles is not
/// recorded again, and one settlement changed (an assistant message's output phase is decided
/// there) is recorded again under its id, superseding the earlier copy.
///
/// Nothing is recorded early from an attempt that may still be retried: items wait here until a
/// tool starts from one — which is what closes a call to replay — or until the response is final.
struct EarlyRecords<'a> {
    rollout: Option<&'a dyn RolloutRecorder>,
    public: &'a AgentSpec,
    /// Completed stream items of the current attempt not recorded yet, in order.
    pending: Vec<RunItem>,
    /// What this turn recorded early, by id.
    recorded: std::collections::HashMap<ItemId, RunItem>,
}

impl<'a> EarlyRecords<'a> {
    fn new(rollout: Option<&'a dyn RolloutRecorder>, public: &'a AgentSpec) -> Self {
        Self {
            rollout,
            public,
            pending: Vec::new(),
            recorded: std::collections::HashMap::new(),
        }
    }

    /// Notes a completed stream item, to be recorded once a call starts.
    fn completed(&mut self, item: &RunItem) {
        if self.rollout.is_some() {
            self.pending.push(item.clone());
        }
    }

    /// Forgets the items of an attempt that is being replaced.
    fn discard_pending(&mut self) {
        self.pending.clear();
    }

    /// Records the completed stream items not recorded yet, ahead of a call that starts now.
    fn record_pending(&mut self) {
        for item in std::mem::take(&mut self.pending) {
            self.record(item);
        }
    }

    /// Records the final response's items not recorded yet, when settlement is about to run calls.
    fn record_before_settlement(&mut self, response: &ModelResponse) {
        self.pending.clear();
        if self.rollout.is_none()
            || !response
                .output()
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::ToolCall(_)))
        {
            return;
        }
        for item in response.output() {
            if !self.recorded.contains_key(item.id()) {
                self.record(item.clone());
            }
        }
    }

    fn record(&mut self, item: RunItem) {
        let Some(rollout) = self.rollout else {
            return;
        };
        let item = crate::turn::resolve::attribute(item, self.public);
        rollout.record(RolloutItem::Item(item.clone()));
        self.recorded.insert(item.id().clone(), item);
    }

    /// Whether `item`, as settled, is already in the rollout exactly as it is.
    fn recorded(&self, item: &RunItem) -> bool {
        self.recorded.get(item.id()) == Some(item)
    }
}

/// Sends `event` to the subscriber and records each session record it carries in the rollout, so
/// the rollout holds exactly the records the stream reports, in the same order.
fn emit(events: RunEvents<'_>, event: RunStreamEvent) {
    if let (Some(rollout), RunStreamEvent::Item(item)) = (events.rollout, &event) {
        rollout.record(RolloutItem::Item(item.clone()));
    }
    if let Some(sender) = events.stream {
        let _ = sender.send(event);
    }
}
