// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::plugins::installed_marketplaces::marketplace_install_root;
use ra_coding::plugins::loader::test_api::*;
use ra_coding::plugins::loader::{
    configured_curated_plugin_ids_from_codex_home, refresh_curated_plugin_cache,
};
use ra_coding::plugins::manifest::{PluginManifestFormat, PluginManifestInterface};
use ra_coding::plugins::marketplace::*;
use ra_coding::plugins::startup_sync::curated_plugins_repo_path;
use ra_coding::plugins::store::PluginStore;
use ra_coding::plugins::*;
use ra_coding::skills::{HostSkillsService, SkillRootLoader};
use ra_config::*;
use ra_core::hook::HookEventName;
use ra_paths::{AbsolutePathBuf, PathUri};
use ra_protocol::server::protocol::Product;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempDir;
use toml::Value;
type PluginLoadOutcome = ra_coding::plugins::PluginLoadOutcome<McpServerConfig>;
type LoadedPlugin = ra_coding::plugins::LoadedPlugin<McpServerConfig>;
#[path = "support/source_paths.rs"]
#[allow(
    dead_code,
    reason = "Each test binary uses a subset of the shared fixtures."
)]
mod source_paths;
use source_paths::PathBufExt;
const MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN: usize = 1024;
const TEST_CURATED_PLUGIN_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const TEST_CURATED_PLUGIN_CACHE_VERSION: &str = "01234567";
fn test_http_client_factory() -> ra_http::HttpClientFactory {
    ra_http::HttpClientFactory::new(ra_http::OutboundProxyPolicy::ReqwestDefault)
}
fn test_skill_root_loader(home: &Path) -> Arc<dyn SkillRootLoader<PluginSkillRoot>> {
    Arc::new(HostSkillsService::new_with_restriction_product(
        home.to_path_buf().abs(),
        false,
        None,
    ))
}
fn test_plugins_manager(home: PathBuf) -> PluginsManager {
    PluginsManager::new(home.clone(), test_skill_root_loader(&home))
}
fn test_plugins_manager_with_options(home: PathBuf, product: Option<Product>) -> PluginsManager {
    PluginsManager::new_with_options(home.clone(), product, test_skill_root_loader(&home))
}
async fn load_plugins_config_input(home: &Path, cwd: &Path) -> PluginsConfigInput {
    let stack = ra_config::loader::load_config_layers_state(
        ra_executor::LOCAL_FS.as_ref(),
        home,
        Some(cwd.to_path_buf().abs()),
        &[],
        LoaderOverrides::without_managed_config_for_tests(),
        &NoopThreadConfigLoader,
    )
    .await
    .unwrap();
    PluginRuntimeConfig::from_layer_stack(
        stack,
        home.to_path_buf().abs(),
        cwd.to_path_buf().abs(),
        test_http_client_factory(),
    )
    .unwrap()
    .plugins_config_input()
}
pub(crate) fn write_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("file should have a parent")).unwrap();
    fs::write(path, contents).unwrap();
}

pub(crate) fn write_curated_plugin(root: &Path, plugin_name: &str) {
    let plugin_root = root.join("plugins").join(plugin_name);
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        &format!(
            r#"{{
  "name": "{plugin_name}",
  "description": "Plugin that includes skills, MCP servers, and app connectors"
}}"#
        ),
    );
    write_file(
        &plugin_root.join("skills/SKILL.md"),
        "---\nname: sample\ndescription: sample\n---\n",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample-docs": {
      "type": "http",
      "url": "https://sample.example/mcp"
    }
  }
}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{
  "apps": {
    "calendar": {
      "id": "connector_calendar"
    }
  }
}"#,
    );
}

pub(crate) fn write_openai_curated_marketplace(root: &Path, plugin_names: &[&str]) {
    write_curated_marketplace(
        root,
        "marketplace.json",
        OPENAI_CURATED_MARKETPLACE_NAME,
        /*display_name*/ None,
        plugin_names,
    );
}

pub(crate) fn write_openai_api_curated_marketplace(root: &Path, plugin_names: &[&str]) {
    write_curated_marketplace(
        root,
        "api_marketplace.json",
        OPENAI_API_CURATED_MARKETPLACE_NAME,
        Some("OpenAI Curated"),
        plugin_names,
    );
}

fn write_curated_marketplace(
    root: &Path,
    manifest_name: &str,
    marketplace_name: &str,
    display_name: Option<&str>,
    plugin_names: &[&str],
) {
    let plugins = plugin_names
        .iter()
        .map(|plugin_name| {
            format!(
                r#"{{
      "name": "{plugin_name}",
      "source": {{
        "source": "local",
        "path": "./plugins/{plugin_name}"
      }}
    }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let interface = display_name
        .map(|display_name| {
            format!(
                r#"
  "interface": {{
    "displayName": "{display_name}"
  }},"#
            )
        })
        .unwrap_or_default();
    write_file(
        &root.join(".agents/plugins").join(manifest_name),
        &format!(
            r#"{{
  "name": "{marketplace_name}",{interface}
  "plugins": [
{plugins}
  ]
}}"#
        ),
    );
    for plugin_name in plugin_names {
        write_curated_plugin(root, plugin_name);
    }
}

pub(crate) fn write_curated_plugin_sha_with(codex_home: &Path, sha: &str) {
    write_file(&codex_home.join(".tmp/plugins.sha"), &format!("{sha}\n"));
}

fn unrestricted_config_layer_stack() -> ConfigLayerStack {
    ConfigLayerStack::default()
}

fn config_layer_stack_with_requirements(
    codex_home: &Path,
    user_config: &str,
    requirements: &str,
) -> ConfigLayerStack {
    let with_sources = compose_requirements([RequirementsLayerEntry::from_toml(
        RequirementSource::Unknown,
        requirements,
    )])
    .expect("compose requirements")
    .expect("requirements should be present");
    let requirements_toml = with_sources.clone().into_toml();
    let requirements = ConfigRequirements::try_from(with_sources).expect("normalize requirements");
    let config_file =
        AbsolutePathBuf::try_from(codex_home.join(CONFIG_TOML_FILE)).expect("absolute config path");
    ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: config_file,
                profile: None,
            },
            toml::from_str(user_config).expect("parse user config"),
        )],
        requirements,
        requirements_toml,
    )
    .expect("build config layer stack")
}

fn plugins_config_input_with_requirements(
    codex_home: &Path,
    user_config: &str,
    requirements: &str,
) -> PluginsConfigInput {
    PluginsConfigInput::new(
        config_layer_stack_with_requirements(codex_home, user_config, requirements),
        toml::from_str::<ra_config::config_toml::ConfigToml>(user_config)
            .unwrap()
            .plugins,
        String::new(),
        /*plugins_enabled*/ true,
        test_http_client_factory(),
    )
}

fn app_declaration(name: &str, connector_id: &str) -> AppDeclaration {
    AppDeclaration {
        name: name.to_string(),
        connector_id: AppConnectorId(connector_id.to_string()),
        category: None,
    }
}

fn write_plugin_with_version(
    root: &Path,
    dir_name: &str,
    manifest_name: &str,
    manifest_version: Option<&str>,
) {
    let plugin_root = root.join(dir_name);
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::create_dir_all(plugin_root.join("skills")).unwrap();
    let version = manifest_version
        .map(|manifest_version| format!(r#","version":"{manifest_version}""#))
        .unwrap_or_default();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{manifest_name}"{version}}}"#),
    )
    .unwrap();
    fs::write(
        plugin_root.join("skills/SKILL.md"),
        format!("---\nname: {manifest_name}-skill\ndescription: test skill\n---\n\n# Test skill\n"),
    )
    .unwrap();
    fs::write(plugin_root.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
}

fn write_plugin(root: &Path, dir_name: &str, manifest_name: &str) {
    write_plugin_with_version(
        root,
        dir_name,
        manifest_name,
        /*manifest_version*/ None,
    );
}

fn init_git_repo(repo: &Path) {
    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.email", "codex-test@example.com"]);
    run_git(repo, &["config", "user.name", "Codex Test"]);
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-m", "initial"]);
}

fn run_git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("git should run: {err}"));
    assert!(
        output.status.success(),
        "git -C {} {} failed\nstdout:\n{}\nstderr:\n{}",
        repo.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn plugin_config_toml(enabled: bool, plugins_feature_enabled: bool) -> String {
    let mut root = toml::map::Map::new();

    let mut features = toml::map::Map::new();
    features.insert(
        "plugins".to_string(),
        Value::Boolean(plugins_feature_enabled),
    );
    root.insert("features".to_string(), Value::Table(features));

    let mut plugin = toml::map::Map::new();
    plugin.insert("enabled".to_string(), Value::Boolean(enabled));

    let mut plugins = toml::map::Map::new();
    plugins.insert("sample@test".to_string(), Value::Table(plugin));
    root.insert("plugins".to_string(), Value::Table(plugins));

    toml::to_string(&Value::Table(root)).expect("plugin test config should serialize")
}

async fn load_plugins_from_config(config_toml: &str, codex_home: &Path) -> PluginLoadOutcome {
    write_file(&codex_home.join(CONFIG_TOML_FILE), config_toml);
    let config = load_config(codex_home, codex_home).await;
    test_plugins_manager_with_options(codex_home.to_path_buf(), Some(Product::Codex))
        .plugins_for_config(&config)
        .await
}

async fn load_config(codex_home: &Path, cwd: &Path) -> PluginsConfigInput {
    load_plugins_config_input(codex_home, cwd).await
}

fn write_cached_plugin(codex_home: &Path, marketplace_name: &str, plugin_name: &str) {
    write_plugin_with_version(
        &codex_home
            .join("plugins/cache")
            .join(marketplace_name)
            .join(plugin_name),
        "local",
        plugin_name,
        /*manifest_version*/ Some("local"),
    );
}

#[tokio::test]
async fn load_plugins_loads_default_skills_and_mcp_servers() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample",
  "description": "Plugin that includes the sample MCP server and Skills"
}"#,
    );
    write_file(
        &plugin_root.join("skills/sample-search/SKILL.md"),
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "type": "http",
      "url": "https://sample.example/mcp",
      "oauth": {
        "clientId": "client-id",
        "callbackPort": 3118
      }
    }
  }
}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{
  "apps": {
    "example": {
      "id": "connector_example"
    }
  }
}"#,
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
        codex_home.path(),
    )
    .await;

    assert_eq!(
        outcome.plugins(),
        vec![LoadedPlugin {
            config_name: "sample@test".to_string(),
            manifest_name: Some("sample".to_string()),
            plugin_namespace: Some("sample".to_string()),
            manifest_description: Some(
                "Plugin that includes the sample MCP server and Skills".to_string(),
            ),
            root: AbsolutePathBuf::try_from(plugin_root.clone()).unwrap(),
            enabled: true,
            skill_roots: vec![plugin_root.join("skills").abs()],
            skill_discovery_mode: SkillDiscoveryMode::Recursive,
            disabled_skill_paths: HashSet::new(),
            has_enabled_skills: true,
            mcp_servers: HashMap::from([(
                "sample".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::StreamableHttp {
                        url: "https://sample.example/mcp".to_string(),
                        bearer_token_env_var: None,
                        http_headers: None,
                        env_http_headers: None,
                        http_headers_helper: None,
                    },
                    environment_id: "local".to_string(),
                    enabled: true,
                    required: false,
                    startup_readiness: Default::default(),
                    supports_parallel_tool_calls: false,
                    tool_input_schema_max_bytes: None,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: None,
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    scopes: None,
                    oauth: Some(McpServerOAuthConfig {
                        client_id: Some("client-id".to_string()),
                        callback_url: None,
                        callback_port: Some(3118),
                        ..Default::default()
                    }),
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            )]),
            apps: vec![app_declaration("example", "connector_example")],
            hook_sources: Vec::new(),
            hook_load_warnings: Vec::new(),
            error: None,
        }]
    );
    assert_eq!(
        outcome.capability_summaries(),
        &[PluginCapabilitySummary {
            config_name: "sample@test".to_string(),
            display_name: "sample".to_string(),
            plugin_namespace: Some("sample".to_string()),
            description: Some("Plugin that includes the sample MCP server and Skills".to_string(),),
            has_skills: true,
            mcp_server_names: vec!["sample".to_string()],
            app_connector_ids: vec![AppConnectorId("connector_example".to_string())],
        }]
    );
    assert_eq!(
        outcome
            .effective_plugin_skill_roots()
            .into_iter()
            .map(|root| root.path)
            .collect::<Vec<_>>(),
        vec![plugin_root.join("skills").abs()]
    );
    assert_eq!(outcome.effective_mcp_servers().len(), 1);
    assert_eq!(
        outcome.effective_apps(),
        vec![AppConnectorId("connector_example".to_string())]
    );
}

