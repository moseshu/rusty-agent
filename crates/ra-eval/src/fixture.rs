//! Deterministic fixtures for the public model boundary.
//!
//! Ported from `agents.testing.ScriptedModel` in openai-agents-python (MIT).
//! Build a fresh model from the same script for each run. Steps are consumed once, in FIFO order;
//! exhaustion and leftover steps are errors rather than implicit repetitions or empty responses.
//! Static [`ModelResponse`] values use the existing serde format and can be stored as JSON fixtures.
//! Responders and stream factories are responsible for their own determinism.

mod errors;
mod step;
mod stream;

pub use errors::{UnconsumedModelSteps, UnexpectedModelCall};
pub use step::{ModelResponder, ModelStep, ModelStreamFactory};

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use futures::{StreamExt, stream as futures_stream};
use ra_core::{
    error::{Error, Result},
    item::{CallId, ItemId, Message, MessageRole, ModelResponse, RunItem, RunItemKind, ToolCall},
    model::{
        Model, ModelProvider, ModelRequest, ModelRetryAdviceRequest, ModelStream, RetryAdvice,
    },
    usage::{RequestUsage, Usage},
};
use tracing::Instrument;

use step::StepOutcome;

/// A detached request snapshot and the entry point used for the call.
///
/// The request retains settings, tools, handoffs, schema, tracing, continuation and cache plan.
/// It deliberately has no serde implementation: model settings may contain secrets.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCall {
    request: ModelRequest,
    streamed: bool,
}

impl ModelCall {
    /// Complete request received by the model.
    #[must_use]
    pub const fn request(&self) -> &ModelRequest {
        &self.request
    }

    /// Whether the streaming model entry point was used.
    #[must_use]
    pub const fn streamed(&self) -> bool {
        self.streamed
    }
}

#[derive(Default)]
struct ScriptState {
    steps: VecDeque<ModelStep>,
    calls: Vec<ModelCall>,
    default_usage: Option<Usage>,
}

/// A provider-neutral model driven by an explicit script.
///
/// Each call records its request and consumes exactly one queued step, including failures and
/// recursively resolved responders. Concurrent calls consume in first-poll order; deterministic
/// fixtures must control that order. No lock is held while user code or async work runs.
#[non_exhaustive]
pub struct ScriptedModel {
    state: Mutex<ScriptState>,
    emit_traces: bool,
    identity: Arc<()>,
}

impl ScriptedModel {
    /// Creates a model from typed steps, responses, output vectors, or owned framework errors.
    #[must_use]
    pub fn new(steps: impl IntoIterator<Item = impl Into<ModelStep>>) -> Self {
        Self {
            state: Mutex::new(ScriptState {
                steps: steps.into_iter().map(Into::into).collect(),
                ..ScriptState::default()
            }),
            emit_traces: false,
            identity: Arc::new(()),
        }
    }

    /// Enables local generation spans containing only stream mode, terminal outcome and stable
    /// error codes. A call dropped before its terminal result records the `cancelled` outcome.
    #[must_use]
    pub const fn with_emit_traces(mut self, emit_traces: bool) -> Self {
        self.emit_traces = emit_traces;
        self
    }

    /// Sets usage copied into steps whose usage equals [`Usage::default`].
    #[must_use]
    pub fn with_default_usage(self, usage: Usage) -> Self {
        self.set_default_usage(Some(usage));
        self
    }

    /// Sets usage copied into steps whose usage equals [`Usage::default`].
    pub fn set_default_usage(&self, usage: Option<Usage>) {
        self.state().default_usage = usage;
    }

    /// Returns detached snapshots, including calls made after exhaustion.
    #[must_use]
    pub fn calls(&self) -> Vec<ModelCall> {
        self.state().calls.clone()
    }

    /// Returns the first recorded call.
    #[must_use]
    pub fn first_call(&self) -> Option<ModelCall> {
        self.state().calls.first().cloned()
    }

    /// Returns the most recent recorded call.
    #[must_use]
    pub fn last_call(&self) -> Option<ModelCall> {
        self.state().calls.last().cloned()
    }

    /// Number of queued steps not yet consumed.
    #[must_use]
    pub fn remaining_steps(&self) -> usize {
        self.state().steps.len()
    }

    /// Appends one step.
    pub fn enqueue(&self, step: impl Into<ModelStep>) {
        let step = step.into();
        self.state().steps.push_back(step);
    }

    /// Converts all steps before atomically appending them to the queue.
    pub fn extend(&self, steps: impl IntoIterator<Item = impl Into<ModelStep>>) {
        let steps: Vec<_> = steps.into_iter().map(Into::into).collect();
        self.state().steps.extend(steps);
    }

    /// Fails when the script still contains unconsumed steps.
    pub fn assert_complete(&self) -> Result<()> {
        let remaining_steps = self.remaining_steps();
        if remaining_steps == 0 {
            Ok(())
        } else {
            let error = UnconsumedModelSteps::new(remaining_steps);
            Err(Error::caller(error.to_string()).with_source(error))
        }
    }

