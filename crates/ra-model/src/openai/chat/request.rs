//! Lower a provider-neutral request into a chat-completion create payload.

use std::collections::BTreeSet;

use ra_core::{
    error::{Error, Result},
    model::{
        ConversationContinuation, ModelHandoffDefinition, ModelRequest, ModelToolDefinition,
        ToolChoice,
    },
    prompt::{CachePlan, MIN_CACHEABLE_PREFIX_TOKENS, estimate_tokens},
};
use serde_json::{Map, Value, json};

use super::{ChatCodec, convert};
use crate::openai::error::behavior_error;

pub(crate) async fn build_request_body(
    codec: &ChatCodec,
    request: &ModelRequest,
    streaming: bool,
) -> Result<Value> {
    reject_unsupported_settings(codec, request)?;
    request.validate_cache_plan()?;

    let mut body = request
        .model_settings()
        .extra_body()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();

    body.insert("model".to_owned(), Value::String(codec.model.clone()));
    body.insert(
        "messages".to_owned(),
        Value::Array(lower_messages(codec, request).await?),
    );

    insert_optional_number(
        &mut body,
        "temperature",
        request.model_settings().temperature(),
    );
    insert_optional_number(&mut body, "top_p", request.model_settings().top_p());
    insert_optional_number(
        &mut body,
        "frequency_penalty",
        request.model_settings().frequency_penalty(),
    );
    insert_optional_number(
        &mut body,
        "presence_penalty",
        request.model_settings().presence_penalty(),
    );
    if let Some(max_tokens) = request.model_settings().max_tokens() {
        body.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    if let Some(effort) = request.model_settings().effort() {
        body.insert(
            "reasoning_effort".to_owned(),
            Value::String(effort.label().to_owned()),
        );
    }
    if !request.model_settings().metadata().is_empty() {
        body.insert(
            "metadata".to_owned(),
            serde_json::to_value(request.model_settings().metadata()).map_err(|error| {
                behavior_error("OpenAI metadata could not be serialized").with_source(error)
            })?,
        );
    }
    if let Some(output_schema) = request.output_schema() {
        body.insert(
            "response_format".to_owned(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": output_schema.name(),
                    "schema": output_schema.schema(),
                    "strict": output_schema.strict()
                }
            }),
        );
    }

    let tools = merge_tools(&mut body, request.tools(), request.handoffs())?;
    apply_tool_choice(&mut body, request, &tools)?;
    apply_parallel_tool_calls(&mut body, codec, request, tools.populated);
    apply_streaming(&mut body, codec, streaming);
    apply_store(&mut body, codec);
    insert_cache_scope(&mut body, codec, request);
    Ok(Value::Object(body))
}

/// Places the stable prefix at index 0, where the protocol caches from.
async fn lower_messages(codec: &ChatCodec, request: &ModelRequest) -> Result<Vec<Value>> {
    let mut messages = convert::lower_messages(codec, request).await?;
    if let Some(instructions) = request.system_instructions() {
        messages.insert(0, json!({"role": "system", "content": instructions}));
    }
    Ok(messages)
}

/// Refuses a setting this protocol has nowhere to put.
fn reject_unsupported_settings(codec: &ChatCodec, request: &ModelRequest) -> Result<()> {
    // The framework owns this field whenever it can send one, so a static value underneath it is
    // rejected rather than silently overridden. `extra_body` is provider *registration* data,
    // shared by every run of that provider, while a cache scope has a session lifetime; a static
    // key there quietly routes every run onto one bucket.
    if request
        .model_settings()
        .extra_body()
        .contains_key("prompt_cache_key")
    {
        return Err(Error::caller(
            "OpenAI extra_body must not set prompt_cache_key: it is static provider registration \
             data shared by every run, while the cache scope on the request has a session \
             lifetime; set the scope on the request instead",
        ));
    }
    // Every shape is refused, `Disabled` included. This protocol has no thinking switch at all, so
    // accepting an explicit "off" and then sending nothing leaves a reasoning model thinking at its
    // default effort with no trace of the setting anywhere in the request.
    if request.model_settings().thinking().is_some() {
        codec.degrade(
            "OpenAI Chat Completions expresses reasoning through reasoning_effort and cannot carry \
             a ThinkingConfig",
        )?;
    }
    // Chat Completions has no server-side conversation at all: the caller replays the whole
    // history or the model sees nothing. Continuing anyway would send a first turn that silently
    // lost every prior one, which reads as a model that forgot rather than as a misconfiguration.
    if !matches!(request.continuation(), ConversationContinuation::None) {
        codec.degrade(
            "OpenAI Chat Completions has no server-managed conversation state; pass the full \
             history instead of a continuation handle",
        )?;
    }
    Ok(())
}

