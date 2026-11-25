// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Real configuration, installation, immutable contribution snapshots and app-server requests.
use ra_coding::{
    plugins::{PluginConfigManager, PluginInstallRequest, PluginServices},
    skills::HostSkillsService,
};
use ra_config::{ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_paths::AbsolutePathBuf;
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest, RequestId},
    server::AppServer,
    transport::{
        ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::sync::mpsc;
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
    let skills = Arc::new(HostSkillsService::new(absolute(home), false));
    PluginServices::new(manager, skills)
}
fn marketplace(root: &Path, version: &str) {
    write(
        &root.join(".agents/plugins/marketplace.json"),
        r#"{"name":"company","plugins":[{"name":"sample","source":{"source":"local","path":"./sample"}}]}"#,
    );
    write(
        &root.join("sample/.codex-plugin/plugin.json"),
        &format!(
            r#"{{"name":"sample","version":"{version}","description":"sample capabilities"}}"#
        ),
    );
    write(
        &root.join("sample/skills/test/SKILL.md"),
        "---\nname: test\ndescription: test from plugin\n---\nversion one\n",
    );
    write(
        &root.join("sample/.mcp.json"),
        r#"{"mcpServers":{"docs":{"url":"https://plugin.example/mcp"}}}"#,
    );
    write(
        &root.join("sample/hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo plugin"}]}]}}"#,
    );
}
async fn call(
    events: &mpsc::Sender<TransportEvent>,
    receiver: &mut mpsc::Receiver<QueuedOutgoingMessage>,
    id: i64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    events
        .send(TransportEvent::IncomingMessage {
            connection_id: ConnectionId::new(1),
            message: JsonRpcMessage::Request(
                JsonRpcRequest::new(RequestId::Integer(id), method).with_params(params),
            ),
        })
        .await
        .unwrap();
    let mut notifications = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let value = serde_json::to_value(message.message()).unwrap();
        if value["id"] == id {
            return (value, notifications);
        }
        notifications.push(value);
    }
}
#[tokio::test]
async fn management_wire_installs_reads_lists_and_uninstalls_with_real_config() {
    let home = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
    marketplace(market.path(), "1.0.0");
    write(
        &home.path().join("config.toml"),
        "[features]\nhooks = true\n[skills.bundled]\nenabled = false\n",
    );
    let services = services(home.path());
    let server = AppServer::new(home.path())
        .unwrap()
        .with_plugin_management(services.protocol_adapter());
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let (writer, mut messages) = mpsc::channel(128);
    events
        .send(connection_opened(
            ConnectionId::new(1),
            ConnectionOrigin::WebSocket,
            writer,
            None,
        ))
        .await
        .unwrap();
    let (response, _) = call(&events, &mut messages, 1, "plugin/list", json!({})).await;
    assert_eq!(response["error"]["message"], "Not initialized");
    let (response, _) = call(
        &events,
        &mut messages,
        2,
        "initialize",
        json!({"clientInfo":{"name":"plugin-test","version":"1"}}),
    )
    .await;
    assert!(response.get("result").is_some(), "{response}");
    let (response, _) = call(
        &events,
        &mut messages,
        3,
        "marketplace/add",
        json!({"source":market.path()}),
    )
    .await;
    assert_eq!(
        response["result"]["marketplaceName"], "company",
        "{response}"
    );
    let (response, _) = call(&events, &mut messages, 4, "plugin/list", json!({})).await;
    let plugin = &response["result"]["marketplaces"][0]["plugins"][0];
    assert_eq!(plugin["id"], "sample@company");
    assert_eq!(plugin["installed"], false);
    assert_eq!(plugin["enabled"], false);
    let (response,mut mutation_notifications)=call(&events,&mut messages,5,"plugin/install",json!({"marketplacePath":market.path().join(".agents/plugins/marketplace.json"),"pluginName":"sample"})).await;
    assert_eq!(response["result"]["authPolicy"], "ON_INSTALL", "{response}");
    let (response,notifications)=call(&events,&mut messages,6,"plugin/read",json!({"marketplacePath":market.path().join(".agents/plugins/marketplace.json"),"pluginName":"sample"})).await;
    assert_eq!(
        response["result"]["plugin"]["summary"]["installed"], true,
        "{response}"
    );
    assert_eq!(
        response["result"]["plugin"]["skills"][0]["name"],
        "sample:test"
    );
    assert_eq!(
        response["result"]["plugin"].get("scheduledTasks"),
        Some(&Value::Null)
    );
    assert_eq!(
        response["result"]["plugin"]["summary"].get("installPolicySource"),
        Some(&Value::Null)
    );
    assert_eq!(
        response["result"]["plugin"]["summary"].get("mustShowInstallationInterstitial"),
        Some(&Value::Null)
    );
    assert_eq!(
        response["result"]["plugin"]["hooks"],
        json!([{
            "key":"sample@company:hooks/hooks.json:session_start:0:0",
            "eventName":"session_start"
        }])
    );
    mutation_notifications.extend(notifications);
    if !mutation_notifications
        .iter()
        .any(|notification| notification["method"] == "skills/changed")
    {
        let notification = tokio::time::timeout(Duration::from_secs(10), messages.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(notification.message()).unwrap()["method"],
            "skills/changed"
        );
    }
    let persisted = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    let persisted: toml::Value = toml::from_str(&persisted).unwrap();
    assert_eq!(
        persisted["plugins"]["sample@company"]["enabled"].as_bool(),
        Some(true)
    );
    let (response, _) = call(&events, &mut messages, 7, "hooks/list", json!({})).await;
    assert_eq!(
        response["result"]["data"][0]["hooks"][0]["trustStatus"], "untrusted",
        "{response}"
    );
    assert_eq!(
        response["result"]["data"][0]["hooks"][0]["pluginId"],
        "sample@company"
    );
    let (response, _) = call(&events, &mut messages, 8, "skills/list", json!({})).await;
    let plugin_skills = response["result"]["data"][0]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|skill| skill["pluginId"] == "sample@company")
        .collect::<Vec<_>>();
    assert_eq!(plugin_skills.len(), 1);
    assert_eq!(plugin_skills[0]["name"], "sample:test");
    let (response, _) = call(
        &events,
        &mut messages,
        9,
        "plugin/uninstall",
        json!({"pluginId":"sample@company"}),
    )
    .await;
    assert_eq!(response["result"], json!({}));
    let (response, _) = call(&events, &mut messages, 10, "plugin/installed", json!({})).await;
    assert_eq!(response["result"]["marketplaces"], json!([]));
    let (response, _) = call(
        &events,
        &mut messages,
        11,
        "marketplace/remove",
        json!({"marketplaceName":"company"}),
    )
    .await;
    assert_eq!(
        response["result"]["marketplaceName"], "company",
        "{response}"
    );
    drop(events);
    task.await.unwrap();
}
#[tokio::test]
async fn activation_refresh_keeps_captured_skills_and_uses_config_mcp_precedence() {
    let home = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
    marketplace(market.path(), "1.0.0");
    write(
        &home.path().join("config.toml"),
        "[skills.bundled]\nenabled=false\n[mcp_servers.docs]\nurl='https://configured.example/mcp'\n",
    );
    let services = services(home.path());
    services
        .install(PluginInstallRequest {
            plugin_name: "sample".into(),
            marketplace_path: absolute(&market.path().join(".agents/plugins/marketplace.json")),
        })
        .await
        .unwrap();
    let first = services.snapshot(absolute(home.path())).await.unwrap();
    let plugin_skills = first
        .skills
        .outcome()
        .skills
        .iter()
        .filter(|skill| skill.plugin_id.as_deref() == Some("sample@company"))
        .collect::<Vec<_>>();
    assert_eq!(plugin_skills.len(), 1);
    let inputs = [ra_protocol::server::protocol::UserInput::mention(
        "sample",
        "plugin://sample@company",
    )];
    let hints = first.explicit_plugin_instructions(&inputs, &["docs".into()]);
    assert_eq!(hints.len(), 1);
    assert!(hints[0].contains("`sample:`"));
    assert!(!hints[0].contains("MCP servers"));
    assert!(matches!(
        first.mcp_catalog.server("docs").unwrap().source(),
        ra_mcp::McpServerSource::Config
    ));
    let skill = plugin_skills[0].clone();
    assert!(
        first
            .skills
            .read_skill_text(&skill)
            .await
            .unwrap()
            .contains("version one")
    );
    services
        .set_enabled("sample@company".into(), false)
        .await
        .unwrap();
    let disabled = services.snapshot(absolute(home.path())).await.unwrap();
    assert!(
        disabled
            .skills
            .outcome()
            .skills
            .iter()
            .all(|skill| skill.plugin_id.as_deref() != Some("sample@company"))
    );
    assert_eq!(
        disabled.skills.outcome().skills.len() + 1,
        first.skills.outcome().skills.len()
    );
    assert!(disabled.plugins.capability_summaries().is_empty());
    assert_eq!(plugin_skills[0].name, "sample:test");
    assert!(
        first
            .skills
            .read_skill_text(&skill)
            .await
            .unwrap()
            .contains("version one")
    );
    assert!(
        disabled
            .mcp_catalog
            .server("docs")
            .unwrap()
            .config()
            .enabled
    );
    services
        .set_enabled("sample@company".into(), true)
        .await
        .unwrap();
    let enabled = services.snapshot(absolute(home.path())).await.unwrap();
    assert_eq!(
        enabled.skills.outcome().skills.len(),
        first.skills.outcome().skills.len()
    );
    assert!(
        enabled
            .skills
            .outcome()
            .skills
            .iter()
            .any(|skill| skill.name == "sample:test"
                && skill.plugin_id.as_deref() == Some("sample@company"))
    );
    let reloaded = services
        .config_manager
        .load_non_project_config()
        .await
        .unwrap();
    assert_eq!(
        reloaded
            .plugins_config_input()
            .config_layer_stack
            .get_active_user_layer()
            .unwrap()
            .config["plugins"]["sample@company"]["enabled"]
            .as_bool(),
        Some(true)
    );
}
#[tokio::test]
async fn manager_reports_local_root_errors_without_discarding_other_scopes() {
    let home = TempDir::new().unwrap();
    let broken = TempDir::new().unwrap();
    write(
        &home.path().join("config.toml"),
        &format!(
            "[skills.bundled]\nenabled=false\n[projects.{:?}]\ntrust_level=\"trusted\"\n",
            broken.path().display().to_string()
        ),
    );
    write(&broken.path().join(".git"), "gitdir: missing");
    write(&broken.path().join(".rusty-agent/config.toml"), "[invalid");
    let services = services(home.path());
    let context = services
        .config_manager
        .marketplace_context(vec![absolute(home.path()), absolute(broken.path())])
        .await
        .unwrap();
    assert_eq!(context.scopes.len(), 1);
    assert_eq!(context.load_errors.len(), 1);
    assert_eq!(context.load_errors[0].path, absolute(broken.path()));
}
#[tokio::test]
async fn config_writes_toggle_plugins_and_report_write_error_codes() {
    let home = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
    marketplace(market.path(), "1.0.0");
    write(
        &home.path().join("config.toml"),
        "[skills.bundled]\nenabled = false\n",
    );
    let services = services(home.path());
    services
        .install(PluginInstallRequest {
            plugin_name: "sample".into(),
            marketplace_path: absolute(&market.path().join(".agents/plugins/marketplace.json")),
        })
        .await
        .unwrap();
    let server = AppServer::new(home.path())
        .unwrap()
        .with_plugin_management(services.protocol_adapter());
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let (writer, mut messages) = mpsc::channel(128);
    events
        .send(connection_opened(
            ConnectionId::new(1),
            ConnectionOrigin::WebSocket,
            writer,
            None,
        ))
        .await
        .unwrap();
    let (response, _) = call(
        &events,
        &mut messages,
        1,
        "initialize",
        json!({"clientInfo":{"name":"plugin-test","version":"1"}}),
    )
    .await;
    assert!(response.get("result").is_some(), "{response}");
    let (response, _) = call(
        &events,
        &mut messages,
        2,
        "config/read",
        json!({"includeLayers":true}),
    )
    .await;
    let version = response["result"]["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layer| layer["name"]["type"] == "user")
        .unwrap()["version"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        response["result"]["origins"]["plugins.sample@company.enabled"]["name"]["type"], "user",
        "{response}"
    );
    let (response,_)=call(&events,&mut messages,3,"config/batchWrite",json!({"edits":[{"keyPath":"plugins.\"sample@company\".enabled","value":false,"mergeStrategy":"upsert"}],"expectedVersion":version})).await;
    assert_eq!(response["result"]["status"], "ok", "{response}");
    let notification = tokio::time::timeout(Duration::from_secs(10), messages.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(notification.message()).unwrap()["method"],
        "skills/changed"
    );
    let (response, _) = call(&events, &mut messages, 4, "skills/list", json!({})).await;
    assert!(
        response["result"]["data"][0]["skills"]
            .as_array()
            .unwrap()
            .iter()
            .all(|skill| skill["pluginId"] != "sample@company"),
        "{response}"
    );
    let (response, _) = call(
        &events,
        &mut messages,
        5,
        "plugin/list",
        json!({"cwds":[market.path()]}),
    )
    .await;
    assert_eq!(
        response["result"]["marketplaces"][0]["plugins"][0]["enabled"], false,
        "{response}"
    );
    let (response,_)=call(&events,&mut messages,6,"config/value/write",json!({"keyPath":"plugins.\"sample@company\".enabled","value":true,"mergeStrategy":"replace","expectedVersion":version})).await;
    assert_eq!(
        response["error"]["data"]["config_write_error_code"], "configVersionConflict",
        "{response}"
    );
    let (response,_)=call(&events,&mut messages,7,"config/value/write",json!({"keyPath":"model","value":"x","mergeStrategy":"replace","filePath":market.path().join("config.toml")})).await;
    assert_eq!(
        response["error"]["data"]["config_write_error_code"], "configLayerReadonly",
        "{response}"
    );
    let (response, _) = call(
        &events,
        &mut messages,
        8,
        "skills/config/write",
        json!({"name":"sample:test","enabled":false}),
    )
    .await;
    assert_eq!(response["result"]["effectiveEnabled"], false, "{response}");
    let persisted: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.path().join("config.toml")).unwrap()).unwrap();
    assert_eq!(
        persisted["skills"]["config"][0]["name"].as_str(),
        Some("sample:test")
    );
    let (response, _) = call(
        &events,
        &mut messages,
        9,
        "skills/config/write",
        json!({"enabled":true}),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    drop(events);
    task.await.unwrap();
}

