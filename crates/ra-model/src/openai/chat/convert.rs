//! Convert between provider-neutral items and Chat Completions messages.
//!
//! Both directions live here because they are one contract read twice. Lowering decides where a
//! reasoning item's signed material goes; lifting decides where it came from, and the two have to
//! agree or a history stops round-tripping after exactly one turn.
//!
//! # Why this is a state machine rather than a map
//!
//! In the neutral item model — which is Responses-shaped — a reasoning item, an assistant message
//! and each tool call are separate, sibling items. Chat Completions requires all of them to be
//! folded into a single `role: "assistant"` message, with the calls in `tool_calls` beside the
//! content. A per-item mapping cannot express that: the mapping for a tool call depends on whether
//! an assistant message is currently open. So the loop keeps one draft assistant message, attaches
//! to it, and flushes when a user or tool message forces the turn to close.

use std::collections::VecDeque;

use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{
        CallId, ContentBlock, FileBlock, FileSource, HandoffCall, ImageBlock, InputItemNormalizer,
        ItemId, Message, MessageRole, ModelInputItem, ModelResponse, RawProviderItem, Reasoning,
        RunItem, RunItemKind, ThinkingBlock, ToolCall,
    },
    model::{ModelHandoffDefinition, ModelRequest, ProviderKey},
    tool::{ToolOutput, ToolOutputBlock},
    usage::{RequestUsage, Usage},
};
use serde_json::{Map, Value, json};

use super::{
    ChatCodec, FAKE_ITEM_ID,
    reasoning::{ReasoningReplayContext, default_should_replay_reasoning},
};
use crate::openai::{
    content::{ResolvedImage, resolve_image, stringify_tool_output},
    error::behavior_error,
};

/// Stands in for a tool result that Chat Completions cannot carry.
///
/// A `role: "tool"` message with no content makes the whole request invalid, so the pairing has to
/// be preserved by something. Naming the omission is better than an empty string: the model reads
/// that the call was answered and that the answer was not text.
const OMITTED_TOOL_OUTPUT: &str = "[tool output omitted]";

/// What a provider reports when it withholds a turn without saying anything else.
const CONTENT_FILTER_REFUSAL: &str = "Response withheld by the provider's content filter.";

// ---------------------------------------------------------------------------------------------
// Lowering: neutral items into Chat messages
// ---------------------------------------------------------------------------------------------

/// Lowers the request's input history into a `messages` array.
///
/// The stable system prefix is **not** added here; the request builder places it at index 0, which
/// is the position the protocol caches from.
pub(crate) async fn lower_messages(
    codec: &ChatCodec,
    request: &ModelRequest,
) -> Result<Vec<Value>> {
    let normalized = InputItemNormalizer::new()
        .normalize_model_items(request.input())
        .map_err(|error| {
            behavior_error("OpenAI model input could not be normalized").with_source(error)
        })?;
    let mut accumulator = MessageAccumulator::default();
    for entry in normalized.entries() {
        lower_item(&mut accumulator, entry.item(), request, codec).await?;
    }
    Ok(accumulator.finish())
}

async fn lower_item(
    accumulator: &mut MessageAccumulator,
    item: &ModelInputItem,
    request: &ModelRequest,
    codec: &ChatCodec,
) -> Result<()> {
    match item {
        ModelInputItem::Message(message) => lower_message(accumulator, message, codec).await,
        ModelInputItem::Reasoning(reasoning) => {
            accumulator.absorb_reasoning(reasoning, codec);
            Ok(())
        }
        ModelInputItem::ToolCall(call) => {
            accumulator.push_tool_call(call.call_id(), call.name(), call.arguments())
        }
        ModelInputItem::HandoffCall(call) => {
            let name = handoff_tool_name(call, request)?;
            accumulator.push_tool_call(call.call_id(), &name, call.arguments())
        }
        ModelInputItem::ToolCallOutput(output) => {
            let content = lower_tool_output(output.output(), codec).await?;
            accumulator.push_tool_message(output.call_id(), content);
            Ok(())
        }
        ModelInputItem::HandoffOutput(output) => {
            let note = output.note().unwrap_or("handoff completed");
            accumulator.push_tool_message(output.call_id(), Value::String(note.to_owned()));
            Ok(())
        }
        // The summary is protocol-neutral by construction, so it survives as an ordinary user
        // turn. The reference implementation refuses compaction on this protocol; that would mean
        // a conversation becomes unsendable the moment it is compacted, which is the opposite of
        // what compaction is for. Its wording belongs to the step that produced it.
        ModelInputItem::Compaction(compaction) => {
            accumulator.push_message(json!({
                "role": MessageRole::User.label(),
                "content": compaction.model_text()
            }));
            Ok(())
        }
        _ => Err(Error::caller(format!(
            "OpenAI Chat Completions has no representation for `{}`",
            item.label()
        ))),
    }
}

