//! Line-level append-only writes; a crash never damages what was already written.

use core::fmt;
use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Path, PathBuf},
};

use ra_core::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result, SessionErrorKind},
    event::{AgentOperationId, EventTimestamp, HostEvent},
    item::{AgentId, CallId, RunItem},
    session::SessionId,
    state::RunId,
    usage::Usage,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, SeekFrom},
};

use super::reader::{RolloutReader, RolloutSummary};

/// Current schema version for rollout envelopes and records.
pub const ROLLOUT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_rollout_schema_version() -> SchemaVersion {
    ROLLOUT_SCHEMA_VERSION
}

/// Execution lifecycle kind for child agent anchor events.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildAnchorKind {
    /// Child agent was spawned.
    Spawned,
    /// Child agent completed execution successfully.
    Completed,
    /// Child agent failed with an error.
    Failed,
    /// Child agent bubbled up an approval request to the parent.
    ApprovalBubbled,
    /// A custom anchor kind preserved for forward compatibility.
    Custom(Cow<'static, str>),
}

impl ChildAnchorKind {
    /// Returns the kind with the given name.
    #[must_use]
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        let name = name.into();
        Self::known(&name).unwrap_or(Self::Custom(name))
    }

    /// String representation of this anchor kind.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Spawned => "spawned",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::ApprovalBubbled => "approval_bubbled",
            Self::Custom(name) => name,
        }
    }

    fn known(name: &str) -> Option<Self> {
        match name {
            "spawned" => Some(Self::Spawned),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "approval_bubbled" => Some(Self::ApprovalBubbled),
            _ => None,
        }
    }
}

impl From<&str> for ChildAnchorKind {
    fn from(name: &str) -> Self {
        Self::known(name).unwrap_or_else(|| Self::Custom(Cow::Owned(name.to_owned())))
    }
}

impl From<String> for ChildAnchorKind {
    fn from(name: String) -> Self {
        match Self::known(&name) {
            Some(known) => known,
            None => Self::Custom(Cow::Owned(name)),
        }
    }
}

impl fmt::Display for ChildAnchorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ChildAnchorKind {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ChildAnchorKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from(s))
    }
}

