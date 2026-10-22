// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Per-turn coding search selection and explicit fallback-policy contracts.

#[path = "../../support/configured_coding_host.rs"]
mod configured_host;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_coding::{
    CodingHost, CodingProfile, build_agent_with_control, host_backed_surface,
    web_search::{
        CodingWebSearch, WebSearchFallbackAdapter, WebSearchMode, WebSearchPolicy,
        WebSearchToolConfig,
    },
};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{CallId, ModelResponse},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel, ToolChoice,
    },
    prompt::PromptRole,
    state::{RunId, ToolUseTracker},
    tool::{Tool, ToolContext},
    web::WebAccess,
};
use ra_model::openai::responses::web_search::{WebSearchContextSize, WebSearchUserLocation};
use ra_runtime::{
    agent::AgentBinding,
    turn::prepare::{PreparedTurn, TurnPreparationRequest, prepare_turn},
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Resolver(ApiProtocol);
struct Uncalled;
#[async_trait]
impl Model for Uncalled {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("preparation must not call a model"))
    }
}
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(ProviderKey::new("test"), None, self.0),
            Arc::new(Uncalled),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct Adapter {
    access: Arc<configured_host::Services>,
    modes: Vec<WebSearchMode>,
    policies: Mutex<Vec<WebSearchPolicy>>,
}
impl WebSearchFallbackAdapter for Adapter {
    fn supports(&self, policy: &WebSearchPolicy) -> bool {
        self.modes.contains(&policy.mode)
    }
    fn bind(&self, policy: &WebSearchPolicy) -> Result<Arc<dyn WebAccess>> {
        assert!(self.supports(policy));
        self.policies.lock().unwrap().push(policy.clone());
        // This fixture serves a fixed in-memory document and search result; no network is used.
        Ok(self.access.clone())
    }
}

fn base(workspace: &TempDir, services: &Arc<configured_host::Services>) -> CodingHost {
    services
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_collaboration_namespace(None)
}
async fn assemble(host: &CodingHost) -> Arc<AgentSpec> {
    build_agent_with_control(
        AgentId::new("search"),
        "Search",
        &PromptRole::Main,
        host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap()
    .agent()
    .clone()
}
async fn prepare(agent: &Arc<AgentSpec>, protocol: ApiProtocol) -> PreparedTurn {
    prepare_turn(
        TurnPreparationRequest::new(
            &AgentBinding::direct(agent.clone()),
            &Resolver(protocol),
            &RunContext::new(RunId::new("search"), agent.as_ref()),
            &CancelScope::root(),
            &ToolUseTracker::new(),
            vec![],
        )
        .with_model_settings(
            ModelSettings::new()
                .with_max_tokens(1024)
                .with_tool_choice(ToolChoice::Tool("web_search".into())),
        ),
    )
    .await
    .unwrap()
}
fn shape(turn: &PreparedTurn) -> (usize, usize, bool) {
    let search: Vec<_> = turn
        .request()
        .tools()
        .iter()
        .filter(|tool| tool.name() == "web_search")
        .collect();
    (
        search.iter().filter(|tool| tool.is_hosted()).count(),
        search.iter().filter(|tool| !tool.is_hosted()).count(),
        turn.request()
            .tools()
            .iter()
            .any(|tool| tool.name() == "web_fetch"),
    )
}
async fn invoke(agent: &Arc<AgentSpec>, tool: &Arc<dyn Tool>, arguments: Value) -> Result<()> {
    let run = RunContext::new(RunId::new("invoke"), agent.as_ref());
    tool.call(ToolContext::new(
        &run,
        tool.as_ref(),
        &CallId::new("web"),
        &arguments,
    ))
    .await
    .map(|_| ())
}

#[tokio::test]
async fn one_agent_switches_between_unique_hosted_and_local_search_with_stable_instructions() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = base(&workspace, &services)
        .with_web_access_fallback(services.clone())
        .with_web_search(CodingWebSearch {
            mode: WebSearchMode::Live,
            ..Default::default()
        });
    let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap();
    assert_eq!(
        surface
            .tool_surface()
            .advertised_names()
            .filter(|name| *name == "web_search")
            .count(),
        1
    );
    assert!(surface.tool_surface().hosted_tools().is_empty());
    let agent = assemble(&host).await;
    let mut prefixes = Vec::new();
    for (protocol, expected) in [
        (ApiProtocol::OpenAiResponses, (1, 0, false)),
        (ApiProtocol::OpenAiChatCompletions, (0, 1, true)),
        (ApiProtocol::AnthropicMessages, (0, 1, true)),
        (ApiProtocol::OpenAiResponses, (1, 0, false)),
    ] {
        let turn = prepare(&agent, protocol).await;
        assert_eq!(shape(&turn), expected);
        assert_eq!(
            turn.request().model_settings().tool_choice(),
            Some(&ToolChoice::Tool("web_search".into()))
        );
        assert_eq!(
            turn.tools()
                .iter()
                .any(|tool| tool.origin().name() == "web_search"),
            expected.1 == 1
        );
        let prefix = turn.request().system_instructions().unwrap().to_owned();
        assert!(!prefix.contains("web_search"));
        assert!(!prefix.contains("web_fetch"));
        assert!(prefix.contains("tools actually declared in this request"));
        prefixes.push(prefix);
    }
    assert!(
        prefixes
            .windows(2)
            .all(|pair| pair[0].as_bytes() == pair[1].as_bytes())
    );
    assert!(
        services
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| !call.starts_with("search:") && !call.starts_with("fetch:"))
    );
}

