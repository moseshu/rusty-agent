//! Conversation items in both directions: run records lowered to what the Conversations API
//! stores, and stored items lifted back into run records.

use ra_core::{
    error::{Error, Result},
    item::{
        Base64FileSource, CallId, ContentBlock, FileBlock, FileSource, ImageBlock, ImageDetail,
        ImageSource, InputItemDigest, ItemId, McpApprovalRequest, McpApprovalResponse, Message,
        MessageRole, OutputPhase, RawProviderItem, Reasoning, RunItem, RunItemKind, ToolCallKind,
        ToolCallOutput,
    },
    model::ProviderKey,
    tool::{ToolOutput, ToolOutputBlock},
};
use serde_json::Value;

use crate::openai::{
    chat::FAKE_ITEM_ID,
    error::behavior_error,
    responses::{
        convert::{
            convert_compaction, convert_custom_tool_call, convert_function_call, required_str,
            text_segments,
        },
        request::lower_input_item,
    },
};

/// Item types whose top-level `id` the Conversations create-item schema requires.
///
/// The reference's `_OPENAI_CONVERSATION_ITEM_TYPES_WITH_REQUIRED_ID`. Most of these are hosted
/// tool items this crate never lowers; the list is kept whole so the rule reads the same.
const ITEM_TYPES_WITH_REQUIRED_ID: [&str; 13] = [
    "file_search_call",
    "web_search_call",
    "computer_call",
    "code_interpreter_call",
    "image_generation_call",
    "local_shell_call",
    "local_shell_call_output",
    "mcp_list_tools",
    "mcp_approval_request",
    "mcp_call",
    "item_reference",
    "program",
    "program_output",
];

/// Filters before a pending batch is captured, while the cursor still counts all run records.
pub(super) fn persistable_items(items: Vec<RunItem>) -> Vec<RunItem> {
    items
        .into_iter()
        .filter(|item| match item.kind() {
            RunItemKind::Reasoning(reasoning) => {
                reasoning
                    .id()
                    .is_some_and(|id| !id.is_empty() && id != FAKE_ITEM_ID)
                    || reasoning
                        .encrypted_content()
                        .is_some_and(|content| !content.is_empty())
            }
            _ => true,
        })
        .collect()
}

/// The reference fingerprints sanitized wire items, with server-assigned ids removed.
///
/// Lowering both pending and retrieved records makes image sources, projected tool metadata,
/// handoffs and neutral compaction compare as the content the conversation actually stores.
pub(super) async fn persistence_digest(item: &RunItem) -> Result<InputItemDigest> {
    let mut lowered = lower_for_conversation(std::slice::from_ref(item)).await?;
    let mut value = lowered.pop().ok_or_else(|| {
        Error::caller("an unpersistable item cannot be fingerprinted as conversation history")
    })?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("id");
    }
    InputItemDigest::compute_serialized(&value).map_err(|error| {
        Error::caller("Session item could not be fingerprinted").with_source(error)
    })
}

/// Lowers the records of one append to the items a conversation is given.
///
/// Each record is lowered as a Responses input item, then cleaned as the reference cleans an item
/// before it saves it to a conversation: the provider's item id is removed unless the item type
/// requires it or the item is reasoning, a placeholder id is removed from every type, and
/// reasoning that has neither an id nor encrypted content is dropped, since the conversation
/// cannot store it.
///
/// A record with no model input — an approval awaiting the host — is refused: a conversation
/// stores only what a model is sent.
pub(super) async fn lower_for_conversation(items: &[RunItem]) -> Result<Vec<Value>> {
    let mut lowered = Vec::with_capacity(items.len());
    for item in items {
        let input = item.to_model_input().ok_or_else(|| {
            Error::caller(format!(
                "an OpenAI conversation stores only model input; `{}` record `{}` has none",
                item.kind().label(),
                item.id()
            ))
        })?;
        let value = sanitize(lower_input_item(&input, &[]).await?);
        if !is_unpersistable(&value) {
            lowered.push(value);
        }
    }
    Ok(lowered)
}

/// The reference's `_sanitize_openai_conversation_item`.
fn sanitize(mut value: Value) -> Value {
    if let Some(fields) = value.as_object_mut() {
        let item_type = fields.get("type").and_then(Value::as_str);
        let keeps_id = item_type == Some("reasoning")
            || item_type.is_some_and(|item_type| ITEM_TYPES_WITH_REQUIRED_ID.contains(&item_type));
        let placeholder = fields.get("id").and_then(Value::as_str) == Some(FAKE_ITEM_ID);
        if placeholder || !keeps_id {
            fields.remove("id");
        }
        fields.remove("provider_data");
    }
    value
}

/// The reference's `_is_unpersistable_for_openai_conversation`.
fn is_unpersistable(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("reasoning") {
        return false;
    }
    let present = |key: &str| {
        value
            .get(key)
            .is_some_and(|field| !field.is_null() && field.as_str() != Some(""))
    };
    !present("id") && !present("encrypted_content")
}

