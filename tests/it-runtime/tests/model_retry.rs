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
    runner::{RunConfig, RunRequest, RunStreamEvent, Runner, TransportFallback},
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
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
}

impl RetryingModel {
    fn new(responses: Vec<Result<ModelResponse>>, advice: Option<RetryAdvice>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            calls: AtomicUsize::new(0),
            advice,
            inputs: Mutex::default(),
        })
    }
}

#[async_trait]
impl Model for RetryingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses.lock().unwrap().remove(0)
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        self.inputs.lock().unwrap().push(request.input().to_vec());
        self.calls.fetch_add(1, Ordering::SeqCst);
        let event = self
            .responses
            .lock()
            .unwrap()
            .remove(0)
            .map(|response| ModelStreamEvent::Completed(Box::new(response)));
        stream::iter(vec![event]).boxed()
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

/// The mirror of the test above, and the reason the switch is worth having: the same failing
/// stream stays replayable when its narration was never forwarded, because nothing a second
/// attempt could duplicate ever left the runtime.
#[tokio::test]
async fn a_stream_nobody_subscribed_to_keeps_its_retry_window_open() {
    let model = Arc::new(StreamThenFailModel {
        calls: AtomicUsize::new(0),
    });
    let cancel = CancelScope::root();
    let mut stream = Runner::run_streamed(request(
        Arc::clone(&model) as Arc<dyn Model>,
        &cancel,
        false,
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
        .expect_err("both attempts fail, so the run does too");

    assert_eq!(
        raw_events, 0,
        "narration is off, so the frame the adapter emitted must not reach the subscriber"
    );
    assert_eq!(error.code(), "provider.network");
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        2,
        "an unpublished stream failure is still replayable"
    );
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

struct CacheRetryModel {
    inner: Arc<RetryingModel>,
    keys: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl Model for CacheRetryModel {
    fn supports_default_prompt_cache_key(&self, _: &ra_core::model::ResolvedModelSettings) -> bool {
        true
    }
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.keys
            .lock()
            .unwrap()
            .push(request.prompt_cache_key().map(str::to_owned));
        self.inner.get_response(request).await
    }
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        self.keys
            .lock()
            .unwrap()
            .push(request.prompt_cache_key().map(str::to_owned));
        self.inner.stream_response(request)
    }
}

#[tokio::test]
async fn retries_reuse_the_same_generated_cache_key() {
    for streaming in [false, true] {
        let model = Arc::new(CacheRetryModel {
            inner: RetryingModel::new(vec![Err(network_error()), Ok(answer())], None),
            keys: Mutex::default(),
        });
        let result = Runner::run(request(model.clone(), &CancelScope::root(), streaming, 1))
            .await
            .unwrap();
        let key = result.state().generated_prompt_cache_key().unwrap();
        assert_eq!(*model.keys.lock().unwrap(), vec![Some(key.to_owned()); 2]);
    }
}

struct AcceptFailure {
    calls: AtomicUsize,
}

#[async_trait]
impl ra_core::model::ModelRetryPolicy for AcceptFailure {
    async fn evaluate(
        &self,
        _context: &ra_core::model::RetryPolicyContext<'_>,
    ) -> ra_core::model::RetryDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ra_core::model::RetryDecision::retry().with_delay(std::time::Duration::ZERO)
    }
}