/// Resolves the wire name a handoff was advertised under.
///
/// The agent that received control does not advertise the handoff that led to it, so the recorded
/// tool name is the only reliable source once the run has moved on.
fn handoff_tool_name(call: &HandoffCall, request: &ModelRequest) -> Result<String> {
    call.tool_name()
        .or_else(|| {
            request
                .handoffs()
                .iter()
                .find(|handoff| handoff.target_agent() == call.target_agent())
                .map(ModelHandoffDefinition::name)
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::caller(format!(
                "handoff call to `{}` carries no tool name and its handoff is not advertised in \
                 this request",
                call.target_agent()
            ))
        })
}

async fn lower_message(
    accumulator: &mut MessageAccumulator,
    message: &Message,
    codec: &ChatCodec,
) -> Result<()> {
    match message.role() {
        MessageRole::Assistant => lower_assistant_message(accumulator, message, codec),
        role => {
            let mut parts = Vec::with_capacity(message.content().len());
            for block in message.content() {
                parts.push(lower_input_block(block, role).await?);
            }
            accumulator.push_message(json!({
                "role": role.label(),
                "content": text_or_parts(parts)
            }));
            Ok(())
        }
    }
}

fn lower_assistant_message(
    accumulator: &mut MessageAccumulator,
    message: &Message,
    codec: &ChatCodec,
) -> Result<()> {
    let mut draft = AssistantDraft::default();
    let mut texts = Vec::new();
    let mut thinking = Vec::new();
    for block in message.content() {
        match block {
            ContentBlock::Text(text) => texts.push(text.text().to_owned()),
            ContentBlock::Refusal(refusal) => draft.refusal = Some(refusal.refusal().to_owned()),
            ContentBlock::Thinking(block) => thinking.push(thinking_block(block)),
            _ => {
                return Err(Error::caller(
                    "unsupported assistant content block for OpenAI Chat Completions",
                ));
            }
        }
    }
    // A single assistant turn is one message, so several text blocks become one string. The
    // protocol has no place to record where the seams were.
    if !texts.is_empty() {
        draft.content = Some(Value::String(texts.join("\n")));
    }
    if !thinking.is_empty() {
        if codec.quirks.thinking_blocks() {
            draft.thinking_blocks = Some(thinking);
        } else {
            codec.degrade(
                "OpenAI Chat Completions has no thinking block; this endpoint has not declared \
                 that it round-trips them, so the signed reasoning is dropped",
            )?;
        }
    }
    accumulator.begin_assistant(draft);
    Ok(())
}

/// Lowers one content block of a user or system message.
async fn lower_input_block(block: &ContentBlock, role: MessageRole) -> Result<Value> {
    match block {
        ContentBlock::Text(text) => Ok(json!({"type": "text", "text": text.text()})),
        ContentBlock::Image(image) if matches!(role, MessageRole::User) => {
            lower_image_part(image).await
        }
        ContentBlock::Image(_) => Err(Error::caller(
            "OpenAI Chat Completions accepts an image only in a user message",
        )),
        _ => Err(Error::caller(format!(
            "OpenAI Chat Completions cannot carry a `{}` block in a {} message",
            block.label(),
            role.label()
        ))),
    }
}

