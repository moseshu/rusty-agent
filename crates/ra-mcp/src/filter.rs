// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Static filtering using original protocol tool names.

use crate::catalog::McpToolInfo;
use std::collections::HashSet;

/// An optional allowlist followed by a denylist; denial always wins.
#[derive(Debug, Default, Clone)]
pub struct ToolFilter {
    enabled: Option<HashSet<String>>,
    disabled: HashSet<String>,
}

impl ToolFilter {
    /// Creates a filter. An empty explicit allowlist excludes every tool.
    #[must_use]
    pub fn new(enabled: Option<HashSet<String>>, disabled: HashSet<String>) -> Self {
        Self { enabled, disabled }
    }
    /// Whether an original MCP name passes both lists.
    #[must_use]
    pub fn allows(&self, tool_name: &str) -> bool {
        if let Some(enabled) = &self.enabled
            && !enabled.contains(tool_name)
        {
            return false;
        }
        !self.disabled.contains(tool_name)
    }
    /// Applies the filter without changing original or allocated identities.
    #[must_use]
    pub fn filter_tools(&self, tools: Vec<McpToolInfo>) -> Vec<McpToolInfo> {
        tools
            .into_iter()
            .filter(|tool| self.allows(&tool.tool.name))
            .collect()
    }
}

/// Tools without UI visibility metadata remain visible; explicit visibility must include model.
#[must_use]
pub fn tool_is_model_visible(tool: &rmcp::model::Tool) -> bool {
    let Some(visibility) = tool
        .meta
        .as_deref()
        .and_then(|meta| meta.get("ui"))
        .and_then(serde_json::Value::as_object)
        .and_then(|ui| ui.get("visibility"))
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    visibility
        .iter()
        .any(|target| target.as_str() == Some("model"))
}