#[tokio::test]
async fn a_custom_policy_sees_nontransient_failures_but_cannot_override_cancellation() {
    for error in [
        Error::caller("rebuild request"),
        Error::provider(ProviderErrorKind::Behavior, "invalid frame"),
        Error::provider(ProviderErrorKind::Auth, "request refused"),
        Error::cancelled("stop"),
    ] {
        let cancelled = error.is_cancelled();
        let policy = Arc::new(AcceptFailure {
            calls: AtomicUsize::new(0),
        });
        let model = RetryingModel::new(vec![Err(error), Ok(answer())], None);
        let request = request(model.clone(), &CancelScope::root(), false, 1).with_config(
            RunConfig::new().with_model_settings(
                ModelSettings::new().with_retry(
                    ModelRetrySettings::new()
                        .with_max_retries(1)
                        .with_policy(policy.clone()),
                ),
            ),
        );
        let result = Runner::run(request).await;
        assert_eq!(result.is_ok(), !cancelled);
        assert_eq!(policy.calls.load(Ordering::SeqCst), usize::from(!cancelled));
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            if cancelled { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn builtin_policies_decide_nontransient_failures_without_a_runner_classification_gate() {
    use ra_core::model::{
        ModelRetryPolicy, NeverRetryPolicy, ProviderSuggestedRetryPolicy, RetryAfterPolicy,
    };
    for (policy, advice, retried) in [
        (
            Arc::new(NeverRetryPolicy) as Arc<dyn ModelRetryPolicy>,
            None,
            false,
        ),
        (Arc::new(NetworkErrorRetryPolicy), None, false),
        (Arc::new(RetryAfterPolicy), None, false),
        (Arc::new(ProviderSuggestedRetryPolicy), None, false),
        (
            Arc::new(ProviderSuggestedRetryPolicy),
            Some(
                RetryAdvice::new()
                    .with_suggested(true)
                    .with_retry_after(std::time::Duration::ZERO),
            ),
            true,
        ),
        (
            Arc::new(RetryAfterPolicy),
            Some(RetryAdvice::new().with_retry_after(std::time::Duration::ZERO)),
            true,
        ),
    ] {
        let model = RetryingModel::new(
            vec![
                Err(Error::provider(
                    ProviderErrorKind::Behavior,
                    "malformed response",
                )),
                Ok(answer()),
            ],
            advice,
        );
        let request = request(model.clone(), &CancelScope::root(), false, 1).with_config(
            RunConfig::new().with_model_settings(
                ModelSettings::new().with_retry(
                    ModelRetrySettings::new()
                        .with_max_retries(1)
                        .with_policy(policy),
                ),
            ),
        );
        assert_eq!(Runner::run(request).await.is_ok(), retried);
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            if retried { 2 } else { 1 }
        );
    }
}

struct GuidanceRecovery;

impl ra_runtime::runner::ModelRetryRecovery for GuidanceRecovery {
    fn recovery_message(&self, _error: &Error, _model: Option<&str>) -> Option<Message> {
        Some(Message::developer("offer a permitted alternative"))
    }

    fn report_retry(&self, _request: &ModelRequest, retry: u32) -> bool {
        retry > 1
    }
}

#[tokio::test]
async fn recovery_is_recorded_before_the_budget_check_and_retry_notices_can_be_suppressed() {
    use ra_core::session::SessionId;
    use ra_runtime::thread::{AgentThread, ThreadEvent, TurnInputRequest, TurnInputSubmission};
    for budget in [0, 2] {
        let model = RetryingModel::new(
            vec![Err(network_error()), Err(network_error()), Ok(answer())],
            None,
        );
        let thread = AgentThread::builder(
            SessionId::new("retry-recovery"),
            AgentBinding::direct(
                AgentSpec::builder()
                    .id(AgentId::new("lead"))
                    .name("Lead")
                    .instructions("answer")
                    .build()
                    .unwrap(),
            ),
            Arc::new(Resolver {
                model: model.clone(),
            }),
        )
        .with_config(
            retry_config(false, budget, std::time::Duration::ZERO)
                .with_model_retry_recovery(Arc::new(GuidanceRecovery)),
        )
        .build();
        assert!(matches!(
            thread
                .start_or_steer_turn(TurnInputRequest::new(vec![
                    ra_core::item::ContentBlock::text("hello")
                ]))
                .await,
            TurnInputSubmission::Started { .. }
        ));
        let mut notices = Vec::new();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), thread.next_event())
                    .await
                    .unwrap()
                    .unwrap();
            match event {
                ThreadEvent::StreamError { message, .. } => notices.push(message),
                ThreadEvent::TurnComplete { .. } | ThreadEvent::TurnAborted { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            notices,
            if budget == 0 {
                vec![]
            } else {
                vec!["Reconnecting... 2/2".to_owned()]
            }
        );
        let history = serde_json::to_string(&thread.history()).unwrap();
        assert!(history.contains("offer a permitted alternative"));
        let inputs = model.inputs.lock().unwrap();
        assert_eq!(inputs.len(), if budget == 0 { 1 } else { 3 });
        if budget > 0 {
            assert!(
                serde_json::to_string(&inputs[1])
                    .unwrap()
                    .contains("offer a permitted alternative")
            );
        }
    }
}

fn connect_error() -> Error {
    ra_core::model::NormalizedProviderError::new(ProviderErrorKind::Network, "connect refused")
        .into_error()
}

/// Retries a refused connection outside the budget, as Codex waits for the network.
#[derive(Default)]
struct ConnectionRecovery {
    previous: Mutex<Vec<u32>>,
}

impl ra_runtime::runner::ModelRetryRecovery for ConnectionRecovery {
    fn budget_exempt_retry(
        &self,
        error: &Error,
        previous: u32,
    ) -> Option<ra_runtime::runner::BudgetExemptRetry> {
        self.previous.lock().unwrap().push(previous);
        error.to_string().contains("connect refused").then(|| {
            ra_runtime::runner::BudgetExemptRetry::new(
                std::time::Duration::ZERO,
                "Reconnecting... waiting for network",
            )
        })
    }
}

#[tokio::test]
async fn budget_exempt_retries_leave_the_retry_budget_for_other_failures() {
    use ra_core::session::SessionId;
    use ra_runtime::thread::{AgentThread, ThreadEvent, TurnInputRequest, TurnInputSubmission};
    let model = RetryingModel::new(
        vec![
            Err(connect_error()),
            Err(connect_error()),
            Err(network_error()),
            Ok(answer()),
        ],
        None,
    );
    let recovery = Arc::new(ConnectionRecovery::default());
    let thread = AgentThread::builder(
        SessionId::new("connection-recovery"),
        AgentBinding::direct(
            AgentSpec::builder()
                .id(AgentId::new("lead"))
                .name("Lead")
                .instructions("answer")
                .build()
                .unwrap(),
        ),
        Arc::new(Resolver {
            model: model.clone(),
        }),
    )
    .with_config(
        retry_config(false, 1, std::time::Duration::ZERO)
            .with_model_retry_recovery(recovery.clone()),
    )
    .build();
    assert!(matches!(
        thread
            .start_or_steer_turn(TurnInputRequest::new(vec![
                ra_core::item::ContentBlock::text("hello")
            ]))
            .await,
        TurnInputSubmission::Started { .. }
    ));
    let mut notices = Vec::new();
    let completed = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), thread.next_event())
            .await
            .unwrap()
            .unwrap();
        match event {
            ThreadEvent::StreamError { message, .. } => notices.push(message),
            ThreadEvent::TurnComplete { .. } => break true,
            ThreadEvent::TurnAborted { .. } => break false,
            _ => {}
        }
    };
    // Two refused connections wait outside the budget of one, which the reset still gets.
    assert!(completed);
    assert_eq!(
        notices,
        [
            "Reconnecting... waiting for network",
            "Reconnecting... waiting for network",
            "Reconnecting... 1/1",
        ]
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 4);
    assert_eq!(*recovery.previous.lock().unwrap(), [0, 1, 2]);
}