#[tokio::test]
async fn load_plugins_loads_manifest_mcp_server_objects() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/counter-sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "counter-sample",
  "version": "1.1.1",
  "description": "Plugin that declares MCP servers in the manifest",
  "mcpServers": {
    "counter": {
      "type": "http",
      "url": "https://sample.example/counter/mcp"
    }
  }
}"#,
    );

    let config_toml = r#"
[features]
plugins = true

[plugins."counter-sample@test"]
enabled = true
"#;
    let outcome = load_plugins_from_config(config_toml, codex_home.path()).await;

    assert_eq!(outcome.plugins()[0].error, None);
    assert_eq!(
        outcome.plugins()[0].mcp_servers,
        HashMap::from([(
            "counter".to_string(),
            McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::StreamableHttp {
                    url: "https://sample.example/counter/mcp".to_string(),
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                    http_headers_helper: None,
                },
                environment_id: "local".to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        )])
    );
}

#[tokio::test]
async fn load_plugins_applies_plugin_mcp_server_policy() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample"
}"#,
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "type": "http",
      "url": "https://sample.example/mcp",
      "default_tools_approval_mode": "prompt",
      "enabled_tools": ["read", "search"],
      "tools": {
        "search": { "approval_mode": "prompt", "output_token_limit": 8000 }
      }
    }
  }
}"#,
    );
    let config_toml = r#"
[features]
plugins = true

[plugins."sample@test"]
enabled = true

[plugins."sample@test".mcp_servers.sample]
enabled = false
default_tools_approval_mode = "approve"
enabled_tools = ["search"]
disabled_tools = ["delete"]

[plugins."sample@test".mcp_servers.sample.tools.search]
approval_mode = "approve"
output_token_limit = 12000
"#;

    let outcome = load_plugins_from_config(config_toml, codex_home.path()).await;
    let server = outcome.plugins()[0]
        .mcp_servers
        .get("sample")
        .expect("sample server");

    assert!(!server.enabled);
    assert_eq!(
        server.default_tools_approval_mode,
        Some(AppToolApproval::Approve)
    );
    assert_eq!(server.enabled_tools, Some(vec!["search".to_string()]));
    assert_eq!(server.disabled_tools, Some(vec!["delete".to_string()]));
    assert_eq!(
        server.tools.get("search"),
        Some(&McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            output_token_limit: std::num::NonZeroUsize::new(8_000),
        })
    );
}

#[tokio::test]
async fn load_plugins_resolves_disabled_skill_names_against_loaded_plugin_skills() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");
    let skill_path = plugin_root.join("skills/sample-search/SKILL.md");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );
    write_file(
        &skill_path,
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );

    let config_toml = r#"[features]
plugins = true

[[skills.config]]
name = "sample:sample-search"
enabled = false

[plugins."sample@test"]
enabled = true
"#;
    let outcome = load_plugins_from_config(config_toml, codex_home.path()).await;
    let skill_path = std::fs::canonicalize(skill_path)
        .expect("skill path should canonicalize")
        .abs();

    assert_eq!(
        outcome.plugins()[0].disabled_skill_paths,
        HashSet::from([PathUri::from_abs_path(&skill_path)])
    );
    assert!(!outcome.plugins()[0].has_enabled_skills);
    assert!(outcome.capability_summaries().is_empty());
}

#[tokio::test]
async fn load_plugins_ignores_unknown_disabled_skill_names() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );
    write_file(
        &plugin_root.join("skills/sample-search/SKILL.md"),
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );

    let config_toml = r#"[features]
plugins = true

[[skills.config]]
name = "sample:missing-skill"
enabled = false

[plugins."sample@test"]
enabled = true
"#;
    let outcome = load_plugins_from_config(config_toml, codex_home.path()).await;

    assert!(outcome.plugins()[0].disabled_skill_paths.is_empty());
    assert!(outcome.plugins()[0].has_enabled_skills);
    assert_eq!(
        outcome.capability_summaries(),
        &[PluginCapabilitySummary {
            config_name: "sample@test".to_string(),
            display_name: "sample".to_string(),
            plugin_namespace: Some("sample".to_string()),
            description: None,
            has_skills: true,
            mcp_server_names: Vec::new(),
            app_connector_ids: Vec::new(),
        }]
    );
}

#[tokio::test]
async fn capability_summary_sanitizes_plugin_descriptions_to_one_line() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample",
  "description": "Plugin that\n includes   the sample\tserver"
}"#,
    );
    write_file(
        &plugin_root.join("skills/sample-search/SKILL.md"),
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
        codex_home.path(),
    )
    .await;

    assert_eq!(
        outcome.plugins()[0].manifest_description.as_deref(),
        Some("Plugin that\n includes   the sample\tserver")
    );
    assert_eq!(
        outcome.capability_summaries()[0].description.as_deref(),
        Some("Plugin that includes the sample server")
    );
}

#[tokio::test]
async fn capability_summary_truncates_overlong_plugin_descriptions() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");
    let too_long = "x".repeat(MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN + 1);

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        &format!(
            r#"{{
  "name": "sample",
  "description": "{too_long}"
}}"#
        ),
    );
    write_file(
        &plugin_root.join("skills/sample-search/SKILL.md"),
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
        codex_home.path(),
    )
    .await;

    assert_eq!(
        outcome.plugins()[0].manifest_description.as_deref(),
        Some(too_long.as_str())
    );
    assert_eq!(
        outcome.capability_summaries()[0].description,
        Some("x".repeat(MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN))
    );
}

#[tokio::test]
async fn load_plugins_uses_manifest_configured_component_paths() {
    for (skills_json, expected_skill_dirs) in [
        (r#""./custom-skills/""#, &["custom-skills"][..]),
        (
            r#"["./custom-skills/", "./extra-skills/"]"#,
            &["custom-skills", "extra-skills"][..],
        ),
        (
            r#"["./custom-skills/", "./custom-skills/"]"#,
            &["custom-skills"][..],
        ),
        (r#""./skills/""#, &["skills"][..]),
        (
            r#"["./skills/abc/", "./skills/edk/"]"#,
            &["skills/abc", "skills/edk"][..],
        ),
    ] {
        let codex_home = TempDir::new().unwrap();
        let plugin_root = codex_home
            .path()
            .join("plugins/cache")
            .join("test/sample/local");

        write_file(
            &plugin_root.join(".codex-plugin/plugin.json"),
            &format!(
                r#"{{
  "name": "sample",
  "skills": {skills_json},
  "mcpServers": "./config/custom.mcp.json",
  "apps": "./config/custom.app.json"
}}"#
            ),
        );
        write_file(
            &plugin_root.join("skills/default-skill/SKILL.md"),
            "---\nname: default-skill\ndescription: default skill\n---\n",
        );
        write_file(
            &plugin_root.join("skills/abc/SKILL.md"),
            "---\nname: abc\ndescription: abc skill\n---\n",
        );
        write_file(
            &plugin_root.join("skills/edk/SKILL.md"),
            "---\nname: edk\ndescription: edk skill\n---\n",
        );
        write_file(
            &plugin_root.join("custom-skills/custom-skill/SKILL.md"),
            "---\nname: custom-skill\ndescription: custom skill\n---\n",
        );
        write_file(
            &plugin_root.join("extra-skills/extra-skill/SKILL.md"),
            "---\nname: extra-skill\ndescription: extra skill\n---\n",
        );
        write_file(
            &plugin_root.join(".mcp.json"),
            r#"{
  "mcpServers": {
    "default": {
      "type": "http",
      "url": "https://default.example/mcp"
    }
  }
}"#,
        );
        write_file(
            &plugin_root.join("config/custom.mcp.json"),
            r#"{
  "mcpServers": {
    "custom": {
      "type": "http",
      "url": "https://custom.example/mcp"
    }
  }
}"#,
        );
        write_file(
            &plugin_root.join(".app.json"),
            r#"{
  "apps": {
    "default-app": {
      "id": "connector_default"
    }
  }
}"#,
        );
        write_file(
            &plugin_root.join("config/custom.app.json"),
            r#"{
  "apps": {
    "custom-app": {
      "id": "connector_custom"
    }
  }
}"#,
        );
        let outcome = load_plugins_from_config(
            &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
            codex_home.path(),
        )
        .await;
        let mut expected_skill_roots = expected_skill_dirs
            .iter()
            .map(|dir| plugin_root.join(dir).abs())
            .collect::<Vec<_>>();
        expected_skill_roots.sort_unstable();
        expected_skill_roots.dedup();

        assert_eq!(outcome.plugins()[0].skill_roots, expected_skill_roots);
        assert_eq!(
            outcome.plugins()[0].mcp_servers,
            HashMap::from([(
                "custom".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::StreamableHttp {
                        url: "https://custom.example/mcp".to_string(),
                        bearer_token_env_var: None,
                        http_headers: None,
                        env_http_headers: None,
                        http_headers_helper: None,
                    },
                    environment_id: "local".to_string(),
                    enabled: true,
                    required: false,
                    startup_readiness: Default::default(),
                    supports_parallel_tool_calls: false,
                    tool_input_schema_max_bytes: None,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: None,
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            )])
        );
        assert_eq!(
            outcome.plugins()[0].apps,
            vec![app_declaration("custom-app", "connector_custom")]
        );
    }
}

#[tokio::test]
async fn install_plugin_materializes_default_command_skills() {
    let codex_home = TempDir::new().unwrap();
    let source_root = codex_home.path().join("source/sample");

    write_file(
        &source_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample",
  "skills": "./custom-skills/"
}"#,
    );
    fs::create_dir_all(source_root.join("custom-skills")).unwrap();
    write_file(
        &source_root.join("custom-skills/source-command-pr-review/SKILL.md"),
        "---\nname: source-command-pr-review\ndescription: Native review skill\n---\n",
    );
    write_file(
        &source_root.join("commands/pr/review.md"),
        "---\ndescription: Review a pull request\n---\nInspect the proposed changes.\n",
    );
    write_file(
        &source_root.join("commands/summarize.md"),
        "---\ndescription: Summarize a change\n---\nSummarize the proposed changes.\n",
    );
    write_file(
        &source_root.join("commands/oversized.md"),
        &format!("---\ndescription: Oversized\n---\n{}", "x".repeat(4_000)),
    );
    write_file(
        &source_root.join(".codex-plugin/migrated-command-skills/undeclared-command/SKILL.md"),
        "---\nname: undeclared-command\ndescription: undeclared command\n---\n",
    );
    let result = PluginStore::new(codex_home.path().to_path_buf())
        .install(
            source_root.abs(),
            PluginId::parse("sample@test").expect("plugin id should parse"),
        )
        .unwrap();
    let migrated_skill = result
        .installed_path
        .join(".codex-plugin/migrated-command-skills/source-command-pr-review/SKILL.md");
    let expected_migrated_skill = "---\nname: \"source-command-pr-review\"\ndescription: \"Review a pull request\"\n---\n\n# source-command-pr-review\n\nUse this skill when the user asks to run the migrated source command `pr-review`.\n\n## Command Template\n\nInspect the proposed changes.\n";
    assert_eq!(
        fs::read_to_string(&migrated_skill).unwrap(),
        expected_migrated_skill
    );
    assert!(
        !result
            .installed_path
            .join(".codex-plugin/migrated-command-skills/undeclared-command")
            .exists()
    );
    assert!(
        !result
            .installed_path
            .join(".codex-plugin/migrated-command-skills/source-command-oversized")
            .exists()
    );

    let manifest = crate::manifest::load_plugin_manifest(&result.installed_path).unwrap();
    let resolved = load_plugin_skill_inventory(
        &result.installed_path,
        &PluginIdentity {
            plugin_id: result.plugin_id.as_key(),
        },
        &manifest,
        PluginManifestFormat::Legacy,
        /*restriction_product*/ None,
        /*plugin_skill_snapshots*/ None,
        test_skill_root_loader(codex_home.path()).as_ref(),
    )
    .await
    .resolve(&SkillConfigRules::default());
    assert_eq!(
        resolved
            .skills
            .iter()
            .map(|skill| skill
                .path_to_skills_md
                .to_abs_path()
                .expect("host skill path"))
            .collect::<Vec<_>>(),
        vec![
            AbsolutePathBuf::from_absolute_path_checked(
                fs::canonicalize(
                    result
                        .installed_path
                        .join("custom-skills/source-command-pr-review/SKILL.md")
                )
                .unwrap()
            )
            .unwrap(),
            AbsolutePathBuf::from_absolute_path_checked(
                fs::canonicalize(result.installed_path.join(
                    ".codex-plugin/migrated-command-skills/source-command-summarize/SKILL.md"
                ))
                .unwrap()
            )
            .unwrap()
        ]
    );
}

