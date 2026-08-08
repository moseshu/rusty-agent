//! Reasoning items and cross-provider replay material.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compat::{SchemaVersion, Unknown};

/// Current reasoning schema version.
pub const REASONING_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A provider-neutral reasoning item.
///
/// `summary` and `content` are normalized views for the UI and provider-neutral logic.
/// `provider_data` is the complete replay source of truth. Empty Anthropic thinking blocks,
/// redacted thinking, and multi-signature sequences cannot always be represented losslessly by
/// the normalized fields, so both layers are retained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    schema_version: SchemaVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default)]
    summary: Vec<String>,
    #[serde(default)]
    content: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_data: Option<Value>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Reasoning {
    /// Creates an empty reasoning item for further configuration through the builder methods.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: REASONING_SCHEMA_VERSION,
            id: None,
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: None,
            provider_data: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the provider reasoning ID.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Replaces the normalized summary segments.
    #[must_use]
    pub fn with_summary(mut self, summary: Vec<String>) -> Self {
        self.summary = summary;
        self
    }

    /// Replaces the normalized reasoning text segments.
    #[must_use]
    pub fn with_content(mut self, content: Vec<String>) -> Self {
        self.content = content;
        self
    }

    /// Sets encrypted content or signatures.
    #[must_use]
    pub fn with_encrypted_content(mut self, encrypted: impl Into<String>) -> Self {
        self.encrypted_content = Some(encrypted.into());
        self
    }

    /// Sets the complete provider replay data.
    #[must_use]
    pub fn with_provider_data(mut self, data: Value) -> Self {
        self.provider_data = Some(data);
        self
    }

    /// Removes only the provider item ID while retaining every replay-bearing field.
    pub(super) fn without_id(mut self) -> Self {
        self.id = None;
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Provider reasoning ID.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Normalized summary segments.
    #[must_use]
    pub fn summary(&self) -> &[String] {
        &self.summary
    }

    /// Normalized reasoning text segments.
    #[must_use]
    pub fn content(&self) -> &[String] {
        &self.content
    }

    /// Encrypted content or signatures.
    #[must_use]
    pub fn encrypted_content(&self) -> Option<&str> {
        self.encrypted_content.as_deref()
    }

    /// Complete provider replay data.
    #[must_use]
    pub const fn provider_data(&self) -> Option<&Value> {
        self.provider_data.as_ref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for Reasoning {
    fn default() -> Self {
        Self::new()
    }
}
