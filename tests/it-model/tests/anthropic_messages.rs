//! Contract tests for the Anthropic Messages adapter.

use futures::StreamExt;
use ra_core::{
    item::{CallId, Message, ModelInputItem, RunItemKind, ToolCall, ToolCallOutput},
    model::{
        Effort, Model, ModelOutputSchema, ModelRequest, ModelSettings, ModelStreamEvent,
        ModelToolDefinition, ProviderKey, SettingsLayers, ToolChoice,
    },
};
use ra_model::anthropic::{AnthropicAuth, AnthropicMessagesModel};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "claude-test";

fn resolved(settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    SettingsLayers::new()
        .with_run(&settings)
        .resolve(&ProviderKey::new("anthropic"))
}

fn request() -> ModelRequest {
    request_with(ModelSettings::new().with_max_tokens(1024))
}

fn request_with(settings: ModelSettings) -> ModelRequest {
    request_items(
        vec![ModelInputItem::Message(Message::user("look this up"))],
        settings,
    )
}

fn request_items(input: Vec<ModelInputItem>, settings: ModelSettings) -> ModelRequest {
    ModelRequest::new(input, resolved(settings))
}

fn sse(body: &'static str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(body, "text/event-stream")
}

async fn stream_errors(body: &'static str) -> ra_core::error::Error {
    let server = MockServer::start().await;
    let model = model(&server, sse(body)).await;
    model
        .stream_response(request())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("the stream should fail rather than panic or finish")
}

async fn model(server: &MockServer, response: ResponseTemplate) -> AnthropicMessagesModel {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(response)
        .mount(server)
        .await;
    AnthropicMessagesModel::new(
        MODEL,
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("mock model should build")
}

#[tokio::test]
async fn lowers_a_messages_request_and_lifts_thinking_text_tools_and_usage() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        ResponseTemplate::new(200)
            .insert_header("request-id", "req_anthropic")
            .set_body_json(json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "inspect source", "signature": "sig_1"},
                    {"type": "text", "text": "I found it."},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"id": 7}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 4, "cache_creation_input_tokens": 6, "output_tokens": 8}
            })),
    )
    .await;
    let response = model
        .get_response(
            request()
                .with_prompt_cache_key("openai-routing")
                .with_system_instructions("stable system")
                .with_tools(vec![ModelToolDefinition::new(
                    "lookup",
                    json!({"type": "object"}),
                )]),
        )
        .await
        .expect("response should convert");

    assert_eq!(response.request_id(), Some("req_anthropic"));
    assert_eq!(response.usage().input_tokens(), 20);
    assert_eq!(response.usage().cached_input_tokens(), 4);
    assert_eq!(response.usage().cache_write_tokens(), 6);
    assert!(matches!(
        response.output()[0].kind(),
        RunItemKind::Reasoning(_)
    ));
    assert!(matches!(
        response.output()[1].kind(),
        RunItemKind::Message(_)
    ));
    assert!(matches!(
        response.output()[2].kind(),
        RunItemKind::ToolCall(_)
    ));

    let requests = server.received_requests().await.expect("requests retained");
    let sent: Value = serde_json::from_slice(&requests[0].body).expect("JSON body");
    assert!(sent.get("prompt_cache_key").is_none());
    assert_eq!(
        requests[0]
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("test-secret")
    );
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01")
    );
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["max_tokens"], 1024);
    assert_eq!(
        sent["system"],
        json!([{"type": "text", "text": "stable system"}])
    );
    assert_eq!(
        sent["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "look this up"}]}])
    );
    assert_eq!(
        sent["tools"][0],
        json!({"name": "lookup", "input_schema": {"type": "object"}})
    );
}

/// A redacted block carries no text and no signature, and only replays if it goes back verbatim.
#[tokio::test]
async fn replays_a_redacted_thinking_block_verbatim() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_redacted",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "redacted_thinking", "data": "EncryptedBlob=="},
                {"type": "text", "text": "done"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 4, "output_tokens": 2}
        })),
    )
    .await;

    let first = model
        .get_response(request())
        .await
        .expect("the redacted block should lift");
    let RunItemKind::Reasoning(reasoning) = first.output()[0].kind() else {
        panic!("a redacted thinking block is a reasoning item");
    };

    model
        .get_response(request_items(
            vec![
                ModelInputItem::Message(Message::user("look this up")),
                ModelInputItem::Reasoning(reasoning.clone()),
                ModelInputItem::Message(Message::user("keep going")),
            ],
            ModelSettings::new().with_max_tokens(1024),
        ))
        .await
        .expect("the lifted block should be sendable again");

    let requests = server.received_requests().await.expect("requests retained");
    let replayed: Value = serde_json::from_slice(&requests[1].body).expect("JSON body");
    assert_eq!(replayed["messages"][1]["role"], "assistant");
    assert_eq!(
        replayed["messages"][1]["content"][0],
        json!({"type": "redacted_thinking", "data": "EncryptedBlob=="})
    );
}

