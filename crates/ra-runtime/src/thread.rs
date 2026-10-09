//! A live thread: a conversation whose turns the host starts, steers and interrupts while they
//! run. Ported from Codex's `CodexThread` (`core/src/codex_thread.rs`) and the parts of its
//! session that decide what submitted input does (`core/src/session/turn_input.rs`), how a turn
//! is aborted (`core/src/tasks/mod.rs`, `Session::interrupt_task`) and how thread settings apply
//! (`core/src/session/thread_settings.rs`).
//!
//! # Turns
//!
//! A Codex turn is one run here. [`AgentThread::start_or_steer_turn`] starts a run on the thread's
//! history followed by the new input when the thread is idle, and otherwise steers the running one:
//! the input is delivered into it at its next model-call boundary (see
//! [`RunRequest::with_pending_input`]). [`AgentThread::steer_turn`] only steers, and only the turn
//! the caller names. When a run ends, the thread keeps what it produced as its history, so the next
//! turn continues the conversation — the history of a run that returned no result is what it
//! settled before it stopped, as a spawned agent's is (see [`crate::agent::control`]).
//!
//! A run that stops to ask for approval keeps its turn, as Codex keeps a turn active while an
//! approval is pending: the checkpoint waits in the thread ([`AgentThread::paused_run`]) until the
//! host answers it through the ordinary [`RunState`] approval API and continues it with
//! [`AgentThread::resume_turn`]. Input steered meanwhile is delivered once it continues.
//!
//! # Interrupting
//!
//! [`AgentThread::interrupt`] cancels the running turn, whether it is working or waiting for
//! approval, and the turn ends aborted, as Codex's `abort_all_tasks` ends it with
//! `TurnAborted`. Input steered into it and not yet delivered is dropped, as Codex clears the
//! turn's pending input. The history keeps the turns the run settled, followed by the marker
//! telling the model the run was interrupted unless [`RunConfig::with_interrupt_message`] turned it
//! off.
//!
//! # Events
//!
//! What happens to the thread's turns is reported in order through [`AgentThread::next_event`], as
//! Codex's thread reports it through `next_event`: a turn's start, then its end — completed, after
//! an error if it failed, or aborted. A turn that starts after another has ended is reported after
//! that end.
//!
//! # Settings
//!
//! The thread's model and reasoning effort apply to every turn it starts; the input that starts or
//! steers a turn may change them first, and [`AgentThread::update_settings`] changes them on their
//! own. Codex applies such a change from the next turn on, and so does the thread: a turn that is
//! already running keeps the settings it started with. [`AgentThread::update_turn_settings`]
//! instead publishes an immutable configuration for subsequent steps of that active turn alone.
//! Each inference step captures it once, including the tools answering that model call. Model
//! changes require explicit [`AgentThreadBuilder::with_step_model_switching`] selection, with the
//! same disabled default as Codex. Product settings arrive through a host-prepared configuration;
//! reasoning summary and service tier use existing provider-specific model-setting extras.
//!
//! # Shutting down
//!
//! [`AgentThread::shutdown`] ends the thread's life, as Codex's `Op::Shutdown` does: input is
//! refused from then on, a running turn is aborted as an interrupt aborts it, and once it has
//! ended the rollout is flushed. What the thread's tools left running outside it — background
//! terminals among them — belongs to the host, which stops it alongside.
//!
//! # What this does not port
//!
//! - Product policy lowering: sandbox and approval policies, permission profiles and reviewer
//!   routing belong to the host adapter. Working directory and collaboration-mode changes are
//!   not implemented by this thread.
//! - Review and compaction turns, which cannot be steered in Codex, a turn's output schema, and
//!   additional context: this thread runs only regular turns on user input.
//! - Server draining, the admission of turns Codex refuses while its host shuts down, and the
//!   automatic, trigger and continuation starts that only its own background work issues.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use ra_core::{
    cancel::{CancelReason, CancelScope, ScopeKind},
    item::{ContentBlock, ItemId, Message, MessageRole, ModelInputItem, RunItem, RunItemKind},
    model::{Effort, ModelResolver, ModelSettings, ResolvedModel},
    session::{
        InterruptedTurnHistoryMarker, SessionId,
        interrupt::is_interrupt,
        rollout::{PersistContext, RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded},
    },
    state::{RunId, RunState},
    tool::ToolServices,
};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::{
    agent::{AgentBinding, control::partial_history},
    runner::{
        ContinuationInput, LiveRunSettings, PendingInput, RunConfig, RunOutcome, RunRequest,
        RunStreamEvent, Runner,
    },
};