/// Lowers one image block to an `image_url` part.
///
/// An uploaded-file reference is refused rather than translated: the Chat `file` part addresses
/// documents, and passing an image identifier through it produces a request the endpoint accepts
/// and a model that never sees the picture.
async fn lower_image_part(image: &ImageBlock) -> Result<Value> {
    match resolve_image(image.source()).await? {
        ResolvedImage::Url(url) => {
            let mut payload = json!({"url": url});
            if let Some(detail) = image.detail() {
                payload["detail"] = Value::String(detail.label().to_owned());
            }
            Ok(json!({"type": "image_url", "image_url": payload}))
        }
        ResolvedImage::ProviderFile(_) => Err(Error::caller(
            "OpenAI Chat Completions cannot reference an uploaded image by file id",
        )),
    }
}

/// Lowers one file block to a `file` part.
fn lower_file_part(file: &FileBlock) -> Result<Value> {
    match file.source() {
        FileSource::Base64(source) => {
            let mut payload = json!({"file_data": source.data()});
            if let Some(filename) = source.filename() {
                payload["filename"] = Value::String(filename.to_owned());
            }
            Ok(json!({"type": "file", "file": payload}))
        }
        FileSource::ProviderFile(source) => {
            Ok(json!({"type": "file", "file": {"file_id": source.file_id()}}))
        }
        _ => Err(Error::caller(
            "unsupported file source for OpenAI Chat Completions",
        )),
    }
}

/// Lowers a tool result into what a `role: "tool"` message accepts.
///
/// Chat Completions carries text in a tool result. Non-text blocks are dropped unless the endpoint
/// has declared it understands them, and a result left with nothing at all becomes a named
/// placeholder rather than an empty message the endpoint would reject.
async fn lower_tool_output(payload: &Value, codec: &ChatCodec) -> Result<Value> {
    let Some(output) = ToolOutput::from_stored(payload)? else {
        return stringify_tool_output(payload).map(Value::String);
    };
    let multimodal = codec.quirks.multimodal_tool_output();
    let mut parts = Vec::new();
    let mut dropped = false;
    for block in output.model_blocks() {
        match block {
            ToolOutputBlock::Text { text } => parts.push(json!({"type": "text", "text": text})),
            ToolOutputBlock::Image(image) if multimodal => {
                parts.push(lower_image_part(&image).await?);
            }
            ToolOutputBlock::File(file) if multimodal => parts.push(lower_file_part(&file)?),
            _ => dropped = true,
        }
    }
    if parts.is_empty() {
        codec.degrade(
            "OpenAI Chat Completions tool results carry text, and this result has none; declare \
             multimodal tool output on the endpoint to send the rest",
        )?;
        return Ok(Value::String(OMITTED_TOOL_OUTPUT.to_owned()));
    }
    if dropped {
        codec.degrade(
            "a tool result contained non-text content that OpenAI Chat Completions cannot carry",
        )?;
    }
    Ok(text_or_parts(parts))
}

/// Collapses a lone text part into a plain string.
///
/// The array form is valid everywhere the protocol is implemented faithfully, and a plain string
/// is valid everywhere at all — several compatible gateways reject an array on the system and tool
/// roles. The two encode exactly the same content, so the more portable one is chosen.
fn text_or_parts(mut parts: Vec<Value>) -> Value {
    if parts.len() == 1
        && let Some(text) = parts[0]
            .get("text")
            .filter(|_| parts[0].get("type").and_then(Value::as_str) == Some("text"))
    {
        return text.clone();
    }
    if parts.is_empty() {
        parts.push(json!({"type": "text", "text": ""}));
    }
    Value::Array(parts)
}

fn thinking_block(block: &ThinkingBlock) -> Value {
    json!({
        "type": "thinking",
        "thinking": block.thinking(),
        "signature": block.signature()
    })
}

// ---------------------------------------------------------------------------------------------
// The assistant-merge state machine
// ---------------------------------------------------------------------------------------------

/// Reasoning text waiting for the assistant message that owns it.
///
/// Both spellings are carried, because a gateway that returned one of them expects that one back:
/// collapsing them would replay `reasoning_content` to an endpoint that only ever speaks
/// `reasoning`, and drop the body of one that speaks both.
#[derive(Default)]
struct PendingReasoning {
    summary: Option<String>,
    body: Option<String>,
}

impl PendingReasoning {
    fn is_empty(&self) -> bool {
        self.summary.is_none() && self.body.is_none()
    }
}

