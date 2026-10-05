//! What a run records into its session's rollout, and the port it records through.
//!
//! Ported from Codex's split between `codex-history` and `codex-rollout`: the values a session
//! records live here, beside the session port, and the file they are written to — its envelope,
//! sequence numbers, checkpoints and persistence policy — lives in the storage layer. The runner
//! depends only on this module, so it records without knowing how or where the records are kept.
//!
//! A run given a [`RolloutRecorder`] records, in order: that it started and on what input, the
//! context its first turn runs in, every record it adds to the session, every host event
//! attributed to it, the usage of every model call it pays for, and how it ended. Codex's turn is
//! this framework's run, so these are Codex's `TurnStarted` with the user message, its
//! `TurnContextItem`, its response items, its event messages, its token usage records and its
//! `TurnComplete` / `TurnAborted`.
//!
//! An agent spawned in an agent tree is a thread of its own, as in Codex, and records into a
//! rollout of its own. The tree creates that rollout through a [`RolloutThreadStore`], describing
//! the thread with a [`RolloutThreadSpawn`]: which session it was spawned from, at what depth and
//! path, and which session the tree's root is. That is Codex's `ThreadSpawn` session source, and
//! it is how a child's rollout is tied to its parent's.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::SessionId;
use crate::{
    agent::control::AgentPath,
    compat::{SchemaVersion, Unknown},
    error::Result,
    event::HostEvent,
    finish::FinishReason,
    item::{AgentId, ModelInputItem, RunItem},
    state::RunId,
    usage::Usage,
};

/// Current schema version of the records in this module.
pub const ROLLOUT_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_schema_version() -> SchemaVersion {
    ROLLOUT_ITEM_SCHEMA_VERSION
}

/// Receives what a run records, in the order it happens.
///
/// [`Self::record`] must not block: it is called from inside the run, including from event sinks
/// that tools emit to synchronously, so an implementation queues the item and writes it elsewhere,
/// as Codex's recorder hands items to its writer task. Items recorded from one task arrive in the
/// order they were recorded. A write that fails is reported by the next [`Self::flush`].
#[async_trait]
pub trait RolloutRecorder: Send + Sync + 'static {
    /// Queues `item` to be recorded.
    fn record(&self, item: RolloutItem);

    /// Waits until everything recorded so far is written.
    ///
    /// # Errors
    ///
    /// Returns an error if writing any item recorded so far failed.
    async fn flush(&self) -> Result<()>;

    /// Makes the thread durable even when no run items have been recorded: Codex's
    /// `ThreadStore::persist_thread`. A deferred file recorder writes its initial session metadata.
    /// The default flushes a recorder whose storage is already materialized.
    ///
    /// # Errors
    ///
    /// Returns an error if materializing the thread or writing its pending items failed.
    async fn persist(&self) -> Result<()> {
        self.flush().await
    }

    /// Writes everything recorded so far, then stops recording and lets go of the rollout, so the
    /// thread can be resumed elsewhere: Codex's `ThreadStore::shutdown_thread`.
    ///
    /// A file recorder stops every handle sharing its writer, and [`Self::flush`] then reports
    /// that recording has stopped. A store without live writers may retain writable handles, as
    /// Codex's in-memory store does. If the pending items cannot be written, the recorder keeps
    /// them and keeps going, so the shutdown can be tried again, as Codex's writer stays alive
    /// when draining fails.
    ///
    /// The default flushes: a recorder that holds nothing open has nothing to let go of.
    ///
    /// # Errors
    ///
    /// Returns an error if writing any item recorded so far failed.
    async fn shutdown(&self) -> Result<()> {
        self.flush().await
    }

    /// Discards a live writer without writing what has not been written yet, letting go of the
    /// rollout: Codex's `ThreadStore::discard_thread`, for a thread whose start failed after its
    /// writer was opened. What is already written stays.
    ///
    /// The default does nothing for stores without live writers, as Codex's in-memory store does.
    ///
    /// # Errors
    ///
    /// Returns an error if the writer could not be stopped.
    async fn discard(&self) -> Result<()> {
        Ok(())
    }
}

/// Creates the rollouts of a session's threads, handing back the recorder each thread
/// records through.
///
/// This is the part of Codex's `ThreadStore` that yields a live thread writer: its
/// `create_thread`. Codex's store keeps the writer and addresses it by thread id; here the writer
/// is the returned [`RolloutRecorder`], held by whoever records — a run takes it through
/// `RunRequest::with_rollout_recorder` — and persisted, flushed, shut down or discarded through
/// it. Resume, reads and the rest of Codex's store speak in the storage layer's records, including
/// caller-supplied replay history, and are in `ra-session`'s `ThreadStore`, which extends this
/// trait.
#[async_trait]
pub trait RolloutThreadStore: Send + Sync + 'static {
    /// Creates the rollout of `session_id`, the thread of an agent spawned as `spawn` describes,
    /// and returns the recorder its runs record through.
    ///
    /// Codex's `create_thread` for a thread whose source is a thread spawn. The tree calls it once
    /// for each agent it spawns, before the agent's first run, and every run of the agent then
    /// records through the recorder it returns — the agent's whole life, across follow-ups and
    /// approvals, in one rollout. Closing the agent and spawning its path again creates another.
    ///
    /// # Errors
    ///
    /// Returns an error if the rollout cannot be created. The spawn then fails, as Codex's does
    /// when it cannot create the thread.
    async fn create_thread(
        &self,
        session_id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>>;
}