/// Sends `stream` and, where the endpoint accepts it, asks for usage on the terminal frame.
///
/// Without `stream_options.include_usage` a streamed call reports no usage at all, and cache hit
/// rates cannot be computed for it. It is still opt-in: several compatible gateways answer the
/// field with an HTTP 400, which costs the whole call rather than one statistic.
fn apply_streaming(body: &mut Map<String, Value>, codec: &ChatCodec, streaming: bool) {
    if !streaming {
        body.remove("stream");
        body.remove("stream_options");
        return;
    }
    body.insert("stream".to_owned(), Value::Bool(true));
    if codec.quirks.stream_usage() && !body.contains_key("stream_options") {
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
}

/// Sends `store` only where retention exists.
fn apply_store(body: &mut Map<String, Value>, codec: &ChatCodec) {
    if body.contains_key("store") || !codec.quirks.store() {
        return;
    }
    // First-party Chat Completions defaults this to true, matching Responses, so that a completion
    // stays retrievable afterwards.
    body.insert("store".to_owned(), Value::Bool(true));
}

/// Lowers a cache scope onto the wire, where the endpoint honours it and the span is worth caching.
///
/// Called after the tool table is merged, and that ordering is load bearing: the cached prefix is
/// the system message *plus the whole tool table*, and measuring before the merge would judge a
/// short instruction block sitting in front of a large tool table as not worth caching.
///
/// Neither an unsupported endpoint nor a short span is an error. Caching is a discount; refusing to
/// issue the request rather than issuing it uncached would trade the call for the saving.
fn insert_cache_scope(body: &mut Map<String, Value>, codec: &ChatCodec, request: &ModelRequest) {
    if !codec.quirks.prompt_cache_key() {
        return;
    }
    let Some(scope) = request.cache_plan().and_then(CachePlan::cache_scope) else {
        return;
    };
    let instructions = request.system_instructions().map_or(0, estimate_tokens);
    let tools = body.get("tools").map_or(0, |tools| {
        serde_json::to_string(tools).map_or(0, |wire| estimate_tokens(&wire))
    });
    if instructions + tools < MIN_CACHEABLE_PREFIX_TOKENS {
        return;
    }
    body.insert(
        "prompt_cache_key".to_owned(),
        Value::String(scope.to_owned()),
    );
}

fn insert_optional_number(body: &mut Map<String, Value>, name: &str, value: Option<f64>) {
    if let Some(value) = value.and_then(serde_json::Number::from_f64) {
        body.insert(name.to_owned(), Value::Number(value));
    }
}

/// The merged tool table and the function names it advertises.
struct MergedTools {
    populated: bool,
    function_names: BTreeSet<String>,
}

/// Appends the protocol-neutral tools and handoffs to any tools already in `extra_body`.
///
/// Replacing the array would make the two mutually exclusive; the names are checked across both
/// sources so a collision fails locally instead of producing an ambiguous call.
fn merge_tools(
    body: &mut Map<String, Value>,
    tools: &[ModelToolDefinition],
    handoffs: &[ModelHandoffDefinition],
) -> Result<MergedTools> {
    let mut lowered = match body.remove("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(existing)) => existing,
        Some(_) => return Err(Error::caller("OpenAI extra_body.tools must be an array")),
    };
    let mut names = BTreeSet::new();
    for name in lowered
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
    {
        validate_function_name(name)?;
        if !names.insert(name.to_owned()) {
            return Err(Error::caller(format!(
                "duplicate OpenAI tool/handoff name `{name}`"
            )));
        }
    }
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
        validate_function_name(name)?;
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
    Ok(MergedTools {
        populated,
        function_names: names,
    })
}

/// Rejects a name the endpoint will reject, while the call site is still visible.
fn validate_function_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(Error::caller(format!(
            "OpenAI tool name `{name}` must be 1-64 characters of [A-Za-z0-9_-]"
        )));
    }
    Ok(())
}

