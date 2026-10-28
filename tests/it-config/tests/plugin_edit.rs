// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_config::{CONFIG_TOML_FILE, clear_user_plugin, set_user_plugin_enabled};
use std::{fs, path::Path};
use tempfile::TempDir;

#[tokio::test]
async fn set_user_plugin_enabled_writes_plugin_entry() {
    let codex_home = TempDir::new().unwrap();

    set_user_plugin_enabled(
        codex_home.path(),
        "demo@market".to_string(),
        /*enabled*/ true,
    )
    .await
    .unwrap();

    let config = read_config(codex_home.path());
    let expected: toml::Value = toml::from_str(
        r#"
[plugins."demo@market"]
enabled = true
        "#,
    )
    .unwrap();
    assert_eq!(config, expected);
}

#[tokio::test]
async fn set_user_plugin_enabled_preserves_existing_plugin_fields() {
    let codex_home = TempDir::new().unwrap();
    fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[plugins."demo@market"]
enabled = false
source = "/tmp/plugin"
"#,
    )
    .unwrap();

    set_user_plugin_enabled(
        codex_home.path(),
        "demo@market".to_string(),
        /*enabled*/ true,
    )
    .await
    .unwrap();

    let config = read_config(codex_home.path());
    let expected: toml::Value = toml::from_str(
        r#"
[plugins."demo@market"]
enabled = true
source = "/tmp/plugin"
        "#,
    )
    .unwrap();
    assert_eq!(config, expected);
}

#[tokio::test]
async fn clear_user_plugin_removes_empty_plugins_table() {
    let codex_home = TempDir::new().unwrap();
    fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[plugins."demo@market"]
enabled = true
"#,
    )
    .unwrap();

    clear_user_plugin(codex_home.path(), "demo@market".to_string())
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).unwrap(),
        ""
    );
}

#[tokio::test]
async fn clear_user_plugin_missing_entry_does_not_create_config() {
    let codex_home = TempDir::new().unwrap();

    clear_user_plugin(codex_home.path(), "demo@market".to_string())
        .await
        .unwrap();

    assert!(!codex_home.path().join(CONFIG_TOML_FILE).exists());
}

#[tokio::test]
#[cfg(unix)]
async fn set_user_plugin_enabled_follows_config_symlink() {
    use std::os::unix::fs::symlink;

    let codex_home = TempDir::new().unwrap();
    let target_path = codex_home.path().join("target_config.toml");
    symlink(&target_path, codex_home.path().join(CONFIG_TOML_FILE)).unwrap();

    set_user_plugin_enabled(
        codex_home.path(),
        "demo@market".to_string(),
        /*enabled*/ true,
    )
    .await
    .unwrap();

    let config = toml::from_str::<toml::Value>(&fs::read_to_string(target_path).unwrap()).unwrap();
    let expected: toml::Value = toml::from_str(
        r#"
[plugins."demo@market"]
enabled = true
        "#,
    )
    .unwrap();
    assert_eq!(config, expected);
}

fn read_config(codex_home: &Path) -> toml::Value {
    toml::from_str(&fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).unwrap()).unwrap()
}
