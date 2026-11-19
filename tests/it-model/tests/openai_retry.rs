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
        ModelStream, NormalizedProviderError, ProviderKey, ReplaySafety, ResolvedModelSettings,
        RetryAdvice, RetryBackoff, SettingsLayers,
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
    SettingsLayers::new().resolve(&ProviderKey::new("openai"))
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

#[rstest]
#[case("insufficient_quota")]
#[case("usage_limit_reached")]
#[case("usage_not_included")]
#[case("rate_limit_error")]
#[tokio::test]
async fn error_type_survives_alongside_the_original_code(#[case] error_type: &str) {
    let server = MockServer::start().await;
    let model = responses_model(
        &server,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "0")
            .set_body_json(json!({"error": {
                "message": "request refused",
                "code": "rate_limit_exceeded",
                "type": error_type,
            }})),
    )
    .await;

    for error in [failed_call(&model).await, failed_stream(&model).await] {
        assert_eq!(facts(&error).error_code(), Some("rate_limit_exceeded"));
        assert_eq!(facts(&error).kind(), ProviderErrorKind::RateLimit);
        assert_eq!(facts(&error).retry_after(), Some(Duration::ZERO));
        assert_eq!(ra_model::openai::error_type(&error), Some(error_type));
    }
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
        RetryBackoff::new().delay(1, Some(requested), JitterSample::ZERO),
        requested,
        "the delay is waited out as the endpoint stated it, as in the SDK"
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