/// Effort and the output schema share one wire field, so neither may erase the other.
#[tokio::test]
async fn lowers_effort_and_the_output_schema_into_one_output_config() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_schema",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "{}"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
    )
    .await;

    model
        .get_response(
            request_with(
                ModelSettings::new()
                    .with_max_tokens(1024)
                    .with_effort(Effort::High),
            )
            .with_output_schema(ModelOutputSchema::new("answer", json!({"type": "object"}))),
        )
        .await
        .expect("structured output should lower");

    let requests = server.received_requests().await.expect("requests retained");
    let sent: Value = serde_json::from_slice(&requests[0].body).expect("JSON body");
    assert_eq!(
        sent["output_config"],
        json!({"effort": "high", "format": {"type": "json_schema", "schema": {"type": "object"}}})
    );
    assert!(sent.get("output_format").is_none());
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("structured-outputs-2025-11-13")
    );
}

/// Forbidding tool calls is a tool choice, not a reason to withdraw the cached tool table.
#[tokio::test]
async fn forbids_tool_calls_without_withdrawing_the_tools() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_none",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "no tools"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
    )
    .await;

    model
        .get_response(
            request_with(
                ModelSettings::new()
                    .with_max_tokens(1024)
                    .with_tool_choice(ToolChoice::None),
            )
            .with_tools(vec![ModelToolDefinition::new(
                "lookup",
                json!({"type": "object"}),
            )]),
        )
        .await
        .expect("a no-tools turn should lower");

    let requests = server.received_requests().await.expect("requests retained");
    let sent: Value = serde_json::from_slice(&requests[0].body).expect("JSON body");
    assert_eq!(sent["tool_choice"], json!({"type": "none"}));
    assert_eq!(sent["tools"][0]["name"], "lookup");
}

#[tokio::test]
async fn rejects_a_tool_result_separated_from_its_tool_use() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"unused": true})),
    )
    .await;
    let call_id = CallId::new("call_1");
    let request = request_items(
        vec![
            ModelInputItem::Message(Message::user("look this up")),
            ModelInputItem::ToolCall(ToolCall::new(call_id.clone(), "lookup", json!({"id": 7}))),
            ModelInputItem::Message(Message::user("do something else first")),
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(call_id, json!("found it"))),
        ],
        ModelSettings::new().with_max_tokens(1024),
    );

    let error = model
        .get_response(request)
        .await
        .expect_err("the invalid tool-result ordering should fail locally");
    assert!(error.to_string().contains("must immediately follow"));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests retained")
            .is_empty()
    );
}

#[tokio::test]
async fn accepts_a_deferred_user_tail_after_the_tool_result_it_explains() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_deferred",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
    )
    .await;
    let call_id = CallId::new("call_deferred");
    let request = request_items(
        vec![
            ModelInputItem::Message(Message::user("look this up")),
            ModelInputItem::ToolCall(ToolCall::new(call_id.clone(), "lookup", json!({"id": 7}))),
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(call_id, json!("found it"))),
            ModelInputItem::Message(Message::user(
                "The lookup result is available through the local cache.",
            )),
        ],
        ModelSettings::new().with_max_tokens(1024),
    );

    model
        .get_response(request)
        .await
        .expect("a user tail after a settled tool call should lower");

    let requests = server.received_requests().await.expect("requests retained");
    let sent: Value = serde_json::from_slice(&requests[0].body).expect("JSON body");
    assert_eq!(
        sent["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "look this up"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "call_deferred", "name": "lookup", "input": {"id": 7}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_deferred", "content": "found it"},
                {"type": "text", "text": "The lookup result is available through the local cache."}
            ]}
        ])
    );
}