#[test]
fn install_plugin_ignores_invalid_commands_manifest_field() {
    let codex_home = TempDir::new().unwrap();
    let source_root = codex_home.path().join("source/sample");
    write_file(
        &source_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","commands":{}}"#,
    );
    write_file(
        &source_root.join("commands/review.md"),
        "---\ndescription: Review\n---\nReview the current change.\n",
    );

    let result = PluginStore::new(codex_home.path().to_path_buf())
        .install(
            source_root.abs(),
            PluginId::parse("sample@test").expect("plugin id should parse"),
        )
        .unwrap();

    assert!(
        !result
            .installed_path
            .join(".codex-plugin/migrated-command-skills")
            .exists()
    );
}

#[test]
fn install_plugin_ignores_command_migration_errors() {
    let codex_home = TempDir::new().unwrap();
    let source_root = codex_home.path().join("source/sample");
    write_file(
        &source_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","commands":"./commands/review.md"}"#,
    );
    fs::create_dir_all(source_root.join("commands")).unwrap();
    fs::write(source_root.join("commands/review.md"), [0xff]).unwrap();

    let result = PluginStore::new(codex_home.path().to_path_buf())
        .install(
            source_root.abs(),
            PluginId::parse("sample@test").expect("plugin id should parse"),
        )
        .unwrap();

    assert!(result.installed_path.join("commands/review.md").is_file());
}

#[tokio::test]
async fn load_plugin_skills_dedupes_overlapping_manifest_roots() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local")
        .abs();
    write_file(
        &plugin_root.join("skills/abc/SKILL.md"),
        "---\nname: abc\ndescription: abc skill\n---\n",
    );
    write_file(
        &plugin_root.join("skills/edk/SKILL.md"),
        "---\nname: edk\ndescription: edk skill\n---\n",
    );
    let manifest = crate::manifest::PluginManifest {
        name: "sample".to_string(),
        version: None,
        description: None,
        keywords: Vec::new(),
        paths: crate::manifest::PluginManifestPaths {
            onboarding_skill: None,
            skills: vec![
                plugin_root.join("skills"),
                plugin_root.join("skills/abc"),
                plugin_root.join("skills/edk"),
                plugin_root.join("skills/abc"),
            ],
            mcp_servers: None,
            apps: None,
            hooks: None,
        },
        interface: None,
    };
    let plugin_id = PluginId::parse("sample@test").expect("plugin id should parse");
    let resolved = load_plugin_skill_inventory(
        &plugin_root,
        &PluginIdentity {
            plugin_id: plugin_id.as_key(),
        },
        &manifest,
        PluginManifestFormat::Legacy,
        /*restriction_product*/ None,
        /*plugin_skill_snapshots*/ None,
        test_skill_root_loader(codex_home.path()).as_ref(),
    )
    .await
    .resolve(&SkillConfigRules::default());

    let skill_paths = resolved
        .skills
        .iter()
        .map(|skill| {
            skill
                .path_to_skills_md
                .to_abs_path()
                .expect("host skill path")
        })
        .collect::<Vec<_>>();
    let canonical_skill_path = |path| {
        AbsolutePathBuf::from_absolute_path_checked(
            fs::canonicalize(plugin_root.join(path)).expect("canonical skill path"),
        )
        .expect("absolute skill path")
    };
    assert_eq!(
        skill_paths,
        vec![
            canonical_skill_path("skills/abc/SKILL.md"),
            canonical_skill_path("skills/edk/SKILL.md")
        ]
    );
}

#[tokio::test]
async fn load_plugins_ignores_manifest_component_paths_without_dot_slash() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample",
  "skills": "custom-skills",
  "mcpServers": "config/custom.mcp.json",
  "apps": "config/custom.app.json"
}"#,
    );
    write_file(
        &plugin_root.join("skills/default-skill/SKILL.md"),
        "---\nname: default-skill\ndescription: default skill\n---\n",
    );
    write_file(
        &plugin_root.join("custom-skills/custom-skill/SKILL.md"),
        "---\nname: custom-skill\ndescription: custom skill\n---\n",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "default": {
      "type": "http",
      "url": "https://default.example/mcp"
    }
  }
}"#,
    );
    write_file(
        &plugin_root.join("config/custom.mcp.json"),
        r#"{
  "mcpServers": {
    "custom": {
      "type": "http",
      "url": "https://custom.example/mcp"
    }
  }
}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{
  "apps": {
    "default-app": {
      "id": "connector_default"
    }
  }
}"#,
    );
    write_file(
        &plugin_root.join("config/custom.app.json"),
        r#"{
  "apps": {
    "custom-app": {
      "id": "connector_custom"
    }
  }
}"#,
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
        codex_home.path(),
    )
    .await;

    assert_eq!(
        outcome.plugins()[0].skill_roots,
        vec![plugin_root.join("skills").abs()]
    );
    assert_eq!(
        outcome.plugins()[0].mcp_servers,
        HashMap::from([(
            "default".to_string(),
            McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::StreamableHttp {
                    url: "https://default.example/mcp".to_string(),
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                    http_headers_helper: None,
                },
                environment_id: "local".to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        )])
    );
    assert_eq!(
        outcome.plugins()[0].apps,
        vec![app_declaration("default-app", "connector_default")]
    );
}

#[tokio::test]
async fn load_plugins_ignores_invalid_manifest_skills_shape() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample",
  "skills": { "path": "./custom-skills/" }
}"#,
    );
    write_file(
        &plugin_root.join("skills/default-skill/SKILL.md"),
        "---\nname: default-skill\ndescription: default skill\n---\n",
    );
    write_file(
        &plugin_root.join("custom-skills/custom-skill/SKILL.md"),
        "---\nname: custom-skill\ndescription: custom skill\n---\n",
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
        codex_home.path(),
    )
    .await;

    assert_eq!(outcome.plugins()[0].error, None);
    assert_eq!(
        outcome.plugins()[0].skill_roots,
        vec![plugin_root.join("skills").abs()]
    );
}

#[tokio::test]
async fn load_plugins_preserves_disabled_plugins_without_effective_contributions() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "type": "http",
      "url": "https://sample.example/mcp"
    }
  }
}"#,
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(
            /*enabled*/ false, /*plugins_feature_enabled*/ true,
        ),
        codex_home.path(),
    )
    .await;

    assert_eq!(
        outcome.plugins(),
        vec![LoadedPlugin {
            config_name: "sample@test".to_string(),
            manifest_name: None,
            plugin_namespace: None,
            manifest_description: None,
            root: AbsolutePathBuf::try_from(plugin_root).unwrap(),
            enabled: false,
            skill_roots: Vec::new(),
            skill_discovery_mode: SkillDiscoveryMode::Recursive,
            disabled_skill_paths: HashSet::new(),
            has_enabled_skills: false,
            mcp_servers: HashMap::new(),
            apps: Vec::new(),
            hook_sources: Vec::new(),
            hook_load_warnings: Vec::new(),
            error: None,
        }]
    );
    assert!(outcome.effective_plugin_skill_roots().is_empty());
    assert!(outcome.effective_mcp_servers().is_empty());
}

#[tokio::test]
async fn load_plugins_returns_empty_when_feature_disabled() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );
    write_file(
        &plugin_root.join("skills/sample-search/SKILL.md"),
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );
    write_file(
        &codex_home.path().join(CONFIG_TOML_FILE),
        &plugin_config_toml(
            /*enabled*/ true, /*plugins_feature_enabled*/ false,
        ),
    );

    let config = load_config(codex_home.path(), codex_home.path()).await;
    let outcome = test_plugins_manager(codex_home.path().to_path_buf())
        .plugins_for_config(&config)
        .await;

    assert_eq!(outcome, PluginLoadOutcome::default());
}

#[tokio::test]
async fn load_plugins_rejects_invalid_plugin_keys() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );

    let mut root = toml::map::Map::new();
    let mut features = toml::map::Map::new();
    features.insert("plugins".to_string(), Value::Boolean(true));
    root.insert("features".to_string(), Value::Table(features));

    let mut plugin = toml::map::Map::new();
    plugin.insert("enabled".to_string(), Value::Boolean(true));

    let mut plugins = toml::map::Map::new();
    plugins.insert("sample".to_string(), Value::Table(plugin));
    root.insert("plugins".to_string(), Value::Table(plugins));

    let outcome = load_plugins_from_config(
        &toml::to_string(&Value::Table(root)).expect("plugin test config should serialize"),
        codex_home.path(),
    )
    .await;

    assert_eq!(outcome.plugins().len(), 1);
    assert_eq!(
        outcome.plugins()[0].error.as_deref(),
        Some("invalid plugin key `sample`; expected <plugin>@<marketplace>")
    );
    assert!(outcome.effective_plugin_skill_roots().is_empty());
    assert!(outcome.effective_mcp_servers().is_empty());
}

#[tokio::test]
async fn install_plugin_updates_config_with_relative_path_and_plugin_key() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin(&repo_root, "sample-plugin", "sample-plugin");
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      },
      "policy": {
        "authentication": "ON_USE"
      }
    }
  ]
}"#,
    )
    .unwrap();

    let result = test_plugins_manager(tmp.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "sample-plugin".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/sample-plugin/local");
    assert_eq!(
        result,
        PluginInstallOutcome {
            plugin_id: PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap(),
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path).unwrap(),
            auth_policy: MarketplacePluginAuthPolicy::OnUse,
        }
    );

    let config = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
    assert!(config.contains(r#"[plugins."sample-plugin@debug"]"#));
    assert!(config.contains("enabled = true"));
}

#[tokio::test]
async fn install_plugin_uses_manifest_version_for_non_curated_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin_with_version(
        &repo_root,
        "sample-plugin",
        "sample-plugin",
        Some("1.2.3-beta+7"),
    );
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    }
  ]
}"#,
    )
    .unwrap();

    let result = test_plugins_manager(tmp.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "sample-plugin".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    let installed_path = tmp
        .path()
        .join("plugins/cache/debug/sample-plugin/1.2.3-beta+7");
    assert_eq!(
        result,
        PluginInstallOutcome {
            plugin_id: PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap(),
            plugin_version: "1.2.3-beta+7".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path).unwrap(),
            auth_policy: MarketplacePluginAuthPolicy::OnInstall,
        }
    );
}

