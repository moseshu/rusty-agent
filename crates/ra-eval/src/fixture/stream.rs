//! Automatic streaming projected onto the framework's neutral event vocabulary.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{ContentBlock, ModelResponse, RunItemKind},
    model::{
        ModelStream, ModelStreamDelta, ModelStreamDeltaKind, ModelStreamEvent, RunItemStreamEvent,
    },
    trace::{SpanOutcome, record_error, record_outcome},
};

use super::errors::UnsupportedAutomaticStream;

/// Records a generation's terminal state, or cancellation when dropped before one is reached.
///
/// Upstream records a cancelled call as a timeout error on its generation span. The framework's
/// span vocabulary keeps cancellation out of the failure tier, so it is recorded as the
/// `cancelled` outcome instead.
pub(super) struct GenerationOutcome {
    span: tracing::Span,
    settled: bool,
}

impl GenerationOutcome {
    pub(super) const fn new(span: tracing::Span) -> Self {
        Self {
            span,
            settled: false,
        }
    }

    pub(super) fn settle<T>(mut self, result: &Result<T>) {
        match result {
            Ok(_) => record_outcome(&self.span, SpanOutcome::Ok),
            Err(error) => record_error(&self.span, error),
        }
        self.settled = true;
    }
}

impl Drop for GenerationOutcome {
    fn drop(&mut self) {
        if !self.settled {
            record_outcome(&self.span, SpanOutcome::Cancelled);
        }
    }
}

struct TracedStream<'a> {
    inner: Option<ModelStream<'a>>,
    span: Option<tracing::Span>,
    emit_traces: bool,
    settled: bool,
    finished: bool,
}

impl Stream for TracedStream<'_> {
    type Item = Result<ModelStreamEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        let span = this
            .span
            .get_or_insert_with(|| generation_span(this.emit_traces, true));
        let next = {
            let _entered = span.enter();
            let next = this
                .inner
                .as_mut()
                .map_or(Poll::Ready(None), |inner| inner.as_mut().poll_next(cx));
            match &next {
                Poll::Ready(Some(Ok(ModelStreamEvent::Completed(_)))) if !this.settled => {
                    record_outcome(span, SpanOutcome::Ok);
                    this.settled = true;
                }
                Poll::Ready(Some(Err(error))) => {
                    record_error(span, error);
                    this.settled = true;
                }
                Poll::Ready(None) => {
                    if !this.settled {
                        record_outcome(span, SpanOutcome::Ok);
                        this.settled = true;
                    }
                    drop(this.inner.take());
                }
                _ => {}
            }
            next
        };
        if matches!(next, Poll::Ready(None)) {
            this.finished = true;
            this.span = None;
        }
        next
    }
}

impl Drop for TracedStream<'_> {
    /// A stream dropped after polling started but before a terminal event was cancelled.
    fn drop(&mut self) {
        if let Some(span) = &self.span
            && !self.settled
        {
            record_outcome(span, SpanOutcome::Cancelled);
        }
        let _entered = self.span.as_ref().map(tracing::Span::enter);
        drop(self.inner.take());
    }
}

pub(super) fn instrument(inner: ModelStream<'_>, emit_traces: bool) -> ModelStream<'_> {
    Box::pin(TracedStream {
        inner: Some(inner),
        span: None,
        emit_traces,
        settled: false,
        finished: false,
    })
}

pub(super) fn generation_span(emit_traces: bool, streamed: bool) -> tracing::Span {
    if emit_traces {
        tracing::info_span!(
            "generation",
            streamed,
            error.code = tracing::field::Empty,
            outcome = tracing::field::Empty
        )
    } else {
        tracing::Span::none()
    }
}

/// Projects a response onto model-boundary events, named by item kind as the model adapters do.
///
/// Semantic names such as `message_output_created` belong to the runner's mapping of step items,
/// not to the model stream.
pub(super) fn automatic_events(response: ModelResponse) -> Result<Vec<ModelStreamEvent>> {
    let mut events = Vec::new();
    for item in response.output() {
        match item.kind() {
            RunItemKind::Message(message) => {
                let empty = ra_core::item::Message::new(message.role(), Vec::new());
                let empty = if let Some(phase) = message.phase() {
                    empty.with_phase(phase)
                } else {
                    empty
                };
                let added =
                    ra_core::item::RunItem::new(item.id().clone(), RunItemKind::Message(empty));
                events.push(ModelStreamEvent::OutputItemAdded(RunItemStreamEvent::new(
                    item.kind().label(),
                    added,
                )));
                for content in message.content() {
                    if let ContentBlock::Text(text) = content {
                        events.push(ModelStreamEvent::Delta(ModelStreamDelta::new(
                            item.id().clone(),
                            ModelStreamDeltaKind::OutputText {
                                delta: text.text().to_owned(),
                            },
                        )));
                    }
                }
            }
            RunItemKind::Reasoning(reasoning) => {
                let mut empty = ra_core::item::Reasoning::new();
                if let Some(id) = reasoning.id() {
                    empty = empty.with_id(id);
                }
                events.push(ModelStreamEvent::OutputItemAdded(RunItemStreamEvent::new(
                    item.kind().label(),
                    ra_core::item::RunItem::new(item.id().clone(), RunItemKind::Reasoning(empty)),
                )));
                for (index, text) in reasoning.summary().iter().enumerate() {
                    let index = i64::try_from(index)
                        .map_err(|_| Error::caller("Too many reasoning parts"))?;
                    events.push(ModelStreamEvent::Delta(ModelStreamDelta::new(
                        item.id().clone(),
                        ModelStreamDeltaKind::ReasoningSummaryPartAdded {
                            summary_index: index,
                        },
                    )));
                    events.push(ModelStreamEvent::Delta(ModelStreamDelta::new(
                        item.id().clone(),
                        ModelStreamDeltaKind::ReasoningSummaryText {
                            delta: text.clone(),
                            summary_index: index,
                        },
                    )));
                }
                for (index, text) in reasoning.content().iter().enumerate() {
                    events.push(ModelStreamEvent::Delta(ModelStreamDelta::new(
                        item.id().clone(),
                        ModelStreamDeltaKind::ReasoningText {
                            delta: text.clone(),
                            content_index: i64::try_from(index)
                                .map_err(|_| Error::caller("Too many reasoning parts"))?,
                        },
                    )));
                }
            }
            RunItemKind::ToolCall(call) if call.kind().is_function() => {}
            _ => {
                let error = UnsupportedAutomaticStream::new(item.kind().label());
                return Err(
                    Error::provider(ProviderErrorKind::Behavior, error.to_string())
                        .with_source(error),
                );
            }
        }
        events.push(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
            item.kind().label(),
            item.clone(),
        )));
    }
    events.push(ModelStreamEvent::Completed(Box::new(response)));
    Ok(events)
}
