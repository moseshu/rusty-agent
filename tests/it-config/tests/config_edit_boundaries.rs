use ra_config::{
    CONFIG_TOML_FILE, MarketplaceConfigUpdate, PluginConfigEdit, RemoveMarketplaceConfigOutcome,
    apply_user_plugin_config_edits, record_user_marketplace, remove_user_marketplace_config,
    set_user_plugin_enabled,
};
use std::fs;

#[tokio::test]
async fn ordered_plugin_edits_preserve_comments_and_other_fields() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(CONFIG_TOML_FILE);
    fs::write(
        &path,
        "# personal preferences\nmodel = 'custom'\n[plugins.'demo.with.dots@market']\nenabled = false # activation\nsource = '/explicit/plugin'\n",
    )
    .unwrap();
    apply_user_plugin_config_edits(
        home.path(),
        vec![
            PluginConfigEdit::SetEnabled {
                plugin_key: "temporary@market".into(),
                enabled: true,
            },
            PluginConfigEdit::Clear {
                plugin_key: "temporary@market".into(),
            },
            PluginConfigEdit::SetEnabled {
                plugin_key: "demo.with.dots@market".into(),
                enabled: true,
            },
        ],
    )
    .await
    .unwrap();
    let raw = fs::read_to_string(path).unwrap();
    assert!(raw.contains("# personal preferences"));
    assert!(raw.contains("# activation"));
    let value: toml::Value = toml::from_str(&raw).unwrap();
    assert_eq!(value["model"].as_str(), Some("custom"));
    let plugins = value["plugins"].as_table().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(
        plugins["demo.with.dots@market"]["enabled"].as_bool(),
        Some(true)
    );
    assert_eq!(
        plugins["demo.with.dots@market"]["source"].as_str(),
        Some("/explicit/plugin")
    );
}

#[tokio::test]
async fn empty_batch_has_no_filesystem_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let absent = home.path().join("not-created");
    apply_user_plugin_config_edits(&absent, Vec::new())
        .await
        .unwrap();
    assert!(!absent.exists());
}

#[tokio::test]
async fn malformed_config_is_not_overwritten_by_either_editor() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(CONFIG_TOML_FILE);
    let invalid = "# retain damaged content\n[plugins\n";
    fs::write(&path, invalid).unwrap();
    let error = set_user_plugin_enabled(home.path(), "demo@market".into(), true)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let update = MarketplaceConfigUpdate {
        source_type: "local",
        source: "/explicit/marketplace",
        ref_name: None,
        sparse_paths: &[],
    };
    assert_eq!(
        record_user_marketplace(home.path(), "demo", &update)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(fs::read_to_string(path).unwrap(), invalid);
}

#[test]
fn case_mismatch_does_not_rewrite_the_marketplace_file() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(CONFIG_TOML_FILE);
    let original = "# exact spelling\nmarketplaces = { Demo = { source_type = 'local', source = '/market' } }\n";
    fs::write(&path, original).unwrap();
    assert_eq!(
        remove_user_marketplace_config(home.path(), "demo").unwrap(),
        RemoveMarketplaceConfigOutcome::NameCaseMismatch {
            configured_name: "Demo".into()
        }
    );
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_editor_follows_relative_symlink_chain_without_replacing_links() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().unwrap();
    let nested = home.path().join("nested");
    fs::create_dir(&nested).unwrap();
    symlink("nested/link.toml", home.path().join(CONFIG_TOML_FILE)).unwrap();
    symlink("../actual.toml", nested.join("link.toml")).unwrap();
    set_user_plugin_enabled(home.path(), "demo@market".into(), true)
        .await
        .unwrap();
    assert!(home.path().join(CONFIG_TOML_FILE).is_symlink());
    assert!(nested.join("link.toml").is_symlink());
    let value: toml::Value =
        toml::from_str(&fs::read_to_string(home.path().join("actual.toml")).unwrap()).unwrap();
    assert_eq!(
        value["plugins"]["demo@market"]["enabled"].as_bool(),
        Some(true)
    );
}