/// Why a turn was aborted: Codex's `TurnAbortReason`, of which the thread raises one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAbortReason {
    /// The host interrupted it.
    Interrupted,
}

/// Something that happened to one of the thread's turns: the turn events of Codex's `EventMsg`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadEvent {
    /// Persistent thread settings were applied. Consumers read the current thread snapshot.
    ThreadSettingsApplied,
    /// A turn started: Codex's `TurnStarted`.
    TurnStarted {
        /// The turn.
        turn_id: String,
        /// When it started, in seconds since the Unix epoch.
        started_at: i64,
    },
    /// The turn failed, and will complete next: Codex's `Error` event within a turn.
    Error {
        /// The turn.
        turn_id: String,
        /// What went wrong.
        message: String,
    },
    /// A turn ended without being aborted, whether or not it failed: Codex's `TurnComplete`.
    TurnComplete {
        /// The turn.
        turn_id: String,
        /// The text of its final answer, if it gave one.
        last_agent_message: Option<String>,
        /// When it ended, in seconds since the Unix epoch.
        completed_at: i64,
        /// How long it ran.
        duration_ms: i64,
    },
    /// A turn was aborted: Codex's `TurnAborted`.
    TurnAborted {
        /// The turn.
        turn_id: String,
        /// Why.
        reason: TurnAbortReason,
        /// When it ended, in seconds since the Unix epoch.
        completed_at: i64,
        /// How long it ran.
        duration_ms: i64,
    },
}

/// The outcome of publishing settings for one running turn: Codex's
/// `TurnSettingsUpdateOutcome`, for the settings supported by this runtime.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnSettingsUpdateOutcome {
    /// Subsequent steps capture the published settings.
    Applied,
    /// The target turn is absent, ending or cancelled.
    TargetUnavailable,
    /// Preparation failed without changing the target or future thread defaults.
    Rejected {
        /// Why the update could not be accepted.
        reason: String,
    },
}

/// Why submitted input was not accepted: Codex's `NotSubmittedReason`, for the turns this thread
/// runs.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotSubmittedReason {
    /// Steering found no running turn, or one that has already finished reading input.
    NoActiveTurn,
    /// Steering found a different turn running.
    ExpectedTurnMismatch {
        /// The turn the caller named.
        expected: String,
        /// The turn running.
        actual: String,
    },
    /// Steering was given no input.
    EmptyInput,
    /// The thread has been shut down and takes no more input. Codex's thread refuses it by having
    /// no session loop left to submit to (`CodexErr::InternalAgentDied`); this thread has no loop of
    /// its own, so it says why.
    ShutDown,
}

/// What the thread did with input given to [`AgentThread::start_or_steer_turn`]: Codex's
/// `TurnInputSubmission`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnInputSubmission {
    /// A turn started; the settings the input carried apply to it.
    Started {
        /// The turn.
        turn_id: String,
    },
    /// The running turn was steered; the settings the input carried apply from the next turn.
    Steered {
        /// The turn.
        turn_id: String,
    },
    /// The input was refused, and its settings were not applied.
    NotSubmitted {
        /// Why.
        reason: NotSubmittedReason,
    },
}

/// What the thread did with input given to [`AgentThread::steer_turn`]: Codex's
/// `SteerSubmission`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerSubmission {
    /// The running turn was steered; the settings the input carried apply from the next turn.
    Steered {
        /// The turn.
        turn_id: String,
    },
    /// The input was refused, and its settings were not applied.
    NotSubmitted {
        /// Why.
        reason: NotSubmittedReason,
    },
}

/// Changes to a thread's settings for the turns it starts from now on: Codex's
/// `ThreadSettingsOverrides`, for the settings a run here has. Unset fields leave a setting as it
/// is.
#[must_use]
#[derive(Debug, Clone, Default)]
pub struct ThreadSettingsOverrides {
    model: Option<String>,
    effort: Option<Effort>,
    model_settings: Option<ModelSettings>,
    /// A product adapter's prepared configuration; runtime-owned knobs are still merged below.
    config: Option<RunConfig>,
}

impl ThreadSettingsOverrides {
    /// Changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs the turns on `model`.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Runs the turns with `effort`.
    pub const fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Installs model settings prepared by an adapter on the existing run-override layer.
    pub fn with_model_settings(mut self, settings: ModelSettings) -> Self {
        self.model_settings = Some(settings);
        self
    }

