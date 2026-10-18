//! MCP tools participate in the ordinary frozen action surface and runner loop.
#[path = "../support/mod.rs"]
mod support;
use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall,
        ToolCallKind,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel, RunItemStreamEvent,
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
    runner::{RunOutcome, RunRequest, RunStreamEvent, Runner},
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
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        stream::once(async move {
            match self.get_response(request).await {
                Ok(response) => {
                    let mut events: Vec<_> = response
                        .output()
                        .iter()
                        .map(|item| {
                            Ok(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
                                item.kind().label(),
                                item.clone(),
                            )))
                        })
                        .collect();
                    events.push(Ok(ModelStreamEvent::Completed(Box::new(response))));
                    events
                }
                Err(error) => vec![Err(error)],
            }
        })
        .flat_map(stream::iter)
        .boxed()
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
    for streamed in [false, true] {
        for fallback in [false, true] {
            for (deferred, naming) in [
                (false, McpToolNaming::Original),
                (true, McpToolNaming::Original),
                (false, McpToolNaming::Namespaced),
                (true, McpToolNaming::Namespaced),
                (false, McpToolNaming::ServerPrefixed),
                (true, McpToolNaming::ServerPrefixed),
            ] {
                let mut fixture = support::Fixture::new("memory");
                let mut search = support::declaration("search");
                search.title = Some("UI-only runner fixture title".into());
                if fallback {
                    search.description = None;
                }
                let expected_description = if fallback {
                    "UI-only runner fixture title"
                } else {
                    "Search fixture"
                };
                fixture.tools_override = Some(vec![search]);
                let mut server = InProcessToolServer::connect(
                    "memory",
                    fixture,
                    McpClientOptions::default().with_cache_tools_list(true),
                )
                .await
                .unwrap();
                // A deferred top-level function is called through the synthetic namespace equal to its
                // name, as the Responses adapter reports it.
                let callable_name = if naming == McpToolNaming::ServerPrefixed {
                    "mcp_memory__search"
                } else {
                    "search"
                };
                let namespace = match naming {
                    McpToolNaming::Namespaced => Some("mcp__memory".to_owned()),
                    _ => deferred.then(|| callable_name.to_owned()),
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
                    callable_name,
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
                let request = RunRequest::new(
                    binding,
                    Arc::new(Resolver(model.clone())),
                    RunId::new("mcp-run"),
                    CancelScope::root(),
                    vec![],
                );
                let mut call_events = Vec::new();
                let result = if streamed {
                    let mut stream = Runner::run_streamed(request);
                    while let Some(event) = stream.next_event().await {
                        if let RunStreamEvent::Item(item) = event
                            && item.id().as_str() == "invoke"
                        {
                            call_events.push(item);
                        }
                    }
                    stream.finish().await.unwrap()
                } else {
                    Runner::run(request).await.unwrap()
                };
                assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
                let item = result
                    .new_items()
                    .iter()
                    .find(|item| item.id().as_str() == "invoke")
                    .unwrap();
                assert_eq!(item.description(), Some(expected_description));
                assert_eq!(item.title(), Some("UI-only runner fixture title"));
                if streamed {
                    assert_eq!(call_events.as_slice(), std::slice::from_ref(item));
                }
                let restored: ra_core::state::RunState =
                    serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
                let item = restored
                    .generated_items()
                    .iter()
                    .find(|item| item.id().as_str() == "invoke")
                    .unwrap();
                assert_eq!(item.description(), Some(expected_description));
                assert_eq!(item.title(), Some("UI-only runner fixture title"));
                {
                    let requests = model.requests.lock().unwrap();
                    assert_eq!(requests.len(), if deferred { 3 } else { 2 });
                    assert_eq!(
                        requests[0]
                            .tools()
                            .iter()
                            .any(|t| t.name() == callable_name),
                        !deferred
                    );
                    for request in requests.iter() {
                        for input in request.input() {
                            if let ra_core::item::ModelInputItem::ToolCall(_) = input {
                                let value = serde_json::to_value(input).unwrap();
                                assert!(
                                    !value.to_string().contains("UI-only runner fixture title")
                                );
                                assert!(value["data"].get("title").is_none());
                                assert!(value["data"].get("description").is_none());
                            }
                        }
                    }
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
    }
}