#[tokio::test]
async fn plain_fallback_does_not_weaken_the_default_cached_mode() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = base(&workspace, &services).with_web_access_fallback(services.clone());
    let agent = assemble(&host).await;
    assert_eq!(
        shape(&prepare(&agent, ApiProtocol::OpenAiResponses).await),
        (1, 0, false)
    );
    for protocol in [
        ApiProtocol::OpenAiChatCompletions,
        ApiProtocol::AnthropicMessages,
    ] {
        assert_eq!(shape(&prepare(&agent, protocol).await), (0, 0, false));
    }
}

#[tokio::test]
async fn cached_adapter_receives_effective_options_and_never_exposes_fetch() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let adapter = Arc::new(Adapter {
        access: services.clone(),
        modes: vec![WebSearchMode::Cached],
        policies: Mutex::new(vec![]),
    });
    let tools = WebSearchToolConfig {
        context_size: Some(WebSearchContextSize::High),
        allowed_domains: Some(vec!["example.test".into()]),
        location: Some(WebSearchUserLocation {
            country: Some("CN".into()),
            city: Some("Shanghai".into()),
            ..Default::default()
        }),
    };
    let host = base(&workspace, &services)
        .with_web_access_fallback_adapter(adapter.clone())
        .with_web_search(CodingWebSearch {
            tools: tools.clone(),
            ..Default::default()
        });
    let agent = assemble(&host).await;
    let turn = prepare(&agent, ApiProtocol::OpenAiChatCompletions).await;
    assert_eq!(shape(&turn), (0, 1, false));
    let search = turn
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "web_search")
        .unwrap();
    let context = RunContext::new(RunId::new("cached-call"), agent.as_ref());
    let arguments = json!({"query":"Rust", "max_results":1});
    let call_id = CallId::new("cached-call");
    let output = search
        .call(ToolContext::new(
            &context,
            search.as_ref(),
            &call_id,
            &arguments,
        ))
        .await
        .unwrap();
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .all(|text| !text.contains("web_fetch"))
    );
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .any(|text| text.contains("untrusted data"))
    );
    assert_eq!(
        *adapter.policies.lock().unwrap(),
        [WebSearchPolicy {
            mode: WebSearchMode::Cached,
            tools
        }]
    );
    assert!(
        services
            .calls
            .lock()
            .unwrap()
            .contains(&"search:Rust".to_owned())
    );
}

#[tokio::test]
async fn common_capability_and_disabled_mode_remove_both_shapes() {
    for config in [
        CodingWebSearch {
            enabled: false,
            mode: WebSearchMode::Live,
            ..Default::default()
        },
        CodingWebSearch {
            mode: WebSearchMode::Disabled,
            ..Default::default()
        },
        CodingWebSearch {
            mode: WebSearchMode::Live,
            allowed_modes: vec![WebSearchMode::Disabled],
            ..Default::default()
        },
    ] {
        let workspace = TempDir::new().unwrap();
        let services = Arc::new(configured_host::Services::default());
        let agent = assemble(
            &base(&workspace, &services)
                .with_web_access_fallback(services.clone())
                .with_web_search(config),
        )
        .await;
        for protocol in [
            ApiProtocol::OpenAiResponses,
            ApiProtocol::OpenAiChatCompletions,
            ApiProtocol::AnthropicMessages,
        ] {
            assert_eq!(shape(&prepare(&agent, protocol).await), (0, 0, false));
        }
    }
}

#[tokio::test]
async fn hosted_support_does_not_disable_an_independent_backend() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = base(&workspace, &services)
        .with_web_access_fallback(services.clone())
        .with_web_search(CodingWebSearch {
            supported: false,
            mode: WebSearchMode::Live,
            ..Default::default()
        });
    let agent = assemble(&host).await;
    assert_eq!(
        shape(&prepare(&agent, ApiProtocol::OpenAiResponses).await),
        (0, 0, false)
    );
    assert_eq!(
        shape(&prepare(&agent, ApiProtocol::OpenAiChatCompletions).await),
        (0, 1, true)
    );
    let preferred = assemble(&host.with_web_search(CodingWebSearch {
        supported: false,
        prefer_standalone: true,
        mode: WebSearchMode::Live,
        ..Default::default()
    }))
    .await;
    assert_eq!(
        shape(&prepare(&preferred, ApiProtocol::OpenAiResponses).await),
        (0, 1, true)
    );
}

