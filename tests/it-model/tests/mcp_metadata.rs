//! Hosted MCP display metadata, persistence, and replay through the real Responses adapter.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::Result,
    filter::{ContextFilter, ContextFilterRequest, ModelInputData},
    item::{ModelInputItem, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel, SettingsLayers,
    },
    state::{RunId, RunState, ToolUse},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, RunStreamEvent, Runner},
};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

fn response(id: &str, output: Vec<Value>) -> Value {
    json!({"id":id, "status":"completed", "output":output})
}
fn list(id: &str, server: &str, tools: Vec<Value>) -> Value {
    json!({"id":id, "type":"mcp_list_tools", "server_label":server, "tools":tools})
}
fn call(id: &str, server: &str) -> Value {
    json!({"id":id, "type":"mcp_call", "server_label":server, "name":"search", "arguments":"{}",
        "output":"result", "status":"completed"})
}
fn message() -> Value {
    json!({"id":"answer", "type":"message", "role":"assistant", "content":[{"type":"output_text","text":"done"}]})
}

struct Responses(Mutex<VecDeque<Value>>);
impl Respond for Responses {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let payload = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        let request: Value = serde_json::from_slice(&request.body).unwrap();
        if request["stream"] == true {
            let mut body = format!(
                "data: {}\n\n",
                json!({"type":"response.created", "response":{"id":payload["id"]}})
            );
            for (index, item) in payload["output"].as_array().unwrap().iter().enumerate() {
                body.push_str(&format!(
                    "data: {}\n\n",
                    json!({"type":"response.output_item.done", "output_index":index, "item":item})
                ));
            }
            body.push_str(&format!(
                "data: {}\n\n",
                json!({"type":"response.completed", "response":payload})
            ));
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        } else {
            ResponseTemplate::new(200).set_body_json(payload)
        }
    }
}

struct Resolver(Arc<dyn Model>);
fn settings() -> ModelSettings {
    ModelSettings::new().with_extra_body(
        ProviderKey::new("openai"),
        BTreeMap::from([(
            "tools".into(),
            json!([
                {"type":"mcp", "server_label":"docs", "server_url":"https://example.test/mcp"},
                {"type":"mcp", "server_label":"other", "server_url":"https://example.test/other"}
            ]),
        )]),
    )
}
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                None,
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            settings(),
            ModelSettings::new(),
        ))
    }
}
async fn model(payloads: Vec<Value>) -> (MockServer, Arc<dyn Model>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Responses(Mutex::new(payloads.into())))
        .mount(&server)
        .await;
    let model = Arc::new(
        OpenAiResponsesModel::new(
            "gpt-test",
            OpenAiAuth::new("test").with_base_url(format!("{}/v1/", server.uri())),
        )
        .unwrap(),
    );
    (server, model)
}
fn request(agent: Arc<AgentSpec>, model: Arc<dyn Model>) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(model)),
        RunId::new("hosted-metadata"),
        CancelScope::root(),
        vec![],
    )
}
async fn drive(request: RunRequest, streamed: bool) -> (RunResult, Vec<RunItem>) {
    if streamed {
        let mut stream = Runner::run_streamed(request);
        let mut calls = Vec::new();
        while let Some(event) = stream.next_event().await {
            if let RunStreamEvent::Item(item) = event
                && matches!(item.kind(), RunItemKind::McpCall(_))
            {
                calls.push(item);
            }
        }
        (stream.finish().await.unwrap(), calls)
    } else {
        (Runner::run(request).await.unwrap(), Vec::new())
    }
}