/// An anchor event on the root timeline denoting child agent activity in an independent transcript.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutChildAnchor {
    #[serde(default = "default_rollout_schema_version")]
    schema_version: SchemaVersion,
    root_run_id: RunId,
    operation_id: AgentOperationId,
    child_agent_id: AgentId,
    kind: ChildAnchorKind,
    transcript_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call_id: Option<CallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutChildAnchor {
    /// Creates a new child anchor event.
    #[must_use]
    pub fn new(
        root_run_id: RunId,
        operation_id: AgentOperationId,
        child_agent_id: AgentId,
        kind: ChildAnchorKind,
        transcript_ref: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ROLLOUT_SCHEMA_VERSION,
            root_run_id,
            operation_id,
            child_agent_id,
            kind,
            transcript_ref: transcript_ref.into(),
            child_run_id: None,
            call_id: None,
            summary: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the child run identifier.
    #[must_use]
    pub fn with_child_run_id(mut self, run_id: RunId) -> Self {
        self.child_run_id = Some(run_id);
        self
    }

    /// Sets the associated tool call identifier.
    #[must_use]
    pub fn with_call_id(mut self, call_id: CallId) -> Self {
        self.call_id = Some(call_id);
        self
    }

    /// Sets an optional human-readable summary.
    #[must_use]
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Root run identifier.
    #[must_use]
    pub const fn root_run_id(&self) -> &RunId {
        &self.root_run_id
    }

    /// Agent operation identifier.
    #[must_use]
    pub const fn operation_id(&self) -> &AgentOperationId {
        &self.operation_id
    }

    /// Child agent identity.
    #[must_use]
    pub const fn child_agent_id(&self) -> &AgentId {
        &self.child_agent_id
    }

    /// Anchor lifecycle kind.
    #[must_use]
    pub const fn kind(&self) -> &ChildAnchorKind {
        &self.kind
    }

    /// Relative path or reference key to the child transcript file.
    #[must_use]
    pub fn transcript_ref(&self) -> &str {
        &self.transcript_ref
    }

    /// Child run identifier, if known.
    #[must_use]
    pub const fn child_run_id(&self) -> Option<&RunId> {
        self.child_run_id.as_ref()
    }

    /// Tool call identifier, if triggered by tool invocation.
    #[must_use]
    pub const fn call_id(&self) -> Option<&CallId> {
        self.call_id.as_ref()
    }

    /// Human-readable summary description.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
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

/// Metadata recorded at session creation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutSessionMeta {
    #[serde(default = "default_rollout_schema_version")]
    schema_version: SchemaVersion,
    session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    originator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<EventTimestamp>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutSessionMeta {
    /// Creates new session metadata.
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        Self {
            schema_version: ROLLOUT_SCHEMA_VERSION,
            session_id,
            cwd: None,
            cli_version: None,
            originator: None,
            model_provider: None,
            created_at: Some(EventTimestamp::now()),
            unknown: Unknown::new(),
        }
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the CLI version string.
    #[must_use]
    pub fn with_cli_version(mut self, version: impl Into<String>) -> Self {
        self.cli_version = Some(version.into());
        self
    }

    /// Sets the originator identifier.
    #[must_use]
    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = Some(originator.into());
        self
    }

    /// Sets the model provider name.
    #[must_use]
    pub fn with_model_provider(mut self, provider: impl Into<String>) -> Self {
        self.model_provider = Some(provider.into());
        self
    }

    /// Sets the creation timestamp.
    #[must_use]
    pub const fn with_created_at(mut self, created_at: EventTimestamp) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// Session identity.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Working directory at session start.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// CLI version.
    #[must_use]
    pub fn cli_version(&self) -> Option<&str> {
        self.cli_version.as_deref()
    }

    /// Originator program or client.
    #[must_use]
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    /// Primary model provider.
    #[must_use]
    pub fn model_provider(&self) -> Option<&str> {
        self.model_provider.as_deref()
    }

    /// Creation timestamp if recorded.
    #[must_use]
    pub const fn created_at(&self) -> Option<EventTimestamp> {
        self.created_at
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
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutTurnContext {
    #[serde(default = "default_rollout_schema_version")]
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
            schema_version: ROLLOUT_SCHEMA_VERSION,
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
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutModelUsage {
    #[serde(default = "default_rollout_schema_version")]
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
            schema_version: ROLLOUT_SCHEMA_VERSION,
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

/// Aggregates over every record preceding this one, written into the log itself.
///
/// This is what makes resume able to skip a full scan. The same summary kept beside the log
/// cannot: nothing stored outside the rollout can establish that a derived value was computed
/// from it, so a snapshot that took one plausible hit still looks valid. Inside the log the
/// figures travel as an ordinary record, covered by the reader's parse contract and by the
/// `timeline_seq` monotonicity check, and a damaged checkpoint line is therefore caught the same
/// way any other damaged line is.
///
/// A checkpoint summarizes the records *before* it. Its own `timeline_seq` marks the boundary:
/// recovery seeds itself from these values and folds in the records that follow.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutCheckpoint {
    #[serde(default = "default_rollout_schema_version")]
    schema_version: SchemaVersion,
    session_id: SessionId,
    usage_totals: Usage,
    persisted_run_max_seq: HashMap<RunId, u64>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutCheckpoint {
    /// Creates a checkpoint carrying the aggregates accumulated so far.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        usage_totals: Usage,
        persisted_run_max_seq: HashMap<RunId, u64>,
    ) -> Self {
        Self {
            schema_version: ROLLOUT_SCHEMA_VERSION,
            session_id,
            usage_totals,
            persisted_run_max_seq,
            unknown: Unknown::new(),
        }
    }

    /// Session this checkpoint belongs to.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Token usage accumulated across every `model_usage` record before this point.
    #[must_use]
    pub const fn usage_totals(&self) -> &Usage {
        &self.usage_totals
    }

    /// Maximum persisted host event sequence per run, as of this point.
    #[must_use]
    pub const fn persisted_run_max_seq(&self) -> &HashMap<RunId, u64> {
        &self.persisted_run_max_seq
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

/// Payload variants carried within a rollout line.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum RolloutPayload {
    /// Initial session-level metadata.
    SessionMeta(RolloutSessionMeta),
    /// Turn configuration snapshot.
    TurnContext(RolloutTurnContext),
    /// Host-visible lifecycle or telemetry event.
    Event(HostEvent),
    /// Authoritative conversation item.
    Item(RunItem),
    /// Unprunable model usage record.
    ModelUsage(RolloutModelUsage),
    /// Child agent execution anchor on the root timeline.
    ChildAnchor(RolloutChildAnchor),
    /// Aggregates over the preceding records, letting resume skip them.
    Checkpoint(RolloutCheckpoint),
    /// Forward-compatible unknown payload.
    Unknown {
        /// Type name identifier.
        type_name: String,
        /// Raw payload value.
        data: serde_json::Value,
    },
}

impl From<RolloutSessionMeta> for RolloutPayload {
    fn from(m: RolloutSessionMeta) -> Self {
        Self::SessionMeta(m)
    }
}

impl From<RolloutTurnContext> for RolloutPayload {
    fn from(c: RolloutTurnContext) -> Self {
        Self::TurnContext(c)
    }
}

impl From<HostEvent> for RolloutPayload {
    fn from(e: HostEvent) -> Self {
        Self::Event(e)
    }
}

impl From<RunItem> for RolloutPayload {
    fn from(i: RunItem) -> Self {
        Self::Item(i)
    }
}

impl From<RolloutModelUsage> for RolloutPayload {
    fn from(u: RolloutModelUsage) -> Self {
        Self::ModelUsage(u)
    }
}

impl From<RolloutChildAnchor> for RolloutPayload {
    fn from(a: RolloutChildAnchor) -> Self {
        Self::ChildAnchor(a)
    }
}

impl From<RolloutCheckpoint> for RolloutPayload {
    fn from(c: RolloutCheckpoint) -> Self {
        Self::Checkpoint(c)
    }
}

/// A persisted rollout record representing a single line in a rollout file.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutRecord {
    #[serde(default = "default_rollout_schema_version")]
    schema_version: SchemaVersion,
    timeline_seq: u64,
    at: EventTimestamp,
    #[serde(rename = "type")]
    type_name: String,
    payload: serde_json::Value,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutRecord {
    /// Creates a new rollout record with the given sequence, timestamp, and payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if the payload cannot be serialized into the envelope.
    pub fn new(timeline_seq: u64, at: EventTimestamp, payload: RolloutPayload) -> Result<Self> {
        let (type_name, payload_val) = match payload {
            RolloutPayload::SessionMeta(meta) => (
                "session_meta".to_string(),
                serde_json::to_value(meta).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize session_meta: {e}"),
                    )
                })?,
            ),
            RolloutPayload::TurnContext(ctx) => (
                "turn_context".to_string(),
                serde_json::to_value(ctx).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize turn_context: {e}"),
                    )
                })?,
            ),
            RolloutPayload::Event(event) => (
                "event".to_string(),
                serde_json::to_value(event).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize event: {e}"),
                    )
                })?,
            ),
            RolloutPayload::Item(item) => (
                "item".to_string(),
                serde_json::to_value(item).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize item: {e}"),
                    )
                })?,
            ),
            RolloutPayload::ModelUsage(usage) => (
                "model_usage".to_string(),
                serde_json::to_value(usage).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize model_usage: {e}"),
                    )
                })?,
            ),
            RolloutPayload::ChildAnchor(anchor) => (
                "child_anchor".to_string(),
                serde_json::to_value(anchor).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize child_anchor: {e}"),
                    )
                })?,
            ),
            RolloutPayload::Checkpoint(checkpoint) => (
                "checkpoint".to_string(),
                serde_json::to_value(checkpoint).map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("failed to serialize checkpoint: {e}"),
                    )
                })?,
            ),
            RolloutPayload::Unknown { type_name, data } => (type_name, data),
        };

        Ok(Self {
            schema_version: ROLLOUT_SCHEMA_VERSION,
            timeline_seq,
            at,
            type_name,
            payload: payload_val,
            unknown: Unknown::new(),
        })
    }

    /// Stable sequence number assigned monotonically on the root timeline at persistence time.
    #[must_use]
    pub const fn timeline_seq(&self) -> u64 {
        self.timeline_seq
    }

    /// Creation timestamp.
    #[must_use]
    pub const fn at(&self) -> EventTimestamp {
        self.at
    }

    /// Schema version of the record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Type name of the payload.
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Raw payload value.
    #[must_use]
    pub const fn payload_value(&self) -> &serde_json::Value {
        &self.payload
    }

    /// Parses and returns the strongly typed payload from this record.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if deserialization of a known type fails.
    pub fn payload(&self) -> Result<RolloutPayload> {
        match self.type_name.as_str() {
            "session_meta" => serde_json::from_value::<RolloutSessionMeta>(self.payload.clone())
                .map(RolloutPayload::SessionMeta)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted session_meta payload: {e}"),
                    )
                    .with_source(e)
                }),
            "turn_context" => serde_json::from_value::<RolloutTurnContext>(self.payload.clone())
                .map(RolloutPayload::TurnContext)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted turn_context payload: {e}"),
                    )
                    .with_source(e)
                }),
            "event" => serde_json::from_value::<HostEvent>(self.payload.clone())
                .map(RolloutPayload::Event)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted event payload: {e}"),
                    )
                    .with_source(e)
                }),
            "item" => serde_json::from_value::<RunItem>(self.payload.clone())
                .map(RolloutPayload::Item)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted item payload: {e}"),
                    )
                    .with_source(e)
                }),
            "model_usage" => serde_json::from_value::<RolloutModelUsage>(self.payload.clone())
                .map(RolloutPayload::ModelUsage)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted model_usage payload: {e}"),
                    )
                    .with_source(e)
                }),
            "child_anchor" => serde_json::from_value::<RolloutChildAnchor>(self.payload.clone())
                .map(RolloutPayload::ChildAnchor)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted child_anchor payload: {e}"),
                    )
                    .with_source(e)
                }),
            "checkpoint" => serde_json::from_value::<RolloutCheckpoint>(self.payload.clone())
                .map(RolloutPayload::Checkpoint)
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted checkpoint payload: {e}"),
                    )
                    .with_source(e)
                }),
            unknown_type => Ok(RolloutPayload::Unknown {
                type_name: unknown_type.to_string(),
                data: self.payload.clone(),
            }),
        }
    }

    /// Unknown fields retained during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A summary written alongside the rollout file, plus the index recovery uses to find the newest
