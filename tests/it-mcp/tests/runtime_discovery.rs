//! MCP discovery uses the real runner context before every ordinary or streamed model call.

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    error::{Error, Result},
    item::{CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    permission::{PermissionDecision, PermissionRule},
    state::RunId,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
    usage::RequestUsage,
};
use ra_mcp::{
    McpServer,
    client::{
        McpClient, McpClientOptions,
        stdio::{McpServerStdio, McpStdioParams},
    },
    filter::ToolFilter,
    in_process::InProcessToolServer,
    resolver::{McpClientSource, McpToolResolver},
    tool::{McpDiscoveryOptions, McpToolNaming},
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
    turn::prepare::ToolNameCollisionPolicy,
};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct AppState {
    stage: AtomicUsize,
}
#[derive(Default)]
struct Observations {
    listings: AtomicUsize,
    calls: Mutex<Vec<String>>,
    contexts: Mutex<Vec<(String, String, usize)>>,
    input_tokens: Mutex<Vec<u64>>,
    reject_listing: AtomicBool,
}
struct Catalog {
    app: Arc<AppState>,
    observations: Arc<Observations>,
}
fn declaration(name: &str) -> rmcp::model::Tool {
    serde_json::from_value(json!({"name":name, "description":"Runtime discovery fixture", "inputSchema":{"type":"object","properties":{}}})).unwrap()
}
impl ServerHandler for Catalog {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        self.observations.listings.fetch_add(1, Ordering::SeqCst);
        if self.observations.reject_listing.load(Ordering::SeqCst) {
            return Err(ErrorData::invalid_params("catalog unavailable", None));
        }
        Ok(ListToolsResult {
            tools: vec![
                declaration("first"),
                declaration("second"),
                declaration("broken"),
            ],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        self.observations
            .calls
            .lock()
            .unwrap()
            .push(request.name.to_string());
        self.app.stage.fetch_add(1, Ordering::SeqCst);
        Ok(
            CallToolResult::success(vec![ContentBlock::text(format!("called {}", request.name))])
                .into(),
        )
    }
}
struct ScriptedModel {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    before_response: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}
impl ScriptedModel {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
            before_response: None,
        })
    }
    fn next(&self, request: ModelRequest) -> Result<ModelResponse> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push(request);
            index
        };
        if let Some(before_response) = &self.before_response {
            before_response(index);
        }
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("model script exhausted"))
    }
    fn names(&self, index: usize) -> Vec<String> {
        self.requests.lock().unwrap()[index]
            .tools()
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }
}
#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.next(request)
    }
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next(request)
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}
struct Resolver(Arc<ScriptedModel>);
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(ProviderKey::new("test"), None, ApiProtocol::OpenAiResponses),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}
fn call(id: &str, name: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, json!({}))),
    )])
    .with_usage(RequestUsage::new(7, 3))
}
fn final_response() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("final"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )])
}
async fn drive(request: RunRequest, streamed: bool) -> Result<RunResult> {
    tokio::time::timeout(Duration::from_secs(5), async {
        if streamed {
            let mut stream = Runner::run_streamed(request);
            while stream.next_event().await.is_some() {}
            stream.finish().await
        } else {
            Runner::run(request).await
        }
    })
    .await
    .unwrap()
}
fn request(
    agent: AgentBinding,
    model: Arc<ScriptedModel>,
    app: Arc<AppState>,
    run_id: &str,
) -> RunRequest {
    RunRequest::new(
        agent,
        Arc::new(Resolver(model)),
        RunId::new(run_id),
        CancelScope::root(),
        vec![],
    )
    .with_app_context(app)
}
fn tool_resolver(client: McpClient, options: McpDiscoveryOptions) -> Arc<McpToolResolver> {
    Arc::new(McpToolResolver::new(
        vec![(Arc::new(client), ToolFilter::default())],
        options,
    ))
}
fn contextual_filter(observations: Arc<Observations>) -> ToolFilter {
    ToolFilter::dynamic(move |context, tool| {
        let observations = observations.clone();
        Box::pin(async move {
            tokio::task::yield_now().await;
            assert!(std::ptr::eq(context.agent(), context.run_context().agent()));
            let stage = context
                .run_context()
                .app_context::<AppState>()
                .unwrap()
                .stage
                .load(Ordering::SeqCst);
            observations.contexts.lock().unwrap().push((
                context.run_context().run_id().as_str().to_owned(),
                context.agent().id().as_str().to_owned(),
                stage,
            ));
            observations
                .input_tokens
                .lock()
                .unwrap()
                .push(context.run_context().usage_totals().input_tokens());
            if tool.name == "broken" {
                return Err(Error::caller("private predicate failure"));
            }
            Ok(tool.name == if stage % 2 == 0 { "first" } else { "second" })
        })
    })
}