#[tokio::test]
async fn install_plugin_writes_marketplace_manifest_fallback_when_missing_plugin_json() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let plugin_root = repo_root.join("plugins/quality-review");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    fs::create_dir_all(plugin_root.join("skills/thermo-nuclear-code-quality-review")).unwrap();
    fs::write(
        plugin_root.join("skills/thermo-nuclear-code-quality-review/SKILL.md"),
        "review skill",
    )
    .unwrap();
    write_file(
        &plugin_root.join("commands/review.md"),
        "---\ndescription: Review code\n---\nReview the current change.\n",
    );
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "quality-review",
      "description": "Strict code quality review focused on maintainability.",
      "source": "./plugins/quality-review",
      "author": {
        "name": "Byron Grogan"
      },
      "skills": [
        "./skills/thermo-nuclear-code-quality-review"
      ],
      "commands": ["./commands/review.md"],
      "category": "code-review"
    }
  ]
}"#,
    )
    .unwrap();

    let result = test_plugins_manager(tmp.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "quality-review".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/quality-review/local");
    assert_eq!(
        result,
        PluginInstallOutcome {
            plugin_id: PluginId::new("quality-review".to_string(), "debug".to_string()).unwrap(),
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
            auth_policy: MarketplacePluginAuthPolicy::OnInstall,
        }
    );
    assert!(!plugin_root.join(".codex-plugin/plugin.json").exists());
    assert!(
        !tmp.path()
            .join("plugins/.marketplace-plugin-source-staging")
            .exists()
    );

    let manifest = crate::manifest::load_plugin_manifest(&installed_path).unwrap();
    assert_eq!(manifest.name, "quality-review");
    assert_eq!(
        manifest.description.as_deref(),
        Some("Strict code quality review focused on maintainability.")
    );
    assert_eq!(
        manifest.paths.skills,
        vec![
            AbsolutePathBuf::try_from(
                installed_path.join("skills/thermo-nuclear-code-quality-review")
            )
            .unwrap()
        ]
    );
    let interface = manifest.interface.expect("fallback interface");
    assert_eq!(interface.developer_name.as_deref(), Some("Byron Grogan"));
    assert_eq!(interface.category.as_deref(), Some("code-review"));
    let fallback_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(installed_path.join(".codex-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        fallback_json["author"],
        serde_json::json!({ "name": "Byron Grogan" })
    );
    assert_eq!(fallback_json["category"], "code-review");
    assert_eq!(
        fs::read_to_string(
            installed_path
                .join(".codex-plugin/migrated-command-skills/source-command-review/SKILL.md")
        )
        .unwrap(),
        "---\nname: \"source-command-review\"\ndescription: \"Review code\"\n---\n\n# source-command-review\n\nUse this skill when the user asks to run the migrated source command `review`.\n\n## Command Template\n\nReview the current change.\n"
    );
}

#[tokio::test]
async fn install_plugin_supports_git_subdir_marketplace_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("marketplace");
    let remote_repo = tmp.path().join("remote-plugin-repo");
    let remote_repo_url = url::Url::from_directory_path(&remote_repo)
        .unwrap()
        .to_string();
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin(&remote_repo, "plugins/toolkit", "toolkit");
    init_git_repo(&remote_repo);
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        format!(
            r#"{{
  "name": "debug",
  "plugins": [
    {{
      "name": "toolkit",
      "source": {{
        "source": "git-subdir",
        "url": "{remote_repo_url}",
        "path": "plugins/toolkit"
      }}
    }}
  ]
}}"#
        ),
    )
    .unwrap();

    let result = test_plugins_manager(tmp.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "toolkit".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/toolkit/local");
    assert_eq!(
        result,
        PluginInstallOutcome {
            plugin_id: PluginId::new("toolkit".to_string(), "debug".to_string()).unwrap(),
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
            auth_policy: MarketplacePluginAuthPolicy::OnInstall,
        }
    );
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
}

#[tokio::test]
async fn install_plugin_supports_relative_git_subdir_marketplace_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("marketplace");
    let remote_repo = repo_root.join("remote-plugin-repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin(&remote_repo, "plugins/toolkit", "toolkit");
    init_git_repo(&remote_repo);
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "toolkit",
      "source": {
        "source": "git-subdir",
        "url": "./remote-plugin-repo",
        "path": "plugins/toolkit"
      }
    }
  ]
}"#,
    )
    .unwrap();

    let result = test_plugins_manager(tmp.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "toolkit".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/toolkit/local");
    assert_eq!(
        result,
        PluginInstallOutcome {
            plugin_id: PluginId::new("toolkit".to_string(), "debug".to_string()).unwrap(),
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
            auth_policy: MarketplacePluginAuthPolicy::OnInstall,
        }
    );
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
}

#[tokio::test]
async fn uninstall_plugin_removes_cache_and_config_entry() {
    let tmp = tempfile::tempdir().unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/local",
        "sample-plugin",
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    let manager = test_plugins_manager(tmp.path().to_path_buf());
    manager
        .uninstall_plugin("sample-plugin@debug".to_string())
        .await
        .unwrap();
    manager
        .uninstall_plugin("sample-plugin@debug".to_string())
        .await
        .unwrap();

    assert!(
        !tmp.path()
            .join("plugins/cache/debug/sample-plugin")
            .exists()
    );
    let config = fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).unwrap();
    assert!(!config.contains(r#"[plugins."sample-plugin@debug"]"#));
}

#[tokio::test]
async fn list_marketplaces_includes_enabled_state() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "enabled-plugin/local",
        "enabled-plugin",
    );
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "disabled-plugin/local",
        "disabled-plugin",
    );
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "enabled-plugin",
      "source": {
        "source": "local",
        "path": "./enabled-plugin"
      }
    },
    {
      "name": "disabled-plugin",
      "source": {
        "source": "local",
        "path": "./disabled-plugin"
      }
    }
  ]
}"#,
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."enabled-plugin@debug"]
enabled = true

[plugins."disabled-plugin@debug"]
enabled = false
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(
            &config,
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            /*include_openai_curated*/ true,
        )
        .unwrap()
        .marketplaces;

    let marketplace = marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    tmp.path().join("repo/.agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("expected repo marketplace entry");

    assert_eq!(
        marketplace,
        ConfiguredMarketplace {
            name: "debug".to_string(),
            path: AbsolutePathBuf::try_from(
                tmp.path().join("repo/.agents/plugins/marketplace.json"),
            )
            .unwrap(),
            interface: None,
            plugins: vec![
                ConfiguredMarketplacePlugin {
                    id: "enabled-plugin@debug".to_string(),
                    name: "enabled-plugin".to_string(),
                    local_version: None,
                    installed_version: Some("local".to_string()),
                    source: MarketplacePluginSource::Local {
                        path: AbsolutePathBuf::try_from(tmp.path().join("repo/enabled-plugin"))
                            .unwrap(),
                    },
                    policy: MarketplacePluginPolicy {
                        installation: MarketplacePluginInstallPolicy::Available,
                        authentication: MarketplacePluginAuthPolicy::OnInstall,
                        products: None,
                    },
                    interface: None,
                    keywords: Vec::new(),
                    manifest_fallback: None,
                    installed: true,
                    enabled: true,
                },
                ConfiguredMarketplacePlugin {
                    id: "disabled-plugin@debug".to_string(),
                    name: "disabled-plugin".to_string(),
                    local_version: None,
                    installed_version: Some("local".to_string()),
                    source: MarketplacePluginSource::Local {
                        path: AbsolutePathBuf::try_from(tmp.path().join("repo/disabled-plugin"),)
                            .unwrap(),
                    },
                    policy: MarketplacePluginPolicy {
                        installation: MarketplacePluginInstallPolicy::Available,
                        authentication: MarketplacePluginAuthPolicy::OnInstall,
                        products: None,
                    },
                    interface: None,
                    keywords: Vec::new(),
                    manifest_fallback: None,
                    installed: true,
                    enabled: false,
                },
            ],
        }
    );
}

#[tokio::test]
async fn list_marketplaces_returns_empty_when_feature_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "enabled-plugin",
      "source": {
        "source": "local",
        "path": "./enabled-plugin"
      }
    }
  ]
}"#,
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = false

[plugins."enabled-plugin@debug"]
enabled = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(
            &config,
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            /*include_openai_curated*/ true,
        )
        .unwrap()
        .marketplaces;

    assert_eq!(marketplaces, Vec::new());
}

#[tokio::test]
async fn list_marketplaces_excludes_plugins_with_explicit_empty_products() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "disabled-plugin",
      "source": {
        "source": "local",
        "path": "./disabled-plugin"
      },
      "policy": {
        "products": []
      }
    },
    {
      "name": "default-plugin",
      "source": {
        "source": "local",
        "path": "./default-plugin"
      }
    }
  ]
}"#,
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(
            &config,
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            /*include_openai_curated*/ true,
        )
        .unwrap()
        .marketplaces;

    let marketplace = marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    tmp.path().join("repo/.agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("expected repo marketplace entry");
    assert_eq!(
        marketplace.plugins,
        vec![ConfiguredMarketplacePlugin {
            id: "default-plugin@debug".to_string(),
            name: "default-plugin".to_string(),
            local_version: None,
            installed_version: None,
            source: MarketplacePluginSource::Local {
                path: AbsolutePathBuf::try_from(tmp.path().join("repo/default-plugin")).unwrap(),
            },
            policy: MarketplacePluginPolicy {
                installation: MarketplacePluginInstallPolicy::Available,
                authentication: MarketplacePluginAuthPolicy::OnInstall,
                products: None,
            },
            interface: None,
            keywords: Vec::new(),
            manifest_fallback: None,
            installed: false,
            enabled: false,
        }]
    );
}

#[tokio::test]
async fn read_plugin_for_config_returns_plugins_disabled_when_feature_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.join(".agents/plugins/marketplace.json")).unwrap();
    fs::write(
        marketplace_path.as_path(),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "enabled-plugin",
      "source": {
        "source": "local",
        "path": "./enabled-plugin"
      }
    }
  ]
}"#,
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = false

[plugins."enabled-plugin@debug"]
enabled = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let err = test_plugins_manager(tmp.path().to_path_buf())
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "enabled-plugin".to_string(),
                marketplace_path,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, MarketplaceError::PluginsDisabled));
}

#[tokio::test]
async fn read_plugin_for_config_uses_marketplace_manifest_fallback_paths_for_local_source() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let plugin_root = repo_root.join("sample-plugin");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": "./sample-plugin",
      "apps": "./config/custom.app.json",
      "mcpServers": {
        "sample-mcp": {
          "command": "sample-mcp"
        }
      }
    }
  ]
}"#,
    );
    write_file(
        &plugin_root.join("config/custom.app.json"),
        r#"{"apps":{"sample-app":{"id":"connector_sample"}}}"#,
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let manager = test_plugins_manager_with_options(tmp.path().to_path_buf(), Some(Product::Codex));
    let outcome = manager
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "sample-plugin".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.plugin.apps,
        vec![AppConnectorId("connector_sample".to_string())]
    );
    assert_eq!(
        outcome.plugin.mcp_server_names,
        vec!["sample-mcp".to_string()]
    );

    let listed_plugin = manager
        .list_marketplaces_for_config(
            &config,
            &[AbsolutePathBuf::try_from(repo_root.clone()).unwrap()],
            /*include_openai_curated*/ false,
        )
        .unwrap()
        .marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(repo_root.join(".agents/plugins/marketplace.json"))
                    .unwrap()
        })
        .unwrap()
        .plugins
        .into_iter()
        .find(|plugin| plugin.name == "sample-plugin")
        .unwrap();
    let listed_detail = manager
        .read_plugin_detail_for_marketplace_plugin(&config, "debug", listed_plugin)
        .await
        .unwrap();
    assert_eq!(
        listed_detail.apps,
        vec![AppConnectorId("connector_sample".to_string())]
    );
    assert_eq!(
        listed_detail.mcp_server_names,
        vec!["sample-mcp".to_string()]
    );
}

