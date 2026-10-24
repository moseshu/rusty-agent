//! Explicit skill source selection in the coding host and its role-aware profiles.
#[path = "../../support/configured_coding_host.rs"]
mod configured_host;

use ra_coding::{CodingHost, CodingProfile, host_backed_surface};
use ra_core::{capability::Capability, prompt::PromptRole};
use ra_exec::fs::Workspace;
use ra_tools::{
    capability::{SkillsCapability, SkillsNsCapability},
    skills_ns::SkillProviders,
};
use std::{collections::BTreeMap, sync::Arc};

#[tokio::test]
async fn codex_like_selects_the_configured_interface_for_every_role_and_tier() {
    let directory = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(directory.path())
        .unwrap()
        .with_web_access(services.clone())
        .with_user_input(services.clone())
        .with_collaboration()
        .with_skill_providers(executor_source(), executor_roots(directory.path()));
    for tier in CodingProfile::SHIPPED {
        for role in [
            PromptRole::Main,
            PromptRole::ReadOnlySpecialist,
            PromptRole::Planner,
            PromptRole::OneOffAnswer,
            PromptRole::Coordinator,
        ] {
            let surface = host_backed_surface(&role, &host, tier).await.unwrap();
            let names = surface
                .tool_surface()
                .advertised_names()
                .collect::<Vec<_>>();
            if tier == CodingProfile::Core || role.is_one_off() {
                assert!(!names.contains(&"skills.list"));
                assert!(!names.contains(&"skills.read"));
            } else {
                assert!(names.contains(&"skills.list"), "{tier:?} {role:?}");
                assert!(names.contains(&"skills.read"), "{tier:?} {role:?}");
            }
            assert!(!names.contains(&"skill"));
            if tier != CodingProfile::Core && role == PromptRole::Main {
                assert_eq!(names.len(), 19);
            }
        }
    }
    let legacy = services.install(CodingHost::open(directory.path()).unwrap());
    let surface = host_backed_surface(&PromptRole::Main, &legacy, CodingProfile::CodexLike)
        .await
        .unwrap();
    assert!(
        surface
            .tool_surface()
            .advertised_names()
            .any(|name| name == "skill")
    );
    assert!(
        !surface
            .tool_surface()
            .advertised_names()
            .any(|name| name.starts_with("skills."))
    );
}

#[tokio::test]
async fn host_assembly_rejects_both_skill_sources_in_either_configuration_order() {
    let directory = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    for host in [
        CodingHost::open(directory.path())
            .unwrap()
            .with_skill_catalog(services.clone())
            .with_skill_providers(executor_source(), executor_roots(directory.path())),
        CodingHost::open(directory.path())
            .unwrap()
            .with_skill_providers(executor_source(), executor_roots(directory.path()))
            .with_skill_catalog(services.clone()),
    ] {
        for role in [PromptRole::Main, PromptRole::OneOffAnswer] {
            let error = host_backed_surface(&role, &host, CodingProfile::Core)
                .await
                .err()
                .unwrap();
            assert_eq!(error.code(), "config");
            assert!(error.to_string().contains(
                "configure either the merged skill catalog or skills namespace providers"
            ));
        }
    }
    let legacy: Arc<dyn Capability> = Arc::new(SkillsCapability::new(services).unwrap());
    let namespace: Arc<dyn Capability> = Arc::new(
        SkillsNsCapability::new(executor_source(), executor_roots(directory.path()), 1000).unwrap(),
    );
    assert_eq!(legacy.kind(), namespace.kind());
    assert!(ra_runtime::capability::CapabilityPlan::resolve(vec![legacy, namespace]).is_err());
}

use async_trait::async_trait;
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
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use ra_tools::skills_ns::{ExecutorSkillProvider, catalog::*, provider::LOCAL_ENVIRONMENT_ID};
use serde_json::{Value, json};