#[tokio::test]
async fn an_unavailable_preferred_backend_does_not_remove_hosted_search() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    for host in [
        base(&workspace, &services).with_web_search(CodingWebSearch {
            prefer_standalone: true,
            ..Default::default()
        }),
        base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch {
                prefer_standalone: true,
                ..Default::default()
            }),
    ] {
        let agent = assemble(&host).await;
        assert_eq!(
            shape(&prepare(&agent, ApiProtocol::OpenAiResponses).await),
            (1, 0, false)
        );
    }
}

#[tokio::test]
async fn explicit_live_and_indexed_modes_require_matching_adapter_support() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let adapter = Arc::new(Adapter {
        access: services.clone(),
        modes: vec![WebSearchMode::Indexed],
        policies: Mutex::new(vec![]),
    });
    let host = base(&workspace, &services)
        .with_web_access_fallback_adapter(adapter.clone())
        .with_web_search(CodingWebSearch {
            mode: WebSearchMode::Indexed,
            ..Default::default()
        });
    let agent = assemble(&host).await;
    let turn = prepare(&agent, ApiProtocol::AnthropicMessages).await;
    assert_eq!(shape(&turn), (0, 1, true));
    let search = turn
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "web_search")
        .unwrap();
    invoke(&agent, search, json!({"query":"index"}))
        .await
        .unwrap();
    assert_eq!(
        adapter.policies.lock().unwrap()[0].mode,
        WebSearchMode::Indexed
    );
    let unsupported = assemble(&host.with_web_search(CodingWebSearch {
        mode: WebSearchMode::Live,
        ..Default::default()
    }))
    .await;
    assert_eq!(
        shape(&prepare(&unsupported, ApiProtocol::AnthropicMessages).await),
        (0, 0, false)
    );
}

#[tokio::test]
async fn plain_fallback_never_ignores_location_filters_or_context_settings() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    for tools in [
        WebSearchToolConfig {
            allowed_domains: Some(vec!["example.test".into()]),
            ..Default::default()
        },
        WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::High),
            ..Default::default()
        },
        WebSearchToolConfig {
            location: Some(WebSearchUserLocation::default()),
            ..Default::default()
        },
    ] {
        let agent = assemble(
            &base(&workspace, &services)
                .with_web_access_fallback(services.clone())
                .with_web_search(CodingWebSearch {
                    mode: WebSearchMode::Live,
                    tools,
                    ..Default::default()
                }),
        )
        .await;
        assert_eq!(
            shape(&prepare(&agent, ApiProtocol::OpenAiChatCompletions).await),
            (0, 0, false)
        );
        assert_eq!(
            shape(&prepare(&agent, ApiProtocol::OpenAiResponses).await),
            (1, 0, false)
        );
    }
}

#[tokio::test]
async fn fetch_domain_policy_checks_boundaries_before_calling_the_backend() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let adapter = Arc::new(Adapter {
        access: services.clone(),
        modes: vec![WebSearchMode::Live],
        policies: Mutex::new(vec![]),
    });
    let agent = assemble(
        &base(&workspace, &services)
            .with_web_access_fallback_adapter(adapter)
            .with_web_search(CodingWebSearch {
                mode: WebSearchMode::Live,
                tools: WebSearchToolConfig {
                    allowed_domains: Some(vec!["example.test".into()]),
                    ..Default::default()
                },
                ..Default::default()
            }),
    )
    .await;
    let turn = prepare(&agent, ApiProtocol::OpenAiChatCompletions).await;
    let fetch = turn
        .tools()
        .iter()
        .find(|tool| tool.origin().name() == "web_fetch")
        .unwrap();
    for url in ["https://example.test/a", "https://docs.example.test/b"] {
        invoke(&agent, fetch, json!({"url":url})).await.unwrap();
    }
    for url in [
        "https://badexample.test/",
        "https://example.test.evil/",
        "file:///example.test/a",
        "invalid",
    ] {
        assert!(invoke(&agent, fetch, json!({"url":url})).await.is_err());
        assert!(
            !services
                .calls
                .lock()
                .unwrap()
                .contains(&format!("fetch:{url}"))
        );
    }
}

#[tokio::test]
async fn builder_order_preserves_native_settings_and_legacy_selection() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let config = CodingWebSearch {
        mode: WebSearchMode::Live,
        ..Default::default()
    };
    for host in [
        base(&workspace, &services)
            .with_web_search(config.clone())
            .with_web_access_fallback(services.clone()),
        base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(config.clone()),
    ] {
        assert_eq!(host.web_search(), Some(&config));
        let agent = assemble(&host).await;
        assert_eq!(
            shape(&prepare(&agent, ApiProtocol::OpenAiResponses).await),
            (1, 0, false)
        );
        assert_eq!(
            shape(&prepare(&agent, ApiProtocol::OpenAiChatCompletions).await),
            (0, 1, true)
        );
        let legacy_host = host.with_web_access(services.clone());
        assert!(legacy_host.web_search().is_none());
        let legacy = assemble(&legacy_host).await;
        for protocol in [
            ApiProtocol::OpenAiResponses,
            ApiProtocol::OpenAiChatCompletions,
            ApiProtocol::AnthropicMessages,
        ] {
            assert_eq!(shape(&prepare(&legacy, protocol).await), (0, 1, true));
        }
        let restored = legacy_host.with_web_access_fallback(services.clone());
        assert_eq!(restored.web_search(), Some(&CodingWebSearch::default()));
    }
}

