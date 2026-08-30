//! Process execution session identity, audited state transitions, and the running-process manager.
//!
//! # Who owns the child process
//!
//! Exactly one task does: the supervisor spawned by [`ProcessManager::execute`]. Everything else —
//! cancellation, eviction, an idle sweep, an interrupt from an interactive tool — asks it for a
//! termination by sending on a channel and never touches the [`Child`] itself.
//!
//! That is a correctness requirement rather than a style preference. A [`Child`] behind a shared
//! lock has to be locked for the whole of `wait()`, which is the entire lifetime of the process, so
//! every would-be killer blocks on the lock until the thing it wants to kill has already died on
//! its own. Routing termination through a channel means no caller ever waits for the process in
//! order to stop it.
//!
//! # What termination means
//!
//! `SIGTERM` to the **process group**, then `SIGKILL` to the group after
//! [`DRAIN_GRACE`]. The group is the point: the child is a shell, and
//! signalling only the shell leaves whatever it started behind as orphans that keep running and
//! keep holding the output pipes open.

use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

pub use ra_core::event::exec::ExecSessionId;
use ra_core::{
    cancel::DRAIN_GRACE,
    event::{
        HostEventEmitter,
        exec::{
            ExecEvent, ExecEvictedEvent, ExecEvictionReason, ExecExitedEvent, ExecOutputEvent,
            ExecStartedEvent, ExecStreamKind, ExecYieldReason, ExecYieldedEvent,
            TerminalInteractionEvent,
        },
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, Notify, mpsc},
    time::Instant as TokioInstant,
};

use crate::{
    command::{ExecCursor, ExecLimits, ExecRequest},
    output::{ExecOutputSummary, HeadTailBuffer},
};

/// The default shell used when a request names none.
const DEFAULT_SHELL: &str = "/bin/sh";

/// Size of one read from a child's stdout or stderr pipe.
const STREAM_CHUNK_BYTES: usize = 4096;

/// Extra time allowed, on top of [`DRAIN_GRACE`], for an evicted process to be reaped and its slot
/// freed. It covers the reap and the bookkeeping that follows the kill, not another wait for the
/// process itself.
const CAPACITY_WAIT_SLACK: Duration = Duration::from_millis(500);

/// The lifecycle state of a process execution session.
///
/// Transitions follow an audited single-direction state machine. Once a session reaches a terminal
/// state ([`Self::Exited`], [`Self::Failed`], [`Self::Cancelled`], [`Self::Expired`]), no further
/// transitions are permitted.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExecSessionState {
    /// Session allocated and reserved, but the child process has not yet spawned.
    Reserved,
    /// Process spawning in progress.
    Starting,
    /// Process running and accepting output collection / interactions.
    Running,
    /// Process exited normally or by signal.
    Exited {
        /// Process exit status code, if available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// Process spawn or communication failed.
    Failed {
        /// Failure explanation.
        error: String,
    },
    /// Execution was explicitly cancelled by the caller or deadline.
    Cancelled,
    /// Session was expired or evicted by host policy.
    Expired {
        /// Eviction reason.
        reason: ExecEvictionReason,
    },
}

/// An error returned when attempting an illegal state machine transition.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("invalid execution session state transition from {from:?} to {to:?}")]
pub struct ExecSessionTransitionError {
    from: ExecSessionState,
    to: ExecSessionState,
}

impl ExecSessionTransitionError {
    /// Creates a new transition error.
    #[must_use]
    pub const fn new(from: ExecSessionState, to: ExecSessionState) -> Self {
        Self { from, to }
    }

    /// State before transition.
    #[must_use]
    pub const fn from(&self) -> &ExecSessionState {
        &self.from
    }

    /// Target state attempted.
    #[must_use]
    pub const fn to(&self) -> &ExecSessionState {
        &self.to
    }
}

impl ExecSessionState {
    /// Whether this state is a terminal final state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled | Self::Expired { .. }
        )
    }

    /// Whether this session is actively running or starting.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Reserved | Self::Starting | Self::Running)
    }

    /// Returns true if transitioning from `self` to `next` is valid.
    #[must_use]
    pub fn can_transition_to(&self, next: &Self) -> bool {
        match self {
            Self::Reserved => {
                matches!(next, Self::Starting | Self::Failed { .. } | Self::Cancelled)
            }
            Self::Starting => matches!(
                next,
                Self::Running | Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled
            ),
            Self::Running => matches!(
                next,
                Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled | Self::Expired { .. }
            ),
            Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled | Self::Expired { .. } => {
                false
            }
        }
    }

    /// Attempts to transition the state machine to `next`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecSessionTransitionError`] if the transition is not permitted.
    pub fn transition_to(&mut self, next: Self) -> Result<(), ExecSessionTransitionError> {
        if self.can_transition_to(&next) {
            *self = next;
            Ok(())
        } else {
            Err(ExecSessionTransitionError {
                from: self.clone(),
                to: next,
            })
        }
    }
}

/// Derives a resource identity for a process execution session.
pub fn session_resource_id(
    session_id: &ExecSessionId,
) -> Result<ra_core::tool::ResourceId, ra_core::error::Error> {
    ra_core::tool::ResourceId::process(session_id.to_string())
}

/// Why a process manager operation could not be carried out.
///
/// Deliberately not a [`ra_core::error::Error`]: this crate runs processes for whoever asks, and
/// `Error::tool` needs a tool name it has no way to know. `exec_command` and the interactive stdin
/// tool both drive the same manager, and a failure labelled with the wrong one of them is a failure
/// the model is told to correct in the wrong place. Callers map these into their own tool's failure
/// vocabulary, where the model-facing sentence is written once.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The process could not be started at all.
    #[error("cannot start `{command}`")]
    Spawn {
        /// The command line that failed to start.
        command: String,
        /// The operating system's refusal.
        #[source]
        source: std::io::Error,
    },
    /// No session with that identifier is known to this manager.
    #[error("execution session `{session_id}` is not known")]
    UnknownSession {
        /// The identifier that resolved to nothing.
        session_id: ExecSessionId,
    },
    /// The session exists, but it has already reached a terminal state.
    #[error("execution session `{session_id}` is no longer running")]
    SessionNotActive {
        /// The session that has already finished.
        session_id: ExecSessionId,
        /// The terminal state it finished in.
        state: ExecSessionState,
    },
    /// The session's standard input is no longer open for writing.
    #[error("execution session `{session_id}` has no open standard input")]
    StdinClosed {
        /// The session whose standard input is closed.
        session_id: ExecSessionId,
    },
    /// Writing to the session's standard input failed.
    #[error("cannot write to standard input of execution session `{session_id}`")]
    StdinWrite {
        /// The session that could not be written to.
        session_id: ExecSessionId,
        /// The operating system's refusal.
        #[source]
        source: std::io::Error,
    },
    /// Every session slot is held by a process that has not ended.
    ///
    /// Reached only after the oldest sessions were asked to stop and given the full drain window,
    /// so it means the machine is wedged rather than merely busy.
    #[error("all {max_sessions} execution slots are held by processes that have not ended")]
    AtCapacity {
        /// The configured ceiling on concurrent sessions.
        max_sessions: usize,
    },
    /// An incremental read named a stream that has no single position to read from.
    ///
    /// A cursor is one offset. `stdout` and `stderr` arrive on two pipes whose interleaving is
    /// never recorded, so a combined offset cannot be split back into the two it came from — and a
    /// reader that resumed from one would silently re-deliver or skip output.
    #[error("an incremental read addresses one stream; `{stream}` has no single cursor")]
    UnaddressableStream {
        /// The stream kind that was asked for.
        stream: &'static str,
    },
}

