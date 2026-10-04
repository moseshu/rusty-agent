//! Typed history lowering, model-visibility fingerprints, and replay-safe compact output.

use ra_core::{
    error::{Error, Result},
    item::{
        InputItemDigest, ItemId, MessageRole, ModelInputItem, RawProviderItem, RunItem, RunItemKind,
    },
    model::{ModelRequest, ProviderKey},
};
use serde_json::Value;

use crate::openai::{
    conversations::items::lift_history_item,
    error::behavior_error,
    responses::request::{lower_input, lower_input_item},
};

pub(super) async fn lower_items(items: &[RunItem], provider: &ProviderKey) -> Result<Vec<Value>> {
    let mut wire = Vec::with_capacity(items.len());
    for item in items {
        if let Some(input) = item.to_model_input() {
            wire.push(lower_item(&input, provider).await?);
        }
    }
    Ok(wire)
}

/// Normalizes the hook's history without requiring every retained item to be sendable.
pub(super) async fn decision_items(
    items: &[RunItem],
    provider: &ProviderKey,
) -> Result<Vec<Value>> {
    let mut normalized = Vec::with_capacity(items.len());
    for input in items.iter().filter_map(RunItem::to_model_input) {
        let mut value = match lower_item(&input, provider).await {
            Ok(value) => value,
            Err(_) => {
                // Python retains arbitrary input dictionaries during candidate normalization.
                // Preserve unsupported typed records for the hook too; only lower_items may
                // produce API input after automatic visibility and suffix selection.
                if let ModelInputItem::ProviderCompaction(compaction) = &input {
                    compaction.payload().clone()
                } else {
                    let serialized = serde_json::to_value(&input).map_err(|error| {
                        Error::caller("Compaction decision item could not be serialized")
                            .with_source(error)
                    })?;
                    let mut fields = serialized
                        .get("data")
                        .and_then(Value::as_object)
                        .cloned()
                        .ok_or_else(|| {
                            Error::caller("Compaction decision item must be an object")
                        })?;
                    fields.remove("schema_version");
                    fields.insert("type".into(), Value::String(input.label().into()));
                    Value::Object(fields)
                }
            }
        };
        if let Some(fields) = value.as_object_mut() {
            fields.remove("created_by");
        }
        normalized.push(value);
    }
    Ok(normalized)
}

async fn lower_item(item: &ModelInputItem, provider: &ProviderKey) -> Result<Value> {
    validate_provider(item, provider)?;
    let mut value = lower_input_item(item, &[]).await?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("created_by");
    }
    Ok(value)
}

fn validate_provider(item: &ModelInputItem, provider: &ProviderKey) -> Result<()> {
    if let ModelInputItem::ProviderCompaction(compaction) = item
        && compaction.provider() != provider.as_str()
    {
        return Err(Error::caller(
            "Provider compaction belongs to a different provider",
        ));
    }
    Ok(())
}

pub(super) async fn digest_item(
    item: &ModelInputItem,
    ignore_ids: bool,
    provider: &ProviderKey,
) -> Result<InputItemDigest> {
    digest_wire(lower_item(item, provider).await?, ignore_ids)
}

pub(super) async fn digest_request(
    request: &ModelRequest,
    ignore_ids: bool,
    provider: &ProviderKey,
) -> Result<Vec<InputItemDigest>> {
    for item in request.input() {
        validate_provider(item, provider)?;
    }
    lower_input(request)
        .await?
        .into_iter()
        .map(|item| digest_wire(item, ignore_ids))
        .collect()
}

fn digest_wire(mut value: Value, ignore_ids: bool) -> Result<InputItemDigest> {
    if let Some(fields) = value.as_object_mut() {
        fields.remove("created_by");
    }
    if ignore_ids && let Some(fields) = value.as_object_mut() {
        fields.remove("id");
    }
    InputItemDigest::compute_serialized(&value).map_err(|error| {
        Error::caller("Compaction item could not be fingerprinted").with_source(error)
    })
}

/// Selects all items except user messages and provider compaction items.
#[must_use]
pub fn select_compaction_candidate_items(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter(|item| {
            item.get("role").and_then(Value::as_str) != Some("user")
                && item.get("type").and_then(Value::as_str) != Some("compaction")
        })
        .cloned()
        .collect()
}

pub(super) fn user_message(item: &RunItem) -> bool {
    matches!(item.kind(), RunItemKind::Message(message) if message.role() == MessageRole::User)
        || matches!(item.kind(), RunItemKind::Compaction(_))
}

/// A partial replacement must not remove call/output or reasoning groups absent from input.
pub(super) fn suffix_is_intact(items: &[RunItem]) -> bool {
    let input: Vec<_> = items.iter().filter_map(RunItem::to_model_input).collect();
    let calls: std::collections::BTreeSet<_> = input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCall(call) => Some(call.call_id()),
            ModelInputItem::HandoffCall(call) => Some(call.call_id()),
            _ => None,
        })
        .collect();
    let outputs: std::collections::BTreeSet<_> = input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) => Some(output.call_id()),
            ModelInputItem::HandoffOutput(output) => Some(output.call_id()),
            _ => None,
        })
        .collect();
    calls == outputs
}

pub(super) fn lift_output(payload: &Value, provider: &ProviderKey) -> Result<Vec<RunItem>> {
    let output = match payload.get("output") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(behavior_error("OpenAI compaction.output must be an array")),
    };
    let has_reasoning = output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    let batch_id = uuid::Uuid::new_v4();
    output
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mut item = item.clone();
            let fields = item
                .as_object_mut()
                .ok_or_else(|| behavior_error("OpenAI compaction output item must be an object"))?;
            fields.remove("created_by");
            let assistant_role = "assistant"; // layering-allow: assistant = OpenAI wire message role
            if !has_reasoning && fields.get("role").and_then(Value::as_str) == Some(assistant_role)
            {
                fields.remove("id");
            }
            normalize_user_content(&mut item)?;
            let id = item.get("id").and_then(Value::as_str).map_or_else(
                || ItemId::new(format!("compact-{batch_id}:{index}")),
                ItemId::new,
            );
            let record = lift_history_item(&item, provider, id)?;
            Ok(record.with_raw_provider_item(RawProviderItem::new(provider.as_str(), item)))
        })
        .collect()
}

fn normalize_user_content(item: &mut Value) -> Result<()> {
    if item.get("type").and_then(Value::as_str) != Some("message")
        || item.get("role").and_then(Value::as_str) != Some("user")
    {
        return Ok(());
    }
    let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for part in parts {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
        let (sources, extras): (&[&str], &[&str]) = match kind {
            "input_image" => (&["image_url", "file_id"], &["detail"]),
            "input_file" => (
                &["file_data", "file_url", "file_id"],
                &["filename", "detail"],
            ),
            _ => continue,
        };
        let source = sources
            .iter()
            .find(|key| {
                part.get(**key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
            .ok_or_else(|| {
                behavior_error(format!("Compaction {kind} item missing a replay source"))
            })?;
        let mut normalized = serde_json::Map::new();
        normalized.insert("type".into(), Value::String(kind.into()));
        normalized.insert((*source).to_string(), part[*source].clone());
        for key in extras {
            if let Some(value) = part
                .get(*key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                normalized.insert((*key).to_string(), Value::String(value.into()));
            }
        }
        *part = Value::Object(normalized);
    }
    Ok(())
}
