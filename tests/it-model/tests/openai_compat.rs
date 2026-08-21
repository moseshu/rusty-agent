//! Contract tests for the `OpenAI`-compatible endpoint layer.
//!
//! The cases here are about configuration reaching the wire, not about the Chat codec: the codec
//! is covered next door. What is locked below is the posture — an endpoint that declared nothing
//! receives nothing unusual — and the claim that one endpoint is described in one place, so that
//! the switches, the static body fields and the credential all arrive together however the
//! provider was reached.

use std::sync::Arc;

use futures::StreamExt;
use ra_core::{
    item::{Message, ModelInputItem},
    model::{Model, ModelProvider, ModelRequest, ModelSettings, ModelStreamEvent, ProviderKey},
    prompt::{CachePlan, ContentHash},
};
use ra_model::{
    compat::{CompatEndpoint, quirks::DoneMarker},
    provider::{ProviderRegistry, UnknownPrefixPolicy, quirks::ProviderQuirks},
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "local-model";
const PROVIDER: &str = "gateway";

/// An endpoint pointed at the mock server, declaring nothing beyond the bare protocol.
fn endpoint(server: &MockServer) -> CompatEndpoint {
    CompatEndpoint::new(format!("{}/v1", server.uri()), MODEL)
}

fn request(input: Vec<ModelInputItem>) -> ModelRequest {
    let settings = ModelSettings::new().resolve(
        &ProviderKey::new(PROVIDER),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &ModelSettings::new(),
    );
    ModelRequest::new(input, settings)
}

fn user_turn() -> Vec<ModelInputItem> {
    vec![ModelInputItem::Message(Message::user("hello"))]
}

fn completion() -> Value {
    json!({
        "id": "chatcmpl_compat",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "ok"}
        }]
    })
}

/// Answers every chat request with the supplied template.
async fn mount(server: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template)
        .mount(server)
        .await;
}

fn model_of(endpoint: &CompatEndpoint) -> Arc<dyn Model> {
    endpoint
        .build_provider()
        .expect("endpoint should build a provider")
        .get_model(None)
        .expect("default model should resolve")
}

/// Sends one request and returns the JSON body that reached the endpoint.
async fn sent_body(server: &MockServer, model: &Arc<dyn Model>, request: ModelRequest) -> Value {
    model
        .get_response(request)
        .await
        .expect("mock completion should convert");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    serde_json::from_slice(&requests[0].body).expect("request should contain JSON")
}

/// The `Authorization` header of the first request the endpoint received, if any.
async fn sent_authorization(server: &MockServer) -> Option<String> {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    requests[0]
        .headers
        .get("authorization")
        .map(|value| value.to_str().expect("header should be text").to_owned())
}

// ---------------------------------------------------------------------------------------------
// The conservative posture
// ---------------------------------------------------------------------------------------------

/// An endpoint that declared nothing gets a request containing nothing first-party.
///
/// Each of these fields is rejected outright by some gateway, which costs the whole call rather
/// than the one feature it was reaching for.
#[tokio::test]
async fn an_undeclared_endpoint_receives_nothing_beyond_the_bare_protocol() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;
    let endpoint = endpoint(&server);
    let model = model_of(&endpoint);

    let instructions = "stable instructions. ".repeat(600);
    let request = request(user_turn())
        .with_system_instructions(instructions.clone())
        .with_cache_plan(
            CachePlan::new(ContentHash::compute(&instructions)).with_cache_scope("thread_abc"),
        );
    let body = sent_body(&server, &model, request).await;

    assert_eq!(body["model"], MODEL);
    for field in [
        "store",
        "stream_options",
        "parallel_tool_calls",
        "prompt_cache_key",
    ] {
        assert!(
            body.get(field).is_none(),
            "an undeclared endpoint must not receive `{field}`: {body}"
        );
    }
    // A local model server takes no credential at all, and inventing a placeholder to satisfy
    // validation would put a meaningless secret into configuration and logs.
    assert_eq!(sent_authorization(&server).await, None);
}

/// A declared credential becomes the bearer header; a blank one is a configuration error.
#[tokio::test]
async fn a_declared_credential_authenticates_and_a_blank_one_is_refused() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;
    let endpoint = endpoint(&server).with_api_key("relay-secret");
    let model = model_of(&endpoint);

    sent_body(&server, &model, request(user_turn())).await;
    assert_eq!(
        sent_authorization(&server).await.as_deref(),
        Some("Bearer relay-secret")
    );

    let blank = endpoint.clone().with_api_key("   ").build_provider();
    assert!(
        blank
            .expect_err("a blank credential should be refused")
            .to_string()
            .contains("API key must not be empty")
    );
}

