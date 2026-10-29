// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Upstream tests for the effective product configuration snapshot.
#![allow(
    unused_imports,
    reason = "Shared imports support the ported configuration cases."
)]
use it_config::support::CloudConfigBundleFixture;
use ra_coding::config::network::*;
use ra_coding::config::providers::*;
use ra_coding::config::test_api::*;
use ra_coding::config::*;
use ra_config::config_toml::*;
use ra_config::features::*;
use ra_config::loader::{load_config_layers_state, project_trust_key};
use ra_config::permission_profile_catalog::PermissionProfileCatalogEntry;
use ra_config::permissions_toml::{
    FilesystemPermissionToml, FilesystemPermissionsToml, NetworkDomainPermissionToml,
    NetworkDomainPermissionsToml, NetworkMitmActionToml, NetworkMitmHookToml, NetworkMitmToml,
    NetworkToml, PermissionProfileToml, PermissionsToml, WorkspaceRootsToml,
};
use ra_config::types::*;
use ra_config::*;
use ra_executor::LOCAL_FS;
use ra_filesystem::legacy::{NetworkAccess, SandboxPolicy};
use ra_filesystem::models::*;
use ra_filesystem::permissions::*;
use ra_paths::{
    AbsolutePathBuf, AbsolutePathBufGuard, LegacyAppPathString, PathConvention, PathUri, Platform,
};
use std::io::ErrorKind;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::{TempDir, tempdir};
const RESIDENCY_HEADER_NAME: &str = "x-openai-internal-codex-residency";
use indexmap::IndexMap;
use ra_config::diagnostics::config_error_from_typed_toml;
use ra_config::strict_config::config_error_from_ignored_toml_fields;
#[path = "config_support/mod.rs"]
mod config_support;
use config_support::*;

#[tokio::test]
async fn test_toml_parsing() {
    let history_with_persistence = r#"
[history]
persistence = "save-all"
"#;
    let history_with_persistence_cfg = toml::from_str::<ConfigToml>(history_with_persistence)
        .expect("TOML deserialization should succeed");
    assert_eq!(
        Some(History {
            persistence: HistoryPersistence::SaveAll,
            max_bytes: None,
        }),
        history_with_persistence_cfg.history
    );

    let history_no_persistence = r#"
[history]
persistence = "none"
"#;

    let history_no_persistence_cfg = toml::from_str::<ConfigToml>(history_no_persistence)
        .expect("TOML deserialization should succeed");
    assert_eq!(
        Some(History {
            persistence: HistoryPersistence::None,
            max_bytes: None,
        }),
        history_no_persistence_cfg.history
    );

    let memories = r#"
[memories]
disable_on_external_context = true
generate_memories = false
use_memories = false
dedicated_tools = true
max_raw_memories_for_consolidation = 512
max_unused_days = 21
max_rollout_age_days = 42
max_rollouts_per_startup = 9
min_rollout_idle_hours = 24
min_rate_limit_remaining_percent = 12
extract_model = "gpt-5-mini"
consolidation_model = "gpt-5.2"
"#;
    let memories_cfg =
        toml::from_str::<ConfigToml>(memories).expect("TOML deserialization should succeed");
    assert_eq!(
        Some(MemoriesToml {
            dual_write: None,
            version: None,
            disable_on_external_context: Some(true),
            generate_memories: Some(false),
            use_memories: Some(false),
            dedicated_tools: Some(true),
            max_raw_memories_for_consolidation: Some(512),
            max_unused_days: Some(21),
            max_rollout_age_days: Some(42),
            max_rollouts_per_startup: Some(9),
            min_rollout_idle_hours: Some(24),
            min_rate_limit_remaining_percent: Some(12),
            extract_model: Some("gpt-5-mini".to_string()),
            consolidation_model: Some("gpt-5.2".to_string()),
        }),
        memories_cfg.memories
    );

    let config = Config::load_from_base_config_with_overrides(
        memories_cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config from memories settings");
    assert_eq!(
        config.memories,
        MemoriesConfig {
            dual_write: false,
            version: ra_config::MemoryVersion::V1,
            disable_on_external_context: true,
            generate_memories: false,
            use_memories: false,
            dedicated_tools: true,
            max_raw_memories_for_consolidation: 512,
            max_unused_days: 21,
            max_rollout_age_days: 42,
            max_rollouts_per_startup: 9,
            min_rollout_idle_hours: 24,
            min_rate_limit_remaining_percent: 12,
            extract_model: Some("gpt-5-mini".to_string()),
            consolidation_model: Some("gpt-5.2".to_string()),
        }
    );

    let legacy_memories_cfg =
        toml::from_str::<ConfigToml>("[memories]\nno_memories_if_mcp_or_web_search = true\n")
            .expect("legacy memories TOML should deserialize");
    assert!(
        MemoriesConfig::from(
            legacy_memories_cfg
                .memories
                .expect("legacy memories config")
        )
        .disable_on_external_context
    );
}

#[tokio::test]
async fn goal_max_token_budget_requires_positive_integer() {
    let config_toml = toml::from_str::<ConfigToml>("[goals]\nmax_goal_token_budget = 25000\n")
        .expect("positive goal token budget should deserialize");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("positive goal token budget should load");
    assert_eq!(config.max_goal_token_budget, Some(25_000));

    for invalid in ["0", "-1", "1.5", "\"100\""] {
        let config = format!("[goals]\nmax_goal_token_budget = {invalid}\n");
        assert!(
            toml::from_str::<ConfigToml>(&config).is_err(),
            "invalid goal token budget should be rejected: {invalid}"
        );
    }
}

#[tokio::test]
async fn managed_residency_warns_about_provider_header_overrides_without_mutating_config()
-> std::io::Result<()> {
    for enforce_residency in [None, Some(ResidencyRequirement::Us)] {
        let cfg = toml::from_str::<ConfigToml>(
            r#"
model_provider = "custom-openai"

[model_providers.custom-openai]
name = "OpenAI"
http_headers = { "X-OpenAI-Internal-Codex-Residency" = "request-override", "x-provider-header" = "preserved" }
env_http_headers = { "x-openai-internal-codex-residency" = "CODEX_TEST_UNSET_RESIDENCY_HEADER", "x-provider-env-header" = "CODEX_TEST_UNSET_PROVIDER_HEADER" }
"#,
        )
        .expect("existing provider configuration should remain loadable");
        let requirements = ConfigRequirements {
            enforce_residency: ConstrainedWithSource::new(
                Constrained::allow_only(enforce_residency),
                enforce_residency.map(|_| RequirementSource::Unknown),
            ),
            ..Default::default()
        };
        let requirements_toml = ConfigRequirementsToml {
            enforce_residency,
            ..Default::default()
        };
        let config_layer_stack =
            ConfigLayerStack::new(Vec::new(), requirements, requirements_toml)?;
        let config = Config::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            cfg,
            ConfigOverrides::default(),
            tempdir()?.abs(),
            config_layer_stack,
        )
        .await?;

        let static_headers = config
            .model_provider
            .http_headers
            .as_ref()
            .expect("static headers should remain configured");
        let environment_headers = config
            .model_provider
            .env_http_headers
            .as_ref()
            .expect("environment-backed headers should remain configured");
        assert_eq!(
            static_headers.get("X-OpenAI-Internal-Codex-Residency"),
            Some(&"request-override".into())
        );
        assert_eq!(
            environment_headers
                .get(RESIDENCY_HEADER_NAME)
                .map(String::as_str),
            Some("CODEX_TEST_UNSET_RESIDENCY_HEADER")
        );
        assert_eq!(
            static_headers.get("x-provider-header"),
            Some(&"preserved".into())
        );
        assert_eq!(
            environment_headers
                .get("x-provider-env-header")
                .map(String::as_str),
            Some("CODEX_TEST_UNSET_PROVIDER_HEADER")
        );

        let expected_warning = format!(
            "Ignoring `{RESIDENCY_HEADER_NAME}` in `model_providers.custom-openai` because managed residency is required."
        );
        assert_eq!(
            config.startup_warnings.contains(&expected_warning),
            enforce_residency.is_some()
        );
    }

    Ok(())
}

#[tokio::test]
async fn load_config_applies_amazon_bedrock_aws_profile_override() {
    let cfg = toml::from_str::<ConfigToml>(
        r#"
model_provider = "amazon-bedrock"

[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#,
    )
    .expect("Amazon Bedrock AWS overrides should deserialize");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config");

    assert_eq!(config.model_provider_id, "amazon-bedrock");
    assert_eq!(
        config
            .model_provider
            .aws
            .as_ref()
            .and_then(|aws| aws.profile.as_deref()),
        Some("codex-bedrock")
    );
    assert_eq!(
        config
            .model_provider
            .aws
            .as_ref()
            .and_then(|aws| aws.region.as_deref()),
        Some("us-west-2")
    );
}

#[tokio::test]
async fn load_config_applies_amazon_bedrock_transport_overrides() {
    let cfg = toml::from_str::<ConfigToml>(
        r#"
model_provider = "amazon-bedrock"

[model_providers.amazon-bedrock]
base_url = "https://bedrock.example.com/v1"
http_headers = { "X-Custom-Header" = "value" }

[model_providers.amazon-bedrock.auth]
command = "print-token"
"#,
    )
    .expect("Amazon Bedrock transport overrides should deserialize");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config");

    let mut expected_provider = built_in_model_providers(/*openai_base_url*/ None)
        .remove("amazon-bedrock")
        .expect("Amazon Bedrock provider should be built in");
    expected_provider.base_url = Some("https://bedrock.example.com/v1".to_string());
    expected_provider.auth = Some(ModelProviderAuthInfo {
        command: "print-token".to_string(),
        args: Vec::new(),
        timeout_ms: std::num::NonZeroU64::new(5_000).expect("timeout should be non-zero"),
        refresh_interval_ms: 300_000,
        cwd: std::env::current_dir()
            .expect("current directory should be available")
            .try_into()
            .expect("current directory should be absolute"),
    });
    expected_provider
        .http_headers
        .get_or_insert_default()
        .insert("X-Custom-Header".to_string(), "value".into());

    assert_eq!(config.model_provider_id, "amazon-bedrock");
    assert_eq!(config.model_provider, expected_provider);
}

#[tokio::test]
async fn runtime_config_defaults_model_availability_nux() {
    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config");

    assert_eq!(
        cfg.model_availability_nux,
        ModelAvailabilityNuxConfig::default()
    );
}

#[tokio::test]
async fn runtime_config_uses_tui_raw_output_mode() {
    let toml = r#"
        [tui]
        raw_output_mode = true
    "#;
    let cfg_toml: ConfigToml = toml::from_str(toml).expect("deserialize raw_output_mode=true");
    let cfg = Config::load_from_base_config_with_overrides(
        cfg_toml,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config");

    assert!(cfg.tui_raw_output_mode);
}

#[tokio::test]
async fn tui_auto_recap_defaults_and_cli_overrides() -> anyhow::Result<()> {
    for (toml, override_value, expected) in [
        ("", None, true),
        ("[tui]", None, true),
        ("[tui]\nauto_recap = true", None, true),
        ("[tui]\nauto_recap = false", None, false),
        ("[tui]\nauto_recap = true", Some(false), false),
        ("[tui]\nauto_recap = false", Some(true), true),
    ] {
        let codex_home = TempDir::new()?;
        std::fs::write(codex_home.path().join(CONFIG_TOML_FILE), toml)?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .cli_overrides(
                override_value
                    .into_iter()
                    .map(|enabled| ("tui.auto_recap".to_string(), TomlValue::Boolean(enabled)))
                    .collect(),
            )
            .build()
            .await?;

        assert_eq!(
            config.tui_auto_recap, expected,
            "config: {toml}, override: {override_value:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_proxy_policy_does_not_start_managed_network_proxy_without_feature()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    assert!(
        config.permissions.network.is_none(),
        "bare profile network.enabled should not start the managed network proxy"
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_proxy_policy_starts_managed_network_proxy() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            proxy_url: Some("http://127.0.0.1:43128".to_string()),
                            enable_socks5: Some(false),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    assert!(
        config.permissions.network.is_none(),
        "profile proxy policy should not start the managed network proxy without the feature"
    );
    Ok(())
}

#[tokio::test]
async fn network_proxy_feature_is_no_op_without_sandbox_network() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(toml::from_str("network_proxy = true").expect("valid features")),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
    assert!(
        config.permissions.network.is_none(),
        "network_proxy should not start the managed network proxy while network access is off"
    );
    Ok(())
}

#[tokio::test]
async fn network_proxy_feature_matrix_preserves_sandbox_network_semantics() -> std::io::Result<()> {
    #[derive(Clone, Copy)]
    enum Surface {
        PermissionProfile,
        LegacyWorkspaceWrite,
    }

    struct Case {
        name: &'static str,
        surface: Surface,
        network_enabled: bool,
        proxy_enabled: bool,
        expected_network_policy: NetworkSandboxPolicy,
    }

    let cases = [
        Case {
            name: "permission profile network disabled without proxy",
            surface: Surface::PermissionProfile,
            network_enabled: false,
            proxy_enabled: false,
            expected_network_policy: NetworkSandboxPolicy::Restricted,
        },
        Case {
            name: "permission profile network disabled with proxy",
            surface: Surface::PermissionProfile,
            network_enabled: false,
            proxy_enabled: true,
            expected_network_policy: NetworkSandboxPolicy::Restricted,
        },
        Case {
            name: "permission profile network enabled without proxy",
            surface: Surface::PermissionProfile,
            network_enabled: true,
            proxy_enabled: false,
            expected_network_policy: NetworkSandboxPolicy::Enabled,
        },
        Case {
            name: "permission profile network enabled with proxy",
            surface: Surface::PermissionProfile,
            network_enabled: true,
            proxy_enabled: true,
            expected_network_policy: NetworkSandboxPolicy::Enabled,
        },
        Case {
            name: "legacy workspace write network disabled without proxy",
            surface: Surface::LegacyWorkspaceWrite,
            network_enabled: false,
            proxy_enabled: false,
            expected_network_policy: NetworkSandboxPolicy::Restricted,
        },
        Case {
            name: "legacy workspace write network disabled with proxy",
            surface: Surface::LegacyWorkspaceWrite,
            network_enabled: false,
            proxy_enabled: true,
            expected_network_policy: NetworkSandboxPolicy::Restricted,
        },
        Case {
            name: "legacy workspace write network enabled without proxy",
            surface: Surface::LegacyWorkspaceWrite,
            network_enabled: true,
            proxy_enabled: false,
            expected_network_policy: NetworkSandboxPolicy::Enabled,
        },
        Case {
            name: "legacy workspace write network enabled with proxy",
            surface: Surface::LegacyWorkspaceWrite,
            network_enabled: true,
            proxy_enabled: true,
            expected_network_policy: NetworkSandboxPolicy::Enabled,
        },
    ];

    for case in cases {
        let codex_home = TempDir::new()?;
        let cwd = TempDir::new()?;
        std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;
        let features = case
            .proxy_enabled
            .then(|| toml::from_str("network_proxy = true").expect("valid features"));
        let base_config = match case.surface {
            Surface::PermissionProfile => ConfigToml {
                default_permissions: Some("dev".to_string()),
                permissions: Some(PermissionsToml {
                    entries: BTreeMap::from([(
                        "dev".to_string(),
                        PermissionProfileToml {
                            description: None,
                            extends: None,
                            workspace_roots: None,
                            filesystem: Some(FilesystemPermissionsToml {
                                glob_scan_max_depth: None,
                                entries: BTreeMap::from([(
                                    ":minimal".to_string(),
                                    FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                                )]),
                            }),
                            network: Some(NetworkToml {
                                enabled: Some(case.network_enabled),
                                ..Default::default()
                            }),
                        },
                    )]),
                }),
                features,
                ..Default::default()
            },
            Surface::LegacyWorkspaceWrite => ConfigToml {
                sandbox_mode: Some(SandboxMode::WorkspaceWrite),
                sandbox_workspace_write: Some(SandboxWorkspaceWrite {
                    network_access: case.network_enabled,
                    ..Default::default()
                }),
                windows: Some(WindowsToml {
                    sandbox: Some(WindowsSandboxModeToml::Elevated),
                    ..Default::default()
                }),
                features,
                ..Default::default()
            },
        };
        let config = Config::load_from_base_config_with_overrides(
            base_config,
            ConfigOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                ..Default::default()
            },
            codex_home.abs(),
        )
        .await?;

        assert_eq!(
            config.permissions.network_sandbox_policy(),
            case.expected_network_policy,
            "{}",
            case.name
        );
        assert_eq!(
            config.permissions.network.is_some(),
            case.network_enabled && case.proxy_enabled,
            "{}",
            case.name
        );
    }

    Ok(())
}

#[tokio::test]
async fn network_proxy_cli_overrides_merge_toggle_with_proxy_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
sandbox_mode = "workspace-write"

[sandbox_workspace_write]
network_access = true

[windows]
sandbox = "elevated"
"#,
    )?;
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![
            (
                "features.network_proxy.enabled".to_string(),
                toml::Value::Boolean(true),
            ),
            (
                "features.network_proxy.enable_socks5".to_string(),
                toml::Value::Boolean(false),
            ),
        ])
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    let network = config
        .permissions
        .network
        .as_ref()
        .expect("network_proxy should start the managed network proxy");
    assert_eq!(network.proxy_host_and_port(), "127.0.0.1:3128");
    assert!(!network.socks_enabled());
    Ok(())
}

