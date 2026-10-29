// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Full product configuration cases previously deferred pending ConfigBuilder.
use it_config::support::CloudConfigBundleFixture;
use ra_coding::config::{Config, ConfigBuilder, ConfigOverrides};
use ra_config::*;
use ra_filesystem::legacy::SandboxPolicy;
use ra_filesystem::{models::*, permissions::*};
use ra_paths::AbsolutePathBuf;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};
use tempfile::{TempDir, tempdir};

#[tokio::test]
async fn resolve_permission_profile_from_effective_configuration() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
default_permissions = "configured-profile"

[permissions.configured-profile]
description = "Configured profile"
extends = ":read-only"

[permissions.configured-profile.network]
enabled = true
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        config.resolve_permission_profile("configured-profile")?,
        toml::from_str::<PermissionProfileToml>(
            r#"
description = "Configured profile"
extends = ":read-only"

[filesystem]
":root" = "read"

[network]
enabled = true
"#
        )?
    );
    Ok(())
}

#[tokio::test]
async fn system_requirements_define_managed_permission_profiles() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
default_permissions = "managed-standard"

[features]
network_proxy = true
"#,
    )
    .await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-standard = true

[permissions.managed-standard]
extends = ":workspace"

[permissions.managed-standard.network]
enabled = true
proxy_url = "http://127.0.0.1:43128"
enable_socks5 = false
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    assert_eq!(
        config
            .config_layer_stack
            .requirements_toml()
            .allowed_permission_profiles,
        Some(BTreeMap::from([("managed-standard".to_string(), true)]))
    );
    assert_eq!(
        config.resolve_permission_profile("managed-standard")?,
        toml::from_str::<PermissionProfileToml>(
            r#"
extends = ":workspace"

[filesystem]
":root" = "read"
":slash_tmp" = "write"
":tmpdir" = "write"

[filesystem.":workspace_roots"]
"." = "write"

[network]
enabled = true
proxy_url = "http://127.0.0.1:43128"
enable_socks5 = false
"#
        )?
    );
    let active_permission_profile = config
        .permissions
        .active_permission_profile()
        .expect("managed profile should be active");
    assert_eq!(active_permission_profile.id, "managed-standard");

    let network = config
        .network_proxy_spec_for_active_permission_profile(
            &active_permission_profile,
            config.permissions.permission_profile(),
        )?
        .expect("managed profile should retain its network proxy configuration");
    assert_eq!(Some(&network), config.permissions.network.as_ref());
    assert_eq!(network.proxy_host_and_port(), "127.0.0.1:43128");
    assert!(!network.socks_enabled());
    Ok(())
}

#[tokio::test]
async fn resolve_permission_profile_inherits_across_configured_and_managed_profiles()
-> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
default_permissions = "configured-child"

[permissions.configured-parent]
extends = ":read-only"

[permissions.configured-parent.network]
enabled = true
proxy_url = "http://127.0.0.1:43128"

[permissions.configured-child]
description = "Configured child"
extends = "managed-parent"

[permissions.configured-child.network]
enable_socks5 = false
"#,
    )
    .await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
[permissions.managed-parent]
extends = ":read-only"

[permissions.managed-parent.network]
enabled = true
proxy_url = "http://127.0.0.1:43129"

[permissions.managed-child]
description = "Managed child"
extends = "configured-parent"

