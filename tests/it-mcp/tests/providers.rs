//! MCP declarations and resource references reach the model adapters under their selected contract.
#[path = "../support/mod.rs"]
mod support;
use ra_core::{
    item::{CallId, ModelInputItem, ToolCall, ToolCallOutput},
    model::{Model, ModelRequest, ModelSettings, ProviderKey, ResolvedModelSettings},
    tool::Tool,
};
use ra_mcp::{
    client::McpClientOptions,
    filter::ToolFilter,
    in_process::InProcessToolServer,
    tool::{McpDiscoveryOptions, McpToolNaming, discover_tools, map_tool_result},
};
use ra_model::{
    anthropic::{AnthropicAuth, AnthropicMessagesModel},
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn resolved() -> ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &ModelSettings::new().with_max_tokens(64),
    )
}

async fn request_tools(model: &dyn Model, server: &MockServer, request: ModelRequest) -> Value {
    model.get_response(request).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    body["tools"].clone()
}

#[tokio::test]
async fn bare_mcp_tools_are_accepted_by_chat_completions_and_anthropic() {
    let mut fixture = support::Fixture::new("providers");
    let mut bare = support::declaration("ping");
    bare.input_schema =
        std::sync::Arc::new(serde_json::from_value(json!({"type":"object"})).unwrap());
    fixture.tools_override = Some(vec![support::declaration("search"), bare]);
    let mut server = InProcessToolServer::connect("docs", fixture, McpClientOptions::default())
        .await
        .unwrap();
    for naming in [McpToolNaming::Original, McpToolNaming::ServerPrefixed] {
        let tools = discover_tools(
            &[(server.client(), ToolFilter::default())],
            &McpDiscoveryOptions::new().with_naming(naming),
        )
        .await
        .unwrap();
        let definitions: Vec<_> = tools.iter().map(|tool| tool.model_definition()).collect();
        let (search, ping) = match naming {
            McpToolNaming::Original => ("search", "ping"),
            _ => ("mcp_docs__search", "mcp_docs__ping"),
        };

        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chat", "object": "chat.completion", "created": 0, "model": "gpt-test",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "ok"}}]
            })))
            .mount(&http)
            .await;
        let chat = OpenAiChatModel::new(
            "gpt-test",
            OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", http.uri())),
        )
        .unwrap();
        let sent = request_tools(
            &chat,
            &http,
            ModelRequest::new(vec![], resolved()).with_tools(definitions.clone()),
        )
        .await;
        assert_eq!(sent[0]["function"]["name"], search);
        assert_eq!(sent[1]["function"]["name"], ping);
        assert_eq!(
            sent[1]["function"]["parameters"],
            json!({"type":"object","properties":{}})
        );

        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg", "type": "message", "role": "assistant", "model": "claude-test",
                "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&http)
            .await;
        let anthropic = AnthropicMessagesModel::new(
            "claude-test",
            AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", http.uri())),
        )
        .unwrap();
        let sent = request_tools(
            &anthropic,
            &http,
            ModelRequest::new(vec![], resolved()).with_tools(definitions),
        )
        .await;
        assert_eq!(sent[0]["name"], search);
        assert_eq!(sent[1]["name"], ping);
        assert_eq!(
            sent[1]["input_schema"],
            json!({"type":"object","properties":{}})
        );
    }
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn long_namespaced_mcp_tools_reach_responses_and_keep_raw_routing() {
    for (server_name, names) in [
        ("docs".to_owned(), vec!["x".repeat(80)]),
        (
            "s".repeat(80),
            vec!["search".to_owned(), "fetch".to_owned()],
        ),
        ("s".repeat(59), vec!["x".repeat(64)]),
    ] {
        let mut fixture = support::Fixture::new("raw");
        fixture.instructions = Some("Shared namespace instructions".into());
        fixture.tools_override = Some(
            names
                .iter()
                .map(|name| support::declaration(name))
                .collect(),
        );
        let mut server =
            InProcessToolServer::connect(&server_name, fixture, McpClientOptions::default())
                .await
                .unwrap();
        let tools = discover_tools(
            &[(server.client(), ToolFilter::default())],
            &McpDiscoveryOptions::new().with_naming(McpToolNaming::Namespaced),
        )
        .await
        .unwrap();
        assert_eq!(tools.len(), names.len());

        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id":"resp", "output":[]})),
            )
            .mount(&http)
            .await;
        let model = OpenAiResponsesModel::new(
            "gpt-test",
            OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", http.uri())),
        )
        .unwrap();
        let sent = request_tools(
            &model,
            &http,
            ModelRequest::new(vec![], resolved())
                .with_tools(tools.iter().map(|tool| tool.model_definition()).collect()),
        )
        .await;
        assert_eq!(sent.as_array().unwrap().len(), 1);
        assert_eq!(sent[0]["type"], "namespace");
        assert_eq!(sent[0]["description"], "Shared namespace instructions");
        assert_eq!(sent[0]["tools"].as_array().unwrap().len(), tools.len());
        for (index, tool) in tools.iter().enumerate() {
            let namespace = tool.info().callable_namespace().unwrap();
            assert!(namespace.len() <= 64);
            assert!(tool.schema().name().len() <= 64);
            assert!(namespace.len() + 2 + tool.schema().name().len() <= 128);
            assert_eq!(sent[0]["name"], namespace);
            assert_eq!(sent[0]["tools"][index]["name"], tool.schema().name());
            assert_eq!(
                tool.invoke(json!({"query":"hello"}))
                    .await
                    .unwrap()
                    .as_text(),
                Some(format!("raw:{}:hello", tool.info().tool().name).as_str()),
            );
        }
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn resource_links_reach_responses_and_anthropic_as_json_text() {
    let result: rmcp::model::CallToolResult = serde_json::from_value(json!({"content":[
        {"type":"resource_link", "uri":"resource://reports/1", "name":"report",
            "description":"Quarterly report", "mimeType":"application/pdf", "size":12},
        {"type":"resource_link", "uri":"https://example.test/report.pdf", "name":"download"}
    ]}))
    .unwrap();
    let expected: Vec<_> = result
        .content
        .iter()
        .map(|block| serde_json::to_value(block).unwrap())
        .collect();
    let output = serde_json::to_value(map_tool_result(result).unwrap()).unwrap();
    for anthropic in [false, true] {
        let http = MockServer::start().await;
        let response = if anthropic {
            json!({"id":"msg", "type":"message", "role":"assistant", "model":"claude-test",
                "content":[{"type":"text", "text":"ok"}], "stop_reason":"end_turn",
                "usage":{"input_tokens":1, "output_tokens":1}})
        } else {
            json!({"id":"resp", "output":[]})
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&http)
            .await;
        let model: Box<dyn Model> = if anthropic {
            Box::new(
                AnthropicMessagesModel::new(
                    "claude-test",
                    AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", http.uri())),
                )
                .unwrap(),
            )
        } else {
            Box::new(
                OpenAiResponsesModel::new(
                    "gpt-test",
                    OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", http.uri())),
                )
                .unwrap(),
            )
        };
        model
            .get_response(ModelRequest::new(
                vec![
                    ModelInputItem::ToolCall(ToolCall::new(
                        CallId::new("fetch-1"),
                        "fetch",
                        json!({}),
                    )),
                    ModelInputItem::ToolCallOutput(ToolCallOutput::new(
                        CallId::new("fetch-1"),
                        output.clone(),
                    )),
                ],
                resolved(),
            ))
            .await
            .unwrap();
        let requests = http.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let (parts, expected_type) = if anthropic {
            (&sent["messages"][1]["content"][0]["content"], "text")
        } else {
            (&sent["input"][1]["output"], "input_text")
        };
        assert_eq!(parts.as_array().unwrap().len(), expected.len());
        for (part, expected) in parts.as_array().unwrap().iter().zip(&expected) {
            assert_eq!(part["type"], expected_type);
            assert_eq!(
                serde_json::from_str::<Value>(part["text"].as_str().unwrap()).unwrap(),
                *expected
            );
        }
    }
}