struct PagingModel {
    package: String,
    resource: String,
    original: String,
    file: std::path::PathBuf,
    observed: std::sync::Mutex<String>,
    step: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl Model for PagingModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        use std::sync::atomic::Ordering;
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        assert!(
            request
                .tools()
                .iter()
                .any(|t| t.qualified_name() == "skills.read")
        );
        assert!(
            !request
                .system_instructions()
                .unwrap()
                .contains("executor reference")
        );
        let mut args = json!({"package":self.package,"resource":self.resource});
        if step > 0 {
            let output = request
                .input()
                .iter()
                .rev()
                .find_map(|item| match item {
                    ModelInputItem::ToolCallOutput(output) => Some(output.output()),
                    _ => None,
                })
                .unwrap();
            let output: ra_core::tool::ToolOutput = serde_json::from_value(output.clone()).unwrap();
            assert!(output.model_excerpt().is_none());
            assert!(output.metadata().truncations().is_empty());
            let page: Value = serde_json::from_str(output.as_text().unwrap()).unwrap();
            self.observed
                .lock()
                .unwrap()
                .push_str(page["contents"].as_str().unwrap());
            if page["next_cursor"].is_null() {
                assert_eq!(*self.observed.lock().unwrap(), self.original);
                return Ok(ModelResponse::new(vec![RunItem::new(
                    ItemId::new(format!("response-{step}")),
                    RunItemKind::Message(Message::assistant("read complete", OutputPhase::Final)),
                )]));
            }
            args["cursor"] = page["next_cursor"].clone();
            // The production host keeps one pair through the run: editing the disk after page one
            // must not replace cached contents on any subsequent model step.
            if step == 1 {
                std::fs::write(&self.file, "changed after first page").unwrap();
            }
        }
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("response-{step}")),
            RunItemKind::ToolCall(
                ToolCall::new(CallId::new(format!("page-{step}")), "read", args)
                    .with_namespace("skills"),
            ),
        )]))
    }
}
struct PagingResolver(Arc<PagingModel>);
impl ModelResolver for PagingResolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("paging".into()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

#[tokio::test]
async fn runtime_keeps_paginated_skill_results_in_the_tail_without_context_truncation() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("skill")).unwrap();
    let original = "executor reference \"\\\n技".repeat(6000);
    let file = directory.path().join("skill/reference.md");
    std::fs::write(&file, &original).unwrap();
    let host = CodingHost::open(directory.path()).unwrap();
    let package = "skill://root/skill".to_string();
    let resource = format!("{package}/reference.md");
    let catalog = SkillCatalog {
        entries: vec![SkillCatalogEntry::new(
            SkillPackageId(package.clone()),
            SkillAuthority::new(SkillSourceKind::Executor, "root"),
            "Reference",
            "Read a reference",
            SkillResourceId::environment(
                format!("{package}/SKILL.md"),
                LOCAL_ENVIRONMENT_ID,
                host.workspace_root().join("skill/SKILL.md"),
            ),
        )],
        warnings: vec![],
    };
    let services = Arc::new(configured_host::Services::default());
    let skill_roots = [(
        "root".to_string(),
        Workspace::open(host.workspace_root()).unwrap(),
    )]
    .into();
    let host = host
        .with_web_access(services.clone())
        .with_user_input(services)
        .with_collaboration()
        .with_skill_providers(
            SkillProviders::new()
                .with_executor_provider(Arc::new(ExecutorSkillProvider::new(catalog))),
            skill_roots,
        );
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    let model = Arc::new(PagingModel {
        package,
        resource,
        original,
        file,
        observed: Default::default(),
        step: Default::default(),
    });
    let request = RunRequest::new(
        AgentBinding::direct(built.agent().clone()),
        Arc::new(PagingResolver(model.clone())),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(
            "Read all reference pages",
        ))],
    )
    .with_config(host.build_run_config())
    .with_services(host.build_tool_services());
    Runner::run(built.bind(request)).await.unwrap();
    assert!(model.step.load(std::sync::atomic::Ordering::SeqCst) > 2);
    assert_eq!(*model.observed.lock().unwrap(), model.original);
}

