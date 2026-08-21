//! Contract tests for the `OpenAI` Chat Completions adapter.
//!
//! The conversion cases below each lock one boundary condition that the reference implementation
//! carries defensive code for, and the streaming cases each lock one reassembly rule. They are
//! written against the wire body and the emitted events rather than against internal helpers,
//! because the whole point of both is what the endpoint and the consumer actually see.

use std::sync::Arc;

use futures::StreamExt;
use ra_core::{
    error::Recoverability,
    item::{
        AgentId, CallId, Compaction, ContentBlock, ImageSource, Message, MessageRole,
        ModelInputItem, OutputPhase, Reasoning, RunItemKind, ToolCall, ToolCallOutput,
    },
    model::{
        Effort, Model, ModelHandoffDefinition, ModelRequest, ModelSettings, ModelStreamEvent,
        ModelToolDefinition, ProviderKey, ToolChoice,
    },
    prompt::{CachePlan, ContentHash},
    tool::{ToolOutput, ToolOutputBlock},
};
use ra_model::openai::{
    auth::OpenAiAuth,
    chat::{ChatLoweringOptions, OpenAiChatModel, reasoning::ReasoningReplayPolicy},
};
use ra_model::provider::quirks::ProviderQuirks;
use rstest::rstest;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "chat-test";

fn resolved(settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn request(input: Vec<ModelInputItem>) -> ModelRequest {
    ModelRequest::new(input, resolved(ModelSettings::new()))
}

/// A model whose endpoint declares nothing beyond the bare protocol.
async fn mounted_model(server: &MockServer, template: ResponseTemplate) -> OpenAiChatModel {
    mounted_model_with(server, template, ProviderQuirks::new()).await
}

async fn mounted_model_with(
    server: &MockServer,
    template: ResponseTemplate,
    quirks: ProviderQuirks,
) -> OpenAiChatModel {
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
    .with_quirks(quirks)
}

fn empty_completion() -> Value {
    json!({
        "id": "chatcmpl_empty",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "ok"}
        }]
    })
}

/// Sends the request and returns the JSON body that reached the endpoint.
async fn sent_body(server: &MockServer, model: &OpenAiChatModel, request: ModelRequest) -> Value {
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

/// Sends the request expecting the adapter to refuse it before any conversion.
async fn rejected(model: &OpenAiChatModel, request: ModelRequest) -> String {
    model
        .get_response(request)
        .await
        .expect_err("request should be refused")
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// Request shape
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn request_shape_locks_messages_tools_and_declared_capabilities() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200)
            .insert_header("x-request-id", "req_chat")
            .set_body_json(empty_completion()),
        ProviderQuirks::new()
            .with_store(true)
            .with_parallel_tool_calls(true),
    )
    .await;

    let settings = ModelSettings::new()
        .with_max_tokens(512)
        .with_effort(Effort::High)
        .with_frequency_penalty(0.5)
        .with_presence_penalty(0.25);
    let input = vec![
        ModelInputItem::Message(Message::user("inspect")),
        ModelInputItem::Message(Message::system("dynamic tail instructions")),
    ];
    let request = ModelRequest::new(input, resolved(settings))
        .with_system_instructions("stable instructions")
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

    let body = sent_body(&server, &model, request).await;

    assert_eq!(body["model"], MODEL);
    // The stable prefix is the first message, which is the position the protocol caches from.
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "stable instructions"})
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][2]["content"], "dynamic tail instructions");
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["frequency_penalty"], 0.5);
    assert_eq!(body["presence_penalty"], 0.25);
    assert_eq!(body["store"], true);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["tool_choice"], "auto");
    // Chat nests the declaration one level deeper than Responses does.
    assert_eq!(
        body["tools"][0],
        json!({
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "lookup an account",
                "parameters": {"type": "object"},
                "strict": true
            }
        })
    );
    assert_eq!(body["tools"][1]["function"]["name"], "delegate_research");
    assert!(
        body.get("stream").is_none(),
        "a non-streaming call must not advertise streaming"
    );
}

#[tokio::test]
async fn undeclared_endpoints_receive_none_of_the_optional_fields() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let request = request(vec![ModelInputItem::Message(Message::user("hello"))]).with_tools(vec![
        ModelToolDefinition::new("lookup", json!({"type": "object"})),
    ]);
    let body = sent_body(&server, &model, request).await;

    assert!(
        body.get("store").is_none(),
        "an endpoint that never declared retention must not be told to store"
    );
    assert!(
        body.get("parallel_tool_calls").is_none(),
        "a gateway that rejects the field must not receive it merely because tools exist"
    );
    assert!(body.get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn an_explicit_parallel_setting_outranks_the_endpoint_declaration() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let settings = ModelSettings::new().with_parallel_tool_calls(false);
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        resolved(settings),
    )
    .with_tools(vec![ModelToolDefinition::new(
        "lookup",
        json!({"type": "object"}),
    )]);
    let body = sent_body(&server, &model, request).await;

    // The caller said these tools must not run concurrently. Dropping that because a capability
    // was never declared would run them concurrently anyway.
    assert_eq!(body["parallel_tool_calls"], false);
}

#[tokio::test]
async fn a_cache_scope_needs_both_a_declared_endpoint_and_a_prefix_worth_caching() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_prompt_cache_key(true),
    )
    .await;

    let instructions = "stable instructions. ".repeat(600);
    let request = request(vec![ModelInputItem::Message(Message::user("hello"))])
        .with_system_instructions(instructions.clone())
        .with_cache_plan(
            CachePlan::new(ContentHash::compute(&instructions)).with_cache_scope("thread_123"),
        );
    let body = sent_body(&server, &model, request).await;

    assert_eq!(body["prompt_cache_key"], "thread_123");
}

