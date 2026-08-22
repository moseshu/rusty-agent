//! The live context of one run, and the single door to host application state.
//!
//! There are exactly two context layers in this framework, and each has exactly one shape: this
//! run-level [`RunContext`], and the call-level
//! [`ToolContext`](crate::tool::ToolContext) derived from it. A tool, a dynamic instruction, a
//! guard and a hook all read the same object, so a run cannot end up with one notion of "who is
//! running" for prompts and another for tools.
//!
//! # What it is not
//!
//! **Not model input.** Nothing here is projected into a
//! [`ModelRequest`](crate::model::ModelRequest); a model request is built explicitly from items,
//! and host objects, approvals and credentials never leak into it by being reachable from a
//! context.
//!
//! **Not a checkpoint.** This value is deliberately not serializable. It holds a live host object
//! and a safe public-agent view, neither of which survives a process. Everything a resumed
//! segment must still know belongs to [`RunState`](crate::state::RunState), which owns those facts;
//! what appears here is a **read view** taken from that owner. That is why [`RunContext::budget`]
//! hands out a borrowed snapshot rather than a counter: a second accumulator that a tool could
//! advance would be a second answer to "what has this run spent", and the one that survives a
//! checkpoint would not be it.
//!
//! # Application state has one door
//!
//! [`RunContext::app_context`] is it. The alternative — making the context generic over the
//! host's type, as the reference implementation's `RunContextWrapper[TContext]` does — spreads
//! that parameter to agents, tools and every registry, and two tools written against different
//! host types can then no longer be registered together. Type erasure plus one checked read keeps
//! the tool surface heterogeneous, and the framework's own ports stay named accessors on
//! [`ToolServices`](crate::tool::ToolServices) rather than anonymous lookups.
//!
//! [`WorkStateHandle::as_any`](crate::state::WorkStateHandle::as_any) is **not** a second door: it
//! reaches the cross-run task state, which is a different thing owned by a different party.

use std::{any::Any, fmt, sync::Arc};

use crate::{
    agent::AgentSpec,
    budget::BudgetSnapshot,
    event::{HostEventEmitter, HostEventSink},
    item::AgentId,
    state::{EventSeqAllocator, PendingControlRequest, RunId},
    usage::Usage,
};

/// The credential-free identity of the public agent a run is attributed to.
///
/// Tools, hooks and dynamic instructions need to know who is speaking, but they must not receive
/// the complete [`AgentSpec`]. Two things reachable from it are the reason, and the second is the
/// sharper one:
///
/// - **Its model settings carry transport headers.** `AgentSpec::model_settings` reaches
///   `ModelSettings::extra_headers`, which is where an API key sits, and a tool that can read one
///   can write it into model-visible output.
/// - **It owns the agent's tools.** `AgentSpec::tools` hands back the executable objects, so a tool
///   holding the spec could call a sibling directly — around caller admission, the repeat and
///   no-progress breakers, approval, guardrails, the per-call timeout, and every record the
///   dispatch chain writes. A capability the framework decides is behind approval would be one
///   method call away from anything already running.
///
/// A future field belongs here as another projection of the public agent. Reaching for the spec
/// again, however convenient, reopens both.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgent {
    id: AgentId,
    name: String,
}

impl RunAgent {
    fn from_spec(agent: &AgentSpec) -> Self {
        Self {
            id: agent.id().clone(),
            name: agent.name().to_owned(),
        }
    }

    /// Stable public identity of the agent.
    #[must_use]
    pub const fn id(&self) -> &AgentId {
        &self.id
    }

    /// Public display name of the agent.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Everything running code may read about the run it is part of.
///
/// Cloning is not offered: the run owns one of these per stage and hands out borrows, so there is
/// no copy that can drift from the state it was projected from.
#[non_exhaustive]
pub struct RunContext {
    run_id: RunId,
    agent: RunAgent,
    app: Option<Arc<dyn Any + Send + Sync>>,
    budget: BudgetSnapshot,
    usage_totals: Usage,
    pending_control_requests: Vec<PendingControlRequest>,
    event_seq_allocator: Option<EventSeqAllocator>,
}

impl RunContext {
    /// Creates the context of a run that is executing `agent` under `run_id`.
    ///
    /// `agent` is the **public** agent — the one the user configured and the one every record is
    /// attributed to. A capability or sandbox step may produce a different instance that actually
    /// executes, and that instance stays inside the framework: a hook reporting it, or a dynamic
    /// instruction branching on it, would be describing something the user never wrote down.
    #[must_use]
    pub fn new(run_id: RunId, agent: &AgentSpec) -> Self {
        Self {
            run_id,
            agent: RunAgent::from_spec(agent),
            app: None,
            budget: BudgetSnapshot::new(),
            usage_totals: Usage::default(),
            pending_control_requests: Vec::new(),
            event_seq_allocator: None,
        }
    }

