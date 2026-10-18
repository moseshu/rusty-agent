//! Display metadata collected from the authoritative normalized MCP history.

use std::collections::HashMap;

use ra_core::item::{RunItem, RunItemKind};
use serde_json::Value;

type DisplayMetadata = (Option<String>, Option<String>);
type MetadataMap = HashMap<(String, String), DisplayMetadata>;

pub(super) fn collect(existing_items: &[RunItem], output: &[RunItem]) -> MetadataMap {
    let mut metadata = HashMap::new();
    let generated = existing_items.iter().chain(output).filter_map(|item| {
        if let RunItemKind::McpListTools(list) = item.kind() {
            Some(list)
        } else {
            None
        }
    });
    for list in generated {
        if let Some(data) = list.provider_data() {
            collect_provider_list(data, &mut metadata);
            continue;
        }
        if list.server().is_empty() {
            continue;
        }
        for tool in list.tools() {
            if tool.name().is_empty() {
                continue;
            }
            let description = non_empty(tool.description()).map(str::to_owned);
            let title = non_empty(tool.title())
                .or_else(|| {
                    non_empty(
                        tool.annotations()
                            .and_then(|value| value.get("title"))
                            .and_then(serde_json::Value::as_str),
                    )
                })
                .map(str::to_owned);
            metadata.insert(
                (list.server().to_owned(), tool.name().to_owned()),
                (description, title),
            );
        }
    }
    metadata
}

fn collect_provider_list(data: &Value, metadata: &mut MetadataMap) {
    if data.get("type").and_then(Value::as_str) != Some("mcp_list_tools") {
        return;
    }
    let Some(server) = non_empty(data.get("server_label").and_then(Value::as_str)) else {
        return;
    };
    let Some(tools) = data.get("tools").and_then(Value::as_array) else {
        return;
    };
    for tool in tools {
        let Some(name) = non_empty(tool.get("name").and_then(Value::as_str)) else {
            continue;
        };
        let description = non_empty(tool.get("description").and_then(Value::as_str));
        let title = non_empty(tool.get("title").and_then(Value::as_str)).or_else(|| {
            non_empty(
                tool.get("annotations")
                    .and_then(|value| value.get("title"))
                    .and_then(Value::as_str),
            )
        });
        metadata.insert(
            (server.to_owned(), name.to_owned()),
            (description.map(str::to_owned), title.map(str::to_owned)),
        );
    }
}

pub(super) fn enrich(item: RunItem, metadata: &MetadataMap) -> RunItem {
    let RunItemKind::McpCall(call) = item.kind() else {
        return item;
    };
    let (description, title) = metadata
        .get(&(call.server().to_owned(), call.name().to_owned()))
        .cloned()
        .unwrap_or_default();
    item.with_tool_call_metadata(description, title)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}
