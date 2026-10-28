// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use anyhow::Result;
use ra_config::AllowDenyRequirementToml;
use ra_config::BrowserUseAccessApprovalLifetimeToml;
use ra_config::BrowserUseOriginPolicyToml;
use ra_config::ComputerUseMacosRequirementsToml;
use ra_config::ComputerUseWindowsExeRequirementToml;
use ra_config::ComputerUseWindowsRequirementsToml;
use ra_config::HookEventsToml;
use ra_config::McpServerCommandMatcher;
use ra_config::McpServerIdentity;
use ra_config::McpServerValueMatcher;
use ra_config::requirements_value_test_api::merge_app_requirements_descending;
use ra_config::*;
use ra_execpolicy::Decision;
use ra_execpolicy::Evaluation;
use ra_execpolicy::RuleMatch;
use ra_filesystem::models::PermissionProfile;
use ra_filesystem::permissions::NetworkSandboxPolicy;
use ra_paths::AbsolutePathBuf;
use ra_paths::AbsolutePathBufGuard;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use toml::from_str;

fn tokens(cmd: &[&str]) -> Vec<String> {
    cmd.iter().map(std::string::ToString::to_string).collect()
}

fn system_requirements_toml_file_for_test() -> Result<AbsolutePathBuf> {
    Ok(AbsolutePathBuf::try_from(
        std::env::temp_dir().join("requirements.toml"),
    )?)
}

#[test]
fn exact_requirement_for_config_path_matches_overlapping_paths() {
    let managed_path = AbsolutePathBuf::try_from(std::env::temp_dir().join("managed"))
        .expect("managed path should be absolute");
    let requirements = ConfigRequirementsToml {
        sqlite_home: Some(managed_path.clone()),
        log_dir: Some(managed_path.clone()),
        model_catalog_json: Some(managed_path),
        check_for_update_on_startup: Some(false),
        allow_login_shell: Some(false),
        feedback: Some(FeedbackConfigToml {
            enabled: Some(false),
        }),
        ..Default::default()
    };
    let cases: &[(&[&str], Option<&str>)] = &[
        (&["sqlite_home"], Some("sqlite_home")),
        (&["log_dir"], Some("log_dir")),
        (&["model_catalog_json"], Some("model_catalog_json")),
        (
            &["check_for_update_on_startup"],
            Some("check_for_update_on_startup"),
        ),
        (&["allow_login_shell"], Some("allow_login_shell")),
        (&["feedback", "enabled"], Some("feedback.enabled")),
        (&[], Some("sqlite_home")),
        (&["feedback"], Some("feedback.enabled")),
        (&["feedback", "other"], None),
        (&["windows", "sandbox"], None),
    ];

    for (segments, expected) in cases {
        let segments = segments.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            requirements.exact_requirement_for_config_path(&segments),
            *expected,
            "segments: {segments:?}"
        );
    }
}

#[test]
fn composite_requirement_source_flattens_and_deduplicates_sources() {
    let mdm_source = RequirementSource::MdmManagedPreferences {
        domain: "com.openai.codex".to_string(),
        key: "requirements_toml_base64".to_string(),
    };
    let legacy_source = RequirementSource::LegacyManagedConfigTomlFromMdm;

    assert_eq!(
        RequirementSource::composite([
            mdm_source.clone(),
            RequirementSource::composite([legacy_source.clone(), mdm_source.clone()]),
        ]),
        RequirementSource::Composite {
            sources: vec![mdm_source, legacy_source],
        }
    );
}

fn with_unknown_source(toml: ConfigRequirementsToml) -> ConfigRequirementsWithSources {
    let ConfigRequirementsToml {
        sqlite_home,
        log_dir,
        model_catalog_json,
        model_provider,
        model_providers,
        check_for_update_on_startup,
        allow_login_shell,
        feedback,
        allowed_approval_policies,
        allowed_approvals_reviewers,
        allowed_sandbox_modes,
        allowed_permission_profiles,
        default_permissions,
        remote_sandbox_config: _,
        allowed_web_search_modes,
        allow_managed_hooks_only,
        allow_browser_and_computer_use,
        allow_appshots,
        allow_remote_control,
        computer_use,
        browser_use,
        in_app_browser,
        windows,
        feature_requirements,
        hooks,
        mcp_servers,
        plugins,
        marketplaces,
        apps,
        rules,
        enforce_residency,
        network,
        application,
        permissions,
        auto_review,
        models,
        additional_developer_instructions,
        guardian_policy_config,
        guardian_extra_policy,
    } = toml;
    ConfigRequirementsWithSources {
        sqlite_home: sqlite_home.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        log_dir: log_dir.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        model_catalog_json: model_catalog_json
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        model_provider: model_provider.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        model_providers: model_providers
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        check_for_update_on_startup: check_for_update_on_startup
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allow_login_shell: allow_login_shell
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        feedback: feedback.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allowed_approval_policies: allowed_approval_policies
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allowed_approvals_reviewers: allowed_approvals_reviewers
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allowed_sandbox_modes: allowed_sandbox_modes
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allowed_permission_profiles: allowed_permission_profiles
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        default_permissions: default_permissions
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allowed_web_search_modes: allowed_web_search_modes
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allow_managed_hooks_only: allow_managed_hooks_only
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allow_browser_and_computer_use: allow_browser_and_computer_use
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allow_appshots: allow_appshots.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        allow_remote_control: allow_remote_control
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        computer_use: computer_use.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        browser_use: browser_use.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        in_app_browser: in_app_browser.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        windows: windows.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        feature_requirements: feature_requirements
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        hooks: hooks.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        mcp_servers: mcp_servers.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        plugins: plugins.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        marketplaces: marketplaces.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        apps: apps.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        rules: rules.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        enforce_residency: enforce_residency
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        network: network.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        application: application.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        permissions: permissions.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        auto_review: auto_review.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        models: models.map(|value| Sourced::new(value, RequirementSource::Unknown)),
        additional_developer_instructions: additional_developer_instructions
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        guardian_policy_config: guardian_policy_config
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
        guardian_extra_policy: guardian_extra_policy
            .map(|value| Sourced::new(value, RequirementSource::Unknown)),
    }
}

#[test]
fn deserialize_allow_managed_hooks_only() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                allow_managed_hooks_only = true
            "#,
    )?;

    assert_eq!(requirements.allow_managed_hooks_only, Some(true));
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn allow_managed_hooks_only_false_is_still_configured() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                allow_managed_hooks_only = false
            "#,
    )?;

    assert_eq!(requirements.allow_managed_hooks_only, Some(false));
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn deserialize_managed_permission_profiles() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                default_permissions = "managed-standard"

                [allowed_permission_profiles]
                managed-standard = true
                managed-build = true

                [permissions.managed-standard]
                extends = ":workspace"

                [permissions.managed-build]
                extends = "managed-standard"
            "#,
    )?;

    assert_eq!(
        requirements.allowed_permission_profiles,
        Some(BTreeMap::from([
            ("managed-build".to_string(), true),
            ("managed-standard".to_string(), true),
        ]))
    );
    assert_eq!(
        requirements.default_permissions,
        Some("managed-standard".to_string())
    );
    let permissions = requirements
        .permissions
        .as_ref()
        .expect("managed permission profiles");
    assert!(permissions.profiles.contains_key("managed-standard"));
    assert!(
        permissions
            .profiles
            .get("managed-build")
            .and_then(|profile| profile.extends.as_deref())
            .is_some()
    );
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn deserialize_allow_appshots() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                allow_appshots = true
            "#,
    )?;

    assert_eq!(requirements.allow_appshots, Some(true));
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn filesystem_requirements_table_cannot_define_a_permission_profile() {
    let err = from_str::<ConfigRequirementsToml>(
        r#"
                [permissions.filesystem]
                extends = ":workspace"
            "#,
    )
    .expect_err("filesystem requirements cannot define a permission profile");

    assert!(
            err.to_string().contains(
                "`permissions.filesystem` is reserved for requirements-level filesystem constraints and cannot define a profile"
            ),
            "unexpected error: {err:#}"
        );
}

