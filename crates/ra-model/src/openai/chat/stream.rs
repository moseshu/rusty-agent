//! Reassemble Chat Completions deltas into a Responses-shaped event stream.
//!
//! # Why the events are not chat-shaped
//!
//! A Chat delta says almost nothing about where it belongs: it carries no item identifier, no
//! output index, and no sequence number, and a tool call arrives as fragments that have to be
//! matched by array position. Handing those to consumers unchanged means every UI, trace writer
//! and terminal-backfill path is written twice, once per `OpenAI` protocol.
//!
//! So this module pays the reassembly cost once and emits the richer vocabulary: item added,
//! content part added, text and argument deltas, item done, response completed — each with an
//! output index this module computes and a monotonic sequence number. Normalized items are emitted
//! alongside as they complete, which is the channel a protocol-neutral consumer reads.
//!
//! The five hard parts, each of which is a real failure this handler exists to prevent: tool-call
//! fragments arrive split and possibly interleaved; the output index has to be derived rather than
//! read; sequence numbers must be monotonic across every emitted event; a thinking block's text
//! and its signature arrive in different deltas; and some deltas carry only provider-private
//! fields yet still must not be dropped.
//!
//! # The synthesized event vocabulary is not frozen
//!
//! Names like `response.output_text.delta`, and the field layout inside those payloads, travel
//! inside a provider-isolated raw event with no stability promise. They are **not** a public
//! contract yet. The streaming milestone owns defining the run-level event channel and the
//! partial-message switch, and it should feel free to rename or restructure what this module
//! emits: the tests here assert the reassembly rules, not the spelling.
//!
//! Read the usage totals assembled at the end the same way. This module fills them in because a
//! stream that reports nothing cannot be reconciled against anything, but per-request usage
//! accounting is a later milestone's contract and may reshape what is recorded here.

use std::collections::VecDeque;

use futures::{Stream, StreamExt, stream, stream::BoxStream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{
        CallId, ContentBlock, ItemId, Message, MessageRole, RawProviderItem, Reasoning, RunItem,
        RunItemKind,
    },
    model::{
        ModelHandoffDefinition, ModelStream, ModelStreamEvent, ProviderKey, RawResponseEvent,
        RunItemStreamEvent,
    },
    usage::Usage,
};
use serde_json::{Map, Value, json};

use super::{ChatCodec, FAKE_ITEM_ID, convert};
use crate::openai::{error::behavior_error, sse::SseFrame};

/// What a provider reports when it withholds a turn without saying anything else.
const CONTENT_FILTER_REFUSAL: &str = "Response withheld by the provider's content filter.";

/// Turns a stream of chat-completion chunks into model stream events.
pub(crate) fn events(
    codec: ChatCodec,
    frames: impl Stream<Item = Result<SseFrame>> + Send + 'static,
    provider: ProviderKey,
    handoffs: Vec<ModelHandoffDefinition>,
    buffer_tool_calls: bool,
) -> ModelStream<'static> {
    let driver = StreamDriver {
        frames: frames.boxed(),
        codec,
        provider,
        handoffs,
        state: StreamingState::default(),
        layout: OutputLayout::default(),
        sequence: 0,
        pending: VecDeque::new(),
        buffered: buffer_tool_calls.then(ToolCallBuffer::default),
        finished: false,
    };
    stream::unfold(driver, |mut driver| async move {
        driver.next_event().await.map(|event| (event, driver))
    })
    .boxed()
}

// ---------------------------------------------------------------------------------------------
// Accumulated state
// ---------------------------------------------------------------------------------------------

/// One streamed function tool call, rebuilt from its fragments.
#[derive(Default)]
struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Reasoning text accumulated across deltas.
#[derive(Default)]
struct ReasoningDraft {
    /// Text from `reasoning_content`, which reads as a summary.
    summary: Vec<String>,
    /// Text from `reasoning`, which reads as the reasoning body.
    ///
    /// Held apart from the thinking text below rather than appended to one list. Which wire field
    /// produced a segment decides what a later turn may resend it as, and a single list cannot
    /// answer that once the two are concatenated.
    body: Option<String>,
    /// Text folded out of the thinking-block sequence when the stream settles.
    thinking_text: Option<String>,
}

impl ReasoningDraft {
    /// The normalized reasoning text, body first, as consumers and later turns read it.
    fn content(&self) -> Vec<String> {
        self.body
            .iter()
            .chain(self.thinking_text.iter())
            .cloned()
            .collect()
    }
}

// Each flag records an independent thing the stream has seen, and folding them into enums would
// invent state transitions the wire format does not have: a turn can be started, content-filtered
// and finished with reasoning all at once.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct StreamingState {
    started: bool,
    completion_id: Option<String>,
    text: Option<(usize, String)>,
    refusal: Option<(usize, String)>,
    reasoning: Option<ReasoningDraft>,
    active_summary: Option<usize>,
    reasoning_done: bool,
    /// Insertion-ordered rather than keyed: the order the provider first mentioned a tool call is
    /// the order its output slot is assigned, and a sorted map would silently reorder calls whose
    /// indexes arrive out of sequence.
    function_calls: Vec<(u64, FunctionCall)>,
    /// Indexes already announced to consumers, so a later fragment does not re-announce them.
    announced_calls: Vec<u64>,
    ignored_calls: Vec<u64>,
    /// The ordered provider thinking-block sequence, kept whole for replay.
    thinking_blocks: Vec<Value>,
    saw_content_filter: bool,
    warned_choices: bool,
    /// Whether the sender said the stream was complete, either way it is allowed to say so.
    saw_done: bool,
    /// The terminal reason itself, not merely that one arrived: `length` means the turn was cut
    /// off, and settling that as an answer is the failure the non-streaming path already refuses.
    finish_reason: Option<String>,
    usage: Option<Value>,
}