    /// Applies a product adapter's previewed configuration when input is admitted. The adapter
    /// clones the current configuration, changing only its product-owned settings.
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The model it sets, if it sets one.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The effort it sets, if it sets one.
    #[must_use]
    pub const fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// Whether it changes nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.effort.is_none()
            && self.model_settings.is_none()
            && self.config.is_none()
    }

    fn apply(&self, config: &mut RunConfig) {
        if self.is_empty() {
            return;
        }
        let mut updated = self
            .config
            .clone()
            .unwrap_or_else(|| std::mem::take(config));
        if let Some(settings) = &self.model_settings {
            updated = updated.with_model_settings(settings.clone());
        }
        if let Some(model) = &self.model {
            updated = updated.with_model(model.clone());
        }
        if let Some(effort) = self.effort {
            let model_settings = updated.model_settings().clone().with_effort(effort);
            updated = updated.with_model_settings(model_settings);
        }
        *config = updated;
    }
}

/// The settings a thread starts its turns with.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSettings {
    model: Option<String>,
    effort: Option<Effort>,
}

impl ThreadSettings {
    /// The model its turns run on, or `None` for the one the agent's resolver chooses.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The reasoning effort its turns run with, if one is set.
    #[must_use]
    pub const fn effort(&self) -> Option<Effort> {
        self.effort
    }
}

/// Input for a turn and the settings it carries: Codex's `TurnInputRequest`, with user input as
/// the only kind of input.
#[must_use]
#[derive(Debug, Clone, Default)]
pub struct TurnInputRequest {
    content: Vec<ContentBlock>,
    thread_settings: ThreadSettingsOverrides,
}

impl TurnInputRequest {
    /// User input made of `content`, sent as one message. Empty content starts a turn that answers
    /// the history as it is, and cannot steer one.
    pub fn new(content: Vec<ContentBlock>) -> Self {
        Self {
            content,
            thread_settings: ThreadSettingsOverrides::new(),
        }
    }

    /// Changes the thread's settings once the input is accepted: for the turn it starts, or from
    /// the next turn if it steers.
    pub fn with_thread_settings(mut self, thread_settings: ThreadSettingsOverrides) -> Self {
        self.thread_settings = thread_settings;
        self
    }

    fn message(&self) -> Option<Message> {
        (!self.content.is_empty()).then(|| Message::new(MessageRole::User, self.content.clone()))
    }
}

/// Builds an [`AgentThread`].
#[must_use]
pub struct AgentThreadBuilder {
    step_model_switching: bool,
    thread_id: SessionId,
    agent: AgentBinding,
    model_resolver: Arc<dyn ModelResolver>,
    config: RunConfig,
    services: ToolServices,
    history: Vec<ModelInputItem>,
    rollout: Option<Arc<dyn RolloutRecorder>>,
}

impl AgentThreadBuilder {
    /// Opts into Codex's experimental step-model-switching capability, disabled by default.
    pub const fn with_step_model_switching(mut self, enabled: bool) -> Self {
        self.step_model_switching = enabled;
        self
    }
    /// Runs every turn with `config`, whose model and reasoning effort are the thread's initial
    /// settings.
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = config;
        self
    }

    /// Gives every turn `services`.
    pub fn with_services(mut self, services: ToolServices) -> Self {
        self.services = services;
        self
    }

    /// Starts the thread on `history`, as a thread resumed from its rollout continues from the
    /// history rebuilt from it. A thread with a rollout recorder takes it as already recorded.
    pub fn with_history(mut self, history: Vec<ModelInputItem>) -> Self {
        self.history = history;
        self
    }

    /// Records every turn into the thread's rollout through `recorder`: only each turn's new input
    /// and what the turn produces, since the rollout already holds the thread's history.
    pub fn with_rollout_recorder(mut self, recorder: Arc<dyn RolloutRecorder>) -> Self {
        self.rollout = Some(recorder);
        self
    }

    /// The thread, idle.
    #[must_use]
    pub fn build(self) -> AgentThread {
        let (events, receiver) = mpsc::unbounded_channel();
        AgentThread {
            inner: Arc::new(ThreadInner {
                step_model_switching: self.step_model_switching,
                thread_id: self.thread_id,
                agent: self.agent,
                model_resolver: self.model_resolver,
                services: self.services,
                rollout: self.rollout,
                state: Mutex::new(ThreadState {
                    history: self.history,
                    config: self.config,
                    active_turn: None,
                    shut_down: false,
                }),
                turn_ended: Notify::new(),
                events,
                receiver: tokio::sync::Mutex::new(receiver),
            }),
        }
    }
}

