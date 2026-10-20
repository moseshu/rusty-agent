//! What a nested run inherits from the run whose tool call started it.
//!
//! The reference hands an agent tool the parent's run configuration through its tool context, and
//! the nested run falls back to it when the tool was given none. Here that context is
//! [`ToolContext`](ra_core::tool::ToolContext), which lives in `ra-core` and cannot name
//! [`RunConfig`] or the model resolver without reversing the crate dependency. So the runner keeps
//! the parent's execution environment in a task-local for the duration of its loop, carries it
//! across the task spawn each tool dispatch makes, and narrows it to the call's own cancellation
//! scope immediately around [`Tool::call`](ra_core::tool::Tool::call).
//!
//! Only the enclosing run identity is exposed outside this crate. A tool invoked directly, outside
//! the runner's dispatch, finds no parent here, which [`super::AgentTool`] reports as a caller error
//! rather than guessing a model provider.
//!
//! The narrowing also carries, for a call the parent is resuming, the checkpoint of the nested run
//! that call paused on. The reference finds that state in its registry by tool-call identity; here
//! the parent's own state holds it, and the runner hands it to exactly the call it belongs to. A
//! nested run installs its own environment around its loop, so the checkpoint never reaches a call
//! the nested run makes in turn.

use std::{
    any::Any,
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};

use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    item::ModelInputItem,
    model::ModelResolver,
    session::rollout::RolloutRecorder,
    state::{EventSeqAllocator, RunId, RunState},
    tool::ToolServices,
};

use crate::{agent::control::AgentTreeRef, budget::RunSpend, runner::RunConfig};

tokio::task_local! {
    static CURRENT: ParentRun;
}

/// The parent run's execution environment, as a nested run starts from it.
#[derive(Clone)]
pub(crate) struct ParentRun {
    run: Arc<ParentRunEnvironment>,
    call_scope: Option<CancelScope>,
    resume: Option<Arc<RunState>>,
}

struct ParentRunEnvironment {
    model_resolver: Arc<dyn ModelResolver>,
    config: RunConfig,
    /// The declaration supplied with the run configuration, including any prepared surface.
    /// Handoffs update the current turn without transferring ownership of that surface.
    initial_agent: Arc<AgentSpec>,
    app_context: Option<Arc<dyn Any + Send + Sync>>,
    run_id: RunId,
    root_run_id: RunId,
    /// The group of the trace the run executes in, which a nested run's trace shares.
    trace_group_id: Option<String>,
    services: ToolServices,
    rollout: Option<Arc<dyn RolloutRecorder>>,
    /// The run's host event sequence, which events the control plane records for it draw from.
    event_seqs: EventSeqAllocator,
    agent_tree: Option<AgentTreeRef>,
    /// What the run has spent, which a nested run it starts bills its own calls to as well.
    spend: Arc<RunSpend>,
    /// The agent running the current turn and the history that turn's request was built from.
    /// The loop replaces it every turn, so a handoff is reflected from the turn it takes effect.
    turn: Mutex<TurnView>,
}

#[derive(Clone)]
struct TurnView {
    agent: Arc<AgentSpec>,
    history: Arc<Vec<ModelInputItem>>,
}

