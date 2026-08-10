//! Run items: the boundary between authoritative session records and model-input views.
//!
//! [`RunItem`] is a complete session record. In addition to a provider-neutral payload, it can
//! retain the producing agent, a raw provider copy, and host data that must not be sent to the
//! model. [`ModelInputItem`] is the dedicated outbound type: projection strips that metadata, and
//! the type has no [`ToolApproval`] variant.
//!
//! These sets must not collapse into one `serde_json::Value`. Doing so makes it easy for resume to
//! send approval records, UI data, or another provider's raw payload in a model request.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compat::{SchemaVersion, Unknown};

pub mod compaction;
pub mod content;
pub mod handoff;
pub mod mcp;
pub mod message;
#[doc(hidden)]
pub mod normalization;
pub mod phase;
pub mod reasoning;
pub mod tool;

pub use compaction::Compaction;
pub use content::{
    Base64FileSource, Base64ImageSource, ContentBlock, FileBlock, FileSource, ImageBlock,
    ImageDetail, ImageSource, LocalImageSource, ProviderFileSource, RefusalBlock, TextBlock,
    ThinkingBlock, UrlSource,
};
pub use handoff::{HandoffCall, HandoffOutput};
pub use mcp::{McpApprovalRequest, McpApprovalResponse, McpListTools, McpTool};
pub use message::{Message, MessageRole, ModelResponse};
#[doc(hidden)]
pub use normalization::{
    InputItemDigest, InputItemNormalizer, InputItemOccurrenceKey, NormalizedInput,
    NormalizedInputItem, OrphanPolicy, ReasoningIdPolicy,
};
pub use phase::OutputPhase;
pub use reasoning::Reasoning;
pub use tool::{ToolApproval, ToolCall, ToolCallOutput};

/// Current run-item schema version.
pub const RUN_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current item-provenance schema version.
pub const ITEM_PROVENANCE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current raw-provider-item schema version.
pub const RAW_PROVIDER_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Stable ID of an item in a session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    /// Creates an ID from a stable host-generated string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// String representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ItemId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pairing ID for a tool, handoff, or MCP call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(String);