    /// Attaches the host's own state object, readable through [`Self::app_context`].
    #[must_use]
    pub fn with_app_context(mut self, app_context: Arc<dyn Any + Send + Sync>) -> Self {
        self.app = Some(app_context);
        self
    }

    /// Sets the budget accounting this stage observes.
    ///
    /// The value is a copy taken from [`RunState`](crate::state::RunState), not a handle into it.
    /// Read views are how a context reports run facts; advancing them is the settlement point's
    /// job.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetSnapshot) -> Self {
        self.budget = budget;
        self
    }

    /// Sets the usage ledger this stage observes.
    ///
    /// A copy taken from [`RunState`](crate::state::RunState) for the same reason the budget is one:
    /// the context reports what the run has spent, and only the settlement point adds to it.
    #[must_use]
    pub fn with_usage_totals(mut self, usage_totals: Usage) -> Self {
        self.usage_totals = usage_totals;
        self
    }

    /// Sets the pending control requests observed by this stage.
    #[must_use]
    pub fn with_pending_control_requests(
        mut self,
        pending_control_requests: Vec<PendingControlRequest>,
    ) -> Self {
        self.pending_control_requests = pending_control_requests;
        self
    }

    /// Attaches the event sequence allocator for host event allocation.
    #[must_use]
    pub fn with_event_seq_allocator(mut self, allocator: EventSeqAllocator) -> Self {
        self.event_seq_allocator = Some(allocator);
        self
    }

    /// Identity of this run, stable across every segment it is resumed in.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The credential-free public agent view this run is attributed to.
    #[must_use]
    pub const fn agent(&self) -> &RunAgent {
        &self.agent
    }

    /// Public identity of the agent that is speaking.
    #[must_use]
    pub const fn agent_id(&self) -> &AgentId {
        self.agent.id()
    }

    /// The host's own state object, when it is of type `T`.
    ///
    /// `None` covers both "the host attached nothing" and "the host attached something else",
    /// which is what makes this a checked read rather than a cast: a tool written for one
    /// application cannot silently reinterpret another's context.
    #[must_use]
    pub fn app_context<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.app.as_ref().and_then(|app| app.downcast_ref::<T>())
    }

    /// Turns the run has taken so far, as of the stage that built this context.
    ///
    /// Read-only by construction. The authoritative accounting is
    /// [`RunState::budget`](crate::state::RunState::budget), and the limits it is measured against
    /// are run configuration rather than a fact about the run.
    #[must_use]
    pub const fn budget(&self) -> &BudgetSnapshot {
        &self.budget
    }

    /// Tokens the run has spent so far, per request and in total.
    ///
    /// The same ledger [`RunState::usage_totals`](crate::state::RunState::usage_totals) owns, as of
    /// the stage that built this context — including the call that has just been paid for, so a
    /// tool reads spend that already includes the turn it is running under.
    #[must_use]
    pub const fn usage_totals(&self) -> &Usage {
        &self.usage_totals
    }

    /// Pending control or approval requests, as of the stage that built this context.
    #[must_use]
    pub fn pending_control_requests(&self) -> &[PendingControlRequest] {
        &self.pending_control_requests
    }

    /// Sequence allocator for host events within this run, when attached.
    #[must_use]
    pub fn event_seq_allocator(&self) -> Option<&EventSeqAllocator> {
        self.event_seq_allocator.as_ref()
    }

    /// Constructs a [`HostEventEmitter`] using this run's sequence allocator and agent identity.
    #[must_use]
    pub fn event_emitter(&self, sink: Arc<dyn HostEventSink>) -> Option<HostEventEmitter> {
        let allocator = self.event_seq_allocator()?.clone();
        Some(HostEventEmitter::new(
            self.agent_id().clone(),
            allocator,
            sink,
        ))
    }
}

impl fmt::Debug for RunContext {
    /// Names what is attached without printing it. A host context is arbitrary application state,
    /// and a run's own log is not the place for it to appear.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("agent_id", self.agent_id())
            .field("has_app_context", &self.app.is_some())
            .field("budget", &self.budget)
            .field(
                "pending_control_requests",
                &self.pending_control_requests.len(),
            )
            .field(
                "has_event_seq_allocator",
                &self.event_seq_allocator.is_some(),
            )
            .finish_non_exhaustive()
    }
}
