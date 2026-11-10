//! Executor cleanup on the default factory, from real client-selected roots and disk manifests.
use super::*;
use ra_coding::mcp::{CodingMcpRuntime, McpServerConfig};
use ra_mcp::rmcp::{self, RoleServer, ServerHandler, model::*, service::RequestContext};
use ra_mcp::{
    McpServer,
    client::{McpClient, McpClientOptions},
    in_process::InProcessToolServer,
    resolver::McpClientSource,
    tool::McpDiscoveryOptions,
};

struct CleanupHandler(Arc<Mutex<Vec<Value>>>, bool);
impl ServerHandler for CleanupHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, rmcp::ErrorData> {
        if self.1 {
            return Ok(ListToolsResult { tools: vec![serde_json::from_value(json!({
                "name":"browser.turn_ended", "title":"Browser_Cleanup",
                "inputSchema":{"type":"object","properties":{}},
                "_meta":{"connector_id":" connector_openai_browser ","connector_name":"Browser",
                    "_codex_apps":{"resource_uri":"browser://selected-session"},"ui":{"visibility":["app"]}}
            })).unwrap()], ..Default::default() });
        }
        Ok(ListToolsResult {
            tools: vec![
                serde_json::from_value(
                    json!({"name":"turn_ended", "inputSchema":{"type":"object","properties":{}}}),
                )
                .unwrap(),
            ],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
        let call = if self.1 {
            json!({"arguments":request.arguments, "meta":context.meta})
        } else {
            serde_json::to_value(request.arguments).unwrap()
        };
        self.0.lock().unwrap().push(call);
        Ok(CallToolResult::success(vec![ContentBlock::text("{}")]).into())
    }
}

struct CleanupServer {
    apps: bool,
    calls: Arc<Mutex<Vec<Value>>>,
    client: Mutex<Option<McpClient>>,
    session: tokio::sync::Mutex<Option<InProcessToolServer>>,
}
impl McpClientSource for CleanupServer {
    fn current_client(&self) -> Result<McpClient> {
        self.client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::caller("not connected"))
    }
}
#[async_trait]
impl McpServer for CleanupServer {
    fn name(&self) -> &str {
        if self.apps { "codex_apps" } else { "node_repl" }
    }
    async fn connect(&self) -> Result<()> {
        let session = InProcessToolServer::connect(
            self.name(),
            CleanupHandler(Arc::clone(&self.calls), self.apps),
            McpClientOptions::default(),
        )
        .await?;
        *self.client.lock().unwrap() = Some(session.client());
        *self.session.lock().await = Some(session);
        Ok(())
    }
    async fn cleanup(&self) -> Result<()> {
        self.client.lock().unwrap().take();
        if let Some(mut session) = self.session.lock().await.take() {
            session.cleanup().await?;
        }
        Ok(())
    }
    async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>> {
        self.current_client()?.list_tools().await
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Option<Value>,
        meta: Option<JsonObject>,
    ) -> Result<CallToolResult> {
        self.current_client()?.call_tool(name, args, meta).await
    }
    async fn list_prompts(&self) -> Result<ListPromptsResult> {
        self.current_client()?.list_prompts().await
    }
    async fn get_prompt(&self, name: &str, args: Option<JsonObject>) -> Result<GetPromptResult> {
        self.current_client()?.get_prompt(name, args).await
    }
}

struct ExecutorFixture {
    home: HooksHome,
    mcp: Arc<CodingMcpRuntime>,
    calls: Arc<Mutex<Vec<Value>>>,
}
impl ExecutorFixture {
    async fn new() -> Self {
        Self::with_apps(false).await
    }