/// A date that has already passed asks for no wait, so it reads as zero and the retry goes at once,
/// as the SDK's `parse_retry_after_value` reads it.
#[tokio::test]
async fn a_delay_that_has_already_elapsed_reads_as_zero() {
    let server = MockServer::start().await;
    let model = chat_model(
        &server,
        ResponseTemplate::new(503)
            .insert_header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")
            .set_body_json(error_body("server_error", "try later")),
    )
    .await;

    let error = failed_call(&model).await;
    assert_eq!(facts(&error).retry_after(), Some(Duration::ZERO));
    assert_eq!(
        advice(&model, &error, false)
            .expect("a provider failure yields advice")
            .retry_after(),
        Some(Duration::ZERO)
    );
    assert_eq!(
        RetryBackoff::new().delay(1, Some(Duration::ZERO), JitterSample::MIDPOINT),
        Duration::ZERO
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

/// Adapters cannot know how their events were consumed. The runtime owns the output veto, while
/// provider advice stays unknown unless the endpoint supplied replay evidence of its own.
#[tokio::test]
async fn an_interrupted_stream_leaves_output_consumption_to_the_runtime() {
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
    assert_eq!(facts(&error).replay_safety(), ReplaySafety::Unknown);
    assert_eq!(
        advice(&model, &error, true)
            .expect("a provider failure yields advice")
            .replay_safety(),
        ReplaySafety::Unknown,
        "advice must not invent replay evidence for an interrupted stream"
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

/// A request continuing a server-managed conversation that failed before any response may have
/// been accepted, so the adapter withholds the verdict on the error itself. An endpoint that
/// answered with a retryable status refused the request instead, and the `OpenAI` adapter's advice
/// calls replaying it safe, as the SDK's `get_openai_retry_advice` does
/// (`test_get_response_with_retry_allows_stateful_retry_for_http_failure_advice` checks a 429).
#[rstest]
#[case::rate_limited_non_streaming(429, false)]
#[case::rate_limited_streaming(429, true)]
#[case::server_error_non_streaming(500, false)]
#[case::server_error_streaming(500, true)]
#[tokio::test]
async fn a_stateful_request_refused_with_a_retryable_status_is_safe_to_replay(
    #[case] status: u16,
    #[case] streaming: bool,
) {
    let server = MockServer::start().await;
    let model = responses_model(
        &server,
        ResponseTemplate::new(status).set_body_json(error_body("server_error", "refused")),
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

    assert_eq!(facts(&error).status_code(), Some(status));
    assert_eq!(
        facts(&error).replay_safety(),
        ReplaySafety::Unknown,
        "the stamp on the error stays withheld"
    );
    let advice = model
        .get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            1,
            streaming,
            &ConversationContinuation::PreviousResponseId("resp_previous".to_owned()),
        ))
        .expect("a provider failure yields advice");
    assert_eq!(advice.suggested(), Some(true));
    assert_eq!(advice.replay_safety(), ReplaySafety::Safe);
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

#[tokio::test]
async fn response_stream_metadata_keeps_status_scopes_and_server_delay() {
    for (headers, message, expected) in [
        (json!({"Retry-After": "0"}), "retry later", Duration::ZERO),
        (
            json!({"retry-after-ms": "125"}),
            "retry later",
            Duration::from_millis(125),
        ),
        (json!({}), "Try again in 250ms", Duration::from_millis(250)),
        (
            json!({}),
            "Try again in 0.5 seconds",
            Duration::from_millis(500),
        ),
    ] {
        let server = MockServer::start().await;
        let frame = json!({"type": "response.failed", "response": {"error": {
            "code": "rate_limit_exceeded", "message": message, "headers": headers,
        }}});
        let model = responses_model(
            &server,
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {frame}\n\n"), "text/event-stream"),
        )
        .await;
        let error = failed_stream(&model).await;
        assert_eq!(error.code(), "provider.behavior");
        assert_eq!(facts(&error).status_code(), None);
        assert_eq!(facts(&error).error_code(), Some("rate_limit_exceeded"));
        assert_eq!(facts(&error).retry_after(), Some(expected));
        assert_eq!(
            ra_model::openai::response_failure_event(&error),
            Some("response.failed")
        );
        let wrapped = Error::caller("wrapped").with_source(error);
        assert_eq!(
            ra_model::openai::response_failure_event(&wrapped),
            Some("response.failed")
        );
    }
}

#[tokio::test]
async fn stream_wait_advice_is_read_only_from_a_failed_response_error() {
    let advice = json!({"retry-after": "60"});
    for (frame, expected) in [
        // Codex's `parse_failed_response` reads `response.error.headers` whatever the code.
        (
            json!({"type": "response.failed", "response": {"error": {
                "code": "server_error", "message": "boom", "headers": advice,
            }}}),
            Some(Duration::from_secs(60)),
        ),
        // Nothing outside that object, and no other failure frame, carries advice there.
        (
            json!({"type": "response.failed", "response": {"headers": advice}}),
            None,
        ),
        (
            json!({"type": "response.failed", "response": {
                "code": "rate_limit_exceeded", "message": "Please try again in 60s",
            }}),
            None,
        ),
        (
            json!({"type": "error", "error": {
                "code": "server_error", "message": "boom", "headers": advice,
            }}),
            None,
        ),
        (
            json!({"type": "response.error", "error": {
                "code": "rate_limit_exceeded", "message": "Please try again in 60s",
                "headers": advice,
            }}),
            None,
        ),
        (
            json!({"type": "error", "code": "rate_limit_exceeded",
                "message": "Please try again in 60s"}),
            None,
        ),
    ] {
        let server = MockServer::start().await;
        let model = responses_model(
            &server,
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {frame}\n\n"), "text/event-stream"),
        )
        .await;
        let error = failed_stream(&model).await;
        assert_eq!(facts(&error).retry_after(), expected, "{frame}");
    }
}

fn stateful() -> ConversationContinuation {
    ConversationContinuation::PreviousResponseId("resp_prev".to_owned())
}

fn stateful_advice(model: &impl Model, error: &Error, streaming: bool) -> RetryAdvice {
    model
        .get_retry_advice(&ModelRetryAdviceRequest::new(
            error,
            1,
            streaming,
            &stateful(),
        ))
        .expect("a provider failure gets advice")
}

/// The SDK's order decides the verdict for a stateful request: an explicit verdict stands, then
/// `x-should-retry`, then a missing status, then the status. So the endpoint's `true` cannot lift
/// an explicit `Unsafe`, its `false` keeps a retryable status from making the replay safe, and a
/// failure with no response or a non-retryable status stays unknown.
#[rstest]
#[case::retryable_status(Some(429), None, ReplaySafety::Unknown, ReplaySafety::Safe)]
#[case::server_error(Some(503), None, ReplaySafety::Unknown, ReplaySafety::Safe)]
#[case::request_timeout(Some(408), None, ReplaySafety::Unknown, ReplaySafety::Safe)]
#[case::conflict(Some(409), None, ReplaySafety::Unknown, ReplaySafety::Safe)]
#[case::rejected_request(Some(400), None, ReplaySafety::Unknown, ReplaySafety::Unknown)]
#[case::endpoint_vouches(Some(400), Some(true), ReplaySafety::Unknown, ReplaySafety::Safe)]
#[case::endpoint_declines_a_retryable_status(
    Some(429),
    Some(false),
    ReplaySafety::Unknown,
    ReplaySafety::Unknown
)]
#[case::endpoint_cannot_lift_an_explicit_unsafe(
    Some(503),
    Some(true),
    ReplaySafety::Unsafe,
    ReplaySafety::Unsafe
)]
#[case::explicit_safe_stands(Some(400), Some(false), ReplaySafety::Safe, ReplaySafety::Safe)]
#[case::no_response(None, None, ReplaySafety::Unknown, ReplaySafety::Unknown)]
fn the_openai_replay_verdict_follows_the_sdk_order(
    #[case] status: Option<u16>,
    #[case] should_retry: Option<bool>,
    #[case] stamped: ReplaySafety,
    #[case] expected: ReplaySafety,
) {
    let mut normalized = NormalizedProviderError::new(
        if status.is_some() {
            ProviderErrorKind::RateLimit
        } else {
            ProviderErrorKind::Network
        },
        "refused",
    )
    .with_replay_safety(stamped);
    if let Some(status) = status {
        normalized = normalized.with_status_code(status);
    }
    if let Some(should_retry) = should_retry {
        normalized = normalized.with_should_retry(should_retry);
    }
    let error = normalized.into_error();
    let auth = || OpenAiAuth::new("test-secret").with_base_url("http://127.0.0.1:9/v1/");
    let responses = OpenAiResponsesModel::new(MODEL, auth()).unwrap();
    let chat = OpenAiChatModel::new(MODEL, auth()).unwrap();
    for streaming in [false, true] {
        assert_eq!(
            stateful_advice(&responses, &error, streaming).replay_safety(),
            expected,
            "responses, streaming {streaming}"
        );
        assert_eq!(
            stateful_advice(&chat, &error, streaming).replay_safety(),
            expected,
            "chat, streaming {streaming}"
        );
    }
}

