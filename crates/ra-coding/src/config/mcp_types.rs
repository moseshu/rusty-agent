// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Shared product MCP approval and per-tool configuration values.

use std::num::NonZeroUsize;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Approval policy carried by product MCP configuration.
///
/// This is distinct from the Agents SDK's server-owned `require_approval` contract.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppToolApproval {
    /// Apply the product's automatic approval policy.
    #[default]
    Auto,
    /// Request approval for every tool call.
    Prompt,
    /// Request approval for tools that write.
    Writes,
    /// Approve without requesting additional approval from this policy.
    Approve,
}

impl AppToolApproval {
    /// Requires approval whenever either policy could require it.
    ///
    /// `Auto` and `Writes` are incomparable; their conservative intersection is `Prompt`.
    #[must_use]
    pub fn restrict_to(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Prompt, _)
            | (_, Self::Prompt)
            | (Self::Auto, Self::Writes)
            | (Self::Writes, Self::Auto) => Self::Prompt,
            (Self::Approve, mode) | (mode, Self::Approve) => mode,
            (Self::Auto, Self::Auto) => Self::Auto,
            (Self::Writes, Self::Writes) => Self::Writes,
        }
    }
}

/// Per-tool settings for a single product MCP server tool.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct McpServerToolConfig {
    /// Approval mode for this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<AppToolApproval>,
    /// Output token budget before the standard serialization allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_limit: Option<NonZeroUsize>,
}

impl McpServerToolConfig {
    /// Applies the stricter explicit output budget without changing approval policy.
    pub fn restrict_output_token_limit(&mut self, limit: Option<NonZeroUsize>) {
        self.output_token_limit = self.output_token_limit.into_iter().chain(limit).min();
    }
}