/// Thinking blocks waiting for the assistant message that owns them.
struct PendingThinking {
    blocks: Vec<Value>,
    /// Whether these are the provider's own ordered sequence rather than a reconstruction.
    native: bool,
}

/// One assistant message under construction.
#[derive(Default)]
struct AssistantDraft {
    content: Option<Value>,
    tool_calls: Vec<Value>,
    refusal: Option<String>,
    thinking_blocks: Option<Vec<Value>>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
}

impl AssistantDraft {
    fn into_message(self) -> Value {
        let mut message = Map::new();
        message.insert(
            "role".to_owned(),
            Value::String(MessageRole::Assistant.label().to_owned()),
        );
        message.insert("content".to_owned(), self.content.unwrap_or(Value::Null));
        // The API rejects an empty array here, so the field is absent rather than empty.
        if !self.tool_calls.is_empty() {
            message.insert("tool_calls".to_owned(), Value::Array(self.tool_calls));
        }
        if let Some(refusal) = self.refusal {
            message.insert("refusal".to_owned(), Value::String(refusal));
        }
        if let Some(blocks) = self.thinking_blocks {
            message.insert("thinking_blocks".to_owned(), Value::Array(blocks));
        }
        if let Some(reasoning) = self.reasoning_content {
            message.insert("reasoning_content".to_owned(), Value::String(reasoning));
        }
        if let Some(reasoning) = self.reasoning {
            message.insert("reasoning".to_owned(), Value::String(reasoning));
        }
        Value::Object(message)
    }
}

#[derive(Default)]
struct MessageAccumulator {
    messages: Vec<Value>,
    current: Option<AssistantDraft>,
    pending_thinking: Option<PendingThinking>,
    pending_reasoning: Option<PendingReasoning>,
}

impl MessageAccumulator {
    fn finish(mut self) -> Vec<Value> {
        self.flush(true);
        self.messages
    }

    /// Closes the open assistant message, if any.
    ///
    /// `clear_pending` is false for exactly one caller: an assistant message arriving directly
    /// after a reasoning item belongs to the same turn, and tool calls may still follow it, so the
    /// reasoning material has to survive the flush that opens the new draft.
    fn flush(&mut self, clear_pending: bool) {
        if let Some(draft) = self.current.take() {
            // An assistant turn that made no tool call has ended. Reasoning text still waiting at
            // this point belongs to nothing, and leaving it would attach it to the next turn's
            // message — reasoning the model never produced, presented as its own.
            if draft.tool_calls.is_empty() {
                self.pending_reasoning = None;
            }
            self.messages.push(draft.into_message());
        } else if clear_pending {
            self.pending_reasoning = None;
        }
        if clear_pending {
            // Thinking blocks belong to the assistant turn that produced them. A reasoning item
            // not directly followed by that turn's message must not leak its signed blocks into a
            // later one: the signature is minted for one turn and rejected anywhere else.
            self.pending_thinking = None;
        }
    }

    /// Opens a new assistant message, adopting whatever reasoning is pending.
    fn begin_assistant(&mut self, mut draft: AssistantDraft) {
        self.flush(false);
        self.attach_pending(&mut draft);
        self.current = Some(draft);
    }

    /// Returns the open assistant message, opening one when a tool call arrives without it.
    fn ensure_assistant(&mut self) -> &mut AssistantDraft {
        let mut draft = self.current.take().unwrap_or_default();
        self.attach_pending(&mut draft);
        self.current.insert(draft)
    }

    fn attach_pending(&mut self, draft: &mut AssistantDraft) {
        if let Some(pending) = self.pending_thinking.take()
            && !pending.blocks.is_empty()
        {
            if pending.native {
                // The provider's own sequence is the replay source of truth: it can express an
                // empty thinking block and a redacted one, and the normalized text and signature
                // fields can express neither. Blocks already carried by the message win, because
                // they describe this same turn and were not reconstructed.
                if draft.thinking_blocks.is_none() {
                    draft.thinking_blocks = Some(pending.blocks);
                }
            } else {
                let mut parts = pending.blocks;
                match draft.content.take() {
                    Some(Value::String(text)) => parts.push(json!({"type": "text", "text": text})),
                    Some(Value::Array(existing)) => parts.extend(existing),
                    _ => {}
                }
                draft.content = Some(Value::Array(parts));
            }
        }
        if let Some(reasoning) = self.pending_reasoning.take() {
            if reasoning.summary.is_some() {
                draft.reasoning_content = reasoning.summary;
            }
            if reasoning.body.is_some() {
                draft.reasoning = reasoning.body;
            }
        }
    }

