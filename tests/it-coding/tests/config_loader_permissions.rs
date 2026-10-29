// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product layer-stack cases from Codex's `core/src/config/config_loader_tests.rs`.
//!
//! Codex builds a full `Config` with `ConfigBuilder`; these cases only read its
//! `config_layer_stack`, so they load that stack directly with `load_config_layers_state`.
//! Cases that assert on the built `Config` wait for the full product configuration.

use ra_config::{
    ConfigPathContext, LoaderOverrides, NoopThreadConfigLoader,
    loader::load_config_layers_state,
    permission_profile_catalog::{PermissionProfileCatalogEntry, permission_profile_catalog},
};
use ra_paths::{AbsolutePathBuf, PathConvention, PathUri};
use tempfile::tempdir;
use toml::Value as TomlValue;

#[tokio::test]
async fn ignore_rules_marks_config_stack_for_exec_policy_rule_skip() -> std::io::Result<()> {
    let tmp = tempdir().expect("tempdir");
    let cwd = AbsolutePathBuf::try_from(tmp.path()).expect("cwd");

    let layers = load_config_layers_state(
        ra_executor::LOCAL_FS.as_ref(),
        tmp.path(),
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides {
            ignore_user_and_project_exec_policy_rules: true,
            ..LoaderOverrides::without_managed_config_for_tests()
        },
        &NoopThreadConfigLoader,
    )
    .await?;

    assert!(layers.ignore_user_and_project_exec_policy_rules());
    Ok(())
}

#[tokio::test]
async fn permission_profile_catalog_marks_profiles_disallowed_by_requirements() -> anyhow::Result<()>
{
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    let requirements_path = tmp.path().join("requirements.toml");
    tokio::fs::write(
        &requirements_path,
        r#"
allowed_sandbox_modes = ["read-only", "workspace-write"]
default_permissions = "managed-standard"

[allowed_permission_profiles]
managed-standard = true

[permissions.managed-standard]
extends = ":workspace"

[permissions.managed-disabled]
extends = ":workspace"
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(tmp.path())?;
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_requirements_path = Some(requirements_path);
    let config_layer_stack = load_config_layers_state(
        ra_executor::LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd.clone()),
        &[] as &[(String, TomlValue)],
        overrides,
        &NoopThreadConfigLoader,
    )
    .await?;

    assert_eq!(
        permission_profile_catalog(
            &config_layer_stack,
            &ConfigPathContext::new(
                PathConvention::native(),
                Some(PathUri::from_abs_path(&cwd)),
                /*user_home_dir*/ None,
            ),
        )?,
        vec![
            PermissionProfileCatalogEntry {
                id: ":read-only".to_string(),
                description: None,
                allowed: false,
            },
            PermissionProfileCatalogEntry {
                id: ":workspace".to_string(),
                description: None,
                allowed: false,
            },
            PermissionProfileCatalogEntry {
                id: ":danger-full-access".to_string(),
                description: None,
                allowed: false,
            },
            PermissionProfileCatalogEntry {
                id: "managed-disabled".to_string(),
                description: None,
                allowed: false,
            },
            PermissionProfileCatalogEntry {
                id: "managed-standard".to_string(),
                description: None,
                allowed: true,
            },
        ]
    );
    Ok(())
}
