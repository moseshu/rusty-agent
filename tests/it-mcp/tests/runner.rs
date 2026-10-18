//! MCP tools participate in the ordinary frozen action surface and runner loop.
#[path = "../support/mod.rs"]
mod support;
use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall,
        ToolCallKind,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::RunId,
};
use ra_mcp::{
    client::McpClientOptions,
    filter::ToolFilter,
    in_process::InProcessToolServer,
    resolver::McpToolResolver,
    tool::{McpDiscoveryOptions, McpToolNaming},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunOutcome, RunRequest, Runner},
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("script exhausted"))
    }
}
struct Resolver(Arc<ScriptedModel>);
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(ProviderKey::new("mock"), None, ApiProtocol::OpenAiResponses),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}
fn call(id: &str, name: &str, namespace: Option<&str>, kind: ToolCallKind) -> ModelResponse {
    let mut call = ToolCall::new(CallId::new(id), name, json!({"query":"fixture"})).with_kind(kind);
    if let Some(namespace) = namespace {
        call = call.with_namespace(namespace);
    }
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(call),
    )])
}

#[tokio::test]
async fn direct_and_deferred_mcp_tools_complete_an_agent_run() {
    for (deferred, naming) in [
        (false, McpToolNaming::Original),
        (true, McpToolNaming::Original),
        (false, McpToolNaming::Namespaced),
        (true, McpToolNaming::Namespaced),
    ] {
        let mut server = InProcessToolServer::connect(
            "memory",
            support::Fixture::new("memory"),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        // A deferred top-level function is called through the synthetic namespace equal to its
        // name, as the Responses adapter reports it.
        let namespace = match naming {
            McpToolNaming::Namespaced => Some("mcp__memory".to_owned()),
            _ => deferred.then(|| "search".to_owned()),
        };
        let mut script = Vec::new();
        if deferred {
            script.push(call(
                "discover",
                "tool_search",
                None,
                ToolCallKind::ToolSearch,
            ));
        }
        script.push(call(
            "invoke",
            "search",
            namespace.as_deref(),
            ToolCallKind::Function,
        ));
        script.push(ModelResponse::new(vec![RunItem::new(
            ItemId::new("answer"),
            RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
        )]));
        let model = Arc::new(ScriptedModel {
            script: Mutex::new(script.into()),
            requests: Mutex::new(vec![]),
        });
        let binding = AgentBinding::direct(
            AgentSpec::builder()
                .name("MCP agent")
                .id(AgentId::new("mcp-agent"))
                .tool_resolver(Arc::new(McpToolResolver::new(
                    vec![(Arc::new(server.client()), ToolFilter::default())],
                    McpDiscoveryOptions::new()
                        .with_naming(naming)
                        .with_tool_search(deferred),
                )))
                .build()
                .unwrap(),
        );
        let result = Runner::run(RunRequest::new(
            binding,
            Arc::new(Resolver(model.clone())),
            RunId::new("mcp-run"),
            CancelScope::root(),
            vec![],
        ))
        .await
        .unwrap();
        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), if deferred { 3 } else { 2 });
            assert_eq!(
                requests[0].tools().iter().any(|t| t.name() == "search"),
                !deferred
            );
        }
        assert!(result.new_items().iter().any(|item| {
            match item.kind() {
                RunItemKind::ToolCallOutput(output) => {
                    output.call_id().as_str() == "invoke"
                        && serde_json::to_string(output)
                            .unwrap()
                            .contains("memory:search:fixture")
                }
                _ => false,
            }
        }));
        server.cleanup().await.unwrap();
    }
}
