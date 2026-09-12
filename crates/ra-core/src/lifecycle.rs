//! The seven lifecycle moments a run narrates, and the two scopes that can be told about them.
//!
//! This is the third extension family in the framework, and the one that decides nothing. The
//! neighbours are worth naming, because what separates them is not packaging:
//!
//! - [`guardrail`](crate::guardrail) is the host's pair of checks around a run, and around each
//!   tool call. A tripwire there stops the run.
//! - [`hook`](crate::hook) is the host's twelve-event extension point. At four of its events it can
//!   refuse a tool call, settle an approval, or hold a delivery back.
//! - This module is **narration**. Every callback returns `Result<()>`, and there is no answer a
//!   host can give here that changes what the run does.
//!
//! That boundary is deliberate rather than a stage this family has not reached yet. Every decision
//! a lifecycle callback could plausibly want already has an owner — refusing a run's input or its
//! answer is a [guardrail](crate::guardrail::InputGuardrail), refusing a tool call is
//! [`HookEventName::PreToolUse`](crate::hook::HookEventName::PreToolUse), and holding a delivery
//! back is [`HookEventName::Stop`](crate::hook::HookEventName::Stop). A second family that could
//! decide would be a second place to look when a run was refused, and the two would first disagree
//! on a resume.
//!
//! # Two scopes, one contract
//!
//! The reference implementation declares two classes, `RunHooksBase` and `AgentHooksBase`. Their
//! moments are the same seven; the start/end naming differs through the two method names `on_agent_start` /
//! `on_start` and `on_agent_end` / `on_end`, which describe the same moment from the two sides.
//!
//! So there is **one trait here and two installation sites**. A hook attached to an
//! [`AgentSpec`](crate::agent::AgentSpec) is told what happens while that agent is the one running,
//! and stops being told anything the moment control transfers away; a hook installed on the run is
//! told about every agent in it. Declaring the same seven moments twice would be seven chances for
//! the two lists to drift, and the first one to drift would be the one a host wrote its code
//! against.
//!
//! Because the two scopes are told about the same moments, a report has to be able to say which one
//! it heard from — that is [`LifecycleScope`], handed to the callback rather than folded into its
//! name. Handoff is dispatched to the receiving agent, as in the reference contract.
//! One object may legitimately be installed in both places, and a hook that could not tell
//! them apart would count its own firings twice.
//!
//! # A lifecycle moment names a thing, and brackets exactly that thing
//!
//! [`LifecycleEvent::ToolStart`] and [`LifecycleEvent::ToolEnd`] are the innermost pair: they fire
//! after the permission chain, the approval, the input guardrails and the deciding hooks have all
//! let the call through, and immediately around the invocation itself. A "tool start" announced
//! before the permission chain would announce a tool that the chain then refuses to run, and a host
//! counting invocations would be counting decisions.
//!
//! [`LifecycleEvent::AgentStart`] and [`LifecycleEvent::AgentEnd`] bracket an agent rather than a
//! run, which is the reference implementation's rule and not an approximation of it: a start is
//! raised every time the running agent changes, and an end only when an agent produced the answer
//! being delivered. A run that hands back with approvals outstanding, or that fails, raises no end
//! at all — it did not finish, and a host counting completions must not be told otherwise.
//!
//! # Failure is the host's own
//!
//! A callback returns [`Result`] and its error propagates, carrying which hook, which moment and
//! which scope the framework added to it. This is the reference implementation's behaviour — it
//! awaits these callbacks without catching — and it is the **opposite** of what
//! [`UserHook`](crate::hook::UserHook) does, which reports a failure and continues. The two differ
//! because the answers differ: a deciding hook that failed has no verdict, and continuing without
//! one is a defined outcome; an observer that failed has left the run's narration silently down,
//! and a callback that only wants to record something can swallow its own errors in one line.
//!
//! # Nothing here is persisted
//!
//! A lifecycle callback observes; it has no verdict a resumed segment would need to know about. The
//! checkpoint schema is untouched, and a run resumed in another process raises
//! [`LifecycleEvent::AgentStart`] again there — see [`AgentStartInput::is_resumed`], which is what
//! says that this is a continuation rather than a run beginning twice.

use core::fmt;

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    agent::AgentSpec,
    context::{RunAgent, RunContext},
    error::Result,
    finish::FinishReason,
    item::{CallId, Message, ModelInputItem, ModelResponse, ToolCallOutput},
    model::ModelSelector,
    tool::{ToolOrigin, ToolOutput, ToolServices},
};

// ---------------------------------------------------------------------------
// the vocabulary
// ---------------------------------------------------------------------------

