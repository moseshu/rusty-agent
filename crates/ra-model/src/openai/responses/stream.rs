//! Decode the Responses event stream.
//!
//! # Why this is so much smaller than the Chat decoder
//!
//! The Responses stream already speaks the vocabulary the internal item model is shaped like. Each
//! frame names its own type, carries the output index it belongs to, and numbers itself; an output
//! item arrives whole on `response.output_item.done`; and the terminal frame carries the complete
//! response object. So there is nothing to reassemble and nothing to synthesize — this decoder
//! forwards what arrived and lifts two of the frames.
//!
//! That difference is worth stating rather than leaving as an accident of line count: on this
//! protocol the raw channel really is raw. A consumer reading those payloads is reading `OpenAI`'s
//! own event schema, not a shape this crate invented, which is not true of the Chat decoder next
//! door and is the reason only that one warns about its spelling being unstable.
//!
//! # What it still has to decide
//!
//! Whether the stream finished. A body that stops carries no evidence either way, and settling on
//! it would report a turn the provider never said it produced. Only `response.completed` is that
//! evidence here, and the failure frames are refused rather than being allowed to fall through to
//! the same "ran out of bytes" ending.

use std::collections::{BTreeMap, VecDeque};

use futures::{Stream, StreamExt, stream, stream::BoxStream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{ItemId, ModelResponse, RunItem},
    model::{
        ModelHandoffDefinition, ModelStream, ModelStreamEvent, ProviderKey, RawResponseEvent,
        ReplaySafety, RunItemStreamEvent, replay_safety_of, stamp_replay_safety,
    },
};
use serde_json::Value;

use super::convert;
use crate::openai::{error::behavior_error, sse::SseFrame};

/// The frame that carries the finished response object.
const COMPLETED: &str = "response.completed";
/// The frame that carries one finished output item.
const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
/// The frame that names the response this stream belongs to.
const CREATED: &str = "response.created";
const INCOMPLETE: &str = "response.incomplete";

pub(super) fn continues_turn(frame: &Value) -> bool {
    match frame.get("type").and_then(Value::as_str) {
        Some(COMPLETED) => {
            frame.pointer("/response/end_turn").and_then(Value::as_bool) == Some(false)
        }
        Some(INCOMPLETE) => {
            frame
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                == Some("interrupted")
        }
        _ => false,
    }
}

/// Collects completed items in arrival order for Codex-style continuation. The stream and the
/// WebSocket reuse baseline both settle from this projection, independently of the final payload.
pub(super) struct CompletedOutput {
    response_id: Option<String>,
    items: Option<Vec<RunItem>>,
}

impl CompletedOutput {
    pub(super) fn new(collect: bool) -> Self {
        Self {
            response_id: None,
            items: collect.then(Vec::new),
        }
    }

    pub(super) fn observe_created(&mut self, frame: &Value) {
        if let Some(id) = frame.pointer("/response/id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
    }

    pub(super) fn lift_done(
        &mut self,
        frame: &Value,
        handoffs: &[ModelHandoffDefinition],
        provider: &ProviderKey,
        model: &str,
    ) -> Result<Option<RunItem>> {
        let item = frame
            .get("item")
            .ok_or_else(|| behavior_error("OpenAI response.output_item.done has no item"))?;
        let response_id = self.response_id.as_deref().unwrap_or_default();
        let index = frame.get("output_index").and_then(Value::as_u64);
        // An indexless frame still needs a distinct synthesized ID if the item has no wire ID.
        let indexless_id;
        let (response_id, index) = match (&self.items, index) {
            (Some(items), None) => {
                indexless_id = format!("{response_id}:done");
                (indexless_id.as_str(), items.len())
            }
            (_, index) => (
                response_id,
                usize::try_from(index.unwrap_or_default()).unwrap_or(usize::MAX),
            ),
        };
        let result =
            convert::convert_output_item(item, response_id, index, handoffs, provider, model)?;
        if let (Some(items), Some(item)) = (&mut self.items, &result) {
            items.push(item.clone());
        }
        Ok(result)
    }

    pub(super) fn settle_continuation(
        &mut self,
        frame: &Value,
        request_id: Option<String>,
    ) -> Result<ModelResponse> {
        let payload = frame
            .get("response")
            .ok_or_else(|| behavior_error("OpenAI continuation frame has no response"))?;
        let response_id = payload
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| behavior_error("OpenAI continuation frame has no response id"))?;
        let items = self.items.take().ok_or_else(|| {
            behavior_error("OpenAI continuation has no completed item collection")
        })?;
        let mut response = ModelResponse::new(items)
            .with_response_id(response_id)
            .with_usage(convert::convert_usage(payload.get("usage")));
        if let Some(request_id) = request_id {
            response = response.with_request_id(request_id);
        }
        Ok(response)
    }
}

