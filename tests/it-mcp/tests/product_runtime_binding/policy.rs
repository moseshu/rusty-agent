// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Managed plugin requirements and step-owned MCP policy through real configuration and stdio.

use super::*;
use ra_coding::plugins::PluginSnapshot;
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    item::CallId,
    state::RunId,
    tool::{Tool, ToolContext, ToolResolver},
};

async fn plugin_snapshot(requirements: &str) -> PluginSnapshot {
    let home = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
    let managed = TempDir::new().unwrap();
    write(&managed.path().join("requirements.toml"), requirements);
    write(
        &home.path().join("config.toml"),
        "[skills.bundled]\nenabled = false\n",
    );
    write(
        &market.path().join(".agents/plugins/marketplace.json"),
        r#"{"name":"company","plugins":[{"name":"sample","source":{"source":"local","path":"./sample"}}]}"#,
    );
    write(
        &market.path().join("sample/.codex-plugin/plugin.json"),
        r#"{"name":"sample","version":"1.0.0"}"#,
    );
    write(
        &market.path().join("sample/.mcp.json"),
        &serde_json::json!({"mcpServers": {
            "matched": {"command":"good-command"},
            "mismatched": {"command":"bad-command"},
            "unlisted": {"url":"https://example.com/mcp"}
        }})
        .to_string(),
    );
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home.path()),
        absolute(home.path()),
        ConfigLoadOptions::from(LoaderOverrides::with_managed_config_path_for_tests(
            managed.path().join("managed_config.toml"),
        )),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let services = PluginServices::new(
        manager,
        Arc::new(HostSkillsService::new(absolute(home.path()), false)),
    );
    services
        .install(PluginInstallRequest {
            plugin_name: "sample".into(),
            marketplace_path: absolute(&market.path().join(".agents/plugins/marketplace.json")),
        })
        .await
        .unwrap();
    services.snapshot(absolute(home.path())).await.unwrap()
}

#[tokio::test]
async fn managed_empty_allowlists_disable_plugin_servers_and_retain_source() {
    for requirements in [
        "[mcp_servers]\n",
        "allowed_approval_policies = [\"on-request\"]\n[plugins.\"sample@company\".mcp_servers]\n",
        "allowed_approval_policies = [\"on-request\"]\n[plugins.\"other@company\".mcp_servers]\n",
    ] {
        let snapshot = plugin_snapshot(requirements).await;
        assert_eq!(snapshot.mcp_catalog.servers().count(), 3);
        for (_, server) in snapshot.mcp_catalog.servers() {
            assert!(!server.config().enabled, "{requirements}");
            assert!(matches!(
                server.config().disabled_reason,
                Some(ra_config::McpServerDisabledReason::Requirements {
                    source: ra_config::RequirementSource::SystemRequirementsToml { .. }
                })
            ));
        }
    }
}

#[tokio::test]
async fn plugin_requirements_match_each_identity_and_do_not_apply_nonempty_global_allowlists() {
    let requirements = r#"
[plugins."sample@company".mcp_servers.matched.identity]
command = "good-command"
[plugins."sample@company".mcp_servers.mismatched.identity]
command = "good-command"
[plugins."sample@company".mcp_servers.unlisted.identity]
url = "https://example.com/mcp"
[mcp_servers.unrelated.identity]
command = "other-command"
"#;
    let snapshot = plugin_snapshot(requirements).await;
    for (name, expected) in [("matched", true), ("mismatched", false), ("unlisted", true)] {
        assert_eq!(
            snapshot.mcp_catalog.server(name).unwrap().config().enabled,
            expected,
            "{name}"
        );
    }
    let snapshot = plugin_snapshot("[plugins.\"sample@company\"]\n").await;
    assert!(
        snapshot
            .mcp_catalog
            .servers()
            .all(|(_, server)| server.config().enabled)
    );
    let snapshot = plugin_snapshot(
        "[plugins.\"sample@company\".mcp_servers.matched.identity]\ncommand = \"good-command\"\n",
    )
    .await;
    assert!(
        !snapshot
            .mcp_catalog
            .server("unlisted")
            .unwrap()
            .config()
            .enabled
    );
}

#[tokio::test]
async fn empty_plugin_requirements_alone_preserve_source_layer_elision() {
    // The source considers a document containing only empty plugin maps empty and elides it.
    // A retained requirements document still enforces the empty map, as tested above.
    let snapshot = plugin_snapshot("[plugins.\"sample@company\".mcp_servers]\n").await;
    assert!(
        snapshot
            .mcp_catalog
            .servers()
            .all(|(_, server)| server.config().enabled)
    );
}

fn context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("policy"))
        .name("Policy")
        .build()
        .unwrap();
    RunContext::new(RunId::new("policy"), &agent)
}