#[tokio::test]
async fn role_and_profile_selection_do_not_install_unselected_search_candidates() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = base(&workspace, &services)
        .with_web_access_fallback(services.clone())
        .with_web_search(CodingWebSearch {
            mode: WebSearchMode::Live,
            ..Default::default()
        });
    for (role, profile) in [
        (PromptRole::Main, CodingProfile::Core),
        (PromptRole::OneOffAnswer, CodingProfile::CodexLike),
    ] {
        let agent =
            build_agent_with_control(AgentId::new("narrow"), "Narrow", &role, &host, profile)
                .await
                .unwrap()
                .agent()
                .clone();
        for protocol in [
            ApiProtocol::OpenAiResponses,
            ApiProtocol::OpenAiChatCompletions,
        ] {
            assert_eq!(shape(&prepare(&agent, protocol).await), (0, 0, false));
        }
    }
}

struct EmptyDiscovery;
#[async_trait]
impl ra_core::tool::ToolResolver for EmptyDiscovery {
    async fn get_tools(
        &self,
        _: &RunContext,
        _: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![])
    }
}

struct Scripted {
    responses: Mutex<Vec<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Model for Scripted {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(Error::caller(format!(
                "script exhausted; requests: {:?}",
                self.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(ModelRequest::input)
                    .collect::<Vec<_>>()
            )));
        }
        Ok(responses.remove(0))
    }
}
struct RunResolver(ApiProtocol, Arc<Scripted>);
impl ModelResolver for RunResolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(ProviderKey::new("test"), None, self.0),
            self.1.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

#[tokio::test]
async fn serialized_function_approval_resumes_with_discovery_after_protocol_change() {
    use ra_core::{
        item::{ItemId, Message, OutputPhase, RunItem, RunItemKind, ToolCall},
        permission::{PermissionDecision, PermissionRule},
        state::RunState,
    };
    use ra_runtime::runner::{RunConfig, RunOutcome, RunRequest, Runner};
    for (name, arguments, expected) in [
        (
            "web_search",
            json!({"query":"Rust", "max_results":null}),
            "search:Rust",
        ),
        (
            "web_fetch",
            json!({"url":"https://example.test/guide"}),
            "fetch:https://example.test/guide",
        ),
    ] {
        let workspace = TempDir::new().unwrap();
        let services = Arc::new(configured_host::Services::default());
        let host = base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch {
                mode: WebSearchMode::Live,
                ..Default::default()
            });
        let agent = assemble(&host)
            .await
            .to_builder()
            .tool_resolver(Arc::new(EmptyDiscovery))
            .build()
            .unwrap();
        let initial = Arc::new(Scripted {
            responses: Mutex::new(vec![ModelResponse::new(vec![RunItem::new(
                ItemId::new("web-call"),
                RunItemKind::ToolCall(ToolCall::new(CallId::new("search-call"), name, arguments)),
            )])]),
            requests: Mutex::new(vec![]),
        });
        let request = |protocol, model| {
            RunRequest::new(
                AgentBinding::direct(agent.clone()),
                Arc::new(RunResolver(protocol, model)),
                RunId::new("resumed-search"),
                CancelScope::root(),
                vec![],
            )
            .with_config(RunConfig::new().with_permission_rules([
                PermissionRule::new(PermissionDecision::Ask).with_tool_name(name),
            ]))
        };
        let paused = Runner::run(request(ApiProtocol::OpenAiChatCompletions, initial.clone()))
            .await
            .unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approval");
        };
        assert_eq!(items.len(), 1);
        let RunItemKind::ToolApproval(approval) = items[0].kind() else {
            panic!("expected tool approval");
        };
        assert_eq!(
            approval.recipient_binding(),
            Some(&json!({"coding_web_fallback": name}))
        );
        assert!(items[0].to_model_input().is_none());
        assert!(
            !services
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("search:") || call.starts_with("fetch:"))
        );
        let checkpoint = serde_json::to_string(paused.state()).unwrap();
        assert!(checkpoint.contains("coding_web_fallback"));
        let mut state: RunState = serde_json::from_str(&checkpoint).unwrap();
        state.approve(&items[0], false).unwrap();
        let final_model = Arc::new(Scripted {
            responses: Mutex::new(vec![ModelResponse::new(vec![RunItem::new(
                ItemId::new("finished"),
                RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
            )])]),
            requests: Mutex::new(vec![]),
        });
        let completed = Runner::run(
            request(ApiProtocol::OpenAiResponses, final_model.clone()).with_state(state),
        )
        .await
        .unwrap();
        assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
        assert_eq!(
            services
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| *call == expected)
                .count(),
            1
        );
        let requests = final_model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let search = requests[0]
            .tools()
            .iter()
            .filter(|tool| tool.name() == "web_search")
            .collect::<Vec<_>>();
        assert_eq!(search.len(), 1);
        assert!(search[0].is_hosted());
        assert!(
            requests[0]
                .input()
                .iter()
                .any(|item| matches!(item, ra_core::item::ModelInputItem::ToolCallOutput(_)))
        );
        drop(requests);
        for model in [&initial, &final_model] {
            for request in model.requests.lock().unwrap().iter() {
                let request = format!("{request:?}");
                assert!(!request.contains("coding_web_fallback"));
                assert!(!request.contains("recipient_binding"));
            }
        }
    }
}