[permissions.managed-child.network]
enable_socks5 = false
"#,
    )
    .await?;

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    for (profile_name, expected) in [
        (
            "configured-child",
            r#"
description = "Configured child"
extends = "managed-parent"

[filesystem]
":root" = "read"

[network]
enabled = true
proxy_url = "http://127.0.0.1:43129"
enable_socks5 = false
"#,
        ),
        (
            "managed-child",
            r#"
description = "Managed child"
extends = "configured-parent"

[filesystem]
":root" = "read"

[network]
enabled = true
proxy_url = "http://127.0.0.1:43128"
enable_socks5 = false
"#,
        ),
    ] {
        assert_eq!(
            config.resolve_permission_profile(profile_name)?,
            toml::from_str::<PermissionProfileToml>(expected)?,
            "{profile_name}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn resolve_permission_profile_rejects_duplicate_configured_and_managed_profiles()
-> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        "default_permissions = \":read-only\"\n",
    )
    .await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
[permissions.duplicate-profile]
extends = ":read-only"
"#,
    )
    .await?;

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.clone())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;
    let config_toml = AbsolutePathBuf::from_absolute_path(codex_home.join(CONFIG_TOML_FILE))?;
    config.config_layer_stack = config.config_layer_stack.with_user_config(
        &config_toml,
        toml::from_str::<TomlValue>(
            r#"
default_permissions = ":read-only"

[permissions.duplicate-profile]
extends = ":workspace"
"#,
        )?,
    )?;

    let error = config
        .resolve_permission_profile("duplicate-profile")
        .expect_err("duplicate configured and managed profiles should be rejected");
    assert_eq!(
        error.to_string(),
        "requirements.toml permissions profile `duplicate-profile` conflicts with a config-defined profile of the same name"
    );
    Ok(())
}

#[tokio::test]
async fn system_allowed_permission_profiles_select_managed_default_without_local_default()
-> anyhow::Result<()> {
    for trust_level in [Some(TrustLevel::Trusted), Some(TrustLevel::Untrusted), None] {
        let tmp = tempdir()?;
        let codex_home = tmp.path().join("home");
        tokio::fs::create_dir_all(&codex_home).await?;
        if let Some(trust_level) = trust_level {
            make_config_for_test(
                &codex_home,
                tmp.path(),
                trust_level,
                /*project_root_markers*/ None,
            )
            .await?;
        }
        let requirements_path = tmp.path().join("requirements.toml");
        tokio::fs::write(
            &requirements_path,
            r#"
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-build = true
managed-standard = true

[permissions.managed-standard.filesystem]
":workspace_roots" = "read"

[permissions.managed-build]
extends = ":workspace"
"#,
        )
        .await?;

        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
        let mut overrides = LoaderOverrides::without_managed_config_for_tests();
        overrides.system_requirements_path = Some(requirements_path);
        let config = ConfigBuilder::default()
            .codex_home(codex_home)
            .fallback_cwd(Some(cwd.to_path_buf()))
            .loader_overrides(overrides)
            .build()
            .await?;

        assert_eq!(
            config
                .permissions
                .active_permission_profile()
                .map(|profile| profile.id),
            Some("managed-standard".to_string()),
            "trust level {trust_level:?}",
        );
        assert!(
            !config.startup_warnings.iter().any(|warning| warning
                .contains("Configured value for `permission_profile` is disallowed")),
            "{:?}",
            config.startup_warnings
        );
    }
    Ok(())
}

#[tokio::test]
async fn system_allowed_permission_profiles_require_managed_default() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
[permissions.managed-standard]
extends = ":read-only"

[allowed_permission_profiles]
managed-standard = true
"#,
    )
    .await?;

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let err = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await
        .expect_err("allowed_permission_profiles without default_permissions should fail");

    assert!(
        err.to_string().contains(
            "default_permissions must be set unless allowed_permission_profiles allows both"
        ),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn system_allowed_permission_profiles_standard_pair_defaults_to_workspace()
-> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
[allowed_permission_profiles]
":read-only" = true
":workspace" = true
"#,
    )
    .await?;

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .map(|profile| profile.id),
        Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string())
    );
    Ok(())
}

#[tokio::test]
async fn system_managed_default_requires_allowed_permission_profiles() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = ":read-only"
"#,
    )
    .await?;

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let err = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await
        .expect_err("managed default without allowed_permission_profiles should fail");

    assert!(
        err.to_string()
            .contains("default_permissions requires allowed_permission_profiles"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn system_allowed_permission_profiles_fall_back_from_disallowed_danger_full_access()
-> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        format!(
            r#"
default_permissions = "{BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS}"
"#
        ),
    )
    .await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-standard = true

[permissions.managed-standard.filesystem]
":workspace_roots" = "read"
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .map(|profile| profile.id),
        Some("managed-standard".to_string())
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning
            .contains("Configured value for `permission_profile` is disallowed by requirements")),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn system_allowed_permission_profiles_fall_back_from_disallowed_workspace()
-> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
default_permissions = ":workspace"
"#,
    )
    .await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-standard = true

[permissions.managed-standard.filesystem]
":workspace_roots" = "read"
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .map(|profile| profile.id),
        Some("managed-standard".to_string())
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning
            .contains("Configured value for `permission_profile` is disallowed by requirements")),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn system_requirements_preserve_allowed_configured_permission_default() -> anyhow::Result<()>
{
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
default_permissions = "managed-build"
"#,
    )
    .await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-build = true
managed-standard = true

[permissions.managed-standard]
extends = ":read-only"

[permissions.managed-build]
extends = ":workspace"
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .map(|profile| profile.id),
        Some("managed-build".to_string())
    );
    Ok(())
}

#[tokio::test]
async fn system_requirements_warn_for_disallowed_explicit_permission_override() -> anyhow::Result<()>
{
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-standard = true

[permissions.managed-standard]
extends = ":workspace"
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .harness_overrides(ConfigOverrides {
            default_permissions: Some("managed-build".to_string()),
            ..ConfigOverrides::default()
        })
        .loader_overrides(overrides)
        .build()
        .await?;

    assert_eq!(
        config
            .permissions
            .active_permission_profile()
            .map(|profile| profile.id),
        Some("managed-standard".to_string())
    );
    assert!(
        config.startup_warnings.iter().any(|warning| warning
            .contains("Configured value for `permission_profile` is disallowed by requirements")),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn credential_broker_ambiguity_guard_survives_permission_profile_selection()
-> anyhow::Result<()> {
    for (key, other_key, other_value, ambiguous, credential_masking) in [
        ("GH_HOST", "gh_host", "second.example", true, false),
        ("VENDOR_HOST", "vendor_host", "second.example", true, false),
        (
            "VENDOR_PASSWORD",
            "vendor_password",
            "second.example",
            true,
            false,
        ),
        ("VENDOR_HOST", "vendor_host", "first.example", false, false),
        ("FOO", "foo", "second.example", false, false),
        ("GH_HOST", "gh_host", "second.example", true, true),
    ] {
        let home = tempdir()?;
        tokio::fs::write(
            home.path().join(CONFIG_TOML_FILE),
            format!(
                r#"
default_permissions = "first"
[features]
credential_masking = {credential_masking}
[features.network_proxy]
enabled = true
credential_broker = true
[features.network_proxy.credentials.vendor]
env = ["VENDOR_PASSWORD"]
patterns = ["pin_[a-z]{{8}}"]
url_prefixes = ["https://api.vendor.example"]
url_prefix_from_env = "VENDOR_HOST"
auth = ["bearer"]
[permissions.first]
extends = ":workspace"
[permissions.first.network]
enabled = true
[permissions.second]
extends = "first"
[shell_environment_policy.set]
{key} = "first.example"
{other_key} = "{other_value}"
"#
            ),
        )
        .await?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await?;
        let expected_enabled = !(cfg!(windows) && ambiguous);
        if credential_masking && !expected_enabled {
            assert_eq!(
                config.startup_warnings.join("\n"),
                r"Credential masking is inactive because shell environment overrides contain conflicting case-insensitive provider keys. Commands will receive unmasked credentials. Remove the conflicting entries from `shell_environment_policy.set`."
            );
        } else {
            assert_eq!(config.startup_warnings, Vec::<String>::new());
        }
        assert_eq!(
            config.permissions.shell_environment_policy.r#set,
            HashMap::from([
                (key.to_string(), "first.example".to_string()),
                (other_key.to_string(), other_value.to_string()),
            ])
        );
        assert_eq!(
            config
                .permissions
                .network
                .as_ref()
                .unwrap()
                .credential_broker_enabled(),
            expected_enabled,
            "{key}"
        );
        let mut selected = config.permissions.active_permission_profile().unwrap();
        selected.id = "second".to_string();
        let rebuilt = config
            .network_proxy_spec_for_active_permission_profile(
                &selected,
                config.permissions.permission_profile(),
            )?
            .unwrap();
        assert_eq!(Some(&rebuilt), config.permissions.network.as_ref(), "{key}");
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_nul_in_required_deny_read_glob() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[permissions.filesystem]
deny_read = ["secrets/**\u0000"]
"#,
            ),
        )
        .build()
        .await
        .expect_err("an unsupported required denial must fail configuration loading");

    assert!(
        err.to_string().contains("unsupported configuration path"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn persisted_permission_profile_id_wins_over_configured_default() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let config_toml = toml::from_str(
        r#"
default_permissions = ":read-only"

[permissions.dev]
extends = ":workspace"
"#,
    )
    .expect("permission profile config should deserialize");

    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            persisted_permission_profile_id: Some("dev".to_string()),
            ..Default::default()
        },
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile {
            id: "dev".to_string(),
            extends: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
        })
    );
    assert_eq!(
        config.legacy_sandbox_policy(),
        SandboxPolicy::new_workspace_write_policy()
    );
    Ok(())
}

