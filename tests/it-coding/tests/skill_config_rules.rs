// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Ordered Host enablement, catalog retention, and the allowlist extension.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use ra_coding::{
    CodingHost, CodingProfile,
    skills::{
        PluginSkillRoot, SkillConfig, SkillConfigRule, SkillConfigRuleSelector, SkillConfigRules,
        SkillDiscoveryConfig, SkillProduct,
    },
};
use ra_core::{
    agent::AgentId, capability::CapabilityFamily, context::RunContext, prompt::PromptRole,
    state::RunId,
};
use ra_tools::skills_ns::catalog::SkillCatalog;
use rstest::rstest;

fn name_rule(name: &str, enabled: bool) -> SkillConfigRule {
    SkillConfigRule {
        selector: SkillConfigRuleSelector::Name(name.to_owned()),
        enabled,
    }
}

fn path_rule(path: &Path, enabled: bool) -> SkillConfigRule {
    SkillConfigRule {
        selector: SkillConfigRuleSelector::Path(path.to_path_buf()),
        enabled,
    }
}

fn path_config(path: &Path, enabled: bool) -> SkillConfig {
    SkillConfig {
        path: Some(path.to_path_buf()),
        name: None,
        enabled,
    }
}

fn name_config(name: &str, enabled: bool) -> SkillConfig {
    SkillConfig {
        path: None,
        name: Some(name.to_owned()),
        enabled,
    }
}

fn write_skill(root: &Path, relative: &str, name: &str) -> PathBuf {
    let path = root.join(relative).join("SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("---\nname: {name}\ndescription: Help with {name}.\n---\nPRIVATE BODY"),
    )
    .unwrap();
    fs::canonicalize(path).unwrap()
}

fn states(catalog: &SkillCatalog) -> Vec<(&str, bool, bool)> {
    catalog
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.enabled, entry.prompt_visible))
        .collect()
}

#[test]
fn path_rule_disables_selected_path_even_without_a_loaded_skill() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("disable-by-path/SKILL.md");
    let rules = SkillConfigRules {
        entries: vec![path_rule(&path, false)],
    };

    assert_eq!(
        rules.resolve_disabled_paths(std::iter::empty()),
        HashSet::from([path])
    );
}

#[test]
fn later_name_rule_reenables_path_disabled_skill() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("reenable-by-name/SKILL.md");
    let rules = SkillConfigRules {
        entries: vec![path_rule(&path, false), name_rule("demo", true)],
    };

    assert!(rules.resolve_disabled_paths([("demo", &path)]).is_empty());
}

#[test]
fn later_path_rule_reenables_one_skill_disabled_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let first_path = temp.path().join("first/SKILL.md");
    let second_path = temp.path().join("second/SKILL.md");
    let rules = SkillConfigRules {
        entries: vec![name_rule("demo", false), path_rule(&first_path, true)],
    };

    assert_eq!(
        rules.resolve_disabled_paths([("demo", &first_path), ("demo", &second_path)]),
        HashSet::from([second_path])
    );
}

#[rstest]
#[case(false)]
#[case(true)]
fn repeated_selectors_follow_the_last_matching_rule(#[case] last_enabled: bool) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("demo/SKILL.md");
    for selector in [
        SkillConfigRuleSelector::Name("demo".into()),
        SkillConfigRuleSelector::Path(path.clone()),
    ] {
        let rules = SkillConfigRules {
            entries: vec![
                SkillConfigRule {
                    selector: selector.clone(),
                    enabled: !last_enabled,
                },
                SkillConfigRule {
                    selector,
                    enabled: last_enabled,
                },
            ],
        };
        assert_eq!(
            rules
                .resolve_disabled_paths([("demo", &path)])
                .contains(&path),
            !last_enabled
        );
    }
}

#[test]
fn effective_names_match_exactly_and_default_rules_disable_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("demo/SKILL.md");
    let skills = [("plugin:demo", &path)];
    assert!(
        SkillConfigRules::default()
            .resolve_disabled_paths(skills)
            .is_empty()
    );
    for name in ["demo", "Plugin:demo", "plugin:*", " plugin:demo "] {
        let rules = SkillConfigRules {
            entries: vec![name_rule(name, false)],
        };
        assert!(rules.resolve_disabled_paths(skills).is_empty(), "{name}");
    }
    let rules = SkillConfigRules {
        entries: vec![name_rule("plugin:demo", false)],
    };
    assert_eq!(rules.resolve_disabled_paths(skills), HashSet::from([path]));
}

