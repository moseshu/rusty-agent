// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The hook-facing projection of the registered Apps catalog. Connector identity comes from
//! tools/list metadata on the configured Apps server, never from a plugin's hook declaration.

use ra_mcp::catalog::McpToolInfo;

pub(super) fn app_tool_info(info: McpToolInfo) -> McpToolInfo {
    if info.server_name() != "codex_apps" {
        return info;
    }
    let mut tool = info.tool().clone();
    let meta_string = |key: &str| {
        tool.meta
            .as_ref()
            .and_then(|meta| meta.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let connector = meta_string("connector_id");
    let name = meta_string("connector_name").or_else(|| meta_string("connector_display_name"));
    if let (Some(name), Some(title)) = (name, tool.title.as_deref()) {
        let prefix = format!("{name}_");
        if let Some(stripped) = title
            .strip_prefix(&prefix)
            .filter(|title| !title.is_empty())
        {
            tool.title = Some(stripped.to_owned());
        }
    }
    McpToolInfo::new("codex_apps", tool).with_connector_id(connector)
}