/// Which of the seven moments raised a callback.
///
/// The code word is what a trace field and a report file a firing under, so it is fixed here rather
/// than derived from a method name.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LifecycleEvent {
    /// An agent became the one running, either at the start of a segment or after a transfer.
    AgentStart,
    /// An agent produced the answer the run delivers.
    AgentEnd,
    /// A model call is about to be made.
    LlmStart,
    /// A model call produced a response.
    LlmEnd,
    /// A tool is about to be invoked, everything having admitted it.
    ToolStart,
    /// A tool invocation settled.
    ToolEnd,
    /// Control transferred to another agent.
    Handoff,
}

impl LifecycleEvent {
    /// Every moment in the vocabulary.
    pub const ALL: &'static [Self] = &[
        Self::AgentStart,
        Self::AgentEnd,
        Self::LlmStart,
        Self::LlmEnd,
        Self::ToolStart,
        Self::ToolEnd,
        Self::Handoff,
    ];

    /// Stable machine-readable slug, for traces and reports.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::LlmStart => "llm_start",
            Self::LlmEnd => "llm_end",
            Self::ToolStart => "tool_start",
            Self::ToolEnd => "tool_end",
            Self::Handoff => "handoff",
        }
    }
}

impl fmt::Display for LifecycleEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// Which registration path a firing came from.
///
/// Handed to the callback rather than folded into its identity. The same host object may reasonably
/// be installed on the run and on one agent — a metrics collector, say, that wants both the
/// whole-run totals and the per-agent split — and a callback that could not say which of the two it
/// was speaking as would report both streams as one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LifecycleScope {
    /// Installed on the run, and told about every agent in it.
    Run,
    /// Installed on one agent, and told only while that agent is the one running.
    Agent,
}

impl LifecycleScope {
    /// Stable machine-readable slug, for traces and reports.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for LifecycleScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

// ---------------------------------------------------------------------------
// what a callback is handed
// ---------------------------------------------------------------------------

/// An agent became the one running.
///
/// Raised once per activation, which is every time the running agent changes: at the start of a
/// segment, and again on the far side of a transfer. A segment is not the same thing as a run — a
/// run continued in another process gets a fresh set of hook objects, and a host setting something
/// up here would have nothing set up on the continuation if this only fired the first time.
/// [`Self::is_resumed`] is what tells the two apart.
///
/// [`Self::input`] and [`Self::is_resumed`] describe the **segment** this activation belongs to, so
/// they read the same for every agent activated inside it. The input is what the caller handed the
/// segment, which on a continuation that replays the checkpoint's own history is that projection.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct AgentStartInput<'a> {
    run: &'a RunContext,
    services: &'a ToolServices,
    input: &'a [ModelInputItem],
    resumed: bool,
}

impl<'a> AgentStartInput<'a> {
    /// Describes an agent activated in a segment starting on `input`.
    pub fn new(run: &'a RunContext, input: &'a [ModelInputItem]) -> Self {
        Self {
            run,
            services: ToolServices::none(),
            input,
            resumed: false,
        }
    }

    /// Marks this segment as a continuation of a run that started elsewhere.
    pub const fn resumed(mut self) -> Self {
        self.resumed = true;
        self
    }

    /// Installs the framework ports this run holds.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run, already attributed to the agent that has just been activated.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// The agent that is about to act.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.run.agent()
    }

    /// Framework ports available, including the task-state handle.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }

    /// What this segment started from, before any filter or capability has touched it.
    #[must_use]
    pub const fn input(&self) -> &'a [ModelInputItem] {
        self.input
    }

    /// Whether this segment continues a run that already had turns behind it.
    #[must_use]
    pub const fn is_resumed(&self) -> bool {
        self.resumed
    }
}

/// An agent produced the answer the run delivers.
///
/// Raised once, on the agent that settled the run, and before the output guardrails look at what it
/// produced — the reference implementation's order, and the useful one: a host that records the
/// answer wants the answer the agent chose, not the one a check may go on to refuse.
///
/// A run that handed back with approvals outstanding raises nothing here, and neither does a run
/// that **failed**: it did not end, it broke, and the seam that turns a terminal failure into
/// something a host can show is the runtime's error handler. Otherwise "how many runs finished"
/// would depend on which family was asked.
///
/// A **soft budget stop** raises nothing either, for the same reason it is not shown to the output
/// guardrails: an exhausted token allowance or turn cap leaves a run that can be continued, and the
/// closeout text it hands back is the host's own rather than an answer the agent chose. The
/// predicate is [`FinishReason::is_complete`], so the two stages cannot drift apart on which
/// endings count.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct AgentEndInput<'a> {
    run: &'a RunContext,
    services: &'a ToolServices,
    finish_reason: FinishReason,
    message: Option<&'a Message>,
    tool_outputs: &'a [ToolCallOutput],
}