#[tokio::test]
async fn server_managed_continuation_is_refused_rather_than_silently_dropped() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let request = request(vec![ModelInputItem::Message(Message::user("hello"))])
        .with_previous_response_id("resp_previous");
    let message = rejected(&model, request).await;
    assert!(
        message.contains("server-managed conversation state"),
        "unexpected message: {message}"
    );
}

#[tokio::test]
async fn a_hosted_mcp_tool_choice_has_no_chat_representation() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let settings = ModelSettings::new().with_tool_choice(ToolChoice::Mcp(
        ra_core::model::McpToolChoice::new("server", "search"),
    ));
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        resolved(settings),
    )
    .with_tools(vec![ModelToolDefinition::new(
        "lookup",
        json!({"type": "object"}),
    )]);
    let message = rejected(&model, request).await;
    assert!(
        message.contains("hosted MCP tool"),
        "unexpected message: {message}"
    );
}

// ---------------------------------------------------------------------------------------------
// Conversion: the seven boundary conditions
// ---------------------------------------------------------------------------------------------

/// An empty `tool_calls` array is rejected by the API, so the field must be absent instead.
#[tokio::test]
async fn an_assistant_turn_without_tool_calls_omits_the_field_entirely() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let request = request(vec![
        ModelInputItem::Message(Message::user("hello")),
        ModelInputItem::Message(Message::assistant("hi there", OutputPhase::Final)),
    ]);
    let body = sent_body(&server, &model, request).await;

    let assistant = &body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "hi there");
    assert!(
        assistant.get("tool_calls").is_none(),
        "an empty array here fails the request: {assistant}"
    );
}

/// Reasoning left over from a turn that ended must not attach itself to the next one.
#[tokio::test]
async fn dangling_reasoning_content_does_not_contaminate_a_later_turn() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_reasoning_content(true),
    )
    .await;

    let request = request(vec![
        ModelInputItem::Reasoning(
            Reasoning::new()
                .with_summary(vec!["first turn thinking".to_owned()])
                .with_provider_data(json!({"model": MODEL})),
        ),
        // No tool call follows, so this turn is over and the reasoning belongs to nothing.
        ModelInputItem::Message(Message::assistant("done", OutputPhase::Final)),
        ModelInputItem::Message(Message::user("and again?")),
        ModelInputItem::Message(Message::assistant("still done", OutputPhase::Final)),
    ]);
    let body = sent_body(&server, &model, request).await;

    assert_eq!(
        body["messages"][0]["reasoning_content"],
        "first turn thinking"
    );
    let later = &body["messages"][2];
    assert_eq!(later["content"], "still done");
    assert!(
        later.get("reasoning_content").is_none(),
        "reasoning from an earlier turn leaked forward: {later}"
    );
}

/// A signature is minted for one turn, so an orphaned reasoning item must not donate its blocks.
#[tokio::test]
async fn thinking_blocks_do_not_survive_into_a_later_assistant_turn() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_thinking_blocks(true),
    )
    .await;

    let request = request(vec![
        ModelInputItem::Reasoning(
            Reasoning::new()
                .with_content(vec!["weighing options".to_owned()])
                .with_encrypted_content("sig-one")
                .with_provider_data(json!({"model": MODEL})),
        ),
        // A user message closes the turn the reasoning belonged to.
        ModelInputItem::Message(Message::user("never mind")),
        ModelInputItem::Message(Message::assistant("ok", OutputPhase::Final)),
    ]);
    let body = sent_body(&server, &model, request).await;

    // The reasoning produced no message of its own, so the user turn is first.
    let assistant = &body["messages"][1];
    assert_eq!(assistant["content"], "ok");
    assert!(
        assistant.get("thinking_blocks").is_none(),
        "signed blocks were re-attributed to a later turn: {assistant}"
    );
}

/// Sibling neutral items become one assistant message with the calls beside the content.
#[tokio::test]
async fn reasoning_message_and_tool_calls_merge_into_one_assistant_message() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_reasoning_content(true),
    )
    .await;

    let request = request(vec![
        ModelInputItem::Message(Message::user("look it up")),
        ModelInputItem::Reasoning(
            Reasoning::new()
                .with_summary(vec!["needs the account tool".to_owned()])
                .with_provider_data(json!({"model": MODEL})),
        ),
        ModelInputItem::Message(Message::assistant("checking", OutputPhase::Commentary)),
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call_a"),
            "lookup",
            json!({"account_id": 42}),
        )),
        ModelInputItem::ToolCall(ToolCall::new(CallId::new("call_b"), "lookup", Value::Null)),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(CallId::new("call_a"), json!("found"))),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(CallId::new("call_b"), json!("found"))),
    ]);
    let body = sent_body(&server, &model, request).await;

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 4, "unexpected shape: {messages:#?}");
    let assistant = &messages[1];
    assert_eq!(assistant["content"], "checking");
    assert_eq!(assistant["reasoning_content"], "needs the account tool");
    assert_eq!(assistant["tool_calls"].as_array().map(Vec::len), Some(2));
    assert_eq!(assistant["tool_calls"][0]["id"], "call_a");
    // Arguments travel as a JSON string, and an absent object is `{}` rather than `null`.
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        "{\"account_id\":42}"
    );
    assert_eq!(assistant["tool_calls"][1]["function"]["arguments"], "{}");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_a");
}