    async fn with_apps(apps: bool) -> Self {
        let home = HooksHome::new();
        home.write_hooks("[features]\ncodex_hooks = true\nexecutor_capability_discovery = true");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server = Arc::new(CleanupServer {
            apps,
            calls: Arc::clone(&calls),
            client: Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
        });
        let mcp = CodingMcpRuntime::new(
            vec![McpServerConfig::new(server)],
            McpDiscoveryOptions::new(),
        )
        .unwrap();
        mcp.reload_mcp_servers().await.unwrap();
        Self { home, mcp, calls }
    }
    async fn wait_calls(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.calls.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    fn root(&self, name: &str) -> Value {
        let root = self.home.path(name);
        std::fs::create_dir_all(root.join(".codex-plugin")).unwrap();
        let mut hooks = serde_json::Map::new();
        for event in ["Stop", "SubagentStop", "Interrupt"] {
            hooks.insert(event.into(), json!([{"hooks":[{"type":"mcp_tool","server":"node_repl","tool":"turn_ended","input":{"source":name,"event":"${hook_event_name}","turn":"${turn_id}"}}]}]));
        }
        std::fs::write(
            root.join(".codex-plugin/plugin.json"),
            json!({"name":"browser","hooks":{"hooks":hooks}}).to_string(),
        )
        .unwrap();
        json!({"id":"browser@openai-bundled","location":{"type":"environment","environmentId":"local","path":root}})
    }
    fn server(&self, capability: Option<Arc<dyn Capability>>) -> Server {
        self.server_at(self.home.workspace.path(), capability, true)
    }
    fn server_at(
        &self,
        workspace: &Path,
        capability: Option<Arc<dyn Capability>>,
        product: bool,
    ) -> Server {
        let root = workspace.canonicalize().unwrap();
        let mut host = CodingHost::open(&root)
            .unwrap()
            .with_mcp_runtime(Arc::clone(&self.mcp));
        if let Some(capability) = capability {
            host = host.with_capability(capability);
        }
        let settings = CodingThreadSettingsAdapter::new(
            &root,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap();
        let factory = CodingThreadFactory::new(
            host,
            Arc::new(Resolver(Arc::clone(&self.home.script))),
            settings,
        );
        let factory = if product {
            factory.with_product_config(self.home.config_builder())
        } else {
            factory
        };
        build(
            Options {
                store: Some(Arc::clone(&self.home.store)),
                factory: Some(Arc::new(factory)),
                unload_delay: Some(Duration::ZERO),
            },
            None,
        )
        .0
    }
    fn environment(&self) -> Value {
        json!({"environmentId":"local","cwd":self.home.workspace.path().canonicalize().unwrap()})
    }
    async fn start(
        &self,
        server: &Server,
        connection: &mut Connection,
        roots: Vec<Value>,
    ) -> String {
        server
            .call(
                connection,
                "thread/start",
                json!({"environments":[self.environment()], "selectedCapabilityRoots":roots}),
            )
            .await["thread"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[tokio::test]
async fn selected_executor_stop_and_interrupt_run_on_the_default_factory() {
    let fixture = ExecutorFixture::new().await;
    let root = fixture.root("selected");
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = fixture.start(&server, &mut connection, vec![root]).await;
    fixture.home.script.answer("done");
    server.run_turn(&mut connection, &thread, "finish").await;
    fixture.wait_calls(1).await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    assert_eq!(fixture.calls.lock().unwrap()[0]["event"], "Stop");

    let gate = Arc::new(tokio::sync::Notify::new());
    fixture
        .home
        .script
        .push(Step::After(Arc::clone(&gate), final_message("held")));
    let before = fixture.home.script.requests.lock().unwrap().len();
    let turn = server.turn_start(&mut connection, &thread, "wait").await;
    for _ in 0..500 {
        if fixture.home.script.requests.lock().unwrap().len() > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let notices = interrupt_turn(&server, &mut connection, &thread, &turn).await;
    fixture.wait_calls(2).await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    assert_eq!(fixture.calls.lock().unwrap()[1]["event"], "Interrupt");
    // Executor-scoped cleanup runs asynchronously; upstream filters its public lifecycle notices.
    assert!(notices.is_empty());
    assert_eq!(fixture.calls.lock().unwrap()[1]["turn"], turn);
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn roots_are_not_selected_implicitly_and_selections_survive_resume_and_fork() {
    let fixture = ExecutorFixture::new().await;
    let root = fixture.root("persisted");
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let empty = fixture.start(&server, &mut connection, Vec::new()).await;
    fixture.home.script.answer("empty");
    server.run_turn(&mut connection, &empty, "no roots").await;
    assert!(fixture.calls.lock().unwrap().is_empty());
    let thread = fixture.start(&server, &mut connection, vec![root]).await;
    fixture.home.script.answer("first");
    server.run_turn(&mut connection, &thread, "first").await;
    super::super::unload(&server, &mut connection, &thread).await;
    server
        .call(&mut connection, "thread/resume", json!({"threadId":thread}))
        .await;
    fixture.home.script.answer("resumed");
    server.run_turn(&mut connection, &thread, "resumed").await;
    let fork = server
        .call(&mut connection, "thread/fork", json!({"threadId":thread}))
        .await;
    let fork_id = fork["thread"]["id"].as_str().unwrap();
    fixture.home.script.answer("forked");
    server.run_turn(&mut connection, fork_id, "forked").await;
    fixture.wait_calls(3).await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 3);
    assert!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call["source"] == "persisted")
    );
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_execution_selections_are_refused_before_a_turn_runs() {
    let fixture = ExecutorFixture::new().await;
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let invalid_start = server
        .call_error(
            &mut connection,
            "thread/start",
            json!({"environments":[{"environmentId":"unknown", "cwd":"/workspace"}]}),
        )
        .await;
    assert_eq!(invalid_start["code"], -32600);
    assert!(
        invalid_start["message"]
            .as_str()
            .unwrap()
            .contains("unknown turn environment id")
    );
    let thread = fixture.start(&server, &mut connection, Vec::new()).await;
    for environments in [
        json!([]),
        json!([{"environmentId":"remote","cwd":"/workspace"}]),
        json!([{"environmentId":"local","cwd":"/different"}]),
        json!([fixture.environment(), fixture.environment()]),
    ] {
        let response = server.call_error(&mut connection, "turn/start", json!({"threadId":thread,"input":[{"type":"text","text":"never"}],"environments":environments})).await;
        assert_eq!(response["code"], -32600, "{response}");
    }
    assert!(fixture.home.script.requests.lock().unwrap().is_empty());
    fixture.mcp.shutdown().await.unwrap();
}

struct BeforeSamplingCompaction {
    entered: Arc<tokio::sync::Notify>,
    wait: bool,
}
#[async_trait]
impl Capability for BeforeSamplingCompaction {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("test_executor_compaction").unwrap()
    }
    fn context_processor(&self) -> Option<&dyn ra_core::capability::ContextProcessor> {
        Some(self)
    }
}
#[async_trait]
impl ra_core::capability::ContextProcessor for BeforeSamplingCompaction {
    async fn process_context(
        &self,
        request: ra_core::capability::ContextProcessorRequest,
        summarizer: &dyn ra_core::capability::ContextSummarizer,
    ) -> Result<ra_core::capability::ContextProcessorResult> {
        request
            .notify_pre_compact(ra_core::hook::CompactTrigger::Automatic)
            .await?;
        self.entered.notify_one();
        if self.wait {
            summarizer
                .summarize(ra_core::capability::ContextSummaryRequest::new(
                    request.input().to_vec(),
                    "test compaction summary",
                ))
                .await?;
        }
        Ok(ra_core::capability::ContextProcessorResult::new(
            request.input().to_vec(),
        ))
    }
}

#[tokio::test]
async fn interrupt_during_compaction_uses_the_selected_step_before_normal_inference() {
    let fixture = ExecutorFixture::new().await;
    let root = fixture.root("compacting");
    let entered = Arc::new(tokio::sync::Notify::new());
    let server = fixture.server(Some(Arc::new(BeforeSamplingCompaction {
        entered: Arc::clone(&entered),
        wait: true,
    })));
    let mut connection = server.connect(1).await;
    let thread = fixture.start(&server, &mut connection, vec![root]).await;
    fixture.home.script.push(Step::After(
        Arc::new(tokio::sync::Notify::new()),
        final_message("summary"),
    ));
    let turn = server.turn_start(&mut connection, &thread, "compact").await;
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.home.script.requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    interrupt_turn(&server, &mut connection, &thread, &turn).await;
    assert_eq!(
        fixture.home.script.requests.lock().unwrap().len(),
        1,
        "only the compaction summary was sampled"
    );
    fixture.wait_calls(1).await;
    assert_eq!(
        *fixture.calls.lock().unwrap(),
        [json!({"source":"compacting","event":"Interrupt","turn":turn})]
    );
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_compaction_hook_stop_still_has_executor_cleanup_from_its_selected_step() {
    let fixture = ExecutorFixture::new().await;
    fixture.home.write_hooks(&format!(
        "[features]\ncodex_hooks = true\nexecutor_capability_discovery = true\n{}",
        hook_section("PreCompact", "printf '%s' '{\"continue\":false}'")
    ));
    let root = fixture.root("stopped-compaction");
    let server = fixture.server(Some(Arc::new(BeforeSamplingCompaction {
        entered: Arc::new(tokio::sync::Notify::new()),
        wait: false,
    })));
    let mut connection = server.connect(1).await;
    let thread = fixture.start(&server, &mut connection, vec![root]).await;
    let turn = server
        .turn_start(&mut connection, &thread, "stop compacting")
        .await;
    connection.notification("turn/completed").await;
    assert!(fixture.home.script.requests.lock().unwrap().is_empty());
    fixture.wait_calls(1).await;
    assert_eq!(
        *fixture.calls.lock().unwrap(),
        [json!({"source":"stopped-compaction","event":"Interrupt","turn":turn})]
    );
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interrupted_prompt_does_not_reuse_the_previous_turns_executor_step() {
    let fixture = ExecutorFixture::new().await;
    let gate = fixture.home.path("prompt-gate");
    std::fs::write(&gate, "open").unwrap();
    fixture.home.write_hooks(&format!(
        "[features]\ncodex_hooks = true\nexecutor_capability_discovery = true\n{}",
        hook_section(
            "UserPromptSubmit",
            &format!("while [ ! -f '{}' ]; do sleep 0.01; done", gate.display())
        )
    ));
    let root = fixture.root("earlier-turn");
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = fixture.start(&server, &mut connection, vec![root]).await;
    fixture.home.script.answer("first");
    server.run_turn(&mut connection, &thread, "first").await;
    std::fs::remove_file(&gate).unwrap();
    let turn = server
        .turn_start(&mut connection, &thread, "before sampling")
        .await;
    connection.notification("hook/started").await;
    interrupt_turn(&server, &mut connection, &thread, &turn).await;
    std::fs::write(&gate, "open").unwrap();
    fixture.wait_calls(1).await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    assert_eq!(fixture.calls.lock().unwrap()[0]["event"], "Stop");
    fixture.mcp.shutdown().await.unwrap();
}

impl ExecutorFixture {
    /// Uses the production thread hook constructor and installer; tests never supply a provider
    /// or fabricate discovery results. Only the host's per-run settings differ.
    async fn run_configuration(
        &self,
    ) -> (
        ra_coding::thread_hooks::CodingThreadHooks,
        CodingThreadSettingsAdapter,
        RunConfig,
    ) {
        let root = self.home.workspace.path().canonicalize().unwrap();
        let config = self
            .home
            .config_builder()
            .harness_overrides(ConfigOverrides {
                cwd: Some(root.clone()),
                bypass_hook_trust: Some(true),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let host = CodingHost::open(&root)
            .unwrap()
            .with_mcp_runtime(Arc::clone(&self.mcp));
        let hooks = ra_coding::thread_hooks::CodingThreadHooks::build_for_test(
            &config,
            &host,
            ra_core::session::SessionId::new("executor-test-thread"),
        )
        .await
        .unwrap();
        let adapter = CodingThreadSettingsAdapter::new(
            &root,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap();
        let config = hooks.install_for_test(adapter.initialize(RunConfig::new()));
        (hooks, adapter, config)
    }
}

#[tokio::test]
async fn concurrent_nested_runs_keep_their_own_sources_and_never_replace_root_interrupt_cleanup() {
    use ra_core::{
        cancel::{CancelReason, CancelScope},
        state::RunId,
    };
    use ra_protocol::server::{ThreadSettingsAdapter, protocol::ThreadPolicyOverrides};
    use ra_runtime::{
        agent::{AgentBinding, tool::AgentAsTool},
        runner::{RunRequest, Runner},
    };
    let fixture = ExecutorFixture::new().await;
    let (_hooks, adapter, base) = fixture.run_configuration().await;
    let select = |name: &str| {
        adapter
            .prepare(
                &base,
                &ThreadPolicyOverrides::new().with_selected_capability_roots(vec![
                    serde_json::from_value(fixture.root(name)).unwrap(),
                ]),
            )
            .unwrap()
    };
    let child_a = AgentSpec::builder()
        .id(AgentId::new("child_a"))
        .name("Child A")
        .instructions("answer")
        .build()
        .unwrap();
    let child_b = AgentSpec::builder()
        .id(AgentId::new("child_b"))
        .name("Child B")
        .instructions("answer")
        .build()
        .unwrap();
    let parent = AgentSpec::builder()
        .id(AgentId::new("parent"))
        .name("Parent")
        .instructions("delegate")
        .tool(Arc::new(
            child_a
                .as_tool()
                .tool_name("child_a")
                .options(
                    ra_core::tool::ToolOptions::new()
                        .with_concurrency(ra_core::tool::ToolConcurrency::Parallel),
                )
                .run_config(select("child-a"))
                .build()
                .unwrap(),
        ))
        .tool(Arc::new(
            child_b
                .as_tool()
                .tool_name("child_b")
                .options(
                    ra_core::tool::ToolOptions::new()
                        .with_concurrency(ra_core::tool::ToolConcurrency::Parallel),
                )
                .run_config(select("child-b"))
                .build()
                .unwrap(),
        ))
        .build()
        .unwrap();
    fixture
        .home
        .script
        .push(Step::Respond(ModelResponse::new(vec![
            RunItem::new(
                ItemId::new("call-a"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("call-a"),
                    "child_a",
                    json!({"input":"a"}),
                )),
            ),
            RunItem::new(
                ItemId::new("call-b"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("call-b"),
                    "child_b",
                    json!({"input":"b"}),
                )),
            ),
        ])));
    let children = Arc::new(tokio::sync::Notify::new());
    fixture.home.script.push(Step::After(
        Arc::clone(&children),
        final_message("child result"),
    ));
    fixture.home.script.answer("other child");
    let hold_parent = Arc::new(tokio::sync::Notify::new());
    fixture
        .home
        .script
        .push(Step::After(hold_parent, final_message("parent result")));
    let cancel = CancelScope::root();
    let task = tokio::spawn(Runner::run(
        RunRequest::new(
            AgentBinding::direct(parent),
            Arc::new(Resolver(Arc::clone(&fixture.home.script))),
            RunId::new("root-run"),
            cancel.clone(),
            vec![ModelInputItem::Message(Message::user("delegate"))],
        )
        .with_config(select("root")),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.home.script.requests.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    children.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.home.script.requests.lock().unwrap().len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel(CancelReason::UserInterrupt);
    assert!(task.await.unwrap().unwrap_err().is_cancelled());
    fixture.wait_calls(3).await;
    let calls = fixture.calls.lock().unwrap().clone();
    let mut child_sources = calls
        .iter()
        .filter(|call| call["event"] == "SubagentStop")
        .map(|call| call["source"].as_str().unwrap())
        .collect::<Vec<_>>();
    child_sources.sort_unstable();
    assert_eq!(child_sources, ["child-a", "child-b"]);
    assert_eq!(calls.last().unwrap()["source"], "root");
    assert_eq!(calls.last().unwrap()["event"], "Interrupt");
    assert!(calls.iter().all(|call| call["turn"] == "root-run"));
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn later_steps_replace_their_own_snapshot_and_unexecuted_runs_have_no_sources() {
    use ra_core::{
        cancel::{CancelReason, CancelScope},
        context::RunContext,
        hook::HookEvent,
        state::RunId,
    };
    use ra_protocol::server::{ThreadSettingsAdapter, protocol::ThreadPolicyOverrides};
    use ra_runtime::{
        agent::AgentBinding,
        runner::{RunRequest, Runner},
    };
    let fixture = ExecutorFixture::new().await;
    let (hooks, adapter, base) = fixture.run_configuration().await;
    let select = |name: &str| {
        adapter
            .prepare(
                &base,
                &ThreadPolicyOverrides::new().with_selected_capability_roots(vec![
                    serde_json::from_value(fixture.root(name)).unwrap(),
                ]),
            )
            .unwrap()
    };
    let runnable = agent_with(
        "approval-agent",
        vec![declared("write", ra_core::tool::ToolApprovalPolicy::Always)],
        Vec::new(),
    );
    let execute = |config: RunConfig| {
        RunRequest::new(
            AgentBinding::direct(Arc::clone(&runnable)),
            Arc::new(Resolver(Arc::clone(&fixture.home.script))),
            RunId::new("step-run"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_config(config)
    };
    fixture
        .home
        .script
        .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new("approval-call"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("approval-call"),
                "write",
                json!({}),
            )),
        )])));
    let first = Runner::run(execute(select("first-step"))).await.unwrap();
    assert!(fixture.calls.lock().unwrap().is_empty());
    let paused_run = Arc::new(RunContext::new(
        RunId::new("step-run"),
        AgentBinding::direct(Arc::clone(&runnable)).public(),
    ));
    let reason = CancelReason::UserInterrupt;
    hooks
        .runtime()
        .user_hooks()
        .bind(
            paused_run,
            CancelScope::root(),
            ra_core::tool::ToolServices::default(),
        )
        .dispatch(HookEvent::Interrupt { reason: &reason })
        .await
        .unwrap();
    let mut restored: ra_core::state::RunState =
        serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
    let ra_runtime::runner::RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected approval");
    };
    for item in items {
        restored.approve(item, false).unwrap();
    }
    fixture.home.script.answer("second");
    Runner::run(execute(select("second-step")).with_state(restored))
        .await
        .unwrap();
    fixture.wait_calls(2).await;
    assert_eq!(fixture.calls.lock().unwrap()[0]["source"], "first-step");
    assert_eq!(fixture.calls.lock().unwrap()[0]["event"], "Interrupt");
    assert_eq!(fixture.calls.lock().unwrap()[1]["source"], "second-step");
    assert_eq!(fixture.calls.lock().unwrap()[1]["event"], "Stop");
    let run = Arc::new(RunContext::new(
        RunId::new("never-executed"),
        AgentBinding::direct(agent()).public(),
    ));
    let bound = hooks.runtime().user_hooks().bind(
        run,
        CancelScope::root(),
        ra_core::tool::ToolServices::default(),
    );
    let reason = CancelReason::UserInterrupt;
    bound
        .dispatch(HookEvent::Interrupt { reason: &reason })
        .await
        .unwrap();
    fixture.wait_calls(2).await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflicting_roots_keep_the_first_location_and_disabled_plugins_do_not_run() {
    let fixture = ExecutorFixture::new().await;
    let first = fixture.root("first-location");
    let conflict = fixture.root("ignored-location");
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = fixture
        .start(&server, &mut connection, vec![first.clone(), conflict])
        .await;
    fixture.home.script.answer("done");
    server.run_turn(&mut connection, &thread, "selected").await;
    fixture.wait_calls(1).await;
    assert_eq!(fixture.calls.lock().unwrap()[0]["source"], "first-location");

    fixture.home.write_hooks("[features]\ncodex_hooks = true\nexecutor_capability_discovery = true\n[plugins._default]\nenabled = false");
    let disabled_server = fixture.server(None);
    let mut disabled_connection = disabled_server.connect(2).await;
    let disabled_thread = fixture
        .start(&disabled_server, &mut disabled_connection, vec![first])
        .await;
    fixture.home.script.answer("disabled");
    disabled_server
        .run_turn(&mut disabled_connection, &disabled_thread, "disabled")
        .await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn apps_cleanup_uses_registered_connector_metadata_including_app_only_tools_and_policy() {
    let fixture = ExecutorFixture::with_apps(true).await;
    let mut root = fixture.root("apps-root");
    root["id"] = json!("browser@openai-curated-remote");
    let manifest_path = fixture.home.path("apps-root/.codex-plugin/plugin.json");
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    for event in ["Stop", "Interrupt", "SubagentStop"] {
        let hook = &mut manifest["hooks"]["hooks"][event][0]["hooks"][0];
        hook["server"] = json!("codex_apps");
        hook["tool"] = json!("browser.turn_ended");
        hook["input"] = json!({});
    }
    std::fs::write(manifest_path, manifest.to_string()).unwrap();
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = fixture
        .start(&server, &mut connection, vec![root.clone()])
        .await;
    fixture.home.script.answer("cleanup");
    server.run_turn(&mut connection, &thread, "cleanup").await;
    fixture.wait_calls(1).await;
    assert_eq!(
        fixture.calls.lock().unwrap()[0]["meta"]["_codex_apps"]["resource_uri"],
        "browser://selected-session"
    );
    assert_eq!(fixture.calls.lock().unwrap()[0]["meta"]["threadId"], thread);
    fixture.home.write_hooks("[features]\ncodex_hooks = true\nexecutor_capability_discovery = true\n[apps.connector_openai_browser.tools.Cleanup]\nenabled = false");
    let disabled_server = fixture.server(None);
    let mut disabled_connection = disabled_server.connect(2).await;
    let disabled_thread = fixture
        .start(&disabled_server, &mut disabled_connection, vec![root])
        .await;
    fixture.home.script.answer("disabled");
    disabled_server
        .run_turn(
            &mut disabled_connection,
            &disabled_thread,
            "disabled by normalized title",
        )
        .await;
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_local_selection_overrides_an_unsupported_default_without_silent_fallback() {
    let fixture = ExecutorFixture::new().await;
    std::fs::write(fixture.home.path("environments.toml"), "default = 'remote'\ninclude_local = true\n[[environments]]\nid = 'remote'\nurl = 'ws://127.0.0.1:1'\n").unwrap();
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let refused = server
        .call_error(&mut connection, "thread/start", json!({}))
        .await;
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("unsupported default environment")
    );
    let unsupported = server
        .call_error(
            &mut connection,
            "thread/start",
            json!({"environments":[{"environmentId":"remote","cwd":"/workspace"}]}),
        )
        .await;
    assert_eq!(unsupported["code"], -32600);
    assert!(
        unsupported["message"]
            .as_str()
            .unwrap()
            .contains("unsupported environment")
    );
    let thread = fixture
        .start(
            &server,
            &mut connection,
            vec![fixture.root("explicit-local")],
        )
        .await;
    fixture.home.script.answer("local");
    server
        .run_turn(&mut connection, &thread, "explicit local")
        .await;
    fixture.wait_calls(1).await;
    assert_eq!(fixture.calls.lock().unwrap()[0]["source"], "explicit-local");
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn turn_environment_replacement_stays_live_but_is_not_persisted() {
    let fixture = ExecutorFixture::new().await;
    let root = fixture.root("turn-environment");
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let started = server
        .call(
            &mut connection,
            "thread/start",
            json!({"selectedCapabilityRoots":[root]}),
        )
        .await;
    let thread = started["thread"]["id"].as_str().unwrap().to_owned();
    let mut environment = fixture.environment();
    let root_uri = ra_paths::PathUri::from_abs_path(
        &ra_paths::AbsolutePathBuf::from_absolute_path(
            fixture.home.workspace.path().canonicalize().unwrap(),
        )
        .unwrap(),
    );
    environment["runtimeWorkspaceRoots"] = json!([root_uri]);
    fixture.home.script.answer("updated");
    server.call(&mut connection, "turn/start", json!({"threadId":thread,"input":[{"type":"text","text":"update environment"}],"environments":[environment.clone()]})).await;
    let settings = connection.notification("thread/settings/updated").await;
    assert!(
        settings["params"]["threadSettings"]
            .get("environments")
            .is_none()
    );
    assert!(
        settings["params"]["threadSettings"]
            .get("selectedCapabilityRoots")
            .is_none()
    );
    connection.notification("turn/completed").await;
    fixture.wait_calls(1).await;
    fixture.home.script.answer("live selection");
    server
        .run_turn(&mut connection, &thread, "retain live selection")
        .await;
    fixture.wait_calls(2).await;
    super::super::unload(&server, &mut connection, &thread).await;
    server
        .call(&mut connection, "thread/resume", json!({"threadId":thread}))
        .await;
    fixture.home.script.answer("retained");
    server
        .run_turn(&mut connection, &thread, "without environment overrides")
        .await;
    fixture.wait_calls(3).await;
    let history = fixture
        .home
        .store
        .load_history(&ra_session::store::LoadThreadHistoryParams::new(
            SessionId::new(&thread),
        ))
        .await
        .unwrap();
    let restored = history
        .records()
        .iter()
        .rev()
        .find_map(|record| match record.payload() {
            Ok(ra_session::rollout::RolloutPayload::Event(event)) => match event.body() {
                HostEventBody::Unknown { family, data } if family == "threadSettingsApplied" => {
                    Some(data.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(restored.get("environments").is_none());
    assert!(restored.get("selectedCapabilityRoots").is_none());
    assert!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call["source"] == "turn-environment")
    );
    fixture.mcp.shutdown().await.unwrap();
}

/// Implicit local defaults follow the current host, while explicitly selected root paths survive.
#[tokio::test]
async fn implicit_local_environment_can_resume_and_fork_in_another_workspace() {
    let fixture = ExecutorFixture::new().await;
    let root = fixture.root("implicit-workspace-root");
    let server_a = fixture.server(None);
    let mut a = server_a.connect(1).await;
    let started = server_a
        .call(
            &mut a,
            "thread/start",
            json!({"selectedCapabilityRoots":[root.clone()]}),
        )
        .await;
    let thread = started["thread"]["id"].as_str().unwrap().to_owned();
    fixture.home.script.answer("created");
    server_a.run_turn(&mut a, &thread, "create").await;
    fixture.wait_calls(1).await;
    super::super::unload(&server_a, &mut a, &thread).await;

    let workspace_b = tempfile::tempdir().unwrap();
    let server_b = fixture.server_at(workspace_b.path(), None, true);
    let mut b = server_b.connect(2).await;
    server_b
        .call(&mut b, "thread/resume", json!({"threadId":thread}))
        .await;
    fixture.home.script.answer("resumed");
    server_b.run_turn(&mut b, &thread, "resume").await;
    let fork = server_b
        .call(&mut b, "thread/fork", json!({"threadId":thread}))
        .await;
    let fork_id = fork["thread"]["id"].as_str().unwrap();
    fixture.home.script.answer("forked");
    server_b.run_turn(&mut b, fork_id, "fork").await;
    fixture.wait_calls(3).await;
    assert!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call["source"] == "implicit-workspace-root")
    );
    let history = fixture
        .home
        .store
        .load_history(&ra_session::store::LoadThreadHistoryParams::new(
            SessionId::new(&thread),
        ))
        .await
        .unwrap();
    let settings = history
        .records()
        .iter()
        .rev()
        .find_map(|record| match record.payload() {
            Ok(ra_session::rollout::RolloutPayload::Event(event)) => match event.body() {
                HostEventBody::Unknown { family, data } if family == "threadSettingsApplied" => {
                    Some(data.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(settings.get("environments").is_none());
    assert_eq!(
        settings["cwd"],
        json!(workspace_b.path().canonicalize().unwrap())
    );
    let expected_root: ra_protocol::server::protocol::SelectedCapabilityRoot =
        serde_json::from_value(root).unwrap();
    assert!(settings.get("selectedCapabilityRoots").is_none());
    for id in [&thread, fork_id] {
        let history = fixture
            .home
            .store
            .load_history(&ra_session::store::LoadThreadHistoryParams::new(
                SessionId::new(id),
            ))
            .await
            .unwrap();
        let meta = history
            .records()
            .iter()
            .find_map(|record| match record.payload().ok()? {
                ra_session::rollout::RolloutPayload::SessionMeta(meta) => Some(meta),
                _ => None,
            })
            .unwrap();
        assert_eq!(meta.selected_capability_roots(), &[json!(expected_root)]);
    }
    fixture.mcp.shutdown().await.unwrap();
}

/// Cold resume and ordinary fork derive environments from the current host, preserving roots.
#[tokio::test]
async fn explicit_local_environment_can_resume_and_fork_in_another_workspace() {
    let fixture = ExecutorFixture::new().await;
    let root = fixture.root("explicit-workspace-root");
    let server_a = fixture.server(None);
    let mut a = server_a.connect(1).await;
    let thread = fixture.start(&server_a, &mut a, vec![root]).await;
    fixture.home.script.answer("created");
    server_a.run_turn(&mut a, &thread, "create").await;
    fixture.wait_calls(1).await;
    super::super::unload(&server_a, &mut a, &thread).await;
    let workspace_b = tempfile::tempdir().unwrap();
    let server_b = fixture.server_at(workspace_b.path(), None, true);
    let mut b = server_b.connect(2).await;
    for method in ["thread/resume", "thread/fork"] {
        let loaded = server_b
            .call(&mut b, method, json!({"threadId":thread}))
            .await;
        assert_eq!(
            loaded["cwd"],
            json!(workspace_b.path().canonicalize().unwrap())
        );
        let id = loaded["thread"]["id"].as_str().unwrap();
        fixture.home.script.answer("current workspace");
        server_b
            .run_turn(&mut b, id, "preserve selected roots")
            .await;
    }
    fixture.wait_calls(3).await;
    assert!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call["source"] == "explicit-workspace-root")
    );
    fixture.mcp.shutdown().await.unwrap();
}

/// An implicit empty product selection does not require a product runtime on a later host.
#[tokio::test]
async fn implicit_local_environment_can_resume_and_fork_without_product_runtime() {
    let fixture = ExecutorFixture::new().await;
    let server_a = fixture.server(None);
    let mut a = server_a.connect(1).await;
    let thread = start_thread(&server_a, &mut a).await;
    fixture.home.script.answer("created");
    server_a.run_turn(&mut a, &thread, "create").await;
    super::super::unload(&server_a, &mut a, &thread).await;
    let workspace_b = tempfile::tempdir().unwrap();
    let server_b = fixture.server_at(workspace_b.path(), None, false);
    let mut b = server_b.connect(2).await;
    server_b
        .call(&mut b, "thread/resume", json!({"threadId":thread}))
        .await;
    fixture.home.script.answer("resumed");
    server_b.run_turn(&mut b, &thread, "resume").await;
    let fork = server_b
        .call(&mut b, "thread/fork", json!({"threadId":thread}))
        .await;
    fixture.home.script.answer("forked");
    server_b
        .run_turn(&mut b, fork["thread"]["id"].as_str().unwrap(), "fork")
        .await;
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn environment_validation_preserves_upstream_common_error_order() {
    let fixture = ExecutorFixture::new().await;
    std::fs::write(fixture.home.path("environments.toml"), "default = 'local'\ninclude_local = true\n[[environments]]\nid = 'remote'\nurl = 'ws://127.0.0.1:1'\n").unwrap();
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let long_cwd = format!("/{}", "x".repeat(8192));
    let local = fixture.environment();
    for (environments, expected) in [
        (
            json!([{"environmentId":"remote","cwd":long_cwd}]),
            "working directory exceeds",
        ),
        (
            json!([local.clone(), {"environmentId":"local","cwd":long_cwd}]),
            "working directory exceeds",
        ),
        (
            json!([local.clone(), local]),
            "duplicate turn environment id",
        ),
        (
            json!([{"environmentId":"remote","cwd":"/workspace"}, {"environmentId":"unknown","cwd":"/workspace"}]),
            "unknown turn environment id",
        ),
    ] {
        let error = server
            .call_error(
                &mut connection,
                "thread/start",
                json!({"environments":environments}),
            )
            .await;
        assert_eq!(error["code"], -32600);
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "{error}"
        );
    }
    fixture.mcp.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_config_declared_mcp_handler_uses_the_thread_executor_and_hidden_tool() {
    let fixture = ExecutorFixture::with_apps(true).await;
    fixture.home.write_hooks(
        r#"
[features]
codex_hooks = true
[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "mcp_tool"
server = "codex_apps"
tool = "browser.turn_ended"
input = {}
"#,
    );
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    fixture.home.script.answer("mcp handler");
    server
        .run_turn(&mut connection, &thread, "call configured hook")
        .await;
    fixture.wait_calls(1).await;
    assert_eq!(fixture.calls.lock().unwrap()[0]["meta"]["threadId"], thread);
    let tools = serde_json::to_string(fixture.home.script.last_request().tools()).unwrap();
    assert!(!tools.contains("browser.turn_ended"), "{tools}");
    fixture.mcp.shutdown().await.unwrap();
}

/// Port of the MCP feedback budget case: a tool override wins over the global limit, including
/// the source's 20 percent serialization allowance, and the replacement reaches model history.
#[tokio::test]
async fn mcp_tool_output_limit_applies_to_hook_feedback() {
    for override_limit in [None, Some(100)] {
        let home = HooksHome::new();
        let script = home.path("mcp_server.py");
        std::fs::write(&script, r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message['method']
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'echo','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':name,'inputSchema':{'type':'object','properties':{}},'annotations':{'readOnlyHint':True,'destructiveHint':False,'openWorldHint':False}} for name in ['echo','feedback']]}
    elif method == 'tools/call':
        text = json.dumps({'continue':False,'stopReason':'hook feedback ' * 100}) if message['params']['name'] == 'feedback' else 'original result'
        result = {'content':[{'type':'text','text':text}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
"#).unwrap();
        let tool_policy = override_limit
            .map(|limit| format!("[mcp_servers.echo.tools.echo]\noutput_token_limit = {limit}\n"))
            .unwrap_or_default();
        home.write_hooks(&format!(
            r#"
tool_output_token_limit = 50
[features]
codex_hooks = true
[mcp_servers.echo]
command = "python3"
default_tools_approval_mode = "approve"
args = [{}]
{tool_policy}
[[hooks.PostToolUse]]
matcher = "^mcp__echo__echo$"
[[hooks.PostToolUse.hooks]]
type = "mcp_tool"
server = "echo"
tool = "feedback"
"#,
            ra_config::TomlValue::String(script.to_string_lossy().into_owned())
        ));
        let config = home.config_builder().build().await.unwrap();
        let mut catalog = ra_mcp::ResolvedMcpCatalog::builder();
        catalog.register(ra_mcp::McpServerRegistration::from_config(
            "echo".into(),
            config.mcp_servers.get()["echo"].clone(),
        ));
        let plan = ra_coding::mcp::product_mcp_servers(
            &catalog.build(),
            home.workspace.path(),
            &config.http_client_factory(),
        );
        assert!(plan.unavailable.is_empty(), "{:?}", plan.unavailable);
        let mcp = CodingMcpRuntime::new(plan.servers, McpDiscoveryOptions::new()).unwrap();
        mcp.reload_mcp_servers().await.unwrap();
        let fixture = ExecutorFixture {
            home,
            mcp,
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let server = fixture.server(None);
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        fixture
            .home
            .script
            .push(Step::Respond(tool_response("echo")));
        fixture.home.script.answer("reviewed");
        server.run_turn(&mut connection, &thread, "call echo").await;
        let output = last_tool_output(&fixture.home.script);
        let text = output["blocks"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{output}"));
        assert!(text.starts_with("hook feedback "), "{output}");
        assert!(text.contains("truncated"), "{output}");
        let range = if override_limit.is_some() {
            400..600
        } else {
            200..350
        };
        assert!(range.contains(&text.len()), "{} bytes: {text}", text.len());
        fixture.mcp.shutdown().await.unwrap();
    }
}

/// Codex truncates every tool's model-visible output, PostToolUse feedback included, with the
/// turn's policy when the tool carries no limit of its own. Resolving that budget must not touch
/// the MCP servers: a built-in tool's PostToolUse neither lists their tools nor waits for them.
#[tokio::test]
async fn builtin_tool_feedback_uses_the_turn_budget_without_listing_mcp_tools() {
    let home = HooksHome::new();
    let script = home.path("mcp_server.py");
    let methods = home.path("mcp_methods.log");
    std::fs::write(&script, format!(r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message['method']
    with open({methods:?}, 'a') as log:
        log.write(method + '\n')
    if method == 'initialize':
        result = {{'protocolVersion':'2024-11-05','capabilities':{{'tools':{{}}}},'serverInfo':{{'name':'echo','version':'1'}}}}
    elif method == 'tools/list':
        result = {{'tools':[{{'name':'echo','inputSchema':{{'type':'object','properties':{{}}}}}}]}}
    else:
        result = {{}}
    print(json.dumps({{'jsonrpc':'2.0','id':message['id'],'result':result}}), flush=True)
"#, methods = methods.display().to_string())).unwrap();
    home.write_hooks(&format!(
        r#"
tool_output_token_limit = 50
[features]
codex_hooks = true
[mcp_servers.echo]
command = "python3"
args = [{}]
{}"#,
        ra_config::TomlValue::String(script.to_string_lossy().into_owned()),
        hook_section(
            "PostToolUse",
            r#"python3 -c "import json; print(json.dumps({'continue': False, 'stopReason': 'hook feedback ' * 200}))""#
        )
    ));
    let config = home.config_builder().build().await.unwrap();
    let mut catalog = ra_mcp::ResolvedMcpCatalog::builder();
    catalog.register(ra_mcp::McpServerRegistration::from_config(
        "echo".into(),
        config.mcp_servers.get()["echo"].clone(),
    ));
    let plan = ra_coding::mcp::product_mcp_servers(
        &catalog.build(),
        home.workspace.path(),
        &config.http_client_factory(),
    );
    assert!(plan.unavailable.is_empty(), "{:?}", plan.unavailable);
    let mcp = CodingMcpRuntime::new(plan.servers, McpDiscoveryOptions::new()).unwrap();
    mcp.reload_mcp_servers().await.unwrap();
    let fixture = ExecutorFixture {
        home,
        mcp,
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let lists_before = std::fs::read_to_string(&methods)
        .unwrap_or_default()
        .lines()
        .filter(|method| *method == "tools/list")
        .count();
    fixture.home.script.push(Step::Respond(ModelResponse::new(vec![RunItem::new(
        ItemId::new("command"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call"),
            "exec_command",
            json!({"cmd":"echo original","justification":null,"prefix_rule":null,"sandbox_permissions":null,"workdir":null,"shell":"/bin/sh","tty":false,"login":false,"yield_time_ms":1000,"timeout_ms":null}),
        )),
    )])));
    fixture.home.script.answer("reviewed");
    server
        .run_turn(&mut connection, &thread, "run a command")
        .await;

    let output = last_tool_output(&fixture.home.script);
    let text = output["blocks"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{output}"));
    assert!(text.starts_with("hook feedback "), "{output}");
    assert!(text.contains("truncated"), "{output}");
    // Fifty tokens are a 200-byte budget for the default byte policy, plus the 20 percent
    // serialization allowance.
    assert!(
        (200..350).contains(&text.len()),
        "{} bytes: {text}",
        text.len()
    );
    // Each model request advertises the MCP tools once; the hook added no listing of its own.
    let lists = std::fs::read_to_string(&methods)
        .unwrap_or_default()
        .lines()
        .filter(|method| *method == "tools/list")
        .count();
    let requests = fixture.home.script.requests.lock().unwrap().len();
    assert_eq!(lists - lists_before, requests);
    fixture.mcp.shutdown().await.unwrap();
}

/// A later discovery must not replace the budget captured by an executing tool; the next turn
/// reuses the same call ID deliberately, so the call-owned handoff must also be consumed.
#[tokio::test]
async fn mcp_feedback_keeps_the_invocations_budget_during_refresh_and_rediscovery() {
    let home = HooksHome::new();
    let script = home.path("owned_mcp.py");
    let entered = home.path("entered");
    let release = home.path("release");
    std::fs::write(&script, format!(r#"
import json, sys, threading, time
lock = threading.Lock()
def send(identifier, result):
    with lock:
        print(json.dumps({{'jsonrpc':'2.0','id':identifier,'result':result}}), flush=True)
def call(identifier):
    open({entered:?}, 'w').close()
    while not __import__('os').path.exists({release:?}):
        time.sleep(0.002)
    send(identifier, {{'content':[{{'type':'text','text':'original result'}}]}})
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message['method']
    if method == 'initialize':
        send(message['id'], {{'protocolVersion':'2024-11-05','capabilities':{{'tools':{{}}}},'serverInfo':{{'name':'echo','version':'1'}}}})
    elif method == 'tools/list':
        send(message['id'], {{'tools':[{{'name':'echo','inputSchema':{{'type':'object','properties':{{}}}},'annotations':{{'readOnlyHint':True,'destructiveHint':False,'openWorldHint':False}}}}]}})
    elif method == 'tools/call':
        threading.Thread(target=call, args=(message['id'],), daemon=True).start()
    else:
        send(message['id'], {{}})
"#, entered=entered.display().to_string(), release=release.display().to_string())).unwrap();
    home.write_hooks(&format!(r#"
tool_output_token_limit = 50
[features]
codex_hooks = true
[mcp_servers.echo]
command = "python3"
default_tools_approval_mode = "approve"
args = [{}]
[mcp_servers.echo.tools.echo]
output_token_limit = 100
{}"#, ra_config::TomlValue::String(script.to_string_lossy().into_owned()), hook_section(
        "PostToolUse", r#"python3 -c "import json; print(json.dumps({'continue': False, 'stopReason': 'hook feedback ' * 200}))""#
    )));
    let config = home.config_builder().build().await.unwrap();
    let mut server_config = config.mcp_servers.get()["echo"].clone();
    let plan = |server_config| {
        let mut catalog = ra_mcp::ResolvedMcpCatalog::builder();
        catalog.register(ra_mcp::McpServerRegistration::from_config(
            "echo".into(),
            server_config,
        ));
        ra_coding::mcp::product_mcp_servers(
            &catalog.build(),
            home.workspace.path(),
            &config.http_client_factory(),
        )
    };
    let mcp = CodingMcpRuntime::new(
        plan(server_config.clone()).servers,
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    mcp.reload_mcp_servers().await.unwrap();
    server_config
        .tools
        .get_mut("echo")
        .unwrap()
        .output_token_limit = std::num::NonZeroUsize::new(10);
    let replacement = plan(server_config).servers;
    let fixture = ExecutorFixture {
        home,
        mcp,
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    fixture
        .home
        .script
        .push(Step::Respond(tool_response("echo")));
    fixture.home.script.answer("first completed");
    server.turn_start(&mut connection, &thread, "first").await;
    timeout(PATIENCE, async {
        while !entered.exists() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    fixture.mcp.replace_mcp_servers(replacement).await.unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("other-run"))
        .name("Other run")
        .build()
        .unwrap();
    let context =
        ra_core::context::RunContext::new(ra_core::state::RunId::new("other-run"), &agent);
    let discovered =
        ra_core::tool::ToolResolver::get_tools(fixture.mcp.as_ref(), &context, &Default::default())
            .await
            .unwrap();
    assert!(discovered.iter().any(|tool| tool.origin().name() == "echo"));
    std::fs::write(&release, "release").unwrap();
    connection.notification("turn/completed").await;
    let first = last_tool_output(&fixture.home.script);
    let first = first["blocks"][0]["text"].as_str().unwrap();
    assert!(
        first.starts_with("hook feedback ") && first.contains("truncated"),
        "{first}"
    );
    assert!(
        (400..600).contains(&first.len()),
        "{}: {first}",
        first.len()
    );
    fixture
        .home
        .script
        .push(Step::Respond(tool_response("echo")));
    fixture.home.script.answer("second completed");
    server.run_turn(&mut connection, &thread, "second").await;
    let second = latest_feedback(&fixture.home.script);
    assert!(
        second.starts_with("hook ") && second.contains("truncated"),
        "{second}"
    );
    assert!(
        (40..160).contains(&second.len()),
        "{}: {second}",
        second.len()
    );
    fixture.mcp.shutdown().await.unwrap();
}

fn latest_feedback(script: &Script) -> String {
    script
        .last_request()
        .input()
        .iter()
        .rev()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) => output.output()["blocks"][0]["text"]
                .as_str()
                .map(str::to_owned),
            _ => None,
        })
        .expect("a model-visible tool output")
}

/// No MCP runtime and no global override: model changes must select their own catalog policy,
/// matched by slug as Codex matches model metadata.
#[tokio::test]
async fn feedback_follows_the_current_turn_model_instead_of_the_initial_model() {
    let home = HooksHome::new();
    let catalog = home.path("catalog.json");
    let model = |slug: &str, mode: &str, limit: usize| {
        json!({
            "slug":slug,"display_name":slug,"description":null,"supported_reasoning_levels":[],
            "shell_type":"unified_exec","visibility":"list","supported_in_api":true,"priority":0,
            "availability_nux":null,"upgrade":null,"support_verbosity":false,"default_verbosity":null,
            "apply_patch_tool_type":null,"truncation_policy":{"mode":mode,"limit":limit},
            "experimental_supported_tools":[],"base_instructions":"Test model instructions"
        })
    };
    std::fs::write(
        &catalog,
        json!({"models":[model("wide", "tokens", 100),model("narrow", "bytes", 80)]}).to_string(),
    )
    .unwrap();
    home.write_hooks(&format!("model = 'wide'\nmodel_catalog_json = {}\n{}",
        ra_config::TomlValue::String(catalog.to_string_lossy().into_owned()),
        hook_section("PostToolUse", r#"python3 -c "import json; print(json.dumps({'continue': False, 'stopReason': 'hook feedback ' * 200}))""#)
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    // Dated and namespaced slugs resolve as Codex resolves model metadata: the longest catalog
    // prefix, then one retry without a `namespace/` segment.
    for (model, range) in [
        ("wide", 400..600),
        ("narrow", 80..180),
        ("wide", 400..600),
        ("narrow-2026-11-01", 80..180),
        ("custom/narrow", 80..180),
    ] {
        home.script.push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new("command"), RunItemKind::ToolCall(ToolCall::new(CallId::new("call"), "exec_command",
                json!({"cmd":"echo original","justification":null,"prefix_rule":null,"sandbox_permissions":null,"workdir":null,"shell":"/bin/sh","tty":false,"login":false,"yield_time_ms":1000,"timeout_ms":null}))),
        )])));
        home.script.answer("reviewed");
        server
            .call(
                &mut connection,
                "turn/start",
                json!({
                    "threadId":thread,"input":[{"type":"text","text":"run command"}],"model":model
                }),
            )
            .await;
        connection.notification("turn/completed").await;
        let text = latest_feedback(&home.script);
        assert!(
            text.starts_with("hook feedback ") && text.contains("truncated"),
            "{model}: {text}"
        );
        assert!(
            range.contains(&text.len()),
            "{model}: {}: {text}",
            text.len()
        );
    }
}

#[path = "mcp_payload.rs"]
mod mcp_payload;

#[path = "mcp_permission.rs"]
mod mcp_permission;

#[path = "mcp_unparsed.rs"]
mod mcp_unparsed;