/// A stream frame whose shape is wrong is a provider failure, never a panic in this process.
#[tokio::test]
async fn rejects_malformed_stream_frames_instead_of_panicking() {
    let error = stream_errors(concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":\"text\"}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    ))
    .await;
    assert_eq!(error.code(), "provider.behavior");
    assert!(error.to_string().contains("content_block"), "{error}");

    let error = stream_errors(concat!(
        "data: {\"type\":\"message_start\",\"message\":\"oops\"}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    ))
    .await;
    assert_eq!(error.code(), "provider.behavior");
    assert!(error.to_string().contains("message"), "{error}");
}

/// A stream that stops early fails once and then ends, rather than repeating its verdict.
///
/// Wrapped in a timeout because the failure this guards against is a stream that never returns
/// `None`: without the bound, a regression would hang the run instead of failing it.
#[tokio::test]
async fn a_truncated_stream_fails_once_and_then_ends() {
    let server = MockServer::start().await;
    let model = model(
        &server,
        sse(concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}\n\n"
        )),
    )
    .await;

    let events = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        model.stream_response(request()).collect::<Vec<_>>(),
    )
    .await
    .expect("a failed stream must end");

    assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
    let error = events
        .last()
        .expect("the failure is the last event")
        .as_ref()
        .expect_err("the last event states the turn never completed");
    assert!(error.to_string().contains("message_stop"), "{error}");
}

/// An overloaded endpoint is worth retrying unchanged, mid-stream as much as on a status line.
#[tokio::test]
async fn classifies_a_mid_stream_overload_as_retryable() {
    let error = stream_errors(concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n"
    ))
    .await;
    assert_eq!(error.code(), "provider.server_error");
    assert!(error.is_retryable(), "{error}");

    let error = stream_errors(concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"prompt is too long: 300000 tokens\"}}\n\n"
    ))
    .await;
    assert_eq!(error.code(), "provider.context_overflow");
}

#[tokio::test]
async fn stream_requires_message_stop_and_backfills_the_completed_response() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let model = model(
        &server,
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(body, "text/event-stream"),
    )
    .await;
    let events = model.stream_response(request()).collect::<Vec<_>>().await;
    assert!(
        events.iter().all(Result::is_ok),
        "stream errors: {events:?}"
    );
    let completed = events
        .iter()
        .filter_map(|event| event.as_ref().ok())
        .find_map(|event| match event {
            ModelStreamEvent::Completed(response) => Some(response),
            _ => None,
        })
        .expect("stream should finish with a response");
    let RunItemKind::Message(message) = completed.output()[0].kind() else {
        panic!("stream should complete with a message");
    };
    assert_eq!(message.text_content(), "done");
    assert_eq!(completed.usage().input_tokens(), 3);
    assert_eq!(completed.usage().output_tokens(), 2);
}

/// Thinking signatures stay private, and adjacent text blocks share their terminal message id.
#[rstest::rstest]
#[case::contiguous(0, 1, 2)]
#[case::sparse(4, 7, 9)]
#[tokio::test]
async fn neutral_deltas_group_text_blocks_and_stream_thinking_without_signatures(
    #[case] thinking: u64,
    #[case] first: u64,
    #[case] second: u64,
) {
    use ra_core::model::ModelStreamDeltaKind;
    let server = MockServer::start().await;
    let frames = [
        json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":thinking,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":thinking,"delta":{"type":"thinking_delta","thinking":"check"}}),
        json!({"type":"content_block_delta","index":thinking,"delta":{"type":"signature_delta","signature":"secret-signature"}}),
        json!({"type":"content_block_start","index":first,"content_block":{"type":"text","text":"Hel"}}),
        json!({"type":"content_block_delta","index":first,"delta":{"type":"text_delta","text":"lo"}}),
        json!({"type":"content_block_start","index":second,"content_block":{"type":"text","text":" world"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ];
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    let model = model(
        &server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;
    let events: Vec<_> = model
        .stream_response(request())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let started: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::OutputItemAdded(item) => Some(item.item().id().as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["msg:0:reasoning", "msg:1:message"]);
    let deltas: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::Delta(delta) => Some(delta),
            _ => None,
        })
        .collect();
    assert!(
        matches!(deltas[0].kind(), ModelStreamDeltaKind::ReasoningText { content_index: 0, delta } if delta == "check")
    );
    let text: String = deltas
        .iter()
        .filter_map(|delta| match delta.kind() {
            ModelStreamDeltaKind::OutputText { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    assert_eq!(deltas.len(), 4);
    for delta in &deltas {
        assert!(
            !serde_json::to_string(delta)
                .unwrap()
                .contains("secret-signature")
        );
    }
    let ModelStreamEvent::Completed(response) = events.last().unwrap() else {
        panic!("terminal response")
    };
    assert_eq!(
        response
            .output()
            .iter()
            .map(|item| item.id().as_str())
            .collect::<Vec<_>>(),
        started
    );
}
