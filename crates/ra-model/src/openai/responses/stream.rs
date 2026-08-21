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
        RunItemStreamEvent,
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
    handoffs: Vec<ModelHandoffDefinition>,
    request_id: Option<String>,
) -> ModelStream<'static> {
    let driver = StreamDriver {
        frames: frames.boxed(),
        provider,
        handoffs,
        request_id,
        response_id: None,
        pending: VecDeque::new(),
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
    /// Whether the terminal frame arrived, which is the only evidence this stream finished.
    settled: bool,
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
        match event_type.as_str() {
            "response.failed" | "response.incomplete" => return Err(failure(frame)),
            "error" => return Err(stream_error(frame)),
            _ => {}
        }

        self.pending
            .push_back(Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                self.provider.clone(),
                event_type.clone(),
                frame.clone(),
            ))));

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
        )?;
        self.settled = true;
        // `response.completed` is terminal for this protocol. Stop reading at it so the terminal
        // model event is also the final event a consumer observes; the trailing `[DONE]` marker is
        // transport punctuation and carries no facts the completed response lacks.
        self.finished = true;
        Ok(ModelStreamEvent::Completed(Box::new(response)))
    }
}

/// Reads the outcome out of a terminal failure frame.
fn failure(frame: &Value) -> Error {
    let reason = frame
        .pointer("/response/incomplete_details/reason")
        .and_then(Value::as_str);
    match reason {
        Some("max_output_tokens") => Error::provider(
            ProviderErrorKind::ContextOverflow,
            "OpenAI response stopped at max_output_tokens",
        ),
        Some("content_filter") => Error::provider(
            ProviderErrorKind::Refusal,
            "OpenAI response stopped on a content filter",
        ),
        _ => {
            let message = frame
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI response stream reported a failed response");
            behavior_error(message.to_owned())
        }
    }
}

/// Reads a mid-stream error frame, which reports a call that will produce nothing further.
fn stream_error(frame: &Value) -> Error {
    let message = frame
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI response stream reported an error");
    behavior_error(message.to_owned())
}
