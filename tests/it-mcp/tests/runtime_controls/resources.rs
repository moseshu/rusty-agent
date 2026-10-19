//! Resource helpers share discovery, initialization and lifecycle ownership with function tools.
use super::*;
use ra_core::{
    item::CallId,
    tool::{Tool as CoreTool, ToolContext, ToolOutput},
};
use ra_mcp::filter::ToolFilter;

async fn read(tool: &dyn CoreTool) -> Result<ToolOutput> {
    let run = context();
    let id = CallId::new("read-resource");
    let args = json!({"server":"docs","uri":"memo://version"});
    tool.call(ToolContext::new(&run, tool, &id, &args)).await
}

async fn helpers(runtime: &CodingMcpRuntime) -> Vec<Arc<dyn CoreTool>> {
    runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap()
        .into_iter()
        .filter(|tool| tool.schema().name() != "echo_1" && tool.schema().name() != "echo_2")
        .collect()
}

#[tokio::test]
async fn resources_survive_tools_list_failure_and_function_filtering() {
    for filter in [false, true] {
        let probe = Probe::new("docs");
        let config = McpServerConfig::new(probe.clone()).with_filter(if filter {
            ToolFilter::new(Some(Default::default()), Default::default())
        } else {
            ToolFilter::default()
        });
        let runtime = CodingMcpRuntime::new(vec![config], McpDiscoveryOptions::new()).unwrap();
        runtime.connect_all().await.unwrap();
        probe.fail_listing.store(!filter, Ordering::SeqCst);
        let tools = helpers(&runtime).await;
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.schema().name())
                .collect::<Vec<_>>(),
            [
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource"
            ]
        );
        assert!(
            read(tools[2].as_ref())
                .await
                .unwrap()
                .as_text()
                .unwrap()
                .contains("session generation 1")
        );
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn resource_bindings_capture_sessions_and_follow_reconnect_only_on_rediscovery() {
    let probe = Probe::new("docs");
    let runtime = runtime(&[probe]);
    runtime.connect_all().await.unwrap();
    let old = helpers(&runtime).await;
    assert!(
        read(old[2].as_ref())
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .contains("session generation 1")
    );
    runtime.reconnect_mcp_server(None, false).await.unwrap();
    assert!(read(old[2].as_ref()).await.is_err());
    let new = helpers(&runtime).await;
    assert!(
        read(new[2].as_ref())
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .contains("session generation 2")
    );
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    assert!(
        runtime
            .get_tools(&context(), &BTreeSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(read(new[2].as_ref()).await.is_err());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resource_helpers_remain_advertised_when_server_function_tools_are_deferred() {
    use ra_core::tool::ToolExposure;
    let probe = Probe::new("docs");
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(probe)],
        McpDiscoveryOptions::new().with_tool_search(true),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(tools[0].options().exposure(), ToolExposure::Deferred);
    for tool in &tools[1..] {
        assert_eq!(tool.options().exposure(), ToolExposure::Advertised);
        assert!(tool.origin().namespace().is_none());
    }
    assert_eq!(tools.len(), 4);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_withdraws_resource_helpers_before_cleanup_has_completed() {
    let probe = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&probe));
    runtime.connect_all().await.unwrap();
    probe.block_cleanup.store(true, Ordering::SeqCst);
    let shutdown = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.shutdown().await }
    });
    tokio::time::timeout(Duration::from_secs(1), probe.cleanup_started.notified())
        .await
        .unwrap();
    assert!(
        runtime
            .get_tools(&context(), &BTreeSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    probe.cleanup_gate.add_permits(1);
    shutdown.await.unwrap().unwrap();
}

#[tokio::test]
async fn coding_host_resource_calls_complete_a_real_runner_round_trip() {
    use ra_coding::{CodingHost, agent::build_agent_with_host};
    use ra_core::{
        cancel::CancelScope,
        item::{
            ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
            ToolCall,
        },
        model::{
            ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings,
            ProviderKey, ResolvedModel,
        },
        prompt::PromptRole,
    };
    use ra_runtime::{
        agent::AgentBinding,
        runner::{RunOutcome, RunRequest, Runner},
    };
    struct ModelProbe(AtomicUsize);
    #[async_trait]
    impl Model for ModelProbe {
        async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
            let turn = self.0.fetch_add(1, Ordering::SeqCst);
            assert!(
                request
                    .tools()
                    .iter()
                    .any(|tool| tool.name() == "read_mcp_resource")
            );
            let kind = match turn {
                0 => RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("list"),
                    "list_mcp_resources",
                    json!({}),
                )),
                1 => {
                    assert!(request.input().iter().any(|item|matches!(item,ModelInputItem::ToolCallOutput(output) if output.call_id().as_str()=="list")));
                    RunItemKind::ToolCall(ToolCall::new(
                        CallId::new("read"),
                        "read_mcp_resource",
                        json!({"server":"docs","uri":"memo://version"}),
                    ))
                }
                2 => {
                    let input = serde_json::to_string(request.input()).unwrap();
                    assert!(input.contains("session generation 1"), "{input}");
                    RunItemKind::Message(Message::assistant(
                        "Read resource successfully",
                        OutputPhase::Final,
                    ))
                }
                _ => panic!("unexpected additional model call"),
            };
            Ok(ModelResponse::new(vec![RunItem::new(
                ItemId::new(format!("response-{turn}")),
                kind,
            )]))
        }
    }
    struct Models(Arc<ModelProbe>);
    impl ModelResolver for Models {
        fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
            Ok(ResolvedModel::new(
                ModelSelector::new(ProviderKey::new("test"), None, ApiProtocol::OpenAiResponses),
                self.0.clone(),
                ModelSettings::new(),
                ModelSettings::new(),
            ))
        }
    }
    let probe = Probe::new("docs");
    let runtime = runtime(&[probe]);
    let host = CodingHost::open(std::env::temp_dir())
        .unwrap()
        .with_mcp_runtime(runtime.clone());
    let agent = build_agent_with_host(AgentId::new("coding"), "Coding", &PromptRole::Main, &host)
        .await
        .unwrap();
    let model = Arc::new(ModelProbe(AtomicUsize::new(0)));
    let result = Runner::run(RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Models(model.clone())),
        RunId::new("resource-round-trip"),
        CancelScope::root(),
        vec![],
    ))
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(model.0.load(Ordering::SeqCst), 3);
    assert!(result.new_items().iter().any(|item|matches!(item.kind(),RunItemKind::ToolCallOutput(output) if output.call_id().as_str()=="read" && serde_json::to_string(output).unwrap().contains("session generation 1"))));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resource_helpers_are_advertised_before_lazy_startup_and_wait_for_the_connection() {
    let probe = Probe::new("docs");
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone())
                .with_lazy_startup(true)
                .with_cached_catalog(super::fixtures::seed("docs", McpClientOptions::default())),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::NotStarted)
    );
    // As Codex advertises them whenever servers are configured, discovery itself starts nothing.
    let tools = helpers(&runtime).await;
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.schema().name())
            .collect::<Vec<_>>(),
        [
            "list_mcp_resources",
            "list_mcp_resource_templates",
            "read_mcp_resource"
        ]
    );
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    assert!(
        read(tools[2].as_ref())
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .contains("session generation 1")
    );
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::Connected)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resource_startup_failures_keep_safe_diagnostics_without_retrying_or_exposing_sources() {
    for lazy in [false, true] {
        let probe = Probe::new("docs");
        probe.failures.store(1, Ordering::SeqCst);
        let runtime = CodingMcpRuntime::new(
            vec![
                McpServerConfig::new(probe.clone())
                    .with_lazy_startup(lazy)
                    .with_cached_catalog(super::fixtures::seed(
                        "docs",
                        McpClientOptions::default(),
                    )),
            ],
            McpDiscoveryOptions::new(),
        )
        .unwrap();
        runtime.connect_all().await.unwrap();
        let original = helpers(&runtime).await;
        let run = context();
        let call_id = CallId::new("failed-resource-startup");
        let mut observations = Vec::new();
        for rediscovered in [false, true] {
            let tools = if rediscovered {
                helpers(&runtime).await
            } else {
                original.clone()
            };
            for index in [2, 0, 1] {
                let tool = &tools[index];
                let args = if index == 2 {
                    json!({"server":"docs", "uri":"memo://version"})
                } else {
                    json!({"server":"docs"})
                };
                let error = tool
                    .call(ToolContext::new(&run, tool.as_ref(), &call_id, &args))
                    .await
                    .unwrap_err();
                let mut source = std::error::Error::source(&error);
                let mut has_private_source = false;
                while let Some(current) = source {
                    has_private_source |= current.to_string().contains("secret-token-123");
                    source = current.source();
                }
                let output = tool
                    .handle_failure(
                        &ToolContext::new(&run, tool.as_ref(), &call_id, &args),
                        &error,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                observations.push((
                    index,
                    output.as_text().unwrap().to_owned(),
                    has_private_source,
                ));
            }
        }
        let attempts = probe.connects.load(Ordering::SeqCst);
        runtime.shutdown().await.unwrap();
        assert_eq!(
            attempts, 1,
            "resource calls must not implicitly retry failed startup"
        );
        for (index, text, has_private_source) in observations {
            let method = [
                "resources/list",
                "resources/templates/list",
                "resources/read",
            ][index];
            assert_eq!(
                text,
                format!(
                    "{method} failed: failed to get client: MCP startup failed: MCP handshake rejected: unsupported protocol"
                )
            );
            assert!(!text.contains("secret-token-123"));
            assert!(
                has_private_source,
                "host diagnostics must retain the original source"
            );
        }
    }
}