/// A custom terminator must be a real payload rather than the empty `data:` field of an SSE frame.
#[tokio::test]
async fn a_blank_custom_done_marker_is_refused() {
    let server = MockServer::start().await;
    let error = endpoint(&server)
        .with_done_marker(DoneMarker::Literal(" \t ".to_owned()))
        .build_provider()
        .expect_err("an empty marker would treat an empty data frame as the end of the stream");

    assert!(
        error.to_string().contains("done marker must not be empty"),
        "unexpected error: {error}"
    );
}

/// Request transport headers replace endpoint defaults instead of being appended as duplicates.
#[tokio::test]
async fn request_headers_override_endpoint_default_headers() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;
    let endpoint = endpoint(&server).with_default_header("x-router-route", "endpoint");
    let model = model_of(&endpoint);
    let settings = ModelSettings::new()
        .with_extra_header("x-router-route", "request")
        .resolve(
            &ProviderKey::new(PROVIDER),
            &ModelSettings::new(),
            &ModelSettings::new(),
            &ModelSettings::new(),
        );

    model
        .get_response(ModelRequest::new(user_turn(), settings))
        .await
        .expect("mock completion should convert");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    let values = requests[0]
        .headers
        .get_all("x-router-route")
        .iter()
        .map(|value| value.to_str().expect("header should be text"))
        .collect::<Vec<_>>();

    assert_eq!(values, ["request"]);
}

/// Declaring a capability is the only thing that puts its field on the wire.
#[tokio::test]
async fn declared_capabilities_reach_the_wire_through_the_endpoint() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;
    let endpoint = endpoint(&server).with_quirks(
        ProviderQuirks::new()
            .with_store(true)
            .with_prompt_cache_key(true),
    );
    let model = model_of(&endpoint);

    let instructions = "stable instructions. ".repeat(600);
    let request = request(user_turn())
        .with_system_instructions(instructions.clone())
        .with_cache_plan(
            CachePlan::new(ContentHash::compute(&instructions)).with_cache_scope("thread_abc"),
        );
    let body = sent_body(&server, &model, request).await;

    assert_eq!(body["store"], true);
    assert_eq!(body["prompt_cache_key"], "thread_abc");
}

/// Streamed usage is asked for only where the endpoint said it exists.
#[tokio::test]
async fn stream_usage_is_requested_only_from_an_endpoint_that_declared_it() {
    for (quirks, expected) in [
        (ProviderQuirks::new(), None),
        (
            ProviderQuirks::new().with_stream_usage(true),
            Some(json!({"include_usage": true})),
        ),
    ] {
        let server = MockServer::start().await;
        mount(
            &server,
            ResponseTemplate::new(200).set_body_raw("data: [DONE]\n\n", "text/event-stream"),
        )
        .await;
        let endpoint = endpoint(&server).with_quirks(quirks);
        let model = model_of(&endpoint);

        // The stream itself is not the subject here; only the body that opened it is.
        let _events = model
            .stream_response(request(user_turn()))
            .collect::<Vec<_>>()
            .await;
        let requests = server
            .received_requests()
            .await
            .expect("wiremock should retain requests");
        let body: Value =
            serde_json::from_slice(&requests[0].body).expect("request should contain JSON");

        assert_eq!(body["stream"], true);
        assert_eq!(body.get("stream_options"), expected.as_ref());
    }
}

/// The lowering policy travels with the endpoint, so a relay can be told to accept the loss.
#[tokio::test]
async fn the_endpoint_carries_the_lowering_policy_it_declares() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;
    let strict = endpoint(&server);
    let lenient = endpoint(&server).with_lowering_options(
        ra_model::openai::chat::ChatLoweringOptions::new().with_strict_feature_validation(false),
    );

    let continued = || request(user_turn()).with_previous_response_id("resp_previous");
    let refused = model_of(&strict)
        .get_response(continued())
        .await
        .expect_err("the default policy should refuse a feature it cannot express")
        .to_string();
    assert!(
        refused.contains("server-managed conversation state"),
        "unexpected message: {refused}"
    );
    model_of(&lenient)
        .get_response(continued())
        .await
        .expect("a lenient endpoint should degrade instead of failing");
}