impl<'a> AgentEndInput<'a> {
    /// Describes a run whose agent reached `finish_reason`.
    pub fn new(run: &'a RunContext, finish_reason: FinishReason) -> Self {
        Self {
            run,
            services: ToolServices::none(),
            finish_reason,
            message: None,
            tool_outputs: &[],
        }
    }

    /// Attaches the assistant message the run delivered.
    pub const fn with_message(mut self, message: &'a Message) -> Self {
        self.message = Some(message);
        self
    }

    /// Attaches the concluding turn's tool outputs, including a tool-produced final answer.
    pub const fn with_tool_outputs(mut self, outputs: &'a [ToolCallOutput]) -> Self {
        self.tool_outputs = outputs;
        self
    }

    /// Tool outputs from the concluding turn, after dispatch transformations.
    #[must_use]
    pub const fn tool_outputs(&self) -> &'a [ToolCallOutput] {
        self.tool_outputs
    }

    /// Installs the framework ports this run holds.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run that is ending.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// The agent that produced the answer.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.run.agent()
    }

    /// Framework ports available, including the task-state handle.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }

    /// Why the loop stopped.
    #[must_use]
    pub const fn finish_reason(&self) -> FinishReason {
        self.finish_reason
    }

    /// The assistant message delivered, when the run produced one.
    ///
    /// Optional for the reason a final-output guardrail's delivery is: a run that promoted a tool
    /// result to its answer produced no assistant message to promote it into, and
    /// [`Self::finish_reason`] is what tells that apart from a model that said nothing.
    #[must_use]
    pub const fn message(&self) -> Option<&'a Message> {
        self.message
    }
}

/// The model is about to be called.
///
/// # It is not handed the request
///
/// A [`ModelRequest`](crate::model::ModelRequest) reaches
/// [`ModelSettings::extra_headers`](crate::model::ModelSettings::extra_headers), which is where an
/// API key sits — the same leak [`RunAgent`] exists to close on the agent side. So what a callback
/// gets is the model-visible content: which model was resolved, the system instructions, and the
/// input items. Everything a host could want to log about a call is here; nothing it could
/// accidentally log a credential through is.
///
/// # One logical call, not one physical request
///
/// A turn's model call may retry and may fall back to another provider, and this fires once for the
/// whole of it. Per-attempt facts are already on the generation spans, which are per request; making
/// this per attempt as well would mean a host counting model calls counted retries, and the pair
/// with [`LifecycleEvent::LlmEnd`] would stop pairing on exactly the calls that had trouble.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct LlmStartInput<'a> {
    run: &'a RunContext,
    services: &'a ToolServices,
    model: &'a ModelSelector,
    system_instructions: Option<&'a str>,
    input: &'a [ModelInputItem],
}

impl<'a> LlmStartInput<'a> {
    /// Describes a call about to be made to `model` on `input`.
    pub fn new(run: &'a RunContext, model: &'a ModelSelector, input: &'a [ModelInputItem]) -> Self {
        Self {
            run,
            services: ToolServices::none(),
            model,
            system_instructions: None,
            input,
        }
    }

    /// Attaches the system instructions the request carries.
    pub const fn with_system_instructions(mut self, instructions: Option<&'a str>) -> Self {
        self.system_instructions = instructions;
        self
    }

    /// Installs the framework ports this run holds.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run making the call.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// The agent the call is made on behalf of.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.run.agent()
    }

    /// Framework ports available, including the task-state handle.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }

    /// The model finally resolved for this call, not the alias the host wrote.
    #[must_use]
    pub const fn model(&self) -> &'a ModelSelector {
        self.model
    }

    /// The system instructions the request carries, when it carries any.
    #[must_use]
    pub const fn system_instructions(&self) -> Option<&'a str> {
        self.system_instructions
    }

    /// The input items the model is about to read.
    #[must_use]
    pub const fn input(&self) -> &'a [ModelInputItem] {
        self.input
    }
}

/// The model call produced a response.
///
/// Only a call that produced one raises this. A call that failed every attempt did not end, it
/// broke, and the run's own error is what says so — the same rule this family applies to a run that
/// failed rather than stopped.
///
/// The response's usage covers the whole logical call, including any attempt that failed before it,
/// and the run's totals as of this moment already include it: a callback here reads the spend with
/// the call it is describing in it.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct LlmEndInput<'a> {
    run: &'a RunContext,
    services: &'a ToolServices,
    model: &'a ModelSelector,
    response: &'a ModelResponse,
}

