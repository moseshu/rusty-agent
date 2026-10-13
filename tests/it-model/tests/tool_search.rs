//! Native client discovery wire contracts from Codex and the Python Responses converter.

use ra_core::{
    item::{CallId, ModelInputItem, RunItemKind, ToolCall, ToolCallKind, ToolCallOutput},
    model::{
        Model, ModelRequest, ModelSettings, ModelToolDefinition, ProviderKey, ResolvedModelSettings,
    },
    tool::{ToolOutput, ToolOutputBlock},
};
use ra_model::{
    anthropic::{AnthropicAuth, AnthropicMessagesModel},
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn settings() -> ResolvedModelSettings {
    ModelSettings::new().with_max_tokens(64).resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &ModelSettings::new(),
    )
}
fn search_definition() -> ModelToolDefinition {
    ModelToolDefinition::tool_search(
        json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    )
    .with_description("Search deferred tools")
}
fn tool(name: &str, namespace: &str) -> ModelToolDefinition {
    ModelToolDefinition::new(name, json!({"type": "object", "properties": {}}))
        .with_namespace(namespace, Some(format!("{namespace} tools")))
        .with_defer_loading(true)
}
async fn model(server: &MockServer, output: Value) -> OpenAiResponsesModel {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "response", "output": output})),
        )
        .mount(server)
        .await;
    OpenAiResponsesModel::new(
        "test",
        OpenAiAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap()
}
async fn body(server: &MockServer) -> Value {
    serde_json::from_slice(
        &server
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .body,
    )
    .unwrap()
}

#[tokio::test]
async fn native_search_call_and_output_lift_and_replay() {
    let server = MockServer::start().await;
    let output = json!([
        {"type": "tool_search_call", "id": "search-call", "call_id": "search", "execution": "client", "created_by": "server",
            "arguments": {"query": "calendar", "limit": 1}},
        {"type": "tool_search_output", "id": "search-output", "call_id": "search", "execution": "client", "status": "completed",
            "tools": [{"type": "namespace", "name": "calendar", "description": "Calendar tools", "tools": [
                {"type": "function", "name": "create_event", "description": "Create events", "parameters": {"type": "object"}, "strict": false, "defer_loading": true}
            ]}]}
    ]);
    let model = model(&server, output).await;
    let response = model
        .get_response(ModelRequest::new(vec![], settings()).with_tools(vec![search_definition()]))
        .await
        .unwrap();
    let RunItemKind::ToolCall(call) = response.output()[0].kind() else {
        panic!("search call");
    };
    assert_eq!(call.kind(), ToolCallKind::ToolSearch);
    assert_eq!(call.arguments(), &json!({"query": "calendar", "limit": 1}));
    let RunItemKind::ToolCallOutput(output) = response.output()[1].kind() else {
        panic!("search output");
    };
    assert_eq!(output.kind(), ToolCallKind::ToolSearch);
    let stored = ToolOutput::from_stored(output.output()).unwrap().unwrap();
    assert_eq!(
        stored.discovered_tools().unwrap()[0].qualified_name(),
        "calendar.create_event"
    );
    let input = response
        .output()
        .iter()
        .filter_map(|item| item.to_model_input())
        .collect();
    model
        .get_response(ModelRequest::new(input, settings()).with_tools(vec![search_definition()]))
        .await
        .unwrap();
    let replay = body(&server).await;
    assert_eq!(replay["input"][0]["type"], "tool_search_call");
    assert_eq!(replay["input"][0]["execution"], "client");
    assert!(replay["input"][0].get("created_by").is_none());
    assert_eq!(replay["input"][1]["type"], "tool_search_output");
    assert_eq!(
        replay["input"][1]["tools"][0]["tools"][0]["defer_loading"],
        true
    );
}

#[tokio::test]
async fn discovered_definitions_coalesce_and_top_level_tools_use_framework_lookup_shape() {
    let server = MockServer::start().await;
    let model = model(&server, json!([])).await;
    let definitions = vec![
        tool("create", "calendar"),
        ModelToolDefinition::custom("compose", None).with_defer_loading(true),
        tool("read", "docs"),
        tool("list", "calendar"),
        ModelToolDefinition::new("lookup", json!({"type": "object"})).with_defer_loading(true),
    ];
    let output = ToolCallOutput::new(
        CallId::new("search"),
        serde_json::to_value(ToolOutput::tool_search(definitions)).unwrap(),
    )
    .with_kind(ToolCallKind::ToolSearch);
    model
        .get_response(ModelRequest::new(
            vec![ModelInputItem::ToolCallOutput(output)],
            settings(),
        ))
        .await
        .unwrap();
    let body = body(&server).await;
    let tools = &body["input"][0]["tools"];
    assert_eq!(tools[0]["name"], "calendar");
    assert_eq!(tools[0]["tools"][0]["name"], "create");
    assert_eq!(tools[0]["tools"][1]["name"], "list");
    assert_eq!(tools[1]["type"], "custom");
    assert_eq!(tools[1]["name"], "compose");
    assert_eq!(tools[1]["defer_loading"], true);
    assert_eq!(tools[2]["name"], "docs");
    assert_eq!(tools[3]["name"], "lookup");
    assert_eq!(tools[3]["tools"][0]["name"], "lookup");
}

