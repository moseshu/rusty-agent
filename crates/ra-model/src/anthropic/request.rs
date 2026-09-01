//! Lower provider-neutral input into an Anthropic Messages request.

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ra_core::{
    error::{Error, Result},
    item::{
        ContentBlock, HandoffCall, ImageBlock, ImageSource, InputItemNormalizer, Message,
        MessageRole, ModelInputItem, ThinkingBlock,
    },
    model::{
        ConversationContinuation, Effort, ModelHandoffDefinition, ModelRequest,
        ModelToolDefinition, ThinkingConfig, ToolChoice,
    },
    prompt::{MIN_CACHEABLE_PREFIX_TOKENS, estimate_tokens},
    tool::{ToolOutput, ToolOutputBlock},
};
use serde_json::{Map, Value, json};

use super::ASSISTANT_ROLE;

const MIN_THINKING_BUDGET_TOKENS: u64 = 1024;

pub(crate) async fn build_request_body(
    model: &str,
    request: &ModelRequest,
    streaming: bool,
) -> Result<Value> {
    reject_unsupported(request)?;
    request.validate_cache_plan()?;
    let max_tokens = request
        .model_settings()
        .max_tokens()
        .ok_or_else(|| Error::caller("Anthropic Messages requires ModelSettings::max_tokens"))?;
    let mut body = request
        .model_settings()
        .extra_body()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("max_tokens".to_owned(), json!(max_tokens));
    body.insert(
        "messages".to_owned(),
        Value::Array(lower_messages(request).await?),
    );
    let tools = merge_tools(&mut body, request.tools(), request.handoffs())?;
    insert_system(&mut body, request, &tools)?;
    apply_tool_choice(
        &mut body,
        request.model_settings().tool_choice(),
        request.model_settings().parallel_tool_calls(),
        &tools,
    )?;
    insert_thinking(
        &mut body,
        request.model_settings().thinking(),
        request.model_settings().effort(),
        max_tokens,
    )?;
    insert_output_config(&mut body, request)?;
    insert_number(
        &mut body,
        "temperature",
        request.model_settings().temperature(),
    );
    insert_number(&mut body, "top_p", request.model_settings().top_p());
    if streaming {
        body.insert("stream".to_owned(), Value::Bool(true));
    } else {
        body.remove("stream");
    }
    Ok(Value::Object(body))
}

fn reject_unsupported(request: &ModelRequest) -> Result<()> {
    if !matches!(request.continuation(), ConversationContinuation::None) {
        return Err(Error::caller(
            "Anthropic Messages has no server-managed conversation state; pass the full history instead of a continuation handle",
        ));
    }
    if request.model_settings().frequency_penalty().is_some()
        || request.model_settings().presence_penalty().is_some()
    {
        return Err(Error::caller(
            "Anthropic Messages does not accept frequency_penalty or presence_penalty",
        ));
    }
    if !request.model_settings().metadata().is_empty() {
        return Err(Error::caller(
            "Anthropic Messages does not have a provider-neutral metadata field; use the Anthropic extra_body bucket for supported metadata",
        ));
    }
    if request
        .model_settings()
        .extra_body()
        .contains_key("cache_control")
    {
        return Err(Error::caller(
            "Anthropic extra_body must not set cache_control; the adapter owns cache breakpoints from the request cache plan",
        ));
    }
    Ok(())
}

fn insert_system(
    body: &mut Map<String, Value>,
    request: &ModelRequest,
    tools: &Tools,
) -> Result<()> {
    let Some(instructions) = request.system_instructions() else {
        return Ok(());
    };
    let mut block = json!({"type": "text", "text": instructions});
    let tool_tokens = body
        .get("tools")
        .and_then(|value| serde_json::to_string(value).ok())
        .map_or(0, |text| estimate_tokens(&text));
    if request.cache_plan().is_some()
        && estimate_tokens(instructions) + tool_tokens >= MIN_CACHEABLE_PREFIX_TOKENS
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    let _ = tools;
    body.insert("system".to_owned(), Value::Array(vec![block]));
    Ok(())
}

