//! Model-visible output of one tool invocation.

use serde::{Deserialize, Serialize};

/// A provider-neutral tool result.
///
/// R2-3 adds image/file blocks and observation metadata. Starting with the text form keeps the
/// `Tool::call` signature stable while that richer value layer is developed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutput {
    /// Plain model-visible text.
    Text {
        /// Output text.
        text: String,
    },
}

impl ToolOutput {
    /// Creates a text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Text projection when this is a text result.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
        }
    }
}