/// Turns a stream of Responses events into model stream events.
pub(crate) fn events(
    frames: impl Stream<Item = Result<SseFrame>> + Send + 'static,
    provider: ProviderKey,
    model: String,
    handoffs: Vec<ModelHandoffDefinition>,
    request_id: Option<String>,
    unstarted: ReplaySafety,
    end_turn_signals: bool,
) -> ModelStream<'static> {
    let driver = StreamDriver {
        frames: frames.boxed(),
        provider,
        model,
        handoffs,
        request_id,
        output: CompletedOutput::new(end_turn_signals),
        output_ids: BTreeMap::new(),
        pending: VecDeque::new(),
        emitted: false,
        unstarted,
        settled: false,
        finished: false,
    };
    stream::unfold(driver, |mut driver| async move {
        driver.next_event().await.map(|event| (event, driver))
    })
    .boxed()
}

struct StreamDriver {
    frames: BoxStream<'static, Result<SseFrame>>,
    provider: ProviderKey,
    /// The model that produced reasoning replay material in this stream.
    model: String,
    handoffs: Vec<ModelHandoffDefinition>,
    /// Transport diagnostics from the response headers, which no frame carries.
    request_id: Option<String>,
    /// The response identifier, used to name an item the endpoint did not name itself.
    output: CompletedOutput,
    /// Canonical identities for deltas whose frame only names an output slot.
    output_ids: BTreeMap<u64, ItemId>,
    /// Events decoded from the last frame, waiting to be yielded.
    ///
    /// One frame can mean two events: the provider's own narration always, plus the normalized
    /// form of the two frames that carry one.
    pending: VecDeque<Result<ModelStreamEvent>>,
    /// Whether any event has already reached the consumer.
    emitted: bool,
    /// The verdict a failure gets while nothing has been emitted, which depends on the request.
    unstarted: ReplaySafety,
    /// Whether the terminal frame arrived, which is the only evidence this stream finished.
    settled: bool,
    finished: bool,
}

impl StreamDriver {
    /// Yields the next event, distinguishing an unstarted call from an interrupted stream.
    ///
    /// After an event, preserve provider evidence without inventing an output-consumption veto.
    /// The runtime knows whether it published output to a public run subscriber, sent narration
    /// to a retry-aware thread, or dispatched a tool; this decoder does not.
    async fn next_event(&mut self) -> Option<Result<ModelStreamEvent>> {
        match self.decode_next().await? {
            Ok(event) => {
                self.emitted = true;
                Some(Ok(event))
            }
            Err(error) => {
                let safety = if self.emitted {
                    replay_safety_of(&error)
                } else {
                    self.unstarted
                };
                Some(Err(stamp_replay_safety(error, safety)))
            }
        }
    }

    async fn decode_next(&mut self) -> Option<Result<ModelStreamEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            match self.frames.next().await {
                Some(Ok(SseFrame::Data(frame))) => {
                    if let Err(error) = self.decode(&frame) {
                        self.finished = true;
                        return Some(Err(error));
                    }
                }
                // `[DONE]` is a marker, not a result. The Responses stream states its outcome in a
                // frame of its own, so a terminator arriving without one leaves this stream
                // unsettled and the check below is what reports it.
                Some(Ok(SseFrame::Done)) => {}
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                None => {
                    self.finished = true;
                    if !self.settled {
                        return Some(Err(Error::provider(
                            ProviderErrorKind::Network,
                            "OpenAI response stream ended without `response.completed`; the turn \
                             is incomplete and reporting it as finished would invent an outcome \
                             the provider never stated",
                        )));
                    }
                }
            }
        }
    }

    /// Turns one frame into the events it means.
    ///
    /// Every frame is forwarded on the raw channel, so a consumer reading it sees the provider's
    /// own stream in full. The two frames that also mean something to a protocol-neutral consumer
    /// are additionally lifted, through the same functions the non-streaming path uses — an item
    /// that streamed in and the same item read from the finished response cannot then disagree
    /// about what it is.
    fn decode(&mut self, frame: &Value) -> Result<()> {
        let event_type = frame
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| behavior_error("OpenAI response event has no type"))?
            .to_owned();

        // A failure frame is the provider stating an outcome, and it is the only place that outcome
        // appears: the body simply ends afterwards, which the check for a missing terminal frame
        // would report as a dropped connection instead of as the refusal or the truncation it was.
        let continue_turn = self.output.items.is_some() && continues_turn(frame);
        let terminal_failure = match event_type.as_str() {
            "response.failed" => Some(failure(&event_type, frame)),
            INCOMPLETE if !continue_turn => Some(failure(&event_type, frame)),
            "error" | "response.error" => Some(stream_error(&event_type, frame)),
            _ => None,
        };

        self.pending
            .push_back(Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                self.provider.clone(),
                event_type.clone(),
                frame.clone(),
            ))));

        // As on the reference, the failure frame itself reaches the consumer first and the error
        // follows it; nothing after it is read, so transport teardown cannot replace the
        // provider's own account of what went wrong.
        if let Some(error) = terminal_failure {
            self.pending.push_back(Err(error));
            self.finished = true;
            return Ok(());
        }

        match event_type.as_str() {
            CREATED => self.output.observe_created(frame),
            OUTPUT_ITEM_DONE => {
                if let Some(item) =
                    self.output
                        .lift_done(frame, &self.handoffs, &self.provider, &self.model)?
                {
                    let event = RunItemStreamEvent::new(item.kind().label(), item);
                    self.pending.push_back(Ok(ModelStreamEvent::RunItem(event)));
                }
            }
            COMPLETED | INCOMPLETE => {
                let event = self.settle(frame, continue_turn)?;
                self.pending.push_back(Ok(event));
            }
            _ => {}
        }
        self.lift_narration(&event_type, frame);
        Ok(())
    }

    fn lift_narration(&mut self, event_type: &str, frame: &Value) {
        use crate::openai::stream_events::{text_delta, text_item_added};
        if event_type == "response.output_item.added" {
            let Some(item) = frame.get("item") else {
                return;
            };
            let Some(kind) = item.get("type").and_then(Value::as_str) else {
                return;
            };
            let index = frame
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let id = item.get("id").and_then(Value::as_str).map_or_else(
                || {
                    ItemId::new(format!(
                        "{}:{index}:{kind}",
                        self.output.response_id.as_deref().unwrap_or_default()
                    ))
                },
                ItemId::new,
            );
            self.output_ids.insert(index, id.clone());
            if let Some(event) = text_item_added(item, id) {
                self.pending.push_back(Ok(event));
            }
        } else {
            let id = frame
                .get("item_id")
                .and_then(Value::as_str)
                .map(ItemId::new)
                .or_else(|| {
                    self.output_ids
                        .get(&frame.get("output_index")?.as_u64()?)
                        .cloned()
                });
            if let Some(delta) = id.and_then(|id| text_delta(event_type, frame, id)) {
                self.pending.push_back(Ok(ModelStreamEvent::Delta(delta)));
            }
        }
    }

    /// Settles a continuation from finished stream items, or reads the ordinary final payload.
    fn settle(&mut self, frame: &Value, continue_turn: bool) -> Result<ModelStreamEvent> {
        let response = if continue_turn {
            self.output
                .settle_continuation(frame, self.request_id.take())?
        } else {
            let payload = frame
                .get("response")
                .ok_or_else(|| behavior_error("OpenAI response.completed has no response"))?;
            convert::convert_response(
                payload,
                self.request_id.take(),
                &self.handoffs,
                &self.provider,
                &self.model,
            )?
        };
        self.settled = true;
        // The terminal model event is last; the trailing `[DONE]` marker adds no response facts.
        self.finished = true;
        Ok(ModelStreamEvent::Completed(Box::new(response)))
    }
}

