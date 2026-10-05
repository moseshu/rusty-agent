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
//! [`fork_history`]), followed by its task.
//!
//! # Closing
//!
//! [`AgentControl::close`] shuts an agent down together with every live agent below it, as Codex's
//! `close_agent` and `shutdown_agent_tree` do: the descendants are found first, then the agent and
//! each descendant in turn is stopped — its run cancelled and waited for — and removed from the
//! tree, so it is no longer listed and its path can be spawned again. The whole subtree is marked
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
//!
//! # What this does not port
//!
//! - **Per-spawn model and reasoning-effort overrides**, agent nicknames, and role descriptions.
//! - **Resume of the tree.** A spawned agent lives as long as its [`AgentControl`]. Its rollout
//!   holds what it takes to rebuild its history, but the tree does not reopen agents from their
//!   rollouts, nor keep Codex's separate store of open and closed spawn edges that its resume reads.

mod budget;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{
        AgentId, AgentSpec,
        control::{
            AgentControlError, AgentControlPort, AgentPath, AgentStatus, InterAgentCommunication,
            LiveAgent, MessageDeliveryMode, SpawnAgentRequest, WaitOutcome, completion_message,
            fork_history,
        },
    },
    cancel::{CancelReason, CancelScope, ScopeKind},
    error::{Error, Result},
    event::{AgentEvent, HostEventEmitter, SubAgentActivityEvent, SubAgentActivityKind},
    finish::FinishReason,
    item::{InputItemNormalizer, ModelInputItem, NormalizedInput, RunItem},
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

use super::{AgentBinding, tool::ParentRun};
use crate::runner::{
    ContinuationInput, RunConfig, RunOutcome, RunRequest, RunResult, RunStreamEvent, Runner,
};

/// Codex's default ceiling on concurrently running spawned agents in one tree.
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
    /// Creates a tree holding only its root, with Codex's default concurrency ceiling.
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
            rollout_budget: OnceLock::new(),
            rollout_store: OnceLock::new(),
            paused_activity: watch::Sender::new(0),
            event_seqs: Mutex::new(HashMap::new()),
        });
        Self { tree }
    }

    /// Limits how deep the tree may grow: an agent spawned by the root is at depth 1, and no agent
    /// may spawn one deeper than `max_depth`.
    ///
    /// Codex's `agent_max_depth`, which its first multi-agent version enforces and the version
    /// these tools port does not: without this call the tree has no depth limit, as in that
    /// version. An agent at the limit is refused every spawn with Codex's message, and the
    /// collaboration tools are not offered to its runs at all, as Codex hides them. Zero keeps
    /// the root from spawning. The setting belongs to the tree, so it applies through every clone
    /// of this control.
    #[must_use]
    pub fn with_max_depth(self, max_depth: usize) -> Self {
        self.tree.max_depth.store(max_depth, Ordering::Release);
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
        self.tree
            .rollout_store
            .set(store)
            .map_err(|_| Error::config("the agent tree already has a rollout store"))?;
        // Only ever set here, behind the store, so it cannot already hold another session.
        let _ = self.tree.root.session_id.set(root_session_id);
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
                let paused = state.paused.as_ref()?;
                Some(PausedAgentRun {
                    path: node.path.clone(),
                    state: paused.state.clone(),
                })
            })
            .collect()
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
        paused
            .resume
            .send(state)
            .map_err(|_| AgentControlError::Unavailable)
    }

    /// Closes the agent at `path` and every live agent below it, and returns the status the agent
    /// had before it was closed.
    ///
    /// Each agent's run is cancelled and waited for — a run paused for approval ends — and the
    /// agent is removed from the tree: it is no longer listed, messages to it are refused as to an
    /// unknown agent, and a later spawn may reuse its path. A closed agent reports nothing to its
    /// parent. Closing the root closes every spawned agent and keeps the root, whose runs belong to
    /// the host; the tree goes on accepting spawns.
    ///
    /// # Errors
    ///
    /// Refuses when the tree has no agent at `path`.
    pub async fn close(&self, path: &AgentPath) -> Result<AgentStatus, AgentControlError> {
        let node = self.tree.node(path).ok_or_else(|| {
            AgentControlError::Unsupported(format!("live agent path `{path}` not found"))
        })?;
        let previous = node.status();
        drop(self.tree.close_subtree(&node, true).await);
        Ok(previous)
    }

    /// Stops every spawned agent: cancels their runs, waits for them to end, and marks each
    /// [`AgentStatus::Shutdown`]. Further spawns and follow-ups are refused.
    pub async fn shutdown(&self) {
        self.tree.shut_down.store(true, Ordering::Release);
        self.tree.cancel.cancel(CancelReason::Shutdown);
        loop {
            let tasks = std::mem::take(&mut *lock(&self.tree.tasks));
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                drop(task.await);
            }
        }
        for node in lock(&self.tree.agents).values() {
            if !node.path.is_root() {
                node.status.send_replace(AgentStatus::Shutdown);
            }
        }
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
    state: RunState,
}

