//! Lift an Anthropic Messages response into provider-neutral run items.

use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{
        CallId, ContentBlock, HandoffCall, ItemId, Message, MessageRole, ModelResponse,
        RawProviderItem, Reasoning, RunItem, RunItemKind, ToolCall,
    },
    model::{ModelHandoffDefinition, ProviderKey},
    usage::{RequestUsage, Usage},
};
use serde_json::Value;

use super::ASSISTANT_ROLE;

pub(crate) fn convert_response(
    payload: &Value,
    request_id: Option<String>,
    handoffs: &[ModelHandoffDefinition],
    provider: &ProviderKey,
) -> Result<ModelResponse> {
    if let Some(error) = payload.get("error").filter(|value| !value.is_null()) {
        return Err(behavior_error(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Anthropic response contained an error"),
        ));
    }
    let id = required_str(payload, "id", "response")?;
    if payload
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "message")
    {
        return Err(behavior_error("Anthropic response.type must be `message`"));
    }
    if payload
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role != ASSISTANT_ROLE)
    {
        return Err(behavior_error(
            "Anthropic response.role must be `assistant`",
        ));
    }
    reject_stop_reason(payload)?;
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| behavior_error("Anthropic response.content must be an array"))?;
    let mut output = Vec::new();
    let mut message_blocks = Vec::new();
    let mut start = 0usize;
    for (index, block) in content.iter().enumerate() {
        match required_str(block, "type", "content block")? {
            "text" => message_blocks.push(ContentBlock::text(required_str(
                block,
                "text",
                "text block",
            )?)),
            "refusal" => message_blocks.push(ContentBlock::refusal(required_str(
                block,
                "refusal",
                "refusal block",
            )?)),
            "thinking" | "redacted_thinking" => {
                flush_message(
                    &mut output,
                    &mut message_blocks,
                    id,
                    start,
                    provider,
                    payload,
                );
                start = index + 1;
                output.push(reasoning_item(block, id, index, provider)?);
            }
            "tool_use" => {
                flush_message(
                    &mut output,
                    &mut message_blocks,
                    id,
                    start,
                    provider,
                    payload,
                );
                start = index + 1;
                output.push(tool_item(block, id, index, handoffs, provider)?);
            }
            other => {
                return Err(behavior_error(format!(
                    "unsupported Anthropic response content block `{other}`"
                )));
            }
        }
    }
    flush_message(
        &mut output,
        &mut message_blocks,
        id,
        start,
        provider,
        payload,
    );
    let mut response = ModelResponse::new(output).with_usage(convert_usage(payload.get("usage")));
    if let Some(request_id) = request_id {
        response = response.with_request_id(request_id);
    }
    Ok(response)
}

fn flush_message(
    output: &mut Vec<RunItem>,
    blocks: &mut Vec<ContentBlock>,
    response_id: &str,
    index: usize,
    provider: &ProviderKey,
    raw: &Value,
) {
    if blocks.is_empty() {
        return;
    }
    let item = RunItem::new(
        ItemId::new(format!("{response_id}:{index}:message")),
        RunItemKind::Message(Message::new(MessageRole::Assistant, std::mem::take(blocks))),
    )
    .with_raw_provider_item(RawProviderItem::new(provider.as_str(), raw.clone()));
    output.push(item);
}

fn reasoning_item(
    block: &Value,
    response_id: &str,
    index: usize,
    provider: &ProviderKey,
) -> Result<RunItem> {
    let kind = required_str(block, "type", "thinking block")?;
    let reasoning = match kind {
        "thinking" => {
            let thinking = required_str(block, "thinking", "thinking block")?;
            let signature = required_str(block, "signature", "thinking block")?;
            Reasoning::new()
                .with_content(vec![thinking.to_owned()])
                .with_encrypted_content(signature)
                .with_provider_data(block.clone())
        }
        "redacted_thinking" => Reasoning::new().with_provider_data(block.clone()),
        _ => unreachable!("caller restricts content type"),
    };
    Ok(RunItem::new(
        ItemId::new(format!("{response_id}:{index}:reasoning")),
        RunItemKind::Reasoning(reasoning),
    )
    .with_raw_provider_item(RawProviderItem::new(provider.as_str(), block.clone())))
}

