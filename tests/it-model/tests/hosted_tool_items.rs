//! Hosted item processing, tool-choice resets, and lossless adapter replay.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::StreamExt;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{ModelResponse, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel, SettingsLayers, ToolChoice,
    },
    session::Session,
    state::{RunId, RunState, ToolUse},
    step::ProcessedResponse,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunOutcome, RunRequest, RunResult, RunStreamEvent, Runner},
};
use ra_session::InMemorySession;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

fn message() -> Value {
    json!({"id":"answer", "type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]})
}
fn hosted_items() -> Vec<Value> {
    vec![
        json!({"id":"file", "type":"file_search_call", "status":"completed", "queries":["query"], "results":[{"file_id":"doc", "score":0.9}]}),
        json!({"id":"web", "type":"web_search_call", "status":"completed", "action":{"type":"search", "query":"query", "sources":[{"url":"https://example.test"}]}}),
        json!({"id":"image", "type":"image_generation_call", "status":"completed", "result":"image-data", "revised_prompt":"prompt", "created_by":"server"}),
        json!({"id":"code", "type":"code_interpreter_call", "status":"completed", "code":"print(1)", "container_id":"container", "outputs":[{"type":"logs", "logs":"1"}]}),
    ]
}
fn search_items() -> Vec<Value> {
    vec![
        json!({"id":"search", "type":"tool_search_call", "execution":"server", "status":"completed", "arguments":{"query":"profile", "paths":["crm"]}, "created_by":"server"}),
        json!({"id":"search-output", "type":"tool_search_output", "execution":"server", "status":"completed", "tools":[{"type":"function", "name":"lookup_account", "parameters":{"type":"object"}, "provider_extra":42}], "created_by":"server"}),
    ]
}
fn input_only_items() -> Vec<Value> {
    vec![
        json!({"type":"computer_call_output", "call_id":"computer", "output":{"type":"computer_screenshot", "image_url":"https://example.test/screenshot.png"}}),
        json!({"type":"mcp_approval_response", "approval_request_id":"approval", "approve":true}),
        json!({"type":"local_shell_call_output", "call_id":"shell", "output":"done"}),
        json!({"type":"apply_patch_call_output", "call_id":"patch", "status":"completed", "output":"patched"}),
    ]
}
fn replay(item: &Value) -> Value {
    let mut item = item.clone();
    item.as_object_mut().unwrap().remove("created_by");
    item
}
fn response(id: usize, items: Vec<Value>) -> Value {
    json!({"id":format!("response-{id}"), "status":"completed", "output":items})
}
struct Responses(Mutex<VecDeque<Value>>);
impl Respond for Responses {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let payload = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["stream"] == true {
            let mut frames =
                vec![json!({"type":"response.created", "response":{"id":payload["id"]}})];
            for (index, item) in payload["output"].as_array().unwrap().iter().enumerate() {
                frames.push(
                    json!({"type":"response.output_item.done", "output_index":index, "item":item}),
                );
            }
            frames.push(json!({"type":"response.completed", "response":payload}));
            let body = frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>();
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        } else {
            ResponseTemplate::new(200).set_body_json(payload)
        }
    }
}
fn settings() -> ModelSettings {
    ModelSettings::new().with_extra_body(
        ProviderKey::new("openai"),
        BTreeMap::from([("tools".into(), json!([{"type":"web_search"}]))]),
    )
}
struct Resolver(Arc<dyn Model>);
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
struct TestModel {
    inner: OpenAiResponsesModel,
    streamed: bool,
    choices: Mutex<Vec<Option<ToolChoice>>>,
}
#[async_trait]
impl Model for TestModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.choices
            .lock()
            .unwrap()
            .push(request.model_settings().tool_choice().cloned());
        self.inner.get_response(request).await
    }
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        if self.streamed {
            self.choices
                .lock()
                .unwrap()
                .push(request.model_settings().tool_choice().cloned());
            self.inner.stream_response(request)
        } else {
            Box::pin(futures::stream::once(async move {
                self.get_response(request)
                    .await
                    .map(|response| ModelStreamEvent::Completed(Box::new(response)))
            }))
        }
    }
}
async fn model(outputs: Vec<Vec<Value>>, streamed: bool) -> (MockServer, Arc<TestModel>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Responses(Mutex::new(
            outputs
                .into_iter()
                .enumerate()
                .map(|(index, output)| response(index, output))
                .collect(),
        )))
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("test").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    (
        server,
        Arc::new(TestModel {
            inner: model,
            streamed,
            choices: Mutex::new(vec![]),
        }),
    )
}
fn request(agent: Arc<AgentSpec>, model: Arc<dyn Model>) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(model)),
        RunId::new("hosted-items"),
        CancelScope::root(),
        vec![],
    )
}
async fn drive(request: RunRequest, streamed: bool) -> (RunResult, Vec<RunItem>) {
    if streamed {
        let mut stream = Runner::run_streamed(request);
        let mut items = vec![];
        while let Some(event) = stream.next_event().await {
            if let RunStreamEvent::Item(item) = event {
                items.push(item);
            }
        }
        (stream.finish().await.unwrap(), items)
    } else {
        (Runner::run(request).await.unwrap(), vec![])
    }
}
fn agent(reset: bool) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(Arc::new(SearchFunction {
            origin: ToolOrigin::new("unused").unwrap(),
            schema: ToolSchema::loose("unused", json!({"type":"object"})).unwrap(),
        }))
        .model_settings(ModelSettings::new().with_tool_choice(ToolChoice::Required))
        .reset_tool_choice(reset)
        .build()
        .unwrap()
}
async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[tokio::test]
async fn every_hosted_call_records_replays_and_resets_without_local_execution() {
    for streamed in [false, true] {
        for reset in [false, true] {
            for raw in hosted_items() {
                let (server, model) =
                    model(vec![vec![raw.clone()], vec![message()]], streamed).await;
                let session = Arc::new(InMemorySession::new("hosted-items"));
                let declaration = agent(reset);
                assert_eq!(
                    declaration
                        .to_builder()
                        .build()
                        .unwrap()
                        .reset_tool_choice(),
                    reset
                );
                let (result, events) = drive(
                    request(declaration, model.clone()).with_session(session.clone()),
                    streamed,
                )
                .await;
                assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
                let stored = session.get_items(None).await.unwrap();
                let stored: Vec<RunItem> =
                    serde_json::from_value(serde_json::to_value(stored).unwrap()).unwrap();
                assert_eq!(stored, result.new_items());
                assert_eq!(stored[0].raw_provider_item().unwrap().payload(), &raw);
                assert!(matches!(stored[0].kind(), RunItemKind::HostedToolCall(_)));
                let processed = ProcessedResponse::builder()
                    .item(stored[0].clone())
                    .build()
                    .unwrap();
                assert!(!processed.has_tools_or_approvals_to_run());
                assert!(processed.functions().is_empty());
                let name = raw["type"].as_str().unwrap().strip_suffix("_call").unwrap();
                assert_eq!(processed.tools_used(), [ToolUse::Hosted(name.into())]);
                assert_eq!(processed.attempts().len(), 1);
                let usage = result
                    .state()
                    .tool_use()
                    .agent(&AgentId::new("worker"))
                    .unwrap();
                assert_eq!(usage.run_calls(), 1);
                if streamed {
                    assert_eq!(events, stored);
                }
                let restored: RunState =
                    serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
                assert_eq!(
                    restored
                        .tool_use()
                        .agent(&AgentId::new("worker"))
                        .unwrap()
                        .run_calls(),
                    1
                );
                assert_eq!(
                    *model.choices.lock().unwrap(),
                    vec![
                        Some(ToolChoice::Required),
                        if reset {
                            None
                        } else {
                            Some(ToolChoice::Required)
                        }
                    ]
                );
                let bodies = bodies(&server).await;
                assert_eq!(bodies[0]["stream"] == true, streamed);
                assert_eq!(
                    bodies.len(),
                    2,
                    "hosted activity must run again without a message"
                );
                assert_eq!(bodies[0]["tool_choice"], "required");
                assert_eq!(
                    bodies[1]["tool_choice"],
                    if reset { "auto" } else { "required" }
                );
                assert_eq!(bodies[1]["input"], json!([replay(&raw)]));
            }
        }
    }
    assert!(
        AgentSpec::builder()
            .id(AgentId::new("default"))
            .name("Default")
            .build()
            .unwrap()
            .reset_tool_choice()
    );
}