impl CallId {
    /// Creates a call ID.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// String representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for CallId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Public identity of the agent that produced an item.
///
/// Only the stable ID is stored here. Immutable `AgentSpec` and execution-agent bindings arrive
/// in R3-1c/R3-12; a session item must not retain an entire agent configuration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    /// Creates an agent ID.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// String representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for AgentId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Information about the producer of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemProvenance {
    schema_version: SchemaVersion,
    agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ItemProvenance {
    /// Creates provenance.
    #[must_use]
    pub fn new(agent_id: AgentId) -> Self {
        Self {
            schema_version: ITEM_PROVENANCE_SCHEMA_VERSION,
            agent_id,
            agent_name: None,
            unknown: Unknown::new(),
        }
    }

    /// Adds an agent name for display. Identity checks still use only the stable ID.
    #[must_use]
    pub fn with_agent_name(mut self, name: impl Into<String>) -> Self {
        self.agent_name = Some(name.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Stable agent ID.
    #[must_use]
    pub const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Display name.
    #[must_use]
    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Isolated copy of a raw provider item.
///
/// This supports exact replay and diagnostics but never becomes next-turn input directly. The
/// adapter must lower a [`ModelInputItem`] again. `provider` is a registry alias, not a protocol
/// enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawProviderItem {
    schema_version: SchemaVersion,
    provider: String,
    payload: Value,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RawProviderItem {
    /// Creates a raw provider copy.
    #[must_use]
    pub fn new(provider: impl Into<String>, payload: Value) -> Self {
        Self {
            schema_version: RAW_PROVIDER_ITEM_SCHEMA_VERSION,
            provider: provider.into(),
            payload,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Registered provider alias.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Raw JSON payload.
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Host extension data stored only in the session and never sent to the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionData(BTreeMap<String, Value>);

impl SessionData {
    /// Creates an empty data set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns one value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Iterates in lexical key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    fn insert(&mut self, key: String, value: Value) {
        self.0.insert(key, value);
    }
}

/// Payload in an authoritative session record.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunItemKind {
    /// Message.
    Message(Message),
    /// Reasoning and its replay material.
    Reasoning(Reasoning),
    /// Tool call.
    ToolCall(ToolCall),
    /// Tool-call output.
    ToolCallOutput(ToolCallOutput),
    /// Handoff call.
    HandoffCall(HandoffCall),
    /// Handoff completion record.
    HandoffOutput(HandoffOutput),
    /// MCP tool-list result.
    McpListTools(McpListTools),
    /// MCP approval request.
    McpApprovalRequest(McpApprovalRequest),
    /// MCP approval response.
    McpApprovalResponse(McpApprovalResponse),
    /// Compaction summary.
    Compaction(Compaction),
    /// Tool call awaiting host approval. This variant cannot be projected into model input.
    ToolApproval(ToolApproval),
}

impl RunItemKind {
    /// Stable machine-readable kind name.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Reasoning(_) => "reasoning",
            Self::ToolCall(_) => "tool_call",
            Self::ToolCallOutput(_) => "tool_call_output",
            Self::HandoffCall(_) => "handoff_call",
            Self::HandoffOutput(_) => "handoff_output",
            Self::McpListTools(_) => "mcp_list_tools",
            Self::McpApprovalRequest(_) => "mcp_approval_request",
            Self::McpApprovalResponse(_) => "mcp_approval_response",
            Self::Compaction(_) => "compaction",
            Self::ToolApproval(_) => "tool_approval",
        }
    }

    /// Whether this item is a pending decision the host has to answer before the run continues.
    ///
    /// [`NextStep::Interruption`](crate::step::NextStep::Interruption) carries exactly these, and
    /// R3-2 classifies a response with the same predicate, so the two cannot drift apart. The match
    /// is exhaustive on purpose: a new item kind must be classified here rather than defaulting to
    /// "not an interruption", because that default is the silent one — the run would continue past
    /// a decision nobody made.
    #[must_use]
    pub const fn is_interruption(&self) -> bool {
        match self {
            Self::ToolApproval(_) | Self::McpApprovalRequest(_) => true,
            Self::Message(_)
            | Self::Reasoning(_)
            | Self::ToolCall(_)
            | Self::ToolCallOutput(_)
            | Self::HandoffCall(_)
            | Self::HandoffOutput(_)
            | Self::McpListTools(_)
            | Self::McpApprovalResponse(_)
            | Self::Compaction(_) => false,
        }
    }

    /// Whether response classification has to bind this item to an action it can answer.
    ///
    /// These are the kinds that leave the conversation invalid when nothing answers them: a call
    /// with no paired output makes the next request malformed, and a hosted approval request with
    /// no decision leaves the server waiting. R3-2's classification must claim every one of them,
    /// which is what stops an unanswered call from being filed as an inert record.
    ///
    /// [`Self::ToolApproval`] is deliberately `false` even though it also awaits an answer: it is a
    /// control-plane record the model never produces, and [`Self::is_interruption`] is the
    /// predicate that carries it. The match is exhaustive for the same reason as that one — a new
    /// kind has to say which side it is on rather than defaulting to the silent one.
    #[must_use]
    pub const fn requires_action_binding(&self) -> bool {
        match self {
            Self::ToolCall(_) | Self::HandoffCall(_) | Self::McpApprovalRequest(_) => true,
            Self::Message(_)
            | Self::Reasoning(_)
            | Self::ToolCallOutput(_)
            | Self::HandoffOutput(_)
            | Self::McpListTools(_)
            | Self::McpApprovalResponse(_)
            | Self::Compaction(_)
            | Self::ToolApproval(_) => false,
        }
    }
}

/// A complete authoritative session record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunItem {
    schema_version: SchemaVersion,
    id: ItemId,
    kind: RunItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<ItemProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raw_provider_item: Option<RawProviderItem>,
    #[serde(default, skip_serializing_if = "SessionData::is_empty")]
    session_data: SessionData,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RunItem {
    /// Creates an authoritative record. The session owner generates the ID; core does not add
    /// implicit randomness.
    #[must_use]
    pub fn new(id: ItemId, kind: RunItemKind) -> Self {
        Self {
            schema_version: RUN_ITEM_SCHEMA_VERSION,
            id,
            kind,
            provenance: None,
            raw_provider_item: None,
            session_data: SessionData::new(),
            unknown: Unknown::new(),
        }
    }

    /// Adds producer information.
    #[must_use]
    pub fn with_provenance(mut self, provenance: ItemProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// Adds a raw provider copy.
    #[must_use]
    pub fn with_raw_provider_item(mut self, raw: RawProviderItem) -> Self {
        self.raw_provider_item = Some(raw);
        self
    }

    /// Adds host data stored only in the session.
    #[must_use]
    pub fn with_session_data(mut self, key: impl Into<String>, value: Value) -> Self {
        self.session_data.insert(key.into(), value);
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Item ID.
    #[must_use]
    pub const fn id(&self) -> &ItemId {
        &self.id
    }

    /// Item payload.
    #[must_use]
    pub const fn kind(&self) -> &RunItemKind {
        &self.kind
    }

    /// Sets the output phase on an assistant message while preserving this record's envelope.
    ///
    /// Other record kinds, and messages from user or system roles, are left untouched. Turn
    /// settlement uses this once it knows how the turn ended: a provider is not authoritative
    /// about whether what it just said was progress or the run's closing delivery.
    #[must_use]
    pub fn with_output_phase(mut self, phase: OutputPhase) -> Self {
        if let RunItemKind::Message(message) = &mut self.kind
            && matches!(message.role(), MessageRole::Assistant)
        {
            message.set_phase(phase);
        }
        self
    }

    /// Producer information.
    #[must_use]
    pub const fn provenance(&self) -> Option<&ItemProvenance> {
        self.provenance.as_ref()
    }

    /// Raw provider copy.
    #[must_use]
    pub const fn raw_provider_item(&self) -> Option<&RawProviderItem> {
        self.raw_provider_item.as_ref()
    }

    /// Host data stored only in the session.
    #[must_use]
    pub const fn session_data(&self) -> &SessionData {
        &self.session_data
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Returns the call ID when this item participates in call/output pairing.
    #[must_use]
    pub const fn call_id(&self) -> Option<&CallId> {
        match &self.kind {
            RunItemKind::ToolCall(item) => Some(item.call_id()),
            RunItemKind::ToolCallOutput(item) => Some(item.call_id()),
            RunItemKind::HandoffCall(item) => Some(item.call_id()),
            RunItemKind::HandoffOutput(item) => Some(item.call_id()),
            RunItemKind::ToolApproval(item) => Some(item.call_id()),
            _ => None,
        }
    }

    /// Whether this record can be projected into model input.
    #[must_use]
    pub const fn is_model_input(&self) -> bool {
        !matches!(self.kind, RunItemKind::ToolApproval(_))
    }

    /// Creates a clean view to send to the model.
    ///
    /// `None` means the item belongs to the session control plane; it is not a conversion error.
    #[must_use]
    pub fn to_model_input(&self) -> Option<ModelInputItem> {
        match &self.kind {
            RunItemKind::Message(item) => Some(ModelInputItem::Message(item.clone())),
            RunItemKind::Reasoning(item) => Some(ModelInputItem::Reasoning(item.clone())),
            RunItemKind::ToolCall(item) => Some(ModelInputItem::ToolCall(item.clone())),
            RunItemKind::ToolCallOutput(item) => Some(ModelInputItem::ToolCallOutput(item.clone())),
            RunItemKind::HandoffCall(item) => Some(ModelInputItem::HandoffCall(item.clone())),
            RunItemKind::HandoffOutput(item) => Some(ModelInputItem::HandoffOutput(item.clone())),
            RunItemKind::McpListTools(item) => Some(ModelInputItem::McpListTools(item.clone())),
            RunItemKind::McpApprovalRequest(item) => {
                Some(ModelInputItem::McpApprovalRequest(item.clone()))
            }
            RunItemKind::McpApprovalResponse(item) => {
                Some(ModelInputItem::McpApprovalResponse(item.clone()))
            }
            RunItemKind::Compaction(item) => Some(ModelInputItem::Compaction(item.clone())),
            RunItemKind::ToolApproval(_) => None,
        }
    }
}

/// A provider-neutral input item that may be sent to a model.
///
/// This type intentionally carries no `ItemId`, provenance, raw provider payload, or session data;
/// none of them are model input. It also intentionally has no `ToolApproval` variant, preventing
/// adapters from forgetting to filter approval records.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ModelInputItem {
    /// Message.
    Message(Message),
    /// Reasoning replay item.
    Reasoning(Reasoning),
    /// Tool call.
    ToolCall(ToolCall),
    /// Tool output.
    ToolCallOutput(ToolCallOutput),
    /// Handoff call.
    HandoffCall(HandoffCall),
    /// Handoff completion record.
    HandoffOutput(HandoffOutput),
    /// MCP tool list.
    McpListTools(McpListTools),
    /// MCP approval request.
    McpApprovalRequest(McpApprovalRequest),
    /// MCP approval response.
    McpApprovalResponse(McpApprovalResponse),
    /// Compaction summary.
    Compaction(Compaction),
}

impl ModelInputItem {
    /// Stable machine-readable kind name.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Reasoning(_) => "reasoning",
            Self::ToolCall(_) => "tool_call",
            Self::ToolCallOutput(_) => "tool_call_output",
            Self::HandoffCall(_) => "handoff_call",
            Self::HandoffOutput(_) => "handoff_output",
            Self::McpListTools(_) => "mcp_list_tools",
            Self::McpApprovalRequest(_) => "mcp_approval_request",
            Self::McpApprovalResponse(_) => "mcp_approval_response",
            Self::Compaction(_) => "compaction",
        }
    }

    /// Returns the call ID when this item participates in call/output pairing.
    #[must_use]
    pub const fn call_id(&self) -> Option<&CallId> {
        match self {
            Self::ToolCall(item) => Some(item.call_id()),
            Self::ToolCallOutput(item) => Some(item.call_id()),
            Self::HandoffCall(item) => Some(item.call_id()),
            Self::HandoffOutput(item) => Some(item.call_id()),
            _ => None,
        }
    }
}
