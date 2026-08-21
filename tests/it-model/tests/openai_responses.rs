use std::{collections::BTreeMap, sync::Arc};

use futures::StreamExt;
use ra_core::{
    error::Recoverability,
    item::{
        AgentId, CallId, Compaction, ContentBlock, FileBlock, FileSource, HandoffCall,
        HandoffOutput, ImageBlock, ImageDetail, ImageSource, ItemId, Message, MessageRole,
        ModelInputItem, OutputPhase, Reasoning, RunItemKind, ToolCall, ToolCallOutput,
    },
    model::{
        Effort, Model, ModelHandoffDefinition, ModelProvider, ModelRequest, ModelSettings,
        ModelStreamEvent, ModelToolDefinition, ProviderKey, ToolChoice,
    },
    prompt::{CachePlan, ContentHash},
    tool::{ObservationMetadata, ToolOutput, ToolOutputBlock, Truncation, TruncationStage},
};
use ra_model::openai::{
    auth::OpenAiAuth,
    responses::{OpenAiResponsesModel, OpenAiResponsesProvider},
};
use ra_model::provider::quirks::ProviderQuirks;
use rstest::rstest;
use serde_json::{Map, Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn resolved(settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn success_payload() -> Value {
    json!({
        "id": "resp_123",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "id": "rs_123",
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "checked"}],
                "content": [{"type": "reasoning_text", "text": "details"}],
                "encrypted_content": "encrypted-replay"
            },
            {
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "status": "completed",
                "content": [{"type": "output_text", "text": "done"}]
            },
            {
                "id": "fc_123",
                "type": "function_call",
                "call_id": "call_123",
                "name": "lookup",
                "arguments": "{\"account_id\":42}",
                "status": "completed"
            },
            {
                "id": "fc_handoff",
                "type": "function_call",
                "call_id": "call_handoff",
                "name": "delegate_research",
                "arguments": "{\"topic\":\"rust\"}",
                "status": "completed"
            }
        ],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 40,
            "total_tokens": 140,
            "input_tokens_details": {"cached_tokens": 60},
            "output_tokens_details": {"reasoning_tokens": 15}
        }
    })
}

async fn mounted_model(server: &MockServer, template: ResponseTemplate) -> OpenAiResponsesModel {
    mounted_model_with_quirks(server, template, ProviderQuirks::new()).await
}

/// A model whose endpoint has declared the capabilities under test.
async fn mounted_model_with_quirks(
    server: &MockServer,
    template: ResponseTemplate,
    quirks: ProviderQuirks,
) -> OpenAiResponsesModel {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer test-secret"))
        .respond_with(template)
        .mount(server)
        .await;
    OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("test-secret")
            .with_base_url(format!("{}/v1/", server.uri()))
            .with_organization("org-test")
            .with_project("project-test")
            .with_default_header("x-client-default", "present"),
    )
    .expect("mock model should build")
    .with_quirks(quirks)
}