#[tokio::test]
async fn budget_exempt_retries_respect_the_replay_boundary() {
    let recovery = Arc::new(ConnectionRecovery::default());
    let model = RetryingModel::new(
        vec![Err(connect_error()), Ok(answer())],
        Some(RetryAdvice::new().with_replay_safety(ReplaySafety::Unsafe)),
    );
    let request = request(model.clone(), &CancelScope::root(), false, 0).with_config(
        retry_config(false, 0, std::time::Duration::ZERO).with_model_retry_recovery(recovery),
    );
    assert!(Runner::run(request).await.is_err());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

/// An adapter that records every advice request and, when `veto_after_first` is set, permits
/// replaying the first failure only.
struct AttemptAwareModel {
    inner: Arc<RetryingModel>,
    advice_attempts: Mutex<Vec<u32>>,
    veto_after_first: bool,
}

#[async_trait]
impl Model for AttemptAwareModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.inner.get_response(request).await
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        self.inner.stream_response(request)
    }

    fn get_retry_advice(
        &self,
        request: &ra_core::model::ModelRetryAdviceRequest<'_>,
    ) -> Option<RetryAdvice> {
        self.advice_attempts.lock().unwrap().push(request.attempt());
        self.veto_after_first.then(|| {
            RetryAdvice::new().with_replay_safety(if request.attempt() == 1 {
                ReplaySafety::Safe
            } else {
                ReplaySafety::Unsafe
            })
        })
    }
}

#[tokio::test]
async fn budget_exempt_retries_preserve_adapter_attempts_after_a_budgeted_retry() {
    for budget in [1, 2] {
        let inner = RetryingModel::new(
            vec![Err(network_error()), Err(connect_error()), Ok(answer())],
            None,
        );
        let model = Arc::new(AttemptAwareModel {
            inner: inner.clone(),
            advice_attempts: Mutex::default(),
            veto_after_first: true,
        });
        let recovery = Arc::new(ConnectionRecovery::default());
        let cancel = CancelScope::root();
        let request = request(model.clone(), &cancel, false, budget).with_config(
            retry_config(false, budget, std::time::Duration::ZERO)
                .with_model_retry_recovery(recovery.clone()),
        );

        // The connection failure is the first exempt candidate, but the adapter must still see
        // the budgeted retry already taken, even when that retry exhausted the budget. It is
        // asked once per failure, and the exempt and budgeted decisions share that answer.
        assert!(Runner::run(request).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(*recovery.previous.lock().unwrap(), [0, 0]);
        assert_eq!(*model.advice_attempts.lock().unwrap(), [1, 2]);
    }
}

/// A policy that records what it is asked and always retries at once.
#[derive(Default)]
struct RecordingPolicy {
    seen: Mutex<Vec<(u32, u32)>>,
}

#[async_trait]
impl ra_core::model::ModelRetryPolicy for RecordingPolicy {
    async fn evaluate(
        &self,
        context: &ra_core::model::RetryPolicyContext<'_>,
    ) -> ra_core::model::RetryDecision {
        self.seen
            .lock()
            .unwrap()
            .push((context.attempt(), context.max_retries()));
        ra_core::model::RetryDecision::retry().with_delay(std::time::Duration::ZERO)
    }
}

/// Attempt numbers and advice timing follow the OpenAI Agents SDK runner: numbers start at one,
/// the adapter is asked about every failure that was not cancelled, also once the budget is spent
/// or when no policy can retry, and the policy only while the budget lasts. Asking the adapter
/// never adds a retry.
#[tokio::test]
async fn attempt_numbers_and_advice_follow_the_sdk_runner() {
    struct Case {
        name: &'static str,
        error: fn() -> Error,
        max_retries: Option<u32>,
        with_policy: bool,
        calls: usize,
        succeeds: bool,
        advice: &'static [u32],
        policy: &'static [(u32, u32)],
    }
    let cases = [
        Case {
            name: "budget of two",
            error: network_error,
            max_retries: Some(2),
            with_policy: true,
            calls: 3,
            succeeds: false,
            advice: &[1, 2, 3],
            policy: &[(1, 2), (2, 2)],
        },
        Case {
            name: "budget of three",
            error: network_error,
            max_retries: Some(3),
            with_policy: true,
            calls: 4,
            succeeds: true,
            advice: &[1, 2, 3],
            policy: &[(1, 3), (2, 3), (3, 3)],
        },
        Case {
            name: "zero budget",
            error: network_error,
            max_retries: Some(0),
            with_policy: true,
            calls: 1,
            succeeds: false,
            advice: &[1],
            policy: &[],
        },
        Case {
            name: "no retry settings",
            error: network_error,
            max_retries: None,
            with_policy: false,
            calls: 1,
            succeeds: false,
            advice: &[1],
            policy: &[],
        },
        Case {
            name: "budget without a policy",
            error: network_error,
            max_retries: Some(2),
            with_policy: false,
            calls: 1,
            succeeds: false,
            advice: &[1],
            policy: &[],
        },
        Case {
            name: "cancellation",
            error: || Error::cancelled("stop"),
            max_retries: Some(2),
            with_policy: true,
            calls: 1,
            succeeds: false,
            advice: &[],
            policy: &[],
        },
    ];
    for case in cases {
        let inner = RetryingModel::new(
            vec![
                Err((case.error)()),
                Err((case.error)()),
                Err((case.error)()),
                Ok(answer()),
            ],
            None,
        );
        let model = Arc::new(AttemptAwareModel {
            inner: inner.clone(),
            advice_attempts: Mutex::default(),
            veto_after_first: false,
        });
        let policy = Arc::new(RecordingPolicy::default());
        let mut settings = ModelSettings::new();
        if let Some(max_retries) = case.max_retries {
            let mut retry = ModelRetrySettings::new().with_max_retries(max_retries);
            if case.with_policy {
                retry = retry.with_policy(policy.clone());
            }
            settings = settings.with_retry(retry);
        }
        let request = request(model.clone(), &CancelScope::root(), false, 0)
            .with_config(RunConfig::new().with_model_settings(settings));

        let result = Runner::run(request).await;
        assert_eq!(result.is_ok(), case.succeeds, "{}", case.name);
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            case.calls,
            "{}",
            case.name
        );
        assert_eq!(
            *model.advice_attempts.lock().unwrap(),
            case.advice,
            "{}",
            case.name
        );
        assert_eq!(*policy.seen.lock().unwrap(), case.policy, "{}", case.name);
    }
}

/// What a retry policy saw, so a test can check the context it scoped its approval with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenContext {
    streaming: bool,
    replay_safety: ReplaySafety,
    response_started: bool,
    stateful: bool,
}