#[test]
fn allow_appshots_false_is_still_configured() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                allow_appshots = false
            "#,
    )?;

    assert_eq!(requirements.allow_appshots, Some(false));
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn allow_remote_control_false_is_still_configured() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                allow_remote_control = false
            "#,
    )?;

    assert_eq!(requirements.allow_remote_control, Some(false));
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn deserialize_browser_and_computer_use_requirements() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                allow_browser_and_computer_use = false

                [browser_use]
                allow_history_access = false
                disable_auto_review = true
                allow_global_persistent_approval = false

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
                publisher_name = "CN=Google LLC, O=Google LLC, L=Mountain View, S=California, C=US"
                product_name = "Google Chrome"
                binary_name = "chrome.exe"
                access = "deny"
            "#,
    )?;

    assert_eq!(requirements.allow_browser_and_computer_use, Some(false));
    assert_eq!(
        requirements.browser_use,
        Some(BrowserUseRequirementsToml {
            extension: None,
            allow_webmcp: None,
            allow_history_access: Some(false),
            disable_auto_review: Some(true),
            allow_global_persistent_approval: Some(false),
            default_origin_policy: Some(BrowserUseOriginPolicyToml {
                access: Some(AllowDenyRequirementToml::Deny),
                downloads: Some(AllowDenyRequirementToml::Allow),
                uploads: Some(AllowDenyRequirementToml::Deny),
                full_cdp_access: Some(AllowDenyRequirementToml::Allow),
                auto_review: Some(AllowDenyRequirementToml::Deny),
                persistent_approval: Some(false),
                access_approval_lifetime: Some(BrowserUseAccessApprovalLifetimeToml::Turn),
            }),
            origins: Some(BTreeMap::from([(
                "https://example.com".to_string(),
                BrowserUseOriginPolicyToml {
                    access: Some(AllowDenyRequirementToml::Allow),
                    downloads: Some(AllowDenyRequirementToml::Deny),
                    uploads: Some(AllowDenyRequirementToml::Allow),
                    full_cdp_access: Some(AllowDenyRequirementToml::Deny),
                    auto_review: Some(AllowDenyRequirementToml::Deny),
                    persistent_approval: Some(true),
                    access_approval_lifetime: Some(BrowserUseAccessApprovalLifetimeToml::Thread,),
                },
            )])),
        })
    );
    assert_eq!(
        requirements.computer_use,
        Some(ComputerUseRequirementsToml {
            allow_locked_computer_use: Some(false),
            allow_persistent_approval: Some(false),
            default_app_access: Some(AllowDenyRequirementToml::Deny),
            macos: Some(ComputerUseMacosRequirementsToml {
                bundle_ids: Some(BTreeMap::from([(
                    "com.apple.Safari".to_string(),
                    AllowDenyRequirementToml::Allow,
                )])),
            }),
            windows: Some(ComputerUseWindowsRequirementsToml {
                aumids: Some(BTreeMap::from([(
                    "Microsoft.Paint_8wekyb3d8bbwe!App".to_string(),
                    AllowDenyRequirementToml::Allow,
                )])),
                exes: Some(vec![ComputerUseWindowsExeRequirementToml {
                    publisher_name:
                        "CN=Google LLC, O=Google LLC, L=Mountain View, S=California, C=US"
                            .to_string(),
                    product_name: "Google Chrome".to_string(),
                    binary_name: Some("chrome.exe".to_string()),
                    access: AllowDenyRequirementToml::Deny,
                }]),
            }),
        })
    );
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn browser_and_computer_use_leaf_requirements_are_not_empty() -> Result<()> {
    for (name, requirements_toml) in [
        (
            "browser history",
            "[browser_use]\nallow_history_access = false",
        ),
        (
            "browser auto-review",
            "[browser_use]\ndisable_auto_review = true",
        ),
        (
            "global persistent approval",
            "[browser_use]\nallow_global_persistent_approval = false",
        ),
        (
            "default origin access",
            "[browser_use.default_origin_policy]\naccess = \"deny\"",
        ),
        (
            "default origin downloads",
            "[browser_use.default_origin_policy]\ndownloads = \"deny\"",
        ),
        (
            "default origin uploads",
            "[browser_use.default_origin_policy]\nuploads = \"deny\"",
        ),
        (
            "default origin full CDP access",
            "[browser_use.default_origin_policy]\nfull_cdp_access = \"deny\"",
        ),
        (
            "default origin auto-review",
            "[browser_use.default_origin_policy]\nauto_review = \"deny\"",
        ),
        (
            "default origin persistent approval",
            "[browser_use.default_origin_policy]\npersistent_approval = false",
        ),
        (
            "default origin access approval lifetime",
            "[browser_use.default_origin_policy]\naccess_approval_lifetime = \"turn\"",
        ),
        (
            "origin access",
            "[browser_use.origins.\"https://example.com\"]\naccess = \"deny\"",
        ),
        (
            "origin downloads",
            "[browser_use.origins.\"https://example.com\"]\ndownloads = \"deny\"",
        ),
        (
            "origin uploads",
            "[browser_use.origins.\"https://example.com\"]\nuploads = \"deny\"",
        ),
        (
            "origin full CDP access",
            "[browser_use.origins.\"https://example.com\"]\nfull_cdp_access = \"deny\"",
        ),
        (
            "origin auto-review",
            "[browser_use.origins.\"https://example.com\"]\nauto_review = \"deny\"",
        ),
        (
            "origin persistent approval",
            "[browser_use.origins.\"https://example.com\"]\npersistent_approval = false",
        ),
        (
            "origin access approval lifetime",
            "[browser_use.origins.\"https://example.com\"]\naccess_approval_lifetime = \"turn\"",
        ),
        (
            "computer persistent approval",
            "[computer_use]\nallow_persistent_approval = false",
        ),
        (
            "default app access",
            "[computer_use]\ndefault_app_access = \"deny\"",
        ),
        (
            "macOS bundle identifier",
            "[computer_use.macos.bundle_ids]\n\"com.example.App\" = \"deny\"",
        ),
        (
            "Windows AUMID",
            "[computer_use.windows.aumids]\n\"Example.App_123!Main\" = \"deny\"",
        ),
        (
            "Windows executable",
            "[[computer_use.windows.exes]]\npublisher_name = \"CN=Example Corp\"\nproduct_name = \"Example App\"\naccess = \"deny\"",
        ),
    ] {
        let requirements: ConfigRequirementsToml = from_str(requirements_toml)?;
        assert!(!requirements.is_empty(), "{name} requirement was dropped");
    }

    Ok(())
}

#[test]
fn deserialize_new_thread_model_defaults() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
                [models.new_thread]
                model = "managed-model"
                model_reasoning_effort = "medium"
                service_tier = "fast"
            "#,
    )?;

    assert_eq!(
        requirements.models,
        Some(ModelsRequirementsToml {
            new_thread: Some(NewThreadModelDefaultsToml {
                model: Some("managed-model".to_string()),
                model_reasoning_effort: Some(ReasoningEffort::Medium),
                service_tier: Some("fast".to_string()),
            }),
        })
    );
    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn auto_review_required_for_model_matches_exact_provider_aliases() {
    let requirements = ConfigRequirements {
        auto_review_required_models: Some(Sourced::new(
            BTreeSet::from(["protected-model".to_string()]),
            RequirementSource::Unknown,
        )),
        ..Default::default()
    };

    for (model, protected) in [
        ("protected-model", true),
        ("protected-model-preview", false),
        ("openai-codex/protected-model-preview", false),
        ("provider_1/protected-model", true),
        ("protected-modelish", false),
        ("/protected-model", false),
        ("bad.provider/protected-model", false),
        ("provider/nested/protected-model", false),
    ] {
        assert_eq!(
            requirements.auto_review_required_for_model(model),
            protected,
            "{model}"
        );
    }
}

