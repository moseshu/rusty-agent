// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Upstream tests for the effective product configuration snapshot.
#![allow(
    unused_imports,
    reason = "Shared imports support the ported configuration cases."
)]
use indexmap::IndexMap;
use it_config::support::CloudConfigBundleFixture;
use ra_coding::config::network::*;
use ra_coding::config::providers::*;
use ra_coding::config::test_api::*;
use ra_coding::config::*;
use ra_config::config_toml::*;
use ra_config::diagnostics::config_error_from_typed_toml;
use ra_config::features::*;
use ra_config::loader::{load_config_layers_state, project_trust_key};
use ra_config::permission_profile_catalog::PermissionProfileCatalogEntry;
use ra_config::permissions_toml::{
    FilesystemPermissionToml, FilesystemPermissionsToml, NetworkDomainPermissionToml,
    NetworkDomainPermissionsToml, NetworkMitmActionToml, NetworkMitmHookToml, NetworkMitmToml,
    NetworkToml, PermissionProfileToml, PermissionsToml, WorkspaceRootsToml,
};
use ra_config::strict_config::config_error_from_ignored_toml_fields;
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
#[path = "config_support/mod.rs"]
mod config_support;
use config_support::*;

#[tokio::test]
async fn cli_overrides_resolve_relative_paths_against_cwd() -> std::io::Result<()> {
    let codex_home = tempdir().expect("tempdir");
    let cwd_dir = tempdir().expect("tempdir");
    let cwd_path = cwd_dir.path().to_path_buf();

    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![(
            "log_dir".to_string(),
            TomlValue::String("run-logs".to_string()),
        )])
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd_path.clone()),
            ..Default::default()
        })
        .build()
        .await?;

    let expected = AbsolutePathBuf::resolve_path_against_base("run-logs", cwd_path);
    assert_eq!(config.log_dir, expected.to_path_buf());
    Ok(())
}

#[tokio::test]
async fn returns_config_error_for_schema_error_in_user_config() {
    let tmp = tempdir().expect("tempdir");
    let contents = "model_context_window = \"not_a_number\"";
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .build()
        .await
        .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    let _guard = ra_paths::AbsolutePathBufGuard::new(tmp.path());
    let expected_config_error =
        ra_config::diagnostics::config_error_from_typed_toml::<ConfigToml>(&config_path, contents)
            .expect("schema error");
    assert_eq!(config_error, &expected_config_error);
}

#[tokio::test]
async fn strict_config_rejects_unknown_user_config_key() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"model = "gpt-5"
unknown_key = true"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    let expected_config_error =
        config_error_from_ignored_toml_fields::<ConfigToml>(&config_path, contents)
            .expect("unknown field error");
    assert_eq!(config_error, &expected_config_error);
}

#[tokio::test]
async fn non_strict_config_rejects_mixed_shell_environment_policy_before_higher_layer_merge() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"
[shell_environment_policy]
exclude = ["LEGACY_*"]

[shell_environment_policy.filters]
"CANONICAL_*" = "include"
"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![(
            "shell_environment_policy.filters.HIGHER_*".to_string(),
            TomlValue::String("exclude".to_string()),
        )])
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect_err("one config layer must not mix legacy lists and filters");

    assert_eq!(
        config_error_from_io(&err),
        &config_error_from_typed_toml::<ConfigToml>(&config_path, contents)
            .expect("mixed shell policy should produce a typed config error")
    );
}

#[tokio::test]
async fn shell_environment_policy_unknown_fields_follow_strict_config() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"
[shell_environment_policy]
future_field = true
"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect("non-strict config should ignore unknown fields");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("strict config should reject unknown fields");

    assert_eq!(
        config_error_from_io(&err).message,
        "unknown configuration field `shell_environment_policy.future_field`"
    );
}

#[tokio::test]
async fn non_strict_config_merges_shell_filter_case_variants_across_layers() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"
[shell_environment_policy.filters]
"SECRET_TOKEN" = "exclude"
"#;
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), contents).expect("write config");

    let config = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![(
            "shell_environment_policy.filters.secret_token".to_string(),
            TomlValue::String("include".to_string()),
        )])
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect("higher-priority case-variant filter should override the lower layer");

    assert!(
        config
            .permissions
            .shell_environment_policy
            .exclude
            .is_empty()
    );
    assert_eq!(
        config.permissions.shell_environment_policy.include_only,
        vec![EnvironmentVariablePattern::new_case_insensitive(
            "secret_token"
        )]
    );
}