#[tokio::test]
async fn respect_system_proxy_cli_override_enables_feature() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[features]
respect_system_proxy = false
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![(
            "features.respect_system_proxy".to_string(),
            toml::Value::Boolean(true),
        )])
        .build()
        .await?;

    assert!(config.respect_system_proxy);
    Ok(())
}

#[tokio::test]
async fn experimental_network_requirements_enable_proxy_without_feature() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[experimental_network]
enabled = true
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::NetworkProxy));
    assert!(config.managed_network_requirements_enabled());
    assert!(
        config
            .permissions
            .network
            .as_ref()
            .expect("experimental_network should configure the managed proxy")
            .enabled()
    );
    Ok(())
}

#[tokio::test]
async fn network_proxy_feature_uses_profile_network_proxy_settings() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(toml::from_str("network_proxy = true").expect("valid features")),
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            proxy_url: Some("http://127.0.0.1:43128".to_string()),
                            enable_socks5: Some(false),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    let network = config
        .permissions
        .network
        .as_ref()
        .expect("network_proxy should start the managed network proxy");
    assert_eq!(network.proxy_host_and_port(), "127.0.0.1:43128");
    assert!(!network.socks_enabled());
    Ok(())
}

#[tokio::test]
async fn disabled_network_proxy_feature_does_not_start_profile_proxy_policy() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(
                toml::from_str(
                    r#"
[network_proxy]
enabled = false
"#,
                )
                .expect("valid features"),
            ),
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            proxy_url: Some("http://127.0.0.1:43128".to_string()),
                            enable_socks5: Some(false),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert!(!config.features.enabled(Feature::NetworkProxy));
    assert!(
        config.permissions.network.is_none(),
        "disabled feature should keep profile proxy policy from starting the managed proxy"
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_network_disabled_by_default_does_not_start_proxy()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            domains: Some(NetworkDomainPermissionsToml {
                                entries: BTreeMap::from([(
                                    "openai.com".to_string(),
                                    NetworkDomainPermissionToml::Allow,
                                )]),
                            }),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert!(config.permissions.network.is_none());
    Ok(())
}

#[tokio::test]
async fn default_permissions_profile_populates_runtime_sandbox_policy() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::create_dir_all(cwd.path().join("docs"))?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let cfg = ConfigToml {
        default_permissions: Some("dev".to_string()),
        permissions: Some(PermissionsToml {
            entries: BTreeMap::from([(
                "dev".to_string(),
                PermissionProfileToml {
                    description: None,
                    extends: None,
                    workspace_roots: None,
                    filesystem: Some(FilesystemPermissionsToml {
                        glob_scan_max_depth: None,
                        entries: BTreeMap::from([
                            (
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            ),
                            (
                                ":workspace_roots".to_string(),
                                FilesystemPermissionToml::Scoped(BTreeMap::from([
                                    (".".to_string(), FileSystemAccessMode::Write),
                                    ("docs".to_string(), FileSystemAccessMode::Read),
                                ])),
                            ),
                        ]),
                    }),
                    network: None,
                },
            )]),
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let cwd_root = cwd.path().abs();
    assert_eq!(
        config.permissions.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Minimal,
                },
                access: FileSystemAccessMode::Read,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: cwd_root.clone().into(),
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: cwd_root.join("docs").into(),
                },
                access: FileSystemAccessMode::Read,
                missing_path_behavior: None,
            },
        ]),
    );
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        }
    );
    assert!(
        !config
            .permissions
            .file_system_sandbox_policy()
            .can_write_local_path_with_cwd(&cwd.path().join(".git"), cwd.path())
    );
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .as_ref()
            .map(|active| active.id.as_str()),
        Some("dev")
    );
    Ok(())
}

#[tokio::test]
async fn default_permissions_extended_profile_preserves_parent_metadata() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([
                    (
                        "base".to_string(),
                        PermissionProfileToml {
                            description: None,
                            extends: None,
                            workspace_roots: None,
                            filesystem: Some(FilesystemPermissionsToml {
                                glob_scan_max_depth: None,
                                entries: BTreeMap::from([(
                                    ":minimal".to_string(),
                                    FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                                )]),
                            }),
                            network: None,
                        },
                    ),
                    (
                        "dev".to_string(),
                        PermissionProfileToml {
                            description: None,
                            extends: Some("base".to_string()),
                            workspace_roots: None,
                            filesystem: None,
                            network: None,
                        },
                    ),
                ]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile {
            id: "dev".to_string(),
            extends: Some("base".to_string()),
        })
    );
    Ok(())
}

#[tokio::test]
async fn permission_profile_override_populates_runtime_permissions() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let permission_profile = PermissionProfile::Disabled;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            permission_profile: Some(permission_profile.clone()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.effective_permission_profile(),
        permission_profile
    );
    assert_eq!(config.permissions.active_permission_profile(), None);
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::DangerFullAccess
    );
    Ok(())
}

#[tokio::test]
async fn permission_profile_override_preserves_managed_unrestricted_filesystem()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let permission_profile = PermissionProfile::Managed {
        file_system: ManagedFileSystemPermissions::Unrestricted,
        network: NetworkSandboxPolicy::Restricted,
    };

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            permission_profile: Some(permission_profile.clone()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.effective_permission_profile(),
        permission_profile
    );
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::ExternalSandbox {
            network_access: NetworkAccess::Restricted,
        }
    );
    Ok(())
}

#[tokio::test]
async fn managed_unrestricted_permission_profile_still_enables_network_requirements()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let permission_profile = PermissionProfile::Managed {
        file_system: ManagedFileSystemPermissions::Unrestricted,
        network: NetworkSandboxPolicy::Enabled,
    };

    let mut config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            permission_profile: Some(permission_profile),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::DangerFullAccess,
        "the legacy projection is intentionally lossy for managed unrestricted profiles"
    );

    let layers = config
        .config_layer_stack
        .all_layers_low_to_high()
        .cloned()
        .collect();
    let mut requirements = config.config_layer_stack.requirements().clone();
    requirements.network = Some(Sourced::new(
        ra_config::NetworkConstraints {
            enabled: Some(true),
            ..Default::default()
        },
        RequirementSource::LegacyManagedConfigTomlFromMdm,
    ));
    let mut requirements_toml = config.config_layer_stack.requirements_toml().clone();
    requirements_toml.network = Some(ra_config::NetworkRequirementsToml {
        enabled: Some(true),
        ..Default::default()
    });
    config.config_layer_stack = ConfigLayerStack::new(layers, requirements, requirements_toml)
        .expect("config layer stack with network requirements");

    assert!(config.managed_network_requirements_enabled());
    Ok(())
}

#[tokio::test]
async fn permission_profile_override_keeps_memories_root_out_of_legacy_projection()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
        ]),
        NetworkSandboxPolicy::Restricted,
    );

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            permission_profile: Some(permission_profile),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let memories_root = codex_home.path().join("memories").abs();
    assert!(
        !config
            .permissions
            .file_system_sandbox_policy()
            .can_write_local_path_with_cwd(memories_root.as_path(), cwd.path())
    );
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        }
    );
    Ok(())
}

#[tokio::test]
async fn permission_profile_override_preserves_configured_network_policy_without_starting_proxy()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let permission_profile = PermissionProfile::Disabled;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            proxy_url: Some("http://127.0.0.1:43128".to_string()),
                            enable_socks5: Some(false),
                            allow_upstream_proxy: Some(false),
                            domains: Some(NetworkDomainPermissionsToml {
                                entries: BTreeMap::from([(
                                    "openai.com".to_string(),
                                    NetworkDomainPermissionToml::Allow,
                                )]),
                            }),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            permission_profile: Some(permission_profile.clone()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;
    assert!(
        config.permissions.network.is_none(),
        "profile network.enabled should not start the managed network proxy"
    );
    assert_eq!(
        config.permissions.effective_permission_profile(),
        permission_profile
    );
    Ok(())
}

#[tokio::test]
async fn workspace_root_glob_none_compiles_to_filesystem_pattern_entry() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    tokio::fs::write(cwd.path().join(".git"), "gitdir: nowhere").await?;
    tokio::fs::write(extra_root.path().join(".git"), "gitdir: nowhere").await?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: Some(2),
                            entries: BTreeMap::from([(
                                ":workspace_roots".to_string(),
                                FilesystemPermissionToml::Scoped(BTreeMap::from([
                                    (".".to_string(), FileSystemAccessMode::Write),
                                    ("**/*.env".to_string(), FileSystemAccessMode::Deny),
                                ])),
                            )]),
                        }),
                        network: None,
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            additional_writable_roots: vec![extra_root.path().to_path_buf()],
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config
            .permissions
            .file_system_sandbox_policy()
            .glob_scan_max_depth,
        Some(2)
    );
    for root in [cwd.path(), extra_root.path()] {
        let expected_pattern = AbsolutePathBuf::resolve_path_against_base("**/*.env", root)
            .to_string_lossy()
            .into_owned();
        assert!(
            config
                .permissions
                .file_system_sandbox_policy()
                .entries
                .contains(&FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: expected_pattern,
                    },
                    access: FileSystemAccessMode::Deny,
                    missing_path_behavior: None,
                })
        );
    }
    assert!(
        !config
            .permissions
            .file_system_sandbox_policy()
            .entries
            .iter()
            .any(|entry| matches!(
                &entry.path,
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots { subpath: Some(subpath) },
                } if subpath == std::path::Path::new("**/*.env")
            )),
        "glob should compile to a filesystem pattern entry, not a literal filesystem entry"
    );
    Ok(())
}

#[tokio::test]
async fn default_permissions_can_select_builtin_profile_without_permissions_table()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert!(config.explicit_permission_profile_mode);
    assert!(config.custom_permission_profiles.is_empty());
    let policy = config.permissions.file_system_sandbox_policy();
    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .as_ref()
            .map(|active| active.id.as_str()),
        Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE)
    );
    assert!(
        policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected :workspace to allow writing the project root, policy: {policy:?}"
    );
    assert!(
        !policy.can_write_local_path_with_cwd(&cwd.path().join(".git"), cwd.path()),
        "expected :workspace to protect project metadata, policy: {policy:?}"
    );
    Ok(())
}

#[tokio::test]
async fn default_permissions_read_only_keeps_add_dir_read_only() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    let extra_root = extra_root.path().abs();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            additional_writable_roots: vec![extra_root.to_path_buf()],
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert!(
        !policy.can_write_local_path_with_cwd(extra_root.as_path(), cwd.path()),
        "expected :read-only to stay read-only for runtime workspace roots, policy: {policy:?}"
    );
    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile::new(
            BUILT_IN_PERMISSION_PROFILE_READ_ONLY,
        ))
    );
    Ok(())
}

#[tokio::test]
async fn workspace_profile_applies_rules_to_runtime_and_profile_workspace_roots()
-> std::io::Result<()> {
    let temp_dir = TempDir::new()?;
    let codex_home = temp_dir.path().join("codex-home");
    let cwd = temp_dir.path().join("frontend");
    let runtime_root = temp_dir.path().join("backend");
    let profile_root = temp_dir.path().join("shared");
    for root in [&cwd, &runtime_root, &profile_root] {
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(root.join(".rusty-agent"))?;
    }

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: Some(WorkspaceRootsToml {
                            entries: BTreeMap::from([(
                                profile_root.to_string_lossy().into_owned(),
                                true,
                            )]),
                        }),
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":workspace_roots".to_string(),
                                FilesystemPermissionToml::Scoped(BTreeMap::from([
                                    (".".to_string(), FileSystemAccessMode::Write),
                                    (".git".to_string(), FileSystemAccessMode::Read),
                                    (".rusty-agent".to_string(), FileSystemAccessMode::Read),
                                ])),
                            )]),
                        }),
                        network: None,
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.clone()),
            additional_writable_roots: vec![runtime_root.clone()],
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let cwd_abs = cwd.abs();
    let runtime_root_abs = runtime_root.abs();
    let profile_root_abs = profile_root.abs();
    assert_eq!(
        config.workspace_roots,
        vec![cwd_abs.clone(), runtime_root_abs.clone()]
    );
    assert_eq!(
        config.permissions.workspace_roots(),
        &[cwd_abs.clone(), runtime_root_abs.clone()]
    );
    assert_eq!(
        config.effective_workspace_roots(),
        vec![
            PathUri::from_abs_path(&cwd_abs),
            PathUri::from_abs_path(&runtime_root_abs),
            PathUri::from_abs_path(&profile_root_abs),
        ]
    );

    let policy = config.permissions.file_system_sandbox_policy();
    for root in [cwd_abs, runtime_root_abs, profile_root_abs.clone()] {
        assert!(
            policy.can_write_local_path_with_cwd(root.as_path(), cwd.as_path()),
            "expected workspace root to be writable, policy: {policy:?}"
        );
        assert!(
            !policy.can_write_local_path_with_cwd(&root.join(".git"), cwd.as_path()),
            "expected .git carveout under {root:?}, policy: {policy:?}"
        );
        assert!(
            !policy.can_write_local_path_with_cwd(&root.join(".rusty-agent"), cwd.as_path()),
            "expected .rusty-agent carveout under {root:?}, policy: {policy:?}"
        );
    }
    assert_eq!(
        config.permissions.profile_workspace_roots(),
        &[profile_root_abs.into()]
    );
    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile::new("dev"))
    );
    Ok(())
}

