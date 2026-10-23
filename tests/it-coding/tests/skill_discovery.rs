//! Host discovery, context filtering, filesystem boundaries, and production run refresh.
#[path = "../../support/configured_coding_host.rs"]
#[allow(dead_code)]
mod configured_host;

use async_trait::async_trait;
use ra_coding::{
    CodingHost, CodingProfile,
    skills::{PluginSkillRoot, SkillDiscoveryConfig, SkillDiscoveryMode},
};
use ra_core::{
    agent::AgentId,
    cancel::CancelScope,
    error::Result,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    prompt::PromptRole,
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

fn write(root: &Path, relative: &str, text: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}
fn skill(root: &Path, relative: &str, name: &str) -> PathBuf {
    write(
        root,
        &format!("{relative}/SKILL.md"),
        &format!("---\nname: {name}\ndescription: Help with {name}.\n---\nPRIVATE BODY {name}"),
    )
}
fn names(catalog: &ra_tools::skills_ns::catalog::SkillCatalog) -> Vec<&str> {
    catalog
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect()
}

#[tokio::test]
async fn discovers_project_and_explicit_plugins_and_filters_exact_qualified_names() {
    let temp = tempfile::tempdir().unwrap();
    skill(temp.path(), ".rusty-agent/skills/release", "release");
    skill(temp.path(), ".rusty-agent/skills/.hidden", "hidden");
    skill(temp.path(), "unselected/skills/other", "other");
    skill(temp.path(), "plugin/skills/release", "release");
    let config = SkillDiscoveryConfig::new().with_plugin_root(
        PluginSkillRoot::new(
            temp.path().join("plugin/skills"),
            temp.path().join("plugin"),
            "acme",
        )
        .unwrap(),
    );
    let catalog = config.discover(temp.path()).await.unwrap();
    assert_eq!(names(&catalog), ["release", "acme:release"]);
    assert!(catalog.warnings.is_empty());
    assert!(
        catalog
            .entries
            .iter()
            .all(|entry| !entry.description.contains("PRIVATE BODY"))
    );
    let filtered = config
        .clone()
        .with_allowlist(["release"])
        .discover(temp.path())
        .await
        .unwrap();
    assert_eq!(names(&filtered), ["release"]);
    assert_eq!(
        names(
            &config
                .clone()
                .with_allowlist(["acme:release"])
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["acme:release"]
    );
    for allowlist in [vec![], vec!["Release"], vec!["acme:*"], vec!["missing"]] {
        assert!(
            config
                .clone()
                .with_allowlist(allowlist)
                .discover(temp.path())
                .await
                .unwrap()
                .entries
                .is_empty()
        );
    }
    assert_eq!(
        names(
            &config
                .without_project_skills()
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["acme:release"]
    );
}

#[tokio::test]
async fn malformed_skills_are_warnings_missing_roots_are_empty_and_good_siblings_survive() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        SkillDiscoveryConfig::new()
            .discover(temp.path())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    write(
        temp.path(),
        ".rusty-agent/skills/fallback/SKILL.md",
        "---\ndescription: Build for AWS: ECS\nmetadata:\n  short-description: Brief\n---\nBODY",
    );
    write(
        temp.path(),
        ".rusty-agent/skills/no-description/SKILL.md",
        "---\nname: nope\n---\n",
    );
    write(
        temp.path(),
        ".rusty-agent/skills/no-fence/SKILL.md",
        "Plain markdown",
    );
    write(
        temp.path(),
        ".rusty-agent/skills/bad-yaml/SKILL.md",
        "---\nname: [unterminated\n---\n",
    );
    let binary = skill(temp.path(), ".rusty-agent/skills/binary", "binary");
    fs::write(binary, [0xff]).unwrap();
    let catalog = SkillDiscoveryConfig::new()
        .discover(temp.path())
        .await
        .unwrap();
    assert_eq!(names(&catalog), ["fallback"]);
    assert_eq!(catalog.entries[0].description, "Build for AWS: ECS");
    assert_eq!(
        catalog.entries[0].short_description.as_deref(),
        Some("Brief")
    );
    assert_eq!(catalog.warnings.len(), 4);
    assert!(
        catalog
            .warnings
            .iter()
            .any(|warning| warning.contains("missing field `description`"))
    );
}

#[tokio::test]
async fn recursive_and_direct_child_discovery_preserve_upstream_limits_and_owner_namespace() {
    let temp = tempfile::tempdir().unwrap();
    skill(temp.path(), "plugin/skills/first", "first");
    skill(temp.path(), "plugin/skills/group/second", "second");
    write(
        temp.path(),
        "plugin/skills/group/.codex-plugin/plugin.json",
        r#"{"name":"nested"}"#,
    );
    let root = PluginSkillRoot::new(
        temp.path().join("plugin/skills"),
        temp.path().join("plugin"),
        "owner",
    )
    .unwrap();
    let recursive = SkillDiscoveryConfig::new()
        .without_project_skills()
        .with_plugin_root(root.clone());
    assert_eq!(
        names(&recursive.discover(temp.path()).await.unwrap()),
        ["owner:first", "owner:second"]
    );
    let direct = SkillDiscoveryConfig::new()
        .without_project_skills()
        .with_plugin_root(root.with_discovery_mode(SkillDiscoveryMode::DirectChildren));
    assert_eq!(
        names(&direct.discover(temp.path()).await.unwrap()),
        ["owner:first"]
    );
    skill(
        temp.path(),
        ".rusty-agent/skills/a/b/c/d/e/f",
        "at-depth-six",
    );
    skill(
        temp.path(),
        ".rusty-agent/skills/a/b/c/d/e/f/g",
        "past-depth-six",
    );
    assert_eq!(
        names(
            &SkillDiscoveryConfig::new()
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["at-depth-six"]
    );
}

#[tokio::test]
async fn nested_manifest_names_use_nearest_valid_namespace_and_first_manifest_precedence() {
    let temp = tempfile::tempdir().unwrap();
    skill(temp.path(), ".rusty-agent/skills/package/first", "first");
    skill(
        temp.path(),
        ".rusty-agent/skills/package/nested/second",
        "second",
    );
    write(
        temp.path(),
        ".rusty-agent/skills/package/.codex-plugin/plugin.json",
        r#"{"name":"parent"}"#,
    );
    write(
        temp.path(),
        ".rusty-agent/skills/package/.claude-plugin/plugin.json",
        r#"{"name":"alternate"}"#,
    );
    write(
        temp.path(),
        ".rusty-agent/skills/package/nested/.codex-plugin/plugin.json",
        r#"{"name":"nearest"}"#,
    );
    assert_eq!(
        names(
            &SkillDiscoveryConfig::new()
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["nearest:second", "parent:first"]
    );
    write(
        temp.path(),
        ".rusty-agent/skills/package/nested/.codex-plugin/plugin.json",
        "invalid JSON",
    );
    assert_eq!(
        names(
            &SkillDiscoveryConfig::new()
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["parent:first", "parent:second"]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn follows_directory_aliases_handles_cycles_and_deduplicates_canonical_paths_before_filtering()
 {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let path = skill(temp.path(), ".rusty-agent/skills/original", "review");
    symlink(
        temp.path().join(".rusty-agent/skills"),
        temp.path().join(".rusty-agent/skills/cycle"),
    )
    .unwrap();
    symlink(
        path.parent().unwrap(),
        temp.path().join(".rusty-agent/skills/alias"),
    )
    .unwrap();
    let config = SkillDiscoveryConfig::new().with_plugin_root(
        PluginSkillRoot::new(
            temp.path().join(".rusty-agent/skills"),
            temp.path().to_path_buf(),
            "duplicate",
        )
        .unwrap(),
    );
    let catalog = config.clone().discover(temp.path()).await.unwrap();
    assert_eq!(names(&catalog), ["review"]);
    assert_eq!(
        catalog.entries[0].main_prompt.as_str(),
        fs::canonicalize(&path).unwrap().to_str().unwrap()
    );
    assert!(
        config
            .with_allowlist(["duplicate:review"])
            .discover(temp.path())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let outside = tempfile::tempdir().unwrap();
    skill(outside.path(), "outside", "external");
    fs::create_dir_all(temp.path().join("plugin/skills")).unwrap();
    symlink(
        outside.path().join("outside"),
        temp.path().join("plugin/skills/external"),
    )
    .unwrap();
    let root = PluginSkillRoot::new(
        temp.path().join("plugin/skills"),
        temp.path().join("plugin"),
        "owner",
    )
    .unwrap();
    assert_eq!(
        names(
            &SkillDiscoveryConfig::new()
                .without_project_skills()
                .with_plugin_root(root.clone())
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["owner:external"]
    );
    let direct = SkillDiscoveryConfig::new()
        .without_project_skills()
        .with_plugin_root(root.with_discovery_mode(SkillDiscoveryMode::DirectChildren))
        .discover(temp.path())
        .await
        .unwrap();
    assert!(direct.entries.is_empty());
    assert_eq!(direct.warnings.len(), 1);
}

struct ReadModel {
    path: PathBuf,
    expected_name: String,
    edit_during_run: PathBuf,
    expected_output: String,
    step: Mutex<usize>,
}
#[async_trait]
impl Model for ReadModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let mut step = self.step.lock().unwrap();
        let prompt = request.system_instructions().unwrap();
        assert!(
            prompt.contains(&format!("- {}:", self.expected_name)),
            "{prompt}"
        );
        assert!(!prompt.contains("PRIVATE BODY"));
        assert!(!prompt.contains("- excluded:"));
        assert!(
            !request
                .tools()
                .iter()
                .any(|tool| tool.qualified_name().starts_with("skills.") || tool.name() == "skill")
        );
        let item = if *step == 0 {
            fs::write(
                &self.edit_during_run,
                "---\nname: must-refresh-next-run\ndescription: Changed during run\n---\n",
            )
            .unwrap();
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("read"),
                "read_file",
                json!({"path":self.path,"offset":null,"limit":null}),
            ))
        } else {
            let result = request
                .input()
                .iter()
                .find_map(|item| match item {
                    ModelInputItem::ToolCallOutput(output) => Some(output.output()),
                    _ => None,
                })
                .unwrap();
            assert!(
                result.to_string().contains(&self.expected_output),
                "{result}"
            );
            RunItemKind::Message(Message::assistant("complete", OutputPhase::Final))
        };
        *step += 1;
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("item-{step}")),
            item,
        )]))
    }
}
struct Resolver(Arc<dyn Model>);
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("catalog".into()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::default(),
            ModelSettings::default(),
        ))
    }
}

