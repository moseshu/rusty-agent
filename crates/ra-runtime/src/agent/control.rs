//! The live agent tree: background agents, their mailboxes, and their runs.
//!
//! Ported from Codex's `MultiAgentV2` local control (`core/src/agent/control*`, the session input
//! queue, and the v2 collaboration tool handlers). One [`AgentControl`] owns one tree. The host
//! binds the root to the runs it starts with [`RunRequest::with_agent_control`]; the agents spawned
//! below it run in the background, each on runs this module starts.
//!
//! # How an agent's runs are driven
//!
//! A spawned agent is a conversation rather than a single run, as a Codex thread is: its history
//! carries over from one run to the next. A run starts when the agent is idle and its mailbox holds
//! a message that asks for one ([`MessageDeliveryMode::TriggerTurn`]) — the spawn task, or a
//! follow-up. Everything waiting in the mailbox becomes that run's new input. While a run is in
//! progress, mail is delivered into it at the next model-call boundary instead (see the runner),
//! which is how a parent redirects a child without stopping it. When a run ends and the mailbox
//! still holds a follow-up, the next run starts at once.
//!
//! A run bound to an agent reports its own status: it marks the agent running when it starts and
//! records how it ended, and a spawned agent's run then sends its parent a `FINAL_ANSWER` message,
//! as Codex's `turn_finished` does. A parent's `wait_agent` returns on that mail.
//!
//! # Interrupting
//!
//! An interrupt cancels the agent's current run. The turns the run had settled before it stopped
//! stay in the agent's history — the run is streamed so they are known even though a cancelled run
//! returns no result — followed, as in Codex, by a marker telling the model the run was interrupted
//! on purpose, unless [`RunConfig::with_interrupt_message`] turns it off. The agent is then idle and
//! can be given a follow-up.
//!
//! This keeps less than Codex does. Codex keeps what the interrupted turn had already streamed;
//! this runner records a turn only when it settles, so the turn in flight at the interrupt — the
//! model's answer and any calls it was running — leaves nothing behind.
//!
//! # Approvals
//!
//! A background run that stops to ask for approval keeps its turn: it stays
//! [`AgentStatus::Running`], holds its execution slot, and waits as a checkpoint the host lists with
//! [`AgentControl::paused_runs`] and continues with [`AgentControl::resume`] — Codex likewise keeps
//! a thread's turn active and routes the request to the user, not into the parent's model turn.
//! The answers are given on the checkpoint with the ordinary [`RunState`] approval API, so a nested
//! question inside the background run is answered the same way as one in any other run. Mail that
//! arrives meanwhile is delivered once the run continues.
//!
//! # Spawning
//!
//! A spawned agent runs the declaration of the agent running the caller's current turn — after a
//! handoff, the agent handed to — or the registered declaration named by `agent_type`. It starts
//! from the caller's history projected as Codex forks it (see
//! [`fork_history`](ra_core::agent::control::fork_history)), followed by its task.
//!
//! Codex builds each spawned agent as a session of its own, with its own services and tool plan.
//! A host whose tools hold per-thread resources, such as a process manager, gives the tree a
//! [`SpawnedAgentFactory`] that prepares each spawned agent the same way; without one, the agent
//! shares the resolved declaration's tool instances with the agent that spawned it.
//!
//! # Closing
//!
//! [`AgentControl::close`] shuts an agent down together with every live agent below it, as Codex's
//! `close_agent` and `shutdown_agent_tree` do: the descendants are found first, then the agent and
//! each descendant in turn is stopped — its run cancelled and waited for, its rollout shut down —
//! and removed from the tree, so it is no longer listed and its path can be spawned again. The whole subtree is marked
//! closed before the first one stops, so nothing can be spawned into it or started in it meanwhile.
//!
//! Cancelling a run does not close anything by default: Codex leaves an interrupted agent's
//! children running, and so does this tree. A host that wants a cancelled run to take its spawned
//! agents with it opts in with [`AgentControl::with_close_descendants_on_cancel`]; a run bound to an
//! agent that ends cancelled then closes every live agent below that agent before it returns.
//!
//! # Budget and depth
//!
//! As in Codex, a spawned agent's spend stays its own: it is not added to the ledger of the run
//! that spawned it, whose result reports that run's calls. What the tree shares is an optional
//! token budget, Codex's rollout budget ([`AgentControl::with_rollout_budget`]): every run in the
//! tree is charged against it, each agent is told what is left as its runs start, and every run
//! stops once it is used up. An agent-tool run is different — it is part of the call that started
//! it, and its spend is that run's too (see [`crate::agent::tool`]).
//!
//! The tree has no depth limit unless given one with [`AgentControl::with_max_depth`], Codex's
//! `agent_max_depth`: the version of Codex these tools port does not check depth, and its first
//! version does.
//!
//! # Activity on the host timeline
//!
//! As in Codex, what happens to a spawned agent is recorded on the timeline of the agent that
//! caused it, as an [`AgentEvent::SubAgentActivity`] host event in the sink of that agent's run.
//! The collaboration tools record that they started, contacted or interrupted an agent; the tree
//! records that a run completed, on the run whose spawn or follow-up started it — Codex's parent
//! turn. That run may have ended by then; the event is still its own, numbered in its sequence.
//! So that a continuation of the run from its checkpoint cannot reissue that number, the tree
//! keeps the sequence of every run that has named itself an origin, and a run continued in the tree
//! — bound to an agent, or an agent-tool run inside it — draws from that same sequence, raised past
//! its own checkpoint and log. A run continued outside the tree relies on its checkpoint alone,
//! which covers only what was numbered before the checkpoint was taken. A run started by mail from
//! several runs, or from none, records no completion, as Codex's does not without a single parent
//! turn. A run that errors, is interrupted or is shut down records none either: its parent learns
//! of it from the `FINAL_ANSWER` mail alone.
//!
//! # Rollouts
//!
//! As in Codex, a spawned agent is a thread of its own: it is given a session at spawn, which its
//! [`LiveAgent`] and the activity on its parent's timeline name, and a tree given a store with
//! [`AgentControl::with_rollout_store`] creates the agent a rollout under that session. Its session
//! metadata says where it came from — the session it was spawned from, the root's, its depth, its
//! path and the agent type asked for — as Codex's `ThreadSpawn` source does. Every run of the agent
//! records into it, so the rollout holds the agent's whole life and rebuilds the history the agent
//! continues from. Codex records a turn's new input against the history its thread already holds;
//! here a run starts on the agent's history followed by its mail, and records only the part the
//! rollout does not hold yet — the history it was forked with, on its first run, and its mail. A
//! run waiting for approval when it is interrupted or shut down is recorded as cancelled, as Codex
//! aborts the turn. The root's runs are the host's, which records them into the root's session.
//! With the compaction capability installed, the agent checks this history before a fresh turn's
//! mail and between steps that need another model call. Its replacement belongs to that agent.
//!
//! Closing an agent, or shutting the tree down, shuts down the agent's rollout once its run has
//! ended, as Codex's session shutdown shuts down its thread's live writer: what is still queued is
//! written and the file is let go of, so the thread can be resumed as soon as the close returns.
//! Codex persists and flushes the thread before it asks the session to shut down, and a flush that
//! fails there fails the close and keeps the thread. Here the run is cancelled before anything is
//! waited for (see [`AgentControl::close`]), so that barrier is the rollout's shutdown, after the
//! run ends: one that fails fails the close and keeps the agent in the tree — closed, cancelled,
//! with its recorder — so closing it again, or shutting the tree down, writes what it still holds.
//!
//! # What this does not port
//!
//! - The V2 tool surface does not expose per-spawn model and reasoning-effort overrides.
//! - Agent nicknames and Codex role descriptions.
//!
//! V1 can reopen a spawned agent by session ID and restore its open descendants. The persisted
//! spawn metadata and explicit close markers in each rollout replace Codex's agent graph table.
//! Storeless trees retain closed conversations in memory for the life of the tree.

mod budget;
mod legacy;
mod spawn;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use ra_core::{
    agent::{
        AgentId, AgentSpec,
        control::{
            AgentControlError, AgentControlPort, AgentPath, AgentStatus, InterAgentCommunication,
            LiveAgent, MessageDeliveryMode, SpawnAgentRequest, WaitOutcome, completion_message,
        },
    },
    cancel::{CancelReason, CancelScope, ScopeKind},
    error::{Error, Result},
    event::{
        AgentEvent, ApprovalEvent, HostEvent, HostEventEmitter, SubAgentActivityEvent,
        SubAgentActivityKind,
    },
    finish::FinishReason,
    item::{Message, ModelInputItem, RunItem},
    model::ModelResolver,
    session::{
        InterruptedTurnHistoryMarker, SessionId,
        interrupt::is_interrupt,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded, RolloutThreadSpawn,
            RolloutThreadStore,
        },
    },
    state::{EventSeqAllocator, RunId, RunState},
    tool::ToolServices,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

pub use budget::RolloutBudgetConfig;
pub(crate) use budget::{RolloutBudget, RolloutBudgetReminder};
pub use spawn::{
    PreparedSpawnedAgent, SpawnedAgentFactory, SpawnedAgentRequest, SpawnedAgentResources,
};

use super::{AgentBinding, tool::ParentRun};
use crate::runner::{
    ContinuationInput, RunConfig, RunOutcome, RunOwner, RunRequest, RunResult, RunStreamEvent,
    Runner, history_compaction::SpawnedRunHistory,
};

/// Default execution ceiling for hosts that construct a tree directly.
pub const DEFAULT_MAX_CONCURRENT_THREADS: usize = 4;

/// Codex's refusal of a spawn past the depth limit.
const DEPTH_LIMIT_REACHED: &str = "Agent depth limit reached. Solve the task yourself.";

/// The control plane of one agent tree.
///
/// Cloning shares the tree. Dropping the last clone cancels every run in it; [`Self::shutdown`]
/// does the same and also waits for those runs to stop.
#[derive(Clone)]
pub struct AgentControl {
    tree: Arc<Tree>,
}