/// checkpoint.
///
/// **Only [`RolloutSidecar::last_checkpoint_offset`] is read back, and only as a hint.** The
/// record found at that offset must parse, be a checkpoint, and name this session before anything
/// is believed, so an offset that is stale, wrong, out of range, or missing costs a full scan and
/// never a wrong answer. Nothing else in this file influences recovery.
///
/// The summary fields cannot: nothing stored beside the log can establish that a derived value
/// was computed from it. A snapshot that took one plausible hit — `next_timeline_seq` off by one —
/// still looks entirely valid, and resuming from it appends a duplicate sequence number that
/// leaves the log unopenable. An offset is safe to keep out here precisely because it is checked
/// against the thing it points into; a total is not, because there is nothing to check it against
/// short of the scan it was meant to avoid.
///
/// # Do not use this as a ledger
///
/// It is an advisory cache, suitable for a status line, a progress indicator, or a dashboard that
/// can be wrong for a moment. It must not back resume, budget enforcement, billing, or any other
/// authoritative decision. Three separate reasons, none of which a reader can detect:
///
/// - **It can be arbitrarily wrong.** Nothing ties these numbers to the log (see above), so a
///   damaged snapshot is indistinguishable from a good one.
/// - **It lags.** It is rewritten when the writer is opened, at each checkpoint, and on
///   [`RolloutWriter::flush`] or [`RolloutWriter::sync_all`] — not on every append. A writer
///   dropped without flushing leaves it at the last checkpoint. A failed write leaves the
///   previous contents and only sets [`RolloutWriter::sidecar_is_stale`], which no outside reader
///   can observe.
/// - **It is one process's view.** It is replaced atomically, so a reader never sees a torn
///   document, but it may still be reading a snapshot the writer has already moved past.
///
/// The authoritative usage ledger is the sequence of `model_usage` records in the rollout itself.
/// [`RolloutReader::scan_summary`](super::reader::RolloutReader::scan_summary) computes it, and
/// while doing so holds every [`RolloutCheckpoint`] it passes to the records it claims to
/// summarize — that full read, not this file, is what detects a tampered checkpoint.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutSidecar {
    #[serde(default = "default_rollout_schema_version")]
    schema_version: SchemaVersion,
    session_id: SessionId,
    next_timeline_seq: u64,
    persisted_run_max_seq: HashMap<RunId, u64>,
    usage_totals: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_checkpoint_offset: Option<u64>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RolloutSidecar {
    /// Constructs a sidecar snapshot.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        next_timeline_seq: u64,
        persisted_run_max_seq: HashMap<RunId, u64>,
        usage_totals: Usage,
    ) -> Self {
        Self {
            schema_version: ROLLOUT_SCHEMA_VERSION,
            session_id,
            next_timeline_seq,
            persisted_run_max_seq,
            usage_totals,
            last_checkpoint_offset: None,
            unknown: Unknown::new(),
        }
    }

    /// Records where the newest checkpoint record starts in the rollout file.
    #[must_use]
    pub const fn with_last_checkpoint_offset(mut self, offset: u64) -> Self {
        self.last_checkpoint_offset = Some(offset);
        self
    }

    /// Byte offset of the newest checkpoint record, when one is known.
    ///
    /// This is the one field recovery reads, and it is treated as a hint rather than a fact: the
    /// record found there is parsed and checked before anything is believed, so an offset that is
    /// stale, wrong, or absent costs a full scan and nothing else.
    #[must_use]
    pub const fn last_checkpoint_offset(&self) -> Option<u64> {
        self.last_checkpoint_offset
    }

    /// Associated session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Next timeline sequence number.
    #[must_use]
    pub const fn next_timeline_seq(&self) -> u64 {
        self.next_timeline_seq
    }

    /// Map of persisted run maximum event sequence numbers.
    #[must_use]
    pub const fn persisted_run_max_seq(&self) -> &HashMap<RunId, u64> {
        &self.persisted_run_max_seq
    }

    /// Accumulated unpruned token usage totals.
    #[must_use]
    pub const fn usage_totals(&self) -> &Usage {
        &self.usage_totals
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

fn sidecar_path_for(path: &Path) -> PathBuf {
    let mut os_str = path.as_os_str().to_os_string();
    os_str.push(".sidecar.json");
    PathBuf::from(os_str)
}

/// A line-level append-only writer for session rollout event streams.
///
/// It guarantees that:
/// 1. Each write reaches the file as a complete newline-terminated JSON object before `append`
///    returns, or the writer is poisoned — see [`RolloutWriter::is_poisoned`]. Records are
///    flushed, not `fsync`ed; call [`RolloutWriter::sync_all`] where durability must outlive a
///    power cut, not just a process crash.
/// 2. `timeline_seq` is assigned monotonically at the exact moment of append.
/// 3. Per-request `Usage` totals are accumulated in unprunable form.
/// 4. `persisted_run_max_seq` is maintained per [`RunId`], recovered by scanning the log.
///
/// Exactly one writer may hold a rollout file, enforced by an advisory lock in
/// [`RolloutWriter::open`]. The [`RolloutSidecar`] written alongside contributes exactly one thing
/// to recovery — the byte offset of the newest checkpoint, which is verified against the log
/// before use. Its summary fields are output only.
#[derive(Debug)]
pub struct RolloutWriter {
    session_id: SessionId,
    path: PathBuf,
    sidecar_path: PathBuf,
    file: File,
    next_timeline_seq: u64,
    persisted_run_max_seq: HashMap<RunId, u64>,
    usage_totals: Usage,
    file_len: u64,
    checkpoint_interval: u64,
    records_since_checkpoint: u64,
    last_checkpoint_offset: Option<u64>,
    sidecar_stale: bool,
    poisoned: bool,
}

/// Records appended between checkpoints by default.
///
/// Sets how much of the tail a resume has to re-read; the cost of the checkpoints themselves is
/// one extra record per interval.
pub const DEFAULT_CHECKPOINT_INTERVAL: u64 = 256;

/// Writer state rebuilt from an existing rollout file, by either route.
#[derive(Debug, Default)]
struct Recovered {
    next_timeline_seq: u64,
    persisted_run_max_seq: HashMap<RunId, u64>,
    usage_totals: Usage,
    file_len: u64,
    last_checkpoint_offset: Option<u64>,
    records_since_checkpoint: u64,
}

/// Takes the writer's exclusive advisory lock on the rollout file.
///
/// The lock lives on the open file description, so it is released when the writer's [`File`] is
/// dropped and, on a crash, by the kernel closing the process's descriptors — there is no lock
/// file to go stale.
///
/// **A non-cooperating writer is out of scope.** `flock` binds only processes that ask for it, so
/// anything that writes to the path without taking the lock can land bytes between the moment
/// recovery reads the tail and the moment the first record is appended, and this writer will
/// still allocate its sequence from what it saw. No advisory scheme can close that window;
/// re-checking after the scan would only shrink it, at the cost of implying a guarantee that is
/// not there. The supported model is one `RolloutWriter` per rollout file and no outside mutation
/// while it is open — the monotonicity check in recovery is what catches a violation afterwards,
/// rather than preventing it.
#[cfg(unix)]
fn lock_exclusive(file: &File, path: &Path) -> Result<()> {
    use rustix::fs::{FlockOperation, flock};

    flock(file, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        Error::session(
            SessionErrorKind::Io,
            format!(
                "another writer already holds the rollout file {}: {e}",
                path.display()
            ),
        )
    })
}

