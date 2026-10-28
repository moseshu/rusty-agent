// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use ra_coding::plugins::installed_marketplaces::marketplace_install_root;
use ra_coding::plugins::marketplace_remove::management_test_api::*;
use ra_coding::plugins::marketplace_remove::*;
use ra_config::ConfigLayerEntry;
use ra_config::MarketplaceConfigUpdate;
use ra_config::record_user_marketplace;
use ra_config::{CONFIG_TOML_FILE, ConfigLayerSource, ConfigLayerStack};
use ra_paths::AbsolutePathBuf;
use std::fs;
use tempfile::TempDir;

#[test]
fn remove_marketplace_sync_removes_config_and_installed_root() {
    let codex_home = TempDir::new().unwrap();
    record_user_marketplace(
        codex_home.path(),
        "debug",
        &MarketplaceConfigUpdate {
            source_type: "git",
            source: "https://github.com/owner/repo.git",
            ref_name: Some("main"),
            sparse_paths: &[],
        },
    )
    .unwrap();
    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    fs::create_dir_all(installed_root.join(".agents/plugins")).unwrap();
    fs::write(
        installed_root.join(".agents/plugins/marketplace.json"),
        "{}",
    )
    .unwrap();

    let outcome = remove_marketplace_sync(
        codex_home.path(),
        &ConfigLayerStack::default(),
        MarketplaceRemoveRequest {
            marketplace_name: "debug".to_string(),
        },
    )
    .unwrap();

    assert_eq!(outcome.marketplace_name, "debug");
    assert_eq!(
        outcome.removed_installed_root,
        Some(AbsolutePathBuf::try_from(installed_root.clone()).unwrap())
    );
    let config = fs::read_to_string(codex_home.path().join(ra_config::CONFIG_TOML_FILE)).unwrap();
    assert!(!config.contains("[marketplaces.debug]"));
    assert!(!installed_root.exists());
}