impl StreamingState {
    fn call_position(&self, index: u64) -> Option<usize> {
        self.function_calls
            .iter()
            .position(|(existing, _)| *existing == index)
    }

    fn completion_id(&self) -> &str {
        self.completion_id.as_deref().unwrap_or(FAKE_ITEM_ID)
    }

    /// Folds one streamed thinking delta into the ordered block sequence.
    ///
    /// A thinking block arrives as a run of text deltas terminated by a single signature delta, so
    /// a signature both belongs to the block being accumulated and marks its end. A redacted block
    /// arrives whole and is kept verbatim, because it carries neither text nor signature and the
    /// normalized fields have no way to say so.
    fn accumulate_thinking(&mut self, block: &Value) -> bool {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("thinking");
        if block_type == "redacted_thinking" {
            self.thinking_blocks.push(block.clone());
            return true;
        }
        if block_type != "thinking" {
            return false;
        }
        let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
        let signature = block.get("signature").and_then(Value::as_str).unwrap_or("");
        if thinking.is_empty() && signature.is_empty() {
            return false;
        }
        let position = self.open_thinking_block();
        let Some(current) = self.thinking_blocks[position].as_object_mut() else {
            return false;
        };
        if !thinking.is_empty() {
            let existing = current
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            current.insert(
                "thinking".to_owned(),
                Value::String(format!("{existing}{thinking}")),
            );
        }
        if !signature.is_empty() {
            // A signature closes the block, so later text starts a new one.
            current.insert("signature".to_owned(), Value::String(signature.to_owned()));
        }
        true
    }

    /// Returns the block still accepting deltas, opening one when needed.
    fn open_thinking_block(&mut self) -> usize {
        let reusable = self.thinking_blocks.last().is_some_and(|block| {
            block.get("type").and_then(Value::as_str) == Some("thinking")
                && block.get("signature").is_none()
        });
        if !reusable {
            self.thinking_blocks
                .push(json!({"type": "thinking", "thinking": ""}));
        }
        self.thinking_blocks.len() - 1
    }
}

/// Output slots already exposed to consumers.
///
/// Chat deltas carry no output index, so it is derived here: reasoning occupies slot 0 when it
/// exists, and the assistant message lands after whichever tool calls were announced before it.
/// Once a slot is handed out it is never recomputed, because a consumer has already keyed its
/// rendering on it.
#[derive(Default)]
struct OutputLayout {
    assistant: Option<usize>,
    functions: Vec<(u64, usize)>,
}

impl OutputLayout {
    const fn reasoning_slots(state: &StreamingState) -> usize {
        if state.reasoning.is_some() { 1 } else { 0 }
    }

    fn assistant_index(&mut self, state: &StreamingState) -> usize {
        *self.assistant.get_or_insert_with(|| {
            let mut index = Self::reasoning_slots(state);
            if !self.functions.is_empty() {
                index += state.function_calls.len();
            }
            index
        })
    }

    fn function_index(&mut self, state: &StreamingState, call_index: u64) -> Result<usize> {
        if let Some((_, slot)) = self
            .functions
            .iter()
            .find(|(existing, _)| *existing == call_index)
        {
            return Ok(*slot);
        }
        let offset = state.call_position(call_index).ok_or_else(|| {
            behavior_error(format!(
                "OpenAI stream referenced tool call index {call_index} before announcing it"
            ))
        })?;
        let reasoning = Self::reasoning_slots(state);
        let slot = match self.assistant {
            None => reasoning + offset,
            Some(assistant) => {
                let before_message = assistant.saturating_sub(reasoning);
                if offset < before_message {
                    reasoning + offset
                } else {
                    reasoning + offset + 1
                }
            }
        };
        self.functions.push((call_index, slot));
        Ok(slot)
    }

    /// How many tool calls were announced before the assistant message claimed its slot.
    fn calls_before_message(&self, state: &StreamingState) -> usize {
        self.assistant.map_or(0, |assistant| {
            assistant.saturating_sub(Self::reasoning_slots(state))
        })
    }
}

/// Tool-call fragments held back until each call is complete.
#[derive(Default)]
struct ToolCallBuffer {
    calls: Vec<(u64, FunctionCall)>,
    /// Indexes forwarded unchanged because they are not function calls.
    passthrough: Vec<u64>,
    saw_passthrough: bool,
}

// ---------------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------------

struct StreamDriver {
    frames: BoxStream<'static, Result<SseFrame>>,
    codec: ChatCodec,
    provider: ProviderKey,
    handoffs: Vec<ModelHandoffDefinition>,
    state: StreamingState,
    layout: OutputLayout,
    sequence: u64,
    pending: VecDeque<Result<ModelStreamEvent>>,
    buffered: Option<ToolCallBuffer>,
    finished: bool,
}

