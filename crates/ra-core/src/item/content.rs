//! Message content blocks.
//!
//! R1-1 implements only the text block required by the item model. R1-2 will add image,
//! thinking, tool-use, and other multimodal variants. The enum is `#[non_exhaustive]`, so adding
//! variants will not force downstream code to match them exhaustively.

use serde::{Deserialize, Serialize};

use crate::compat::{SchemaVersion, Unknown};

/// Current content-block schema version.
pub const CONTENT_BLOCK_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A provider-neutral message content block.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ContentBlock {
    /// UTF-8 text.
    Text(TextBlock),
}

impl ContentBlock {
    /// Creates a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock::new(text))
    }

    /// Returns the text when this is a text block.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(block) => Some(block.text()),
        }
    }
}

/// A text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlock {
    schema_version: SchemaVersion,
    text: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl TextBlock {
    /// Creates a text block.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            text: text.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Text content.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