#[tokio::test]
async fn responses_validates_search_configuration_and_keeps_real_function_name() {
    let server = MockServer::start().await;
    let model = model(&server, json!([])).await;
    for (tools, error) in [
        (
            vec![search_definition(), search_definition()],
            "Only one ToolSearchTool()",
        ),
        (
            vec![tool("lookup", "records")],
            "Deferred-loading Responses tools require ToolSearchTool()",
        ),
    ] {
        assert!(
            model
                .get_response(ModelRequest::new(vec![], settings()).with_tools(tools))
                .await
                .unwrap_err()
                .to_string()
                .contains(error)
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    model
        .get_response(ModelRequest::new(vec![], settings()).with_tools(vec![
            ModelToolDefinition::new("tool_search", json!({"type": "object"})),
            search_definition(),
        ]))
        .await
        .unwrap();
    let body = body(&server).await;
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "tool_search");
    assert_eq!(body["tools"][1]["type"], "tool_search");
    assert!(body["tools"][1].get("name").is_none());
}

#[tokio::test]
async fn other_backends_refuse_search_definitions_and_search_history() {
    let server = MockServer::start().await;
    let models: Vec<Box<dyn Model>> = vec![
        Box::new(
            OpenAiChatModel::new(
                "test",
                OpenAiAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
            )
            .unwrap(),
        ),
        Box::new(
            AnthropicMessagesModel::new(
                "test",
                AnthropicAuth::new("secret").with_base_url(format!("{}/", server.uri())),
            )
            .unwrap(),
        ),
    ];
    for model in models {
        assert!(
            model
                .get_response(
                    ModelRequest::new(
                        vec![ModelInputItem::Message(ra_core::item::Message::user(
                            "hello"
                        ))],
                        settings()
                    )
                    .with_tools(vec![search_definition()])
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("only supported with OpenAI Responses")
        );
        let call = ToolCall::new(
            CallId::new("search"),
            "tool_search",
            json!({"query": "calendar"}),
        )
        .with_kind(ToolCallKind::ToolSearch);
        let output = ToolCallOutput::new(
            CallId::new("search"),
            serde_json::to_value(ToolOutput::tool_search(vec![])).unwrap(),
        )
        .with_kind(ToolCallKind::ToolSearch);
        let error = model
            .get_response(ModelRequest::new(
                vec![
                    ModelInputItem::ToolCall(call),
                    ModelInputItem::ToolCallOutput(output),
                ],
                settings(),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Tool search history"), "{error}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn responses_sends_structured_audio_in_tool_output() {
    let server = MockServer::start().await;
    let model = model(&server, json!([])).await;
    let url = "data:audio/wav;base64,YXVkaW8=";
    let output = ToolCallOutput::new(
        CallId::new("audio"),
        serde_json::to_value(ToolOutput::block(ToolOutputBlock::audio(url))).unwrap(),
    );
    model
        .get_response(ModelRequest::new(
            vec![ModelInputItem::ToolCallOutput(output)],
            settings(),
        ))
        .await
        .unwrap();
    assert_eq!(
        body(&server).await["input"][0]["output"],
        json!([{ "type": "input_audio", "audio_url": url }])
    );
}

/// Explicit same-name namespaces stay forbidden; only discovery synthesizes the reserved shape.
#[tokio::test]
async fn explicit_deferred_same_name_namespace_is_still_rejected() {
    let server = MockServer::start().await;
    let model = model(&server, json!([])).await;
    let error = model
        .get_response(
            ModelRequest::new(vec![], settings())
                .with_tools(vec![tool("lookup", "lookup"), search_definition()]),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("reserves the synthetic namespace"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// The framework accepts a deferred top-level function named like an explicit namespace. Both
/// reach the wire in one namespace, which must carry the explicit description rather than refuse
/// a history entry that every later request would replay.
#[tokio::test]
async fn deferred_top_level_function_shares_an_explicit_namespace_of_its_name() {
    for (explicit, expected) in [(Some("CRM tools"), "CRM tools"), (None, "")] {
        let server = MockServer::start().await;
        let model = model(&server, json!([])).await;
        let definitions = vec![
            ModelToolDefinition::new("crm", json!({"type": "object"})).with_defer_loading(true),
            ModelToolDefinition::new("lookup", json!({"type": "object"}))
                .with_namespace("crm", explicit.map(str::to_owned))
                .with_defer_loading(true),
        ];
        let output = ToolCallOutput::new(
            CallId::new("search"),
            serde_json::to_value(ToolOutput::tool_search(definitions)).unwrap(),
        )
        .with_kind(ToolCallKind::ToolSearch);
        model
            .get_response(ModelRequest::new(
                vec![ModelInputItem::ToolCallOutput(output)],
                settings(),
            ))
            .await
            .unwrap();
        let body = body(&server).await;
        let tools = body["input"][0]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "namespace");
        assert_eq!(tools[0]["name"], "crm");
        assert_eq!(tools[0]["description"], expected);
        let names: Vec<_> = tools[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["crm", "lookup"]);
    }
}