#[tokio::test]
async fn every_turn_filters_with_current_run_state_and_public_agent_in_both_runners() {
    for streamed in [false, true] {
        for cache in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let mut server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: observations.clone(),
                },
                McpClientOptions::default()
                    .with_cache_tools_list(cache)
                    .with_tool_filter(contextual_filter(observations.clone())),
            )
            .await
            .unwrap();
            let public = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public agent")
                .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
                .build()
                .unwrap();
            let execution = public
                .to_builder()
                .id(AgentId::new("execution"))
                .name("Private execution")
                .build()
                .unwrap();
            let model = ScriptedModel::new(vec![
                call("one", "first"),
                call("two", "second"),
                final_response(),
            ]);
            let result = drive(
                request(
                    AgentBinding::prepared(public, execution),
                    model.clone(),
                    app,
                    "actual-run",
                ),
                streamed,
            )
            .await
            .unwrap();
            assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(model.names(0), ["first"]);
            assert_eq!(model.names(1), ["second"]);
            assert_eq!(model.names(2), ["first"]);
            assert_eq!(*observations.calls.lock().unwrap(), ["first", "second"]);
            assert_eq!(
                observations.listings.load(Ordering::SeqCst),
                if cache { 1 } else { 3 }
            );
            let contexts = observations.contexts.lock().unwrap().clone();
            assert_eq!(contexts.len(), 9);
            for (index, (run_id, agent_id, stage)) in contexts.iter().enumerate() {
                assert_eq!(run_id, "actual-run");
                assert_eq!(agent_id, "public");
                assert_eq!(*stage, index / 3);
            }
            assert_eq!(
                *observations.input_tokens.lock().unwrap(),
                [0, 0, 0, 7, 7, 7, 14, 14, 14]
            );
            assert_eq!(server.client().cached_tools().unwrap().len(), 3);
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn model_and_dispatch_share_the_inventory_resolved_before_the_call() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default()
                .with_cache_tools_list(true)
                .with_tool_filter(contextual_filter(observations.clone())),
        )
        .await
        .unwrap();
        let client = server.client();
        let model = Arc::new(ScriptedModel {
            responses: Mutex::new(vec![call("one", "first"), final_response()].into()),
            requests: Mutex::new(vec![]),
            before_response: Some(Arc::new(move |index| {
                if index == 0 {
                    client.set_tool_filter(ToolFilter::new(Some(HashSet::new()), HashSet::new()));
                }
            })),
        });
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        drive(
            request(agent.into(), model.clone(), app, "actual-run"),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(model.names(0), ["first"]);
        assert!(model.names(1).is_empty());
        assert_eq!(*observations.calls.lock().unwrap(), ["first"]);
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn handoff_resolves_the_target_inventory_with_the_target_public_identity() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let observed = observations.clone();
        let filter = ToolFilter::dynamic_sync(move |context, tool| {
            assert_eq!(context.run_context().run_id().as_str(), "handoff-run");
            observed.contexts.lock().unwrap().push((
                "handoff-run".into(),
                context.agent().id().as_str().into(),
                0,
            ));
            Ok(tool.name
                == if context.agent().id().as_str() == "source" {
                    "first"
                } else {
                    "second"
                })
        });
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default()
                .with_cache_tools_list(true)
                .with_tool_filter(filter),
        )
        .await
        .unwrap();
        let resolver = tool_resolver(server.client(), McpDiscoveryOptions::new());
        let target = AgentSpec::builder()
            .id(AgentId::new("target"))
            .name("Target")
            .tool_resolver(resolver.clone())
            .build()
            .unwrap();
        let handoff = target.as_handoff().unwrap();
        let model = ScriptedModel::new(vec![
            call("transfer", handoff.schema().name()),
            call("target-call", "second"),
            final_response(),
        ]);
        let source = AgentSpec::builder()
            .id(AgentId::new("source"))
            .name("Source")
            .handoff(handoff)
            .tool_resolver(resolver)
            .build()
            .unwrap();
        let registry = AgentRegistry::builder()
            .register(source.clone())
            .register(target)
            .build()
            .unwrap();
        drive(
            request(source.into(), model.clone(), app, "handoff-run")
                .with_config(RunConfig::new().with_agent_registry(registry)),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(model.names(0), ["first"]);
        assert_eq!(model.names(1), ["second"]);
        assert_eq!(model.names(2), ["second"]);
        assert_eq!(*observations.calls.lock().unwrap(), ["second"]);
        let contexts = observations.contexts.lock().unwrap().clone();
        assert!(contexts[..3].iter().all(|(_, agent, _)| agent == "source"));
        assert!(contexts[3..].iter().all(|(_, agent, _)| agent == "target"));
        assert_eq!(observations.listings.load(Ordering::SeqCst), 1);
        server.cleanup().await.unwrap();
    }
}