/// A live thread: Codex's `CodexThread`. Cloning shares the thread.
#[derive(Clone)]
pub struct AgentThread {
    inner: Arc<ThreadInner>,
}

impl std::fmt::Debug for AgentThread {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentThread")
            .field("thread_id", &self.inner.thread_id)
            .field("running", &self.is_running())
            .finish_non_exhaustive()
    }
}

struct ThreadInner {
    step_model_switching: bool,
    thread_id: SessionId,
    agent: AgentBinding,
    model_resolver: Arc<dyn ModelResolver>,
    services: ToolServices,
    rollout: Option<Arc<dyn RolloutRecorder>>,
    state: Mutex<ThreadState>,
    /// Woken when a turn ends, for a start that waits for a finishing turn to be gone.
    turn_ended: Notify,
    events: mpsc::UnboundedSender<ThreadEvent>,
    receiver: tokio::sync::Mutex<mpsc::UnboundedReceiver<ThreadEvent>>,
}

struct ThreadState {
    history: Vec<ModelInputItem>,
    /// The configuration the next turn starts with, the thread's settings among it.
    config: RunConfig,
    active_turn: Option<ActiveTurn>,
    /// Set by [`AgentThread::shutdown`]: no input is taken any more.
    shut_down: bool,
}

/// The running turn: Codex's `ActiveTurn`.
struct ActiveTurn {
    turn_id: String,
    pending: PendingInput,
    settings: LiveRunSettings,
    cancel: CancelScope,
    /// Set by an interrupt, so the turn ends aborted however its run comes back.
    interrupted: bool,
    /// The run ended and its history is being persisted before its terminal event.
    ending: bool,
    /// The checkpoint of a run waiting for approval, and where to send it once answered.
    paused: Option<PausedTurn>,
}

struct PausedTurn {
    state: RunState,
    resume: oneshot::Sender<RunState>,
}

impl AgentThread {
    /// A thread of `thread_id` whose turns `agent` runs on models `model_resolver` resolves.
    pub fn builder(
        thread_id: SessionId,
        agent: AgentBinding,
        model_resolver: Arc<dyn ModelResolver>,
    ) -> AgentThreadBuilder {
        AgentThreadBuilder {
            step_model_switching: false,
            thread_id,
            agent,
            model_resolver,
            config: RunConfig::new(),
            services: ToolServices::new(),
            history: Vec::new(),
            rollout: None,
        }
    }

    /// The thread's identity.
    #[must_use]
    pub fn thread_id(&self) -> &SessionId {
        &self.inner.thread_id
    }

