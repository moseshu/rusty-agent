//! Lift a completed Responses payload into provider-neutral run items.

use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{
        CallId, ContentBlock, HandoffCall, ItemId, Message, MessageRole, ModelResponse,
        OutputPhase, RawProviderItem, Reasoning, RunItem, RunItemKind, ToolCall,
    },
    model::{ModelHandoffDefinition, ProviderKey},
    usage::Usage,
};
use serde_json::Value;

use crate::openai::error::behavior_error;

pub(crate) struct ConvertedResponse {
    pub(crate) response: ModelResponse,
    pub(crate) raw_response: Value,
    pub(crate) provider: ProviderKey,
}

pub(crate) fn convert_response(
    payload: Value,
    request_id: Option<String>,
    handoffs: &[ModelHandoffDefinition],
    provider: &ProviderKey,
) -> Result<ConvertedResponse> {
    if let Some(error) = payload.get("error").filter(|value| !value.is_null()) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("OpenAI response contained an error");
        return Err(behavior_error(message));
    }
    reject_unfinished_response(&payload)?;

    let response_id = required_str(&payload, "id", "response")?;
    let output = payload
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| behavior_error("OpenAI response.output must be an array"))?;
    let mut items = Vec::with_capacity(output.len());
    for (index, item) in output.iter().enumerate() {
        items.push(convert_output_item(
            item,
            response_id,
            index,
            handoffs,
            provider,
        )?);
    }

    let usage = convert_usage(payload.get("usage"));
    let mut response = ModelResponse::new(items)
        .with_response_id(response_id)
        .with_usage(usage);
    if let Some(request_id) = request_id {
        response = response.with_request_id(request_id);
    }
    Ok(ConvertedResponse {
        response,
        raw_response: payload,
        provider: provider.clone(),
    })
}

/// Rejects any terminal state that is not a finished response.
///
/// A truncated response carries a well-formed `output` array, so lifting it silently would hand
/// the runner a half-written message as if it were a final answer. `ModelResponse` has no field
/// for a partial result yet (R1-8), which leaves the classified error as the honest signal.
fn reject_unfinished_response(payload: &Value) -> Result<()> {
    let Some(status) = payload.get("status").and_then(Value::as_str) else {
        return Ok(());
    };
    let reason = || {
        payload
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .unwrap_or("unspecified")
    };
    match status {
        "completed" => Ok(()),
        "incomplete" => match reason() {
            "max_output_tokens" => Err(Error::provider(
                ProviderErrorKind::ContextOverflow,
                "OpenAI response stopped at max_output_tokens",
            )),
            "content_filter" => Err(Error::provider(
                ProviderErrorKind::Refusal,
                "OpenAI response stopped on a content filter",
            )),
            other => Err(behavior_error(format!(
                "OpenAI response is incomplete (`{other}`)"
            ))),
        },
        "cancelled" => Err(behavior_error("OpenAI response was cancelled")),
        // `background: true` returns before the output exists; R1-7 owns polling and streaming.
        other => Err(behavior_error(format!(
            "OpenAI response is not finished (status `{other}`)"
        ))),
    }
}

fn convert_output_item(
    item: &Value,
    response_id: &str,
    index: usize,
    handoffs: &[ModelHandoffDefinition],
    provider: &ProviderKey,
) -> Result<RunItem> {
    let item_type = required_str(item, "type", "response output item")?;
    let item_id = item.get("id").and_then(Value::as_str).map_or_else(
        || ItemId::new(format!("{response_id}:{index}:{item_type}")),
        ItemId::new,
    );
    let kind = match item_type {
        "message" => RunItemKind::Message(convert_message(item)?),
        "reasoning" => RunItemKind::Reasoning(convert_reasoning(item)),
        "function_call" => convert_function_call(item, handoffs)?,
        // Hosted-tool items (web search, file search, hosted MCP, image generation) have no
        // protocol-neutral payload yet. They can only appear when `extra_body.tools` requested
        // them, so the message names the item rather than silently dropping part of the turn.
        other => {
            return Err(behavior_error(format!(
                "OpenAI response output item `{other}` has no provider-neutral item type"
            )));
        }
    };
    Ok(RunItem::new(item_id, kind)
        .with_raw_provider_item(RawProviderItem::new(provider.as_str(), item.clone())))
}

fn convert_message(item: &Value) -> Result<Message> {
    let role = match required_str(item, "role", "response message")? {
        "assistant" => MessageRole::Assistant, // layering-allow: assistant = OpenAI wire message role
        other => {
            return Err(behavior_error(format!(
                "unexpected OpenAI response message role `{other}`"
            )));
        }
    };
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| behavior_error("OpenAI response message.content must be an array"))?
        .iter()
        .map(
            |content| match required_str(content, "type", "response message content")? {
                "output_text" => Ok(ContentBlock::text(required_str(
                    content,
                    "text",
                    "output_text",
                )?)),
                "refusal" => Ok(ContentBlock::refusal(required_str(
                    content, "refusal", "refusal",
                )?)),
                other => Err(behavior_error(format!(
                    "unsupported OpenAI message content `{other}`"
                ))),
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let mut message = Message::new(role, content);
    if let Some(phase) = item.get("phase").and_then(Value::as_str) {
        let phase = match phase {
            "commentary" => OutputPhase::Commentary,
            "final_answer" | "final" => OutputPhase::Final,
            other => {
                return Err(behavior_error(format!(
                    "unsupported OpenAI message phase `{other}`"
                )));
            }
        };
        message = message.with_phase(phase);
    }
    Ok(message)
}

fn convert_reasoning(item: &Value) -> Reasoning {
    let summary = text_segments(item.get("summary"));
    let content = text_segments(item.get("content"));
    let mut reasoning = Reasoning::new()
        .with_summary(summary)
        .with_content(content)
        .with_provider_data(item.clone());
    if let Some(id) = item.get("id").and_then(Value::as_str) {
        reasoning = reasoning.with_id(id);
    }
    if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) {
        reasoning = reasoning.with_encrypted_content(encrypted);
    }
    reasoning
}

fn text_segments(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn convert_function_call(item: &Value, handoffs: &[ModelHandoffDefinition]) -> Result<RunItemKind> {
    let call_id = CallId::new(required_str(item, "call_id", "function_call")?);
    let name = required_str(item, "name", "function_call")?;
    let arguments_text = required_str(item, "arguments", "function_call")?;
    let arguments: Value = serde_json::from_str(arguments_text).map_err(|error| {
        behavior_error(format!(
            "OpenAI function `{name}` returned invalid JSON arguments"
        ))
        .with_source(error)
    })?;

    if let Some(handoff) = handoffs.iter().find(|handoff| handoff.name() == name) {
        Ok(RunItemKind::HandoffCall(
            HandoffCall::new(call_id, handoff.target_agent().clone(), arguments)
                .with_tool_name(name),
        ))
    } else {
        Ok(RunItemKind::ToolCall(ToolCall::new(
            call_id, name, arguments,
        )))
    }
}

fn convert_usage(value: Option<&Value>) -> Usage {
    let input = value
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .and_then(|usage| usage.pointer("/input_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = value
        .and_then(|usage| usage.pointer("/output_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage::new(input, output)
        .with_cached_input_tokens(cached)
        .with_reasoning_tokens(reasoning)
}

fn required_str<'a>(value: &'a Value, key: &str, owner: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| behavior_error(format!("OpenAI {owner}.{key} must be a string")))
}
