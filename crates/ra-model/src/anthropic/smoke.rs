//! A deliberately narrow request preview used by the provider compatibility matrix.
//!
//! This is not an Anthropic model implementation. It has no transport, response conversion, or
//! streaming surface; those belong to the full adapter. Keeping the preview here lets the matrix
//! test compare all supported protocol payloads without exposing an evolving production codec.

use std::collections::BTreeSet;

use ra_core::{
    error::{Error, Result},
    item::{ContentBlock, MessageRole, ModelInputItem},
    model::{Effort, ModelRequest, ThinkingConfig, ToolChoice},
    tool::{ToolOutput, ToolOutputBlock},
};
use serde_json::{Map, Value, json};

const ANTHROPIC_ASSISTANT_ROLE: &str = "assistant"; // layering-allow: assistant = Anthropic wire message role

/// Smallest explicit thinking budget Anthropic accepts on the models that still take one.
const MIN_THINKING_BUDGET_TOKENS: u64 = 1024;

/// Lowers the matrix-supported subset of a request into an Anthropic Messages payload.
///
/// This hidden entry point exists solely for the repository's compatibility smoke test. It
/// rejects request material that needs the full Anthropic adapter rather than silently producing
/// an incomplete payload.
pub fn preview_request_payload(model: &str, request: &ModelRequest) -> Result<Value> {
    if model.trim().is_empty() {
        return Err(Error::config("Anthropic model name must not be empty"));
    }
    request.validate_cache_plan()?;
    if request.output_schema().is_some() {
        return Err(Error::caller(
            "Anthropic compatibility preview does not lower structured output schemas",
        ));
    }
    if !request.model_settings().extra_body().is_empty()
        || !request.model_settings().metadata().is_empty()
    {
        return Err(Error::caller(
            "Anthropic compatibility preview only accepts provider-neutral request fields",
        ));
    }

    let max_tokens = request.model_settings().max_tokens().ok_or_else(|| {
        Error::caller("Anthropic compatibility preview requires ModelSettings::max_tokens")
    })?;
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("max_tokens".to_owned(), json!(max_tokens));
    if let Some(instructions) = request.system_instructions() {
        body.insert(
            "system".to_owned(),
            json!([{"type": "text", "text": instructions}]),
        );
    }
    body.insert(
        "messages".to_owned(),
        Value::Array(lower_messages(request)?),
    );

    let tool_names = insert_tools(&mut body, request)?;
    insert_tool_choice(
        &mut body,
        request.model_settings().tool_choice(),
        &tool_names,
        request.model_settings().parallel_tool_calls(),
    )?;
    let effort = request.model_settings().effort();
    insert_thinking(
        &mut body,
        request.model_settings().thinking(),
        effort,
        max_tokens,
    )?;
    if let Some(effort) = effort {
        body.insert(
            "output_config".to_owned(),
            json!({"effort": effort.label()}),
        );
    }
    if let Some(temperature) = request.model_settings().temperature() {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = request.model_settings().top_p() {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    Ok(Value::Object(body))
}

/// Lowers the provider-neutral thinking setting and validates the couplings Anthropic enforces.
///
/// Three constraints live here rather than in `ra-core`, because all three are couplings between
/// Anthropic request fields that have no counterpart elsewhere. `OpenAI`'s effort setting has no
/// comparable relationship to `max_tokens`, and no floor of its own, so the same neutral settings
/// stay valid there.
///
/// - An explicit budget must leave room under the output ceiling: `max_tokens > budget_tokens`.
/// - An explicit budget has a floor of [`MIN_THINKING_BUDGET_TOKENS`].
/// - Thinking may only be turned off below the top two effort levels; the pair is refused there.
///
/// One limit is worth stating because this preview cannot enforce it: the explicit-budget shape is
/// only accepted by older Anthropic models, and current ones take `adaptive` alone and answer a
/// budget with a 400. Choosing between them needs a model-capability axis the framework does not
/// have yet, so the preview lowers the shape it is handed and leaves the choice to the caller.
fn insert_thinking(
    body: &mut Map<String, Value>,
    thinking: Option<ThinkingConfig>,
    effort: Option<Effort>,
    max_tokens: u64,
) -> Result<()> {
    let Some(thinking) = thinking else {
        return Ok(());
    };

    let value = match thinking {
        ThinkingConfig::Adaptive => json!({"type": "adaptive"}),
        ThinkingConfig::Enabled { budget_tokens } => {
            if budget_tokens < MIN_THINKING_BUDGET_TOKENS {
                return Err(Error::caller(format!(
                    "Anthropic thinking.budget_tokens ({budget_tokens}) must be at least {MIN_THINKING_BUDGET_TOKENS}"
                )));
            }
            if max_tokens <= budget_tokens {
                return Err(Error::caller(format!(
                    "Anthropic max_tokens ({max_tokens}) must be greater than thinking.budget_tokens ({budget_tokens})"
                )));
            }
            json!({"type": "enabled", "budget_tokens": budget_tokens})
        }
        ThinkingConfig::Disabled => {
            if let Some(effort @ (Effort::XHigh | Effort::Max)) = effort {
                return Err(Error::caller(format!(
                    "Anthropic rejects thinking.type=disabled at output_config.effort={effort}; \
                     lower the effort or keep thinking on"
                )));
            }
            json!({"type": "disabled"})
        }
        _ => {
            return Err(Error::caller(
                "Anthropic compatibility preview does not support this ThinkingConfig variant",
            ));
        }
    };
    body.insert("thinking".to_owned(), value);
    Ok(())
}

fn lower_messages(request: &ModelRequest) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    for item in request.input() {
        match item {
            ModelInputItem::Message(message) => match message.role() {
                MessageRole::User | MessageRole::Assistant => {
                    messages.push(LoweredMessage::message(
                        message.role().label(),
                        lower_message_content(message.content())?,
                    ));
                }
                MessageRole::System => {
                    return Err(Error::caller(
                        "Anthropic compatibility preview requires system instructions in the stable prefix",
                    ));
                }
                _ => {
                    return Err(Error::caller(
                        "unsupported message role for Anthropic compatibility preview",
                    ));
                }
            },
            ModelInputItem::ToolCall(call) => {
                let block = json!({
                    "type": "tool_use",
                    "id": call.call_id().as_str(),
                    "name": call.name(),
                    "input": call.arguments()
                });
                if let Some(message) = messages
                    .last_mut()
                    .filter(|message| message.role == ANTHROPIC_ASSISTANT_ROLE)
                {
                    message.content.push(block);
                } else {
                    messages.push(LoweredMessage::message(
                        ANTHROPIC_ASSISTANT_ROLE,
                        vec![block],
                    ));
                }
            }
            ModelInputItem::ToolCallOutput(output) => {
                let mut result = json!({
                    "type": "tool_result",
                    "tool_use_id": output.call_id().as_str(),
                    "content": lower_tool_result(output.output())?
                });
                if output.is_error() {
                    result["is_error"] = Value::Bool(true);
                }
                if let Some(message) = messages
                    .last_mut()
                    .filter(|message| message.role == "user" && message.is_tool_result_group)
                {
                    message.content.push(result);
                } else {
                    messages.push(LoweredMessage::tool_results(vec![result]));
                }
            }
            _ => {
                return Err(Error::caller(format!(
                    "Anthropic compatibility preview cannot lower `{}`",
                    item.label()
                )));
            }
        }
    }
    Ok(messages
        .into_iter()
        .map(LoweredMessage::into_value)
        .collect())
}

/// One Anthropic message while adjacent tool blocks are being assembled.
///
/// Tool use is a content block rather than a message of its own. Consecutive calls must therefore
/// remain in the same assistant turn, and their consecutive results must remain in one user turn.
struct LoweredMessage {
    role: &'static str,
    content: Vec<Value>,
    is_tool_result_group: bool,
}

impl LoweredMessage {
    fn message(role: &'static str, content: Vec<Value>) -> Self {
        Self {
            role,
            content,
            is_tool_result_group: false,
        }
    }

    fn tool_results(content: Vec<Value>) -> Self {
        Self {
            role: "user",
            content,
            is_tool_result_group: true,
        }
    }

    fn into_value(self) -> Value {
        json!({"role": self.role, "content": self.content})
    }
}

fn lower_message_content(blocks: &[ContentBlock]) -> Result<Vec<Value>> {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => Ok(json!({"type": "text", "text": text.text()})),
            _ => Err(Error::caller(format!(
                "Anthropic compatibility preview cannot lower `{}` message content",
                block.label()
            ))),
        })
        .collect()
}