#[tokio::test]
async fn non_strict_config_rejects_malformed_shell_policy_before_representation_conversion() {
    let cases = [
        (
            r#"
[shell_environment_policy]
exclude = ["SECRET_*", 17]
"#,
            vec![(
                "shell_environment_policy.filters.PATH".to_string(),
                TomlValue::String("include".to_string()),
            )],
        ),
        (
            r#"
[shell_environment_policy.filters]
"SECRET_*" = "keep"
"#,
            vec![(
                "shell_environment_policy.exclude".to_string(),
                TomlValue::Array(vec![TomlValue::String("PATH".to_string())]),
            )],
        ),
    ];

    for (contents, cli_overrides) in cases {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(CONFIG_TOML_FILE), contents).expect("write config");

        ConfigBuilder::default()
            .codex_home(tmp.path().to_path_buf())
            .fallback_cwd(Some(tmp.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .cli_overrides(cli_overrides)
            .strict_config(/*strict_config*/ false)
            .build()
            .await
            .expect_err("malformed shell policy should be rejected before conversion");
    }
}

#[tokio::test]
async fn non_strict_config_allows_replaced_shell_policy_fields_outside_filter_representation() {
    let cases = [
        (
            r#"
[shell_environment_policy]
inherit = "invalid"
set = ["invalid"]
"#,
            vec![
                (
                    "shell_environment_policy.inherit".to_string(),
                    TomlValue::String("core".to_string()),
                ),
                (
                    "shell_environment_policy.set.PATH".to_string(),
                    TomlValue::String("/bin".to_string()),
                ),
            ],
        ),
        (
            r#"shell_environment_policy = 17"#,
            vec![(
                "shell_environment_policy.inherit".to_string(),
                TomlValue::String("core".to_string()),
            )],
        ),
    ];

    for (contents, cli_overrides) in cases {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(CONFIG_TOML_FILE), contents).expect("write config");

        ConfigBuilder::default()
            .codex_home(tmp.path().to_path_buf())
            .fallback_cwd(Some(tmp.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .cli_overrides(cli_overrides)
            .strict_config(/*strict_config*/ false)
            .build()
            .await
            .expect("replaced shell policy fields should preserve normal overlay behavior");
    }
}

#[tokio::test]
async fn malformed_higher_shell_filter_reports_its_layer_when_lower_fields_are_replaced() {
    let tmp = tempdir().expect("tempdir");
    let managed_path = tmp.path().join("managed_config.toml");
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"[shell_environment_policy]
inherit = "invalid"
set = ["invalid"]
"#,
    )
    .expect("write user config");
    std::fs::write(
        &managed_path,
        r#"[shell_environment_policy]
inherit = "core"
set = { PATH = "/bin" }

[shell_environment_policy.filters]
"SECRET_*" = "keep"
"#,
    )
    .expect("write managed config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::with_managed_config_path_for_tests(
            managed_path.clone(),
        ))
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect_err("malformed shell filter should be rejected");

    let config_error = config_error_from_io(&err);
    assert_eq!(config_error.path, managed_path);
    assert!(config_error.message.contains("unknown variant `keep`"));
}

#[tokio::test]
async fn strict_config_rejects_unknown_cli_override_key() {
    let tmp = tempdir().expect("tempdir");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![(
            "foo".to_string(),
            TomlValue::String("bar".to_string()),
        )])
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    assert_eq!(
        err.to_string(),
        "unknown configuration field `foo` in -c/--config override"
    );
}

#[tokio::test]
async fn strict_config_rejects_unknown_cli_override_key_with_relative_path_override() {
    let tmp = tempdir().expect("tempdir");
    let instructions_path = tmp.path().join("instructions.md");
    std::fs::write(&instructions_path, "instructions").expect("write instructions");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![
            (
                "model_instructions_file".to_string(),
                TomlValue::String("instructions.md".to_string()),
            ),
            ("foo".to_string(), TomlValue::String("bar".to_string())),
        ])
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    assert_eq!(
        err.to_string(),
        "unknown configuration field `foo` in -c/--config override"
    );
}

#[tokio::test]
async fn strict_config_rejects_unknown_feature_cli_override_key() {
    let tmp = tempdir().expect("tempdir");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![("features.foo".to_string(), TomlValue::Boolean(true))])
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    assert_eq!(
        err.to_string(),
        "unknown configuration field `features.foo` in -c/--config override"
    );
}

#[tokio::test]
async fn strict_config_rejects_unknown_feature_user_config_key() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"[features]
foo = true"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    assert_eq!(
        config_error.message,
        "unknown configuration field `features.foo`"
    );
    assert_eq!(config_error.range.start.line, 2);
    assert_eq!(config_error.range.start.column, 1);
}