/// The status verdict is for requests continuing provider-held state: a stateless streamed
/// failure keeps the neutral verdict.
#[test]
fn a_retryable_status_makes_only_a_stateful_replay_safe() {
    let error = NormalizedProviderError::new(ProviderErrorKind::RateLimit, "slow down")
        .with_status_code(429)
        .into_error();
    let model = OpenAiResponsesModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url("http://127.0.0.1:9/v1/"),
    )
    .unwrap();
    assert_eq!(
        advice(&model, &error, true).unwrap().replay_safety(),
        ReplaySafety::Unknown
    );
}

/// The verdict is the `OpenAI` endpoints' contract; another provider sharing the neutral advice
/// does not inherit it.
#[test]
fn the_openai_replay_verdict_does_not_reach_other_providers() {
    use ra_model::anthropic::{AnthropicAuth, AnthropicMessagesModel};

    let model = AnthropicMessagesModel::new(
        MODEL,
        AnthropicAuth::new("test-secret").with_base_url("http://127.0.0.1:9/v1/"),
    )
    .unwrap();
    let error = NormalizedProviderError::new(ProviderErrorKind::RateLimit, "slow down")
        .with_status_code(429)
        .with_should_retry(true)
        .into_error();
    assert_eq!(
        stateful_advice(&model, &error, true).replay_safety(),
        ReplaySafety::Unknown
    );
}
