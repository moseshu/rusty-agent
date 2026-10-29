// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
//! Optional model-catalog overrides for harness-owned incremental tool notices.

use serde::Deserialize;
use serde::Serialize;

/// Incremental tool notice overrides; missing/null or >512-byte values use bundled defaults.
/// Empty strings omit static text but retain runtime names and instructions.
#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct IncrementalToolMessages {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_update_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_tools_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_namespaces_header: Option<String>,
    /// Prefix supporting `{name}`; the namespace instructions are appended verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_instructions_prefix: Option<String>,
    /// Message supporting `{name}` when a namespace's instructions are cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_instructions_cleared: Option<String>,
}
