//! Custom tools across the three adapters.
//!
//! OpenAI Responses carries them natively: a `custom` tool entry, `custom_tool_call` items lifted
//! into custom calls, and custom calls and their outputs replayed as `custom_tool_call` and
//! `custom_tool_call_output`. Chat Completions and Anthropic Messages refuse them, as the reference's
//! Chat converter does, unless the caller opts into advertising them as a function taking one string
//! `input` — this framework's extension — in which case a call to that function comes back as a
//! custom call.

use ra_core::{
    item::{CallId, Message, ModelInputItem, RunItemKind, ToolCall, ToolCallKind, ToolCallOutput},
    model::{
        CustomToolFormat, CustomToolGrammarSyntax, Model, ModelRequest, ModelSettings,
        ModelToolDefinition, ProviderKey, ToolChoice,
    },
};
use ra_model::anthropic::{AnthropicAuth, AnthropicMessagesModel};
use ra_model::openai::{
    auth::OpenAiAuth,
    chat::{ChatLoweringOptions, OpenAiChatModel},
    responses::OpenAiResponsesModel,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const PATCH: &str = "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch\n";

fn settings(provider: &str, settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new(provider),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn grammar_tool() -> ModelToolDefinition {
    ModelToolDefinition::custom(
        "apply_patch",
        Some(CustomToolFormat::Grammar {
            syntax: CustomToolGrammarSyntax::Lark,
            definition: "start: \"x\"".to_owned(),
        }),
    )
    .with_description("Edits files.")
}

fn function_tool() -> ModelToolDefinition {
    ModelToolDefinition::new("lookup", json!({"type": "object"}))
}

/// A custom call and its output, as a run's history holds them.
fn custom_history() -> Vec<ModelInputItem> {
    vec![
        ModelInputItem::Message(Message::user("patch it")),
        ModelInputItem::ToolCall(ToolCall::custom(
            CallId::new("call_patch"),
            "apply_patch",
            PATCH,
        )),
        ModelInputItem::ToolCallOutput(
            ToolCallOutput::new(CallId::new("call_patch"), json!("Created a.txt"))
                .with_kind(ToolCallKind::Custom),
        ),
    ]
}

async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.expect("requests retained");
    serde_json::from_slice(&requests[0].body).expect("JSON body")
}

fn only_call(output: &[ra_core::item::RunItem]) -> &ToolCall {
    let calls: Vec<&ToolCall> = output
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "{output:?}");
    calls[0]
}

// ---- OpenAI Responses ----------------------------------------------------------------------

async fn responses_model(server: &MockServer, body: Value) -> OpenAiResponsesModel {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
    OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model")
}

/// The reference's `CustomToolParam`: name, description, and the format when there is one.
#[tokio::test]
async fn responses_advertises_custom_tools_in_their_own_form() {
    let server = MockServer::start().await;
    let model = responses_model(&server, json!({"id": "resp_1", "output": []})).await;

    model
        .get_response(
            ModelRequest::new(
                vec![],
                settings(
                    "openai",
                    ModelSettings::new().with_tool_choice(ToolChoice::Tool("apply_patch".into())),
                ),
            )
            .with_tools(vec![
                grammar_tool(),
                ModelToolDefinition::custom("free", Some(CustomToolFormat::Text)),
                ModelToolDefinition::custom("bare", None),
                function_tool(),
            ]),
        )
        .await
        .expect("request");

    let body = sent_body(&server).await;
    assert_eq!(
        body["tools"],
        json!([
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Edits files.",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: \"x\""},
            },
            {"type": "custom", "name": "free", "description": "", "format": {"type": "text"}},
            {"type": "custom", "name": "bare", "description": ""},
            {
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object"},
                "strict": false,
            },
        ])
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "custom", "name": "apply_patch"})
    );
}

#[tokio::test]
async fn responses_lifts_a_custom_call_and_replays_it_with_its_output() {
    let server = MockServer::start().await;
    let model = responses_model(
        &server,
        json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{
                "id": "ctc_1",
                "type": "custom_tool_call",
                "call_id": "call_patch",
                "name": "apply_patch",
                "input": PATCH,
            }],
        }),
    )
    .await;

    let response = model
        .get_response(
            ModelRequest::new(custom_history(), settings("openai", ModelSettings::new()))
                .with_tools(vec![grammar_tool()]),
        )
        .await
        .expect("response");

    let call = only_call(response.output());
    assert_eq!(call.kind(), ToolCallKind::Custom);
    assert_eq!(call.name(), "apply_patch");
    assert_eq!(call.arguments(), &json!(PATCH));

    let body = sent_body(&server).await;
    assert_eq!(
        body["input"][1],
        json!({
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
            "input": PATCH,
        })
    );
    assert_eq!(
        body["input"][2],
        json!({
            "type": "custom_tool_call_output",
            "call_id": "call_patch",
            "output": "Created a.txt",
        })
    );
}

// ---- Chat Completions ----------------------------------------------------------------------

async fn chat_model(server: &MockServer, as_functions: bool) -> OpenAiChatModel {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl_1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_patch",
                        "type": "function",
                        "function": {
                            "name": "apply_patch",
                            "arguments": json!({"input": PATCH}).to_string(),
                        },
                    }],
                },
            }],
        })))
        .mount(server)
        .await;
    OpenAiChatModel::new(
        "chat-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model")
    .with_lowering_options(ChatLoweringOptions::new().with_custom_tools_as_functions(as_functions))
}

