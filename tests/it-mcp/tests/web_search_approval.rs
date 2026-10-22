//! MCP search approvals retain their recipient beside protocol-selected local alternatives.

#[path = "../../support/configured_coding_host.rs"]
mod services;
#[path = "../support/mod.rs"]
mod support;

use async_trait::async_trait;
use ra_coding::{CodingHost, CodingProfile, build_agent_with_control, web_search::CodingWebSearch};
use ra_core::{
    agent::AgentId,
    cancel::CancelScope,
    error::{Error, Result},
    item::{CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    prompt::PromptRole,
    state::{RunId, RunState},
};
use ra_mcp::{
    client::McpClientOptions, filter::ToolFilter, in_process::InProcessToolServer,
    resolver::McpToolResolver, tool::McpDiscoveryOptions,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
    turn::prepare::ToolNameCollisionPolicy,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct Scripted {
    response: Mutex<Option<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Model for Scripted {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.response
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| Error::caller("unexpected extra model request"))
    }
}
struct Models(Arc<Scripted>);
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

#[tokio::test]
async fn same_name_mcp_search_approval_resumes_under_each_collision_policy() {
    for policy in [
        ToolNameCollisionPolicy::Warn,
        ToolNameCollisionPolicy::Error,
    ] {
        let mut fixture = support::Fixture::new("mcp-search");
        fixture.tools_override = Some(vec![support::declaration("web_search")]);
        let mut server = InProcessToolServer::connect(
            "docs",
            fixture,
            McpClientOptions::default().with_require_approval(true),
        )
        .await
        .unwrap();
        let services = Arc::new(services::Services::default());
        let host = services
            .install(CodingHost::open(std::env::temp_dir()).unwrap())
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch::default());
        let built = build_agent_with_control(
            AgentId::new("coding"),
            "Coding",
            &PromptRole::Main,
            &host,
            CodingProfile::CodexLike,
        )
        .await
        .unwrap();
        let agent = built
            .agent()
            .to_builder()
            .tool_resolver(Arc::new(McpToolResolver::new(
                vec![(Arc::new(server.client()), ToolFilter::default())],
                McpDiscoveryOptions::new(),
            )))
            .build()
            .unwrap();
        let config = || RunConfig::new().with_tool_name_collision_policy(policy);
        let request = |model| {
            RunRequest::new(
                AgentBinding::direct(agent.clone()),
                Arc::new(Models(model)),
                RunId::new("mcp-search"),
                CancelScope::root(),
                vec![],
            )
            .with_config(config())
        };
        let initial = Arc::new(Scripted {
            response: Mutex::new(Some(ModelResponse::new(vec![RunItem::new(
                ItemId::new("search"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("search"),
                    "web_search",
                    json!({"query":"Rust"}),
                )),
            )]))),
            requests: Mutex::new(vec![]),
        });
        let paused = Runner::run(request(initial.clone())).await.unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected an MCP approval");
        };
        assert_eq!(items.len(), 1);
        let mut state: RunState =
            serde_json::from_str(&serde_json::to_string(paused.state()).unwrap()).unwrap();
        state.approve(&items[0], false).unwrap();
        let final_model = Arc::new(Scripted {
            response: Mutex::new(Some(ModelResponse::new(vec![RunItem::new(
                ItemId::new("finished"),
                RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
            )]))),
            requests: Mutex::new(vec![]),
        });
        let completed = Runner::run(request(final_model.clone()).with_state(state))
            .await
            .unwrap();
        assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
        let requests = final_model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            serde_json::to_string(requests[0].input())
                .unwrap()
                .contains("mcp-search:web_search:Rust")
        );
        assert!(
            !services
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("search:"))
        );
        drop(requests);
        built.agent_control().unwrap().shutdown().await;
        server.cleanup().await.unwrap();
    }
}
