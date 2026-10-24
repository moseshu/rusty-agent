//! Pinned explicit-selection behavior, authoritative source reads and inference requirements.

use ra_core::{
    agent::AgentSpec,
    capability::Capability,
    context::RunContext,
    item::{AgentId, Message, ModelInputItem},
    state::RunId,
};
use ra_exec::fs::Workspace;
use ra_tools::{
    capability::SkillsNsCapability,
    skills_ns::{catalog::*, provider::*, *},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

fn entry(name: &str, kind: SkillSourceKind, authority: &str, path: &str) -> SkillCatalogEntry {
    SkillCatalogEntry::new(
        SkillPackageId(path.into()),
        SkillAuthority::new(kind, authority),
        name,
        "Test skill",
        SkillResourceId::new(path),
    )
    .with_display_path(path)
}
fn text(input: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(input))
}
fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    RunContext::new(RunId::new("invocation"), &agent)
}
fn message_text(message: &Message) -> String {
    message
        .content()
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn selection_matches_executed_codex_parser_and_selector() {
    let fixture: Value = serde_json::from_str(include_str!(
        "fixtures/skill_invocation/upstream-selection.json"
    ))
    .unwrap();
    let catalog = SkillCatalog {
        entries: fixture["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let mut result = entry(
                    e["name"].as_str().unwrap(),
                    SkillSourceKind::Executor,
                    &e["authority"].to_string(),
                    e["path"].as_str().unwrap(),
                );
                result.id = SkillPackageId(e["package"].as_str().unwrap().into());
                result.enabled = e["enabled"].as_bool().unwrap();
                if result.name == "hidden" {
                    result.prompt_visible = false;
                }
                result
            })
            .collect(),
        warnings: vec![],
    };
    for case in fixture["cases"].as_array().unwrap() {
        let inputs: Vec<_> = case["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                if let Some(t) = i["text"].as_str() {
                    SkillInput::Text { text: t.into() }
                } else if i["kind"] == "skill" {
                    SkillInput::Skill {
                        name: i["name"].as_str().unwrap().into(),
                        path: i["path"].as_str().unwrap().into(),
                    }
                } else {
                    SkillInput::Mention {
                        name: i["name"].as_str().unwrap().into(),
                        path: i["path"].as_str().unwrap().into(),
                    }
                }
            })
            .collect();
        let mut selected: Vec<_> = collect_explicit_skill_mentions(&inputs, &catalog)
            .iter()
            .map(|entry| {
                catalog
                    .entries
                    .iter()
                    .position(|e| e.id == entry.id && e.authority == entry.authority)
                    .unwrap()
            })
            .collect();
        selected.sort_unstable();
        assert_eq!(json!(selected), case["selected"], "{}", case["id"]);
        let host_selected: Vec<_> =
            collect_host_skill_mentions(&inputs, &catalog, &std::collections::HashMap::new())
                .iter()
                .map(|entry| {
                    catalog
                        .entries
                        .iter()
                        .position(|e| e.id == entry.id && e.authority == entry.authority)
                        .unwrap()
                })
                .collect();
        assert_eq!(
            json!(host_selected),
            case["selected_host"],
            "Host {}",
            case["id"]
        );
    }
}

struct Provider {
    catalog: SkillCatalog,
    contents: String,
    reads: Mutex<Vec<(SkillAuthority, SkillPackageId, SkillResourceId)>>,
    fail: bool,
}
impl SkillProvider for Provider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Ok(self.catalog.clone()) })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            self.reads.lock().unwrap().push((
                request.authority,
                request.package,
                request.resource.clone(),
            ));
            if self.fail {
                Err(SkillProviderError::new("unavailable"))
            } else {
                Ok(SkillReadResult {
                    resource: request.resource,
                    contents: self.contents.clone(),
                })
            }
        })
    }
}
fn provider(entries: Vec<SkillCatalogEntry>, contents: String, fail: bool) -> Arc<Provider> {
    Arc::new(Provider {
        catalog: SkillCatalog {
            entries,
            warnings: vec![],
        },
        contents,
        reads: Mutex::new(vec![]),
        fail,
    })
}

