//! Lower provider-neutral model input into a Responses create payload.

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ra_core::{
    error::{Error, Result},
    item::{
        ContentBlock, ImageSource, InputItemNormalizer, Message, MessageRole, ModelInputItem,
        OutputPhase,
    },
    model::{
        ConversationContinuation, Effort, ModelHandoffDefinition, ModelRequest,
        ModelToolDefinition, ThinkingConfig, ToolChoice,
    },
};
use serde_json::{Map, Value, json};

use crate::openai::error::behavior_error;

const ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";

pub(crate) async fn build_request_body(model: &str, request: &ModelRequest) -> Result<Value> {
    reject_unsupported_settings(request)?;

    let mut body = request
        .model_settings()
        .extra_body()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();

    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert(
        "input".to_owned(),
        Value::Array(lower_input(request).await?),
    );
    body.insert("include".to_owned(), required_includes(&body)?);

    if let Some(instructions) = request.system_instructions() {
        body.insert(
            "instructions".to_owned(),
            Value::String(instructions.to_owned()),
        );
    }
    match request.continuation() {
        ConversationContinuation::None => {}
        ConversationContinuation::PreviousResponseId(id) => {
            body.insert("previous_response_id".to_owned(), Value::String(id.clone()));
            body.remove("conversation");
        }
        ConversationContinuation::ConversationId(id) => {
            body.insert("conversation".to_owned(), Value::String(id.clone()));
            body.remove("previous_response_id");
        }
        _ => {
            return Err(Error::caller(
                "unsupported server continuation for OpenAI Responses",
            ));
        }
    }
    resolve_store(&mut body, request.continuation())?;

    insert_optional_number(
        &mut body,
        "temperature",
        request.model_settings().temperature(),
    );
    insert_optional_number(&mut body, "top_p", request.model_settings().top_p());
    if let Some(max_tokens) = request.model_settings().max_tokens() {
        body.insert("max_output_tokens".to_owned(), json!(max_tokens));
    }
    if !request.model_settings().metadata().is_empty() {
        body.insert(
            "metadata".to_owned(),
            serde_json::to_value(request.model_settings().metadata()).map_err(|error| {
                behavior_error("OpenAI metadata could not be serialized").with_source(error)
            })?,
        );
    }
    merge_effort(&mut body, request.model_settings().effort())?;
    if let Some(output_schema) = request.output_schema() {
        body.insert(
            "text".to_owned(),
            json!({
                "format": {
                    "type": "json_schema",
                    "name": output_schema.name(),
                    "schema": output_schema.schema(),
                    "strict": output_schema.strict()
                }
            }),
        );
    }

    let tools = merge_tools(&mut body, request.tools(), request.handoffs())?;
    if tools {
        if let Some(tool_choice) = request.model_settings().tool_choice() {
            body.insert(
                "tool_choice".to_owned(),
                lower_tool_choice(Some(tool_choice))?,
            );
        } else if !body.contains_key("tool_choice") {
            body.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
        }
        if let Some(parallel) = request.model_settings().parallel_tool_calls() {
            body.insert("parallel_tool_calls".to_owned(), Value::Bool(parallel));
        } else if !body.contains_key("parallel_tool_calls") {
            body.insert("parallel_tool_calls".to_owned(), Value::Bool(true));
        }
    } else if let Some(tool_choice) = request.model_settings().tool_choice() {
        if !matches!(tool_choice, ToolChoice::None | ToolChoice::Auto) {
            return Err(Error::caller(
                "OpenAI tool_choice requires at least one tool or handoff",
            ));
        }
        body.insert(
            "tool_choice".to_owned(),
            lower_tool_choice(Some(tool_choice))?,
        );
    }

    Ok(Value::Object(body))
}

/// Derives `store` from the requested continuation mode.
///
/// A response created with `store=false` is never retrievable, so chaining the next turn onto it
/// with `previous_response_id` cannot work. Callers that want server-side state must let the
/// server keep it; callers that want zero retention must replay history themselves. An explicit
/// `extra_body.store` still wins, except for the one combination that is guaranteed to fail.
fn resolve_store(
    body: &mut Map<String, Value>,
    continuation: &ConversationContinuation,
) -> Result<()> {
    let server_side = !matches!(continuation, ConversationContinuation::None);
    match body.get("store") {
        None | Some(Value::Null) => {
            body.insert("store".to_owned(), Value::Bool(server_side));
            Ok(())
        }
        Some(Value::Bool(false))
            if matches!(
                continuation,
                ConversationContinuation::PreviousResponseId(_)
            ) =>
        {
            Err(Error::caller(
                "OpenAI previous_response_id requires the previous response to be stored; \
                 `extra_body.store = false` makes the chain unresolvable",
            ))
        }
        Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(Error::caller("OpenAI extra_body.store must be a boolean")),
    }
}