impl PausedAgentRun {
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
}

impl AgentHandle {
    fn new(tree: &Arc<Tree>, node: Arc<AgentNode>) -> Self {
        Self {
            tree: Arc::downgrade(tree),
            node,
        }
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
        {
            drop(tree.close_subtree(&self.node, false).await);
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
        self.node.status.send_replace(status.clone());
        let (Some(tree), Some(parent)) = (tree, self.node.path.parent()) else {
            return;
        };
        if let (Some(parent_node), Some(message)) = (
            tree.node(&parent),
            completion_message(&parent, &self.node.path, &status),
        ) {
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
        let agent = match request.agent_type() {
            Some(agent_type) => Arc::clone(
                parent
                    .config()
                    .agent_registry()
                    .get(agent_type)
                    .ok_or_else(|| {
                        AgentControlError::InvalidRequest(format!(
                            "unknown agent_type `{agent_type}`"
                        ))
                    })?,
            ),
            // The agent running the caller's current turn, which a handoff may have changed
            // since the run started.
            None => parent.current_agent(),
        };
        let history = request.fork_mode().map_or_else(Vec::new, |mode| {
            fork_history(&parent.current_history(), *mode)
        });
        if !tree.limiter.has_capacity() {
            return Err(AgentControlError::AgentLimitReached {
                max_threads: tree.limiter.max_threads,
            });
        }
        if lock(&tree.agents).contains_key(&path) {
            return Err(path_taken(&path));
        }
        let session_id = SessionId::generate();
        let rollout =
            create_rollout(&tree, &self.node, &session_id, &path, request.agent_type()).await?;

        let environment = ChildEnvironment {
            agent: Arc::clone(&agent),
            model_resolver: Arc::clone(parent.model_resolver()),
            config: parent.config().clone(),
            app_context: parent.app_context().map(Arc::clone),
            services: parent.services().clone(),
            spawned_by: parent.run_id().clone(),
            rollout,
        };
        let child = ChildRuntime::new(
            environment,
            tree.cancel.child(ScopeKind::custom("agent")),
            history,
        );
        let node = Arc::new(AgentNode::new(
            path.clone(),
            Some(agent.id().clone()),
            Some(child),
        ));
        let _ = node.session_id.set(session_id);
        {
            let mut agents = lock(&tree.agents);
            // Checked again under the lock a close marks its subtree under, so a spawn racing a
            // close cannot leave a child below an agent that is going away, or add one to a
            // subtree being emptied.
            if self.node.is_closed() {
                return Err(closed_error(&self.node.path));
            }
            if self.node.is_closing_subtree() {
                return Err(closing_error(&self.node.path));
            }
            if agents.contains_key(&path) {
                return Err(path_taken(&path));
            }
            agents.insert(path.clone(), Arc::clone(&node));
        }
        node.mailbox.push_from(
            InterAgentCommunication::from_agent(
                self.node.path.clone(),
                path.clone(),
                request.message(),
                MessageDeliveryMode::TriggerTurn,
            ),
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
        self.node.mailbox.wait(timeout).await
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
    let mut spawn = RolloutThreadSpawn::new(
        root_session_id.clone(),
        parent_session_id.clone(),
        u32::try_from(depth(path)).unwrap_or(u32::MAX),
        path.clone(),
    );
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
    rollout_budget: OnceLock<Arc<RolloutBudget>>,
    /// Where the spawned agents' rollouts are created, once the host has given the tree a store.
    rollout_store: OnceLock<Arc<dyn RolloutThreadStore>>,
    /// Ticks whenever an agent's run pauses for approval.
    paused_activity: watch::Sender<u64>,
    /// The host event sequence of every run that has named itself as the origin of another run,
    /// which records that run's completion from it later. Kept for the tree's lifetime: the run
    /// may be continued at any time, and its continuation must draw from the same sequence.
    event_seqs: Mutex<HashMap<RunId, EventSeqAllocator>>,
}

impl Tree {
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

    /// Codex's `exceeds_thread_spawn_depth_limit` for an agent `caller` would spawn.
    fn spawn_depth_exceeded(&self, caller: &AgentPath) -> bool {
        depth(caller).saturating_add(1) > self.max_depth.load(Ordering::Acquire)
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
    /// not the root, and returns a receiver that resolves once the close is finished.
    ///
    /// Ported from Codex's `shutdown_agent_tree`: the descendants are collected before anything
    /// stops, then the agent and each descendant in depth-first order, children by path, is
    /// waited for and removed. Two things differ from Codex, both so that a close cannot be left
    /// half done:
    ///
    /// - Everything that stops a close from being outgrown or abandoned happens here, before the
    ///   first wait: every target is marked closed and has its run cancelled, and `node` refuses
    ///   spawns until the close is finished. The marking happens under the lock spawns take, so
    ///   the subtree cannot grow while it is being taken down — not even below the root, which
    ///   stays open.
    /// - The waiting and removal run as a task of the tree rather than in the caller, so a caller
    ///   that stops waiting — a timeout, a `select!` — does not leave closed agents behind.
    ///   [`AgentControl::shutdown`] waits for that task like any other.
    fn close_subtree(
        self: &Arc<Self>,
        node: &Arc<AgentNode>,
        including_node: bool,
    ) -> oneshot::Receiver<()> {
        let (doomed, closing) = {
            let agents = lock(&self.agents);
            let mut doomed: Vec<Arc<AgentNode>> = agents
                .values()
                .filter(|other| other.path.starts_with(&node.path))
                .filter(|other| !other.path.is_root())
                .filter(|other| including_node || other.path != node.path)
                .map(Arc::clone)
                .collect();
            // Segment-wise order is a depth-first walk that visits siblings by name, which is the
            // order Codex's walk produces; plain string order would put `a-b` before `a/b`.
            doomed.sort_by(|left, right| {
                left.path
                    .as_str()
                    .split('/')
                    .cmp(right.path.as_str().split('/'))
            });
            for agent in &doomed {
                agent.closed.store(true, Ordering::Release);
            }
            (doomed, ClosingSubtree::new(Arc::clone(node)))
        };
        for agent in &doomed {
            if let Some(child) = &agent.child {
                child.scope.cancel(CancelReason::Shutdown);
            }
        }
        let tree = Arc::clone(self);
        let (done, finished) = oneshot::channel();
        let task = tokio::spawn(async move {
            for agent in doomed {
                tree.stop(&agent).await;
            }
            drop(closing);
            // Nobody may be waiting any more; the close is done either way.
            let _ = done.send(());
        });
        let mut tasks = lock(&self.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
        finished
    }

    /// Waits for a closed agent's cancelled run to end, and removes the agent from the tree.
    async fn stop(&self, node: &Arc<AgentNode>) {
        if let Some(child) = &node.child {
            // Passing the state lock pairs with the check in `ensure_running`: any run claimed
            // before the agent was marked closed has cleared `idle` by now, and none is claimed
            // after.
            drop(lock(&child.state));
            let mut idle = child.idle.subscribe();
            // The sender lives in the node held here, so the wait cannot fail.
            drop(idle.wait_for(|idle| *idle).await);
        }
        node.status.send_replace(AgentStatus::Shutdown);
        let mut agents = lock(&self.agents);
        if agents
            .get(&node.path)
            .is_some_and(|current| Arc::ptr_eq(current, node))
        {
            agents.remove(&node.path);
        }
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
            if state.current_run.is_some() || node.is_closed() || !node.mailbox.has_trigger() {
                return;
            }
            let scope = child.scope.child(ScopeKind::Run);
            state.current_run = Some(scope.clone());
            child.idle.send_replace(false);
            scope
        };
        let task = tokio::spawn(drive(Arc::downgrade(self), Arc::clone(node), scope));
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
        let guard = strong.limiter.admit();
        let handle = AgentHandle::new(&strong, Arc::clone(&node));
        drop(strong);

        let (mail, parent_run_id, origin) = node.mailbox.take_for_run();
        let (mut input, recorded) = {
            let mut state = lock(&child.state);
            state.run_origin = origin;
            (state.history.clone(), state.recorded)
        };
        input.extend(
            mail.iter()
                .map(|mail| ModelInputItem::Message(mail.to_message())),
        );
        let (history, started) = run_to_end(
            &tree,
            child,
            &handle,
            &scope,
            input,
            recorded,
            parent_run_id,
        )
        .await;
        drop(guard);

        let mut state = lock(&child.state);
        // A run that started recorded its whole input, and its history follows from what it
        // recorded; one that never started recorded nothing, and its input is still to be.
        if started {
            state.recorded = history.len();
        }
        state.history = history;
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
    input: Vec<ModelInputItem>,
    recorded: usize,
    parent_run_id: Option<RunId>,
) -> (Vec<ModelInputItem>, bool) {
    let mut fallback = input.clone();
    let mut start = RunStart::Fresh {
        input,
        recorded,
        parent_run_id,
    };
    loop {
        let (state, history) = match run_once(
            &child.environment,
            handle.clone(),
            scope.clone(),
            start,
            fallback,
        )
        .await
        {
            RunEnd::NotStarted(history) => return (history, false),
            RunEnd::Finished(history) => return (history, true),
            RunEnd::Paused { state, history } => (state, history),
        };
        let run_id = state.run_id().clone();
        let (resume, answered) = oneshot::channel();
        lock(&child.state).paused = Some(PausedRun {
            state: *state,
            resume,
        });
        if let Some(tree) = tree.upgrade() {
            tree.paused_activity
                .send_modify(|count| *count = count.wrapping_add(1));
        }
        let answered = tokio::select! {
            answered = answered => answered.ok(),
            () = scope.cancelled() => None,
        };
        lock(&child.state).paused = None;
        let Some(state) = answered else {
            let mut history = history;
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
                    record_cancelled(rollout.as_ref(), run_id, marker).await;
                }
                handle.run_ended(&Err(error)).await;
            }
            return (history, true);
        };
        fallback = history;
        start = RunStart::Resume(Box::new(state));
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
    Fresh {
        input: Vec<ModelInputItem>,
        recorded: usize,
        /// The run every trigger mail it starts on came from, if they agree on one.
        parent_run_id: Option<RunId>,
    },
    Resume(Box<RunState>),
}

/// How a run ended: done with the agent's new history, paused for approval, or never started.
enum RunEnd {
    NotStarted(Vec<ModelInputItem>),
    Finished(Vec<ModelInputItem>),
    Paused {
        state: Box<RunState>,
        history: Vec<ModelInputItem>,
    },
}

/// Runs one segment of an agent's run. `fallback` is the history to keep if the segment returns no
/// result.
async fn run_once(
    environment: &ChildEnvironment,
    handle: AgentHandle,
    scope: CancelScope,
    start: RunStart,
    fallback: Vec<ModelInputItem>,
) -> RunEnd {
    let request = match start {
        RunStart::Fresh {
            input,
            recorded,
            parent_run_id,
        } => {
            let request = RunRequest::new(
                AgentBinding::direct(Arc::clone(&environment.agent)),
                Arc::clone(&environment.model_resolver),
                RunId::generate(),
                scope,
                input,
            )
            .with_recorded_input(recorded);
            // Every run of a spawned agent has a parent, which makes it a sub-agent run to its
            // hooks: Codex runs `SubagentStop` for every turn of a spawned thread, whether or not
            // the turn has a parent turn. The run whose trigger mail started this one is that
            // parent; when the mail names no single run — the host sent it, or several runs did —
            // the run that spawned the agent is.
            request
                .with_parent_run_id(parent_run_id.unwrap_or_else(|| environment.spawned_by.clone()))
        }
        // The checkpoint carries the parent and the history; empty input projects it.
        RunStart::Resume(state) => Ok(RunRequest::new(
            AgentBinding::direct(Arc::clone(&environment.agent)),
            Arc::clone(&environment.model_resolver),
            state.run_id().clone(),
            scope,
            Vec::new(),
        )
        .with_state(*state)),
    };
    let mut request = match request {
        Ok(request) => request
            .with_config(environment.config.clone())
            .with_services(environment.services.clone())
            .with_agent_control(handle),
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

    let mut stream = Runner::run_streamed(request);
    let mut produced = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::Item(item) = event {
            produced.push(item);
        }
    }
    match stream.finish().await {
        Ok(result) if !result.outcome().interruptions().is_empty() => RunEnd::Paused {
            history: result.continuation_input(ContinuationInput::Normalized),
            state: Box::new(result.state().clone()),
        },
        Ok(result) => RunEnd::Finished(result.continuation_input(ContinuationInput::Normalized)),
        Err(_) => RunEnd::Finished(partial_history(fallback, &produced)),
    }
}

/// The history after a run that returned no result: its input and the records of the turns it
/// settled, normalized so any call left without an output is dropped from the next request.
fn partial_history(mut input: Vec<ModelInputItem>, produced: &[RunItem]) -> Vec<ModelInputItem> {
    input.extend(produced.iter().filter_map(RunItem::to_model_input));
    InputItemNormalizer::new()
        .normalize_model_items(&input)
        .map_or(input, NormalizedInput::into_items)
}

struct AgentNode {
    path: AgentPath,
    /// The session of the agent's own rollout; the root's once the tree is given a store.
    session_id: OnceLock<SessionId>,
    agent_id: Mutex<Option<AgentId>>,
    status: watch::Sender<AgentStatus>,
    mailbox: Mailbox,
    /// Set once the agent is being closed; it then starts no run and accepts nothing more.
    closed: AtomicBool,
    /// How many closes of the agents below this one are in progress; it spawns nothing meanwhile.
    closing_subtree: AtomicUsize,
    /// Absent for the root, whose runs the host starts.
    child: Option<ChildRuntime>,
    /// How many thresholds of the tree's rollout budget the agent has been told about.
    budget_reminder_delivered: Mutex<Option<usize>>,
}

impl AgentNode {
    fn new(path: AgentPath, agent_id: Option<AgentId>, child: Option<ChildRuntime>) -> Self {
        Self {
            path,
            session_id: OnceLock::new(),
            agent_id: Mutex::new(agent_id),
            status: watch::Sender::new(AgentStatus::PendingInit),
            mailbox: Mailbox::new(),
            closed: AtomicBool::new(false),
            closing_subtree: AtomicUsize::new(0),
            child,
            budget_reminder_delivered: Mutex::new(None),
        }
    }

    fn status(&self) -> AgentStatus {
        self.status.borrow().clone()
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
    agent: Arc<AgentSpec>,
    model_resolver: Arc<dyn ModelResolver>,
    config: RunConfig,
    app_context: Option<Arc<dyn std::any::Any + Send + Sync>>,
    services: ToolServices,
    /// The run that spawned the agent: the parent of a run whose trigger mail names no single run.
    spawned_by: RunId,
    /// The agent's own rollout, when the tree records its agents.
    rollout: Option<Arc<dyn RolloutRecorder>>,
}

struct ChildRuntime {
    environment: ChildEnvironment,
    /// Parent of the agent's run scopes, below the tree's.
    scope: CancelScope,
    state: Mutex<ChildState>,
    /// Whether no driver is running the agent: cleared when a run is claimed, set when the
    /// driver hands its last run back. A close waits on it.
    idle: watch::Sender<bool>,
}

impl ChildRuntime {
    /// An agent about to start its first run on `history`, none of which its rollout holds yet.
    fn new(
        environment: ChildEnvironment,
        scope: CancelScope,
        history: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            environment,
            scope,
            state: Mutex::new(ChildState {
                history,
                recorded: 0,
                current_run: None,
                paused: None,
                run_origin: None,
            }),
            idle: watch::Sender::new(true),
        }
    }
}

struct ChildState {
    history: Vec<ModelInputItem>,
    /// How many leading items of `history` the agent's rollout holds: none of the history it was
    /// forked with, and all of it once a run has started on it.
    recorded: usize,
    /// The scope of the run in progress, or of the one about to start.
    current_run: Option<CancelScope>,
    /// The run's checkpoint while it waits for the host to answer an approval.
    paused: Option<PausedRun>,
    /// The run whose spawn or follow-up started the current run, whose timeline records the run's
    /// completion.
    run_origin: Option<HostEventEmitter>,
}

struct PausedRun {
    state: RunState,
    resume: oneshot::Sender<RunState>,
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

    async fn wait(&self, timeout: Duration) -> WaitOutcome {
        // Subscribed before the queue is read, so a delivery between the two is not missed.
        let mut activity = self.activity.subscribe();
        if self.has_pending() {
            return WaitOutcome::MailboxActivity;
        }
        match tokio::time::timeout(timeout, activity.changed()).await {
            Ok(Ok(())) => WaitOutcome::MailboxActivity,
            Ok(Err(_)) | Err(_) => WaitOutcome::TimedOut,
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
fn depth(path: &AgentPath) -> usize {
    std::iter::successors(path.parent(), AgentPath::parent).count()
}

/// Locks a mutex whose holders never panic while holding it; a poisoned one is still consistent.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