/// A tool result carries text here; the rest needs an endpoint that declared it can take it.
#[tokio::test]
async fn tool_results_keep_text_and_need_a_declaration_for_anything_else() {
    let output = ToolOutput::new(vec![
        ToolOutputBlock::text("the readout"),
        ToolOutputBlock::Image(ra_core::item::ImageBlock::new(ImageSource::base64(
            "image/png",
            "aW1hZ2U=",
        ))),
    ])
    .expect("tool output should build");
    let stored = serde_json::to_value(&output).expect("tool output should serialize");

    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await
    .with_lowering_options(ChatLoweringOptions::new().with_strict_feature_validation(false));

    let body = sent_body(
        &server,
        &model,
        request(vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call_a"),
            stored.clone(),
        ))]),
    )
    .await;
    assert_eq!(body["messages"][0]["content"], "the readout");

    let declared = MockServer::start().await;
    let multimodal = mounted_model_with(
        &declared,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_multimodal_tool_output(true),
    )
    .await;
    let body = sent_body(
        &declared,
        &multimodal,
        request(vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call_a"),
            stored,
        ))]),
    )
    .await;
    let parts = body["messages"][0]["content"]
        .as_array()
        .expect("declared endpoints receive every part");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["type"], "image_url");
}

/// A result with nothing sendable still has to answer its call, and strict mode says so loudly.
#[tokio::test]
async fn a_tool_result_with_no_text_is_named_rather_than_emptied() {
    let output = ToolOutput::block(ToolOutputBlock::Image(ra_core::item::ImageBlock::new(
        ImageSource::base64("image/png", "aW1hZ2U="),
    )));
    let stored = serde_json::to_value(&output).expect("tool output should serialize");

    let server = MockServer::start().await;
    let strict = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;
    let message = rejected(
        &strict,
        request(vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call_a"),
            stored.clone(),
        ))]),
    )
    .await;
    assert!(message.contains("carry text"), "unexpected: {message}");

    let lenient = MockServer::start().await;
    let model = mounted_model(
        &lenient,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await
    .with_lowering_options(ChatLoweringOptions::new().with_strict_feature_validation(false));
    let body = sent_body(
        &lenient,
        &model,
        request(vec![ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call_a"),
            stored,
        ))]),
    )
    .await;
    assert_eq!(body["messages"][0]["content"], "[tool output omitted]");
    assert_eq!(body["messages"][0]["tool_call_id"], "call_a");
}

/// The provider sequence is replay truth: an empty block and a redacted one survive it intact.
#[tokio::test]
async fn the_provider_thinking_sequence_round_trips_where_the_normalized_fields_cannot() {
    let blocks = json!([
        {"type": "thinking", "thinking": "", "signature": "sig-empty"},
        {"type": "redacted_thinking", "data": "opaque"},
        {"type": "thinking", "thinking": "visible", "signature": "sig-last"}
    ]);
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_thinking_blocks(true),
    )
    .await;

    let request = request(vec![
        ModelInputItem::Reasoning(
            Reasoning::new()
                // Normalized fields keep only what they can express; the payload keeps everything.
                .with_content(vec!["visible".to_owned()])
                .with_encrypted_content("sig-last")
                .with_provider_data(json!({"model": MODEL, "thinking_blocks": blocks})),
        ),
        ModelInputItem::Message(Message::assistant("answer", OutputPhase::Final)),
    ]);
    let body = sent_body(&server, &model, request).await;

    assert_eq!(
        body["messages"][0]["thinking_blocks"], blocks,
        "the provider sequence must be resent byte for byte"
    );
}

/// Typed content blocks make the alias problem unrepresentable, and refuse what they cannot carry.
#[tokio::test]
async fn content_blocks_lower_to_canonical_parts_without_guessing() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let body = sent_body(
        &server,
        &model,
        request(vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![
                ContentBlock::text("look at this"),
                ContentBlock::image(ImageSource::base64("image/png", "aW1hZ2U=")),
            ],
        ))]),
    )
    .await;
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({"type": "text", "text": "look at this"})
    );
    assert_eq!(
        body["messages"][0]["content"][1],
        json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aW1hZ2U="}})
    );

    // A block with no faithful Chat shape is refused at the boundary that knows it, rather than
    // being coerced into a part that means something else.
    let rejecting = MockServer::start().await;
    let model = mounted_model(
        &rejecting,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;
    let message = rejected(
        &model,
        request(vec![ModelInputItem::Message(Message::new(
            MessageRole::User,
            vec![ContentBlock::image(ImageSource::provider_file("file_123"))],
        ))]),
    )
    .await;
    assert!(message.contains("file id"), "unexpected: {message}");
}

/// Compaction stays sendable: refusing it would make a conversation unusable once compressed.
#[tokio::test]
async fn a_compaction_summary_lowers_to_an_ordinary_user_turn() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let body = sent_body(
        &server,
        &model,
        request(vec![ModelInputItem::Compaction(Compaction::new(
            "earlier: the user asked about accounts",
            Vec::new(),
        ))]),
    )
    .await;
    assert_eq!(
        body["messages"][0],
        json!({"role": "user", "content": "earlier: the user asked about accounts"})
    );
}