/// Merges `effort` into any provider-private `reasoning` object instead of replacing it.
///
/// `reasoning.summary`, `reasoning.context` and `reasoning.mode` have no protocol-neutral
/// counterpart and can only arrive through `extra_body`; a whole-object overwrite would drop them
/// as a side effect of setting an unrelated field.
fn merge_effort(body: &mut Map<String, Value>, effort: Option<Effort>) -> Result<()> {
    let Some(effort) = effort else {
        return Ok(());
    };
    let mut reasoning = match body.remove("reasoning") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(existing)) => existing,
        Some(_) => {
            return Err(Error::caller(
                "OpenAI extra_body.reasoning must be an object",
            ));
        }
    };
    reasoning.insert(
        "effort".to_owned(),
        Value::String(effort.label().to_owned()),
    );
    body.insert("reasoning".to_owned(), Value::Object(reasoning));
    Ok(())
}

fn reject_unsupported_settings(request: &ModelRequest) -> Result<()> {
    if request.model_settings().frequency_penalty().is_some()
        || request.model_settings().presence_penalty().is_some()
    {
        return Err(Error::caller(
            "OpenAI Responses does not accept frequency_penalty or presence_penalty",
        ));
    }
    if let Some(thinking) = request.model_settings().thinking()
        && !matches!(thinking, ThinkingConfig::Disabled)
    {
        return Err(Error::caller(
            "OpenAI Responses uses effort rather than ThinkingConfig",
        ));
    }
    Ok(())
}

/// Unions the encrypted-reasoning include with any `extra_body.include` entries.
///
/// The include is added unconditionally. Gating it would need a model-capability axis, which does
/// not exist yet (see R1-3a / R1-13): deriving it from the request instead — "only ask for
/// encrypted reasoning once reasoning is visible" — silently breaks the first turn of every
/// reasoning model, because that turn is exactly where the replay material is first issued.
fn required_includes(body: &Map<String, Value>) -> Result<Value> {
    let mut includes = BTreeSet::new();
    if let Some(existing) = body.get("include") {
        let Some(values) = existing.as_array() else {
            return Err(Error::caller("OpenAI extra_body.include must be an array"));
        };
        for value in values {
            let Some(value) = value.as_str() else {
                return Err(Error::caller(
                    "OpenAI extra_body.include entries must be strings",
                ));
            };
            includes.insert(value.to_owned());
        }
    }
    includes.insert(ENCRYPTED_REASONING_INCLUDE.to_owned());
    Ok(Value::Array(
        includes.into_iter().map(Value::String).collect(),
    ))
}

fn insert_optional_number(body: &mut Map<String, Value>, name: &str, value: Option<f64>) {
    if let Some(value) = value.and_then(serde_json::Number::from_f64) {
        body.insert(name.to_owned(), Value::Number(value));
    }
}

async fn lower_input(request: &ModelRequest) -> Result<Vec<Value>> {
    let normalized = InputItemNormalizer::new()
        .normalize_model_items(request.input())
        .map_err(|error| {
            behavior_error("OpenAI model input could not be normalized").with_source(error)
        })?;
    let mut input = Vec::with_capacity(normalized.entries().len());
    for entry in normalized.entries() {
        input.push(lower_input_item(entry.item(), request).await?);
    }
    Ok(input)
}