    /// Whether a turn is running: Codex's `AgentStatus::Running`.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.inner.lock().active_turn.is_some()
    }

    /// The running turn, if there is one.
    #[must_use]
    pub fn active_turn_id(&self) -> Option<String> {
        self.inner
            .lock()
            .active_turn
            .as_ref()
            .map(|turn| turn.turn_id.clone())
    }

    /// The settings the next turn starts with.
    #[must_use]
    pub fn settings(&self) -> ThreadSettings {
        let state = self.inner.lock();
        ThreadSettings {
            model: state.config.model().map(str::to_owned),
            effort: state.config.model_settings().effort(),
        }
    }

    /// Whether the host enabled model-setting publications within a turn.
    #[must_use]
    pub fn step_model_switching_enabled(&self) -> bool {
        self.inner.step_model_switching
    }

    /// A snapshot for a host adapter to preview product configuration without mutating the thread.
    pub fn run_config(&self) -> RunConfig {
        self.inner.lock().config.clone()
    }

    /// The current settings of a live target, independent of future thread defaults.
    #[must_use]
    pub fn turn_config(&self, turn_id: &str) -> Option<RunConfig> {
        self.inner
            .lock()
            .active_turn
            .as_ref()
            .filter(|active| {
                active.turn_id == turn_id
                    && !active.cancel.is_cancelled()
                    && !active.pending.is_closed()
            })
            .and_then(|active| active.settings.capture())
    }

    /// Records the host adapter's thread-settings snapshot without putting product fields into
    /// the model history. Codex persists `ThreadSettingsApplied` as an event, too; the open host
    /// event family is the Rust adaptation for a snapshot whose schema belongs to the host.
    pub async fn persist_settings_snapshot(
        &self,
        snapshot: serde_json::Value,
    ) -> ra_core::error::Result<()> {
        use ra_core::event::{HostEvent, HostEventBody};
        if let Some(rollout) = &self.inner.rollout {
            let allocator = RunState::start(RunId::new(uuid::Uuid::now_v7().to_string()))
                .restore_event_seq_allocator(None);
            rollout.record(RolloutItem::Event(HostEvent::allocate(
                &allocator,
                self.inner.agent.public_id().clone(),
                HostEventBody::Unknown {
                    family: "threadSettingsApplied".into(),
                    data: snapshot,
                },
            )?));
            rollout.persist(PersistContext::Standard).await?;
        }
        Ok(())
    }

    /// Resolves a selector for an adapter's provider-specific settings projection.
    pub fn resolve_model(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        self.inner.model_resolver.resolve_model(model)
    }

    /// The thread's history: what the next turn starts from.
    #[must_use]
    pub fn history(&self) -> Vec<ModelInputItem> {
        self.inner.lock().history.clone()
    }

    /// Starts a turn on `request` if the thread is idle, and otherwise steers the running turn
    /// with it: Codex's `start_or_steer_turn`.
    ///
    /// A turn that has finished reading input but not yet ended is waited for, and a new turn
    /// started after it, so the input is not lost to a run that will not read it.
    ///
    /// Must be called within a Tokio runtime, on which the turn runs.
    pub async fn start_or_steer_turn(&self, request: TurnInputRequest) -> TurnInputSubmission {
        loop {
            let ended = {
                let mut state = self.inner.lock();
                if state.shut_down {
                    return TurnInputSubmission::NotSubmitted {
                        reason: NotSubmittedReason::ShutDown,
                    };
                }
                let Some(active) = &state.active_turn else {
                    let turn_id = self.start_turn(&mut state, &request);
                    return TurnInputSubmission::Started { turn_id };
                };
                let Some(message) = request.message() else {
                    return TurnInputSubmission::NotSubmitted {
                        reason: NotSubmittedReason::EmptyInput,
                    };
                };
                if active.pending.push(vec![message]).is_ok() {
                    let turn_id = active.turn_id.clone();
                    request.thread_settings.apply(&mut state.config);
                    if !request.thread_settings.is_empty() {
                        self.inner.emit(ThreadEvent::ThreadSettingsApplied);
                    }
                    return TurnInputSubmission::Steered { turn_id };
                }
                // Registered before the lock is let go, so the end cannot slip past unseen.
                let mut ended = Box::pin(self.inner.turn_ended.notified());
                ended.as_mut().enable();
                ended
            };
            ended.await;
        }
    }

    /// Steers the running turn with `request` if it is `expected_turn_id`: Codex's `steer_turn`.
    #[must_use]
    pub fn steer_turn(
        &self,
        request: &TurnInputRequest,
        expected_turn_id: &str,
    ) -> SteerSubmission {
        let mut state = self.inner.lock();
        if state.shut_down {
            return SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::ShutDown,
            };
        }
        let Some(active) = &state.active_turn else {
            return SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::NoActiveTurn,
            };
        };
        if active.turn_id != expected_turn_id {
            return SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::ExpectedTurnMismatch {
                    expected: expected_turn_id.to_owned(),
                    actual: active.turn_id.clone(),
                },
            };
        }
        let Some(message) = request.message() else {
            return SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::EmptyInput,
            };
        };
        if active.pending.push(vec![message]).is_err() {
            // The run has finished reading input; the turn ends without it.
            return SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::NoActiveTurn,
            };
        }
        let turn_id = active.turn_id.clone();
        request.thread_settings.apply(&mut state.config);
        if !request.thread_settings.is_empty() {
            self.inner.emit(ThreadEvent::ThreadSettingsApplied);
        }
        SteerSubmission::Steered { turn_id }
    }

    /// Aborts the running turn, if there is one: Codex's `Op::Interrupt`. Returns whether there
    /// was. The turn reports that it was aborted once its run has stopped.
    #[must_use = "an interrupt with no running turn does nothing"]
    pub fn interrupt(&self) -> bool {
        let mut state = self.inner.lock();
        let Some(active) = state.active_turn.as_mut() else {
            return false;
        };
        if active.ending {
            return false;
        }
        active.interrupted = true;
        active.cancel.cancel(CancelReason::UserInterrupt);
        true
    }

    /// Changes the settings of the turns the thread starts from now on: Codex's
    /// `Op::ThreadSettings`. A running turn keeps its own.
    pub fn update_settings(&self, overrides: &ThreadSettingsOverrides) {
        overrides.apply(&mut self.inner.lock().config);
        if !overrides.is_empty() {
            self.inner.emit(ThreadEvent::ThreadSettingsApplied);
        }
    }

    /// Publishes settings for the named turn alone, as Codex's `apply_turn_settings` does.
    /// A step already in progress and future thread defaults remain unchanged.
    #[must_use]
    pub fn update_turn_settings(
        &self,
        turn_id: &str,
        overrides: &ThreadSettingsOverrides,
    ) -> TurnSettingsUpdateOutcome {
        if !self.inner.step_model_switching
            && (overrides.is_empty()
                || overrides.model.is_some()
                || overrides.effort.is_some()
                || overrides.model_settings.is_some())
        {
            return TurnSettingsUpdateOutcome::Rejected {
                reason: "turn settings updates require the step_model_switching feature".into(),
            };
        }
        let state = self.inner.lock();
        let Some(active) = state.active_turn.as_ref().filter(|active| {
            active.turn_id == turn_id
                && !active.cancel.is_cancelled()
                && !active.pending.is_closed()
        }) else {
            return TurnSettingsUpdateOutcome::TargetUnavailable;
        };
        let settings = active.settings.clone();
        drop(state);
        if let Some(model) = overrides.model()
            && let Err(error) = self.inner.model_resolver.resolve_model(Some(model))
        {
            return TurnSettingsUpdateOutcome::Rejected {
                reason: error.to_string(),
            };
        }
        let state = self.inner.lock();
        if !state.active_turn.as_ref().is_some_and(|active| {
            active.turn_id == turn_id
                && !active.cancel.is_cancelled()
                && !active.pending.is_closed()
                && !active.ending
        }) {
            return TurnSettingsUpdateOutcome::TargetUnavailable;
        }
        if settings.update(|config| overrides.apply(config)) {
            TurnSettingsUpdateOutcome::Applied
        } else {
            TurnSettingsUpdateOutcome::TargetUnavailable
        }
    }

    /// The checkpoint of the running turn's run, if it is waiting for approval.
    #[must_use]
    pub fn paused_run(&self) -> Option<RunState> {
        self.inner
            .lock()
            .active_turn
            .as_ref()
            .and_then(|turn| turn.paused.as_ref())
            .map(|paused| paused.state.clone())
    }

    /// Continues the running turn's run from `state`, its checkpoint with the approvals answered.
    ///
    /// # Errors
    ///
    /// Returns `state` unchanged if the running turn is not waiting for approval, or `state` is
    /// not the checkpoint of its run.
    pub fn resume_turn(&self, state: RunState) -> std::result::Result<(), Box<RunState>> {
        let mut guard = self.inner.lock();
        let Some(active) = guard.active_turn.as_mut() else {
            return Err(Box::new(state));
        };
        let matches = active
            .paused
            .as_ref()
            .is_some_and(|paused| paused.state.run_id() == state.run_id());
        if !matches {
            return Err(Box::new(state));
        }
        let Some(paused) = active.paused.take() else {
            return Err(Box::new(state));
        };
        paused.resume.send(state).map_err(Box::new)
    }

    /// Shuts the thread down: Codex's `shutdown_and_wait`. Returns once it is down.
    ///
    /// Input is refused from now on ([`NotSubmittedReason::ShutDown`]). A running turn — working or
    /// waiting for approval — is aborted as [`AgentThread::interrupt`] aborts it, as Codex aborts
    /// its tasks on shutdown, and is waited for; then the rollout is flushed. Shutting down a thread
    /// that is already down only waits for that.
    pub async fn shutdown(&self) {
        loop {
            let ended = {
                let mut state = self.inner.lock();
                state.shut_down = true;
                let Some(active) = state.active_turn.as_mut() else {
                    break;
                };
                if !active.ending && !active.interrupted {
                    active.interrupted = true;
                    active.cancel.cancel(CancelReason::UserInterrupt);
                }
                // Registered before the lock is let go, so the end cannot slip past unseen.
                let mut ended = Box::pin(self.inner.turn_ended.notified());
                ended.as_mut().enable();
                ended
            };
            ended.await;
        }
        if let Some(rollout) = &self.inner.rollout
            && let Err(error) = rollout.flush().await
        {
            tracing::warn!(thread_id = %self.inner.thread_id, %error, "the thread's rollout could not be flushed");
        }
    }

    /// Whether [`AgentThread::shutdown`] has been called.
    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.inner.lock().shut_down
    }

    /// The next thing that happened to the thread's turns: Codex's `next_event`. Waits for it.
    ///
    /// Events are read once, by whichever caller asks first.
    pub async fn next_event(&self) -> Option<ThreadEvent> {
        self.inner.receiver.lock().await.recv().await
    }

    /// Starts a turn on `request`, under the lock, so its start is reported before anything a
    /// later caller does with the thread.
    fn start_turn(&self, state: &mut ThreadState, request: &TurnInputRequest) -> String {
        request.thread_settings.apply(&mut state.config);
        if !request.thread_settings.is_empty() {
            self.inner.emit(ThreadEvent::ThreadSettingsApplied);
        }
        let turn_id = uuid::Uuid::now_v7().to_string();
        let cancel = CancelScope::root().child(ScopeKind::Run);
        let pending = PendingInput::new();
        let settings = LiveRunSettings::new(state.config.clone());
        state.active_turn = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            pending: pending.clone(),
            settings: settings.clone(),
            cancel: cancel.clone(),
            interrupted: false,
            ending: false,
            paused: None,
        });
        let started_at = unix_seconds();
        self.inner.emit(ThreadEvent::TurnStarted {
            turn_id: turn_id.clone(),
            started_at,
        });
        let turn = StartedTurn {
            turn_id: turn_id.clone(),
            history: state.history.clone(),
            input: request.message(),
            config: state.config.clone(),
            pending,
            settings,
            cancel,
        };
        tokio::spawn(run_turn(Arc::clone(&self.inner), turn));
        turn_id
    }
}