#[tokio::test]
async fn request_shape_locks_store_replay_tools_and_stable_instructions() {
    let server = MockServer::start().await;
    let model = mounted_model_with_quirks(
        &server,
        ResponseTemplate::new(200)
            .insert_header("x-request-id", "req_123")
            .set_body_json(success_payload()),
        ProviderQuirks::new().with_prompt_cache_key(true),
    )
    .await;

    let extra_body = Map::from_iter([
        (
            "include".to_owned(),
            json!(["message.output_text.logprobs"]),
        ),
        ("service_tier".to_owned(), json!("flex")),
        ("reasoning".to_owned(), json!({"summary": "auto"})),
    ]);
    let settings = ModelSettings::new()
        .with_max_tokens(512)
        .with_effort(Effort::High)
        .with_tool_choice(ToolChoice::Auto)
        .with_extra_header("x-request-header", "present")
        .with_extra_query("region", "test")
        .with_extra_body(ProviderKey::new("openai"), extra_body.into_iter().collect());
    let input = vec![
        ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![
                ContentBlock::text("inspect"),
                ContentBlock::image(ImageSource::base64("image/png", "aW1hZ2U=")),
            ],
        )),
        ModelInputItem::Message(
            Message::new(MessageRole::Assistant, vec![ContentBlock::text("working")])
                .with_phase(OutputPhase::Commentary),
        ),
        ModelInputItem::Reasoning(
            Reasoning::new()
                .with_id("rs_prior")
                .with_summary(vec!["prior".to_owned()])
                .with_encrypted_content("encrypted-prior"),
        ),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call_remote"),
            json!({"ok": true}),
        )),
        ModelInputItem::Message(Message::system("dynamic tail instructions")),
    ];
    let request = ModelRequest::new(input, resolved(settings))
        .with_system_instructions("stable instructions")
        .with_cache_plan(
            CachePlan::new(ContentHash::compute("stable instructions"))
                .with_cache_scope("thread_123"),
        )
        .with_previous_response_id("resp_previous")
        .with_tools(vec![
            ModelToolDefinition::new("lookup", json!({"type": "object"}))
                .with_description("lookup an account")
                .with_strict(true),
        ])
        .with_handoffs(vec![ModelHandoffDefinition::new(
            AgentId::new("research-agent"),
            "delegate_research",
            json!({"type": "object"}),
        )]);

    let response = model
        .get_response(request)
        .await
        .expect("mock response should convert");

    assert_eq!(response.response_id(), Some("resp_123"));
    assert_eq!(response.request_id(), Some("req_123"));
    assert_eq!(response.usage().input_tokens(), 100);
    assert_eq!(response.usage().cached_input_tokens(), 60);
    assert_eq!(response.usage().reasoning_tokens(), 15);
    assert_eq!(response.output().len(), 4);
    let RunItemKind::Reasoning(reasoning) = response.output()[0].kind() else {
        panic!("first output should be reasoning");
    };
    assert_eq!(reasoning.encrypted_content(), Some("encrypted-replay"));
    assert!(reasoning.provider_data().is_some());
    let RunItemKind::Message(message) = response.output()[1].kind() else {
        panic!("second output should be a message");
    };
    assert_eq!(message.phase(), Some(OutputPhase::Final));
    let RunItemKind::HandoffCall(handoff) = response.output()[3].kind() else {
        panic!("registered handoff function should lift as a handoff");
    };
    assert_eq!(handoff.target_agent().as_str(), "research-agent");
    // Recorded so a later turn can replay it without the handoff still being advertised.
    assert_eq!(handoff.tool_name(), Some("delegate_research"));
    assert_eq!(
        response.output()[0]
            .raw_provider_item()
            .expect("raw item should be retained")
            .provider(),
        "openai"
    );

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    assert_eq!(requests.len(), 1);
    let body: Value =
        serde_json::from_slice(&requests[0].body).expect("request should contain JSON");
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["instructions"], "stable instructions");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "this fixture's instructions and single tool are far below the caching floor, so the \
         declared capability alone does not put a key on the wire"
    );
    assert_eq!(body["previous_response_id"], "resp_previous");
    assert!(body.get("conversation").is_none());
    assert_eq!(body["service_tier"], "flex");
    assert_eq!(body["max_output_tokens"], 512);
    // `effort` merges into the provider-private reasoning object instead of replacing it.
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        body["include"],
        json!([
            "message.output_text.logprobs",
            "reasoning.encrypted_content"
        ])
    );
    assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(
        body["input"][1]["phase"],
        Value::String("commentary".to_owned())
    );
    assert_eq!(body["input"][2]["encrypted_content"], "encrypted-prior");
    assert_eq!(body["input"][3]["type"], "function_call_output");
    assert_eq!(body["input"][4]["role"], "system");
    assert_eq!(
        body["input"][4]["content"][0]["text"],
        "dynamic tail instructions"
    );
    assert!(
        requests[0]
            .url
            .query()
            .is_some_and(|query| query == "region=test")
    );
    let headers = &requests[0].headers;
    assert_eq!(
        headers
            .get("openai-organization")
            .and_then(|v| v.to_str().ok()),
        Some("org-test")
    );
    assert_eq!(
        headers.get("openai-project").and_then(|v| v.to_str().ok()),
        Some("project-test")
    );
    assert_eq!(
        headers
            .get("x-client-default")
            .and_then(|v| v.to_str().ok()),
        Some("present")
    );
    assert_eq!(
        headers
            .get("x-request-header")
            .and_then(|v| v.to_str().ok()),
        Some("present")
    );
}

