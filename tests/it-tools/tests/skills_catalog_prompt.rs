//! Catalog prompt parity, run binding and the namespace's shared discovery lifecycle.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use ra_core::{
    agent::AgentSpec,
    capability::Capability,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::ToolContext,
};
use ra_exec::fs::Workspace;
use ra_tools::{
    capability::SkillsNsCapability,
    skills_ns::{SkillProvider, SkillProviders, catalog::*, provider::*},
};
use serde_json::{Value, json};

fn run(id: &str) -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    RunContext::new(RunId::new(id), &agent)
}

struct CatalogProvider {
    catalog: Mutex<SkillCatalog>,
    queries: Mutex<Vec<String>>,
}
impl SkillProvider for CatalogProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            self.queries.lock().unwrap().push(query.turn_id);
            Ok(self.catalog.lock().unwrap().clone())
        })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            Ok(SkillReadResult {
                resource: request.resource,
                contents: "Selected instructions.".into(),
            })
        })
    }
}
fn provider(catalog: SkillCatalog) -> Arc<CatalogProvider> {
    Arc::new(CatalogProvider {
        catalog: Mutex::new(catalog),
        queries: Mutex::new(Vec::new()),
    })
}
fn executor_entry(name: &str) -> SkillCatalogEntry {
    SkillCatalogEntry::new(
        SkillPackageId(format!("skill://root/{name}")),
        SkillAuthority::new(SkillSourceKind::Executor, "root"),
        name,
        "Fix lint errors.",
        SkillResourceId::new(format!("skill://root/{name}/SKILL.md")),
    )
}
fn roots(workspace: &Workspace) -> BTreeMap<String, Workspace> {
    [("root".into(), workspace.clone())].into()
}

