//! Provider-neutral messages and terminal model responses.

use serde::{Deserialize, Serialize};

use super::{ContentBlock, ModelInputItem, OutputPhase, RunItem};
use crate::{
    compat::{SchemaVersion, Unknown},
    usage::Usage,
};

/// Current message schema version.
pub const MESSAGE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current model-response schema version.
pub const MODEL_RESPONSE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Message role.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// Stable or dynamic system instructions.
    System,
    /// User input.
    User,
    /// Model output.
    Assistant,
}

impl MessageRole {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant", // layering-allow: assistant = model role, not a product
        }
    }
}

/// A provider-neutral message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    schema_version: SchemaVersion,
    role: MessageRole,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phase: Option<OutputPhase>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Message {
    /// Creates a message.
    #[must_use]
    pub fn new(role: MessageRole, content: Vec<ContentBlock>) -> Self {
        Self {
            schema_version: MESSAGE_SCHEMA_VERSION,
            role,
            content,
            phase: None,
            unknown: Unknown::new(),
        }
    }

    /// Creates a message containing one text block.
    #[must_use]
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self::new(role, vec![ContentBlock::text(text)])
    }

    /// Creates a user text message.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::text(MessageRole::User, text)
    }

    /// Creates a system text message.
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self::text(MessageRole::System, text)
    }

    /// Creates an assistant text message with an output phase.
    #[must_use]
    pub fn assistant(text: impl Into<String>, phase: OutputPhase) -> Self {
        let mut message = Self::text(MessageRole::Assistant, text);
        message.phase = Some(phase);
        message
    }

    /// Sets the assistant output phase while preserving all content blocks.
    #[must_use]
    pub const fn with_phase(mut self, phase: OutputPhase) -> Self {
        self.set_phase(phase);
        self
    }

    /// In-place form, so rewriting one field does not copy the content blocks.
    pub(crate) const fn set_phase(&mut self, phase: OutputPhase) {
        self.phase = Some(phase);
    }

    /// Whether two messages agree on everything except the output channel.
    ///
    /// Compared by aligning the one field and testing the whole value, rather than by listing the
    /// others: a field added to this struct later has to be covered by the comparison without
    /// anyone remembering to extend it here.
    pub(crate) fn matches_ignoring_phase(&self, other: &Self) -> bool {
        let mut aligned = self.clone();
        aligned.phase = other.phase;
        aligned == *other
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Message role.
    #[must_use]
    pub const fn role(&self) -> MessageRole {
        self.role
    }

    /// Content blocks.
    #[must_use]
    pub fn content(&self) -> &[ContentBlock] {
        &self.content
    }

    /// Assistant output phase.
    #[must_use]
    pub const fn phase(&self) -> Option<OutputPhase> {
        self.phase
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Concatenates all text blocks and skips every non-text block.
    ///
    /// A refusal is not text and never appears here; ask [`Self::refusal_content`] for it.
    #[must_use]
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect()
    }

    /// Concatenates all refusal blocks, or `None` when the model did not refuse.
    ///
    /// This is the mechanical signal R1-12 escalates on. Matching refusal wording inside
    /// [`Self::text_content`] would be a guess about provider phrasing.
    #[must_use]
    pub fn refusal_content(&self) -> Option<String> {
        let refusal: String = self
            .content
            .iter()
            .filter_map(ContentBlock::as_refusal)
            .collect();
        (!refusal.is_empty()).then_some(refusal)
    }
}

/// Provider-neutral terminal state of a complete model call.
///
/// `response_id` is optional; providers without server-side continuation IDs, such as Chat and
/// Anthropic, leave it as `None`. `request_id` is transport diagnostics only and must not be used
/// as conversation identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    schema_version: SchemaVersion,
    #[serde(default)]
    output: Vec<RunItem>,
    #[serde(default)]
    usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ModelResponse {
    /// Creates a model response.
    #[must_use]
    pub fn new(output: Vec<RunItem>) -> Self {
        Self {
            schema_version: MODEL_RESPONSE_SCHEMA_VERSION,
            output,
            usage: Usage::default(),
            response_id: None,
            request_id: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets usage for this call.
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Sets the provider response ID.
    #[must_use]
    pub fn with_response_id(mut self, response_id: impl Into<String>) -> Self {
        self.response_id = Some(response_id.into());
        self
    }

    /// Sets the transport request ID.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Model output items.
    #[must_use]
    pub fn output(&self) -> &[RunItem] {
        &self.output
    }

    /// Usage totals.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Response ID available for provider-side continuation.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Request ID for this transport call.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Converts output into input for the next model turn.
    ///
    /// Session-only approval items are excluded by the typed projection. Provenance, raw provider
    /// copies, and custom session data are also omitted.
    #[must_use]
    pub fn to_input_items(&self) -> Vec<ModelInputItem> {
        self.output
            .iter()
            .filter_map(RunItem::to_model_input)
            .collect()
    }
}
