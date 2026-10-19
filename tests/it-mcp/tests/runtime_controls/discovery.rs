//! Per-server discovery isolation and model-call tool catalogs.
use super::*;

#[tokio::test]
async fn listing_failure_isolated_without_hiding_joint_naming_errors() {
    let good = Probe::new("good");
    let bad = Probe::new("bad");
    let runtime = runtime(&[good.clone(), bad.clone()]);
    runtime.connect_all().await.unwrap();
    bad.fail_listing.store(true, Ordering::SeqCst);
    assert_eq!(names(&runtime).await, ["echo_1"]);
    let statuses = serde_json::to_value(runtime.get_mcp_status()).unwrap();
    assert_eq!(statuses[1]["runtimeStatus"], "connected");
    assert!(
        statuses[1]["toolsError"]
            .as_str()
            .unwrap()
            .contains("catalog unavailable")
    );
    assert_eq!(runtime.errors()[0].1, "tools/list");
    bad.fail_listing.store(false, Ordering::SeqCst);
    assert!(
        runtime
            .get_tools(&context(), &BTreeSet::new())
            .await
            .is_err()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_listing_does_not_abort_the_runners_model_call() {
    use ra_core::{
        cancel::CancelScope,
        item::{ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind},
        model::{
            ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings,
            ProviderKey, ResolvedModel,
        },
    };
    use ra_runtime::{
        agent::AgentBinding,
        runner::{RunOutcome, RunRequest, Runner},
    };
    struct Observe;
    #[async_trait]
    impl Model for Observe {
        async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
            assert_eq!(
                request
                    .tools()
                    .iter()
                    .map(|tool| tool.name())
                    .collect::<Vec<_>>(),
                [
                    "echo_1",
                    "list_mcp_resources",
                    "list_mcp_resource_templates",
                    "read_mcp_resource"
                ]
            );
            Ok(ModelResponse::new(vec![RunItem::new(
                ItemId::new("final"),
                RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
            )]))
        }
    }
    struct Models;
    impl ModelResolver for Models {
        fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
            Ok(ResolvedModel::new(
                ModelSelector::new(ProviderKey::new("test"), None, ApiProtocol::OpenAiResponses),
                Arc::new(Observe),
                ModelSettings::new(),
                ModelSettings::new(),
            ))
        }
    }
    let good = Probe::new("good");
    let bad = Probe::new("bad");
    let runtime = runtime(&[good.clone(), bad.clone()]);
    runtime.connect_all().await.unwrap();
    bad.fail_listing.store(true, Ordering::SeqCst);
    let agent = AgentSpec::builder()
        .id(AgentId::new("test"))
        .name("Test")
        .tool_resolver(runtime.clone())
        .build()
        .unwrap();
    let result = Runner::run(RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Models),
        RunId::new("discovery-isolation"),
        CancelScope::root(),
        vec![],
    ))
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    runtime.shutdown().await.unwrap();
}