#[test]
fn merge_unset_fields_copies_every_field_and_sets_sources() {
    let mut target = ConfigRequirementsWithSources::default();
    let source = RequirementSource::LegacyManagedConfigTomlFromMdm;

    let allowed_approval_policies = vec![AskForApproval::UnlessTrusted, AskForApproval::Never];
    let allowed_approvals_reviewers = vec![ApprovalsReviewer::AutoReview, ApprovalsReviewer::User];
    let allowed_sandbox_modes = vec![
        SandboxModeRequirement::WorkspaceWrite,
        SandboxModeRequirement::DangerFullAccess,
    ];
    let allowed_web_search_modes = vec![
        WebSearchModeRequirement::Cached,
        WebSearchModeRequirement::Live,
    ];
    let feature_requirements = FeatureRequirementsToml {
        entries: BTreeMap::from([("personality".to_string(), true)]),
    };
    let browser_use = BrowserUseRequirementsToml {
        extension: None,
        allow_webmcp: None,
        allow_history_access: Some(false),
        disable_auto_review: Some(true),
        allow_global_persistent_approval: None,
        default_origin_policy: None,
        origins: None,
    };
    let computer_use = ComputerUseRequirementsToml {
        allow_locked_computer_use: Some(false),
        allow_persistent_approval: Some(false),
        default_app_access: None,
        macos: None,
        windows: None,
    };
    let auto_review = AutoReviewRequirementsToml {
        required_on_models: Some(vec!["managed-model".to_string()]),
        ignore_rules: None,
    };
    let models = ModelsRequirementsToml {
        new_thread: Some(NewThreadModelDefaultsToml {
            model: Some("managed-model".to_string()),
            model_reasoning_effort: Some(ReasoningEffort::Medium),
            service_tier: Some("fast".to_string()),
        }),
    };
    let sqlite_home = AbsolutePathBuf::try_from(std::env::temp_dir().join("managed-state"))
        .expect("managed sqlite home should be absolute");
    let log_dir = AbsolutePathBuf::try_from(std::env::temp_dir().join("managed-logs"))
        .expect("managed log dir should be absolute");
    let model_catalog_json =
        AbsolutePathBuf::try_from(std::env::temp_dir().join("managed-models.json"))
            .expect("managed model catalog path should be absolute");
    let feedback = FeedbackConfigToml {
        enabled: Some(false),
    };
    let windows = WindowsRequirementsToml {
        allowed_sandbox_implementations: Some(vec![WindowsSandboxImplementationToml::Elevated]),
        allow_mxc: None,
        require_mxc: Some(true),
    };
    let enforce_residency = ResidencyRequirement::Us;
    let enforce_source = source.clone();
    let additional_developer_instructions = "Follow the company policy.".to_string();
    let guardian_policy_config = "Use the company-managed guardian policy.".to_string();
    let guardian_extra_policy = "Use the company-managed extra policy.".to_string();

    // Intentionally constructed without `..Default::default()` so adding a new field to
    // `ConfigRequirementsToml` forces this test to be updated.
    let other = ConfigRequirementsToml {
        sqlite_home: Some(sqlite_home.clone()),
        log_dir: Some(log_dir.clone()),
        model_catalog_json: Some(model_catalog_json.clone()),
        model_provider: Some("gateway".to_string()),
        model_providers: Some(HashMap::new()),
        check_for_update_on_startup: Some(false),
        allow_login_shell: Some(false),
        feedback: Some(feedback.clone()),
        allowed_approval_policies: Some(allowed_approval_policies.clone()),
        allowed_approvals_reviewers: Some(allowed_approvals_reviewers.clone()),
        allowed_sandbox_modes: Some(allowed_sandbox_modes.clone()),
        allowed_permission_profiles: Some(BTreeMap::from([("managed".to_string(), true)])),
        default_permissions: Some("managed".to_string()),
        remote_sandbox_config: None,
        allowed_web_search_modes: Some(allowed_web_search_modes.clone()),
        allow_managed_hooks_only: Some(true),
        allow_browser_and_computer_use: Some(false),
        allow_appshots: Some(false),
        allow_remote_control: Some(false),
        computer_use: Some(computer_use.clone()),
        browser_use: Some(browser_use.clone()),
        in_app_browser: None,
        windows: Some(windows.clone()),
        feature_requirements: Some(feature_requirements.clone()),
        hooks: None,
        mcp_servers: None,
        plugins: None,
        marketplaces: None,
        apps: None,
        rules: None,
        enforce_residency: Some(enforce_residency),
        network: None,
        application: None,
        permissions: None,
        auto_review: Some(auto_review.clone()),
        models: Some(models.clone()),
        additional_developer_instructions: Some(additional_developer_instructions.clone()),
        guardian_policy_config: Some(guardian_policy_config.clone()),
        guardian_extra_policy: Some(guardian_extra_policy.clone()),
    };

    target.merge_unset_fields(source.clone(), other);

    assert_eq!(
        target,
        ConfigRequirementsWithSources {
            sqlite_home: Some(Sourced::new(sqlite_home, source.clone())),
            log_dir: Some(Sourced::new(log_dir, source.clone())),
            model_catalog_json: Some(Sourced::new(model_catalog_json, source.clone())),
            model_provider: Some(Sourced::new("gateway".to_string(), source.clone())),
            model_providers: Some(Sourced::new(HashMap::new(), source.clone())),
            check_for_update_on_startup: Some(Sourced::new(/*value*/ false, source.clone(),)),
            allow_login_shell: Some(Sourced::new(/*value*/ false, source.clone())),
            feedback: Some(Sourced::new(feedback, source.clone())),
            allowed_approval_policies: Some(Sourced::new(
                allowed_approval_policies,
                source.clone()
            )),
            allowed_approvals_reviewers: Some(Sourced::new(
                allowed_approvals_reviewers,
                source.clone(),
            )),
            allowed_sandbox_modes: Some(Sourced::new(allowed_sandbox_modes, source.clone(),)),
            allowed_permission_profiles: Some(Sourced::new(
                BTreeMap::from([("managed".to_string(), true)]),
                source.clone(),
            )),
            default_permissions: Some(Sourced::new("managed".to_string(), source.clone(),)),
            allowed_web_search_modes: Some(Sourced::new(
                allowed_web_search_modes,
                enforce_source.clone(),
            )),
            allow_managed_hooks_only: Some(Sourced::new(
                /*value*/ true,
                enforce_source.clone(),
            )),
            allow_browser_and_computer_use: Some(Sourced::new(
                /*value*/ false,
                enforce_source.clone(),
            )),
            allow_appshots: Some(Sourced::new(/*value*/ false, enforce_source.clone(),)),
            allow_remote_control: Some(Sourced::new(/*value*/ false, enforce_source.clone(),)),
            computer_use: Some(Sourced::new(computer_use, enforce_source.clone())),
            browser_use: Some(Sourced::new(browser_use, enforce_source.clone())),
            in_app_browser: None,
            windows: Some(Sourced::new(windows, enforce_source.clone())),
            feature_requirements: Some(Sourced::new(feature_requirements, enforce_source.clone(),)),
            hooks: None,
            mcp_servers: None,
            plugins: None,
            marketplaces: None,
            apps: None,
            rules: None,
            enforce_residency: Some(Sourced::new(enforce_residency, enforce_source)),
            network: None,
            application: None,
            permissions: None,
            auto_review: Some(Sourced::new(auto_review, source.clone())),
            models: Some(Sourced::new(models, source.clone())),
            additional_developer_instructions: Some(Sourced::new(
                additional_developer_instructions,
                source.clone(),
            )),
            guardian_policy_config: Some(Sourced::new(guardian_policy_config, source.clone())),
            guardian_extra_policy: Some(Sourced::new(guardian_extra_policy, source)),
        }
    );
}

#[test]
fn merge_unset_fields_fills_missing_values() -> Result<()> {
    let source: ConfigRequirementsToml = from_str(
        r#"
                allowed_approval_policies = ["on-request"]
            "#,
    )?;

    let source_location = RequirementSource::MdmManagedPreferences {
        domain: "com.codex".to_string(),
        key: "allowed_approval_policies".to_string(),
    };

    let mut empty_target = ConfigRequirementsWithSources::default();
    empty_target.merge_unset_fields(source_location.clone(), source);
    assert_eq!(
        empty_target,
        ConfigRequirementsWithSources {
            allowed_approval_policies: Some(Sourced::new(
                vec![AskForApproval::OnRequest],
                source_location,
            )),
            allowed_approvals_reviewers: None,
            allowed_sandbox_modes: None,
            allowed_permission_profiles: None,
            default_permissions: None,
            allowed_web_search_modes: None,
            allow_managed_hooks_only: None,
            allow_appshots: None,
            allow_remote_control: None,
            computer_use: None,
            browser_use: None,
            in_app_browser: None,
            windows: None,
            feature_requirements: None,
            hooks: None,
            mcp_servers: None,
            plugins: None,
            marketplaces: None,
            apps: None,
            rules: None,
            enforce_residency: None,
            network: None,
            application: None,
            permissions: None,
            models: None,
            guardian_policy_config: None,
            ..Default::default()
        }
    );
    Ok(())
}

#[test]
fn merge_unset_fields_does_not_overwrite_existing_values() -> Result<()> {
    let existing_source = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let mut populated_target = ConfigRequirementsWithSources::default();
    let populated_requirements: ConfigRequirementsToml = from_str(
        r#"
                allowed_approval_policies = ["never"]
                guardian_extra_policy = "Use the managed extra policy."
            "#,
    )?;
    populated_target.merge_unset_fields(existing_source.clone(), populated_requirements);

    let source: ConfigRequirementsToml = from_str(
        r#"
                allowed_approval_policies = ["on-request"]
                guardian_extra_policy = "Use the lower-priority extra policy."
            "#,
    )?;
    let source_location = RequirementSource::MdmManagedPreferences {
        domain: "com.codex".to_string(),
        key: "allowed_approval_policies".to_string(),
    };
    populated_target.merge_unset_fields(source_location, source);

    assert_eq!(
        populated_target,
        ConfigRequirementsWithSources {
            allowed_approval_policies: Some(Sourced::new(
                vec![AskForApproval::Never],
                existing_source.clone(),
            )),
            allowed_approvals_reviewers: None,
            allowed_sandbox_modes: None,
            allowed_permission_profiles: None,
            default_permissions: None,
            allowed_web_search_modes: None,
            allow_managed_hooks_only: None,
            allow_appshots: None,
            allow_remote_control: None,
            computer_use: None,
            browser_use: None,
            in_app_browser: None,
            windows: None,
            feature_requirements: None,
            hooks: None,
            mcp_servers: None,
            plugins: None,
            marketplaces: None,
            apps: None,
            rules: None,
            enforce_residency: None,
            network: None,
            application: None,
            permissions: None,
            models: None,
            guardian_policy_config: None,
            guardian_extra_policy: Some(Sourced::new(
                "Use the managed extra policy.".to_string(),
                existing_source,
            )),
            ..Default::default()
        }
    );
    Ok(())
}

#[test]
fn merge_unset_fields_ignores_blank_guardian_override() {
    let mut target = ConfigRequirementsWithSources::default();
    target.merge_unset_fields(
        RequirementSource::LegacyManagedConfigTomlFromMdm,
        ConfigRequirementsToml {
            guardian_policy_config: Some("   \n\t".to_string()),
            guardian_extra_policy: Some("   \n\t".to_string()),
            ..Default::default()
        },
    );
    let source = RequirementSource::SystemRequirementsToml {
        file: system_requirements_toml_file_for_test().expect("system requirements.toml path"),
    };
    target.merge_unset_fields(
        source.clone(),
        ConfigRequirementsToml {
            guardian_policy_config: Some("Use the system guardian policy.".to_string()),
            guardian_extra_policy: Some("Use the system extra policy.".to_string()),
            ..Default::default()
        },
    );

    assert_eq!(
        target,
        ConfigRequirementsWithSources {
            guardian_policy_config: Some(Sourced::new(
                "Use the system guardian policy.".to_string(),
                source.clone(),
            )),
            guardian_extra_policy: Some(Sourced::new(
                "Use the system extra policy.".to_string(),
                source,
            )),
            ..Default::default()
        },
    );
}

#[test]
fn deserialize_guardian_policy_config() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
guardian_policy_config = """
Use the cloud-managed guardian policy.
"""
guardian_extra_policy = """
Use the cloud-managed extra policy.
"""
"#,
    )?;

    assert_eq!(
        requirements,
        ConfigRequirementsToml {
            guardian_policy_config: Some("Use the cloud-managed guardian policy.\n".to_string()),
            guardian_extra_policy: Some("Use the cloud-managed extra policy.\n".to_string()),
            ..Default::default()
        }
    );
    Ok(())
}