async fn lower_input_item(item: &ModelInputItem, request: &ModelRequest) -> Result<Value> {
    match item {
        ModelInputItem::Message(message) => lower_message(message).await,
        ModelInputItem::Reasoning(reasoning) => Ok(lower_reasoning(reasoning)),
        ModelInputItem::ToolCall(call) => Ok(json!({
            "type": "function_call",
            "call_id": call.call_id().as_str(),
            "name": call.name(),
            "arguments": serde_json::to_string(call.arguments()).map_err(|error| {
                behavior_error("tool-call arguments could not be serialized").with_source(error)
            })?
        })),
        ModelInputItem::ToolCallOutput(output) => Ok(json!({
            "type": "function_call_output",
            "call_id": output.call_id().as_str(),
            "output": output_string(output.output())?
        })),
        ModelInputItem::HandoffCall(call) => {
            // The agent that received control does not advertise the handoff that led to it, so
            // the recorded tool name is the only reliable source once the run has moved on.
            let name = call
                .tool_name()
                .or_else(|| {
                    request
                        .handoffs()
                        .iter()
                        .find(|handoff| handoff.target_agent() == call.target_agent())
                        .map(ModelHandoffDefinition::name)
                })
                .ok_or_else(|| {
                    Error::caller(format!(
                        "handoff call to `{}` carries no tool name and its handoff is not \
                         advertised in this request",
                        call.target_agent()
                    ))
                })?;
            Ok(json!({
                "type": "function_call",
                "call_id": call.call_id().as_str(),
                "name": name,
                "arguments": serde_json::to_string(call.arguments()).map_err(|error| {
                    behavior_error("handoff arguments could not be serialized").with_source(error)
                })?
            }))
        }
        ModelInputItem::HandoffOutput(output) => Ok(json!({
            "type": "function_call_output",
            "call_id": output.call_id().as_str(),
            "output": output.note().unwrap_or("handoff completed")
        })),
        ModelInputItem::McpApprovalRequest(request) => Ok(json!({
            "type": "mcp_approval_request",
            "id": request.request_id(),
            "server_label": request.server(),
            "name": request.tool_name(),
            "arguments": serde_json::to_string(request.arguments()).map_err(|error| {
                behavior_error("MCP approval arguments could not be serialized").with_source(error)
            })?
        })),
        ModelInputItem::McpApprovalResponse(response) => {
            let mut value = json!({
                "type": "mcp_approval_response",
                "approval_request_id": response.request_id(),
                "approve": response.approved()
            });
            if let Some(reason) = response.reason() {
                value["reason"] = Value::String(reason.to_owned());
            }
            Ok(value)
        }
        // The summary is protocol-neutral by construction, so it survives as an ordinary user
        // turn. Its wording belongs to the compaction step that produced it (R5), not here.
        ModelInputItem::Compaction(compaction) => Ok(json!({
            "type": "message",
            "role": MessageRole::User.label(),
            "content": [{"type": "input_text", "text": compaction.summary()}]
        })),
        ModelInputItem::McpListTools(_) => Err(Error::caller(format!(
            "OpenAI Responses cannot lower `{}` without provider replay data",
            item.label()
        ))),
        _ => Err(Error::caller(
            "unsupported model-input item for OpenAI Responses",
        )),
    }
}