#[tokio::test]
async fn read_plugin_for_config_does_not_fallback_from_invalid_plugin_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let plugin_root = repo_root.join("sample-plugin");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": "./sample-plugin",
      "description": "Fallback metadata"
    }
  ]
}"#,
    );
    write_file(&plugin_root.join(".codex-plugin/plugin.json"), "{");
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let err = test_plugins_manager(tmp.path().to_path_buf())
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "sample-plugin".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.to_string(), "missing or invalid plugin.json");
}

#[tokio::test]
async fn read_plugin_for_config_uses_user_layer_skill_settings_only() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let plugin_root = repo_root.join("enabled-plugin");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "enabled-plugin",
      "source": {
        "source": "local",
        "path": "./enabled-plugin"
      }
    }
  ]
}"#,
    );
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"enabled-plugin"}"#,
    );
    write_file(
        &plugin_root.join("skills/sample-search/SKILL.md"),
        "---\nname: sample-search\ndescription: search sample data\n---\n",
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."enabled-plugin@debug"]
enabled = true
"#,
    );
    write_file(
        &repo_root.join(".codex/config.toml"),
        r#"[[skills.config]]
name = "enabled-plugin:sample-search"
enabled = false
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let outcome = test_plugins_manager(tmp.path().to_path_buf())
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "enabled-plugin".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    assert!(outcome.plugin.disabled_skill_paths.is_empty());
}

#[tokio::test]
async fn read_plugin_for_config_uninstalled_git_source_requires_install_without_cloning() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let missing_remote_repo = tmp.path().join("missing-remote-plugin-repo");
    let missing_remote_repo_url = url::Url::from_directory_path(&missing_remote_repo)
        .unwrap()
        .to_string();
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        &format!(
            r#"{{
  "name": "debug",
  "plugins": [
    {{
      "name": "toolkit",
      "source": {{
        "source": "git-subdir",
        "url": "{missing_remote_repo_url}",
        "path": "plugins/toolkit"
      }},
      "policy": {{
        "installation": "AVAILABLE",
        "authentication": "ON_INSTALL"
      }}
    }}
  ]
}}"#
        ),
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let outcome = test_plugins_manager(tmp.path().to_path_buf())
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "toolkit".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.plugin.details_unavailable_reason,
        Some(PluginDetailsUnavailableReason::InstallRequiredForRemoteSource)
    );
    assert!(!outcome.plugin.installed);
    let expected_description = format!(
        "This is a cross-repo plugin. Install it to view more detailed information. The source of the plugin is {missing_remote_repo_url}, path `plugins/toolkit`."
    );
    assert_eq!(
        outcome.plugin.description.as_deref(),
        Some(expected_description.as_str())
    );
    assert!(outcome.plugin.skills.is_empty());
    assert!(outcome.plugin.apps.is_empty());
    assert!(outcome.plugin.mcp_server_names.is_empty());
    assert!(
        !tmp.path()
            .join("plugins/.marketplace-plugin-source-staging")
            .exists()
    );
}

#[tokio::test]
async fn read_plugin_for_config_installed_git_source_reads_from_cache_without_cloning() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let missing_remote_repo = tmp.path().join("missing-remote-plugin-repo");
    let missing_remote_repo_url = url::Url::from_directory_path(&missing_remote_repo)
        .unwrap()
        .to_string();
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        &format!(
            r#"{{
  "name": "debug",
  "plugins": [
    {{
      "name": "toolkit",
      "source": {{
        "source": "git-subdir",
        "url": "{missing_remote_repo_url}",
        "path": "plugins/toolkit"
      }},
      "category": "Developer Tools"
    }}
  ]
}}"#
        ),
    );
    let cached_plugin_root = tmp.path().join("plugins/cache/debug/toolkit/local");
    write_file(
        &cached_plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "toolkit",
  "description": "Cached toolkit plugin",
  "interface": {
    "displayName": "Toolkit"
  }
}"#,
    );
    write_file(
        &cached_plugin_root.join("skills/search/SKILL.md"),
        "---\nname: search\ndescription: search cached data\n---\n",
    );
    write_file(
        &cached_plugin_root.join(".app.json"),
        r#"{
  "apps": {
    "calendar": {
      "id": "connector_calendar",
      "category": "First Category"
    },
    "calendar_duplicate": {
      "id": "connector_calendar",
      "category": "Second Category"
    }
  }
}"#,
    );
    write_file(
        &cached_plugin_root.join(".mcp.json"),
        r#"{"mcpServers":{"toolkit":{"command":"toolkit-mcp"}}}"#,
    );
    write_file(
        &cached_plugin_root.join("hooks/hooks.json"),
        r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo startup"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo first"
          },
          {
            "type": "command",
            "command": "echo second"
          }
        ]
      }
    ]
  }
}"#,
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."toolkit@debug"]
enabled = true

[hooks.state."toolkit@debug:hooks/hooks.json:pre_tool_use:0:0"]
enabled = false
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let outcome = test_plugins_manager_with_options(tmp.path().to_path_buf(), Some(Product::Codex))
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "toolkit".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    assert_eq!(outcome.plugin.details_unavailable_reason, None);
    assert_eq!(
        outcome.plugin.description.as_deref(),
        Some("Cached toolkit plugin")
    );
    assert_eq!(
        outcome.plugin.interface,
        Some(PluginManifestInterface {
            display_name: Some("Toolkit".to_string()),
            category: Some("Developer Tools".to_string()),
            ..Default::default()
        })
    );
    assert!(outcome.plugin.installed);
    assert_eq!(outcome.plugin.skills.len(), 1);
    assert_eq!(outcome.plugin.skills[0].name, "toolkit:search");
    assert_eq!(
        outcome.plugin.apps,
        vec![AppConnectorId("connector_calendar".to_string())]
    );
    assert_eq!(
        outcome.plugin.app_category_by_id,
        HashMap::from([(
            "connector_calendar".to_string(),
            "First Category".to_string()
        )])
    );
    assert_eq!(
        outcome.plugin.hooks,
        vec![
            PluginHookSummary {
                key: "toolkit@debug:hooks/hooks.json:pre_tool_use:0:0".to_string(),
                event_name: HookEventName::PreToolUse,
            },
            PluginHookSummary {
                key: "toolkit@debug:hooks/hooks.json:pre_tool_use:0:1".to_string(),
                event_name: HookEventName::PreToolUse,
            },
            PluginHookSummary {
                key: "toolkit@debug:hooks/hooks.json:session_start:0:0".to_string(),
                event_name: HookEventName::SessionStart,
            },
        ]
    );
    assert_eq!(outcome.plugin.mcp_server_names, vec!["toolkit".to_string()]);
    assert!(
        !tmp.path()
            .join("plugins/.marketplace-plugin-source-staging")
            .exists()
    );
}

#[tokio::test]
async fn list_marketplaces_installed_git_source_reads_metadata_from_cache_without_cloning() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let missing_remote_repo = tmp.path().join("missing-remote-plugin-repo");
    let missing_remote_repo_url = url::Url::from_directory_path(&missing_remote_repo)
        .unwrap()
        .to_string();
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        &format!(
            r#"{{
  "name": "debug",
  "plugins": [
    {{
      "name": "toolkit",
      "source": {{
        "source": "git-subdir",
        "url": "{missing_remote_repo_url}",
        "path": "plugins/toolkit"
      }},
      "category": "Developer Tools"
    }}
  ]
}}"#
        ),
    );
    let cached_plugin_root = tmp.path().join("plugins/cache/debug/toolkit/local");
    write_file(
        &cached_plugin_root.join(".codex-plugin/plugin.json"),
        r##"{
  "name": "toolkit",
  "interface": {
    "displayName": "Toolkit",
    "shortDescription": "Search cached data",
    "category": "Cached Category",
    "brandColor": "#3B82F6",
    "composerIcon": "./assets/icon.png",
    "logo": "./assets/logo.png",
    "screenshots": ["./assets/screenshot.png"]
  }
}"##,
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."toolkit@debug"]
enabled = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(
            &config,
            &[AbsolutePathBuf::try_from(repo_root.clone()).unwrap()],
            /*include_openai_curated*/ true,
        )
        .unwrap()
        .marketplaces;

    let marketplace = marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(repo_root.join(".agents/plugins/marketplace.json"))
                    .unwrap()
        })
        .expect("debug marketplace should be listed");

    let mut plugins = marketplace.plugins;
    assert!(plugins[0].manifest_fallback.is_some());
    plugins[0].manifest_fallback = None;
    assert_eq!(
        plugins,
        vec![ConfiguredMarketplacePlugin {
            id: "toolkit@debug".to_string(),
            name: "toolkit".to_string(),
            local_version: None,
            installed_version: Some("local".to_string()),
            source: MarketplacePluginSource::Git {
                url: missing_remote_repo_url,
                path: Some("plugins/toolkit".to_string()),
                ref_name: None,
                sha: None,
            },
            policy: MarketplacePluginPolicy {
                installation: MarketplacePluginInstallPolicy::Available,
                authentication: MarketplacePluginAuthPolicy::OnInstall,
                products: None,
            },
            interface: Some(PluginManifestInterface {
                display_name: Some("Toolkit".to_string()),
                short_description: Some("Search cached data".to_string()),
                category: Some("Developer Tools".to_string()),
                brand_color: Some("#3B82F6".to_string()),
                composer_icon: Some(
                    AbsolutePathBuf::try_from(cached_plugin_root.join("assets/icon.png")).unwrap(),
                ),
                logo: Some(
                    AbsolutePathBuf::try_from(cached_plugin_root.join("assets/logo.png")).unwrap(),
                ),
                screenshots: vec![
                    AbsolutePathBuf::try_from(cached_plugin_root.join("assets/screenshot.png"))
                        .unwrap(),
                ],
                ..Default::default()
            }),
            keywords: Vec::new(),
            manifest_fallback: None,
            installed: true,
            enabled: true,
        }]
    );
    assert!(
        !tmp.path()
            .join("plugins/.marketplace-plugin-source-staging")
            .exists()
    );
}

#[tokio::test]
async fn list_marketplaces_includes_curated_repo_marketplace() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    let plugin_root = curated_root.join("plugins/linear");

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );
    fs::create_dir_all(curated_root.join(".agents/plugins")).unwrap();
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::write(
        curated_root.join(".agents/plugins/api_marketplace.json"),
        r#"{
  "name": "openai-api-curated",
  "plugins": [
    {
      "name": "linear",
      "source": {
        "source": "local",
        "path": "./plugins/linear"
      }
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"linear"}"#,
    )
    .unwrap();

    let config = load_config(tmp.path(), tmp.path()).await;
    let marketplaces =
        test_plugins_manager_with_options(tmp.path().to_path_buf(), Some(Product::Codex))
            .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ true)
            .unwrap()
            .marketplaces;

    let curated_marketplace = marketplaces
        .into_iter()
        .find(|marketplace| marketplace.name == "openai-api-curated")
        .expect("curated marketplace should be listed");

    assert_eq!(
        curated_marketplace,
        ConfiguredMarketplace {
            name: "openai-api-curated".to_string(),
            path: AbsolutePathBuf::try_from(
                curated_root.join(".agents/plugins/api_marketplace.json")
            )
            .unwrap(),
            interface: None,
            plugins: vec![ConfiguredMarketplacePlugin {
                id: "linear@openai-api-curated".to_string(),
                name: "linear".to_string(),
                local_version: None,
                installed_version: None,
                source: MarketplacePluginSource::Local {
                    path: AbsolutePathBuf::try_from(curated_root.join("plugins/linear")).unwrap(),
                },
                policy: MarketplacePluginPolicy {
                    installation: MarketplacePluginInstallPolicy::Available,
                    authentication: MarketplacePluginAuthPolicy::OnInstall,
                    products: None,
                },
                interface: None,
                keywords: Vec::new(),
                manifest_fallback: None,
                installed: false,
                enabled: false,
            }],
        }
    );
}