impl AgentControl {
    /// Creates a tree holding only its root, with the default execution ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_concurrent_threads(DEFAULT_MAX_CONCURRENT_THREADS)
    }

    /// Creates a tree in which at most `max_threads` spawned agents run at once. The root does not
    /// count against it.
    #[must_use]
    pub fn with_max_concurrent_threads(max_threads: usize) -> Self {
        let root = Arc::new(AgentNode::new(AgentPath::root(), None, None));
        let tree = Arc::new(Tree {
            root: Arc::clone(&root),
            root_thread: OnceLock::new(),
            cancel: CancelScope::root(),
            limiter: Arc::new(ExecutionLimiter {
                active: AtomicUsize::new(0),
                max_threads,
            }),
            agents: Mutex::new(BTreeMap::from([(AgentPath::root(), root)])),
            tasks: Mutex::new(Vec::new()),
            shut_down: AtomicBool::new(false),
            close_descendants_on_cancel: AtomicBool::new(false),
            max_depth: AtomicUsize::new(usize::MAX),
            resident_limits: OnceLock::new(),
            closed_history: Mutex::new(HashMap::new()),
            legacy_children_loaded: tokio::sync::OnceCell::new(),
            live_agent_lookup: OnceLock::new(),
            rollout_budget: OnceLock::new(),
            rollout_store: OnceLock::new(),
            root_lineage: OnceLock::new(),
            reservations: Mutex::new(BTreeMap::new()),
            frozen_spawns: Mutex::new(Vec::new()),
            spawned_agent_factory: OnceLock::new(),
            shutdown: Mutex::new(None),
            paused_activity: watch::Sender::new(0),
            event_seqs: Mutex::new(HashMap::new()),
            #[cfg(feature = "test-api")]
            before_pause: OnceLock::new(),
        });
        Self { tree }
    }

    /// Limits how deep the tree may grow: an agent spawned by the root is at depth 1, and no agent
    /// may spawn one deeper than `max_depth`.
    ///
    /// A host can set this limit independently of collaboration version. Product V1 trees also
    /// enforce their configured lineage-aware limit. At the limit, collaboration tools are hidden
    /// and spawning is refused. Zero keeps the root from spawning. The limit applies through every
    /// clone of this control.
    #[must_use]
    pub fn with_max_depth(self, max_depth: usize) -> Self {
        self.tree.max_depth.store(max_depth, Ordering::Release);
        self
    }

    /// Applies resident-agent admission and a lineage-aware depth limit when `enabled` holds.
    /// A resident agent holds a slot until shutdown, including after its run has finished.
    #[must_use]
    pub fn with_resident_agent_limits(
        self,
        max_threads: usize,
        max_depth: i32,
        enabled: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        let _ = self.tree.resident_limits.set(legacy::ResidentLimits::new(
            max_threads,
            max_depth,
            enabled,
        ));
        self
    }

    /// Gives the tree a shared token budget, Codex's rollout budget.
    ///
    /// Every run in the tree — the host's runs bound to the root, every spawned agent's runs, and
    /// the agent-tool runs any of them start — is charged for each model call in weighted tokens,
    /// and each agent is told what is left when its runs start: the whole budget the first time,
    /// and again whenever the remainder has fallen past another of the configured thresholds.
    /// Once the charges reach the limit, each run in the tree ends after its next response with
    /// [`FinishReason::BudgetExhausted`] — the response that spent the budget included, whose answer
    /// is then kept in history but not delivered. Off by default, as in Codex; see
    /// [`RolloutBudgetConfig`] for how calls are charged and where this differs from Codex.
    ///
    /// # Errors
    ///
    /// Refuses a configuration Codex refuses — a zero limit, a reminder at zero or at or above the
    /// limit, a negative or non-finite weight — and a second budget for the same tree.
    pub fn with_rollout_budget(self, config: RolloutBudgetConfig) -> Result<Self> {
        config.validate()?;
        self.tree
            .rollout_budget
            .set(Arc::new(RolloutBudget::new(config)))
            .map_err(|_| Error::config("the agent tree already has a rollout budget"))?;
        Ok(self)
    }

    /// Records every agent the tree spawns into a rollout of its own, created through `store`; the
    /// root's runs are those of session `root_session_id`.
    ///
    /// Codex's threads: a spawned agent is a thread with its own rollout, whose session metadata
    /// names the thread it was spawned from, the tree's root and the agent's depth and path (see
    /// [`RolloutThreadSpawn`]). Each spawned agent is given a session of its own whether or not the
    /// tree has a store — [`LiveAgent::session_id`](ra_core::agent::control::LiveAgent::session_id)
    /// and the activity on its parent's timeline name it — and with one, the agent's rollout is
    /// created as it is spawned, and every run of the agent records into it as a run given
    /// [`RunRequest::with_rollout_recorder`] does: across follow-ups and pauses for approval, for
    /// as long as the agent lives. A run records only what is new to the agent's thread: the
    /// history it starts on came from the agent's earlier runs and is already there. A store that
    /// cannot create the rollout fails the spawn.
    ///
    /// The root's runs are the host's: it records them itself, with
    /// [`RunRequest::with_rollout_recorder`], into the rollout of `root_session_id`. Off by
    /// default; Codex always records its threads.
    ///
    /// # Errors
    ///
    /// Refuses a second store for the same tree.
    pub fn with_rollout_store(
        self,
        store: Arc<dyn RolloutThreadStore>,
        root_session_id: SessionId,
    ) -> Result<Self> {
        self.set_root_session(root_session_id)?;
        self.tree
            .rollout_store
            .set(store)
            .map_err(|_| Error::config("the agent tree already has a rollout store"))?;
        Ok(self)
    }

    /// Names the session the root's runs belong to, without recording the tree's agents: the
    /// identity a spawned agent's hooks report as its parent's, and the root and parent sessions
    /// its spawn record names once the tree is also given a store with
    /// [`Self::with_rollout_store`], which must then name the same session.
    ///
    /// # Errors
    ///
    /// Refuses a session other than the one the root already has.
    pub fn with_root_session_id(self, root_session_id: SessionId) -> Result<Self> {
        self.set_root_session(root_session_id)?;
        Ok(self)
    }

    /// Places the root inside the tree its own thread was spawned in: the root is the thread
    /// `spawn` describes, resumed on its own, and the spawn records of its agents continue that
    /// lineage — the original root session, depths counted from the root's own, and paths below its
    /// own — as Codex derives a spawned thread's depth and path from its parent's source. Addressing
    /// within this tree is unchanged.
    ///
    /// # Errors
    ///
    /// Refuses a second lineage for the same tree.
    pub fn with_root_lineage(self, spawn: &RolloutThreadSpawn) -> Result<Self> {
        self.tree
            .root_lineage
            .set(RootLineage {
                root_session_id: spawn.root_session_id().clone(),
                depth: spawn.depth(),
                path: spawn.agent_path().clone(),
            })
            .map_err(|_| Error::config("the agent tree already has a lineage"))?;
        Ok(self)
    }

    fn set_root_session(&self, root_session_id: SessionId) -> Result<()> {
        match self.tree.root.session_id.get() {
            Some(existing) if *existing != root_session_id => Err(Error::config(format!(
                "the agent tree's root is session `{existing}`, not `{root_session_id}`"
            ))),
            Some(_) => Ok(()),
            None => {
                let _ = self.tree.root.session_id.set(root_session_id);
                Ok(())
            }
        }
    }

    /// Prepares every agent the tree spawns with `factory`, as Codex's thread manager prepares each
    /// spawned thread as a session of its own (see [`SpawnedAgentFactory`]).
    ///
    /// Without a factory a spawned agent runs the declaration the spawn resolved and shares its
    /// tool instances with the agent that spawned it. With one, the factory decides what the agent
    /// runs and may give it resources of its own, which the tree releases once the agent is closed
    /// or the tree shuts down.
    ///
    /// # Errors
    ///
    /// Refuses a second factory for the same tree.
    pub fn with_spawned_agent_factory(self, factory: Arc<dyn SpawnedAgentFactory>) -> Result<Self> {
        self.tree
            .spawned_agent_factory
            .set(factory)
            .map_err(|_| Error::config("the agent tree already has a spawned-agent factory"))?;
        Ok(self)
    }

    /// Sets whether a cancelled run closes the agents below the agent it is bound to.
    ///
    /// Off by default, as in Codex, where interrupting an agent leaves its children running. When
    /// on, a run bound to an agent in this tree that ends cancelled — the host's root run, or a
    /// spawned agent's run stopped by an interrupt — closes every live agent below that agent, as
    /// [`Self::close`] does, before it returns. The agent itself stays: only its run was cancelled.
    /// The setting belongs to the tree, so it applies through every clone of this control.
    #[must_use]
    pub fn with_close_descendants_on_cancel(self, enabled: bool) -> Self {
        self.tree
            .close_descendants_on_cancel
            .store(enabled, Ordering::Release);
        self
    }

    /// The root agent's view of the tree, for the runs the host starts.
    #[must_use]
    pub fn root(&self) -> AgentHandle {
        AgentHandle::new(&self.tree, Arc::clone(&self.tree.root))
    }

    /// The handle of the agent at `path`, if the tree has one.
    #[must_use]
    pub fn handle(&self, path: &AgentPath) -> Option<AgentHandle> {
        self.tree
            .node(path)
            .map(|node| AgentHandle::new(&self.tree, node))
    }

    /// Every agent in the tree, the root first.
    #[must_use]
    pub fn agents(&self) -> Vec<LiveAgent> {
        self.tree.list(None)
    }

    /// Finds a live agent in this tree by its session ID, including the root.
    #[must_use]
    pub fn live_agent(&self, session_id: &SessionId) -> Option<LiveAgent> {
        self.tree
            .legacy_agent(session_id)
            .map(|node| node.snapshot())
    }

    /// Installs the host's lookup for V1 resume to find agents in other loaded trees.
    /// Subsequent calls keep the first lookup.
    pub fn set_live_agent_lookup(
        &self,
        lookup: impl Fn(&SessionId) -> Option<LiveAgent> + Send + Sync + 'static,
    ) {
        let _ = self.tree.live_agent_lookup.set(Box::new(lookup));
    }

    /// The current status of the agent at `path`.
    #[must_use]
    pub fn status(&self, path: &AgentPath) -> Option<AgentStatus> {
        self.tree.node(path).map(|node| node.status())
    }

    /// Waits until the agent at `path` has a status `accept` accepts, and returns it. Returns `None`
    /// when there is no such agent.
    pub async fn wait_for_status(
        &self,
        path: &AgentPath,
        mut accept: impl FnMut(&AgentStatus) -> bool,
    ) -> Option<AgentStatus> {
        let node = self.tree.node(path)?;
        let mut receiver = node.status.subscribe();
        let status = receiver.wait_for(|status| accept(status)).await.ok()?;
        Some(status.clone())
    }

    /// The spawned agents' runs waiting for the host to answer an approval, in path order.
    ///
    /// A background run that stops for approval is kept paused rather than ended. The host answers
    /// on the checkpoint, with [`RunState::approve`] and [`RunState::reject`] as for any run, and
    /// continues it with [`Self::resume`].
    #[must_use]
    pub fn paused_runs(&self) -> Vec<PausedAgentRun> {
        lock(&self.tree.agents)
            .values()
            .filter_map(|node| {
                let child = node.child.as_ref()?;
                let state = lock(&child.state);
                if node.is_closed()
                    || state
                        .current_run
                        .as_ref()
                        .is_none_or(CancelScope::is_cancelled)
                {
                    return None;
                }
                let paused = state.paused.as_ref()?;
                Some(PausedAgentRun {
                    path: node.path.clone(),
                    session_id: node.session_id.get()?.clone(),
                    state: paused.state.clone(),
                    config: match (&child.environment.config, &child.environment.app_context) {
                        (config, Some(context)) if config.app_context().is_none() => {
                            config.clone().with_app_context(Arc::clone(context))
                        }
                        (config, _) => config.clone(),
                    },
                })
            })
            .collect()
    }

    /// Subscribes before inspecting [`Self::paused_runs`], so a host can discover and withdraw
    /// approval batches without missing a pause, resume or cancellation between snapshots.
    #[must_use]
    pub fn subscribe_paused_runs(&self) -> watch::Receiver<u64> {
        self.tree.paused_activity.subscribe()
    }

    /// Records an approval on a spawned run's own event sequence and rollout before it resumes.
    /// Stale checkpoints are refused, including an earlier approval stop of the same run.
    pub fn record_approval(
        &self,
        path: &AgentPath,
        state: &mut RunState,
        event: ApprovalEvent,
    ) -> Result<()> {
        let node = self
            .tree
            .node(path)
            .ok_or_else(|| Error::caller("the agent is no longer live"))?;
        let child = node
            .child
            .as_ref()
            .ok_or_else(|| Error::caller("the root's approvals belong to its host"))?;
        let mut guard = lock(&child.state);
        let live = !node.is_closed()
            && guard
                .current_run
                .as_ref()
                .is_some_and(|scope| !scope.is_cancelled());
        let paused = guard
            .paused
            .as_mut()
            .filter(|paused| live && crate::thread::same_approval_pause(&paused.state, state))
            .ok_or_else(|| {
                Error::caller("the agent is no longer waiting on this approval batch")
            })?;
        let allocator = &paused.event_seqs;
        allocator.advance_past(&state.restore_event_seq_allocator(None));
        let event = HostEvent::allocate(
            allocator,
            child.environment.agent.id().clone(),
            event.into(),
        )?;
        state.snapshot_event_seq(allocator);
        paused.state.snapshot_event_seq(allocator);
        drop(guard);
        if let Some(rollout) = &child.environment.rollout {
            rollout.record(RolloutItem::Event(event.clone()));
        }
        if let Some(sink) = child.environment.services.event_sink() {
            sink.emit(event);
        }
        Ok(())
    }

    /// Waits until some spawned agent's run is paused for approval, and returns the first in path
    /// order.
    pub async fn wait_for_paused_run(&self) -> PausedAgentRun {
        let mut activity = self.tree.paused_activity.subscribe();
        loop {
            if let Some(paused) = self.paused_runs().into_iter().next() {
                return paused;
            }
            if activity.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    /// Continues the paused run of the agent at `path` from `state`, the run's checkpoint with the
    /// host's answers applied.
    ///
    /// # Errors
    ///
    /// Refuses when that agent has no paused run, or `state` is not that run's checkpoint.
    pub fn resume(&self, path: &AgentPath, state: RunState) -> Result<(), AgentControlError> {
        let node = self.tree.node(path).ok_or_else(|| {
            AgentControlError::Unsupported(format!("live agent path `{path}` not found"))
        })?;
        let child = node.child.as_ref().ok_or_else(|| {
            AgentControlError::Unsupported(format!("agent `{path}` has no paused run"))
        })?;
        let mut child_state = lock(&child.state);
        let paused = child_state.paused.take().ok_or_else(|| {
            AgentControlError::Unsupported(format!("agent `{path}` has no paused run"))
        })?;
        if paused.state.run_id() != state.run_id() {
            let run_id = paused.state.run_id().clone();
            child_state.paused = Some(paused);
            return Err(AgentControlError::InvalidRequest(format!(
                "the state does not belong to the paused run `{run_id}` of agent `{path}`"
            )));
        }
        drop(child_state);
        self.tree
            .paused_activity
            .send_modify(|count| *count = count.wrapping_add(1));
        paused
            .resume
            .send(state)
            .map_err(|_| AgentControlError::Unavailable)
    }

    /// Closes the agent at `path` and every live agent below it, and returns the status the agent
    /// had before it was closed.
    ///
    /// Each agent's run is cancelled and waited for — a run paused for approval ends — its rollout
    /// is shut down, and the agent is removed from the tree: it is no longer listed, messages to it
    /// are refused as to an unknown agent, and a later spawn may reuse its path. A closed agent
    /// reports nothing to its parent. Closing the root closes every spawned agent and keeps the root, whose runs belong to
    /// the host; the tree goes on accepting spawns.
    ///
    /// # Errors
    ///
    /// Refuses when the tree has no agent at `path`. Fails when an agent's rollout cannot be shut
    /// down: that agent stays in the tree, closed, and so does every agent the close had not
    /// reached yet, as Codex's close stops at a descendant it cannot shut down. The agent itself is
    /// still followed by its descendants. Closing it again tries again.
    pub async fn close(&self, path: &AgentPath) -> Result<AgentStatus, AgentControlError> {
        let node = self.tree.node(path).ok_or_else(|| {
            AgentControlError::Unsupported(format!("live agent path `{path}` not found"))
        })?;
        let previous = node.status();
        let report = self.tree.close_subtree(&node, true).finished().await;
        match report.failed().first() {
            Some((_, error)) => Err(AgentControlError::Unsupported(error.clone())),
            None => Ok(previous),
        }
    }

    /// Stops every spawned agent: cancels their runs, waits for them to end, shuts down their
    /// rollouts, and marks each [`AgentStatus::Shutdown`]. Further spawns and follow-ups are
    /// refused. A rollout that cannot be shut down is logged and kept, and shutting the tree down
    /// again tries it again.
    pub async fn shutdown(&self) {
        drop(self.shutdown_close().finished().await);
    }

    /// Starts shutting the tree down as [`Self::shutdown`] does, or joins the shutdown in
    /// progress, and returns what it froze — every spawned agent, and every spawn still preparing
    /// its agent — before anything is waited for, with the report of how each ended once it is
    /// done. A shutdown that left an agent's rollout open is started again by the next call.
    #[must_use]
    pub fn shutdown_close(&self) -> AgentClose {
        let (frozen, finished) = {
            let mut shutdown = lock(&self.tree.shutdown);
            if shutdown
                .as_ref()
                .is_none_or(|pending| pending.peek().is_some_and(|report| !report.is_complete()))
            {
                // Admission and shutdown observe the same lock. A preparation already in flight
                // may finish, but its agent is then admitted closed and stopped here.
                {
                    let _agents = lock(&self.tree.agents);
                    self.tree.shut_down.store(true, Ordering::Release);
                }
                self.tree.cancel.cancel(CancelReason::Shutdown);
                let tree = Arc::clone(&self.tree);
                let task = tokio::spawn(async move { tree.finish_shutdown().await });
                *shutdown = Some(
                    async move {
                        task.await.unwrap_or_else(|error| {
                            tracing::error!(%error, "the agent tree's shutdown task failed");
                            AgentCloseReport::default()
                        })
                    }
                    .boxed()
                    .shared(),
                );
            }
            (self.tree.live_instances(), shutdown.as_ref().cloned())
        };
        let (done, receiver) = oneshot::channel();
        if let Some(finished) = finished {
            drop(tokio::spawn(async move {
                let _ = done.send(finished.await);
            }));
        } else {
            let _ = done.send(AgentCloseReport::default());
        }
        AgentClose {
            frozen,
            finished: receiver,
        }
    }

    /// The report of the shutdown in progress or last finished, without starting another: what a
    /// host closing the tree's thread reads after the thread's own shutdown ran the tree's, so a
    /// failure there is reported rather than retried at once. Starts a shutdown only when none was
    /// ever started.
    pub async fn shutdown_report(&self) -> AgentCloseReport {
        let started = lock(&self.tree.shutdown).as_ref().cloned();
        match started {
            Some(shutdown) => shutdown.await,
            None => self.shutdown_close().finished().await,
        }
    }

    /// Closes the agent of `session_id` and every agent below it, as [`Self::close`] closes the one
    /// at a path, and returns what the close froze before anything is waited for. Matching on the
    /// session rather than the path, under the tree's lock, keeps a later agent spawned at the
    /// same path out of it. A spawn still preparing that agent is frozen and its agent admitted
    /// closed; one whose cleanup failed is cleaned up again. `None` when the tree has no agent or
    /// spawn of that session — it has already gone.
    #[must_use]
    pub fn close_session(&self, session_id: &SessionId) -> Option<AgentClose> {
        if let Some(node) = self.tree.node_of_session(session_id) {
            return Some(self.tree.close_subtree(&node, true));
        }
        let reservation = lock(&self.tree.reservations)
            .values()
            .find(|reservation| reservation.session_id == *session_id)
            .map(Arc::clone)?;
        let target = CloseTarget::freeze(&reservation);
        Some(self.tree.spawn_close(vec![target], None, None))
    }

    /// The run the agent of `session_id` is in, and the configuration its runs use: what a
    /// decision made for that agent while it runs reads, as Codex reads its spawned session's
    /// active turn. `None` while the agent runs nothing, or when the tree has no such agent.
    #[must_use]
    pub fn running_run(&self, session_id: &SessionId) -> Option<(RunId, RunConfig)> {
        let node = self.tree.node_of_session(session_id)?;
        let child = node.child.as_ref()?;
        let run_id = child.running_run.borrow().clone()?;
        Some((run_id, child.environment.config.clone()))
    }

    /// Waits until the agent of `session_id` is no longer in the run `run_id`: it ended, the agent
    /// was closed, or the tree has no such agent.
    pub async fn run_ended(&self, session_id: &SessionId, run_id: &RunId) {
        let Some(node) = self.tree.node_of_session(session_id) else {
            return;
        };
        let Some(child) = node.child.as_ref() else {
            return;
        };
        let mut running = child.running_run.subscribe();
        let _ = running
            .wait_for(|running| running.as_ref() != Some(run_id))
            .await;
    }

    pub(crate) fn bind_root_thread(&self, thread: crate::thread::WeakAgentThread) {
        let _ = self.tree.root_thread.set(thread);
    }

    /// Adds `messages` to the context of the agent of `session_id`, as Codex's `inject_no_new_turn`
    /// adds them to a session: to the run in progress, which takes them in and records them at its
    /// next step, or, while the agent runs nothing, to the history its next run starts from and
    /// records. Whether the tree has such an agent.
    #[must_use]
    pub fn inject_context(&self, session_id: &SessionId, messages: Vec<Message>) -> bool {
        let Some(node) = self.tree.node_of_session(session_id) else {
            return false;
        };
        node.inject_context(messages)
    }

    /// Calls `hook` each time a spawned agent's run has stopped for approval, before the tree takes
    /// the pause: the moment the runner has closed the run's context lane and the paused run's own
    /// lane is not there yet. Only for tests; a second hook is ignored.
    #[cfg(feature = "test-api")]
    #[doc(hidden)]
    pub fn set_before_pause_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.tree.before_pause.set(hook);
    }

    /// Every spawned agent of the tree and every spawn still holding a path, as instances.
    #[must_use]
    pub fn live_instances(&self) -> Vec<AgentInstance> {
        self.tree.live_instances()
    }

    /// Refuses every further spawn and follow-up run, as [`Self::shutdown`] does, without stopping
    /// anything yet, and returns every agent and spawn the tree holds at that moment: what a host
    /// closing the tree's thread freezes before the thread's own `SessionEnd` hooks run, so the
    /// agents' rollouts are let go of only after them.
    #[must_use]
    pub fn stop_spawning(&self) -> Vec<AgentInstance> {
        {
            let _agents = lock(&self.tree.agents);
            self.tree.shut_down.store(true, Ordering::Release);
            // Reservations are locked after the agents, as everywhere: no spawn is reserved or
            // settled between the flag and this snapshot.
            let reservations = lock(&self.tree.reservations);
            let mut frozen = lock(&self.tree.frozen_spawns);
            for reservation in reservations.values() {
                if !frozen.iter().any(|kept| Arc::ptr_eq(kept, reservation)) {
                    frozen.push(Arc::clone(reservation));
                }
            }
        }
        self.tree.live_instances()
    }

    /// Waits until every spawn the tree holds a path for has settled, and reports each: one that
    /// cleaned up is closed; one whose rollout could not be discarded is not, and stays reserved,
    /// holding it. A spawn whose cleanup had already failed is cleaned up again first.
    ///
    /// [`Self::shutdown`] does not wait for spawns still preparing their agents — the tree refuses
    /// them when they finish — so a host that must know every rollout of the tree was let go of,
    /// before moving or removing them, waits here after shutting the tree down.
    pub async fn settle_spawns(&self) -> AgentCloseReport {
        let mut reservations: Vec<Arc<Reservation>> = lock(&self.tree.reservations)
            .values()
            .map(Arc::clone)
            .collect();
        // A spawn the host froze that has settled since is reported too: it cleaned up, and the
        // host holding it as frozen learns it is closed only here.
        for frozen in lock(&self.tree.frozen_spawns).iter() {
            if !reservations
                .iter()
                .any(|reservation| Arc::ptr_eq(reservation, frozen))
            {
                reservations.push(Arc::clone(frozen));
            }
        }
        let mut report = AgentCloseReport::default();
        for reservation in reservations {
            if matches!(
                *reservation.state.borrow(),
                ReservationState::CleanupFailed(_)
            ) {
                self.tree.retry_reservation(&reservation).await;
            }
            match reservation.settled().await {
                ReservationState::CleanupFailed(error) => {
                    report.failed.push((reservation.instance(), error));
                }
                ReservationState::Admitted(_) => {}
                ReservationState::Released
                | ReservationState::Discarded
                | ReservationState::Pending => report.closed.push(reservation.instance()),
            }
        }
        report
    }
}