/// The result returned by a process execution attempt.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecExecutionResult {
    /// The process completed synchronously within the initial yield timeout.
    Completed(ExecOutputSummary),
    /// The process was still running when the yield timeout elapsed and yielded to background.
    Yielded {
        /// The assigned execution session identifier.
        session_id: ExecSessionId,
        /// Captured output up to the yield point.
        summary: ExecOutputSummary,
    },
}

impl ExecExecutionResult {
    /// The output summary regardless of completion status.
    #[must_use]
    pub fn summary(&self) -> &ExecOutputSummary {
        match self {
            Self::Completed(s) | Self::Yielded { summary: s, .. } => s,
        }
    }

    /// The session identifier if the command yielded to background.
    #[must_use]
    pub fn session_id(&self) -> Option<&ExecSessionId> {
        match self {
            Self::Completed(_) => None,
            Self::Yielded { session_id, .. } => Some(session_id),
        }
    }
}

/// What a termination request asks the supervisor to deliver to the process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminationSignal {
    /// Ctrl-C. The process may catch it and stop on its own terms, or ignore it.
    Interrupt,
    /// Stop now: `SIGTERM`, escalated to [`Self::Kill`] once [`DRAIN_GRACE`] has passed.
    Terminate,
    /// `SIGKILL`. Not requestable from outside: it is only ever the end of an escalation.
    Kill,
}

/// How a session that the manager stopped should be recorded.
#[derive(Debug, Clone)]
enum TerminalOutcome {
    /// The caller asked for it.
    Cancelled,
    /// Host policy took the session away.
    Evicted(ExecEvictionReason),
}

struct ExecSession {
    state: ExecSessionState,
    stdout_buffer: HeadTailBuffer,
    stderr_buffer: HeadTailBuffer,
    last_active_at: Instant,
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    pid: Option<u32>,
    /// Behind its own lock, because writing to it can block for as long as the process declines to
    /// read. Under the session's lock, one write into a full pipe would stall every reader of this
    /// session's state — including the cancellation that is the only thing able to end the stall.
    child_stdin: Arc<Mutex<Option<ChildStdin>>>,
    /// Sends termination requests to the supervisor that owns the child process.
    terminate: Option<mpsc::UnboundedSender<TerminationSignal>>,
    exit_code: Option<i32>,
    /// The output positions the interactive tool has already returned to its caller.
    ///
    /// This is deliberately separate from [`ExecCursor`]: a general reader owns its own cursor,
    /// while the model has one shared `write_stdin` conversation with a session. Keeping the
    /// latter here means a command can finish between two model calls without its unread tail
    /// being mistaken for output that was already delivered.
    interactive_stdout_cursor: u64,
    interactive_stderr_cursor: u64,
    output_notify: Arc<Notify>,
    exit_notify: Arc<Notify>,
}

impl ExecSession {
    fn new(capacity: usize) -> Self {
        Self {
            state: ExecSessionState::Reserved,
            stdout_buffer: HeadTailBuffer::new(capacity),
            stderr_buffer: HeadTailBuffer::new(capacity),
            last_active_at: Instant::now(),
            started_at: None,
            ended_at: None,
            pid: None,
            child_stdin: Arc::new(Mutex::new(None)),
            terminate: None,
            exit_code: None,
            interactive_stdout_cursor: 0,
            interactive_stderr_cursor: 0,
            output_notify: Arc::new(Notify::new()),
            exit_notify: Arc::new(Notify::new()),
        }
    }

    fn duration(&self) -> Duration {
        match (self.started_at, self.ended_at) {
            (Some(start), Some(end)) => end.saturating_duration_since(start),
            (Some(start), None) => Instant::now().saturating_duration_since(start),
            (None, _) => Duration::ZERO,
        }
    }

    /// Asks the supervisor for a signal. A closed channel means the process is already gone.
    fn request_termination(&self, signal: TerminationSignal) {
        if let Some(sender) = &self.terminate {
            let _ = sender.send(signal);
        }
    }

    /// Records a terminal state, reporting whether it took effect.
    ///
    /// The first reason recorded is the one that stays: a session cancelled a moment before its
    /// process happened to exit died of the cancellation, and reporting the exit instead would lose
    /// the only fact that explains the run.
    ///
    /// A refused transition leaves the session untouched rather than marking it ended anyway. The
    /// state machine refuses some pairs on purpose — a session that never spawned cannot have
    /// exited — and a session recorded as over while its state says otherwise is worse than either.
    fn finish(&mut self, state: ExecSessionState) -> bool {
        if self.state.is_terminal() || self.state.transition_to(state).is_err() {
            return false;
        }
        self.ended_at = Some(Instant::now());
        self.exit_notify.notify_waiters();
        true
    }

    fn summary(&self) -> ExecOutputSummary {
        let retained = self
            .stdout_buffer
            .retained_bytes()
            .saturating_add(self.stderr_buffer.retained_bytes());
        let mut summary = ExecOutputSummary::new(
            self.stdout_buffer.to_string_lossy(),
            self.stderr_buffer.to_string_lossy(),
        )
        .with_stdout_bytes(self.stdout_buffer.total_bytes())
        .with_stderr_bytes(self.stderr_buffer.total_bytes())
        .with_retained_bytes(retained)
        .with_duration(self.duration())
        .with_truncated(self.stdout_buffer.is_truncated() || self.stderr_buffer.is_truncated());
        if let Some(code) = self.exit_code {
            summary = summary.with_exit_code(code);
        }
        summary
    }
}