/// The reference's converter raises for any tool that is not a function, strict or not.
#[tokio::test]
async fn chat_refuses_a_custom_tool_and_a_custom_call_by_default() {
    let server = MockServer::start().await;
    let model = chat_model(&server, false).await;

    let error = model
        .get_response(
            ModelRequest::new(vec![], settings("openai", ModelSettings::new()))
                .with_tools(vec![grammar_tool()]),
        )
        .await
        .expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("custom tool `apply_patch` is not supported with the Chat Completions API"),
        "{error}"
    );

    let error = model
        .get_response(ModelRequest::new(
            custom_history(),
            settings("openai", ModelSettings::new()),
        ))
        .await
        .expect_err("refused");
    assert!(
        error.to_string().contains("custom tool `apply_patch`"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn chat_advertises_custom_tools_as_functions_when_asked() {
    let server = MockServer::start().await;
    let model = chat_model(&server, true).await;

    let response = model
        .get_response(
            ModelRequest::new(custom_history(), settings("openai", ModelSettings::new()))
                .with_tools(vec![grammar_tool()]),
        )
        .await
        .expect("response");

    let call = only_call(response.output());
    assert_eq!(call.kind(), ToolCallKind::Custom);
    assert_eq!(call.arguments(), &json!(PATCH));

    let body = sent_body(&server).await;
    let function = &body["tools"][0]["function"];
    assert_eq!(function["name"], "apply_patch");
    assert_eq!(function["description"], "Edits files.");
    assert_eq!(function["strict"], false);
    assert_eq!(function["parameters"]["required"], json!(["input"]));
    assert_eq!(function["parameters"]["additionalProperties"], false);
    assert_eq!(
        function["parameters"]["properties"]["input"]["type"],
        "string"
    );
    let replayed = &body["messages"][1]["tool_calls"][0]["function"];
    assert_eq!(replayed["name"], "apply_patch");
    let arguments: Value =
        serde_json::from_str(replayed["arguments"].as_str().expect("arguments")).unwrap();
    assert_eq!(arguments, json!({"input": PATCH}));
}

// ---- Anthropic Messages --------------------------------------------------------------------

async fn anthropic_model(server: &MockServer, as_functions: bool) -> AnthropicMessagesModel {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "apply_patch", "input": {"input": PATCH}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })))
        .mount(server)
        .await;
    AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model")
    .with_custom_tools_as_functions(as_functions)
}

fn anthropic_request(input: Vec<ModelInputItem>) -> ModelRequest {
    ModelRequest::new(
        input,
        settings("anthropic", ModelSettings::new().with_max_tokens(1024)),
    )
}

#[tokio::test]
async fn anthropic_refuses_a_custom_tool_and_a_custom_call_by_default() {
    let server = MockServer::start().await;
    let model = anthropic_model(&server, false).await;

    let error = model
        .get_response(
            anthropic_request(vec![ModelInputItem::Message(Message::user("hi"))])
                .with_tools(vec![grammar_tool()]),
        )
        .await
        .expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("custom tool `apply_patch` is not supported with Anthropic Messages"),
        "{error}"
    );

    let error = model
        .get_response(anthropic_request(custom_history()))
        .await
        .expect_err("refused");
    assert!(
        error.to_string().contains("custom tool `apply_patch`"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn anthropic_advertises_custom_tools_as_functions_when_asked() {
    let server = MockServer::start().await;
    let model = anthropic_model(&server, true).await;

    let response = model
        .get_response(
            anthropic_request(custom_history()).with_tools(vec![grammar_tool(), function_tool()]),
        )
        .await
        .expect("response");

    let call = only_call(response.output());
    assert_eq!(call.kind(), ToolCallKind::Custom);
    assert_eq!(call.arguments(), &json!(PATCH));

    let body = sent_body(&server).await;
    assert_eq!(body["tools"][0]["name"], "apply_patch");
    assert_eq!(body["tools"][0]["description"], "Edits files.");
    assert_eq!(
        body["tools"][0]["input_schema"]["required"],
        json!(["input"])
    );
    assert_eq!(body["tools"][1]["input_schema"], json!({"type": "object"}));
    let tool_use = &body["messages"][1]["content"][0];
    assert_eq!(tool_use["type"], "tool_use");
    assert_eq!(tool_use["input"], json!({"input": PATCH}));
}

/// A call to the function whose arguments are not an `input` string stays a function call, so the
/// tool reports the malformed input to the model instead of the turn failing.
#[tokio::test]
async fn a_malformed_function_form_call_stays_a_function_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "apply_patch", "input": {"patch": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })))
        .mount(&server)
        .await;
    let model = AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("model")
    .with_custom_tools_as_functions(true);

    let response = model
        .get_response(
            anthropic_request(vec![ModelInputItem::Message(Message::user("hi"))])
                .with_tools(vec![grammar_tool()]),
        )
        .await
        .expect("response");

    let call = only_call(response.output());
    assert_eq!(call.kind(), ToolCallKind::Function);
    assert_eq!(call.arguments(), &json!({"patch": 1}));
}