#[tokio::test]
async fn hosted_candidate_bytes_are_checked_without_adding_a_second_search_slot() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = base(&workspace, &services)
        .with_web_access_fallback(services.clone())
        .with_web_search(CodingWebSearch {
            tools: WebSearchToolConfig {
                allowed_domains: Some(vec!["example.test".repeat(10_000)]),
                ..Default::default()
            },
            ..Default::default()
        });
    assert!(
        host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn selected_search_and_tool_choice_reach_each_protocol_wire() {
    use ra_model::{
        anthropic::{AnthropicMessagesModel, auth::AnthropicAuth},
        openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let agent = assemble(
        &base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch {
                mode: WebSearchMode::Live,
                ..Default::default()
            }),
    )
    .await;
    for protocol in [
        ApiProtocol::OpenAiResponses,
        ApiProtocol::OpenAiChatCompletions,
        ApiProtocol::AnthropicMessages,
    ] {
        let server = MockServer::start().await;
        let payload = match protocol {
            ApiProtocol::OpenAiResponses => {
                json!({"id":"response", "output":[], "status":"completed"})
            }
            ApiProtocol::OpenAiChatCompletions => {
                json!({"id":"chat", "choices":[{"message":{"role":"assistant", "content":"done"}, "finish_reason":"stop"}]})
            }
            _ => {
                json!({"id":"message", "type":"message", "role":"assistant", "model":"test", "content":[{"type":"text", "text":"done"}], "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":1}})
            }
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server)
            .await;
        let endpoint = format!("{}/v1", server.uri());
        let model: Box<dyn Model> = match protocol {
            ApiProtocol::OpenAiResponses => Box::new(
                OpenAiResponsesModel::new("test", OpenAiAuth::new("test").with_base_url(endpoint))
                    .unwrap(),
            ),
            ApiProtocol::OpenAiChatCompletions => Box::new(
                OpenAiChatModel::new("test", OpenAiAuth::new("test").with_base_url(endpoint))
                    .unwrap(),
            ),
            _ => Box::new(
                AnthropicMessagesModel::new(
                    "test",
                    AnthropicAuth::new("test").with_base_url(endpoint),
                )
                .unwrap(),
            ),
        };
        model
            .get_response(prepare(&agent, protocol).await.into_request())
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(!serialized.contains("coding_web_fallback"));
        assert!(!serialized.contains("recipient_binding"));
        let tools = body["tools"].as_array().unwrap();
        match protocol {
            ApiProtocol::OpenAiResponses => {
                assert_eq!(
                    tools
                        .iter()
                        .filter(|tool| tool["type"] == "web_search")
                        .count(),
                    1
                );
                assert_eq!(body["tool_choice"], json!({"type":"web_search"}));
            }
            ApiProtocol::OpenAiChatCompletions => {
                assert_eq!(
                    tools
                        .iter()
                        .filter(|tool| tool["function"]["name"] == "web_search")
                        .count(),
                    1
                );
                assert_eq!(
                    body["tool_choice"],
                    json!({"type":"function", "function":{"name":"web_search"}})
                );
            }
            _ => {
                assert_eq!(
                    tools
                        .iter()
                        .filter(|tool| tool["name"] == "web_search")
                        .count(),
                    1
                );
                assert_eq!(
                    body["tool_choice"],
                    json!({"type":"tool", "name":"web_search"})
                );
            }
        }
    }
}

#[tokio::test]
async fn hosted_search_history_remains_an_explicit_chat_replay_limit() {
    use ra_core::item::{HostedToolCall, ModelInputItem, RawProviderItem};
    use ra_model::openai::{auth::OpenAiAuth, chat::OpenAiChatModel};
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let agent = assemble(
        &base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch {
                mode: WebSearchMode::Live,
                ..Default::default()
            }),
    )
    .await;
    let request = prepare(&agent, ApiProtocol::OpenAiChatCompletions).await.into_request().with_input(vec![
        ModelInputItem::HostedToolCall(HostedToolCall::new("web_search", RawProviderItem::new("openai",
            json!({"id":"web", "type":"web_search_call", "status":"completed", "action":{"type":"search", "query":"Rust"}}))))
    ]);
    let model = OpenAiChatModel::new(
        "test",
        OpenAiAuth::new("test").with_base_url("http://127.0.0.1:1/v1"),
    )
    .unwrap();
    let error = model.get_response(request).await.unwrap_err();
    assert!(error.to_string().contains("hosted_tool_call"), "{error}");
}