    fn state(&self) -> MutexGuard<'_, ScriptState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take_step(&self, request: ModelRequest, streamed: bool) -> Result<(ModelCall, ModelStep)> {
        let call = ModelCall { request, streamed };
        let mut state = self.state();
        let call_index = state.calls.len();
        state.calls.push(call.clone());
        if let Some(step) = state.steps.pop_front() {
            Ok((call, step))
        } else {
            let error = UnexpectedModelCall::new(call, call_index);
            Err(Error::caller(error.to_string()).with_source(error))
        }
    }

    async fn resolve_step(&self, call: &ModelCall, mut step: ModelStep) -> Result<ModelStep> {
        loop {
            match step.outcome {
                StepOutcome::Responder(responder) => step = responder(call.clone()).await,
                StepOutcome::Error(mut error, advice) => {
                    errors::configure_retry_advice(&mut error, Arc::clone(&self.identity), advice);
                    return Err(error);
                }
                _ => {
                    if step.response.usage() == &Usage::default() {
                        let usage = self
                            .state()
                            .default_usage
                            .clone()
                            .unwrap_or_else(|| Usage::from_request(RequestUsage::new(0, 0)));
                        step.response = step.response.with_usage(usage);
                    }
                    return Ok(step);
                }
            }
        }
    }
}

impl Default for ScriptedModel {
    fn default() -> Self {
        Self::new(Vec::<ModelStep>::new())
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let span = stream::generation_span(self.emit_traces, false);
        let outcome = stream::GenerationOutcome::new(span.clone());
        let result = async {
            let (call, step) = self.take_step(request, false)?;
            Ok(self.resolve_step(&call, step).await?.response)
        }
        .instrument(span)
        .await;
        outcome.settle(&result);
        result
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        let prepare = async move {
            let result = async {
                let (call, step) = self.take_step(request, true)?;
                let step = self.resolve_step(&call, step).await?;
                match step.outcome {
                    StepOutcome::Stream(events) => {
                        Ok(Box::pin(futures_stream::iter(events.into_iter().map(Ok)))
                            as ModelStream<'static>)
                    }
                    StepOutcome::StreamFactory(factory) => {
                        let identity = Arc::clone(&self.identity);
                        Ok(factory(call)
                            .map(move |result| {
                                result.map_err(|mut error| {
                                    errors::configure_retry_advice(
                                        &mut error,
                                        Arc::clone(&identity),
                                        None,
                                    );
                                    error
                                })
                            })
                            .boxed())
                    }
                    _ => stream::automatic_events(step.response).map(|events| {
                        Box::pin(futures_stream::iter(events.into_iter().map(Ok)))
                            as ModelStream<'static>
                    }),
                }
            }
            .await;
            result.unwrap_or_else(|error| Box::pin(futures_stream::once(async { Err(error) })))
        };
        stream::instrument(
            Box::pin(futures_stream::once(prepare).flatten()),
            self.emit_traces,
        )
    }

    fn get_retry_advice(&self, request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
        errors::retry_advice(request.error(), &self.identity)
    }
}

/// Returns the same injected model for every requested name, including the provider default.
///
/// This ports the upstream single-model provider used to run scripted models through a resolver.
/// Register it with the host's existing provider registry; it owns no protocol or credentials.
#[non_exhaustive]
#[derive(Clone)]
pub struct ScriptedProvider {
    model: Arc<dyn Model>,
}

impl ScriptedProvider {
    /// Creates a provider sharing one model and its script across all resolved names.
    #[must_use]
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn get_model(&self, _model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        Ok(Arc::clone(&self.model))
    }
}

/// Builds an assistant message, using `scripted-message` when no item ID is provided.
#[must_use]
pub fn assistant_message(text: impl Into<String>, item_id: Option<&str>) -> RunItem {
    RunItem::new(
        ItemId::new(item_id.unwrap_or("scripted-message")),
        RunItemKind::Message(Message::text(MessageRole::Assistant, text)),
    )
}

/// Builds a function call with parsed arguments, defaulting the item ID to the call ID.
///
/// The core boundary represents arguments as JSON values rather than provider JSON strings.
/// Namespaces and other call metadata can be configured on [`ToolCall`] and wrapped in a run item.
#[must_use]
pub fn function_call(
    name: impl Into<String>,
    arguments: serde_json::Value,
    call_id: &str,
    item_id: Option<&str>,
    namespace: Option<&str>,
) -> RunItem {
    let mut call = ToolCall::new(CallId::new(call_id), name, arguments);
    if let Some(namespace) = namespace {
        call = call.with_namespace(namespace);
    }
    RunItem::new(
        ItemId::new(item_id.unwrap_or(call_id)),
        RunItemKind::ToolCall(call),
    )
}
