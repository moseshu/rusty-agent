//! Full coding assembly with native discovery, MCP execution and resources on the Responses wire.

use super::*;
use ra_coding::{CodingHost, CodingProfile, build_agent_with_control};
use ra_core::{cancel::CancelScope, prompt::PromptRole};
use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunOutcome, RunRequest, Runner},
};
use wiremock::{
    Mock, MockServer, Respond, ResponseTemplate,
    matchers::{method, path},
};

#[path = "../../../support/configured_coding_host.rs"]
mod services;

struct Endpoint(AtomicUsize);

impl Respond for Endpoint {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        let step = self.0.fetch_add(1, Ordering::SeqCst);
        let output = match step {
            0 => {
                json!({"type": "tool_search_call", "id": "search", "call_id": "search", "execution": "client", "arguments": {"query": "echo_1", "limit": 1}})
            }
            1 => {
                json!({"type": "function_call", "id": "echo", "call_id": "echo", "name": "echo_1", "namespace": "echo_1", "arguments": "{}"})
            }
            2 => {
                json!({"type": "function_call", "id": "read", "call_id": "read", "name": "read_mcp_resource", "arguments": "{\"server\":\"docs\",\"uri\":\"memo://version\"}"})
            }
            3 => {
                json!({"type": "message", "id": "final", "role": "assistant", "content": [{"type": "output_text", "text": "complete", "annotations": []}]})
            }
            _ => panic!("unexpected request {step}"),
        };
        let frames = [
            json!({"type": "response.output_item.added", "output_index": 0, "item": output}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": output}),
            json!({"type": "response.completed", "response": {"id": format!("resp-{step}"), "status": "completed", "output": [output]}}),
        ];
        let body: String = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect();
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }
}

struct Models(Arc<OpenAiResponsesModel>);

impl ra_core::model::ModelResolver for Models {
    fn resolve_model(&self, _: Option<&str>) -> Result<ra_core::model::ResolvedModel> {
        use ra_core::model::{
            ApiProtocol, ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
        };
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                Some("test".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

#[tokio::test]
async fn configured_host_search_execution_and_resources_reach_the_provider() {
    let endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(Endpoint(AtomicUsize::new(0)))
        .mount(&endpoint)
        .await;
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(Probe::new("docs"))],
        McpDiscoveryOptions::new().with_tool_search(true),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let host = Arc::new(services::Services::default())
        .install(CodingHost::open(std::env::temp_dir()).unwrap())
        .with_mcp_runtime(runtime.clone());
    let built = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    let model = OpenAiResponsesModel::new(
        "test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", endpoint.uri())),
    )
    .unwrap();
    let result = Runner::run(
        built.bind(
            RunRequest::new(
                AgentBinding::direct(Arc::clone(built.agent())),
                Arc::new(Models(Arc::new(model))),
                RunId::new("configured-wire"),
                CancelScope::root(),
                vec![],
            )
            .with_config(host.build_run_config())
            .with_services(host.build_tool_services()),
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    let tools = bodies[0]["tools"].as_array().unwrap();
    // Twelve top-level functions, the six collaboration tools in Codex's `collaboration`
    // namespace, three resource entries and native search.
    assert_eq!(tools.len(), 17);
    assert!(tools.iter().any(|tool| tool["type"] == "tool_search"));
    let collaboration = tools
        .iter()
        .find(|tool| tool["type"] == "namespace" && tool["name"] == "collaboration")
        .expect("the collaboration namespace");
    assert_eq!(collaboration["tools"].as_array().unwrap().len(), 6);
    assert!(
        !tools.iter().any(|tool| tool["name"] == "agent"
            || tool["name"] == "mcp"
            || tool["name"] == "echo_1")
    );
    let limit = CodingProfile::CodexLike
        .to_tool_profile()
        .unwrap()
        .budget()
        .max_advertised_bytes()
        .unwrap();
    let bytes = serde_json::to_vec(tools).unwrap().len();
    println!("configured-native-wire-budget: {bytes}/{limit}");
    assert!(bytes <= limit);
    assert!(
        bodies[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "tool_search_output"
                && item["tools"].to_string().contains("echo_1"))
    );
    let history = serde_json::to_string(result.new_items()).unwrap();
    assert!(history.contains("called"), "{history}");
    assert!(history.contains("session generation 1"), "{history}");
    built.agent_control().unwrap().shutdown().await;
    runtime.shutdown().await.unwrap();
}
