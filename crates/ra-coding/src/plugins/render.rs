// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain the source bounded plugin instruction renderer."
)]
use super::PluginCapabilitySummary;
const MAX_EXPLICIT_PLUGIN_INSTRUCTIONS_BYTES: usize = 4 * 1024;
const TRUNCATED_PLUGIN_INSTRUCTIONS_SUFFIX: &str =
    "\n- Additional plugin capabilities omitted to fit the context limit.";

#[must_use]
pub fn render_explicit_plugin_instructions(
    plugin: &PluginCapabilitySummary,
    available_mcp_servers: &[String],
    available_apps: &[String],
) -> Option<String> {
    let mut lines = vec![format!(
        "Capabilities from the `{}` plugin:",
        plugin.display_name
    )];

    if !available_apps.is_empty() {
        lines.push(
            concat!(
                "- For the user request that explicitly selected this plugin, and only for that ",
                "request, if `tool_search` is available and an app from this plugin may help, ",
                "search for its tools before falling back to unrelated or built-in tools."
            )
            .to_string(),
        );
    }

    if plugin.has_skills {
        let skill_namespace = plugin
            .plugin_namespace
            .as_deref()
            .unwrap_or(plugin.display_name.as_str());
        lines.push(format!(
            "- Skills from this plugin are prefixed with `{skill_namespace}:`."
        ));
    }

    if !available_apps.is_empty() {
        lines.push(format!(
            "- Apps from this plugin available in this session: {}.",
            available_apps
                .iter()
                .map(|app| format!("`{app}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if !available_mcp_servers.is_empty() {
        lines.push(format!(
            "- MCP servers from this plugin available in this session: {}.",
            available_mcp_servers
                .iter()
                .map(|server| format!("`{server}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if lines.len() == 1 {
        return None;
    }

    lines.push("Use these plugin-associated capabilities to help solve the task.".to_string());

    Some(bound_explicit_plugin_instructions(lines.join("\n")))
}

fn bound_explicit_plugin_instructions(rendered: String) -> String {
    if rendered.len() <= MAX_EXPLICIT_PLUGIN_INSTRUCTIONS_BYTES {
        return rendered;
    }

    let max_prefix_bytes = MAX_EXPLICIT_PLUGIN_INSTRUCTIONS_BYTES
        .saturating_sub(TRUNCATED_PLUGIN_INSTRUCTIONS_SUFFIX.len());
    let prefix = take_bytes_at_char_boundary(&rendered, max_prefix_bytes);
    format!("{prefix}{TRUNCATED_PLUGIN_INSTRUCTIONS_SUFFIX}")
}

pub(crate) fn take_bytes_at_char_boundary(s: &str, maxb: usize) -> &str {
    if s.len() <= maxb {
        return s;
    }
    let mut last_ok = 0;
    for (i, ch) in s.char_indices() {
        let nb = i + ch.len_utf8();
        if nb > maxb {
            break;
        }
        last_ok = nb;
    }
    &s[..last_ok]
}
/// Shared usage guidance; individual plugins remain represented by their actual capabilities.
#[must_use]
pub fn render_plugins_section(plugins: &[PluginCapabilitySummary]) -> Option<String> {
    if plugins.is_empty() {
        return None;
    }
    let body = available_plugins_body();
    Some(format!(
        "<plugins_instructions>{body}</plugins_instructions>"
    ))
}
fn available_plugins_body() -> String {
    let mut lines = vec![
        "## Plugins".to_string(),
        "A plugin is a local bundle of skills, MCP servers, and apps.".to_string(),
    ];

    lines.push("### How to use plugins".to_string());
    lines.push(
            r"- Skill naming: If a plugin contributes skills, those skill entries are prefixed with `plugin_name:` in the Skills list.
- MCP naming: Plugin-provided MCP tools keep standard MCP identifiers such as `mcp__server__tool`; use tool provenance to tell which plugin they come from.
- Trigger rules: If the user explicitly names a plugin, prefer capabilities associated with that plugin for that turn.
- Relationship to capabilities: Plugins are not invoked directly. Use their underlying skills, MCP tools, and app tools to help solve the task.
- Relevance: Determine what a plugin can help with from explicit user mention or from the plugin-associated skills, MCP tools, and apps exposed elsewhere in this turn.
- Missing/blocked: If the user requests a plugin that does not have relevant callable capabilities for the task, say so briefly and continue with the best fallback."
                .to_string(),
        );

    format!("\n{}\n", lines.join("\n"))
}