#[tokio::test]
async fn production_runs_refresh_host_catalogs_and_allowlists_do_not_block_ordinary_reads() {
    let temp = tempfile::tempdir().unwrap();
    let visible = skill(temp.path(), ".rusty-agent/skills/visible", "visible");
    let excluded = fs::canonicalize(skill(
        temp.path(),
        ".rusty-agent/skills/excluded",
        "excluded",
    ))
    .unwrap();
    let host = CodingHost::open(temp.path())
        .unwrap()
        .with_skill_discovery(SkillDiscoveryConfig::new().with_allowlist(["visible", "updated"]));
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
    for name in ["visible", "updated"] {
        if name == "updated" {
            fs::write(
                &visible,
                "---\nname: updated\ndescription: Changed on disk\n---\n",
            )
            .unwrap();
        }
        let model = Arc::new(ReadModel {
            path: excluded.clone(),
            expected_name: name.into(),
            edit_during_run: visible.clone(),
            expected_output: "PRIVATE BODY excluded".into(),
            step: Mutex::new(0),
        });
        let request = RunRequest::new(
            AgentBinding::direct(Arc::clone(built.agent())),
            Arc::new(Resolver(model.clone())),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(
                "Use the listed skill",
            ))],
        )
        .with_config(host.build_run_config())
        .with_services(host.build_tool_services());
        Runner::run(built.bind(request)).await.unwrap();
        assert_eq!(*model.step.lock().unwrap(), 2);
    }
}

