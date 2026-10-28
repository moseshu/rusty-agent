// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_config::{
    CONFIG_TOML_FILE, MarketplaceConfigUpdate, RemoveMarketplaceConfigOutcome,
    record_user_marketplace, remove_user_marketplace, remove_user_marketplace_config,
};
use std::fs;
use tempfile::TempDir;

#[test]
fn record_user_marketplace_omits_runtime_update_metadata() {
    let codex_home = TempDir::new().unwrap();
    let update = MarketplaceConfigUpdate {
        source_type: "git",
        source: "https://github.com/owner/repo.git",
        ref_name: Some("main"),
        sparse_paths: &[],
    };

    record_user_marketplace(codex_home.path(), "debug", &update).unwrap();

    let config: toml::Value =
        toml::from_str(&fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).unwrap())
            .unwrap();
    let marketplace = config
        .get("marketplaces")
        .and_then(|marketplaces| marketplaces.get("debug"))
        .expect("marketplace declaration");
    assert_eq!(
        marketplace.get("source_type").and_then(toml::Value::as_str),
        Some("git")
    );
    assert_eq!(
        marketplace.get("source").and_then(toml::Value::as_str),
        Some("https://github.com/owner/repo.git")
    );
    assert_eq!(
        marketplace.get("ref").and_then(toml::Value::as_str),
        Some("main")
    );
    assert!(marketplace.get("last_updated").is_none());
    assert!(marketplace.get("last_revision").is_none());
}

#[test]
fn remove_user_marketplace_removes_requested_entry() {
    let codex_home = TempDir::new().unwrap();
    let update = MarketplaceConfigUpdate {
        source_type: "git",
        source: "https://github.com/owner/repo.git",
        ref_name: Some("main"),
        sparse_paths: &[],
    };
    record_user_marketplace(codex_home.path(), "debug", &update).unwrap();
    record_user_marketplace(codex_home.path(), "other", &update).unwrap();

    let removed = remove_user_marketplace(codex_home.path(), "debug").unwrap();

    assert!(removed);
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).unwrap())
            .unwrap();
    let marketplaces = config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
        .unwrap();
    assert_eq!(marketplaces.len(), 1);
    assert!(marketplaces.contains_key("other"));
}

#[test]
fn remove_user_marketplace_returns_false_when_missing() {
    let codex_home = TempDir::new().unwrap();

    let removed = remove_user_marketplace(codex_home.path(), "debug").unwrap();

    assert!(!removed);
}

#[test]
fn remove_user_marketplace_config_reports_case_mismatch() {
    let codex_home = TempDir::new().unwrap();
    let update = MarketplaceConfigUpdate {
        source_type: "git",
        source: "https://github.com/owner/repo.git",
        ref_name: Some("main"),
        sparse_paths: &[],
    };
    record_user_marketplace(codex_home.path(), "debug", &update).unwrap();

    let outcome = remove_user_marketplace_config(codex_home.path(), "Debug").unwrap();

    assert_eq!(
        outcome,
        RemoveMarketplaceConfigOutcome::NameCaseMismatch {
            configured_name: "debug".to_string()
        }
    );
}

#[test]
fn remove_user_marketplace_config_removes_inline_table_entry() {
    let codex_home = TempDir::new().unwrap();
    fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
marketplaces = {
  debug = { source_type = "git", source = "https://github.com/owner/repo.git" },
  other = { source_type = "local", source = "/tmp/marketplace" },
}
"#,
    )
    .unwrap();

    let outcome = remove_user_marketplace_config(codex_home.path(), "debug").unwrap();

    assert_eq!(outcome, RemoveMarketplaceConfigOutcome::Removed);
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).unwrap())
            .unwrap();
    let marketplaces = config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
        .unwrap();
    assert_eq!(marketplaces.len(), 1);
    assert!(marketplaces.contains_key("other"));
}