#[tokio::test]
async fn store_follows_the_continuation_mode_and_rejects_the_broken_combination() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_store", "output": []})),
    )
    .await;

    // No server-side state: replay history locally and keep nothing on the server.
    model
        .get_response(ModelRequest::new(vec![], resolved(ModelSettings::new())))
        .await
        .expect("stateless request should succeed");
    // Chaining requires the previous response to still exist server-side.
    model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new()))
                .with_previous_response_id("resp_previous"),
        )
        .await
        .expect("chained request should succeed");

    let bodies = server
        .received_requests()
        .await
        .expect("wiremock should retain requests")
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).expect("JSON body"))
        .collect::<Vec<_>>();
    assert_eq!(bodies[0]["store"], false);
    assert_eq!(bodies[1]["store"], true);

    let conflicting = ModelSettings::new().with_extra_body(
        ProviderKey::new("openai"),
        BTreeMap::from_iter([("store".to_owned(), json!(false))]),
    );
    let error = model
        .get_response(
            ModelRequest::new(vec![], resolved(conflicting))
                .with_previous_response_id("resp_previous"),
        )
        .await
        .expect_err("an unresolvable chain should fail locally");
    assert_eq!(error.code(), "caller");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("wiremock should retain requests")
            .len(),
        2
    );
}

#[tokio::test]
async fn handoff_history_replays_after_control_moved_to_the_target_agent() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_handoff", "output": []})),
    )
    .await;

    // The receiving agent no longer advertises the handoff that transferred control to it.
    let input = vec![
        ModelInputItem::HandoffCall(
            HandoffCall::new(
                CallId::new("call_handoff"),
                AgentId::new("research-agent"),
                json!({"topic": "rust"}),
            )
            .with_tool_name("delegate_research"),
        ),
        ModelInputItem::HandoffOutput(HandoffOutput::new(
            CallId::new("call_handoff"),
            AgentId::new("triage-agent"),
            AgentId::new("research-agent"),
        )),
    ];
    model
        .get_response(ModelRequest::new(input, resolved(ModelSettings::new())))
        .await
        .expect("handoff history should still lower");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert_eq!(body["input"][0]["type"], "function_call");
    assert_eq!(body["input"][0]["name"], "delegate_research");
    assert_eq!(body["input"][1]["type"], "function_call_output");
}

#[tokio::test]
async fn compacted_history_and_unanswered_calls_survive_the_provider_boundary() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_compact", "output": []})),
    )
    .await;

    let input = vec![
        ModelInputItem::Compaction(Compaction::new(
            "earlier turns: the user asked for a refactor",
            vec![ItemId::new("item-1")],
        )),
        // R1-17 drops this call: it has no output, so replaying it is a 400.
        ModelInputItem::Reasoning(Reasoning::new().with_id("rs_pending")),
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call_pending"),
            "pending",
            json!({}),
        )),
    ];
    model
        .get_response(ModelRequest::new(input, resolved(ModelSettings::new())))
        .await
        .expect("compacted history should lower");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    let items = body["input"].as_array().expect("input should be an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(
        items[0]["content"][0]["text"],
        "earlier turns: the user asked for a refactor"
    );
}

#[tokio::test]
async fn hosted_tools_from_extra_body_survive_alongside_function_tools() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_tools", "output": []})),
    )
    .await;

    let settings = ModelSettings::new().with_extra_body(
        ProviderKey::new("openai"),
        BTreeMap::from_iter([("tools".to_owned(), json!([{"type": "web_search"}]))]),
    );
    model
        .get_response(
            ModelRequest::new(vec![], resolved(settings)).with_tools(vec![
                ModelToolDefinition::new("lookup", json!({"type": "object"})),
            ]),
        )
        .await
        .expect("merged tool request should succeed");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert_eq!(body["tools"][0], json!({"type": "web_search"}));
    assert_eq!(body["tools"][1]["name"], "lookup");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
}