/// A policy that always retries, approving unsafe replays when `approve` is set, and records the
/// context it was given.
struct ApprovingPolicy {
    approve: bool,
    seen: Mutex<Vec<SeenContext>>,
}

impl ApprovingPolicy {
    fn new(approve: bool) -> Arc<Self> {
        Arc::new(Self {
            approve,
            seen: Mutex::default(),
        })
    }
}

#[async_trait]
impl ra_core::model::ModelRetryPolicy for ApprovingPolicy {
    async fn evaluate(
        &self,
        context: &ra_core::model::RetryPolicyContext<'_>,
    ) -> ra_core::model::RetryDecision {
        self.seen.lock().unwrap().push(SeenContext {
            streaming: context.is_streaming(),
            replay_safety: context.replay_safety(),
            response_started: context.response_started(),
            stateful: context.is_stateful_request(),
        });
        let decision = ra_core::model::RetryDecision::retry().with_delay(std::time::Duration::ZERO);
        if self.approve {
            decision.with_unsafe_replay_approval()
        } else {
            decision
        }
    }
}

/// The advice the SDK's WebSocket adapter gives a failure after the first response event.
fn started_unsafe_advice() -> RetryAdvice {
    RetryAdvice::new()
        .with_suggested(false)
        .with_replay_safety(ReplaySafety::Unsafe)
        .with_response_started(true)
}

fn bare_request(continuation: ra_core::model::ConversationContinuation) -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        ra_core::model::SettingsLayers::new().resolve(&ProviderKey::new("test-provider")),
    )
    .with_continuation(continuation)
}

/// Whether the runtime would retry the first failure of `request` under `policy`, with the
/// adapter answering `advice`.
async fn retries_first_failure(
    advice: Option<RetryAdvice>,
    policy: Arc<ApprovingPolicy>,
    request: &ModelRequest,
    streaming: bool,
) -> bool {
    let model: Arc<dyn Model> = RetryingModel::new(Vec::new(), advice);
    let settings = ModelRetrySettings::new()
        .with_max_retries(1)
        .with_policy(policy);
    ra_runtime::runner::retry_test_api::decide_first_failure(
        &model,
        request,
        &network_error(),
        &settings,
        streaming,
        &CancelScope::root(),
    )
    .await
    .unwrap()
    .is_some()
}

/// A non-streaming failure the adapter marked unsafe reaches the policy, which sees the structured
/// facts it needs to scope an approval, and is replayed only when the policy approves unsafe
/// replay explicitly: an ordinary retry decision never bypasses replay protection.
#[tokio::test]
async fn an_unsafe_non_streaming_failure_replays_only_with_explicit_approval() {
    let request = bare_request(ra_core::model::ConversationContinuation::None);

    let approving = ApprovingPolicy::new(true);
    assert!(
        retries_first_failure(
            Some(started_unsafe_advice()),
            approving.clone(),
            &request,
            false
        )
        .await
    );
    assert_eq!(
        *approving.seen.lock().unwrap(),
        [SeenContext {
            streaming: false,
            replay_safety: ReplaySafety::Unsafe,
            response_started: true,
            stateful: false,
        }]
    );

    let ordinary = ApprovingPolicy::new(false);
    assert!(
        !retries_first_failure(
            Some(started_unsafe_advice()),
            ordinary.clone(),
            &request,
            false
        )
        .await
    );
    assert_eq!(ordinary.seen.lock().unwrap().len(), 1);
}

/// A streamed failure the adapter marked unsafe stays blocked before the policy runs, so an
/// approval cannot replay it.
#[tokio::test]
async fn unsafe_replay_approval_is_ignored_for_streamed_calls() {
    let request = bare_request(ra_core::model::ConversationContinuation::None);
    let policy = ApprovingPolicy::new(true);
    assert!(
        !retries_first_failure(
            Some(started_unsafe_advice()),
            policy.clone(),
            &request,
            true
        )
        .await
    );
    assert!(policy.seen.lock().unwrap().is_empty());
}

/// A request continuing provider-held state fails closed: it is replayed when the adapter called
/// the failure safe, or when the policy approved a failure the adapter marked unsafe, but never
/// when replay safety is unknown, approval or not.
#[tokio::test]
async fn a_stateful_request_replays_only_on_safe_advice_or_an_approved_unsafe_failure() {
    let safe = RetryAdvice::new()
        .with_suggested(true)
        .with_replay_safety(ReplaySafety::Safe);
    let unknown = RetryAdvice::new().with_suggested(true);
    let cases = [
        ("safe advice, ordinary retry", Some(safe), false, true),
        (
            "unsafe advice, approved",
            Some(started_unsafe_advice()),
            true,
            true,
        ),
        (
            "unsafe advice, ordinary retry",
            Some(started_unsafe_advice()),
            false,
            false,
        ),
        (
            "unknown advice, ordinary retry",
            Some(unknown),
            false,
            false,
        ),
        ("no advice, approved", None, true, false),
    ];
    for continuation in [
        ra_core::model::ConversationContinuation::PreviousResponseId("resp_1".to_owned()),
        ra_core::model::ConversationContinuation::ConversationId(
            ra_core::model::ProviderConversationId::new("conv_1"),
        ),
    ] {
        let request = bare_request(continuation);
        for (name, advice, approve, retried) in cases.clone() {
            let policy = ApprovingPolicy::new(approve);
            assert_eq!(
                retries_first_failure(advice, policy.clone(), &request, false).await,
                retried,
                "{name}"
            );
            assert!(policy.seen.lock().unwrap()[0].stateful, "{name}");
        }
    }
}

/// One summary stream: a failure, a stream that closes before its terminal response, or an answer.
enum SummaryStep {
    Fail(Error),
    CloseEarly,
    Answer(ModelResponse),
}

/// A model whose summary requests are scripted streams, recording the streaming flag the runtime
/// reports when it asks for retry advice. Its non-streaming entry point fails, so a summary sent
/// through it shows up as an error.
struct SummaryModel {
    steps: Mutex<Vec<SummaryStep>>,
    streams: AtomicUsize,
    advice: RetryAdvice,
    advice_streaming: Mutex<Vec<bool>>,
    provider_retries_disabled: Mutex<Vec<bool>>,
}