/// Non-Unix platforms have no lock implementation, so opening a writer is refused outright.
///
/// Running unlocked is not the safer default here: two writers on one rollout interleave their
/// appends and hand out the same `timeline_seq`, which corrupts the log rather than degrading
/// service. Refusing keeps the failure at `open()`, where it is legible. Lifting this means
/// implementing the platform lock (`LockFileEx` on Windows), not deleting the check.
#[cfg(not(unix))]
fn lock_exclusive(_file: &File, path: &Path) -> Result<()> {
    Err(Error::session(
        SessionErrorKind::Io,
        format!(
            "rollout writing is unsupported on this platform: no advisory file lock is available \
             for {}, and running without one corrupts the log when two writers overlap",
            path.display()
        ),
    ))
}

/// Rejects a scanned range that cannot belong to this session or cannot be ordered.
fn validate_scan(
    path: &Path,
    session_id: &SessionId,
    summary: &RolloutSummary,
    seed: Option<&CheckpointSeed>,
) -> Result<()> {
    // Identity is established by the checkpoint when the fast path supplied one; a tail scan may
    // never reach a `session_meta` record.
    if let Some(found) = summary.session_id()
        && found != session_id
    {
        return Err(Error::caller(format!(
            "rollout file at {} belongs to session {}, not {}",
            path.display(),
            found.as_str(),
            session_id.as_str()
        )));
    }

    if let Some((prev, offending)) = summary.non_monotonic_timeline_seq() {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!(
                "rollout timeline_seq does not increase at {} (after {prev}, got {offending}); \
                 the file was written by more than one writer or edited externally",
                path.display()
            ),
        ));
    }

    // The join between the checkpoint and the tail has to hold that rule too; the tail scan only
    // sees records after the boundary, so it cannot notice a first record that fails to advance
    // past the checkpoint.
    if let Some(seed) = seed
        && let Some(first) = summary.first_timeline_seq()
        && first <= seed.timeline_seq
    {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!(
                "rollout timeline_seq does not increase across the checkpoint at {} \
                 (checkpoint {}, next {first})",
                path.display(),
                seed.timeline_seq
            ),
        ));
    }

    Ok(())
}