/// The set of sessions this manager knows about, and the order they arrived in.
///
/// One lock over all three collections rather than one lock each. Two locks taken in one order by
/// registration and the other order by retirement is a deadlock that only shows up when a process
/// exits at the same moment another starts, which is to say under exactly the load that makes it
/// hardest to reproduce.
struct RegistryState {
    sessions: HashMap<ExecSessionId, Arc<Mutex<ExecSession>>>,
    /// Sessions whose process may still be running, oldest first.
    active: VecDeque<ExecSessionId>,
    /// Finished sessions still answerable by a poll, oldest first.
    retired: VecDeque<ExecSessionId>,
}

struct SessionRegistry {
    state: Mutex<RegistryState>,
    /// How many finished sessions stay readable before the oldest is dropped.
    retention: usize,
    /// Fires whenever a session leaves the active set, so a caller waiting for room can stop
    /// waiting the moment there is some rather than polling for it.
    retired_notify: Arc<Notify>,
}

impl SessionRegistry {
    fn new(retention: usize) -> Self {
        Self {
            state: Mutex::new(RegistryState {
                sessions: HashMap::new(),
                active: VecDeque::new(),
                retired: VecDeque::new(),
            }),
            retention,
            retired_notify: Arc::new(Notify::new()),
        }
    }

    async fn register(&self, session_id: ExecSessionId, session: Arc<Mutex<ExecSession>>) {
        let mut state = self.state.lock().await;
        state.sessions.insert(session_id.clone(), session);
        state.active.push_back(session_id);
    }

    async fn get(&self, session_id: &ExecSessionId) -> Option<Arc<Mutex<ExecSession>>> {
        let state = self.state.lock().await;
        state.sessions.get(session_id).map(Arc::clone)
    }

    /// Moves a session out of the active set once its process is gone for good, dropping the
    /// oldest retired one if the retention window is full.
    ///
    /// **Called when the process is reaped, not when it is asked to stop.** The two are up to
    /// [`DRAIN_GRACE`] apart, and during that window the process group is still running and still
    /// holding whatever it holds. Retiring on the request instead would free the slot before the
    /// resource, so a caller starting sessions in a loop could keep "making room" against processes
    /// that had not gone anywhere and run arbitrarily many of them at once.
    ///
    /// Idempotent: a spawn that never produced a process retires directly, and every process that
    /// did retires through its supervisor.
    async fn retire(&self, session_id: &ExecSessionId) {
        {
            let mut state = self.state.lock().await;
            let was_active = state
                .active
                .iter()
                .position(|id| id == session_id)
                .map(|index| state.active.remove(index));
            if was_active.is_none() {
                return;
            }
            state.retired.push_back(session_id.clone());
            while state.retired.len() > self.retention {
                if let Some(dropped) = state.retired.pop_front() {
                    state.sessions.remove(&dropped);
                }
            }
        }
        self.retired_notify.notify_waiters();
    }

    async fn active_ids(&self) -> Vec<ExecSessionId> {
        let state = self.state.lock().await;
        state.active.iter().cloned().collect()
    }

    fn retirement_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.retired_notify)
    }
}

/// A thread-safe runtime manager for command execution sessions and background processes.
pub struct ProcessManager {
    registry: Arc<SessionRegistry>,
    limits: ExecLimits,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new(ExecLimits::default())
    }
}

impl ProcessManager {
    /// Creates a process manager whose limits are the ceiling for every command it runs.
    ///
    /// A command may ask for less than these allow and gets what it asked for; a command asking for
    /// more gets the ceiling. `max_sessions` additionally bounds how many run at once and how many
    /// finished ones stay readable afterwards.
    #[must_use]
    pub fn new(limits: ExecLimits) -> Self {
        let retention = limits.max_sessions().max(1);
        Self {
            registry: Arc::new(SessionRegistry::new(retention)),
            limits,
        }
    }

    /// The ceilings this manager holds every command to.
    ///
    /// A request's own limits are applied on top of these and may only tighten them; see
    /// [`ExecLimits::tightened_by`]. `max_sessions` is the manager's alone — it bounds how many
    /// commands run at once, which no single command is in a position to decide.
    #[must_use]
    pub const fn limits(&self) -> &ExecLimits {
        &self.limits
    }

    /// Spawns and executes a command according to the given request.
    ///
    /// If the process completes within the effective initial yield timeout, it returns
    /// [`ExecExecutionResult::Completed`]. Otherwise, it transitions to background execution
    /// and returns [`ExecExecutionResult::Yielded`].
    ///
    /// The request's limits are tightened by this manager's before anything is spawned, so every
    /// deadline and ceiling that follows is the stricter of what the caller asked for and what the
    /// host allows.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::AtCapacity`] when no slot could be freed for this command, and
    /// [`ExecError::Spawn`] when the process could not be started.
    pub async fn execute(
        &self,
        request: ExecRequest,
        emitter: Option<&HostEventEmitter>,
    ) -> Result<ExecExecutionResult, ExecError> {
        // Resolved once, into the request itself, so that nothing downstream can reach past it to
        // the untightened values — the bug that comes back otherwise is one code path reading
        // `request.limits()` directly and quietly running without the host's ceiling.
        let effective = request.limits().tightened_by(&self.limits);
        let request = request.with_limits(effective);

        self.enforce_capacity().await?;

        let session_id = ExecSessionId::generate();
        let session = Arc::new(Mutex::new(ExecSession::new(
            request.limits().max_capture_bytes(),
        )));
        // Registered before the process exists, so that a session which yields is already
        // addressable by the identifier the caller is about to be handed.
        self.registry
            .register(session_id.clone(), Arc::clone(&session))
            .await;

        {
            let mut sess = session.lock().await;
            let _ = sess.state.transition_to(ExecSessionState::Starting);
        }

        let mut child = match spawn_child(&request) {
            Ok(child) => child,
            Err(error) => {
                {
                    let mut sess = session.lock().await;
                    sess.finish(ExecSessionState::Failed {
                        error: error.to_string(),
                    });
                }
                self.registry.retire(&session_id).await;
                return Err(ExecError::Spawn {
                    command: request.command().to_owned(),
                    source: error,
                });
            }
        };

        let pid = child.id();
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();
        let child_stdin = child.stdin.take();
        let (terminate_tx, terminate_rx) = mpsc::unbounded_channel();

        let (exit_notify, stdin_slot) = {
            let mut sess = session.lock().await;
            let _ = sess.state.transition_to(ExecSessionState::Running);
            sess.started_at = Some(Instant::now());
            sess.last_active_at = Instant::now();
            sess.pid = pid;
            *sess.child_stdin.lock().await = child_stdin;
            sess.terminate = Some(terminate_tx);
            (Arc::clone(&sess.exit_notify), Arc::clone(&sess.child_stdin))
        };

        // Armed before the supervisor can possibly report an exit. `Notify` wakes the waiters
        // registered at the moment it fires and keeps nothing for anyone who arrives later, so a
        // process that finishes in under a millisecond would otherwise leave this call waiting out
        // the full yield timeout and then reporting a finished command as still running.
        let exit_notified = exit_notify.notified();
        tokio::pin!(exit_notified);
        exit_notified.as_mut().enable();

        emit_started(emitter, &session_id, &request, pid);

        let wait_task = self.spawn_supervisor(
            child,
            Pipes {
                stdout: child_stdout,
                stderr: child_stderr,
                stdin: stdin_slot,
            },
            terminate_rx,
            &request,
            (&session_id, &session),
            emitter,
        );

        tokio::select! {
            () = &mut exit_notified => {
                let _ = wait_task.await;
                let summary = session.lock().await.summary();
                Ok(ExecExecutionResult::Completed(summary))
            }
            () = tokio::time::sleep(request.limits().initial_yield_timeout()) => {
                let summary = session.lock().await.summary();
                if let Some(em) = emitter {
                    let _ = em.emit(ExecEvent::Yielded(ExecYieldedEvent::new(
                        session_id.clone(),
                        summary.duration_ms(),
                        summary.total_bytes(),
                        ExecYieldReason::InitialTimeout,
                    )));
                }
                Ok(ExecExecutionResult::Yielded { session_id, summary })
            }
        }
    }