struct ProvidedTools(Vec<Arc<dyn Tool>>);
#[async_trait]
impl ra_core::tool::ToolResolver for ProvidedTools {
    async fn get_tools(
        &self,
        _: &RunContext,
        _: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn an_unrelated_same_name_dynamic_tool_does_not_suppress_hosted_search() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let agent = assemble(
        &base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch {
                prefer_standalone: true,
                ..Default::default()
            }),
    )
    .await;
    let unrelated: Arc<dyn Tool> =
        Arc::new(ra_tools::web::WebSearchTool::new(services.clone()).unwrap());
    let agent = agent
        .to_builder()
        .tool_resolver(Arc::new(ProvidedTools(vec![unrelated.clone()])))
        .build()
        .unwrap();
    let turn = prepare(&agent, ApiProtocol::OpenAiResponses).await;
    assert!(
        turn.request()
            .tools()
            .iter()
            .any(|tool| tool.is_hosted() && tool.name() == "web_search")
    );
    assert!(
        turn.tools()
            .iter()
            .any(|tool| Arc::ptr_eq(tool, &unrelated))
    );
    assert_eq!(turn.request().tools()[0].name(), "web_search");
    assert!(!turn.request().tools()[0].is_hosted());
}

#[tokio::test]
async fn serialized_same_name_resolver_approvals_keep_their_original_backend() {
    use ra_core::{
        item::{ItemId, Message, OutputPhase, RunItem, RunItemKind, ToolCall},
        permission::{PermissionDecision, PermissionRule},
        state::RunState,
    };
    use ra_runtime::{
        runner::{RunConfig, RunOutcome, RunRequest, Runner},
        turn::prepare::ToolNameCollisionPolicy,
    };
    for collision in [
        ToolNameCollisionPolicy::Warn,
        ToolNameCollisionPolicy::Error,
    ] {
        for (name, arguments, expected) in [
            (
                "web_search",
                json!({"query":"Rust", "max_results":null}),
                "search:Rust",
            ),
            (
                "web_fetch",
                json!({"url":"https://example.test/guide"}),
                "fetch:https://example.test/guide",
            ),
        ] {
            let workspace = TempDir::new().unwrap();
            let fallback_access = Arc::new(configured_host::Services::default());
            let resolver_access = Arc::new(configured_host::Services::default());
            let provided: Arc<dyn Tool> = if name == "web_search" {
                Arc::new(ra_tools::web::WebSearchTool::new(resolver_access.clone()).unwrap())
            } else {
                Arc::new(ra_tools::web::WebFetchTool::new(resolver_access.clone()).unwrap())
            };
            assert_eq!(provided.recipient_binding(), None);
            let agent = assemble(
                &base(&workspace, &fallback_access)
                    .with_web_access_fallback(fallback_access.clone())
                    .with_web_search(CodingWebSearch {
                        mode: WebSearchMode::Live,
                        ..Default::default()
                    }),
            )
            .await
            .to_builder()
            .tool_resolver(Arc::new(ProvidedTools(vec![provided])))
            .build()
            .unwrap();
            let request = |model| {
                RunRequest::new(
                    AgentBinding::direct(agent.clone()),
                    Arc::new(RunResolver(ApiProtocol::OpenAiResponses, model)),
                    RunId::new("resolver-approval"),
                    CancelScope::root(),
                    vec![],
                )
                .with_config(
                    RunConfig::new()
                        .with_tool_name_collision_policy(collision)
                        .with_permission_rules([
                            PermissionRule::new(PermissionDecision::Ask).with_tool_name(name)
                        ]),
                )
            };
            let initial = Arc::new(Scripted {
                responses: Mutex::new(vec![ModelResponse::new(vec![RunItem::new(
                    ItemId::new("web-call"),
                    RunItemKind::ToolCall(ToolCall::new(CallId::new("approved"), name, arguments)),
                )])]),
                requests: Mutex::new(vec![]),
            });
            let paused = Runner::run(request(initial)).await.unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected resolver approval");
            };
            let RunItemKind::ToolApproval(approval) = items[0].kind() else {
                panic!("expected tool approval");
            };
            assert_eq!(approval.recipient_binding(), None);
            let mut state: RunState =
                serde_json::from_str(&serde_json::to_string(paused.state()).unwrap()).unwrap();
            state.approve(&items[0], false).unwrap();
            let final_model = Arc::new(Scripted {
                responses: Mutex::new(vec![ModelResponse::new(vec![RunItem::new(
                    ItemId::new("finished"),
                    RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
                )])]),
                requests: Mutex::new(vec![]),
            });
            let completed = Runner::run(request(final_model).with_state(state))
                .await
                .unwrap();
            assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(*resolver_access.calls.lock().unwrap(), [expected]);
            assert!(
                !fallback_access
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|call| call.starts_with("search:") || call.starts_with("fetch:"))
            );
        }
    }
}