struct LocalTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

struct ReconnectTool {
    tool: Arc<LocalTool>,
    server: Arc<dyn McpServer>,
}
#[async_trait]
impl Tool for ReconnectTool {
    fn origin(&self) -> &ToolOrigin {
        self.tool.origin()
    }
    fn schema(&self) -> &ToolSchema {
        self.tool.schema()
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        self.server.cleanup().await?;
        self.server.connect().await?;
        Ok(ToolOutput::text("reconnected"))
    }
}

#[tokio::test]
async fn resolver_follows_configured_server_reconnects_on_the_next_turn_and_run() {
    for streamed in [false, true] {
        for cache_tools_list in [false, true] {
            let app = Arc::new(AppState::default());
            let server = Arc::new(
                McpServerStdio::new(McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")))
                    .with_name("docs")
                    .with_options(
                        McpClientOptions::default().with_cache_tools_list(cache_tools_list),
                    ),
            );
            let resolver = Arc::new(McpToolResolver::new(
                vec![(server.clone(), ToolFilter::default())],
                McpDiscoveryOptions::new(),
            ));
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool(Arc::new(ReconnectTool {
                    tool: LocalTool::new("reconnect"),
                    server: server.clone(),
                }))
                .tool_resolver(resolver)
                .build()
                .unwrap();
            server.connect().await.unwrap();
            for run_id in ["first-run", "second-run"] {
                let old_client = server.client().unwrap();
                let model = ScriptedModel::new(vec![
                    call("reconnect", "reconnect"),
                    call("search", "search"),
                    final_response(),
                ]);
                let result = drive(
                    request(agent.clone().into(), model.clone(), app.clone(), run_id),
                    streamed,
                )
                .await
                .unwrap();
                assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
                assert_eq!(model.names(0), model.names(1));
                assert_eq!(model.names(1), model.names(2));
                assert!(model.names(1).iter().any(|name| name == "search"));
                assert!(!old_client.is_connected().await);
                assert!(old_client.list_tools().await.is_err());
                assert!(server.client().unwrap().is_connected().await);
                server.cleanup().await.unwrap();
                server.connect().await.unwrap();
            }
            server.cleanup().await.unwrap();
        }
    }
}

struct ObservedTool {
    tool: Arc<LocalTool>,
    order: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl Tool for ObservedTool {
    fn origin(&self) -> &ToolOrigin {
        self.tool.origin()
    }
    fn schema(&self) -> &ToolSchema {
        self.tool.schema()
    }
    fn options(&self) -> ra_core::tool::ToolOptions {
        ra_core::tool::ToolOptions::new()
            .with_availability(ra_core::tool::ToolAvailability::Dynamic)
    }
    async fn is_enabled(&self, _: &ra_core::context::RunContext) -> Result<bool> {
        self.order.lock().unwrap().push("local");
        Ok(true)
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("local"))
    }
}

#[tokio::test]
async fn preparation_matches_normal_and_resumed_upstream_handoff_order() {
    for streamed in [false, true] {
        for naming in [McpToolNaming::Original, McpToolNaming::ServerPrefixed] {
            let order = Arc::new(Mutex::new(Vec::new()));
            let app = Arc::new(AppState::default());
            let filter_order = order.clone();
            let mut server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: Arc::default(),
                },
                McpClientOptions::default().with_tool_filter(ToolFilter::dynamic_sync(
                    move |_, tool| {
                        if tool.name == "first" {
                            filter_order.lock().unwrap().push("mcp");
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    },
                )),
            )
            .await
            .unwrap();
            let target = AgentSpec::builder()
                .id(AgentId::new("target"))
                .name("Target")
                .build()
                .unwrap();
            let handoff_order = order.clone();
            let handoff = target.as_handoff().unwrap().with_availability_fn(move |_| {
                handoff_order.lock().unwrap().push("handoff");
                std::future::ready(Ok(true))
            });
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool(Arc::new(ObservedTool {
                    tool: LocalTool::new("local"),
                    order: order.clone(),
                }))
                .handoff(handoff)
                .tool_resolver(tool_resolver(
                    server.client(),
                    McpDiscoveryOptions::new().with_naming(naming),
                ))
                .build()
                .unwrap();
            let registry = AgentRegistry::builder()
                .register(agent.clone())
                .register(target)
                .build()
                .unwrap();
            let name = if naming == McpToolNaming::ServerPrefixed {
                "mcp_docs__first"
            } else {
                "first"
            };
            let config = RunConfig::new()
                .with_agent_registry(registry)
                .with_permission_rules([
                    PermissionRule::new(PermissionDecision::Ask).with_tool_name(name)
                ]);
            let normal_order = if naming == McpToolNaming::ServerPrefixed {
                vec!["handoff", "mcp", "local", "handoff"]
            } else {
                vec!["mcp", "local", "handoff"]
            };
            let paused = drive(
                request(
                    agent.clone().into(),
                    ScriptedModel::new(vec![call("approval", name)]),
                    app.clone(),
                    "order-run",
                )
                .with_config(config.clone()),
                streamed,
            )
            .await
            .unwrap();
            assert_eq!(*order.lock().unwrap(), normal_order);
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected approval");
            };
            let mut state = paused.state().clone();
            state.approve(&items[0], false).unwrap();
            order.lock().unwrap().clear();
            drive(
                request(
                    agent.into(),
                    ScriptedModel::new(vec![final_response()]),
                    app,
                    "order-run",
                )
                .with_config(config)
                .with_state(state),
                streamed,
            )
            .await
            .unwrap();
            let mut expected = vec!["handoff", "mcp", "local"];
            expected.extend(normal_order);
            assert_eq!(*order.lock().unwrap(), expected);
            server.cleanup().await.unwrap();
        }
    }
}
impl LocalTool {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::loose(name, json!({"type":"object","properties":{}})).unwrap(),
        })
    }
}
#[async_trait]
impl Tool for LocalTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("local"))
    }
}