/// Lifting a reasoning field and then dropping it on the next turn is a silent loss.
///
/// Asserted against the outbound body rather than the lifted item, because that is where the loss
/// shows: an item can hold text that no later request ever carries.
#[rstest]
#[case::body_only(json!({"reasoning": "the body spelling"}), None, Some("the body spelling"))]
#[case::summary_only(
    json!({"reasoning_content": "the summary spelling"}),
    Some("the summary spelling"),
    None
)]
#[case::both(
    json!({"reasoning_content": "the summary spelling", "reasoning": "the body spelling"}),
    Some("the summary spelling"),
    Some("the body spelling")
)]
#[tokio::test]
async fn every_lifted_reasoning_field_survives_into_the_next_request(
    #[case] returned: Value,
    #[case] expected_summary: Option<&str>,
    #[case] expected_body: Option<&str>,
) {
    let quirks = ProviderQuirks::new().with_reasoning_content(true);
    let mut message = json!({"role": "assistant", "content": "answer"});
    for (key, value) in returned.as_object().expect("a reasoning-bearing message") {
        message[key] = value.clone();
    }

    let first = MockServer::start().await;
    let model = mounted_model_with(
        &first,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_reasoning",
            "choices": [{"index": 0, "finish_reason": "stop", "message": message}]
        })),
        quirks,
    )
    .await;
    let response = model
        .get_response(request(vec![ModelInputItem::Message(Message::user("hi"))]))
        .await
        .expect("completion should lift");

    // Feed the recorded turn straight back, exactly as a runner would.
    let second = MockServer::start().await;
    let next = mounted_model_with(
        &second,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        quirks,
    )
    .await;
    let mut history = response.to_input_items();
    history.push(ModelInputItem::Message(Message::user("and now?")));
    let body = sent_body(&second, &next, request(history)).await;

    let assistant = body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "assistant")
        })
        .expect("the recorded assistant turn should be replayed");
    assert_eq!(
        assistant.get("reasoning_content").and_then(Value::as_str),
        expected_summary,
        "replayed the wrong summary field: {assistant}"
    );
    assert_eq!(
        assistant.get("reasoning").and_then(Value::as_str),
        expected_body,
        "a gateway wants back the field it sent, and this one was dropped: {assistant}"
    );
}

/// Reasoning is replayed only to the model that produced it, whatever the policy says.
#[tokio::test]
async fn reasoning_from_another_model_is_never_replayed() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
        ProviderQuirks::new().with_reasoning_content(true),
    )
    .await
    // Even a policy that always says yes cannot override the ownership check on the item.
    .with_reasoning_replay(ReasoningReplayPolicy::new(|context| {
        context.origin_model() == Some(context.model())
    }));

    let body = sent_body(
        &server,
        &model,
        request(vec![
            ModelInputItem::Reasoning(
                Reasoning::new()
                    .with_summary(vec!["thought elsewhere".to_owned()])
                    .with_provider_data(json!({"model": "some-other-model"})),
            ),
            ModelInputItem::Message(Message::assistant("answer", OutputPhase::Final)),
        ]),
    )
    .await;
    assert!(
        body["messages"][0].get("reasoning_content").is_none(),
        "another model's reasoning was presented as this model's own: {}",
        body["messages"][0]
    );
}

// ---------------------------------------------------------------------------------------------
// Lifting a completed response
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_completion_lifts_into_reasoning_message_and_paired_calls() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200)
            .insert_header("x-request-id", "req_chat")
            .set_body_json(json!({
                "id": "chatcmpl_123",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": "on it",
                        "reasoning_content": "the account tool answers this",
                        "tool_calls": [
                            {
                                "id": "call_a",
                                "type": "function",
                                "function": {"name": "lookup", "arguments": "{\"account_id\":42}"}
                            },
                            {
                                "id": "call_h",
                                "type": "function",
                                "function": {"name": "delegate_research", "arguments": "{}"}
                            }
                        ]
                    }
                }],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 40,
                    "total_tokens": 140,
                    "prompt_tokens_details": {"cached_tokens": 60},
                    "completion_tokens_details": {"reasoning_tokens": 15}
                }
            })),
        ProviderQuirks::new().with_reasoning_content(true),
    )
    .await;

    let request = request(vec![ModelInputItem::Message(Message::user("look it up"))])
        .with_handoffs(vec![ModelHandoffDefinition::new(
            AgentId::new("research-agent"),
            "delegate_research",
            json!({"type": "object"}),
        )]);
    let response = model
        .get_response(request)
        .await
        .expect("completion should lift");

    assert_eq!(response.request_id(), Some("req_chat"));
    assert!(
        response.response_id().is_none(),
        "a chatcmpl id cannot be continued from, so advertising one would be a lie"
    );
    assert_eq!(response.usage().input_tokens(), 100);
    assert_eq!(response.usage().cached_input_tokens(), 60);
    assert_eq!(response.usage().reasoning_tokens(), 15);

    assert_eq!(response.output().len(), 4);
    let RunItemKind::Reasoning(reasoning) = response.output()[0].kind() else {
        panic!("first item should be reasoning");
    };
    assert_eq!(reasoning.summary(), ["the account tool answers this"]);
    assert!(
        reasoning.id().is_none(),
        "Chat assigns no item id, and inventing one would travel into a Responses request"
    );
    assert_eq!(
        reasoning.provider_data().and_then(|data| data.get("model")),
        Some(&json!(MODEL)),
        "the origin has to be recorded or a later turn cannot tell whose reasoning this is"
    );
    let RunItemKind::Message(message) = response.output()[1].kind() else {
        panic!("second item should be a message");
    };
    assert_eq!(message.text_content(), "on it");
    let RunItemKind::ToolCall(call) = response.output()[2].kind() else {
        panic!("third item should be a tool call");
    };
    assert_eq!(call.call_id().as_str(), "call_a");
    assert_eq!(call.arguments(), &json!({"account_id": 42}));
    assert!(
        matches!(response.output()[3].kind(), RunItemKind::HandoffCall(_)),
        "a call whose name matches an advertised handoff is a handoff"
    );
}

#[tokio::test]
async fn reasoning_content_needs_a_declaration_before_it_is_believed() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_123",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "answer",
                    "reasoning_content": "gateway-specific field"
                }
            }]
        })),
    )
    .await;

    let response = model
        .get_response(request(vec![ModelInputItem::Message(Message::user("hi"))]))
        .await
        .expect("completion should lift");
    assert_eq!(response.output().len(), 1);
    assert!(matches!(
        response.output()[0].kind(),
        RunItemKind::Message(_)
    ));
}