#[tokio::test]
async fn list_marketplaces_can_skip_openai_curated_before_loading() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );
    write_file(
        &curated_root.join(".agents/plugins/marketplace.json"),
        "{not valid json",
    );

    let config = load_config(tmp.path(), tmp.path()).await;
    let outcome = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ false)
        .unwrap();

    assert_eq!(outcome.errors, Vec::new());
    assert_eq!(
        outcome
            .marketplaces
            .iter()
            .any(|marketplace| marketplace.name == OPENAI_CURATED_MARKETPLACE_NAME),
        false
    );
}

#[tokio::test]
async fn list_marketplaces_uses_api_curated_manifest_when_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );
    write_file(
        &curated_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "openai-curated",
  "plugins": [
    {
      "name": "siwc-plugin",
      "source": {
        "source": "local",
        "path": "./plugins/siwc-plugin"
      }
    }
  ]
}"#,
    );
    write_file(
        &curated_root.join(".agents/plugins/api_marketplace.json"),
        r#"{
  "name": "openai-api-curated",
  "interface": {
    "displayName": "OpenAI Curated"
  },
  "plugins": [
    {
      "name": "api-plugin",
      "source": {
        "source": "local",
        "path": "./plugins/api-plugin"
      }
    }
  ]
}"#,
    );

    let config = load_config(tmp.path(), tmp.path()).await;
    let manager = test_plugins_manager_with_options(tmp.path().to_path_buf(), Some(Product::Codex));
    let marketplaces = manager
        .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ true)
        .unwrap()
        .marketplaces;
    let curated_marketplace = marketplaces
        .into_iter()
        .find(|marketplace| marketplace.name == OPENAI_API_CURATED_MARKETPLACE_NAME)
        .expect("API curated marketplace should be listed");

    assert_eq!(
        curated_marketplace,
        ConfiguredMarketplace {
            name: "openai-api-curated".to_string(),
            path: AbsolutePathBuf::try_from(
                curated_root.join(".agents/plugins/api_marketplace.json")
            )
            .unwrap(),
            interface: Some(MarketplaceInterface {
                display_name: Some("OpenAI Curated".to_string()),
            }),
            plugins: vec![ConfiguredMarketplacePlugin {
                id: "api-plugin@openai-api-curated".to_string(),
                name: "api-plugin".to_string(),
                local_version: None,
                installed_version: None,
                source: MarketplacePluginSource::Local {
                    path: AbsolutePathBuf::try_from(curated_root.join("plugins/api-plugin"))
                        .unwrap(),
                },
                policy: MarketplacePluginPolicy {
                    installation: MarketplacePluginInstallPolicy::Available,
                    authentication: MarketplacePluginAuthPolicy::OnInstall,
                    products: None,
                },
                interface: None,
                keywords: Vec::new(),
                manifest_fallback: None,
                installed: false,
                enabled: false,
            }],
        }
    );
}

#[tokio::test]
async fn list_marketplaces_skips_missing_api_curated_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );
    write_file(
        &curated_root.join(".agents/plugins/marketplace.json"),
        "{not valid json",
    );

    let config = load_config(tmp.path(), tmp.path()).await;
    let manager = test_plugins_manager_with_options(tmp.path().to_path_buf(), Some(Product::Codex));
    let outcome = manager
        .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ true)
        .unwrap();

    assert_eq!(outcome.errors, Vec::new());
    assert_eq!(
        outcome
            .marketplaces
            .iter()
            .any(|marketplace| marketplace.name == OPENAI_API_CURATED_MARKETPLACE_NAME),
        false
    );
}

#[tokio::test]
async fn list_marketplaces_includes_installed_marketplace_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let marketplace_root = marketplace_install_root(tmp.path()).join("debug");
    let plugin_root = marketplace_root.join("plugins/sample");

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[marketplaces.debug]
last_updated = "2026-04-10T12:34:56Z"
source_type = "git"
source = "/tmp/debug"
"#,
    );
    fs::create_dir_all(marketplace_root.join(".agents/plugins")).unwrap();
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::write(
        marketplace_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample",
      "source": {
        "source": "local",
        "path": "./plugins/sample"
      }
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )
    .unwrap();
    let config = load_config(tmp.path(), tmp.path()).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ true)
        .unwrap()
        .marketplaces;

    let marketplace = marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    marketplace_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("installed marketplace should be listed");

    assert_eq!(
        marketplace.path,
        AbsolutePathBuf::try_from(marketplace_root.join(".agents/plugins/marketplace.json"))
            .unwrap()
    );
    assert_eq!(marketplace.plugins.len(), 1);
    assert_eq!(marketplace.plugins[0].id, "sample@debug");
    assert_eq!(
        marketplace.plugins[0].source,
        MarketplacePluginSource::Local {
            path: AbsolutePathBuf::try_from(plugin_root).unwrap(),
        }
    );
}

#[tokio::test]
async fn list_marketplaces_uses_config_when_known_registry_is_malformed() {
    let tmp = tempfile::tempdir().unwrap();
    let marketplace_root = marketplace_install_root(tmp.path()).join("debug");
    let plugin_root = marketplace_root.join("plugins/sample");
    let registry_path = tmp.path().join(".tmp/known_marketplaces.json");

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[marketplaces.debug]
last_updated = "2026-04-10T12:34:56Z"
source_type = "git"
source = "/tmp/debug"
"#,
    );
    fs::create_dir_all(marketplace_root.join(".agents/plugins")).unwrap();
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::write(
        marketplace_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample",
      "source": {
        "source": "local",
        "path": "./plugins/sample"
      }
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )
    .unwrap();
    fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    fs::write(registry_path, "{not valid json").unwrap();

    let config = load_config(tmp.path(), tmp.path()).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ true)
        .unwrap()
        .marketplaces;

    let marketplace = marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    marketplace_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("configured marketplace should be discovered");

    assert_eq!(marketplace.plugins[0].id, "sample@debug");
}

#[tokio::test]
async fn list_marketplaces_ignores_installed_roots_missing_from_config() {
    let tmp = tempfile::tempdir().unwrap();
    let marketplace_root = marketplace_install_root(tmp.path()).join("debug");
    let plugin_root = marketplace_root.join("plugins/sample");

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );
    fs::create_dir_all(marketplace_root.join(".agents/plugins")).unwrap();
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::write(
        marketplace_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample",
      "source": {
        "source": "local",
        "path": "./plugins/sample"
      }
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )
    .unwrap();
    let config = load_config(tmp.path(), tmp.path()).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(&config, &[], /*include_openai_curated*/ true)
        .unwrap()
        .marketplaces;

    assert!(
        marketplaces.iter().all(|marketplace| {
            marketplace.path
                != AbsolutePathBuf::try_from(
                    marketplace_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap()
        }),
        "installed marketplace root missing from config should not be listed"
    );
}

#[tokio::test]
async fn list_marketplaces_uses_first_duplicate_plugin_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_a_root = tmp.path().join("repo-a");
    let repo_b_root = tmp.path().join("repo-b");
    fs::create_dir_all(repo_a_root.join(".git")).unwrap();
    fs::create_dir_all(repo_b_root.join(".git")).unwrap();
    fs::create_dir_all(repo_a_root.join(".agents/plugins")).unwrap();
    fs::create_dir_all(repo_b_root.join(".agents/plugins")).unwrap();
    fs::write(
        repo_a_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "dup-plugin",
      "source": {
        "source": "local",
        "path": "./from-a"
      }
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(
        repo_b_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "dup-plugin",
      "source": {
        "source": "local",
        "path": "./from-b"
      }
    },
    {
      "name": "b-only-plugin",
      "source": {
        "source": "local",
        "path": "./from-b-only"
      }
    }
  ]
}"#,
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."dup-plugin@debug"]
enabled = true

[plugins."b-only-plugin@debug"]
enabled = false
"#,
    );

    let config = load_config(tmp.path(), &repo_a_root).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(
            &config,
            &[
                AbsolutePathBuf::try_from(repo_a_root).unwrap(),
                AbsolutePathBuf::try_from(repo_b_root).unwrap(),
            ],
            /*include_openai_curated*/ true,
        )
        .unwrap()
        .marketplaces;

    let repo_a_marketplace = marketplaces
        .iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    tmp.path().join("repo-a/.agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("repo-a marketplace should be listed");
    assert_eq!(
        repo_a_marketplace.plugins,
        vec![ConfiguredMarketplacePlugin {
            id: "dup-plugin@debug".to_string(),
            name: "dup-plugin".to_string(),
            local_version: None,
            installed_version: None,
            source: MarketplacePluginSource::Local {
                path: AbsolutePathBuf::try_from(tmp.path().join("repo-a/from-a")).unwrap(),
            },
            policy: MarketplacePluginPolicy {
                installation: MarketplacePluginInstallPolicy::Available,
                authentication: MarketplacePluginAuthPolicy::OnInstall,
                products: None,
            },
            interface: None,
            keywords: Vec::new(),
            manifest_fallback: None,
            installed: false,
            enabled: true,
        }]
    );

    let repo_b_marketplace = marketplaces
        .iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    tmp.path().join("repo-b/.agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("repo-b marketplace should be listed");
    assert_eq!(
        repo_b_marketplace.plugins,
        vec![ConfiguredMarketplacePlugin {
            id: "b-only-plugin@debug".to_string(),
            name: "b-only-plugin".to_string(),
            local_version: None,
            installed_version: None,
            source: MarketplacePluginSource::Local {
                path: AbsolutePathBuf::try_from(tmp.path().join("repo-b/from-b-only")).unwrap(),
            },
            policy: MarketplacePluginPolicy {
                installation: MarketplacePluginInstallPolicy::Available,
                authentication: MarketplacePluginAuthPolicy::OnInstall,
                products: None,
            },
            interface: None,
            keywords: Vec::new(),
            manifest_fallback: None,
            installed: false,
            enabled: false,
        }]
    );

    let duplicate_plugin_count = marketplaces
        .iter()
        .flat_map(|marketplace| marketplace.plugins.iter())
        .filter(|plugin| plugin.name == "dup-plugin")
        .count();
    assert_eq!(duplicate_plugin_count, 1);
}

#[tokio::test]
async fn list_marketplaces_marks_configured_plugin_uninstalled_when_cache_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    }
  ]
}"#,
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    let config = load_config(tmp.path(), &repo_root).await;
    let marketplaces = test_plugins_manager(tmp.path().to_path_buf())
        .list_marketplaces_for_config(
            &config,
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            /*include_openai_curated*/ true,
        )
        .unwrap()
        .marketplaces;

    let marketplace = marketplaces
        .into_iter()
        .find(|marketplace| {
            marketplace.path
                == AbsolutePathBuf::try_from(
                    tmp.path().join("repo/.agents/plugins/marketplace.json"),
                )
                .unwrap()
        })
        .expect("expected repo marketplace entry");

    assert_eq!(
        marketplace,
        ConfiguredMarketplace {
            name: "debug".to_string(),
            path: AbsolutePathBuf::try_from(
                tmp.path().join("repo/.agents/plugins/marketplace.json"),
            )
            .unwrap(),
            interface: None,
            plugins: vec![ConfiguredMarketplacePlugin {
                id: "sample-plugin@debug".to_string(),
                name: "sample-plugin".to_string(),
                local_version: None,
                installed_version: None,
                source: MarketplacePluginSource::Local {
                    path: AbsolutePathBuf::try_from(tmp.path().join("repo/sample-plugin")).unwrap(),
                },
                policy: MarketplacePluginPolicy {
                    installation: MarketplacePluginInstallPolicy::Available,
                    authentication: MarketplacePluginAuthPolicy::OnInstall,
                    products: None,
                },
                interface: None,
                keywords: Vec::new(),
                manifest_fallback: None,
                installed: false,
                enabled: true,
            }],
        }
    );
}