    /// Starts the output readers and the one task that owns the child process.
    fn spawn_supervisor(
        &self,
        child: Child,
        pipes: Pipes,
        terminate_rx: mpsc::UnboundedReceiver<TerminationSignal>,
        request: &ExecRequest,
        session: (&ExecSessionId, &Arc<Mutex<ExecSession>>),
        emitter: Option<&HostEventEmitter>,
    ) -> tokio::task::JoinHandle<()> {
        let (session_id, session) = session;
        let stdout_task = spawn_stream_reader(
            pipes.stdout,
            Arc::clone(session),
            ExecStreamKind::Stdout,
            session_id.clone(),
            emitter.cloned(),
        );
        let stderr_task = spawn_stream_reader(
            pipes.stderr,
            Arc::clone(session),
            ExecStreamKind::Stderr,
            session_id.clone(),
            emitter.cloned(),
        );
        let context = SupervisorContext {
            session: Arc::clone(session),
            registry: Arc::clone(&self.registry),
            session_id: session_id.clone(),
            emitter: emitter.cloned(),
            pid: child.id(),
            total_timeout: request.limits().total_timeout(),
            idle_timeout: request.limits().idle_timeout(),
            stdin: pipes.stdin,
        };
        tokio::spawn(supervise(
            child,
            terminate_rx,
            stdout_task,
            stderr_task,
            context,
        ))
    }