/// Where a spawned agent's thread comes from: Codex's `SubAgentSource::ThreadSpawn`, with the
/// session ids its `SessionMeta` carries beside it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutThreadSpawn {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    root_session_id: SessionId,
    parent_session_id: SessionId,
    depth: u32,
    agent_path: AgentPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_type: Option<AgentId>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutThreadSpawn {
    /// Describes an agent at `agent_path`, `depth` below the root, spawned by the agent whose
    /// session is `parent_session_id`, in the tree whose root's session is `root_session_id`.
    #[must_use]
    pub fn new(
        root_session_id: SessionId,
        parent_session_id: SessionId,
        depth: u32,
        agent_path: AgentPath,
    ) -> Self {
        Self {
            schema_version: ROLLOUT_ITEM_SCHEMA_VERSION,
            root_session_id,
            parent_session_id,
            depth,
            agent_path,
            agent_type: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the registered agent the spawn asked for: Codex's `agent_role`.
    #[must_use]
    pub fn with_agent_type(mut self, agent_type: AgentId) -> Self {
        self.agent_type = Some(agent_type);
        self
    }

    /// The session of the tree's root, shared by every thread in the tree: Codex's `session_id`.
    #[must_use]
    pub const fn root_session_id(&self) -> &SessionId {
        &self.root_session_id
    }

    /// The session of the agent that spawned this one: Codex's `parent_thread_id`.
    #[must_use]
    pub const fn parent_session_id(&self) -> &SessionId {
        &self.parent_session_id
    }

    /// How far below the root the agent is; one for an agent the root spawned.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// The agent's path in the tree.
    #[must_use]
    pub const fn agent_path(&self) -> &AgentPath {
        &self.agent_path
    }

    /// The registered agent the spawn asked for, if it named one; otherwise the agent runs the
    /// declaration of the agent that spawned it.
    #[must_use]
    pub const fn agent_type(&self) -> Option<&AgentId> {
        self.agent_type.as_ref()
    }

    /// Schema version of the record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// One thing a run records.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum RolloutItem {
    /// A run, or a segment continuing it, started.
    RunStarted(RolloutRunStarted),
    /// The context a run's turns are executed in.
    TurnContext(RolloutTurnContext),
    /// A record the run added to the session.
    Item(RunItem),
    /// A host event attributed to the run.
    Event(HostEvent),
    /// The usage of model calls the run paid for.
    ModelUsage(RolloutModelUsage),
    /// A run, or the segment of it that started last, ended.
    RunEnded(RolloutRunEnded),
}

/// A run or a segment of it started: Codex's `TurnStarted` with the user input of the turn.
///
/// A segment that continues a run from its checkpoint records its own start under the same run id,
/// with whatever new input it was given — or, when the caller supplied the input that segment runs
/// on in place of the checkpoint's history, with that input marked as the continuation base.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutRunStarted {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    input: Vec<ModelInputItem>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    continuation_base: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutRunStarted {
    /// Creates the start of `run_id`, run by `agent_id`.
    #[must_use]
    pub fn new(run_id: RunId, agent_id: AgentId) -> Self {
        Self {
            schema_version: ROLLOUT_ITEM_SCHEMA_VERSION,
            run_id,
            agent_id,
            parent_run_id: None,
            input: Vec::new(),
            continuation_base: false,
            unknown: Unknown::new(),
        }
    }

    /// Sets the run that started this one.
    #[must_use]
    pub fn with_parent_run_id(mut self, parent_run_id: RunId) -> Self {
        self.parent_run_id = Some(parent_run_id);
        self
    }

    /// Sets the new input the run or segment started on.
    #[must_use]
    pub fn with_input(mut self, input: Vec<ModelInputItem>) -> Self {
        self.input = input;
        self.continuation_base = false;
        self
    }

    /// Sets the input a continuing segment runs on in place of what its run recorded so far.
    ///
    /// A caller that continues a run from its checkpoint and supplies input makes that input the
    /// base of the segment's model calls: it already holds the run's history, projected as the
    /// caller chose, followed by whatever the caller added. Recording it as new input would count
    /// that history twice.
    #[must_use]
    pub fn with_continuation_base(mut self, input: Vec<ModelInputItem>) -> Self {
        self.input = input;
        self.continuation_base = true;
        self
    }

    /// The run.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The public agent the run started with.
    #[must_use]
    pub const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// The run that started this one, if any.
    #[must_use]
    pub const fn parent_run_id(&self) -> Option<&RunId> {
        self.parent_run_id.as_ref()
    }

    /// The input the run or segment started on: new input, or the continuation base when
    /// [`Self::input_is_continuation_base`] says so.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Whether [`Self::input`] replaces the history the run recorded before this segment rather
    /// than adding to it; see [`Self::with_continuation_base`].
    #[must_use]
    pub const fn input_is_continuation_base(&self) -> bool {
        self.continuation_base
    }

    /// Schema version of the record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// How a run or segment ended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutRunEnd {
    /// The run reached its end: Codex's `TurnComplete`.
    Completed,
    /// The run stopped to ask the host for approval and can be continued.
    Interrupted,
    /// The run was cancelled: Codex's `TurnAborted`.
    Cancelled,
    /// The run failed with an error.
    Failed,
}

/// A run or segment ended.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutRunEnded {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    end: RolloutRunEnd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutRunEnded {
    /// Creates the end of `run_id`.
    #[must_use]
    pub fn new(run_id: RunId, end: RolloutRunEnd) -> Self {
        Self {
            schema_version: ROLLOUT_ITEM_SCHEMA_VERSION,
            run_id,
            end,
            finish_reason: None,
            error: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets why a completed run finished.
    #[must_use]
    pub const fn with_finish_reason(mut self, reason: FinishReason) -> Self {
        self.finish_reason = Some(reason);
        self
    }

    /// Sets the error a failed run ended with.
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    /// The run.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// How it ended.
    #[must_use]
    pub const fn end(&self) -> RolloutRunEnd {
        self.end
    }

    /// Why a completed run finished.
    #[must_use]
    pub const fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }

    /// The error a failed run ended with.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Schema version of the record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Execution and configuration context captured at the start of a turn.
///
/// Codex's `TurnContextItem`: a run records one as its first model call is made, with the model
/// and effort that call resolved to.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutTurnContext {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    turn_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutTurnContext {
    /// Creates a new turn context record.
    #[must_use]
    pub fn new(run_id: RunId, turn_index: u32) -> Self {
        Self {
            schema_version: ROLLOUT_ITEM_SCHEMA_VERSION,
            run_id,
            turn_index,
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the approval policy name.
    #[must_use]
    pub fn with_approval_policy(mut self, policy: impl Into<String>) -> Self {
        self.approval_policy = Some(policy.into());
        self
    }

    /// Sets the sandbox policy name.
    #[must_use]
    pub fn with_sandbox_policy(mut self, policy: impl Into<String>) -> Self {
        self.sandbox_policy = Some(policy.into());
        self
    }

    /// Sets the active model identifier.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the effort level.
    #[must_use]
    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    /// Active run identifier.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Zero-based turn index.
    #[must_use]
    pub const fn turn_index(&self) -> u32 {
        self.turn_index
    }

    /// Working directory.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Approval policy name.
    #[must_use]
    pub fn approval_policy(&self) -> Option<&str> {
        self.approval_policy.as_deref()
    }

    /// Sandbox policy name.
    #[must_use]
    pub fn sandbox_policy(&self) -> Option<&str> {
        self.sandbox_policy.as_deref()
    }

    /// Model name.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Effort level string.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Schema version of the record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Unprunable model usage record written upon each model completion settlement.
///
/// This is the reconciliation baseline for every total derived from it. Items are compacted and
/// pruned, and a run's checkpointed totals therefore cannot be checked against the responses still
/// present in its state; they can be checked against these records, which are never removed. The
/// per-request entries inside [`Self::usage`] are the finest granularity that survives, which is
/// what makes a cache hit rate computable for one call rather than only for a whole session.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutModelUsage {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    usage: Usage,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutModelUsage {
    /// Creates a model usage record.
    #[must_use]
    pub fn new(run_id: RunId, usage: Usage) -> Self {
        Self {
            schema_version: ROLLOUT_ITEM_SCHEMA_VERSION,
            run_id,
            turn_index: None,
            model: None,
            usage,
            unknown: Unknown::new(),
        }
    }

    /// Sets the turn index.
    #[must_use]
    pub const fn with_turn_index(mut self, turn_index: u32) -> Self {
        self.turn_index = Some(turn_index);
        self
    }

    /// Sets the model identifier.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Active run identifier.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Turn index, if known.
    #[must_use]
    pub const fn turn_index(&self) -> Option<u32> {
        self.turn_index
    }

    /// Model identifier, if known.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Token usage details.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Schema version of the record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