#[tokio::test]
async fn prefixed_discovery_reserves_local_tools_and_enabled_handoffs_before_freezing() {
    let app = Arc::new(AppState::default());
    let observations = Arc::new(Observations::default());
    let mut server = InProcessToolServer::connect(
        "docs",
        Catalog {
            app: app.clone(),
            observations: observations.clone(),
        },
        McpClientOptions::default().with_tool_filter(contextual_filter(observations)),
    )
    .await
    .unwrap();
    let target = AgentSpec::builder()
        .id(AgentId::new("target"))
        .name("Target")
        .build()
        .unwrap();
    let handoff = target
        .as_handoff()
        .unwrap()
        .with_tool_name("mcp_docs__first")
        .unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("source"))
        .name("Source")
        .tool(LocalTool::new("mcp_docs__first"))
        .handoff(handoff)
        .tool_resolver(tool_resolver(
            server.client(),
            McpDiscoveryOptions::new().with_naming(McpToolNaming::ServerPrefixed),
        ))
        .build()
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(agent.clone())
        .register(target)
        .build()
        .unwrap();
    let model = ScriptedModel::new(vec![final_response()]);
    drive(
        request(agent.into(), model.clone(), app, "prefix-run")
            .with_config(RunConfig::new().with_agent_registry(registry)),
        false,
    )
    .await
    .unwrap();
    let names = model.names(0);
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with("mcp_docs__first_"));
    assert_eq!(
        model.requests.lock().unwrap()[0].handoffs()[0].name(),
        "mcp_docs__first"
    );
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn inventory_failures_stop_both_runners_before_the_model_call() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        observations.reject_listing.store(true, Ordering::SeqCst);
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("source"))
            .name("Source")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![final_response()]);
        let error = drive(
            request(agent.into(), model.clone(), app, "failed-run"),
            streamed,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("catalog unavailable"));
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(observations.calls.lock().unwrap().is_empty());
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_contextual_discovery_stops_before_the_model_call() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let entered_filter = entered.clone();
        let filter = ToolFilter::dynamic(move |context, _| {
            let entered = entered_filter.clone();
            Box::pin(async move {
                assert_eq!(context.run_context().run_id().as_str(), "cancelled-run");
                entered.notify_one();
                std::future::pending::<Result<bool>>().await
            })
        });
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default().with_tool_filter(filter),
        )
        .await
        .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("source"))
            .name("Source")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![final_response()]);
        let cancel = CancelScope::root();
        let request = RunRequest::new(
            agent.into(),
            Arc::new(Resolver(model.clone())),
            RunId::new("cancelled-run"),
            cancel.clone(),
            vec![],
        )
        .with_app_context(app);
        let task = tokio::spawn(drive(request, streamed));
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        cancel.cancel(CancelReason::UserInterrupt);
        let _result = task.await.unwrap();
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(observations.calls.lock().unwrap().is_empty());
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn a_prepared_execution_can_remove_the_public_agents_resolver() {
    let app = Arc::new(AppState::default());
    let observations = Arc::new(Observations::default());
    let mut server = InProcessToolServer::connect(
        "docs",
        Catalog {
            app: app.clone(),
            observations: observations.clone(),
        },
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    let public = AgentSpec::builder()
        .id(AgentId::new("public"))
        .name("Public")
        .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
        .build()
        .unwrap();
    let execution = public.to_builder().clear_tool_resolver().build().unwrap();
    let model = ScriptedModel::new(vec![final_response()]);
    drive(
        request(
            AgentBinding::prepared(public, execution),
            model.clone(),
            app,
            "prepared-run",
        ),
        false,
    )
    .await
    .unwrap();
    assert!(model.names(0).is_empty());
    assert_eq!(observations.listings.load(Ordering::SeqCst), 0);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn resumed_approval_resolves_the_current_inventory_without_retargeting_a_missing_tool() {
    for streamed in [false, true] {
        for remains_available in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let mut server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: observations.clone(),
                },
                McpClientOptions::default()
                    .with_cache_tools_list(true)
                    .with_tool_filter(contextual_filter(observations.clone())),
            )
            .await
            .unwrap();
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
                .build()
                .unwrap();
            let config = RunConfig::new()
                .with_permission_rules([
                    PermissionRule::new(PermissionDecision::Ask).with_tool_name("first")
                ]);
            let first_model = ScriptedModel::new(vec![call("approval", "first")]);
            let paused = drive(
                request(
                    agent.clone().into(),
                    first_model,
                    app.clone(),
                    "approval-run",
                )
                .with_config(config.clone()),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected approval");
            };
            assert!(observations.calls.lock().unwrap().is_empty());
            let mut state: ra_core::state::RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            let RunItemKind::ToolApproval(approval) = items[0].kind() else {
                panic!("expected tool approval");
            };
            assert_eq!(
                approval.recipient_binding(),
                Some(&json!(["docs", "first", 0]))
            );
            state.approve(&items[0], false).unwrap();
            if !remains_available {
                server
                    .client()
                    .set_tool_filter(ToolFilter::new(Some(HashSet::new()), HashSet::new()));
            }
            let model = ScriptedModel::new(vec![final_response()]);
            let result = drive(
                request(agent.into(), model.clone(), app, "approval-run")
                    .with_config(config)
                    .with_state(state),
                streamed,
            )
            .await;
            if remains_available {
                assert!(matches!(
                    result.unwrap().outcome(),
                    RunOutcome::Completed { .. }
                ));
                assert_eq!(*observations.calls.lock().unwrap(), ["first"]);
                assert_eq!(model.names(0), ["second"]);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("no longer provides")
                );
                assert!(observations.calls.lock().unwrap().is_empty());
                assert!(model.requests.lock().unwrap().is_empty());
            }
            assert_eq!(observations.listings.load(Ordering::SeqCst), 1);
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn shared_agent_filters_each_concurrent_runs_own_context() {
    for streamed in [false, true] {
        let first_app = Arc::new(AppState::default());
        let second_app = Arc::new(AppState::default());
        second_app.stage.store(1, Ordering::SeqCst);
        let observations = Arc::new(Observations::default());
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: first_app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default()
                .with_cache_tools_list(true)
                .with_tool_filter(contextual_filter(observations.clone())),
        )
        .await
        .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("shared"))
            .name("Shared")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let first_model = ScriptedModel::new(vec![final_response()]);
        let second_model = ScriptedModel::new(vec![final_response()]);
        let (first, second) = tokio::join!(
            drive(
                request(
                    agent.clone().into(),
                    first_model.clone(),
                    first_app,
                    "first-run"
                ),
                streamed
            ),
            drive(
                request(agent.into(), second_model.clone(), second_app, "second-run"),
                streamed
            ),
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(first_model.names(0), ["first"]);
        assert_eq!(second_model.names(0), ["second"]);
        let contexts = observations.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 6);
        assert!(
            contexts
                .iter()
                .all(|(run_id, agent, stage)| agent == "shared"
                    && (*stage == usize::from(run_id == "second-run")))
        );
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn dynamic_inventory_collisions_follow_the_configured_policy_before_the_model_call() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default().with_tool_filter(contextual_filter(observations)),
        )
        .await
        .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool(LocalTool::new("first"))
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![final_response()]);
        let error = drive(
            request(agent.into(), model.clone(), app, "collision-run").with_config(
                RunConfig::new().with_tool_name_collision_policy(ToolNameCollisionPolicy::Error),
            ),
            streamed,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("more than once"));
        assert!(model.requests.lock().unwrap().is_empty());
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn serialized_approval_cannot_switch_to_another_server_or_a_local_replacement() {
    for streamed in [false, true] {
        for local_replacement in [false, true] {
            let app = Arc::new(AppState::default());
            let first_observations = Arc::new(Observations::default());
            let second_observations = Arc::new(Observations::default());
            let mut first_server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: first_observations.clone(),
                },
                McpClientOptions::default().with_cache_tools_list(true),
            )
            .await
            .unwrap();
            let mut second_server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: second_observations.clone(),
                },
                McpClientOptions::default().with_cache_tools_list(true),
            )
            .await
            .unwrap();
            let servers = [first_server.client(), second_server.client()]
                .into_iter()
                .enumerate()
                .map(|(index, client)| {
                    (
                        Arc::new(client) as Arc<dyn McpClientSource>,
                        ToolFilter::dynamic_sync(move |context, tool| {
                            Ok(tool.name == "first"
                                && context
                                    .run_context()
                                    .app_context::<AppState>()
                                    .unwrap()
                                    .stage
                                    .load(Ordering::SeqCst)
                                    == index)
                        }),
                    )
                })
                .collect();
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(Arc::new(McpToolResolver::new(
                    servers,
                    McpDiscoveryOptions::new(),
                )))
                .build()
                .unwrap();
            let config = RunConfig::new()
                .with_permission_rules([
                    PermissionRule::new(PermissionDecision::Ask).with_tool_name("first")
                ]);
            let paused = drive(
                request(
                    agent.clone().into(),
                    ScriptedModel::new(vec![call("approval", "first")]),
                    app.clone(),
                    "recipient-run",
                )
                .with_config(config.clone()),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected approval");
            };
            let mut state: ra_core::state::RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            state.approve(&items[0], false).unwrap();
            app.stage.store(1, Ordering::SeqCst);
            let agent = if local_replacement {
                agent
                    .to_builder()
                    .clear_tool_resolver()
                    .tool(LocalTool::new("first"))
                    .build()
                    .unwrap()
            } else {
                agent
            };
            let model = ScriptedModel::new(vec![final_response()]);
            let error = drive(
                request(agent.into(), model.clone(), app, "recipient-run")
                    .with_config(config)
                    .with_state(state),
                streamed,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("different recipient binding"));
            assert!(model.requests.lock().unwrap().is_empty());
            assert!(first_observations.calls.lock().unwrap().is_empty());
            assert!(second_observations.calls.lock().unwrap().is_empty());
            first_server.cleanup().await.unwrap();
            second_server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn approvals_from_one_interrupted_turn_share_one_rediscovered_inventory() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default()
                .with_cache_tools_list(true)
                .with_tool_filter(contextual_filter(observations.clone())),
        )
        .await
        .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let config = RunConfig::new()
            .with_permission_rules([
                PermissionRule::new(PermissionDecision::Ask).with_tool_name("first")
            ]);
        let mut calls = call("one", "first").output().to_vec();
        calls.extend(call("two", "first").output().iter().cloned());
        let model = ScriptedModel::new(vec![ModelResponse::new(calls), final_response()]);
        let paused = drive(
            request(
                agent.clone().into(),
                model.clone(),
                app.clone(),
                "batch-run",
            )
            .with_config(config.clone()),
            streamed,
        )
        .await
        .unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approvals");
        };
        assert_eq!(items.len(), 2);
        let mut state = paused.state().clone();
        for approval in items {
            state.approve(approval, false).unwrap();
        }
        drive(
            request(agent.into(), model.clone(), app, "batch-run")
                .with_config(config)
                .with_state(state),
            streamed,
        )
        .await
        .unwrap();
        assert_eq!(*observations.calls.lock().unwrap(), ["first", "first"]);
        assert_eq!(observations.contexts.lock().unwrap().len(), 9);
        assert_eq!(model.names(1), ["first"]);
        server.cleanup().await.unwrap();
    }
}
