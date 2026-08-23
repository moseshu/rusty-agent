//! Runner-managed model retries and the boundary between an unstarted and a published stream.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    error::{Error, ProviderErrorKind, Result},
    item::{ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelRetrySettings, ModelSelector,
        ModelSettings, ModelStream, ModelStreamEvent, NetworkErrorRetryPolicy, ProviderKey,
        RawResponseEvent, ReplaySafety, ResolvedModel, RetryAdvice, RetryBackoffSettings,
    },
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunRequest, RunStreamEvent, Runner},
};
use serde_json::json;

fn network_error() -> Error {
    ra_core::model::NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset")
        .into_error()
}

fn network_error_with_retry_after(delay: std::time::Duration) -> Error {
    ra_core::model::NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset")
        .with_retry_after(delay)
        .into_error()
}

fn answer() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("answer"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )])
}

struct RetryingModel {
    responses: Mutex<Vec<Result<ModelResponse>>>,
    calls: AtomicUsize,
    advice: Option<RetryAdvice>,
}

impl RetryingModel {
    fn new(responses: Vec<Result<ModelResponse>>, advice: Option<RetryAdvice>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            calls: AtomicUsize::new(0),
            advice,
        })
    }
}

#[async_trait]
impl Model for RetryingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses.lock().unwrap().remove(0)
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }

    fn get_retry_advice(
        &self,
        _request: &ra_core::model::ModelRetryAdviceRequest<'_>,
    ) -> Option<RetryAdvice> {
        self.advice.clone()
    }
}

struct StreamThenFailModel {
    calls: AtomicUsize,
}

#[async_trait]
impl Model for StreamThenFailModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("this fixture only supports streaming"))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        stream::iter(vec![
            Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                ProviderKey::new("test-provider"),
                "response.created",
                json!({}),
            ))),
            Err(network_error()),
        ])
        .boxed()
    }
}

struct Resolver {
    model: Arc<dyn Model>,
}

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("test-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn retry_config(streaming: bool, max_retries: u32, delay: std::time::Duration) -> RunConfig {
    let retry = ModelRetrySettings::new()
        .with_max_retries(max_retries)
        .with_backoff(
            RetryBackoffSettings::new()
                .with_initial_delay(delay)
                .with_jitter(false),
        )
        .with_policy(Arc::new(NetworkErrorRetryPolicy));
    RunConfig::new()
        .with_partial_messages(streaming)
        .with_model_settings(ModelSettings::new().with_retry(retry))
}

fn request(
    model: Arc<dyn Model>,
    cancel: &CancelScope,
    streaming: bool,
    max_retries: u32,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(
            AgentSpec::builder()
                .id(AgentId::new("retry-agent"))
                .name("Retry agent")
                .instructions("answer")
                .build()
                .unwrap(),
        ),
        Arc::new(Resolver { model }),
        RunId::new("retry-run"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("hello"))],
    )
    .with_config(retry_config(
        streaming,
        max_retries,
        std::time::Duration::ZERO,
    ))
}

#[tokio::test]
async fn a_safe_non_streaming_failure_retries_once_and_keeps_the_attempt_in_usage() {
    let model = RetryingModel::new(vec![Err(network_error()), Ok(answer())], None);
    let cancel = CancelScope::root();

    let result = Runner::run(request(
        Arc::clone(&model) as Arc<dyn Model>,
        &cancel,
        false,
        1,
    ))
    .await
    .unwrap();

    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.turns(), 1, "a retry is not a second agent turn");
    assert_eq!(result.usage().requests(), 2);
    assert_eq!(result.usage().request_usage_entries().len(), 2);
}

#[tokio::test]
async fn an_adapter_marking_replay_unsafe_vetoes_even_a_retryable_failure() {
    let model = RetryingModel::new(
        vec![Err(network_error()), Ok(answer())],
        Some(RetryAdvice::new().with_replay_safety(ReplaySafety::Unsafe)),
    );
    let cancel = CancelScope::root();

    let error = Runner::run(request(
        Arc::clone(&model) as Arc<dyn Model>,
        &cancel,
        false,
        1,
    ))
    .await
    .expect_err("unsafe replay must not open a second request");

    assert_eq!(error.code(), "provider.network");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_published_stream_event_closes_the_retry_window() {
    let model = Arc::new(StreamThenFailModel {
        calls: AtomicUsize::new(0),
    });
    let cancel = CancelScope::root();
    let mut stream = Runner::run_streamed(request(
        Arc::clone(&model) as Arc<dyn Model>,
        &cancel,
        true,
        1,
    ));
    let mut raw_events = 0;
    while let Some(event) = stream.next_event().await {
        if matches!(event, RunStreamEvent::RawResponse(_)) {
            raw_events += 1;
        }
    }
    let error = stream
        .finish()
        .await
        .expect_err("a published stream must not be replayed");

    assert_eq!(raw_events, 1);
    assert_eq!(error.code(), "provider.network");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_explicit_zero_retry_budget_does_not_call_the_policy_again() {
    let model = RetryingModel::new(vec![Err(network_error()), Ok(answer())], None);
    let cancel = CancelScope::root();

    let error = Runner::run(request(
        Arc::clone(&model) as Arc<dyn Model>,
        &cancel,
        false,
        0,
    ))
    .await
    .expect_err("zero retry budget must fail on the first attempt");

    assert_eq!(error.code(), "provider.network");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_backoff_stops_the_wait_without_sending_the_retry() {
    let model = RetryingModel::new(vec![Err(network_error()), Ok(answer())], None);
    let cancel = CancelScope::root();
    let request = request(Arc::clone(&model) as Arc<dyn Model>, &cancel, false, 1)
        .with_config(retry_config(false, 1, std::time::Duration::from_secs(60)));
    let task = tokio::spawn(Runner::run(request));

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while model.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the initial call should have started");
    cancel.cancel(CancelReason::UserInterrupt);

    let error = task
        .await
        .expect("runner task should not panic")
        .expect_err("cancellation must interrupt backoff");
    assert!(error.is_cancelled());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn normalized_retry_after_is_honored_without_adapter_advice() {
    let delay = std::time::Duration::from_millis(25);
    let model = RetryingModel::new(
        vec![Err(network_error_with_retry_after(delay)), Ok(answer())],
        None,
    );
    let cancel = CancelScope::root();
    let start = std::time::Instant::now();

    Runner::run(request(
        Arc::clone(&model) as Arc<dyn Model>,
        &cancel,
        false,
        1,
    ))
    .await
    .expect("normalized retry-after should permit the retry");

    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(
        start.elapsed() >= delay,
        "the retry must honor the normalized retry-after rather than the zero configured backoff"
    );
}
