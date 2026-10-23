//! Production assembly installs catalog instructions only with the namespace tools.
#[path = "../../support/configured_coding_host.rs"]
#[allow(dead_code)]
mod configured_host;

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use ra_coding::{CodingHost, CodingProfile, skills::SkillCatalogPromptConfig};
use ra_core::{
    agent::AgentId,
    cancel::CancelScope,
    capability::CapabilityFamily,
    context::RunContext,
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
use ra_exec::fs::Workspace;
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use ra_tools::skills_ns::{
    ExecutorSkillProvider, SkillProvider, SkillProviders, catalog::*, provider::*,
};
use serde_json::{Value, json};

struct CountingProvider {
    catalog: Mutex<SkillCatalog>,
    calls: AtomicUsize,
}
impl SkillProvider for CountingProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.catalog.lock().unwrap().clone())
        })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            ExecutorSkillProvider::new(SkillCatalog::default())
                .read(request)
                .await
        })
    }
}

struct CatalogModel {
    expect_catalog: bool,
    read_package: bool,
    calls: AtomicUsize,
}
#[async_trait]
impl Model for CatalogModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        let instructions = request.system_instructions().unwrap();
        assert_eq!(
            instructions.contains("<skills_instructions>"),
            self.expect_catalog
        );
        assert_eq!(
            instructions.matches("<skills_instructions>").count(),
            usize::from(self.expect_catalog)
        );
        assert_eq!(
            request
                .tools()
                .iter()
                .any(|tool| tool.qualified_name() == "skills.read"),
            self.expect_catalog
        );
        if self.expect_catalog {
            assert!(
                instructions.contains("- skill-0: Fix lint errors. (executor package: e0/skill-0)"),
                "{}",
                instructions.split("<skills_instructions>").last().unwrap()
            );
            assert!(instructions.contains("### Skill roots"));
            assert!(instructions.contains("### How to use skills"));
            assert!(
                instructions.contains("If a read is paginated, follow `next_cursor` until EOF.")
            );
            assert!(!instructions.contains("actual skill body"));
        }
        if self.read_package && step == 0 {
            let line = instructions
                .lines()
                .find(|line| line.starts_with("- skill-0:"))
                .unwrap();
            let package = line
                .split("(executor package: ")
                .nth(1)
                .unwrap()
                .strip_suffix(')')
                .unwrap();
            return Ok(ModelResponse::new(vec![RunItem::new(
                ItemId::new("read-skill"),
                RunItemKind::ToolCall(
                    ToolCall::new(
                        CallId::new("read-skill"),
                        "read",
                        json!({"package":package}),
                    )
                    .with_namespace("skills"),
                ),
            )]));
        }
        if self.read_package {
            let result = request
                .input()
                .iter()
                .find_map(|item| match item {
                    ModelInputItem::ToolCallOutput(output) => Some(output.output()),
                    _ => None,
                })
                .unwrap();
            let output: ra_core::tool::ToolOutput = serde_json::from_value(result.clone()).unwrap();
            let page: Value = serde_json::from_str(output.as_text().unwrap()).unwrap();
            assert_eq!(page["contents"], "actual skill body from an external root");
        }
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("answer-{step}")),
            RunItemKind::Message(Message::assistant("complete", OutputPhase::Final)),
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
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn host(coding: &Workspace, external: &Workspace) -> (CodingHost, Arc<CountingProvider>) {
    let package_root = format!(
        "skill://external/{}",
        "long-shared-catalog-prefix/"
            .repeat(8)
            .trim_end_matches('/')
    );
    std::fs::write(
        external.root().join("SKILL.md"),
        "actual skill body from an external root",
    )
    .unwrap();
    let catalog = SkillCatalog {
        entries: (0..8)
            .map(|index| {
                let package = format!("{package_root}/skill-{index}");
                SkillCatalogEntry::new(
                    SkillPackageId(package.clone()),
                    SkillAuthority::new(SkillSourceKind::Executor, "external"),
                    format!("skill-{index}"),
                    "Full description.",
                    SkillResourceId::environment(
                        format!("{package}/SKILL.md"),
                        LOCAL_ENVIRONMENT_ID,
                        external.root().join("SKILL.md"),
                    ),
                )
                .with_short_description(Some("Fix lint errors.".into()))
                .with_alias_root(&package_root)
            })
            .collect(),
        warnings: vec![],
    };
    let provider = Arc::new(CountingProvider {
        catalog: Mutex::new(catalog),
        calls: AtomicUsize::new(0),
    });
    let services = Arc::new(configured_host::Services::default());
    let host = CodingHost::open(coding.root())
        .unwrap()
        .with_web_access(services.clone())
        .with_user_input(services)
        .with_collaboration()
        .with_skill_catalog_prompt(
            SkillCatalogPromptConfig::new()
                .with_max_context_tokens(NonZeroUsize::new(500).unwrap())
                .with_usage_instructions(),
        )
        .with_skill_providers(
            SkillProviders::new().with_executor_provider(provider.clone()),
            BTreeMap::from([("external".into(), external.clone())]),
        );
    (host, provider)
}