    fn push_message(&mut self, message: Value) {
        self.flush(true);
        self.messages.push(message);
    }

    fn push_tool_message(&mut self, call_id: &CallId, content: Value) {
        let mut message = Map::new();
        message.insert("role".to_owned(), Value::String("tool".to_owned()));
        message.insert(
            "tool_call_id".to_owned(),
            Value::String(call_id.as_str().to_owned()),
        );
        message.insert("content".to_owned(), content);
        self.push_message(Value::Object(message));
    }

    fn push_tool_call(&mut self, call_id: &CallId, name: &str, arguments: &Value) -> Result<()> {
        // The wire field is a JSON *string*, and an absent argument object is `{}` rather than
        // `null`: a model asked to re-read its own call sees the empty object it produced.
        let arguments = if arguments.is_null() {
            "{}".to_owned()
        } else {
            serde_json::to_string(arguments).map_err(|error| {
                behavior_error("tool-call arguments could not be serialized").with_source(error)
            })?
        };
        let draft = self.ensure_assistant();
        draft.tool_calls.push(json!({
            "id": call_id.as_str(),
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        }));
        Ok(())
    }

    /// Holds a reasoning item's replay material until the assistant message that owns it.
    fn absorb_reasoning(&mut self, reasoning: &Reasoning, codec: &ChatCodec) {
        let provider_data = reasoning.provider_data();
        let origin_model = provider_data
            .and_then(|data| data.get("model"))
            .and_then(Value::as_str);
        let context = ReasoningReplayContext::new(&codec.model, &codec.base_url)
            .with_origin_model(origin_model)
            .with_provider_data(provider_data);

        // The caller policy is deliberately not consulted for thinking blocks. A signature is
        // minted for one model and refused by every other, so ownership is a correctness rule
        // rather than a preference a configuration may override.
        if codec.quirks.thinking_blocks() && default_should_replay_reasoning(&context) {
            self.pending_thinking = replayable_thinking_blocks(reasoning);
        }
        if codec.quirks.reasoning_content() && codec.replay.allows(&context) {
            self.pending_reasoning = replayable_reasoning(reasoning);
        }
    }
}