#[tokio::test]
async fn the_final_turn_budget_counts_only_the_selected_search_shape() {
    use ra_runtime::turn::prepare::ActionSurfaceBudget;
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let agent = assemble(
        &base(&workspace, &services)
            .with_web_access_fallback(services.clone())
            .with_web_search(CodingWebSearch {
                mode: WebSearchMode::Live,
                ..Default::default()
            }),
    )
    .await;
    for protocol in [
        ApiProtocol::OpenAiResponses,
        ApiProtocol::OpenAiChatCompletions,
    ] {
        let count = prepare(&agent, protocol).await.request().tools().len();
        for limit in [count, count - 1] {
            let result = prepare_turn(
                TurnPreparationRequest::new(
                    &AgentBinding::direct(agent.clone()),
                    &Resolver(protocol),
                    &RunContext::new(RunId::new("budget"), agent.as_ref()),
                    &CancelScope::root(),
                    &ToolUseTracker::new(),
                    vec![],
                )
                .with_action_surface_budget(ActionSurfaceBudget::new(limit)),
            )
            .await;
            assert_eq!(result.is_ok(), limit == count);
        }
    }
}

#[tokio::test]
async fn permission_profile_changes_resolve_the_effective_fallback_mode_each_turn() {
    use ra_coding::thread_settings::CodingThreadSettingsAdapter;
    use ra_protocol::server::protocol::SandboxPolicy;
    use ra_runtime::runner::RunConfig;
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let agent =
        assemble(&base(&workspace, &services).with_web_access_fallback(services.clone())).await;
    for (policy, expected) in [
        (
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            (0, 0, false),
        ),
        (SandboxPolicy::DangerFullAccess, (0, 1, true)),
    ] {
        let config = CodingThreadSettingsAdapter::new(workspace.path(), policy)
            .unwrap()
            .initialize(RunConfig::new());
        let context = RunContext::new(RunId::new("permissions"), agent.as_ref())
            .with_app_context(config.app_context().unwrap().clone());
        let turn = prepare_turn(TurnPreparationRequest::new(
            &AgentBinding::direct(agent.clone()),
            &Resolver(ApiProtocol::OpenAiChatCompletions),
            &context,
            &CancelScope::root(),
            &ToolUseTracker::new(),
            vec![],
        ))
        .await
        .unwrap();
        assert_eq!(shape(&turn), expected);
    }
}

#[tokio::test]
async fn resumed_calls_reject_stricter_current_modes_before_binding_the_backend() {
    for mode in [
        WebSearchMode::Cached,
        WebSearchMode::Indexed,
        WebSearchMode::Disabled,
    ] {
        let workspace = TempDir::new().unwrap();
        let services = Arc::new(configured_host::Services::default());
        let adapter = Arc::new(Adapter {
            access: services.clone(),
            modes: vec![
                WebSearchMode::Cached,
                WebSearchMode::Indexed,
                WebSearchMode::Live,
            ],
            policies: Mutex::new(vec![]),
        });
        let agent = assemble(
            &base(&workspace, &services)
                .with_web_access_fallback_adapter(adapter.clone())
                .with_web_search(CodingWebSearch {
                    mode,
                    ..Default::default()
                }),
        )
        .await;
        let approved = serde_json::to_value(WebSearchPolicy {
            mode: WebSearchMode::Live,
            tools: WebSearchToolConfig::default(),
        })
        .unwrap();
        let context = RunContext::new(RunId::new("narrowed"), agent.as_ref());
        for (name, arguments) in [
            ("web_search", json!({"query":"Rust", "max_results":null})),
            ("web_fetch", json!({"url":"https://example.test/guide"})),
        ] {
            let tool = agent
                .tools()
                .iter()
                .find(|tool| tool.origin().name() == name)
                .unwrap();
            let call_id = CallId::new("approved");
            let error = tool
                .call(
                    ToolContext::new(&context, tool.as_ref(), &call_id, &arguments)
                        .with_resume_state(&approved),
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("current search policy"),
                "{error}"
            );
        }
        assert!(adapter.policies.lock().unwrap().is_empty());
        assert!(
            !services
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("search:") || call.starts_with("fetch:"))
        );
    }
}

