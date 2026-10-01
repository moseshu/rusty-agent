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
//! returns no result — and the agent is then idle and can be given a follow-up.
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
//! # What this does not port
//!
//! - **Per-spawn model and reasoning-effort overrides**, agent nicknames, and role descriptions.
//! - **Persistence and resume of the tree.** A spawned agent lives as long as its [`AgentControl`].
//! - **Host events and usage accrual to the parent**, which belong to the event and budget work.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex, Weak,
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
    error::Result,
    finish::FinishReason,
    item::{InputItemNormalizer, ModelInputItem, NormalizedInput, RunItem},
    model::ModelResolver,
    state::{RunId, RunState},
    tool::ToolServices,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

use super::{AgentBinding, tool::ParentRun};
use crate::runner::{
    ContinuationInput, RunConfig, RunOutcome, RunRequest, RunResult, RunStreamEvent, Runner,
};

/// Codex's default ceiling on concurrently running spawned agents in one tree.
pub const DEFAULT_MAX_CONCURRENT_THREADS: usize = 4;

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
            paused_activity: watch::Sender::new(0),
        });
        Self { tree }
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

    /// Takes the mail a run bound to this agent delivers at a model-call boundary.
    pub(crate) fn take_mail(&self) -> Vec<InterAgentCommunication> {
        self.node.mailbox.take_all()
    }

    /// Marks the agent running a run of `agent`.
    pub(crate) fn run_started(&self, agent: &AgentId) {
        *lock(&self.node.agent_id) = Some(agent.clone());
        self.node.status.send_replace(AgentStatus::Running);
    }

    /// Records how a run bound to this agent ended and, for a spawned agent, reports it to the
    /// parent.
    pub(crate) fn run_finished(&self, result: &Result<RunResult>) {
        let status = terminal_status(result);
        let tree = self.tree.upgrade();
        let shutting_down = tree
            .as_ref()
            .is_none_or(|tree| tree.shut_down.load(Ordering::Acquire));
        if shutting_down && !self.node.path.is_root() {
            self.node.status.send_replace(AgentStatus::Shutdown);
            return;
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

        let environment = ChildEnvironment {
            agent: Arc::clone(&agent),
            model_resolver: Arc::clone(parent.model_resolver()),
            config: parent.config().clone(),
            app_context: parent.app_context().map(Arc::clone),
            services: parent.services().clone(),
            parent_run_id: parent.run_id().clone(),
        };
        let child = ChildRuntime {
            environment,
            scope: tree.cancel.child(ScopeKind::custom("agent")),
            state: Mutex::new(ChildState {
                history,
                current_run: None,
                paused: None,
            }),
        };
        let node = Arc::new(AgentNode::new(
            path.clone(),
            Some(agent.id().clone()),
            Some(child),
        ));
        {
            let mut agents = lock(&tree.agents);
            if agents.contains_key(&path) {
                return Err(AgentControlError::Unsupported(format!(
                    "agent path `{path}` already exists"
                )));
            }
            agents.insert(path.clone(), Arc::clone(&node));
        }
        node.mailbox.push(InterAgentCommunication::from_agent(
            self.node.path.clone(),
            path.clone(),
            request.message(),
            MessageDeliveryMode::TriggerTurn,
        ));
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
        if node.status() == AgentStatus::Shutdown || tree.shut_down.load(Ordering::Acquire) {
            return Err(AgentControlError::Unsupported(format!(
                "agent `{}` is closed",
                node.path
            )));
        }
        if mode == MessageDeliveryMode::TriggerTurn
            && !node.is_running()
            && !tree.limiter.has_capacity()
        {
            return Err(AgentControlError::AgentLimitReached {
                max_threads: tree.limiter.max_threads,
            });
        }
        node.mailbox.push(InterAgentCommunication::from_agent(
            self.node.path.clone(),
            node.path.clone(),
            &message,
            mode,
        ));
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
    /// Ticks whenever an agent's run pauses for approval.
    paused_activity: watch::Sender<u64>,
}

impl Tree {
    fn node(&self, path: &AgentPath) -> Option<Arc<AgentNode>> {
        lock(&self.agents).get(path).map(Arc::clone)
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
            if state.current_run.is_some() || !node.mailbox.has_trigger() {
                return;
            }
            let scope = child.scope.child(ScopeKind::Run);
            state.current_run = Some(scope.clone());
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
            break;
        };
        let guard = strong.limiter.admit();
        let handle = AgentHandle::new(&strong, Arc::clone(&node));
        drop(strong);

        let mut input = lock(&child.state).history.clone();
        input.extend(
            node.mailbox
                .take_all()
                .iter()
                .map(|mail| ModelInputItem::Message(mail.to_message())),
        );
        let history = run_to_end(&tree, child, &handle, &scope, input).await;
        drop(guard);

        let mut state = lock(&child.state);
        state.history = history;
        let shut_down = tree
            .upgrade()
            .is_none_or(|tree| tree.shut_down.load(Ordering::Acquire));
        if shut_down || !node.mailbox.has_trigger() {
            state.current_run = None;
            break;
        }
        scope = child.scope.child(ScopeKind::Run);
        state.current_run = Some(scope.clone());
    }
}