/// Recovers the reasoning text to resend, in the wire field it originally arrived in.
///
/// The recorded provider payload wins, because only it says which spelling the endpoint used. The
/// fallback to the normalized summary covers records written before that was tracked, and by other
/// adapters.
///
/// There is deliberately **no** fallback for the body. `content` is also where thinking-block text
/// and Responses-style reasoning text land, so inventing a `reasoning` field out of it would send
/// a gateway text it never produced, under a name it may not accept.
fn replayable_reasoning(reasoning: &Reasoning) -> Option<PendingReasoning> {
    let recorded = reasoning.provider_data().and_then(Value::as_object);
    let recorded_field = |name: &str| {
        recorded
            .and_then(|data| data.get(name))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let summary = recorded_field("reasoning_content").or_else(|| {
        let joined = reasoning.summary().join("\n");
        (!joined.is_empty()).then_some(joined)
    });
    let pending = PendingReasoning {
        summary,
        body: recorded_field("reasoning"),
    };
    (!pending.is_empty()).then_some(pending)
}

/// Recovers the thinking-block sequence to resend, preferring the provider's own copy.
fn replayable_thinking_blocks(reasoning: &Reasoning) -> Option<PendingThinking> {
    if let Some(blocks) = reasoning
        .provider_data()
        .and_then(|data| data.get("thinking_blocks"))
        .and_then(Value::as_array)
        .filter(|blocks| !blocks.is_empty() && blocks.iter().all(Value::is_object))
    {
        return Some(PendingThinking {
            blocks: blocks.clone(),
            native: true,
        });
    }
    if reasoning.content().is_empty() {
        return None;
    }
    // A record written before provider payloads were retained keeps only normalized text and a
    // newline-joined signature list, so the blocks are rebuilt in order and the signatures dealt
    // back out one per block.
    let mut signatures = reasoning
        .encrypted_content()
        .map(|encrypted| {
            encrypted
                .split('\n')
                .map(str::to_owned)
                .collect::<VecDeque<_>>()
        })
        .unwrap_or_default();
    let blocks = reasoning
        .content()
        .iter()
        .map(|text| {
            let mut block = json!({"type": "thinking", "thinking": text});
            if let Some(signature) = signatures.pop_front() {
                block["signature"] = Value::String(signature);
            }
            block
        })
        .collect();
    Some(PendingThinking {
        blocks,
        native: false,
    })
}

// ---------------------------------------------------------------------------------------------
// Lifting: a completed Chat response into neutral items
// ---------------------------------------------------------------------------------------------

/// Lifts a completed chat completion into provider-neutral run items.
///
/// The wire payload is not returned alongside: every lifted item already carries the fragment it
/// came from, so a second whole-body copy would only be a second place for the two to disagree.
pub(crate) fn convert_completion(
    codec: &ChatCodec,
    payload: &Value,
    request_id: Option<String>,
    handoffs: &[ModelHandoffDefinition],
    provider: &ProviderKey,
) -> Result<ModelResponse> {
    if let Some(error) = payload.get("error").filter(|value| !value.is_null()) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("OpenAI chat completion contained an error");
        return Err(behavior_error(message));
    }
    let completion_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(FAKE_ITEM_ID);
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| behavior_error("OpenAI chat completion.choices must be an array"))?;
    if choices.len() > 1 {
        codec.degrade(
            "OpenAI Chat Completions returned several choices; only the first can be turned into \
             run items",
        )?;
    }
    if choices.is_empty() {
        return Err(behavior_error("OpenAI chat completion returned no choices"));
    }
    let choice = primary_choice(choices).ok_or_else(|| {
        behavior_error("OpenAI chat completion returned choices but none at index 0")
    })?;
    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
    reject_unfinished_choice(finish_reason)?;
    let message = choice
        .get("message")
        .ok_or_else(|| behavior_error("OpenAI chat completion choice has no message"))?;

    let items = lift_message(
        codec,
        message,
        finish_reason,
        completion_id,
        handoffs,
        provider,
    )?;
    let mut response = ModelResponse::new(items).with_usage(convert_usage(payload.get("usage")));
    if let Some(request_id) = request_id {
        response = response.with_request_id(request_id);
    }
    // `response_id` stays empty on purpose. A `chatcmpl-` identifier cannot be continued from, and
    // filling the field would advertise a server-side conversation this protocol does not have.
    Ok(response)
}

