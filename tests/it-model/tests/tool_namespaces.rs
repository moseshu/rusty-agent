//! Tools advertised in a namespace, as the reference carries them
//! (`models/openai_responses.py` `convert_tools`, `_tool_identity.py`, and
//! `ensure_function_tool_supports_responses_only_features` on its other backends): OpenAI Responses
//! groups a namespace's functions under one `namespace` entry, reads the namespace a call names and
//! hands it back in the history; the other adapters refuse a namespaced tool.

use ra_core::{
    agent::AgentId,
    item::{CallId, Message, ModelInputItem, RunItemKind, ToolCall, ToolCallOutput},
    model::{
        Model, ModelHandoffDefinition, ModelRequest, ModelSettings, ModelToolDefinition,
        ProviderKey, ResolvedModelSettings, ToolChoice,
    },
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

fn resolved(settings: ModelSettings) -> ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

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
    .unwrap()
}

async fn request_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests[0].body).unwrap()
}

fn schema() -> Value {
    json!({"type": "object", "properties": {}})
}

fn function(name: &str) -> ModelToolDefinition {
    ModelToolDefinition::new(name, schema()).with_description(format!("{name} it"))
}

fn in_namespace(namespace: &str, name: &str, description: &str) -> ModelToolDefinition {
    function(name).with_namespace(namespace, Some(description.to_owned()))
}

/// The reference's `convert_tools`: a namespace stands where its first function would, holding
/// every function advertised in it in order, beside the top-level functions; handoffs come last.
#[tokio::test]
async fn responses_groups_a_namespace_where_its_first_function_stands() {
    let server = MockServer::start().await;
    let model = responses_model(&server, json!({"id": "resp", "output": []})).await;
    let request = ModelRequest::new(vec![], resolved(ModelSettings::new()))
        .with_tools(vec![
            function("lookup"),
            in_namespace("tickets", "search", "Ticket tools"),
            function("summarize"),
            in_namespace("tickets", "update", "Ticket tools"),
            in_namespace("docs", "search", "Doc tools"),
        ])
        .with_handoffs(vec![ModelHandoffDefinition::new(
            AgentId::new("reviewer"),
            "search",
            json!({"type": "object"}),
        )]);
    model.get_response(request).await.unwrap();

    let body = request_body(&server).await;
    let lowered = |name: &str| {
        json!({
            "type": "function",
            "name": name,
            "description": format!("{name} it"),
            "parameters": schema(),
            "strict": false,
        })
    };
    assert_eq!(
        body["tools"],
        json!([
            lowered("lookup"),
            {
                "type": "namespace",
                "name": "tickets",
                "description": "Ticket tools",
                "tools": [lowered("search"), lowered("update")],
            },
            lowered("summarize"),
            {
                "type": "namespace",
                "name": "docs",
                "description": "Doc tools",
                "tools": [lowered("search")],
            },
            {
                "type": "function",
                "name": "search",
                "parameters": {"type": "object"},
                "strict": false,
            },
        ])
    );
}

/// The reference refuses a namespace its tools describe differently; a namespace with no
/// description is described as empty.
#[tokio::test]
async fn responses_requires_one_description_per_namespace() {
    let server = MockServer::start().await;
    let model = responses_model(&server, json!({"id": "resp", "output": []})).await;
    let error = model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new())).with_tools(vec![
                in_namespace("tickets", "search", "Ticket tools"),
                in_namespace("tickets", "update", "Other tools"),
            ]),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("All tools in namespace 'tickets' must share the same description."),
        "{error}"
    );

    model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new()))
                .with_tools(vec![function("search").with_namespace("tickets", None)]),
        )
        .await
        .unwrap();
    assert_eq!(request_body(&server).await["tools"][0]["description"], "");
}