/// `choices` numbers its entries, and both paths have to read that number rather than the order.
#[tokio::test]
async fn the_answered_choice_is_selected_by_index_not_by_array_position() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_unordered",
            "choices": [
                {
                    "index": 1,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "the other candidate"}
                },
                {
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "the answer"}
                }
            ]
        })),
    )
    .await
    .with_lowering_options(ChatLoweringOptions::new().with_strict_feature_validation(false));

    let response = model
        .get_response(request(vec![ModelInputItem::Message(Message::user("hi"))]))
        .await
        .expect("completion should lift");
    let RunItemKind::Message(message) = response.output()[0].kind() else {
        panic!("expected a message");
    };
    assert_eq!(
        message.text_content(),
        "the answer",
        "the streaming path selects by index, and answering a different choice here would make \
         the two entry points disagree about the same response"
    );
}

/// Both reasoning spellings are read on both entry points, or the two disagree about a response.
#[tokio::test]
async fn both_reasoning_spellings_lift_on_the_non_streaming_path_too() {
    let server = MockServer::start().await;
    let model = mounted_model_with(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_reasoning",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "answer",
                    "reasoning_content": "the summary spelling",
                    "reasoning": "the body spelling"
                }
            }]
        })),
        ProviderQuirks::new().with_reasoning_content(true),
    )
    .await;

    let response = model
        .get_response(request(vec![ModelInputItem::Message(Message::user("hi"))]))
        .await
        .expect("completion should lift");
    let RunItemKind::Reasoning(reasoning) = response.output()[0].kind() else {
        panic!("expected a reasoning item");
    };
    assert_eq!(reasoning.summary(), ["the summary spelling"]);
    assert_eq!(
        reasoning.content(),
        ["the body spelling"],
        "the streaming path reads this field, so dropping it here makes the same response lift \
         differently depending on how it was requested"
    );
}

#[tokio::test]
async fn a_truncated_completion_is_classified_rather_than_served_as_an_answer() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_cut",
            "choices": [{
                "index": 0,
                "finish_reason": "length",
                "message": {"role": "assistant", "content": "half a sen"}
            }]
        })),
    )
    .await;

    let error = model
        .get_response(request(vec![ModelInputItem::Message(Message::user("hi"))]))
        .await
        .expect_err("a truncated turn is not a final answer");
    assert_eq!(error.recoverability(), Recoverability::RetryableWithChange);
}

#[tokio::test]
async fn a_withheld_turn_becomes_a_refusal_rather_than_an_empty_one() {
    let server = MockServer::start().await;
    let model = mounted_model(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_filtered",
            "choices": [{
                "index": 0,
                "finish_reason": "content_filter",
                "message": {"role": "assistant", "content": ""}
            }]
        })),
    )
    .await;

    let response = model
        .get_response(request(vec![ModelInputItem::Message(Message::user("hi"))]))
        .await
        .expect("a filtered turn still lifts");
    let RunItemKind::Message(message) = response.output()[0].kind() else {
        panic!("expected a message");
    };
    assert!(
        message.refusal_content().is_some(),
        "model fallback escalates on a mechanical refusal, not on prose"
    );
}

// ---------------------------------------------------------------------------------------------
// Streaming: the five reassembly rules
// ---------------------------------------------------------------------------------------------

/// Collects the whole stream, returning the raw events by type and the normalized items.
async fn collect_stream(
    model: &OpenAiChatModel,
    request: ModelRequest,
) -> (Vec<(String, Value, u64)>, Vec<RunItemKind>) {
    let (raw, items, _) = collect_stream_with_terminal(model, request).await;
    (raw, items)
}

/// The same collection, keeping the terminal response the stream settled into.
async fn collect_stream_with_terminal(
    model: &OpenAiChatModel,
    request: ModelRequest,
) -> (
    Vec<(String, Value, u64)>,
    Vec<RunItemKind>,
    Option<ra_core::item::ModelResponse>,
) {
    let events = model.stream_response(request).collect::<Vec<_>>().await;
    let mut raw = Vec::new();
    let mut items = Vec::new();
    let mut terminal = None;
    for event in events {
        match event.expect("stream should not fail") {
            ModelStreamEvent::RawResponse(event) => raw.push((
                event.event_type().to_owned(),
                event.payload().clone(),
                event.payload()["sequence_number"]
                    .as_u64()
                    .expect("every event carries a sequence number"),
            )),
            ModelStreamEvent::RunItem(event) => items.push(event.item().kind().clone()),
            ModelStreamEvent::Completed(response) => {
                assert!(
                    terminal.replace(*response).is_none(),
                    "a stream settles exactly once"
                );
            }
            _ => panic!("unexpected model stream event"),
        }
    }
    (raw, items, terminal)
}