/// Selects the choice this adapter answers for.
///
/// By `index`, never by array position. The protocol numbers choices explicitly and nowhere
/// promises the array is ordered, so reading position instead would answer with a different
/// choice than the streaming path picks for the same response — and the two paths are supposed to
/// be interchangeable. A choice carrying no `index` is read as index 0, which is what a provider
/// sending one unnumbered choice means.
pub(crate) fn primary_choice(choices: &[Value]) -> Option<&Value> {
    choices
        .iter()
        .find(|choice| choice.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
}

/// Rejects a terminal state that is not a finished answer.
///
/// A truncated completion carries a well-formed message, so lifting it silently hands the runner
/// half a sentence as if the model had finished. The reference implementation returns it; this
/// adapter classifies it, matching the sibling Responses adapter, which refuses the identical
/// `incomplete / max_output_tokens` state for the identical reason.
///
/// Shared with the streaming path, which reaches the same terminal reasons by a different route.
/// A turn stopped at the token limit is not an answer, and which entry point the caller happened
/// to use has no bearing on that.
pub(crate) fn reject_unfinished_choice(finish_reason: Option<&str>) -> Result<()> {
    if finish_reason == Some("length") {
        return Err(Error::provider(
            ProviderErrorKind::ContextOverflow,
            "OpenAI chat completion stopped at the output token limit",
        ));
    }
    Ok(())
}

fn lift_message(
    codec: &ChatCodec,
    message: &Value,
    finish_reason: Option<&str>,
    completion_id: &str,
    handoffs: &[ModelHandoffDefinition],
    provider: &ProviderKey,
) -> Result<Vec<RunItem>> {
    let text = non_empty(message.get("content"));
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    // Some providers signal a withheld turn only through the finish reason and an otherwise empty
    // message. Preserving it as a refusal keeps the mechanical signal model fallback escalates on;
    // an empty turn would read as the model having nothing to say.
    let refusal = non_empty(message.get("refusal")).or_else(|| {
        (finish_reason == Some("content_filter") && text.is_none() && tool_calls.is_empty())
            .then_some(CONTENT_FILTER_REFUSAL)
    });

    let mut items = Vec::new();
    let mut next = ItemIdCounter::new(completion_id);
    if let Some(reasoning) = lift_reasoning(codec, message, completion_id) {
        items.push(next.item(RunItemKind::Reasoning(reasoning), message, provider));
    }
    if text.is_some() || refusal.is_some() {
        let mut content = Vec::new();
        if let Some(text) = text {
            content.push(ContentBlock::text(text));
        }
        if let Some(refusal) = refusal {
            content.push(ContentBlock::refusal(refusal));
        }
        let assistant = Message::new(MessageRole::Assistant, content);
        items.push(next.item(RunItemKind::Message(assistant), message, provider));
    }
    for call in tool_calls {
        let Some(kind) = lift_tool_call(codec, call, handoffs)? else {
            continue;
        };
        items.push(next.item(kind, call, provider));
    }
    Ok(items)
}

/// Lifts one `tool_calls` entry, or `None` when it is a shape this protocol cannot execute.
fn lift_tool_call(
    codec: &ChatCodec,
    call: &Value,
    handoffs: &[ModelHandoffDefinition],
) -> Result<Option<RunItemKind>> {
    let call_type = call
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if call_type != "function" {
        codec.degrade(&format!(
            "OpenAI Chat Completions returned a `{call_type}` tool call, which has no neutral \
             representation"
        ))?;
        return Ok(None);
    }
    let call_id = required_str(call, "id", "tool_call")?;
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .ok_or_else(|| behavior_error("OpenAI tool_call.function.name must be a string"))?;
    let arguments = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    tool_call_item(CallId::new(call_id), name, arguments, handoffs).map(Some)
}

/// Turns a wire tool call into the item kind it denotes, resolving handoffs by advertised name.
pub(crate) fn tool_call_item(
    call_id: CallId,
    name: &str,
    arguments: &str,
    handoffs: &[ModelHandoffDefinition],
) -> Result<RunItemKind> {
    let arguments = if arguments.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(arguments).map_err(|error| {
            behavior_error(format!(
                "OpenAI function `{name}` returned invalid JSON arguments"
            ))
            .with_source(error)
        })?
    };
    match handoffs.iter().find(|handoff| handoff.name() == name) {
        Some(handoff) => Ok(RunItemKind::HandoffCall(
            HandoffCall::new(call_id, handoff.target_agent().clone(), arguments)
                .with_tool_name(name),
        )),
        None => Ok(RunItemKind::ToolCall(ToolCall::new(
            call_id, name, arguments,
        ))),
    }
}