/// A call of a namespace's function names the namespace; the call is read with it, and handed
/// back with it in the history. A call naming a namespace is never read as a handoff.
#[tokio::test]
async fn responses_reads_and_replays_the_namespace_a_call_names() {
    let server = MockServer::start().await;
    let model = responses_model(
        &server,
        json!({
            "id": "resp",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "namespace": "tickets",
                    "name": "search",
                    "arguments": "{\"id\":7}"
                },
                {
                    "type": "function_call",
                    "call_id": "call-2",
                    "namespace": "",
                    "name": "lookup",
                    "arguments": "{}"
                }
            ]
        }),
    )
    .await;
    let history = vec![
        ModelInputItem::Message(Message::user("find it")),
        ModelInputItem::ToolCall(
            ToolCall::new(CallId::new("call-0"), "search", json!({"id": 3}))
                .with_namespace("tickets"),
        ),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(CallId::new("call-0"), json!("found"))),
    ];
    let response = model
        .get_response(
            ModelRequest::new(history, resolved(ModelSettings::new()))
                .with_tools(vec![in_namespace("tickets", "search", "Ticket tools")])
                .with_handoffs(vec![ModelHandoffDefinition::new(
                    AgentId::new("reviewer"),
                    "search",
                    json!({"type": "object"}),
                )]),
        )
        .await
        .unwrap();

    let calls: Vec<_> = response
        .output()
        .iter()
        .map(|item| match item.kind() {
            RunItemKind::ToolCall(call) => (call.namespace(), call.name().to_owned()),
            other => panic!("expected a tool call, got {other:?}"),
        })
        .collect();
    assert_eq!(
        calls,
        [
            (Some("tickets"), "search".to_owned()),
            (None, "lookup".to_owned())
        ]
    );

    let body = request_body(&server).await;
    assert_eq!(
        body["input"][1],
        json!({
            "type": "function_call",
            "call_id": "call-0",
            "namespace": "tickets",
            "name": "search",
            "arguments": "{\"id\":3}",
        })
    );
}

/// The reference lets a named `tool_choice` target a namespaced function by its qualified name,
/// and refuses one naming a namespace or a function within one alone.
#[tokio::test]
async fn responses_tool_choice_names_a_namespaced_function_by_its_qualified_name() {
    let server = MockServer::start().await;
    let model = responses_model(&server, json!({"id": "resp", "output": []})).await;
    let tools = vec![
        in_namespace("tickets", "search", "Ticket tools"),
        function("lookup"),
    ];
    let choose = |name: &str| {
        ModelRequest::new(
            vec![],
            resolved(ModelSettings::new().with_tool_choice(ToolChoice::Tool(name.to_owned()))),
        )
        .with_tools(tools.clone())
    };

    model.get_response(choose("tickets.search")).await.unwrap();
    assert_eq!(
        request_body(&server).await["tool_choice"],
        json!({"type": "function", "name": "tickets.search"})
    );
    for name in ["search", "tickets"] {
        let error = model.get_response(choose(name)).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Named tool_choice must target a callable tool"),
            "{error}"
        );
    }
}

/// A custom tool has no place in a namespace, which holds functions.
#[tokio::test]
async fn responses_refuses_a_namespaced_custom_tool() {
    let server = MockServer::start().await;
    let model = responses_model(&server, json!({"id": "resp", "output": []})).await;
    let error = model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new())).with_tools(vec![
                ModelToolDefinition::custom("patch", None).with_namespace("edits", None),
            ]),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("`edits.patch`"), "{error}");
}

/// The framework rejects the reserved synthetic namespace shape even when a host constructs the
/// model request directly instead of registering executable tools through an agent.
#[tokio::test]
async fn responses_refuses_same_name_namespace_shapes_before_sending() {
    let server = MockServer::start().await;
    let model = responses_model(&server, json!({"id": "resp", "output": []})).await;
    let error = model
        .get_response(
            ModelRequest::new(vec![], resolved(ModelSettings::new()))
                .with_tools(vec![in_namespace("app", "app", "App tools")]),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains(
        "Responses tool-search reserves the synthetic namespace `app.app` for deferred top-level function tools. Rename the namespace or tool name to avoid ambiguous dispatch."
    ), "{error}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// The reference's other backends refuse a namespaced tool before anything is sent.
#[tokio::test]
async fn other_protocols_refuse_namespaced_tools() {
    let request = || {
        ModelRequest::new(vec![], resolved(ModelSettings::new().with_max_tokens(64))).with_tools(
            vec![
                function("lookup"),
                in_namespace("tickets", "search", "Ticket tools"),
            ],
        )
    };

    let server = MockServer::start().await;
    let chat = OpenAiChatModel::new(
        "gpt-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    let error = chat.get_response(request()).await.unwrap_err();
    assert!(
        error.to_string().contains(
            "The following function-tool features are only supported with OpenAI Responses \
             models: tool namespace. Tool `tickets.search` cannot be used with Chat \
             Completions-compatible models."
        ),
        "{error}"
    );

    let anthropic = AnthropicMessagesModel::new(
        "claude-test",
        AnthropicAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    let error = anthropic.get_response(request()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Tool `tickets.search` cannot be used with Anthropic Messages models."),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