/// Mounts a chat model answering with the supplied SSE frames, terminated by `[DONE]`.
async fn streaming_model(server: &MockServer, frames: &[Value]) -> OpenAiChatModel {
    let body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    mounted_stream(
        server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await
}

/// Mounts a chat model answering a streaming request with whatever the caller supplies.
async fn mounted_stream(server: &MockServer, template: ResponseTemplate) -> OpenAiChatModel {
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

/// Consumes the stream, returning the terminal error when it failed.
async fn stream_error(model: &OpenAiChatModel) -> ra_core::error::Error {
    model
        .stream_response(request(vec![ModelInputItem::Message(Message::user("go"))]))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("the stream should report the failure")
}

fn delta(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl_stream",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

/// Fragments of one call arrive split, and two calls interleave.
#[tokio::test]
async fn tool_call_fragments_are_reassembled_across_interleaved_chunks() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{\"acc"}}]}),
                None,
            ),
            delta(
                json!({"tool_calls": [{"index": 1, "id": "call_b", "function": {"name": "search", "arguments": "{\"q\":"}}]}),
                None,
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "ount\":42}"}}]}),
                None,
            ),
            delta(
                json!({"tool_calls": [{"index": 1, "function": {"arguments": "\"rust\"}"}}]}),
                Some("tool_calls"),
            ),
        ],
    )
    .await;

    let (_, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let calls = items
        .iter()
        .filter_map(|kind| match kind {
            RunItemKind::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call_id().as_str(), "call_a");
    assert_eq!(calls[0].arguments(), &json!({"account": 42}));
    assert_eq!(calls[1].call_id().as_str(), "call_b");
    assert_eq!(calls[1].arguments(), &json!({"q": "rust"}));
}

/// Chat deltas carry no output index, so one is derived and never contradicted afterwards.
#[tokio::test]
async fn output_indexes_are_derived_for_reasoning_message_and_tool_calls() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(json!({"reasoning_content": "thinking"}), None),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{}"}}]}),
                None,
            ),
            delta(json!({"content": "and here is the answer"}), Some("stop")),
        ],
    )
    .await
    .with_quirks(ProviderQuirks::new().with_reasoning_content(true));

    let (raw, _) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let indexes = raw
        .iter()
        .filter(|(event_type, _, _)| event_type == "response.output_item.done")
        .map(|(_, payload, _)| {
            (
                payload["item"]["type"].as_str().unwrap_or("").to_owned(),
                payload["output_index"].as_u64().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        indexes,
        vec![
            ("reasoning".to_owned(), 0),
            ("function_call".to_owned(), 1),
            ("message".to_owned(), 2)
        ],
        "the message must land after the call that was announced before it"
    );
}

/// A turn that says nothing must not reserve a slot for the message it never sent.
#[tokio::test]
async fn a_tool_calls_only_turn_starts_its_calls_at_the_first_slot() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[delta(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{}"}}]}),
            Some("tool_calls"),
        )],
    )
    .await;

    let (raw, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let done = raw
        .iter()
        .filter(|(event_type, _, _)| event_type == "response.output_item.done")
        .map(|(_, payload, _)| payload["output_index"].as_u64().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        done,
        vec![0],
        "a gap at index 0 leaves a consumer waiting for an item that never arrives"
    );
    assert_eq!(items.len(), 1);
}

/// A stream that ends without announcing why still owes the tools it was asked for.
#[tokio::test]
async fn buffered_calls_survive_a_stream_that_ends_without_a_finish_reason() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[delta(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{\"a\":1}"}}]}),
            None,
        )],
    )
    .await
    .with_buffered_tool_calls(true);

    let (_, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let RunItemKind::ToolCall(call) = &items[0] else {
        panic!("the held call must still be released, got {:?}", items);
    };
    assert_eq!(call.call_id().as_str(), "call_a");
    assert_eq!(call.arguments(), &json!({"a": 1}));
}

/// One counter orders every event, whichever kind it is.
#[tokio::test]
async fn sequence_numbers_increase_monotonically_across_every_event() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(json!({"reasoning_content": "thinking"}), None),
            delta(json!({"content": "first"}), None),
            delta(json!({"content": " second"}), Some("stop")),
        ],
    )
    .await
    .with_quirks(ProviderQuirks::new().with_reasoning_content(true));

    let (raw, _) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let sequences = raw
        .iter()
        .map(|(_, _, sequence)| *sequence)
        .collect::<Vec<_>>();
    assert_eq!(
        sequences,
        (0..sequences.len() as u64).collect::<Vec<_>>(),
        "a gap or a repeat here reorders a consumer's view of the turn"
    );
    assert_eq!(
        raw.last().map(|(event_type, _, _)| event_type.as_str()),
        Some("response.completed")
    );
}

/// A thinking block's text and its signature arrive in different deltas.
#[tokio::test]
async fn thinking_text_and_signature_accumulate_into_one_block() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "weigh"}]}),
                None,
            ),
            delta(
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "ing it"}]}),
                None,
            ),
            delta(
                json!({"thinking_blocks": [{"type": "thinking", "signature": "sig-one"}]}),
                None,
            ),
            delta(json!({"content": "answer"}), Some("stop")),
        ],
    )
    .await;

    let (_, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let RunItemKind::Reasoning(reasoning) = &items[0] else {
        panic!("expected a reasoning item, got {:?}", items[0]);
    };
    let blocks = reasoning
        .provider_data()
        .and_then(|data| data.get("thinking_blocks"))
        .and_then(Value::as_array)
        .expect("the provider sequence is the replay source of truth");
    assert_eq!(
        blocks,
        &vec![json!({"type": "thinking", "thinking": "weighing it", "signature": "sig-one"})],
        "a signature closes the block it belongs to rather than opening a new one"
    );
    assert_eq!(reasoning.encrypted_content(), Some("sig-one"));
}