/// Reads the outcome out of a `response.failed` or `response.incomplete` frame.
///
/// The message is the reference's `format_response_terminal_failure`: the event, then the
/// response's `status`, `error` and `incomplete_details` when present. Those two objects are shown
/// as compact JSON where the reference shows its SDK's representation of them.
///
/// The reference reports every such frame as a model behaviour error. A truncation and a content
/// filter are classified here instead, as the non-streaming path classifies the same statuses, so
/// a caller can tell them from a malformed response.
fn failure(event_type: &str, frame: &Value) -> Error {
    let response = frame.get("response");
    let field = |name: &str| response.and_then(|response| response.get(name));
    let mut details = Vec::new();
    if let Some(status) = field("status")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        details.push(format!("status={status}"));
    }
    for name in ["error", "incomplete_details"] {
        if let Some(value) = field(name).filter(|value| !value.is_null()) {
            details.push(format!("{name}={value}"));
        }
    }
    let message = terminal_message(event_type, &details);
    let kind = match field("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        Some("max_output_tokens") => ProviderErrorKind::OutputLimit,
        Some("content_filter") => ProviderErrorKind::Refusal,
        _ => ProviderErrorKind::Behavior,
    };
    crate::openai::error::stream_failure(event_type, frame, kind, message)
}

/// Reads an `error` or `response.error` frame, which reports a call that will produce nothing
/// further.
///
/// The message is the reference's `format_response_error_event`: `code`, `message` and `param`
/// when present. They sit at the top of an `error` frame and inside an `error` object on a
/// `response.error` frame, so both places are read, the top first.
fn stream_error(event_type: &str, frame: &Value) -> Error {
    let nested = frame.get("error").filter(|value| value.is_object());
    let field = |name: &str| {
        frame
            .get(name)
            .filter(|value| !value.is_null())
            .or_else(|| nested.and_then(|error| error.get(name)))
            .filter(|value| !value.is_null() && value.as_str() != Some(""))
    };
    let details: Vec<String> = ["code", "message", "param"]
        .into_iter()
        .filter_map(|name| {
            field(name).map(|value| match value.as_str() {
                Some(text) => format!("{name}={text}"),
                None => format!("{name}={value}"),
            })
        })
        .collect();
    crate::openai::error::stream_failure(
        event_type,
        frame,
        ProviderErrorKind::Behavior,
        terminal_message(event_type, &details),
    )
}

/// The reference's wording for a stream that ended on a failure event.
fn terminal_message(event_type: &str, details: &[String]) -> String {
    let message = format!("Responses stream ended with terminal event `{event_type}`.");
    if details.is_empty() {
        message
    } else {
        format!("{message} {}.", details.join("; "))
    }
}
