// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Effective defaults and invalid input behavior of the product builder.
use ra_coding::config::*;
use ra_config::config_toml::*;
use ra_config::features::*;
use ra_config::*;
use ra_filesystem::permissions::FileSystemAccessMode;
use ra_paths::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::{path::PathBuf, time::Duration};
use tempfile::{TempDir, tempdir};
const AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES: usize = 2000;
const TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES: usize = 2000;

#[tokio::test]
async fn load_config_normalizes_relative_cwd_override() -> std::io::Result<()> {
    let expected_cwd = AbsolutePathBuf::relative_to_current_dir("nested")?;
    let codex_home = tempdir()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(PathBuf::from("nested")),
            ..Default::default()
        },
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert_eq!(config.cwd, expected_cwd);
    Ok(())
}

#[tokio::test]
async fn load_config_applies_optional_mcp_startup_grace() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml: ConfigToml = toml::from_str("mcp_optional_startup_grace_ms = 2500")
        .expect("optional MCP startup grace should parse from config.toml");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert_eq!(
        config.mcp_optional_startup_grace,
        Duration::from_millis(2500)
    );
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_thread_unload_delay() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    for (toml, seconds) in [
        ("", 1800),
        ("thread_unload_delay_secs = 0", 0),
        ("thread_unload_delay_secs = 60", 60),
    ] {
        let config = Config::load_from_base_config_with_overrides(
            toml::from_str(toml)?,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;
        assert_eq!(config.thread_unload_delay, Duration::from_secs(seconds));
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_thread_unload_delay_overflow() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let error = Config::load_from_base_config_with_overrides(
        ConfigToml {
            thread_unload_delay_secs: Some(u64::MAX),
            ..Default::default()
        },
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await
    .expect_err("idle timeout must fit in a monotonic deadline");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "thread_unload_delay_secs is too large");
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_experimental_request_user_input_enabled() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            tools: Some(ToolsToml {
                web_search: None,
                experimental_request_user_input: Some(ExperimentalRequestUserInput {
                    enabled: false,
                }),
                update_plan: None,
            }),
            ..ConfigToml::default()
        },
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert!(!config.experimental_request_user_input_enabled);
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_non_prefixed_mcp_tool_servers() -> std::io::Result<()> {
    let cases = [
        (
            "[features]\nnon_prefixed_mcp_tool_names = false\n",
            None,
            true,
        ),
        (
            "[features]\nnon_prefixed_mcp_tool_names = true\n",
            None,
            false,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = true\n",
            None,
            false,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = true\nserver_names = [\"history\", \"notes\"]\n",
            Some(vec!["history".to_string(), "notes".to_string()]),
            true,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = true\nserver_names = []\n",
            Some(Vec::new()),
            true,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = false\nserver_names = [\"history\"]\n",
            None,
            true,
        ),
    ];

    for (config_contents, expected_servers, expected_prefix) in cases {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str::<ConfigToml>(config_contents)
            .expect("TOML deserialization should succeed");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;

        assert_eq!(config.non_prefixed_mcp_tool_servers, expected_servers);
        assert_eq!(config.prefix_mcp_tool_names(), expected_prefix);
    }
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_update_plan_enabled() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    for (config_toml, expected_enabled) in [
        ("", false),
        ("[tools.update_plan]", false),
        ("[tools.update_plan]\nenabled = false", false),
        ("[tools.update_plan]\nenabled = true", true),
    ] {
        let config = Config::load_from_base_config_with_overrides(
            toml::from_str(config_toml).expect("TOML deserialization should succeed"),
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;

        assert_eq!(
            config.update_plan_enabled, expected_enabled,
            "{config_toml}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_code_mode_config() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml: ConfigToml = toml::from_str(
        r#"
[features.code_mode]
enabled = true
default_exec_yield_time_ms = 10000
experimental_show_cell_overhead = true
tool_input_schema_max_bytes = 36000
excluded_tool_namespaces = ["mcp__codex_apps", "multi_agent_v1"]
direct_only_tool_namespaces = ["mcp__history", "mcp__notes"]

[features.code_mode_host]
enabled = true
disable_in_process_fallback = true
"#,
    )
    .expect("TOML deserialization should succeed");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert_eq!(config.code_mode.default_exec_yield_time_ms, 10_000);
    assert!(config.code_mode.experimental_show_cell_overhead);
    assert_eq!(config.code_mode.tool_input_schema_max_bytes, Some(36_000));
    assert_eq!(
        config.code_mode.excluded_tool_namespaces,
        vec!["mcp__codex_apps".to_string(), "multi_agent_v1".to_string()]
    );
    assert_eq!(
        config.code_mode.direct_only_tool_namespaces,
        vec!["mcp__history".to_string(), "mcp__notes".to_string()]
    );
    assert!(config.code_mode.disable_in_process_fallback);
    assert!(config.features.enabled(Feature::CodeMode));
    assert!(config.features.enabled(Feature::CodeModeHost));
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_tool_registry_config() -> std::io::Result<()> {
    let codex_home = tempdir()?;

    for (config_toml, error_on_tool_collisions, turn_metadata_includes_tool_info) in [
        ("", false, false),
        (
            "[features.tool_registry]\nerror_on_tool_collisions = true\n",
            true,
            false,
        ),
        (
            "[features.tool_registry]\nturn_metadata_includes_tool_info = true\n",
            false,
            true,
        ),
    ] {
        let config_toml: ConfigToml =
            toml::from_str(config_toml).expect("TOML deserialization should succeed");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;

        assert_eq!(
            config.tool_registry.error_on_tool_collisions,
            error_on_tool_collisions
        );
        assert_eq!(
            config.tool_registry.turn_metadata_includes_tool_info,
            turn_metadata_includes_tool_info
        );
        assert!(!config.features.enabled(Feature::CodeMode));
    }

    Ok(())
}

#[tokio::test]
async fn load_config_resolves_token_budget_config() -> std::io::Result<()> {
    for (config_toml, expected) in [
        (
            "[features]\ntoken_budget = true\n",
            Some(TokenBudgetConfig::default()),
        ),
        (
            "features.context_management.experimental_mode = true\n",
            None,
        ),
        (
            r#"
[features.token_budget]
enabled = true
use_history_notes_extension = true
reminder_threshold_tokens = 16000
reminder_message_template = "Custom reminder: {n_remaining} tokens."
guidance_message = "Preserve important state before compaction."
auto_compact_fallback_prompt = "  Write notes immediately.  "
auto_compact_fallback_buffer_tokens = 8000
"#,
            Some(TokenBudgetConfig {
                use_history_notes_extension: true,
                reminder_threshold_tokens: Some(16_000),
                reminder_message_template: "Custom reminder: {n_remaining} tokens.".to_string(),
                guidance_message: Some("Preserve important state before compaction.".to_string()),
                auto_compact_fallback_prompt: Some("Write notes immediately.".to_string()),
                auto_compact_fallback_buffer_tokens: Some(8_000),
            }),
        ),
    ] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(config_toml).expect("TOML should deserialize");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;

        assert_eq!(
            config.features.enabled(Feature::TokenBudget),
            expected.is_some()
        );
        if expected.is_none() {
            assert!(config.features.enabled(Feature::ContextManagement));
        }
        assert_eq!(config.token_budget, expected);
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_overlong_auto_compact_fallback_prompt() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let prompt = "x".repeat(AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES + 1);
    let config_toml = toml::from_str(&format!(
        "[features.token_budget]\nenabled = true\nauto_compact_fallback_prompt = {prompt:?}\n"
    ))
    .expect("TOML should deserialize");
    let error = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await
    .expect_err("overlong fallback prompt should be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_invalid_token_budget_reminder_template() -> std::io::Result<()> {
    for reminder_message_template in [
        String::new(),
        "x".repeat(TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES + 1),
    ] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(&format!(
            "[features.token_budget]\nenabled = true\nreminder_message_template = {reminder_message_template:?}\n"
        ))
        .expect("TOML should deserialize");
        let error = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await
        .expect_err("invalid reminder template should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_non_positive_token_budget_reminder_threshold() -> std::io::Result<()> {
    for reminder_threshold_tokens in [-1, 0] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(&format!(
            "[features.token_budget]\nenabled = true\nreminder_threshold_tokens = {reminder_threshold_tokens}\n"
        ))
        .expect("TOML should deserialize");
        let error = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await
        .expect_err("non-positive reminder threshold should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "features.token_budget.reminder_threshold_tokens must be positive"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_non_positive_auto_compact_fallback_buffer() -> std::io::Result<()> {
    for auto_compact_fallback_buffer_tokens in [-1, 0] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(&format!(
            "[features.token_budget]\nenabled = true\nauto_compact_fallback_prompt = \"Write notes.\"\nauto_compact_fallback_buffer_tokens = {auto_compact_fallback_buffer_tokens}\n"
        ))
        .expect("TOML should deserialize");
        let error = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await
        .expect_err("non-positive fallback buffer should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "features.token_budget.auto_compact_fallback_buffer_tokens must be positive"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_missing_auto_compact_fallback_buffer() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml = toml::from_str(
        "[features.token_budget]\nenabled = true\nauto_compact_fallback_prompt = \"Write notes.\"\n",
    )
    .expect("TOML should deserialize");

    let error = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await
    .expect_err("missing fallback buffer should be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "features.token_budget.auto_compact_fallback_buffer_tokens is required when auto_compact_fallback_prompt is set"
    );

    Ok(())
}

#[tokio::test]
async fn load_config_resolves_rollout_budget() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml: ConfigToml = toml::from_str(
        r#"
[features.rollout_budget]
enabled = true
limit_tokens = 100000
reminder_at_remaining_tokens = [50000, 25000, 10000]
sampling_token_weight = 1.0
prefill_token_weight = 0.1
"#,
    )
    .expect("TOML deserialization should succeed");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert!(config.features.enabled(Feature::RolloutBudget));
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert_eq!(
        config.rollout_budget,
        Some(RolloutBudgetConfig {
            limit_tokens: 100_000,
            reminder_at_remaining_tokens: vec![50_000, 25_000, 10_000],
            sampling_token_weight: 1.0,
            prefill_token_weight: 0.1,
        })
    );
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_enabled_rollout_budget_without_limit() -> std::io::Result<()> {
    for config_toml in [
        "[features]\nrollout_budget = true\n",
        "[features.rollout_budget]\nenabled = true\n",
    ] {
        let codex_home = tempdir()?;
        let config_toml: ConfigToml =
            toml::from_str(config_toml).expect("TOML deserialization should succeed");
        let err = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await
        .expect_err("enabled rollout budget without limit_tokens should be rejected");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "features.rollout_budget.limit_tokens is required when rollout_budget is enabled"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_current_time_reminder() -> std::io::Result<()> {
    for (config_toml, expected) in [
        (
            r#"
[features]
current_time_reminder = true
"#,
            CurrentTimeReminderConfig::default(),
        ),
        (
            r#"
[features.current_time_reminder]
enabled = true
reminder_interval_seconds = 0
clock_source = "external"
delivery_mode = "after_user_or_tool_output"
sleep_tool = true
"#,
            CurrentTimeReminderConfig {
                reminder_interval_seconds: 0,
                clock_source: CurrentTimeSource::External,
                delivery_mode: CurrentTimeReminderDeliveryMode::AfterUserOrToolOutput,
                sleep_tool: true,
            },
        ),
    ] {
        let config = load_current_time_reminder_config(config_toml).await?;
        assert!(config.features.enabled(Feature::CurrentTimeReminder));
        assert_eq!(config.current_time_reminder, Some(expected));
    }
    Ok(())
}

#[tokio::test]
async fn respect_system_proxy_feature_resolves_enabled() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(
                toml::from_str(
                    r#"
respect_system_proxy = true
"#,
                )
                .expect("valid features"),
            ),
            ..Default::default()
        },
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert!(config.respect_system_proxy);
    assert_eq!(
        config.http_client_factory().outbound_proxy_policy(),
        ra_http::OutboundProxyPolicy::RespectSystemProxy
    );
    assert_eq!(
        config.plugins_config_input().remote_plugin_service_config(),
        ra_coding::plugins::remote::RemotePluginServiceConfig::new(
            config.chatgpt_base_url,
            ra_http::HttpClientFactory::new(ra_http::OutboundProxyPolicy::RespectSystemProxy,),
        )
    );
    Ok(())
}

#[tokio::test]
async fn missing_explicit_default_permissions_remains_an_error() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;

    let error = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            default_permissions: Some("removed-profile".to_string()),
            ..Default::default()
        },
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await
    .expect_err("explicit missing profile should be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "default_permissions requires a `[permissions]` table"
    );
    Ok(())
}

#[tokio::test]
async fn permissions_profiles_require_default_permissions() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    let err = Config::load_from_base_config_with_overrides(
        ConfigToml {
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
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await
    .expect_err("missing default_permissions should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "config defines `[permissions]` profiles but does not set `default_permissions`"
    );
    Ok(())
}

async fn load_current_time_reminder_config(config_toml: &str) -> std::io::Result<Config> {
    let codex_home = tempdir()?;
    let config_toml = toml::from_str(config_toml).expect("TOML should deserialize");
    Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await
}