#[tokio::test]
async fn host_catalog_selection_preserves_roles_tiers_and_explicit_compatibility_exclusion() {
    let temp = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    skill(temp.path(), ".rusty-agent/skills/visible", "visible");
    let host = CodingHost::open(temp.path())
        .unwrap()
        .with_web_access(services.clone())
        .with_user_input(services.clone())
        .with_collaboration()
        .with_skill_discovery(SkillDiscoveryConfig::new());
    for tier in CodingProfile::SHIPPED {
        for role in [
            PromptRole::Main,
            PromptRole::ReadOnlySpecialist,
            PromptRole::Planner,
            PromptRole::OneOffAnswer,
            PromptRole::Coordinator,
        ] {
            let built = ra_coding::build_agent_with_control(
                AgentId::new("coding"),
                "Coding",
                &role,
                &host,
                tier,
            )
            .await
            .unwrap();
            let config = built.configure_run(host.build_run_config());
            let capability = config.capabilities().iter().find(|capability| {
                capability.kind() == ra_core::capability::CapabilityFamily::SKILLS
            });
            assert_eq!(
                capability.is_some(),
                !role.is_one_off(),
                "{role:?} {tier:?}"
            );
            assert!(
                !built.agent().tools().iter().any(|tool| tool
                    .origin()
                    .qualified_name()
                    .starts_with("skills.")
                    || tool.origin().qualified_name() == "skill")
            );
        }
    }
    let error = ra_coding::host_backed_surface(
        &PromptRole::Main,
        &host.with_skill_catalog(services),
        CodingProfile::Core,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code(), "config");
    let default_host = CodingHost::open(temp.path()).unwrap();
    let surface =
        ra_coding::host_backed_surface(&PromptRole::Main, &default_host, CodingProfile::Core)
            .await
            .unwrap();
    assert!(!surface.installed().iter().any(|capability| capability.family() == &ra_core::capability::CapabilityFamily::SKILLS));
}

