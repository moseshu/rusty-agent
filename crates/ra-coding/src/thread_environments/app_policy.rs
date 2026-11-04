// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The enabled projection of connectors' `AppToolPolicyEvaluator`, used for cleanup hooks only.
//! Approval policy is deliberately left to the existing MCP execution path.

use ra_config::{ConfigLayerStack, types::AppsConfigToml};
use ra_mcp::catalog::McpToolInfo;
use serde::Deserialize;

pub(crate) fn app_tool_enabled(stack: &ConfigLayerStack, info: &McpToolInfo) -> bool {
    let mut apps = stack
        .effective_config()
        .as_table()
        .and_then(|table| table.get("apps"))
        .cloned()
        .and_then(|value| AppsConfigToml::deserialize(value).ok())
        .unwrap_or_default();
    if let Some(requirements) = stack.requirements_toml().apps.as_ref() {
        for (id, requirement) in &requirements.apps {
            if requirement.enabled == Some(false) {
                apps.apps.entry(id.clone()).or_default().enabled = false;
            }
        }
    }
    let app = info.connector_id().and_then(|id| apps.apps.get(id));
    if !app.map_or_else(
        || {
            apps.default
                .as_ref()
                .is_none_or(|defaults| defaults.enabled)
        },
        |app| app.enabled,
    ) {
        return false;
    }
    let tool = info.tool();
    let configured = app.and_then(|app| app.tools.as_ref()).and_then(|tools| {
        tools.tools.get(tool.name.as_ref()).or_else(|| {
            tool.title
                .as_deref()
                .and_then(|title| tools.tools.get(title))
        })
    });
    if let Some(enabled) = configured.and_then(|tool| tool.enabled) {
        return enabled;
    }
    if let Some(enabled) = app.and_then(|app| app.default_tools_enabled) {
        return enabled;
    }
    let destructive = app
        .and_then(|app| app.destructive_enabled)
        .unwrap_or_else(|| {
            apps.default
                .as_ref()
                .is_none_or(|defaults| defaults.destructive_enabled)
        });
    let open_world = app
        .and_then(|app| app.open_world_enabled)
        .unwrap_or_else(|| {
            apps.default
                .as_ref()
                .is_none_or(|defaults| defaults.open_world_enabled)
        });
    (destructive
        || !tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.destructive_hint)
            .unwrap_or(true))
        && (open_world
            || !tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.open_world_hint)
                .unwrap_or(true))
}
