//! Hosted MCP item lifting and replay; execution remains owned by the Responses server.

use ra_core::{
    error::{Error, Result},
    item::{McpCall, McpListTools},
};
use serde_json::{Value, json};

use super::convert::required_str;
use crate::openai::error::behavior_error;

pub(crate) fn lift_call(item: &Value) -> Result<McpCall> {
    let mut value = normalized(item)?;
    value.remove("type");
    serde_json::from_value(Value::Object(value))
        .map_err(|_| behavior_error("invalid hosted MCP call"))
}

pub(crate) fn lift_list(item: &Value) -> McpListTools {
    McpListTools::new(
        item.get("server_label")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        Vec::new(),
    )
    .with_provider_data(item.clone())
}

fn normalized(item: &Value) -> Result<serde_json::Map<String, Value>> {
    let server = required_str(item, "server_label", "hosted MCP item")?;
    let mut fields = item
        .as_object()
        .cloned()
        .ok_or_else(|| behavior_error("hosted MCP item must be an object"))?;
    fields.remove("server_label");
    fields.insert("server".into(), Value::String(server.to_owned()));
    fields.insert("schema_version".into(), json!(1));
    Ok(fields)
}

pub(crate) fn lower_call(call: &McpCall) -> Result<Value> {
    lower(
        serde_json::to_value(call).map_err(|_| Error::caller("cannot serialize MCP call"))?,
        "mcp_call",
    )
}

pub(crate) fn lower_list(list: &McpListTools) -> Result<Value> {
    if let Some(data) = list.provider_data() {
        return Ok(data.clone());
    }
    let mut value =
        serde_json::to_value(list).map_err(|_| Error::caller("cannot serialize MCP tool list"))?;
    if value.get("id").and_then(Value::as_str).is_none() {
        return Err(Error::caller(
            "OpenAI Responses MCP tool-list replay requires its provider item id",
        ));
    }
    if let Some(tools) = value.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(fields) = tool.as_object_mut() {
                fields.remove("schema_version");
            }
        }
    }
    lower(value, "mcp_list_tools")
}

fn lower(mut value: Value, kind: &str) -> Result<Value> {
    let fields = value
        .as_object_mut()
        .ok_or_else(|| Error::caller("invalid normalized MCP item"))?;
    fields.remove("schema_version");
    let server = fields
        .remove("server")
        .ok_or_else(|| Error::caller("MCP item has no server label"))?;
    fields.insert("server_label".into(), server);
    fields.insert("type".into(), Value::String(kind.to_owned()));
    Ok(value)
}