#[tokio::test]
async fn missing_persisted_permission_profile_id_uses_configured_default() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let project_key = cwd.path().to_string_lossy().to_string();

    for (configured_default, configured_sandbox, expected_active_profile) in [
        (
            Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()),
            None,
            Some(ActivePermissionProfile::read_only()),
        ),
        (None, Some(SandboxMode::ReadOnly), None),
    ] {
        let config = Config::load_from_base_config_with_overrides(
            ConfigToml {
                default_permissions: configured_default,
                sandbox_mode: configured_sandbox,
                projects: Some(HashMap::from([(
                    project_key.clone(),
                    ProjectConfig {
                        trust_level: Some(TrustLevel::Trusted),
                    },
                )])),
                ..Default::default()
            },
            ConfigOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                persisted_permission_profile_id: Some("removed-profile".to_string()),
                ..Default::default()
            },
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;

        assert_eq!(
            config.permissions.active_permission_profile(),
            expected_active_profile
        );
        assert_eq!(
            config.permissions.effective_permission_profile(),
            PermissionProfile::read_only()
        );
    }
    Ok(())
}

#[tokio::test]
async fn invalid_persisted_permission_profile_uses_configured_default() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    for invalid_profile in [
        "extends = \"removed-parent\"",
        "extends = \":read-only\"\n\n[permissions.dev.filesystem]\nglob_scan_max_depth = 0",
    ] {
        let config_toml = toml::from_str(&format!(
            "default_permissions = \":danger-full-access\"\n\n[permissions.dev]\n{invalid_profile}"
        ))
        .expect("permission profile config should deserialize");

        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                persisted_permission_profile_id: Some("dev".to_string()),
                ..Default::default()
            },
            AbsolutePathBuf::from_absolute_path(codex_home.path())?,
        )
        .await?;

        assert_eq!(
            config.permissions.active_permission_profile(),
            Some(ActivePermissionProfile::new(
                BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS,
            ))
        );
        assert_eq!(
            config.permissions.effective_permission_profile(),
            PermissionProfile::Disabled
        );
    }
    Ok(())
}

#[tokio::test]
async fn persisted_profile_cycle_uses_configured_default() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let config_toml = toml::from_str(
        r#"
default_permissions = ":danger-full-access"

[permissions.dev]
extends = "base"

[permissions.base]
extends = "dev"
"#,
    )
    .expect("permission profile config should deserialize");

    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            persisted_permission_profile_id: Some("dev".to_string()),
            ..Default::default()
        },
        AbsolutePathBuf::from_absolute_path(codex_home.path())?,
    )
    .await?;

    assert_eq!(
        config.permissions.active_permission_profile(),
        Some(ActivePermissionProfile::new(
            BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS,
        ))
    );
    assert_eq!(
        config.permissions.effective_permission_profile(),
        PermissionProfile::Disabled
    );
    Ok(())
}

