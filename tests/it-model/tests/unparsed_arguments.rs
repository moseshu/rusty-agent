//! Function-call argument text that is not JSON, through the OpenAI and Anthropic adapters. In an
//! OpenAI response it does not fail the response: the call keeps the text, with `null` arguments,
//! and is answered as a failure when it runs, as the reference parses arguments only when it
//! invokes the tool. Streamed Anthropic tool input whose fragments join into text that is not JSON
//! is kept the same way. Replay hands the text back to OpenAI verbatim; Anthropic, whose
//! `tool_use.input` must be an object, gets `{}` beside the paired result's error flag.

use std::sync::Arc;

use futures::StreamExt;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::Result,
    item::{
        CallId, HandoffCall, HandoffOutput, Message, ModelInputItem, ModelResponse, RunItem,
        RunItemKind, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelHandoffDefinition, ModelRequest, ModelResolver, ModelSelector,
        ModelSettings, ModelStreamEvent, ProviderKey, ResolvedModel, SettingsLayers,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_model::{
    anthropic::{AnthropicAuth, AnthropicMessagesModel},
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const TEXT: &str = "{bad";

fn request(input: Vec<ModelInputItem>) -> ModelRequest {
    ModelRequest::new(
        input,
        SettingsLayers::new()
            .with_run(&ModelSettings::new().with_max_tokens(256))
            .resolve(&ProviderKey::new("openai")),
    )
}

fn asks() -> ModelRequest {
    request(vec![ModelInputItem::Message(Message::user("call it"))])
}

fn with_handoff(request: ModelRequest) -> ModelRequest {
    request.with_handoffs(vec![ModelHandoffDefinition::new(
        AgentId::new("reviewer"),
        "transfer_to_reviewer",
        json!({"type": "object"}),
    )])
}

async fn responses(server: &MockServer, template: ResponseTemplate) -> OpenAiResponsesModel {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(template)
        .mount(server)
        .await;
    OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap()
}

async fn chat(server: &MockServer, template: ResponseTemplate) -> OpenAiChatModel {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template)
        .mount(server)
        .await;
    OpenAiChatModel::new(
        "gpt-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap()
}

async fn anthropic(server: &MockServer) -> AnthropicMessagesModel {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(server)
        .await;
    AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap()
}

fn sse(frames: &[Value], done: bool) -> ResponseTemplate {
    let mut body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn responses_call(name: &str, arguments: &str) -> Value {
    json!({"id": "fc", "type": "function_call", "call_id": "call-1", "name": name, "arguments": arguments})
}

fn responses_body(name: &str, arguments: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"id": "resp", "output": [responses_call(name, arguments)]}))
}

fn responses_frames(name: &str, arguments: &str) -> ResponseTemplate {
    let call = responses_call(name, arguments);
    let mut added = call.clone();
    added["arguments"] = json!("");
    sse(
        &[
            json!({"type":"response.created","response":{"id":"resp"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":added}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc","delta":arguments}),
            json!({"type":"response.output_item.done","output_index":0,"item":call}),
            json!({"type":"response.completed","response":{"id":"resp","status":"completed","output":[call]}}),
        ],
        false,
    )
}

fn chat_body(name: &str, arguments: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chat", "object": "chat.completion", "created": 0, "model": "gpt-test",
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": name, "arguments": arguments}}]
            }
        }]
    }))
}