fn lower_reasoning(reasoning: &ra_core::item::Reasoning) -> Value {
    let mut value = Map::new();
    value.insert("type".to_owned(), Value::String("reasoning".to_owned()));
    if let Some(id) = reasoning.id() {
        value.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    value.insert(
        "summary".to_owned(),
        Value::Array(
            reasoning
                .summary()
                .iter()
                .map(|text| json!({"type": "summary_text", "text": text}))
                .collect(),
        ),
    );
    if !reasoning.content().is_empty() {
        value.insert(
            "content".to_owned(),
            Value::Array(
                reasoning
                    .content()
                    .iter()
                    .map(|text| json!({"type": "reasoning_text", "text": text}))
                    .collect(),
            ),
        );
    }
    if let Some(encrypted) = reasoning.encrypted_content() {
        value.insert(
            "encrypted_content".to_owned(),
            Value::String(encrypted.to_owned()),
        );
    }
    Value::Object(value)
}

async fn lower_message(message: &Message) -> Result<Value> {
    let mut content = Vec::with_capacity(message.content().len());
    for block in message.content() {
        content.push(lower_content(block, message.role()).await?);
    }
    let mut value = json!({
        "type": "message",
        "role": message.role().label(),
        "content": content
    });
    if let Some(phase) = message.phase() {
        let phase = match phase {
            OutputPhase::Commentary => "commentary",
            OutputPhase::Final => "final_answer",
            _ => {
                return Err(Error::caller(
                    "unsupported output phase for OpenAI Responses",
                ));
            }
        };
        value["phase"] = Value::String(phase.to_owned());
    }
    Ok(value)
}

async fn lower_content(block: &ContentBlock, role: MessageRole) -> Result<Value> {
    match block {
        ContentBlock::Text(text) => Ok(json!({
            "type": if matches!(role, MessageRole::Assistant) { "output_text" } else { "input_text" },
            "text": text.text()
        })),
        ContentBlock::Image(image) => {
            if matches!(role, MessageRole::Assistant) {
                return Err(Error::caller(
                    "OpenAI Responses assistant history cannot contain an image block",
                ));
            }
            let image_url = match image.source() {
                ImageSource::Base64(source) => {
                    format!("data:{};base64,{}", source.media_type(), source.data())
                }
                ImageSource::LocalPath(source) => {
                    let bytes = tokio::fs::read(source.path()).await.map_err(|error| {
                        Error::caller(format!(
                            "could not read local image `{}`",
                            source.path().display()
                        ))
                        .with_source(error)
                    })?;
                    format!(
                        "data:{};base64,{}",
                        infer_media_type(source.path()),
                        STANDARD.encode(bytes)
                    )
                }
                _ => {
                    return Err(Error::caller(
                        "unsupported image source for OpenAI Responses",
                    ));
                }
            };
            Ok(json!({"type": "input_image", "image_url": image_url}))
        }
        ContentBlock::Refusal(refusal) if matches!(role, MessageRole::Assistant) => {
            Ok(json!({"type": "refusal", "refusal": refusal.refusal()}))
        }
        ContentBlock::Refusal(_) => Err(Error::caller(
            "OpenAI Responses accepts refusal blocks only in assistant history",
        )),
        ContentBlock::Thinking(_) => Err(Error::caller(
            "Anthropic thinking blocks cannot be sent through OpenAI Responses",
        )),
        _ => Err(Error::caller(
            "unsupported content block for OpenAI Responses",
        )),
    }
}

fn infer_media_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

fn output_string(output: &Value) -> Result<String> {
    output.as_str().map_or_else(
        || {
            serde_json::to_string(output).map_err(|error| {
                behavior_error("tool output could not be serialized").with_source(error)
            })
        },
        |text| Ok(text.to_owned()),
    )
}

/// Appends the protocol-neutral tools and handoffs to any hosted tools from `extra_body`.
///
/// Hosted tools (web search, file search, hosted MCP) have no protocol-neutral representation and
/// can only be requested through `extra_body`. Replacing the array would make the two mutually
/// exclusive; the names are checked across both sources so a collision fails locally instead of
/// producing an ambiguous call. Returns whether the merged array is non-empty.
fn merge_tools(
    body: &mut Map<String, Value>,
    tools: &[ModelToolDefinition],
    handoffs: &[ModelHandoffDefinition],
) -> Result<bool> {
    let mut lowered = match body.remove("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(existing)) => existing,
        Some(_) => return Err(Error::caller("OpenAI extra_body.tools must be an array")),
    };
    let mut names = lowered
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    lowered.reserve(tools.len() + handoffs.len());

    let neutral = tools
        .iter()
        .map(|tool| {
            (
                tool.name(),
                tool.description(),
                tool.input_schema(),
                tool.strict(),
            )
        })
        .chain(handoffs.iter().map(|handoff| {
            (
                handoff.name(),
                handoff.description(),
                handoff.input_schema(),
                handoff.strict(),
            )
        }));
    for (name, description, schema, strict) in neutral {
        if !names.insert(name.to_owned()) {
            return Err(Error::caller(format!(
                "duplicate OpenAI tool/handoff name `{name}`"
            )));
        }
        lowered.push(function_tool(name, description, schema, strict));
    }

    let populated = !lowered.is_empty();
    if populated {
        body.insert("tools".to_owned(), Value::Array(lowered));
    }
    Ok(populated)
}

fn function_tool(name: &str, description: Option<&str>, schema: &Value, strict: bool) -> Value {
    let mut tool = json!({
        "type": "function",
        "name": name,
        "parameters": schema,
        "strict": strict
    });
    if let Some(description) = description {
        tool["description"] = Value::String(description.to_owned());
    }
    tool
}

fn lower_tool_choice(choice: Option<&ToolChoice>) -> Result<Value> {
    match choice.unwrap_or(&ToolChoice::Auto) {
        ToolChoice::Auto => Ok(Value::String("auto".to_owned())),
        ToolChoice::Required => Ok(Value::String("required".to_owned())),
        ToolChoice::None => Ok(Value::String("none".to_owned())),
        ToolChoice::Tool(name) => Ok(json!({"type": "function", "name": name})),
        ToolChoice::Mcp(_) => Err(Error::caller(
            "OpenAI Responses MCP tool choice requires a hosted MCP tool definition",
        )),
        _ => Err(Error::caller(
            "unsupported tool choice for OpenAI Responses",
        )),
    }
}