#[tokio::test]
async fn explicit_builtin_workspace_profile_ignores_legacy_workspace_write_settings()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
            sandbox_workspace_write: Some(SandboxWorkspaceWrite {
                writable_roots: vec![extra_root.path().abs()],
                network_access: true,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
    assert!(
        !policy.entries.iter().any(|entry| matches!(
            &entry.path,
            FileSystemPath::Path { path }
                if path.to_abs_path().is_ok_and(|path| path.as_path() == extra_root.path())
        )),
        "explicit :workspace should not inherit sandbox_workspace_write roots as concrete grants, \
         policy: {policy:?}"
    );
    Ok(())
}

#[tokio::test]
async fn default_permissions_profile_can_extend_builtin_workspace() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("workspace-with-network".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "workspace-with-network".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":tmpdir".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert!(
        policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected profile extending :workspace to keep project-root writes, policy: {policy:?}"
    );
    assert!(
        !policy.can_write_local_path_with_cwd(&cwd.path().join(".git"), cwd.path()),
        "expected profile extending :workspace to keep metadata carveouts, policy: {policy:?}"
    );
    assert!(
        policy.entries.iter().any(|entry| matches!(
            entry,
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::SlashTmp,
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            }
        )),
        "expected profile extending :workspace to keep inherited :slash_tmp writes, policy: {policy:?}"
    );
    assert!(
        policy.entries.iter().any(|entry| matches!(
            entry,
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                access: FileSystemAccessMode::Read,
                missing_path_behavior: None,
            }
        )),
        "expected child :tmpdir read entry to replace the inherited write entry, policy: {policy:?}"
    );
    assert!(
        !policy.entries.iter().any(|entry| matches!(
            entry,
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            }
        )),
        "expected inherited :tmpdir write entry to be removed, policy: {policy:?}"
    );
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile {
            id: "workspace-with-network".to_string(),
            extends: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
        })
    );
    Ok(())
}

#[tokio::test]
async fn default_permissions_profile_can_extend_builtin_read_only() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("read-only-with-network".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "read-only-with-network".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()),
                        workspace_roots: None,
                        filesystem: None,
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert!(
        policy.can_read_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected profile extending :read-only to keep read access, policy: {policy:?}"
    );
    assert!(
        !policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected profile extending :read-only to stay non-writable, policy: {policy:?}"
    );
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile {
            id: "read-only-with-network".to_string(),
            extends: Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()),
        })
    );
    Ok(())
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn empty_config_defaults_to_builtin_profile_for_trusted_project(
    #[case] prefer_mxc: bool,
) -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let project_key = cwd.path().to_string_lossy().to_string();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(FeaturesToml::from(BTreeMap::from([(
                "prefer_mxc".to_string(),
                prefer_mxc,
            )]))),
            projects: Some(HashMap::from([(
                project_key,
                ProjectConfig {
                    trust_level: Some(TrustLevel::Trusted),
                },
            )])),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let mxc_selected = prefer_mxc && native_mxc_available();
    let policy = config.permissions.file_system_sandbox_policy();
    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .as_ref()
            .map(|active| active.id.as_str()),
        Some(if cfg!(target_os = "windows") && !mxc_selected {
            BUILT_IN_PERMISSION_PROFILE_READ_ONLY
        } else {
            BUILT_IN_PERMISSION_PROFILE_WORKSPACE
        })
    );
    if cfg!(target_os = "windows") && !mxc_selected {
        assert!(
            !policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
            "expected trusted project fallback to stay read-only without Windows sandbox support, policy: {policy:?}"
        );
    } else {
        assert!(
            policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
            "expected trusted project fallback to use :workspace, policy: {policy:?}"
        );
        assert!(
            !policy.can_write_local_path_with_cwd(&cwd.path().join(".rusty-agent"), cwd.path()),
            "expected :workspace metadata carveouts, policy: {policy:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn empty_config_defaults_to_builtin_profile_for_untrusted_project() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let project_key = cwd.path().to_string_lossy().to_string();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            projects: Some(HashMap::from([(
                project_key,
                ProjectConfig {
                    trust_level: Some(TrustLevel::Untrusted),
                },
            )])),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .as_ref()
            .map(|active| active.id.as_str()),
        Some(if cfg!(target_os = "windows") {
            BUILT_IN_PERMISSION_PROFILE_READ_ONLY
        } else {
            BUILT_IN_PERMISSION_PROFILE_WORKSPACE
        })
    );
    assert!(
        policy.can_read_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected untrusted project fallback to allow reads, policy: {policy:?}"
    );
    if cfg!(target_os = "windows") {
        assert!(
            !policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
            "expected untrusted project fallback to stay read-only without Windows sandbox support, policy: {policy:?}"
        );
    } else {
        assert!(
            policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
            "expected untrusted project fallback to use :workspace, policy: {policy:?}"
        );
        assert!(
            !policy.can_write_local_path_with_cwd(&cwd.path().join(".rusty-agent"), cwd.path()),
            "expected :workspace metadata carveouts, policy: {policy:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn implicit_builtin_workspace_profile_preserves_sandbox_workspace_write_settings()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    let extra_root = extra_root.path().abs();
    let project_key = cwd.path().to_string_lossy().to_string();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            projects: Some(HashMap::from([(
                project_key,
                ProjectConfig {
                    trust_level: Some(TrustLevel::Trusted),
                },
            )])),
            sandbox_workspace_write: Some(SandboxWorkspaceWrite {
                writable_roots: vec![extra_root.clone()],
                network_access: true,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: false,
            }),
            windows: Some(WindowsToml {
                sandbox: Some(WindowsSandboxModeToml::Elevated),
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert!(
        policy.can_write_local_path_with_cwd(extra_root.as_path(), cwd.path()),
        "expected implicit :workspace to preserve sandbox_workspace_write.writable_roots, policy: {policy:?}"
    );
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    assert_eq!(
        config.permissions.active_permission_profile(),
        None,
        "implicit :workspace cannot be faithfully re-selected when it includes \
         legacy sandbox_workspace_write settings"
    );
    match config.legacy_sandbox_policy() {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            assert!(writable_roots.contains(&extra_root));
            assert!(network_access);
            assert!(exclude_tmpdir_env_var);
            assert!(!exclude_slash_tmp);
        }
        sandbox_policy => panic!("expected workspace-write projection, got {sandbox_policy:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn implicit_builtin_workspace_profile_preserves_add_dir_metadata_carveouts()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    for subpath in [".git", ".agents", ".rusty-agent", ".aws"] {
        std::fs::create_dir_all(extra_root.path().join(subpath))?;
    }
    let project_key = cwd.path().to_string_lossy().to_string();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            projects: Some(HashMap::from([(
                project_key,
                ProjectConfig {
                    trust_level: Some(TrustLevel::Trusted),
                },
            )])),
            windows: Some(WindowsToml {
                sandbox: Some(WindowsSandboxModeToml::Elevated),
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            additional_writable_roots: vec![extra_root.path().to_path_buf()],
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    let extra_root = extra_root.path().abs();
    assert!(
        policy.can_write_local_path_with_cwd(extra_root.as_path(), cwd.path()),
        "expected implicit :workspace to preserve additional writable roots, policy: {policy:?}"
    );
    for subpath in [".git", ".agents", ".rusty-agent", ".aws"] {
        assert!(
            !policy.can_write_local_path_with_cwd(&extra_root.join(subpath), cwd.path()),
            "expected implicit :workspace to preserve legacy metadata carveout for {subpath}, \
             policy: {policy:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn empty_config_defaults_to_builtin_read_only_without_trust_decision() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    let policy = config.permissions.file_system_sandbox_policy();
    assert!(
        policy.can_read_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected :read-only to allow reads, policy: {policy:?}"
    );
    assert!(
        !policy.can_write_local_path_with_cwd(cwd.path(), cwd.path()),
        "expected :read-only to deny writes, policy: {policy:?}"
    );
    Ok(())
}

#[tokio::test]
async fn default_permissions_can_select_builtin_full_access_profile() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS.to_string()),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.effective_permission_profile(),
        PermissionProfile::Disabled
    );
    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .as_ref()
            .map(|active| active.id.as_str()),
        Some(BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS)
    );
    Ok(())
}

#[tokio::test]
async fn legacy_danger_no_sandbox_is_rejected() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let err = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(":danger-no-sandbox".to_string()),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await
    .expect_err("legacy full-access alias should be rejected");

    assert_eq!(
        err.to_string(),
        "default_permissions refers to unknown built-in profile `:danger-no-sandbox`"
    );
    Ok(())
}

#[tokio::test]
async fn user_defined_permission_profile_names_cannot_use_builtin_prefix() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let err = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(":custom".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    ":custom".to_string(),
                    PermissionProfileToml::default(),
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await
    .expect_err("reserved profile name should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "permissions profile `:custom` uses a reserved built-in profile prefix"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_builtin_permission_profile_name_is_rejected() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let err = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some(":unknown".to_string()),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await
    .expect_err("unknown built-in profile name should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "default_permissions refers to unknown built-in profile `:unknown`"
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_allow_direct_write_roots_outside_workspace_root()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;
    let external_write_dir = TempDir::new()?;
    let external_write_path =
        AbsolutePathBuf::from_absolute_path(std::fs::canonicalize(external_write_dir.path())?)?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: Some("Workspace access.".to_string()),
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                external_write_path.to_string_lossy().into_owned(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Write),
                            )]),
                        }),
                        network: None,
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.custom_permission_profiles,
        vec![PermissionProfileCatalogEntry {
            id: "dev".to_string(),
            description: Some("Workspace access.".to_string()),
            allowed: true,
        }]
    );
    assert!(
        config
            .permissions
            .file_system_sandbox_policy()
            .can_write_local_path_with_cwd(external_write_path.as_path(), cwd.path())
    );
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![external_write_path],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        }
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_reject_nested_entries_for_non_workspace_roots() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let err = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Scoped(BTreeMap::from([(
                                    "docs".to_string(),
                                    FileSystemAccessMode::Read,
                                )])),
                            )]),
                        }),
                        network: None,
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await
    .expect_err("nested entries outside :workspace_roots should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "filesystem path `:minimal` does not support nested entries"
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_reject_workspace_root_parent_traversal() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let err = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":workspace_roots".to_string(),
                                FilesystemPermissionToml::Scoped(BTreeMap::from([(
                                    "../sibling".to_string(),
                                    FileSystemAccessMode::Read,
                                )])),
                            )]),
                        }),
                        network: None,
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await
    .expect_err("parent traversal should be rejected for project root subpaths");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "filesystem subpath `../sibling` must be a descendant path without `.` or `..` components"
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_allow_network_enablement() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([(
                    "dev".to_string(),
                    PermissionProfileToml {
                        description: None,
                        extends: None,
                        workspace_roots: None,
                        filesystem: Some(FilesystemPermissionsToml {
                            glob_scan_max_depth: None,
                            entries: BTreeMap::from([(
                                ":minimal".to_string(),
                                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
                            )]),
                        }),
                        network: Some(NetworkToml {
                            enabled: Some(true),
                            ..Default::default()
                        }),
                    },
                )]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert!(
        config.permissions.network_sandbox_policy().is_enabled(),
        "expected network sandbox policy to be enabled",
    );
    assert!(config.legacy_sandbox_policy().has_full_network_access());
    Ok(())
}

#[tokio::test]
async fn runtime_config_resolves_disable_paste_burst() -> anyhow::Result<()> {
    for (toml, expected) in [
        ("", false),
        ("disable_paste_burst = true", true),
        ("disable_paste_burst = true\n[tui]", true),
        ("[tui]\ndisable_paste_burst = true", true),
        (
            "disable_paste_burst = true\n[tui]\ndisable_paste_burst = false",
            false,
        ),
        (
            "disable_paste_burst = false\n[tui]\ndisable_paste_burst = true",
            true,
        ),
    ] {
        let config = Config::load_from_base_config_with_overrides(
            toml::from_str(toml)?,
            ConfigOverrides::default(),
            tempdir()?.abs(),
        )
        .await?;

        assert_eq!(config.disable_paste_burst, expected, "config: {toml}");
    }
    Ok(())
}

