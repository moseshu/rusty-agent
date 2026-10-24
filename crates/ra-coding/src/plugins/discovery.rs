// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host manifest discovery and format precedence from Codex.

use std::path::{Path, PathBuf};

/// Legacy manifest candidates, highest precedence first.
pub const DISCOVERABLE_PLUGIN_MANIFEST_PATHS: &[&str] = &[
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/plugin.json",
];

/// The portable manifest location below a package root.
pub const AGENT_PLUGIN_MANIFEST_RELATIVE_PATH: &str = "plugin.json";
/// Published Agent Plugins v1 manifest schema:
/// <https://github.com/agentplugins/agent-plugins-spec/blob/main/schemas/1.0.0/plugin.schema.json>
pub const AGENT_PLUGIN_SCHEMA_URI: &str =
    "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
/// Supported portable manifest schema identifiers.
pub const SUPPORTED_AGENT_PLUGIN_SCHEMA_URIS: &[&str] = &[AGENT_PLUGIN_SCHEMA_URI];
/// The prefix identifying portable plugin schema versions.
pub const AGENT_PLUGIN_SCHEMA_PREFIX: &str = "https://agent-plugins.org/schemas/";

/// Whether a root document belongs to a recognized portable schema family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPluginSchemaStatus {
    /// A supported portable schema.
    Supported,
    /// A recognized schema family with an unsupported version.
    Unsupported,
    /// A document not recognized as a portable manifest.
    Unrelated,
}

/// Classifies a root JSON document without validating the manifest fields.
#[must_use]
pub fn agent_plugin_schema_status(contents: &str) -> AgentPluginSchemaStatus {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(contents) else {
        return AgentPluginSchemaStatus::Unrelated;
    };
    let Some(schema) = value.get("$schema").and_then(serde_json::Value::as_str) else {
        return AgentPluginSchemaStatus::Unrelated;
    };
    if SUPPORTED_AGENT_PLUGIN_SCHEMA_URIS.contains(&schema) {
        AgentPluginSchemaStatus::Supported
    } else if schema.starts_with(AGENT_PLUGIN_SCHEMA_PREFIX) {
        AgentPluginSchemaStatus::Unsupported
    } else {
        AgentPluginSchemaStatus::Unrelated
    }
}

/// Selects a manifest without following manifest or metadata-directory symlinks.
#[must_use]
pub fn find_plugin_manifest_path(plugin_root: &Path) -> Option<PathBuf> {
    let agent_manifest_path = plugin_root.join(AGENT_PLUGIN_MANIFEST_RELATIVE_PATH);
    match std::fs::symlink_metadata(&agent_manifest_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            return None;
        }
        Ok(_) => {
            if std::fs::read_to_string(&agent_manifest_path)
                .ok()
                .is_some_and(|contents| {
                    agent_plugin_schema_status(&contents) != AgentPluginSchemaStatus::Unrelated
                })
            {
                return Some(agent_manifest_path);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }

    for relative_path in DISCOVERABLE_PLUGIN_MANIFEST_PATHS {
        let manifest_path = plugin_root.join(relative_path);
        let manifest_parent = manifest_path.parent()?;
        match std::fs::symlink_metadata(manifest_parent) {
            Ok(metadata) if !metadata.file_type().is_dir() => return None,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
        match std::fs::symlink_metadata(&manifest_path) {
            Ok(metadata) if metadata.file_type().is_file() => return Some(manifest_path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => return None,
        }
    }
    None
}