fn chat_frames(name: &str, arguments: &str) -> ResponseTemplate {
    let chunk = |delta: Value, finish: Option<&str>| {
        json!({
            "id": "chatcmpl", "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    };
    sse(
        &[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "call-1", "function": {"name": name, "arguments": arguments}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ],
        true,
    )
}

/// The items a stream emitted and the response it settled into.
async fn streamed(model: &dyn Model, request: ModelRequest) -> (Vec<RunItemKind>, ModelResponse) {
    let mut items = Vec::new();
    let mut terminal = None;
    for event in model.stream_response(request).collect::<Vec<_>>().await {
        match event.unwrap() {
            ModelStreamEvent::RunItem(event) => items.push(event.item().kind().clone()),
            ModelStreamEvent::Completed(response) => terminal = Some(*response),
            _ => {}
        }
    }
    (items, terminal.expect("the stream settles"))
}

fn the_call(kind: &RunItemKind) -> &ToolCall {
    match kind {
        RunItemKind::ToolCall(call) => call,
        other => panic!("expected a tool call, got {other:?}"),
    }
}

fn assert_unparsed_call(kind: &RunItemKind, label: &str) {
    let call = the_call(kind);
    assert_eq!(call.name(), "echo", "{label}");
    assert_eq!(call.arguments(), &Value::Null, "{label}");
    assert_eq!(call.unparsed_arguments(), Some(TEXT), "{label}");
}

fn assert_unparsed_handoff(kind: &RunItemKind, label: &str) {
    let RunItemKind::HandoffCall(call) = kind else {
        panic!("{label}: expected a handoff call, got {kind:?}");
    };
    assert_eq!(call.target_agent(), &AgentId::new("reviewer"), "{label}");
    assert_eq!(call.tool_name(), Some("transfer_to_reviewer"), "{label}");
    assert_eq!(call.arguments(), &Value::Null, "{label}");
    assert_eq!(call.unparsed_arguments(), Some(TEXT), "{label}");
}

/// Item 1: both OpenAI adapters, buffered and streamed, keep argument text that is not JSON as
/// the call's own record of it instead of failing the response.
#[tokio::test]
async fn openai_adapters_keep_argument_text_that_is_not_json() {
    let server = MockServer::start().await;
    let model = responses(&server, responses_body("echo", TEXT)).await;
    let response = model.get_response(asks()).await.unwrap();
    assert_unparsed_call(response.output()[0].kind(), "responses");

    let server = MockServer::start().await;
    let model = responses(&server, responses_frames("echo", TEXT)).await;
    let (items, response) = streamed(&model, asks()).await;
    assert_unparsed_call(&items[0], "responses stream item");
    assert_unparsed_call(response.output()[0].kind(), "responses stream response");

    let server = MockServer::start().await;
    let model = chat(&server, chat_body("echo", TEXT)).await;
    let response = model.get_response(asks()).await.unwrap();
    assert_unparsed_call(response.output()[0].kind(), "chat");

    let server = MockServer::start().await;
    let model = chat(&server, chat_frames("echo", TEXT)).await;
    let (items, response) = streamed(&model, asks()).await;
    let calls: Vec<_> = items
        .iter()
        .filter(|kind| matches!(kind, RunItemKind::ToolCall(_)))
        .collect();
    assert_eq!(calls.len(), 1);
    assert_unparsed_call(calls[0], "chat stream item");
    assert_unparsed_call(response.output()[0].kind(), "chat stream response");
}

/// Item 1: text that is JSON, blank text and `null` read as before, with no unparsed text.
#[tokio::test]
async fn parsable_and_blank_arguments_carry_no_unparsed_text() {
    for (text, expected) in [
        ("", json!({})),
        ("  ", json!({})),
        ("null", Value::Null),
        ("{}", json!({})),
        ("\"{bad\"", json!("{bad")),
    ] {
        let server = MockServer::start().await;
        let model = responses(&server, responses_body("echo", text)).await;
        let response = model.get_response(asks()).await.unwrap();
        let call = the_call(response.output()[0].kind());
        assert_eq!(call.arguments(), &expected, "responses {text:?}");
        assert_eq!(call.unparsed_arguments(), None, "responses {text:?}");

        let server = MockServer::start().await;
        let model = chat(&server, chat_body("echo", text)).await;
        let response = model.get_response(asks()).await.unwrap();
        let call = the_call(response.output()[0].kind());
        assert_eq!(call.arguments(), &expected, "chat {text:?}");
        assert_eq!(call.unparsed_arguments(), None, "chat {text:?}");
    }
}

/// Item 2: a handoff's argument text that is not JSON becomes a handoff call that keeps it.
#[tokio::test]
async fn handoff_argument_text_that_is_not_json_is_kept_on_the_handoff_call() {
    let server = MockServer::start().await;
    let model = responses(&server, responses_body("transfer_to_reviewer", TEXT)).await;
    let response = model.get_response(with_handoff(asks())).await.unwrap();
    assert_unparsed_handoff(response.output()[0].kind(), "responses");

    let server = MockServer::start().await;
    let model = chat(&server, chat_body("transfer_to_reviewer", TEXT)).await;
    let response = model.get_response(with_handoff(asks())).await.unwrap();
    assert_unparsed_handoff(response.output()[0].kind(), "chat");

    let server = MockServer::start().await;
    let model = chat(&server, chat_frames("transfer_to_reviewer", TEXT)).await;
    let (_, response) = streamed(&model, with_handoff(asks())).await;
    assert_unparsed_handoff(response.output()[0].kind(), "chat stream");
}

/// A history in which call-1 had argument text that was not JSON and was answered as a failure.
fn replayed_history(handoff: bool) -> Vec<ModelInputItem> {
    let call = if handoff {
        ModelInputItem::HandoffCall(
            HandoffCall::new(CallId::new("call-1"), AgentId::new("reviewer"), Value::Null)
                .with_tool_name("transfer_to_reviewer")
                .with_unparsed_arguments(TEXT),
        )
    } else {
        ModelInputItem::ToolCall(ToolCall::unparsed(CallId::new("call-1"), "echo", TEXT))
    };
    // A handoff that went ahead is answered by its transfer; a tool call by its failure.
    let output = if handoff {
        ModelInputItem::HandoffOutput(HandoffOutput::new(
            CallId::new("call-1"),
            AgentId::new("worker"),
            AgentId::new("reviewer"),
        ))
    } else {
        ModelInputItem::ToolCallOutput(
            ToolCallOutput::new(CallId::new("call-1"), json!("arguments are not valid JSON"))
                .with_error(true),
        )
    };
    vec![
        ModelInputItem::Message(Message::user("call it")),
        call,
        output,
    ]
}

async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests.last().unwrap().body).unwrap()
}

/// Item 3: the OpenAI adapters hand the text back exactly as the model wrote it, for a tool call
/// and for a handoff call.
#[tokio::test]
async fn openai_replay_hands_back_the_text_verbatim() {
    for handoff in [false, true] {
        let server = MockServer::start().await;
        let model = responses(
            &server,
            ResponseTemplate::new(200).set_body_json(json!({"id": "resp", "output": []})),
        )
        .await;
        model
            .get_response(with_handoff(request(replayed_history(handoff))))
            .await
            .unwrap();
        let body = sent_body(&server).await;
        let call = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(
            call["arguments"],
            json!(TEXT),
            "responses handoff={handoff}"
        );

        let server = MockServer::start().await;
        let model = chat(&server, chat_body("echo", "{}")).await;
        model
            .get_response(with_handoff(request(replayed_history(handoff))))
            .await
            .unwrap();
        let body = sent_body(&server).await;
        let call = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|message| message.get("tool_calls"))
            .unwrap();
        assert_eq!(
            call[0]["function"]["arguments"],
            json!(TEXT),
            "chat handoff={handoff}"
        );
    }
}