#[cfg(unix)]
#[test]
fn non_utf8_cwd_home_and_tmpdir_remain_valid_permission_values() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let native_cwd = root.join(OsString::from_vec(b"cwd-\xff".to_vec()));
    let home = root.join(OsString::from_vec(b"home-\xfe".to_vec()));
    let temporary = root.join(OsString::from_vec(b"tmp-\xfd".to_vec()));
    // Permission contexts can name absent directories. APFS rejects invalid UTF-8 filenames,
    // so keep the workspace native and pass non-UTF-8 context values without creating them.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_permission_context_child",
            "--ignored",
            "--nocapture",
        ])
        .env("RA_SKILL_NATIVE_ROOT", root)
        .env("RA_SKILL_NATIVE_CWD", &native_cwd)
        .env("HOME", &home)
        .env("TMPDIR", &temporary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed; 0 failed;")
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "Subprocess entry for native permission paths; executed by its parent test"]
async fn native_permission_context_child() {
    use ra_coding::{
        permissions::FileSystemPermissions, thread_settings::CodingThreadSettingsAdapter,
    };
    use ra_protocol::server::protocol::SandboxPolicy;
    let root = std::path::PathBuf::from(std::env::var_os("RA_SKILL_NATIVE_ROOT").unwrap());
    std::fs::create_dir(root.join("skill")).unwrap();
    let original = "executor reference\n".repeat(8000);
    let file = root.join("skill/reference.md");
    let host = CodingHost::open(&root).unwrap();
    let package = "skill://root/skill".to_string();
    let resource = format!("{package}/reference.md");
    let catalog = SkillCatalog {
        entries: vec![SkillCatalogEntry::new(
            SkillPackageId(package.clone()),
            SkillAuthority::new(SkillSourceKind::Executor, "root"),
            "Reference",
            "Read a reference",
            SkillResourceId::environment(
                format!("{package}/SKILL.md"),
                LOCAL_ENVIRONMENT_ID,
                host.workspace_root().join("skill/SKILL.md"),
            ),
        )],
        warnings: vec![],
    };
    let services = Arc::new(configured_host::Services::default());
    let roots = [("root".to_string(), Workspace::open(&root).unwrap())].into();
    let host = host
        .with_web_access(services.clone())
        .with_user_input(services)
        .with_collaboration()
        .with_skill_providers(
            SkillProviders::new()
                .with_executor_provider(Arc::new(ExecutorSkillProvider::new(catalog))),
            roots,
        );
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    for restricted in [false, true] {
        std::fs::write(&file, &original).unwrap();
        let model = Arc::new(PagingModel {
            package: package.clone(),
            resource: resource.clone(),
            original: original.clone(),
            file: file.clone(),
            observed: Default::default(),
            step: Default::default(),
        });
        let native_cwd = std::path::PathBuf::from(std::env::var_os("RA_SKILL_NATIVE_CWD").unwrap());
        let adapter = CodingThreadSettingsAdapter::new(
            native_cwd,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap();
        let adapter = if restricted {
            adapter
                .with_file_system_read_policy(FileSystemPermissions::from_read_write_roots(
                    Some(vec![host.workspace_root().to_path_buf()]),
                    None,
                ))
                .unwrap()
        } else {
            adapter
        };
        let request = RunRequest::new(
            AgentBinding::direct(built.agent().clone()),
            Arc::new(PagingResolver(model.clone())),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("read reference"))],
        )
        .with_config(adapter.initialize(host.build_run_config()))
        .with_services(host.build_tool_services());
        Runner::run(built.bind(request)).await.unwrap();
        assert_eq!(*model.observed.lock().unwrap(), original);
    }
}

