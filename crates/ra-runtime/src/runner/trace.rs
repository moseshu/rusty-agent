// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Observations at the actual model-attempt and settled-turn boundaries.

use ra_core::{
    model::ModelRequest,
    session::rollout::{RolloutRecorder, RolloutTraceEvent, RolloutTraceMode},
};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use super::TurnRecord;

/// Carries the dispatcher during both polling and destruction, including task aborts.
pub(crate) fn carry_subscriber<F: Future>(future: Pin<Box<F>>) -> impl Future<Output = F::Output> {
    with_dispatch(future, tracing::dispatcher::get_default(Clone::clone))
}

pub(crate) fn with_dispatch<F: Future>(
    future: Pin<Box<F>>,
    dispatch: tracing::Dispatch,
) -> impl Future<Output = F::Output> {
    DispatchFuture {
        inner: Some(future),
        dispatch,
    }
}

struct DispatchFuture<F> {
    inner: Option<Pin<Box<F>>>,
    dispatch: tracing::Dispatch,
}

impl<F: Future> Future for DispatchFuture<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _default = tracing::dispatcher::set_default(&this.dispatch);
        match &mut this.inner {
            Some(future) => future.as_mut().poll(context),
            None => Poll::Pending,
        }
    }
}

impl<F> Drop for DispatchFuture<F> {
    fn drop(&mut self) {
        let _default = tracing::dispatcher::set_default(&self.dispatch);
        drop(self.inner.take());
    }
}

pub(super) fn observe(
    recorder: Option<&dyn RolloutRecorder>,
    kind: &str,
    mut fields: Value,
    payload: impl FnOnce() -> serde_json::Result<Value>,
) {
    let Some(recorder) = recorder else { return };
    let mode = recorder.trace_mode();
    if mode == RolloutTraceMode::Disabled {
        return;
    }
    let payload = if mode == RolloutTraceMode::Payloads {
        if let Ok(value) = payload() {
            Some(value)
        } else {
            fields["capture_error"] = json!("serialization");
            None
        }
    } else {
        None
    };
    recorder.record_trace(RolloutTraceEvent::new(kind, fields, payload));
}

pub(super) fn next_step(recorder: Option<&dyn RolloutRecorder>, record: Option<&TurnRecord>) {
    if let Some(record) = record {
        observe(
            recorder,
            "next_step",
            json!({
                "turn.index": record.turn().saturating_sub(1),
                "agent.id": record.agent(),
                "next_step": record.next_step_code(),
                "finish.reason": record.finish_reason(),
            }),
            || Ok(Value::Null),
        );
    }
}

pub(super) fn request_payload(request: &ModelRequest) -> serde_json::Result<Value> {
    Ok(json!({
        "system_instructions": request.system_instructions(),
        "input": request.input(),
        "model_settings": request.model_settings().to_traceable_value()?,
        "tools": request.tools().iter().map(|tool| json!({
            "kind": tool.kind(), "name": tool.name(), "namespace": tool.namespace(),
            "namespace_description": tool.namespace_description(),
            "description": tool.description(), "input_schema": tool.input_schema(),
            "strict": tool.strict(), "defer_loading": tool.defer_loading(),
            "output_json_schema": tool.output_json_schema(), "allowed_callers": tool.allowed_callers(),
        })).collect::<Vec<_>>(),
        "handoffs": request.handoffs().iter().map(|handoff| json!({
            "name": handoff.name(), "target_agent": handoff.target_agent(),
            "description": handoff.description(), "input_schema": handoff.input_schema(),
            "strict": handoff.strict(),
        })).collect::<Vec<_>>(),
        "output_schema": request.output_schema().map(|schema| json!({
            "name": schema.name(), "schema": schema.schema(), "strict": schema.strict(),
        })),
        "continuation": request.continuation(),
        "cache_plan": request.cache_plan(),
        "tracing": request.tracing(),
    }))
}

/// Ensures dropping a pending attempt leaves a cancellation observation before its span closes.
pub(super) struct AttemptObservation<'a> {
    recorder: Option<&'a dyn RolloutRecorder>,
    span: tracing::Span,
    active: bool,
}

impl<'a> AttemptObservation<'a> {
    pub(super) fn new(recorder: Option<&'a dyn RolloutRecorder>, span: tracing::Span) -> Self {
        Self {
            recorder,
            span,
            active: true,
        }
    }

    pub(super) fn finish(&mut self) {
        self.active = false;
    }
}

impl Drop for AttemptObservation<'_> {
    fn drop(&mut self) {
        if self.active {
            ra_core::trace::record_outcome(&self.span, ra_core::trace::SpanOutcome::Cancelled);
            self.span.in_scope(|| {
                observe(
                    self.recorder,
                    "model_error",
                    json!({"outcome": "cancelled"}),
                    || Ok(Value::Null),
                );
            });
        }
    }
}
