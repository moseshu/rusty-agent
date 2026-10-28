// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Granular approval configuration categories.
#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Preserve independent fields in the source configuration schema."
)]
pub struct GranularApprovalConfig {
    /// Whether to allow shell command approval requests, including inline
    /// `with_additional_permissions` and `require_escalated` requests.
    pub sandbox_approval: bool,
    /// Whether to allow prompts triggered by execpolicy `prompt` rules.
    pub rules: bool,
    /// Whether to allow approval prompts triggered by skill script execution.
    #[serde(default)]
    pub skill_approval: bool,
    /// Whether to allow prompts triggered by the `request_permissions` tool.
    #[serde(default)]
    pub request_permissions: bool,
    /// Whether to allow MCP elicitation prompts.
    pub mcp_elicitations: bool,
}
