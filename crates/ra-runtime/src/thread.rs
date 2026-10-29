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
//! Codex's thread reports it through `next_event`: a turn's start, the records it adds to the
//! thread as it adds them, then its end — completed, after an error if it failed, or aborted. A
//! turn that starts after another has ended is reported after that end.
//!
//! The records are the ones the thread's rollout keeps, in the order it keeps them: the input the
//! turn started on, under ids [`turn_input_item_id`] gives it, then what the run produced and the
//! input steered into it, then whatever the turn's end adds — input it never read, or the marker an
//! interrupt leaves. Records are observed at the rollout boundary, including tool calls recorded
//! before execution; settlement may replace a record under the same id. This keeps the SDK's
//! run-item stream unchanged while supplying Codex-style early starts to the thread listener.
//! Host execution events are forwarded too, including background exits after the turn ended.
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
//! # Output schema
//!
//! A turn may be started with a JSON schema for its final answer
//! ([`TurnStartOptions::with_final_output_json_schema`]), as Codex's `final_output_json_schema`.
//! It holds for that turn alone: every model call of the turn's run is asked for it, in strict
//! mode and under Codex's name for it ([`TURN_OUTPUT_SCHEMA_NAME`]), in place of what the agent
//! declares; the next turn goes back to the agent's declaration unless it is given one too. Input
//! that would steer a running turn and names a schema is taken only if the turn runs with an equal
//! one ([`NotSubmittedReason::ActiveTurnOutputSchemaMismatch`]), compared as JSON values, so the
//! order of an object's keys does not matter. Like Codex, the thread leaves checking the schema to
//! the provider and does not validate the answer against it.
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
//! - Review and compaction turns, which cannot be steered in Codex, and additional context: this
//!   thread runs only regular turns on user input.
//! - Server draining, the admission of turns Codex refuses while its host shuts down, and the
//!   automatic, trigger and continuation starts that only its own background work issues.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use ra_core::{
    cancel::{CancelReason, CancelScope, ScopeKind},
    event::HostEvent,
    item::{ContentBlock, ItemId, Message, MessageRole, ModelInputItem, RunItem, RunItemKind},
    model::{Effort, ModelResolver, ModelSettings, ResolvedModel},
    output::OutputSchema,
    session::{
        InterruptedTurnHistoryMarker, SessionId,
        interrupt::is_interrupt,
        rollout::{
            PersistContext, RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded,
            RolloutRunStarted,
        },
    },
    state::{EventSeqAllocator, RunId, RunState},
    tool::ToolServices,
};
use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::{
    agent::{
        AgentBinding,
        control::{AgentControl, partial_history},
    },
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
// An event is moved once, from the thread to whoever reads it; boxing the record would only add an
// allocation to every item a turn reports.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
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
    /// The turn added `item` to the thread: what Codex's `ItemStarted` and `ItemCompleted` report,
    /// as a record rather than as its display.
    Item {
        /// The turn.
        turn_id: String,
        /// The record, as the thread's rollout keeps it.
        item: RunItem,
    },
    /// An incomplete message or reasoning item opened; never written to the rollout.
    ItemStarted {
        /// The turn.
        turn_id: String,
        /// The incomplete display item.
        item: RunItem,
    },
    /// Text narration from the current model call, never written to the rollout.
    Delta {
        /// The turn.
        turn_id: String,
        /// The item and text part this narration belongs to.
        delta: ra_core::model::ModelStreamDelta,
    },
    /// A model request failed and is being sent again: Codex's `StreamError`. Intermediate,
    /// so the turn does not fail on it.
    StreamError {
        /// The turn.
        turn_id: String,
        /// What the client is told, such as `Reconnecting... 1/3`.
        message: String,
        /// The error that caused the retry.
        additional_details: String,
    },
    /// A host execution event, including a background command ending after its turn.
    HostEvent(HostEvent),
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
    /// The turn's run stopped on approvals and waits for the host to answer them: its checkpoint
    /// is [`AgentThread::paused_run`] until it is resumed or the turn is interrupted.
    TurnAwaitingApproval {
        /// The turn.
        turn_id: String,
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
    /// Steering named an output schema other than the one the running turn uses.
    ActiveTurnOutputSchemaMismatch,
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

/// The id under which the turn `turn_id` reports the message at `index` of the input it started
/// on. The rollout keeps a run's input without ids; a reader rebuilding the turn from it gives the
/// message the same id the live thread reported it under.
#[must_use]
pub fn turn_input_item_id(turn_id: &str, index: usize) -> ItemId {
    ItemId::new(format!("{turn_id}-input-{index}"))
}

/// The name a turn's output schema is sent to the provider under: the one Codex gives it.
pub const TURN_OUTPUT_SCHEMA_NAME: &str = "codex_output_schema";

/// Options that apply only if the input starts a turn: Codex's `TurnStartOptions`, for the options
/// a turn here has.
///
/// When the input steers a running turn instead, the turn's own options stay, and an output
/// schema given here must equal the turn's for the input to be taken.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnStartOptions {
    final_output_json_schema: Option<Value>,
}

impl TurnStartOptions {
    /// No options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Constrains the final answer of the turn to `schema`, a JSON schema.
    pub fn with_final_output_json_schema(mut self, schema: Value) -> Self {
        self.final_output_json_schema = Some(schema);
        self
    }

    /// The schema the turn's final answer is constrained to, if any.
    #[must_use]
    pub const fn final_output_json_schema(&self) -> Option<&Value> {
        self.final_output_json_schema.as_ref()
    }
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
    start: TurnStartOptions,
}

impl TurnInputRequest {
    /// User input made of `content`, sent as one message. Empty content starts a turn that answers
    /// the history as it is, and cannot steer one.
    pub fn new(content: Vec<ContentBlock>) -> Self {
        Self {
            content,
            thread_settings: ThreadSettingsOverrides::new(),
            start: TurnStartOptions::new(),
        }
    }

    /// Changes the thread's settings once the input is accepted: for the turn it starts, or from
    /// the next turn if it steers.
    pub fn with_thread_settings(mut self, thread_settings: ThreadSettingsOverrides) -> Self {
        self.thread_settings = thread_settings;
        self
    }

    /// Options for the turn the input starts, if it starts one: Codex's `on_start`.
    pub fn on_start(mut self, start: TurnStartOptions) -> Self {
        self.start = start;
        self
    }

    /// Whether the input may steer `active`: it names no output schema, or the one `active` uses.
    /// Codex compares the two as JSON values.
    fn fits_output_schema_of(&self, active: &ActiveTurn) -> bool {
        self.start
            .final_output_json_schema
            .as_ref()
            .is_none_or(|schema| active.final_output_json_schema.as_ref() == Some(schema))
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
    session_start_source: ra_core::hook::SessionStartSource,
    rollout: Option<Arc<dyn RolloutRecorder>>,
    agent_control: Option<AgentControl>,
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

    /// Owns this thread's collaboration tree and binds every turn, including approval resumes,
    /// to its root. Shutdown cancels and waits for the tree's background agents; interrupting a
    /// turn leaves them alive, following the control tree's cancellation policy.
    pub fn with_agent_control(mut self, control: AgentControl) -> Self {
        self.agent_control = Some(control);
        self
    }

    /// The services every turn will be given, for a host to add a port of its own to before the
    /// thread is built.
    pub const fn services(&self) -> &ToolServices {
        &self.services
    }

    /// Starts the thread on `history`, as a thread resumed from its rollout continues from the
    /// history rebuilt from it. A thread with a rollout recorder takes it as already recorded.
    pub fn with_history(mut self, history: Vec<ModelInputItem>) -> Self {
        self.history = history;
        self.session_start_source = ra_core::hook::SessionStartSource::Resume;
        self
    }

    /// Sets the lifecycle source after selecting the initial history.
    pub const fn with_session_start_source(
        mut self,
        source: ra_core::hook::SessionStartSource,
    ) -> Self {
        self.session_start_source = source;
        self
    }

    /// Records every turn into the thread's rollout through `recorder`: only each turn's new input
    /// and what the turn produces, since the rollout already holds the thread's history.
    pub fn with_rollout_recorder(mut self, recorder: Arc<dyn RolloutRecorder>) -> Self {
        self.rollout = Some(recorder);
        self
    }

    /// Applies `overrides` to the settings the thread starts with, as Codex applies the overrides
    /// a thread is started or resumed with to the configuration it builds the session from. Unlike
    /// [`AgentThread::update_settings`] it reports nothing: the thread does not exist yet.
    pub fn with_settings(mut self, overrides: &ThreadSettingsOverrides) -> Self {
        overrides.apply(&mut self.config);
        self
    }

    /// The thread whose builder this is.
    #[must_use]
    pub const fn thread_id(&self) -> &SessionId {
        &self.thread_id
    }

    /// The configuration the thread will run its turns with, for a host adapter to prepare
    /// product settings on before the thread is built.
    pub const fn config(&self) -> &RunConfig {
        &self.config
    }

    /// The agent the thread's turns will run, for a host to check what the thread is loaded with
    /// before it is built.
    #[must_use]
    pub const fn agent(&self) -> &AgentBinding {
        &self.agent
    }

    /// Runs the thread's turns with `agent` instead, as a host that prepares the agent further
    /// before the thread is built — giving it tools its client provides, say — binds the prepared
    /// instance in place of the one the thread was made with.
    pub fn with_agent(mut self, agent: AgentBinding) -> Self {
        self.agent = agent;
        self
    }

    /// Resolves `model` as the thread will, so a model it cannot run on is refused before the
    /// thread is built.
    ///
    /// # Errors
    ///
    /// Returns the resolver's error for a model it does not know.
    pub fn resolve_model(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        self.model_resolver.resolve_model(model)
    }

    /// The thread, idle.
    #[must_use]
    pub fn build(self) -> AgentThread {
        let hooks = self
            .config
            .user_hooks()
            .clone()
            .with_session_id(self.thread_id.clone());
        let config = self.config.with_user_hooks(hooks);
        let (events, receiver) = mpsc::unbounded_channel();
        AgentThread {
            inner: Arc::new(ThreadInner {
                step_model_switching: self.step_model_switching,
                thread_id: self.thread_id,
                agent: self.agent,
                model_resolver: self.model_resolver,
                services: self.services,
                rollout: self.rollout,
                agent_control: self.agent_control,
                state: Mutex::new(ThreadState {
                    history: self.history,
                    config,
                    active_turn: None,
                    shut_down: false,
                    pending_session_start: Some(self.session_start_source),
                    session_ended: false,
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
    agent_control: Option<AgentControl>,
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
    pending_session_start: Option<ra_core::hook::SessionStartSource>,
    session_ended: bool,
}

/// The running turn: Codex's `ActiveTurn`.
struct ActiveTurn {
    turn_id: String,
    pending: PendingInput,
    settings: LiveRunSettings,
    cancel: CancelScope,
    /// The schema the turn's final answer is constrained to, which input steering it must match.
    final_output_json_schema: Option<Value>,
    /// Set by an interrupt, so the turn ends aborted however its run comes back.
    interrupted: bool,
    /// The run ended and its history is being persisted before its terminal event.
    ending: bool,
    /// The checkpoint of a run waiting for approval, and where to send it once answered.
    paused: Option<PausedTurn>,
}

struct PausedTurn {
    state: RunState,
    resume: oneshot::Sender<(AgentBinding, RunState)>,
    /// The step configuration that issued the calls waiting on approval.
    config: RunConfig,
    /// Shared with live tool emitters and every continuation of this run.
    event_seqs: EventSeqAllocator,
    /// Where the host's answers are recorded while the run waits for them.
    recorder: Arc<ThreadRecorder>,
}

impl AgentThread {
    /// The collaboration tree owned by this thread, when it has one. Hosts use it to route
    /// spawned agents' approval checkpoints independently of the parent turn's lifecycle.
    #[must_use]
    pub fn agent_control(&self) -> Option<&AgentControl> {
        self.inner.agent_control.as_ref()
    }
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
            session_start_source: ra_core::hook::SessionStartSource::Startup,
            rollout: None,
            agent_control: None,
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
        if let Some(rollout) = &self.inner.rollout {
            self.record_settings_snapshot(snapshot)?;
            rollout.persist(PersistContext::Standard).await?;
        }
        Ok(())
    }

    /// Records the settings snapshot as [`AgentThread::persist_settings_snapshot`] does, but
    /// leaves it queued until the rollout is next written, so a thread whose rollout is not
    /// written yet is not written for it: Codex records the settings a thread was started or
    /// resumed with alongside its next turn rather than before it.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot be made into an event.
    pub fn record_settings_snapshot(
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
        }
        Ok(())
    }

    /// Resolves a selector for an adapter's provider-specific settings projection.
    pub fn resolve_model(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        self.inner.model_resolver.resolve_model(model)
    }

    /// The model settings declared by the agent instance that executes this thread.
    #[must_use]
    pub fn agent_model_settings(&self) -> &ModelSettings {
        self.inner.agent.execution().model_settings()
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
                if !request.fits_output_schema_of(active) {
                    return TurnInputSubmission::NotSubmitted {
                        reason: NotSubmittedReason::ActiveTurnOutputSchemaMismatch,
                    };
                }
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
        if !request.fits_output_schema_of(active) {
            return SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::ActiveTurnOutputSchemaMismatch,
            };
        }
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

    /// The approval checkpoint and issuing step's configuration for `turn_id`, captured together.
    /// Returns nothing once that turn is interrupted, ending, or no longer waiting for approval.
    #[must_use]
    pub fn paused_run_with_config(&self, turn_id: &str) -> Option<(RunState, RunConfig)> {
        let guard = self.inner.lock();
        let active = guard.active_turn.as_ref()?;
        if active.turn_id != turn_id || active.cancel.is_cancelled() || active.ending {
            return None;
        }
        let paused = active.paused.as_ref()?;
        let mut state = paused.state.clone();
        state.snapshot_event_seq(&paused.event_seqs);
        Some((state, paused.config.clone()))
    }

    /// Continues the running turn's run from `state`, its checkpoint with the approvals answered.
    /// Handoff targets are resolved through the issuing step's agent registry because checkpoints
    /// store stable identities rather than the SDK's current-agent object.
    /// The starting agent reuses the thread's binding, including its prepared execution surface.
    ///
    /// # Errors
    ///
    /// Returns `state` unchanged if the running turn is not waiting for approval, `state` is not
    /// the checkpoint of its run, or its current agent is absent or cannot be resolved. Resolution
    /// failure leaves the turn waiting for approval so the caller can retry.
    pub fn resume_turn(&self, state: RunState) -> std::result::Result<(), Box<RunState>> {
        let mut state = state;
        let mut guard = self.inner.lock();
        let Some(active) = guard.active_turn.as_mut() else {
            return Err(Box::new(state));
        };
        let Some(paused) = active
            .paused
            .as_ref()
            .filter(|paused| same_approval_pause(&paused.state, &state))
        else {
            return Err(Box::new(state));
        };
        let agent = match state.current_agent() {
            Some(current) if current == self.inner.agent.public_id() => self.inner.agent.clone(),
            Some(current) => {
                let Some(agent) = paused.config.agent_registry().get(current) else {
                    return Err(Box::new(state));
                };
                AgentBinding::direct(Arc::clone(agent))
            }
            None => return Err(Box::new(state)),
        };
        let Some(paused) = active.paused.take() else {
            return Err(Box::new(state));
        };
        state.snapshot_event_seq(&paused.event_seqs);
        paused
            .resume
            .send((agent, state))
            .map_err(|(_, state)| Box::new(state))
    }

    /// Records how the host answered the approval of a call of the running turn's paused run,
    /// before `state`, that run's checkpoint, is resumed with the answer: the event joins the
    /// turn's rollout and is reported as [`ThreadEvent::HostEvent`], so the answer is kept however
    /// the turn then ends. It draws from the allocator shared with this run's live tool emitters
    /// and snapshots its bound into `state` for the resumed run.
    ///
    /// # Errors
    ///
    /// Fails if the running turn is not waiting for approval, or `state` is not the checkpoint of
    /// its run.
    pub fn record_approval(
        &self,
        state: &mut RunState,
        event: ra_core::event::ApprovalEvent,
    ) -> ra_core::error::Result<()> {
        let mut guard = self.inner.lock();
        let paused = guard.active_turn.as_mut().and_then(|active| {
            if active.cancel.is_cancelled() || active.ending {
                return None;
            }
            active.paused.as_mut()
        });
        let Some(paused) = paused.filter(|paused| same_approval_pause(&paused.state, state)) else {
            return Err(ra_core::error::Error::caller(
                "the thread's running turn is not waiting for approval on this run",
            ));
        };
        let allocator = &paused.event_seqs;
        allocator.advance_past(&state.restore_event_seq_allocator(None));
        let event = ra_core::event::HostEvent::allocate(
            allocator,
            self.inner.agent.public_id().clone(),
            event.into(),
        )?;
        state.snapshot_event_seq(allocator);
        paused.state.snapshot_event_seq(allocator);
        paused.recorder.record(RolloutItem::Event(event));
        Ok(())
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
        let end_hooks = {
            let mut state = self.inner.lock();
            let needed = !state.session_ended;
            state.session_ended = true;
            needed.then(|| state.config.user_hooks().clone())
        };
        if let Some(hooks) = end_hooks {
            let run = Arc::new(ra_core::context::RunContext::new(
                RunId::new(format!("session-end-{}", self.inner.thread_id)),
                self.inner.agent.public(),
            ));
            let _ = hooks
                .bind(run, CancelScope::root(), self.inner.services.clone())
                .dispatch(ra_core::hook::HookEvent::SessionEnd {
                    session_id: &self.inner.thread_id,
                    reason: "other",
                })
                .await;
        }
        if let Some(control) = &self.inner.agent_control {
            control.shutdown().await;
        }
        if let Some(rollout) = &self.inner.rollout
            && let Err(error) = rollout.flush().await
        {
            tracing::warn!(thread_id = %self.inner.thread_id, %error, "the thread's rollout could not be flushed");
        }
    }

    /// Writes out what the thread's rollout recorder still holds, so a reader of the store sees
    /// every record the thread has produced so far. Does nothing for a thread without a rollout.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error if the records cannot be written.
    pub async fn flush_rollout(&self) -> ra_core::error::Result<()> {
        match &self.inner.rollout {
            Some(rollout) => rollout.flush().await,
            None => Ok(()),
        }
    }

    /// Materializes the thread's rollout for an explicit metadata or lifecycle mutation,
    /// including before its first turn: Codex's `ensure_rollout_materialized`.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error if its pending records cannot be persisted.
    pub async fn ensure_rollout_materialized(&self) -> ra_core::error::Result<()> {
        match &self.inner.rollout {
            Some(rollout) => rollout.persist(PersistContext::Standard).await,
            None => Ok(()),
        }
    }

    /// Closes the rollout after the thread's tasks have stopped, releasing storage ownership
    /// before its records are moved or deleted: Codex's `shutdown_thread` storage barrier.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error if closing the writer fails.
    pub async fn close_rollout(&self) -> ra_core::error::Result<()> {
        match &self.inner.rollout {
            Some(rollout) => rollout.shutdown().await,
            None => Ok(()),
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
        let final_output_json_schema = request.start.final_output_json_schema.clone();
        let output_schema = final_output_json_schema.clone().map(|schema| {
            OutputSchema::json_schema(TURN_OUTPUT_SCHEMA_NAME, schema).with_strict(true)
        });
        state.active_turn = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            pending: pending.clone(),
            settings: settings.clone(),
            cancel: cancel.clone(),
            final_output_json_schema,
            interrupted: false,
            ending: false,
            paused: None,
        });
        let started_at = unix_seconds();
        self.inner.emit(ThreadEvent::TurnStarted {
            turn_id: turn_id.clone(),
            started_at,
        });
        let input = request.message();
        if let Some(message) = &input {
            self.inner.emit(ThreadEvent::Item {
                turn_id: turn_id.clone(),
                item: RunItem::new(
                    turn_input_item_id(&turn_id, 0),
                    RunItemKind::Message(message.clone()),
                ),
            });
        }
        let turn = StartedTurn {
            turn_id: turn_id.clone(),
            history: state.history.clone(),
            input,
            config: state.config.clone(),
            output_schema,
            pending,
            settings,
            cancel,
        };
        tokio::spawn(crate::runner::carry_subscriber(Box::pin(run_turn(
            Arc::clone(&self.inner),
            turn,
        ))));
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
    /// The turn's output schema, which every segment of its run is given.
    output_schema: Option<OutputSchema>,
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

/// Observes the same records used for replay, including calls recorded before execution.
/// The runner's terminal record waits until the thread has saved unconsumed steering input.
/// A weak reference lets background execution report its exit without keeping a thread alive.
struct ThreadRecorder {
    thread: Weak<ThreadInner>,
    turn_id: String,
    store: Option<Arc<dyn RolloutRecorder>>,
    ending: Mutex<Option<RolloutItem>>,
}

impl ThreadRecorder {
    async fn finish(&self) {
        let ending = self
            .ending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let (Some(store), Some(ending)) = (&self.store, ending) {
            store.record(ending);
        }
        if let Err(error) = self.flush().await {
            tracing::warn!(%error, "the thread's rollout could not be flushed");
        }
    }
}

impl crate::runner::stream::ModelStreamSink for ThreadRecorder {
    fn emit(&self, event: &RunStreamEvent) {
        let Some(thread) = self.thread.upgrade() else {
            return;
        };
        match event {
            RunStreamEvent::ItemStarted(item) => thread.emit(ThreadEvent::ItemStarted {
                turn_id: self.turn_id.clone(),
                item: item.clone(),
            }),
            RunStreamEvent::Delta(delta) => thread.emit(ThreadEvent::Delta {
                turn_id: self.turn_id.clone(),
                delta: delta.clone(),
            }),
            _ => {}
        }
    }

    fn retrying(&self, retry: u32, max_retries: u32, error: &ra_core::error::Error) {
        let Some(thread) = self.thread.upgrade() else {
            return;
        };
        thread.emit(ThreadEvent::StreamError {
            turn_id: self.turn_id.clone(),
            message: format!("Reconnecting... {retry}/{max_retries}"),
            additional_details: error.to_string(),
        });
    }
}

#[async_trait::async_trait]
impl RolloutRecorder for ThreadRecorder {
    fn trace_mode(&self) -> ra_core::session::rollout::RolloutTraceMode {
        self.store
            .as_ref()
            .map_or_else(Default::default, |store| store.trace_mode())
    }

    fn record_trace(&self, event: ra_core::session::rollout::RolloutTraceEvent) {
        if let Some(store) = &self.store {
            store.record_trace(event);
        }
    }

    fn child_trace(
        &self,
        recorder: Option<Arc<dyn RolloutRecorder>>,
    ) -> Option<Arc<dyn RolloutRecorder>> {
        self.store
            .as_ref()
            .and_then(|store| store.child_trace(recorder))
    }

    fn record(&self, item: RolloutItem) {
        if matches!(&item, RolloutItem::RunEnded(end) if end.end() != RolloutRunEnd::Interrupted) {
            *self.ending.lock().unwrap_or_else(PoisonError::into_inner) = Some(item);
            return;
        }
        if let Some(store) = &self.store {
            store.record(item.clone());
        }
        if let Some(thread) = self.thread.upgrade() {
            match item {
                RolloutItem::Item(item) => thread.emit(ThreadEvent::Item {
                    turn_id: self.turn_id.clone(),
                    item,
                }),
                RolloutItem::Event(event) => thread.emit(ThreadEvent::HostEvent(event)),
                _ => {}
            }
        }
    }

    async fn flush(&self) -> ra_core::error::Result<()> {
        if let Some(store) = &self.store {
            store.flush().await
        } else {
            Ok(())
        }
    }

    async fn persist(&self, context: PersistContext) -> ra_core::error::Result<()> {
        if let Some(store) = &self.store {
            store.persist(context).await
        } else {
            Ok(())
        }
    }
}

/// Runs a turn to its end and reports how it ended.
#[allow(
    clippy::too_many_lines,
    reason = "Keep turn acceptance, persistence and terminal event ordering together."
)]
async fn run_turn(inner: Arc<ThreadInner>, turn: StartedTurn) {
    let started = Instant::now();
    let StartedTurn {
        turn_id,
        history,
        input,
        config,
        output_schema,
        pending,
        settings,
        cancel,
    } = turn;
    let history_len = history.len();
    let run_input = history;
    let submitted: Vec<_> = input.map(ModelInputItem::Message).into_iter().collect();
    let recorder = Arc::new(ThreadRecorder {
        thread: Arc::downgrade(&inner),
        turn_id: turn_id.clone(),
        store: inner.rollout.clone(),
        ending: Mutex::new(None),
    });
    let (run_input, stopped, hook_context) =
        prepare_host_input(&inner, &recorder, &config, &cancel, run_input, submitted).await;
    let (end, history) = if stopped {
        recorder.record(RolloutItem::RunStarted(RolloutRunStarted::new(
            RunId::new(&turn_id),
            inner.agent.public_id().clone(),
        )));
        for item in run_input.iter().skip(history_len) {
            if let ModelInputItem::Message(message) = item {
                recorder.record(RolloutItem::Item(RunItem::new(
                    ItemId::new(uuid::Uuid::now_v7().to_string()),
                    RunItemKind::Message(message.clone()),
                )));
            }
        }
        recorder.record(RolloutItem::RunEnded(RolloutRunEnded::new(
            RunId::new(&turn_id),
            if cancel.is_cancelled() {
                RolloutRunEnd::Cancelled
            } else {
                RolloutRunEnd::Completed
            },
        )));
        (
            if cancel.is_cancelled() {
                TurnEnd::Cancelled
            } else {
                TurnEnd::Completed(None)
            },
            run_input,
        )
    } else {
        run_to_end(
            &inner,
            &recorder,
            config,
            output_schema.as_ref(),
            &pending,
            &hook_context,
            &settings,
            &cancel,
            run_input,
            history_len,
        )
        .await
    };
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
            let item = RunItem::new(
                ItemId::new(uuid::Uuid::now_v7().to_string()),
                RunItemKind::Message(message.clone()),
            );
            recorder.record(RolloutItem::Item(item));
            history.push(ModelInputItem::Message(message));
        }
        if let Some(rollout) = &inner.rollout
            && let Err(error) = rollout.persist(PersistContext::Standard).await
        {
            tracing::warn!(%turn_id, %error, "the thread's pending input could not be persisted");
        }
    }
    recorder.finish().await;
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
    recorder: &Arc<ThreadRecorder>,
    mut config: RunConfig,
    output_schema: Option<&OutputSchema>,
    pending: &PendingInput,
    hook_context: &ra_core::hook::HookContextQueue,
    settings: &LiveRunSettings,
    cancel: &CancelScope,
    input: Vec<ModelInputItem>,
    history_len: usize,
) -> (TurnEnd, Vec<ModelInputItem>) {
    let mut fallback = input.clone();
    let mut request = RunRequest::new(
        inner.agent.clone(),
        Arc::clone(&inner.model_resolver),
        RunId::new(&recorder.turn_id),
        cancel.clone(),
        input,
    )
    .with_recorded_input(history_len);
    let event_seqs = request.event_seq_allocator().clone();
    loop {
        request = request
            .with_config(config.clone())
            .with_services(
                inner
                    .services
                    .clone()
                    .with_hook_context(hook_context.clone()),
            )
            .with_pending_input(pending.clone());
        request = request.with_live_settings(settings.clone());
        if let Some(control) = &inner.agent_control {
            request = request.with_agent_control(control.root());
        }
        request = request.with_rollout_recorder(recorder.clone());
        request = request.with_model_stream_sink(recorder.clone());
        if let Some(output_schema) = output_schema {
            request = request.with_output_schema(output_schema.clone());
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
                let mut history = partial_history(fallback, &produced);
                let mut contexts = error
                    .run_state()
                    .map_or_else(Vec::new, |state| state.clone().take_hook_context());
                contexts.extend(hook_context.close());
                record_context_history(recorder, &mut history, contexts);
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
        let context_on_cancel = state.clone().take_hook_context();
        let (resume, answered) = oneshot::channel();
        let paused = inner.lock().active_turn.as_mut().map(|active| {
            active.paused = Some(PausedTurn {
                state,
                resume,
                config: config.clone(),
                event_seqs: event_seqs.clone(),
                recorder: Arc::clone(recorder),
            });
        });
        if paused.is_some() {
            inner.emit(ThreadEvent::TurnAwaitingApproval {
                turn_id: recorder.turn_id.clone(),
            });
        }
        let answered = tokio::select! {
            answered = answered => answered.ok(),
            () = cancel.cancelled() => None,
        };
        if let Some(active) = inner.lock().active_turn.as_mut() {
            active.paused = None;
        }
        let Some((agent, state)) = answered else {
            let mut history = history;
            record_context_history(recorder, &mut history, context_on_cancel);
            let history = cancel_paused(recorder.as_ref(), &config, cancel, run_id, history);
            return (TurnEnd::Cancelled, history);
        };
        fallback = history;
        request = RunRequest::new(
            agent,
            Arc::clone(&inner.model_resolver),
            state.run_id().clone(),
            cancel.clone(),
            Vec::new(),
        )
        .with_state_and_shared_event_seqs(state, &event_seqs);
    }
}

/// Ends a run cancelled while it waited for approval, as the runner ends one cancelled while it
/// works: after the interrupted-run marker, which the returned history keeps, and recorded as
/// cancelled. Codex aborts a turn waiting for approval the same way it aborts one that is working.
fn cancel_paused(
    recorder: &ThreadRecorder,
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
    if let Some(marker) = marker {
        recorder.record(RolloutItem::Item(marker));
    }
    recorder.record(RolloutItem::RunEnded(RolloutRunEnded::new(
        run_id,
        RolloutRunEnd::Cancelled,
    )));
    history
}

/// Repeated stops of one run still have distinct approval item identities.
pub(crate) fn same_approval_pause(left: &RunState, right: &RunState) -> bool {
    left.run_id() == right.run_id()
        && left
            .pending_interruption_items()
            .map(RunItem::id)
            .eq(right.pending_interruption_items().map(RunItem::id))
}

/// Seconds since the Unix epoch, as Codex stamps its turn events.
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

/// Session owners inspect new prompts before handing accepted input to the runner.
async fn prepare_host_input(
    inner: &ThreadInner,
    recorder: &ThreadRecorder,
    config: &RunConfig,
    cancel: &CancelScope,
    mut history: Vec<ModelInputItem>,
    submitted: Vec<ModelInputItem>,
) -> (Vec<ModelInputItem>, bool, ra_core::hook::HookContextQueue) {
    use ra_core::hook::{HookContextQueue, HookDecision, HookEvent};
    let queue = HookContextQueue::default();
    let run = Arc::new(ra_core::context::RunContext::new(
        RunId::new(&recorder.turn_id),
        inner.agent.public(),
    ));
    let hooks = config.user_hooks().bind(
        run,
        cancel.clone(),
        inner.services.clone().with_hook_context(queue.clone()),
    );
    history.extend(queue.take().into_iter().map(ModelInputItem::Message));
    let source = {
        let mut state = inner.lock();
        state.pending_session_start.take()
    };
    let result: ra_core::error::Result<bool> = async {
        if let Some(source) = source {
            let decision = hooks
                .dispatch(HookEvent::SessionStart {
                    session_id: &inner.thread_id,
                    resumed: source == ra_core::hook::SessionStartSource::Resume,
                    source,
                })
                .await;
            history.extend(queue.take().into_iter().map(ModelInputItem::Message));
            if matches!(decision?, HookDecision::Deny { .. }) {
                return Ok(true);
            }
        }
        let decision = if submitted.is_empty() {
            HookDecision::Continue
        } else {
            hooks
                .dispatch(HookEvent::UserPromptSubmit { input: &submitted })
                .await?
        };
        let stopped = matches!(decision, HookDecision::Deny { .. });
        if !stopped {
            history.extend(submitted);
        }
        Ok(stopped)
    }
    .await;
    let stopped = result.unwrap_or_else(|error| {
        tracing::warn!(turn_id = %recorder.turn_id, %error, "host input hooks were interrupted");
        true
    });
    if stopped {
        history.extend(queue.close().into_iter().map(ModelInputItem::Message));
    }
    (history, stopped, queue)
}

/// Retains context when a thread ends a run instead of resuming its checkpoint.
fn record_context_history(
    recorder: &ThreadRecorder,
    history: &mut Vec<ModelInputItem>,
    messages: Vec<Message>,
) {
    for message in messages {
        recorder.record(RolloutItem::Item(RunItem::new(
            ItemId::new(uuid::Uuid::now_v7().to_string()),
            RunItemKind::Message(message.clone()),
        )));
        history.push(ModelInputItem::Message(message));
    }
}