/// Item 3: Anthropic's `tool_use.input` must be an object, so the call is replayed as `{}`; a
/// tool call's paired result keeps its error flag.
#[tokio::test]
async fn anthropic_replay_sends_an_empty_input_beside_the_error_result() {
    for handoff in [false, true] {
        let server = MockServer::start().await;
        let model = anthropic(&server).await;
        model
            .get_response(with_handoff(request(replayed_history(handoff))))
            .await
            .unwrap();
        let body = sent_body(&server).await;
        let blocks: Vec<&Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .collect();
        let tool_use = blocks
            .iter()
            .find(|block| block["type"] == "tool_use")
            .unwrap();
        assert_eq!(tool_use["input"], json!({}), "handoff={handoff}");
        let result = blocks
            .iter()
            .find(|block| block["type"] == "tool_result")
            .unwrap();
        assert_eq!(result["tool_use_id"], "call-1", "handoff={handoff}");
        if !handoff {
            assert_eq!(result["is_error"], json!(true));
        }
    }
}

struct Echo {
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait::async_trait]
impl Tool for Echo {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        panic!("a call whose arguments were not JSON never reaches the tool")
    }
}

struct Resolver {
    model: Arc<dyn Model>,
    protocol: ApiProtocol,
}

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test-model".into()),
                self.protocol,
            ),
            self.model.clone(),
            ModelSettings::new().with_max_tokens(256),
            ModelSettings::new(),
        ))
    }
}