#[test]
fn blank_guardian_policy_config_is_empty() -> Result<()> {
    for key in ["guardian_policy_config", "guardian_extra_policy"] {
        let requirements: ConfigRequirementsToml =
            from_str(&format!("{key} = \"\"\"\n\n\"\"\"\n"))?;
        assert!(requirements.is_empty(), "{key}");

        let requirements: ConfigRequirementsToml =
            from_str(&format!("{key} = \"Use the managed policy.\"\n"))?;
        assert!(!requirements.is_empty(), "{key}");
    }
    Ok(())
}

#[test]
fn allowed_approvals_reviewers_is_not_empty() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str(
        r#"
allowed_approvals_reviewers = ["user"]
"#,
    )?;

    assert!(!requirements.is_empty());
    Ok(())
}

#[test]
fn deserialize_filesystem_deny_read_requirements() -> Result<()> {
    let deny_read_0 = if cfg!(windows) {
        r"C:\Users\alice\.gitconfig"
    } else {
        "/home/alice/.gitconfig"
    };
    let deny_read_1 = if cfg!(windows) {
        r"C:\Users\alice\.ssh"
    } else {
        "/home/alice/.ssh"
    };
    let toml_str = format!(
        r#"
            [permissions.filesystem]
            deny_read = [{deny_read_0:?}, {deny_read_1:?}]
        "#
    );

    let config: ConfigRequirementsToml = from_str(&toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.filesystem,
        Some(Sourced::new(
            FilesystemConstraints {
                deny_read: vec![
                    AbsolutePathBuf::from_absolute_path(deny_read_0)?.into(),
                    AbsolutePathBuf::from_absolute_path(deny_read_1)?.into(),
                ],
            },
            RequirementSource::Unknown,
        ))
    );

    Ok(())
}

#[test]
fn deserialize_filesystem_deny_read_glob_requirements() -> Result<()> {
    let temp_dir = std::env::temp_dir();
    let _guard = AbsolutePathBufGuard::new(&temp_dir);
    let config: ConfigRequirementsToml = from_str(
        r#"
            [permissions.filesystem]
            deny_read = ["./private/**/*.txt"]
        "#,
    )?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.filesystem,
        Some(Sourced::new(
            FilesystemConstraints {
                deny_read: vec![
                    FilesystemDenyReadPattern::from_input("./private/**/*.txt")
                        .expect("normalize glob pattern"),
                ],
            },
            RequirementSource::Unknown,
        ))
    );
    Ok(())
}

#[test]
fn deserialize_apps_requirements() -> Result<()> {
    let toml_str = r#"
            [apps.connector_123123]
            enabled = false
        "#;
    let requirements: ConfigRequirementsToml = from_str(toml_str)?;

    assert_eq!(
        requirements.apps,
        Some(AppsRequirementsToml {
            apps: BTreeMap::from([(
                "connector_123123".to_string(),
                AppRequirementToml {
                    enabled: Some(false),
                    tools: None,
                },
            )]),
        })
    );
    Ok(())
}

#[test]
fn deserialize_apps_tool_requirements() -> Result<()> {
    let toml_str = r#"
            [apps.connector_123123.tools."calendar/list_events"]
            approval_mode = "approve"
        "#;
    let requirements: ConfigRequirementsToml = from_str(toml_str)?;

    assert_eq!(
        requirements.apps,
        Some(AppsRequirementsToml {
            apps: BTreeMap::from([(
                "connector_123123".to_string(),
                AppRequirementToml {
                    enabled: None,
                    tools: Some(AppToolsRequirementsToml {
                        tools: BTreeMap::from([(
                            "calendar/list_events".to_string(),
                            AppToolRequirementToml {
                                approval_mode: Some(AppToolApproval::Approve),
                                analytics_result_source: None,
                            },
                        )]),
                    }),
                },
            )]),
        })
    );
    Ok(())
}

#[test]
fn app_tool_result_source_requirements_parse_and_merge() -> Result<()> {
    let rule = r#"
            [apps.connector_123123.tools."messages/search"]
            analytics_result_source = { format = "detailed_message_search_v1", type = "message_room" }
            "#;
    let requirements: ConfigRequirementsToml = from_str(rule)?;
    assert!(!requirements.is_empty());
    let source = requirements.apps.expect("apps should be present");

    for higher_rule in [
        None,
        Some(("unsupported", "other_resource")),
        Some(("detailed_message_search_v1", "other_resource")),
    ] {
        let mut merged = source.clone();
        merged
            .apps
            .get_mut("connector_123123")
            .expect("app should be present")
            .tools
            .as_mut()
            .expect("tools should be present")
            .tools
            .get_mut("messages/search")
            .expect("tool should be present")
            .analytics_result_source = higher_rule.map(|(format, source_type)| {
            from_str(&format!("format = {format:?}\ntype = {source_type:?}"))
                .expect("complete source rule should parse, including unknown formats")
        });
        let expected = if higher_rule.is_none() {
            source.clone()
        } else {
            merged.clone()
        };

        merge_app_requirements_descending(&mut merged, source.clone());

        assert_eq!(merged, expected);
    }

    for incomplete_rule in [
        r#"format = "detailed_message_search_v1""#,
        r#"type = "message_room""#,
    ] {
        assert!(from_str::<AppToolResultSourceRequirementToml>(incomplete_rule).is_err());
    }
    Ok(())
}

fn apps_requirements(entries: &[(&str, Option<bool>)]) -> AppsRequirementsToml {
    AppsRequirementsToml {
        apps: entries
            .iter()
            .map(|(app_id, enabled)| {
                (
                    (*app_id).to_string(),
                    AppRequirementToml {
                        enabled: *enabled,
                        tools: None,
                    },
                )
            })
            .collect(),
    }
}

fn app_tool_requirements(
    app_id: &str,
    tool_name: &str,
    approval_mode: AppToolApproval,
) -> AppsRequirementsToml {
    AppsRequirementsToml {
        apps: BTreeMap::from([(
            app_id.to_string(),
            AppRequirementToml {
                enabled: None,
                tools: Some(AppToolsRequirementsToml {
                    tools: BTreeMap::from([(
                        tool_name.to_string(),
                        AppToolRequirementToml {
                            approval_mode: Some(approval_mode),
                            analytics_result_source: None,
                        },
                    )]),
                }),
            },
        )]),
    }
}

#[test]
fn merge_app_requirements_descending_unions_distinct_apps() {
    let mut merged = apps_requirements(&[("connector_high", Some(false))]);
    let lower = apps_requirements(&[("connector_low", Some(true))]);

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        apps_requirements(&[
            ("connector_high", Some(false)),
            ("connector_low", Some(true))
        ]),
    );
}

#[test]
fn merge_app_requirements_descending_prefers_false_from_lower_precedence() {
    let mut merged = apps_requirements(&[("connector_123123", Some(true))]);
    let lower = apps_requirements(&[("connector_123123", Some(false))]);

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        apps_requirements(&[("connector_123123", Some(false))]),
    );
}

#[test]
fn merge_app_requirements_descending_keeps_higher_true_when_lower_is_unset() {
    let mut merged = apps_requirements(&[("connector_123123", Some(true))]);
    let lower = apps_requirements(&[("connector_123123", None)]);

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        apps_requirements(&[("connector_123123", Some(true))]),
    );
}

#[test]
fn merge_app_requirements_descending_uses_lower_value_when_higher_missing() {
    let mut merged = apps_requirements(&[]);
    let lower = apps_requirements(&[("connector_123123", Some(true))]);

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        apps_requirements(&[("connector_123123", Some(true))]),
    );
}

#[test]
fn merge_app_requirements_descending_preserves_higher_false_when_lower_missing_app() {
    let mut merged = apps_requirements(&[("connector_123123", Some(false))]);
    let lower = apps_requirements(&[]);

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        apps_requirements(&[("connector_123123", Some(false))]),
    );
}

#[test]
fn merge_app_requirements_descending_preserves_higher_tool_approval_mode() {
    let mut merged = app_tool_requirements(
        "connector_123123",
        "calendar/list_events",
        AppToolApproval::Approve,
    );
    let lower = app_tool_requirements(
        "connector_123123",
        "calendar/list_events",
        AppToolApproval::Prompt,
    );

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        app_tool_requirements(
            "connector_123123",
            "calendar/list_events",
            AppToolApproval::Approve,
        )
    );
}

#[test]
fn merge_app_requirements_descending_uses_lower_tool_approval_when_higher_missing() {
    let mut merged = apps_requirements(&[("connector_123123", None)]);
    let lower = app_tool_requirements(
        "connector_123123",
        "calendar/list_events",
        AppToolApproval::Approve,
    );

    merge_app_requirements_descending(&mut merged, lower);

    assert_eq!(
        merged,
        app_tool_requirements(
            "connector_123123",
            "calendar/list_events",
            AppToolApproval::Approve,
        )
    );
}

#[test]
fn merge_unset_fields_merges_apps_across_sources_with_enabled_evaluation() {
    let higher_source = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let lower_source = RequirementSource::MdmManagedPreferences {
        domain: "com.openai.codex".to_string(),
        key: "requirements_toml_base64".to_string(),
    };
    let mut target = ConfigRequirementsWithSources::default();

    target.merge_unset_fields(
        higher_source.clone(),
        ConfigRequirementsToml {
            apps: Some(apps_requirements(&[
                ("connector_high", Some(true)),
                ("connector_shared", Some(true)),
            ])),
            ..Default::default()
        },
    );
    target.merge_unset_fields(
        lower_source,
        ConfigRequirementsToml {
            apps: Some(apps_requirements(&[
                ("connector_low", Some(false)),
                ("connector_shared", Some(false)),
            ])),
            ..Default::default()
        },
    );

    let apps = target.apps.expect("apps should be present");
    assert_eq!(
        apps.value,
        apps_requirements(&[
            ("connector_high", Some(true)),
            ("connector_low", Some(false)),
            ("connector_shared", Some(false)),
        ])
    );
    assert_eq!(apps.source, higher_source);
}

