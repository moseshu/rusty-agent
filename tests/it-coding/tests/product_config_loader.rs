//! Product consumers use the real source stack rather than a parallel configuration model.
use anyhow::Result;
use it_config::support::CloudConfigBundleFixture;
use ra_coding::{
    exec_policy::{ExecPolicyManager, ExecPolicySources, load_exec_policy},
    plugins::PluginConfigManager,
};
use ra_config::{ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_paths::AbsolutePathBuf;
use std::{path::Path, sync::Arc};

fn manager(home: &Path, requirements: &str, ignore_user_config: bool) -> PluginConfigManager {
    let home = AbsolutePathBuf::from_absolute_path(home).unwrap();
    PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        home.clone(),
        home,
        ConfigLoadOptions {
            loader_overrides: LoaderOverrides {
                ignore_user_config,
                ..LoaderOverrides::without_managed_config_for_tests()
            },
            cloud_config_bundle: CloudConfigBundleFixture::loader_with_enterprise_requirement(
                requirements,
            ),
            strict_config: false,
        },
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
}

#[tokio::test]
async fn managed_provider_wins_for_plugin_consumers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::write(
        temp.path().join("config.toml"),
        "model = 'custom-model'\nmodel_provider = 'user-provider'",
    )?;
    let config_manager = manager(temp.path(), "model_provider = 'managed-provider'", false);
    let cfg = config_manager
        .load_for_cwd(AbsolutePathBuf::from_absolute_path(temp.path())?)
        .await?;
    assert_eq!(cfg.model_provider_id, "managed-provider");
    assert_eq!(
        cfg.config_layer_stack.required_model_provider(),
        Some("managed-provider")
    );
    Ok(())
}

#[tokio::test]
async fn malformed_environment_file_is_skipped_only_with_ignore_user_config() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("environments.toml"), "invalid = [")?;
    assert!(
        manager(temp.path(), "", false)
            .prepare_environment_manager()
            .await
            .is_err()
    );
    manager(temp.path(), "", true)
        .prepare_environment_manager()
        .await?;
    Ok(())
}

#[tokio::test]
async fn rules_keep_managed_rules_when_user_and_project_rules_are_ignored() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::create_dir(temp.path().join("rules"))?;
    std::fs::write(
        temp.path().join("rules/default.rules"),
        "prefix_rule(pattern=['echo'], decision='allow')",
    )?;
    let manager = manager(
        temp.path(),
        "[[rules.prefix_rules]]\npattern = [{ token = 'managed-command' }]\ndecision = 'forbidden'",
        false,
    );
    let stack = manager
        .load_config_layers_for_cwd(Some(AbsolutePathBuf::from_absolute_path(temp.path())?))
        .await?;
    let sources = ExecPolicySources::from_config_layer_stack(&stack);
    assert!(
        sources
            .config_folders()
            .iter()
            .any(|folder| folder == temp.path())
    );
    ExecPolicyManager::load(&sources).await?;
    let policy = load_exec_policy(&sources).await?;
    assert_eq!(
        policy
            .check(&["echo".to_owned()], &|_| ra_execpolicy::Decision::Prompt)
            .decision,
        ra_execpolicy::Decision::Allow
    );
    assert_eq!(
        policy
            .check(&["managed-command".to_owned()], &|_| {
                ra_execpolicy::Decision::Allow
            })
            .decision,
        ra_execpolicy::Decision::Forbidden
    );
    let stack = stack.with_user_and_project_exec_policy_rules_ignored(true);
    let sources = ExecPolicySources::from_config_layer_stack(&stack);
    assert!(
        !sources
            .config_folders()
            .iter()
            .any(|folder| folder == temp.path())
    );
    assert!(sources.requirements().is_some());
    ExecPolicyManager::load(&sources).await?;
    let policy = load_exec_policy(&sources).await?;
    assert_eq!(
        policy
            .check(&["echo".to_owned()], &|_| ra_execpolicy::Decision::Prompt)
            .decision,
        ra_execpolicy::Decision::Prompt
    );
    assert_eq!(
        policy
            .check(&["managed-command".to_owned()], &|_| {
                ra_execpolicy::Decision::Allow
            })
            .decision,
        ra_execpolicy::Decision::Forbidden
    );
    Ok(())
}

#[tokio::test]
async fn disabled_project_layers_do_not_supply_execution_rules() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir(&home)?;
    std::fs::create_dir_all(project.join(".git"))?;
    std::fs::create_dir_all(project.join(".rusty-agent/rules"))?;
    std::fs::write(
        project.join(".rusty-agent/config.toml"),
        "model = 'project-model'",
    )?;
    std::fs::write(
        project.join(".rusty-agent/rules/default.rules"),
        "prefix_rule(pattern=['untrusted-command'], decision='allow')",
    )?;
    let stack = manager(&home, "", false)
        .load_config_layers_for_cwd(Some(AbsolutePathBuf::from_absolute_path(&project)?))
        .await?;
    assert!(
        stack
            .all_layers_low_to_high()
            .any(|layer| layer.disabled_reason.is_some()
                && matches!(layer.name, ra_config::ConfigLayerSource::Project { .. }))
    );
    let sources = ExecPolicySources::from_config_layer_stack(&stack);
    assert!(
        !sources
            .config_folders()
            .iter()
            .any(|folder| folder == &project.join(".rusty-agent"))
    );
    let policy = load_exec_policy(&sources).await?;
    assert_eq!(
        policy
            .check(&["untrusted-command".to_owned()], &|_| {
                ra_execpolicy::Decision::Prompt
            })
            .decision,
        ra_execpolicy::Decision::Prompt
    );
    Ok(())
}

#[tokio::test]
async fn permission_catalog_excludes_full_access_when_managed_denials_apply() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let stack = manager(
        temp.path(),
        "[permissions.filesystem]\ndeny_read = ['/private-config']",
        false,
    )
    .load_config_layers_for_cwd(None)
    .await?;
    let context = ra_config::ConfigPathContext::new(
        ra_paths::PathConvention::native(),
        Some(ra_paths::PathUri::from_host_native_path(temp.path())?),
        None,
    );
    let catalog =
        ra_config::permission_profile_catalog::permission_profile_catalog(&stack, &context)?;
    assert!(
        catalog
            .iter()
            .any(|entry| entry.id == ":read-only" && entry.allowed)
    );
    assert!(
        catalog
            .iter()
            .any(|entry| entry.id == ":danger-full-access" && !entry.allowed)
    );
    Ok(())
}