#[tokio::test]
async fn tool_names_the_endpoint_would_reject_fail_locally() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_name", "output": []})),
    )
    .await;

    // `ra-core` keeps tool names open because the character set is an OpenAI fact, so the
    // constraint has to be enforced here rather than discovered as a 400.
    let error = model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new())).with_tools(vec![
                ModelToolDefinition::new("mcp.github.search", json!({"type": "object"})),
            ]),
        )
        .await
        .expect_err("a dotted tool name is not accepted by OpenAI");
    assert_eq!(error.code(), "caller");

    for choice in ["bad.name", "missing"] {
        let request = ModelRequest::new(
            vec![],
            resolved(ModelSettings::new().with_tool_choice(ToolChoice::Tool(choice.to_owned()))),
        )
        .with_tools(vec![ModelToolDefinition::new(
            "lookup",
            json!({"type": "object"}),
        )]);
        let error = model
            .get_response(request)
            .await
            .expect_err("named tool choice must be valid and present in the request");
        assert_eq!(error.code(), "caller");
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock should answer")
            .is_empty()
    );
}

#[tokio::test]
async fn truncated_responses_are_not_reported_as_finished_answers() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_truncated",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "id": "msg_partial",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "half a sen"}]
            }]
        })),
    )
    .await;

    let error = model
        .get_response(ModelRequest::new(vec![], resolved(ModelSettings::new())))
        .await
        .expect_err("a truncated response must not lift as a final answer");
    assert_eq!(error.code(), "provider.context_overflow");
}

#[tokio::test]
async fn local_image_paths_are_materialized_only_at_the_provider_boundary() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_image", "output": []})),
    )
    .await;
    let image_path =
        std::env::temp_dir().join(format!("rusty-agent-r1-4-{}.webp", std::process::id()));
    std::fs::write(&image_path, b"image-bytes").expect("fixture image should be writable");

    let result = model
        .get_response(ModelRequest::new(
            vec![ModelInputItem::Message(Message::new(
                MessageRole::User,
                vec![ContentBlock::image_path(&image_path)],
            ))],
            resolved(ModelSettings::new()),
        ))
        .await;
    std::fs::remove_file(&image_path).expect("fixture image should be removable");
    result.expect("local image request should succeed");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    let image_url = body["input"][0]["content"][0]["image_url"]
        .as_str()
        .expect("image URL should be a string");
    assert!(image_url.starts_with("data:image/webp;base64,"));
}

#[tokio::test]
async fn conversation_id_is_lowered_without_previous_response_id() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_conversation",
            "output": [],
            "usage": null
        })),
    )
    .await;
    model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new()))
                .with_conversation_id("conv_123"),
        )
        .await
        .expect("conversation request should succeed");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert_eq!(body["conversation"], "conv_123");
    assert!(body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn provider_caches_models_and_redacts_credentials_from_debug() {
    let auth = OpenAiAuth::new("super-secret").with_base_url("https://example.test/v1/");
    assert!(!format!("{auth:?}").contains("super-secret"));
    let provider =
        OpenAiResponsesProvider::new(auth, "default-model").expect("provider should build");
    let first = provider
        .get_model(None)
        .expect("default model should resolve");
    let second = provider
        .get_model(Some("default-model"))
        .expect("named default should resolve");
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn http_errors_map_to_provider_recoverability() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(429).set_body_json(json!({
            "error": {"code": "rate_limit_exceeded", "message": "slow down"}
        })),
    )
    .await;
    let error = model
        .get_response(ModelRequest::new(vec![], resolved(ModelSettings::new())))
        .await
        .expect_err("429 should fail");
    assert_eq!(error.code(), "provider.rate_limit");
    assert_eq!(error.recoverability(), Recoverability::Retryable);
}

#[tokio::test]
async fn invalid_function_arguments_are_model_behavior_errors() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_bad_call",
            "output": [{
                "type": "function_call",
                "call_id": "call_bad",
                "name": "lookup",
                "arguments": "{not-json"
            }]
        })),
    )
    .await;
    let error = model
        .get_response(ModelRequest::new(vec![], resolved(ModelSettings::new())))
        .await
        .expect_err("invalid model arguments should fail conversion");
    assert_eq!(error.code(), "provider.behavior");
    assert_eq!(error.recoverability(), Recoverability::RetryableWithChange);
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