#[test]
fn refresh_curated_plugin_cache_replaces_existing_local_version_with_short_sha_version() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &["slack"]);
    write_curated_plugin_sha_with(tmp.path(), TEST_CURATED_PLUGIN_SHA);
    let plugin_id = PluginId::new(
        "slack".to_string(),
        OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/openai-curated"),
        "slack/local",
        "slack",
    );

    assert!(
        refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should succeed")
    );

    assert!(
        !tmp.path()
            .join("plugins/cache/openai-curated/slack/local")
            .exists()
    );
    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/slack/{TEST_CURATED_PLUGIN_CACHE_VERSION}"
            ))
            .is_dir()
    );
}

#[test]
fn refresh_curated_plugin_cache_reinstalls_missing_configured_plugin_with_current_short_version() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &["slack"]);
    write_curated_plugin_sha_with(tmp.path(), TEST_CURATED_PLUGIN_SHA);
    let plugin_id = PluginId::new(
        "slack".to_string(),
        OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();

    assert!(
        refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should recreate missing configured plugin")
    );

    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/slack/{TEST_CURATED_PLUGIN_CACHE_VERSION}"
            ))
            .is_dir()
    );
}

#[test]
fn refresh_curated_plugin_cache_reinstalls_missing_api_curated_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &[]);
    write_openai_api_curated_marketplace(&curated_root, &["api-only"]);
    write_curated_plugin_sha_with(tmp.path(), TEST_CURATED_PLUGIN_SHA);
    let plugin_id = PluginId::new(
        "api-only".to_string(),
        OPENAI_API_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();

    assert!(
        refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should recreate missing configured API curated plugin")
    );

    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-api-curated/api-only/{TEST_CURATED_PLUGIN_CACHE_VERSION}"
            ))
            .is_dir()
    );
}

#[test]
fn refresh_curated_plugin_cache_leaves_api_curated_plugin_when_api_manifest_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &[]);
    write_cached_plugin(tmp.path(), OPENAI_API_CURATED_MARKETPLACE_NAME, "api-only");
    let plugin_id = PluginId::new(
        "api-only".to_string(),
        OPENAI_API_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();

    assert!(
        !refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should skip missing API curated manifest")
    );
    assert!(
        tmp.path()
            .join("plugins/cache/openai-api-curated/api-only/local")
            .is_dir()
    );
}

#[test]
fn refresh_curated_plugin_cache_removes_cache_for_plugin_removed_from_marketplace() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &[]);
    let plugin_id = PluginId::new(
        "google-sheets".to_string(),
        OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();
    let plugin_cache_root = tmp
        .path()
        .join("plugins/cache/openai-curated/google-sheets");
    write_plugin(
        &tmp.path().join("plugins/cache/openai-curated"),
        &format!("google-sheets/{TEST_CURATED_PLUGIN_CACHE_VERSION}"),
        "google-sheets",
    );

    assert!(
        refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should remove stale configured plugin")
    );

    assert!(!plugin_cache_root.exists());
}

#[test]
fn refresh_curated_plugin_cache_returns_false_when_configured_plugins_are_current() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &["slack"]);
    let plugin_id = PluginId::new(
        "slack".to_string(),
        OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/openai-curated"),
        &format!("slack/{TEST_CURATED_PLUGIN_CACHE_VERSION}"),
        "slack",
    );

    assert!(
        !refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should be a no-op when configured plugins are current")
    );
}

#[test]
fn refresh_curated_plugin_cache_migrates_full_sha_cache_version_to_short_version() {
    let tmp = tempfile::tempdir().unwrap();
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &["slack"]);
    let plugin_id = PluginId::new(
        "slack".to_string(),
        OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/openai-curated"),
        &format!("slack/{TEST_CURATED_PLUGIN_SHA}"),
        "slack",
    );

    assert!(
        refresh_curated_plugin_cache(tmp.path(), TEST_CURATED_PLUGIN_SHA, &[plugin_id])
            .expect("cache refresh should migrate the full sha cache version")
    );
    assert!(
        !tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/slack/{TEST_CURATED_PLUGIN_SHA}"
            ))
            .exists()
    );
    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/slack/{TEST_CURATED_PLUGIN_CACHE_VERSION}"
            ))
            .is_dir()
    );
}

#[test]
fn refresh_non_curated_plugin_cache_replaces_existing_local_version_with_manifest_version() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin_with_version(&repo_root, "sample-plugin", "sample-plugin", Some("1.2.3"));
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    }
  ]
}"#,
    );
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/local",
        "sample-plugin",
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    assert!(
        refresh_non_curated_plugin_cache(
            tmp.path(),
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            &["sample-plugin@debug".to_string()],
        )
        .expect("cache refresh should succeed")
    );

    assert!(
        !tmp.path()
            .join("plugins/cache/debug/sample-plugin/local")
            .exists()
    );
    assert!(
        tmp.path()
            .join("plugins/cache/debug/sample-plugin/1.2.3")
            .is_dir()
    );
}

#[test]
fn refresh_non_curated_plugin_cache_reinstalls_missing_configured_plugin_with_manifest_version() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin_with_version(&repo_root, "sample-plugin", "sample-plugin", Some("1.2.3"));
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    }
  ]
}"#,
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    assert!(
        refresh_non_curated_plugin_cache(
            tmp.path(),
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            &["sample-plugin@debug".to_string()],
        )
        .expect("cache refresh should reinstall missing configured plugin")
    );

    assert!(
        tmp.path()
            .join("plugins/cache/debug/sample-plugin/1.2.3")
            .is_dir()
    );
}

#[test]
fn refresh_non_curated_plugin_cache_refreshes_configured_git_source() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    let remote_repo = tmp.path().join("remote-plugin-repo");
    let remote_repo_url = url::Url::from_directory_path(&remote_repo)
        .unwrap()
        .to_string();
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    write_plugin_with_version(
        &remote_repo,
        "plugins/sample-plugin",
        "sample-plugin",
        Some("1.2.3"),
    );
    init_git_repo(&remote_repo);
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        &format!(
            r#"{{
  "name": "debug",
  "plugins": [
    {{
      "name": "sample-plugin",
      "source": {{
        "source": "git-subdir",
        "url": "{remote_repo_url}",
        "path": "plugins/sample-plugin"
      }}
    }}
  ]
}}"#
        ),
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    assert!(
        refresh_non_curated_plugin_cache(
            tmp.path(),
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            &["sample-plugin@debug".to_string()],
        )
        .expect("cache refresh should materialize configured Git plugin")
    );

    assert!(
        tmp.path()
            .join("plugins/cache/debug/sample-plugin/1.2.3")
            .is_dir()
    );
}

#[test]
fn refresh_non_curated_plugin_cache_returns_false_when_configured_plugins_are_current() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin_with_version(&repo_root, "sample-plugin", "sample-plugin", Some("1.2.3"));
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    }
  ]
}"#,
    );
    write_plugin_with_version(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/1.2.3",
        "sample-plugin",
        Some("1.2.3"),
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    assert!(
        !refresh_non_curated_plugin_cache(
            tmp.path(),
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            &["sample-plugin@debug".to_string()],
        )
        .expect("cache refresh should be a no-op when configured plugins are current")
    );
}

#[test]
fn refresh_non_curated_plugin_cache_force_reinstalls_current_local_version() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin(&repo_root, "sample-plugin", "sample-plugin");
    fs::write(repo_root.join("sample-plugin/skills/SKILL.md"), "new skill").unwrap();
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    }
  ]
}"#,
    );
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/local",
        "sample-plugin",
    );
    fs::write(
        tmp.path()
            .join("plugins/cache/debug/sample-plugin/local/skills/SKILL.md"),
        "old skill",
    )
    .unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    assert!(
        refresh_non_curated_plugin_cache_force_reinstall(
            tmp.path(),
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            &["sample-plugin@debug".to_string()],
        )
        .expect("cache refresh should reinstall unchanged local version")
    );

    assert_eq!(
        fs::read_to_string(
            tmp.path()
                .join("plugins/cache/debug/sample-plugin/local/skills/SKILL.md")
        )
        .unwrap(),
        "new skill"
    );
}

#[test]
fn refresh_non_curated_plugin_cache_ignores_invalid_unconfigured_plugin_versions() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin_with_version(&repo_root, "sample-plugin", "sample-plugin", Some("1.2.3"));
    write_plugin_with_version(&repo_root, "broken-plugin", "broken-plugin", Some("   "));
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      }
    },
    {
      "name": "broken-plugin",
      "source": {
        "source": "local",
        "path": "./broken-plugin"
      }
    }
  ]
}"#,
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    assert!(
        refresh_non_curated_plugin_cache(
            tmp.path(),
            &[AbsolutePathBuf::try_from(repo_root).unwrap()],
            &["sample-plugin@debug".to_string()],
        )
        .expect("cache refresh should ignore unrelated invalid plugin manifests")
    );

    assert!(
        tmp.path()
            .join("plugins/cache/debug/sample-plugin/1.2.3")
            .is_dir()
    );
}

#[test]
fn refresh_non_curated_plugin_cache_continues_after_plugin_error() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin_with_version(&repo_root, "z-good", "z-good", Some("1.2.3"));
    write_file(
        &repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "a-broken",
      "source": {
        "source": "local",
        "path": "./missing"
      }
    },
    {
      "name": "z-good",
      "source": {
        "source": "local",
        "path": "./z-good"
      }
    }
  ]
}"#,
    );

    let err = refresh_non_curated_plugin_cache(
        tmp.path(),
        &[AbsolutePathBuf::try_from(repo_root).unwrap()],
        &["a-broken@debug".to_string(), "z-good@debug".to_string()],
    )
    .expect_err("broken plugin should be reported after refreshing the remaining plugins");

    assert!(err.contains("a-broken@debug"));
    assert!(tmp.path().join("plugins/cache/debug/z-good/1.2.3").is_dir());
}

#[tokio::test]
async fn marketplace_policy_projection_disables_installed_plugin_and_invalidates_cache() {
    let codex_home = TempDir::new().expect("create Codex home");
    write_plugin(
        &codex_home.path().join("plugins/cache/company"),
        "sample/local",
        "sample",
    );
    let user_config = r#"
[marketplaces.company]
source_type = "git"
source = "https://github.com/example/company.git"

[plugins."sample@company"]
enabled = true
"#;
    let allowed = plugins_config_input_with_requirements(
        codex_home.path(),
        user_config,
        r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.company]
source = "git"
url = "https://github.com/example/company.git"
"#,
    );
    let blocked = plugins_config_input_with_requirements(
        codex_home.path(),
        user_config,
        r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.other]
source = "git"
url = "https://github.com/example/other.git"
"#,
    );
    let manager = test_plugins_manager(codex_home.path().to_path_buf());

    let allowed_outcome = manager.plugins_for_config(&allowed).await;
    assert_eq!(allowed_outcome.plugins().len(), 1);
    assert_eq!(allowed_outcome.plugins()[0].config_name, "sample@company");

    let blocked_outcome = manager.plugins_for_config(&blocked).await;
    assert_eq!(blocked_outcome, PluginLoadOutcome::default());
}

#[tokio::test]
async fn plugin_read_rejects_marketplace_blocked_by_requirements() {
    let codex_home = TempDir::new().expect("create Codex home");
    let marketplace_root = codex_home.path().join("marketplace");
    write_plugin(&marketplace_root, "sample", "sample");
    write_file(
        &marketplace_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "company",
  "plugins": [
    {
      "name": "sample",
      "source": {"source": "local", "path": "./sample"}
    }
  ]
}"#,
    );
    let config = plugins_config_input_with_requirements(
        codex_home.path(),
        "",
        r#"
[marketplaces]
restrict_to_allowed_sources = true
"#,
    );
    let marketplace_path =
        AbsolutePathBuf::try_from(marketplace_root.join(".agents/plugins/marketplace.json"))
            .expect("absolute marketplace path");

    let err = test_plugins_manager(codex_home.path().to_path_buf())
        .read_plugin_for_config(
            &config,
            &PluginReadRequest {
                plugin_name: "sample".to_string(),
                marketplace_path,
            },
        )
        .await
        .expect_err("blocked marketplace should not be readable");
    assert!(matches!(
        err,
        MarketplaceError::InvalidMarketplaceFile { .. }
    ));
}