#[test]
fn merge_unset_fields_apps_empty_higher_source_does_not_block_lower_disables() {
    let mut target = ConfigRequirementsWithSources::default();

    target.merge_unset_fields(
        RequirementSource::LegacyManagedConfigTomlFromMdm,
        ConfigRequirementsToml {
            apps: Some(apps_requirements(&[])),
            ..Default::default()
        },
    );
    target.merge_unset_fields(
        RequirementSource::LegacyManagedConfigTomlFromMdm,
        ConfigRequirementsToml {
            apps: Some(apps_requirements(&[("connector_123123", Some(false))])),
            ..Default::default()
        },
    );

    assert_eq!(
        target.apps.map(|apps| apps.value),
        Some(apps_requirements(&[("connector_123123", Some(false))])),
    );
}

#[test]
fn constraint_error_includes_requirement_source() -> Result<()> {
    let source: ConfigRequirementsToml = from_str(
        r#"
                allowed_approval_policies = ["on-request"]
                allowed_approvals_reviewers = ["auto_review"]
                allowed_sandbox_modes = ["read-only"]
            "#,
    )?;

    let requirements_toml_file = system_requirements_toml_file_for_test()?;
    let source_location = RequirementSource::SystemRequirementsToml {
        file: requirements_toml_file,
    };

    let mut target = ConfigRequirementsWithSources::default();
    target.merge_unset_fields(source_location.clone(), source);
    let requirements = ConfigRequirements::try_from(target)?;

    assert_eq!(
        requirements.approval_policy.can_set(&AskForApproval::Never),
        Err(ConstraintError::InvalidValue {
            field_name: "approval_policy",
            candidate: "Never".into(),
            allowed: "[OnRequest]".into(),
            requirement_source: source_location.clone(),
        })
    );
    assert_eq!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::Disabled),
        Err(ConstraintError::InvalidValue {
            field_name: "sandbox_mode",
            candidate: "DangerFullAccess".into(),
            allowed: "[ReadOnly]".into(),
            requirement_source: source_location.clone(),
        })
    );
    assert_eq!(
        requirements
            .approvals_reviewer
            .can_set(&ApprovalsReviewer::User),
        Err(ConstraintError::InvalidValue {
            field_name: "approvals_reviewer",
            candidate: "User".into(),
            allowed: "[AutoReview]".into(),
            requirement_source: source_location,
        })
    );

    Ok(())
}

#[test]
fn constraint_error_includes_composite_requirement_source() -> Result<()> {
    let source: ConfigRequirementsToml = from_str(
        r#"
                allowed_approval_policies = ["on-request"]
            "#,
    )?;

    let source_location = RequirementSource::composite([
        RequirementSource::MdmManagedPreferences {
            domain: "com.openai.codex".to_string(),
            key: "requirements_toml_base64".to_string(),
        },
        RequirementSource::LegacyManagedConfigTomlFromMdm,
    ]);

    let mut target = ConfigRequirementsWithSources::default();
    target.merge_unset_fields(source_location.clone(), source);
    let requirements = ConfigRequirements::try_from(target)?;

    assert_eq!(
        requirements.approval_policy.can_set(&AskForApproval::Never),
        Err(ConstraintError::InvalidValue {
            field_name: "approval_policy",
            candidate: "Never".into(),
            allowed: "[OnRequest]".into(),
            requirement_source: source_location,
        })
    );

    Ok(())
}

#[test]
fn constrained_fields_store_requirement_source() -> Result<()> {
    let source: ConfigRequirementsToml = from_str(
        r#"
                allowed_approval_policies = ["on-request"]
                allowed_approvals_reviewers = ["auto_review"]
                allowed_sandbox_modes = ["read-only"]
                allowed_web_search_modes = ["cached"]
                enforce_residency = "us"
                [features]
                personality = true
            "#,
    )?;

    let source_location = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let mut target = ConfigRequirementsWithSources::default();
    target.merge_unset_fields(source_location.clone(), source);
    let requirements = ConfigRequirements::try_from(target)?;

    assert_eq!(
        requirements.approval_policy.source,
        Some(source_location.clone())
    );
    assert_eq!(
        requirements.approvals_reviewer.source,
        Some(source_location.clone())
    );
    assert_eq!(
        requirements.permission_profile.source,
        Some(source_location.clone())
    );
    assert_eq!(
        requirements.web_search_mode.source,
        Some(source_location.clone())
    );
    assert_eq!(
        requirements
            .feature_requirements
            .as_ref()
            .map(|requirements| requirements.source.clone()),
        Some(source_location.clone())
    );
    assert_eq!(requirements.enforce_residency.source, Some(source_location));

    Ok(())
}