impl SummaryModel {
    fn new(steps: Vec<SummaryStep>, advice: RetryAdvice) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps),
            streams: AtomicUsize::new(0),
            advice,
            advice_streaming: Mutex::default(),
            provider_retries_disabled: Mutex::default(),
        })
    }
}

#[async_trait]
impl Model for SummaryModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("summary requests stream"))
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        self.provider_retries_disabled
            .lock()
            .unwrap()
            .push(request.should_disable_provider_managed_retries());
        let created = Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
            ProviderKey::new("test-provider"),
            "response.created",
            json!({}),
        )));
        let events = match self.steps.lock().unwrap().remove(0) {
            SummaryStep::Fail(error) => vec![created, Err(error)],
            SummaryStep::CloseEarly => vec![created],
            SummaryStep::Answer(response) => {
                vec![created, Ok(ModelStreamEvent::Completed(Box::new(response)))]
            }
        };
        stream::iter(events).boxed()
    }

    fn get_retry_advice(
        &self,
        request: &ra_core::model::ModelRetryAdviceRequest<'_>,
    ) -> Option<RetryAdvice> {
        self.advice_streaming
            .lock()
            .unwrap()
            .push(request.is_streaming());
        Some(self.advice.clone())
    }
}

fn summary_answer() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("summary"),
        RunItemKind::Message(Message::assistant(
            r#"{"primary_request_and_intent":"intent","key_technical_concepts":"concepts","files_and_code_sections":"files","errors_and_fixes":"errors","problem_solving":"solving","pending_tasks":"tasks","current_work":"work","optional_next_step":"next"}"#,
            OutputPhase::Final,
        )),
    )])
}

/// Runs a thread's manual compaction against `model` under `policy` and reports whether the
/// summary replaced the history.
async fn compact_with(model: Arc<SummaryModel>, policy: Arc<ApprovingPolicy>) -> bool {
    use ra_core::session::SessionId;
    use ra_runtime::thread::{AgentThread, ThreadEvent};

    let thread = AgentThread::builder(
        SessionId::new("summary-retry"),
        AgentBinding::direct(
            AgentSpec::builder()
                .id(AgentId::new("lead"))
                .name("Lead")
                .instructions("answer")
                .build()
                .unwrap(),
        ),
        Arc::new(Resolver { model }),
    )
    .with_config(
        RunConfig::new()
            .with_capability(Arc::new(
                ra_context::compaction::CompactionCapability::default(),
            ))
            .with_compaction_retry(
                ModelRetrySettings::new()
                    .with_max_retries(1)
                    .with_policy(policy),
            ),
    )
    .build();
    thread.compact().unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), thread.next_event())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event,
            ThreadEvent::TurnComplete { .. } | ThreadEvent::TurnAborted { .. }
        ) {
            break;
        }
    }
    serde_json::to_string(&thread.history())
        .unwrap()
        .contains("intent")
}

/// A thread's compaction summary streams and is drained to its terminal response, as Codex's
/// `drain_to_completed` sends its compaction request. Nothing the failed stream carried was
/// forwarded, so its retry window stays open: a failure whose replay safety is unknown is retried,
/// and so is a stream that closed before its terminal response. A failure the adapter marked
/// unsafe after the response started is final for a streamed request, approval or not.
#[tokio::test]
async fn a_summary_request_streams_and_retries_within_the_replay_boundary() {
    let unknown = RetryAdvice::new().with_suggested(true);

    let model = SummaryModel::new(
        vec![
            SummaryStep::Fail(network_error()),
            SummaryStep::Answer(summary_answer()),
        ],
        unknown.clone(),
    );
    let policy = ApprovingPolicy::new(false);
    assert!(compact_with(model.clone(), policy.clone()).await);
    assert_eq!(model.streams.load(Ordering::SeqCst), 2);
    assert_eq!(*model.advice_streaming.lock().unwrap(), [true]);
    // The summary's retry is the runtime's, so the adapter's own retries stay off for it.
    assert_eq!(
        *model.provider_retries_disabled.lock().unwrap(),
        [false, true]
    );
    assert_eq!(
        *policy.seen.lock().unwrap(),
        [SeenContext {
            streaming: true,
            replay_safety: ReplaySafety::Unknown,
            response_started: false,
            stateful: false,
        }]
    );

    let model = SummaryModel::new(
        vec![
            SummaryStep::CloseEarly,
            SummaryStep::Answer(summary_answer()),
        ],
        unknown,
    );
    let policy = ApprovingPolicy::new(false);
    assert!(compact_with(model.clone(), policy.clone()).await);
    assert_eq!(model.streams.load(Ordering::SeqCst), 2);
    assert_eq!(policy.seen.lock().unwrap().len(), 1);

    let model = SummaryModel::new(
        vec![
            SummaryStep::Fail(network_error()),
            SummaryStep::Answer(summary_answer()),
        ],
        started_unsafe_advice(),
    );
    let policy = ApprovingPolicy::new(true);
    assert!(!compact_with(model.clone(), policy.clone()).await);
    assert_eq!(model.streams.load(Ordering::SeqCst), 1);
    assert!(policy.seen.lock().unwrap().is_empty());
}

/// As the SDK's `test_get_response_with_retry_allows_custom_policy_to_override_provider_veto`: a
/// policy may retry a failure the provider advised against, here with the delay the provider
/// named. What it overrides is the provider's `suggested=false`, not replay protection: the same
/// policy cannot replay a failure the adapter marked unsafe.
#[tokio::test]
async fn a_policy_may_override_a_provider_veto_but_not_replay_protection() {
    let request = bare_request(ra_core::model::ConversationContinuation::None);
    let veto = RetryAdvice::new()
        .with_suggested(false)
        .with_retry_after(std::time::Duration::from_millis(1_750))
        .with_reason("server veto");
    let settings = ModelRetrySettings::new()
        .with_max_retries(1)
        .with_policy(Arc::new(ra_core::model::RetryAfterPolicy));
    let decide = |advice: RetryAdvice| {
        let model: Arc<dyn Model> = RetryingModel::new(Vec::new(), Some(advice));
        let settings = settings.clone();
        let request = request.clone();
        async move {
            ra_runtime::runner::retry_test_api::decide_first_failure(
                &model,
                &request,
                &network_error(),
                &settings,
                false,
                &CancelScope::root(),
            )
            .await
            .unwrap()
        }
    };

    assert_eq!(
        decide(veto.clone()).await,
        Some(std::time::Duration::from_millis(1_750))
    );
    assert_eq!(
        decide(veto.with_replay_safety(ReplaySafety::Unsafe)).await,
        None
    );
}