#[tokio::test]
async fn resumed_calls_compare_domain_scopes_and_keep_the_approved_policy() {
    for (issued_domains, current_domains, enabled, permitted) in [
        (None, Some(vec!["example.test"]), true, false),
        (
            Some(vec!["example.test"]),
            Some(vec!["docs.example.test"]),
            true,
            false,
        ),
        (
            Some(vec!["example.test"]),
            Some(vec!["other.test"]),
            true,
            false,
        ),
        (Some(vec!["example.test"]), Some(vec![]), true, false),
        (
            Some(vec!["docs.example.test"]),
            Some(vec![".EXAMPLE.test"]),
            true,
            true,
        ),
        (Some(vec!["example.test"]), None, true, true),
        (None, None, false, false),
    ] {
        let workspace = TempDir::new().unwrap();
        let services = Arc::new(configured_host::Services::default());
        let adapter = Arc::new(Adapter {
            access: services.clone(),
            modes: vec![WebSearchMode::Cached, WebSearchMode::Live],
            policies: Mutex::new(vec![]),
        });
        let domains = |values: Option<Vec<&str>>| {
            values.map(|values| values.into_iter().map(str::to_owned).collect())
        };
        let agent = assemble(
            &base(&workspace, &services)
                .with_web_access_fallback_adapter(adapter.clone())
                .with_web_search(CodingWebSearch {
                    enabled,
                    mode: WebSearchMode::Live,
                    tools: WebSearchToolConfig {
                        allowed_domains: domains(current_domains),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
        )
        .await;
        let issued = WebSearchPolicy {
            mode: WebSearchMode::Cached,
            tools: WebSearchToolConfig {
                allowed_domains: domains(issued_domains),
                context_size: Some(WebSearchContextSize::Low),
                ..Default::default()
            },
        };
        let approved = serde_json::to_value(&issued).unwrap();
        let context = RunContext::new(RunId::new("domains"), agent.as_ref());
        let tool = agent
            .tools()
            .iter()
            .find(|tool| tool.origin().name() == "web_search")
            .unwrap();
        let call_id = CallId::new("approved");
        let arguments = json!({"query":"Rust", "max_results":null});
        let result = tool
            .call(
                ToolContext::new(&context, tool.as_ref(), &call_id, &arguments)
                    .with_resume_state(&approved),
            )
            .await;
        if permitted {
            result.unwrap();
            assert_eq!(*adapter.policies.lock().unwrap(), [issued]);
            assert!(
                services
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|call| call == "search:Rust")
            );
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("current search policy")
            );
            assert!(adapter.policies.lock().unwrap().is_empty());
            assert!(
                !services
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|call| call.starts_with("search:"))
            );
        }
    }
}

#[tokio::test]
async fn serialized_search_and_fetch_approvals_respect_current_permission_profiles() {
    use ra_coding::thread_settings::CodingThreadSettingsAdapter;
    use ra_core::{
        item::{ItemId, Message, OutputPhase, RunItem, RunItemKind, ToolCall},
        permission::{PermissionDecision, PermissionRule},
        state::RunState,
    };
    use ra_protocol::server::protocol::SandboxPolicy;
    use ra_runtime::runner::{RunConfig, RunOutcome, RunRequest, Runner};
    for (name, arguments) in [
        ("web_search", json!({"query":"Rust", "max_results":null})),
        ("web_fetch", json!({"url":"https://example.test/guide"})),
    ] {
        let workspace = TempDir::new().unwrap();
        let services = Arc::new(configured_host::Services::default());
        let adapter = Arc::new(Adapter {
            access: services.clone(),
            modes: vec![WebSearchMode::Cached, WebSearchMode::Live],
            policies: Mutex::new(vec![]),
        });
        let agent = assemble(
            &base(&workspace, &services).with_web_access_fallback_adapter(adapter.clone()),
        )
        .await;
        let request = |policy, model| {
            let config = CodingThreadSettingsAdapter::new(workspace.path(), policy)
                .unwrap()
                .initialize(RunConfig::new().with_permission_rules([
                    PermissionRule::new(PermissionDecision::Ask).with_tool_name(name),
                ]));
            RunRequest::new(
                AgentBinding::direct(agent.clone()),
                Arc::new(RunResolver(ApiProtocol::OpenAiChatCompletions, model)),
                RunId::new("narrowed-approval"),
                CancelScope::root(),
                vec![],
            )
            .with_config(config)
        };
        let initial = Arc::new(Scripted {
            responses: Mutex::new(vec![ModelResponse::new(vec![RunItem::new(
                ItemId::new("web-call"),
                RunItemKind::ToolCall(ToolCall::new(CallId::new("approved"), name, arguments)),
            )])]),
            requests: Mutex::new(vec![]),
        });
        let paused = Runner::run(request(SandboxPolicy::DangerFullAccess, initial))
            .await
            .unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approval");
        };
        let mut state: RunState =
            serde_json::from_str(&serde_json::to_string(paused.state()).unwrap()).unwrap();
        state.approve(&items[0], false).unwrap();
        let final_model = Arc::new(Scripted {
            responses: Mutex::new(vec![ModelResponse::new(vec![RunItem::new(
                ItemId::new("finished"),
                RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
            )])]),
            requests: Mutex::new(vec![]),
        });
        let completed = Runner::run(
            request(
                SandboxPolicy::ReadOnly {
                    network_access: false,
                },
                final_model.clone(),
            )
            .with_state(state),
        )
        .await
        .unwrap();
        assert!(matches!(completed.outcome(), RunOutcome::Completed { .. }));
        assert!(
            serde_json::to_string(final_model.requests.lock().unwrap()[0].input())
                .unwrap()
                .contains("The web backend could not answer.")
        );
        assert!(adapter.policies.lock().unwrap().is_empty());
        assert!(
            !services
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("search:") || call.starts_with("fetch:"))
        );
    }
}
