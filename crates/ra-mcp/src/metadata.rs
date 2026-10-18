//! MCP display metadata, following the Python SDK's metadata helpers.
//!
//! UI metadata retains the original description and resolved title. Only the model-facing
//! description falls back to the title. Empty strings are absent; whitespace is preserved.

use rmcp::model::Tool;
use std::sync::Arc;

/// Resolved display metadata for an MCP tool, corresponding to Python's frozen metadata value.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct McpToolMetadata {
    description: Option<String>,
    title: Option<String>,
}

impl McpToolMetadata {
    /// Original long-form description, without a title fallback.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Display title, preferring explicit title over annotations.title, without a name fallback.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Description for a model definition: description, then title, then the empty string.
    #[must_use]
    pub fn description_for_model(&self) -> &str {
        self.description()
            .or_else(|| self.title())
            .unwrap_or_default()
    }
}

impl From<&Tool> for McpToolMetadata {
    fn from(tool: &Tool) -> Self {
        Self {
            description: non_empty(tool.description.as_deref()).map(str::to_owned),
            title: non_empty(tool.title.as_deref())
                .or_else(|| {
                    non_empty(
                        tool.annotations
                            .as_ref()
                            .and_then(|value| value.title.as_deref()),
                    )
                })
                .map(str::to_owned),
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// One immutable declaration and its metadata travel together through cache and filtering.
#[derive(Clone)]
pub(crate) struct CachedTool {
    pub(crate) tool: Tool,
    pub(crate) metadata: Arc<McpToolMetadata>,
}

impl From<Tool> for CachedTool {
    fn from(tool: Tool) -> Self {
        Self {
            metadata: Arc::new(McpToolMetadata::from(&tool)),
            tool,
        }
    }
}