// ---------------------------------------------------------------------------------------------
// One endpoint, one place
// ---------------------------------------------------------------------------------------------

/// A registration carries both halves of the endpoint description at once.
///
/// The capability switches say which standard fields this endpoint cannot accept, and the static
/// `extra_body` says which non-standard ones it additionally needs. Onboarding a vendor sets both
/// in one value, and both arrive on a request routed through the registry.
#[tokio::test]
async fn one_registration_carries_the_capabilities_and_the_static_body_fields() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;

    let mut extra_body = ra_core::model::JsonMap::new();
    extra_body.insert("guided_json".to_owned(), json!({"type": "object"}));
    let registration = endpoint(&server)
        .with_quirks(ProviderQuirks::new().with_store(true))
        .with_extra_body(extra_body)
        .into_registration(ProviderKey::new(PROVIDER))
        .expect("a valid endpoint should register");
    let registry = ProviderRegistry::builder(ProviderKey::new(PROVIDER))
        .register(registration)
        .build()
        .expect("registry should build");

    let resolution = registry
        .resolve_model(Some("hosted-model"))
        .expect("model should resolve");
    let settings = resolution.resolve_settings(&ModelSettings::new(), &ModelSettings::new());
    let request = ModelRequest::new(user_turn(), settings);
    let body = sent_body(&server, resolution.model(), request).await;

    assert_eq!(body["model"], "hosted-model");
    assert_eq!(body["store"], true);
    assert_eq!(body["guided_json"], json!({"type": "object"}));
}

/// The registration is the single runtime source of capabilities, not the copy taken from the
/// endpoint: overriding it afterwards has to win, or the registry would describe an endpoint the
/// provider does not agree with.
#[tokio::test]
async fn capabilities_overridden_on_the_registration_beat_the_endpoint_declaration() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;

    let registration = endpoint(&server)
        .with_quirks(ProviderQuirks::new().with_store(true))
        .into_registration(ProviderKey::new(PROVIDER))
        .expect("a valid endpoint should register")
        .with_quirks(ProviderQuirks::new());
    let registry = ProviderRegistry::builder(ProviderKey::new(PROVIDER))
        .register(registration)
        .build()
        .expect("registry should build");

    let resolution = registry
        .resolve_model(None)
        .expect("provider default should resolve");
    let settings = resolution.resolve_settings(&ModelSettings::new(), &ModelSettings::new());
    let body = sent_body(
        &server,
        resolution.model(),
        ModelRequest::new(user_turn(), settings),
    )
    .await;

    assert!(
        body.get("store").is_none(),
        "the registration's declaration must be the one that reaches the wire: {body}"
    );
}

/// An unregistered vendor prefix reaches the compat endpoint as the whole model name.
///
/// The relay is what knows how to route `some-vendor/some-model`; splitting the string locally
/// would send it a model it never published under that name.
#[tokio::test]
async fn an_unregistered_prefix_arrives_as_the_complete_model_name() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;

    let registry = ProviderRegistry::builder(ProviderKey::new(PROVIDER))
        .unknown_prefix_policy(UnknownPrefixPolicy::ForwardTo(ProviderKey::new(PROVIDER)))
        .register(
            endpoint(&server)
                .into_registration(ProviderKey::new(PROVIDER))
                .expect("a valid endpoint should register"),
        )
        .build()
        .expect("registry should build");

    let resolution = registry
        .resolve_model(Some("some-vendor/some-model"))
        .expect("an unknown prefix should be forwarded");
    let settings = resolution.resolve_settings(&ModelSettings::new(), &ModelSettings::new());
    let body = sent_body(
        &server,
        resolution.model(),
        ModelRequest::new(user_turn(), settings),
    )
    .await;

    assert_eq!(body["model"], "some-vendor/some-model");
}

/// Static body fields have nowhere to travel on the entry point that builds no registration, so
/// asking for them there is refused rather than quietly served without them.
#[tokio::test]
async fn static_body_fields_are_refused_by_the_entry_point_that_cannot_carry_them() {
    let mut extra_body = ra_core::model::JsonMap::new();
    extra_body.insert("guided_json".to_owned(), json!({"type": "object"}));

    let refused = CompatEndpoint::new("https://relay.example.com/v1", MODEL)
        .with_extra_body(extra_body)
        .build_provider()
        .expect_err("an endpoint with static body fields should not build a bare provider")
        .to_string();
    assert!(
        refused.contains("into_registration"),
        "unexpected message: {refused}"
    );
}