impl<'a> LlmEndInput<'a> {
    /// Describes the response `model` produced.
    pub fn new(run: &'a RunContext, model: &'a ModelSelector, response: &'a ModelResponse) -> Self {
        Self {
            run,
            services: ToolServices::none(),
            model,
            response,
        }
    }

    /// Installs the framework ports this run holds.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run that made the call.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// The agent the call was made on behalf of.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.run.agent()
    }

    /// Framework ports available, including the task-state handle.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }

    /// The model that answered.
    #[must_use]
    pub const fn model(&self) -> &'a ModelSelector {
        self.model
    }

    /// What the call produced, with the usage of every attempt it took.
    #[must_use]
    pub const fn response(&self) -> &'a ModelResponse {
        self.response
    }
}

/// A tool is about to be invoked.
///
/// Raised for calls that are actually going to run: caller admission, the loop breakers, the
/// deciding hooks, the permission chain, the approval and the input guardrails have all let this
/// one through, and the invocation is the next thing that happens. A call any of those refused
/// raises nothing here, because no tool was about to start.
///
/// It is raised after the concurrency and resource gates too, so the pair with
/// [`LifecycleEvent::ToolEnd`] measures execution rather than execution plus queueing —
/// `tool.admission_wait_ms` is where the waiting already is.
///
/// The tool is named by its [`ToolOrigin`] rather than handed over as an object, which is where
/// this differs from the reference implementation's `on_tool_start(context, agent, tool)`. A
/// `&dyn Tool` reaching an observer is a callable tool, and calling it from here would go around
/// caller admission, the breakers, approval, the guardrails and every record the dispatch chain
/// writes.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ToolStartInput<'a> {
    run: &'a RunContext,
    services: &'a ToolServices,
    origin: &'a ToolOrigin,
    call_id: &'a CallId,
    arguments: &'a Value,
}

impl<'a> ToolStartInput<'a> {
    /// Describes a call about to be invoked.
    pub fn new(
        run: &'a RunContext,
        origin: &'a ToolOrigin,
        call_id: &'a CallId,
        arguments: &'a Value,
    ) -> Self {
        Self {
            run,
            services: ToolServices::none(),
            origin,
            call_id,
            arguments,
        }
    }

    /// Installs the framework ports this call may reach.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run this call is part of.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// The agent whose surface the call came from.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.run.agent()
    }

    /// Framework ports available to this call, including the task-state handle.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }

    /// Stable identity of the tool being invoked.
    #[must_use]
    pub const fn origin(&self) -> &'a ToolOrigin {
        self.origin
    }

    /// Provider call ID paired with the eventual result.
    #[must_use]
    pub const fn call_id(&self) -> &'a CallId {
        self.call_id
    }

    /// The arguments the tool is about to run with.
    #[must_use]
    pub const fn arguments(&self) -> &'a Value {
        self.arguments
    }
}

/// A tool invocation settled.
///
/// Raised immediately after invocation, before failure handling, output guardrails and projection.
/// The result is the tool's own output or error, not the model-facing observation: the reference
/// implementation hands over the local tool's own result, and a fallible Rust invocation says the
/// same thing with an explicit [`Result`]. Cancellation can tear down the callback along with the
/// containing turn.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ToolEndInput<'a> {
    call: ToolStartInput<'a>,
    outcome: &'a Result<ToolOutput>,
}

impl<'a> ToolEndInput<'a> {
    /// Describes the raw result of a completed invocation.
    pub const fn new(call: ToolStartInput<'a>, outcome: &'a Result<ToolOutput>) -> Self {
        Self { call, outcome }
    }

    /// The call this result answers, in the view the start callback was given.
    pub const fn call(&self) -> ToolStartInput<'a> {
        self.call
    }

    /// The run this call was part of.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.call.run()
    }

    /// The agent whose surface the call came from.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.call.agent()
    }

    /// Framework ports the call reached.
    pub const fn services(&self) -> &'a ToolServices {
        self.call.services()
    }

    /// Stable identity of the tool that ran.
    #[must_use]
    pub const fn origin(&self) -> &'a ToolOrigin {
        self.call.origin()
    }

    /// Provider call ID this result is paired with.
    #[must_use]
    pub const fn call_id(&self) -> &'a CallId {
        self.call.call_id()
    }

    /// The arguments the call ran with.
    #[must_use]
    pub const fn arguments(&self) -> &'a Value {
        self.call.arguments()
    }

    /// The tool's raw output or error, before model-facing transformations.
    pub fn output(&self) -> core::result::Result<&'a ToolOutput, &'a crate::error::Error> {
        self.outcome.as_ref()
    }

    /// The invocation's failure code, or `None` when the tool succeeded.
    #[must_use]
    pub fn failure_code(&self) -> Option<&'static str> {
        self.outcome.as_ref().err().map(crate::error::Error::code)
    }
}