    /// Makes room for one more session by evicting the oldest ones still running.
    ///
    /// Counts only sessions whose process may still be running. Finished sessions stay readable so
    /// a caller can still collect their output, and counting them would let a manager that had run
    /// its quota of short commands refuse to start anything ever again.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::AtCapacity`] when no slot could be freed within the drain window.
    async fn enforce_capacity(&self) -> Result<(), ExecError> {
        let max_sessions = self.limits.max_sessions().max(1);
        let room_for_one = max_sessions.saturating_sub(1);
        // A stopped process is given `DRAIN_GRACE` to leave on its own, so waiting out one full
        // grace plus a little is the longest an eviction can honestly take.
        let deadline = TokioInstant::now() + DRAIN_GRACE + CAPACITY_WAIT_SLACK;

        loop {
            // Armed before the count is read. A session retiring between the read and the wait
            // would otherwise go unnoticed and this would wait for a slot it already had.
            let retired = self.registry.retirement_signal();
            let freed = retired.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();

            let active = self.registry.active_ids().await;
            let over = active.len().saturating_sub(room_for_one);
            if over == 0 {
                return Ok(());
            }

            // Already-stopping sessions ignore this; what frees their slot is their process ending.
            for session_id in active.iter().take(over) {
                self.stop(
                    session_id,
                    TerminalOutcome::Evicted(ExecEvictionReason::CapacityExceeded),
                )
                .await;
            }

            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                // Refused rather than admitted over the ceiling. Admitting "just one more" is what
                // makes the ceiling meaningless: the process that would not die is still there on
                // the next call, which times out the same way and admits one more again, and the
                // count climbs without bound on exactly the machine least able to take it. A limit
                // that yields whenever it is inconvenient is not a limit.
                tracing::warn!(
                    active = active.len(),
                    max_sessions,
                    "refusing a session: every slot is held by a process that has not ended"
                );
                return Err(ExecError::AtCapacity { max_sessions });
            }
        }
    }

    /// Fetches the current lifecycle state of a session.
    pub async fn get_session_state(&self, session_id: &ExecSessionId) -> Option<ExecSessionState> {
        let session = self.registry.get(session_id).await?;
        let state = session.lock().await.state.clone();
        Some(state)
    }

    /// Fetches an output summary snapshot of a session.
    pub async fn get_output_summary(
        &self,
        session_id: &ExecSessionId,
    ) -> Option<ExecOutputSummary> {
        let session = self.registry.get(session_id).await?;
        let summary = session.lock().await.summary();
        Some(summary)
    }

    /// Returns the output positions not yet delivered through the interactive-tool path.
    ///
    /// A regular [`Self::read_output`] caller does not affect these positions: it supplies and
    /// owns its own [`ExecCursor`].
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnknownSession`] if the manager no longer retains the identifier.
    pub async fn interactive_output_cursors(
        &self,
        session_id: &ExecSessionId,
    ) -> Result<(u64, u64), ExecError> {
        let session =
            self.registry
                .get(session_id)
                .await
                .ok_or_else(|| ExecError::UnknownSession {
                    session_id: session_id.clone(),
                })?;
        let sess = session.lock().await;
        Ok((
            sess.interactive_stdout_cursor,
            sess.interactive_stderr_cursor,
        ))
    }

    /// Records output through these positions as delivered by the interactive tool.
    ///
    /// Positions advance only and are clamped to output the session has actually produced. That
    /// makes a delayed or repeated completion unable to move the cursor backward or skip output
    /// by naming a future offset.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnknownSession`] if the manager no longer retains the identifier.
    pub async fn mark_interactive_output_delivered(
        &self,
        session_id: &ExecSessionId,
        stdout_cursor: u64,
        stderr_cursor: u64,
    ) -> Result<(), ExecError> {
        let session =
            self.registry
                .get(session_id)
                .await
                .ok_or_else(|| ExecError::UnknownSession {
                    session_id: session_id.clone(),
                })?;
        let mut sess = session.lock().await;
        let stdout_limit = u64::try_from(sess.stdout_buffer.total_bytes()).unwrap_or(u64::MAX);
        let stderr_limit = u64::try_from(sess.stderr_buffer.total_bytes()).unwrap_or(u64::MAX);
        sess.interactive_stdout_cursor = sess
            .interactive_stdout_cursor
            .max(stdout_cursor.min(stdout_limit));
        sess.interactive_stderr_cursor = sess
            .interactive_stderr_cursor
            .max(stderr_cursor.min(stderr_limit));
        Ok(())
    }

    /// Reads output produced at or after the cursor's position.
    ///
    /// `Ok(None)` means the stream has produced nothing past the cursor yet. The returned cursor is
    /// where the next read resumes; it is not `offset + text.len()`, because a stream that has
    /// overrun its capture ceiling reports the omitted bytes in place of delivering them.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnknownSession`] for an identifier this manager does not hold, and
    /// [`ExecError::UnaddressableStream`] for a cursor that names more than one stream.
    pub async fn read_output(
        &self,
        session_id: &ExecSessionId,
        cursor: ExecCursor,
    ) -> Result<Option<(String, ExecCursor)>, ExecError> {
        let session =
            self.registry
                .get(session_id)
                .await
                .ok_or_else(|| ExecError::UnknownSession {
                    session_id: session_id.clone(),
                })?;

        let mut sess = session.lock().await;
        let buffer = match cursor.stream() {
            ExecStreamKind::Stdout => &sess.stdout_buffer,
            ExecStreamKind::Stderr => &sess.stderr_buffer,
            other => {
                return Err(ExecError::UnaddressableStream {
                    stream: stream_label(other),
                });
            }
        };

        let offset = usize::try_from(cursor.offset()).unwrap_or(usize::MAX);
        let Some((text, next_offset)) = buffer.read_from(offset) else {
            return Ok(None);
        };
        // A poll is interaction: a session someone is reading from is not an idle session, and the
        // idle sweep would otherwise take it away while it was being watched.
        sess.last_active_at = Instant::now();
        let next = ExecCursor::new(cursor.stream(), next_offset as u64);
        Ok(Some((text, next)))
    }

    /// Waits until either output stream advances beyond the supplied cursors, the session exits,
    /// or `timeout` elapses.
    ///
    /// This is the companion to [`Self::read_output`] for an interactive client. The cursors make
    /// the wait about output produced after one particular interaction rather than about a session
    /// that may have been printing for minutes. A timeout is a successful wait with no news; use
    /// [`Self::read_output`] afterward to obtain whichever stream data arrived.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnknownSession`] if the manager no longer retains the identifier.
    pub async fn wait_for_output(
        &self,
        session_id: &ExecSessionId,
        stdout_cursor: u64,
        stderr_cursor: u64,
        timeout: Duration,
    ) -> Result<(), ExecError> {
        let session =
            self.registry
                .get(session_id)
                .await
                .ok_or_else(|| ExecError::UnknownSession {
                    session_id: session_id.clone(),
                })?;

        // `Notify::notify_waiters` does not retain a permit. Arm both waiters before inspecting
        // the buffers, then inspect them under the session lock: output landing on either side of
        // that inspection is therefore either visible in the snapshot or wakes this call.
        let (output_notify, exit_notify) = {
            let sess = session.lock().await;
            (
                Arc::clone(&sess.output_notify),
                Arc::clone(&sess.exit_notify),
            )
        };
        let output_ready = output_notify.notified();
        let exit_ready = exit_notify.notified();
        tokio::pin!(output_ready);
        tokio::pin!(exit_ready);
        output_ready.as_mut().enable();
        exit_ready.as_mut().enable();

        {
            let sess = session.lock().await;
            if sess.stdout_buffer.total_bytes()
                > usize::try_from(stdout_cursor).unwrap_or(usize::MAX)
                || sess.stderr_buffer.total_bytes()
                    > usize::try_from(stderr_cursor).unwrap_or(usize::MAX)
                || sess.state.is_terminal()
            {
                return Ok(());
            }
        }

        tokio::select! {
            () = &mut output_ready => {}
            () = &mut exit_ready => {}
            () = tokio::time::sleep(timeout) => {}
        }
        Ok(())
    }

    /// Delivers interactive standard input or an interrupt to a running session.
    ///
    /// An interrupt is `SIGINT` to the process group, not a kill: a program that installs a handler
    /// gets to run it, which is the difference between stopping a test run and losing its report.
    ///
    /// A write waits for the process to read, which for a process that never reads is forever. It
    /// therefore happens with the session's own lock released and only the standard input lock
    /// held, so that a stalled write blocks nothing but the next write — a cancellation arriving
    /// mid-write is what ends the stall, and it could not do that from behind the same lock.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnknownSession`], [`ExecError::SessionNotActive`],
    /// [`ExecError::StdinClosed`], or [`ExecError::StdinWrite`].
    pub async fn write_stdin(
        &self,
        session_id: &ExecSessionId,
        chars: Option<&str>,
        is_interrupt: bool,
        emitter: Option<&HostEventEmitter>,
    ) -> Result<(), ExecError> {
        let session =
            self.registry
                .get(session_id)
                .await
                .ok_or_else(|| ExecError::UnknownSession {
                    session_id: session_id.clone(),
                })?;

        let stdin_slot = {
            let mut sess = session.lock().await;
            if !sess.state.is_active() {
                return Err(ExecError::SessionNotActive {
                    session_id: session_id.clone(),
                    state: sess.state.clone(),
                });
            }
            sess.last_active_at = Instant::now();
            if is_interrupt {
                sess.request_termination(TerminationSignal::Interrupt);
            }
            Arc::clone(&sess.child_stdin)
        };

        if is_interrupt {
            if let Some(em) = emitter {
                let _ = em.emit(ExecEvent::TerminalInteraction(
                    TerminalInteractionEvent::new(session_id.clone(), 0, true),
                ));
            }
            return Ok(());
        }

        let Some(input) = chars.filter(|text| !text.is_empty()) else {
            return Ok(());
        };

        {
            let mut slot = stdin_slot.lock().await;
            let stdin = slot.as_mut().ok_or_else(|| ExecError::StdinClosed {
                session_id: session_id.clone(),
            })?;
            // Written verbatim, with no newline appended: the caller decides whether the line is
            // finished, and a program reading raw keystrokes must see exactly what was sent.
            stdin
                .write_all(input.as_bytes())
                .await
                .map_err(|source| ExecError::StdinWrite {
                    session_id: session_id.clone(),
                    source,
                })?;
            stdin
                .flush()
                .await
                .map_err(|source| ExecError::StdinWrite {
                    session_id: session_id.clone(),
                    source,
                })?;
        }

        // Recorded after the write lands, so the timestamp says when input reached the process
        // rather than when someone started trying to send it.
        session.lock().await.last_active_at = Instant::now();
        if let Some(em) = emitter {
            let _ = em.emit(ExecEvent::TerminalInteraction(
                TerminalInteractionEvent::new(session_id.clone(), input.len(), false),
            ));
        }
        Ok(())
    }

    /// Explicitly cancels a session, terminating its process group.
    ///
    /// Returns as soon as the signal is on its way. The process is given [`DRAIN_GRACE`] to exit on
    /// its own before it is killed, so the session may still be winding down when this returns —
    /// what has already happened is that its recorded death is a cancellation and nothing else.
    pub async fn cancel(&self, session_id: &ExecSessionId) {
        self.stop(session_id, TerminalOutcome::Cancelled).await;
    }

    /// Evicts a session due to host policy, terminating its process group.
    ///
    /// The eviction event is emitted by the session's own supervisor once the process is gone, on
    /// the emitter the session was started with — not here. One session has one death, and the
    /// supervisor is the only thing positioned to say what it was.
    pub async fn evict(&self, session_id: &ExecSessionId, reason: ExecEvictionReason) {
        self.stop(session_id, TerminalOutcome::Evicted(reason))
            .await;
    }

    /// Records a session's death and asks its supervisor to deliver it.
    ///
    /// Deliberately does not retire the session. The recorded state says why it is ending and the
    /// signal is on its way, but the process group survives until it stops or the grace runs out —
    /// and until then it is still occupying the capacity it was admitted against.
    ///
    /// The state written here is the death the session will be reported as having, because
    /// [`ExecSession::finish`] refuses to overwrite one. A deadline that expires while the process
    /// is still draining loses to the reason that already stopped it.
    async fn stop(&self, session_id: &ExecSessionId, outcome: TerminalOutcome) {
        let Some(session) = self.registry.get(session_id).await else {
            return;
        };
        let never_ran = {
            let mut sess = session.lock().await;
            if sess.state.is_terminal() {
                return;
            }
            sess.request_termination(TerminationSignal::Terminate);
            let recorded = sess.finish(match outcome {
                TerminalOutcome::Cancelled => ExecSessionState::Cancelled,
                TerminalOutcome::Evicted(reason) => ExecSessionState::Expired { reason },
            });
            if !recorded {
                // A session taken away before its process existed cannot be recorded as expired:
                // the machine admits that only from a run that started. `Cancelled` is what is
                // true of it — it never ran.
                sess.finish(ExecSessionState::Cancelled);
            }
            !recorded
        };
        if never_ran {
            // No process, so no supervisor will ever reap it and free its slot.
            self.registry.retire(session_id).await;
        }
    }

    /// Returns the sessions whose process may still be running, oldest first.
    ///
    /// A session that has been told to stop stays here until its process is actually gone, because
    /// until then its process really may still be running.
    pub async fn active_sessions(&self) -> Vec<ExecSessionId> {
        self.registry.active_ids().await
    }
}