#[tokio::test]
async fn strict_config_accepts_removed_shared_compression_key_without_changing_compression() {
    for enabled in [false, true] {
        let tmp = tempdir().expect("tempdir");
        let removed = !enabled;
        std::fs::write(
            tmp.path().join(CONFIG_TOML_FILE),
            format!(
                "[features]\nlocal_thread_store_compression = {enabled}\nlocal_thread_store_shared_compression = {removed}\n"
            ),
        )
        .expect("write config");

        let config = ConfigBuilder::default()
            .codex_home(tmp.path().to_path_buf())
            .fallback_cwd(Some(tmp.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .strict_config(/*strict_config*/ true)
            .build()
            .await
            .expect("removed feature remains accepted");

        assert_eq!(
            config
                .features
                .enabled(Feature::LocalThreadStoreCompression),
            enabled
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn managed_preferences_expand_home_directory_in_workspace_write_roots() -> anyhow::Result<()>
{
    use base64::Engine;
    use ra_filesystem::legacy::SandboxPolicy;

    let Some(home) = dirs::home_dir() else {
        return Ok(());
    };
    let tmp = tempdir()?;

    let mut loader_overrides =
        LoaderOverrides::with_managed_config_path_for_tests(tmp.path().join("managed_config.toml"));
    loader_overrides.managed_preferences_base64 = Some(
        base64::prelude::BASE64_STANDARD.encode(
            r#"
sandbox_mode = "workspace-write"
[sandbox_workspace_write]
writable_roots = ["~/code"]
"#
            .as_bytes(),
        ),
    );

    let config = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(loader_overrides)
        .build()
        .await?;

    let expected_root = AbsolutePathBuf::from_absolute_path(home.join("code"))?;
    match &config.legacy_sandbox_policy() {
        SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
            assert_eq!(
                writable_roots
                    .iter()
                    .filter(|root| **root == expected_root)
                    .count(),
                1,
            );
        }
        other => panic!("expected workspace-write policy, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn system_requirements_control_in_app_updates() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;

    let default_config = ConfigBuilder::default()
        .codex_home(codex_home.clone())
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    assert!(default_config.features.enabled(Feature::InAppUpdates));

    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
[features]
in_app_updates = false
"#,
    )
    .await?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let managed_config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(overrides)
        .build()
        .await?;

    assert!(!managed_config.features.enabled(Feature::InAppUpdates));
    Ok(())
}

#[tokio::test]
async fn system_managed_default_must_be_allowed() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
default_permissions = "managed-build"

[allowed_permission_profiles]
managed-standard = true

[permissions.managed-standard]
extends = ":read-only"

[permissions.managed-build]
extends = ":workspace"
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
        .expect_err("managed default outside allowed_permission_profiles should fail");

    assert!(
        err.to_string().contains(
            "default_permissions `managed-build` must be allowed by allowed_permission_profiles"
        ),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn load_config_layers_can_ignore_managed_requirements() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;

    let managed_config_path = tmp.path().join("managed_config.toml");
    tokio::fs::write(
        &managed_config_path,
        r#"approval_policy = "never"
"#,
    )
    .await?;
    let system_requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &system_requirements_path,
        r#"allowed_sandbox_modes = ["read-only"]
"#,
    )
    .await?;

    let mut overrides = LoaderOverrides::with_managed_config_path_for_tests(managed_config_path);
    overrides.system_requirements_path = Some(system_requirements_path);
    overrides.ignore_managed_requirements = true;

    let cloud_config_bundle = CloudConfigBundleFixture::loader_with_enterprise_requirement(
        r#"allowed_approval_policies = ["never"]"#,
    );

    let mut config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(overrides)
        .cloud_config_bundle(cloud_config_bundle)
        .build()
        .await?;

    assert!(
        config
            .permissions
            .approval_policy
            .can_set(&AskForApproval::OnRequest)
            .is_ok(),
        "ignoring managed requirements should leave on-request approval allowed"
    );
    config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("ignoring managed requirements should allow setting on-request approval");

    Ok(())
}

#[tokio::test]
async fn project_paths_resolve_relative_to_dot_codex_and_override_in_order() -> std::io::Result<()>
{
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    tokio::fs::create_dir_all(project_root.join(".rusty-agent")).await?;
    tokio::fs::create_dir_all(nested.join(".rusty-agent")).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;

    let root_cfg = r#"
model_instructions_file = "root.txt"
"#;
    let nested_cfg = r#"
model_instructions_file = "child.txt"
"#;
    tokio::fs::write(
        project_root.join(".rusty-agent").join(CONFIG_TOML_FILE),
        root_cfg,
    )
    .await?;
    tokio::fs::write(
        nested.join(".rusty-agent").join(CONFIG_TOML_FILE),
        nested_cfg,
    )
    .await?;
    tokio::fs::write(
        project_root.join(".rusty-agent").join("root.txt"),
        "root instructions",
    )
    .await?;
    tokio::fs::write(
        nested.join(".rusty-agent").join("child.txt"),
        "child instructions",
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(
        &codex_home,
        &project_root,
        TrustLevel::Trusted,
        /*project_root_markers*/ None,
    )
    .await?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested.clone()),
            ..ConfigOverrides::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("child instructions")
    );

    Ok(())
}

#[tokio::test]
async fn cli_override_model_instructions_file_sets_base_instructions() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(codex_home.join(CONFIG_TOML_FILE), "").await?;

    let cwd = tmp.path().join("work");
    tokio::fs::create_dir_all(&cwd).await?;

    let instructions_path = tmp.path().join("instr.md");
    tokio::fs::write(&instructions_path, "cli override instructions").await?;

    let cli_overrides = vec![(
        "model_instructions_file".to_string(),
        TomlValue::String(instructions_path.to_string_lossy().to_string()),
    )];

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .cli_overrides(cli_overrides)
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd),
            ..ConfigOverrides::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("cli override instructions")
    );

    Ok(())
}

