//! Product MCP configuration reaching a live coding MCP runtime: real config layers, plugin
//! `.mcp.json` declarations, a real stdio server, and reconciliation after config writes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ra_coding::mcp::{CodingMcpRuntime, product_mcp_servers};
use ra_coding::plugins::{PluginConfigManager, PluginInstallRequest, PluginServices};
use ra_coding::skills::HostSkillsService;
use ra_config::{ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_mcp::tool::McpDiscoveryOptions;
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::protocol::{
    ConfigBatchWriteParams, ConfigEdit, McpServerConnectionStatus as Status, MergeStrategy,
};
use tempfile::TempDir;

const FIXTURE: &str = env!("CARGO_BIN_EXE_mcp_fixture");

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn services(home: &Path) -> Arc<PluginServices> {
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home),
        absolute(home),
        ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    PluginServices::new(
        manager,
        Arc::new(HostSkillsService::new(absolute(home), false)),
    )
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

async fn wait_status(runtime: &CodingMcpRuntime, name: &str, status: Status) {
    for _ in 0..500 {
        if runtime
            .get_mcp_status()
            .iter()
            .any(|entry| entry.name() == name && entry.runtime_status() == Some(status))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{name} never reached {status:?}: {:?}",
        runtime.get_mcp_status()
    );
}

#[tokio::test]
async fn bound_runtime_follows_config_and_plugin_changes() {
    let home = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
    write(
        &home.path().join("config.toml"),
        &format!(
            "[skills.bundled]\nenabled = false\n[mcp_servers.configured]\ncommand = {}\n",
            toml_string(FIXTURE)
        ),
    );
    write(
        &market.path().join(".agents/plugins/marketplace.json"),
        r#"{"name":"company","plugins":[{"name":"sample","source":{"source":"local","path":"./sample"}}]}"#,
    );
    write(
        &market.path().join("sample/.codex-plugin/plugin.json"),
        r#"{"name":"sample","version":"1.0.0","description":"sample"}"#,
    );
    write(
        &market.path().join("sample/.mcp.json"),
        &serde_json::json!({"mcpServers":{"plugin_docs":{"command":FIXTURE}}}).to_string(),
    );
    let services = services(home.path());
    let runtime = CodingMcpRuntime::new(Vec::new(), McpDiscoveryOptions::new()).unwrap();
    runtime.start_background();
    let binding = services.bind_mcp_runtime(&runtime, absolute(home.path()));

    let outcome = binding.wait_for(services.generation()).await.unwrap();
    assert_eq!(outcome.error(), None);
    assert!(outcome.unavailable().is_empty(), "{outcome:?}");
    wait_status(&runtime, "configured", Status::Connected).await;

    services
        .install(PluginInstallRequest {
            plugin_name: "sample".into(),
            marketplace_path: absolute(&market.path().join(".agents/plugins/marketplace.json")),
        })
        .await
        .unwrap();
    binding.wait_for(services.generation()).await;
    wait_status(&runtime, "plugin_docs", Status::Connected).await;
    assert_eq!(
        runtime
            .get_mcp_status()
            .iter()
            .map(|status| status.name().to_owned())
            .collect::<Vec<_>>(),
        ["configured", "plugin_docs"]
    );

    services
        .batch_write_config(ConfigBatchWriteParams {
            edits: vec![
                ConfigEdit {
                    key_path: "plugins.\"sample@company\".enabled".to_owned(),
                    value: serde_json::json!(false),
                    merge_strategy: MergeStrategy::Upsert,
                },
                ConfigEdit {
                    key_path: "mcp_servers.configured.enabled".to_owned(),
                    value: serde_json::json!(false),
                    merge_strategy: MergeStrategy::Replace,
                },
            ],
            file_path: None,
            expected_version: None,
            reload_user_config: false,
        })
        .await
        .unwrap();
    let outcome = binding.wait_for(services.generation()).await.unwrap();
    assert_eq!(outcome.error(), None);
    let statuses = runtime.get_mcp_status();
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    assert_eq!(statuses[0].name(), "configured");
    assert_eq!(statuses[0].runtime_status(), Some(Status::Disabled));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_identity_tracks_transport_and_referenced_environment() {
    let home = TempDir::new().unwrap();
    let identity = |config: &str| {
        let home = home.path().to_path_buf();
        let config = config.to_owned();
        async move {
            write(&home.join("config.toml"), &config);
            let services = services(&home);
            let snapshot = services.snapshot(absolute(&home)).await.unwrap();
            let servers = product_mcp_servers(
                &snapshot.mcp_catalog,
                &home,
                &snapshot.config.http_client_factory,
            );
            servers.servers[0].connection_identity().unwrap().to_owned()
        }
    };
    let base = format!("[mcp_servers.docs]\ncommand = {}\n", toml_string(FIXTURE));
    let first = identity(&base).await;
    assert_eq!(identity(&base).await, first);
    assert_eq!(
        identity(&format!(
            "{base}enabled_tools = [\"search\"]\ntool_timeout_sec = 9\n"
        ))
        .await,
        first,
        "filters and call timeouts do not need a new connection"
    );
    assert_ne!(
        identity(&format!("{base}args = [\"--delay-init\"]\n")).await,
        first
    );
    assert_ne!(
        identity(&format!(
            "{base}env_vars = [\"RA_TEST_UNSET_IDENTITY_VAR\"]\n"
        ))
        .await,
        first
    );
}

#[path = "product_runtime_binding/policy.rs"]
mod policy;

#[path = "product_runtime_binding/tool_catalog_cache.rs"]
mod tool_catalog_cache;

#[test]
fn product_registry_rejects_executor_only_transports() {
    use ra_mcp::{McpCatalogBuilder, McpServerRegistration};
    use serde_json::json;

    let home = TempDir::new().unwrap();
    let mut catalog = McpCatalogBuilder::default();
    for (name, config) in [
        ("local_stdio", json!({"command": FIXTURE})),
        ("local_http", json!({"url": "http://127.0.0.1:1/mcp"})),
        (
            "local_env_header",
            json!({
                "url": "http://127.0.0.1:1/mcp",
                "env_http_headers": {"X-MCP-Test": "RA_TEST_MCP_HEADER"},
            }),
        ),
        (
            "local_bearer",
            json!({
                "url": "http://127.0.0.1:1/mcp",
                "bearer_token_env_var": "RA_TEST_MCP_BEARER",
            }),
        ),
        (
            "local_helper",
            json!({
                "url": "http://127.0.0.1:1/mcp",
                "http_headers_helper": "exit 1",
            }),
        ),
        (
            "remote_http",
            json!({"url": "http://127.0.0.1:1/mcp", "environment_id": "executor-1"}),
        ),
    ] {
        catalog.register(McpServerRegistration::from_executor_config(
            name.into(),
            serde_json::from_value(config).unwrap(),
        ));
    }

    let plan = product_mcp_servers(
        &catalog.build(),
        home.path(),
        &HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    assert!(plan.servers.is_empty());
    assert_eq!(plan.unavailable.len(), 6);
    for (name, diagnostic) in plan.unavailable {
        let reason = if name == "remote_http" {
            "which this registry does not connect"
        } else {
            "requires a remote HTTP transport"
        };
        assert!(diagnostic.contains(reason), "{name}: {diagnostic}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn configured_http_headers_helper_runs_in_the_product_working_directory() {
    use ra_protocol::server::McpRuntimeControl;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{header, method},
    };

    let endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-helper", "product-header"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["method"] == "notifications/initialized" {
                return ResponseTemplate::new(202);
            }
            let result = match body["method"].as_str().unwrap() {
                "initialize" => json!({
                    "protocolVersion": body["params"]["protocolVersion"],
                    "serverInfo": {"name": "test", "version": "1"},
                    "capabilities": {"tools": {}}
                }),
                "tools/list" => json!({"tools": [{
                    "name": "from_helper", "inputSchema": {"type": "object"}
                }]}),
                method => panic!("unexpected MCP method {method}"),
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc":"2.0", "id":body["id"], "result":result}))
        })
        .expect(3)
        .mount(&endpoint)
        .await;
    let home = TempDir::new().unwrap();
    let helper = r#"printf x >> helper-runs; sleep 6; printf '%s' '{"X-Helper":"product-header"}'"#;
    write(
        &home.path().join("config.toml"),
        &format!(
            "[mcp_servers.docs]\nurl = {}\nhttp_headers_helper = {}\nstartup_timeout_sec = 15\n",
            toml_string(&endpoint.uri()),
            toml_string(helper),
        ),
    );
    let snapshot = services(home.path())
        .snapshot(absolute(home.path()))
        .await
        .unwrap();
    let registrations = product_mcp_servers(
        &snapshot.mcp_catalog,
        home.path(),
        &snapshot.config.http_client_factory,
    );
    assert!(registrations.unavailable.is_empty());
    assert!(!home.path().join("helper-runs").exists());

    let runtime = CodingMcpRuntime::new(registrations.servers, McpDiscoveryOptions::new()).unwrap();
    let statuses = runtime.connect_all().await.unwrap();
    assert_eq!(statuses[0].runtime_status(), Some(Status::Connected));
    for _ in 0..2 {
        let inventory = McpRuntimeControl::inventory(runtime.as_ref(), "docs", false).await;
        assert!(serde_json::to_value(inventory).unwrap()["tools"]["from_helper"].is_object());
    }
    runtime.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join("helper-runs")).unwrap(),
        "x"
    );
    endpoint.verify().await;
}

