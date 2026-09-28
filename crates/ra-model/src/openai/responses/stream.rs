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

use std::collections::VecDeque;

use futures::{Stream, StreamExt, stream, stream::BoxStream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    model::{
        ModelHandoffDefinition, ModelStream, ModelStreamEvent, ProviderKey, RawResponseEvent,
        ReplaySafety, RunItemStreamEvent, stamp_replay_safety,
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

/// Turns a stream of Responses events into model stream events.
pub(crate) fn events(
    frames: impl Stream<Item = Result<SseFrame>> + Send + 'static,
    provider: ProviderKey,
    model: String,
    handoffs: Vec<ModelHandoffDefinition>,
    request_id: Option<String>,
    unstarted: ReplaySafety,
) -> ModelStream<'static> {
    let driver = StreamDriver {
        frames: frames.boxed(),
        provider,
        model,
        handoffs,
        request_id,
        response_id: None,
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
    response_id: Option<String>,
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
    /// Yields the next event, recording what a later failure would have to be replayed over.
    ///
    /// This is the only layer that can answer the replay question. The frame reader below has no
    /// idea what its bytes were turned into, and the policy above receives the error long after the
    /// stream that produced it is gone.
    async fn next_event(&mut self) -> Option<Result<ModelStreamEvent>> {
        match self.decode_next().await? {
            Ok(event) => {
                self.emitted = true;
                Some(Ok(event))
            }
            // Any event already delivered makes a transparent replay a duplicate for whoever read
            // it, whether that event carried output or only narration.
            Err(error) => Some(Err(stamp_replay_safety(
                error,
                if self.emitted {
                    ReplaySafety::Unsafe
                } else {
                    self.unstarted
                },
            ))),
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
        let terminal_failure = match event_type.as_str() {
            "response.failed" | "response.incomplete" => Some(failure(&event_type, frame)),
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
            CREATED => {
                if let Some(id) = frame.pointer("/response/id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
            }
            OUTPUT_ITEM_DONE => {
                let event = self.lift_output_item(frame)?;
                self.pending.push_back(Ok(event));
            }
            COMPLETED => {
                let event = self.settle(frame)?;
                self.pending.push_back(Ok(event));
            }
            _ => {}
        }
        Ok(())
    }

    /// Publishes a finished output item on the normalized channel.
    fn lift_output_item(&mut self, frame: &Value) -> Result<ModelStreamEvent> {
        let item = frame
            .get("item")
            .ok_or_else(|| behavior_error("OpenAI response.output_item.done has no item"))?;
        let index = frame
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let run_item = convert::convert_output_item(
            item,
            self.response_id.as_deref().unwrap_or_default(),
            usize::try_from(index).unwrap_or(usize::MAX),
            &self.handoffs,
            &self.provider,
            &self.model,
        )?;
        let name = run_item.kind().label();
        Ok(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
            name, run_item,
        )))
    }

    /// Lifts the finished response object the terminal frame carries.
    ///
    /// The whole object is there, so this is the non-streaming conversion applied to a payload that
    /// arrived in pieces — including its refusal of a response whose status is not `completed`.
    fn settle(&mut self, frame: &Value) -> Result<ModelStreamEvent> {
        let payload = frame
            .get("response")
            .ok_or_else(|| behavior_error("OpenAI response.completed has no response"))?;
        let response = convert::convert_response(
            payload,
            self.request_id.clone(),
            &self.handoffs,
            &self.provider,
            &self.model,
        )?;
        self.settled = true;
        // `response.completed` is terminal for this protocol. Stop reading at it so the terminal
        // model event is also the final event a consumer observes; the trailing `[DONE]` marker is
        // transport punctuation and carries no facts the completed response lacks.
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
        Some("max_output_tokens") => ProviderErrorKind::ContextOverflow,
        Some("content_filter") => ProviderErrorKind::Refusal,
        _ => return behavior_error(message),
    };
    Error::provider(kind, message)
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
    behavior_error(terminal_message(event_type, &details))
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