impl Tree {
    /// The tree owns shutdown so cancelling any waiter leaves cleanup running for other waiters.
    /// A failed rollout close permits another shutdown attempt after this task completes.
    async fn finish_shutdown(self: &Arc<Self>) -> AgentCloseReport {
        let mut report = AgentCloseReport::default();
        // A spawn still preparing its agent is not waited for: the tree refuses the agent when the
        // preparation finishes, and the spawn cleans up. A spawn whose cleanup already failed is
        // cleaned up again here; one that cannot be stays reserved for the next shutdown.
        let failed: Vec<Arc<Reservation>> = lock(&self.reservations)
            .values()
            .filter(|reservation| {
                matches!(
                    *reservation.state.borrow(),
                    ReservationState::CleanupFailed(_)
                )
            })
            .map(Arc::clone)
            .collect();
        for reservation in failed {
            self.retry_reservation(&reservation).await;
            match &*reservation.state.borrow() {
                ReservationState::CleanupFailed(error) => {
                    report.failed.push((reservation.instance(), error.clone()));
                }
                _ => report.closed.push(reservation.instance()),
            }
        }
        loop {
            let tasks = std::mem::take(&mut *lock(&self.tasks));
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                drop(task.await);
            }
        }
        let spawned: Vec<Arc<AgentNode>> = lock(&self.agents)
            .values()
            .filter(|node| !node.path.is_root())
            .map(Arc::clone)
            .collect();
        for node in spawned {
            if let Some(child) = &node.child {
                drop(lock(&child.state));
                let mut idle = child.idle.subscribe();
                drop(idle.wait_for(|idle| *idle).await);
            }
            // Logged by the node; a rollout that could not be shut down is tried again by the next
            // shutdown.
            match node.shut_down_rollout().await {
                Ok(()) => report.closed.push(node.instance()),
                Err(error) => report.failed.push((node.instance(), error.to_string())),
            }
            node.release_resources().await;
            node.status.send_replace(AgentStatus::Shutdown);
        }
        report
    }

    fn live_instances(&self) -> Vec<AgentInstance> {
        let agents = lock(&self.agents);
        let reservations = lock(&self.reservations);
        agents
            .values()
            .filter(|node| !node.path.is_root())
            .map(|node| node.instance())
            .chain(
                reservations
                    .values()
                    .map(|reservation| reservation.instance()),
            )
            .collect()
    }
}

impl Default for AgentControl {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AgentControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentControl")
            .field("agents", &lock(&self.tree.agents).len())
            .field("max_threads", &self.tree.limiter.max_threads)
            .finish_non_exhaustive()
    }
}

/// A spawned agent's run that is waiting for the host to answer an approval.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct PausedAgentRun {
    path: AgentPath,
    session_id: SessionId,
    state: RunState,
    config: RunConfig,
}

impl PausedAgentRun {
    /// The spawned agent's session, which owns its requests and approval records.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The run configuration used by the issuing step.
    pub const fn config(&self) -> &RunConfig {
        &self.config
    }
    /// The agent whose run is paused.
    #[must_use]
    pub const fn path(&self) -> &AgentPath {
        &self.path
    }

    /// The run's checkpoint. Answer its pending items and pass it to [`AgentControl::resume`].
    #[must_use]
    pub const fn state(&self) -> &RunState {
        &self.state
    }
}

/// One agent's view of its tree: the [`AgentControlPort`] its runs' tools are given.
///
/// It holds the tree weakly, so a handle kept by a tool or a stored run does not keep a tree alive
/// that its [`AgentControl`] has let go of; operations on such a handle report the control plane
/// unavailable.
#[derive(Clone)]
pub struct AgentHandle {
    tree: Weak<Tree>,
    node: Arc<AgentNode>,
    pending_input: Option<crate::runner::PendingInput>,
}

impl AgentHandle {
    fn new(tree: &Arc<Tree>, node: Arc<AgentNode>) -> Self {
        Self {
            tree: Arc::downgrade(tree),
            node,
            pending_input: None,
        }
    }

    /// Binds the active run's existing input queue without changing the tree-owned handle.
    pub(crate) fn with_pending_input(
        mut self,
        pending: Option<crate::runner::PendingInput>,
    ) -> Self {
        self.pending_input = pending;
        self
    }

    /// The agent this handle acts for.
    #[must_use]
    pub fn path(&self) -> &AgentPath {
        &self.node.path
    }

    /// The agent's current status.
    #[must_use]
    pub fn status(&self) -> AgentStatus {
        self.node.status()
    }

    /// Whether mail is waiting for this agent's next model call.
    #[must_use]
    pub fn has_pending_mail(&self) -> bool {
        self.node.mailbox.has_pending()
    }

    fn tree(&self) -> Result<Arc<Tree>, AgentControlError> {
        self.tree.upgrade().ok_or(AgentControlError::Unavailable)
    }

    /// The tree this handle's agent belongs to.
    pub(crate) fn tree_ref(&self) -> AgentTreeRef {
        AgentTreeRef(self.tree.clone())
    }

    /// The rollout budget of this agent's tree, if it has one and is still alive.
    pub(crate) fn rollout_budget(&self) -> Option<Arc<RolloutBudget>> {
        self.tree.upgrade()?.rollout_budget.get().map(Arc::clone)
    }

    /// The reminder of the tree's remaining budget this agent is owed, if any.
    pub(crate) fn pending_budget_reminder(&self) -> Option<RolloutBudgetReminder> {
        self.rollout_budget()?
            .pending_reminder(*lock(&self.node.budget_reminder_delivered))
    }

    /// Records that `reminder` is in this agent's history.
    pub(crate) fn mark_budget_reminder_delivered(&self, reminder: RolloutBudgetReminder) {
        *lock(&self.node.budget_reminder_delivered) = Some(reminder.reminder_index);
    }

    /// Takes the mail a run bound to this agent delivers at a model-call boundary.
    pub(crate) fn take_mail(&self) -> Vec<InterAgentCommunication> {
        if self
            .tree
            .upgrade()
            .is_some_and(|tree| tree.resident_limits().is_some())
            && let Some(child) = &self.node.child
        {
            let state = lock(&child.state);
            if state
                .current_run
                .as_ref()
                .is_some_and(CancelScope::is_cancelled)
            {
                return Vec::new();
            }
            return self.node.mailbox.take_all();
        }
        self.node.mailbox.take_all()
    }

    /// Marks the agent running a run of `agent`.
    pub(crate) fn run_started(&self, agent: &AgentId) {
        *lock(&self.node.agent_id) = Some(agent.clone());
        self.node.status.send_replace(AgentStatus::Running);
    }

    /// Records how a run bound to this agent ended, as [`Self::run_finished`] does, and when the
    /// run was cancelled in a tree that asks for it, closes the agents below this one.
    pub(crate) async fn run_ended(&self, result: &Result<RunResult>) {
        self.run_finished(result);
        if !result
            .as_ref()
            .is_err_and(ra_core::error::Error::is_cancelled)
        {
            return;
        }
        let Some(tree) = self.tree.upgrade() else {
            return;
        };
        // A closing agent's subtree is already being closed, and a shutdown stops everything.
        if tree.close_descendants_on_cancel.load(Ordering::Acquire)
            && !tree.shut_down.load(Ordering::Acquire)
            && !self.node.is_closed()
            && let Some((_, error)) = tree
                .close_subtree(&self.node, false)
                .finished()
                .await
                .failed()
                .first()
        {
            tracing::warn!(
                path = %self.node.path,
                %error,
                "the agents below a cancelled run could not all be closed"
            );
        }
    }

    /// Records how a run bound to this agent ended and, for a spawned agent, reports it to the
    /// parent.
    pub(crate) fn run_finished(&self, result: &Result<RunResult>) {
        let status = terminal_status(result);
        let tree = self.tree.upgrade();
        let shutting_down = tree
            .as_ref()
            .is_none_or(|tree| tree.shut_down.load(Ordering::Acquire))
            || self.node.is_closed();
        if shutting_down && !self.node.path.is_root() {
            self.node.status.send_replace(AgentStatus::Shutdown);
            return;
        }
        // Recorded before the status is published, so a host woken by the status finds it.
        if let (AgentStatus::Completed(_), Ok(result)) = (&status, result) {
            self.record_completion(result.state().run_id());
        }
        if let Some(tree) = tree
            .as_ref()
            .filter(|tree| tree.resident_limits().is_some())
        {
            tree.notify_legacy_completion(&self.node, &status);
            self.node.status.send_replace(status);
            return;
        }
        self.node.status.send_replace(status.clone());
        if let Some(tree) = tree
            && let Some(parent) = self.node.path.parent()
            && let Some(parent_node) = tree.node(&parent)
            && let Some(message) = completion_message(&parent, &self.node.path, &status)
        {
            parent_node.mailbox.push(message);
        }
    }

    /// Records on the timeline of the run that started this agent's current run that the run
    /// completed, as Codex's `notify_parent_of_terminal_turn` does for a turn with a parent turn.
    /// A run started by mail from more than one run, or by none, records nothing.
    fn record_completion(&self, run_id: &RunId) {
        let Some(child) = &self.node.child else {
            return;
        };
        let Some(origin) = lock(&child.state).run_origin.clone() else {
            return;
        };
        let mut event = SubAgentActivityEvent::new(
            format!("subagent-completed-{run_id}"),
            self.node.path.clone(),
            SubAgentActivityKind::Completed,
        );
        if let Some(session_id) = self.node.session_id.get() {
            event = event.with_agent_session_id(session_id.clone());
        }
        if let Err(error) = origin.emit_agent(AgentEvent::SubAgentActivity(event)) {
            tracing::warn!(
                agent = %self.node.path,
                %error,
                "a sub-agent completion could not be recorded on the host event channel"
            );
        }
    }
}