fn tool_item(
    block: &Value,
    response_id: &str,
    index: usize,
    handoffs: &[ModelHandoffDefinition],
    provider: &ProviderKey,
) -> Result<RunItem> {
    let call_id = CallId::new(required_str(block, "id", "tool_use block")?);
    let name = required_str(block, "name", "tool_use block")?;
    let input = block
        .get("input")
        .cloned()
        .ok_or_else(|| behavior_error("Anthropic tool_use block.input is required"))?;
    let kind = if let Some(handoff) = handoffs.iter().find(|handoff| handoff.name() == name) {
        RunItemKind::HandoffCall(
            HandoffCall::new(call_id, handoff.target_agent().clone(), input).with_tool_name(name),
        )
    } else {
        RunItemKind::ToolCall(ToolCall::new(call_id, name, input))
    };
    Ok(RunItem::new(
        ItemId::new(format!("{response_id}:{index}:{}", kind.label())),
        kind,
    )
    .with_raw_provider_item(RawProviderItem::new(provider.as_str(), block.clone())))
}

fn reject_stop_reason(payload: &Value) -> Result<()> {
    match payload.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn" | "tool_use" | "stop_sequence") | None => Ok(()),
        Some("max_tokens") => Err(Error::provider(
            ProviderErrorKind::ContextOverflow,
            "Anthropic response stopped at max_tokens",
        )),
        Some("refusal") => Err(Error::provider(
            ProviderErrorKind::Refusal,
            "Anthropic refused the request",
        )),
        Some(other) => Err(behavior_error(format!(
            "Anthropic response stopped with unsupported reason `{other}`"
        ))),
    }
}

fn convert_usage(value: Option<&Value>) -> Usage {
    let plain_input = value
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .and_then(|usage| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = value
        .and_then(|usage| usage.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Anthropic reports cache reads and writes outside input_tokens. The neutral ledger counts all
    // model input once, then marks the cached portions as details rather than extra spend.
    Usage::from_request(
        RequestUsage::new(
            plain_input
                .saturating_add(cached)
                .saturating_add(cache_write),
            output,
        )
        .with_cached_input_tokens(cached)
        .with_cache_write_tokens(cache_write),
    )
}

fn required_str<'a>(value: &'a Value, key: &str, owner: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| behavior_error(format!("Anthropic {owner}.{key} must be a string")))
}
pub(crate) fn behavior_error(message: impl Into<String>) -> Error {
    Error::provider(ProviderErrorKind::Behavior, message)
}

/// Whether an `invalid_request_error` is the context window turning the prompt away.
///
/// Anthropic states it in prose rather than in a distinct error type, so the message is the only
/// place the distinction exists — and it is a distinction worth keeping, because compacting first
/// is the one thing that makes the same turn sendable.
pub(crate) fn is_context_overflow(message: &str) -> bool {
    message.contains("prompt is too long") || message.contains("context window")
}

/// Classifies a streamed `error` frame from the vendor's error type alone.
///
/// A frame has no status line to fall back on, which is why the type is read here rather than
/// mapped from a status as the non-streaming path does.
pub(crate) fn stream_error_kind(code: Option<&str>, message: &str) -> ProviderErrorKind {
    match code {
        Some("invalid_request_error") if is_context_overflow(message) => {
            ProviderErrorKind::ContextOverflow
        }
        Some("overloaded_error" | "api_error") => ProviderErrorKind::ServerError,
        Some("rate_limit_error") => ProviderErrorKind::RateLimit,
        Some("timeout_error") => ProviderErrorKind::Timeout,
        // A spent balance is not an authentication failure, but it is the same verdict for the
        // caller: nothing about the request can be changed to get past it, and a person has to act.
        Some("authentication_error" | "permission_error" | "billing_error") => {
            ProviderErrorKind::Auth
        }
        Some("invalid_request_error" | "not_found_error" | "request_too_large") => {
            ProviderErrorKind::BadRequest
        }
        // An unrecognized type is a protocol the adapter does not know it is speaking.
        _ => ProviderErrorKind::Behavior,
    }
}