#[test]
fn deserialize_allowed_approval_policies() -> Result<()> {
    let toml_str = r#"
            allowed_approval_policies = ["on-request", "never"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.approval_policy.value(),
        AskForApproval::OnRequest,
        "currently, there is no way to specify the default value for approval policy in the toml, so it picks the first allowed value"
    );
    assert!(
        requirements
            .approval_policy
            .can_set(&AskForApproval::OnRequest)
            .is_ok()
    );
    assert_eq!(
        requirements
            .approval_policy
            .can_set(&AskForApproval::UnlessTrusted),
        Err(ConstraintError::InvalidValue {
            field_name: "approval_policy",
            candidate: "UnlessTrusted".into(),
            allowed: "[OnRequest, Never]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );
    assert!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::read_only())
            .is_ok()
    );

    Ok(())
}

#[test]
fn deserialize_allowed_approvals_reviewers() -> Result<()> {
    let toml_str = r#"
            allowed_approvals_reviewers = ["auto_review", "user"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.approvals_reviewer.value(),
        ApprovalsReviewer::AutoReview,
        "currently, there is no way to specify the default value for approvals reviewer in the toml, so it picks the first allowed value"
    );
    assert!(
        requirements
            .approvals_reviewer
            .can_set(&ApprovalsReviewer::AutoReview)
            .is_ok()
    );
    assert!(
        requirements
            .approvals_reviewer
            .can_set(&ApprovalsReviewer::User)
            .is_ok()
    );

    Ok(())
}

#[test]
fn deserialize_allowed_windows_sandbox_implementations() -> Result<()> {
    let requirements: ConfigRequirementsToml = from_str("[windows]\nrequire_mxc = true")?;
    assert!(!requirements.is_empty());

    let toml_str = r#"
            [windows]
            allowed_sandbox_implementations = ["elevated"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.windows_sandbox_mode.value(),
        Some(WindowsSandboxModeToml::Elevated)
    );
    assert!(
        requirements
            .windows_sandbox_mode
            .can_set(&Some(WindowsSandboxModeToml::Elevated))
            .is_ok()
    );
    assert!(
        requirements
            .windows_sandbox_mode
            .can_set(&Some(WindowsSandboxModeToml::Unelevated))
            .is_err()
    );
    assert!(
        requirements
            .windows_sandbox_mode
            .can_set(&Some(WindowsSandboxModeToml::Mxc))
            .is_ok()
    );
    assert!(requirements.windows_sandbox_mode.can_set(&None).is_err());

    for allow_mxc in [true, false] {
        let config = from_str(&format!("{toml_str}\nallow_mxc = {allow_mxc}"))?;
        let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;
        let constraint = &requirements.windows_sandbox_mode;
        assert_eq!(
            [
                Some(WindowsSandboxModeToml::Elevated),
                Some(WindowsSandboxModeToml::Unelevated),
                Some(WindowsSandboxModeToml::Mxc),
                None,
            ]
            .map(|mode| constraint.can_set(&mode).is_ok()),
            [true, false, allow_mxc, false]
        );
    }

    Ok(())
}

#[test]
fn empty_allowed_windows_sandbox_implementations_is_rejected() -> Result<()> {
    let toml_str = r#"
            [windows]
            allowed_sandbox_implementations = []
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;

    assert_eq!(
        ConfigRequirements::try_from(with_unknown_source(config)),
        Err(ConstraintError::EmptyField {
            field_name: "windows.allowed_sandbox_implementations".to_string(),
        })
    );

    Ok(())
}

#[test]
fn allowed_windows_sandbox_implementations_prefer_elevated_fallback() -> Result<()> {
    let toml_str = r#"
            [windows]
            allowed_sandbox_implementations = ["unelevated", "elevated"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.windows_sandbox_mode.value(),
        Some(WindowsSandboxModeToml::Elevated)
    );

    Ok(())
}

#[test]
fn deserialize_legacy_allowed_approvals_reviewer() -> Result<()> {
    let toml_str = r#"
            allowed_approvals_reviewers = ["guardian_subagent", "user"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.approvals_reviewer.value(),
        ApprovalsReviewer::AutoReview
    );

    Ok(())
}

#[test]
fn empty_allowed_approvals_reviewers_is_rejected() -> Result<()> {
    let toml_str = r#"
            allowed_approvals_reviewers = []
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let err = ConfigRequirements::try_from(with_unknown_source(config))
        .expect_err("empty approvals reviewer allow-list should be rejected");

    assert_eq!(
        err,
        ConstraintError::EmptyField {
            field_name: "allowed_approvals_reviewers".to_string(),
        }
    );

    Ok(())
}

#[test]
fn deserialize_allowed_sandbox_modes() -> Result<()> {
    let toml_str = r#"
            allowed_sandbox_modes = ["read-only", "workspace-write"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    let root = if cfg!(windows) { "C:\\repo" } else { "/repo" };
    assert!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::read_only())
            .is_ok()
    );
    let workspace_write_profile = PermissionProfile::workspace_write_with(
        &[AbsolutePathBuf::from_absolute_path(root)?],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ false,
        /*exclude_slash_tmp*/ false,
    );
    assert!(
        requirements
            .permission_profile
            .can_set(&workspace_write_profile)
            .is_ok()
    );
    assert_eq!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::Disabled),
        Err(ConstraintError::InvalidValue {
            field_name: "sandbox_mode",
            candidate: "DangerFullAccess".into(),
            allowed: "[ReadOnly, WorkspaceWrite]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );
    assert_eq!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::External {
                network: NetworkSandboxPolicy::Restricted,
            }),
        Err(ConstraintError::InvalidValue {
            field_name: "sandbox_mode",
            candidate: "ExternalSandbox".into(),
            allowed: "[ReadOnly, WorkspaceWrite]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );

    Ok(())
}

#[test]
fn deserialize_remote_sandbox_config_requires_hostname_patterns_list() -> Result<()> {
    let toml_str = r#"
            [[remote_sandbox_config]]
            hostname_patterns = ["*.org", "runner-??.ci"]
            allowed_sandbox_modes = ["read-only", "workspace-write"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;

    assert_eq!(
        config.remote_sandbox_config,
        Some(vec![RemoteSandboxConfigToml {
            hostname_patterns: vec!["*.org".to_string(), "runner-??.ci".to_string()],
            allowed_sandbox_modes: vec![
                SandboxModeRequirement::ReadOnly,
                SandboxModeRequirement::WorkspaceWrite,
            ],
        }])
    );

    let err = from_str::<ConfigRequirementsToml>(
        r#"
                [[remote_sandbox_config]]
                hostname_patterns = "*.org"
                allowed_sandbox_modes = ["read-only"]
            "#,
    )
    .expect_err("hostname_patterns should be list-only");
    assert!(
        err.to_string().contains("invalid type: string"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[test]
fn remote_sandbox_config_first_match_overrides_top_level() -> Result<()> {
    let source = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let mut requirements_toml: ConfigRequirementsToml = from_str(
        r#"
                allowed_sandbox_modes = ["read-only"]

                [[remote_sandbox_config]]
                hostname_patterns = ["build-*.example.com"]
                allowed_sandbox_modes = ["read-only", "workspace-write"]

                [[remote_sandbox_config]]
                hostname_patterns = ["build-01.example.com"]
                allowed_sandbox_modes = ["read-only", "danger-full-access"]
            "#,
    )?;
    requirements_toml.apply_remote_sandbox_config(Some("BUILD-01.EXAMPLE.COM."));
    let mut requirements_with_sources = ConfigRequirementsWithSources::default();
    requirements_with_sources.merge_unset_fields(source.clone(), requirements_toml);

    assert_eq!(
        requirements_with_sources
            .allowed_sandbox_modes
            .as_ref()
            .map(|sourced| sourced.value.clone()),
        Some(vec![
            SandboxModeRequirement::ReadOnly,
            SandboxModeRequirement::WorkspaceWrite,
        ])
    );

    let requirements = ConfigRequirements::try_from(requirements_with_sources)?;
    let root = if cfg!(windows) { "C:\\repo" } else { "/repo" };
    let workspace_write_profile = PermissionProfile::workspace_write_with(
        &[AbsolutePathBuf::from_absolute_path(root)?],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ false,
        /*exclude_slash_tmp*/ false,
    );
    assert!(
        requirements
            .permission_profile
            .can_set(&workspace_write_profile)
            .is_ok()
    );
    assert_eq!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::Disabled),
        Err(ConstraintError::InvalidValue {
            field_name: "sandbox_mode",
            candidate: "DangerFullAccess".into(),
            allowed: "[ReadOnly, WorkspaceWrite]".into(),
            requirement_source: source,
        })
    );

    Ok(())
}

#[test]
fn remote_sandbox_config_non_match_preserves_top_level() -> Result<()> {
    let mut requirements_toml: ConfigRequirementsToml = from_str(
        r#"
                allowed_sandbox_modes = ["read-only"]

                [[remote_sandbox_config]]
                hostname_patterns = ["build-*.example.com"]
                allowed_sandbox_modes = ["read-only", "workspace-write"]
            "#,
    )?;
    requirements_toml.apply_remote_sandbox_config(Some("laptop.example.com"));
    let mut requirements_with_sources = ConfigRequirementsWithSources::default();
    requirements_with_sources.merge_unset_fields(RequirementSource::Unknown, requirements_toml);
    let requirements = ConfigRequirements::try_from(requirements_with_sources)?;

    assert_eq!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::Disabled),
        Err(ConstraintError::InvalidValue {
            field_name: "sandbox_mode",
            candidate: "DangerFullAccess".into(),
            allowed: "[ReadOnly]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );

    Ok(())
}

#[test]
fn remote_sandbox_config_does_not_override_higher_precedence_sandbox_modes() -> Result<()> {
    let high_source = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let mut high_precedence: ConfigRequirementsToml = from_str(
        r#"
                allowed_sandbox_modes = ["read-only"]
            "#,
    )?;
    high_precedence.apply_remote_sandbox_config(Some("runner-01.ci.example.com"));

    let mut low_precedence: ConfigRequirementsToml = from_str(
        r#"
                [[remote_sandbox_config]]
                hostname_patterns = ["runner-*.ci.example.com"]
                allowed_sandbox_modes = ["read-only", "workspace-write"]
            "#,
    )?;
    low_precedence.apply_remote_sandbox_config(Some("runner-01.ci.example.com"));

    let mut requirements_with_sources = ConfigRequirementsWithSources::default();
    requirements_with_sources.merge_unset_fields(high_source.clone(), high_precedence);
    requirements_with_sources.merge_unset_fields(RequirementSource::Unknown, low_precedence);
    let requirements = ConfigRequirements::try_from(requirements_with_sources)?;

    assert_eq!(
        requirements
            .permission_profile
            .can_set(&PermissionProfile::workspace_write()),
        Err(ConstraintError::InvalidValue {
            field_name: "sandbox_mode",
            candidate: "WorkspaceWrite".into(),
            allowed: "[ReadOnly]".into(),
            requirement_source: high_source,
        })
    );

    Ok(())
}

#[test]
fn deserialize_allowed_web_search_modes() -> Result<()> {
    let toml_str = r#"
            allowed_web_search_modes = ["cached"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(requirements.web_search_mode.value(), WebSearchMode::Cached);
    assert!(
        requirements
            .web_search_mode
            .can_set(&WebSearchMode::Disabled)
            .is_ok()
    );
    assert_eq!(
        requirements.web_search_mode.can_set(&WebSearchMode::Live),
        Err(ConstraintError::InvalidValue {
            field_name: "web_search_mode",
            candidate: "Live".into(),
            allowed: "[Disabled, Cached]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );
    assert!(
        requirements
            .web_search_mode
            .can_set(&WebSearchMode::Cached)
            .is_ok()
    );

    Ok(())
}

#[test]
fn allowed_web_search_modes_supports_indexed() -> Result<()> {
    let config: ConfigRequirementsToml = from_str(
        r#"
                allowed_web_search_modes = ["indexed"]
            "#,
    )?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(requirements.web_search_mode.value(), WebSearchMode::Indexed);
    for mode in [WebSearchMode::Disabled, WebSearchMode::Indexed] {
        assert!(requirements.web_search_mode.can_set(&mode).is_ok());
    }
    for mode in [WebSearchMode::Cached, WebSearchMode::Live] {
        assert_eq!(
            requirements.web_search_mode.can_set(&mode),
            Err(ConstraintError::InvalidValue {
                field_name: "web_search_mode",
                candidate: format!("{mode:?}"),
                allowed: "[Disabled, Indexed]".into(),
                requirement_source: RequirementSource::Unknown,
            })
        );
    }

    Ok(())
}

#[test]
fn allowed_web_search_modes_allows_disabled() -> Result<()> {
    let toml_str = r#"
            allowed_web_search_modes = ["disabled"]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.web_search_mode.value(),
        WebSearchMode::Disabled
    );
    assert!(
        requirements
            .web_search_mode
            .can_set(&WebSearchMode::Disabled)
            .is_ok()
    );
    assert_eq!(
        requirements.web_search_mode.can_set(&WebSearchMode::Cached),
        Err(ConstraintError::InvalidValue {
            field_name: "web_search_mode",
            candidate: "Cached".into(),
            allowed: "[Disabled]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );
    Ok(())
}

#[test]
fn allowed_web_search_modes_empty_restricts_to_disabled() -> Result<()> {
    let toml_str = r#"
            allowed_web_search_modes = []
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.web_search_mode.value(),
        WebSearchMode::Disabled
    );
    assert!(
        requirements
            .web_search_mode
            .can_set(&WebSearchMode::Disabled)
            .is_ok()
    );
    assert_eq!(
        requirements.web_search_mode.can_set(&WebSearchMode::Cached),
        Err(ConstraintError::InvalidValue {
            field_name: "web_search_mode",
            candidate: "Cached".into(),
            allowed: "[Disabled]".into(),
            requirement_source: RequirementSource::Unknown,
        })
    );
    Ok(())
}

#[test]
fn deserialize_feature_requirements() -> Result<()> {
    let toml_str = r#"
            [features]
            apps = false
            personality = true
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;

    assert_eq!(
        requirements.feature_requirements,
        Some(Sourced::new(
            FeatureRequirementsToml {
                entries: BTreeMap::from([
                    ("apps".to_string(), false),
                    ("personality".to_string(), true),
                ]),
            },
            RequirementSource::Unknown,
        ))
    );

    Ok(())
}

#[test]
fn deserialize_managed_hooks_requirements() -> Result<()> {
    let toml_str = r#"
managed_dir = "/enterprise/hooks"
windows_managed_dir = 'C:\enterprise\hooks'

[[PreToolUse]]
matcher = "^Bash$"

[[PreToolUse.hooks]]
type = "command"
command = "python3 /enterprise/hooks/pre.py"
timeout = 10
statusMessage = "checking"
        "#;
    let hooks: ManagedHooksRequirementsToml = from_str(toml_str)?;

    assert_eq!(
        hooks.managed_dir.as_deref(),
        Some(std::path::Path::new("/enterprise/hooks"))
    );
    assert_eq!(hooks.handler_count(), 1);
    assert_eq!(hooks.hooks.pre_tool_use.len(), 1);
    Ok(())
}

#[test]
fn merge_unset_fields_does_not_overwrite_existing_hooks() -> Result<()> {
    let mut target = ConfigRequirementsWithSources::default();
    target.merge_unset_fields(
        RequirementSource::LegacyManagedConfigTomlFromMdm,
        from_str::<ConfigRequirementsToml>(
            r#"
[hooks]
managed_dir = "/cloud/hooks"

[[hooks.PreToolUse]]
matcher = "^Bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "python3 /cloud/hooks/pre.py"
                "#,
        )?,
    );
    target.merge_unset_fields(
        RequirementSource::SystemRequirementsToml {
            file: system_requirements_toml_file_for_test()?,
        },
        from_str::<ConfigRequirementsToml>(
            r#"
[hooks]
managed_dir = "/system/hooks"

[[hooks.PreToolUse]]
matcher = "^Bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "python3 /system/hooks/pre.py"
                "#,
        )?,
    );

    assert_eq!(
        target
            .hooks
            .as_ref()
            .and_then(|hooks| hooks.value.managed_dir.as_ref())
            .map(std::path::PathBuf::as_path),
        Some(std::path::Path::new("/cloud/hooks"))
    );
    assert_eq!(
        target.hooks.as_ref().map(|hooks| hooks.source.clone()),
        Some(RequirementSource::LegacyManagedConfigTomlFromMdm)
    );
    Ok(())
}

#[test]
fn managed_hooks_constraint_rejects_drift() -> Result<()> {
    let config: ConfigRequirementsToml = from_str(
        r#"
[hooks]
managed_dir = "/enterprise/hooks"

[[hooks.PreToolUse]]
matcher = "^Bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "python3 /enterprise/hooks/pre.py"
            "#,
    )?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;
    let mut managed_hooks = requirements
        .managed_hooks
        .expect("expected managed hooks requirements");

    let err = managed_hooks
        .set(ManagedHooksRequirementsToml {
            managed_dir: Some(std::path::PathBuf::from("/other/hooks")),
            windows_managed_dir: None,
            hooks: HookEventsToml::default(),
        })
        .expect_err("managed hooks should reject drift");

    assert!(matches!(
        err,
        ConstraintError::InvalidValue {
            field_name: "hooks",
            requirement_source: RequirementSource::Unknown,
            ..
        }
    ));
    Ok(())
}

#[test]
fn network_requirements_are_preserved_as_constraints_with_source() -> Result<()> {
    let toml_str = r#"
            [experimental_network]
            enabled = true
            allow_upstream_proxy = false
            dangerously_allow_all_unix_sockets = true
            managed_allowed_domains_only = true
            allow_local_binding = false

            [experimental_network.domains]
            "api.example.com" = "allow"
            "*.openai.com" = "allow"
            "blocked.example.com" = "deny"

            [experimental_network.unix_sockets]
            "/tmp/example.sock" = "allow"
            "/tmp/blocked.sock" = "deny"

            [[experimental_network.header_injections]]
            host = "api.example.com"
            methods = ["POST"]
            path_prefixes = ["/console/v1"]

            [experimental_network.header_injections.headers]
            "x-statsig-change-source" = "codex"
        "#;

    let source = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let mut requirements_with_sources = ConfigRequirementsWithSources::default();
    requirements_with_sources.merge_unset_fields(source.clone(), from_str(toml_str)?);

    let requirements = ConfigRequirements::try_from(requirements_with_sources)?;
    let sourced_network = requirements
        .network
        .expect("network requirements should be preserved as constraints");

    assert_eq!(sourced_network.source, source);
    assert_eq!(sourced_network.value.enabled, Some(true));
    assert_eq!(sourced_network.value.allow_upstream_proxy, Some(false));
    assert_eq!(
        sourced_network.value.dangerously_allow_all_unix_sockets,
        Some(true)
    );
    assert_eq!(
        sourced_network.value.domains.as_ref(),
        Some(&NetworkDomainPermissionsToml {
            entries: BTreeMap::from([
                (
                    "*.openai.com".to_string(),
                    NetworkDomainPermissionToml::Allow,
                ),
                (
                    "api.example.com".to_string(),
                    NetworkDomainPermissionToml::Allow,
                ),
                (
                    "blocked.example.com".to_string(),
                    NetworkDomainPermissionToml::Deny,
                ),
            ]),
        })
    );
    assert_eq!(
        sourced_network.value.managed_allowed_domains_only,
        Some(true)
    );
    assert_eq!(
        sourced_network.value.unix_sockets.as_ref(),
        Some(&NetworkUnixSocketPermissionsToml {
            entries: BTreeMap::from([
                (
                    "/tmp/blocked.sock".to_string(),
                    NetworkUnixSocketPermissionToml::Deny,
                ),
                (
                    "/tmp/example.sock".to_string(),
                    NetworkUnixSocketPermissionToml::Allow,
                ),
            ]),
        })
    );
    assert_eq!(sourced_network.value.allow_local_binding, Some(false));
    assert_eq!(
        sourced_network.value.header_injections,
        Some(vec![NetworkHeaderInjectionToml {
            host: "api.example.com".to_string(),
            methods: vec!["POST".to_string()],
            path_prefixes: vec!["/console/v1".to_string()],
            headers: BTreeMap::from([
                ("x-statsig-change-source".to_string(), "codex".to_string(),)
            ]),
        }])
    );
    let debug = format!("{:?}", sourced_network.value.header_injections);
    assert!(debug.contains("x-statsig-change-source"));
    assert!(!debug.contains("codex"));

    Ok(())
}

#[test]
fn legacy_network_requirements_are_preserved_as_constraints_with_source() -> Result<()> {
    let toml_str = r#"
            [experimental_network]
            enabled = true
            allow_upstream_proxy = false
            dangerously_allow_all_unix_sockets = true
            allowed_domains = ["api.example.com", "*.openai.com"]
            managed_allowed_domains_only = true
            denied_domains = ["blocked.example.com"]
            allow_unix_sockets = ["/tmp/example.sock"]
            allow_local_binding = false
        "#;

    let source = RequirementSource::LegacyManagedConfigTomlFromMdm;
    let mut requirements_with_sources = ConfigRequirementsWithSources::default();
    requirements_with_sources.merge_unset_fields(source.clone(), from_str(toml_str)?);

    let requirements = ConfigRequirements::try_from(requirements_with_sources)?;
    let sourced_network = requirements
        .network
        .expect("network requirements should be preserved as constraints");

    assert_eq!(sourced_network.source, source);
    assert_eq!(sourced_network.value.enabled, Some(true));
    assert_eq!(sourced_network.value.allow_upstream_proxy, Some(false));
    assert_eq!(
        sourced_network.value.dangerously_allow_all_unix_sockets,
        Some(true)
    );
    assert_eq!(
        sourced_network.value.domains.as_ref(),
        Some(&NetworkDomainPermissionsToml {
            entries: BTreeMap::from([
                (
                    "*.openai.com".to_string(),
                    NetworkDomainPermissionToml::Allow,
                ),
                (
                    "api.example.com".to_string(),
                    NetworkDomainPermissionToml::Allow,
                ),
                (
                    "blocked.example.com".to_string(),
                    NetworkDomainPermissionToml::Deny,
                ),
            ]),
        })
    );
    assert_eq!(
        sourced_network.value.managed_allowed_domains_only,
        Some(true)
    );
    assert_eq!(
        sourced_network.value.unix_sockets.as_ref(),
        Some(&NetworkUnixSocketPermissionsToml {
            entries: BTreeMap::from([(
                "/tmp/example.sock".to_string(),
                NetworkUnixSocketPermissionToml::Allow,
            )]),
        })
    );
    assert_eq!(sourced_network.value.allow_local_binding, Some(false));

    Ok(())
}

#[test]
fn mixed_legacy_and_canonical_network_requirements_are_rejected() {
    let err = from_str::<ConfigRequirementsToml>(
        r#"
                [experimental_network]
                allowed_domains = ["api.example.com"]

                [experimental_network.domains]
                "*.openai.com" = "allow"
            "#,
    )
    .expect_err("mixed network domain shapes should fail");

    assert!(
        err.to_string()
            .contains("`experimental_network.domains` cannot be combined"),
        "unexpected error: {err:#}"
    );

    let err = from_str::<ConfigRequirementsToml>(
        r#"
                [experimental_network]
                allow_unix_sockets = ["/tmp/example.sock"]

                [experimental_network.unix_sockets]
                "/tmp/another.sock" = "allow"
            "#,
    )
    .expect_err("mixed network unix socket shapes should fail");

    assert!(
        err.to_string()
            .contains("`experimental_network.unix_sockets` cannot be combined"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn network_permission_containers_project_allowed_and_denied_entries() {
    let domains = NetworkDomainPermissionsToml {
        entries: BTreeMap::from([
            (
                "*.openai.com".to_string(),
                NetworkDomainPermissionToml::Allow,
            ),
            (
                "api.example.com".to_string(),
                NetworkDomainPermissionToml::Allow,
            ),
            (
                "blocked.example.com".to_string(),
                NetworkDomainPermissionToml::Deny,
            ),
        ]),
    };
    let unix_sockets = NetworkUnixSocketPermissionsToml {
        entries: BTreeMap::from([
            (
                "/tmp/example.sock".to_string(),
                NetworkUnixSocketPermissionToml::Allow,
            ),
            (
                "/tmp/ignored.sock".to_string(),
                NetworkUnixSocketPermissionToml::Deny,
            ),
        ]),
    };

    assert_eq!(
        domains.allowed_domains(),
        Some(vec![
            "*.openai.com".to_string(),
            "api.example.com".to_string()
        ])
    );
    assert_eq!(
        domains.denied_domains(),
        Some(vec!["blocked.example.com".to_string()])
    );
    assert_eq!(
        NetworkDomainPermissionsToml {
            entries: BTreeMap::from([(
                "api.example.com".to_string(),
                NetworkDomainPermissionToml::Allow,
            )]),
        }
        .denied_domains(),
        None
    );
    assert_eq!(
        unix_sockets.allow_unix_sockets(),
        vec!["/tmp/example.sock".to_string()]
    );
}

#[test]
fn deserialize_mcp_server_requirements() -> Result<()> {
    let toml_str = r#"
            [mcp_servers.docs]
            description = "ignored legacy field"

            [mcp_servers.docs.identity]
            command = "codex-mcp"

            [mcp_servers.remote.identity]
            url = "https://example.com/mcp"
        "#;
    let requirements: ConfigRequirements = with_unknown_source(from_str(toml_str)?).try_into()?;

    assert_eq!(
        requirements.mcp_servers,
        Some(Sourced::new(
            BTreeMap::from([
                (
                    "docs".to_string(),
                    McpServerRequirement::Identity {
                        identity: McpServerIdentity::Command {
                            command: "codex-mcp".to_string(),
                        },
                    },
                ),
                (
                    "remote".to_string(),
                    McpServerRequirement::Identity {
                        identity: McpServerIdentity::Url {
                            url: "https://example.com/mcp".to_string(),
                        },
                    },
                ),
            ]),
            RequirementSource::Unknown,
        ))
    );
    Ok(())
}

#[test]
fn deserialize_mcp_server_matcher_requirements() -> Result<()> {
    let toml_str = r#"
            [mcp_servers.internal_mcp_proxy.identity]
            command = { executable = "company-cli", args = [
                { match = "exact", value = "mcp" },
                { match = "exact", value = "proxy" },
                { match = "exact", value = "--server" },
                { match = "regex", expression = '^https://[A-Za-z0-9-]+\.mcp\.internal\.example\.com(?::443)?(?:/.*)?$' },
            ] }
        "#;
    let requirements: ConfigRequirements = with_unknown_source(from_str(toml_str)?).try_into()?;

    assert_eq!(
            requirements.mcp_servers,
            Some(Sourced::new(
                BTreeMap::from([(
                    "internal_mcp_proxy".to_string(),
                    McpServerRequirement::Command(McpServerCommandMatcher {
                        executable: "company-cli".to_string(),
                        args: vec![
                            McpServerValueMatcher::Exact {
                                value: "mcp".to_string(),
                            },
                            McpServerValueMatcher::Exact {
                                value: "proxy".to_string(),
                            },
                            McpServerValueMatcher::Exact {
                                value: "--server".to_string(),
                            },
                            McpServerValueMatcher::Regex {
                                expression: r"^https://[A-Za-z0-9-]+\.mcp\.internal\.example\.com(?::443)?(?:/.*)?$"
                                    .to_string(),
                            },
                        ],
                    }),
                )]),
                RequirementSource::Unknown,
            ))
        );
    Ok(())
}

#[test]
fn invalid_mcp_server_requirement_regex_reports_the_server_name_and_source() -> Result<()> {
    let toml_str = r#"
            [mcp_servers.broken_rule.identity]
            url = { match = "regex", expression = "[" }
        "#;

    let err = ConfigRequirements::try_from(with_unknown_source(from_str(toml_str)?))
        .expect_err("invalid matcher regex should fail requirements normalization");
    let ConstraintError::McpServerRequirementParse {
        server_name,
        requirement_source,
        reason,
    } = err
    else {
        panic!("unexpected error: {err:?}");
    };

    assert_eq!(server_name, "broken_rule");
    assert_eq!(requirement_source, RequirementSource::Unknown);
    assert!(reason.contains("invalid regex `[`"), "{reason}");
    Ok(())
}

#[test]
fn deserialize_plugin_mcp_server_requirements() -> Result<()> {
    let toml_str = r#"
            [plugins."sample@test".mcp_servers.sample.identity]
            command = "sample-mcp"

            [plugins."remote@test".mcp_servers.remote.identity]
            url = "https://example.com/mcp"
        "#;
    let requirements: ConfigRequirements = with_unknown_source(from_str(toml_str)?).try_into()?;

    assert_eq!(
        requirements.plugins,
        Some(Sourced::new(
            BTreeMap::from([
                (
                    "remote@test".to_string(),
                    PluginRequirementsToml {
                        mcp_servers: Some(BTreeMap::from([(
                            "remote".to_string(),
                            McpServerRequirement::Identity {
                                identity: McpServerIdentity::Url {
                                    url: "https://example.com/mcp".to_string(),
                                },
                            },
                        )])),
                    },
                ),
                (
                    "sample@test".to_string(),
                    PluginRequirementsToml {
                        mcp_servers: Some(BTreeMap::from([(
                            "sample".to_string(),
                            McpServerRequirement::Identity {
                                identity: McpServerIdentity::Command {
                                    command: "sample-mcp".to_string(),
                                },
                            },
                        )])),
                    },
                ),
            ]),
            RequirementSource::Unknown,
        ))
    );
    Ok(())
}

#[test]
fn deserialize_plugin_mcp_server_matcher_requirement() -> Result<()> {
    let toml_str = r#"
            [plugins."sample@test".mcp_servers.internal_proxy.identity]
            command = { executable = "company-cli", args = [
                { match = "exact", value = "mcp" },
                { match = "regex", expression = '^https://[a-z]+\.example\.com$' },
            ] }
        "#;
    let requirements: ConfigRequirements = with_unknown_source(from_str(toml_str)?).try_into()?;

    assert_eq!(
        requirements.plugins,
        Some(Sourced::new(
            BTreeMap::from([(
                "sample@test".to_string(),
                PluginRequirementsToml {
                    mcp_servers: Some(BTreeMap::from([(
                        "internal_proxy".to_string(),
                        McpServerRequirement::Command(McpServerCommandMatcher {
                            executable: "company-cli".to_string(),
                            args: vec![
                                McpServerValueMatcher::Exact {
                                    value: "mcp".to_string(),
                                },
                                McpServerValueMatcher::Regex {
                                    expression: r"^https://[a-z]+\.example\.com$".to_string(),
                                },
                            ],
                        }),
                    )])),
                },
            )]),
            RequirementSource::Unknown,
        ))
    );
    Ok(())
}

#[test]
fn invalid_plugin_mcp_server_regex_reports_plugin_and_server_name() -> Result<()> {
    let toml_str = r#"
            [plugins."sample@test".mcp_servers.broken_rule.identity]
            url = { match = "regex", expression = "[" }
        "#;

    let err = ConfigRequirements::try_from(with_unknown_source(from_str(toml_str)?))
        .expect_err("invalid plugin MCP regex should fail requirements normalization");
    let ConstraintError::McpServerRequirementParse {
        server_name,
        requirement_source,
        reason,
    } = err
    else {
        panic!("unexpected error: {err:?}");
    };

    assert_eq!(server_name, "sample@test/broken_rule");
    assert_eq!(requirement_source, RequirementSource::Unknown);
    assert!(reason.contains("invalid regex `[`"), "{reason}");
    Ok(())
}

#[test]
fn deserialize_exec_policy_requirements() -> Result<()> {
    let toml_str = r#"
            [rules]
            prefix_rules = [
                { pattern = [{ token = "rm" }], decision = "forbidden" },
            ]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements: ConfigRequirements = with_unknown_source(config).try_into()?;
    let policy = requirements.exec_policy.expect("exec policy").value;

    assert_eq!(
        policy.as_ref().check(&tokens(&["rm", "-rf"]), &|_| {
            panic!("rule should match so heuristic should not be called");
        }),
        Evaluation {
            decision: Decision::Forbidden,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["rm"]),
                decision: Decision::Forbidden,
                resolved_program: None,
                justification: None,
            }],
        }
    );

    Ok(())
}

#[test]
fn exec_policy_error_includes_requirement_source() -> Result<()> {
    let toml_str = r#"
            [rules]
            prefix_rules = [
                { pattern = [{ token = "rm" }] },
            ]
        "#;
    let config: ConfigRequirementsToml = from_str(toml_str)?;
    let requirements_toml_file = system_requirements_toml_file_for_test()?;
    let source_location = RequirementSource::SystemRequirementsToml {
        file: requirements_toml_file,
    };

    let mut requirements_with_sources = ConfigRequirementsWithSources::default();
    requirements_with_sources.merge_unset_fields(source_location.clone(), config);
    let err =
        ConfigRequirements::try_from(requirements_with_sources).expect_err("invalid exec policy");

    assert_eq!(
        err,
        ConstraintError::ExecPolicyParse {
            requirement_source: source_location,
            reason: "rules prefix_rule at index 0 is missing a decision".to_string(),
        }
    );

    Ok(())
}