#[tokio::test]
async fn hidden_prompt_truncates_at_utf8_boundary_and_keeps_full_read_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let root = Workspace::open(temp.path()).unwrap();
    let skill = entry(
        "hidden",
        SkillSourceKind::Executor,
        "root",
        "skill://root/hidden/SKILL.md",
    )
    .hidden_from_prompt();
    let body = format!("{}érest", "a".repeat(7999));
    let source = provider(vec![skill.clone()], body, false);
    let capability = SkillsNsCapability::new(
        SkillProviders::new().with_executor_provider(source.clone()),
        BTreeMap::from([("root".into(), root)]),
        4096,
    )
    .unwrap()
    .bind(&run())
    .unwrap()
    .unwrap();
    let messages = capability
        .contribute_turn_input(&run(), &[text("$hidden $hidden")])
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    let message = message_text(&messages[0]);
    let expected_metadata = json!({"authority":{"kind":"executor","id":"root"},"package":skill.id.0,"main_resource":skill.main_prompt.as_str()});
    assert_eq!(
        message,
        format!(
            "<skill>\n<name>hidden</name>\n<path>skill://root/hidden/SKILL.md</path>\n<resource_access>{expected_metadata}</resource_access>\n{}\n</skill>",
            "a".repeat(7999)
        )
    );
    assert_eq!(
        *source.reads.lock().unwrap(),
        vec![(skill.authority, skill.id, skill.main_prompt)]
    );
    assert!(
        capability
            .contribute_turn_input(&run(), &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn failed_reads_do_not_block_other_skills_and_non_user_text_does_not_trigger() {
    let failing = provider(
        vec![entry(
            "broken",
            SkillSourceKind::Host,
            "host",
            "/broken/SKILL.md",
        )],
        String::new(),
        true,
    );
    let working = provider(
        vec![entry(
            "cloud",
            SkillSourceKind::Cloud,
            CLOUD_AUTHORITY_ID,
            "cloud/SKILL.md",
        )],
        "Cloud instructions".into(),
        false,
    );
    let cap = SkillsNsCapability::new(
        SkillProviders::new()
            .with_host_provider(failing.clone())
            .with_cloud_provider(working.clone()),
        BTreeMap::new(),
        4096,
    )
    .unwrap()
    .bind(&run())
    .unwrap()
    .unwrap();
    assert!(
        cap.contribute_turn_input(
            &run(),
            &[ModelInputItem::Message(Message::system("$cloud"))]
        )
        .await
        .unwrap()
        .is_empty()
    );
    let result = cap
        .contribute_turn_input(&run(), &[text("$broken $cloud")])
        .await
        .unwrap();
    assert_eq!(result.len(), 1);
    assert!(message_text(&result[0]).contains("Cloud instructions"));
    assert_eq!(failing.reads.lock().unwrap().len(), 1);
    assert_eq!(working.reads.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn core_host_documents_remain_complete_and_plugin_documents_are_bounded() {
    for (kind, expected_len) in [
        (HostSkillPromptKind::Core, 9000),
        (HostSkillPromptKind::AgentPlugin, 8000),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("SKILL.md");
        std::fs::write(&path, "a".repeat(9000)).unwrap();
        let skill = entry(
            "host",
            SkillSourceKind::Host,
            "host",
            path.to_str().unwrap(),
        )
        .with_host_prompt_kind(kind);
        let source = provider(
            vec![skill],
            "Provider reads are not Core Host reads".into(),
            true,
        );
        let cap = SkillsNsCapability::host_catalog(
            SkillProviders::new().with_host_provider(source.clone()),
        )
        .unwrap()
        .bind(&run())
        .unwrap()
        .unwrap();
        let messages = cap
            .contribute_turn_input(&run(), &[text("$host")])
            .await
            .unwrap();
        assert_eq!(
            message_text(&messages[0]),
            format!(
                "<skill>\n<name>host</name>\n<path>{}</path>\n{}\n</skill>",
                path.display(),
                "a".repeat(expected_len)
            )
        );
        assert!(source.reads.lock().unwrap().is_empty());
    }
}

#[test]
fn requirements_use_environment_bindings_and_enabled_state_without_prompt_visibility() {
    let names = vec!["review".into()];
    let required = || {
        [EnvironmentSkillRequirements {
            environment_id: "env-a",
            skill_names: &names,
        }]
    };
    assert!(validate_required_skills(None, []).is_ok());
    assert_eq!(
        validate_required_skills(None, required()).unwrap_err(),
        "Required skills cannot be validated because the skills extension is unavailable"
    );
    for (enabled, environment, bound, available) in [
        (true, "env-a", true, true),
        (false, "env-a", true, false),
        (true, "env-b", true, false),
        (true, "env-a", false, false),
    ] {
        let mut skill = entry(
            "review",
            SkillSourceKind::Executor,
            "authority-not-env",
            "review/SKILL.md",
        )
        .hidden_from_prompt();
        skill.enabled = enabled;
        if bound {
            skill.main_prompt = SkillResourceId::environment(
                "review/SKILL.md",
                environment,
                "/skills/SKILL.md".into(),
            );
        }
        let catalog = SkillCatalog {
            entries: vec![skill],
            warnings: vec![],
        };
        assert_eq!(
            validate_required_skills(Some((&catalog, None)), required()).is_ok(),
            available
        );
        assert_eq!(
            validate_required_skills(Some((&SkillCatalog::default(), Some(&catalog))), required())
                .is_ok(),
            available
        );
    }
}

#[tokio::test]
async fn required_skill_checks_do_not_read_and_missing_integration_is_an_error() {
    let capability = SkillsNsCapability::new(SkillProviders::new(), BTreeMap::new(), 4096)
        .unwrap()
        .with_required_skills("env-a", ["review"])
        .bind(&run())
        .unwrap()
        .unwrap();
    assert!(
        capability
            .contribute_turn_input(&run(), &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("skills extension is unavailable")
    );
    let source = provider(vec![], String::new(), false);
    let cap =
        SkillsNsCapability::host_catalog(SkillProviders::new().with_host_provider(source.clone()))
            .unwrap()
            .with_required_skills("env-a", ["review"])
            .bind(&run())
            .unwrap()
            .unwrap();
    assert!(
        cap.contribute_turn_input(&run(), &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("Required skill \"review\" from environment \"env-a\" is unavailable")
    );
    assert!(source.reads.lock().unwrap().is_empty());
}

#[test]
fn host_selection_uses_discovery_aliases_and_requires_paths_for_connector_conflicts() {
    let skill = entry(
        "review",
        SkillSourceKind::Host,
        "host",
        "/canonical/SKILL.md",
    )
    .with_display_path("/alias/SKILL.md");
    let catalog = SkillCatalog {
        entries: vec![skill.clone()],
        warnings: vec![],
    };
    let connectors = std::collections::HashMap::from([("review".to_string(), 1)]);
    assert!(
        collect_host_skill_mentions(
            &[SkillInput::Text {
                text: "$review".into()
            }],
            &catalog,
            &connectors
        )
        .is_empty()
    );
    for path in ["/canonical/SKILL.md", "/alias/SKILL.md"] {
        assert_eq!(
            collect_host_skill_mentions(
                &[SkillInput::Text {
                    text: format!("[$review]({path})")
                }],
                &catalog,
                &connectors
            ),
            vec![skill.clone()]
        );
    }
}

#[tokio::test]
async fn executor_selection_supersedes_core_host_prompts_with_the_same_name() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(temp.path()).unwrap();
    let executor = provider(
        vec![entry(
            "review",
            SkillSourceKind::Executor,
            "root",
            "/executor/SKILL.md",
        )],
        "Executor instructions".into(),
        false,
    );
    let host_path = temp.path().join("host.md");
    std::fs::write(&host_path, "Host instructions").unwrap();
    let host = provider(
        vec![
            entry(
                "review",
                SkillSourceKind::Host,
                "host",
                host_path.to_str().unwrap(),
            )
            .with_host_prompt_kind(HostSkillPromptKind::Core),
        ],
        "Unused extension reader".into(),
        true,
    );
    let cap = SkillsNsCapability::new(
        SkillProviders::new()
            .with_executor_provider(executor.clone())
            .with_host_provider(host.clone()),
        [("root".into(), workspace)].into(),
        4096,
    )
    .unwrap()
    .bind(&run())
    .unwrap()
    .unwrap();
    let messages = cap
        .contribute_turn_input(&run(), &[text("$review")])
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert!(message_text(&messages[0]).contains("Executor instructions"));
    assert_eq!(executor.reads.lock().unwrap().len(), 1);
    assert!(host.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fragment_locator_limits_preserve_complete_hidden_resource_access() {
    let name = format!("{}é", "a".repeat(255));
    let path = format!("/{}é/SKILL.md", "x".repeat(1022));
    let skill =
        entry(&name, SkillSourceKind::Cloud, CLOUD_AUTHORITY_ID, &path).hidden_from_prompt();
    let source = provider(vec![skill.clone()], "a".repeat(8000), false);
    let cap = SkillsNsCapability::new(
        SkillProviders::new().with_cloud_provider(source),
        BTreeMap::new(),
        4096,
    )
    .unwrap()
    .bind(&run())
    .unwrap()
    .unwrap();
    let messages = cap
        .contribute_turn_input(&run(), &[text(&format!("[$selected]({path})"))])
        .await
        .unwrap();
    let fragment = message_text(&messages[0]);
    assert!(fragment.contains(&format!("<name>{}</name>", "a".repeat(255))));
    let rendered_path = fragment
        .split("<path>")
        .nth(1)
        .unwrap()
        .split("</path>")
        .next()
        .unwrap();
    assert_eq!(rendered_path.len(), 1023);
    assert_eq!(rendered_path, format!("/{}", "x".repeat(1022)));
    let metadata = fragment
        .split("<resource_access>")
        .nth(1)
        .unwrap()
        .split("</resource_access>")
        .next()
        .unwrap();
    let metadata: Value = serde_json::from_str(metadata).unwrap();
    assert_eq!(metadata["package"], skill.id.0);
    assert_eq!(metadata["main_resource"], path);
    assert!(fragment.ends_with(&format!("\n{}\n</skill>", "a".repeat(8000))));
}

#[tokio::test]
async fn replacing_environment_requirements_preserves_selection_and_error_order() {
    let source = provider(vec![], String::new(), false);
    let cap = SkillsNsCapability::host_catalog(SkillProviders::new().with_host_provider(source))
        .unwrap()
        .with_required_skills("env-z", ["old"])
        .with_required_skills("env-a", ["another"])
        .with_required_skills("env-z", ["replacement"])
        .bind(&run())
        .unwrap()
        .unwrap();
    let error = cap.contribute_turn_input(&run(), &[]).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Required skill \"replacement\" from environment \"env-z\" is unavailable")
    );
}

#[tokio::test]
async fn empty_requirement_lists_remove_environment_prerequisites() {
    let cap = SkillsNsCapability::new(SkillProviders::new(), BTreeMap::new(), 4096)
        .unwrap()
        .with_required_skills("env", ["review"])
        .with_required_skills("env", std::iter::empty::<String>())
        .bind(&run())
        .unwrap()
        .unwrap();
    assert!(
        cap.contribute_turn_input(&run(), &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn core_host_fragments_precede_extension_fragments_in_the_returned_input() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("SKILL.md");
    std::fs::write(&path, "Host instructions").unwrap();
    let host = provider(
        vec![
            entry(
                "host",
                SkillSourceKind::Host,
                "host",
                path.to_str().unwrap(),
            )
            .with_host_prompt_kind(HostSkillPromptKind::Core),
        ],
        "Unused extension reader".into(),
        true,
    );
    let cloud = provider(
        vec![entry(
            "cloud",
            SkillSourceKind::Cloud,
            CLOUD_AUTHORITY_ID,
            "cloud/SKILL.md",
        )],
        "Cloud instructions".into(),
        false,
    );
    let cap = SkillsNsCapability::new(
        SkillProviders::new()
            .with_host_provider(host)
            .with_cloud_provider(cloud),
        BTreeMap::new(),
        4096,
    )
    .unwrap()
    .bind(&run())
    .unwrap()
    .unwrap();
    let fragments = cap
        .contribute_turn_input(&run(), &[text("$cloud $host")])
        .await
        .unwrap();
    assert_eq!(fragments.len(), 2);
    assert!(message_text(&fragments[0]).contains("<name>host</name>"));
    assert!(message_text(&fragments[1]).contains("<name>cloud</name>"));
}

#[test]
fn core_host_snapshots_preserve_loaded_membership_without_absorbing_external_entries() {
    let core = entry("core", SkillSourceKind::Host, "host", "/core/SKILL.md")
        .with_host_prompt_kind(HostSkillPromptKind::Core)
        .disabled();
    let foreign = entry(
        "foreign",
        SkillSourceKind::Host,
        "foreign",
        "/foreign/SKILL.md",
    );
    let snapshot = HostSkillsSnapshot::from_catalog(&SkillCatalog {
        entries: vec![core, foreign],
        warnings: vec![],
    });
    assert!(snapshot.contains("/core/SKILL.md"));
    assert!(!snapshot.contains("/foreign/SKILL.md"));
}

#[test]
fn core_host_reads_resolve_the_stored_path_instead_of_the_callers_spelling() {
    let skill = entry("core", SkillSourceKind::Host, "host", "/core/SKILL.md")
        .with_host_prompt_kind(HostSkillPromptKind::Core);
    let snapshot = HostSkillsSnapshot::from_catalog(&SkillCatalog {
        entries: vec![skill],
        warnings: vec![],
    });
    assert_eq!(
        snapshot.resource_path("/core/link/../SKILL.md"),
        Some(std::path::Path::new("/core/SKILL.md"))
    );
}

#[test]
fn host_text_links_reject_relative_paths_and_file_uris_but_structured_paths_keep_cwd_resolution() {
    let relative = std::path::Path::new("relative-skill/SKILL.md");
    let absolute = std::env::current_dir().unwrap().join(relative);
    let skill = entry(
        "review",
        SkillSourceKind::Host,
        "host",
        absolute.to_str().unwrap(),
    );
    let catalog = SkillCatalog {
        entries: vec![skill.clone()],
        warnings: vec![],
    };
    for path in [
        relative.to_str().unwrap().to_string(),
        format!("file://{}", absolute.display()),
    ] {
        assert!(
            collect_host_skill_mentions(
                &[SkillInput::Text {
                    text: format!("[$review]({path})")
                }],
                &catalog,
                &std::collections::HashMap::new()
            )
            .is_empty()
        );
    }
    assert_eq!(
        collect_host_skill_mentions(
            &[SkillInput::Skill {
                name: "review".into(),
                path: relative.into()
            }],
            &catalog,
            &std::collections::HashMap::new()
        ),
        vec![skill]
    );
}

#[test]
fn host_text_and_structured_paths_expand_home_before_absolute_matching() {
    let home = std::env::home_dir().expect("home directory for the native Host path contract");
    let skill = entry(
        "review",
        SkillSourceKind::Host,
        "host",
        home.join("fixture/SKILL.md").to_str().unwrap(),
    );
    let catalog = SkillCatalog {
        entries: vec![skill.clone()],
        warnings: vec![],
    };
    for inputs in [
        vec![SkillInput::Text {
            text: "[$review](~/fixture/SKILL.md)".into(),
        }],
        vec![SkillInput::Skill {
            name: "review".into(),
            path: "~/fixture/SKILL.md".into(),
        }],
    ] {
        assert_eq!(
            collect_host_skill_mentions(&inputs, &catalog, &std::collections::HashMap::new()),
            vec![skill.clone()]
        );
    }
    assert!(
        collect_host_skill_mentions(
            &[SkillInput::Text {
                text: "[$review](~another-user/fixture/SKILL.md)".into()
            }],
            &catalog,
            &std::collections::HashMap::new()
        )
        .is_empty()
    );
}