async fn lower_messages(request: &ModelRequest) -> Result<Vec<Value>> {
    let normalized = InputItemNormalizer::new()
        .normalize_model_items(request.input())
        .map_err(|error| {
            Error::caller("Anthropic model input could not be normalized").with_source(error)
        })?;
    let mut accumulator = MessageAccumulator::default();
    let mut pending_tool_results = BTreeSet::new();
    for entry in normalized.entries() {
        match entry.item() {
            ModelInputItem::Message(message) => {
                ensure_tool_results_are_adjacent(&pending_tool_results)?;
                accumulator.message(message).await?;
            }
            ModelInputItem::Reasoning(reasoning) => {
                ensure_tool_results_are_adjacent(&pending_tool_results)?;
                accumulator.push(ASSISTANT_ROLE, vec![thinking_from_reasoning(reasoning)?]);
            }
            ModelInputItem::ToolCall(call) => {
                pending_tool_results.insert(call.call_id().clone());
                accumulator.push(ASSISTANT_ROLE, vec![json!({"type":"tool_use", "id":call.call_id().as_str(), "name":call.name(), "input":call.arguments()})]);
            }
            ModelInputItem::HandoffCall(call) => {
                pending_tool_results.insert(call.call_id().clone());
                let name = handoff_name(call, request)?;
                accumulator.push(ASSISTANT_ROLE, vec![json!({"type":"tool_use", "id":call.call_id().as_str(), "name":name, "input":call.arguments()})]);
            }
            ModelInputItem::ToolCallOutput(output) => {
                remove_pending_tool_result(&mut pending_tool_results, output.call_id())?;
                let mut result = json!({"type":"tool_result", "tool_use_id":output.call_id().as_str(), "content":lower_tool_output(output.output()).await?});
                if output.is_error() {
                    result["is_error"] = Value::Bool(true);
                }
                accumulator.push_tool_result(result);
            }
            ModelInputItem::HandoffOutput(output) => {
                remove_pending_tool_result(&mut pending_tool_results, output.call_id())?;
                accumulator.push_tool_result(json!({"type":"tool_result", "tool_use_id":output.call_id().as_str(), "content":output.note().unwrap_or("handoff completed")}));
            }
            ModelInputItem::Compaction(compaction) => {
                ensure_tool_results_are_adjacent(&pending_tool_results)?;
                accumulator.push(
                    "user",
                    vec![json!({"type":"text", "text":compaction.model_text()})],
                );
            }
            item => {
                return Err(Error::caller(format!(
                    "Anthropic Messages has no representation for `{}`",
                    item.label()
                )));
            }
        }
    }
    ensure_tool_results_are_adjacent(&pending_tool_results)?;
    Ok(accumulator.finish())
}

fn ensure_tool_results_are_adjacent(pending: &BTreeSet<ra_core::item::CallId>) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    Err(Error::caller(
        "Anthropic tool_result blocks must immediately follow their assistant tool_use blocks",
    ))
}

fn remove_pending_tool_result(
    pending: &mut BTreeSet<ra_core::item::CallId>,
    call_id: &ra_core::item::CallId,
) -> Result<()> {
    if pending.remove(call_id) {
        Ok(())
    } else {
        Err(Error::caller(
            "Anthropic tool_result has no preceding tool_use block in this request",
        ))
    }
}

#[derive(Default)]
struct MessageAccumulator {
    messages: Vec<Value>,
}
impl MessageAccumulator {
    fn push(&mut self, role: &str, blocks: Vec<Value>) {
        if let Some(last) = self
            .messages
            .last_mut()
            .filter(|last| last.get("role").and_then(Value::as_str) == Some(role))
        {
            last["content"]
                .as_array_mut()
                .expect("constructed content array")
                .extend(blocks);
        } else {
            self.messages.push(json!({"role": role, "content": blocks}));
        }
    }

    /// Adds a result as the first block of a user turn, or alongside its sibling results.
    fn push_tool_result(&mut self, block: Value) {
        let can_extend = self.messages.last().is_some_and(|last| {
            last.get("role").and_then(Value::as_str) == Some("user")
                && last
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().all(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                    })
        });
        if can_extend {
            self.messages
                .last_mut()
                .and_then(|message| message.get_mut("content"))
                .and_then(Value::as_array_mut)
                .expect("constructed tool-result content array")
                .push(block);
        } else {
            self.messages
                .push(json!({"role": "user", "content": [block]}));
        }
    }
    async fn message(&mut self, message: &Message) -> Result<()> {
        let role = match message.role() {
            MessageRole::User => "user",
            MessageRole::Assistant => ASSISTANT_ROLE,
            MessageRole::System => {
                return Err(Error::caller(
                    "Anthropic system messages belong in ModelRequest::system_instructions, not the input history",
                ));
            }
            _ => {
                return Err(Error::caller(
                    "unsupported message role for Anthropic Messages",
                ));
            }
        };
        let mut blocks = Vec::with_capacity(message.content().len());
        for block in message.content() {
            blocks.push(lower_content(block, message.role()).await?);
        }
        self.push(role, blocks);
        Ok(())
    }
    fn finish(self) -> Vec<Value> {
        self.messages
    }
}