#[test]
fn remove_marketplace_sync_rejects_unknown_marketplace() {
    let codex_home = TempDir::new().unwrap();

    let err = remove_marketplace_sync(
        codex_home.path(),
        &ConfigLayerStack::default(),
        MarketplaceRemoveRequest {
            marketplace_name: "debug".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(
        err.to_string(),
        "marketplace `debug` is not configured or installed"
    );
}

#[test]
fn remove_marketplace_sync_preserves_shadowed_system_marketplace() {
    let codex_home = TempDir::new().unwrap();
    let config = "[marketplaces.debug]\nsource_type = \"git\"\nsource = \"https://github.com/owner/repo.git\"\n";
    let user_file = AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, codex_home.path());
    let system_file = AbsolutePathBuf::resolve_path_against_base("system.toml", codex_home.path());
    fs::write(&user_file, config).unwrap();
    fs::write(&system_file, config).unwrap();
    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    fs::create_dir_all(&installed_root).unwrap();
    let marker = installed_root.join("marker.txt");
    fs::write(&marker, "installed").unwrap();
    let stack = ConfigLayerStack::new(
        vec![
            ConfigLayerEntry::new(
                ConfigLayerSource::System {
                    file: system_file.clone(),
                },
                toml::from_str(config).unwrap(),
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::User {
                    file: user_file.clone(),
                    profile: None,
                },
                toml::from_str(config).unwrap(),
            ),
        ],
        Default::default(),
        Default::default(),
    )
    .unwrap();

    let err = remove_marketplace_sync(
        codex_home.path(),
        &stack,
        MarketplaceRemoveRequest {
            marketplace_name: "debug".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(
        err.to_string(),
        format!(
            "marketplace `debug` is configured in system ({}); remove it from that configuration source instead",
            system_file.display()
        ),
    );
    assert_eq!(fs::read_to_string(user_file).unwrap(), config);
    assert_eq!(fs::read_to_string(system_file).unwrap(), config);
    assert_eq!(fs::read_to_string(marker).unwrap(), "installed");
}

#[test]
fn remove_marketplace_sync_rejects_case_mismatched_configured_name() {
    let codex_home = TempDir::new().unwrap();
    record_user_marketplace(
        codex_home.path(),
        "debug",
        &MarketplaceConfigUpdate {
            source_type: "git",
            source: "https://github.com/owner/repo.git",
            ref_name: Some("main"),
            sparse_paths: &[],
        },
    )
    .unwrap();
    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    fs::create_dir_all(&installed_root).unwrap();

    let err = remove_marketplace_sync(
        codex_home.path(),
        &ConfigLayerStack::default(),
        MarketplaceRemoveRequest {
            marketplace_name: "Debug".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(
        err.to_string(),
        "marketplace `Debug` does not match configured marketplace `debug` exactly"
    );
    assert!(installed_root.exists());
    let config = fs::read_to_string(codex_home.path().join(ra_config::CONFIG_TOML_FILE)).unwrap();
    assert!(config.contains("[marketplaces.debug]"));
}

#[test]
fn remove_marketplace_sync_keeps_installed_root_when_config_removal_fails() {
    let codex_home = TempDir::new().unwrap();
    fs::write(
        codex_home.path().join(ra_config::CONFIG_TOML_FILE),
        "[marketplaces.debug\n",
    )
    .unwrap();
    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    fs::create_dir_all(&installed_root).unwrap();

    let err = remove_marketplace_sync(
        codex_home.path(),
        &ConfigLayerStack::default(),
        MarketplaceRemoveRequest {
            marketplace_name: "debug".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("failed to remove marketplace 'debug' from user config.toml")
    );
    assert!(installed_root.exists());
}

#[test]
fn remove_marketplace_sync_removes_file_installed_root() {
    let codex_home = TempDir::new().unwrap();
    record_user_marketplace(
        codex_home.path(),
        "debug",
        &MarketplaceConfigUpdate {
            source_type: "git",
            source: "https://github.com/owner/repo.git",
            ref_name: Some("main"),
            sparse_paths: &[],
        },
    )
    .unwrap();
    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    fs::create_dir_all(installed_root.parent().unwrap()).unwrap();
    fs::write(&installed_root, "corrupt install root").unwrap();

    let outcome = remove_marketplace_sync(
        codex_home.path(),
        &ConfigLayerStack::default(),
        MarketplaceRemoveRequest {
            marketplace_name: "debug".to_string(),
        },
    )
    .unwrap();

    assert_eq!(
        outcome,
        MarketplaceRemoveOutcome {
            marketplace_name: "debug".to_string(),
            removed_installed_root: Some(
                AbsolutePathBuf::try_from(installed_root.clone()).unwrap()
            ),
        }
    );
    assert!(!installed_root.exists());
    let config = fs::read_to_string(codex_home.path().join(ra_config::CONFIG_TOML_FILE)).unwrap();
    assert!(!config.contains("[marketplaces.debug]"));
}

#[test]
fn remove_marketplace_sync_removes_inline_config_entry() {
    let codex_home = TempDir::new().unwrap();
    fs::write(
        codex_home.path().join(ra_config::CONFIG_TOML_FILE),
        r#"
marketplaces = { debug = { source_type = "git", source = "https://github.com/owner/repo.git" } }
"#,
    )
    .unwrap();
    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    fs::create_dir_all(&installed_root).unwrap();

    let outcome = remove_marketplace_sync(
        codex_home.path(),
        &ConfigLayerStack::default(),
        MarketplaceRemoveRequest {
            marketplace_name: "debug".to_string(),
        },
    )
    .unwrap();

    assert_eq!(outcome.marketplace_name, "debug");
    assert_eq!(
        outcome.removed_installed_root,
        Some(AbsolutePathBuf::try_from(installed_root.clone()).unwrap())
    );
    assert!(!installed_root.exists());
    let config = fs::read_to_string(codex_home.path().join(ra_config::CONFIG_TOML_FILE)).unwrap();
    assert!(!config.contains("debug"));
}