impl ThreadInner {
    fn lock(&self) -> MutexGuard<'_, ThreadState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: ThreadEvent) {
        // The receiver lives as long as the thread, so a send cannot fail while anyone could read.
        let _ = self.events.send(event);
    }
}

/// What a turn runs on, fixed when it starts.
struct StartedTurn {
    turn_id: String,
    history: Vec<ModelInputItem>,
    input: Option<Message>,
    config: RunConfig,
    pending: PendingInput,
    settings: LiveRunSettings,
    cancel: CancelScope,
}

/// How a turn's run ended.
enum TurnEnd {
    /// It completed, with this final answer, if any.
    Completed(Option<String>),
    /// It failed.
    Failed(String),
    /// It was cancelled.
    Cancelled,
}

/// Runs a turn to its end and reports how it ended.
async fn run_turn(inner: Arc<ThreadInner>, turn: StartedTurn) {
    let started = Instant::now();
    let StartedTurn {
        turn_id,
        history,
        input,
        config,
        pending,
        settings,
        cancel,
    } = turn;
    let recorded = history.len();
    let mut run_input = history;
    run_input.extend(input.map(ModelInputItem::Message));
    let (end, history) = run_to_end(
        &inner, &turn_id, config, &pending, &settings, &cancel, run_input, recorded,
    )
    .await;
    settings.close();
    // What the run never read is dropped if the turn was aborted, as Codex clears it; otherwise it
    // stays in the history, as Codex records what is left when its task finishes.
    let leftover = pending.close();

    // Close the queue and freeze the terminal decision under the thread lock. Persistence may
    // await without letting a late interrupt reinterpret which inputs the history keeps.
    let interrupted = {
        let mut state = inner.lock();
        let Some(active) = state.active_turn.as_mut() else {
            tracing::error!(%turn_id, "a running turn lost its active state");
            return;
        };
        let interrupted = active.interrupted;
        active.ending = true;
        interrupted
    };
    let mut history = history;
    if !interrupted && !leftover.is_empty() {
        for message in leftover {
            if let Some(rollout) = &inner.rollout {
                rollout.record(RolloutItem::Item(RunItem::new(
                    ItemId::new(uuid::Uuid::now_v7().to_string()),
                    RunItemKind::Message(message.clone()),
                )));
            }
            history.push(ModelInputItem::Message(message));
        }
        if let Some(rollout) = &inner.rollout
            && let Err(error) = rollout.persist(PersistContext::Standard).await
        {
            tracing::warn!(%turn_id, %error, "the thread's pending input could not be persisted");
        }
    }
    let completed_at = unix_seconds();
    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let mut state = inner.lock();
    state.history = history;
    state.active_turn = None;
    if interrupted {
        inner.emit(ThreadEvent::TurnAborted {
            turn_id,
            reason: TurnAbortReason::Interrupted,
            completed_at,
            duration_ms,
        });
    } else {
        let last_agent_message = match end {
            TurnEnd::Completed(message) => message,
            TurnEnd::Failed(message) => {
                inner.emit(ThreadEvent::Error {
                    turn_id: turn_id.clone(),
                    message,
                });
                None
            }
            // Cancelled from outside the thread: it ends as a turn that stopped, not one the host
            // asked to abort.
            TurnEnd::Cancelled => None,
        };
        inner.emit(ThreadEvent::TurnComplete {
            turn_id,
            last_agent_message,
            completed_at,
            duration_ms,
        });
    }
    drop(state);
    inner.turn_ended.notify_waiters();
}