#[tokio::test]
async fn current_response_metadata_wins_for_every_hosted_call_and_replay_drops_display_fields() {
    for streamed in [false, true] {
        let output = vec![
            call("docs-call", "docs"),
            call("other-call", "other"),
            call("unknown-call", "missing"),
            list(
                "old-list",
                "docs",
                vec![
                    json!({"name":"search", "input_schema":{}, "description":"Old", "title":"Old title"}),
                ],
            ),
            list(
                "other-list",
                "other",
                vec![
                    json!({"name":"search", "input_schema":{}, "description":"Other description", "title":"Explicit", "annotations":{"title":"Ignored"}}),
                ],
            ),
            list(
                "latest-list",
                "docs",
                vec![
                    Value::Null,
                    json!({"name":123}),
                    json!({"name":""}),
                    json!({"name":"search", "input_schema":{}, "description":123, "title":true}),
                    json!({"name":"search", "description":"", "title":"", "annotations":{"title":"Latest title"}, "provider_extra":[1,2]}),
                ],
            ),
            json!({"id":"non-array", "type":"mcp_list_tools", "server_label":"docs", "tools":null}),
            json!({"id":"non-string-server", "type":"mcp_list_tools", "server_label":123, "tools":[{"name":"search", "title":"Ignored"}]}),
            json!({"id":"empty-server", "type":"mcp_list_tools", "server_label":"", "tools":[{"name":"search", "title":"Ignored"}]}),
            message(),
        ];
        let (server, model) = model(vec![
            response("result", output.clone()),
            response("replay", vec![]),
        ])
        .await;
        let agent = AgentSpec::builder()
            .id(AgentId::new("hosted"))
            .name("Hosted")
            .build()
            .unwrap();
        let (result, events) = drive(request(agent, model.clone()), streamed).await;
        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
        let calls: Vec<_> = result
            .new_items()
            .iter()
            .filter(|item| matches!(item.kind(), RunItemKind::McpCall(_)))
            .collect();
        assert_eq!(calls.len(), 3);
        let usage = result
            .state()
            .tool_use()
            .agent(&AgentId::new("hosted"))
            .unwrap();
        assert_eq!(usage.turn_calls(), 3);
        assert_eq!(
            usage.entries()[0].identity(),
            &ToolUse::Mcp {
                server: "docs".into(),
                tool_name: "search".into(),
            }
        );
        assert_eq!(
            (calls[0].description(), calls[0].title()),
            (None, Some("Latest title"))
        );
        assert_eq!(
            (calls[1].description(), calls[1].title()),
            (Some("Other description"), Some("Explicit"))
        );
        assert_eq!((calls[2].description(), calls[2].title()), (None, None));
        assert_eq!(calls[0].raw_provider_item().unwrap().payload(), &output[0]);
        if streamed {
            assert_eq!(events.iter().collect::<Vec<_>>(), calls);
        }
        let restored: RunState =
            serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
        let call = restored
            .generated_items()
            .iter()
            .find(|item| item.id().as_str() == "docs-call")
            .unwrap();
        assert_eq!(
            (call.description(), call.title()),
            (None, Some("Latest title"))
        );
        let input = restored
            .generated_items()
            .iter()
            .filter_map(RunItem::to_model_input)
            .collect();
        let provider = ProviderKey::new("openai");
        let settings = settings();
        let resolved = SettingsLayers::new().with_run(&settings).resolve(&provider);
        model
            .get_response(ModelRequest::new(input, resolved))
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let replay: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        for (index, expected) in output[..output.len() - 1].iter().enumerate() {
            assert_eq!(&replay["input"][index], expected);
            if index < 3 {
                assert!(replay["input"][index].get("title").is_none());
                assert!(replay["input"][index].get("description").is_none());
            }
        }
    }
}

struct Advance {
    origin: ToolOrigin,
    schema: ToolSchema,
}
impl Advance {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("advance").unwrap(),
            schema: ToolSchema::loose("advance", json!({"type":"object","properties":{}})).unwrap(),
        }
    }
}
#[async_trait]
impl Tool for Advance {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always)
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("continue"))
    }
}
struct HideDirectory;
impl ContextFilter for HideDirectory {
    fn name(&self) -> &str {
        "hide-directory"
    }
    fn filter_model_input(
        &self,
        _: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        let input = data
            .input()
            .iter()
            .filter(|item| !matches!(item, ModelInputItem::McpListTools(_)))
            .cloned()
            .collect();
        Ok(data.with_input(input))
    }
}

#[tokio::test]
async fn restored_pre_step_items_supply_hosted_metadata_after_the_model_filter_hides_the_directory()
{
    for streamed in [false, true] {
        let (server, model) = model(vec![
            response("first", vec![list("directory", "docs", vec![json!({"name":"search","input_schema":{},"description":"Search docs","annotations":{"title":"Search title"}})]),
                json!({"id":"advance", "type":"function_call", "call_id":"advance-call", "name":"advance", "arguments":"{}"})]),
            response("second", vec![call("hosted-call", "docs"), message()]),
        ]).await;
        let agent = AgentSpec::builder()
            .id(AgentId::new("hosted"))
            .name("Hosted")
            .tool(Arc::new(Advance::new()))
            .build()
            .unwrap();
        let config = RunConfig::new().with_context_filter(Arc::new(HideDirectory));
        let (paused, _) = drive(
            request(agent.clone(), model.clone()).with_config(config.clone()),
            streamed,
        )
        .await;
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approval");
        };
        let mut state: RunState =
            serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
        state.approve(&items[0], false).unwrap();
        let (resumed, events) = drive(
            request(agent, model).with_config(config).with_state(state),
            streamed,
        )
        .await;
        let item = resumed
            .new_items()
            .iter()
            .find(|item| matches!(item.kind(), RunItemKind::McpCall(_)))
            .unwrap();
        assert_eq!(
            (item.description(), item.title()),
            (Some("Search docs"), Some("Search title"))
        );
        if streamed {
            assert_eq!(events.as_slice(), std::slice::from_ref(item));
        }
        let requests = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert!(!sent["input"].to_string().contains("mcp_list_tools"));
        assert!(!sent["input"].to_string().contains("Search title"));
    }
}