impl StreamDriver {
    async fn next_event(&mut self) -> Option<Result<ModelStreamEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            match self.frames.next().await {
                Some(Ok(SseFrame::Data(chunk))) => {
                    if let Err(error) = self.process_chunk(&chunk) {
                        self.finished = true;
                        return Some(Err(error));
                    }
                }
                Some(Ok(SseFrame::Done)) => self.state.saw_done = true,
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                None => {
                    self.finished = true;
                    // Settle only against evidence that the sender finished. Reaching the end of
                    // the body is not that evidence, and treating it as such is what turns a cut
                    // connection into a confident, empty answer.
                    if let Some(error) = self.truncation_error() {
                        return Some(Err(error));
                    }
                    // The same terminal states the non-streaming path refuses. A turn stopped at
                    // the token limit carries a well-formed half sentence either way, and which
                    // entry point the caller used cannot decide whether it counts as an answer.
                    if let Err(error) =
                        convert::reject_unfinished_choice(self.state.finish_reason.as_deref())
                    {
                        return Some(Err(error));
                    }
                    if let Err(error) = self.finalize() {
                        return Some(Err(error));
                    }
                }
            }
        }
    }

    /// Reports a stream that stopped without the sender ever saying it was finished.
    ///
    /// A Chat stream ends in one of two ways that mean "complete": the `[DONE]` terminator, or a
    /// terminal `finish_reason` on the choice. Either is accepted, because gateways omit one or
    /// the other; neither is not. Without one of them the bytes simply ran out, which happens when
    /// a proxy drops the connection after a chunk it already forwarded — and settling that into a
    /// `response.completed` hands the caller a partial turn wearing a success.
    ///
    /// An endpoint declared to send no terminator at all gets a third way, and it costs exactly
    /// what this check was defending: for that endpoint a cut connection and a finished turn are
    /// the same bytes, and this settles both. It still refuses a stream that never delivered a
    /// chunk, because that is the failure worth keeping closed — an empty answer delivered
    /// confidently reads as a model with nothing to say rather than as a connection that never
    /// carried one.
    ///
    /// The two messages are deliberately different. Whether anything was already emitted decides
    /// whether the request can be replayed at all, and that is the one fact a retry policy above
    /// cannot recover once this error is built.
    fn truncation_error(&self) -> Option<Error> {
        if self.state.saw_done || self.state.finish_reason.is_some() {
            return None;
        }
        if self.codec.terminator.end_of_body_is_terminal() && self.state.started {
            return None;
        }
        let message = if self.state.started {
            "OpenAI stream ended after partial output, without `[DONE]` or a finish reason; the \
             turn is incomplete and its emitted events cannot be replayed transparently"
        } else {
            "OpenAI stream closed without sending an event, `[DONE]`, or a finish reason"
        };
        Some(Error::provider(ProviderErrorKind::Network, message))
    }

    /// Every emitted event shares one counter, so a consumer can order events it received on
    /// different channels without inspecting their contents.
    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }

    fn emit(&mut self, event_type: &str, mut payload: Value) {
        let sequence = self.next_sequence();
        if let Some(object) = payload.as_object_mut() {
            object.insert("type".to_owned(), Value::String(event_type.to_owned()));
            object.insert("sequence_number".to_owned(), json!(sequence));
        }
        self.pending
            .push_back(Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                self.provider.clone(),
                event_type,
                payload,
            ))));
    }

    /// Publishes a completed item on the normalized channel.
    fn emit_item(&mut self, kind: RunItemKind, output_index: usize, raw: Value) {
        let id = ItemId::new(format!(
            "{}:{}:{}",
            self.state.completion_id(),
            output_index,
            kind.label()
        ));
        let name = kind.label();
        let item = RunItem::new(id, kind)
            .with_raw_provider_item(RawProviderItem::new(self.provider.as_str(), raw));
        self.pending
            .push_back(Ok(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
                name, item,
            ))));
    }

    fn process_chunk(&mut self, chunk: &Value) -> Result<()> {
        if !self.state.started {
            self.state.started = true;
            self.emit(
                "response.created",
                json!({"response": {"status": "in_progress"}}),
            );
        }
        if let Some(id) = chunk.get("id").and_then(Value::as_str)
            && self.state.completion_id.is_none()
        {
            self.state.completion_id = Some(id.to_owned());
        }
        // Usage does not always ride the last chunk, so it is captured wherever it appears.
        if let Some(usage) = chunk.get("usage").filter(|value| !value.is_null()) {
            self.state.usage = Some(usage.clone());
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return Ok(());
        };
        if choices.len() > 1 && !self.state.warned_choices {
            self.state.warned_choices = true;
            self.codec.degrade(
                "OpenAI Chat Completions streamed several choices; only the first is processed",
            )?;
        }
        let Some(choice) = convert::primary_choice(choices) else {
            return Ok(());
        };
        let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
        if let Some(reason) = finish_reason {
            self.state.finish_reason = Some(reason.to_owned());
        }
        if finish_reason == Some("content_filter") {
            self.state.saw_content_filter = true;
        }
        let Some(delta) = choice.get("delta").filter(|value| value.is_object()) else {
            return Ok(());
        };
        for delta in self.intercept_tool_calls(delta, finish_reason)? {
            self.process_delta(&delta)?;
        }
        Ok(())
    }

    /// Holds tool-call fragments back until the call is complete, when buffering is enabled.
    ///
    /// Fragments for one call are split across chunks and, on some endpoints, interleaved with
    /// fragments of another. A consumer that acts on a partial argument string acts on invalid
    /// JSON, so buffering trades incremental rendering for a call that is always well formed.
    /// Deltas that carry other output pass through untouched — dropping them would lose text the
    /// model already produced.
    ///
    /// Completed calls come back as a **second delta** rather than written over the first. The
    /// delta that releases them may itself be carrying a call this adapter does not buffer — a
    /// custom tool call, which is a valid response shape — and overwriting the array would delete
    /// it before anything could reject it, turning a refusal into silence. The reference
    /// implementation keeps the two apart the same way, by emitting the completed calls as a chunk
    /// of their own.
    fn intercept_tool_calls(
        &mut self,
        delta: &Value,
        finish_reason: Option<&str>,
    ) -> Result<Vec<Value>> {
        if self.buffered.is_none() {
            return Ok(vec![delta.clone()]);
        }
        let mut remaining = Vec::new();
        for fragment in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0);
            let buffer = self.buffered.get_or_insert_with(ToolCallBuffer::default);
            if buffer.passthrough.contains(&index) || !should_buffer(fragment) {
                if !buffer.passthrough.contains(&index) {
                    buffer.passthrough.push(index);
                }
                buffer.saw_passthrough = true;
                remaining.push(fragment.clone());
            } else {
                accumulate_fragment(&mut buffer.calls, index, fragment);
            }
        }

        let mut passthrough = delta.clone();
        if let Some(object) = passthrough.as_object_mut() {
            if remaining.is_empty() {
                object.remove("tool_calls");
            } else {
                object.insert("tool_calls".to_owned(), Value::Array(remaining));
            }
        }
        let carries_output = has_passthrough_output(&passthrough);
        let mut deltas = vec![passthrough];
        if finished_tool_calls(finish_reason)
            && let Some(released) = self.release_buffered_calls(carries_output)?
        {
            deltas.push(released);
        }
        Ok(deltas)
    }

    /// Turns the held fragments into one delta carrying the completed calls.
    ///
    /// `carries_output` says whether the delta that triggered the release already had output of
    /// its own. That is what separates "the model asked for tools and they were buffered" from
    /// "the endpoint claimed a tool-call ending and sent no tool call at all".
    fn release_buffered_calls(&mut self, carries_output: bool) -> Result<Option<Value>> {
        let Some(buffer) = self.buffered.as_mut() else {
            return Ok(None);
        };
        if buffer.calls.is_empty() {
            if !buffer.saw_passthrough && !carries_output {
                return Err(behavior_error(
                    "OpenAI stream finished with finish_reason `tool_calls` but sent no tool call \
                     fragments",
                ));
            }
            return Ok(None);
        }
        let mut released = Vec::with_capacity(buffer.calls.len());
        for (index, call) in std::mem::take(&mut buffer.calls) {
            if call.call_id.is_empty() {
                return Err(behavior_error(
                    "buffered OpenAI tool call ended without a tool call id",
                ));
            }
            if call.name.is_empty() {
                return Err(behavior_error(
                    "buffered OpenAI tool call ended without a function name",
                ));
            }
            released.push(json!({
                "index": index,
                "id": call.call_id,
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments}
            }));
        }
        Ok(Some(json!({"tool_calls": released})))
    }

    fn process_delta(&mut self, delta: &Value) -> Result<()> {
        self.process_thinking_blocks(delta);
        self.process_reasoning_content(delta);
        self.process_reasoning_text(delta);
        self.close_summary_part_if_superseded(delta);
        self.process_content(delta);
        self.process_refusal(delta);
        self.process_tool_calls(delta)
    }

    fn process_thinking_blocks(&mut self, delta: &Value) {
        let blocks = delta
            .get("thinking_blocks")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .to_vec();
        let mut accumulated = false;
        for block in &blocks {
            accumulated |= self.state.accumulate_thinking(block);
        }
        if accumulated {
            self.open_reasoning_item();
        }
    }

    fn process_reasoning_content(&mut self, delta: &Value) {
        // Neither of the reasoning fields is part of Chat Completions; both are gateway additions,
        // so both wait on the same declaration the non-streaming path checks. Without this the
        // same response yields a reasoning item down one entry point and not the other.
        if !self.codec.quirks.reasoning_content() {
            return;
        }
        let Some(text) = non_empty(delta.get("reasoning_content")) else {
            return;
        };
        let text = text.to_owned();
        self.open_reasoning_item();
        if self.state.active_summary.is_none() {
            let summary_index = self
                .state
                .reasoning
                .as_ref()
                .map_or(0, |reasoning| reasoning.summary.len());
            if let Some(reasoning) = self.state.reasoning.as_mut() {
                reasoning.summary.push(String::new());
            }
            self.state.active_summary = Some(summary_index);
            self.emit(
                "response.reasoning_summary_part.added",
                json!({
                    "item_id": FAKE_ITEM_ID,
                    "output_index": 0,
                    "summary_index": summary_index,
                    "part": {"type": "summary_text", "text": ""}
                }),
            );
        }
        let summary_index = self.state.active_summary.unwrap_or(0);
        self.emit(
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": FAKE_ITEM_ID,
                "output_index": 0,
                "summary_index": summary_index,
                "delta": text
            }),
        );
        if let Some(reasoning) = self.state.reasoning.as_mut()
            && let Some(segment) = reasoning.summary.get_mut(summary_index)
        {
            segment.push_str(&text);
        }
    }

    fn process_reasoning_text(&mut self, delta: &Value) {
        if !self.codec.quirks.reasoning_content() {
            return;
        }
        let Some(text) = non_empty(delta.get("reasoning")) else {
            return;
        };
        let text = text.to_owned();
        self.open_reasoning_item();
        self.emit(
            "response.reasoning_text.delta",
            json!({
                "item_id": FAKE_ITEM_ID,
                "output_index": 0,
                "content_index": 0,
                "delta": text
            }),
        );
        if let Some(reasoning) = self.state.reasoning.as_mut() {
            reasoning.body.get_or_insert_default().push_str(&text);
        }
    }

    /// Closes the open summary part once the turn moves on to visible output.
    fn close_summary_part_if_superseded(&mut self, delta: &Value) {
        let moved_on = delta.get("content").is_some_and(|value| !value.is_null())
            || non_empty(delta.get("refusal")).is_some()
            || delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty());
        if moved_on && non_empty(delta.get("reasoning_content")).is_none() {
            self.finish_summary_part();
        }
    }

    fn process_content(&mut self, delta: &Value) {
        let Some(content) = delta.get("content").and_then(Value::as_str) else {
            return;
        };
        // An empty leading content delta is dropped rather than opening a text part: materializing
        // an empty part adds a text block to the completed response that the model never wrote,
        // and a withheld turn emits exactly this warm-up chunk before its terminal content filter.
        if self.state.text.is_none() && content.is_empty() {
            return;
        }
        let content = content.to_owned();
        if self.state.text.is_none() {
            let content_index = usize::from(self.state.refusal.is_some());
            self.state.text = Some((content_index, String::new()));
            let output_index = self.layout.assistant_index(&self.state);
            self.emit(
                "response.output_item.added",
                json!({"output_index": output_index, "item": in_progress_message()}),
            );
            self.emit(
                "response.content_part.added",
                json!({
                    "content_index": content_index,
                    "item_id": FAKE_ITEM_ID,
                    "output_index": output_index,
                    "part": {"type": "output_text", "text": "", "annotations": []}
                }),
            );
        }
        let content_index = self.state.text.as_ref().map_or(0, |(index, _)| *index);
        let output_index = self.layout.assistant_index(&self.state);
        self.emit(
            "response.output_text.delta",
            json!({
                "content_index": content_index,
                "item_id": FAKE_ITEM_ID,
                "output_index": output_index,
                "delta": content
            }),
        );
        if let Some((_, accumulated)) = self.state.text.as_mut() {
            accumulated.push_str(&content);
        }
    }

    fn process_refusal(&mut self, delta: &Value) {
        let Some(refusal) = non_empty(delta.get("refusal")) else {
            return;
        };
        let refusal = refusal.to_owned();
        if self.state.refusal.is_none() {
            let content_index = usize::from(self.state.text.is_some());
            self.state.refusal = Some((content_index, String::new()));
            let output_index = self.layout.assistant_index(&self.state);
            self.emit(
                "response.output_item.added",
                json!({"output_index": output_index, "item": in_progress_message()}),
            );
            self.emit(
                "response.content_part.added",
                json!({
                    "content_index": content_index,
                    "item_id": FAKE_ITEM_ID,
                    "output_index": output_index,
                    "part": {"type": "refusal", "refusal": ""}
                }),
            );
        }
        let content_index = self.state.refusal.as_ref().map_or(0, |(index, _)| *index);
        let output_index = self.layout.assistant_index(&self.state);
        self.emit(
            "response.refusal.delta",
            json!({
                "content_index": content_index,
                "item_id": FAKE_ITEM_ID,
                "output_index": output_index,
                "delta": refusal
            }),
        );
        if let Some((_, accumulated)) = self.state.refusal.as_mut() {
            accumulated.push_str(&refusal);
        }
    }

    fn process_tool_calls(&mut self, delta: &Value) -> Result<()> {
        let fragments = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .to_vec();
        for fragment in &fragments {
            let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0);
            if self.state.ignored_calls.contains(&index) {
                continue;
            }
            if !should_buffer(fragment) {
                self.codec.degrade(
                    "OpenAI Chat Completions streamed a custom tool call, which has no neutral \
                     representation",
                )?;
                self.state.ignored_calls.push(index);
                continue;
            }
            accumulate_fragment(&mut self.state.function_calls, index, fragment);
            let arguments = fragment
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            self.announce_or_stream_call(index, &arguments)?;
        }
        Ok(())
    }

    /// Announces a tool call as soon as it is identifiable, then streams its arguments.
    fn announce_or_stream_call(&mut self, index: u64, arguments: &str) -> Result<()> {
        let identified = self
            .state
            .call_position(index)
            .and_then(|position| self.state.function_calls.get(position))
            .is_some_and(|(_, call)| !call.name.is_empty() && !call.call_id.is_empty());
        if identified && !self.state.announced_calls.contains(&index) {
            self.state.announced_calls.push(index);
            let output_index = self.layout.function_index(&self.state, index)?;
            let item = self.function_call_item_with(index, Some(String::new()));
            self.emit(
                "response.output_item.added",
                json!({"output_index": output_index, "item": item}),
            );
        }
        if self.state.announced_calls.contains(&index) && !arguments.is_empty() {
            let output_index = self.layout.function_index(&self.state, index)?;
            self.emit(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": FAKE_ITEM_ID,
                    "output_index": output_index,
                    "delta": arguments
                }),
            );
        }
        Ok(())
    }

    fn open_reasoning_item(&mut self) {
        if self.state.reasoning.is_some() {
            return;
        }
        self.state.reasoning = Some(ReasoningDraft::default());
        self.emit(
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": {"id": FAKE_ITEM_ID, "type": "reasoning", "summary": []}
            }),
        );
    }

    fn finish_summary_part(&mut self) {
        let Some(summary_index) = self.state.active_summary else {
            return;
        };
        let text = self
            .state
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.summary.get(summary_index))
            .cloned();
        self.state.active_summary = None;
        let Some(text) = text else {
            return;
        };
        self.emit(
            "response.reasoning_summary_part.done",
            json!({
                "item_id": FAKE_ITEM_ID,
                "output_index": 0,
                "summary_index": summary_index,
                "part": {"type": "summary_text", "text": text}
            }),
        );
    }

    /// Renders one tool call in the Responses shape, optionally overriding its arguments.
    ///
    /// The override exists for the announcement event, which names the call before its arguments
    /// have arrived.
    fn function_call_item_with(&self, index: u64, arguments: Option<String>) -> Value {
        let (call_id, name, accumulated) = self
            .state
            .call_position(index)
            .and_then(|position| self.state.function_calls.get(position))
            .map_or_else(
                || (String::new(), String::new(), String::new()),
                |(_, call)| {
                    (
                        call.call_id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    )
                },
            );
        json!({
            "id": FAKE_ITEM_ID,
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments.unwrap_or(accumulated)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Terminal settlement
// ---------------------------------------------------------------------------------------------

impl StreamDriver {
    /// Closes every open part and publishes the completed items.
    ///
    /// Ordering matters and is not arbitrary: parts close before the items that contain them, and
    /// the completed response is last so that a consumer treating it as the end marker has already
    /// received everything it summarizes.
    fn finalize(&mut self) -> Result<()> {
        self.flush_buffered_calls()?;
        self.synthesize_withheld_refusal();
        self.finalize_thinking_blocks();
        self.finish_reasoning_item();
        self.finish_content_parts();

        let mut outputs = Vec::new();
        if let Some(reasoning) = self.reasoning_item_payload() {
            outputs.push(reasoning);
        }
        let before_message = self.layout.calls_before_message(&self.state);
        let call_indexes = self
            .state
            .function_calls
            .iter()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        for index in &call_indexes {
            self.finish_function_call(*index)?;
        }
        let message = self.finish_assistant_message();

        for index in call_indexes.iter().take(before_message) {
            outputs.push(self.function_call_item_with(*index, None));
        }
        if let Some(message) = message {
            outputs.push(message);
        }
        for index in call_indexes.iter().skip(before_message) {
            outputs.push(self.function_call_item_with(*index, None));
        }

        let usage = convert::convert_usage(self.state.usage.as_ref());
        self.emit(
            "response.completed",
            json!({
                "response": {
                    "id": self.state.completion_id(),
                    "object": "response",
                    "status": "completed",
                    "output": outputs,
                    "usage": responses_usage(&usage)
                }
            }),
        );
        Ok(())
    }

    /// Releases tool calls still held back when the stream ended without announcing why.
    ///
    /// A provider that closes the connection without a `tool_calls` finish reason has still asked
    /// for those tools. Dropping them because the terminator was missing would lose the entire
    /// point of the turn.
    fn flush_buffered_calls(&mut self) -> Result<()> {
        let held = self
            .buffered
            .as_ref()
            .is_some_and(|buffer| !buffer.calls.is_empty());
        if !held {
            return Ok(());
        }
        // The held calls are known to be non-empty here, so the "claimed tool calls and sent none"
        // check cannot fire and the flag it reads is irrelevant.
        if let Some(delta) = self.release_buffered_calls(true)? {
            self.process_delta(&delta)?;
        }
        Ok(())
    }

    /// Records a turn the provider withheld without saying anything else.
    ///
    /// A filtered turn can end with no output at all, and its warm-up empty content delta was
    /// suppressed so no text part opened. Announcing a fresh message and placing the refusal at
    /// content index 0 keeps the refusal a mechanical signal rather than an empty turn.
    fn synthesize_withheld_refusal(&mut self) {
        if !self.state.saw_content_filter
            || self.state.text.is_some()
            || self.state.refusal.is_some()
            || !self.state.function_calls.is_empty()
        {
            return;
        }
        self.state.refusal = Some((0, CONTENT_FILTER_REFUSAL.to_owned()));
        let output_index = self.layout.assistant_index(&self.state);
        self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": in_progress_message()}),
        );
        self.emit(
            "response.content_part.added",
            json!({
                "content_index": 0,
                "item_id": FAKE_ITEM_ID,
                "output_index": output_index,
                "part": {"type": "refusal", "refusal": ""}
            }),
        );
        self.emit(
            "response.refusal.delta",
            json!({
                "content_index": 0,
                "item_id": FAKE_ITEM_ID,
                "output_index": output_index,
                "delta": CONTENT_FILTER_REFUSAL
            }),
        );
    }

    /// Folds the accumulated thinking blocks into the reasoning item's normalized fields.
    fn finalize_thinking_blocks(&mut self) {
        if self.state.thinking_blocks.is_empty() {
            return;
        }
        let Some(reasoning) = self.state.reasoning.as_mut() else {
            return;
        };
        let text = self
            .state
            .thinking_blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
            .filter_map(|block| block.get("thinking").and_then(Value::as_str))
            .collect::<String>();
        if !text.is_empty() {
            reasoning.thinking_text = Some(text);
        }
    }

    fn finish_reasoning_item(&mut self) {
        if self.state.reasoning.is_none() || self.state.reasoning_done {
            return;
        }
        let has_summary = self
            .state
            .reasoning
            .as_ref()
            .is_some_and(|reasoning| !reasoning.summary.is_empty());
        if has_summary {
            self.finish_summary_part();
        } else if let Some(text) = self
            .state
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.content().first().cloned())
        {
            self.emit(
                "response.reasoning_text.done",
                json!({
                    "item_id": FAKE_ITEM_ID,
                    "output_index": 0,
                    "content_index": 0,
                    "text": text
                }),
            );
        }
        let Some(payload) = self.reasoning_item_payload() else {
            return;
        };
        self.emit(
            "response.output_item.done",
            json!({"output_index": 0, "item": payload.clone()}),
        );
        if let Some(reasoning) = self.neutral_reasoning() {
            self.emit_item(RunItemKind::Reasoning(reasoning), 0, payload);
        }
        self.state.reasoning_done = true;
    }

    fn finish_content_parts(&mut self) {
        // Asking for the slot is what assigns it, so a turn that opened no content part must not
        // ask: the message index would be reserved for a message that never exists, and every tool
        // call would be pushed one slot past it.
        if self.state.text.is_none() && self.state.refusal.is_none() {
            return;
        }
        let output_index = self.layout.assistant_index(&self.state);
        if let Some((content_index, text)) = self.state.text.clone() {
            self.emit(
                "response.content_part.done",
                json!({
                    "content_index": content_index,
                    "item_id": FAKE_ITEM_ID,
                    "output_index": output_index,
                    "part": {"type": "output_text", "text": text, "annotations": []}
                }),
            );
        }
        if let Some((content_index, refusal)) = self.state.refusal.clone() {
            self.emit(
                "response.content_part.done",
                json!({
                    "content_index": content_index,
                    "item_id": FAKE_ITEM_ID,
                    "output_index": output_index,
                    "part": {"type": "refusal", "refusal": refusal}
                }),
            );
        }
    }

    /// Emits the terminal events for one tool call, announcing it first if it never streamed.
    ///
    /// A call whose name never arrived was never announced, so a consumer that only saw deltas has
    /// no record of it. Emitting the whole sequence at once here is what keeps the call visible
    /// instead of silently dropped.
    fn finish_function_call(&mut self, index: u64) -> Result<()> {
        let output_index = self.layout.function_index(&self.state, index)?;
        let payload = self.function_call_item_with(index, None);
        if !self.state.announced_calls.contains(&index) {
            self.state.announced_calls.push(index);
            self.emit(
                "response.output_item.added",
                json!({"output_index": output_index, "item": payload.clone()}),
            );
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            self.emit(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": FAKE_ITEM_ID,
                    "output_index": output_index,
                    "delta": arguments
                }),
            );
        }
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": payload.clone()}),
        );
        let call_id = payload
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = payload
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("");
        let kind = convert::tool_call_item(CallId::new(call_id), name, arguments, &self.handoffs)?;
        self.emit_item(kind, output_index, payload);
        Ok(())
    }

    /// Closes the assistant message, returning the payload the completed response carries.
    fn finish_assistant_message(&mut self) -> Option<Value> {
        if self.state.text.is_none() && self.state.refusal.is_none() {
            return None;
        }
        // Parts are assembled in the order of the content indexes already announced: a refusal
        // that opened before any text holds index 0, and appending text first would contradict
        // what consumers were told.
        let mut parts = Vec::new();
        if let Some((index, text)) = self.state.text.clone() {
            parts.push((index, ContentBlock::text(text)));
        }
        if let Some((index, refusal)) = self.state.refusal.clone() {
            parts.push((index, ContentBlock::refusal(refusal)));
        }
        parts.sort_by_key(|(index, _)| *index);

        let payload = json!({
            "id": FAKE_ITEM_ID,
            "type": "message",
            "role": "assistant", // layering-allow: assistant = OpenAI wire message role
            "status": "completed",
            "content": parts.iter().map(|(_, block)| responses_content_part(block)).collect::<Vec<_>>()
        });
        let output_index = self.layout.assistant_index(&self.state);
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": payload.clone()}),
        );
        let message = Message::new(
            MessageRole::Assistant,
            parts.into_iter().map(|(_, block)| block).collect(),
        );
        self.emit_item(RunItemKind::Message(message), output_index, payload.clone());
        Some(payload)
    }

    /// Renders the reasoning item in the Responses shape the events carry.
    fn reasoning_item_payload(&self) -> Option<Value> {
        let reasoning = self.state.reasoning.as_ref()?;
        let mut payload = json!({
            "id": FAKE_ITEM_ID,
            "type": "reasoning",
            "summary": reasoning
                .summary
                .iter()
                .map(|text| json!({"type": "summary_text", "text": text}))
                .collect::<Vec<_>>(),
            "content": reasoning
                .content()
                .iter()
                .map(|text| json!({"type": "reasoning_text", "text": text}))
                .collect::<Vec<_>>()
        });
        let mut provider_data = self.reasoning_provider_data(reasoning);
        if !self.state.thinking_blocks.is_empty() {
            provider_data.insert(
                "thinking_blocks".to_owned(),
                Value::Array(self.state.thinking_blocks.clone()),
            );
        }
        if let Some(signature) = self.last_thinking_signature() {
            payload["encrypted_content"] = Value::String(signature);
        }
        payload["provider_data"] = Value::Object(provider_data);
        Some(payload)
    }

    /// Builds the normalized reasoning item, keeping the provider sequence as replay truth.
    fn neutral_reasoning(&self) -> Option<Reasoning> {
        let draft = self.state.reasoning.as_ref()?;
        let mut reasoning = Reasoning::new()
            .with_summary(draft.summary.clone())
            .with_content(draft.content())
            .with_provider_data(Value::Object(self.reasoning_provider_data(draft)));
        if let Some(signature) = self.last_thinking_signature() {
            reasoning = reasoning.with_encrypted_content(signature);
        }
        Some(reasoning)
    }

    /// Records everything a later turn needs to reproduce this reasoning on the wire.
    ///
    /// Built in one place for both the event payload and the normalized item, so the record a
    /// consumer sees and the record a replay reads can never describe different reasoning.
    fn reasoning_provider_data(&self, draft: &ReasoningDraft) -> Map<String, Value> {
        let summary = draft.summary.join("\n");
        let mut provider_data = convert::reasoning_provider_data(
            &self.codec.model,
            self.state.completion_id(),
            (!summary.is_empty()).then_some(summary.as_str()),
            draft.body.as_deref(),
        );
        if !self.state.thinking_blocks.is_empty() {
            provider_data.insert(
                "thinking_blocks".to_owned(),
                Value::Array(self.state.thinking_blocks.clone()),
            );
        }
        provider_data
    }

    /// The signature that closes the accumulated thinking sequence, when one arrived.
    fn last_thinking_signature(&self) -> Option<String> {
        self.state
            .thinking_blocks
            .iter()
            .filter_map(|block| block.get("signature").and_then(Value::as_str))
            .next_back()
            .map(str::to_owned)
    }
}