/// Whether the runtime would retry a non-streaming first failure under `policy`, with the adapter
/// answering `advice`.
async fn retries_with_policy(
    advice: RetryAdvice,
    policy: Arc<dyn ra_core::model::ModelRetryPolicy>,
    continuation: ra_core::model::ConversationContinuation,
) -> bool {
    let model: Arc<dyn Model> = RetryingModel::new(Vec::new(), Some(advice));
    let settings = ModelRetrySettings::new()
        .with_max_retries(1)
        .with_policy(policy);
    ra_runtime::runner::retry_test_api::decide_first_failure(
        &model,
        &bare_request(continuation),
        &network_error(),
        &settings,
        false,
        &CancelScope::root(),
    )
    .await
    .unwrap()
    .is_some()
}

/// The SDK's combinator cases through the runtime's replay vetoes:
/// - an unsafe-replay approval resolves a provider veto over an unsafe replay under `all` and
///   `any`, in either order (`test_unsafe_replay_approval_resolves_provider_veto_in_policy_combinators`);
/// - a provider hard veto stops `any(provider_suggested, network_error)`
///   (`test_get_response_with_retry_honors_provider_hard_veto`);
/// - a combined ordinary retry replays neither a stateful request the provider vetoed as unsafe
///   nor one whose safety it left unknown (`test_composed_policy_cannot_mutate_provider_authority`,
///   whose mutation half the borrowed context rules out in Rust).
#[tokio::test]
async fn combined_policies_follow_the_sdk_through_the_replay_vetoes() {
    use ra_core::model::{
        AllRetryPolicy, AnyRetryPolicy, ConversationContinuation, ModelRetryPolicy,
        NetworkErrorRetryPolicy, ProviderSuggestedRetryPolicy,
    };

    let combine = |any: bool, list: Vec<Arc<dyn ModelRetryPolicy>>| -> Arc<dyn ModelRetryPolicy> {
        if any {
            Arc::new(AnyRetryPolicy::new(list))
        } else {
            Arc::new(AllRetryPolicy::new(list))
        }
    };
    for any in [false, true] {
        for provider_first in [false, true] {
            let approving: Arc<dyn ModelRetryPolicy> = ApprovingPolicy::new(true);
            let provider: Arc<dyn ModelRetryPolicy> = Arc::new(ProviderSuggestedRetryPolicy);
            let list = if provider_first {
                vec![provider, approving]
            } else {
                vec![approving, provider]
            };
            assert!(
                retries_with_policy(
                    started_unsafe_advice(),
                    combine(any, list),
                    ConversationContinuation::None
                )
                .await,
                "any {any}, provider first {provider_first}"
            );
        }

        let vetoed = RetryAdvice::new()
            .with_suggested(false)
            .with_reason("server veto");
        assert!(
            !retries_with_policy(
                vetoed,
                combine(
                    any,
                    vec![
                        Arc::new(ProviderSuggestedRetryPolicy),
                        Arc::new(NetworkErrorRetryPolicy)
                    ]
                ),
                ConversationContinuation::None
            )
            .await,
            "any {any}"
        );

        for advice in [
            started_unsafe_advice(),
            RetryAdvice::new().with_replay_safety(ReplaySafety::Unknown),
        ] {
            assert!(
                !retries_with_policy(
                    advice,
                    combine(
                        any,
                        vec![
                            ApprovingPolicy::new(false),
                            Arc::new(ProviderSuggestedRetryPolicy)
                        ]
                    ),
                    ConversationContinuation::PreviousResponseId("resp_1".to_owned())
                )
                .await,
                "any {any}"
            );
        }
    }
}

/// Records, for every attempt, which of its own retries the runtime asked the adapter to skip:
/// `(provider-managed, WebSocket pre-event)`.
struct RetryFlagModel {
    responses: Mutex<Vec<Result<ModelResponse>>>,
    flags: Mutex<Vec<(bool, bool)>>,
}

impl RetryFlagModel {
    fn new(responses: Vec<Result<ModelResponse>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            flags: Mutex::default(),
        })
    }
}

#[async_trait]
impl Model for RetryFlagModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("this fixture only supports streaming"))
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        self.flags.lock().unwrap().push((
            request.should_disable_provider_managed_retries(),
            request.should_disable_websocket_pre_event_retries(),
        ));
        let event = self
            .responses
            .lock()
            .unwrap()
            .remove(0)
            .map(|response| ModelStreamEvent::Completed(Box::new(response)));
        stream::iter(vec![event]).boxed()
    }
}

/// Runs one turn against `model` with `retry` as the run's retry settings.
async fn run_with_retry(model: Arc<RetryFlagModel>, retry: ModelRetrySettings) -> Result<()> {
    let cancel = CancelScope::root();
    let mut request = request(model as Arc<dyn Model>, &cancel, false, 0);
    request = request
        .with_config(RunConfig::new().with_model_settings(ModelSettings::new().with_retry(retry)));
    Runner::run(request).await.map(|_| ())
}