async fn lower_content(block: &ContentBlock, role: MessageRole) -> Result<Value> {
    match block {
        ContentBlock::Text(text) => Ok(json!({"type":"text", "text":text.text()})),
        ContentBlock::Refusal(refusal) if matches!(role, MessageRole::Assistant) => {
            Ok(json!({"type":"text", "text":refusal.refusal()}))
        }
        ContentBlock::Thinking(thinking) if matches!(role, MessageRole::Assistant) => {
            Ok(thinking_block(thinking))
        }
        ContentBlock::Image(image) if matches!(role, MessageRole::User) => lower_image(image).await,
        ContentBlock::Image(_) => Err(Error::caller(
            "Anthropic Messages accepts image blocks only in user messages",
        )),
        _ => Err(Error::caller(format!(
            "Anthropic Messages cannot carry a `{}` block in a {} message",
            block.label(),
            role.label()
        ))),
    }
}

fn thinking_from_reasoning(reasoning: &ra_core::item::Reasoning) -> Result<Value> {
    // Both carriers replay by going back exactly as they arrived, so the stored block wins over any
    // reconstruction. `redacted_thinking` has to be named here as well as `thinking`: it carries no
    // readable text and no signature of its own, so demanding a signature from it would make every
    // turn after a redacted block unsendable.
    if let Some(raw) = reasoning.provider_data().filter(|raw| {
        matches!(
            raw.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    }) {
        return Ok(raw.clone());
    }
    let signature = reasoning.encrypted_content().ok_or_else(|| {
        Error::caller(
            "Anthropic reasoning replay requires the original thinking block and signature",
        )
    })?;
    Ok(json!({"type":"thinking", "thinking":reasoning.content().join("\n"), "signature":signature}))
}
fn thinking_block(block: &ThinkingBlock) -> Value {
    json!({"type":"thinking", "thinking":block.thinking(), "signature":block.signature()})
}

async fn lower_image(image: &ImageBlock) -> Result<Value> {
    let source = match image.source() {
        ImageSource::Base64(source) => {
            json!({"type":"base64", "media_type":source.media_type(), "data":source.data()})
        }
        ImageSource::Url(source) => json!({"type":"url", "url":source.url()}),
        ImageSource::LocalPath(source) => {
            let bytes = tokio::fs::read(source.path()).await.map_err(|error| {
                Error::caller(format!(
                    "could not read local image `{}`",
                    source.path().display()
                ))
                .with_source(error)
            })?;
            json!({"type":"base64", "media_type":media_type(source.path()), "data":STANDARD.encode(bytes)})
        }
        ImageSource::ProviderFile(_) => {
            return Err(Error::caller(
                "Anthropic Messages cannot reference an uploaded image by provider file id",
            ));
        }
        _ => {
            return Err(Error::caller(
                "unsupported image source for Anthropic Messages",
            ));
        }
    };
    Ok(json!({"type":"image", "source":source}))
}
fn media_type(path: &std::path::Path) -> &'static str {
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

async fn lower_tool_output(payload: &Value) -> Result<Value> {
    let Some(output) = ToolOutput::from_stored(payload)? else {
        return payload.as_str().map_or_else(
            || {
                serde_json::to_string(payload)
                    .map_err(|error| {
                        Error::caller("could not serialize Anthropic tool result")
                            .with_source(error)
                    })
                    .map(Value::String)
            },
            |text| Ok(Value::String(text.to_owned())),
        );
    };
    let mut blocks = Vec::new();
    for block in output.model_blocks() {
        match block {
            ToolOutputBlock::Text { text } => blocks.push(json!({"type":"text", "text":text})),
            ToolOutputBlock::Image(image) => blocks.push(lower_image(&image).await?),
            ToolOutputBlock::File(_) => {
                return Err(Error::caller(
                    "Anthropic Messages tool results cannot carry provider-neutral file blocks",
                ));
            }
            _ => {
                return Err(Error::caller(
                    "unsupported structured tool-output block for Anthropic Messages",
                ));
            }
        }
    }
    Ok(Value::Array(blocks))
}

fn handoff_name(call: &HandoffCall, request: &ModelRequest) -> Result<String> {
    call.tool_name().or_else(|| request.handoffs().iter().find(|handoff| handoff.target_agent() == call.target_agent()).map(ModelHandoffDefinition::name)).map(str::to_owned).ok_or_else(|| Error::caller(format!("handoff call to `{}` carries no tool name and its handoff is not advertised in this request", call.target_agent())))
}

struct Tools {
    names: BTreeSet<String>,
    populated: bool,
}
fn merge_tools(
    body: &mut Map<String, Value>,
    tools: &[ModelToolDefinition],
    handoffs: &[ModelHandoffDefinition],
) -> Result<Tools> {
    let mut lowered = match body.remove("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(Error::caller("Anthropic extra_body.tools must be an array")),
    };
    let mut names = BTreeSet::new();
    for tool in &lowered {
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            validate_tool_name(name)?;
            if !names.insert(name.to_owned()) {
                return Err(Error::caller(format!(
                    "duplicate Anthropic tool/handoff name `{name}`"
                )));
            }
        }
    }
    for (name, description, schema) in tools
        .iter()
        .map(|tool| (tool.name(), tool.description(), tool.input_schema()))
        .chain(handoffs.iter().map(|handoff| {
            (
                handoff.name(),
                handoff.description(),
                handoff.input_schema(),
            )
        }))
    {
        validate_tool_name(name)?;
        if !names.insert(name.to_owned()) {
            return Err(Error::caller(format!(
                "duplicate Anthropic tool/handoff name `{name}`"
            )));
        }
        let mut tool = json!({"name":name, "input_schema":schema});
        if let Some(description) = description {
            tool["description"] = Value::String(description.to_owned());
        }
        lowered.push(tool);
    }
    let populated = !lowered.is_empty();
    if populated {
        body.insert("tools".to_owned(), Value::Array(lowered));
    }
    Ok(Tools { names, populated })
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
fn apply_tool_choice(
    body: &mut Map<String, Value>,
    choice: Option<&ToolChoice>,
    parallel: Option<bool>,
    tools: &Tools,
) -> Result<()> {
    let choice = match choice {
        // With no tool table there is nothing to forbid, and a `tool_choice` beside an absent
        // `tools` is not a request Anthropic accepts.
        Some(ToolChoice::None) if !tools.populated => return Ok(()),
        None if parallel != Some(false) => return Ok(()),
        Some(choice) => choice,
        None => &ToolChoice::Auto,
    };
    let mut value = match choice {
        ToolChoice::Auto => json!({"type":"auto"}),
        ToolChoice::Required => json!({"type":"any"}),
        // Saying "no tools this turn" here rather than by withdrawing the tool table is also the
        // cache-preserving way to say it: `tool_choice` does not invalidate the cached
        // tools-and-system prefix, while changing the tool definitions rebuilds all of it.
        ToolChoice::None => json!({"type":"none"}),
        ToolChoice::Tool(name) => {
            validate_tool_name(name)?;
            if !tools.names.contains(name) {
                return Err(Error::caller(format!(
                    "Anthropic tool_choice names unavailable function `{name}`"
                )));
            }
            json!({"type":"tool", "name":name})
        }
        ToolChoice::Mcp(_) => {
            return Err(Error::caller(
                "Anthropic Messages cannot select a hosted MCP tool",
            ));
        }
        _ => {
            return Err(Error::caller(
                "unsupported tool choice for Anthropic Messages",
            ));
        }
    };
    if parallel == Some(false) && !matches!(choice, ToolChoice::None) {
        value["disable_parallel_tool_use"] = Value::Bool(true);
    }
    body.insert("tool_choice".to_owned(), value);
    Ok(())
}
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
        ThinkingConfig::Adaptive => json!({"type":"adaptive"}),
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
            json!({"type":"enabled", "budget_tokens":budget_tokens})
        }
        ThinkingConfig::Disabled => {
            if matches!(effort, Some(Effort::XHigh | Effort::Max)) {
                return Err(Error::caller(
                    "Anthropic rejects thinking.type=disabled at the highest effort levels",
                ));
            }
            json!({"type":"disabled"})
        }
        _ => {
            return Err(Error::caller(
                "unsupported ThinkingConfig for Anthropic Messages",
            ));
        }
    };
    body.insert("thinking".to_owned(), value);
    Ok(())
}
/// Writes the effort level and the structured-output schema, which share one wire field.
///
/// `output_config` holds both, so they are written together rather than each inserting a key of its
/// own: the schema's former home, a top-level `output_format`, is the deprecated spelling, and
/// moving it into `output_config` without merging would have let whichever of the two was written
/// last erase the other.
///
/// Anything the caller put under `output_config` in the provider bucket is kept — that is where
/// fields this crate does not model yet, such as a task budget, travel.
fn insert_output_config(body: &mut Map<String, Value>, request: &ModelRequest) -> Result<()> {
    let mut config = match body.remove("output_config") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(existing)) => existing,
        Some(_) => {
            return Err(Error::caller(
                "Anthropic extra_body.output_config must be an object",
            ));
        }
    };
    if let Some(effort) = request.model_settings().effort() {
        config.insert(
            "effort".to_owned(),
            Value::String(effort.label().to_owned()),
        );
    }
    if let Some(schema) = request.output_schema() {
        config.insert(
            "format".to_owned(),
            json!({"type": "json_schema", "schema": schema.schema()}),
        );
    }
    if !config.is_empty() {
        body.insert("output_config".to_owned(), Value::Object(config));
    }
    Ok(())
}

fn insert_number(body: &mut Map<String, Value>, name: &str, value: Option<f64>) {
    if let Some(number) = value.and_then(serde_json::Number::from_f64) {
        body.insert(name.to_owned(), Value::Number(number));
    }
}