impl ParentRun {
    /// Captures a run's environment before its loop consumes the request.
    ///
    /// `agent` owns any prepared surface in `config` and initially runs the current turn. The loop
    /// reports each turn's agent separately, so a handoff preserves that original ownership.
    /// `services` are the ports the run was given. An agent spawned into the background from this
    /// run starts from both. `event_seqs` is the run's host event sequence, `agent_tree` the agent
    /// tree it executes in, and `spend` its live spend. `trace_group_id` is the group of the trace
    /// the run executes in.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        model_resolver: Arc<dyn ModelResolver>,
        config: RunConfig,
        app_context: Option<Arc<dyn Any + Send + Sync>>,
        run_id: RunId,
        parent_run_id: Option<&RunId>,
        trace_group_id: Option<String>,
        agent: Arc<AgentSpec>,
        services: ToolServices,
        rollout: Option<Arc<dyn RolloutRecorder>>,
        event_seqs: EventSeqAllocator,
        agent_tree: Option<AgentTreeRef>,
        spend: Arc<RunSpend>,
    ) -> Self {
        // Only agent-tool runs inherit this identity; see `enclosing`.
        let root_run_id = Self::enclosing(parent_run_id)
            .map_or_else(|| run_id.clone(), |parent| parent.run.root_run_id.clone());
        Self {
            run: Arc::new(ParentRunEnvironment {
                model_resolver,
                config,
                initial_agent: Arc::clone(&agent),
                app_context,
                run_id,
                root_run_id,
                trace_group_id,
                services,
                rollout,
                event_seqs,
                agent_tree,
                spend,
                turn: Mutex::new(TurnView {
                    agent,
                    history: Arc::new(Vec::new()),
                }),
            }),
            call_scope: None,
            resume: None,
        }
    }

    /// The environment of the run whose code is executing, if a runner installed one.
    pub(crate) fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok()
    }

    /// The agent-tool run that started a nested run with `parent_run_id`, if one is executing.
    ///
    /// Only agent-tool runs have one. Background agents have independent runs, even when their
    /// checkpoints record a parent for hooks and communication.
    pub(crate) fn enclosing(parent_run_id: Option<&RunId>) -> Option<Self> {
        Self::current().filter(|parent| Some(parent.run_id()) == parent_run_id)
    }

    /// Runs `future` as code belonging to this run.
    pub(crate) async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }

    /// Narrows the environment to one tool call: its cancellation scope, and the paused nested run
    /// it continues, if it is being resumed.
    #[must_use]
    pub(crate) fn for_call(
        mut self,
        call_scope: CancelScope,
        resume: Option<Arc<RunState>>,
    ) -> Self {
        self.call_scope = Some(call_scope);
        self.resume = resume;
        self
    }

    pub(crate) fn model_resolver(&self) -> &Arc<dyn ModelResolver> {
        &self.run.model_resolver
    }

    pub(crate) fn config(&self) -> &RunConfig {
        &self.run.config
    }

    /// The original declaration supplied with the run configuration, before any handoff.
    pub(crate) fn initial_agent(&self) -> &Arc<AgentSpec> {
        &self.run.initial_agent
    }

    pub(crate) fn app_context(&self) -> Option<&Arc<dyn Any + Send + Sync>> {
        self.run.app_context.as_ref()
    }

    pub(crate) fn run_id(&self) -> &RunId {
        &self.run.run_id
    }

    /// The group of the trace this run executes in.
    pub(crate) fn trace_group_id(&self) -> Option<&str> {
        self.run.trace_group_id.as_deref()
    }

    /// Records the agent running this turn and the history its request was built from.
    pub(crate) fn enter_turn(&self, agent: &Arc<AgentSpec>, history: Vec<ModelInputItem>) {
        *self.run.turn.lock().unwrap_or_else(PoisonError::into_inner) = TurnView {
            agent: Arc::clone(agent),
            history: Arc::new(history),
        };
    }

    /// The agent running the current turn.
    pub(crate) fn current_agent(&self) -> Arc<AgentSpec> {
        Arc::clone(&self.turn_view().agent)
    }

    /// The history the current turn's request was built from.
    pub(crate) fn current_history(&self) -> Arc<Vec<ModelInputItem>> {
        self.turn_view().history
    }

    fn turn_view(&self) -> TurnView {
        self.run
            .turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn services(&self) -> &ToolServices {
        &self.run.services
    }

    pub(crate) fn child_rollout(
        &self,
        recorder: Option<Arc<dyn RolloutRecorder>>,
    ) -> Option<Arc<dyn RolloutRecorder>> {
        self.run
            .rollout
            .as_ref()
            .and_then(|parent| parent.child_trace(recorder.clone()))
            .or(recorder)
    }

    /// The run's host event sequence.
    pub(crate) fn event_seqs(&self) -> &EventSeqAllocator {
        &self.run.event_seqs
    }

    /// The agent tree the run executes in, if any, which a nested run it starts executes in too.
    pub(crate) fn agent_tree(&self) -> Option<&AgentTreeRef> {
        self.run.agent_tree.as_ref()
    }

    /// What the run has spent, as a nested run it starts sees it.
    pub(crate) fn spend(&self) -> &Arc<RunSpend> {
        &self.run.spend
    }

    /// The scope of the tool call being executed; absent outside a dispatched call.
    pub(crate) const fn call_scope(&self) -> Option<&CancelScope> {
        self.call_scope.as_ref()
    }

    /// The paused nested run the call being executed continues; absent for a fresh call.
    pub(crate) fn nested_resume(&self) -> Option<&RunState> {
        self.resume.as_deref()
    }
}

/// The outermost run enclosing this agent-tool invocation, if a runner installed one.
pub(super) fn current_root_run_id() -> Option<RunId> {
    ParentRun::current().map(|parent| parent.run.root_run_id.clone())
}

/// Carries the current run's environment into a future that is about to be spawned.
///
/// A task-local does not follow a spawn, and the batch executor spawns every tool dispatch; without
/// this, a nested agent called from a batch would find no parent at all.
pub(crate) fn carry<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    let parent = ParentRun::current();
    async move {
        match parent {
            Some(parent) => parent.scope(future).await,
            None => future.await,
        }
    }
}

/// Runs a tool invocation narrowed to its own call, when it belongs to a run.
pub(crate) async fn within_call<F: Future>(
    call_scope: &CancelScope,
    resume: Option<Arc<RunState>>,
    future: F,
) -> F::Output {
    match ParentRun::current() {
        Some(parent) => {
            parent
                .for_call(call_scope.clone(), resume)
                .scope(future)
                .await
        }
        None => future.await,
    }
}