/// Rebuilds a reasoning item from whatever reasoning surface the endpoint added.
///
/// The provider payload is retained whole as the replay source of truth; the normalized text and
/// signature fields below are derived from it and cannot express an empty or redacted block.
///
/// The item's `id` is deliberately left unset. Chat Completions assigns none, and a sentinel
/// written here would travel into a later Responses request as if it were a real provider
/// identifier.
pub(crate) fn lift_reasoning(
    codec: &ChatCodec,
    message: &Value,
    completion_id: &str,
) -> Option<Reasoning> {
    let declared = codec.quirks.reasoning_content();
    // Both spellings are gateway additions rather than protocol fields, so both wait on the same
    // declaration — and the streaming path reads both, which is the only reason this one does too.
    // A response that yields a reasoning item down one entry point has to yield it down the other.
    let summary = declared
        .then(|| non_empty(message.get("reasoning_content")))
        .flatten();
    let body = declared
        .then(|| non_empty(message.get("reasoning")))
        .flatten();
    let thinking_blocks = message
        .get("thinking_blocks")
        .and_then(Value::as_array)
        .filter(|blocks| !blocks.is_empty());
    if summary.is_none() && body.is_none() && thinking_blocks.is_none() {
        return None;
    }

    let mut provider_data = reasoning_provider_data(&codec.model, completion_id, summary, body);
    let mut reasoning = Reasoning::new();
    if let Some(text) = summary {
        reasoning = reasoning.with_summary(vec![text.to_owned()]);
    }
    let mut content = body.map(str::to_owned).into_iter().collect::<Vec<_>>();
    if let Some(blocks) = thinking_blocks {
        provider_data.insert("thinking_blocks".to_owned(), Value::Array(blocks.clone()));
        content.extend(
            blocks
                .iter()
                .filter_map(|block| non_empty(block.get("thinking")).map(str::to_owned)),
        );
        let signatures = blocks
            .iter()
            .filter_map(|block| non_empty(block.get("signature")))
            .collect::<Vec<_>>();
        if !signatures.is_empty() {
            reasoning = reasoning.with_encrypted_content(signatures.join("\n"));
        }
    }
    if !content.is_empty() {
        reasoning = reasoning.with_content(content);
    }
    Some(reasoning.with_provider_data(Value::Object(provider_data)))
}

/// Records which model produced a reasoning item, so a later turn can refuse to replay it
/// somewhere else.
pub(crate) fn reasoning_provider_data(
    model: &str,
    completion_id: &str,
    summary: Option<&str>,
    body: Option<&str>,
) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("model".to_owned(), Value::String(model.to_owned()));
    data.insert(
        "response_id".to_owned(),
        Value::String(completion_id.to_owned()),
    );
    // Which spelling arrived is itself replay material. The normalized `summary` and `content`
    // views cannot say whether text came back as `reasoning_content` or as `reasoning`, and a
    // gateway wants the same field it sent — so the wire names are recorded here, where this type
    // documents the replay source of truth to live.
    if let Some(summary) = summary {
        data.insert(
            "reasoning_content".to_owned(),
            Value::String(summary.to_owned()),
        );
    }
    if let Some(body) = body {
        data.insert("reasoning".to_owned(), Value::String(body.to_owned()));
    }
    data
}

/// Hands out deterministic identifiers for the items lifted out of one completion.
pub(crate) struct ItemIdCounter<'a> {
    completion_id: &'a str,
    index: usize,
}

impl<'a> ItemIdCounter<'a> {
    pub(crate) const fn new(completion_id: &'a str) -> Self {
        Self {
            completion_id,
            index: 0,
        }
    }

    /// Wraps one payload with the next identifier and the untouched provider fragment.
    pub(crate) fn item(
        &mut self,
        kind: RunItemKind,
        raw: &Value,
        provider: &ProviderKey,
    ) -> RunItem {
        let id = ItemId::new(format!(
            "{}:{}:{}",
            self.completion_id,
            self.index,
            kind.label()
        ));
        self.index += 1;
        RunItem::new(id, kind)
            .with_raw_provider_item(RawProviderItem::new(provider.as_str(), raw.clone()))
    }
}

pub(crate) fn convert_usage(value: Option<&Value>) -> Usage {
    let field = |name: &str| {
        value
            .and_then(|usage| usage.get(name))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let detail = |pointer: &str| {
        value
            .and_then(|usage| usage.pointer(pointer))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    // One call, one entry, whether or not the endpoint sent a usage block; see the Responses side
    // for why an unreported cost is still a request that happened.
    Usage::from_request(
        RequestUsage::new(field("prompt_tokens"), field("completion_tokens"))
            .with_cached_input_tokens(detail("/prompt_tokens_details/cached_tokens"))
            .with_cache_write_tokens(detail("/prompt_tokens_details/cache_write_tokens"))
            .with_reasoning_tokens(detail("/completion_tokens_details/reasoning_tokens")),
    )
}

/// Reads a string field, treating an empty one as absent.
fn non_empty(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn required_str<'a>(value: &'a Value, key: &str, owner: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| behavior_error(format!("OpenAI {owner}.{key} must be a string")))
}
