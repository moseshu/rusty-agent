// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Builder precedence and effective snapshot consumers.
use it_config::support::CloudConfigBundleFixture;
use ra_coding::config::{ConfigBuilder, ConfigOverrides};
use ra_config::LoaderOverrides;
use tempfile::tempdir;

#[tokio::test]
async fn provider_selection_keeps_all_four_tiers_and_plugin_projection() -> anyhow::Result<()> {
    for (configured, cli, harness, required, expected) in [
        (None, None, None, None, "openai"),
        (Some("configured"), None, None, None, "configured"),
        (Some("configured"), Some("cli"), None, None, "cli"),
        (
            Some("configured"),
            Some("cli"),
            Some("harness"),
            None,
            "harness",
        ),
        (
            Some("configured"),
            Some("cli"),
            Some("harness"),
            Some("managed"),
            "managed",
        ),
    ] {
        let home = tempdir()?;
        let mut contents = configured
            .map(|id| format!("model_provider = {id:?}\n"))
            .unwrap_or_default();
        for id in ["configured", "cli", "harness", "managed"] {
            contents.push_str(&format!(
                "\n[model_providers.{id}]\nname = {id:?}\nbase_url = 'https://example.test/v1'\n"
            ));
        }
        tokio::fs::write(home.path().join("config.toml"), contents).await?;
        let mut builder = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .harness_overrides(ConfigOverrides {
                model_provider: harness.map(str::to_string),
                ..Default::default()
            })
            .cli_overrides(
                cli.map(|id| {
                    vec![(
                        "model_provider".to_string(),
                        toml::Value::String(id.to_string()),
                    )]
                })
                .unwrap_or_default(),
            );
        if let Some(required) = required {
            builder = builder.cloud_config_bundle(
                CloudConfigBundleFixture::loader_with_enterprise_requirement(format!(
                    "model_provider = {required:?}"
                )),
            );
        }
        let config = builder.build().await?;
        assert_eq!(config.model_provider_id, expected);
        assert_eq!(
            config.model_provider.name,
            if expected == "openai" {
                "OpenAI"
            } else {
                expected
            }
        );
        assert_eq!(config.plugin_runtime_config().model_provider_id, expected);
        assert_eq!(config.plugins_config_input().model_provider_id, expected);
        assert!(config.model_providers.contains_key("openai"));
        assert!(config.model_providers.contains_key("ollama"));
    }
    Ok(())
}

#[tokio::test]
async fn service_tier_override_can_explicitly_select_standard_routing() -> anyhow::Result<()> {
    for (override_value, expected) in [
        (None, "flex"),
        (Some(None), "default"),
        (Some(Some("custom".to_string())), "custom"),
    ] {
        let home = tempdir()?;
        tokio::fs::write(home.path().join("config.toml"), "service_tier = 'flex'").await?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .harness_overrides(ConfigOverrides {
                service_tier: override_value,
                ..Default::default()
            })
            .build()
            .await?;
        assert_eq!(config.service_tier.as_deref(), Some(expected));
    }
    Ok(())
}

#[tokio::test]
async fn invalid_provider_and_conflicting_permission_overrides_keep_source_errors()
-> anyhow::Result<()> {
    for (overrides, expected) in [
        (
            ConfigOverrides {
                model_provider: Some("missing".to_string()),
                ..Default::default()
            },
            "Model provider `missing` not found",
        ),
        (
            ConfigOverrides {
                sandbox_mode: Some(ra_config::SandboxMode::ReadOnly),
                default_permissions: Some(":workspace".to_string()),
                ..Default::default()
            },
            "`sandbox_mode` and `default_permissions` overrides cannot both be set",
        ),
    ] {
        let home = tempdir()?;
        let error = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .harness_overrides(overrides)
            .build()
            .await
            .expect_err("invalid input");
        assert_eq!(error.to_string(), expected);
    }
    Ok(())
}

#[tokio::test]
async fn explicit_instructions_do_not_hide_a_bad_configured_instruction_file() -> anyhow::Result<()>
{
    let home = tempdir()?;
    tokio::fs::write(home.path().join("instructions.txt"), "   ").await?;
    tokio::fs::write(
        home.path().join("config.toml"),
        "model_instructions_file = 'instructions.txt'",
    )
    .await?;
    let error = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .harness_overrides(ConfigOverrides {
            base_instructions: Some("explicit".to_string()),
            ..Default::default()
        })
        .build()
        .await
        .expect_err("configured files are validated before selecting overrides");
    assert!(
        error
            .to_string()
            .contains("model instructions file is empty:")
    );
    Ok(())
}

#[tokio::test]
async fn requirements_fallback_keeps_the_danger_full_access_never_guard() -> anyhow::Result<()> {
    let home = tempdir()?;
    tokio::fs::write(
        home.path().join("config.toml"),
        "sandbox_mode = 'danger-full-access'\napproval_policy = 'never'",
    )
    .await?;
    let error = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                "allowed_sandbox_modes = ['read-only']",
            ),
        )
        .build()
        .await
        .expect_err("fallback would disable all approvals");
    assert!(
        error
            .to_string()
            .contains("read-only permissions with approvals disabled")
    );
    Ok(())
}

#[tokio::test]
async fn strict_builder_preserves_field_diagnostics_and_file_origin() -> anyhow::Result<()> {
    let home = tempdir()?;
    let path = home.path().join("config.toml");
    tokio::fs::write(&path, "model = 42").await?;
    let error = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(true)
        .build()
        .await
        .expect_err("typed configuration must fail");
    let load_error = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<ra_config::ConfigLoadError>())
        .expect("source diagnostic survives builder");
    assert_eq!(load_error.config_error().path, path);
    Ok(())
}

#[tokio::test]
async fn local_required_plugin_backend_url_is_projected_with_provenance() -> anyhow::Result<()> {
    let home = tempdir()?;
    tokio::fs::write(
        home.path().join("config.toml"),
        "chatgpt_base_url = 'https://user.example.test/'",
    )
    .await?;
    let requirements = home.path().join("requirements.toml");
    tokio::fs::write(
        &requirements,
        "chatgpt_base_url = 'https://managed.example.test/'",
    )
    .await?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements);
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;
    assert_eq!(config.chatgpt_base_url, "https://managed.example.test/");
    assert_eq!(
        config.plugin_runtime_config().chatgpt_base_url,
        config.chatgpt_base_url
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("chatgpt_base_url")
                && warning.contains("required value"))
    );
    let projected = ra_coding::plugins::PluginRuntimeConfig::from_layer_stack(
        config.config_layer_stack.clone(),
        config.codex_home.clone(),
        config.cwd.clone(),
        config.http_client_factory(),
    )?;
    assert_eq!(projected.chatgpt_base_url, config.chatgpt_base_url);
    assert!(
        config
            .config_layer_stack
            .requirements()
            .chatgpt_base_url
            .as_ref()
            .is_some()
    );
    Ok(())
}