/// Runs a turn's run to its end — across any number of pauses for approval — and returns how it
/// ended and the thread's history after it. A spawned agent's runs are driven the same way (see
/// [`crate::agent::control`]).
#[allow(clippy::too_many_arguments)]
async fn run_to_end(
    inner: &Arc<ThreadInner>,
    turn_id: &str,
    mut config: RunConfig,
    pending: &PendingInput,
    settings: &LiveRunSettings,
    cancel: &CancelScope,
    input: Vec<ModelInputItem>,
    recorded: usize,
) -> (TurnEnd, Vec<ModelInputItem>) {
    let mut fallback = input.clone();
    let mut request = RunRequest::new(
        inner.agent.clone(),
        Arc::clone(&inner.model_resolver),
        RunId::new(turn_id),
        cancel.clone(),
        input,
    )
    .with_recorded_input(recorded);
    loop {
        request = request
            .with_config(config.clone())
            .with_services(inner.services.clone())
            .with_pending_input(pending.clone());
        request = request.with_live_settings(settings.clone());
        if let Some(rollout) = &inner.rollout {
            request = request.with_rollout_recorder(Arc::clone(rollout));
        }
        let mut stream = Runner::run_streamed(request);
        let mut produced = Vec::new();
        while let Some(event) = stream.next_event().await {
            if let RunStreamEvent::Item(item) = event {
                produced.push(item);
            }
        }
        let result = match stream.finish().await {
            Ok(result) => result,
            Err(error) => {
                let history = partial_history(fallback, &produced);
                if cancel.is_cancelled() {
                    return (TurnEnd::Cancelled, history);
                }
                return (TurnEnd::Failed(error.to_string()), history);
            }
        };
        let history = result.continuation_input(ContinuationInput::Normalized);
        if let RunOutcome::Completed { .. } = result.outcome() {
            let message = result.final_message().map(Message::text_content);
            return (TurnEnd::Completed(message), history);
        }

        // Outstanding tools retain the step that issued them across an approval pause.
        config = settings.captured_step().unwrap_or(config);
        // Paused for approval: the turn stays active until the host answers or interrupts.
        let state = result.state().clone();
        let run_id = state.run_id().clone();
        let (resume, answered) = oneshot::channel();
        if let Some(active) = inner.lock().active_turn.as_mut() {
            active.paused = Some(PausedTurn { state, resume });
        }
        let answered = tokio::select! {
            answered = answered => answered.ok(),
            () = cancel.cancelled() => None,
        };
        if let Some(active) = inner.lock().active_turn.as_mut() {
            active.paused = None;
        }
        let Some(state) = answered else {
            let history = cancel_paused(inner, &config, cancel, run_id, history).await;
            return (TurnEnd::Cancelled, history);
        };
        fallback = history;
        request = RunRequest::new(
            inner.agent.clone(),
            Arc::clone(&inner.model_resolver),
            state.run_id().clone(),
            cancel.clone(),
            Vec::new(),
        )
        .with_state(state);
    }
}