#[tokio::test]
async fn product_helper_startup_reports_exit_status_without_command_output() {
    let home = TempDir::new().unwrap();
    let helper = if cfg!(windows) {
        "echo private-helper-output & exit /b 23"
    } else {
        "printf private-helper-output; exit 23"
    };
    write(
        &home.path().join("config.toml"),
        &format!(
            "[mcp_servers.docs]\nurl = \"http://127.0.0.1:1/mcp\"\nhttp_headers_helper = {}\n",
            toml_string(helper),
        ),
    );
    let snapshot = services(home.path())
        .snapshot(absolute(home.path()))
        .await
        .unwrap();
    let registrations = product_mcp_servers(
        &snapshot.mcp_catalog,
        home.path(),
        &snapshot.config.http_client_factory,
    );
    assert!(registrations.unavailable.is_empty());

    let runtime = CodingMcpRuntime::new(registrations.servers, McpDiscoveryOptions::new()).unwrap();
    let statuses = runtime.connect_all().await.unwrap();
    assert_eq!(statuses[0].runtime_status(), Some(Status::Failed));
    let diagnostic = statuses[0].startup_error().unwrap();
    assert!(diagnostic.contains("exited with status 23"), "{diagnostic}");
    assert!(!diagnostic.contains("private-helper-output"));
    runtime.shutdown().await.unwrap();
}