/// Lifts one stored conversation item into a run record.
///
/// The record's identity is the item's conversation id, and the stored item itself is kept as
/// the record's raw provider copy. A kind with no provider-neutral counterpart — a hosted tool's
/// call, for one — is an error rather than a silent gap in the history.
pub(crate) fn lift_conversation_item(item: &Value, provider: &ProviderKey) -> Result<RunItem> {
    let id = required_str(item, "id", "conversation item")?;
    lift_history_item(item, provider, ItemId::new(id))
}

/// Lifts a Responses history item with a separate local identity when the wire omits one.
pub(crate) fn lift_history_item(
    item: &Value,
    provider: &ProviderKey,
    id: ItemId,
) -> Result<RunItem> {
    let item_type = required_str(item, "type", "conversation item")?;
    let kind = match item_type {
        "message" => RunItemKind::Message(lift_message(item)?),
        "reasoning" => RunItemKind::Reasoning(lift_reasoning(item)),
        "function_call" => convert_function_call(item, &[])?,
        "custom_tool_call" => convert_custom_tool_call(item)?,
        "function_call_output" => {
            RunItemKind::ToolCallOutput(lift_tool_output(item, ToolCallKind::Function)?)
        }
        "custom_tool_call_output" => {
            RunItemKind::ToolCallOutput(lift_tool_output(item, ToolCallKind::Custom)?)
        }
        "mcp_approval_request" => RunItemKind::McpApprovalRequest(lift_mcp_request(item)?),
        "mcp_approval_response" => RunItemKind::McpApprovalResponse(lift_mcp_response(item)?),
        "compaction" => RunItemKind::ProviderCompaction(convert_compaction(item, provider)),
        other => {
            return Err(behavior_error(format!(
                "OpenAI conversation item `{other}` has no provider-neutral item type"
            )));
        }
    };
    Ok(RunItem::new(id, kind)
        .with_raw_provider_item(RawProviderItem::new(provider.as_str(), item.clone())))
}

fn lift_message(item: &Value) -> Result<Message> {
    let role = match required_str(item, "role", "conversation message")? {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant, // layering-allow: assistant = OpenAI wire message role
        // The neutral model has one instruction role; `developer` is the newer name for it.
        "system" | "developer" => MessageRole::System,
        other => {
            return Err(behavior_error(format!(
                "unexpected OpenAI conversation message role `{other}`"
            )));
        }
    };
    let content = match item.get("content") {
        Some(Value::String(text)) => vec![ContentBlock::text(text.clone())],
        Some(Value::Array(parts)) => parts.iter().map(lift_content).collect::<Result<_>>()?,
        _ => {
            return Err(behavior_error(
                "OpenAI conversation message.content must be a string or an array",
            ));
        }
    };
    let mut message = Message::new(role, content);
    if let Some(phase) = item.get("phase").and_then(Value::as_str) {
        message = message.with_phase(match phase {
            "commentary" => OutputPhase::Commentary,
            "final_answer" | "final" => OutputPhase::Final,
            other => {
                return Err(behavior_error(format!(
                    "unsupported OpenAI message phase `{other}`"
                )));
            }
        });
    }
    Ok(message)
}

fn lift_content(part: &Value) -> Result<ContentBlock> {
    match required_str(part, "type", "conversation message content")? {
        "input_text" | "output_text" | "text" => Ok(ContentBlock::text(required_str(
            part,
            "text",
            "conversation text",
        )?)),
        "refusal" => Ok(ContentBlock::refusal(required_str(
            part, "refusal", "refusal",
        )?)),
        "input_image" => Ok(ContentBlock::Image(lift_image(part)?)),
        "input_file" => Ok(ContentBlock::File(lift_file(part)?)),
        other => Err(behavior_error(format!(
            "unsupported OpenAI conversation message content `{other}`"
        ))),
    }
}

/// Lifts an `input_image` part. An inline `data:` URL stays a URL: it lowers back unchanged.
fn lift_image(part: &Value) -> Result<ImageBlock> {
    let source = if let Some(url) = part.get("image_url").and_then(Value::as_str) {
        ImageSource::url(url)
    } else if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
        ImageSource::provider_file(file_id)
    } else {
        return Err(behavior_error(
            "OpenAI conversation input_image carries neither image_url nor file_id",
        ));
    };
    let mut image = ImageBlock::new(source);
    if let Some(detail) = part.get("detail").and_then(Value::as_str) {
        image = image.with_detail(match detail {
            "low" => ImageDetail::Low,
            "high" => ImageDetail::High,
            "auto" => ImageDetail::Auto,
            other => {
                return Err(behavior_error(format!(
                    "unsupported OpenAI image detail `{other}`"
                )));
            }
        });
    }
    Ok(image)
}