fn config(timeout: f64, policy: &str) -> String {
    format!(
        "[skills.bundled]\nenabled = false\n[mcp_servers.docs]\ncommand = {}\nargs = [\"--policy-fixture\"]\ntool_timeout_sec = {timeout}\n{policy}\n",
        toml_string(FIXTURE)
    )
}

async fn tools(runtime: &CodingMcpRuntime) -> Vec<Arc<dyn Tool>> {
    runtime
        .get_tools(&context(), &Default::default())
        .await
        .unwrap()
}

async fn requires_approval(tool: &dyn Tool) -> bool {
    let context = context();
    let id = CallId::new("approval");
    let args = serde_json::json!({});
    tool.needs_approval(&ToolContext::new(&context, tool, &id, &args))
        .await
        .unwrap()
}

async fn invoke(tool: &dyn Tool) -> ra_core::error::Result<ra_core::tool::ToolOutput> {
    let context = context();
    let id = CallId::new("invoke");
    let args = serde_json::json!({});
    let result = tool
        .call(ToolContext::new(&context, tool, &id, &args))
        .await;
    if let Err(error) = &result
        && let Some(approval) = ra_core::tool::ToolApprovalRequest::of(error)
    {
        return tool
            .call(ToolContext::new(&context, tool, &id, &args).with_resume_state(approval.state()))
            .await;
    }
    result
}

fn find<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a dyn Tool {
    tools
        .iter()
        .find(|tool| tool.schema().name() == name)
        .unwrap()
        .as_ref()
}

#[tokio::test]
async fn product_approval_modes_and_per_tool_overrides_follow_annotations() {
    for (policy, expected) in [
        ("", [false, true, false, true]),
        (
            "default_tools_approval_mode = \"auto\"",
            [false, true, false, true],
        ),
        ("default_tools_approval_mode = \"prompt\"", [true; 4]),
        ("default_tools_approval_mode = \"approve\"", [false; 4]),
        (
            "default_tools_approval_mode = \"writes\"",
            [false, false, true, true],
        ),
        (
            "default_tools_approval_mode = \"prompt\"\n[mcp_servers.docs.tools.read_only]\napproval_mode = \"approve\"\n[mcp_servers.docs.tools.safe_write]\napproval_mode = \"approve\"",
            [false, true, false, true],
        ),
    ] {
        let home = TempDir::new().unwrap();
        write(&home.path().join("config.toml"), &config(5.0, policy));
        let services = services(home.path());
        let snapshot = services.snapshot(absolute(home.path())).await.unwrap();
        let plan = product_mcp_servers(
            &snapshot.mcp_catalog,
            home.path(),
            &snapshot.config.http_client_factory,
        );
        let runtime = CodingMcpRuntime::new(plan.servers, McpDiscoveryOptions::new()).unwrap();
        runtime.connect_all().await.unwrap();
        let tools = tools(&runtime).await;
        for (name, expected) in ["read_only", "destructive", "safe_write", "unknown"]
            .into_iter()
            .zip(expected)
        {
            assert_eq!(
                requires_approval(find(&tools, name)).await,
                expected,
                "{policy}: {name}"
            );
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn refresh_reuses_transport_and_captures_new_timeout_and_approval_for_new_bindings() {
    let home = TempDir::new().unwrap();
    write(
        &home.path().join("config.toml"),
        &config(0.6, "default_tools_approval_mode = \"prompt\""),
    );
    let services = services(home.path());
    let runtime = CodingMcpRuntime::new(Vec::new(), McpDiscoveryOptions::new()).unwrap();
    runtime.start_background();
    let binding = services.bind_mcp_runtime(&runtime, absolute(home.path()));
    assert_eq!(
        binding
            .wait_for(services.generation())
            .await
            .unwrap()
            .error(),
        None
    );
    wait_status(&runtime, "docs", Status::Connected).await;
    let old = tools(&runtime).await;
    let old_process = invoke(find(&old, "env")).await.unwrap();
    write(
        &home.path().join("config.toml"),
        &config(0.1, "default_tools_approval_mode = \"approve\""),
    );
    services.invalidate();
    assert_eq!(
        binding
            .wait_for(services.generation())
            .await
            .unwrap()
            .error(),
        None
    );
    let new = tools(&runtime).await;
    assert_eq!(
        invoke(find(&new, "env")).await.unwrap().as_text(),
        old_process.as_text()
    );
    assert!(requires_approval(find(&old, "wait")).await);
    assert!(!requires_approval(find(&new, "wait")).await);
    let new_call =
        tokio::time::timeout(Duration::from_millis(450), invoke(find(&new, "wait"))).await;
    assert!(
        new_call.unwrap().is_err(),
        "new binding must use the new tool deadline"
    );
    let old_call =
        tokio::time::timeout(Duration::from_millis(250), invoke(find(&old, "wait"))).await;
    assert!(
        old_call.is_err(),
        "old binding must retain its original deadline"
    );
    runtime.shutdown().await.unwrap();
}
