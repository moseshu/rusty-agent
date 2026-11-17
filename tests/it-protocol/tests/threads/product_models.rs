//! Product configuration exercised through the app server and a Responses HTTP endpoint.

use super::*;
use ra_coding::config::ConfigBuilder;
use ra_config::LoaderOverrides;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct ProductModels {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl ProductModels {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            workspace: tempfile::tempdir().unwrap(),
        }
    }

    fn write(&self, filename: &str, text: &str) {
        std::fs::write(self.home.path().join(filename), text).unwrap();
    }

    fn server(&self, resolver: Option<Arc<dyn ModelResolver>>) -> Server {
        self.server_with_role(resolver, ra_core::prompt::PromptRole::Main)
    }

    fn server_with_role(
        &self,
        resolver: Option<Arc<dyn ModelResolver>>,
        role: ra_core::prompt::PromptRole,
    ) -> Server {
        let root = self.workspace.path().canonicalize().unwrap();
        let home = self.home.path().canonicalize().unwrap();
        let host = CodingHost::open(&root).unwrap();
        let settings = CodingThreadSettingsAdapter::new(
            &root,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap()
        .with_rules_home(&home);
        let builder = ConfigBuilder::default()
            .codex_home(home.clone())
            .loader_overrides(LoaderOverrides {
                managed_config_path: Some(home.join("managed_config.toml")),
                system_config_path: Some(home.join("system_config.toml")),
                system_requirements_path: Some(home.join("requirements.toml")),
                ..LoaderOverrides::without_managed_config_for_tests()
            });
        let factory = match resolver {
            Some(resolver) => {
                CodingThreadFactory::new(host, resolver, settings).with_product_config(builder)
            }
            None => CodingThreadFactory::from_product_config(host, settings, builder),
        }
        .with_role(role);
        build(
            Options {
                store: Some(Arc::new(InMemoryThreadStore::new())),
                factory: Some(Arc::new(factory)),
                unload_delay: None,
            },
            None,
        )
        .0
    }
}

async fn responses_endpoint() -> MockServer {
    let message = json!({
        "id": "msg_product", "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "Done"}]
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_product", "status": "completed", "output": [message],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    let endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {completed}\n\n"), "text/event-stream"),
        )
        .mount(&endpoint)
        .await;
    endpoint
}

async fn start(server: &Server, connection: &mut Connection, params: Value) -> (String, Value) {
    let started = server.call(connection, "thread/start", params).await;
    (
        started["thread"]["id"].as_str().unwrap().to_owned(),
        started,
    )
}

#[tokio::test]
async fn product_models_reach_selected_provider_with_catalog_and_config_defaults() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    let catalog = json!({"models": [{
        "slug": "vendor/default-model", "display_name": "Default model",
        "default_reasoning_level": "low", "supported_reasoning_levels": [],
        "shell_type": "unified_exec", "visibility": "list", "supported_in_api": true,
        "priority": 1, "support_verbosity": true, "default_verbosity": "medium",
        "truncation_policy": {"mode": "tokens", "limit": 1000},
        "experimental_supported_tools": [], "context_window": 8000,
        "model_messages": {"instructions_template": "Catalog instructions"}
    }]});
    product.write("catalog.json", &catalog.to_string());
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "unused"
model_reasoning_effort = "minimal"
model_reasoning_summary = "detailed"
model_verbosity = "high"
service_tier = "flex"
developer_instructions = "Product developer instructions"
model_catalog_json = {:?}
[responses_api_metadata]
deployment = "test"
window_number = "999"
misalignment_override = "targeted_continuation"
"#,
            product.home.path().join("catalog.json")
        ),
    );
    product.write(
        "requirements.toml",
        &format!(
            r#"
model_provider = "gateway"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1/"
experimental_bearer_token = "test-token"
stream_max_retries = 0
[model_providers.gateway.http_headers]
x-product-header = "configured"
[model_providers.gateway.query_params]
api_version = "product"
"#,
            endpoint.uri()
        ),
    );

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "vendor/default-model");
    assert_eq!(started["modelProvider"], "gateway");
    server
        .run_turn(&mut connection, &thread, "Use product configuration")
        .await;

    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.headers["authorization"], "Bearer test-token");
    assert_eq!(request.headers["x-product-header"], "configured");
    assert_eq!(request.url.query(), Some("api_version=product"));
    let body: Value = request.body_json().unwrap();
    assert_eq!(body["model"], "vendor/default-model");
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("You are the main autonomous agent.")
    );
    assert_developer_prefix(&body, "Catalog instructions");
    assert_eq!(body["input"][1]["role"], "developer");
    assert_eq!(
        body["input"][1]["content"][0]["text"],
        "Product developer instructions"
    );
    assert_eq!(
        body["reasoning"],
        json!({"effort":"minimal", "summary":"detailed"})
    );
    assert_eq!(body["text"]["verbosity"], "high");
    assert_eq!(body["service_tier"], "flex");
    assert_eq!(body["client_metadata"], json!({"deployment": "test"}));
}

#[tokio::test]
async fn explicit_thread_and_turn_model_win_over_product_model() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "product-default"
model_provider = "gateway"
instructions = "Configured base instructions"
service_tier = "flex"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1"
"#,
            endpoint.uri()
        ),
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(
        &server,
        &mut connection,
        json!({"model":"vendor/thread-model", "serviceTier":null}),
    )
    .await;
    assert_eq!(started["model"], "vendor/thread-model");
    server
        .run_turn(&mut connection, &thread, "Use thread model")
        .await;
    server
        .call(
            &mut connection,
            "turn/start",
            json!({
                "threadId": thread, "model": "vendor/turn-model", "effort": "high",
                "input": [{"type":"text", "text":"Use turn model", "text_elements":[]}]
            }),
        )
        .await;
    connection.notification("turn/completed").await;
    let requests = endpoint.received_requests().await.unwrap();
    let bodies = requests
        .iter()
        .map(|request| request.body_json::<Value>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["model"], "vendor/thread-model");
    assert_eq!(bodies[1]["model"], "vendor/turn-model");
    assert_eq!(bodies[1]["reasoning"]["effort"], "high");
    assert!(bodies.iter().all(|body| body.get("service_tier").is_none()));
    for body in &bodies {
        assert_developer_prefix(body, "Configured base instructions");
    }
    assert_eq!(bodies[0]["input"][0]["id"], bodies[1]["input"][0]["id"]);
}

