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
