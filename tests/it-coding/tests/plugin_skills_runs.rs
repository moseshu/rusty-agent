//! Plugin services supplying each run's Host skill catalog through `CodingHost`.

use std::path::Path;
use std::sync::Arc;

use ra_coding::plugins::{PluginConfigManager, PluginInstallRequest, PluginServices};
use ra_coding::skills::HostSkillsService;
use ra_coding::{CodingHost, CodingProfile};
use ra_config::{ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_core::{
    agent::AgentId, capability::CapabilityFamily, context::RunContext, prompt::PromptRole,
    state::RunId,
};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_paths::AbsolutePathBuf;
use tempfile::TempDir;

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

async fn skills_prompt(host: &CodingHost) -> String {
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
    let run_config = built.configure_run(host.build_run_config());
    let capability = run_config
        .capabilities()
        .iter()
        .find(|capability| capability.kind() == CapabilityFamily::SKILLS)
        .unwrap();
    let run = RunContext::new(RunId::generate(), built.agent());
    capability
        .bind(&run)
        .unwrap()
        .unwrap()
        .instructions()
        .await
        .unwrap()
        .map(|instructions| instructions.content().to_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn each_run_lists_the_current_plugin_skills() {
    let home = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let market = TempDir::new().unwrap();
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
        r#"{"name":"sample","version":"1.0.0","description":"sample"}"#,
    );
    write(
        &market.path().join("sample/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review pull requests.\n---\nREVIEW BODY\n",
    );
    write(
        &workspace.path().join(".rusty-agent/skills/local/SKILL.md"),
        "---\nname: local\ndescription: Repository helper.\n---\nLOCAL BODY\n",
    );
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home.path()),
        absolute(workspace.path()),
        ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let services = PluginServices::new(
        manager,
        Arc::new(HostSkillsService::new(absolute(home.path()), false)),
    );
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_plugin_skills(Arc::clone(&services));

    let before = skills_prompt(&host).await;
    assert!(!before.contains("sample:review"), "{before}");

    services
        .install(PluginInstallRequest {
            plugin_name: "sample".into(),
            marketplace_path: absolute(&market.path().join(".agents/plugins/marketplace.json")),
        })
        .await
        .unwrap();
    let installed = skills_prompt(&host).await;
    assert!(
        installed.contains("- sample:review: Review pull requests."),
        "{installed}"
    );
    assert!(!installed.contains("REVIEW BODY"));

    services
        .set_enabled("sample@company".into(), false)
        .await
        .unwrap();
    let disabled = skills_prompt(&host).await;
    assert!(!disabled.contains("sample:review"), "{disabled}");
}

#[tokio::test]
async fn plugin_skills_and_host_discovery_are_exclusive() {
    let home = TempDir::new().unwrap();
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home.path()),
        absolute(home.path()),
        ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let services = PluginServices::new(
        manager,
        Arc::new(HostSkillsService::new(absolute(home.path()), false)),
    );
    let host = CodingHost::open(home.path())
        .unwrap()
        .with_plugin_skills(services)
        .with_skill_discovery(ra_coding::skills::SkillDiscoveryConfig::new());
    let error = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .err()
    .expect("both Host skill sources must be rejected");
    assert!(error.to_string().contains("not both"), "{error}");
}
