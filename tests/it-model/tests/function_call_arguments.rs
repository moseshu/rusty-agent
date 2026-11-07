//! The arguments a function call carries, as the OpenAI adapters parse them. An empty or blank
//! argument string is an absent value and becomes `{}`; JSON `null` stays `null`. Codex's MCP
//! handler draws the same line (`mcp_tool_call.rs`: blank text is `None`, shown to hooks as `{}`,
//! while `"null"` is `Some(Null)`), and the product's hook payloads rely on it.

use ra_core::{
    item::{Message, ModelInputItem, RunItemKind},
    model::{Model, ModelRequest, ModelSettings, ProviderKey, SettingsLayers},
};
use ra_model::openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

type ArgumentCase = (&'static str, fn() -> Value);

const CASES: [ArgumentCase; 4] = [
    ("", || json!({})),
    ("  ", || json!({})),
    ("null", || Value::Null),
    ("{}", || json!({})),
];

fn request() -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("call it"))],
        SettingsLayers::new()
            .with_run(&ModelSettings::new())
            .resolve(&ProviderKey::new("openai")),
    )
}

fn arguments(output: &[ra_core::item::RunItem]) -> Value {
    match output[0].kind() {
        RunItemKind::ToolCall(call) => call.arguments().clone(),
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[tokio::test]
async fn responses_reads_blank_arguments_as_an_empty_object_and_keeps_null() {
    for (text, expected) in CASES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp",
                "output": [{"type": "function_call", "call_id": "call-1", "name": "echo", "arguments": text}]
            })))
            .mount(&server)
            .await;
        let model = OpenAiResponsesModel::new(
            "gpt-test",
            OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
        )
        .unwrap();
        let response = model.get_response(request()).await.unwrap();
        assert_eq!(arguments(response.output()), expected(), "{text:?}");
    }
}

#[tokio::test]
async fn chat_reads_blank_arguments_as_an_empty_object_and_keeps_null() {
    for (text, expected) in CASES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chat",
                "object": "chat.completion",
                "created": 0,
                "model": "gpt-test",
                "choices": [{
                    "index": 0,
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "echo", "arguments": text}}]
                    }
                }]
            })))
            .mount(&server)
            .await;
        let model = OpenAiChatModel::new(
            "gpt-test",
            OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
        )
        .unwrap();
        let response = model.get_response(request()).await.unwrap();
        assert_eq!(arguments(response.output()), expected(), "{text:?}");
    }
}