fn insert_tools(body: &mut Map<String, Value>, request: &ModelRequest) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let tools = request
        .tools()
        .iter()
        .map(|tool| (tool.name(), tool.description(), tool.input_schema()))
        .chain(request.handoffs().iter().map(|handoff| {
            (
                handoff.name(),
                handoff.description(),
                handoff.input_schema(),
            )
        }))
        .map(|(name, description, schema)| {
            validate_tool_name(name)?;
            if !names.insert(name.to_owned()) {
                return Err(Error::caller(format!(
                    "duplicate Anthropic tool/handoff name `{name}`"
                )));
            }
            let mut tool = json!({"name": name, "input_schema": schema});
            if let Some(description) = description {
                tool["description"] = Value::String(description.to_owned());
            }
            Ok(tool)
        })
        .collect::<Result<Vec<_>>>()?;
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Ok(names)
}

fn insert_tool_choice(
    body: &mut Map<String, Value>,
    choice: Option<&ToolChoice>,
    tool_names: &BTreeSet<String>,
    parallel: Option<bool>,
) -> Result<()> {
    let choice = match choice {
        // Nothing to forbid without a tool table, and Anthropic does not accept a `tool_choice`
        // beside an absent `tools`.
        Some(ToolChoice::None) if tool_names.is_empty() => return Ok(()),
        Some(choice) => choice,
        None if parallel == Some(false) => &ToolChoice::Auto,
        None => return Ok(()),
    };
    let mut value = match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Tool(name) => {
            validate_tool_name(name)?;
            if !tool_names.contains(name) {
                return Err(Error::caller(format!(
                    "Anthropic tool_choice names unavailable function `{name}`"
                )));
            }
            json!({"type": "tool", "name": name})
        }
        ToolChoice::Mcp(_) => {
            return Err(Error::caller(
                "Anthropic compatibility preview cannot select a hosted MCP tool",
            ));
        }
        _ => {
            return Err(Error::caller(
                "unsupported tool choice for Anthropic compatibility preview",
            ));
        }
    };
    if parallel == Some(false) {
        value["disable_parallel_tool_use"] = Value::Bool(true);
    }
    body.insert("tool_choice".to_owned(), value);
    Ok(())
}

fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(Error::caller(format!(
            "Anthropic tool name `{name}` must be 1-64 characters of [A-Za-z0-9_-]"
        )));
    }
    Ok(())
}

/// Lowers stored tool output through the same model-visible projection as the `OpenAI` adapters.
fn lower_tool_result(payload: &Value) -> Result<Value> {
    let Some(output) = ToolOutput::from_stored(payload)? else {
        return stringify_tool_output(payload).map(Value::String);
    };
    let blocks = output
        .model_blocks()
        .into_iter()
        .map(|block| match block {
            ToolOutputBlock::Text { text } => Ok(json!({"type": "text", "text": text})),
            ToolOutputBlock::Image(_) | ToolOutputBlock::File(_) => Err(Error::caller(
                "Anthropic compatibility preview cannot lower non-text structured tool output",
            )),
            _ => Err(Error::caller(
                "unsupported structured tool-output block for Anthropic compatibility preview",
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(blocks))
}

/// Renders a legacy host-supplied tool result without adding quotes around an existing string.
fn stringify_tool_output(output: &Value) -> Result<String> {
    output.as_str().map_or_else(
        || {
            serde_json::to_string(output).map_err(|error| {
                Error::caller("could not serialize Anthropic tool result").with_source(error)
            })
        },
        |text| Ok(text.to_owned()),
    )
}