#[tokio::test]
async fn inline_instructions_set_base_instructions() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"instructions = "snapshot instructions""#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home)
        .build()
        .await?;

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("snapshot instructions")
    );

    Ok(())
}

#[tokio::test]
async fn cli_override_can_update_project_local_mcp_server_when_project_is_trusted()
-> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    let dot_codex = project_root.join(".rusty-agent");
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&nested).await?;
    tokio::fs::create_dir_all(&dot_codex).await?;
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;
    tokio::fs::write(
        dot_codex.join(CONFIG_TOML_FILE),
        r#"
[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"
enabled = false
"#,
    )
    .await?;
    make_config_for_test(
        &codex_home,
        &project_root,
        TrustLevel::Trusted,
        /*project_root_markers*/ None,
    )
    .await?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .cli_overrides(vec![(
            "mcp_servers.sentry.enabled".to_string(),
            TomlValue::Boolean(true),
        )])
        .fallback_cwd(Some(nested))
        .build()
        .await?;

    let server = config
        .mcp_servers
        .get()
        .get("sentry")
        .expect("trusted project MCP server should load");
    assert!(server.enabled);

    Ok(())
}

#[tokio::test]
async fn cli_override_for_disabled_project_local_mcp_server_returns_invalid_transport()
-> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    let dot_codex = project_root.join(".rusty-agent");
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&nested).await?;
    tokio::fs::create_dir_all(&dot_codex).await?;
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;
    tokio::fs::write(
        dot_codex.join(CONFIG_TOML_FILE),
        r#"
[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"
enabled = false
"#,
    )
    .await?;

    let err = ConfigBuilder::default()
        .codex_home(codex_home)
        .cli_overrides(vec![(
            "mcp_servers.sentry.enabled".to_string(),
            TomlValue::Boolean(true),
        )])
        .fallback_cwd(Some(nested))
        .build()
        .await
        .expect_err("untrusted project layer should not provide MCP transport");

    assert!(
        err.to_string().contains("invalid transport")
            && err.to_string().contains("mcp_servers.sentry"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[tokio::test]
async fn async_compaction_config() {
    use std::io::ErrorKind::InvalidInput;

    for (settings, expected) in [
        ("[features]\nasync_compaction = true", Ok((true, 65))),
        ("[features]\nasync_compaction = false", Ok((false, 65))),
        ("[features.async_compaction]", Ok((false, 65))),
        (
            "[features.async_compaction]\nenabled = true",
            Ok((true, 65)),
        ),
        (
            "[features.async_compaction]\nmodel_async_compact_threshold_percent = 80",
            Ok((false, 80)),
        ),
        (
            "[features.async_compaction]\nenabled = false\nmodel_async_compact_threshold_percent = 80",
            Ok((false, 80)),
        ),
        (
            "[features.async_compaction]\nenabled = true\nmodel_async_compact_threshold_percent = 75",
            Ok((true, 75)),
        ),
        (
            "[features.async_compaction]\nmodel_async_compact_threshold_percent = 0",
            Err(InvalidInput),
        ),
        (
            "[features.async_compaction]\nmodel_async_compact_threshold_percent = 101",
            Err(InvalidInput),
        ),
    ] {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(CONFIG_TOML_FILE), settings).expect("write config");
        let actual = ConfigBuilder::default()
            .codex_home(tmp.path().to_path_buf())
            .fallback_cwd(Some(tmp.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .map(|config| {
                (
                    config.features.enabled(Feature::AsyncCompaction),
                    config.async_compaction_threshold_percent,
                )
            })
            .map_err(|error| error.kind());
        assert_eq!(actual, expected, "{settings}");
    }
}