fn fixture_catalog(case: &Value, workspace: &Workspace, kind: &str) -> SkillCatalog {
    let entries = case["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["authority"]["kind"] == kind)
        .map(|entry| {
            let id = entry["id"].as_str().unwrap();
            let source = if kind == "executor" {
                SkillSourceKind::Executor
            } else {
                SkillSourceKind::Cloud
            };
            let resource = if kind == "executor" {
                SkillResourceId::environment(
                    entry["main_prompt"].as_str().unwrap(),
                    LOCAL_ENVIRONMENT_ID,
                    workspace.root().join("skill/SKILL.md"),
                )
            } else {
                SkillResourceId::new(entry["main_prompt"].as_str().unwrap())
            };
            let mut result = SkillCatalogEntry::new(
                SkillPackageId(id.into()),
                SkillAuthority::new(source, entry["authority"]["id"].as_str().unwrap()),
                entry["name"].as_str().unwrap(),
                entry["description"].as_str().unwrap(),
                resource,
            )
            .with_short_description(entry["short_description"].as_str().map(str::to_string));
            if let Some(root) = entry["alias_root"].as_str() {
                result = result.with_alias_root(root);
            }
            if let Some(order) = entry["alias_root_order"].as_u64() {
                result = result.with_alias_root_order(usize::try_from(order).unwrap());
            }
            if let Some(path) = entry["display_path"].as_str() {
                result = result.with_display_path(path);
            }
            if entry["enabled"] == false {
                result = result.disabled();
            }
            if entry["prompt_visible"] == false {
                result = result.hidden_from_prompt();
            }
            result
        })
        .collect();
    SkillCatalog {
        entries,
        warnings: vec![],
    }
}

#[tokio::test]
async fn catalog_text_and_allocation_match_the_executed_upstream_renderer() {
    let reference: Vec<Value> = serde_json::from_str(include_str!(
        "fixtures/skills_catalog_prompt/upstream-render.json"
    ))
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let mut fixed_allowances = BTreeMap::new();
    for record in reference {
        let case = &record["case"];
        let providers = SkillProviders::new()
            .with_executor_provider(provider(fixture_catalog(case, &workspace, "executor")))
            .with_cloud_provider(provider(fixture_catalog(case, &workspace, "cloud")));
        let max_tokens = case["max_context_tokens"]
            .as_u64()
            .and_then(|n| NonZeroUsize::new(usize::try_from(n).unwrap()));
        let usage = case["usage"].as_bool().unwrap();
        let mut capability = SkillsNsCapability::new(providers, roots(&workspace), 8000)
            .unwrap()
            .with_catalog_context_window(case["context_window"].as_i64());
        if let Some(limit) = max_tokens {
            capability = capability.with_catalog_max_context_tokens(limit);
        }
        if usage {
            capability = capability.with_skills_usage_instructions();
        }
        assert!(capability.static_instructions().await.unwrap().is_none());
        assert!(capability.instructions().await.unwrap().is_none());
        let bound = capability.bind(&run("oracle")).unwrap().unwrap();
        let actual = bound.instructions().await.unwrap().map(|section| {
            assert_eq!(section.source(), &capability.kind().prompt_source());
            let ceiling = section.token_budget().unwrap();
            assert!(ceiling >= section.token_estimate(), "{}", case["name"]);
            let metadata_tokens = record["budget"]["tokens"]
                .as_u64()
                .unwrap_or_else(|| record["budget"]["characters"].as_u64().unwrap().div_ceil(4));
            let fixed = ceiling
                .checked_sub(usize::try_from(metadata_tokens).unwrap())
                .unwrap();
            assert_eq!(fixed, *fixed_allowances.entry(usage).or_insert(fixed));
            section.content().to_string()
        });
        assert_eq!(
            actual,
            record["prompt"].as_str().map(str::to_string),
            "{}",
            case["name"]
        );
    }
}

#[tokio::test]
async fn catalog_aliases_are_readable_directly_without_listing_and_short_descriptions_are_prompt_only()
 {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let root = format!(
        "skill://root/{}",
        "long-package-prefix/".repeat(8).trim_end_matches('/')
    );
    let catalog = SkillCatalog {
        entries: (0..10)
            .map(|index| {
                let id = format!("{root}/skill-{index}");
                SkillCatalogEntry::new(
                    SkillPackageId(id.clone()),
                    SkillAuthority::new(SkillSourceKind::Executor, "root"),
                    format!("skill-{index}"),
                    "Full purpose in skills.list.",
                    SkillResourceId::environment(
                        format!("{id}/SKILL.md"),
                        LOCAL_ENVIRONMENT_ID,
                        workspace.root().join("skill/SKILL.md"),
                    ),
                )
                .with_short_description(Some("Concise catalog purpose.".into()))
                .with_alias_root(&root)
            })
            .collect(),
        warnings: vec![],
    };
    let provider = provider(catalog);
    let capability = SkillsNsCapability::new(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots(&workspace),
        8000,
    )
    .unwrap()
    .with_skills_usage_instructions();
    let run = run("direct");
    let bound = capability.bind(&run).unwrap().unwrap();
    let prompt = bound.instructions().await.unwrap().unwrap();
    assert!(prompt.content().contains("(executor package: e0/skill-0)"));
    assert!(prompt.content().contains("Concise catalog purpose."));
    assert!(!prompt.content().contains("Full purpose in skills.list."));
    assert!(prompt.content().contains("The main agent must read each required instruction or reference file itself before acting on it."));
    let tool = capability.read().unwrap();
    let id = CallId::new("read");
    let args = json!({"package":"e0/skill-0"});
    let output = tool
        .call(ToolContext::new(&run, tool.as_ref(), &id, &args))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(output.as_text().unwrap()).unwrap();
    assert_eq!(output["contents"], "Selected instructions.");
    assert_eq!(output["resource"], format!("{root}/skill-0/SKILL.md"));
    let tool = capability.list().unwrap();
    let args = json!({"authority":{"kind":"executor"}});
    let output = tool
        .call(ToolContext::new(&run, tool.as_ref(), &id, &args))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(output.as_text().unwrap()).unwrap();
    assert_eq!(
        output["skills"][0]["description"],
        "Full purpose in skills.list."
    );
    assert_eq!(*provider.queries.lock().unwrap(), ["direct"]);
}

#[tokio::test]
async fn bound_catalog_and_tools_share_one_load_and_refresh_together_for_a_new_run() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let provider = provider(SkillCatalog {
        entries: vec![executor_entry("first")],
        warnings: vec![],
    });
    let capability = SkillsNsCapability::new(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots(&workspace),
        8000,
    )
    .unwrap();
    let first_run = run("first-run");
    let bound = capability.bind(&first_run).unwrap().unwrap();
    let tool = capability.list().unwrap();
    let id = CallId::new("list");
    let args = json!({"authority":{"kind":"executor"}});
    let (prompt, listing) = tokio::join!(
        bound.instructions(),
        tool.call(ToolContext::new(&first_run, tool.as_ref(), &id, &args))
    );
    assert!(prompt.unwrap().unwrap().content().contains("- first:"));
    assert!(
        listing
            .unwrap()
            .as_text()
            .unwrap()
            .contains("skill://root/first")
    );
    *provider.catalog.lock().unwrap() = SkillCatalog {
        entries: vec![executor_entry("second")],
        warnings: vec![],
    };
    assert!(
        bound
            .instructions()
            .await
            .unwrap()
            .unwrap()
            .content()
            .contains("- first:")
    );
    let second = capability.bind(&run("second-run")).unwrap().unwrap();
    let prompt = second.instructions().await.unwrap().unwrap();
    assert!(prompt.content().contains("- second:"));
    assert!(!prompt.content().contains("- first:"));
    assert_eq!(
        *provider.queries.lock().unwrap(),
        ["first-run", "second-run"]
    );
}