#[tokio::test]
async fn responses_wire_snapshot_includes_the_explicit_executor_skill_interface() {
    use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let directory = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(directory.path())
        .unwrap()
        .with_web_access(services.clone())
        .with_user_input(services)
        .with_collaboration()
        .with_skill_providers(executor_source(), executor_roots(directory.path()));
    let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"wire","output":[]})))
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        "gpt-test",
        OpenAiAuth::new("secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .unwrap();
    let settings = ra_core::model::SettingsLayers::new()
        .with_run(&ModelSettings::new())
        .resolve(&ProviderKey::new("openai"));
    model
        .get_response(
            ModelRequest::new(vec![], settings)
                .with_tools(surface.tool_surface().model_definitions()),
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let ceiling = CodingProfile::CodexLike
        .to_tool_profile()
        .unwrap()
        .budget()
        .max_advertised_bytes()
        .unwrap();
    let size = serde_json::to_vec(&body["tools"]).unwrap().len();
    assert!(
        size <= ceiling,
        "executor skills wire table {size}/{ceiling}"
    );
    insta::assert_json_snapshot!("responses_executor_skills_tools", body["tools"]);
}

fn executor_source() -> SkillProviders {
    SkillProviders::new().with_executor_provider(Arc::new(ExecutorSkillProvider::new(
        SkillCatalog::default(),
    )))
}

fn executor_roots(path: &std::path::Path) -> BTreeMap<String, Workspace> {
    [("root".to_string(), Workspace::open(path).unwrap())].into()
}

#[tokio::test]
async fn external_skill_roots_are_selected_by_authority_and_do_not_grant_the_coding_workspace() {
    use ra_core::{context::RunContext, tool::ToolContext};
    let coding = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first = Workspace::open(first.path()).unwrap();
    let second = Workspace::open(second.path()).unwrap();
    std::fs::write(first.root().join("SKILL.md"), "first external instructions").unwrap();
    std::fs::write(
        second.root().join("SKILL.md"),
        "second external instructions",
    )
    .unwrap();
    let host = CodingHost::open(coding.path()).unwrap();
    std::fs::write(
        host.workspace_root().join("SKILL.md"),
        "coding workspace secret",
    )
    .unwrap();
    let make_entry = |id: &str, package: &str, path| {
        SkillCatalogEntry::new(
            SkillPackageId(format!("skill://{id}/{package}")),
            SkillAuthority::new(SkillSourceKind::Executor, id),
            package,
            "Read instructions",
            SkillResourceId::environment(
                format!("skill://{id}/{package}/SKILL.md"),
                LOCAL_ENVIRONMENT_ID,
                path,
            ),
        )
    };
    let catalog = SkillCatalog {
        entries: vec![
            make_entry("first", "pkg", first.root().join("SKILL.md")),
            make_entry("second", "pkg", second.root().join("SKILL.md")),
            make_entry("first", "other-root", second.root().join("SKILL.md")),
            make_entry("first", "coding", host.workspace_root().join("SKILL.md")),
            make_entry("unselected", "pkg", first.root().join("SKILL.md")),
        ],
        warnings: vec![],
    };
    let root_paths = [first.root().to_path_buf(), second.root().to_path_buf()];
    let roots = [("first".to_string(), first), ("second".to_string(), second)].into();
    let services = Arc::new(configured_host::Services::default());
    let host = host
        .with_web_access(services.clone())
        .with_user_input(services)
        .with_collaboration()
        .with_skill_providers(
            SkillProviders::new()
                .with_executor_provider(Arc::new(ExecutorSkillProvider::new(catalog))),
            roots,
        );
    let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap();
    let tool = surface
        .tool_surface()
        .tools()
        .iter()
        .find(|tool| tool.origin().qualified_name() == "skills.read")
        .unwrap();
    let agent = ra_core::agent::AgentSpec::builder()
        .id(AgentId::new("reader"))
        .name("Reader")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("external-roots"), &agent);
    let call_id = CallId::new("read");
    for (index, (id, contents)) in [
        ("first", "first external instructions"),
        ("second", "second external instructions"),
    ]
    .into_iter()
    .enumerate()
    {
        let args = json!({"package":format!("skill://{id}/pkg")});
        let output = tool
            .call(ToolContext::new(&run, tool.as_ref(), &call_id, &args))
            .await
            .unwrap();
        let page: Value = serde_json::from_str(output.as_text().unwrap()).unwrap();
        assert_eq!(page["contents"], contents);
        assert_eq!(
            page["skill_root"],
            root_paths[index].to_string_lossy().as_ref()
        );
    }
    for (package, message) in [
        ("skill://first/other-root", "failed to read skill resource"),
        ("skill://first/coding", "failed to read skill resource"),
        ("skill://unselected/pkg", "skill package is not available"),
    ] {
        let args = json!({"package":package});
        let error = tool
            .call(ToolContext::new(&run, tool.as_ref(), &call_id, &args))
            .await
            .unwrap_err();
        let output = tool
            .handle_failure(
                &ToolContext::new(&run, tool.as_ref(), &call_id, &args),
                &error,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.as_text(), Some(message));
    }
}