/// Control transferred from one agent to another.
///
/// Raised before the transfer takes effect, so [`Self::from`] identifies the source agent
/// and [`Self::to`] identifies the target. Run-scoped hooks observe every transfer; the
/// agent-scoped callback belongs to the receiving agent, matching `AgentHooksBase.on_handoff`.
/// The receiving agent's start callback follows the transfer.
///
/// Neither side is handed an [`AgentSpec`], for the reason [`RunAgent`] gives: the spec reaches
/// transport headers and the other agent's executable tools, and an extension point that could read
/// either would be a way around the whole dispatch chain.
///
/// # No transfer happens in this build
///
/// Turn settlement refuses a handoff call, so nothing reaches this moment yet. It is wired at the
/// one point control actually changes hands rather than left out, because that point is also where
/// the agent-scoped hooks are swapped, and a transfer that moved the narration without announcing
/// it would be the harder half done and the visible half missing.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct HandoffInput<'a> {
    run: &'a RunContext,
    services: &'a ToolServices,
    to: RunAgent,
}

impl<'a> HandoffInput<'a> {
    /// Describes a transfer out of the agent `run` is attributed to and into `to`.
    ///
    /// The target arrives as its declaration and is projected here rather than at the call site,
    /// which is what keeps an [`AgentSpec`] from reaching a callback by being passed through.
    pub fn new(run: &'a RunContext, to: &AgentSpec) -> Self {
        Self {
            run,
            services: ToolServices::none(),
            to: RunAgent::from_spec(to),
        }
    }

    /// Installs the framework ports this run holds.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run, still attributed to the agent handing control over.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// Framework ports available, including the task-state handle.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }

    /// The agent handing control over.
    #[must_use]
    pub const fn from(&self) -> &'a RunAgent {
        self.run.agent()
    }

    /// The agent that speaks next.
    #[must_use]
    pub const fn to(&self) -> &RunAgent {
        &self.to
    }
}

// ---------------------------------------------------------------------------
// the contract
// ---------------------------------------------------------------------------

/// Narration of what happens in a run, at whichever scope it was installed.
///
/// Attach one to an [`AgentSpec`] and it travels with the agent: an agent reached by a handoff
/// brings its own, and the agent that handed over stops being told anything. Install one on the run
/// and it is told about every agent in it. The scope reaches the callback as an argument, so an
/// object installed in both places can tell its two streams of firings apart.
///
/// Every method has a default that does nothing, so an implementation covers only the moments it
/// cares about. Unlike a [`UserHook`](crate::hook::UserHook) there is nothing to register: this
/// family decides nothing, so there is no scope a wrong declaration could widen and no ambiguity a
/// subscription would have to resolve.
///
/// [`Self::name`] is a display label, exactly as [`UserHook::name`](crate::hook::UserHook::name)
/// and [`InputGuardrail::name`](crate::guardrail::InputGuardrail::name) are. Two hooks may share
/// one; both are installed and both are told. Nothing looks a hook up by it.
#[async_trait]
pub trait LifecycleHook: Send + Sync + 'static {
    /// Display label for traces, shared by its metric.
    fn name(&self) -> &str;

    /// An agent became the one running.
    async fn on_agent_start(
        &self,
        scope: LifecycleScope,
        input: &AgentStartInput<'_>,
    ) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }

    /// An agent produced the answer the run delivers.
    async fn on_agent_end(&self, scope: LifecycleScope, input: &AgentEndInput<'_>) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }

    /// The model is about to be called.
    async fn on_llm_start(&self, scope: LifecycleScope, input: &LlmStartInput<'_>) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }

    /// The model call produced a response.
    async fn on_llm_end(&self, scope: LifecycleScope, input: &LlmEndInput<'_>) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }

    /// A tool is about to be invoked.
    async fn on_tool_start(&self, scope: LifecycleScope, input: &ToolStartInput<'_>) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }

    /// A tool invocation settled.
    async fn on_tool_end(&self, scope: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }

    /// Control transferred to another agent.
    async fn on_handoff(&self, scope: LifecycleScope, input: &HandoffInput<'_>) -> Result<()> {
        let _ = (scope, input);
        Ok(())
    }
}
