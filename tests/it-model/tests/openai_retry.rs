//! What a failed `OpenAI` call tells a retry policy.
//!
//! Every case here is written against the error and the advice a caller actually receives, because
//! the point of normalization is that the facts survive the trip out of the adapter. Asserting on
//! the message text would pass just as well if the fields were never populated at all — which is
//! the failure this milestone exists to prevent.

use futures::StreamExt;
use ra_core::{
    error::{Error, ProviderErrorKind, Recoverability},
    item::{Message, ModelInputItem},
    model::{
        ConversationContinuation, JitterSample, Model, ModelRequest, ModelRetryAdviceRequest,
        ModelSettings, ModelStream, NormalizedProviderError, ProviderKey, ReplaySafety,
        ResolvedModelSettings, RetryAdvice, RetryBackoff,
    },
};
use ra_model::openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel};
use ra_model::retry::{
    REASON_ENDPOINT_VERDICT, REASON_ERROR_CLASS, RETRY_AFTER_HEADER, RetryHints,
};
use rstest::rstest;
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "retry-test";

fn resolved() -> ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &ModelSettings::new(),
    )
}

fn request() -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        resolved(),
    )
}

/// The body every `OpenAI`-shaped endpoint sends with a refusal.
fn error_body(code: &str, message: &str) -> Value {
    json!({"error": {"message": message, "type": "invalid_request_error", "code": code}})
}

async fn chat_model(server: &MockServer, template: ResponseTemplate) -> OpenAiChatModel {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template)
        .mount(server)
        .await;
    OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("mock model should build")
}

async fn responses_model(server: &MockServer, template: ResponseTemplate) -> OpenAiResponsesModel {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(template)
        .mount(server)
        .await;
    OpenAiResponsesModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("mock model should build")
}

/// Runs one non-streaming call that is expected to fail.
async fn failed_call(model: &impl Model) -> Error {
    model
        .get_response(request())
        .await
        .expect_err("the endpoint refused this call")
}

/// Drains a streaming call and returns the failure it ends in.
async fn failed_stream(model: &impl Model) -> Error {
    drain_stream(model.stream_response(request())).await
}

async fn drain_stream(mut stream: ModelStream<'_>) -> Error {
    let mut last = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => last = Some(event),
            Err(error) => return error,
        }
    }
    panic!("the stream was expected to fail, last event: {last:?}");
}

fn advice(model: &impl Model, error: &Error, streaming: bool) -> Option<RetryAdvice> {
    model.get_retry_advice(&ModelRetryAdviceRequest::new(
        error,
        0,
        streaming,
        &ConversationContinuation::None,
    ))
}

fn facts(error: &Error) -> &NormalizedProviderError {
    NormalizedProviderError::from_error(error).expect("a provider failure must carry its facts")
}

/// A rate limit is the one failure where the endpoint knows how long to wait, and that number has
/// to arrive as a field: a policy cannot read it out of the message without parsing prose.
#[tokio::test]
async fn a_rate_limited_call_reports_the_status_code_and_the_delay_it_asked_for() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "2")
            .insert_header("x-request-id", "req_rate_limited")
            .set_body_json(error_body("rate_limit_exceeded", "slow down")),
    )
    .await;

    let error = failed_call(&model).await;
    assert_eq!(error.recoverability(), Recoverability::Retryable);

    let facts = facts(&error);
    assert_eq!(facts.kind(), ProviderErrorKind::RateLimit);
    assert_eq!(facts.status_code(), Some(429));
    assert_eq!(facts.error_code(), Some("rate_limit_exceeded"));
    assert_eq!(facts.request_id(), Some("req_rate_limited"));
    assert_eq!(facts.retry_after(), Some(Duration::from_secs(2)));

    let advice = advice(&model, &error, false).expect("a provider failure yields advice");
    assert_eq!(advice.suggested(), Some(true));
    assert_eq!(advice.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(advice.reason(), Some(REASON_ERROR_CLASS));
    assert_eq!(
        advice.replay_safety(),
        ReplaySafety::Safe,
        "a non-streaming call handed the caller nothing before it failed"
    );
}

/// An endpoint sending both spellings means the coarse one for clients that do not know the
/// precise one; taking the coarse one anyway throws away the precision it offered.
#[tokio::test]
async fn a_millisecond_delay_is_preferred_over_the_whole_second_one() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "1")
            .insert_header("retry-after-ms", "1500")
            .set_body_json(error_body("rate_limit_exceeded", "slow down")),
    )
    .await;

    let error = failed_call(&model).await;
    assert_eq!(
        facts(&error).retry_after(),
        Some(Duration::from_millis(1500))
    );
}