#[tokio::test]
async fn optional_metadata_policy_preserves_visibility_product_restrictions_and_fail_open() {
    use ra_coding::skills::SkillProduct;
    let temp = tempfile::tempdir().unwrap();
    for name in ["implicit", "manual", "codex", "invalid"] {
        skill(temp.path(), &format!(".rusty-agent/skills/{name}"), name);
    }
    write(
        temp.path(),
        ".rusty-agent/skills/manual/agents/openai.yaml",
        "policy:\n  allow_implicit_invocation: false\n",
    );
    write(
        temp.path(),
        ".rusty-agent/skills/codex/agents/openai.yaml",
        "policy:\n  products: [CODEX]\n",
    );
    write(
        temp.path(),
        ".rusty-agent/skills/invalid/agents/openai.yaml",
        "policy:\n  allow_implicit_invocation: false\ninterface:\n  display_name: []\n",
    );
    let config = SkillDiscoveryConfig::new();
    let catalog = config.clone().discover(temp.path()).await.unwrap();
    assert_eq!(names(&catalog), ["implicit", "invalid", "manual"]);
    assert!(
        !catalog
            .entries
            .iter()
            .find(|entry| entry.name == "manual")
            .unwrap()
            .prompt_visible
    );
    assert!(
        catalog
            .entries
            .iter()
            .find(|entry| entry.name == "invalid")
            .unwrap()
            .prompt_visible
    );
    assert_eq!(
        names(
            &config
                .clone()
                .with_restriction_product(SkillProduct::Codex)
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["codex", "implicit", "invalid", "manual"]
    );
    assert_eq!(
        names(
            &config
                .with_restriction_product(SkillProduct::Atlas)
                .discover(temp.path())
                .await
                .unwrap()
        ),
        ["implicit", "invalid", "manual"]
    );
}

#[tokio::test]
async fn maximum_qualified_names_and_traversal_warning_match_upstream_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    skill(temp.path(), "plugin/skills/valid", &"x".repeat(64));
    skill(temp.path(), "plugin/skills/invalid", &"x".repeat(65));
    let root = PluginSkillRoot::new(
        temp.path().join("plugin/skills"),
        temp.path().join("plugin"),
        "p".repeat(64),
    )
    .unwrap();
    let catalog = SkillDiscoveryConfig::new()
        .without_project_skills()
        .with_plugin_root(root)
        .discover(temp.path())
        .await
        .unwrap();
    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(catalog.entries[0].name.chars().count(), 129);
    assert_eq!(catalog.warnings.len(), 1);
    for index in 0..2001 {
        fs::create_dir_all(
            temp.path()
                .join(format!(".rusty-agent/skills/d-{index:04}")),
        )
        .unwrap();
    }
    let catalog = SkillDiscoveryConfig::new()
        .discover(temp.path())
        .await
        .unwrap();
    assert_eq!(catalog.warnings.len(), 1);
    assert!(catalog.warnings[0].contains("traversal limit"));
}