/// The three standard streams, taken off the child before anything else can read them.
struct Pipes {
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

/// Everything the supervisor needs that is not the child process itself.
struct SupervisorContext {
    session: Arc<Mutex<ExecSession>>,
    registry: Arc<SessionRegistry>,
    session_id: ExecSessionId,
    emitter: Option<HostEventEmitter>,
    pid: Option<u32>,
    total_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    /// Closed once the process is gone, which is what lets a reader on the other side see EOF.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

/// What watching one process to its end produced.
struct ProcessOutcome {
    status: std::io::Result<std::process::ExitStatus>,
    /// Set when a deadline took the process away rather than it stopping by itself.
    expiry: Option<ExecEvictionReason>,
    /// Whether anything asked for this process to stop. It decides whether the group is swept once
    /// the leader is gone: a command left to finish on its own may legitimately have backgrounded
    /// something, but a command that was told to stop has no such claim.
    termination_requested: bool,
}

/// Waits for the process, delivering signals and enforcing both deadlines while it runs.
///
/// The deadlines live here rather than in a sweeper the host has to remember to run. A timeout that
/// fires only when something else happens to call a reaper is a timeout in name only, and this task
/// is already awake for the whole life of the process.
async fn watch_process(
    child: &mut Child,
    terminate_rx: &mut mpsc::UnboundedReceiver<TerminationSignal>,
    context: &SupervisorContext,
) -> ProcessOutcome {
    let mut outcome = ProcessOutcome {
        status: Ok(std::process::ExitStatus::default()),
        expiry: None,
        termination_requested: false,
    };
    let mut hard_kill_at: Option<TokioInstant> = None;
    let mut total_deadline = context
        .total_timeout
        .map(|timeout| TokioInstant::now() + timeout);
    let mut idle_deadline = context
        .idle_timeout
        .map(|timeout| TokioInstant::now() + timeout);
    let mut accepting_requests = true;

    outcome.status = loop {
        let escalation_at = hard_kill_at;
        let total_at = total_deadline;
        let idle_at = idle_deadline;
        tokio::select! {
            status = child.wait() => break status,
            request = terminate_rx.recv(), if accepting_requests => match request {
                Some(TerminationSignal::Interrupt) => {
                    signal_group(Some(child), context.pid, TerminationSignal::Interrupt);
                }
                Some(_) => {
                    hard_kill_at = Some(begin_termination(child, context.pid, &mut outcome));
                }
                // Every sender is gone, so nothing can ask again; keep waiting for the process
                // rather than spinning on a channel that will only ever answer immediately.
                None => accepting_requests = false,
            },
            () = sleep_until(escalation_at) => {
                // To the group, not to the leader. `SIGTERM` reached everything the shell started,
                // and a process that chose to ignore it is exactly the one that must not be left
                // behind because the escalation went somewhere narrower than the signal it follows.
                signal_group(Some(child), context.pid, TerminationSignal::Kill);
                hard_kill_at = None;
            }
            () = sleep_until(total_at) => {
                outcome.expiry = Some(ExecEvictionReason::TotalTimeout);
                total_deadline = None;
                hard_kill_at = Some(begin_termination(child, context.pid, &mut outcome));
            }
            () = sleep_until(idle_at) => {
                // Re-checked against the session rather than trusted: output and interactive input
                // both count as activity, and either may have arrived since this timer was armed.
                let idle_for = {
                    let sess = context.session.lock().await;
                    Instant::now().saturating_duration_since(sess.last_active_at)
                };
                match context.idle_timeout.and_then(|limit| limit.checked_sub(idle_for)) {
                    Some(remaining) if !remaining.is_zero() => {
                        idle_deadline = Some(TokioInstant::now() + remaining);
                    }
                    _ => {
                        outcome.expiry = Some(ExecEvictionReason::IdleTimeout);
                        idle_deadline = None;
                        hard_kill_at = Some(begin_termination(child, context.pid, &mut outcome));
                    }
                }
            }
        }
    };
    outcome
}

/// Starts the drain protocol: `SIGTERM` to the group now, `SIGKILL` to it once the grace elapses.
///
/// Returns when that escalation is due, which the caller arms.
fn begin_termination(
    child: &mut Child,
    pid: Option<u32>,
    outcome: &mut ProcessOutcome,
) -> TokioInstant {
    outcome.termination_requested = true;
    signal_group(Some(child), pid, TerminationSignal::Terminate);
    TokioInstant::now() + DRAIN_GRACE
}

/// Owns one child process from spawn to reaped, and is the only thing that signals it.
async fn supervise(
    mut child: Child,
    mut terminate_rx: mpsc::UnboundedReceiver<TerminationSignal>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    context: SupervisorContext,
) {
    let ProcessOutcome {
        status,
        expiry,
        termination_requested,
    } = watch_process(&mut child, &mut terminate_rx, &context).await;

    if termination_requested {
        // The leader is reaped, and anything it started that ignored `SIGTERM` is still running.
        // Addressing the group by the leader's identifier after the reap is sound precisely while
        // it matters: a group identifier is not reused while the group still has members, and an
        // empty group answers with an error this call discards.
        signal_group(None, context.pid, TerminationSignal::Kill);
    }

    // Bounded, because the pipes stay open as long as anything the command started still holds
    // them. Waiting forever on a daemon the command left behind would hold back the result of a
    // process that has already exited.
    if tokio::time::timeout(DRAIN_GRACE, async {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    })
    .await
    .is_err()
    {
        // No output is lost by aborting: whatever was read is already in the buffers.
        tracing::debug!(
            session_id = %context.session_id,
            "output readers outlived the process; abandoning the remaining pipe"
        );
    }

    let exit_code = status
        .as_ref()
        .ok()
        .and_then(std::process::ExitStatus::code);
    let (recorded_state, duration_ms, stdout_bytes, stderr_bytes, notify) = {
        let mut sess = context.session.lock().await;
        sess.exit_code = exit_code;
        // Only when nothing has ended this session yet. A cancelled session ended when it was
        // cancelled; overwriting that with the moment its process finally died would charge the
        // command for the grace period it was given to stop.
        sess.ended_at.get_or_insert_with(Instant::now);
        sess.terminate = None;
        let terminal = match expiry {
            Some(reason) => ExecSessionState::Expired { reason },
            None => ExecSessionState::Exited { exit_code },
        };
        // A no-op when the session was already cancelled or evicted, which is the point: the
        // process died because something asked it to, and the exit it then produced is a
        // consequence rather than the cause. A deadline that expired while the process was
        // draining loses here too, which is why the event below is built from what was recorded
        // rather than from the deadline this task happened to observe.
        sess.finish(terminal);
        (
            sess.state.clone(),
            duration_to_millis(sess.duration()),
            sess.stdout_buffer.total_bytes(),
            sess.stderr_buffer.total_bytes(),
            Arc::clone(&sess.exit_notify),
        )
    };

    // Dropped with no session lock held. Holding the write half open is what keeps a reader like
    // `cat` from ever seeing end of file, and a write that was in flight when the process died
    // returns immediately with a broken pipe, so this waits on nothing worth waiting for.
    *context.stdin.lock().await = None;

    context.registry.retire(&context.session_id).await;

    if let Some(em) = &context.emitter {
        // Exactly one eviction event per session, carrying the reason that actually won. Emitting
        // where the eviction was *requested* instead produced two of them whenever a deadline
        // expired during the drain: one for the reason that was recorded and one for the reason
        // that lost, disagreeing about how the same session died.
        if let ExecSessionState::Expired { reason } = recorded_state {
            let _ = em.emit(ExecEvent::Evicted(ExecEvictedEvent::new(
                context.session_id.clone(),
                reason,
            )));
        }
        let mut event = ExecExitedEvent::new(
            context.session_id.clone(),
            duration_ms,
            stdout_bytes,
            stderr_bytes,
        );
        if let Some(code) = exit_code {
            event = event.with_exit_code(code);
        }
        let _ = em.emit(ExecEvent::Exited(event));
    }
    notify.notify_waiters();
}

/// Reads one pipe into the session's buffer, emitting each chunk as it arrives.
fn spawn_stream_reader(
    reader: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    session: Arc<Mutex<ExecSession>>,
    stream: ExecStreamKind,
    session_id: ExecSessionId,
    emitter: Option<HostEventEmitter>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut reader) = reader else {
            return;
        };
        let mut buffer = [0_u8; STREAM_CHUNK_BYTES];
        let mut decoder = Utf8ChunkDecoder::new();
        // This task is the only writer of its stream's buffer, so its own count is the buffer's
        // offset — and unlike the buffer it can be read at end of stream without taking a lock.
        let mut produced = 0_usize;
        let mut truncated = false;
        loop {
            let read = match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let chunk = &buffer[..read];
            // Decoded across chunk boundaries, not per chunk: a 4 KiB read lands wherever it lands,
            // and converting each one on its own turns every multi-byte character unlucky enough to
            // straddle the boundary into replacement characters in the event stream.
            let held_before = decoder.held_bytes();
            let text = decoder.push(chunk);
            let held_after = decoder.held_bytes();
            let notify = {
                let mut sess = session.lock().await;
                let sink = match stream {
                    ExecStreamKind::Stderr => &mut sess.stderr_buffer,
                    _ => &mut sess.stdout_buffer,
                };
                sink.push_bytes(chunk);
                truncated = sink.is_truncated();
                sess.last_active_at = Instant::now();
                Arc::clone(&sess.output_notify)
            };
            // The event describes the bytes its text actually covers, which starts at whatever the
            // decoder was holding back from the previous read and ends wherever it is holding back
            // now. Reporting this read's own span instead would put the event a character ahead of
            // the text it carries.
            let text_offset = produced.saturating_sub(held_before);
            let text_bytes = read.saturating_add(held_before).saturating_sub(held_after);
            produced = produced.saturating_add(read);
            emit_output(
                emitter.as_ref(),
                &session_id,
                stream,
                text_offset,
                text_bytes,
                text,
                truncated,
            );
            notify.notify_waiters();
        }