fn assert_developer_prefix(body: &Value, text: &str) {
    let prefix = &body["input"][0];
    assert_eq!(prefix["type"], "message");
    assert_eq!(prefix["role"], "developer");
    assert_eq!(
        prefix["content"],
        json!([{"type": "input_text", "text": text}])
    );
    let id = prefix["id"].as_str().unwrap();
    assert!(id.starts_with("msg_"));
    assert_eq!(id.len(), "msg_".len() + 36);
}

#[tokio::test]
async fn product_model_retains_the_read_only_specialist_prefix() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "product-default"
model_provider = "gateway"
instructions = "Configured base instructions"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1"
"#,
            endpoint.uri()
        ),
    );
    let server = product.server_with_role(None, ra_core::prompt::PromptRole::ReadOnlySpecialist);
    let mut connection = server.connect(1).await;
    for _ in 0..2 {
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        server
            .run_turn(&mut connection, &thread, "Inspect the repository")
            .await;
    }
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let bodies = requests
        .iter()
        .map(|request| request.body_json::<Value>().unwrap())
        .collect::<Vec<_>>();
    for body in &bodies {
        assert_developer_prefix(body, "Configured base instructions");
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.contains("You are a read-only specialist agent."));
        assert!(
            instructions.contains(
                "Use tools only for non-mutating exploration; do not edit repository files"
            )
        );
    }
    assert_ne!(bodies[0]["input"][0]["id"], bodies[1]["input"][0]["id"]);
}

#[tokio::test]
async fn injected_resolver_retains_ownership_of_product_thread_requests() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "configured-model"
model_provider = "gateway"
model_reasoning_effort = "minimal"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1"
env_key = "RUSTY_AGENT_TEST_MISSING_PRODUCT_CREDENTIAL"
"#,
            endpoint.uri()
        ),
    );
    let script = Arc::new(Script::default());
    script.answer("From injected resolver");
    let server = product.server(Some(Arc::new(Resolver(Arc::clone(&script)))));
    let mut connection = server.connect(1).await;
    let (thread, started) =
        start(&server, &mut connection, json!({"model":"explicit-model"})).await;
    assert_eq!(started["model"], "explicit-model");
    assert_eq!(started["modelProvider"], "test-provider");
    server
        .run_turn(&mut connection, &thread, "Use injected resolver")
        .await;
    assert!(endpoint.received_requests().await.unwrap().is_empty());
    assert!(script.last_request().model_settings().effort().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn product_model_refreshes_command_auth_and_retries_http_server_errors() {
    use wiremock::matchers::header;

    let endpoint = responses_endpoint().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer expired"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .with_priority(1)
        .mount(&endpoint)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer refreshed"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&endpoint)
        .await;

    let product = ProductModels::new();
    product.write(
        "token.sh",
        r#"
if test -f issued; then
    printf 'refreshed\n'
else
    touch issued
    printf 'expired\n'
fi
"#,
    );
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "local-model"
model_provider = "gateway"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1"
request_max_retries = 1
stream_max_retries = 0
[model_providers.gateway.auth]
command = "sh"
args = ["token.sh"]
cwd = {:?}
refresh_interval_ms = 0
"#,
            endpoint.uri(),
            product.home.path().canonicalize().unwrap()
        ),
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    server
        .run_turn(&mut connection, &thread, "Refresh credentials and retry")
        .await;

    let requests = endpoint.received_requests().await.unwrap();
    let authorization = requests
        .iter()
        .map(|request| request.headers["authorization"].to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        authorization,
        ["Bearer expired", "Bearer refreshed", "Bearer refreshed"]
    );
}

#[tokio::test]
async fn product_model_context_window_triggers_compaction_before_the_next_request() {
    let product = ProductModels::new();
    product.write(
        "config.toml",
        r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "local-model"
model_context_window = 1000
model_auto_compact_token_limit = 80
"#,
    );
    let script = Arc::new(Script::default());
    let server = product.server(Some(Arc::new(Resolver(Arc::clone(&script)))));
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    let first = final_message("original response").with_usage(ra_core::usage::Usage::from_request(
        ra_core::usage::RequestUsage::new(80, 1),
    ));
    script.push(Step::Respond(first));
    server.run_turn(&mut connection, &thread, "first").await;
    assert_eq!(script.requests.lock().unwrap().len(), 1);

    script.answer(
        &json!({
            "primary_request_and_intent": "intent",
            "key_technical_concepts": "concepts",
            "files_and_code_sections": "files",
            "errors_and_fixes": "errors",
            "problem_solving": "solutions",
            "pending_tasks": "tasks",
            "current_work": "summary of earlier conversation",
            "optional_next_step": "continue"
        })
        .to_string(),
    );
    script.answer("answer after compaction");
    server.run_turn(&mut connection, &thread, "second").await;
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let second_sampling = &requests[2];
    assert!(second_sampling.input().iter().any(|item| {
        match item {
            ModelInputItem::Compaction(_) => true,
            ModelInputItem::Message(message) => message
                .text_content()
                .contains("summary of earlier conversation"),
            _ => false,
        }
    }));
}