async fn execute(
    host: &CodingHost,
    role: &PromptRole,
    tier: CodingProfile,
    model: Arc<CatalogModel>,
) {
    let built =
        ra_coding::build_agent_with_control(AgentId::new("coding"), "Coding", role, host, tier)
            .await
            .unwrap();
    assert!(
        !built
            .agent()
            .instructions()
            .unwrap()
            .as_static()
            .unwrap()
            .contains("<skills_instructions>")
    );
    let request = RunRequest::new(
        AgentBinding::direct(built.agent().clone()),
        Arc::new(Resolver(model)),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(
            "Use the listed skill",
        ))],
    )
    .with_config(host.build_run_config())
    .with_services(host.build_tool_services());
    Runner::run(built.bind(request)).await.unwrap();
}

#[tokio::test]
async fn a_model_reads_an_aliased_external_package_from_the_catalog_without_listing() {
    let coding = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let (host, provider) = host(
        &Workspace::open(coding.path()).unwrap(),
        &Workspace::open(external.path()).unwrap(),
    );
    let model = Arc::new(CatalogModel {
        expect_catalog: true,
        read_package: true,
        calls: AtomicUsize::new(0),
    });
    execute(
        &host,
        &PromptRole::Main,
        CodingProfile::CodexLike,
        model.clone(),
    )
    .await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn every_role_and_tier_receives_catalog_context_only_when_it_installs_the_namespace() {
    let coding = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let (host, provider) = host(
        &Workspace::open(coding.path()).unwrap(),
        &Workspace::open(external.path()).unwrap(),
    );
    let mut catalog_runs = 0;
    for tier in CodingProfile::SHIPPED {
        for role in [
            PromptRole::Main,
            PromptRole::ReadOnlySpecialist,
            PromptRole::Planner,
            PromptRole::OneOffAnswer,
            PromptRole::Coordinator,
        ] {
            let expect_catalog = tier != CodingProfile::Core && !role.is_one_off();
            let before = provider.calls.load(Ordering::SeqCst);
            let model = Arc::new(CatalogModel {
                expect_catalog,
                read_package: false,
                calls: AtomicUsize::new(0),
            });
            execute(&host, &role, tier, model).await;
            catalog_runs += usize::from(expect_catalog);
            assert_eq!(
                provider.calls.load(Ordering::SeqCst) - before,
                usize::from(expect_catalog)
            );
        }
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), catalog_runs);
}

#[derive(Default)]
struct RecordingModel(Mutex<Vec<String>>);

#[async_trait]
impl Model for RecordingModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0
            .lock()
            .unwrap()
            .push(request.system_instructions().unwrap().to_string());
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new("recorded"),
            RunItemKind::Message(Message::assistant("complete", OutputPhase::Final)),
        )]))
    }
}

#[tokio::test]
async fn reusing_one_agent_refreshes_the_catalog_for_each_new_run() {
    let coding = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let (host, provider) = host(
        &Workspace::open(coding.path()).unwrap(),
        &Workspace::open(external.path()).unwrap(),
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
    let model = Arc::new(RecordingModel::default());
    for run_id in ["first-run", "second-run"] {
        let request = RunRequest::new(
            AgentBinding::direct(built.agent().clone()),
            Arc::new(Resolver(model.clone())),
            RunId::new(run_id),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(
                "Inspect the catalog",
            ))],
        )
        .with_config(host.build_run_config())
        .with_services(host.build_tool_services());
        Runner::run(built.bind(request)).await.unwrap();
        provider.catalog.lock().unwrap().entries[0].short_description =
            Some("Updated between runs.".into());
    }
    let prompts = model.0.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("- skill-0: Fix lint errors."));
    assert!(prompts[1].contains("- skill-0: Updated between runs."));
    assert!(!prompts[0].contains("Updated between runs."));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    for prompt in prompts.iter() {
        assert_eq!(prompt.matches("<skills_instructions>").count(), 1);
    }
}