#[tokio::test]
async fn core_retains_host_catalogs_when_namespace_providers_are_also_configured() {
    use ra_core::capability::CapabilityFamily;
    use ra_exec::fs::Workspace;
    use ra_tools::skills_ns::{ExecutorSkillProvider, SkillProviders, catalog::SkillCatalog};
    let temp = tempfile::tempdir().unwrap();
    skill(temp.path(), ".rusty-agent/skills/host", "host-skill");
    let host = CodingHost::open(temp.path())
        .unwrap()
        .with_skill_discovery(SkillDiscoveryConfig::new())
        .with_skill_providers(
            SkillProviders::new().with_executor_provider(Arc::new(ExecutorSkillProvider::new(
                SkillCatalog::default(),
            ))),
            [("root".into(), Workspace::open(temp.path()).unwrap())].into(),
        );
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
    let config = built.configure_run(host.build_run_config());
    let capability = config
        .capabilities()
        .iter()
        .find(|capability| capability.kind() == CapabilityFamily::SKILLS)
        .unwrap();
    assert!(capability.tools().is_empty());
    let run = ra_core::context::RunContext::new(RunId::generate(), built.agent());
    assert!(
        capability
            .bind(&run)
            .unwrap()
            .unwrap()
            .instructions()
            .await
            .unwrap()
            .unwrap()
            .content()
            .contains("- host-skill:")
    );
}

#[tokio::test]
async fn discovering_an_external_plugin_does_not_grant_the_workspace_reader_access() {
    let workspace = tempfile::tempdir().unwrap();
    let plugin = tempfile::tempdir().unwrap();
    let external = fs::canonicalize(skill(plugin.path(), "skills/review", "review")).unwrap();
    let scratch = workspace.path().join("changed-during-run.md");
    let discovery = SkillDiscoveryConfig::new()
        .without_project_skills()
        .with_plugin_root(
            PluginSkillRoot::new(
                plugin.path().join("skills"),
                plugin.path().to_path_buf(),
                "acme",
            )
            .unwrap(),
        );
    let host = CodingHost::open(workspace.path())
        .unwrap()
        .with_skill_discovery(discovery);
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
    let model = Arc::new(ReadModel {
        path: external,
        expected_name: "acme:review".into(),
        edit_during_run: scratch,
        expected_output: "outside the workspace".into(),
        step: Mutex::new(0),
    });
    let request = RunRequest::new(
        AgentBinding::direct(Arc::clone(built.agent())),
        Arc::new(Resolver(model.clone())),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(
            "Read the listed plugin",
        ))],
    )
    .with_config(host.build_run_config())
    .with_services(host.build_tool_services());
    Runner::run(built.bind(request)).await.unwrap();
    assert_eq!(*model.step.lock().unwrap(), 2);
}