#[tokio::test]
async fn server_search_activity_runs_again_but_does_not_reset_choice() {
    for streamed in [false, true] {
        // Output-only activity counts too; no fabricated local pairing is required.
        for raw in [search_items(), vec![search_items().remove(1)]] {
            let (server, model) = model(vec![raw.clone(), vec![message()]], streamed).await;
            let (result, _) = drive(request(agent(true), model), streamed).await;
            let processed = raw
                .iter()
                .zip(result.new_items())
                .fold(ProcessedResponse::builder(), |builder, (_, item)| {
                    builder.item(item.clone())
                })
                .build()
                .unwrap();
            assert_eq!(
                processed.tools_used(),
                [ToolUse::Hosted("tool_search".into())]
            );
            assert!(processed.attempts().is_empty());
            assert!(!processed.has_tools_or_approvals_to_run());
            let usage = result
                .state()
                .tool_use()
                .agent(&AgentId::new("worker"))
                .unwrap();
            assert_eq!(usage.run_calls(), 0);
            let bodies = bodies(&server).await;
            assert_eq!(bodies.len(), 2);
            assert_eq!(bodies[1]["tool_choice"], "required");
            assert_eq!(
                bodies[1]["input"],
                Value::Array(raw.iter().map(replay).collect())
            );
        }
    }
}