/// Item 4: through the runner, the response with argument text that is not JSON does not fail the
/// run; the next request carries the call, with its text (Anthropic: `{}`), and its error output,
/// and the run ends.
#[tokio::test]
async fn the_runner_answers_the_call_and_continues() {
    for protocol in [
        ApiProtocol::OpenAiResponses,
        ApiProtocol::OpenAiChatCompletions,
        ApiProtocol::AnthropicMessages,
    ] {
        let server = MockServer::start().await;
        // The runner streams, so both turns answer with event streams.
        let (first, second, endpoint) = match protocol {
            ApiProtocol::OpenAiResponses => {
                let message = json!({"id": "msg", "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]});
                (
                    responses_frames("echo", TEXT),
                    sse(
                        &[
                            json!({"type":"response.created","response":{"id":"resp-2"}}),
                            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","content":[]}}),
                            json!({"type":"response.output_text.delta","output_index":0,"delta":"done"}),
                            json!({"type":"response.output_item.done","output_index":0,"item":message}),
                            json!({"type":"response.completed","response":{"id":"resp-2","status":"completed","output":[message]}}),
                        ],
                        false,
                    ),
                    "/v1/responses",
                )
            }
            ApiProtocol::AnthropicMessages => (
                anthropic_tool_frames("echo", &["{b", "ad"]),
                sse(
                    &[
                        json!({"type":"message_start","message":{"id":"m-2","type":"message","role":"assistant","content":[],"model":"claude-test","usage":{"input_tokens":1,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                        json!({"type":"message_stop"}),
                    ],
                    false,
                ),
                "/v1/messages",
            ),
            _ => (
                chat_frames("echo", TEXT),
                sse(
                    &[
                        json!({"id": "chatcmpl-2", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": null}]}),
                        json!({"id": "chatcmpl-2", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                    ],
                    true,
                ),
                "/v1/chat/completions",
            ),
        };
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(first)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(second)
            .mount(&server)
            .await;
        let auth = || OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri()));
        let model: Arc<dyn Model> = match protocol {
            ApiProtocol::OpenAiResponses => {
                Arc::new(OpenAiResponsesModel::new("gpt-test", auth()).unwrap())
            }
            ApiProtocol::AnthropicMessages => Arc::new(
                AnthropicMessagesModel::new(
                    "claude-test",
                    AnthropicAuth::new("test-secret")
                        .with_base_url(format!("{}/v1/", server.uri())),
                )
                .unwrap(),
            ),
            _ => Arc::new(OpenAiChatModel::new("gpt-test", auth()).unwrap()),
        };
        let agent = AgentSpec::builder()
            .id(AgentId::new("worker"))
            .name("Worker")
            .tool(Arc::new(Echo {
                origin: ToolOrigin::new("echo").unwrap(),
                schema: ToolSchema::new(
                    "echo",
                    json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
                )
                .unwrap(),
            }))
            .build()
            .unwrap();
        let result = Runner::run(RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver { model, protocol }),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("call it"))],
        ))
        .await
        .unwrap();
        assert_eq!(result.final_text(), "done", "{protocol:?}");
        assert!(result.new_items().iter().any(|item: &RunItem| matches!(
            item.kind(),
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "call-1" && output.is_error()
        )));

        let body = sent_body(&server).await;
        let text = body.to_string();
        match protocol {
            ApiProtocol::OpenAiResponses => {
                let input = body["input"].as_array().unwrap();
                let call = input
                    .iter()
                    .find(|item| item["type"] == "function_call")
                    .unwrap();
                assert_eq!(call["arguments"], json!(TEXT));
                assert!(
                    input
                        .iter()
                        .any(|item| item["type"] == "function_call_output"
                            && item["call_id"] == "call-1"),
                    "{text}"
                );
            }
            ApiProtocol::AnthropicMessages => {
                let blocks: Vec<&Value> = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|message| message["content"].as_array())
                    .flatten()
                    .collect();
                let tool_use = blocks
                    .iter()
                    .find(|block| block["type"] == "tool_use")
                    .unwrap();
                assert_eq!(tool_use["input"], json!({}), "{text}");
                let result = blocks
                    .iter()
                    .find(|block| block["type"] == "tool_result")
                    .unwrap();
                assert_eq!(result["tool_use_id"], "call-1", "{text}");
                assert_eq!(result["is_error"], json!(true), "{text}");
            }
            _ => {
                let messages = body["messages"].as_array().unwrap();
                let call = messages
                    .iter()
                    .find_map(|message| message.get("tool_calls"))
                    .unwrap();
                assert_eq!(call[0]["function"]["arguments"], json!(TEXT));
                assert!(
                    messages
                        .iter()
                        .any(|message| message["role"] == "tool"
                            && message["tool_call_id"] == "call-1"),
                    "{text}"
                );
            }
        }
        assert!(text.contains("tool.invalid_input"), "{text}");
    }
}

/// Item 2, streamed through Responses: the handoff call keeps the text.
#[tokio::test]
async fn a_streamed_responses_handoff_keeps_its_text() {
    let server = MockServer::start().await;
    let model = responses(&server, responses_frames("transfer_to_reviewer", TEXT)).await;
    let (items, response) = streamed(&model, with_handoff(asks())).await;
    assert_unparsed_handoff(&items[0], "responses stream item");
    assert_unparsed_handoff(response.output()[0].kind(), "responses stream response");
}

/// Item 1, as real streams arrive: Chat argument fragments that are not JSON only once joined
/// are kept as the joined text.
#[tokio::test]
async fn chat_fragments_that_join_into_text_that_is_not_json_are_kept() {
    let chunk = |delta: Value, finish: Option<&str>| {
        json!({
            "id": "chatcmpl", "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    };
    let server = MockServer::start().await;
    let model = chat(
        &server,
        sse(
            &[
                chunk(
                    json!({"tool_calls": [{"index": 0, "id": "call-1", "function": {"name": "echo", "arguments": "{\"path\": "}}]}),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"a.txt\""}}]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ],
            true,
        ),
    )
    .await;
    let (_, response) = streamed(&model, asks()).await;
    let call = the_call(response.output()[0].kind());
    assert_eq!(call.arguments(), &Value::Null);
    assert_eq!(call.unparsed_arguments(), Some("{\"path\": \"a.txt\""));
}

/// Anthropic stream frames for one `tool_use` block named `name` whose input arrives as the
/// `input_json_delta` fragments `fragments`.
fn anthropic_tool_frames(name: &str, fragments: &[&str]) -> ResponseTemplate {
    sse(
        &anthropic_tool_events(0, name, fragments, "tool_use"),
        false,
    )
}

fn anthropic_tool_events(
    index: u64,
    name: &str,
    fragments: &[&str],
    stop_reason: &str,
) -> Vec<Value> {
    let mut frames = vec![
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-test","usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":"call-1","name":name,"input":{}}}),
    ];
    frames.extend(fragments.iter().map(|fragment| {
        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":fragment}})
    }));
    frames.extend([
        json!({"type":"content_block_stop","index":index}),
        json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]);
    frames
}

async fn anthropic_streaming(
    server: &MockServer,
    template: ResponseTemplate,
) -> AnthropicMessagesModel {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(template)
        .mount(server)
        .await;
    AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap()
}

/// Fine-grained tool streaming keeps malformed input on the call while preserving raw payloads.
/// The tool block's stream index (3) differs from its position in the content (1), so the text is
/// tied to the assembled block, not to the SSE index.
#[tokio::test]
async fn anthropic_stream_fragments_that_join_into_text_that_is_not_json_are_kept() {
    let server = MockServer::start().await;
    let mut frames = anthropic_tool_events(3, "echo", &["{\"path\": ", "\"a.txt\""], "tool_use");
    let narration = json!({"type":"text","text":"Calling echo"});
    frames.splice(
        1..1,
        [
            json!({"type":"content_block_start","index":0,"content_block":narration}),
            json!({"type":"content_block_stop","index":0}),
        ],
    );
    let tool_block = frames[3]["content_block"].clone();
    let model = anthropic_streaming(&server, sse(&frames, false)).await;
    let (items, response) = streamed(&model, asks()).await;
    for (kind, label) in [
        (&items[1], "stream item"),
        (response.output()[1].kind(), "response"),
    ] {
        let call = the_call(kind);
        assert_eq!(call.name(), "echo", "{label}");
        assert_eq!(call.arguments(), &Value::Null, "{label}");
        assert_eq!(
            call.unparsed_arguments(),
            Some("{\"path\": \"a.txt\""),
            "{label}"
        );
    }
    let raw = |index: usize| {
        response.output()[index]
            .raw_provider_item()
            .unwrap()
            .payload()
    };
    assert_eq!(raw(0)["content"], json!([narration, tool_block]));
    assert_eq!(raw(1), &tool_block);
}

/// A buffered response has no stream state: a provider field that happens to share a decoder
/// name is kept, and the valid input stays the call's arguments.
#[tokio::test]
async fn anthropic_nonstream_tool_input_preserves_provider_fields() {
    let server = MockServer::start().await;
    let block = json!({
        "type":"tool_use", "id":"call-1", "name":"echo", "input":{"path":"a.txt"},
        "__unparsed_input":"provider metadata", "__input_json":"provider metadata"
    });
    let model = anthropic_streaming(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"m", "type":"message", "role":"assistant", "content":[block], "stop_reason":"tool_use"
        })),
    )
    .await;
    let response = model.get_response(asks()).await.unwrap();
    let item = &response.output()[0];
    let call = the_call(item.kind());
    assert_eq!(call.arguments(), &block["input"]);
    assert_eq!(call.unparsed_arguments(), None);
    assert_eq!(item.raw_provider_item().unwrap().payload(), &block);
}

/// Tool input cut short by `max_tokens` is not kept as a call: the response fails with
/// `provider.context_overflow`, as every Anthropic response stopped at `max_tokens` does.
#[tokio::test]
async fn anthropic_stream_with_truncated_tool_input_reports_max_tokens() {
    let server = MockServer::start().await;
    let frames = anthropic_tool_events(0, "echo", &["{b", "ad"], "max_tokens");
    let model = anthropic_streaming(&server, sse(&frames, false)).await;
    let events = model.stream_response(asks()).collect::<Vec<_>>().await;
    assert!(!events.iter().any(|event| matches!(
        event,
        Ok(ModelStreamEvent::RunItem(_) | ModelStreamEvent::Completed(_))
    )));
    assert_eq!(
        events.last().unwrap().as_ref().unwrap_err().code(),
        "provider.context_overflow"
    );
}

/// A handoff whose streamed input is not JSON keeps the text on the handoff call.
#[tokio::test]
async fn an_anthropic_streamed_handoff_keeps_its_text() {
    let server = MockServer::start().await;
    let model = anthropic_streaming(
        &server,
        anthropic_tool_frames("transfer_to_reviewer", &["{b", "ad"]),
    )
    .await;
    let (items, response) = streamed(&model, with_handoff(asks())).await;
    assert_unparsed_handoff(&items[0], "anthropic stream item");
    assert_unparsed_handoff(response.output()[0].kind(), "anthropic stream response");
}

/// Input that parses is the call's arguments, and blank input keeps the initial `{}`.
#[tokio::test]
async fn anthropic_stream_input_that_parses_or_is_blank_carries_no_unparsed_text() {
    for (fragments, expected) in [
        (vec!["{\"path\": ", "\"a.txt\"}"], json!({"path": "a.txt"})),
        (vec![""], json!({})),
        (vec![" ", "\n"], json!({})),
        (vec![], json!({})),
    ] {
        let server = MockServer::start().await;
        let model = anthropic_streaming(&server, anthropic_tool_frames("echo", &fragments)).await;
        let (_, response) = streamed(&model, asks()).await;
        let call = the_call(response.output()[0].kind());
        assert_eq!(call.arguments(), &expected, "{fragments:?}");
        assert_eq!(call.unparsed_arguments(), None, "{fragments:?}");
    }
}