        // Whatever the decoder was still holding cannot be completed now, and it is already in the
        // capture buffer. Releasing it keeps the events and the captured output describing the same
        // bytes, which is the whole reason the decoder holds anything back.
        let held = decoder.held_bytes();
        let tail = decoder.flush();
        emit_output(
            emitter.as_ref(),
            &session_id,
            stream,
            produced.saturating_sub(held),
            held,
            tail,
            truncated,
        );
    })
}

/// Emits one output event, unless there is no text to describe.
fn emit_output(
    emitter: Option<&HostEventEmitter>,
    session_id: &ExecSessionId,
    stream: ExecStreamKind,
    offset: usize,
    bytes: usize,
    text: String,
    truncated: bool,
) {
    let Some(em) = emitter else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let _ = em.emit(ExecEvent::Output(
        ExecOutputEvent::new(session_id.clone(), stream, offset as u64, bytes, text)
            .with_truncated(truncated),
    ));
}

/// Builds and starts the child process for a request.
fn spawn_child(request: &ExecRequest) -> std::io::Result<Child> {
    let shell = request.shell().unwrap_or_else(|| Path::new(DEFAULT_SHELL));
    let mut cmd = Command::new(shell);
    if request.login() {
        cmd.arg("-l");
    }
    cmd.arg("-c");
    cmd.arg(request.command());
    // POSIX gives the words after the command string to the shell as `$0`, `$1`, and so on, which
    // is the only way a caller can pass data to a command without it going through quoting.
    cmd.args(request.args());

    if let Some(cwd) = request.cwd() {
        cmd.current_dir(cwd);
    }

    // Set before the caller's own variables, so a request can still override them deliberately.
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "dumb");
    cmd.env("PAGER", "cat");
    for (key, val) in request.env() {
        cmd.env(key, val);
    }

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    // Its own process group, so that terminating the shell terminates everything it started.
    #[cfg(unix)]
    cmd.process_group(0);

    cmd.spawn()
}

