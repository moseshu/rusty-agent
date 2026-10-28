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
            "[skills.bundled]\nenabled = false\n[mcp_servers.configured]\ncommand = {}\n[mcp_servers.helper]\nurl = \"https://helper.example/mcp\"\nhttp_headers_helper = \"print-headers\"\n",
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
    assert_eq!(outcome.unavailable().len(), 1, "{outcome:?}");
    assert_eq!(outcome.unavailable()[0].0, "helper");
    assert!(outcome.unavailable()[0].1.contains("header helper"));
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
