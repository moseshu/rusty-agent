// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain source skill metadata, loading and snapshot contracts."
)]
use crate::skills::SkillDiscoveryMode;
use ra_paths::AbsolutePathBuf;
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginIdentity {
    pub plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginSkillRoot {
    pub path: AbsolutePathBuf,
    pub plugin_identity: PluginIdentity,
    pub plugin_namespace: String,
    pub plugin_root: AbsolutePathBuf,
    pub discovery_mode: SkillDiscoveryMode,
}