struct SearchFunction {
    origin: ToolOrigin,
    schema: ToolSchema,
}
#[async_trait]
impl Tool for SearchFunction {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("found"))
    }
}
#[tokio::test]
async fn function_named_tool_search_resets_after_mixed_server_search_records() {
    for streamed in [false, true] {
        let mut output = search_items();
        output.push(json!({"id":"function", "type":"function_call", "call_id":"function-call", "name":"tool_search", "arguments":"{}"}));
        // A hosted item after discovery also detects name/identity shifts in reset tracking.
        output.push(hosted_items().remove(0));
        let (server, model) = model(vec![output, vec![message()]], streamed).await;
        let agent = agent(true)
            .to_builder()
            .tool(Arc::new(SearchFunction {
                origin: ToolOrigin::new("tool_search").unwrap(),
                schema: ToolSchema::loose("tool_search", json!({"type":"object"})).unwrap(),
            }))
            .build()
            .unwrap();
        let (result, _) = drive(request(agent, model), streamed).await;
        let usage = result
            .state()
            .tool_use()
            .agent(&AgentId::new("worker"))
            .unwrap();
        assert_eq!(usage.run_calls(), 2);
        assert_eq!(
            usage.entries()[0].identity(),
            &ToolUse::Tool(ToolOrigin::new("tool_search").unwrap().lookup_key().clone())
        );
        assert_eq!(
            usage.entries()[1].identity(),
            &ToolUse::Hosted("file_search".into())
        );
        let bodies = bodies(&server).await;
        assert_eq!(bodies[1]["tool_choice"], "auto");
        assert!(
            bodies[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call_output"
                    && item["call_id"] == "function-call")
        );
    }
}

#[tokio::test]
async fn tool_choice_reset_tracks_this_run_across_discovery_only_turns() {
    for streamed in [false, true] {
        let (server, model) = model(
            vec![
                vec![hosted_items().remove(0)],
                search_items(),
                vec![message()],
                vec![hosted_items().remove(0)],
                vec![message()],
            ],
            streamed,
        )
        .await;
        for _ in 0..2 {
            let (result, _) = drive(request(agent(true), model.clone()), streamed).await;
            assert_eq!(
                result
                    .state()
                    .tool_use()
                    .agent(&AgentId::new("worker"))
                    .unwrap()
                    .run_calls(),
                1
            );
        }
        let bodies = bodies(&server).await;
        assert_eq!(bodies.len(), 5);
        assert_eq!(bodies[1]["tool_choice"], "auto");
        assert_eq!(bodies[2]["tool_choice"], "auto");
        assert_eq!(bodies[3]["tool_choice"], "required");
        assert_eq!(bodies[3]["input"], json!([]));
        assert_eq!(bodies[4]["tool_choice"], "auto");
        assert_eq!(
            *model.choices.lock().unwrap(),
            vec![
                Some(ToolChoice::Required),
                None,
                None,
                Some(ToolChoice::Required),
                None
            ]
        );
    }
}