/// Buffering must not swallow output the model already produced.
#[tokio::test]
async fn buffering_tool_calls_still_forwards_text_produced_alongside_them() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(json!({"content": "let me check"}), None),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{\"a"}}]}),
                None,
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\":1}"}}]}),
                Some("tool_calls"),
            ),
        ],
    )
    .await
    .with_buffered_tool_calls(true);

    let (raw, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let argument_deltas = raw
        .iter()
        .filter(|(event_type, _, _)| event_type == "response.function_call_arguments.delta")
        .map(|(_, payload, _)| payload["delta"].as_str().unwrap_or("").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        argument_deltas,
        vec!["{\"a\":1}".to_owned()],
        "buffering trades incremental fragments for one well-formed argument string"
    );
    let text = items.iter().find_map(|kind| match kind {
        RunItemKind::Message(message) => Some(message.text_content()),
        _ => None,
    });
    assert_eq!(
        text.as_deref(),
        Some("let me check"),
        "text produced alongside a buffered call must still reach the consumer"
    );
    let calls = items
        .iter()
        .filter(|kind| matches!(kind, RunItemKind::ToolCall(_)))
        .count();
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn a_streamed_call_reports_usage_when_the_endpoint_declares_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                delta(json!({"content": "hi"}), Some("stop")),
                json!({
                    "id": "chatcmpl_stream",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 2,
                        "total_tokens": 12,
                        "prompt_tokens_details": {"cached_tokens": 4}
                    }
                })
            ),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("mock model should build")
    .with_quirks(ProviderQuirks::new().with_stream_usage(true));

    let (raw, _) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("JSON body");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));

    let (_, completed, _) = raw
        .iter()
        .find(|(event_type, _, _)| event_type == "response.completed")
        .expect("the stream must end with a completed response");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 10);
    assert_eq!(
        completed["response"]["usage"]["input_tokens_details"]["cached_tokens"], 4,
        "without this detail a run's cache hit rate cannot be computed"
    );
}

/// A stream ends by stating the same terminal facts the non-streaming entry point returns.
///
/// Deltas alone are not a response: usage rides a frame of its own, the request identifier is a
/// response header, and neither can be recovered from the narration. A consumer that had to fold
/// them itself would be re-deriving a vocabulary this adapter does not promise to keep stable.
#[tokio::test]
async fn a_stream_settles_into_a_terminal_response_carrying_what_the_deltas_cannot() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req_streamed")
                .set_body_raw(
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        delta(json!({"content": "hi"}), Some("stop")),
                        json!({
                            "id": "chatcmpl_stream",
                            "choices": [],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
                        })
                    ),
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;
    let model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("mock model should build")
    .with_quirks(ProviderQuirks::new().with_stream_usage(true));

    let (raw, items, terminal) = collect_stream_with_terminal(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;

    let terminal = terminal.expect("a settled stream states its terminal facts");
    assert_eq!(terminal.usage().input_tokens(), 10);
    assert_eq!(terminal.usage().output_tokens(), 2);
    assert_eq!(terminal.request_id(), Some("req_streamed"));
    // A `chatcmpl-` identifier cannot be continued from, so the field stays empty here exactly as
    // it does on the non-streaming path rather than advertising a conversation that does not exist.
    assert_eq!(terminal.response_id(), None);
    assert_eq!(
        terminal
            .output()
            .iter()
            .map(|item| item.kind().clone())
            .collect::<Vec<_>>(),
        items,
        "the terminal response lists what the normalized channel already published"
    );
    assert_eq!(
        raw.last().map(|(event_type, _, _)| event_type.as_str()),
        Some("response.completed"),
        "the raw narration still ends where it did; the terminal facts follow it"
    );
}

/// The terminal response lists items in output order, not in the order they happened to close.
///
/// A message that opened between two tool calls is closed after both of them, so publication order
/// and output order genuinely differ here. A consumer replaying this turn reads the output order.
#[tokio::test]
async fn the_terminal_response_orders_items_by_output_slot_not_by_completion() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{}"}}]}),
                None,
            ),
            delta(json!({"content": "working on it"}), None),
            delta(
                json!({"tool_calls": [{"index": 1, "id": "call_b", "function": {"name": "lookup", "arguments": "{}"}}]}),
                Some("tool_calls"),
            ),
        ],
    )
    .await;

    let (raw, items, terminal) = collect_stream_with_terminal(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;
    let terminal = terminal.expect("a settled stream states its terminal facts");

    let slots = raw
        .iter()
        .filter(|(event_type, _, _)| event_type == "response.output_item.done")
        .map(|(_, payload, _)| {
            (
                payload["item"]["type"].as_str().unwrap_or("").to_owned(),
                payload["output_index"].as_u64().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        slots,
        vec![
            ("function_call".to_owned(), 0),
            ("function_call".to_owned(), 2),
            ("message".to_owned(), 1)
        ]
    );

    let labels = |kinds: &[RunItemKind]| {
        kinds
            .iter()
            .map(|kind| kind.label().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(&items),
        vec!["tool_call", "tool_call", "message"],
        "publication order follows when each item closed"
    );
    assert_eq!(
        labels(
            &terminal
                .output()
                .iter()
                .map(|item| item.kind().clone())
                .collect::<Vec<_>>()
        ),
        vec!["tool_call", "message", "tool_call"],
        "the terminal response follows the output slots the consumer was told about"
    );
}

/// Running out of bytes is not the sender saying it finished.
#[tokio::test]
async fn a_stream_cut_short_fails_instead_of_reporting_a_complete_turn() {
    let server = MockServer::start().await;
    // Text deltas, then the connection simply ends: no `[DONE]`, no finish reason.
    let body = format!(
        "data: {}\n\n",
        delta(json!({"content": "half an ans"}), None)
    );
    let model = mounted_stream(
        &server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;

    let error = stream_error(&model).await;
    assert_eq!(error.recoverability(), Recoverability::Retryable);
    assert!(
        error.to_string().contains("partial output"),
        "the message has to say output was already emitted, because that decides whether the \
         request may be replayed at all: {error}"
    );
}

/// A gateway that ignores `stream=true` must not read as a turn in which the model said nothing.
#[tokio::test]
async fn a_streaming_request_answered_with_json_is_refused() {
    let server = MockServer::start().await;
    let model = mounted_stream(
        &server,
        ResponseTemplate::new(200).set_body_json(empty_completion()),
    )
    .await;

    let error = stream_error(&model).await;
    assert!(
        error.to_string().contains("does not honour `stream=true`"),
        "unexpected message: {error}"
    );
    assert!(
        !error.to_string().contains("ok"),
        "the completion body is the model's output and must not be quoted into an error: {error}"
    );
}

/// A turn cut off at the token limit is not an answer, whichever entry point delivered it.
#[tokio::test]
async fn a_streamed_turn_stopped_at_the_token_limit_is_classified_like_a_completed_one() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[delta(json!({"content": "half a sen"}), Some("length"))],
    )
    .await;

    let error = stream_error(&model).await;
    assert_eq!(
        error.recoverability(),
        Recoverability::RetryableWithChange,
        "the non-streaming path classifies this exact state, and the two must agree: {error}"
    );
    assert!(
        error.to_string().contains("output token limit"),
        "unexpected message: {error}"
    );
}

/// A gateway-private reasoning field needs the same declaration on both entry points.
#[tokio::test]
async fn streamed_reasoning_content_needs_a_declaration_before_it_is_believed() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(json!({"reasoning_content": "gateway-specific field"}), None),
            delta(json!({"reasoning": "another gateway spelling"}), None),
            delta(json!({"content": "answer"}), Some("stop")),
        ],
    )
    .await;

    let (_, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;
    assert_eq!(
        items.len(),
        1,
        "an undeclared endpoint's private fields were believed: {items:?}"
    );
    assert!(matches!(items[0], RunItemKind::Message(_)));
}