/// Renders one neutral content block in the Responses wire shape.
fn responses_content_part(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Refusal(refusal) => {
            json!({"type": "refusal", "refusal": refusal.refusal()})
        }
        other => json!({
            "type": "output_text",
            "text": other.as_text().unwrap_or_default(),
            "annotations": []
        }),
    }
}

/// Renders usage in the Responses shape, so both protocols report it identically.
fn responses_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens(),
        "output_tokens": usage.output_tokens(),
        "total_tokens": usage.total_tokens(),
        "input_tokens_details": {
            "cached_tokens": usage.cached_input_tokens(),
            "cache_write_tokens": usage.cache_write_tokens()
        },
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens()}
    })
}

/// Renders the assistant message envelope announced before its first content part.
fn in_progress_message() -> Value {
    json!({
        "id": FAKE_ITEM_ID,
        "type": "message",
        "role": "assistant", // layering-allow: assistant = OpenAI wire message role
        "status": "in_progress",
        "content": []
    })
}

/// Whether a fragment describes a function call this adapter can execute.
fn should_buffer(fragment: &Value) -> bool {
    matches!(
        fragment.get("type").and_then(Value::as_str),
        None | Some("function")
    )
}

/// Whether the choice ended because the model asked for tools.
fn finished_tool_calls(finish_reason: Option<&str>) -> bool {
    finish_reason == Some("tool_calls")
}