#[tokio::test]
async fn selected_product_installs_plugin_input_contributor_with_developer_instructions() {
    use ra_core::{
        context::RunContext,
        item::{AgentId, ContentBlock, Message, MessageRole, ModelInputItem},
        state::RunId,
    };
    let home = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
    marketplace(market.path(), "1.0.0");
    write(
        &home.path().join("config.toml"),
        "[skills.bundled]\nenabled=false\n",
    );
    let services = services(home.path());
    services
        .install(PluginInstallRequest {
            plugin_name: "sample".into(),
            marketplace_path: absolute(&market.path().join(".agents/plugins/marketplace.json")),
        })
        .await
        .unwrap();
    let host = ra_coding::CodingHost::open(home.path())
        .unwrap()
        .with_plugin_skills(services);
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &ra_core::prompt::PromptRole::Main,
        &host,
        ra_coding::CodingProfile::Core,
    )
    .await
    .unwrap();
    let config = built.configure_run(host.build_run_config());
    let contributor = config
        .capabilities()
        .iter()
        .find(|c| c.kind().as_str() == "plugin_mentions")
        .unwrap();
    let run = RunContext::new(RunId::generate(), built.agent());
    let input = [ModelInputItem::Message(Message::new(
        MessageRole::User,
        vec![
            ContentBlock::text("Use [@sample](plugin://sample@company)"),
            ContentBlock::Mention {
                name: "sample".into(),
                path: "plugin://sample@company".into(),
            },
        ],
    ))];
    let messages = contributor
        .contribute_turn_input(&run, &input)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role(), MessageRole::Developer);
    assert!(messages[0].text_content().contains("`sample:`"));
    assert!(!messages[0].text_content().contains("Apps from"));
    assert!(
        contributor
            .contribute_turn_input(&run, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

/// The trusted-hash write must address one escaped leaf and retain sibling hook state.
#[tokio::test]
async fn hook_trusted_hash_write_targets_only_the_escaped_leaf() {
    let home = TempDir::new().unwrap();
    let key = r#"plugin."quoted"\path"#;
    write(
        &home.path().join("config.toml"),
        &format!(
            "[hooks.state.'{key}']\nenabled = false\n[hooks.state.other]\ntrusted_hash = 'keep'\n"
        ),
    );
    services(home.path())
        .write_config_value(
            serde_json::from_value(json!({
                "keyPath":r#"hooks.state."plugin.\"quoted\"\\path".trusted_hash"#,
                "mergeStrategy":"replace", "value":"sha256:current"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let config: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.path().join("config.toml")).unwrap()).unwrap();
    assert_eq!(
        config["hooks"]["state"][key]["trusted_hash"].as_str(),
        Some("sha256:current")
    );
    assert_eq!(
        config["hooks"]["state"][key]["enabled"].as_bool(),
        Some(false)
    );
    assert_eq!(
        config["hooks"]["state"]["other"]["trusted_hash"].as_str(),
        Some("keep")
    );
    assert_eq!(config["hooks"]["state"].as_table().unwrap().len(), 2);
}

// Codex's `configRequirements/read` cases from `app-server/tests/suite/v2/config_rpc.rs` and the
// `map_requirements_to_api` tests of `config_processor.rs`, driven through the RPC with a
// `requirements.toml` fixture.
//
// Source tests not carried over:
// - `managed_auth_settings_are_exposed_enforced_and_read_only`: the CLI credential store and the
//   ChatGPT backend URL are account settings this product does not report.

/// Answers one `configRequirements/read` against `requirements`, or none when it is `None`.
async fn read_requirements(
    requirements: Option<&str>,
) -> ra_protocol::server::protocol::ConfigRequirementsReadResponse {
    let response = read_requirements_json(requirements).await;
    serde_json::from_value(response.clone()).unwrap_or_else(|error| panic!("{error}: {response}"))
}

/// The raw result of one `configRequirements/read` against `requirements`.
async fn read_requirements_json(requirements: Option<&str>) -> Value {
    let response = read_requirements_rpc(requirements).await;
    assert!(response.get("result").is_some(), "{response}");
    response["result"].clone()
}

async fn read_requirements_rpc(requirements: Option<&str>) -> Value {
    let home = TempDir::new().unwrap();
    let requirements_path = home.path().join("requirements.toml");
    if let Some(requirements) = requirements {
        write(&requirements_path, requirements);
    }
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home.path()),
        absolute(home.path()),
        ConfigLoadOptions::from(LoaderOverrides {
            system_requirements_path: Some(requirements_path),
            ..LoaderOverrides::without_managed_config_for_tests()
        }),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let skills = Arc::new(HostSkillsService::new(absolute(home.path()), false));
    let services = PluginServices::new(manager, skills);
    let server = AppServer::new(home.path())
        .unwrap()
        .with_plugin_management(services.protocol_adapter());
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let (writer, mut messages) = mpsc::channel(128);
    events
        .send(connection_opened(
            ConnectionId::new(1),
            ConnectionOrigin::WebSocket,
            writer,
            None,
        ))
        .await
        .unwrap();
    let (response, _) = call(
        &events,
        &mut messages,
        1,
        "initialize",
        json!({"clientInfo":{"name":"requirements-test","version":"1"}}),
    )
    .await;
    assert!(response.get("result").is_some(), "{response}");
    let (response, _) = call(
        &events,
        &mut messages,
        2,
        "configRequirements/read",
        Value::Null,
    )
    .await;
    drop(events);
    task.await.unwrap();
    response
}

/// The wire names Codex's clients read, which a round trip through the same records would not
/// catch.
#[tokio::test]
async fn config_requirements_read_uses_codex_wire_names() {
    let response = read_requirements_json(Some(
        r#"allow_remote_control = false
chatgpt_base_url = "https://managed.example.com/backend-api/"

[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "echo managed"
timeout = 5
additionalContextLimit = 4096
"#,
    ))
    .await;
    assert_eq!(response["supportsIndependentSpeedModes"], true);
    let requirements = &response["requirements"];
    assert_eq!(requirements["allowRemoteControl"], false);
    assert_eq!(
        requirements["hooks"]["SessionStart"][0]["hooks"][0],
        json!({
            "type": "command",
            "command": "echo managed",
            "commandWindows": null,
            "timeoutSec": 5,
            "async": false,
            "statusMessage": null,
            "additionalContextLimit": 4096,
        })
    );
    assert!(requirements.get("allowedLoginMethods").is_none());
    assert!(requirements.get("chatgptBaseUrl").is_none());
}

#[tokio::test]
async fn config_requirements_read_is_null_without_requirements() {
    let response = read_requirements(None).await;
    assert_eq!(response.supports_independent_speed_modes, Some(true));
    assert_eq!(response.requirements, None);
}

#[tokio::test]
async fn config_requirements_read_is_null_with_only_account_requirements() {
    let response = read_requirements_json(Some(
        r#"chatgpt_base_url = "https://managed.example.com/backend-api/""#,
    ))
    .await;
    assert_eq!(
        response,
        json!({"requirements": null, "supportsIndependentSpeedModes": true})
    );
}

#[tokio::test]
async fn config_requirements_read_keeps_application_and_agent_network_policies_separate() {
    let response = read_requirements_json(Some(
        r#"
[experimental_network.domains]
"github.com" = "allow"

[application.network.domains]
"GOV.example.com." = "allow"
"github.com" = "deny"
"#,
    ))
    .await;
    let requirements = &response["requirements"];
    assert_eq!(
        requirements["application"],
        json!({"network": {"enabled": true, "domains": {"gov.example.com": "allow", "github.com": "deny"}}})
    );
    assert_eq!(
        requirements["network"]["domains"],
        json!({"github.com": "allow"})
    );
}

#[tokio::test]
async fn config_requirements_read_rejects_invalid_application_network_domains() {
    let response = read_requirements_rpc(Some(
        "[application.network.domains]\n'*.example.com' = 'allow'",
    ))
    .await;
    let error = &response["error"];
    assert_eq!(error["code"], -32603, "{response}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("application.network.domains")),
        "{response}"
    );
}

#[tokio::test]
async fn config_requirements_read_exposes_independent_speed_policy() {
    for (fast_enabled, ultrafast_enabled) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let response = read_requirements(Some(&format!(
            "[features]\nfast_mode = {fast_enabled}\nultrafast_mode = {ultrafast_enabled}\n"
        )))
        .await;
        assert_eq!(
            (
                response.supports_independent_speed_modes,
                response
                    .requirements
                    .expect("speed requirements")
                    .feature_requirements
            ),
            (
                Some(true),
                Some(std::collections::BTreeMap::from([
                    ("fast_mode".to_string(), fast_enabled),
                    ("ultrafast_mode".to_string(), ultrafast_enabled),
                ]))
            ),
        );
    }
}

#[tokio::test]
async fn config_requirements_read_includes_remote_control_and_managed_hooks() {
    use ra_protocol::server::protocol::config_requirements::ConfiguredHookHandler;
    let response = read_requirements(Some(
        r#"allow_remote_control = false

[hooks]

[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = "echo managed"
additionalContextLimit = 4096

[[hooks.SessionStart.hooks]]
type = "mcp_tool"
server = "security"
tool = "scan"
input = { path = "${tool_input.file_path}", metadata = { enabled = true, retries = 2 } }
timeout = 30
statusMessage = "Scanning file"
"#,
    ))
    .await;
    let requirements = response
        .requirements
        .expect("managed requirements should be returned");
    assert_eq!(requirements.allow_remote_control, Some(false));
    assert_eq!(
        requirements
            .hooks
            .expect("managed hooks should be returned")
            .session_start[0]
            .hooks,
        vec![
            ConfiguredHookHandler::Command {
                command: "echo managed".to_string(),
                command_windows: None,
                timeout_sec: None,
                r#async: false,
                status_message: None,
                additional_context_limit: Some(4_096),
            },
            ConfiguredHookHandler::McpTool {
                server: "security".to_string(),
                tool: "scan".to_string(),
                input: serde_json::from_value(json!({
                    "path": "${tool_input.file_path}",
                    "metadata": { "enabled": true, "retries": 2 },
                }))
                .unwrap(),
                timeout_sec: Some(30),
                status_message: Some("Scanning file".to_string()),
            },
        ]
    );
}

#[tokio::test]
async fn config_requirements_read_includes_browser_and_computer_use_schema() {
    use ra_protocol::server::protocol::config_requirements::{
        AllowDenyRequirement, BrowserUseAccessApprovalLifetime, BrowserUseExtensionRequirements,
        BrowserUseOriginPolicy, BrowserUseRequirements, ComputerUseMacosRequirements,
        ComputerUseRequirements, ComputerUseWindowsExeRequirement, ComputerUseWindowsRequirements,
        RequestHeader,
    };
    use std::collections::BTreeMap;
    let response = read_requirements(Some(
        r#"
allow_browser_and_computer_use = false

[browser_use]
allow_history_access = false
disable_auto_review = true
allow_global_persistent_approval = false

[browser_use.extension]
request_headers = [{ name = "x-browser-agent", value = "ChatGPT/{{session_id}}" }]

[browser_use.default_origin_policy]
access = "deny"
downloads = "allow"
uploads = "deny"
full_cdp_access = "allow"
auto_review = "deny"
persistent_approval = false
access_approval_lifetime = "turn"

[browser_use.origins."https://example.com"]
access = "allow"
downloads = "deny"
uploads = "allow"
full_cdp_access = "deny"
auto_review = "deny"
persistent_approval = true
access_approval_lifetime = "thread"

[computer_use]
allow_locked_computer_use = false
allow_persistent_approval = false
default_app_access = "deny"

[computer_use.macos.bundle_ids]
"com.apple.Safari" = "allow"

[computer_use.windows.aumids]
"Microsoft.Paint_8wekyb3d8bbwe!App" = "allow"

[[computer_use.windows.exes]]
publisher_name = "CN=Google LLC"
product_name = "Google Chrome"
binary_name = "chrome.exe"
access = "deny"
"#,
    ))
    .await;
    let requirements = response
        .requirements
        .expect("managed requirements should be returned");
    assert_eq!(requirements.allow_browser_and_computer_use, Some(false));
    assert_eq!(
        requirements.browser_use,
        Some(BrowserUseRequirements {
            extension: Some(BrowserUseExtensionRequirements {
                request_headers: Some(vec![RequestHeader {
                    name: "x-browser-agent".to_string(),
                    value: "ChatGPT/{{session_id}}".to_string(),
                }]),
            }),
            allow_webmcp: None,
            allow_history_access: Some(false),
            disable_auto_review: Some(true),
            allow_global_persistent_approval: Some(false),
            default_origin_policy: Some(BrowserUseOriginPolicy {
                access: Some(AllowDenyRequirement::Deny),
                downloads: Some(AllowDenyRequirement::Allow),
                uploads: Some(AllowDenyRequirement::Deny),
                full_cdp_access: Some(AllowDenyRequirement::Allow),
                auto_review: Some(AllowDenyRequirement::Deny),
                persistent_approval: Some(false),
                access_approval_lifetime: Some(BrowserUseAccessApprovalLifetime::Turn),
            }),
            origins: Some(BTreeMap::from([(
                "https://example.com".to_string(),
                BrowserUseOriginPolicy {
                    access: Some(AllowDenyRequirement::Allow),
                    downloads: Some(AllowDenyRequirement::Deny),
                    uploads: Some(AllowDenyRequirement::Allow),
                    full_cdp_access: Some(AllowDenyRequirement::Deny),
                    auto_review: Some(AllowDenyRequirement::Deny),
                    persistent_approval: Some(true),
                    access_approval_lifetime: Some(BrowserUseAccessApprovalLifetime::Thread),
                },
            )])),
        })
    );
    assert_eq!(
        requirements.computer_use,
        Some(ComputerUseRequirements {
            allow_locked_computer_use: Some(false),
            allow_persistent_approval: Some(false),
            default_app_access: Some(AllowDenyRequirement::Deny),
            macos: Some(ComputerUseMacosRequirements {
                bundle_ids: Some(BTreeMap::from([(
                    "com.apple.Safari".to_string(),
                    AllowDenyRequirement::Allow,
                )])),
            }),
            windows: Some(ComputerUseWindowsRequirements {
                aumids: Some(BTreeMap::from([(
                    "Microsoft.Paint_8wekyb3d8bbwe!App".to_string(),
                    AllowDenyRequirement::Allow,
                )])),
                exes: Some(vec![ComputerUseWindowsExeRequirement {
                    publisher_name: "CN=Google LLC".to_string(),
                    product_name: "Google Chrome".to_string(),
                    binary_name: Some("chrome.exe".to_string()),
                    access: AllowDenyRequirement::Deny,
                }]),
            }),
        })
    );
}

#[tokio::test]
async fn config_requirements_read_includes_in_app_updates_policy() {
    let response = read_requirements(Some("[features]\nin_app_updates = false\n")).await;
    assert_eq!(
        response
            .requirements
            .and_then(|requirements| requirements.feature_requirements),
        Some(std::collections::BTreeMap::from([(
            "in_app_updates".to_string(),
            false,
        )]))
    );
}

#[tokio::test]
async fn config_requirements_read_includes_managed_model_policy_and_instructions() {
    let response = read_requirements(Some(
        r#"
additional_developer_instructions = "Follow the managed policy.\nPreserve its formatting."

[auto_review]
required_on_models = ["gpt-protected", "gpt-sensitive"]
ignore_rules = ["gpt-protected"]

[models.new_thread]
model = "gpt-managed"
model_reasoning_effort = "medium"
service_tier = "fast"
"#,
    ))
    .await;
    let requirements = response.requirements.expect("managed requirements");
    assert_eq!(
        requirements.additional_developer_instructions.as_deref(),
        Some("Follow the managed policy.\nPreserve its formatting.")
    );
    let auto_review = requirements
        .auto_review
        .expect("managed automatic-review requirements");
    assert_eq!(
        auto_review.required_on_models,
        Some(vec![
            "gpt-protected".to_string(),
            "gpt-sensitive".to_string()
        ])
    );
    assert_eq!(
        auto_review.ignore_rules,
        Some(vec!["gpt-protected".to_string()])
    );
    let models = requirements.models.expect("managed model requirements");
    let defaults = models.new_thread.expect("managed new-thread defaults");
    assert_eq!(defaults.model.as_deref(), Some("gpt-managed"));
    assert_eq!(
        defaults.model_reasoning_effort,
        Some(ra_config::ReasoningEffort::Medium)
    );
    assert_eq!(defaults.service_tier.as_deref(), Some("fast"));
}

/// Codex's `requirements_api_includes_allow_managed_hooks_only`, `..._allow_appshots`,
/// `..._permission_default_and_allowlist`, `..._allowed_windows_sandbox_implementations` and
/// `..._exact_managed_values`.
#[tokio::test]
async fn config_requirements_read_maps_managed_values() {
    use ra_protocol::server::protocol::config_requirements::{
        FeedbackRequirements, WindowsSandboxImplementation,
    };
    let managed = std::env::temp_dir();
    let sqlite_home = managed.join("managed-state");
    let log_dir = managed.join("managed-logs");
    let model_catalog_json = managed.join("managed-models.json");
    let response = read_requirements(Some(&format!(
        r#"
allow_managed_hooks_only = true
allow_appshots = false
allowed_permission_profiles = {{ managed-build = false, managed-standard = true }}
default_permissions = "managed-standard"
sqlite_home = {sqlite_home:?}
log_dir = {log_dir:?}
model_catalog_json = {model_catalog_json:?}
check_for_update_on_startup = false
allow_login_shell = false
allowed_sandbox_modes = ["read-only", "external-sandbox"]
allowed_web_search_modes = ["cached"]

[windows]
allowed_sandbox_implementations = ["elevated", "unelevated"]

[feedback]
enabled = false
"#
    )))
    .await;
    let requirements = response.requirements.expect("managed requirements");
    assert_eq!(requirements.allow_managed_hooks_only, Some(true));
    assert_eq!(requirements.hooks, None);
    assert_eq!(requirements.allow_appshots, Some(false));
    assert_eq!(
        requirements.allowed_permission_profiles,
        Some(std::collections::BTreeMap::from([
            ("managed-build".to_string(), false),
            ("managed-standard".to_string(), true),
        ]))
    );
    assert_eq!(
        requirements.default_permissions.as_deref(),
        Some("managed-standard")
    );
    assert_eq!(
        requirements.allowed_windows_sandbox_implementations,
        Some(vec![
            WindowsSandboxImplementation::Elevated,
            WindowsSandboxImplementation::Unelevated,
        ])
    );
    // An external sandbox has no wire mode, and a disabled search stays selectable.
    assert_eq!(
        requirements.allowed_sandbox_modes,
        Some(vec![ra_config::SandboxMode::ReadOnly])
    );
    assert_eq!(
        requirements.allowed_web_search_modes,
        Some(vec![
            ra_config::WebSearchMode::Cached,
            ra_config::WebSearchMode::Disabled,
        ])
    );
    let uri = |path: std::path::PathBuf| Some(ra_paths::PathUri::from(absolute(&path)));
    assert_eq!(requirements.sqlite_home, uri(sqlite_home));
    assert_eq!(requirements.log_dir, uri(log_dir));
    assert_eq!(requirements.model_catalog_json, uri(model_catalog_json));
    assert_eq!(requirements.check_for_update_on_startup, Some(false));
    assert_eq!(requirements.allow_login_shell, Some(false));
    assert_eq!(
        requirements.feedback,
        Some(FeedbackRequirements {
            enabled: Some(false),
        })
    );
}