fn emit_started(
    emitter: Option<&HostEventEmitter>,
    session_id: &ExecSessionId,
    request: &ExecRequest,
    pid: Option<u32>,
) {
    let Some(em) = emitter else {
        return;
    };
    let mut started = ExecStartedEvent::new(session_id.clone(), request.command().to_owned())
        .with_args(request.args().to_vec())
        .with_pty(request.pty());
    if let Some(cwd) = request.cwd() {
        started = started.with_cwd(cwd.to_string_lossy().to_string());
    }
    if let Some(pid) = pid {
        started = started.with_pid(pid);
    }
    let _ = em.emit(ExecEvent::Started(started));
}

/// Sends a signal to the child's process group, falling back to the child alone.
///
/// The fallback matters on two paths that look nothing alike: a platform with no process groups,
/// and a process that has already been reaped, where the identifier no longer names anything and
/// signalling it could reach whatever inherited the number.
#[cfg(unix)]
fn signal_group(child: Option<&mut Child>, pid: Option<u32>, signal: TerminationSignal) {
    use rustix::process::{Pid, Signal, kill_process_group};

    let group = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw);
    let Some(group) = group else {
        kill_leader(child);
        return;
    };
    let signal = match signal {
        TerminationSignal::Interrupt => Signal::INT,
        TerminationSignal::Terminate => Signal::TERM,
        TerminationSignal::Kill => Signal::KILL,
    };
    if kill_process_group(group, signal).is_err() {
        kill_leader(child);
    }
}

/// Without process groups there is one blunt instrument, and an interrupt is not it.
#[cfg(not(unix))]
fn signal_group(child: Option<&mut Child>, _pid: Option<u32>, signal: TerminationSignal) {
    if signal != TerminationSignal::Interrupt {
        kill_leader(child);
    }
}

/// The fallback when the group cannot be addressed: kill the one process this build still holds.
fn kill_leader(child: Option<&mut Child>) {
    if let Some(child) = child {
        let _ = child.start_kill();
    }
}

/// Waits until `at`, or forever when there is nothing to wait for.
async fn sleep_until(at: Option<TokioInstant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

const fn stream_label(stream: ExecStreamKind) -> &'static str {
    match stream {
        ExecStreamKind::Stdout => "stdout",
        ExecStreamKind::Stderr => "stderr",
        ExecStreamKind::Combined => "combined",
        _ => "unknown",
    }
}

/// Decodes a byte stream into text without splitting characters at read boundaries.
///
/// Holds back at most the three bytes of an incomplete trailing character and prepends them to the
/// next chunk. Bytes that are not valid UTF-8 at all are replaced immediately rather than held,
/// because a stream that is binary would otherwise never release anything.
struct Utf8ChunkDecoder {
    pending: Vec<u8>,
}

impl Utf8ChunkDecoder {
    const fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// How many bytes are being held back for the next read.
    fn held_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Decodes a chunk, holding back only an incomplete character at its very end.
    ///
    /// Every byte is accounted for. An earlier version stopped at the first invalid sequence and
    /// returned the valid prefix, silently dropping the rest of the chunk — so one stray byte in
    /// the middle of a build log cost the event stream everything after it, while the captured
    /// output kept all of it and the two no longer described the same command.
    fn push(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);

        let mut text = String::with_capacity(bytes.len());
        let mut rest = bytes.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    // Already validated by the call above; the fallback is unreachable and exists
                    // only because this crate does not unwrap.
                    text.push_str(std::str::from_utf8(&rest[..valid_up_to]).unwrap_or_default());
                    // `Some` is a sequence that can never become valid, whatever arrives next;
                    // `None` is an incomplete character at the very end, kept for the next read.
                    let Some(invalid) = error.error_len() else {
                        self.pending.extend_from_slice(&rest[valid_up_to..]);
                        break;
                    };
                    text.push(char::REPLACEMENT_CHARACTER);
                    rest = &rest[valid_up_to.saturating_add(invalid)..];
                }
            }
        }
        text
    }

    /// Releases whatever was being held back, once no further bytes can complete it.
    ///
    /// Called at end of stream. Without it the last one to three bytes of a stream that ends
    /// mid-character never reach any event, while the captured output has them.
    fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let pending = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&pending).into_owned()
    }
}

#[inline]
const fn duration_to_millis(d: Duration) -> u64 {
    let millis = d.as_millis();
    if millis > u64::MAX as u128 {
        u64::MAX
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            millis as u64
        }
    }
}