/// Renders frames as an event stream body.
fn sse_body(frames: &[Value]) -> String {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

/// The frames a finished turn arrives as, ending with the response object itself.
fn stream_frames() -> Vec<Value> {
    let payload = success_payload();
    let output = payload["output"]
        .as_array()
        .expect("fixture output")
        .clone();
    let mut frames = vec![json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": "resp_123", "status": "in_progress"}
    })];
    for (index, item) in output.iter().enumerate() {
        frames.push(json!({
            "type": "response.output_item.added",
            "sequence_number": index * 2 + 1,
            "output_index": index,
            "item": {"id": item["id"], "type": item["type"]}
        }));
        frames.push(json!({
            "type": "response.output_item.done",
            "sequence_number": index * 2 + 2,
            "output_index": index,
            "item": item
        }));
    }
    frames.push(json!({
        "type": "response.completed",
        "sequence_number": output.len() * 2 + 1,
        "response": payload
    }));
    frames
}

async fn streamed_events(
    server: &MockServer,
    frames: &[Value],
) -> Vec<ra_core::error::Result<ModelStreamEvent>> {
    let model = mounted_model(
        server,
        ResponseTemplate::new(200)
            .insert_header("x-request-id", "req_stream")
            .set_body_raw(sse_body(frames), "text/event-stream"),
    )
    .await;
    model
        .stream_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new())).with_handoffs(vec![
                ModelHandoffDefinition::new(
                    AgentId::new("research-agent"),
                    "delegate_research",
                    json!({"type": "object"}),
                ),
            ]),
        )
        .collect::<Vec<_>>()
        .await
}

/// The provider's own events are forwarded in full, and the items are lifted as they finish.
///
/// Nothing is synthesized on this protocol: the frames already carry types, output indexes and
/// sequence numbers, so a consumer reading the raw channel is reading `OpenAI`'s event schema
/// rather than a shape this crate invented.
#[tokio::test]
async fn the_stream_forwards_every_provider_event_and_lifts_items_as_they_finish() {
    let server = MockServer::start().await;
    let events = streamed_events(&server, &stream_frames()).await;

    let mut raw = Vec::new();
    let mut items = Vec::new();
    let mut terminal = None;
    for event in events {
        match event.expect("no frame in this fixture fails") {
            ModelStreamEvent::RawResponse(event) => raw.push(event.event_type().to_owned()),
            ModelStreamEvent::RunItem(event) => items.push(event.item().id().as_str().to_owned()),
            ModelStreamEvent::Completed(response) => terminal = Some(*response),
            _ => panic!("unexpected model stream event"),
        }
    }

    assert_eq!(raw.first().map(String::as_str), Some("response.created"));
    assert_eq!(raw.last().map(String::as_str), Some("response.completed"));
    assert_eq!(
        raw.iter()
            .filter(|event| *event == "response.output_item.done")
            .count(),
        4,
        "a frame that is lifted is still forwarded, so a protocol-aware consumer sees it too"
    );
    // The identifiers come from the items themselves, so the same turn read either way names its
    // items identically.
    assert_eq!(items, ["rs_123", "msg_123", "fc_123", "fc_handoff"]);

    let terminal = terminal.expect("a completed stream states its terminal facts");
    assert_eq!(terminal.response_id(), Some("resp_123"));
    assert_eq!(terminal.request_id(), Some("req_stream"));
    assert_eq!(terminal.usage().cached_input_tokens(), 60);
    assert_eq!(
        terminal
            .output()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        items,
        "the terminal response and the streamed items describe one turn"
    );
}

/// The completed frame closes the model channel even if a broken endpoint sends more SSE data.
#[tokio::test]
async fn a_completed_response_is_the_last_model_stream_event() {
    let server = MockServer::start().await;
    let mut frames = stream_frames();
    frames.push(json!({
        "type": "response.output_text.delta",
        "sequence_number": 99,
        "output_index": 1,
        "delta": "must not be forwarded"
    }));
    let events = streamed_events(&server, &frames).await;

    assert!(matches!(
        events.last(),
        Some(Ok(ModelStreamEvent::Completed(_)))
    ));
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Ok(ModelStreamEvent::RawResponse(event))
                if event.event_type() == "response.output_text.delta"
        )),
        "data after response.completed must not escape the terminal model stream"
    );
}