#[tokio::test]
async fn hosted_mcp_without_a_message_runs_again_and_resets_tool_choice() {
    for streamed in [false, true] {
        let call = json!({"id":"mcp", "type":"mcp_call", "server_label":"docs", "name":"search", "arguments":"{}", "output":"found", "status":"completed"});
        let (server, model) = model(vec![vec![call.clone()], vec![message()]], streamed).await;
        let (result, _) = drive(request(agent(true), model.clone()), streamed).await;
        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
        assert_eq!(result.new_items().len(), 2);
        assert!(matches!(
            result.new_items()[0].kind(),
            RunItemKind::McpCall(_)
        ));
        assert!(matches!(
            result.new_items()[1].kind(),
            RunItemKind::Message(_)
        ));
        let usage = result
            .state()
            .tool_use()
            .agent(&AgentId::new("worker"))
            .unwrap();
        assert_eq!(usage.run_calls(), 1);
        assert_eq!(
            usage.entries()[0].identity(),
            &ToolUse::Mcp {
                server: "docs".into(),
                tool_name: "search".into()
            }
        );
        let bodies = bodies(&server).await;
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["input"], json!([call]));
        assert_eq!(
            *model.choices.lock().unwrap(),
            vec![Some(ToolChoice::Required), None]
        );
    }
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
#[tokio::test]
async fn unknown_and_input_only_output_warn_once_and_are_skipped_in_stream_and_history() {
    for streamed in [false, true] {
        let capture = Capture(Arc::new(Mutex::new(vec![])));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let raw = hosted_items().remove(1);
        let mut unknown = input_only_items();
        unknown.extend([
            json!({"type":"future_tool_event", "id":"unknown-one"}),
            json!({"type":"future_tool_event", "id":"unknown-two"}),
        ]);
        let mut output = vec![raw.clone()];
        output.extend(unknown.clone());
        output.push(message());
        let (server, model) = model(vec![output, vec![message()]], streamed).await;
        let session = Arc::new(InMemorySession::new("unknown-items"));
        let (result, events) = drive(
            request(agent(true), model.clone()).with_session(session.clone()),
            streamed,
        )
        .await;
        assert_eq!(result.new_items().len(), 2);
        if streamed {
            assert_eq!(events.len(), 2);
        }
        let stored = session.get_items(None).await.unwrap();
        model
            .get_response(ModelRequest::new(
                stored.iter().filter_map(RunItem::to_model_input).collect(),
                SettingsLayers::new()
                    .with_run(&settings())
                    .resolve(&ProviderKey::new("openai")),
            ))
            .await
            .unwrap();
        let bodies = bodies(&server).await;
        assert_eq!(bodies[1]["input"][0], replay(&raw));
        assert_eq!(bodies[1]["input"].as_array().unwrap().len(), 2);
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("WARN"), "{logs}");
        assert_eq!(
            logs.matches("Unexpected output type, ignoring").count(),
            unknown.len(),
            "{logs}"
        );
        for item in unknown {
            assert!(logs.contains(item["type"].as_str().unwrap()), "{logs}");
        }
    }
}

async fn output_error(item: &Value, streamed: bool) -> Error {
    let (_server, model) = model(vec![vec![item.clone(), message()]], streamed).await;
    let request = ModelRequest::new(
        vec![],
        SettingsLayers::new().resolve(&ProviderKey::new("openai")),
    );
    if streamed {
        let mut stream = model.stream_response(request);
        while let Some(event) = stream.next().await {
            match event {
                Err(error) => return error,
                Ok(ModelStreamEvent::RunItem(_) | ModelStreamEvent::Completed(_)) => {
                    panic!("invalid item was accepted")
                }
                _ => {}
            }
        }
        panic!("invalid item must fail")
    } else {
        model.get_response(request).await.unwrap_err()
    }
}

#[tokio::test]
async fn known_actions_approvals_and_program_items_fail_explicitly() {
    let mut items: Vec<_> = [
        "computer_call",
        "mcp_approval_request",
        "local_shell_call",
        "shell_call",
        "shell_call_output",
        "apply_patch_call",
        "program",
        "program_output",
    ]
    .into_iter()
    .map(|kind| json!({"type":kind, "id":"unsupported"}))
    .collect();
    for kind in [
        "function_call",
        "web_search_call",
        "tool_search_call",
        "future_tool_event",
    ] {
        items.push(json!({"type":kind, "id":"owned", "name":"tool_search", "call_id":"owned", "arguments":"{}", "caller":{"type":"program", "id":"parent"}}));
    }
    for streamed in [false, true] {
        for item in &items {
            let error = output_error(item, streamed).await;
            assert!(
                error.to_string().contains("not supported"),
                "{item}: {error}"
            );
        }
    }
}

#[tokio::test]
async fn hosted_and_server_search_items_enforce_the_upstream_caller_contract() {
    for streamed in [false, true] {
        let items = hosted_items().into_iter().chain(search_items()).chain([
            json!({"id":"mcp", "type":"mcp_call", "server_label":"docs", "name":"search", "arguments":"{}", "output":"found", "status":"completed"}),
            json!({"id":"directory", "type":"mcp_list_tools", "server_label":"docs", "tools":[]}),
        ]);
        for raw in items {
            for caller in [Value::Null, json!({"type":"direct"})] {
                let mut item = raw.clone();
                item["caller"] = caller;
                let (_server, model) = model(vec![vec![item.clone(), message()]], streamed).await;
                let (result, _) = drive(request(agent(true), model), streamed).await;
                assert_eq!(
                    result.new_items()[0].raw_provider_item().unwrap().payload(),
                    &item
                );
            }
            for caller in [
                json!({"type":"automation"}),
                json!({}),
                json!({"type":null}),
                json!("direct"),
                json!([]),
            ] {
                let mut item = raw.clone();
                item["caller"] = caller;
                let error = output_error(&item, streamed).await;
                assert!(
                    error.to_string().contains("with unsupported caller type"),
                    "{item}: {error}"
                );
            }
        }
    }
}