#[tokio::test]
async fn plugin_capability_summary_uses_manifest_mcp_server_objects() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/counter-sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "counter-sample",
  "version": "1.1.1",
  "mcpServers": {
    "counter": {
      "type": "http",
      "url": "https://sample.example/counter/mcp"
    }
  }
}"#,
    );

    let summary = plugin_capability_summary_without_cache(
        &PluginId::parse("counter-sample@test").expect("plugin id should parse"),
        &plugin_root.abs(),
        test_skill_root_loader(codex_home.path()).as_ref(),
    )
    .await;

    assert_eq!(
        summary,
        Some(PluginCapabilitySummary {
            config_name: "counter-sample@test".to_string(),
            display_name: "counter-sample".to_string(),
            plugin_namespace: Some("counter-sample".to_string()),
            description: None,
            has_skills: false,
            mcp_server_names: vec!["counter".to_string()],
            app_connector_ids: Vec::new(),
        })
    );
}

#[tokio::test]
async fn effective_apps_dedupes_connector_ids_across_plugins() {
    let codex_home = TempDir::new().unwrap();
    let plugin_a_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/plugin-a/local");
    let plugin_b_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/plugin-b/local");

    write_file(
        &plugin_a_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"plugin-a"}"#,
    );
    write_file(
        &plugin_a_root.join(".app.json"),
        r#"{
  "apps": {
    "example": {
      "id": "connector_example"
    }
  }
}"#,
    );
    write_file(
        &plugin_b_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"plugin-b"}"#,
    );
    write_file(
        &plugin_b_root.join(".app.json"),
        r#"{
  "apps": {
    "chat": {
      "id": "connector_example"
    },
    "gmail": {
      "id": "connector_gmail"
    }
  }
}"#,
    );

    let mut root = toml::map::Map::new();
    let mut features = toml::map::Map::new();
    features.insert("plugins".to_string(), Value::Boolean(true));
    features.insert("apps".to_string(), Value::Boolean(true));
    root.insert("features".to_string(), Value::Table(features));

    let mut plugins = toml::map::Map::new();

    let mut plugin_a = toml::map::Map::new();
    plugin_a.insert("enabled".to_string(), Value::Boolean(true));
    plugins.insert("plugin-a@test".to_string(), Value::Table(plugin_a));

    let mut plugin_b = toml::map::Map::new();
    plugin_b.insert("enabled".to_string(), Value::Boolean(true));
    plugins.insert("plugin-b@test".to_string(), Value::Table(plugin_b));

    root.insert("plugins".to_string(), Value::Table(plugins));
    let config_toml =
        toml::to_string(&Value::Table(root)).expect("plugin test config should serialize");

    let outcome = load_plugins_from_config(&config_toml, codex_home.path()).await;

    assert_eq!(
        outcome.effective_apps(),
        vec![
            AppConnectorId("connector_example".to_string()),
            AppConnectorId("connector_gmail".to_string()),
        ]
    );
}

#[tokio::test]
async fn effective_apps_preserves_app_config_order() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{
  "apps": {
    "slack": {
      "id": "connector_slack"
    },
    "github": {
      "id": "connector_github"
    },
    "slack-copy": {
      "id": "connector_slack"
    }
  }
}"#,
    );

    let outcome = load_plugins_from_config(
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
        codex_home.path(),
    )
    .await;

    assert_eq!(
        outcome.effective_apps(),
        vec![
            AppConnectorId("connector_slack".to_string()),
            AppConnectorId("connector_github".to_string()),
        ]
    );
}

#[test]
fn capability_index_filters_inactive_and_zero_capability_plugins() {
    let codex_home = TempDir::new().unwrap();
    let connector = |id: &str| AppConnectorId(id.to_string());
    let app = |name: &str, connector_id: &str| app_declaration(name, connector_id);
    let http_server = |url: &str| McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: url.to_string(),
            bearer_token_env_var: None,
            http_headers: None,
            env_http_headers: None,
            http_headers_helper: None,
        },
        environment_id: "local".to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    };
    let plugin = |config_name: &str, dir_name: &str, manifest_name: &str| LoadedPlugin {
        config_name: config_name.to_string(),
        manifest_name: Some(manifest_name.to_string()),
        plugin_namespace: Some(
            config_name
                .split_once('@')
                .map_or(config_name, |(name, _)| name)
                .to_string(),
        ),
        manifest_description: None,
        root: AbsolutePathBuf::try_from(codex_home.path().join(dir_name)).unwrap(),
        enabled: true,
        skill_roots: Vec::new(),
        skill_discovery_mode: SkillDiscoveryMode::Recursive,
        disabled_skill_paths: HashSet::new(),
        has_enabled_skills: false,
        mcp_servers: HashMap::new(),
        apps: Vec::new(),
        hook_sources: Vec::new(),
        hook_load_warnings: Vec::new(),
        error: None,
    };
    let summary = |config_name: &str, display_name: &str| PluginCapabilitySummary {
        config_name: config_name.to_string(),
        display_name: display_name.to_string(),
        plugin_namespace: Some(
            config_name
                .split_once('@')
                .map_or(config_name, |(name, _)| name)
                .to_string(),
        ),
        description: None,
        ..PluginCapabilitySummary::default()
    };
    let outcome = PluginLoadOutcome::from_plugins(vec![
        LoadedPlugin {
            skill_roots: vec![codex_home.path().join("skills-plugin/skills").abs()],
            has_enabled_skills: true,
            ..plugin("skills@test", "skills-plugin", "skills-plugin")
        },
        LoadedPlugin {
            mcp_servers: HashMap::from([("alpha".to_string(), http_server("https://alpha"))]),
            apps: vec![app("example", "connector_example")],
            ..plugin("alpha@test", "alpha-plugin", "alpha-plugin")
        },
        LoadedPlugin {
            mcp_servers: HashMap::from([("beta".to_string(), http_server("https://beta"))]),
            apps: vec![
                app("example", "connector_example"),
                app("gmail", "connector_gmail"),
            ],
            ..plugin("beta@test", "beta-plugin", "beta-plugin")
        },
        plugin("empty@test", "empty-plugin", "empty-plugin"),
        LoadedPlugin {
            enabled: false,
            skill_roots: vec![codex_home.path().join("disabled-plugin/skills").abs()],
            apps: vec![app("hidden", "connector_hidden")],
            ..plugin("disabled@test", "disabled-plugin", "disabled-plugin")
        },
        LoadedPlugin {
            apps: vec![app("broken", "connector_broken")],
            error: Some("failed to load".to_string()),
            ..plugin("broken@test", "broken-plugin", "broken-plugin")
        },
    ]);

    assert_eq!(
        outcome.capability_summaries(),
        &[
            PluginCapabilitySummary {
                has_skills: true,
                ..summary("skills@test", "skills-plugin")
            },
            PluginCapabilitySummary {
                mcp_server_names: vec!["alpha".to_string()],
                app_connector_ids: vec![connector("connector_example")],
                ..summary("alpha@test", "alpha-plugin")
            },
            PluginCapabilitySummary {
                mcp_server_names: vec!["beta".to_string()],
                app_connector_ids: vec![
                    connector("connector_example"),
                    connector("connector_gmail"),
                ],
                ..summary("beta@test", "beta-plugin")
            },
        ]
    );
}

#[tokio::test]
async fn plugin_cache_reuses_effective_configurations() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");
    write_plugin(
        codex_home.path().join("plugins/cache/test").as_path(),
        "sample/local",
        "sample",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "url": "https://sample.example/mcp"
    }
  }
}"#,
    );

    let user_file = codex_home.path().join(CONFIG_TOML_FILE).abs();
    let user_config: toml::Value = toml::from_str(&plugin_config_toml(
        /*enabled*/ true, /*plugins_feature_enabled*/ true,
    ))
    .expect("user config should parse");
    let stack = |session_config: &str| {
        ConfigLayerStack::new(
            vec![
                ConfigLayerEntry::new(
                    ConfigLayerSource::User {
                        file: user_file.clone(),
                        profile: None,
                    },
                    user_config.clone(),
                ),
                ConfigLayerEntry::new(
                    ConfigLayerSource::SessionFlags,
                    toml::from_str(session_config).expect("session config should parse"),
                ),
            ],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .expect("config layer stack should build")
    };
    let config = |session_config| {
        PluginsConfigInput::new(
            stack(session_config),
            stack(session_config)
                .effective_config()
                .try_into::<ra_config::config_toml::ConfigToml>()
                .unwrap()
                .plugins,
            String::new(),
            /*plugins_enabled*/ true,
            test_http_client_factory(),
        )
    };
    let manager = test_plugins_manager(codex_home.path().to_path_buf());

    let first_config = config(r#"model = "first""#);
    let second_config = config(
        r#"[plugins."sample@test".mcp_servers.sample]
enabled = false"#,
    );
    let first = manager.plugins_for_config(&first_config).await;
    let first_snapshots = manager
        .plugin_skill_snapshots_for_config(&first_config)
        .expect("first configuration snapshots");
    let second = manager.plugins_for_config(&second_config).await;
    let second_snapshots = manager
        .plugin_skill_snapshots_for_config(&second_config)
        .expect("second configuration snapshots");
    std::fs::remove_file(plugin_root.join(".mcp.json")).unwrap();

    assert_eq!(
        manager.plugin_skill_snapshots_for_config(&first_config),
        Some(first_snapshots),
    );
    assert_eq!(
        manager
            .plugins_for_config(&config(r#"model = "second""#))
            .await,
        first,
    );
    assert_eq!(
        manager.plugin_skill_snapshots_for_config(&second_config),
        Some(second_snapshots),
    );
    assert_eq!(manager.plugins_for_config(&second_config).await, second);
    manager.clear_cache();
    assert_eq!(
        [first_config, second_config]
            .iter()
            .map(|config| manager.plugin_skill_snapshots_for_config(config))
            .collect::<Vec<_>>(),
        vec![None, None],
    );
}

#[test]
fn curated_plugin_ids_from_config_keys_reads_latest_codex_home_user_config() {
    let tmp = tempfile::tempdir().unwrap();
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."slack@openai-curated"]
enabled = true

[plugins."api-only@openai-api-curated"]
enabled = true

[plugins."sample@debug"]
enabled = true
"#,
    );

    assert_eq!(
        configured_curated_plugin_ids_from_codex_home(tmp.path())
            .into_iter()
            .map(|plugin_id| plugin_id.as_key())
            .collect::<Vec<_>>(),
        vec![
            "api-only@openai-api-curated".to_string(),
            "slack@openai-curated".to_string(),
        ]
    );

    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    assert_eq!(
        configured_curated_plugin_ids_from_codex_home(tmp.path()),
        Vec::<PluginId>::new()
    );
}

#[tokio::test]
async fn plugin_hooks_for_layer_stack_loads_configured_plugin_hooks() {
    let codex_home = TempDir::new().unwrap();
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");
    write_plugin(
        codex_home.path().join("plugins/cache/test").as_path(),
        "sample/local",
        "sample",
    );
    write_file(
        &plugin_root.join("hooks/hooks.json"),
        r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo startup"
          }
        ]
      }
    ]
  }
}"#,
    );
    write_file(
        &codex_home.path().join(CONFIG_TOML_FILE),
        &plugin_config_toml(/*enabled*/ true, /*plugins_feature_enabled*/ true),
    );
    let config = load_config(codex_home.path(), codex_home.path()).await;

    let outcome = test_plugins_manager(codex_home.path().to_path_buf())
        .plugin_hooks_for_layer_stack(&config.config_layer_stack, &config)
        .await;

    assert_eq!(outcome.hook_sources.len(), 1);
    assert_eq!(
        outcome.hook_sources[0].source_relative_path,
        "hooks/hooks.json"
    );
    assert_eq!(outcome.hook_load_warnings, Vec::<String>::new());
}