#[tokio::test]
async fn disable_paste_burst_preserves_layering_and_new_key_precedence() -> anyhow::Result<()> {
    for (toml, key, value, expected) in [
        (
            "disable_paste_burst = true",
            "disable_paste_burst",
            false,
            false,
        ),
        (
            "disable_paste_burst = true",
            "tui.disable_paste_burst",
            false,
            false,
        ),
        (
            "[tui]\ndisable_paste_burst = true",
            "tui.disable_paste_burst",
            false,
            false,
        ),
        (
            "[tui]\ndisable_paste_burst = true",
            "disable_paste_burst",
            false,
            true,
        ),
        (
            "[tui]\ndisable_paste_burst = false",
            "disable_paste_burst",
            true,
            false,
        ),
    ] {
        let codex_home = TempDir::new()?;
        std::fs::write(codex_home.path().join(CONFIG_TOML_FILE), toml)?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .cli_overrides(vec![(key.to_string(), TomlValue::Boolean(value))])
            .build()
            .await?;

        assert_eq!(
            config.disable_paste_burst, expected,
            "config: {toml}, override: {key}={value}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn runtime_config_resolves_terminal_resize_reflow_defaults_and_overrides() {
    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load default config");

    assert_eq!(
        cfg.terminal_resize_reflow,
        TerminalResizeReflowConfig::default()
    );
    assert_eq!(
        cfg.terminal_resize_reflow.max_rows,
        TerminalResizeReflowMaxRows::Auto
    );

    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml {
            tui: Some(Tui {
                terminal_resize_reflow_max_rows: Some(9000),
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load overridden config");

    assert_eq!(
        cfg.terminal_resize_reflow.max_rows,
        TerminalResizeReflowMaxRows::Limit(9000)
    );

    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml {
            tui: Some(Tui {
                terminal_resize_reflow_max_rows: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config with disabled resize reflow limits");

    assert_eq!(
        cfg.terminal_resize_reflow.max_rows,
        TerminalResizeReflowMaxRows::Disabled
    );
}

#[tokio::test]
async fn legacy_remote_thread_store_endpoint_is_rejected() {
    let cfg: ConfigToml =
        toml::from_str(r#"experimental_thread_store_endpoint = "https://example.com""#)
            .expect("legacy remote thread-store endpoint should still deserialize");

    let err = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect_err("legacy remote thread-store endpoint should be rejected at load time");

    assert!(
        err.to_string()
            .contains("experimental_thread_store_endpoint")
    );
    assert!(err.to_string().contains("no longer supported"));
}

#[tokio::test]
async fn runtime_config_resolves_session_picker_view_default_and_override() {
    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load default config");

    assert_eq!(cfg.tui_session_picker_view, SessionPickerViewMode::Dense);

    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml {
            tui: Some(Tui {
                session_picker_view: Some(SessionPickerViewMode::Comfortable),
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load root override config");

    assert_eq!(
        cfg.tui_session_picker_view,
        SessionPickerViewMode::Comfortable
    );
}

#[tokio::test]
async fn runtime_config_resolves_resume_cwd_default_and_override() {
    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load default config");

    assert_eq!(cfg.tui_resume_cwd, None);

    let cfg = Config::load_from_base_config_with_overrides(
        ConfigToml {
            tui: Some(Tui {
                resume_cwd: Some(ResumeCwdMode::Session),
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load root override config");

    assert_eq!(cfg.tui_resume_cwd, Some(ResumeCwdMode::Session));
}

#[tokio::test]
async fn legacy_sandbox_mode_builds_profiles_with_compatible_projection() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let extra_root = test_absolute_path("/tmp/legacy-extra-root");
    let cases = vec![
        (
            "danger-full-access".to_string(),
            r#"sandbox_mode = "danger-full-access"
"#
            .to_string(),
        ),
        (
            "read-only".to_string(),
            r#"sandbox_mode = "read-only"
"#
            .to_string(),
        ),
        (
            "workspace-write".to_string(),
            format!(
                r#"sandbox_mode = "workspace-write"

[sandbox_workspace_write]
writable_roots = [{}]
exclude_tmpdir_env_var = true
exclude_slash_tmp = true
"#,
                serde_json::json!(extra_root)
            ),
        ),
    ];

    for (name, config_toml) in cases {
        let cfg = toml::from_str::<ConfigToml>(&config_toml)
            .unwrap_or_else(|err| panic!("case `{name}` should parse: {err}"));
        let config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                ..Default::default()
            },
            codex_home.abs(),
        )
        .await?;

        let sandbox_policy = config.legacy_sandbox_policy();
        let file_system_policy = config.permissions.file_system_sandbox_policy();
        let network_policy = config.permissions.network_sandbox_policy();

        assert_eq!(
            network_policy,
            NetworkSandboxPolicy::from(&sandbox_policy),
            "case `{name}` should preserve network semantics from legacy config"
        );
        assert_eq!(
            file_system_policy
                .to_legacy_sandbox_policy(network_policy, cwd.path())
                .unwrap_or_else(|err| panic!("case `{name}` should round-trip: {err}")),
            sandbox_policy,
            "case `{name}` should preserve its legacy compatibility projection"
        );

        match name.as_str() {
            "danger-full-access" | "read-only" => {
                assert_eq!(
                    file_system_policy,
                    FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
                        &sandbox_policy,
                        cwd.path()
                    ),
                    "case `{name}` should match the legacy filesystem projection exactly"
                );
            }
            "workspace-write" => {
                if cfg!(target_os = "windows") {
                    assert_eq!(
                        sandbox_policy,
                        SandboxPolicy::new_read_only_policy(),
                        "legacy workspace-write should keep the existing Windows downgrade when \
                         the experimental Windows sandbox is disabled"
                    );
                    assert_eq!(
                        file_system_policy,
                        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
                            &sandbox_policy,
                            cwd.path()
                        ),
                        "downgraded workspace-write should match the legacy read-only projection"
                    );
                    continue;
                }
                assert_eq!(
                    config.permissions.workspace_roots(),
                    &[cwd.abs(), extra_root.clone()]
                );
                assert!(
                    file_system_policy
                        .entries
                        .contains(&FileSystemSandboxEntry {
                            path: cwd.abs().into(),
                            access: FileSystemAccessMode::Write,
                            missing_path_behavior: None,
                        })
                );
                assert!(
                    file_system_policy
                        .entries
                        .contains(&FileSystemSandboxEntry {
                            path: FileSystemPath::Path {
                                path: extra_root.clone().into(),
                            },
                            access: FileSystemAccessMode::Write,
                            missing_path_behavior: None,
                        })
                );
                for subpath in [".git", ".agents", ".rusty-agent", ".aws"] {
                    assert!(
                        file_system_policy
                            .entries
                            .contains(&FileSystemSandboxEntry {
                                path: FileSystemPath::Path {
                                    path: AbsolutePathBuf::resolve_path_against_base(
                                        subpath,
                                        cwd.path()
                                    )
                                    .into(),
                                },
                                access: FileSystemAccessMode::Read,
                                missing_path_behavior: Some(
                                    ra_filesystem::permissions::FileSystemSandboxEntryMissingPathBehavior::Skip,
                                ),
                            }),
                        "case `{name}` should materialize `{subpath}` for the runtime workspace \
                         root"
                    );
                }
            }
            _ => unreachable!("unexpected test case `{name}`"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn add_dir_override_extends_workspace_writable_roots() -> std::io::Result<()> {
    let temp_dir = TempDir::new()?;
    let frontend = temp_dir.path().join("frontend");
    let backend = temp_dir.path().join("backend");
    std::fs::create_dir_all(&frontend)?;
    std::fs::create_dir_all(&backend)?;

    let overrides = ConfigOverrides {
        cwd: Some(frontend),
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        additional_writable_roots: vec![PathBuf::from("../backend"), backend.clone()],
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        overrides,
        temp_dir.path().abs(),
    )
    .await?;

    let expected_backend = backend.abs();
    if cfg!(target_os = "windows") {
        match &config.legacy_sandbox_policy() {
            SandboxPolicy::ReadOnly { .. } => {}
            other => panic!("expected read-only policy on Windows, got {other:?}"),
        }
    } else {
        match &config.legacy_sandbox_policy() {
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
                assert_eq!(
                    writable_roots
                        .iter()
                        .filter(|root| **root == expected_backend)
                        .count(),
                    1,
                    "expected single writable root entry for {}",
                    expected_backend.display()
                );
            }
            other => panic!("expected workspace-write policy, got {other:?}"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn sqlite_home_defaults_to_codex_home_for_workspace_write() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.sqlite.home(), codex_home.path());

    Ok(())
}

#[tokio::test]
async fn workspace_write_includes_configured_writable_root_once_without_memories_root()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let memories_root = codex_home.path().join("memories");
    let writable_root = codex_home.path().join("writable").abs();
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            sandbox_workspace_write: Some(SandboxWorkspaceWrite {
                writable_roots: vec![writable_root.clone(), writable_root.clone()],
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides {
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    if cfg!(target_os = "windows") {
        match &config.legacy_sandbox_policy() {
            SandboxPolicy::ReadOnly { .. } => {}
            other => panic!("expected read-only policy on Windows, got {other:?}"),
        }
    } else {
        assert!(
            !memories_root.exists(),
            "expected config load not to create memories root at {}",
            memories_root.display()
        );
        let expected_memories_root = memories_root.abs();
        match &config.legacy_sandbox_policy() {
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
                assert!(!writable_roots.contains(&expected_memories_root));
                assert_eq!(
                    writable_roots
                        .iter()
                        .filter(|root| **root == writable_root)
                        .count(),
                    1,
                    "expected single writable root entry for {}",
                    writable_root.display()
                );
            }
            other => panic!("expected workspace-write policy, got {other:?}"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn memory_tool_makes_memories_root_readable_without_creating_or_widening_writes()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let memories_root = codex_home.path().join("memories");
    let memories_root_abs = memories_root.abs();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(FeaturesToml::from(BTreeMap::from([(
                "memories".to_string(),
                true,
            )]))),
            sandbox_workspace_write: Some(SandboxWorkspaceWrite {
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
                ..Default::default()
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert!(
        !memories_root.exists(),
        "expected config load not to create memories root at {}",
        memories_root.display()
    );
    let file_system_policy = config.permissions.file_system_sandbox_policy();
    assert!(
        file_system_policy.can_read_local_path_with_cwd(memories_root_abs.as_path(), cwd.path())
    );
    assert!(
        !file_system_policy.can_write_local_path_with_cwd(memories_root_abs.as_path(), cwd.path())
    );

    if cfg!(target_os = "windows") {
        match &config.legacy_sandbox_policy() {
            SandboxPolicy::ReadOnly { .. } => {}
            other => panic!("expected read-only policy on Windows, got {other:?}"),
        }
    } else {
        match &config.legacy_sandbox_policy() {
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
                assert!(!writable_roots.contains(&memories_root_abs));
            }
            other => panic!("expected workspace-write policy, got {other:?}"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn config_resolves_default_oauth_store_mode() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml::default();

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_oauth_credentials_store_mode,
        resolve_mcp_oauth_credentials_store_mode(
            OAuthCredentialsStoreMode::Auto,
            config_package_version(),
        ),
    );

    Ok(())
}

#[tokio::test]
async fn feedback_enabled_defaults_to_true() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        feedback: Some(FeedbackConfigToml::default()),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(config.feedback_enabled);

    Ok(())
}

#[tokio::test]
async fn project_profiles_are_ignored() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let workspace_key = workspace.path().to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"
[projects."{workspace_key}"]
trust_level = "trusted"
"#,
        ),
    )?;
    let project_config_dir = workspace.path().join(".rusty-agent");
    std::fs::create_dir_all(&project_config_dir)?;
    std::fs::write(
        project_config_dir.join(CONFIG_TOML_FILE),
        r#"
profile = "project"

[profiles.project]
model = "gpt-project-local"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(workspace.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(config.model, None);
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("profile")
                && warning.contains("profiles")
                && warning.contains(
                    "If you want these settings to apply, manually set them in your user-level config.toml."
                )
        }),
        "expected warning for ignored project-local profile keys: {:?}",
        config.startup_warnings
    );

    Ok(())
}

#[tokio::test]
async fn feature_table_overrides_legacy_flags() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let mut entries = BTreeMap::new();
    entries.insert("apply_patch_freeform".to_string(), false);
    let cfg = ConfigToml {
        features: Some(FeaturesToml::from(entries)),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(!config.features.enabled(Feature::ApplyPatchFreeform));

    Ok(())
}

#[tokio::test]
async fn legacy_toggles_map_to_features() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        experimental_use_unified_exec_tool: Some(true),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(config.features.enabled(Feature::UnifiedExec));

    Ok(())
}

#[tokio::test]
async fn legacy_unified_exec_disable_flags_do_not_disable_command_execution() -> std::io::Result<()>
{
    for cfg in [
        ConfigToml {
            features: Some(FeaturesToml::from(BTreeMap::from([(
                "unified_exec".to_string(),
                false,
            )]))),
            ..Default::default()
        },
        ConfigToml {
            experimental_use_unified_exec_tool: Some(false),
            ..Default::default()
        },
    ] {
        let codex_home = TempDir::new()?;
        let mut config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert!(config.features.enabled(Feature::UnifiedExec));
        assert!(config.features.enabled(Feature::ShellTool));

        config
            .features
            .disable(Feature::UnifiedExec)
            .expect("legacy unified-exec toggle should normalize successfully");
        assert!(config.features.enabled(Feature::UnifiedExec));

        config
            .features
            .disable(Feature::ShellTool)
            .expect("shell tool should remain independently configurable");
        assert!(!config.features.enabled(Feature::ShellTool));
        assert!(config.features.enabled(Feature::UnifiedExec));
    }

    Ok(())
}

#[tokio::test]
async fn responses_websocket_features_do_not_change_wire_api() -> std::io::Result<()> {
    for feature_key in ["responses_websockets", "responses_websockets_v2"] {
        let codex_home = TempDir::new()?;
        let mut entries = BTreeMap::new();
        entries.insert(feature_key.to_string(), true);
        let cfg = ConfigToml {
            features: Some(FeaturesToml::from(entries)),
            ..Default::default()
        };

        let config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(config.model_provider.wire_api, WireApi::Responses);
    }

    Ok(())
}

#[tokio::test]
async fn config_honors_explicit_file_oauth_store_mode() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        mcp_oauth_credentials_store: Some(OAuthCredentialsStoreMode::File),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_oauth_credentials_store_mode,
        OAuthCredentialsStoreMode::File,
    );

    Ok(())
}

#[tokio::test]
async fn managed_config_overrides_oauth_store_mode() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let managed_path = codex_home.path().join("managed_config.toml");
    let config_path = codex_home.path().join(CONFIG_TOML_FILE);

    std::fs::write(&config_path, "mcp_oauth_credentials_store = \"file\"\n")?;
    std::fs::write(&managed_path, "mcp_oauth_credentials_store = \"keyring\"\n")?;

    let overrides = LoaderOverrides::with_managed_config_path_for_tests(managed_path.clone());

    let cwd = codex_home.path().abs();
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        codex_home.path(),
        Some(cwd),
        &Vec::new(),
        overrides,
        &ra_config::NoopThreadConfigLoader,
    )
    .await?;
    let cfg =
        deserialize_config_toml_with_base(config_layer_stack.effective_config(), codex_home.path())
            .map_err(|e| {
                tracing::error!("Failed to deserialize overridden config: {e}");
                e
            })?;
    assert_eq!(
        cfg.mcp_oauth_credentials_store,
        Some(OAuthCredentialsStoreMode::Keyring),
    );

    let final_config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;
    assert_eq!(
        final_config.mcp_oauth_credentials_store_mode,
        resolve_mcp_oauth_credentials_store_mode(
            OAuthCredentialsStoreMode::Keyring,
            config_package_version(),
        ),
    );

    Ok(())
}

#[tokio::test]
async fn for_config_writes_selected_user_config_file() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let base_config = codex_home.path().join(CONFIG_TOML_FILE);
    let selected_config = codex_home.path().join("work.config.toml");
    tokio::fs::write(&base_config, r#"model_provider = "openai""#).await?;
    tokio::fs::write(&selected_config, r#"model = "gpt-old""#).await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .loader_overrides(LoaderOverrides {
            user_config_path: Some(selected_config.abs()),
            user_config_profile: Some("work".parse().expect("profile-v2 name")),
            ..LoaderOverrides::without_managed_config_for_tests()
        })
        .build()
        .await?;

    ConfigEditsBuilder::for_config_layer_stack(
        &config.config_layer_stack,
        config.codex_home.as_path(),
    )
    .set_model(Some("gpt-new"), Some(ReasoningEffort::High))
    .apply()
    .await?;

    let selected_serialized = tokio::fs::read_to_string(&selected_config).await?;
    let selected: ConfigToml = toml::from_str(&selected_serialized)?;
    assert_eq!(selected.model.as_deref(), Some("gpt-new"));
    assert_eq!(selected.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        tokio::fs::read_to_string(&base_config).await?,
        r#"model_provider = "openai""#
    );

    Ok(())
}

#[tokio::test]
async fn cli_override_sets_compact_prompt() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let overrides = ConfigOverrides {
        compact_prompt: Some("Use the compact override".to_string()),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        overrides,
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.compact_prompt.as_deref(),
        Some("Use the compact override")
    );

    Ok(())
}

#[tokio::test]
async fn loads_compact_prompt_from_file() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = codex_home.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;

    let prompt_path = workspace.join("compact_prompt.txt");
    std::fs::write(&prompt_path, "  summarize differently  ")?;

    let cfg = ConfigToml {
        experimental_compact_prompt_file: Some(prompt_path.abs()),
        ..Default::default()
    };

    let overrides = ConfigOverrides {
        cwd: Some(workspace),
        ..Default::default()
    };

    let config =
        Config::load_from_base_config_with_overrides(cfg, overrides, codex_home.abs()).await?;

    assert_eq!(
        config.compact_prompt.as_deref(),
        Some("summarize differently")
    );

    Ok(())
}

#[tokio::test]
async fn load_config_uses_requirements_guardian_policy_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config_layer_stack = ConfigLayerStack::new(
        Vec::new(),
        Default::default(),
        ra_config::ConfigRequirementsToml {
            guardian_policy_config: Some(
                "  Use the workspace-managed guardian policy.  ".to_string(),
            ),
            ..Default::default()
        },
    )
    .map_err(std::io::Error::other)?;

    let config = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
        config_layer_stack,
    )
    .await?;

    assert_eq!(
        config.guardian_policy_config.as_deref(),
        Some("Use the workspace-managed guardian policy.")
    );

    Ok(())
}

#[rstest::rstest]
#[case("", ra_config::TranscriptFormat::Line)]
#[case(
    "[guardianv2]\ntranscript_mode = 'line'",
    ra_config::TranscriptFormat::Line
)]
#[case(
    "[guardianv2]\ntranscript_mode = 'json'",
    ra_config::TranscriptFormat::Json
)]
#[tokio::test]
async fn load_config_uses_auto_review_guardian_policy_config_and_template(
    #[case] features: &str,
    #[case] expected_mode: ra_config::TranscriptFormat,
) -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        features: Some(toml::from_str(features).unwrap()),
        auto_review: Some(AutoReviewToml {
            circuit_break_action: None,
            policy: Some("  Use the user-configured guardian policy.  ".to_string()),
            extra_policy: Some("  Use the user-configured additional policy.  ".to_string()),
            experimental_policy_template: Some(
                "  Configured template: {{ tenant_policy_config }}  ".to_string(),
            ),
            experimental_conversation_history_prompt: None,
            conversation_history_max_output_tokens: None,
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        (
            config.guardian_transcript_mode,
            config.guardian_policy_config.as_deref(),
            config.guardian_extra_policy.as_deref(),
            config.guardian_policy_template.as_deref(),
        ),
        (
            expected_mode,
            Some("Use the user-configured guardian policy."),
            Some("Use the user-configured additional policy."),
            Some("Configured template: {{ tenant_policy_config }}"),
        )
    );

    Ok(())
}

#[tokio::test]
async fn requirements_guardian_policy_beats_auto_review() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    for (managed_extra, expected_extra) in [
        (
            Some("  Use the managed additional policy.  "),
            "Use the managed additional policy.",
        ),
        (Some("   "), "Use the user-configured additional policy."),
        (None, "Use the user-configured additional policy."),
    ] {
        let config_layer_stack = ConfigLayerStack::new(
            Vec::new(),
            Default::default(),
            ra_config::ConfigRequirementsToml {
                guardian_policy_config: Some("Use the managed guardian policy.".to_string()),
                guardian_extra_policy: managed_extra.map(str::to_owned),
                ..Default::default()
            },
        )
        .map_err(std::io::Error::other)?;
        let cfg = ConfigToml {
            auto_review: Some(AutoReviewToml {
                circuit_break_action: None,
                policy: Some("Use the user-configured guardian policy.".to_string()),
                extra_policy: Some("Use the user-configured additional policy.".to_string()),
                experimental_policy_template: None,
                experimental_conversation_history_prompt: None,
                conversation_history_max_output_tokens: None,
            }),
            ..Default::default()
        };

        let config = Config::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            cfg,
            ConfigOverrides {
                cwd: Some(codex_home.path().to_path_buf()),
                ..Default::default()
            },
            codex_home.abs(),
            config_layer_stack,
        )
        .await?;

        assert_eq!(
            (
                config.guardian_policy_config.as_deref(),
                config.guardian_extra_policy.as_deref(),
            ),
            (
                Some("Use the managed guardian policy."),
                Some(expected_extra),
            )
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_ignores_empty_auto_review_guardian_policy_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        auto_review: Some(AutoReviewToml {
            circuit_break_action: None,
            policy: Some("   ".to_string()),
            extra_policy: Some("   ".to_string()),
            experimental_policy_template: None,
            experimental_conversation_history_prompt: Some(String::new()),
            conversation_history_max_output_tokens: None,
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        (
            config.guardian_policy_config,
            config.guardian_extra_policy,
            config.guardian_conversation_history_prompt,
        ),
        (None, None, None)
    );

    Ok(())
}

#[tokio::test]
async fn load_config_ignores_empty_requirements_guardian_policy_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config_layer_stack = ConfigLayerStack::new(
        Vec::new(),
        Default::default(),
        ra_config::ConfigRequirementsToml {
            guardian_policy_config: Some("   ".to_string()),
            guardian_extra_policy: Some("   ".to_string()),
            ..Default::default()
        },
    )
    .map_err(std::io::Error::other)?;

    let config = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
        config_layer_stack,
    )
    .await?;

    assert_eq!(
        (config.guardian_policy_config, config.guardian_extra_policy),
        (None, None)
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_missing_agent_role_config_file() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let missing_path = codex_home.path().join("agents").join("researcher.toml");
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: Some(missing_path.abs()),
                    nickname_candidates: None,
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("missing role config file should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let message = err.to_string();
    assert!(message.contains("agents.researcher.config_file"));
    assert!(message.contains("must point to an existing file"));

    Ok(())
}

#[tokio::test]
async fn agent_role_relative_config_file_resolves_against_config_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"",
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Hypatia", "Noether"]
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&role_config_path)
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia", "Noether"])
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_relative_config_file_resolves_from_config_layer() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"",
    )
    .await?;
    let layer_config = toml::from_str(
        r#"[agents.researcher]
description = "Research role"
config_file = "./agents/researcher.toml"
"#,
    )
    .expect("agent role layer config should parse");
    let config_layer_stack = ra_config::ConfigLayerStack::new(
        vec![ra_config::ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: codex_home.path().join(CONFIG_TOML_FILE).abs(),
                profile: None,
            },
            layer_config,
        )],
        Default::default(),
        ra_config::ConfigRequirementsToml::default(),
    )
    .map_err(std::io::Error::other)?;

    let config = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
        config_layer_stack,
    )
    .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&role_config_path)
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_file_metadata_overrides_config_toml_metadata() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
description = "Role metadata from file"
nickname_candidates = ["Hypatia"]
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Noether"]
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    let role = config
        .agent_roles
        .get("researcher")
        .expect("researcher role should load");
    assert_eq!(role.description.as_deref(), Some("Role metadata from file"));
    assert_eq!(role.config_file.as_ref(), Some(&role_config_path));
    assert_eq!(
        role.nickname_candidates
            .as_ref()
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia"])
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_file_without_developer_instructions_is_dropped_with_warning()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    let workspace_key = repo_root.path().to_string_lossy().replace('\\', "\\\\");
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[projects."{workspace_key}"]
trust_level = "trusted"
"#
        ),
    )
    .await?;

    let standalone_agents_dir = repo_root.path().join(".rusty-agent").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