/// Lifts reasoning without provider replay data: no model is known to have produced it.
fn lift_reasoning(item: &Value) -> Reasoning {
    let mut reasoning = Reasoning::new()
        .with_summary(text_segments(item.get("summary")))
        .with_content(text_segments(item.get("content")));
    if let Some(id) = item.get("id").and_then(Value::as_str) {
        reasoning = reasoning.with_id(id);
    }
    if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) {
        reasoning = reasoning.with_encrypted_content(encrypted);
    }
    reasoning
}

/// Lifts a tool output: text stays text, and content parts become a structured tool result so
/// they lower back as the same parts.
fn lift_tool_output(item: &Value, kind: ToolCallKind) -> Result<ToolCallOutput> {
    let call_id = CallId::new(required_str(item, "call_id", "conversation tool output")?);
    let output = match item.get("output") {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => {
            let blocks = parts
                .iter()
                .map(lift_output_block)
                .collect::<Result<Vec<_>>>()?;
            let output = ToolOutput::new(blocks)?;
            serde_json::to_value(output).map_err(|error| {
                behavior_error("OpenAI conversation tool output could not be stored")
                    .with_source(error)
            })?
        }
        _ => {
            return Err(behavior_error(
                "OpenAI conversation tool output must be a string or an array",
            ));
        }
    };
    Ok(ToolCallOutput::new(call_id, output).with_kind(kind))
}

fn lift_output_block(part: &Value) -> Result<ToolOutputBlock> {
    match required_str(part, "type", "conversation tool output content")? {
        "input_text" | "output_text" | "text" => Ok(ToolOutputBlock::text(required_str(
            part,
            "text",
            "conversation text",
        )?)),
        "input_image" => Ok(ToolOutputBlock::Image(lift_image(part)?)),
        "input_file" => Ok(ToolOutputBlock::File(lift_file(part)?)),
        other => Err(behavior_error(format!(
            "unsupported OpenAI conversation tool output content `{other}`"
        ))),
    }
}

/// Reconstructs every file source the Responses tool-output adapter lowers.
fn lift_file(part: &Value) -> Result<FileBlock> {
    let source = if let Some(data) = part.get("file_data").and_then(Value::as_str) {
        let mut source = Base64FileSource::new(data);
        if let Some(filename) = part.get("filename").and_then(Value::as_str) {
            source = source.with_filename(filename);
        }
        FileSource::Base64(source)
    } else if let Some(url) = part.get("file_url").and_then(Value::as_str) {
        FileSource::url(url)
    } else if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
        FileSource::provider_file(file_id)
    } else {
        return Err(behavior_error(
            "OpenAI conversation input_file carries no file_data, file_url or file_id",
        ));
    };
    let file = FileBlock::new(source);
    // Replay metadata belongs to this adapter. Carry it in the record's extension envelope,
    // rather than adding Responses-only detail or filename fields to the neutral file source.
    let metadata: serde_json::Map<String, Value> = ["filename", "detail"]
        .into_iter()
        .filter_map(|key| {
            if key == "filename" && matches!(file.source(), FileSource::Base64(_)) {
                return None;
            }
            part.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| (key.to_owned(), Value::String(value.to_owned())))
        })
        .collect();
    if metadata.is_empty() {
        return Ok(file);
    }
    let mut encoded = serde_json::to_value(file).map_err(|error| {
        behavior_error("File replay metadata could not be serialized").with_source(error)
    })?;
    encoded["openai"] = Value::Object(metadata);
    serde_json::from_value(encoded).map_err(|error| {
        behavior_error("File replay metadata could not be stored").with_source(error)
    })
}

fn lift_mcp_request(item: &Value) -> Result<McpApprovalRequest> {
    let arguments = parse_arguments(item, "mcp_approval_request")?;
    Ok(McpApprovalRequest::new(
        required_str(item, "id", "mcp_approval_request")?,
        required_str(item, "server_label", "mcp_approval_request")?,
        required_str(item, "name", "mcp_approval_request")?,
        arguments,
    ))
}

fn lift_mcp_response(item: &Value) -> Result<McpApprovalResponse> {
    let approved = item
        .get("approve")
        .and_then(Value::as_bool)
        .ok_or_else(|| behavior_error("OpenAI mcp_approval_response.approve must be a boolean"))?;
    let mut response = McpApprovalResponse::new(
        required_str(item, "approval_request_id", "mcp_approval_response")?,
        approved,
    );
    if let Some(reason) = item.get("reason").and_then(Value::as_str) {
        response = response.with_reason(reason);
    }
    Ok(response)
}

fn parse_arguments(item: &Value, owner: &str) -> Result<Value> {
    let text = required_str(item, "arguments", owner)?;
    serde_json::from_str(text).map_err(|error| {
        behavior_error(format!("OpenAI {owner} carries invalid JSON arguments")).with_source(error)
    })
}

/// The conversation id of a stored item, which removing it requires.
pub(super) fn stored_item_id(item: &Value) -> Result<&str> {
    required_str(item, "id", "conversation item")
}