struct SwitchingResolver(Arc<RecordingModel>);
impl ModelResolver for SwitchingResolver {
    fn resolve_model(&self, name: Option<&str>) -> Result<ResolvedModel> {
        let canonical = match name {
            Some("large-alias") => Some("gpt-daybreak-red-latest"),
            Some("unknown-alias") => Some("private-unknown-model"),
            Some("sdk-only-alias") => Some("gpt-4.1"),
            Some("versioned-alias") => Some("gpt-5.5-pro-2026-04-23"),
            Some("namespaced-alias") => Some("openai-codex/gpt-daybreak-red-latest-2026-01-01"),
            Some("unnamed-alias") => None,
            _ => Some("openai/gpt-5.5"),
        };
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                canonical.map(str::to_owned),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

async fn catalogs_for_resolved_models(config: SkillCatalogPromptConfig) -> Vec<String> {
    let coding = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let (host, provider) = host(
        &Workspace::open(coding.path()).unwrap(),
        &Workspace::open(external.path()).unwrap(),
    );
    provider.catalog.lock().unwrap().entries = (0..800)
        .map(|index| {
            let name = format!("skill-{index:03}");
            let package = format!("skill://external/{name}");
            SkillCatalogEntry::new(
                SkillPackageId(package.clone()),
                SkillAuthority::new(SkillSourceKind::Executor, "external"),
                name,
                "A description long enough to keep the catalog under sustained budget pressure.",
                SkillResourceId::new(format!("{package}/SKILL.md")),
            )
        })
        .collect();
    let host = host.with_skill_catalog_prompt(config);
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    let model = Arc::new(RecordingModel::default());
    for override_name in [
        Some("small-alias"),
        Some("large-alias"),
        Some("unknown-alias"),
        None,
        Some("sdk-only-alias"),
        Some("versioned-alias"),
        Some("namespaced-alias"),
        Some("unnamed-alias"),
    ] {
        let mut config = host.build_run_config();
        if let Some(name) = override_name {
            config = config.with_model(name);
        }
        let request = RunRequest::new(
            AgentBinding::direct(built.agent().clone()),
            Arc::new(SwitchingResolver(model.clone())),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(
                "Inspect the catalog",
            ))],
        )
        .with_config(config)
        .with_services(host.build_tool_services());
        Runner::run(built.bind(request)).await.unwrap();
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 8);
    let prompts = model.0.lock().unwrap().clone();
    prompts
        .into_iter()
        .map(|prompt| {
            prompt
                .split("<skills_instructions>")
                .nth(1)
                .unwrap()
                .split("</skills_instructions>")
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn resolved_run_models_scale_catalogs_while_explicit_token_limits_take_precedence() {
    let count = |prompt: &String| {
        prompt
            .lines()
            .filter(|line| line.starts_with("- skill-"))
            .count()
    };
    let prompts = catalogs_for_resolved_models(SkillCatalogPromptConfig::new()).await;
    let counts = prompts.iter().map(count).collect::<Vec<_>>();
    assert!(
        counts[1] > counts[0],
        "large versus small resolved window: {counts:?}"
    );
    assert_eq!(prompts[0], prompts[2], "unknown named models use 272,000");
    assert_eq!(
        prompts[0], prompts[3],
        "the resolver's default has the same canonical model"
    );
    assert_eq!(
        prompts[2], prompts[4],
        "named models absent from the Codex table use 272,000"
    );
    assert_eq!(
        prompts[0], prompts[5],
        "dated variants use the model prefix"
    );
    assert_eq!(
        prompts[1], prompts[6],
        "provider namespaces retry the suffix"
    );
    assert!(
        counts[0] > counts[7],
        "only unnamed models use the character fallback"
    );

    let limited = catalogs_for_resolved_models(
        SkillCatalogPromptConfig::new().with_max_context_tokens(NonZeroUsize::new(100).unwrap()),
    )
    .await;
    for prompt in &limited[1..] {
        assert_eq!(prompt, &limited[0]);
    }
    assert!(count(&limited[0]) < counts[7]);

    let windows = ra_context::window::ContextWindowConfig::new(
        BTreeMap::from([
            (" GPT-5.5 ".into(), 10_000),
            ("gpt-daybreak-red-latest".into(), 400_000),
            ("gpt-4.1".into(), 20_000),
            ("private-unknown-model".into(), 30_000),
            ("gpt-5.5-pro-2026-04-23".into(), 20_000),
            (
                "openai-codex/gpt-daybreak-red-latest-2026-01-01".into(),
                25_000,
            ),
        ]),
        Default::default(),
    )
    .unwrap();
    let custom =
        catalogs_for_resolved_models(SkillCatalogPromptConfig::new().with_context_windows(windows))
            .await;
    assert!(count(&custom[0]) < counts[0]);
    assert!(count(&custom[1]) > count(&custom[0]));
    assert!(
        count(&custom[2]) < counts[2],
        "host overrides precede the unknown fallback"
    );
    assert!(count(&custom[4]) < counts[4]);
    assert!(
        count(&custom[5]) < counts[5],
        "host overrides precede prefix matching"
    );
    assert!(
        count(&custom[6]) < counts[6],
        "host overrides precede namespace retry"
    );
    assert_eq!(
        custom[7], prompts[7],
        "unnamed models keep the character fallback"
    );
    assert_eq!(
        ra_context::window::ContextWindowConfig::default().context_window("gpt-5.5"),
        Some(1_047_576),
        "compaction retains the SDK capacity independently of the skill catalog"
    );
}

#[tokio::test]
async fn catalog_budgets_match_every_pinned_codex_model_window() {
    let reference: Value = serde_json::from_str(include_str!(
        "fixtures/skills_catalog_prompt/upstream-model-windows.json"
    ))
    .unwrap();
    let models = reference["context_windows"].as_object().unwrap();
    assert_eq!(models.len(), 11);
    for include_usage in [false, true] {
        let coding = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let (host, _) = host(
            &Workspace::open(coding.path()).unwrap(),
            &Workspace::open(external.path()).unwrap(),
        );
        let mut config = SkillCatalogPromptConfig::new().with_context_windows(Default::default());
        if include_usage {
            config = config.with_usage_instructions();
        }
        let host = host.with_skill_catalog_prompt(config);
        let built = ra_coding::build_agent_with_control(
            AgentId::new("coding"),
            "Coding",
            &PromptRole::Main,
            &host,
            CodingProfile::CodexLike,
        )
        .await
        .unwrap();
        let config = built.configure_run(host.build_run_config());
        let capability = config
            .capabilities()
            .iter()
            .find(|capability| capability.kind() == CapabilityFamily::SKILLS)
            .unwrap();
        let fixed_tokens = if include_usage { 970 } else { 207 };
        for (model, window) in models {
            let metadata_tokens = usize::try_from(window.as_u64().unwrap() * 2 / 100).unwrap();
            for model in [model.clone(), format!("openai/{model}")] {
                let run = RunContext::new(RunId::generate(), built.agent())
                    .with_resolved_model_name(&model);
                let bound = capability.bind(&run).unwrap().unwrap();
                let section = bound.instructions().await.unwrap().unwrap();
                assert_eq!(
                    section.token_budget(),
                    Some(metadata_tokens + fixed_tokens),
                    "{model}, usage: {include_usage}"
                );
            }
        }
        for lookup in reference["lookups"].as_array().unwrap() {
            let model = lookup["model"].as_str().unwrap();
            let metadata_tokens =
                usize::try_from(lookup["context_window"].as_u64().unwrap() * 2 / 100).unwrap();
            let run =
                RunContext::new(RunId::generate(), built.agent()).with_resolved_model_name(model);
            let section = capability
                .bind(&run)
                .unwrap()
                .unwrap()
                .instructions()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                section.token_budget(),
                Some(metadata_tokens + fixed_tokens),
                "{model}"
            );
        }
        let unnamed = capability
            .bind(&RunContext::new(RunId::generate(), built.agent()))
            .unwrap()
            .unwrap()
            .instructions()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unnamed.token_budget(), Some(2_000 + fixed_tokens));
    }
}

#[test]
fn model_lookup_preserves_longest_prefix_ties_and_full_name_priority() {
    let reference: Value = serde_json::from_str(include_str!(
        "fixtures/skills_catalog_prompt/upstream-model-windows.json"
    ))
    .unwrap();
    let candidates = reference["synthetic_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| {
            (
                candidate[0].as_str().unwrap(),
                candidate[1].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    for lookup in reference["synthetic_lookups"].as_array().unwrap() {
        let model = lookup["model"].as_str().unwrap();
        assert_eq!(
            ra_coding::skills::model_context_window_for_candidates(model, &candidates),
            lookup["context_window"].as_u64().unwrap(),
            "{model}"
        );
    }
}