#[tokio::test]
async fn discovery_keeps_disabled_plugin_entries_and_applies_the_allowlist_after_rules() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), ".rusty-agent/skills/review", "review");
    write_skill(temp.path(), ".rusty-agent/skills/excluded", "excluded");
    write_skill(temp.path(), "plugin/skills/review", "review");
    let config = SkillDiscoveryConfig::new()
        .with_plugin_root(
            PluginSkillRoot::new(
                temp.path().join("plugin/skills"),
                temp.path().join("plugin"),
                "acme",
            )
            .unwrap(),
        )
        .with_config_rules(SkillConfigRules {
            entries: vec![name_rule("acme:review", false), name_rule("excluded", true)],
        });
    let catalog = config.discover(temp.path()).await.unwrap();
    assert!(catalog.warnings.is_empty());
    assert_eq!(
        states(&catalog),
        [
            ("excluded", true, true),
            ("review", true, true),
            ("acme:review", false, true)
        ]
    );
    let filtered = config
        .clone()
        .with_allowlist(["review", "acme:review"])
        .discover(temp.path())
        .await
        .unwrap();
    assert_eq!(
        states(&filtered),
        [("review", true, true), ("acme:review", false, true)]
    );
    assert!(
        config
            .with_allowlist(std::iter::empty::<String>())
            .discover(temp.path())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn discovery_applies_name_rules_to_all_paths_and_path_overrides_to_one_document() {
    let temp = tempfile::tempdir().unwrap();
    let first = write_skill(temp.path(), ".rusty-agent/skills/first", "demo");
    write_skill(temp.path(), ".rusty-agent/skills/second", "demo");
    write_skill(temp.path(), ".rusty-agent/skills/other", "other");
    let config = SkillDiscoveryConfig::new().with_config_rules(SkillConfigRules {
        entries: vec![name_rule("demo", false), path_rule(&first, true)],
    });
    assert_eq!(
        states(&config.discover(temp.path()).await.unwrap()),
        [
            ("demo", true, true),
            ("demo", false, true),
            ("other", true, true)
        ]
    );
    assert_eq!(
        states(
            &config
                .with_config_rules(SkillConfigRules {
                    entries: vec![name_rule("demo", true), path_rule(&first, false)],
                })
                .discover(temp.path())
                .await
                .unwrap()
        ),
        [
            ("demo", false, true),
            ("demo", true, true),
            ("other", true, true)
        ]
    );
}

#[tokio::test]
async fn rebinding_rules_does_not_reuse_another_hosts_enablement_for_the_same_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_skill(temp.path(), ".rusty-agent/skills/demo", "demo");
    let disabled = SkillDiscoveryConfig::new().with_config_rules(SkillConfigRules {
        entries: vec![path_rule(&path, false)],
    });
    let reenabled = disabled.clone().with_config_rules(SkillConfigRules {
        entries: vec![path_rule(&path, false), name_rule("demo", true)],
    });
    for (config, expected_enabled) in [(&disabled, false), (&reenabled, true), (&disabled, false)] {
        let host = CodingHost::open(temp.path())
            .unwrap()
            .with_skill_discovery(config.clone());
        let built = ra_coding::build_agent_with_control(
            AgentId::new("coding"),
            "Coding",
            &PromptRole::Main,
            &host,
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
        let instructions = capability
            .bind(&run)
            .unwrap()
            .unwrap()
            .instructions()
            .await
            .unwrap();
        assert_eq!(
            instructions
                .as_ref()
                .is_some_and(|instructions| instructions.content().contains("- demo:")),
            expected_enabled
        );
    }
}

#[tokio::test]
async fn rule_enablement_preserves_policy_product_filters_and_prompt_visibility() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["visible", "disabled", "manual", "restricted", "excluded"] {
        write_skill(temp.path(), &format!(".rusty-agent/skills/{name}"), name);
    }
    for (name, policy) in [
        ("manual", "allow_implicit_invocation: false"),
        ("restricted", "products: [CODEX]"),
    ] {
        let directory = temp
            .path()
            .join(format!(".rusty-agent/skills/{name}/agents"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("openai.yaml"),
            format!("policy:\n  {policy}\n"),
        )
        .unwrap();
    }
    let config = SkillDiscoveryConfig::new()
        .with_config_rules(SkillConfigRules {
            entries: vec![
                name_rule("disabled", false),
                name_rule("manual", false),
                name_rule("manual", true),
                name_rule("restricted", true),
                name_rule("excluded", true),
            ],
        })
        .with_allowlist(["visible", "disabled", "manual", "restricted"]);
    assert_eq!(
        states(&config.discover(temp.path()).await.unwrap()),
        [
            ("disabled", false, true),
            ("manual", true, false),
            ("visible", true, true)
        ]
    );
    assert_eq!(
        states(
            &config
                .clone()
                .with_restriction_product(SkillProduct::Codex)
                .discover(temp.path())
                .await
                .unwrap()
        ),
        [
            ("disabled", false, true),
            ("manual", true, false),
            ("restricted", true, true),
            ("visible", true, true)
        ]
    );
    let host = CodingHost::open(temp.path())
        .unwrap()
        .with_skill_discovery(config);
    let built = ra_coding::build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
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
    let instructions = capability
        .bind(&run)
        .unwrap()
        .unwrap()
        .instructions()
        .await
        .unwrap()
        .unwrap();
    let prompt = instructions.content();
    assert!(prompt.contains("- visible:"), "{prompt}");
    for hidden in ["disabled", "manual", "restricted", "excluded"] {
        assert!(!prompt.contains(&format!("- {hidden}:")), "{prompt}");
    }
    assert!(!prompt.contains("PRIVATE BODY"));
}

#[cfg(unix)]
#[test]
fn path_rules_preserve_distinct_non_utf8_native_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    // Some local filesystems reject these bytes, so exercise native path identity without IO.
    let first = PathBuf::from(OsString::from_vec(b"/skills/native-\xff/SKILL.md".to_vec()));
    let second = PathBuf::from(OsString::from_vec(b"/skills/native-\xfe/SKILL.md".to_vec()));
    assert_eq!(first.to_string_lossy(), second.to_string_lossy());
    let rules = SkillConfigRules {
        entries: vec![path_rule(&first, false)],
    };
    assert_eq!(
        rules.resolve_disabled_paths([("native", &first), ("native", &second)]),
        HashSet::from([first])
    );
}