/// Ends a run cancelled while it waited for approval, as the runner ends one cancelled while it
/// works: after the interrupted-run marker, which the returned history keeps, and recorded as
/// cancelled. Codex aborts a turn waiting for approval the same way it aborts one that is working.
async fn cancel_paused(
    inner: &ThreadInner,
    config: &RunConfig,
    cancel: &CancelScope,
    run_id: RunId,
    mut history: Vec<ModelInputItem>,
) -> Vec<ModelInputItem> {
    let marker = cancel
        .reason()
        .is_some_and(|reason| is_interrupt(&reason))
        .then(|| {
            InterruptedTurnHistoryMarker::from_settings(config.interrupt_message(), false).item()
        })
        .flatten();
    if let Some(item) = marker.as_ref().and_then(RunItem::to_model_input) {
        history.push(item);
    }
    if let Some(rollout) = &inner.rollout {
        if let Some(marker) = marker {
            rollout.record(RolloutItem::Item(marker));
        }
        rollout.record(RolloutItem::RunEnded(RolloutRunEnded::new(
            run_id.clone(),
            RolloutRunEnd::Cancelled,
        )));
        if let Err(error) = rollout.flush().await {
            tracing::warn!(run_id = %run_id, %error, "the thread's rollout could not be flushed");
        }
    }
    history
}

/// Seconds since the Unix epoch, as Codex stamps its turn events.
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}