/// Buffering must not delete a call it does not buffer.
#[tokio::test]
async fn a_custom_tool_call_survives_the_release_of_buffered_function_calls() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{}"}}]}),
                None,
            ),
            // The terminal frame carries a custom call beside the buffered function call.
            delta(
                json!({"tool_calls": [
                    {"index": 1, "id": "call_c", "type": "custom", "custom": {"name": "run", "input": "ls"}}
                ]}),
                Some("tool_calls"),
            ),
        ],
    )
    .await
    .with_buffered_tool_calls(true);

    // Strict validation is the default, so the shape this adapter cannot execute is reported
    // rather than quietly dropped on the way past.
    let error = stream_error(&model).await;
    assert!(
        error.to_string().contains("custom"),
        "the custom call was overwritten by the released buffer instead of being reported: {error}"
    );
}

/// Reporting it is the strict behaviour; dropping it is the lenient one, and both must see it.
#[tokio::test]
async fn a_lenient_endpoint_keeps_the_function_call_beside_a_dropped_custom_call() {
    let server = MockServer::start().await;
    let model = streaming_model(
        &server,
        &[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "lookup", "arguments": "{\"a\":1}"}}]}),
                None,
            ),
            delta(
                json!({"tool_calls": [
                    {"index": 1, "id": "call_c", "type": "custom", "custom": {"name": "run", "input": "ls"}}
                ]}),
                Some("tool_calls"),
            ),
        ],
    )
    .await
    .with_buffered_tool_calls(true)
    .with_lowering_options(ChatLoweringOptions::new().with_strict_feature_validation(false));

    let (_, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;
    let calls = items
        .iter()
        .filter_map(|kind| match kind {
            RunItemKind::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "the buffered call must still be released");
    assert_eq!(calls[0].call_id().as_str(), "call_a");
    assert_eq!(calls[0].arguments(), &json!({"a": 1}));
}

/// Gateways omit one terminator or the other, so either alone has to be enough.
#[tokio::test]
async fn a_finish_reason_alone_settles_a_stream_that_never_sends_done() {
    let server = MockServer::start().await;
    let body = format!(
        "data: {}\n\n",
        delta(json!({"content": "hi"}), Some("stop"))
    );
    let model = mounted_stream(
        &server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;

    let (raw, items) = collect_stream(
        &model,
        request(vec![ModelInputItem::Message(Message::user("go"))]),
    )
    .await;
    assert_eq!(
        raw.last().map(|(event_type, _, _)| event_type.as_str()),
        Some("response.completed")
    );
    assert_eq!(items.len(), 1);
}

/// An empty body is the same failure as a cut one, and says so differently.
#[tokio::test]
async fn a_stream_that_sends_nothing_at_all_fails() {
    let server = MockServer::start().await;
    let model = mounted_stream(
        &server,
        ResponseTemplate::new(200).set_body_raw(String::new(), "text/event-stream"),
    )
    .await;

    let error = stream_error(&model).await;
    assert!(
        error.to_string().contains("without sending an event"),
        "unexpected message: {error}"
    );
}

#[tokio::test]
async fn a_failed_streaming_request_surfaces_as_one_error_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "slow down", "code": "rate_limit_exceeded"}
        })))
        .mount(&server)
        .await;
    let model = OpenAiChatModel::new(
        MODEL,
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("mock model should build");

    let events = model
        .stream_response(request(vec![ModelInputItem::Message(Message::user("go"))]))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(events.len(), 1);
    let error = events
        .into_iter()
        .next()
        .and_then(Result::err)
        .expect("the stream should carry the failure");
    assert_eq!(error.recoverability(), Recoverability::Retryable);
}

/// The two protocols are separate implementations, so the shared provider stays one `Arc`.
#[tokio::test]
async fn the_provider_caches_one_instance_per_model_name() {
    let auth = OpenAiAuth::new("test-secret");
    let provider = ra_model::openai::chat::OpenAiChatProvider::new(auth, "chat-default")
        .expect("provider should build");
    let first = ra_core::model::ModelProvider::get_model(&provider, None)
        .expect("default model should resolve");
    let second = ra_core::model::ModelProvider::get_model(&provider, Some("chat-default"))
        .expect("named model should resolve");
    assert!(Arc::ptr_eq(&first, &second));
}