/// The date form of the header is the one an endpoint behind a CDN tends to send, so dropping it
/// would quietly turn "the endpoint asked for a wait" into "the endpoint asked for nothing".
#[tokio::test]
async fn a_delay_expressed_as_a_date_is_honored() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(503)
            .insert_header("retry-after", "Wed, 21 Oct 2099 07:28:00 GMT")
            .set_body_json(error_body("server_error", "try later")),
    )
    .await;

    let error = failed_call(&model).await;
    let facts = facts(&error);
    assert_eq!(facts.kind(), ProviderErrorKind::ServerError);
    let requested = facts.retry_after().expect("the date names a wait");
    assert!(
        requested > Duration::from_secs(60),
        "a date decades out is a long wait, not a missing one: {requested:?}"
    );
    assert_eq!(
        RetryBackoff::new().delay(0, Some(requested), JitterSample::ZERO),
        RetryBackoff::new().base_delay(0),
        "the delay is recorded as the endpoint stated it, and the honored window is what keeps an \
         absurd one from parking the run"
    );
}

/// HTTP-date has three accepted wire spellings. The obsolete RFC 850 date remains useful as a
/// regression case because it exercises the century rule that a hand-written IMF-only parser misses.
#[test]
fn retry_hints_accept_all_standard_http_date_forms() {
    for value in [
        "Sat, 06 Nov 2094 08:49:37 GMT",
        "Tuesday, 01-Jan-69 00:00:00 GMT",
        "Sat Nov  6 08:49:37 2094",
    ] {
        let hints =
            RetryHints::from_headers(|header| (header == RETRY_AFTER_HEADER).then_some(value));
        assert!(
            hints.retry_after().is_some(),
            "header should parse: {value}"
        );
    }
}

/// A date that has already passed names no wait, so it reads as absent rather than as zero: a
/// present delay is a delay worth waiting.
#[tokio::test]
async fn a_delay_that_has_already_elapsed_reads_as_no_delay() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(503)
            .insert_header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")
            .set_body_json(error_body("server_error", "try later")),
    )
    .await;

    let error = failed_call(&model).await;
    assert_eq!(facts(&error).retry_after(), None);
    assert_eq!(
        advice(&model, &error, false)
            .expect("a provider failure yields advice")
            .retry_after(),
        None
    );
}

/// A conflict is a well-formed request that lost a race, which the reference client retries. The
/// two protocols must classify it the same way — a status class is not a protocol detail.
#[rstest]
#[case::chat(false)]
#[case::responses(true)]
#[tokio::test]
async fn a_conflict_stays_retryable_on_both_protocols(#[case] responses_protocol: bool) {
    let server = MockServer::start().await;
    let template = ResponseTemplate::new(409)
        .insert_header("x-request-id", "req_conflict")
        .set_body_json(error_body("conflict", "the resource changed underneath"));

    let error = if responses_protocol {
        failed_call(&responses_model(&server, template).await).await
    } else {
        failed_call(&chat_model(&server, template).await).await
    };

    assert_eq!(error.code(), "provider.conflict");
    assert_eq!(error.recoverability(), Recoverability::Retryable);
    let facts = facts(&error);
    assert_eq!(facts.kind(), ProviderErrorKind::Conflict);
    assert_eq!(facts.status_code(), Some(409));
    assert_eq!(facts.request_id(), Some("req_conflict"));

    let model = OpenAiChatModel::new(MODEL, OpenAiAuth::new("test-secret")).expect("model builds");
    assert_eq!(
        advice(&model, &error, false)
            .expect("a provider failure yields advice")
            .suggested(),
        Some(true)
    );
}

/// An endpoint that states the verdict knows things about its own state that a status class cannot
/// express — including that a 500 it is about to repeat is not worth another attempt.
#[tokio::test]
async fn the_endpoint_verdict_outranks_what_the_status_class_would_suggest() {
    let server = MockServer::start().await;
    let model = responses_model(
        &server,
        ResponseTemplate::new(500)
            .insert_header("x-should-retry", "false")
            .set_body_json(error_body("server_error", "this one will not get better")),
    )
    .await;

    let error = failed_call(&model).await;
    assert_eq!(
        error.recoverability(),
        Recoverability::Retryable,
        "the taxonomy still classifies a 5xx by its class"
    );
    assert_eq!(facts(&error).should_retry(), Some(false));

    let advice = advice(&model, &error, false).expect("a provider failure yields advice");
    assert_eq!(advice.suggested(), Some(false));
    assert_eq!(advice.reason(), Some(REASON_ENDPOINT_VERDICT));
}

