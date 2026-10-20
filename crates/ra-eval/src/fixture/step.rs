//! Owned scripted outcomes, with mutually exclusive outcomes enforced by construction.

use std::{future::Future, sync::Arc};

use futures::future::BoxFuture;
use ra_core::{
    error::Error,
    item::{ModelResponse, RunItem},
    model::{ModelStream, ModelStreamEvent, RetryAdvice},
    usage::Usage,
};

use super::ModelCall;

/// Resolves a call to another step, asynchronously if needed.
pub type ModelResponder = Arc<dyn Fn(ModelCall) -> BoxFuture<'static, ModelStep> + Send + Sync>;

/// Creates an exact stream for a captured call. Dropping the model stream drops this stream too.
pub type ModelStreamFactory = Arc<dyn Fn(ModelCall) -> ModelStream<'static> + Send + Sync>;

pub(super) enum StepOutcome {
    Response,
    Error(Error, Option<RetryAdvice>),
    Responder(ModelResponder),
    Stream(Vec<ModelStreamEvent>),
    StreamFactory(ModelStreamFactory),
}

/// One scripted model outcome.
///
/// Values are owned because framework errors and async streams are not clonable. Construct a new
/// script for each independent run. Exact-stream steps retain a non-streaming fallback response.
#[non_exhaustive]
pub struct ModelStep {
    pub(super) response: ModelResponse,
    pub(super) outcome: StepOutcome,
}

impl ModelStep {
    /// Creates a response step with zero usage and the stable default ID `resp-789`.
    #[must_use]
    pub fn new(output: Vec<RunItem>) -> Self {
        Self {
            response: ModelResponse::new(output)
                .with_usage(Usage::default())
                .with_response_id("resp-789"),
            outcome: StepOutcome::Response,
        }
    }

    /// Creates a step that returns the owned error with unchanged classification and display.
    ///
    /// Advice is stored in a node that becomes the error's immediate source, with the original
    /// source beneath it: walk the whole chain rather than downcasting only the first source. The
    /// core's cancellation, budget and guardrail variants have no source slot, so they cannot
    /// carry advice and remain unchanged.
    #[must_use]
    pub fn raise_error(error: Error, retry_advice: Option<RetryAdvice>) -> Self {
        Self {
            outcome: StepOutcome::Error(error, retry_advice),
            ..Self::default()
        }
    }

    /// Creates a synchronous responder, which may return another responder step.
    #[must_use]
    pub fn respond(responder: impl Fn(ModelCall) -> ModelStep + Send + Sync + 'static) -> Self {
        Self::respond_async(move |call| futures::future::ready(responder(call)))
    }

    /// Creates an asynchronous responder, evaluated outside the script lock.
    #[must_use]
    pub fn respond_async<F, Fut>(responder: F) -> Self
    where
        F: Fn(ModelCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ModelStep> + Send + 'static,
    {
        Self {
            outcome: StepOutcome::Responder(Arc::new(move |call| Box::pin(responder(call)))),
            ..Self::default()
        }
    }

    /// Uses exactly these events when streamed, without appending a terminal response.
    #[must_use]
    pub fn stream(events: Vec<ModelStreamEvent>) -> Self {
        Self {
            outcome: StepOutcome::Stream(events),
            ..Self::default()
        }
    }

    /// Creates an exact stream dynamically from the captured call.
    #[must_use]
    pub fn stream_factory(
        factory: impl Fn(ModelCall) -> ModelStream<'static> + Send + Sync + 'static,
    ) -> Self {
        Self {
            outcome: StepOutcome::StreamFactory(Arc::new(factory)),
            ..Self::default()
        }
    }

    /// Replaces the response, including usage and optional transport IDs, without changing outcome.
    /// This also configures the non-streaming fallback of an exact-stream step.
    #[must_use]
    pub fn with_response(mut self, response: ModelResponse) -> Self {
        self.response = response;
        self
    }

    /// Sets usage. Only usage equal to the all-zero default receives the model's default usage.
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.response = self.response.with_usage(usage);
        self
    }

    /// Sets a response ID, preserving empty strings.
    #[must_use]
    pub fn with_response_id(mut self, response_id: impl Into<String>) -> Self {
        self.response = self.response.with_response_id(response_id);
        self
    }

    /// Sets the request ID returned with the response.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.response = self.response.with_request_id(request_id);
        self
    }
}

impl Default for ModelStep {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl From<ModelResponse> for ModelStep {
    fn from(response: ModelResponse) -> Self {
        Self {
            response,
            outcome: StepOutcome::Response,
        }
    }
}

impl From<Vec<RunItem>> for ModelStep {
    fn from(output: Vec<RunItem>) -> Self {
        Self::new(output)
    }
}

impl From<Error> for ModelStep {
    fn from(error: Error) -> Self {
        Self::raise_error(error, None)
    }
}
