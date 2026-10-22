// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex-style native hosted search assembly and per-turn access-mode resolution.

#[path = "../../support/configured_coding_host.rs"]
mod configured_host;

use std::sync::Arc;

use async_trait::async_trait;
use ra_coding::{
    CodingHost, CodingProfile, build_agent_with_control, host_backed_surface,
    web_search::{CodingWebSearch, WebSearchMode, WebSearchToolConfig, WebSearchToolType},
};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::ModelResponse,
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings,
        ModelToolKind, ProviderKey, ResolvedModel, ToolChoice,
    },
    prompt::PromptRole,
    state::{RunId, ToolUseTracker},
};
use ra_model::openai::responses::web_search::{WebSearchContextSize, WebSearchUserLocation};
use ra_protocol::server::protocol::SandboxPolicy;
use ra_runtime::{
    agent::AgentBinding,
    runner::RunConfig,
    turn::prepare::{ActionSurfaceBudget, TurnPreparationRequest, prepare_turn},
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Resolver;
struct ProtocolResolver {
    protocol: ApiProtocol,
    model: Arc<dyn Model>,
}
struct UncalledModel;
#[async_trait]
impl Model for UncalledModel {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("preparation must not issue a model request"))
    }
}
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                None,
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(UncalledModel),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

impl ModelResolver for ProtocolResolver {
    fn resolve_model(&self, model: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                model.map(str::to_owned),
                self.protocol,
            ),
            self.model.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn native(request: &ModelRequest) -> Option<&Value> {
    request.tools().iter().find_map(|tool| match tool.kind() {
        ModelToolKind::Hosted { declaration, .. } => Some(declaration),
        _ => None,
    })
}

#[tokio::test]
async fn native_search_assembles_without_webaccess_and_is_counted_in_inventory() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_skill_catalog(services.clone())
        .with_user_input(services.clone())
        .with_collaboration()
        .with_web_search(CodingWebSearch::default());
    let assembled = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap();
    let surface = assembled.tool_surface();
    assert_eq!(surface.hosted_tools().len(), 1);
    assert!(surface.advertised_names().any(|name| name == "web_search"));
    assert!(!surface.advertised_names().any(|name| name == "web_fetch"));
    assert!(
        !surface
            .tools()
            .iter()
            .any(|tool| tool.origin().name() == "web_search")
    );
    assert_eq!(surface.advertised_count(), surface.tools().len() + 1);
    assert_eq!(
        surface.model_definitions().len(),
        surface.advertised_count()
    );
    assert!(
        assembled.installed().iter().any(
            |family| family.family().as_str() == "web" && family.advertised() == ["web_search"]
        )
    );
    let agent = build_agent_with_control(
        AgentId::new("worker"),
        "Worker",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    // Hosted search is declared in the request's tool table, not in the stable inventory: each
    // turn decides from its resolved protocol whether the request carries it.
    let instructions = agent
        .agent()
        .instructions()
        .unwrap()
        .as_static()
        .unwrap()
        .to_owned();
    assert!(!instructions.contains("web_search"), "{instructions}");
    assert!(!instructions.contains("web_fetch"), "{instructions}");
    assert!(instructions.contains("any built-in provider tool included in this request"));
    let binding = AgentBinding::direct(agent.agent().clone());
    let context = RunContext::new(RunId::new("native"), agent.agent().as_ref());
    let turn = prepare_turn(
        TurnPreparationRequest::new(
            &binding,
            &Resolver,
            &context,
            &CancelScope::root(),
            &ToolUseTracker::default(),
            vec![],
        )
        .with_model_settings(ModelSettings::new().with_tool_choice(ToolChoice::Required)),
    )
    .await
    .unwrap();
    assert_eq!(
        native(turn.request()),
        Some(&json!({"type":"web_search","external_web_access":false}))
    );
    assert_eq!(
        turn.request().model_settings().tool_choice(),
        Some(&ToolChoice::Required)
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
async fn product_modes_and_options_match_the_codex_specification() {
    for (mode, expected) in [
        (WebSearchMode::Disabled, None),
        (WebSearchMode::Cached, Some((false, None))),
        (WebSearchMode::Live, Some((true, None))),
        (WebSearchMode::Indexed, Some((true, Some(true)))),
    ] {
        let config = CodingWebSearch {
            mode,
            tool_type: WebSearchToolType::TextAndImage,
            tools: WebSearchToolConfig {
                context_size: Some(WebSearchContextSize::High),
                allowed_domains: Some(vec!["example.com".into()]),
                location: Some(WebSearchUserLocation {
                    country: Some("US".into()),
                    city: Some("New York".into()),
                    timezone: Some("America/New_York".into()),
                    ..WebSearchUserLocation::default()
                }),
            },
            ..CodingWebSearch::default()
        };
        let definition = config.definition_for_turn(false).unwrap();
        if let Some((external, indexed)) = expected {
            let ModelToolKind::Hosted {
                declaration,
                includes,
                ..
            } = definition.unwrap().kind().clone()
            else {
                panic!("hosted declaration")
            };
            assert_eq!(declaration["external_web_access"], external);
            assert_eq!(
                declaration.get("indexed_web_access"),
                indexed.map(|value| if value {
                    &Value::Bool(true)
                } else {
                    &Value::Bool(false)
                })
            );
            assert_eq!(declaration["search_context_size"], "high");
            assert_eq!(
                declaration["filters"],
                json!({"allowed_domains":["example.com"]})
            );
            assert_eq!(
                declaration["user_location"],
                json!({"type":"approximate","country":"US","city":"New York","timezone":"America/New_York"})
            );
            assert_eq!(
                declaration["search_content_types"],
                json!(["text", "image"])
            );
            assert_eq!(includes, ["web_search_call.results"]);
        } else {
            assert!(definition.is_none());
        }
    }
}

#[test]
fn permission_profile_and_provider_limits_use_codex_fallback_order() {
    use WebSearchMode::{Cached, Disabled, Indexed, Live};
    let config = CodingWebSearch::default();
    assert_eq!(config.mode_for_turn(false), Cached);
    assert_eq!(config.mode_for_turn(true), Live);
    assert_eq!(
        CodingWebSearch {
            mode: Indexed,
            ..config.clone()
        }
        .mode_for_turn(true),
        Indexed
    );
    assert_eq!(
        CodingWebSearch {
            mode: Disabled,
            ..config.clone()
        }
        .mode_for_turn(true),
        Disabled
    );
    for mode in [Live, Indexed] {
        let config = CodingWebSearch {
            mode,
            external_web_access: false,
            ..config.clone()
        };
        assert_eq!(config.mode_for_turn(true), Cached);
        assert_eq!(
            CodingWebSearch {
                allowed_modes: vec![mode],
                ..config
            }
            .mode_for_turn(true),
            Disabled
        );
    }
    assert_eq!(
        CodingWebSearch {
            supported: false,
            ..config
        }
        .mode_for_turn(true),
        Disabled
    );
}

#[test]
fn configuration_merge_replaces_domains_and_merges_location_fields() {
    let base:WebSearchToolConfig=serde_json::from_value(json!({"context_size":"high","allowed_domains":["base.com"],"location":{"country":"US","region":"California","city":"San Francisco"}})).unwrap();
    let over: WebSearchToolConfig = serde_json::from_value(
        json!({"allowed_domains":[],"location":{"city":"New York","timezone":"America/New_York"}}),
    )
    .unwrap();
    let merged = base.merge(&over);
    assert_eq!(merged.context_size, Some(WebSearchContextSize::High));
    assert_eq!(merged.allowed_domains, Some(vec![]));
    assert_eq!(
        merged.location.unwrap(),
        WebSearchUserLocation {
            country: Some("US".into()),
            region: Some("California".into()),
            city: Some("New York".into()),
            timezone: Some("America/New_York".into()),
            ..WebSearchUserLocation::default()
        }
    );
}

#[tokio::test]
async fn permission_changes_are_resolved_on_each_preparation_and_hosted_tools_consume_budget() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_skill_catalog(services.clone())
        .with_user_input(services.clone())
        .with_collaboration()
        .with_web_search(CodingWebSearch::default());
    let assembled = build_agent_with_control(
        AgentId::new("worker"),
        "Worker",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    let agent = assembled.agent();
    let binding = AgentBinding::direct(agent.clone());
    for (policy, expected) in [
        (
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            false,
        ),
        (SandboxPolicy::DangerFullAccess, true),
    ] {
        let adapter =
            ra_coding::thread_settings::CodingThreadSettingsAdapter::new(workspace.path(), policy)
                .unwrap();
        let config = adapter.initialize(RunConfig::new());
        let context = RunContext::new(RunId::new("permissions"), agent.as_ref())
            .with_app_context(config.app_context().unwrap().clone());
        let turn = prepare_turn(TurnPreparationRequest::new(
            &binding,
            &Resolver,
            &context,
            &CancelScope::root(),
            &ToolUseTracker::default(),
            vec![],
        ))
        .await
        .unwrap();
        assert_eq!(
            native(turn.request()).unwrap()["external_web_access"],
            expected
        );
    }
    let only_hosted = AgentSpec::builder()
        .id(AgentId::new("only-web"))
        .name("Web")
        .hosted_tool(
            CodingWebSearch::default()
                .definition_for_turn(false)
                .unwrap()
                .unwrap(),
        )
        .build()
        .unwrap();
    let binding = AgentBinding::direct(only_hosted.clone());
    let context = RunContext::new(RunId::new("budget"), only_hosted.as_ref());
    assert!(
        prepare_turn(
            TurnPreparationRequest::new(
                &binding,
                &Resolver,
                &context,
                &CancelScope::root(),
                &ToolUseTracker::default(),
                vec![]
            )
            .with_action_surface_budget(ActionSurfaceBudget::new(0))
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn core_one_off_disabled_and_explicit_webaccess_surfaces_remain_distinct() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let extension = services.install(CodingHost::open(workspace.path()).unwrap());
    let native_host = extension
        .clone()
        .with_web_search(CodingWebSearch::default());
    for (host, role, profile) in [
        (&native_host, PromptRole::Main, CodingProfile::Core),
        (
            &native_host,
            PromptRole::OneOffAnswer,
            CodingProfile::CodexLike,
        ),
    ] {
        let assembled = host_backed_surface(&role, host, profile).await.unwrap();
        assert!(assembled.tool_surface().hosted_tools().is_empty());
        assert!(
            !assembled
                .tool_surface()
                .advertised_names()
                .any(|name| name == "web_search")
        );
    }
    let disabled = native_host.with_web_search(CodingWebSearch {
        mode: WebSearchMode::Disabled,
        ..CodingWebSearch::default()
    });
    assert!(
        host_backed_surface(&PromptRole::Main, &disabled, CodingProfile::CodexLike)
            .await
            .unwrap()
            .tool_surface()
            .hosted_tools()
            .is_empty()
    );
    let assembled = host_backed_surface(&PromptRole::Main, &extension, CodingProfile::CodexLike)
        .await
        .unwrap();
    assert!(assembled.tool_surface().hosted_tools().is_empty());
    assert!(
        assembled
            .tool_surface()
            .advertised_names()
            .any(|name| name == "web_search")
    );
    assert!(
        assembled
            .tool_surface()
            .advertised_names()
            .any(|name| name == "web_fetch")
    );
}

#[tokio::test]
async fn configured_coding_search_reaches_the_native_http_tool_table() {
    use futures::StreamExt;
    use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    for streaming in [false, true] {
        let workspace = TempDir::new().unwrap();
        let services = Arc::new(configured_host::Services::default());
        let host = CodingHost::open(workspace.path())
            .unwrap()
            .with_skill_catalog(services.clone())
            .with_user_input(services.clone())
            .with_collaboration()
            .with_web_search(CodingWebSearch {
                mode: WebSearchMode::Indexed,
                tool_type: WebSearchToolType::TextAndImage,
                tools: WebSearchToolConfig {
                    context_size: Some(WebSearchContextSize::High),
                    allowed_domains: Some(vec!["example.com".into()]),
                    ..WebSearchToolConfig::default()
                },
                ..CodingWebSearch::default()
            });
        let agent = build_agent_with_control(
            AgentId::new("coding-web"),
            "Coding",
            &PromptRole::Main,
            &host,
            CodingProfile::CodexLike,
        )
        .await
        .unwrap();
        let binding = AgentBinding::direct(agent.agent().clone());
        let context = RunContext::new(RunId::new("coding-wire"), agent.agent().as_ref());
        let turn = prepare_turn(
            TurnPreparationRequest::new(
                &binding,
                &Resolver,
                &context,
                &CancelScope::root(),
                &ToolUseTracker::default(),
                vec![],
            )
            .with_model_settings(
                ModelSettings::new().with_tool_choice(ToolChoice::Tool("web_search".into())),
            ),
        )
        .await
        .unwrap();
        let server = MockServer::start().await;
        let response = json!({"id":"coding-web-response","output":[],"status":"completed"});
        let template = if streaming {
            ResponseTemplate::new(200).set_body_raw(
                format!(
                    "data: {}\n\n",
                    json!({"type":"response.completed","response":response})
                ),
                "text/event-stream",
            )
        } else {
            ResponseTemplate::new(200).set_body_json(response)
        };
        Mock::given(method("POST"))
            .respond_with(template)
            .mount(&server)
            .await;
        let model = OpenAiResponsesModel::new(
            "coding-web-model",
            OpenAiAuth::new("test").with_base_url(format!("{}/v1", server.uri())),
        )
        .unwrap();
        if streaming {
            let mut stream = model.stream_response(turn.into_request());
            while let Some(event) = stream.next().await {
                event.unwrap();
            }
        } else {
            model.get_response(turn.into_request()).await.unwrap();
        }
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let tools = body["tools"].as_array().unwrap();
        let native = tools
            .iter()
            .find(|tool| tool["type"] == "web_search")
            .unwrap();
        assert_eq!(
            native,
            &json!({"type":"web_search","external_web_access":true,"indexed_web_access":true,"search_context_size":"high","filters":{"allowed_domains":["example.com"]},"search_content_types":["text","image"]})
        );
        assert!(
            !tools
                .iter()
                .any(|tool| tool["name"] == "web_search" || tool["name"] == "web_fetch")
        );
        assert_eq!(body["tool_choice"], json!({"type":"web_search"}));
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content", "web_search_call.results"])
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
}

#[rstest::rstest]
#[case::codex_chat(CodingProfile::CodexLike, ApiProtocol::OpenAiChatCompletions)]
#[case::codex_anthropic(CodingProfile::CodexLike, ApiProtocol::AnthropicMessages)]
#[case::full_chat(CodingProfile::Full, ApiProtocol::OpenAiChatCompletions)]
#[case::full_anthropic(CodingProfile::Full, ApiProtocol::AnthropicMessages)]
#[tokio::test]
async fn default_native_search_follows_the_resolved_turn_protocol(
    #[case] profile: CodingProfile,
    #[case] protocol: ApiProtocol,
) {
    use ra_model::{
        anthropic::{AnthropicAuth, AnthropicMessagesModel},
        openai::{
            auth::OpenAiAuth,
            chat::{ChatLoweringOptions, OpenAiChatModel},
        },
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_skill_catalog(services.clone())
        .with_user_input(services)
        .with_collaboration()
        .with_collaboration_namespace(None);
    let agent = build_agent_with_control(
        AgentId::new("default-search"),
        "Coding",
        &PromptRole::Main,
        &host,
        profile,
    )
    .await
    .unwrap();
    // The stable prefix is shared by every protocol this agent may meet, so it never names the
    // hosted entry that Chat and Anthropic turns omit.
    assert!(
        !agent
            .agent()
            .instructions()
            .unwrap()
            .as_static()
            .unwrap()
            .contains("web_search")
    );
    let binding = AgentBinding::direct(agent.agent().clone());
    let context = RunContext::new(RunId::new("protocol-switch"), agent.agent().as_ref());
    let server = MockServer::start().await;
    let base_url = format!("{}/v1/", server.uri());
    let (model, response): (Arc<dyn Model>, Value) = match protocol {
        ApiProtocol::OpenAiChatCompletions => (
            Arc::new(
                OpenAiChatModel::new(
                    "test-model",
                    OpenAiAuth::new("test").with_base_url(base_url),
                )
                .unwrap()
                .with_lowering_options(
                    ChatLoweringOptions::default().with_custom_tools_as_functions(true),
                ),
            ),
            json!({"id":"chat-default","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}]}),
        ),
        ApiProtocol::AnthropicMessages => (
            Arc::new(
                AnthropicMessagesModel::new(
                    "test-model",
                    AnthropicAuth::new("test").with_base_url(base_url),
                )
                .unwrap()
                .with_custom_tools_as_functions(true),
            ),
            json!({"id":"message-default","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}),
        ),
        _ => unreachable!("this regression covers Chat and Anthropic"),
    };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
        .mount(&server)
        .await;
    let resolver = ProtocolResolver { protocol, model };
    let cancel = CancelScope::root();
    let tool_use = ToolUseTracker::default();

    // The same immutable agent can switch protocols between turns without losing native search.
    for active_resolver in [&Resolver as &dyn ModelResolver, &resolver, &Resolver] {
        let turn = prepare_turn(
            TurnPreparationRequest::new(
                &binding,
                active_resolver,
                &context,
                &cancel,
                &tool_use,
                vec![],
            )
            .with_model("test-provider/selected-model")
            .with_model_settings(
                ModelSettings::new()
                    .with_max_tokens(1024)
                    .with_tool_choice(ToolChoice::Required),
            ),
        )
        .await
        .unwrap();
        if turn.selector().protocol() == ApiProtocol::OpenAiResponses {
            assert!(native(turn.request()).is_some());
            continue;
        }
        assert_eq!(turn.selector().protocol(), protocol);
        assert_eq!(
            turn.selector().model(),
            Some("test-provider/selected-model")
        );
        assert!(native(turn.request()).is_none());
        assert!(turn.request().tools().iter().all(|tool| !tool.is_hosted()));
        assert!(
            !turn
                .action_surface()
                .advertised_names()
                .any(|name| name == "web_search" || name == "web_fetch")
        );
        assert_eq!(turn.action_surface().advertised_count(), turn.tools().len());
        assert_eq!(
            turn.request().model_settings().tool_choice(),
            Some(&ToolChoice::Required)
        );
        let model = turn.model().clone();
        model.get_response(turn.into_request()).await.unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let expected_path = match protocol {
        ApiProtocol::OpenAiChatCompletions => "/v1/chat/completions",
        ApiProtocol::AnthropicMessages => "/v1/messages",
        _ => unreachable!(),
    };
    assert_eq!(requests[0].url.path(), expected_path);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body["tools"].as_array().unwrap().iter().all(|tool| {
        tool["type"] != "web_search"
            && tool["name"] != "web_search"
            && tool["name"] != "web_fetch"
            && tool["function"]["name"] != "web_search"
            && tool["function"]["name"] != "web_fetch"
    }));
}

#[tokio::test]
async fn custom_web_functions_cannot_silently_replace_selected_native_search() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_skill_catalog(services.clone())
        .with_user_input(services.clone())
        .with_collaboration()
        .with_capability(Arc::new(
            ra_tools::capability::WebCapability::new(services).unwrap(),
        ));
    let error = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("selection key"), "{error}");
}