/// As the SDK's `test_get_response_with_retry_keeps_provider_retries_on_first_attempt`,
/// `..._disables_websocket_pre_event_retries_when_runner_managed`,
/// `..._disables_provider_retries_when_explicitly_disabled`,
/// `..._disables_websocket_pre_event_retries_when_disabled` and
/// `..._keeps_provider_retries_without_runner_policy`: a stateless call keeps the adapter's own
/// retries on its first attempt and loses them on the runtime's retry; a policy that retries safe
/// transport failures takes over the WebSocket reconnects; an explicit zero budget turns both off;
/// a budget without a policy leaves both on.
#[tokio::test]
async fn the_runtime_tells_the_adapter_which_of_its_own_retries_to_skip() {
    let backoff = RetryBackoffSettings::new()
        .with_initial_delay(std::time::Duration::ZERO)
        .with_jitter(false);

    let model = RetryFlagModel::new(vec![Err(network_error()), Ok(answer())]);
    run_with_retry(
        Arc::clone(&model),
        ModelRetrySettings::new()
            .with_max_retries(1)
            .with_backoff(backoff.clone())
            .with_policy(Arc::new(NetworkErrorRetryPolicy)),
    )
    .await
    .unwrap();
    assert_eq!(*model.flags.lock().unwrap(), [(false, true), (true, true)]);

    let model = RetryFlagModel::new(vec![Ok(answer())]);
    run_with_retry(
        Arc::clone(&model),
        ModelRetrySettings::new()
            .with_max_retries(0)
            .with_policy(Arc::new(ra_core::model::NeverRetryPolicy)),
    )
    .await
    .unwrap();
    assert_eq!(*model.flags.lock().unwrap(), [(true, true)]);

    let model = RetryFlagModel::new(vec![Ok(answer())]);
    run_with_retry(
        Arc::clone(&model),
        ModelRetrySettings::new().with_max_retries(2),
    )
    .await
    .unwrap();
    assert_eq!(*model.flags.lock().unwrap(), [(false, false)]);

    // A policy that does not retry transport failures leaves the WebSocket reconnects alone.
    let model = RetryFlagModel::new(vec![Err(network_error())]);
    run_with_retry(
        Arc::clone(&model),
        ModelRetrySettings::new()
            .with_max_retries(1)
            .with_backoff(backoff)
            .with_policy(Arc::new(ra_core::model::HttpStatusRetryPolicy::new([429]))),
    )
    .await
    .unwrap_err();
    assert_eq!(*model.flags.lock().unwrap(), [(false, false)]);
}

/// The SDK's stateful cases, through the seam because the runtime does not yet produce a request
/// continuing provider-held state: `..._disables_provider_retries_on_first_stateful_provider_hint`,
/// `..._disables_stateful_provider_retries_with_narrow_policy`,
/// `..._keeps_stateful_provider_retries_when_budget_omitted`,
/// `..._disables_stateful_provider_retries_for_network_only_policy`,
/// `..._disables_stateful_provider_retries_for_partial_policy`,
/// `..._keeps_websocket_pre_event_retries_with_unrelated_policy`,
/// `..._keeps_websocket_pre_event_retries_for_partial_all_policy` and
/// `..._disables_provider_retries_on_stateful_compat_replay`.
#[test]
fn a_stateful_request_routes_its_replays_through_the_runtime() {
    use ra_core::model::{
        AllRetryPolicy, AnyRetryPolicy, ConversationContinuation, HttpStatusRetryPolicy,
        ModelRetryPolicy, ProviderSuggestedRetryPolicy,
    };
    use ra_runtime::runner::retry_test_api::coordinate_provider_retries;

    let flags = |settings: Option<&ModelRetrySettings>, replay: bool| {
        let request = coordinate_provider_retries(
            bare_request(ConversationContinuation::PreviousResponseId(
                "resp_prev".to_owned(),
            )),
            settings,
            replay,
        );
        (
            request.should_disable_provider_managed_retries(),
            request.should_disable_websocket_pre_event_retries(),
        )
    };
    let budgeted = |policy: Arc<dyn ModelRetryPolicy>| {
        ModelRetrySettings::new()
            .with_max_retries(1)
            .with_policy(policy)
    };
    let network = || Arc::new(NetworkErrorRetryPolicy) as Arc<dyn ModelRetryPolicy>;
    let status = |code| Arc::new(HttpStatusRetryPolicy::new([code])) as Arc<dyn ModelRetryPolicy>;

    let suggested = budgeted(Arc::new(ProviderSuggestedRetryPolicy));
    assert_eq!(flags(Some(&suggested), false), (true, true));
    assert_eq!(flags(Some(&suggested), true), (true, true));

    assert_eq!(flags(Some(&budgeted(status(429))), false), (true, false));
    assert_eq!(
        flags(
            Some(&ModelRetrySettings::new().with_policy(network())),
            false
        ),
        (false, false)
    );
    assert_eq!(flags(Some(&budgeted(network())), false), (true, true));
    assert_eq!(
        flags(
            Some(&budgeted(Arc::new(AnyRetryPolicy::new([
                network(),
                status(500)
            ])))),
            false
        ),
        (true, true)
    );
    assert_eq!(
        flags(
            Some(&budgeted(Arc::new(AllRetryPolicy::new([
                network(),
                status(500)
            ])))),
            false
        ),
        (true, false)
    );

    // Without retry settings the first attempt keeps the adapter's retries; any replay the runtime
    // schedules for a stateful request has passed its replay checks and turns them off.
    assert_eq!(flags(None, false), (false, false));
    assert_eq!(flags(None, true), (true, false));
}

/// A context processor that asks for one summary through the runtime and leaves the input as is.
struct SummarizingProcessor;

#[async_trait]
impl ra_core::capability::ContextProcessor for SummarizingProcessor {
    async fn process_context(
        &self,
        request: ra_core::capability::ContextProcessorRequest,
        summarizer: &dyn ra_core::capability::ContextSummarizer,
    ) -> Result<ra_core::capability::ContextProcessorResult> {
        summarizer
            .summarize(ra_core::capability::ContextSummaryRequest::new(
                request.input().to_vec(),
                "summarize",
            ))
            .await?;
        Ok(ra_core::capability::ContextProcessorResult::new(
            request.input().to_vec(),
        ))
    }
}