/// Whether a delta carries output that must reach consumers even while tool calls are buffered.
///
/// Some deltas exist only to carry provider-private fields. Dropping one because it had no
/// standard output loses material the next turn has to resend, so the test is deliberately broad.
fn has_passthrough_output(delta: &Value) -> bool {
    delta.get("content").is_some_and(|value| !value.is_null())
        || delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
        || non_empty(delta.get("refusal")).is_some()
        || non_empty(delta.get("reasoning_content")).is_some()
        || non_empty(delta.get("reasoning")).is_some()
        || delta
            .get("thinking_blocks")
            .and_then(Value::as_array)
            .is_some_and(|blocks| !blocks.is_empty())
}

/// Folds one wire fragment into the call it belongs to.
fn accumulate_fragment(calls: &mut Vec<(u64, FunctionCall)>, index: u64, fragment: &Value) {
    let position = calls
        .iter()
        .position(|(existing, _)| *existing == index)
        .unwrap_or_else(|| {
            calls.push((index, FunctionCall::default()));
            calls.len() - 1
        });
    let call = &mut calls[position].1;
    if let Some(id) = non_empty(fragment.get("id")) {
        id.clone_into(&mut call.call_id);
    }
    if let Some(name) = fragment.pointer("/function/name").and_then(Value::as_str)
        && !name.is_empty()
    {
        // The name is complete in the first fragment that carries it; only arguments are split.
        name.clone_into(&mut call.name);
    }
    if let Some(arguments) = fragment
        .pointer("/function/arguments")
        .and_then(Value::as_str)
    {
        call.arguments.push_str(arguments);
    }
}

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}
