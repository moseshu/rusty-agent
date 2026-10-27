// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Inline executor hook declarations and trusted routing.
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use super::PluginId;
use ra_config::HookEventsToml;
use ra_paths::PathUri;
use serde_json::{Map, Value};
/// Inline plugin hooks discovered in an executor environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorPluginHookSource {
    pub plugin_id: PluginId,
    pub environment_id: String,
    /// An admitted MCP target can run outside the plugin's source environment.
    pub mcp_environment_id: Option<String>,
    /// Trusted MCP routing metadata for this cleanup target.
    pub mcp_metadata: Option<Map<String, Value>>,
    pub plugin_root: PathUri,
    pub manifest_path: PathUri,
    pub source_relative_path: String,
    pub hooks: HookEventsToml,
}