name = "researcher"
description = "Role metadata from file"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        standalone_agents_dir.join("reviewer.toml"),
        r#"
name = "reviewer"
description = "Review role"
developer_instructions = "Review carefully"
model = "gpt-5.2"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("must define `developer_instructions`"))
    );

    Ok(())
}

#[tokio::test]
async fn legacy_agent_role_config_file_allows_missing_developer_instructions() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
model = "gpt-5.2"
model_reasoning_effort = "high"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role from config")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&role_config_path)
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_without_description_after_merge_is_dropped_with_warning() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
config_file = "./agents/researcher.toml"

[agents.reviewer]
description = "Review role"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("agent role `researcher` must define a description"))
    );

    Ok(())
}

#[tokio::test]
async fn discovered_agent_role_file_without_name_is_dropped_with_warning() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    let workspace_key = repo_root.path().to_string_lossy().replace('\\', "\\\\");
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[projects."{workspace_key}"]
trust_level = "trusted"
"#
        ),
    )
    .await?;

    let standalone_agents_dir = repo_root.path().join(".rusty-agent").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
description = "Role metadata from file"
developer_instructions = "Research carefully"
"#,
    )
    .await?;
    tokio::fs::write(
        standalone_agents_dir.join("reviewer.toml"),
        r#"
name = "reviewer"
description = "Review role"
developer_instructions = "Review carefully"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("must define a non-empty `name`"))
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_file_name_takes_precedence_over_config_key() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
name = "archivist"
description = "Role metadata from file"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    let role = config
        .agent_roles
        .get("archivist")
        .expect("role should use file-provided name");
    assert_eq!(role.description.as_deref(), Some("Role metadata from file"));
    assert_eq!(role.config_file.as_ref(), Some(&role_config_path));

    Ok(())
}

#[tokio::test]
async fn loads_legacy_split_agent_roles_from_config_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let researcher_path = codex_home.path().join("agents").join("researcher.toml");
    let reviewer_path = codex_home.path().join("agents").join("reviewer.toml");
    tokio::fs::create_dir_all(
        researcher_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &researcher_path,
        "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"",
    )
    .await?;
    tokio::fs::write(
        &reviewer_path,
        "developer_instructions = \"Review carefully\"\nmodel = \"gpt-4.1\"",
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Hypatia", "Noether"]

[agents.reviewer]
description = "Review role"
config_file = "./agents/reviewer.toml"
nickname_candidates = ["Atlas"]
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&researcher_path)
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia", "Noether"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.config_file.as_ref()),
        Some(&reviewer_path)
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Atlas"])
    );

    Ok(())
}

#[tokio::test]
async fn discovers_multiple_standalone_agent_role_files() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    let workspace_key = repo_root.path().to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[projects."{workspace_key}"]
trust_level = "trusted"
"#
        ),
    )?;

    let root_agent = repo_root
        .path()
        .join(".rusty-agent")
        .join("agents")
        .join("root.toml");
    std::fs::create_dir_all(
        root_agent
            .parent()
            .expect("root agent should have a parent directory"),
    )?;
    std::fs::write(
        &root_agent,
        r#"
name = "researcher"
description = "from root"
developer_instructions = "Research carefully"
"#,
    )?;

    let nested_agent = repo_root
        .path()
        .join("packages")
        .join(".rusty-agent")
        .join("agents")
        .join("review")
        .join("nested.toml");
    std::fs::create_dir_all(
        nested_agent
            .parent()
            .expect("nested agent should have a parent directory"),
    )?;
    std::fs::write(
        &nested_agent,
        r#"
name = "reviewer"
description = "from nested"
nickname_candidates = ["Atlas"]
developer_instructions = "Review carefully"
"#,
    )?;

    let sibling_agent = repo_root
        .path()
        .join("packages")
        .join(".rusty-agent")
        .join("agents")
        .join("writer.toml");
    std::fs::create_dir_all(
        sibling_agent
            .parent()
            .expect("sibling agent should have a parent directory"),
    )?;
    std::fs::write(
        &sibling_agent,
        r#"
name = "writer"
description = "from sibling"
nickname_candidates = ["Sagan"]
developer_instructions = "Write carefully"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("from root")
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("from nested")
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Atlas"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.description.as_deref()),
        Some("from sibling")
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Sagan"])
    );

    Ok(())
}

#[tokio::test]
async fn mixed_legacy_and_standalone_agent_role_sources_merge_with_precedence()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    let workspace_key = repo_root.path().to_string_lossy().replace('\\', "\\\\");
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[projects."{workspace_key}"]
trust_level = "trusted"

[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Noether"]

[agents.critic]
description = "Critic role from config"
config_file = "./agents/critic.toml"
nickname_candidates = ["Ada"]
"#
        ),
    )
    .await?;

    let home_agents_dir = codex_home.path().join("agents");
    tokio::fs::create_dir_all(&home_agents_dir).await?;
    tokio::fs::write(
        home_agents_dir.join("researcher.toml"),
        r#"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        home_agents_dir.join("critic.toml"),
        r#"
developer_instructions = "Critique carefully"
model = "gpt-4.1"
"#,
    )
    .await?;

    let standalone_agents_dir = repo_root.path().join(".rusty-agent").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
name = "researcher"
description = "Research role from file"
nickname_candidates = ["Hypatia"]
developer_instructions = "Research from file"
model = "gpt-5-mini"
"#,
    )
    .await?;
    tokio::fs::write(
        standalone_agents_dir.join("writer.toml"),
        r#"
name = "writer"
description = "Writer role from file"
nickname_candidates = ["Sagan"]
developer_instructions = "Write carefully"
model = "gpt-5.2"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role from file")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&standalone_agents_dir.join("researcher.toml"))
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("critic")
            .and_then(|role| role.description.as_deref()),
        Some("Critic role from config")
    );
    assert_eq!(
        config
            .agent_roles
            .get("critic")
            .and_then(|role| role.config_file.as_ref()),
        Some(&home_agents_dir.join("critic.toml"))
    );
    assert_eq!(
        config
            .agent_roles
            .get("critic")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Ada"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.description.as_deref()),
        Some("Writer role from file")
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Sagan"])
    );

    Ok(())
}

#[tokio::test]
async fn higher_precedence_agent_role_can_inherit_description_from_lower_layer()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    let workspace_key = repo_root.path().to_string_lossy().replace('\\', "\\\\");
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[projects."{workspace_key}"]
trust_level = "trusted"

[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
"#
        ),
    )
    .await?;

    let home_agents_dir = codex_home.path().join("agents");
    tokio::fs::create_dir_all(&home_agents_dir).await?;
    tokio::fs::write(
        home_agents_dir.join("researcher.toml"),
        r#"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;

    let standalone_agents_dir = repo_root.path().join(".rusty-agent").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
name = "researcher"
nickname_candidates = ["Hypatia"]
developer_instructions = "Research from file"
model = "gpt-5-mini"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role from config")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&standalone_agents_dir.join("researcher.toml"))
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia"])
    );

    Ok(())
}

#[tokio::test]
async fn load_config_resolves_agent_controls() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: Some(false),
            max_depth: Some(2),
            default_subagent_model: Some("gpt-5.6-terra".to_string()),
            default_subagent_reasoning_effort: Some(ReasoningEffort::High),
            interrupt_message: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        (
            config.agents_enabled,
            config.agent_max_depth,
            config.agent_default_subagent_model.as_deref(),
            config.agent_default_subagent_reasoning_effort,
            config.agent_interrupt_message_enabled,
        ),
        (
            false,
            2,
            Some("gpt-5.6-terra"),
            Some(ReasoningEffort::High),
            false,
        )
    );

    Ok(())
}

#[tokio::test]
async fn load_config_normalizes_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(vec![
                        "  Hypatia  ".to_string(),
                        "Noether".to_string(),
                    ]),
                },
            )]),
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia", "Noether"])
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_empty_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(Vec::new()),
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("empty nickname candidates should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string()
            .contains("agents.researcher.nickname_candidates")
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_duplicate_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(vec!["Hypatia".to_string(), " Hypatia ".to_string()]),
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("duplicate nickname candidates should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string()
            .contains("agents.researcher.nickname_candidates cannot contain duplicates")
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_unsafe_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(vec!["Agent <One>".to_string()]),
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("unsafe nickname candidates should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains(
            "agents.researcher.nickname_candidates may only contain ASCII letters, digits, spaces, hyphens, and underscores"
        ));

    Ok(())
}

#[tokio::test]
async fn model_catalog_json_loads_from_path() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let catalog_path = codex_home.path().join("catalog.json");
    let mut catalog = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    catalog.models = catalog.models.into_iter().take(1).collect();
    std::fs::write(
        &catalog_path,
        serde_json::to_string(&catalog).expect("serialize catalog"),
    )?;

    let cfg = ConfigToml {
        model_catalog_json: Some(catalog_path.abs()),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.model_catalog, Some(catalog));
    Ok(())
}

#[tokio::test]
async fn model_catalog_json_rejects_empty_catalog() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let catalog_path = codex_home.path().join("catalog.json");
    std::fs::write(&catalog_path, r#"{"models":[]}"#)?;

    let cfg = ConfigToml {
        model_catalog_json: Some(catalog_path.abs()),
        ..Default::default()
    };

    let err = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await
    .expect_err("empty custom catalog should fail config load");

    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("must contain at least one model"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn legacy_profile_selection_is_rejected() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.profile = Some("gpt3".to_string());

    let err = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await
    .expect_err("legacy profile selection should be rejected");

    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(
        err.to_string()
            .contains("legacy `profile = \"gpt3\"` config is no longer supported"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn metrics_exporter_defaults_to_statsig_when_missing() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.otel.metrics_exporter, OtelExporterKind::Statsig);
    assert!(!config.otel.agent_response_logging_enabled());
    assert!(!config.otel.guardian_assessment_logging_enabled());
    Ok(())
}

#[tokio::test]
async fn trace_exporter_defaults_to_none_when_log_exporter_is_set() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;
    let mut cfg = fixture.cfg.clone();
    cfg.otel = Some(OtelConfigToml {
        tool_result: toml::from_str("max_bytes = 8192").expect("tool-result logging config"),
        log_agent_responses: Some(true),
        log_guardian_assessments: Some(true),
        exporter: Some(OtelExporterKind::OtlpHttp {
            endpoint: "http://localhost:14318/v1/logs".to_string(),
            headers: HashMap::new(),
            protocol: ra_config::types::OtelHttpProtocol::Binary,
            tls: None,
        }),
        metrics_exporter: Some(OtelExporterKind::None),
        ..Default::default()
    });

    let mut config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.otel.tool_result.max_bytes, 8192);
    assert!(config.otel.agent_response_logging_enabled());
    assert!(config.otel.guardian_assessment_logging_enabled());
    assert!(matches!(
        config.otel.exporter,
        OtelExporterKind::OtlpHttp { .. }
    ));
    assert_eq!(config.otel.trace_exporter, OtelExporterKind::None);
    config.otel.exporter = OtelExporterKind::None;
    assert!(!config.otel.guardian_assessment_logging_enabled());
    Ok(())
}

