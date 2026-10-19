//! Cached declarations, invocation-driven startup and binding ownership.
use super::fixtures::{invoke, seed, settle};
use super::*;
use ra_core::item::CallId;
use ra_mcp::filter::ToolFilter;

#[tokio::test]
async fn cached_visible_tools_defer_startup_until_first_call_and_share_initialization() {
    let probe = Probe::new("docs");
    probe.block_connect.store(true, Ordering::SeqCst);
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone())
                .with_lazy_startup(true)
                .with_cached_catalog(seed("docs", McpClientOptions::default())),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    runtime.reload_mcp_servers().await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::NotStarted)
    );
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(tools[0].schema().name(), "echo_1");
    let first = tokio::spawn({
        let tool = tools[0].clone();
        async move { invoke(tool.as_ref()).await }
    });
    probe.started.notified().await;
    let second = tokio::spawn({
        let tool = tools[0].clone();
        async move { invoke(tool.as_ref()).await }
    });
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::Starting)
    );
    probe.gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cached_startup_is_eager_by_default_and_when_filters_hide_all_tools() {
    for hidden in [false, true] {
        let probe = Probe::new("docs");
        let mut config = McpServerConfig::new(probe.clone())
            .with_cached_catalog(seed("docs", McpClientOptions::default()));
        if hidden {
            config = config.with_lazy_startup(true).with_filter(ToolFilter::new(
                Some(Default::default()),
                Default::default(),
            ));
        }
        let runtime = CodingMcpRuntime::new(vec![config], McpDiscoveryOptions::new()).unwrap();
        runtime.connect_all().await.unwrap();
        assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
        if hidden {
            assert!(names(&runtime).await.is_empty());
        } else {
            assert_eq!(names(&runtime).await, ["echo_1"]);
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cached_binding_keeps_approval_and_guardrails_without_starting_transport() {
    use ra_core::tool::{ToolApprovalPolicy, ToolGuardrailId};
    let probe = Probe::new("docs");
    let options = McpClientOptions::default()
        .with_require_approval(true)
        .with_tool_input_guardrails([ToolGuardrailId::new("input").unwrap()])
        .with_tool_output_guardrails([ToolGuardrailId::new("output").unwrap()]);
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone())
                .with_lazy_startup(true)
                .with_cached_catalog(seed("docs", options)),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    let options = tools[0].options();
    assert_eq!(options.approval(), ToolApprovalPolicy::Always);
    assert_eq!(options.input_guardrails().len(), 1);
    assert_eq!(options.output_guardrails().len(), 1);
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn disabled_dormant_binding_never_wakes_a_new_session() {
    let probe = Probe::new("docs");
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone())
                .with_lazy_startup(true)
                .with_cached_catalog(seed("docs", McpClientOptions::default())),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    assert!(invoke(tools[0].as_ref()).await.is_err());
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_lazy_call_waiter_does_not_cancel_shared_startup() {
    let probe = Probe::new("docs");
    probe.block_connect.store(true, Ordering::SeqCst);
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone())
                .with_lazy_startup(true)
                .with_cached_catalog(seed("docs", McpClientOptions::default())),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    let first = tokio::spawn({
        let tool = tools[0].clone();
        async move { invoke(tool.as_ref()).await }
    });
    probe.started.notified().await;
    first.abort();
    let _ = first.await;
    probe.gate.add_permits(1);
    invoke(tools[0].as_ref()).await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cached_approval_serialization_does_not_connect_until_approved_resume() {
    use ra_core::{
        cancel::CancelScope,
        item::{
            ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind,
            ToolCall as CoreToolCall,
        },
        model::{
            ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings,
            ProviderKey, ResolvedModel,
        },
    };
    use ra_runtime::{
        agent::AgentBinding,
        runner::{RunOutcome, RunRequest, Runner},
    };
    struct Script(Mutex<std::collections::VecDeque<ModelResponse>>);
    #[async_trait]
    impl Model for Script {
        async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::caller("script exhausted"))
        }
    }
    struct Models(Arc<Script>);
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
    for approve in [false, true] {
        let probe = Probe::new("docs");
        let runtime = CodingMcpRuntime::new(
            vec![
                McpServerConfig::new(probe.clone())
                    .with_lazy_startup(true)
                    .with_cached_catalog(seed(
                        "docs",
                        McpClientOptions::default().with_require_approval(true),
                    )),
            ],
            McpDiscoveryOptions::new(),
        )
        .unwrap();
        runtime.connect_all().await.unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("test"))
            .name("Test")
            .tool_resolver(runtime.clone())
            .build()
            .unwrap();
        let model = Arc::new(Script(Mutex::new(
            vec![
                ModelResponse::new(vec![RunItem::new(
                    ItemId::new("call"),
                    RunItemKind::ToolCall(CoreToolCall::new(
                        CallId::new("call"),
                        "echo_1",
                        json!({}),
                    )),
                )]),
                ModelResponse::new(vec![RunItem::new(
                    ItemId::new("final"),
                    RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
                )]),
            ]
            .into(),
        )));
        let models = Arc::new(Models(model));
        let request = || {
            RunRequest::new(
                AgentBinding::direct(agent.clone()),
                models.clone(),
                RunId::new("lazy-approval"),
                CancelScope::root(),
                vec![],
            )
        };
        let paused = Runner::run(request()).await.unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approval");
        };
        assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
        let mut state = paused.state().clone();
        if approve {
            state.approve(&items[0], false).unwrap();
        } else {
            state.reject(&items[0], false).unwrap();
        }
        let state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        let resumed = Runner::run(request().with_state(state)).await.unwrap();
        assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
        assert_eq!(probe.connects.load(Ordering::SeqCst), usize::from(approve));
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn dropping_runtime_cancels_startup_even_when_a_tool_binding_retains_the_entry() {
    let probe = Probe::new("docs");
    probe.block_connect.store(true, Ordering::SeqCst);
    let options = McpServerManagerOptions::default()
        .with_connect_timeout(None)
        .unwrap();
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone())
                .with_lazy_startup(true)
                .with_lifecycle_options(options)
                .with_cached_catalog(seed("docs", McpClientOptions::default())),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    let caller = tokio::spawn({
        let tool = tools[0].clone();
        async move { invoke(tool.as_ref()).await }
    });
    probe.started.notified().await;
    drop(runtime);
    tokio::time::timeout(Duration::from_secs(3), probe.cleanup_started.notified())
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), caller)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(!error.is_cancelled());
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert_eq!(probe.cleanups.load(Ordering::SeqCst), 1);
}