async fn make_config_for_test(
    codex_home: &Path,
    project_path: &Path,
    trust_level: TrustLevel,
    project_root_markers: Option<Vec<String>>,
) -> std::io::Result<()> {
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        toml::to_string(&ConfigToml {
            projects: Some(HashMap::from([(
                project_path.to_string_lossy().to_string(),
                ProjectConfig {
                    trust_level: Some(trust_level),
                },
            )])),
            project_root_markers,
            ..Default::default()
        })
        .expect("serialize config"),
    )
    .await
}

#[tokio::test]
async fn credential_masking_respects_config_precedence() -> std::io::Result<()> {
    for (features, requirements, expected_proxy, expected_broker) in [
        (
            "credential_masking = true\n[features.network_proxy]\nenabled = true\ncredential_broker = false",
            "",
            true,
            true,
        ),
        (
            "credential_masking = false\n[features.network_proxy]\nenabled = true\ncredential_broker = true",
            "",
            true,
            true,
        ),
        (
            r#"credential_masking = false
[features.network_proxy]
enabled = false
[features.network_proxy.credentials.vendor]
env = ["VENDOR_TOKEN"]
patterns = ["vendor_[a-z]{8}"]
url_prefixes = ["https://api.vendor.example"]
auth = ["bearer"]"#,
            "[features]\ncredential_masking = true\n[experimental_network]\nenabled = true",
            true,
            true,
        ),
        (
            "credential_masking = true\nnetwork_proxy = true",
            "[features]\ncredential_masking = false",
            true,
            false,
        ),
        (
            "credential_masking = true\nnetwork_proxy = true",
            "[experimental_network]\nenabled = false",
            false,
            false,
        ),
        (
            "credential_masking = true",
            "[experimental_network]\nallow_local_binding = true",
            false,
            false,
        ),
    ] {
        let home = tempdir()?;
        let source =
            format!("openai_base_url = 'https://api.example.test/v1'\n[features]\n{features}\n");
        let configured: ConfigToml = toml::from_str(&source).expect("valid config");
        tokio::fs::write(home.path().join(CONFIG_TOML_FILE), source).await?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(home.path().to_path_buf()),
                permission_profile: Some(PermissionProfile::Managed {
                    file_system: ManagedFileSystemPermissions::Unrestricted,
                    network: NetworkSandboxPolicy::Enabled,
                }),
                ..Default::default()
            })
            .cloud_config_bundle(
                CloudConfigBundleFixture::loader_with_enterprise_requirement(requirements),
            )
            .build()
            .await?;
        let network = config.permissions.network.as_ref().expect("proxy settings");
        assert_eq!(
            (network.enabled(), network.credential_broker_enabled()),
            (expected_proxy, expected_broker),
            "{features}; {requirements}"
        );
        // Runtime proxy construction is separate; this case verifies the retained spec.
        let proxy = network.config();
        if expected_broker {
            let expected_providers = configured
                .features
                .as_ref()
                .and_then(|features| features.network_proxy.as_ref())
                .and_then(|feature| match feature {
                    ra_config::FeatureToml::Enabled(_) => None,
                    ra_config::FeatureToml::Config(config) => Some(config),
                })
                .and_then(|network_proxy| network_proxy.credentials.clone())
                .unwrap_or_default();
            assert_eq!(proxy.credential_providers, expected_providers);
            assert_eq!(
                proxy.credential_broker_openai_host.as_deref(),
                Some("api.example.test")
            );
        }

        let active_profile = ActivePermissionProfile {
            id: BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string(),
            extends: None,
        };
        let reloaded = config.network_proxy_spec_for_active_permission_profile(
            &active_profile,
            &config.permissions.effective_permission_profile(),
        )?;
        assert_eq!(
            reloaded, config.permissions.network,
            "{features}; {requirements}"
        );
        if requirements.is_empty() {
            assert_eq!(
                config.network_proxy_spec_for_active_permission_profile(
                    &active_profile,
                    &PermissionProfile::read_only(),
                )?,
                None,
                "profile change should leave masking inactive when the proxy is disabled"
            );
        }
    }
    Ok(())
}