/// Chat Completions nests the declaration one level deeper than Responses does.
fn function_tool(name: &str, description: Option<&str>, schema: &Value, strict: bool) -> Value {
    let mut function = json!({
        "name": name,
        "parameters": schema,
        "strict": strict
    });
    if let Some(description) = description {
        function["description"] = Value::String(description.to_owned());
    }
    json!({"type": "function", "function": function})
}

fn apply_tool_choice(
    body: &mut Map<String, Value>,
    request: &ModelRequest,
    tools: &MergedTools,
) -> Result<()> {
    if !tools.populated {
        let Some(tool_choice) = request.model_settings().tool_choice() else {
            return Ok(());
        };
        if !matches!(tool_choice, ToolChoice::None | ToolChoice::Auto) {
            return Err(Error::caller(
                "OpenAI tool_choice requires at least one tool or handoff",
            ));
        }
        body.insert(
            "tool_choice".to_owned(),
            lower_tool_choice(tool_choice, &tools.function_names)?,
        );
        return Ok(());
    }

    if let Some(tool_choice) = request.model_settings().tool_choice() {
        body.insert(
            "tool_choice".to_owned(),
            lower_tool_choice(tool_choice, &tools.function_names)?,
        );
    } else if !body.contains_key("tool_choice") {
        body.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
    }
    Ok(())
}

/// Sends `parallel_tool_calls` when asked for, or by default only where the endpoint accepts it.
///
/// An explicit setting always wins: a caller that turned parallel calls off is expressing a
/// constraint on the run, and dropping it because a capability was never declared would run tools
/// concurrently that the caller said must not be.
fn apply_parallel_tool_calls(
    body: &mut Map<String, Value>,
    codec: &ChatCodec,
    request: &ModelRequest,
    tools_present: bool,
) {
    if let Some(parallel) = request.model_settings().parallel_tool_calls() {
        body.insert("parallel_tool_calls".to_owned(), Value::Bool(parallel));
    } else if tools_present
        && codec.quirks.parallel_tool_calls()
        && !body.contains_key("parallel_tool_calls")
    {
        body.insert("parallel_tool_calls".to_owned(), Value::Bool(true));
    }
}

fn lower_tool_choice(choice: &ToolChoice, function_names: &BTreeSet<String>) -> Result<Value> {
    match choice {
        ToolChoice::Auto => Ok(Value::String("auto".to_owned())),
        ToolChoice::Required => Ok(Value::String("required".to_owned())),
        ToolChoice::None => Ok(Value::String("none".to_owned())),
        ToolChoice::Tool(name) => {
            validate_function_name(name)?;
            if !function_names.contains(name) {
                return Err(Error::caller(format!(
                    "OpenAI tool_choice names unavailable function `{name}`"
                )));
            }
            Ok(json!({"type": "function", "function": {"name": name}}))
        }
        // Hosted MCP is a Responses feature. There is no Chat Completions request field that would
        // name a server-side tool, so the choice cannot be honoured and is refused rather than
        // quietly turned into `auto`.
        ToolChoice::Mcp(_) => Err(Error::caller(
            "OpenAI Chat Completions cannot select a hosted MCP tool",
        )),
        _ => Err(Error::caller(
            "unsupported tool choice for OpenAI Chat Completions",
        )),
    }
}