/// The one case the whole replay dimension exists for: a stream that already delivered events
/// cannot be repeated transparently, however retryable the failure that ended it looks.
#[tokio::test]
async fn a_stream_that_already_emitted_events_is_not_replay_safe() {
    let server = MockServer::start().await;
    // One text delta, then the connection simply ends: no `[DONE]`, no finish reason.
    let body = format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl_stream",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": "half an ans"}, "finish_reason": null}]
        })
    );
    let model = chat_model(
        &server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;

    let error = failed_stream(&model).await;
    assert_eq!(error.recoverability(), Recoverability::Retryable);
    assert_eq!(facts(&error).replay_safety(), ReplaySafety::Unsafe);
    assert_eq!(
        advice(&model, &error, true)
            .expect("a provider failure yields advice")
            .replay_safety(),
        ReplaySafety::Unsafe,
        "advice must not soften a verdict the decoder established"
    );
}

/// The mirror image: refused before a single frame existed, so nobody has seen part of this turn
/// and replaying it duplicates nothing.
#[tokio::test]
async fn a_stream_refused_before_it_started_is_replay_safe() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(429)
            .insert_header("retry-after-ms", "250")
            .set_body_json(error_body("rate_limit_exceeded", "slow down")),
    )
    .await;

    let error = failed_stream(&model).await;
    let facts = facts(&error);
    assert_eq!(facts.replay_safety(), ReplaySafety::Safe);
    assert_eq!(facts.status_code(), Some(429));
    assert_eq!(facts.retry_after(), Some(Duration::from_millis(250)));
}

/// A request continuing a server-managed conversation may have been accepted and appended before
/// it failed — which a client that never saw a response cannot tell. Calling that safe would let a
/// replay write the same turn twice into state the client cannot inspect.
#[rstest]
#[case::non_streaming(false)]
#[case::streaming(true)]
#[tokio::test]
async fn a_stateful_request_is_never_declared_replay_safe(#[case] streaming: bool) {
    let server = MockServer::start().await;
    let model = responses_model(
        &server,
        ResponseTemplate::new(500).set_body_json(error_body("server_error", "gateway fell over")),
    )
    .await;
    let stateful = request().with_previous_response_id("resp_previous");

    let error = if streaming {
        drain_stream(model.stream_response(stateful)).await
    } else {
        model
            .get_response(stateful)
            .await
            .expect_err("the endpoint refused this call")
    };

    assert_eq!(
        facts(&error).replay_safety(),
        ReplaySafety::Unknown,
        "the verdict is withheld, not granted"
    );
    let advice = model
        .get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            0,
            streaming,
            &ConversationContinuation::PreviousResponseId("resp_previous".to_owned()),
        ))
        .expect("a provider failure yields advice");
    assert_eq!(advice.suggested(), Some(true));
    assert_eq!(
        advice.replay_safety(),
        ReplaySafety::Unknown,
        "advice must not upgrade a verdict the adapter withheld"
    );
}

/// A request rejected while it was being built failed inside this process. Answering it with
/// provider advice would dress a code defect up as a transient condition.
#[tokio::test]
async fn a_failure_that_never_reached_the_endpoint_gets_no_provider_advice() {
    let model = OpenAiChatModel::new(MODEL, OpenAiAuth::new("test-secret"))
        .expect("model should build without a server");

    let caller = Error::caller("this tool cannot be expressed on Chat Completions");
    assert!(advice(&model, &caller, false).is_none());
    assert!(advice(&model, &Error::cancelled("用户中断"), true).is_none());
}

/// A connection that carried no event at all is a dropped connection, not an empty answer — and
/// since nothing reached the consumer, it is the one streaming failure that may be replayed.
#[tokio::test]
async fn a_stream_that_delivered_nothing_stays_replayable() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(200).set_body_raw(String::new(), "text/event-stream"),
    )
    .await;

    let error = failed_stream(&model).await;
    let facts = facts(&error);
    assert_eq!(facts.kind(), ProviderErrorKind::Network);
    assert_eq!(facts.replay_safety(), ReplaySafety::Safe);
}

/// Nothing above the adapter should have to know an event stream is involved to read the facts.
#[tokio::test]
async fn streamed_and_non_streamed_failures_normalize_the_same_way() {
    let server = MockServer::start().await;
    let template = || {
        ResponseTemplate::new(401)
            .insert_header("x-request-id", "req_denied")
            .set_body_json(error_body("invalid_api_key", "bad key"))
    };
    let model = chat_model(&server, template()).await;

    let direct = failed_call(&model).await;
    let streamed = failed_stream(&model).await;

    for error in [&direct, &streamed] {
        let facts = facts(error);
        assert_eq!(facts.kind(), ProviderErrorKind::Auth);
        assert_eq!(facts.status_code(), Some(401));
        assert_eq!(facts.error_code(), Some("invalid_api_key"));
        assert_eq!(facts.request_id(), Some("req_denied"));
        assert_eq!(error.recoverability(), Recoverability::NeedsIntervention);
    }
}