/// The streamed entry point asks for a stream; the other one must not.
#[tokio::test]
async fn only_the_streaming_entry_point_asks_the_endpoint_to_stream() {
    let server = MockServer::start().await;
    let _ = streamed_events(&server, &stream_frames()).await;
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert_eq!(body["stream"], true);

    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
    )
    .await;
    model
        .get_response(ModelRequest::new(vec![], resolved(ModelSettings::new())))
        .await
        .expect("the non-streaming path still works");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert!(
        body.get("stream").is_none(),
        "a body waiting for one document must not ask for a stream: {body}"
    );
}

/// Running out of bytes is not the provider saying it finished.
#[tokio::test]
async fn a_stream_without_the_terminal_frame_is_refused() {
    let server = MockServer::start().await;
    let mut frames = stream_frames();
    frames.pop();
    let events = streamed_events(&server, &frames).await;

    let error = events
        .into_iter()
        .find_map(Result::err)
        .expect("an unsettled stream must report the failure");
    assert!(
        error.to_string().contains("without `response.completed`"),
        "unexpected message: {error}"
    );
}

/// A failure frame states an outcome that nothing later in the stream repeats.
#[rstest]
#[case::truncated("max_output_tokens", "provider.context_overflow")]
#[case::filtered("content_filter", "provider.refusal")]
#[tokio::test]
async fn a_failure_frame_is_classified_rather_than_read_as_a_dropped_connection(
    #[case] reason: &str,
    #[case] expected: &str,
) {
    let server = MockServer::start().await;
    let events = streamed_events(
        &server,
        &[
            json!({"type": "response.created", "response": {"id": "resp_123"}}),
            json!({
                "type": "response.incomplete",
                "response": {
                    "id": "resp_123",
                    "status": "incomplete",
                    "incomplete_details": {"reason": reason}
                }
            }),
        ],
    )
    .await;

    let error = events
        .into_iter()
        .find_map(Result::err)
        .expect("a failure frame must fail the stream");
    assert_eq!(error.code(), expected);
}

/// An endpoint that ignores `stream=true` answers with one document and no events at all.
#[tokio::test]
async fn an_endpoint_that_ignores_the_stream_flag_is_reported_as_such() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
    )
    .await;
    let events = model
        .stream_response(ModelRequest::new(vec![], resolved(ModelSettings::new())))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.len(), 1);
    let error = events
        .into_iter()
        .find_map(Result::err)
        .expect("a non-stream answer must fail rather than settle as an empty turn");
    assert!(
        error.to_string().contains("does not honour `stream=true`"),
        "unexpected message: {error}"
    );
}

#[tokio::test]
async fn unsupported_responses_settings_fail_before_http() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "unused", "output": []})),
    )
    .await;
    let error = model
        .get_response(ModelRequest::new(
            vec![],
            resolved(ModelSettings::new().with_frequency_penalty(0.5)),
        ))
        .await
        .expect_err("unsupported settings should fail locally");
    assert_eq!(error.code(), "caller");
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock should answer")
            .is_empty()
    );
}

#[tokio::test]
async fn test_openai_responses_01() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_output", "output": []})),
    )
    .await;

    // R2-3: the structured value is what the session stores; this is the one point where its
    // metadata becomes model-visible, and it becomes prose rather than fields.
    let output = ToolOutput::new(vec![
        ToolOutputBlock::text("hit-1"),
        ToolOutputBlock::Image(
            ImageBlock::new(ImageSource::provider_file("file-1")).with_detail(ImageDetail::Low),
        ),
        ToolOutputBlock::File(FileBlock::new(FileSource::url("https://example.com/a.pdf"))),
    ])
    .expect("blocks are non-empty")
    .with_metadata(
        ObservationMetadata::new()
            .with_truncation(Truncation::new(TruncationStage::Tool, 9_000, 200))
            .with_guidance("narrow the search with a path prefix"),
    );
    let input = vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call_structured"),
        serde_json::to_value(&output).expect("tool output should serialize"),
    ))];

    model
        .get_response(ModelRequest::new(input, resolved(ModelSettings::new())))
        .await
        .expect("structured tool output should lower");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    let parts = body["input"][0]["output"]
        .as_array()
        .expect("structured output must lower to a content-part array");

    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0]["type"], "input_text");
    let note = parts[0]["text"].as_str().expect("metadata block is text");
    assert!(note.contains("truncated by tool"));
    assert!(note.contains("narrow the search with a path prefix"));
    assert_eq!(parts[1], json!({"type": "input_text", "text": "hit-1"}));
    assert_eq!(
        parts[2],
        json!({"type": "input_image", "file_id": "file-1", "detail": "low"})
    );
    assert_eq!(
        parts[3],
        json!({"type": "input_file", "file_url": "https://example.com/a.pdf"})
    );
}