/// What a verified checkpoint contributes to recovery.
struct CheckpointSeed {
    offset: u64,
    resume_offset: u64,
    timeline_seq: u64,
    usage_totals: Usage,
    persisted_run_max_seq: HashMap<RunId, u64>,
}

/// Reads the checkpoint the sidecar points at, or `None` if it cannot be used.
///
/// The sidecar supplies only a byte offset. Everything believed afterwards comes from the record
/// found there, which is an ordinary rollout line and is parsed and checked like one: it has to
/// be a checkpoint, and it has to name this session. A wrong, stale, or missing offset therefore
/// costs a full scan and never a wrong answer, which is what lets the index live outside the log
/// when the summary itself could not.
///
/// `None` on every failure, deliberately: a miss is a normal outcome and the caller's response is
/// the same in all cases — scan from the top.
async fn checkpoint_seed(
    path: &Path,
    sidecar_path: &Path,
    session_id: &SessionId,
) -> Option<CheckpointSeed> {
    let bytes = tokio::fs::read(sidecar_path).await.ok()?;
    let sidecar: RolloutSidecar = serde_json::from_slice(&bytes).ok()?;
    let offset = sidecar.last_checkpoint_offset()?;

    read_checkpoint_at(path, offset, session_id).await
}

/// Reads the checkpoint record starting at `offset`, or `None` if this session cannot use it.
///
/// Also the test recovery applies before adopting an offset as the index: writing back one that
/// the fast path would refuse costs every later open a full scan and leaves the index unable to
/// heal itself.
async fn read_checkpoint_at(
    path: &Path,
    offset: u64,
    session_id: &SessionId,
) -> Option<CheckpointSeed> {
    let mut file = File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    if offset >= len {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).await.ok()?;

    let mut line = Vec::new();
    let read = BufReader::new(file)
        .read_until(b'\n', &mut line)
        .await
        .ok()?;
    if read == 0 || line.last() != Some(&b'\n') {
        return None;
    }

    let record: RolloutRecord = serde_json::from_slice(&line).ok()?;
    let RolloutPayload::Checkpoint(checkpoint) = record.payload().ok()? else {
        return None;
    };
    if checkpoint.session_id() != session_id {
        return None;
    }

    Some(CheckpointSeed {
        offset,
        resume_offset: offset + read as u64,
        timeline_seq: record.timeline_seq(),
        usage_totals: checkpoint.usage_totals().clone(),
        persisted_run_max_seq: checkpoint.persisted_run_max_seq().clone(),
    })
}

/// Reads the final byte of `file`, whose length is `end`, to see whether the last line is closed.
///
/// Leaves the cursor back at `end` so the caller can keep appending.
async fn ends_with_newline(file: &mut File, end: u64) -> Result<bool> {
    let io_err = |e: std::io::Error| {
        Error::session(
            SessionErrorKind::Io,
            format!("failed to inspect rollout file tail: {e}"),
        )
        .with_source(e)
    };

    file.seek(SeekFrom::Start(end - 1)).await.map_err(io_err)?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await.map_err(io_err)?;
    file.seek(SeekFrom::Start(end)).await.map_err(io_err)?;

    Ok(last[0] == b'\n')
}