// Codex reads these entries from user and session TOML layers. The layers are concatenated here,
// lowest precedence first, which is the input order its loader applies across layers.
#[test]
fn later_configuration_entries_reenable_or_disable_the_same_path() {
    let temp = tempfile::tempdir().unwrap();
    let skill_path = temp.path().join("skills/demo/SKILL.md");
    for enabled in [true, false] {
        assert_eq!(
            SkillConfigRules::from_configs([
                path_config(&skill_path, !enabled),
                path_config(&skill_path, enabled),
            ]),
            SkillConfigRules {
                entries: vec![path_rule(&skill_path, enabled)],
            }
        );
    }
}

#[test]
fn configuration_preserves_name_selectors() {
    assert_eq!(
        SkillConfigRules::from_configs([name_config("github:yeet", false)]),
        SkillConfigRules {
            entries: vec![name_rule("github:yeet", false)],
        }
    );
}

#[test]
fn configuration_preserves_order_across_path_and_name_selectors() {
    let temp = tempfile::tempdir().unwrap();
    let skill_path = temp.path().join("skills/demo/SKILL.md");
    assert_eq!(
        SkillConfigRules::from_configs([
            path_config(&skill_path, false),
            name_config("github:yeet", true),
        ]),
        SkillConfigRules {
            entries: vec![
                path_rule(&skill_path, false),
                name_rule("github:yeet", true)
            ],
        }
    );
}

#[test]
fn configuration_trims_names_skips_invalid_entries_and_moves_repeated_selectors() {
    let temp = tempfile::tempdir().unwrap();
    let skill_path = temp.path().join("skills/demo/SKILL.md");
    let invalid = [
        name_config("  ", false),
        SkillConfig {
            path: Some(skill_path.clone()),
            name: Some("demo".into()),
            enabled: false,
        },
        SkillConfig {
            path: None,
            name: None,
            enabled: false,
        },
        path_config(Path::new("skills/demo/SKILL.md"), false),
    ];
    assert_eq!(
        SkillConfigRules::from_configs(invalid.clone()),
        SkillConfigRules::default()
    );
    assert_eq!(
        SkillConfigRules::from_configs(
            [
                name_config(" demo ", false),
                path_config(&skill_path, false),
                name_config("demo", true),
            ]
            .into_iter()
            .chain(invalid)
        ),
        SkillConfigRules {
            entries: vec![path_rule(&skill_path, false), name_rule("demo", true)],
        }
    );
}

#[cfg(unix)]
#[tokio::test]
async fn configuration_canonicalizes_existing_paths_before_discovery_matches_them() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let canonical = write_skill(temp.path(), ".rusty-agent/skills/demo", "demo");
    symlink(
        temp.path().join(".rusty-agent/skills/demo"),
        temp.path().join("alias"),
    )
    .unwrap();
    let aliased = temp.path().join("alias/./SKILL.md");
    assert_ne!(aliased, canonical);
    let rules = SkillConfigRules::from_configs([path_config(&aliased, false)]);
    assert_eq!(
        rules,
        SkillConfigRules {
            entries: vec![path_rule(&canonical, false)],
        }
    );
    let catalog = SkillDiscoveryConfig::new()
        .with_config_rules(rules)
        .discover(temp.path())
        .await
        .unwrap();
    assert_eq!(states(&catalog), [("demo", false, true)]);
}

#[test]
fn configuration_entries_deserialize_with_optional_selectors() {
    let entries: Vec<SkillConfig> = serde_json::from_str(
        r#"[{"name":"acme:review","enabled":false},{"path":"/skills/demo/SKILL.md","enabled":true}]"#,
    )
    .unwrap();
    assert_eq!(
        entries,
        [
            name_config("acme:review", false),
            path_config(Path::new("/skills/demo/SKILL.md"), true),
        ]
    );
    assert_eq!(
        serde_json::to_value(&entries[0]).unwrap(),
        serde_json::json!({"name":"acme:review","enabled":false})
    );
    assert!(serde_json::from_str::<SkillConfig>(r#"{"name":"demo"}"#).is_err());
}