#[tokio::test]
async fn load_config_applies_otel_trace_metadata() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg = toml::from_str(
        r#"
[otel.span_attributes]
"example.trace_attr" = "enabled"

[otel.tracestate.example]
alpha = "one"
beta = "two"
"#,
    )
    .expect("TOML deserialization should succeed");

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.otel.span_attributes,
        BTreeMap::from([("example.trace_attr".to_string(), "enabled".to_string())])
    );
    assert_eq!(
        config.otel.tracestate,
        BTreeMap::from([(
            "example".to_string(),
            BTreeMap::from([
                ("alpha".to_string(), "one".to_string()),
                ("beta".to_string(), "two".to_string()),
            ]),
        )])
    );
    Ok(())
}

#[tokio::test]
async fn load_config_drops_invalid_otel_trace_metadata_entries() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg = toml::from_str(
        r#"
[otel]
environment = "test"

[otel.span_attributes]
"" = "missing-key"
"example.trace_attr" = "enabled"

[otel.tracestate.example]
alpha = "one"
beta = "two\ntoo"

[otel.tracestate.bad]
alpha = "one\ntwo"
"#,
    )
    .expect("TOML deserialization should succeed");

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.otel.environment, "test");
    assert_eq!(
        config.otel.span_attributes,
        BTreeMap::from([("example.trace_attr".to_string(), "enabled".to_string())])
    );
    assert_eq!(
        config.otel.tracestate,
        BTreeMap::from([(
            "example".to_string(),
            BTreeMap::from([("alpha".to_string(), "one".to_string())]),
        )])
    );
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Ignoring invalid `otel.span_attributes` config")
                && warning.contains("configured span attribute key must not be empty")
        }),
        "{:?}",
        config.startup_warnings
    );
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Ignoring invalid `otel.tracestate` config")
                && warning.contains("invalid configured tracestate value for example.beta")
        }),
        "{:?}",
        config.startup_warnings
    );
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Ignoring invalid `otel.tracestate` config")
                && warning.contains("invalid configured tracestate value for bad.alpha")
        }),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn explicit_null_service_tier_override_maps_to_default_service_tier() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            service_tier: Some(None),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())
    );
    assert_eq!(config.notices.fast_default_opt_out, None);
    Ok(())
}

#[tokio::test]
async fn default_service_tier_override_uses_default_request_value() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            service_tier: Some(Some("default".to_string())),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())
    );
    Ok(())
}

#[tokio::test]
async fn legacy_fast_service_tier_override_uses_priority_request_value() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            service_tier: Some(Some("fast".to_string())),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    Ok(())
}

#[tokio::test]
async fn config_toml_priority_service_tier_uses_priority_request_value() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.service_tier = Some(ServiceTier::Fast.request_value().to_string());
    let cwd = fixture.cwd_path();
    let codex_home = fixture.codex_home();

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg,
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home,
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    Ok(())
}

#[tokio::test]
async fn config_toml_service_tier_accepts_arbitrary_string() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.service_tier = Some("experimental-tier-id".to_string());
    let cwd = fixture.cwd_path();
    let codex_home = fixture.codex_home();

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg,
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home,
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some("experimental-tier-id".to_string())
    );
    Ok(())
}

#[tokio::test]
async fn config_toml_legacy_fast_service_tier_uses_priority_request_value() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.service_tier = Some("fast".to_string());
    let cwd = fixture.cwd_path();
    let codex_home = fixture.codex_home();

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg,
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home,
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    Ok(())
}

#[tokio::test]
async fn fast_default_opt_out_notice_config_is_respected() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;
    let mut cfg = fixture.cfg.clone();
    cfg.notice = Some(Notice {
        fast_default_opt_out: Some(true),
        ..Default::default()
    });

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.service_tier, None);
    assert_eq!(config.notices.fast_default_opt_out, Some(true));
    Ok(())
}

#[tokio::test]
async fn test_requirements_web_search_mode_allowlist_does_not_warn_when_unset() -> anyhow::Result<()>
{
    let fixture = create_test_fixture()?;

    let requirements_toml = ra_config::ConfigRequirementsToml {
        model_provider: None,
        model_providers: None,
        chatgpt_base_url: None,
        sqlite_home: None,
        log_dir: None,
        model_catalog_json: None,
        check_for_update_on_startup: None,
        allow_login_shell: None,
        feedback: None,
        allowed_approval_policies: None,
        allowed_approvals_reviewers: None,
        allowed_sandbox_modes: None,
        allowed_permission_profiles: None,
        default_permissions: None,
        remote_sandbox_config: None,
        allowed_web_search_modes: Some(vec![ra_config::WebSearchModeRequirement::Cached]),
        application: None,
        allow_managed_hooks_only: None,
        allow_appshots: None,
        allow_remote_control: None,
        allow_browser_and_computer_use: None,
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
        permissions: None,
        auto_review: None,
        models: None,
        additional_developer_instructions: None,
        guardian_policy_config: None,
        guardian_extra_policy: None,
    };
    let requirement_source = ra_config::RequirementSource::Unknown;
    let requirement_source_for_error = requirement_source.clone();
    let allowed = vec![WebSearchMode::Disabled, WebSearchMode::Cached];
    let constrained = Constrained::new(WebSearchMode::Cached, move |candidate| {
        if matches!(candidate, WebSearchMode::Cached | WebSearchMode::Disabled) {
            Ok(())
        } else {
            Err(ConstraintError::InvalidValue {
                field_name: "web_search_mode",
                candidate: format!("{candidate:?}"),
                allowed: format!("{allowed:?}"),
                requirement_source: requirement_source_for_error.clone(),
            })
        }
    })?;
    let requirements = ra_config::ConfigRequirements {
        web_search_mode: ra_config::ConstrainedWithSource::new(
            constrained,
            Some(requirement_source),
        ),
        ..Default::default()
    };
    let config_layer_stack =
        ra_config::ConfigLayerStack::new(Vec::new(), requirements, requirements_toml)
            .expect("config layer stack");

    let config = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
        config_layer_stack,
    )
    .await?;

    assert!(
        !config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("Configured value for `web_search_mode`")),
        "{:?}",
        config.startup_warnings
    );

    Ok(())
}

#[tokio::test]
async fn test_load_config_rejects_legacy_ollama_chat_provider_with_helpful_error()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        model_provider: Some(LEGACY_OLLAMA_CHAT_PROVIDER_ID.to_string()),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        error
            .to_string()
            .contains(OLLAMA_CHAT_PROVIDER_REMOVED_ERROR)
    );

    Ok(())
}

#[tokio::test]
async fn config_loads_mcp_oauth_callback_port_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let toml = r#"
model = "gpt-5.4"
mcp_oauth_callback_port = 5678
"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for callback port");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.mcp_oauth_callback_port, Some(5678));
    Ok(())
}

#[tokio::test]
async fn config_loads_allow_login_shell_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg: ConfigToml = toml::from_str(
        r#"
model = "gpt-5.4"
allow_login_shell = false
"#,
    )
    .expect("TOML deserialization should succeed for allow_login_shell");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(!config.permissions.allow_login_shell);
    Ok(())
}

#[tokio::test]
async fn config_loads_apps_mcp_product_sku_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let toml = r#"
model = "gpt-5.4"
apps_mcp_product_sku = "tpp"
"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for apps MCP SKU");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.apps_mcp_product_sku.as_deref(), Some("tpp"));
    Ok(())
}

#[tokio::test]
async fn config_loads_cloud_skills_with_legacy_noop() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    for (settings, expected) in [
        ("", true),
        ("[cloud.skills]", true),
        ("[cloud.skills]\nenabled = false", false),
        ("[cloud.skills]\nenabled = true", true),
        ("[orchestrator.skills]\nenabled = false", true),
        ("[orchestrator.skills]\nenabled = true", true),
        (
            "[orchestrator.skills]\nenabled = false\n[cloud.skills]",
            true,
        ),
        (
            "[orchestrator.skills]\nenabled = true\n[cloud.skills]\nenabled = false",
            false,
        ),
        (
            "[orchestrator.skills]\nenabled = false\n[cloud.skills]\nenabled = true",
            true,
        ),
    ] {
        let cfg: ConfigToml = toml::from_str(&format!(
            "model = \"gpt-5.4\"\n{settings}\n[orchestrator.mcp]\nenabled = false"
        ))
        .expect("cloud skill settings should deserialize");
        let config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(
            (config.cloud_skill_enabled, config.orchestrator_mcp_enabled),
            (expected, false),
            "{settings}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn config_loads_orchestrator_settings_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg: ConfigToml = toml::from_str(
        r#"
model = "gpt-5.4"

[orchestrator.skills]
enabled = false

[orchestrator.mcp]
enabled = false
"#,
    )
    .expect("TOML deserialization should succeed for orchestrator settings");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        (config.cloud_skill_enabled, config.orchestrator_mcp_enabled),
        (true, false)
    );
    Ok(())
}

#[tokio::test]
async fn config_loads_mcp_oauth_callback_url_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let toml = r#"
model = "gpt-5.4"
mcp_oauth_callback_url = "https://example.com/callback"
"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for callback URL");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_oauth_callback_url.as_deref(),
        Some("https://example.com/callback")
    );
    Ok(())
}

#[tokio::test]
async fn untrusted_parent_repo_with_incomplete_child_git_keeps_unless_trusted_approval_policy()
-> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let repo = TempDir::new()?;
    let cwd = repo.path().join("sub");
    std::fs::create_dir_all(repo.path().join(".git"))?;
    std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(cwd.join(".git"))?;

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            projects: Some(HashMap::from([(
                repo.path().to_string_lossy().to_string(),
                ProjectConfig {
                    trust_level: Some(TrustLevel::Untrusted),
                },
            )])),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::UnlessTrusted
    );
    Ok(())
}

#[tokio::test]
async fn test_untrusted_project_gets_unless_trusted_approval_policy() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let test_project_dir = TempDir::new()?;
    let test_path = test_project_dir.path();

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            projects: Some(HashMap::from([(
                test_path.to_string_lossy().to_string(),
                ProjectConfig {
                    trust_level: Some(TrustLevel::Untrusted),
                },
            )])),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(test_path.to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    // Verify that untrusted projects get the internal always-prompt policy.
    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::UnlessTrusted,
        "Expected UnlessTrusted approval policy for untrusted project"
    );

    // Verify that untrusted projects still get WorkspaceWrite sandbox (or ReadOnly on Windows)
    if cfg!(target_os = "windows") {
        assert!(
            matches!(
                &config.legacy_sandbox_policy(),
                SandboxPolicy::ReadOnly { .. }
            ),
            "Expected ReadOnly on Windows"
        );
    } else {
        assert!(
            matches!(
                &config.legacy_sandbox_policy(),
                SandboxPolicy::WorkspaceWrite { .. }
            ),
            "Expected WorkspaceWrite sandbox for untrusted project"
        );
    }

    Ok(())
}

#[tokio::test]
async fn oversized_managed_developer_instructions_are_rejected_during_config_load()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    // The policy text alone fits; rendering its required context markers must not.
    let instructions = "x".repeat(10_000_usize.saturating_mul(4));

    let error = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(format!(
                "additional_developer_instructions = {instructions:?}"
            )),
        )
        .build()
        .await
        .expect_err("oversized managed instructions must fail during config loading");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let message = error.to_string();
    assert!(
        message.starts_with(
            "`additional_developer_instructions` from enterprise-managed requirements Base requirements (req_1) exceeds the model-context limit of 10000 estimated tokens"
        ),
        "unexpected config-load error: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn requirements_disallowing_default_sandbox_falls_back_to_required_default()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_sandbox_modes = ["read-only"]"#,
            ),
        )
        .build()
        .await?;
    assert_eq!(
        config.legacy_sandbox_policy(),
        SandboxPolicy::new_read_only_policy()
    );
    Ok(())
}

#[tokio::test]
async fn explicit_sandbox_mode_falls_back_when_disallowed_by_requirements() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"sandbox_mode = "danger-full-access"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_sandbox_modes = ["read-only"]"#,
            ),
        )
        .build()
        .await?;
    assert_eq!(
        config.legacy_sandbox_policy(),
        SandboxPolicy::new_read_only_policy()
    );
    Ok(())
}

#[tokio::test]
async fn windows_sandbox_mode_falls_back_when_disallowed_by_requirements() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[windows]
sandbox = "unelevated"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"[windows]
allowed_sandbox_implementations = ["elevated"]
"#,
            ),
        )
        .build()
        .await?;

    assert_eq!(
        config.permissions.windows_sandbox_mode,
        Some(ra_config::types::WindowsSandboxModeToml::Elevated)
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning
            .contains("Configured value for `windows.sandbox` is disallowed by requirements")),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn danger_full_access_with_never_is_rejected_when_requirements_force_read_only()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"approval_policy = "never"
sandbox_mode = "danger-full-access"
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_sandbox_modes = ["read-only"]"#,
            ),
        )
        .build()
        .await
        .expect_err("requirements-constrained yolo should require sandbox approval");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "`approval_policy = \"never\"` cannot be used because requirements do not allow `sandbox_mode = \"danger-full-access\"`; Codex would fall back to read-only permissions with approvals disabled. Choose an `approval_policy` based on what you need, such as `on-request`, or choose an allowed sandbox mode."
    );
    Ok(())
}

#[tokio::test]
async fn named_full_access_profile_with_never_is_rejected_when_requirements_force_read_only()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"approval_policy = "never"
default_permissions = "dev"

[permissions.dev.filesystem]
":root" = "write"
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_sandbox_modes = ["read-only"]"#,
            ),
        )
        .build()
        .await
        .expect_err("requirements-constrained full-access profile should require sandbox approval");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "`approval_policy = \"never\"` cannot be used because requirements do not allow `sandbox_mode = \"danger-full-access\"`; Codex would fall back to read-only permissions with approvals disabled. Choose an `approval_policy` based on what you need, such as `on-request`, or choose an allowed sandbox mode."
    );
    Ok(())
}

#[tokio::test]
async fn permission_profile_override_falls_back_when_disallowed_by_requirements()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        "[features.network_proxy]\nenabled = true\nproxy_url = 'http://127.0.0.1:43128'\n",
    )?;
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .harness_overrides(ConfigOverrides {
            permission_profile: Some(PermissionProfile::Disabled),
            ..Default::default()
        })
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_sandbox_modes = ["read-only"]"#,
            ),
        )
        .build()
        .await?;

    let expected_sandbox_policy = SandboxPolicy::new_read_only_policy();
    assert_eq!(config.legacy_sandbox_policy(), expected_sandbox_policy);
    assert_eq!(
        config.permissions.effective_permission_profile(),
        PermissionProfile::read_only()
    );
    // Prepare the proxy while the candidate permits network, before fallback.
    let expected_network = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://127.0.0.1:43128".to_string(),
            ..Default::default()
        },
        /*requirements*/ None,
        &PermissionProfile::read_only(),
    )?;
    assert_eq!(config.permissions.network, Some(expected_network));
    Ok(())
}