impl RolloutWriter {
    /// Creates or opens a rollout file at the specified path for the given session.
    ///
    /// If the file already exists, `next_timeline_seq`, `persisted_run_max_seq` and
    /// `usage_totals` are recovered from it. When the sidecar points at a usable
    /// [`RolloutCheckpoint`], that record supplies the totals for everything before it and only
    /// the tail is scanned; otherwise the whole file is.
    ///
    /// **The fast path does not read the bytes before the checkpoint, so it cannot notice
    /// corruption there** — including a checkpoint whose own figures were edited in place, which
    /// is believed here. Anything that skips reading cannot verify what it skipped.
    /// [`RolloutReader::scan_summary`](super::reader::RolloutReader::scan_summary) is the
    /// authoritative check: it reads every record, parses every payload, and holds each
    /// checkpoint to the records it summarizes.
    /// [`RolloutReader::read_all`](super::reader::RolloutReader::read_all) is not — it validates
    /// line-level envelopes only, leaving payloads unparsed and checkpoints unchecked.
    ///
    /// A crash can leave an unterminated fragment at the end of the file. Those bytes are dropped
    /// so that subsequent appends do not splice onto them — **only** the bytes the reader
    /// classified as an unterminated fragment. A newline-terminated record is never discarded,
    /// even when this build cannot parse its payload: that is what a record written by a newer
    /// build looks like from here, and deleting it would be exactly the silent downgrade data
    /// loss that [`Unknown`] retention exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if directory creation, file opening, or recovery fails. A failure to
    /// write the sidecar is not one of them; see [`RolloutWriter::sidecar_is_stale`].
    pub async fn open(path: impl Into<PathBuf>, session_id: SessionId) -> Result<Self> {
        let path = path.into();
        let sidecar_path = sidecar_path_for(&path);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to create rollout directory: {e}"),
                )
                .with_source(e)
            })?;
        }

        // One handle for the whole lifetime, in append mode. `O_APPEND` moves every write to the
        // current end of file inside the write syscall, so a stale cursor can never land a record
        // on top of one someone else already wrote.
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to open rollout file for writing: {e}"),
                )
                .with_source(e)
            })?;

        // Take the lock before reading anything: recovery reads the tail to pick the next
        // `timeline_seq`, so scanning, allocating, and appending all have to sit inside it. A
        // second writer that scanned concurrently would choose the same sequence number.
        lock_exclusive(&file, &path)?;

        let mut recovered = Recovered::default();
        let file_len = file
            .metadata()
            .await
            .map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to stat rollout file: {e}"),
                )
                .with_source(e)
            })?
            .len();

        if file_len > 0 {
            recovered = Self::recover(&path, &sidecar_path, &session_id, &mut file).await?;
        }

        let mut writer = Self {
            session_id,
            path,
            sidecar_path,
            file,
            next_timeline_seq: recovered.next_timeline_seq,
            persisted_run_max_seq: recovered.persisted_run_max_seq,
            usage_totals: recovered.usage_totals,
            file_len: recovered.file_len,
            checkpoint_interval: DEFAULT_CHECKPOINT_INTERVAL,
            records_since_checkpoint: recovered.records_since_checkpoint,
            last_checkpoint_offset: recovered.last_checkpoint_offset,
            sidecar_stale: false,
            poisoned: false,
        };
        writer.refresh_sidecar().await;
        Ok(writer)
    }

    /// Rebuilds writer state for a file that already has bytes in it.
    ///
    /// Tries the checkpoint fast path first and falls back to a full scan. Either way the result
    /// is derived from the log: the checkpoint is a record inside the rollout, so believing it is
    /// believing the log, which is exactly what a summary stored beside the log could not offer.
    async fn recover(
        path: &Path,
        sidecar_path: &Path,
        session_id: &SessionId,
        file: &mut File,
    ) -> Result<Recovered> {
        let seed = checkpoint_seed(path, sidecar_path, session_id).await;
        let scan_from = seed.as_ref().map_or(0, |s| s.resume_offset);

        let summary = RolloutReader::open(path)
            .scan_summary_from(scan_from)
            .await?;

        validate_scan(path, session_id, &summary, seed.as_ref())?;

        if summary.corrupted_trailing_bytes().is_some() {
            file.set_len(summary.valid_bytes_len()).await.map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to truncate corrupted rollout tail: {e}"),
                )
                .with_source(e)
            })?;
        }

        let end = file
            .metadata()
            .await
            .map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to stat rollout file: {e}"),
                )
                .with_source(e)
            })?
            .len();

        // A torn write can stop on a byte boundary that still parses, leaving a record the reader
        // accepted but no newline behind it. Close the line before anything is appended, or the
        // next record would be spliced onto its tail.
        let end = if end > 0 && !ends_with_newline(file, end).await? {
            file.write_all(b"\n").await.map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to terminate trailing rollout line: {e}"),
                )
                .with_source(e)
            })?;
            end + 1
        } else {
            end
        };

        let (mut usage_totals, mut persisted_run_max_seq, checkpoint_seq, last_checkpoint_offset) =
            match seed {
                Some(seed) => (
                    seed.usage_totals,
                    seed.persisted_run_max_seq,
                    Some(seed.timeline_seq),
                    Some(seed.offset),
                ),
                None => (Usage::default(), HashMap::new(), None, None),
            };

        // A checkpoint newer than the one the index pointed at wins: the index can lag when a
        // sidecar write fails, and keeping the stale offset would make every later resume re-read
        // the same growing tail forever. It only wins if this session can use it, though —
        // indexing a checkpoint the fast path will refuse pins recovery to a full scan for good.
        let adopted_checkpoint_offset = match summary.last_checkpoint_offset() {
            Some(scanned)
                if read_checkpoint_at(path, scanned, session_id)
                    .await
                    .is_some() =>
            {
                Some(scanned)
            }
            _ => last_checkpoint_offset,
        };

        usage_totals = usage_totals.accumulate(summary.usage_totals());
        for (run_id, seq) in summary.persisted_run_max_seq() {
            let entry = persisted_run_max_seq.entry(run_id.clone()).or_insert(0);
            *entry = (*entry).max(*seq);
        }

        let last_seq = match (summary.last_timeline_seq(), checkpoint_seq) {
            (Some(tail), Some(cp)) => Some(tail.max(cp)),
            (Some(tail), None) => Some(tail),
            (None, cp) => cp,
        };

        Ok(Recovered {
            next_timeline_seq: last_seq.map_or(0, |last| last.saturating_add(1)),
            persisted_run_max_seq,
            usage_totals,
            file_len: end,
            last_checkpoint_offset: adopted_checkpoint_offset,
            // Carrying this over is what keeps the interval honest across restarts. Resetting it
            // to zero lets a process that stops just short of the interval start counting again,
            // so a session restarted often enough would never checkpoint at all.
            records_since_checkpoint: summary.records_since_last_checkpoint() as u64,
        })
    }

    /// Convenience constructor creating a rollout file named `rollout-<session_id>.jsonl` in `base_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if `session_id` contains invalid path characters or if opening fails.
    pub async fn create_for_session(
        base_dir: impl AsRef<Path>,
        session_id: SessionId,
    ) -> Result<Self> {
        let id_str = session_id.as_str();
        if id_str.contains('/') || id_str.contains('\\') || id_str.contains("..") {
            return Err(Error::caller(format!(
                "invalid characters in session_id for rollout filename: {id_str}"
            )));
        }

        let filename = format!("rollout-{id_str}.jsonl");
        let path = base_dir.as_ref().join(filename);
        Self::open(path, session_id).await
    }

    /// Session identity associated with this rollout writer.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Filesystem path of the rollout file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Filesystem path of the sidecar file.
    #[must_use]
    pub fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    /// Next timeline sequence number to be assigned.
    #[must_use]
    pub const fn next_timeline_seq(&self) -> u64 {
        self.next_timeline_seq
    }

    /// Accumulated unpruned token usage totals across all model usage records in this session.
    #[must_use]
    pub const fn usage_totals(&self) -> &Usage {
        &self.usage_totals
    }

    /// Returns the maximum persisted host event sequence number for the given run, if any.
    #[must_use]
    pub fn persisted_run_max_seq(&self, run_id: &RunId) -> Option<u64> {
        self.persisted_run_max_seq.get(run_id).copied()
    }

    /// All tracked `persisted_run_max_seq` entries.
    #[must_use]
    pub const fn persisted_run_max_seq_map(&self) -> &HashMap<RunId, u64> {
        &self.persisted_run_max_seq
    }

    /// Appends a payload to the rollout file, assigning a new `timeline_seq`.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if sequence numbers are exhausted, if writing fails, or if an earlier
    /// write left the commit state unknown — see [`RolloutWriter::is_poisoned`].
    pub async fn append(&mut self, payload: impl Into<RolloutPayload>) -> Result<RolloutRecord> {
        let record = self.append_inner(payload.into()).await?;

        // The checkpoint goes in after the record it accounts for, so it summarizes a prefix that
        // is already durable. A failure here is not the caller's to carry: the record they asked
        // for is on disk, and a missing checkpoint only costs the next resume a longer scan.
        if self.records_since_checkpoint >= self.checkpoint_interval
            && let Err(e) = self.write_checkpoint().await
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "failed to append rollout checkpoint; resume will scan further back"
            );
        }

        Ok(record)
    }

    /// Appends a checkpoint summarizing every record written so far.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn write_checkpoint(&mut self) -> Result<RolloutRecord> {
        let checkpoint = RolloutCheckpoint::new(
            self.session_id.clone(),
            self.usage_totals.clone(),
            self.persisted_run_max_seq.clone(),
        );

        let offset = self.file_len;
        let record = self
            .append_inner(RolloutPayload::Checkpoint(checkpoint))
            .await?;

        self.last_checkpoint_offset = Some(offset);
        self.records_since_checkpoint = 0;
        self.refresh_sidecar().await;

        Ok(record)
    }

    /// Sets how many records may be appended between checkpoints.
    ///
    /// Lower values shorten the tail a resume must re-read and cost one extra record more often.
    /// Zero is treated as one: every append is followed by a checkpoint.
    pub const fn set_checkpoint_interval(&mut self, records: u64) {
        self.checkpoint_interval = if records == 0 { 1 } else { records };
    }

    /// Byte offset of the newest checkpoint written or recovered, if any.
    #[must_use]
    pub const fn last_checkpoint_offset(&self) -> Option<u64> {
        self.last_checkpoint_offset
    }

    async fn append_inner(&mut self, payload: RolloutPayload) -> Result<RolloutRecord> {
        if self.poisoned {
            return Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "rollout writer for {} is poisoned by an earlier write whose outcome is \
                     unknown; drop it and reopen to recover",
                    self.path.display()
                ),
            ));
        }

        let timeline_seq = self.next_timeline_seq;
        let next_seq = timeline_seq
            .checked_add(1)
            .ok_or_else(|| Error::caller("rollout timeline sequence numbers exhausted u64::MAX"))?;
        let at = EventTimestamp::now();

        // Calculate candidate sidecar updates
        let mut candidate_run_seq = None;
        let mut candidate_usage = None;
        match &payload {
            RolloutPayload::Event(event) => {
                let current_max = self
                    .persisted_run_max_seq
                    .get(event.run_id())
                    .copied()
                    .unwrap_or(0);
                candidate_run_seq = Some((event.run_id().clone(), current_max.max(event.seq())));
            }
            RolloutPayload::ModelUsage(mu) => {
                candidate_usage = Some(self.usage_totals.accumulate(mu.usage()));
            }
            _ => {}
        }

        let record = RolloutRecord::new(timeline_seq, at, payload)?;
        let mut line = serde_json::to_string(&record).map_err(|e| {
            Error::session(
                SessionErrorKind::Corrupted,
                format!("failed to serialize rollout record: {e}"),
            )
        })?;
        line.push('\n');

        // From here to the end of the flush the commit outcome is unknown on failure: the kernel
        // may have taken none, some, or all of the bytes. The writer is poisoned rather than left
        // usable, because the two ways a caller would naturally carry on are both wrong — retrying
        // `append` would write a second record behind a partial line and fuse the two into one
        // unparsable line, and continuing with other payloads would reuse a `timeline_seq` that
        // may already be on disk. Reopening is the recovery path: it repairs an unterminated tail
        // and re-derives the sequence from what actually landed.
        if let Err(e) = self.file.write_all(line.as_bytes()).await {
            self.poisoned = true;
            return Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "failed to write rollout record at timeline_seq {timeline_seq}; the record may \
                     be partially on disk, so this writer is poisoned and must be reopened: {e}"
                ),
            )
            .with_source(e));
        }

        if let Err(e) = self.file.flush().await {
            self.poisoned = true;
            return Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "failed to flush rollout record at timeline_seq {timeline_seq}; whether it \
                     reached the file is unknown, so this writer is poisoned and must be \
                     reopened: {e}"
                ),
            )
            .with_source(e));
        }

        // Only update in-memory state after flush succeeds
        self.next_timeline_seq = next_seq;
        self.file_len += line.len() as u64;
        self.records_since_checkpoint += 1;
        if let Some((run_id, max_seq)) = candidate_run_seq {
            self.persisted_run_max_seq.insert(run_id, max_seq);
        }
        if let Some(new_usage) = candidate_usage {
            self.usage_totals = new_usage;
        }

        Ok(record)
    }

    /// Appends session metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn append_session_meta(&mut self, meta: RolloutSessionMeta) -> Result<RolloutRecord> {
        self.append(RolloutPayload::SessionMeta(meta)).await
    }

    /// Appends a turn configuration snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn append_turn_context(&mut self, ctx: RolloutTurnContext) -> Result<RolloutRecord> {
        self.append(RolloutPayload::TurnContext(ctx)).await
    }

    /// Appends a host event.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn append_event(&mut self, event: HostEvent) -> Result<RolloutRecord> {
        self.append(RolloutPayload::Event(event)).await
    }

    /// Appends a session conversation item.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn append_item(&mut self, item: RunItem) -> Result<RolloutRecord> {
        self.append(RolloutPayload::Item(item)).await
    }

    /// Appends an unprunable model usage record.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn append_model_usage(&mut self, usage: RolloutModelUsage) -> Result<RolloutRecord> {
        self.append(RolloutPayload::ModelUsage(usage)).await
    }

    /// Appends a child agent execution anchor.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if writing fails.
    pub async fn append_child_anchor(
        &mut self,
        anchor: RolloutChildAnchor,
    ) -> Result<RolloutRecord> {
        self.append(RolloutPayload::ChildAnchor(anchor)).await
    }

    /// Flushes unwritten buffers to underlying storage.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if flushing fails, which also poisons the writer: buffered bytes may
    /// have landed in part, so what actually reached the file is no longer known.
    pub async fn flush(&mut self) -> Result<()> {
        if let Err(e) = self.file.flush().await {
            self.poisoned = true;
            return Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "failed to flush rollout file; buffered bytes may be partially written, so \
                     this writer is poisoned and must be reopened: {e}"
                ),
            )
            .with_source(e));
        }
        self.refresh_sidecar().await;
        Ok(())
    }

    /// Flushes buffers and synchronizes all OS file data and metadata to disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if syncing fails, which also poisons the writer. A failed `fsync` is not
    /// merely "not durable yet": the kernel may drop the dirty pages on reporting the error, so
    /// records this writer already counted can be gone from the file.
    pub async fn sync_all(&mut self) -> Result<()> {
        if let Err(e) = self.file.sync_all().await {
            self.poisoned = true;
            return Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "failed to sync rollout file; already-counted records may not have survived, \
                     so this writer is poisoned and must be reopened: {e}"
                ),
            )
            .with_source(e));
        }
        self.refresh_sidecar().await;
        Ok(())
    }

    /// Whether an earlier write left the commit outcome unknown, disabling further appends.
    ///
    /// A poisoned writer must be dropped and the path reopened; recovery repairs whatever the
    /// failed write left behind and re-derives `next_timeline_seq` from the bytes that landed.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Whether the sidecar on disk is known to lag the writer's in-memory state.
    ///
    /// Set when a sidecar write fails. The rollout file itself is unaffected — every value in the
    /// sidecar can be rebuilt by rescanning it — so this is advisory, not an error condition.
    #[must_use]
    pub const fn sidecar_is_stale(&self) -> bool {
        self.sidecar_stale
    }

    /// Rewrites the sidecar snapshot, recording staleness rather than failing the caller.
    ///
    /// The sidecar is a derived cache. Propagating its write failure out of the `append` that
    /// preceded it would report an already-durable record as lost, and a caller that retried on
    /// that error would write the record a second time.
    async fn refresh_sidecar(&mut self) {
        match self.write_sidecar().await {
            Ok(()) => self.sidecar_stale = false,
            Err(e) => {
                self.sidecar_stale = true;
                tracing::warn!(
                    path = %self.sidecar_path.display(),
                    error = %e,
                    "failed to update rollout sidecar; resume will fall back to a full scan"
                );
            }
        }
    }

    /// Replaces the sidecar atomically, so a reader never observes a half-written document.
    ///
    /// Written to a temporary file in the same directory and renamed over the target: `rename`
    /// within a directory is atomic, whereas truncating the real file and writing into it leaves
    /// a window where a concurrent reader sees an empty or partial JSON document. The temporary
    /// name is derived from the target so a crash leaves an obvious artefact next to it rather
    /// than a corrupt sidecar.
    async fn write_sidecar(&self) -> Result<()> {
        let mut sidecar = RolloutSidecar::new(
            self.session_id.clone(),
            self.next_timeline_seq,
            self.persisted_run_max_seq.clone(),
            self.usage_totals.clone(),
        );
        if let Some(offset) = self.last_checkpoint_offset {
            sidecar = sidecar.with_last_checkpoint_offset(offset);
        }

        let data = serde_json::to_vec_pretty(&sidecar).map_err(|e| {
            Error::session(
                SessionErrorKind::Corrupted,
                format!("failed to serialize rollout sidecar: {e}"),
            )
        })?;

        let io_err = |what: &str, e: std::io::Error| {
            Error::session(
                SessionErrorKind::Io,
                format!("failed to {what} rollout sidecar file: {e}"),
            )
            .with_source(e)
        };

        let mut tmp_name = self.sidecar_path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);

        tokio::fs::write(&tmp_path, data)
            .await
            .map_err(|e| io_err("write", e))?;

        match tokio::fs::rename(&tmp_path, &self.sidecar_path).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                Err(io_err("replace", e))
            }
        }
    }
}