#[tokio::test]
async fn test_openai_responses_02() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_legacy", "output": []})),
    )
    .await;

    // Hosts stored bare values before R2-3 existed, and a resumed session replays them. Failing
    // here would make an old transcript unreplayable for a shape the framework itself once wrote.
    let input = vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call_legacy"),
        json!({"rows": 2}),
    ))];

    model
        .get_response(ModelRequest::new(input, resolved(ModelSettings::new())))
        .await
        .expect("host payloads should still lower");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert_eq!(body["input"][0]["output"], json!("{\"rows\":2}"));
}

#[tokio::test]
async fn test_openai_responses_03() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_legacy_text", "output": []})),
    )
    .await;

    let input = vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call_legacy_text"),
        json!({"type": "text", "text": "written before R2-3"}),
    ))];

    model
        .get_response(ModelRequest::new(input, resolved(ModelSettings::new())))
        .await
        .expect("R2-1 tool output should replay");

    let requests = server
        .received_requests()
        .await
        .expect("request should exist");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request should be JSON");
    assert_eq!(
        body["input"][0]["output"],
        json!([{"type": "input_text", "text": "written before R2-3"}])
    );
}

#[tokio::test]
async fn test_openai_responses_04() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"id": "resp_unreadable", "output": []})),
    )
    .await;

    // A record a newer build wrote: it claims to be a tool result and this build has no variant
    // for its block kind. Falling back to stringification would paste that JSON into the model's
    // context, which is the failure the structured shape exists to end.
    let input = vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call_unreadable"),
        json!({
            "schema_version": 1,
            "blocks": [{"type": "hologram", "data": "…"}]
        }),
    ))];

    let error = model
        .get_response(ModelRequest::new(input, resolved(ModelSettings::new())))
        .await
        .expect_err("an unreadable tool result must not be silently stringified");

    assert!(error.to_string().contains("unreadable"));
    assert!(error.to_string().contains("hologram"));
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock should answer")
            .is_empty()
    );
}

/// An endpoint that has not declared the capability receives no `prompt_cache_key`.
///
/// Responses accepts the field, but whether *this* endpoint honours it follows from which vendor
/// answers, not from the wire format: a compatible gateway behind a custom base URL speaks the same
/// protocol and may reject it. The scope is still carried and validated — it simply is not sent.
#[tokio::test]
async fn a_cache_scope_is_not_lowered_to_an_endpoint_that_did_not_declare_support() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
    )
    .await;

    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::text("hello")],
        ))],
        resolved(ModelSettings::new()),
    )
    .with_system_instructions("stable instructions")
    .with_cache_plan(
        CachePlan::new(ContentHash::compute("stable instructions")).with_cache_scope("thread_123"),
    );

    model
        .get_response(request)
        .await
        .expect("an undeclarable cache scope must not fail the request");

    let requests = server.received_requests().await.expect("recorded requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request should contain JSON");
    assert_eq!(body["instructions"], "stable instructions");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "an endpoint that may reject the field must not be sent it"
    );
}

/// A plan whose hash does not name the instructions being sent fails before the call is paid for.
///
/// The hash is the plan's only claim about what it applies to. A plan naming other bytes would have
/// every later cache decision reasoning about a prefix this request never contained.
#[tokio::test]
async fn a_cache_plan_naming_other_bytes_fails_the_request() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
    )
    .await;

    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::text("hello")],
        ))],
        resolved(ModelSettings::new()),
    )
    .with_system_instructions("stable instructions")
    .with_cache_plan(CachePlan::new(ContentHash::compute(
        "different instructions",
    )));

    let err = model
        .get_response(request)
        .await
        .expect_err("a plan naming other bytes must not be sent");
    assert!(
        err.to_string().contains("cache plan"),
        "the error must point at the plan: {err}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "the request must fail before it is paid for"
    );
}