#[tokio::test]
async fn active_profile_is_cleared_when_requirements_force_fallback() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    for overrides in [
        ConfigOverrides {
            default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS.to_string()),
            ..Default::default()
        },
        ConfigOverrides {
            persisted_permission_profile_id: Some(
                BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS.to_string(),
            ),
            ..Default::default()
        },
    ] {
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .harness_overrides(overrides)
            .cloud_config_bundle(
                CloudConfigBundleFixture::loader_with_enterprise_requirement(
                    r#"allowed_sandbox_modes = ["read-only"]"#,
                ),
            )
            .build()
            .await?;

        assert_eq!(
            config.permissions.effective_permission_profile(),
            PermissionProfile::read_only()
        );
        assert_eq!(config.permissions.active_permission_profile(), None);
        assert!(
            config
                .startup_warnings
                .iter()
                .any(|warning| warning.contains(
                    "Configured value for `permission_profile` is disallowed by requirements"
                )),
            "{:?}",
            config.startup_warnings
        );
    }
    Ok(())
}

#[tokio::test]
async fn bypass_hook_trust_adds_startup_warning() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .harness_overrides(ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..Default::default()
        })
        .build()
        .await?;

    assert!(
        config.startup_warnings.iter().any(|warning| warning
            == "`--dangerously-bypass-hook-trust` is enabled. Enabled hooks may run without review for this invocation."),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn permission_profile_override_preserves_split_write_roots() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = codex_home.path().join("workspace");
    let outside_root = codex_home.path().join("outside-write");
    std::fs::create_dir_all(&cwd)?;
    std::fs::create_dir_all(&outside_root)?;
    let outside_root =
        AbsolutePathBuf::from_absolute_path(outside_root).expect("outside root is absolute");
    let file_system_sandbox_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
            missing_path_behavior: None,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: outside_root.clone().into(),
            },
            access: FileSystemAccessMode::Write,
            missing_path_behavior: None,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::Managed,
        &file_system_sandbox_policy,
        NetworkSandboxPolicy::Restricted,
    );

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd))
        .harness_overrides(ConfigOverrides {
            permission_profile: Some(permission_profile),
            ..Default::default()
        })
        .build()
        .await?;

    assert!(
        config
            .permissions
            .file_system_sandbox_policy()
            .can_write_local_path_with_cwd(outside_root.as_path(), config.cwd.as_path())
    );
    assert!(matches!(
        &config.legacy_sandbox_policy(),
        SandboxPolicy::WorkspaceWrite { .. }
    ));
    assert_eq!(
        config.permissions.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
    Ok(())
}

#[tokio::test]
async fn requirements_disallowing_default_approval_falls_back_to_required_default()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let workspace_key = workspace.path().to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"
[projects."{workspace_key}"]
trust_level = "untrusted"
"#
        ),
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(workspace.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_approval_policies = ["on-request"]"#,
            ),
        )
        .build()
        .await?;

    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::OnRequest
    );
    Ok(())
}

#[tokio::test]
async fn explicit_untrusted_approval_policy_is_rejected() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"approval_policy = "untrusted"
"#,
    )?;

    let error = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("untrusted approval policy should be rejected");
    assert!(
        error
            .to_string()
            .contains("approval_policy = \"untrusted\" is no longer supported"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn feature_requirements_normalize_effective_feature_values() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
view_image = true
shell_tool = false
use_xaa = true
"#,
            ),
        )
        .build()
        .await?;

    assert!(config.features.enabled(Feature::ViewImage));
    assert!(!config.features.enabled(Feature::ShellTool));
    assert!(config.features.enabled(Feature::UseXaa));
    assert!(
        !config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("Configured value for `features`")),
        "{:?}",
        config.startup_warnings
    );

    Ok(())
}

#[tokio::test]
async fn feature_requirements_can_still_disable_unified_exec() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        "[features]\nuse_xaa = true\n",
    )?;

    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
unified_exec = false
shell_tool = true
use_xaa = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::UnifiedExec));
    assert!(config.features.enabled(Feature::ShellTool));
    assert!(!config.features.enabled(Feature::UseXaa));
    assert!(
        !config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("Ignoring unknown `features` requirement")),
        "{:?}",
        config.startup_warnings
    );

    config
        .features
        .enable(Feature::UnifiedExec)
        .expect("managed feature mutations should normalize successfully");
    assert!(!config.features.enabled(Feature::UnifiedExec));
    assert!(config.features.enabled(Feature::ShellTool));

    Ok(())
}

#[tokio::test]
async fn feature_requirements_auto_review_disables_guardian_approval() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
auto_review = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::GuardianApproval));

    Ok(())
}

#[tokio::test]
async fn browser_feature_requirements_are_valid() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
in_app_browser = false
browser_annotation_api = false
browser_use = false
browser_use_full_cdp_access = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::InAppBrowser));
    assert!(!config.features.enabled(Feature::BrowserAnnotationApi));
    assert!(!config.features.enabled(Feature::BrowserUse));
    assert!(!config.features.enabled(Feature::BrowserUseFullCdpAccess));

    Ok(())
}

#[tokio::test]
async fn in_app_chat_feature_requirements_are_valid() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
in_app_chat = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::InAppChat));

    Ok(())
}

#[tokio::test]
async fn in_app_dictation_feature_requirements_are_valid() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
in_app_dictation = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::InAppDictation));

    Ok(())
}

#[tokio::test]
async fn in_app_local_automation_feature_requirements_are_valid() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
in_app_local_automation = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(!config.features.enabled(Feature::InAppLocalAutomation));

    Ok(())
}

#[tokio::test]
async fn explicit_feature_config_is_normalized_by_requirements() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[features]
view_image = false
shell_tool = true
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
view_image = true
shell_tool = false
"#,
            ),
        )
        .build()
        .await?;

    assert!(config.features.enabled(Feature::ViewImage));
    assert!(!config.features.enabled(Feature::ShellTool));
    assert!(
        !config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("Configured value for `features`")),
        "{:?}",
        config.startup_warnings
    );

    Ok(())
}

#[tokio::test]
async fn approvals_reviewer_defaults_to_manual_only_without_guardian_feature() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::User);
    Ok(())
}

#[tokio::test]
async fn prompt_instruction_blocks_can_be_disabled_from_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"include_permissions_instructions = false
include_apps_instructions = false
include_collaboration_mode_instructions = false
include_environment_context = false
include_environment_context_time = false

[skills]
include_instructions = false
"#,
    )?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert!(!config.include_permissions_instructions);
    assert!(!config.include_apps_instructions);
    assert!(!config.include_collaboration_mode_instructions);
    assert!(!config.include_skill_instructions);
    assert!(!config.include_environment_context);
    assert!(!config.include_environment_context_time);
    Ok(())
}

#[tokio::test]
async fn approvals_reviewer_stays_manual_only_when_guardian_feature_is_enabled()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features]
guardian_approval = true
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::User);
    Ok(())
}

#[tokio::test]
async fn approvals_reviewer_can_be_set_in_config_without_guardian_approval() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"approvals_reviewer = "user"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::User);
    Ok(())
}

#[tokio::test]
async fn requirements_disallowing_default_approvals_reviewer_falls_back_to_required_default()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_approvals_reviewers = ["guardian_subagent"]"#,
            ),
        )
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::AutoReview);
    Ok(())
}

#[tokio::test]
async fn root_approvals_reviewer_falls_back_when_disallowed_by_requirements() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"approvals_reviewer = "user"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_approvals_reviewers = ["guardian_subagent"]"#,
            ),
        )
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::AutoReview);
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning
                .contains("Configured value for `approvals_reviewer` is disallowed by requirements")
        }),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn profile_approvals_reviewer_falls_back_when_disallowed_by_requirements()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let selected_config = codex_home.path().join("default.config.toml");
    std::fs::write(
        &selected_config,
        r#"approvals_reviewer = "user"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides {
            user_config_path: Some(selected_config.abs()),
            user_config_profile: Some("default".parse().expect("profile-v2 name")),
            ..LoaderOverrides::without_managed_config_for_tests()
        })
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_approvals_reviewers = ["guardian_subagent"]"#,
            ),
        )
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::AutoReview);
    Ok(())
}

#[tokio::test]
async fn approvals_reviewer_preserves_valid_user_choice_when_allowed_by_requirements()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"approvals_reviewer = "guardian_subagent"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_approvals_reviewers = ["user", "guardian_subagent"]"#,
            ),
        )
        .build()
        .await?;

    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::AutoReview);
    assert!(
        config
            .startup_warnings
            .iter()
            .all(|warning| !warning.contains("approvals_reviewer")),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn smart_approvals_alias_is_ignored() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features]
smart_approvals = true
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert!(config.features.enabled(Feature::GuardianApproval));
    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::User);

    let serialized = tokio::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).await?;
    assert!(serialized.contains("smart_approvals = true"));
    assert!(!serialized.contains("guardian_approval"));
    assert!(!serialized.contains("approvals_reviewer"));

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_config_from_feature_table() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 5
min_wait_timeout_ms = 2500
max_wait_timeout_ms = 120000
default_wait_timeout_ms = 30000
usage_hint_text = "Custom delegation guidance."
root_agent_usage_hint_text = "Root guidance."
subagent_usage_hint_text = "Subagent guidance."
subagent_developer_instructions = "  Delegate carefully.  "
preserve_fork_prefix = true
multi_agent_mode_hint_text = "Custom mode guidance."
tool_namespace = "agents"
hide_spawn_agent_metadata = true
expose_spawn_agent_model_overrides = false
wait_agent_enabled = false
disable_native_tools = true
disable_direct_message = true
message_board_in_memory = true
non_code_mode_only = true

[agents]
max_concurrent_threads_per_session = 9
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert!(config.features.enabled(Feature::MultiAgentV2));
    assert_eq!(config.multi_agent_v2.max_concurrent_threads_per_session, 5);
    assert_eq!(config.multi_agent_v2.min_wait_timeout_ms, 2500);
    assert_eq!(config.multi_agent_v2.max_wait_timeout_ms, 120000);
    assert_eq!(config.multi_agent_v2.default_wait_timeout_ms, 30000);
    assert_eq!(
        (
            config.agent_max_threads,
            config.effective_agent_max_threads(MultiAgentVersion::V2)
        ),
        (Some(9), Some(4))
    );
    assert_eq!(
        config.multi_agent_v2.usage_hint_text.as_deref(),
        Some("Custom delegation guidance.")
    );
    assert_eq!(
        config.multi_agent_v2.root_agent_usage_hint_text.as_deref(),
        Some("Root guidance.")
    );
    assert_eq!(
        config.multi_agent_v2.subagent_usage_hint_text.as_deref(),
        Some("Subagent guidance.")
    );
    assert_eq!(
        config
            .multi_agent_v2
            .subagent_developer_instructions
            .as_deref(),
        Some("Delegate carefully.")
    );
    assert_eq!(
        config.multi_agent_v2.multi_agent_mode_hint_text.as_deref(),
        Some("Custom mode guidance.")
    );
    assert_eq!(
        config.multi_agent_v2.tool_namespace.as_deref(),
        Some("agents")
    );
    assert!(config.multi_agent_v2.hide_spawn_agent_metadata);
    assert!(config.multi_agent_v2.preserve_fork_prefix);
    assert!(!config.multi_agent_v2.expose_spawn_agent_model_overrides);
    assert!(!config.multi_agent_v2.wait_agent_enabled);
    assert!(config.multi_agent_v2.disable_native_tools);
    assert!(config.multi_agent_v2.disable_direct_message);
    assert!(config.multi_agent_v2.message_board_in_memory);
    assert!(config.multi_agent_v2.non_code_mode_only);

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_default_session_thread_cap_counts_root() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        config.multi_agent_v2,
        resolve_multi_agent_v2_config(&ConfigToml::default())
    );
    assert_eq!(
        (
            config.agent_max_threads,
            config.effective_agent_max_threads(MultiAgentVersion::V2)
        ),
        (None, Some(3))
    );

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_allows_disabled_wait_agent_without_sleep_tool() -> std::io::Result<()> {
    for config_toml in [
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false
"#,
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false

[features.current_time_reminder]
enabled = true
sleep_tool = false
"#,
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false

[features.current_time_reminder]
enabled = false
sleep_tool = true
"#,
    ] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(config_toml).expect("TOML should deserialize");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert!(!config.multi_agent_v2.wait_agent_enabled);
    }

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_uses_agents_max_concurrent_threads_per_session() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true

[agents]
max_concurrent_threads_per_session = 7
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        (
            config.multi_agent_v2.max_concurrent_threads_per_session,
            config.effective_agent_max_threads(MultiAgentVersion::V2),
        ),
        (8, Some(7))
    );

    Ok(())
}

#[tokio::test]
async fn catalog_v2_allows_agents_thread_limit_when_feature_disabled() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = false

[agents]
max_concurrent_threads_per_session = 3
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        (
            config.multi_agent_v2.max_concurrent_threads_per_session,
            config.effective_agent_max_threads(MultiAgentVersion::V2),
        ),
        (4, Some(3))
    );

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_rejects_invalid_wait_timeouts() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 0
max_wait_timeout_ms = 0
default_wait_timeout_ms = 0
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.multi_agent_v2.min_wait_timeout_ms, 0);
    assert_eq!(config.multi_agent_v2.max_wait_timeout_ms, 0);
    assert_eq!(config.multi_agent_v2.default_wait_timeout_ms, 0);

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = -1
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("negative min_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.min_wait_timeout_ms must be at least 0"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 3600001
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("too large min_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.min_wait_timeout_ms must be at most 3600000"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_wait_timeout_ms = -1
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("negative max_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.max_wait_timeout_ms must be at least 0"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_wait_timeout_ms = 3600001
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("too large max_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.max_wait_timeout_ms must be at most 3600000"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
default_wait_timeout_ms = -1
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("negative default_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.default_wait_timeout_ms must be at least 0"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 1000
max_wait_timeout_ms = 500
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("min greater than max should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.min_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 1000
max_wait_timeout_ms = 2000
default_wait_timeout_ms = 500
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("default less than min should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.default_wait_timeout_ms must be at least features.multi_agent_v2.min_wait_timeout_ms"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 1000
max_wait_timeout_ms = 2000
default_wait_timeout_ms = 2500
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("default greater than max should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.default_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms"
    );

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_rejects_invalid_tool_namespace() -> std::io::Result<()> {
    for (namespace, expected_message) in [
        (
            "bad namespace",
            "features.multi_agent_v2.tool_namespace must match ^[a-zA-Z0-9_-]+$",
        ),
        (
            "functions",
            "features.multi_agent_v2.tool_namespace uses a reserved namespace: functions",
        ),
    ] {
        let codex_home = TempDir::new()?;
        std::fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            format!(
                r#"[features.multi_agent_v2]
enabled = true
tool_namespace = "{namespace}"
"#
            ),
        )?;

        let err = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .build()
            .await
            .expect_err("invalid multi_agent_v2 tool namespace should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), expected_message);
    }

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_session_thread_cap_one_disallows_subagents() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 1
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.multi_agent_v2.max_concurrent_threads_per_session, 1);
    assert_eq!(
        (
            config.agent_max_threads,
            config.effective_agent_max_threads(MultiAgentVersion::V2)
        ),
        (None, Some(0))
    );

    Ok(())
}

#[tokio::test]
async fn feature_requirements_normalize_runtime_feature_mutations() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
view_image = true
shell_tool = false
"#,
            ),
        )
        .build()
        .await?;

    let mut requested = config.features.get().clone();
    requested
        .disable(Feature::ViewImage)
        .enable(Feature::ShellTool);
    assert!(config.features.can_set(&requested).is_ok());
    config
        .features
        .set(requested)
        .expect("managed feature mutations should normalize successfully");

    assert!(config.features.enabled(Feature::ViewImage));
    assert!(!config.features.enabled(Feature::ShellTool));

    Ok(())
}

#[tokio::test]
async fn feature_requirements_warn_on_collab_legacy_alias() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
collab = true
"#,
            ),
        )
        .build()
        .await?;

    assert!(config.features.enabled(Feature::Collab));
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Using legacy `features` requirement `collab`")
                && warning.contains("prefer canonical feature key `multi_agent`")
        }),
        "{:?}",
        config.startup_warnings
    );

    Ok(())
}