#[tokio::test]
async fn empty_namespace_configuration_leaves_legacy_selection_and_profile_floor_unchanged() {
    let coding = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let legacy = services.install(CodingHost::open(coding.path()).unwrap());
    let baseline = host_backed_surface(&PromptRole::Main, &legacy, CodingProfile::CodexLike)
        .await
        .unwrap();
    for (providers, roots) in [
        (SkillProviders::new(), BTreeMap::new()),
        (SkillProviders::new(), executor_roots(coding.path())),
        (
            SkillProviders::new().with_provider(
                ra_tools::skills_ns::provider::SkillProviderSource::new(
                    SkillSourceKind::custom("private"),
                    "private",
                    Arc::new(ExecutorSkillProvider::new(SkillCatalog::default())),
                ),
            ),
            executor_roots(coding.path()),
        ),
        (executor_source(), BTreeMap::new()),
    ] {
        let host = legacy.clone().with_skill_providers(providers, roots);
        let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
            .await
            .unwrap();
        assert_eq!(
            surface
                .tool_surface()
                .advertised_names()
                .collect::<Vec<_>>(),
            baseline
                .tool_surface()
                .advertised_names()
                .collect::<Vec<_>>()
        );
        assert_eq!(surface.budget(), baseline.budget());
        assert_eq!(surface.tool_surface().advertised_count(), 18);
    }
    let host = CodingHost::open(coding.path())
        .unwrap()
        .with_skill_providers(SkillProviders::new(), BTreeMap::new());
    let core = host_backed_surface(&PromptRole::Main, &host, CodingProfile::Core)
        .await
        .unwrap();
    assert_eq!(core.tool_surface().advertised_count(), 6);
}

#[tokio::test]
async fn cloud_without_executor_roots_selects_the_namespace_and_its_profile_floor() {
    let coding = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let base = CodingHost::open(coding.path())
        .unwrap()
        .with_web_access(services.clone())
        .with_user_input(services)
        .with_collaboration();
    let executor = base
        .clone()
        .with_skill_providers(executor_source(), executor_roots(coding.path()));
    let baseline = host_backed_surface(&PromptRole::Main, &executor, CodingProfile::CodexLike)
        .await
        .unwrap();
    let cloud = SkillProviders::new().with_cloud_provider(Arc::new(ExecutorSkillProvider::new(
        SkillCatalog::default(),
    )));
    for providers in [
        cloud.clone(),
        cloud.with_executor_provider(Arc::new(
            ExecutorSkillProvider::new(SkillCatalog::default()),
        )),
    ] {
        let host = base
            .clone()
            .with_skill_providers(providers, BTreeMap::new());
        let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
            .await
            .unwrap();
        assert_eq!(
            surface.tool_surface().model_definitions(),
            baseline.tool_surface().model_definitions()
        );
        assert_eq!(surface.budget(), baseline.budget());
        assert_eq!(surface.tool_surface().advertised_count(), 19);
    }
}