/// Runs one run of an agent to its end — across any number of pauses for approval — and returns
/// the agent's history after it.
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
) -> Vec<ModelInputItem> {
    let mut fallback = input.clone();
    let mut start = RunStart::Fresh(input);
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
            RunEnd::Finished(history) => return history,
            RunEnd::Paused { state, history } => (state, history),
        };
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
            if let Err(error) = scope.ensure_not_cancelled() {
                handle.run_finished(&Err(error));
            }
            return history;
        };
        fallback = history;
        start = RunStart::Resume(Box::new(state));
    }
}

/// How a run starts: on new input, or from a checkpoint the host has answered.
enum RunStart {
    Fresh(Vec<ModelInputItem>),
    Resume(Box<RunState>),
}

/// How a run ended: done with the agent's new history, or paused for approval.
enum RunEnd {
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
        RunStart::Fresh(input) => RunRequest::new(
            AgentBinding::direct(Arc::clone(&environment.agent)),
            Arc::clone(&environment.model_resolver),
            RunId::generate(),
            scope,
            input,
        )
        .with_parent_run_id(environment.parent_run_id.clone()),
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
            return RunEnd::Finished(fallback);
        }
    };
    if let Some(app_context) = &environment.app_context {
        request = request.with_app_context(Arc::clone(app_context));
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
    agent_id: Mutex<Option<AgentId>>,
    status: watch::Sender<AgentStatus>,
    mailbox: Mailbox,
    /// Absent for the root, whose runs the host starts.
    child: Option<ChildRuntime>,
}

impl AgentNode {
    fn new(path: AgentPath, agent_id: Option<AgentId>, child: Option<ChildRuntime>) -> Self {
        Self {
            path,
            agent_id: Mutex::new(agent_id),
            status: watch::Sender::new(AgentStatus::PendingInit),
            mailbox: Mailbox::new(),
            child,
        }
    }

    fn status(&self) -> AgentStatus {
        self.status.borrow().clone()
    }

    fn is_running(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|child| lock(&child.state).current_run.is_some())
    }

    fn snapshot(&self) -> LiveAgent {
        LiveAgent::new(
            self.path.clone(),
            lock(&self.agent_id).clone(),
            self.status(),
        )
    }
}

/// What a spawned agent's runs start from, captured from the run that spawned it.
struct ChildEnvironment {
    agent: Arc<AgentSpec>,
    model_resolver: Arc<dyn ModelResolver>,
    config: RunConfig,
    app_context: Option<Arc<dyn std::any::Any + Send + Sync>>,
    services: ToolServices,
    parent_run_id: RunId,
}

struct ChildRuntime {
    environment: ChildEnvironment,
    /// Parent of the agent's run scopes, below the tree's.
    scope: CancelScope,
    state: Mutex<ChildState>,
}

struct ChildState {
    history: Vec<ModelInputItem>,
    /// The scope of the run in progress, or of the one about to start.
    current_run: Option<CancelScope>,
    /// The run's checkpoint while it waits for the host to answer an approval.
    paused: Option<PausedRun>,
}

struct PausedRun {
    state: RunState,
    resume: oneshot::Sender<RunState>,
}

/// An agent's pending mail, with a counter that ticks on every delivery for waiters.
struct Mailbox {
    queue: Mutex<VecDeque<InterAgentCommunication>>,
    activity: watch::Sender<u64>,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            activity: watch::Sender::new(0),
        }
    }

    fn push(&self, mail: InterAgentCommunication) {
        lock(&self.queue).push_back(mail);
        self.activity
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    fn take_all(&self) -> Vec<InterAgentCommunication> {
        lock(&self.queue).drain(..).collect()
    }

    fn has_pending(&self) -> bool {
        !lock(&self.queue).is_empty()
    }

    fn has_trigger(&self) -> bool {
        lock(&self.queue)
            .iter()
            .any(InterAgentCommunication::trigger_turn)
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

/// Locks a mutex whose holders never panic while holding it; a poisoned one is still consistent.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