/// A run-scoped context processor sends its summary once and declines on failure, so the runtime
/// owns none of that request's retries and leaves the adapter's own in place, even under a policy
/// that would take the WebSocket reconnects over for an ordinary call. An explicit zero budget
/// still opts out of every retry.
#[tokio::test]
async fn a_summary_sent_once_keeps_the_adapter_retries_the_runtime_does_not_replace() {
    for (max_retries, summary, call) in [
        (1, (false, false), (false, true)),
        (0, (true, true), (true, true)),
    ] {
        let model = RetryFlagModel::new(vec![Ok(answer()), Ok(answer())]);
        let cancel = CancelScope::root();
        let request = request(Arc::clone(&model) as Arc<dyn Model>, &cancel, false, 0).with_config(
            RunConfig::new()
                .with_context_processor(Arc::new(SummarizingProcessor))
                .with_model_settings(
                    ModelSettings::new().with_retry(
                        ModelRetrySettings::new()
                            .with_max_retries(max_retries)
                            .with_policy(Arc::new(NetworkErrorRetryPolicy)),
                    ),
                ),
        );
        Runner::run(request).await.unwrap();
        assert_eq!(
            *model.flags.lock().unwrap(),
            [summary, call],
            "max_retries={max_retries}"
        );
    }
}

#[derive(Debug, Default)]
struct RecordingNetworkRetryPolicy(Mutex<Vec<(u32, u32)>>);

#[async_trait]
impl ra_core::model::ModelRetryPolicy for RecordingNetworkRetryPolicy {
    async fn evaluate(
        &self,
        context: &ra_core::model::RetryPolicyContext<'_>,
    ) -> ra_core::model::RetryDecision {
        self.0
            .lock()
            .unwrap()
            .push((context.attempt(), context.max_retries()));
        NetworkErrorRetryPolicy.evaluate(context).await
    }
}

#[derive(Default)]
struct FallbackRecovery {
    switches: AtomicUsize,
    retry_at: Option<tokio::time::Instant>,
    switch_delay: std::time::Duration,
}

#[async_trait]
impl ra_runtime::runner::ModelRetryRecovery for FallbackRecovery {
    async fn switch_to_fallback_transport(
        &self,
        _request: &ModelRequest,
        error: &Error,
        _advice: Option<&RetryAdvice>,
    ) -> Option<TransportFallback> {
        if error.is_retryable() && self.switches.fetch_add(1, Ordering::SeqCst) == 0 {
            if !self.switch_delay.is_zero() {
                tokio::time::sleep(self.switch_delay).await;
            }
            Some(TransportFallback::new("Falling back.", self.retry_at))
        } else {
            None
        }
    }
}

async fn run_with_fallback(
    model: Arc<RetryingModel>,
    fallback: Option<Arc<FallbackRecovery>>,
    expected_policy_calls: usize,
) -> Result<()> {
    let cancel = CancelScope::root();
    let policy = Arc::new(RecordingNetworkRetryPolicy::default());
    let mut config = RunConfig::new().with_model_settings(
        ModelSettings::new().with_retry(
            ModelRetrySettings::new()
                .with_max_retries(1)
                .with_backoff(
                    RetryBackoffSettings::new()
                        .with_initial_delay(std::time::Duration::ZERO)
                        .with_jitter(false),
                )
                .with_policy(policy.clone()),
        ),
    );
    if let Some(fallback) = fallback {
        config = config.with_model_retry_recovery(fallback);
    }
    let request = request(model as Arc<dyn Model>, &cancel, false, 0).with_config(config);
    let result = Runner::run(request).await.map(|_| ());
    assert_eq!(
        *policy.0.lock().unwrap(),
        vec![(1, 1); expected_policy_calls],
        "the framework policy only sees attempts inside the configured budget"
    );
    result
}

/// The recovery owns the switch, while the runtime owns the real retry budgets and replay safety.
#[tokio::test(start_paused = true)]
async fn a_spent_budget_moves_to_the_fallback_transport_only_when_the_recovery_opts_in() {
    use std::time::Duration;
    use tokio::time::Instant;

    let model = RetryingModel::new(vec![Err(network_error()), Err(network_error())], None);
    assert!(
        run_with_fallback(Arc::clone(&model), None, 1)
            .await
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);

    // Time spent switching transports counts toward the original server deadline.
    let start = Instant::now();
    let retry_at = start + Duration::from_secs(10);
    let recovery = Arc::new(FallbackRecovery {
        retry_at: Some(retry_at),
        switch_delay: Duration::from_secs(4),
        ..FallbackRecovery::default()
    });
    let model = RetryingModel::new(
        vec![
            Err(network_error()),
            Err(network_error()),
            Err(network_error()),
            Ok(answer()),
        ],
        None,
    );
    run_with_fallback(Arc::clone(&model), Some(Arc::clone(&recovery)), 2)
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 4);
    assert_eq!(recovery.switches.load(Ordering::SeqCst), 1);
    assert!((retry_at..retry_at + Duration::from_millis(10)).contains(&Instant::now()));

    // The session already uses its fallback transport, so the second spent budget is final.
    let recovery = Arc::new(FallbackRecovery::default());
    let model = RetryingModel::new(
        vec![
            Err(network_error()),
            Err(network_error()),
            Err(network_error()),
            Err(network_error()),
        ],
        None,
    );
    assert!(
        run_with_fallback(Arc::clone(&model), Some(Arc::clone(&recovery)), 2)
            .await
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 4);
    assert_eq!(recovery.switches.load(Ordering::SeqCst), 2);

    // The runtime refuses an unsafe replay before asking the recovery to switch transports.
    let recovery = Arc::new(FallbackRecovery::default());
    let unsafe_error = Err(ra_core::model::stamp_replay_safety(
        network_error(),
        ReplaySafety::Unsafe,
    ));
    let model = RetryingModel::new(vec![Err(network_error()), unsafe_error], None);
    assert!(
        run_with_fallback(Arc::clone(&model), Some(Arc::clone(&recovery)), 1)
            .await
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(recovery.switches.load(Ordering::SeqCst), 0);
}