#[tokio::test]
async fn feature_requirements_warn_and_ignore_unknown_feature() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
made_up_feature = true
"#,
            ),
        )
        .build()
        .await?;

    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning
                .contains("Ignoring unknown `features` requirement `made_up_feature`")),
        "{:?}",
        config.startup_warnings
    );

    Ok(())
}

#[tokio::test]
async fn tool_suggest_discoverables_load_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tool_suggest]
discoverables = [
  { type = "connector", id = "connector_alpha" },
  { type = "plugin", id = "plugin_alpha@openai-curated" },
  { type = "connector", id = "   " }
]
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: vec![
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: "connector_alpha".to_string(),
                },
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Plugin,
                    id: "plugin_alpha@openai-curated".to_string(),
                },
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: "   ".to_string(),
                },
            ],
            disabled_tools: Vec::new(),
        })
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.tool_suggest,
        ToolSuggestConfig {
            discoverables: vec![
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: "connector_alpha".to_string(),
                },
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Plugin,
                    id: "plugin_alpha@openai-curated".to_string(),
                },
            ],
            disabled_tools: Vec::new(),
        }
    );
    Ok(())
}

#[tokio::test]
async fn tool_suggest_disabled_tools_load_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tool_suggest]
disabled_tools = [
  { type = "connector", id = " connector_calendar " },
  { type = "connector", id = "connector_calendar" },
  { type = "connector", id = "   " },
  { type = "plugin", id = "slack@openai-curated" }
]
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: Vec::new(),
            disabled_tools: vec![
                ToolSuggestDisabledTool::connector(" connector_calendar "),
                ToolSuggestDisabledTool::connector("connector_calendar"),
                ToolSuggestDisabledTool::connector("   "),
                ToolSuggestDisabledTool::plugin("slack@openai-curated"),
            ],
        })
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.tool_suggest,
        ToolSuggestConfig {
            discoverables: Vec::new(),
            disabled_tools: vec![
                ToolSuggestDisabledTool::connector("connector_calendar"),
                ToolSuggestDisabledTool::plugin("slack@openai-curated"),
            ],
        }
    );
    Ok(())
}

#[tokio::test]
async fn tool_suggest_disabled_tools_merge_across_config_layers() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let workspace_key = workspace.path().to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"
[projects."{workspace_key}"]
trust_level = "trusted"

[tool_suggest]
disabled_tools = [
  {{ type = "connector", id = " user_connector " }},
  {{ type = "plugin", id = "shared_plugin" }},
  {{ type = "connector", id = "project_connector" }},
]
"#
        ),
    )?;

    let project_config_dir = workspace.path().join(".rusty-agent");
    std::fs::create_dir_all(&project_config_dir)?;
    std::fs::write(
        project_config_dir.join(CONFIG_TOML_FILE),
        r#"
[tool_suggest]
disabled_tools = [
  { type = "connector", id = "project_connector" },
  { type = "plugin", id = "project_plugin" },
  { type = "plugin", id = "shared_plugin" },
]
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(workspace.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.tool_suggest.disabled_tools,
        vec![
            ToolSuggestDisabledTool::connector("user_connector"),
            ToolSuggestDisabledTool::plugin("shared_plugin"),
            ToolSuggestDisabledTool::connector("project_connector"),
            ToolSuggestDisabledTool::plugin("project_plugin"),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn experimental_realtime_start_instructions_load_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
experimental_realtime_start_instructions = "start instructions from config"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.experimental_realtime_start_instructions.as_deref(),
        Some("start instructions from config")
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.experimental_realtime_start_instructions.as_deref(),
        Some("start instructions from config")
    );
    Ok(())
}

#[tokio::test]
async fn experimental_realtime_ws_base_url_loads_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"experimental_realtime_ws_base_url = "http://127.0.0.1:8011"
experimental_realtime_webrtc_call_base_url = "http://127.0.0.1:8082/v1"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.experimental_realtime_ws_base_url.as_deref(),
        Some("http://127.0.0.1:8011")
    );
    assert_eq!(
        cfg.experimental_realtime_webrtc_call_base_url.as_deref(),
        Some("http://127.0.0.1:8082/v1")
    );
    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.experimental_realtime_ws_base_url.as_deref(),
        Some("http://127.0.0.1:8011")
    );
    assert_eq!(
        config.experimental_realtime_webrtc_call_base_url.as_deref(),
        Some("http://127.0.0.1:8082/v1")
    );
    Ok(())
}

#[tokio::test]
async fn experimental_realtime_ws_backend_prompt_loads_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
experimental_realtime_ws_backend_prompt = "prompt from config"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.experimental_realtime_ws_backend_prompt.as_deref(),
        Some("prompt from config")
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.experimental_realtime_ws_backend_prompt.as_deref(),
        Some("prompt from config")
    );
    Ok(())
}

#[tokio::test]
async fn experimental_realtime_ws_startup_context_loads_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
experimental_realtime_ws_startup_context = "startup context from config"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.experimental_realtime_ws_startup_context.as_deref(),
        Some("startup context from config")
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.experimental_realtime_ws_startup_context.as_deref(),
        Some("startup context from config")
    );
    Ok(())
}

#[tokio::test]
async fn experimental_realtime_ws_model_loads_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
experimental_realtime_ws_model = "realtime-test-model"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.experimental_realtime_ws_model.as_deref(),
        Some("realtime-test-model")
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.experimental_realtime_ws_model.as_deref(),
        Some("realtime-test-model")
    );
    Ok(())
}

#[tokio::test]
async fn realtime_config_partial_table_uses_realtime_defaults() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
[realtime]
voice = "marin"
"#,
    )
    .expect("TOML deserialization should succeed");

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.realtime,
        RealtimeConfig {
            voice: Some(RealtimeVoice::Marin),
            ..RealtimeConfig::default()
        }
    );
    Ok(())
}

#[tokio::test]
async fn realtime_loads_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
[realtime]
version = "v2"
type = "transcription"
transport = "webrtc"
voice = "cedar"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.realtime,
        Some(RealtimeToml {
            version: Some(RealtimeWsVersion::V2),
            session_type: Some(RealtimeWsMode::Transcription),
            transport: Some(RealtimeTransport::WebRtc),
            voice: Some(RealtimeVoice::Cedar),
        })
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.realtime,
        RealtimeConfig {
            version: RealtimeWsVersion::V2,
            session_type: RealtimeWsMode::Transcription,
            transport: RealtimeTransport::WebRtc,
            voice: Some(RealtimeVoice::Cedar),
        }
    );
    Ok(())
}

#[tokio::test]
async fn realtime_audio_loads_from_config_toml() -> std::io::Result<()> {
    for selection in ["1", "[1, 2]"] {
        let cfg: ConfigToml = toml::from_str(&format!(
            "[audio]\nmicrophone = \"USB Mic\"\nmicrophone_channel = {selection}\nspeaker = \"Desk Speakers\"\n"
        )).expect("TOML deserialization should succeed");
        let expected_audio = cfg.audio.as_ref().unwrap().clone();
        let codex_home = TempDir::new()?;
        let config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;
        assert_eq!(
            config.realtime_audio,
            ra_config::config_toml::RealtimeAudioConfig {
                microphone: Some("USB Mic".into()),
                speaker: Some("Desk Speakers".into()),
                microphone_channel: expected_audio.microphone_channel,
            }
        );
    }
    for invalid in ["0", "[1, 0]"] {
        assert!(
            toml::from_str::<ConfigToml>(&format!("[audio]\nmicrophone_channel = {invalid}"))
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_allow_unknown_special_paths() -> std::io::Result<()> {
    let config = load_workspace_permission_profile(PermissionProfileToml {
        description: None,
        extends: None,
        workspace_roots: None,
        filesystem: Some(FilesystemPermissionsToml {
            glob_scan_max_depth: None,
            entries: BTreeMap::from([(
                ":future_special_path".to_string(),
                FilesystemPermissionToml::Access(FileSystemAccessMode::Read),
            )]),
        }),
        network: None,
    })
    .await?;

    assert_eq!(
        config.permissions.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::unknown(
                    ":future_special_path",
                    /*subpath*/ None
                ),
            },
            access: FileSystemAccessMode::Read,
            missing_path_behavior: None,
        }]),
    );
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::ReadOnly {
            network_access: false,
        }
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning.contains(
            "Configured filesystem path `:future_special_path` is not recognized by this version of rusty-agent and will be ignored."
        )),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_allow_unknown_special_paths_with_nested_entries()
-> std::io::Result<()> {
    let config = load_workspace_permission_profile(PermissionProfileToml {
        description: None,
        extends: None,
        workspace_roots: None,
        filesystem: Some(FilesystemPermissionsToml {
            glob_scan_max_depth: None,
            entries: BTreeMap::from([(
                ":future_special_path".to_string(),
                FilesystemPermissionToml::Scoped(BTreeMap::from([(
                    "docs".to_string(),
                    FileSystemAccessMode::Read,
                )])),
            )]),
        }),
        network: None,
    })
    .await?;

    assert_eq!(
        config.permissions.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::unknown(":future_special_path", Some("docs".into())),
            },
            access: FileSystemAccessMode::Read,
            missing_path_behavior: None,
        }]),
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning.contains(
            "Configured filesystem path `:future_special_path` with nested entry `docs` is not recognized by this version of rusty-agent and will be ignored."
        )),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_allow_missing_filesystem_with_warning() -> std::io::Result<()> {
    let config = load_workspace_permission_profile(PermissionProfileToml {
        description: None,
        extends: None,
        workspace_roots: None,
        filesystem: None,
        network: None,
    })
    .await?;

    assert_eq!(
        config.permissions.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::restricted(Vec::new())
    );
    assert_eq!(
        &config.legacy_sandbox_policy(),
        &SandboxPolicy::ReadOnly {
            network_access: false,
        }
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning.contains(
            "Permissions profile `dev` does not define any recognized filesystem entries for this version of Codex."
        )),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_allow_empty_filesystem_with_warning() -> std::io::Result<()> {
    let config = load_workspace_permission_profile(PermissionProfileToml {
        description: None,
        extends: None,
        workspace_roots: None,
        filesystem: Some(FilesystemPermissionsToml {
            glob_scan_max_depth: None,
            entries: BTreeMap::new(),
        }),
        network: None,
    })
    .await?;

    assert_eq!(
        config.permissions.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::restricted(Vec::new())
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning.contains(
            "Permissions profile `dev` does not define any recognized filesystem entries for this version of Codex."
        )),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn exact_requirements_apply_to_runtime_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let catalog_path = codex_home.path().join("required-models.json");
    let mut catalog = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    catalog.models = catalog.models.into_iter().take(1).collect();
    std::fs::write(
        &catalog_path,
        serde_json::to_string(&catalog).expect("serialize catalog"),
    )?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
check_for_update_on_startup = true
allow_login_shell = true

[feedback]
enabled = true
"#,
    )?;

    let required_sqlite_home = codex_home.path().join("required-state");
    let required_log_dir = codex_home.path().join("required-logs");
    let requirements = format!(
        r#"
sqlite_home = {:?}
log_dir = {:?}
model_catalog_json = {:?}
check_for_update_on_startup = false
allow_login_shell = false

[feedback]
enabled = false
"#,
        required_sqlite_home.display(),
        required_log_dir.display(),
        catalog_path.display(),
    );
    let config = load_with_enterprise_requirement(&codex_home, requirements).await?;

    assert_eq!(config.sqlite.home(), required_sqlite_home.as_path());
    assert_eq!(config.log_dir, required_log_dir);
    assert_eq!(config.model_catalog, Some(catalog));
    assert!(!config.check_for_update_on_startup);
    assert!(!config.permissions.allow_login_shell);
    assert!(!config.feedback_enabled);
    assert!(config.startup_warnings.iter().any(|warning| {
        warning.contains("Configured value for `check_for_update_on_startup` is overridden")
    }));
    Ok(())
}

#[tokio::test]
async fn absent_allow_login_shell_does_not_report_an_override() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config = load_with_enterprise_requirement(&codex_home, "allow_login_shell = false").await?;

    assert!(!config.permissions.allow_login_shell);
    assert!(
        config
            .startup_warnings
            .iter()
            .all(|warning| !warning.contains("allow_login_shell"))
    );
    Ok(())
}

#[tokio::test]
async fn system_proxy_fallback_config_matches_bootstrap() -> std::io::Result<()> {
    for (features, expected_fallback, expected_policy) in [
        ("", true, ra_http::OutboundProxyPolicy::ReqwestDefault),
        (
            "respect_system_proxy = false",
            true,
            ra_http::OutboundProxyPolicy::ReqwestDefault,
        ),
        (
            "system_proxy_fallback = false",
            false,
            ra_http::OutboundProxyPolicy::ReqwestDefault,
        ),
        (
            "respect_system_proxy = true",
            false,
            ra_http::OutboundProxyPolicy::RespectSystemProxy,
        ),
        (
            "respect_system_proxy = true\nsystem_proxy_fallback = false",
            false,
            ra_http::OutboundProxyPolicy::RespectSystemProxy,
        ),
    ] {
        let codex_home = TempDir::new()?;
        std::fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            format!("[features]\n{features}\n"),
        )?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        let bootstrap = ConfigToml {
            features: Some(toml::from_str(features).expect("valid feature configuration")),
            ..Default::default()
        };
        let factory =
            resolve_bootstrap_http_client_factory(&bootstrap, /*feature_requirements*/ None)?;
        assert_eq!(
            factory.allows_system_proxy_fallback(),
            expected_fallback,
            "{features}"
        );
        assert_eq!(
            factory.outbound_proxy_policy(),
            expected_policy,
            "{features}"
        );
        assert_eq!(config.http_client_factory(), factory, "{features}");
    }
    Ok(())
}

#[tokio::test]
async fn system_proxy_fallback_honors_feature_requirements() -> std::io::Result<()> {
    for (configured, required) in [(true, false), (false, true)] {
        let cfg = ConfigToml {
            features: Some(
                toml::from_str(&format!("system_proxy_fallback = {configured}"))
                    .expect("valid features"),
            ),
            ..Default::default()
        };
        let requirements = Sourced::new(
            FeatureRequirementsToml {
                entries: BTreeMap::from([("system_proxy_fallback".to_string(), required)]),
            },
            RequirementSource::Unknown,
        );
        let bootstrap = resolve_bootstrap_http_client_factory(&cfg, Some(&requirements))?;
        assert_eq!(bootstrap.allows_system_proxy_fallback(), required);
        let codex_home = TempDir::new()?;
        std::fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            format!("[features]\nsystem_proxy_fallback = {configured}\n"),
        )?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .cloud_config_bundle(
                CloudConfigBundleFixture::loader_with_enterprise_requirement(format!(
                    "[features]\nsystem_proxy_fallback = {required}\n"
                )),
            )
            .build()
            .await?;
        assert_eq!(config.http_client_factory(), bootstrap);
    }
    Ok(())
}

#[tokio::test]
async fn project_cannot_be_the_only_xaa_opt_in_source() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config_layer_stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::Project {
                dot_codex_folder: codex_home.path().join("project/.rusty-agent").abs(),
            },
            toml::toml! {
                [features]
                use_xaa = true
            }
            .into(),
        )],
        Default::default(),
        Default::default(),
    )?;
    let cfg = config_layer_stack
        .effective_config()
        .try_into()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    let error = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        cfg,
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
        config_layer_stack,
    )
    .await
    .expect_err("project-only XAA opt-in must fail");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("must be selected in a non-project config layer")
    );
    Ok(())
}