/// The tree a run executes in, for a run not bound to one of its agents — a nested agent-tool run
/// started inside it — as well as for one that is.
#[derive(Clone)]
pub(crate) struct AgentTreeRef(Weak<Tree>);

impl AgentTreeRef {
    /// The host event sequence a run starting with `allocator` must draw from, so that a run
    /// continued from its checkpoint shares one sequence with the completions an earlier segment
    /// of it asked to be told about.
    pub(crate) fn adopt_event_seqs(&self, allocator: EventSeqAllocator) -> EventSeqAllocator {
        match self.0.upgrade() {
            Some(tree) => tree.adopt_event_seqs(allocator),
            None => allocator,
        }
    }
}

impl AgentHandle {
    fn check_spawn_allowed(&self, tree: &Tree) -> Result<(), AgentControlError> {
        if tree.shut_down.load(Ordering::Acquire) {
            return Err(AgentControlError::Unavailable);
        }
        if self.node.is_closed() {
            return Err(closed_error(&self.node.path));
        }
        if self.node.is_closing_subtree() {
            return Err(closing_error(&self.node.path));
        }
        if tree.spawn_depth_exceeded(&self.node.path) {
            return Err(AgentControlError::Unsupported(
                DEPTH_LIMIT_REACHED.to_owned(),
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for AgentHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentHandle")
            .field("path", &self.node.path)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentControlPort for AgentHandle {
    fn caller(&self) -> &AgentPath {
        &self.node.path
    }

    async fn spawn(&self, request: SpawnAgentRequest) -> Result<LiveAgent, AgentControlError> {
        let tree = self.tree()?;
        self.restore_legacy_children(&tree).await?;
        self.check_spawn_allowed(&tree)?;
        let parent = ParentRun::current().ok_or_else(|| {
            AgentControlError::Unsupported(
                "agents can only be spawned by an agent running under a runner".to_owned(),
            )
        })?;
        let path = self
            .node
            .path
            .join(request.task_name())
            .map_err(AgentControlError::Unsupported)?;
        let agent = spawned_declaration(&tree, &parent, request.agent_type())?;
        let config = tree
            .spawn_run_config(&request, &agent, parent.config())
            .await?;
        let (history, reported_tokens) = parent.fork_history(request.fork_mode().copied());
        if tree.resident_limits().is_none() && !tree.limiter.has_capacity() {
            return Err(AgentControlError::AgentLimitReached {
                max_threads: tree.limiter.max_threads,
            });
        }
        // As Codex reserves a spawn slot before creating the thread: a spawn the tree refuses never
        // creates anything, and one it accepts is part of the caller's subtree from here on.
        let session_id = SessionId::generate();
        let reservation = tree.reserve(&self.node, &path, &session_id)?;
        let mut guard = SpawnGuard::new(&tree, Arc::clone(&reservation));
        let prepared = tree
            .prepare_spawned(
                &path,
                agent,
                request.agent_type(),
                &session_id,
                self.node.session_id.get(),
                request.fork_mode().is_some(),
            )
            .await?;
        let config = prepared.configure_run(config, parent.initial_agent());
        let (agent, resources) = prepared.into_parts();
        guard.resources.clone_from(&resources);
        let rollout = parent.child_rollout(
            create_rollout(&tree, &self.node, &session_id, &path, request.agent_type()).await?,
        );
        guard.recorder.clone_from(&rollout);
        if let (Some(resources), Some(rollout)) = (&resources, &rollout) {
            resources.rollout_created(rollout);
        }

        let environment = ChildEnvironment::from_parent(
            &parent,
            Arc::clone(&agent),
            request.agent_type().cloned(),
            config,
            rollout,
            resources,
        );
        let child = ChildRuntime::new(
            environment,
            tree.cancel.child(ScopeKind::custom("agent")),
            history,
            reported_tokens,
        );
        let node = Arc::new(AgentNode::with_instance(
            path.clone(),
            Some(agent.id().clone()),
            Some(child),
            reservation.instance,
        ));
        let _ = node.session_id.set(session_id);
        match tree.commit(&reservation, &node) {
            Commit::Admitted => guard.disarm(),
            // A close that reached the reservation first now closes the agent: it was admitted
            // closed, and the close it was frozen by stops it like any other agent of the subtree.
            Commit::AdmittedClosed(refusal) => {
                guard.disarm();
                return Err(refusal);
            }
            // The tree shut down meanwhile: the agent is not admitted, and the guard cleans up
            // what the spawn has of it.
            Commit::Refused(refusal) => return Err(refusal),
        }
        let communication = match request.input() {
            Some(input) => InterAgentCommunication::from_user_input(
                self.node.path.clone(),
                path.clone(),
                input.clone(),
            ),
            None => InterAgentCommunication::from_agent(
                self.node.path.clone(),
                path.clone(),
                request.message(),
                MessageDeliveryMode::TriggerTurn,
            ),
        };
        node.mailbox.push_from(
            communication,
            Some(parent.run_id().clone()),
            tree.origin_of(&parent),
        );
        tree.ensure_running(&node);
        Ok(node.snapshot())
    }

    async fn send(
        &self,
        target: &str,
        message: String,
        mode: MessageDeliveryMode,
    ) -> Result<AgentPath, AgentControlError> {
        let tree = self.tree()?;
        let node = tree.resolve(&self.node.path, target)?;
        if mode == MessageDeliveryMode::TriggerTurn && node.path.is_root() {
            return Err(AgentControlError::Unsupported(
                "Follow-up tasks can't target the root agent".to_owned(),
            ));
        }
        if node.status() == AgentStatus::Shutdown
            || node.is_closed()
            || tree.shut_down.load(Ordering::Acquire)
        {
            return Err(closed_error(&node.path));
        }
        if mode == MessageDeliveryMode::TriggerTurn
            && !node.is_running()
            && !tree.limiter.has_capacity()
        {
            return Err(AgentControlError::AgentLimitReached {
                max_threads: tree.limiter.max_threads,
            });
        }
        // Only a follow-up names the run that asked for it, as only Codex's trigger-turn mail
        // carries a parent turn.
        let parent = (mode == MessageDeliveryMode::TriggerTurn)
            .then(ParentRun::current)
            .flatten();
        let parent_run_id = parent.as_ref().map(|parent| parent.run_id().clone());
        let origin = parent.as_ref().and_then(|parent| tree.origin_of(parent));
        node.mailbox.push_from(
            InterAgentCommunication::from_agent(
                self.node.path.clone(),
                node.path.clone(),
                &message,
                mode,
            ),
            parent_run_id,
            origin,
        );
        if mode == MessageDeliveryMode::TriggerTurn {
            tree.ensure_running(&node);
        }
        Ok(node.path.clone())
    }

    async fn interrupt(&self, target: &str) -> Result<AgentStatus, AgentControlError> {
        let tree = self.tree()?;
        let node = tree.resolve(&self.node.path, target)?;
        if node.path.is_root() {
            return Err(AgentControlError::Unsupported(
                "root is not a spawned agent".to_owned(),
            ));
        }
        if node.path == self.node.path {
            return Err(AgentControlError::Unsupported(
                "an agent cannot interrupt itself; return your result and let the parent \
                 interrupt you if needed"
                    .to_owned(),
            ));
        }
        let previous = node.status();
        if let Some(child) = &node.child
            && let Some(run) = &lock(&child.state).current_run
        {
            run.cancel(CancelReason::UserInterrupt);
        }
        Ok(previous)
    }

    async fn list(&self, path_prefix: Option<&str>) -> Result<Vec<LiveAgent>, AgentControlError> {
        let tree = self.tree()?;
        let prefix = path_prefix
            .map(|prefix| {
                self.node
                    .path
                    .resolve(prefix)
                    .map_err(AgentControlError::Unsupported)
            })
            .transpose()?;
        Ok(tree.list(prefix.as_ref()))
    }

    async fn wait_for_mailbox(&self, timeout: Duration) -> WaitOutcome {
        self.node
            .mailbox
            .wait(timeout, self.pending_input.as_ref())
            .await
    }

    async fn send_input(
        &self,
        target: &SessionId,
        input: Message,
        interrupt: bool,
    ) -> Result<(String, AgentPath), AgentControlError> {
        self.send_legacy_input(target, input, interrupt).await
    }

    async fn wait_for_agents(
        &self,
        targets: &[SessionId],
        timeout: Duration,
    ) -> Result<BTreeMap<String, AgentStatus>, AgentControlError> {
        self.wait_for_legacy_agents(targets, timeout).await
    }

    async fn close_agent(&self, target: &SessionId) -> Result<LiveAgent, AgentControlError> {
        self.close_legacy_agent(target).await
    }

    async fn resume_agent(&self, target: &SessionId) -> Result<LiveAgent, AgentControlError> {
        self.resume_legacy_agent(target).await
    }

    fn agent_session_id(&self, path: &AgentPath) -> Option<SessionId> {
        self.tree.upgrade()?.node(path)?.session_id.get().cloned()
    }

    fn spawn_depth_exceeded(&self) -> bool {
        self.tree
            .upgrade()
            .is_some_and(|tree| tree.spawn_depth_exceeded(&self.node.path))
    }
}

/// The declaration a spawn resolves: the registered agent named by `agent_type`, or the agent
/// running the caller's current turn, which a handoff may have changed since the run started.
fn spawned_declaration(
    tree: &Tree,
    parent: &ParentRun,
    agent_type: Option<&AgentId>,
) -> Result<Arc<AgentSpec>, AgentControlError> {
    let Some(agent_type) = agent_type else {
        return Ok(parent.current_agent());
    };
    if let Some(agent) = parent.config().agent_registry().get(agent_type) {
        return Ok(Arc::clone(agent));
    }
    if tree
        .spawned_agent_factory
        .get()
        .is_some_and(|factory| factory.owns_agent_type(agent_type))
    {
        return Ok(parent.current_agent());
    }
    let message = if tree.resident_limits().is_some() {
        format!("unknown agent_type '{agent_type}'")
    } else {
        format!("unknown agent_type `{agent_type}`")
    };
    Err(AgentControlError::InvalidRequest(message))
}

/// Releases a spawned agent's own resources, if it was given any.
fn path_taken(path: &AgentPath) -> AgentControlError {
    AgentControlError::Unsupported(format!("agent path `{path}` already exists"))
}

/// Creates the rollout of the agent being spawned at `path` by `caller`, when the tree has a store
/// for them, as Codex creates a spawned thread with its `ThreadSpawn` source.
async fn create_rollout(
    tree: &Tree,
    caller: &AgentNode,
    session_id: &SessionId,
    path: &AgentPath,
    agent_type: Option<&AgentId>,
) -> Result<Option<Arc<dyn RolloutRecorder>>, AgentControlError> {
    let Some(store) = tree.rollout_store.get() else {
        return Ok(None);
    };
    let (Some(root_session_id), Some(parent_session_id)) =
        (tree.root.session_id.get(), caller.session_id.get())
    else {
        return Err(AgentControlError::Unavailable);
    };
    let local_depth = u32::try_from(depth(path)).unwrap_or(u32::MAX);
    let mut spawn = match tree.root_lineage.get() {
        None => RolloutThreadSpawn::new(
            root_session_id.clone(),
            parent_session_id.clone(),
            local_depth,
            path.clone(),
        ),
        Some(lineage) => RolloutThreadSpawn::new(
            lineage.root_session_id.clone(),
            parent_session_id.clone(),
            lineage.depth.saturating_add(local_depth),
            below(&lineage.path, path).map_err(AgentControlError::Unsupported)?,
        ),
    };
    if let Some(agent_type) = agent_type {
        spawn = spawn.with_agent_type(agent_type.clone());
    }
    store
        .create_thread(session_id, &spawn)
        .await
        .map(Some)
        .map_err(|error| {
            AgentControlError::Unsupported(format!(
                "the rollout of agent `{path}` could not be created: {error}"
            ))
        })
}

fn closed_error(path: &AgentPath) -> AgentControlError {
    AgentControlError::Unsupported(format!("agent `{path}` is closed"))
}

fn closing_error(path: &AgentPath) -> AgentControlError {
    AgentControlError::Unsupported(format!(
        "the agents below `{path}` are being closed; spawn again once that is done"
    ))
}

/// The status a run's result leaves its agent in.
fn terminal_status(result: &Result<RunResult>) -> AgentStatus {
    match result {
        Err(error) if error.is_cancelled() => AgentStatus::Interrupted,
        Err(error) => AgentStatus::Errored(error.to_string()),
        Ok(result) => match result.outcome() {
            // Waiting for an approval is not the end of the run: it continues once answered, and
            // Codex reports a thread in that state as running.
            RunOutcome::Interrupted { .. } => AgentStatus::Running,
            RunOutcome::Completed { reason } => match result.final_message() {
                Some(_) => AgentStatus::Completed(Some(result.final_text())),
                None if matches!(
                    reason,
                    FinishReason::MaxTurns | FinishReason::BudgetExhausted
                ) =>
                {
                    AgentStatus::Errored(format!("the agent stopped before concluding ({reason})"))
                }
                None => AgentStatus::Completed(None),
            },
        },
    }
}

struct Tree {
    root: Arc<AgentNode>,
    root_thread: OnceLock<crate::thread::WeakAgentThread>,
    /// Parent of every spawned agent's scope; cancelled on shutdown and when the tree is dropped.
    cancel: CancelScope,
    limiter: Arc<ExecutionLimiter>,
    agents: Mutex<BTreeMap<AgentPath, Arc<AgentNode>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    shut_down: AtomicBool,
    /// Whether a cancelled run closes the agents below the agent it is bound to.
    close_descendants_on_cancel: AtomicBool,
    /// The deepest an agent may be; `usize::MAX` when the tree has no limit.
    max_depth: AtomicUsize,
    resident_limits: OnceLock<legacy::ResidentLimits>,
    closed_history: Mutex<HashMap<SessionId, legacy::ClosedAgent>>,
    legacy_children_loaded: tokio::sync::OnceCell<()>,
    live_agent_lookup: OnceLock<Box<legacy::LiveAgentLookup>>,
    rollout_budget: OnceLock<Arc<RolloutBudget>>,
    /// Where the spawned agents' rollouts are created, once the host has given the tree a store.
    rollout_store: OnceLock<Arc<dyn RolloutThreadStore>>,
    /// How each spawned agent is prepared, once the host has given the tree a factory.
    spawned_agent_factory: OnceLock<Arc<dyn SpawnedAgentFactory>>,
    /// The shared shutdown task; a failed rollout close can replace a completed attempt.
    shutdown: Mutex<Option<Shared<BoxFuture<'static, AgentCloseReport>>>>,
    /// Ticks whenever an agent's run pauses for approval.
    paused_activity: watch::Sender<u64>,
    /// The spawns [`AgentControl::stop_spawning`] returned, kept with their outcome after they
    /// settle and leave `reservations`, so [`AgentControl::settle_spawns`] reports a spawn the host
    /// froze as closed even once nothing of it is left in the tree.
    frozen_spawns: Mutex<Vec<Arc<Reservation>>>,
    /// Called once a run has stopped for approval and before the tree takes its pause.
    #[cfg(feature = "test-api")]
    before_pause: OnceLock<Arc<dyn Fn() + Send + Sync>>,
    /// The host event sequence of every run that has named itself as the origin of another run,
    /// which records that run's completion from it later. Kept for the tree's lifetime: the run
    /// may be continued at any time, and its continuation must draw from the same sequence.
    event_seqs: Mutex<HashMap<RunId, EventSeqAllocator>>,
    /// Where the root itself sits in a larger tree, for a root that is itself a spawned thread.
    root_lineage: OnceLock<RootLineage>,
    /// Paths reserved by spawns that have not yet added their agent, or whose cleanup failed.
    /// Locked after `agents` whenever both are held.
    reservations: Mutex<BTreeMap<AgentPath, Arc<Reservation>>>,
}

/// The spawn record of a root that is itself a spawned thread, resumed on its own: what its
/// agents' spawn records continue from, as Codex derives a spawned thread's depth and path from its
/// parent's source.
struct RootLineage {
    root_session_id: SessionId,
    depth: u32,
    path: AgentPath,
}

impl Tree {
    async fn spawn_run_config(
        &self,
        request: &SpawnAgentRequest,
        agent: &AgentSpec,
        parent: &RunConfig,
    ) -> Result<RunConfig, AgentControlError> {
        if let Some(factory) = self.spawned_agent_factory.get()
            && let Some(config) = factory
                .configure_spawn(request, agent, parent)
                .await
                .map_err(|error| {
                    let message = match error {
                        Error::Caller { message, .. } => message,
                        other => other.to_string(),
                    };
                    AgentControlError::InvalidRequest(message)
                })?
        {
            return Ok(config);
        }
        if let Some(label) = request.reasoning_effort_label()
            && request.reasoning_effort().is_none()
        {
            return Err(AgentControlError::InvalidRequest(format!(
                "Invalid reasoning effort: {label}"
            )));
        }
        let mut config = parent.clone();
        if let Some(model) = request.model() {
            config = config.with_model(model);
        }
        if let Some(effort) = request.reasoning_effort() {
            let settings = config.model_settings().clone().with_effort(effort);
            config = config.with_model_settings(settings);
        }
        Ok(config)
    }

    /// What the agent spawned at `path` runs, and what it holds of its own: the tree's
    /// spawned-agent factory's answer, or the resolved declaration as it is without one.
    async fn prepare_spawned(
        &self,
        path: &AgentPath,
        agent: Arc<AgentSpec>,
        agent_type: Option<&AgentId>,
        session_id: &SessionId,
        parent_session_id: Option<&SessionId>,
        forked: bool,
    ) -> Result<PreparedSpawnedAgent, AgentControlError> {
        let Some(factory) = self.spawned_agent_factory.get() else {
            return Ok(PreparedSpawnedAgent::new(agent));
        };
        factory
            .prepare(SpawnedAgentRequest::new(
                path,
                &agent,
                agent_type,
                session_id,
                parent_session_id,
                forked,
            ))
            .await
            .map_err(|error| {
                AgentControlError::Unsupported(format!(
                    "agent `{path}` could not be prepared: {error}"
                ))
            })
    }

    /// Reserves `path` below `caller` for the agent of `session_id`, unless the tree is shut down,
    /// the caller or its subtree is being closed, or the path is taken by an agent or another
    /// reservation: Codex's `reserve_spawn_slot`, which comes before anything of the agent exists.
    ///
    /// Checked under the lock a close marks its subtree under, so a spawn racing a close cannot
    /// leave a child below an agent that is going away, or add one to a subtree being emptied; a
    /// close that comes after the reservation freezes it and waits for it to settle.
    fn reserve(
        &self,
        caller: &AgentNode,
        path: &AgentPath,
        session_id: &SessionId,
    ) -> Result<Arc<Reservation>, AgentControlError> {
        let agents = lock(&self.agents);
        if self.shut_down.load(Ordering::Acquire) {
            return Err(AgentControlError::Unavailable);
        }
        if caller.is_closed() {
            return Err(closed_error(&caller.path));
        }
        if caller.is_closing_subtree() {
            return Err(closing_error(&caller.path));
        }
        let mut reservations = lock(&self.reservations);
        if agents.contains_key(path)
            || reservations.contains_key(path)
            || agents
                .values()
                .any(|node| node.session_id.get() == Some(session_id))
            || reservations
                .values()
                .any(|reservation| &reservation.session_id == session_id)
        {
            return Err(path_taken(path));
        }
        if let Some(limits) = self.resident_limits() {
            let occupied = agents.len().saturating_sub(1) + reservations.len();
            if occupied >= limits.max_threads {
                return Err(AgentControlError::AgentLimitReached {
                    max_threads: limits.max_threads,
                });
            }
        }
        let reservation = Arc::new(Reservation::new(path.clone(), session_id.clone()));
        reservations.insert(path.clone(), Arc::clone(&reservation));
        Ok(reservation)
    }

    /// Replaces `reservation` with `node`, its agent. Once the tree has shut down the agent is not
    /// admitted: the spawn is refused and its guard cleans up, as a preparation the shutdown did
    /// not wait for is refused when it finishes. A reservation a close has frozen is still
    /// admitted — its rollout exists and belongs to the subtree being closed — but closed, with its
    /// run scope cancelled, and the close that froze it stops it; the spawn is then refused.
    fn commit(&self, reservation: &Arc<Reservation>, node: &Arc<AgentNode>) -> Commit {
        let frozen = {
            let mut agents = lock(&self.agents);
            if self.shut_down.load(Ordering::Acquire) {
                return Commit::Refused(AgentControlError::Unavailable);
            }
            let mut reservations = lock(&self.reservations);
            reservations.remove(&reservation.path);
            agents.insert(node.path.clone(), Arc::clone(node));
            if self.resident_limits().is_some() {
                lock(&self.closed_history).remove(&reservation.session_id);
            }
            let frozen = reservation.frozen.load(Ordering::Acquire);
            if frozen {
                node.closed.store(true, Ordering::Release);
            }
            frozen
        };
        if frozen && let Some(child) = &node.child {
            child.scope.cancel(CancelReason::Shutdown);
        }
        reservation
            .state
            .send_replace(ReservationState::Admitted(Arc::clone(node)));
        if frozen {
            Commit::AdmittedClosed(closing_error(&node.path))
        } else {
            Commit::Admitted
        }
    }

    /// Cleans up after a spawn that ended before its agent was admitted — refused by the factory or
    /// the store, failed, or cancelled — as Codex's `PendingSpawn` cleans up a cancelled child:
    /// the recorder is discarded, then the resources are released, then the path is freed. A
    /// recorder that cannot be discarded keeps the reservation, holding it, for a later close to
    /// try again.
    async fn settle_reservation(
        &self,
        reservation: &Arc<Reservation>,
        recorder: Option<Arc<dyn RolloutRecorder>>,
        resources: Option<Arc<dyn SpawnedAgentResources>>,
    ) {
        if let Some(resources) = resources {
            resources.shutdown().await;
        }
        if let Some(recorder) = recorder {
            if let Err(error) = recorder.discard().await {
                tracing::warn!(
                    path = %reservation.path,
                    %error,
                    "a spawn that never started could not discard its rollout; closing it again \
                     retries"
                );
                *lock(&reservation.undiscarded) = Some(recorder);
                reservation
                    .state
                    .send_replace(ReservationState::CleanupFailed(error.to_string()));
                return;
            }
            self.free_reservation(reservation);
            reservation.state.send_replace(ReservationState::Discarded);
        } else {
            self.free_reservation(reservation);
            reservation.state.send_replace(ReservationState::Released);
        }
    }

    /// Tries again to discard the rollout of a spawn whose cleanup failed. Every caller that
    /// arrives while a retry runs waits for that one and sees its outcome, so none of them reports
    /// the path free before the rollout was actually discarded. The retry runs as a task of the
    /// tree, so a caller that stops waiting does not leave the recorder taken.
    async fn retry_reservation(self: &Arc<Self>, reservation: &Arc<Reservation>) {
        let retry = {
            let mut retrying = lock(&reservation.retrying);
            if let Some(retry) = retrying.as_ref().filter(|retry| retry.peek().is_none()) {
                retry.clone()
            } else {
                let tree = Arc::clone(self);
                let target = Arc::clone(reservation);
                let task = tokio::spawn(async move {
                    let recorder = lock(&target.undiscarded).take();
                    if recorder.is_some() {
                        tree.settle_reservation(&target, recorder, None).await;
                    }
                });
                let retry = async move {
                    drop(task.await);
                }
                .boxed()
                .shared();
                *retrying = Some(retry.clone());
                retry
            }
        };
        retry.await;
    }

    fn free_reservation(&self, reservation: &Arc<Reservation>) {
        let mut reservations = lock(&self.reservations);
        if reservations
            .get(&reservation.path)
            .is_some_and(|current| Arc::ptr_eq(current, reservation))
        {
            reservations.remove(&reservation.path);
        }
    }

    /// Keeps `task` as one of the tree's, which shutting the tree down waits for.
    fn keep_task(&self, task: JoinHandle<()>) {
        let mut tasks = lock(&self.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// The authoritative sequence of `allocator`'s run, registering `allocator` as that sequence
    /// when the run has none yet.
    fn share_event_seqs(&self, allocator: &EventSeqAllocator) -> EventSeqAllocator {
        let mut seqs = lock(&self.event_seqs);
        let shared = seqs
            .entry(allocator.run_id().clone())
            .or_insert_with(|| allocator.clone());
        shared.advance_past(allocator);
        shared.clone()
    }

    /// The sequence a run starting with `allocator` draws from: the registered one, raised past
    /// `allocator`, when an earlier segment of the run registered it; otherwise `allocator` itself.
    fn adopt_event_seqs(&self, allocator: EventSeqAllocator) -> EventSeqAllocator {
        match lock(&self.event_seqs).get(allocator.run_id()) {
            Some(shared) => {
                shared.advance_past(&allocator);
                shared.clone()
            }
            None => allocator,
        }
    }

    /// An emitter attributed to `parent` — its run and the agent of its current turn — drawing
    /// from the run's authoritative sequence, when the run has an event sink.
    fn origin_of(&self, parent: &ParentRun) -> Option<HostEventEmitter> {
        let sink = parent.services().event_sink()?;
        Some(HostEventEmitter::new(
            parent.current_agent().id().clone(),
            self.share_event_seqs(parent.event_seqs()),
            Arc::clone(sink),
        ))
    }

    fn node(&self, path: &AgentPath) -> Option<Arc<AgentNode>> {
        lock(&self.agents).get(path).map(Arc::clone)
    }

    fn node_of_session(&self, session_id: &SessionId) -> Option<Arc<AgentNode>> {
        lock(&self.agents)
            .values()
            .find(|node| !node.path.is_root() && node.session_id.get() == Some(session_id))
            .map(Arc::clone)
    }

    /// Codex's `exceeds_thread_spawn_depth_limit` for an agent `caller` would spawn.
    fn spawn_depth_exceeded(&self, caller: &AgentPath) -> bool {
        depth(caller).saturating_add(1) > self.max_depth.load(Ordering::Acquire)
            || self.resident_limits().is_some_and(|limits| {
                let base = self.root_lineage.get().map_or(0, |lineage| lineage.depth);
                let local = u32::try_from(depth(caller)).unwrap_or(u32::MAX);
                let next = base.saturating_add(local).saturating_add(1);
                i32::try_from(next).unwrap_or(i32::MAX) > limits.max_depth
            })
    }

    fn resolve(
        &self,
        caller: &AgentPath,
        target: &str,
    ) -> Result<Arc<AgentNode>, AgentControlError> {
        let path = caller
            .resolve(target)
            .map_err(AgentControlError::Unsupported)?;
        self.node(&path).ok_or_else(|| {
            AgentControlError::Unsupported(format!("live agent path `{path}` not found"))
        })
    }

    fn list(&self, prefix: Option<&AgentPath>) -> Vec<LiveAgent> {
        // A `BTreeMap` keyed by path already lists `/root` first and the rest in path order.
        lock(&self.agents)
            .values()
            .filter(|node| prefix.is_none_or(|prefix| node.path.starts_with(prefix)))
            .map(|node| node.snapshot())
            .collect()
    }

    /// Closes the live agents below `node`, and `node` itself when `including_node` is set and it is
    /// not the root, and returns a receiver that resolves once the close is finished, with the error
    /// of an agent whose rollout could not be shut down.
    ///
    /// Ported from Codex's `shutdown_agent_tree`: the descendants are collected before anything
    /// stops, then the agent and each descendant in depth-first order, children by path, is
    /// waited for, has its rollout shut down, and is removed. Two things differ from Codex, both
    /// so that a close cannot be outgrown or abandoned:
    ///
    /// - Everything that stops a close from being outgrown or abandoned happens here, before the
    ///   first wait: every target is marked closed and has its run cancelled, and `node` refuses
    ///   spawns until the close is finished. The marking happens under the lock spawns take, so
    ///   the subtree cannot grow while it is being taken down — not even below the root, which
    ///   stays open.
    /// - The waiting and removal run as a task of the tree rather than in the caller, so a caller
    ///   that stops waiting — a timeout, a `select!` — does not leave closed agents behind.
    ///   [`AgentControl::shutdown`] waits for that task like any other.
    fn close_subtree(self: &Arc<Self>, node: &Arc<AgentNode>, including_node: bool) -> AgentClose {
        let mut targets = self.freeze_subtree(node, including_node);
        // Segment-wise order is a depth-first walk that visits siblings by name, which is the
        // order Codex's walk produces; plain string order would put `a-b` before `a/b`.
        targets.sort_by(|left, right| {
            left.path()
                .as_str()
                .split('/')
                .cmp(right.path().as_str().split('/'))
        });
        let closing = ClosingSubtree::new(Arc::clone(node));
        let target = including_node.then(|| node.path.clone());
        self.spawn_close(targets, target, Some(closing))
    }

    /// Marks every agent below `node` — and `node` itself with `including_node`, unless it is the
    /// root — closed, and every reservation below it frozen, under the lock spawns take, then
    /// cancels the agents' runs.
    fn freeze_subtree(&self, node: &Arc<AgentNode>, including_node: bool) -> Vec<CloseTarget> {
        let targets = {
            let agents = lock(&self.agents);
            // The paths below `node` are its subtree only while `node` is the agent at its path: once
            // it has gone, a later agent spawned at the same path is another instance with a
            // subtree of its own, which this close must not reach.
            if !node.path.is_root()
                && !agents
                    .get(&node.path)
                    .is_some_and(|current| Arc::ptr_eq(current, node))
            {
                return Vec::new();
            }
            let reservations = lock(&self.reservations);
            let below = |path: &AgentPath| {
                path.starts_with(&node.path)
                    && !path.is_root()
                    && (including_node || *path != node.path)
            };
            let mut targets: Vec<CloseTarget> = agents
                .values()
                .filter(|other| below(&other.path))
                .map(|agent| {
                    agent.closed.store(true, Ordering::Release);
                    CloseTarget::Agent(Arc::clone(agent))
                })
                .collect();
            targets.extend(
                reservations
                    .values()
                    .filter(|reservation| below(&reservation.path))
                    .map(CloseTarget::freeze),
            );
            targets
        };
        for target in &targets {
            if let CloseTarget::Agent(agent) = target
                && let Some(child) = &agent.child
            {
                child.scope.cancel(CancelReason::Shutdown);
            }
        }
        targets
    }

    /// Stops `targets` in order as a task of the tree, and reports how each ended.
    ///
    /// As Codex's `shutdown_agent_tree`: the agent named `target` is followed by its descendants
    /// even when it fails, while a descendant's failure ends the close there. Whatever is left
    /// stays in the tree, closed, for a later close to finish.
    fn spawn_close(
        self: &Arc<Self>,
        targets: Vec<CloseTarget>,
        target: Option<AgentPath>,
        closing: Option<ClosingSubtree>,
    ) -> AgentClose {
        let frozen = targets.iter().map(CloseTarget::instance).collect();
        let tree = Arc::clone(self);
        let (done, finished) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut report = AgentCloseReport::default();
            let mut remaining = targets.into_iter();
            while let Some(next) = remaining.next() {
                let instance = next.instance();
                let is_target = target.as_ref() == Some(next.path());
                match tree.close_target(next).await {
                    Ok(()) => report.closed.push(instance),
                    Err(error) => {
                        report.failed.push((instance, error));
                        if !is_target {
                            report.failed.extend(remaining.by_ref().map(|left| {
                                (
                                    left.instance(),
                                    "not reached: an earlier agent of the close failed".to_owned(),
                                )
                            }));
                        }
                    }
                }
            }
            drop(closing);
            // Nobody may be waiting any more; the close is done either way.
            let _ = done.send(report);
        });
        self.keep_task(task);
        AgentClose { frozen, finished }
    }

    async fn close_target(
        self: &Arc<Self>,
        target: CloseTarget,
    ) -> std::result::Result<(), String> {
        match target {
            CloseTarget::Agent(agent) => self.stop_once(&agent).await,
            CloseTarget::Reservation {
                reservation,
                failed_before,
            } => {
                if failed_before {
                    self.retry_reservation(&reservation).await;
                }
                match reservation.settled().await {
                    ReservationState::Admitted(agent) => self.stop_once(&agent).await,
                    ReservationState::Released
                    | ReservationState::Discarded
                    | ReservationState::Pending => Ok(()),
                    ReservationState::CleanupFailed(error) => Err(format!(
                        "the spawn of `{}` could not discard its rollout: {error}; close it again \
                         to retry",
                        reservation.path
                    )),
                }
            }
        }
    }

    /// Stops `node` once: a close that reaches an agent whose stop is in progress waits for it and
    /// shares its result, and only a stop that has failed is attempted again. The stop runs as a
    /// task of the tree, so no caller's cancellation abandons it.
    async fn stop_once(self: &Arc<Self>, node: &Arc<AgentNode>) -> std::result::Result<(), String> {
        let stopping = {
            let mut stopping = lock(&node.stopping);
            let reusable = stopping
                .as_ref()
                .filter(|stop| stop.peek().is_none_or(std::result::Result::is_ok));
            if let Some(stop) = reusable {
                stop.clone()
            } else {
                let tree = Arc::clone(self);
                let agent = Arc::clone(node);
                let task = tokio::spawn(async move {
                    tree.stop(&agent).await.map_err(|error| error.to_string())
                });
                let stop = async move {
                    task.await
                        .unwrap_or_else(|error| Err(format!("the agent's stop failed: {error}")))
                }
                .boxed()
                .shared();
                *stopping = Some(stop.clone());
                stop
            }
        };
        stopping.await
    }

    /// Waits for a closed agent's cancelled run to end, shuts down its rollout, and removes the
    /// agent from the tree.
    ///
    /// As Codex's `shutdown_live_agent`, whose durability barrier fails before the thread is
    /// removed: a rollout that cannot be shut down keeps the agent in the tree, closed and with its
    /// recorder, so closing it again — or shutting the tree down — tries again.
    async fn stop(&self, node: &Arc<AgentNode>) -> Result<(), AgentControlError> {
        if let Some(child) = &node.child {
            // Passing the state lock pairs with the check in `ensure_running`: any run claimed
            // before the agent was marked closed has cleared `idle` by now, and none is claimed
            // after.
            drop(lock(&child.state));
            let mut idle = child.idle.subscribe();
            // The sender lives in the node held here, so the wait cannot fail.
            drop(idle.wait_for(|idle| *idle).await);
        }
        node.shut_down_rollout().await.map_err(|error| {
            AgentControlError::Unsupported(format!(
                "the rollout of agent `{}` could not be shut down: {error}; close it again to retry",
                node.path
            ))
        })?;
        node.release_resources().await;
        if self.resident_limits().is_some() && !self.shut_down.load(Ordering::Acquire) {
            self.notify_legacy_completion(node, &AgentStatus::Shutdown);
        }
        node.status.send_replace(AgentStatus::Shutdown);
        let mut agents = lock(&self.agents);
        if agents
            .get(&node.path)
            .is_some_and(|current| Arc::ptr_eq(current, node))
        {
            if self.resident_limits().is_some()
                && self.rollout_store.get().is_none()
                && let (Some(id), Some(child)) = (node.session_id.get(), node.child.as_ref())
            {
                let state = lock(&child.state);
                lock(&self.closed_history).insert(
                    id.clone(),
                    legacy::ClosedAgent {
                        path: node.path.clone(),
                        agent_type: child.environment.agent_type.clone(),
                        history: state.history.clone(),
                        explicitly_closed: node.explicitly_closed.load(Ordering::Acquire),
                    },
                );
            }
            agents.remove(&node.path);
        }
        Ok(())
    }

    /// Starts a run on `node` if it is idle and its mailbox asks for one.
    fn ensure_running(self: &Arc<Self>, node: &Arc<AgentNode>) {
        let Some(child) = &node.child else {
            return;
        };
        if self.shut_down.load(Ordering::Acquire) {
            return;
        }
        let scope = {
            let mut state = lock(&child.state);
            // Checked under the state lock too, which a close takes before it waits for the agent
            // to go idle: a run claimed before the close marked the agent is waited for, and none
            // is claimed after.
            if self.shut_down.load(Ordering::Acquire)
                || state.current_run.is_some()
                || node.is_closed()
                || !node.mailbox.has_trigger()
            {
                return;
            }
            let scope = child.scope.child(ScopeKind::Run);
            state.current_run = Some(scope.clone());
            child.idle.send_replace(false);
            if self.resident_limits().is_some() {
                node.status.send_replace(AgentStatus::PendingInit);
            }
            scope
        };
        let task = tokio::spawn(crate::runner::with_dispatch(
            Box::pin(drive(Arc::downgrade(self), Arc::clone(node), scope)),
            child.environment.dispatch.clone(),
        ));
        let mut tasks = lock(&self.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.cancel.cancel(CancelReason::Shutdown);
    }
}

/// Runs an agent until its mailbox no longer asks for another run.
///
/// The run scope is claimed by whoever starts the driver, under the same lock that checks for a
/// trigger, and handed back under it here; a follow-up that arrives while this driver is finishing
/// is therefore either seen below or starts a new driver.
async fn drive(tree: Weak<Tree>, node: Arc<AgentNode>, mut scope: CancelScope) {
    let Some(child) = &node.child else {
        return;
    };
    loop {
        let Some(strong) = tree.upgrade() else {
            lock(&child.state).current_run = None;
            child.idle.send_replace(true);
            break;
        };
        let resident = strong.resident_limits().is_some();
        let guard = strong.limiter.admit();
        let handle = AgentHandle::new(&strong, Arc::clone(&node));
        drop(strong);

        let (mail, parent_run_id, mut input, recorded) = {
            let mut state = lock(&child.state);
            // An interrupt can reach the claimed scope before the driver takes its input. Give
            // queued replacement input a fresh scope, keeping it out of the abandoned run.
            if resident
                && scope.is_cancelled()
                && !child.scope.is_cancelled()
                && node.mailbox.has_trigger()
            {
                scope = child.scope.child(ScopeKind::Run);
                state.current_run = Some(scope.clone());
            }
            let (mail, parent_run_id, origin) = node.mailbox.take_for_run();
            state.run_origin = origin;
            (mail, parent_run_id, state.history.clone(), state.recorded)
        };
        let history_len = input.len();
        input.extend(
            mail.iter()
                .map(|mail| ModelInputItem::Message(mail.to_message())),
        );
        let (history, started) = run_to_end(
            &tree,
            child,
            &handle,
            &scope,
            FreshRun {
                input,
                recorded,
                history_len,
                parent_run_id,
            },
        )
        .await;
        drop(guard);
        child.running_run.send_replace(None);

        let mut state = lock(&child.state);
        state.run_context = None;
        let pending = std::mem::take(&mut state.pending_context);
        // A run that started recorded its whole input, and its history follows from what it
        // recorded; one that never started recorded nothing, and its input is still to be.
        if started {
            state.recorded = history.len();
        }
        state.history = history;
        if !pending.is_empty() {
            append_context(child, &mut state, pending);
        }
        let shut_down = tree
            .upgrade()
            .is_none_or(|tree| tree.shut_down.load(Ordering::Acquire));
        if shut_down || node.is_closed() || !node.mailbox.has_trigger() {
            state.current_run = None;
            child.idle.send_replace(true);
            break;
        }
        scope = child.scope.child(ScopeKind::Run);
        state.current_run = Some(scope.clone());
        if resident {
            node.status.send_replace(AgentStatus::PendingInit);
        }
    }
}

/// Runs one run of an agent to its end — across any number of pauses for approval — and returns
/// the agent's history after it, and whether the run started at all.
///
/// A run that stops to ask for approval is kept as it is: its checkpoint waits here, with the run's
/// scope and execution slot still held, until the host answers through [`AgentControl::resume`] and
/// the same run continues from it. Codex likewise keeps a thread's turn active while an approval
/// is pending. An interrupt or shutdown in the meantime ends the run instead.
async fn run_to_end(
    tree: &Weak<Tree>,
    child: &ChildRuntime,
    handle: &AgentHandle,
    scope: &CancelScope,
    fresh: FreshRun,
) -> (Vec<ModelInputItem>, bool) {
    let mut fallback = fresh.input.clone();
    let mut start = RunStart::Fresh(fresh);
    // The lane the run reads injected context from; see below for how a pause moves it.
    let mut hook_context = ra_core::hook::HookContextQueue::default();
    loop {
        let (state, history, event_seqs) = match run_once(
            child,
            handle.clone(),
            scope.clone(),
            start,
            fallback,
            &hook_context,
        )
        .await
        {
            RunEnd::NotStarted(history) => return (history, false),
            RunEnd::Finished(history) => return (history, true),
            RunEnd::Paused {
                state,
                history,
                event_seqs,
            } => (state, history, event_seqs),
        };
        let run_id = state.run_id().clone();
        #[cfg(feature = "test-api")]
        if let Some(before_pause) = tree
            .upgrade()
            .and_then(|tree| tree.before_pause.get().cloned())
        {
            before_pause();
        }
        // The context the checkpoint already holds, which a cancelled run writes to its history.
        let context_on_cancel = state.as_ref().clone().take_hook_context();
        let (resume, answered) = oneshot::channel();
        park_paused_run(
            child,
            &mut hook_context,
            PausedRun {
                state: *state,
                resume,
                event_seqs,
            },
        );
        if let Some(tree) = tree.upgrade() {
            tree.paused_activity
                .send_modify(|count| *count = count.wrapping_add(1));
        }
        let answered = tokio::select! {
            answered = answered => answered.ok(),
            () = scope.cancelled() => None,
        };
        lock(&child.state).paused = None;
        if let Some(tree) = tree.upgrade() {
            tree.paused_activity
                .send_modify(|count| *count = count.wrapping_add(1));
        }
        let Some(state) = answered else {
            let mut history = history;
            record_undelivered_context(
                tree,
                child.environment.rollout.as_ref(),
                &mut history,
                context_on_cancel,
                hook_context.close_entries(),
            );
            if let Err(error) = scope.ensure_not_cancelled() {
                // The run ends here rather than in the runner, whose last segment ended paused: as
                // Codex aborts a turn that is waiting for approval, it ends cancelled, after the
                // marker its interrupt records.
                let marker = scope
                    .reason()
                    .is_some_and(|reason| is_interrupt(&reason))
                    .then(|| {
                        InterruptedTurnHistoryMarker::from_settings(
                            child.environment.config.interrupt_message(),
                            true,
                        )
                        .item()
                    })
                    .flatten();
                if let Some(input) = marker.as_ref().and_then(RunItem::to_model_input) {
                    history.push(input);
                }
                if let Some(rollout) = &child.environment.rollout {
                    record_cancelled(rollout.as_ref(), run_id.clone(), marker).await;
                }
                handle.run_ended(&Err(error)).await;
                if let Some(resources) = &child.environment.resources {
                    resources.run_ended(&run_id);
                }
            }
            return (history, true);
        };
        fallback = history;
        start = RunStart::Resume(Box::new(state));
    }
}

/// Keeps `paused` with the agent until the host answers it, as the thread parks a paused turn.
///
/// The runner closed the run's lane when the run stopped, so the agent moves to a fresh one, which
/// the continued run reads: context injected while the host answers is neither lost nor added to
/// the checkpoint handed out. Context that arrived after the runner closed the old lane went to the
/// agent's pending context, which the tree reads only once the whole run ends: it moves to the new
/// lane as well, so the continued run reads it too.
fn park_paused_run(
    child: &ChildRuntime,
    hook_context: &mut ra_core::hook::HookContextQueue,
    paused: PausedRun,
) {
    let mut child_state = lock(&child.state);
    if hook_context.is_closed() {
        *hook_context = ra_core::hook::HookContextQueue::default();
    }
    let arrived = std::mem::take(&mut child_state.pending_context);
    if !arrived.is_empty()
        && let Err(rejected) = hook_context.push(arrived)
    {
        child_state.pending_context = rejected;
    }
    child_state.run_context = Some(hook_context.clone());
    child_state.paused = Some(paused);
}

/// Writes the context a run took in but never delivered to its history and rollout — a run waiting
/// for approval that is cancelled instead of continued, or a run that fails or is cancelled before
/// its next step: what its checkpoint held, then what its lane received. As the thread does for
/// such a turn, background context is dropped once the tree is shutting down.
fn record_undelivered_context(
    tree: &Weak<Tree>,
    rollout: Option<&Arc<dyn RolloutRecorder>>,
    history: &mut Vec<ModelInputItem>,
    checkpointed: Vec<Message>,
    queued: (
        Vec<(Message, ra_core::hook::HookContextOrigin)>,
        Vec<String>,
    ),
) {
    let shutting_down = tree
        .upgrade()
        .is_none_or(|tree| tree.shut_down.load(Ordering::Acquire));
    let queued = queued.0.into_iter().filter_map(|(message, origin)| {
        (!shutting_down || origin == ra_core::hook::HookContextOrigin::Synchronous)
            .then_some(message)
    });
    for message in checkpointed.into_iter().chain(queued) {
        if let Some(rollout) = rollout {
            rollout.record(RolloutItem::Item(RunItem::new(
                ra_core::item::ItemId::new(uuid::Uuid::now_v7().to_string()),
                ra_core::item::RunItemKind::Message(message.clone()),
            )));
        }
        history.push(ModelInputItem::Message(message));
    }
}

/// Records that a run waiting for approval was cancelled, after its interrupted-run marker if it
/// has one, and waits for the rollout to write it.
async fn record_cancelled(rollout: &dyn RolloutRecorder, run_id: RunId, marker: Option<RunItem>) {
    if let Some(marker) = marker {
        rollout.record(RolloutItem::Item(marker));
    }
    rollout.record(RolloutItem::RunEnded(RolloutRunEnded::new(
        run_id.clone(),
        RolloutRunEnd::Cancelled,
    )));
    if let Err(error) = rollout.flush().await {
        tracing::warn!(run_id = %run_id, %error, "the agent's rollout could not be flushed");
    }
}

/// How a run starts: on the agent's history and its mail, of which the agent's rollout already
/// holds the first `recorded` items, or from a checkpoint the host has answered.
enum RunStart {
    Fresh(FreshRun),
    Resume(Box<RunState>),
}

struct FreshRun {
    input: Vec<ModelInputItem>,
    recorded: usize,
    /// Where the mail that triggered this turn begins.
    history_len: usize,
    /// The run every trigger mail it starts on came from, if they agree on one.
    parent_run_id: Option<RunId>,
}

impl RunStart {
    fn request(
        self,
        environment: &ChildEnvironment,
        run_id: RunId,
        scope: CancelScope,
    ) -> Result<RunRequest> {
        match self {
            Self::Fresh(FreshRun {
                input,
                recorded,
                parent_run_id,
                ..
            }) => {
                let request = RunRequest::new(
                    AgentBinding::direct(Arc::clone(&environment.agent)),
                    Arc::clone(&environment.model_resolver),
                    run_id,
                    scope,
                    input,
                )
                .with_recorded_input(recorded);
                // Every run of a spawned agent has a parent, which makes it a sub-agent run to its
                // hooks: Codex runs `SubagentStop` for every turn of a spawned thread, whether or not
                // the turn has a parent turn. The run whose trigger mail started this one is that
                // parent; when the mail names no single run — the host sent it, or several runs did —
                // the run that spawned the agent is.
                request.with_parent_run_id(
                    parent_run_id.unwrap_or_else(|| environment.spawned_by.clone()),
                )
            }
            // The checkpoint carries the parent and the history; empty input projects it.
            Self::Resume(state) => Ok(RunRequest::new(
                AgentBinding::direct(Arc::clone(&environment.agent)),
                Arc::clone(&environment.model_resolver),
                run_id,
                scope,
                Vec::new(),
            )
            .with_state(*state)),
        }
    }
}

/// How a run ended: done with the agent's new history, paused for approval, or never started.
enum RunEnd {
    NotStarted(Vec<ModelInputItem>),
    Finished(Vec<ModelInputItem>),
    Paused {
        state: Box<RunState>,
        history: Vec<ModelInputItem>,
        event_seqs: EventSeqAllocator,
    },
}

/// Runs one segment of an agent's run. `fallback` is the history to keep if the segment returns no
/// result.
async fn run_once(
    child: &ChildRuntime,
    handle: AgentHandle,
    scope: CancelScope,
    start: RunStart,
    fallback: Vec<ModelInputItem>,
    hook_context: &ra_core::hook::HookContextQueue,
) -> RunEnd {
    let environment = &child.environment;
    let run_id = match &start {
        RunStart::Fresh(_) => RunId::generate(),
        RunStart::Resume(state) => state.run_id().clone(),
    };
    let lane = hook_context;
    let hook_context = hook_context.clone();
    child.running_run.send_replace(Some(run_id.clone()));
    lock(&child.state).run_context = Some(hook_context.clone());
    let history_len = match &start {
        RunStart::Fresh(fresh) => Some(fresh.history_len),
        RunStart::Resume(_) => None,
    };
    let history = Arc::new(SpawnedRunHistory::new(
        history_len,
        lock(&child.state).reported_tokens,
    ));
    let request = start.request(environment, run_id.clone(), scope);
    let mut request = match request {
        Ok(request) => request
            .with_config(environment.config.clone())
            .with_services(environment.services.clone().with_hook_context(hook_context))
            .with_agent_control(handle.clone())
            .leaving_failed_context_queued(),
        Err(error) => {
            handle.run_finished(&Err(error));
            return RunEnd::NotStarted(fallback);
        }
    };
    if let Some(app_context) = &environment.app_context {
        request = request.with_app_context(Arc::clone(app_context));
    }
    if let Some(rollout) = &environment.rollout {
        request = request.with_rollout_recorder(Arc::clone(rollout));
    }
    request = request.with_owner(RunOwner::Spawned(Arc::clone(&history)));

    let event_seqs = handle.tree().map_or_else(
        |_| request.event_seq_allocator().clone(),
        |tree| tree.share_event_seqs(request.event_seq_allocator()),
    );
    let mut stream = Runner::run_streamed(request);
    let mut produced = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::Item(item) = event {
            produced.push(item);
        }
    }
    let result = stream.finish().await;
    lock(&child.state).reported_tokens = history.reported_tokens();
    let end = match result {
        Ok(result) if !result.outcome().interruptions().is_empty() => {
            return RunEnd::Paused {
                history: result.continuation_input(ContinuationInput::Normalized),
                state: Box::new(result.state().clone()),
                event_seqs,
            };
        }
        Ok(result) => RunEnd::Finished(result.continuation_input(ContinuationInput::Normalized)),
        Err(error) => {
            // What the run took in and had not delivered is left in its lane, sealed, with where
            // each entry came from; a checkpoint its error carries may hold context from before
            // the run continued. As the thread does with a failed turn, both join the history the
            // agent goes on from, after what the run settled, and its rollout.
            let mut history = history.recover(fallback, &produced);
            let checkpointed = error
                .run_state()
                .map_or_else(Vec::new, |state| state.clone().take_hook_context());
            record_undelivered_context(
                &handle.tree,
                environment.rollout.as_ref(),
                &mut history,
                checkpointed,
                lane.close_entries(),
            );
            RunEnd::Finished(history)
        }
    };
    if let Some(resources) = &environment.resources {
        resources.run_ended(&run_id);
    }
    end
}

struct AgentNode {
    path: AgentPath,
    /// Which agent of its session this is; see [`AgentInstance`].
    instance: u64,
    /// The stop in progress, shared by every close that reaches the agent while it runs; a stop
    /// that failed is replaced by the next close's.
    stopping: Mutex<Option<SharedStop>>,
    /// The session of the agent's own rollout; the root's once the tree is given a store.
    session_id: OnceLock<SessionId>,
    agent_id: Mutex<Option<AgentId>>,
    status: watch::Sender<AgentStatus>,
    mailbox: Mailbox,
    /// Set once the agent is being closed; it then starts no run and accepts nothing more.
    closed: AtomicBool,
    explicitly_closed: AtomicBool,
    /// How many closes of the agents below this one are in progress; it spawns nothing meanwhile.
    closing_subtree: AtomicUsize,
    /// Absent for the root, whose runs the host starts.
    child: Option<ChildRuntime>,
    /// Set once the agent's rollout has been shut down.
    rollout_shut_down: AtomicBool,
    legacy_completion_sent: AtomicBool,
    /// Shared completion of the agent's resource cleanup, independent of any caller's wait.
    resources_released: OnceLock<Shared<BoxFuture<'static, ()>>>,
    /// How many thresholds of the tree's rollout budget the agent has been told about.
    budget_reminder_delivered: Mutex<Option<usize>>,
}

impl AgentNode {
    fn new(path: AgentPath, agent_id: Option<AgentId>, child: Option<ChildRuntime>) -> Self {
        Self::with_instance(path, agent_id, child, next_instance())
    }

    fn with_instance(
        path: AgentPath,
        agent_id: Option<AgentId>,
        child: Option<ChildRuntime>,
        instance: u64,
    ) -> Self {
        Self {
            path,
            instance,
            stopping: Mutex::new(None),
            session_id: OnceLock::new(),
            agent_id: Mutex::new(agent_id),
            status: watch::Sender::new(AgentStatus::PendingInit),
            mailbox: Mailbox::new(),
            closed: AtomicBool::new(false),
            explicitly_closed: AtomicBool::new(false),
            closing_subtree: AtomicUsize::new(0),
            child,
            rollout_shut_down: AtomicBool::new(false),
            legacy_completion_sent: AtomicBool::new(false),
            resources_released: OnceLock::new(),
            budget_reminder_delivered: Mutex::new(None),
        }
    }

    fn status(&self) -> AgentStatus {
        self.status.borrow().clone()
    }

    fn instance(&self) -> AgentInstance {
        AgentInstance {
            session_id: self
                .session_id
                .get()
                .cloned()
                .unwrap_or_else(|| SessionId::new(self.path.as_str())),
            instance: self.instance,
        }
    }

    fn inject_context(&self, messages: Vec<Message>) -> bool {
        let Some(child) = &self.child else {
            return false;
        };
        let mut state = lock(&child.state);
        let mut messages = messages;
        // A run waiting for approval uses the fresh context lane of its continuation.
        if let Some(queue) = &state.run_context {
            match queue.push(messages) {
                Ok(()) => return true,
                Err(rejected) => messages = rejected,
            }
        }
        if state.current_run.is_some() {
            // The finishing run will write its history back before we append this context.
            state.pending_context.extend(messages);
        } else {
            append_context(child, &mut state, messages);
        }
        true
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn is_closing_subtree(&self) -> bool {
        self.closing_subtree.load(Ordering::Acquire) > 0
    }

    fn is_running(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|child| lock(&child.state).current_run.is_some())
    }

    /// Writes what the agent's rollout still holds and lets go of it, once the agent's runs have
    /// stopped: Codex's session shutdown of the thread's live writer. The rollout can then be
    /// resumed elsewhere. A rollout already shut down is not shut down again; one that could not
    /// be keeps what it holds, and the next call tries again.
    async fn shut_down_rollout(&self) -> Result<()> {
        let Some(rollout) = self
            .child
            .as_ref()
            .and_then(|child| child.environment.rollout.as_ref())
        else {
            return Ok(());
        };
        if self.rollout_shut_down.load(Ordering::Acquire) {
            return Ok(());
        }
        // Codex's `shutdown_live_agent` materializes and flushes the thread before shutting it
        // down, so an agent that recorded nothing still leaves its rollout behind.
        match rollout.shutdown_materialized().await {
            Ok(()) => {
                self.rollout_shut_down.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.path,
                    %error,
                    "the agent's rollout could not be shut down"
                );
                Err(error)
            }
        }
    }

    /// Releases what the tree's spawned-agent factory gave the agent, once its runs have stopped:
    /// Codex's session shutdown of the thread's own processes. Released at most once.
    async fn release_resources(&self) {
        let resources = self
            .child
            .as_ref()
            .and_then(|child| child.environment.resources.as_ref());
        if let Some(resources) = resources {
            let finished = self.resources_released.get_or_init(|| {
                let resources = Arc::clone(resources);
                let task = tokio::spawn(async move { resources.shutdown().await });
                async move {
                    if let Err(error) = task.await {
                        tracing::error!(%error, "a spawned agent's resource cleanup failed");
                    }
                }
                .boxed()
                .shared()
            });
            finished.clone().await;
        }
    }

    fn snapshot(&self) -> LiveAgent {
        let agent = LiveAgent::new(
            self.path.clone(),
            lock(&self.agent_id).clone(),
            self.status(),
        );
        match self.session_id.get() {
            Some(session_id) => agent.with_session_id(session_id.clone()),
            None => agent,
        }
    }
}

/// Holds an agent's subtree closed to new spawns for as long as a close of it is in progress.
/// A path a spawn holds before its agent exists: Codex's `SpawnReservation`.
struct Reservation {
    path: AgentPath,
    session_id: SessionId,
    instance: u64,
    /// Set by a close or the tree's shutdown that reached the reservation; the agent is then
    /// admitted closed.
    frozen: AtomicBool,
    state: watch::Sender<ReservationState>,
    /// The rollout of a spawn whose cleanup could not discard it, kept for a later close to retry.
    undiscarded: Mutex<Option<Arc<dyn RolloutRecorder>>>,
    /// The retry of that cleanup in progress, shared by everyone closing the reservation.
    retrying: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
}

impl Reservation {
    fn new(path: AgentPath, session_id: SessionId) -> Self {
        Self {
            path,
            session_id,
            instance: next_instance(),
            frozen: AtomicBool::new(false),
            state: watch::Sender::new(ReservationState::Pending),
            undiscarded: Mutex::new(None),
            retrying: Mutex::new(None),
        }
    }

    fn instance(&self) -> AgentInstance {
        AgentInstance {
            session_id: self.session_id.clone(),
            instance: self.instance,
        }
    }

    /// Waits until the spawn holding the reservation has admitted its agent or cleaned up.
    async fn settled(&self) -> ReservationState {
        let mut state = self.state.subscribe();
        let settled = state
            .wait_for(|state| !matches!(state, ReservationState::Pending))
            .await
            .map(|state| state.clone());
        // The sender lives in the reservation held here, so the wait cannot fail.
        settled.unwrap_or(ReservationState::Released)
    }
}

/// How a spawn's agent was taken into the tree.
enum Commit {
    Admitted,
    AdmittedClosed(AgentControlError),
    Refused(AgentControlError),
}

#[derive(Clone)]
enum ReservationState {
    /// The spawn is still preparing the agent.
    Pending,
    /// The agent was added to the tree.
    Admitted(Arc<AgentNode>),
    /// The spawn ended before anything of the agent's rollout existed.
    Released,
    /// The spawn ended after creating the agent's rollout, which was discarded.
    Discarded,
    /// The spawn ended and its rollout could not be discarded; the reservation still holds it.
    CleanupFailed(String),
}

/// Holds what a spawn has of its agent until the agent is admitted, and cleans it up if the spawn
/// ends first — returning an error, or dropped while it waits: Codex's `SpawnReservation` and
/// `PendingSpawn` in one, taking each part over as the spawn gets it, so nothing falls between
/// them.
struct SpawnGuard {
    tree: Weak<Tree>,
    reservation: Arc<Reservation>,
    resources: Option<Arc<dyn SpawnedAgentResources>>,
    recorder: Option<Arc<dyn RolloutRecorder>>,
    reopened_recorder: Option<Arc<dyn RolloutRecorder>>,
    armed: bool,
}

impl SpawnGuard {
    fn new(tree: &Arc<Tree>, reservation: Arc<Reservation>) -> Self {
        Self {
            tree: Arc::downgrade(tree),
            reservation,
            resources: None,
            recorder: None,
            reopened_recorder: None,
            armed: true,
        }
    }

    /// The agent was admitted: it holds everything from here on.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let reservation = Arc::clone(&self.reservation);
        let recorder = self.recorder.take();
        let reopened_recorder = self.reopened_recorder.take();
        let resources = self.resources.take();
        let Some(tree) = self.tree.upgrade() else {
            // The tree is gone, and with it every close that could wait for the reservation.
            reservation.state.send_replace(ReservationState::Released);
            return;
        };
        // The cleanup belongs to the tree, as its closes do, so the caller's cancellation cannot
        // abandon it, and shutting the tree down waits for it.
        let cleanup = Arc::clone(&tree);
        let task = tokio::spawn(async move {
            // Releasing ownership of a reopened writer must preserve its existing history.
            if let Some(recorder) = reopened_recorder
                && let Err(error) = recorder.shutdown().await
            {
                tracing::warn!(%error, path = %reservation.path, "could not shut down a reopened rollout");
            }
            cleanup
                .settle_reservation(&reservation, recorder, resources)
                .await;
        });
        tree.keep_task(task);
    }
}

/// One instance of an agent: its session, and which agent with that session it is.
///
/// A session names one rollout, but a thread resumed after it was closed is another instance of
/// the same session; a close's result says which instance it is about, so it is not taken for
/// that of a later one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentInstance {
    session_id: SessionId,
    instance: u64,
}

impl AgentInstance {
    /// The agent's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// What a close froze, before it waits for anything, and the result once it has finished.
#[derive(Debug)]
pub struct AgentClose {
    frozen: Vec<AgentInstance>,
    finished: oneshot::Receiver<AgentCloseReport>,
}

impl AgentClose {
    /// Every agent the close stopped from spawning or running, including agents a spawn was still
    /// preparing, in the order they are closed.
    #[must_use]
    pub fn frozen(&self) -> &[AgentInstance] {
        &self.frozen
    }

    /// Waits for the close to finish. The close itself belongs to the tree: giving up the wait
    /// does not stop it, and its report can be awaited later.
    pub async fn finished(self) -> AgentCloseReport {
        self.finished.await.unwrap_or_else(|_| AgentCloseReport {
            closed: Vec::new(),
            failed: self
                .frozen
                .into_iter()
                .map(|agent| (agent, "the agent tree went away".to_owned()))
                .collect(),
        })
    }
}

/// How a close ended, agent by agent.
#[derive(Debug, Clone, Default)]
pub struct AgentCloseReport {
    closed: Vec<AgentInstance>,
    failed: Vec<(AgentInstance, String)>,
}

impl AgentCloseReport {
    /// The agents that were closed: their runs ended, their rollouts were written out and let go
    /// of, and they were removed from the tree. A spawn that had not admitted its agent counts once
    /// it has cleaned up.
    #[must_use]
    pub fn closed(&self) -> &[AgentInstance] {
        &self.closed
    }

    /// The agents that are not closed yet, with why: they stay in the tree, closed, holding their
    /// rollouts, and closing them again tries again.
    #[must_use]
    pub fn failed(&self) -> &[(AgentInstance, String)] {
        &self.failed
    }

    /// Whether every agent was closed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

fn next_instance() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The stop of one agent, shared by every close that waits for it.
type SharedStop = Shared<BoxFuture<'static, std::result::Result<(), String>>>;

/// One thing a close stops: an agent, or a spawn that holds its path but has not admitted its
/// agent yet.
enum CloseTarget {
    Agent(Arc<AgentNode>),
    Reservation {
        reservation: Arc<Reservation>,
        /// Whether its cleanup had already failed when the close reached it, so the close retries
        /// it rather than waiting for the spawn.
        failed_before: bool,
    },
}

impl CloseTarget {
    /// Freezes `reservation`: its agent will be admitted closed.
    fn freeze(reservation: &Arc<Reservation>) -> Self {
        reservation.frozen.store(true, Ordering::Release);
        Self::Reservation {
            reservation: Arc::clone(reservation),
            failed_before: matches!(
                *reservation.state.borrow(),
                ReservationState::CleanupFailed(_)
            ),
        }
    }

    fn path(&self) -> &AgentPath {
        match self {
            Self::Agent(agent) => &agent.path,
            Self::Reservation { reservation, .. } => &reservation.path,
        }
    }

    fn instance(&self) -> AgentInstance {
        match self {
            Self::Agent(agent) => agent.instance(),
            Self::Reservation { reservation, .. } => reservation.instance(),
        }
    }
}

struct ClosingSubtree(Arc<AgentNode>);

impl ClosingSubtree {
    fn new(node: Arc<AgentNode>) -> Self {
        node.closing_subtree.fetch_add(1, Ordering::AcqRel);
        Self(node)
    }
}

impl Drop for ClosingSubtree {
    fn drop(&mut self) {
        self.0.closing_subtree.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a spawned agent's runs start from, captured from the run that spawned it.
struct ChildEnvironment {
    agent_type: Option<AgentId>,
    dispatch: tracing::Dispatch,
    agent: Arc<AgentSpec>,
    model_resolver: Arc<dyn ModelResolver>,
    config: RunConfig,
    app_context: Option<Arc<dyn std::any::Any + Send + Sync>>,
    services: ToolServices,
    /// The run that spawned the agent: the parent of a run whose trigger mail names no single run.
    spawned_by: RunId,
    /// The agent's own rollout, when the tree records its agents.
    rollout: Option<Arc<dyn RolloutRecorder>>,
    /// What the tree's spawned-agent factory gave the agent to hold apart from the tree.
    resources: Option<Arc<dyn SpawnedAgentResources>>,
}

impl ChildEnvironment {
    fn from_parent(
        parent: &ParentRun,
        agent: Arc<AgentSpec>,
        agent_type: Option<AgentId>,
        config: RunConfig,
        rollout: Option<Arc<dyn RolloutRecorder>>,
        resources: Option<Arc<dyn SpawnedAgentResources>>,
    ) -> Self {
        Self {
            agent,
            agent_type,
            model_resolver: Arc::clone(parent.model_resolver()),
            config,
            app_context: parent.app_context().map(Arc::clone),
            services: parent.services().clone(),
            spawned_by: parent.run_id().clone(),
            rollout,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            resources,
        }
    }
}

struct ChildRuntime {
    environment: ChildEnvironment,
    /// Parent of the agent's run scopes, below the tree's.
    scope: CancelScope,
    state: Mutex<ChildState>,
    /// Whether no driver is running the agent: cleared when a run is claimed, set when the
    /// driver hands its last run back. A close waits on it.
    idle: watch::Sender<bool>,
    /// The run the agent is in, from its start until it ends — through any pause for approval.
    running_run: watch::Sender<Option<RunId>>,
}

impl ChildRuntime {
    /// An agent about to start its first run on `history`, none of which its rollout holds yet.
    fn new(
        environment: ChildEnvironment,
        scope: CancelScope,
        history: Vec<ModelInputItem>,
        reported_tokens: Option<u64>,
    ) -> Self {
        Self {
            environment,
            scope,
            state: Mutex::new(ChildState {
                history,
                recorded: 0,
                reported_tokens,
                current_run: None,
                paused: None,
                run_origin: None,
                run_context: None,
                pending_context: Vec::new(),
            }),
            idle: watch::Sender::new(true),
            running_run: watch::Sender::new(None),
        }
    }
}

struct ChildState {
    history: Vec<ModelInputItem>,
    /// How many leading items of `history` the agent's rollout holds: none of the history it was
    /// forked with, and all of it once a run has started on it.
    recorded: usize,
    /// The last sampling's usage, reset whenever the agent installs a replacement history.
    reported_tokens: Option<u64>,
    /// The scope of the run in progress, or of the one about to start.
    current_run: Option<CancelScope>,
    /// The run's checkpoint while it waits for the host to answer an approval.
    paused: Option<PausedRun>,
    /// The run whose spawn or follow-up started the current run, whose timeline records the run's
    /// completion.
    run_origin: Option<HostEventEmitter>,
    /// Where context for the run in progress goes, delivered at its next step.
    run_context: Option<ra_core::hook::HookContextQueue>,
    /// Context that arrived while the run was ending, after its queue closed: it joins the history
    /// once the run's own result is written back, as the thread's pending context does.
    pending_context: Vec<Message>,
}

struct PausedRun {
    state: RunState,
    resume: oneshot::Sender<RunState>,
    event_seqs: EventSeqAllocator,
}

/// An agent's pending mail, with a counter that ticks on every delivery for waiters.
struct Mailbox {
    queue: Mutex<VecDeque<Mail>>,
    activity: watch::Sender<u64>,
}

/// One message waiting in a mailbox, with the run that sent it when that run asked for a run of
/// the recipient: Codex's pending mail and the turn start options it was sent with.
struct Mail {
    communication: InterAgentCommunication,
    parent_run_id: Option<RunId>,
    origin: Option<HostEventEmitter>,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            activity: watch::Sender::new(0),
        }
    }

    fn push(&self, mail: InterAgentCommunication) {
        self.push_from(mail, None, None);
    }

    fn push_from(
        &self,
        communication: InterAgentCommunication,
        parent_run_id: Option<RunId>,
        origin: Option<HostEventEmitter>,
    ) {
        lock(&self.queue).push_back(Mail {
            communication,
            parent_run_id,
            origin,
        });
        self.activity
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    fn take_all(&self) -> Vec<InterAgentCommunication> {
        lock(&self.queue)
            .drain(..)
            .map(|mail| mail.communication)
            .collect()
    }

    /// Takes the mail a new run starts on, and the run that asked for it.
    ///
    /// As Codex's `drain_mailbox_input_items` keeps a parent turn only when every trigger-turn mail
    /// names the same one, the origin is kept only when every trigger mail came from the same run.
    fn take_for_run(
        &self,
    ) -> (
        Vec<InterAgentCommunication>,
        Option<RunId>,
        Option<HostEventEmitter>,
    ) {
        let mail: Vec<Mail> = lock(&self.queue).drain(..).collect();
        // Run identity is retained even when the sender has no event sink.
        let parent_run_id = mail
            .iter()
            .filter(|mail| mail.communication.trigger_turn())
            .map(|mail| mail.parent_run_id.as_ref())
            .reduce(|expected, candidate| expected.filter(|expected| candidate == Some(expected)))
            .flatten()
            .cloned();
        let origin = mail
            .iter()
            .filter(|mail| mail.communication.trigger_turn())
            .map(|mail| mail.origin.as_ref())
            .reduce(|expected, candidate| {
                expected.filter(|expected| {
                    candidate.is_some_and(|candidate| candidate.run_id() == expected.run_id())
                })
            })
            .flatten()
            .cloned();
        let communications = mail.into_iter().map(|mail| mail.communication).collect();
        (communications, parent_run_id, origin)
    }

    fn has_pending(&self) -> bool {
        !lock(&self.queue).is_empty()
    }

    fn has_trigger(&self) -> bool {
        lock(&self.queue)
            .iter()
            .any(|mail| mail.communication.trigger_turn())
    }

    async fn wait(
        &self,
        timeout: Duration,
        pending: Option<&crate::runner::PendingInput>,
    ) -> WaitOutcome {
        // Subscribe to both existing queues before inspecting them, so no arrival is lost.
        let mut activity = self.activity.subscribe();
        let mut steering = pending.map(crate::runner::PendingInput::subscribe_activity);
        // Codex reports pending turn input before pending mailbox input.
        if pending.is_some_and(|input| !input.is_empty()) {
            return WaitOutcome::Steered;
        }
        if self.has_pending() {
            return WaitOutcome::MailboxActivity;
        }
        let wait = async {
            if let Some(steering) = steering.as_mut() {
                tokio::select! {
                    biased;
                    result = steering.changed() => if result.is_ok() { WaitOutcome::Steered } else { WaitOutcome::TimedOut },
                    result = activity.changed() => if result.is_ok() { WaitOutcome::MailboxActivity } else { WaitOutcome::TimedOut },
                }
            } else {
                match activity.changed().await {
                    Ok(()) => WaitOutcome::MailboxActivity,
                    Err(_) => WaitOutcome::TimedOut,
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(outcome) => outcome,
            Err(_) => WaitOutcome::TimedOut,
        }
    }
}

/// Counts spawned agents' runs in progress. Capacity is checked where work is accepted and is not
/// reserved, as in Codex's local admission.
struct ExecutionLimiter {
    active: AtomicUsize,
    max_threads: usize,
}

impl ExecutionLimiter {
    fn has_capacity(&self) -> bool {
        self.active.load(Ordering::Acquire) < self.max_threads
    }

    fn admit(self: &Arc<Self>) -> ExecutionGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        ExecutionGuard {
            limiter: Arc::clone(self),
        }
    }
}

struct ExecutionGuard {
    limiter: Arc<ExecutionLimiter>,
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How far below the root `path` is: the root is at 0 and an agent it spawns at 1.
/// Adds context to the history of an agent that runs nothing: recorded into its rollout at once
/// and kept as recorded, as Codex's `inject_no_new_turn` records into an idle session, when the
/// rollout already holds the whole history; otherwise its next run records it with the rest.
fn append_context(child: &ChildRuntime, state: &mut ChildState, messages: Vec<Message>) {
    let record_now = state.recorded == state.history.len();
    for message in messages {
        if record_now && let Some(rollout) = &child.environment.rollout {
            rollout.record(RolloutItem::Item(RunItem::new(
                ra_core::item::ItemId::new(uuid::Uuid::now_v7().to_string()),
                ra_core::item::RunItemKind::Message(message.clone()),
            )));
        }
        state.history.push(ModelInputItem::Message(message));
    }
    if record_now && child.environment.rollout.is_some() {
        state.recorded = state.history.len();
        if let Some(rollout) = child.environment.rollout.clone() {
            drop(tokio::spawn(async move {
                if let Err(error) = rollout
                    .persist(ra_core::session::rollout::PersistContext::Standard)
                    .await
                {
                    tracing::warn!(%error, "context injected into an idle agent could not be persisted");
                }
            }));
        }
    }
}

fn depth(path: &AgentPath) -> usize {
    std::iter::successors(path.parent(), AgentPath::parent).count()
}

/// `path`, a path of this tree, placed below `base`: the names below this tree's root, joined onto
/// `base` in order.
fn below(base: &AgentPath, path: &AgentPath) -> std::result::Result<AgentPath, String> {
    let mut ancestry = Vec::new();
    let mut current = path.clone();
    while !current.is_root() {
        ancestry.push(current.name().to_owned());
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    ancestry
        .iter()
        .rev()
        .try_fold(base.clone(), |joined, name| joined.join(name))
}

/// Locks a mutex whose holders never panic while holding it; a poisoned one is still consistent.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