#[tokio::test]
async fn unavailable_executor_and_hidden_catalogs_contribute_no_prompt() {
    let provider = provider(SkillCatalog {
        entries: vec![executor_entry("hidden").hidden_from_prompt()],
        warnings: vec!["discovery warning".into()],
    });
    let capability = SkillsNsCapability::new(
        SkillProviders::new().with_executor_provider(provider.clone()),
        BTreeMap::new(),
        8000,
    )
    .unwrap();
    let bound = capability.bind(&run("no-roots")).unwrap().unwrap();
    assert!(bound.instructions().await.unwrap().is_none());
    assert!(provider.queries.lock().unwrap().is_empty());
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let capability = SkillsNsCapability::new(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots(&workspace),
        8000,
    )
    .unwrap();
    let bound = capability.bind(&run("hidden")).unwrap().unwrap();
    assert!(bound.instructions().await.unwrap().is_none());
    assert_eq!(*provider.queries.lock().unwrap(), ["hidden"]);
}

struct ListingFailure;
impl SkillProvider for ListingFailure {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Err(SkillProviderError::new("private discovery failure")) })
    }

    fn read<'a>(&'a self, _: SkillReadRequest<'a>) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async { Err(SkillProviderError::new("unavailable")) })
    }
}

#[tokio::test]
async fn discovery_failure_does_not_hide_cloud_skills_or_publish_unsupported_authorities() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let cloud = provider(SkillCatalog {
        entries: [CLOUD_AUTHORITY_ID, "unsupported-cloud"]
            .into_iter()
            .map(|authority| {
                SkillCatalogEntry::new(
                    SkillPackageId(format!("skill://{authority}/available")),
                    SkillAuthority::new(SkillSourceKind::Cloud, authority),
                    authority,
                    "Cloud purpose.",
                    SkillResourceId::new("opaque-main"),
                )
            })
            .collect(),
        warnings: vec![],
    });
    for executor_roots in [BTreeMap::new(), roots(&workspace)] {
        let capability = SkillsNsCapability::new(
            SkillProviders::new()
                .with_executor_provider(Arc::new(ListingFailure))
                .with_cloud_provider(cloud.clone()),
            executor_roots,
            8000,
        )
        .unwrap();
        let bound = capability.bind(&run("cloud-fallback")).unwrap().unwrap();
        let prompt = bound.instructions().await.unwrap().unwrap();
        assert!(
            prompt
                .content()
                .contains("(cloud package: skill://codex_apps/available)")
        );
        assert!(!prompt.content().contains("unsupported-cloud"));
        assert!(!prompt.content().contains("private discovery failure"));
        assert!(!prompt.content().contains("executor skills unavailable"));
    }
}