/// The full endpoint URL pasted out of a vendor's documentation is refused where it is readable.
#[tokio::test]
async fn a_base_url_that_already_names_the_request_path_is_refused() {
    let refused = CompatEndpoint::new("https://relay.example.com/v1/chat/completions", MODEL)
        .build_provider()
        .expect_err("a base URL containing the request path should be refused")
        .to_string();
    assert!(
        refused.contains("already ends in `/chat/completions`"),
        "unexpected message: {refused}"
    );

    CompatEndpoint::new("https://relay.example.com/v1", MODEL)
        .build_provider()
        .expect("the endpoint root should be accepted");
}

// ---------------------------------------------------------------------------------------------
// Stream termination
// ---------------------------------------------------------------------------------------------

/// Collects a stream, returning the raw event types in order and the terminal error if any.
async fn stream_outcome(endpoint: &CompatEndpoint) -> (Vec<String>, Option<ra_core::error::Error>) {
    let events = model_of(endpoint)
        .stream_response(request(user_turn()))
        .collect::<Vec<_>>()
        .await;
    let mut types = Vec::new();
    let mut failure = None;
    for event in events {
        match event {
            Ok(ModelStreamEvent::RawResponse(event)) => types.push(event.event_type().to_owned()),
            Ok(_) => {}
            Err(error) => failure = Some(error),
        }
    }
    (types, failure)
}

fn chunk() -> Value {
    json!({
        "id": "chatcmpl_stream",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]
    })
}

/// Mounts a stream body made of one content chunk plus whatever terminates it.
async fn mount_stream(server: &MockServer, terminator: &str) {
    let body = format!("data: {}\n\n{terminator}", chunk());
    mount(
        server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;
}

/// The default assumes the protocol, so a stream with no terminal evidence is a failure.
#[tokio::test]
async fn an_undeclared_endpoint_refuses_a_stream_that_never_said_it_finished() {
    let server = MockServer::start().await;
    mount_stream(&server, "").await;

    let (_, failure) = stream_outcome(&endpoint(&server)).await;
    let message = failure
        .expect("a stream without terminal evidence should fail")
        .to_string();
    assert!(
        message.contains("without `[DONE]` or a finish reason"),
        "unexpected message: {message}"
    );
}

/// A declared marker terminates the stream, and `[DONE]` keeps working alongside it.
#[tokio::test]
async fn a_declared_marker_terminates_the_stream_without_displacing_done() {
    for terminator in ["data: [END]\n\n", "data: [DONE]\n\n"] {
        let server = MockServer::start().await;
        mount_stream(&server, terminator).await;
        let endpoint = endpoint(&server).with_done_marker(DoneMarker::Literal("[END]".to_owned()));

        let (types, failure) = stream_outcome(&endpoint).await;
        assert!(failure.is_none(), "`{terminator}` should terminate cleanly");
        assert_eq!(types.last().map(String::as_str), Some("response.completed"));
    }
}

/// An endpoint declared to send no terminator settles when the body ends.
#[tokio::test]
async fn an_endpoint_without_a_terminator_settles_at_the_end_of_the_body() {
    let server = MockServer::start().await;
    mount_stream(&server, "").await;
    let endpoint = endpoint(&server).with_done_marker(DoneMarker::Absent);

    let (types, failure) = stream_outcome(&endpoint).await;
    assert!(failure.is_none(), "the declared ending should be accepted");
    assert_eq!(types.last().map(String::as_str), Some("response.completed"));
}

/// That declaration does not extend to a stream that delivered nothing at all.
///
/// It is the one failure worth keeping closed whatever the endpoint says: an empty turn settled as
/// a success reads as a model with nothing to say rather than as a connection that carried none.
#[tokio::test]
async fn an_endpoint_without_a_terminator_still_refuses_a_stream_that_delivered_nothing() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_raw(String::new(), "text/event-stream"),
    )
    .await;
    let endpoint = endpoint(&server).with_done_marker(DoneMarker::Absent);

    let (_, failure) = stream_outcome(&endpoint).await;
    let message = failure
        .expect("an empty stream should fail whatever the endpoint declared")
        .to_string();
    assert!(
        message.contains("without sending an event"),
        "unexpected message: {message}"
    );
}