/// A static `extra_body` cache key is rejected rather than silently winning or losing.
///
/// `extra_body` is provider *registration* data, shared by every run of that provider, while a
/// cache scope has a session lifetime. A key placed there routes every run onto one bucket — which
/// reads as configured caching while defeating the very scoping it appears to configure. Failing
/// names the conflict at the one moment someone can fix it.
#[tokio::test]
async fn a_static_extra_body_cache_key_is_rejected() {
    let server = MockServer::start().await;
    let model = mounted_model_with_quirks(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
        ProviderQuirks::new().with_prompt_cache_key(true),
    )
    .await;

    let extra_body = Map::from_iter([("prompt_cache_key".to_owned(), json!("static_key"))]);
    let settings = ModelSettings::new()
        .with_extra_body(ProviderKey::new("openai"), extra_body.into_iter().collect());

    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::text("hello")],
        ))],
        resolved(settings),
    )
    .with_system_instructions("stable instructions")
    .with_cache_plan(
        CachePlan::new(ContentHash::compute("stable instructions")).with_cache_scope("run-7"),
    );

    let err = model
        .get_response(request)
        .await
        .expect_err("a static cache key must not silently replace the run scope");
    assert!(
        err.to_string().contains("prompt_cache_key"),
        "the error must name the offending field: {err}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "the conflict must be reported before the request is paid for"
    );
}

/// Hosted tools from `extra_body` count toward the caching floor, exactly as neutral ones do.
///
/// This is the case no earlier layer can decide. Hosted tools — web search, file search, hosted MCP
/// — have no protocol-neutral representation and arrive only through `extra_body`, so preparation
/// cannot see them. A short instruction block in front of a large hosted-tool table is well past
/// the floor, and measuring before the merge would silently deny it the discount.
#[tokio::test]
async fn hosted_tools_count_toward_the_caching_floor() {
    let server = MockServer::start().await;
    let model = mounted_model_with_quirks(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
        ProviderQuirks::new().with_prompt_cache_key(true),
    )
    .await;

    // One large hosted tool, declared the only way hosted tools can be declared.
    let hosted = json!([{
        "type": "mcp",
        "name": "hosted_search",
        "description": "x".repeat(8_000),
    }]);
    let extra_body = Map::from_iter([("tools".to_owned(), hosted)]);
    let settings = ModelSettings::new()
        .with_extra_body(ProviderKey::new("openai"), extra_body.into_iter().collect());

    let short_instructions = "Be helpful.";
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::text("hello")],
        ))],
        resolved(settings),
    )
    .with_system_instructions(short_instructions)
    .with_cache_plan(
        CachePlan::new(ContentHash::compute(short_instructions)).with_cache_scope("run-7"),
    );

    model
        .get_response(request)
        .await
        .expect("the request must be sent");

    let requests = server.received_requests().await.expect("recorded requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request should contain JSON");
    assert_eq!(
        body["prompt_cache_key"], "run-7",
        "the hosted tool table puts the cached prefix past the floor, so the scope is sent"
    );
}

/// A short prefix with nothing else in front of it is not worth a cache key.
#[tokio::test]
async fn a_short_span_with_no_tool_table_gets_no_cache_key() {
    let server = MockServer::start().await;
    let model = mounted_model_with_quirks(
        &server,
        ResponseTemplate::new(200).set_body_json(success_payload()),
        ProviderQuirks::new().with_prompt_cache_key(true),
    )
    .await;

    let short_instructions = "Be helpful.";
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::text("hello")],
        ))],
        resolved(ModelSettings::new()),
    )
    .with_system_instructions(short_instructions)
    .with_cache_plan(
        CachePlan::new(ContentHash::compute(short_instructions)).with_cache_scope("run-7"),
    );

    model
        .get_response(request)
        .await
        .expect("the request must be sent");

    let requests = server.received_requests().await.expect("recorded requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request should contain JSON");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "no provider caches a span this short, so asking buys nothing"
    );
}
