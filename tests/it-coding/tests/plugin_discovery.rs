// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::plugins::discovery::{
    AGENT_PLUGIN_MANIFEST_RELATIVE_PATH, AGENT_PLUGIN_SCHEMA_URI, AgentPluginSchemaStatus,
    agent_plugin_schema_status, find_plugin_manifest_path,
};
use std::fs;
use tempfile::tempdir;
const ALTERNATE_PLUGIN_CLA_MANIFEST_RELATIVE_PATH: &str = ".claude-plugin/plugin.json";
const ALTERNATE_PLUGIN_CUR_MANIFEST_RELATIVE_PATH: &str = ".cursor-plugin/plugin.json";

#[test]
fn recognizes_schema_declared_root_plugin_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/portable");
    let skill_path = plugin_root.join("skills/search/SKILL.md");
    fs::create_dir_all(skill_path.parent().expect("parent")).expect("mkdir");
    let manifest_path = plugin_root.join(AGENT_PLUGIN_MANIFEST_RELATIVE_PATH);
    fs::write(
        &manifest_path,
        format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"portable"}}"#),
    )
    .expect("write manifest");

    assert_eq!(find_plugin_manifest_path(&plugin_root), Some(manifest_path));
}

#[test]
fn ignores_unrelated_root_plugin_manifest_before_legacy_fallback() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let legacy_path = plugin_root.join(".codex-plugin/plugin.json");
    fs::create_dir_all(legacy_path.parent().expect("parent")).expect("mkdir");
    fs::write(plugin_root.join("plugin.json"), r#"{"name":"npm-package"}"#)
        .expect("write unrelated root");
    fs::write(&legacy_path, r#"{"name":"sample"}"#).expect("write legacy");

    assert_eq!(find_plugin_manifest_path(&plugin_root), Some(legacy_path));
}

#[test]
fn rejects_nonregular_root_plugin_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let legacy_path = plugin_root.join(".codex-plugin/plugin.json");
    fs::create_dir_all(plugin_root.join("plugin.json")).expect("root manifest directory");
    fs::create_dir_all(legacy_path.parent().expect("parent")).expect("legacy parent");
    fs::write(&legacy_path, r#"{"name":"sample"}"#).expect("legacy manifest");

    assert_eq!(find_plugin_manifest_path(&plugin_root), None);
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_root_plugin_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let manifest_target = tmp.path().join("manifest.json");
    let legacy_path = plugin_root.join(".codex-plugin/plugin.json");
    fs::create_dir_all(&plugin_root).expect("plugin root");
    fs::write(
        &manifest_target,
        format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"sample"}}"#),
    )
    .expect("manifest target");
    std::os::unix::fs::symlink(&manifest_target, plugin_root.join("plugin.json"))
        .expect("root manifest symlink");
    fs::create_dir_all(legacy_path.parent().expect("parent")).expect("legacy parent");
    fs::write(&legacy_path, r#"{"name":"sample"}"#).expect("legacy manifest");

    assert_eq!(find_plugin_manifest_path(&plugin_root), None);
}

#[test]
fn rejects_nonregular_legacy_plugin_manifest_before_lower_precedence_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let codex_path = plugin_root.join(".codex-plugin/plugin.json");
    let claude_path = plugin_root.join(ALTERNATE_PLUGIN_CLA_MANIFEST_RELATIVE_PATH);
    fs::create_dir_all(&codex_path).expect("nonregular Codex manifest");
    fs::create_dir_all(claude_path.parent().expect("Claude manifest parent"))
        .expect("Claude manifest parent");
    fs::write(&claude_path, r#"{"name":"sample"}"#).expect("Claude manifest");

    assert_eq!(find_plugin_manifest_path(&plugin_root), None);
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_legacy_plugin_manifest_before_lower_precedence_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let codex_path = plugin_root.join(".codex-plugin/plugin.json");
    let claude_path = plugin_root.join(ALTERNATE_PLUGIN_CLA_MANIFEST_RELATIVE_PATH);
    fs::create_dir_all(codex_path.parent().expect("Codex manifest parent"))
        .expect("Codex manifest parent");
    fs::create_dir_all(claude_path.parent().expect("Claude manifest parent"))
        .expect("Claude manifest parent");
    fs::write(plugin_root.join("benign.json"), r#"{"name":"sample"}"#).expect("benign manifest");
    fs::write(&claude_path, r#"{"name":"sample"}"#).expect("Claude manifest");
    std::os::unix::fs::symlink("../benign.json", &codex_path).expect("Codex manifest symlink");

    assert_eq!(find_plugin_manifest_path(&plugin_root), None);
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_legacy_plugin_manifest_directory_before_lower_precedence_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let manifest_directory = tmp.path().join("manifest-directory");
    let claude_path = plugin_root.join(ALTERNATE_PLUGIN_CLA_MANIFEST_RELATIVE_PATH);
    fs::create_dir_all(&manifest_directory).expect("manifest target directory");
    fs::create_dir_all(claude_path.parent().expect("Claude manifest parent"))
        .expect("Claude manifest parent");
    fs::write(
        manifest_directory.join("plugin.json"),
        r#"{"name":"sample"}"#,
    )
    .expect("benign manifest");
    fs::write(&claude_path, r#"{"name":"sample"}"#).expect("Claude manifest");
    std::os::unix::fs::symlink(&manifest_directory, plugin_root.join(".codex-plugin"))
        .expect("Codex manifest directory symlink");

    assert_eq!(find_plugin_manifest_path(&plugin_root), None);
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_claude_manifest_before_cursor_manifest() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let claude_path = plugin_root.join(ALTERNATE_PLUGIN_CLA_MANIFEST_RELATIVE_PATH);
    let cursor_path = plugin_root.join(ALTERNATE_PLUGIN_CUR_MANIFEST_RELATIVE_PATH);
    fs::create_dir_all(claude_path.parent().expect("Claude manifest parent"))
        .expect("Claude manifest parent");
    fs::create_dir_all(cursor_path.parent().expect("Cursor manifest parent"))
        .expect("Cursor manifest parent");
    let manifest_target = plugin_root.join("benign.json");
    fs::write(&manifest_target, r#"{"name":"sample"}"#).expect("benign manifest");
    fs::write(&cursor_path, r#"{"name":"sample"}"#).expect("Cursor manifest");
    std::os::unix::fs::symlink(&manifest_target, &claude_path).expect("Claude manifest symlink");

    assert_eq!(find_plugin_manifest_path(&plugin_root), None);
}

#[test]
fn preserves_codex_claude_cursor_legacy_precedence() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("plugins/sample");
    let codex_path = plugin_root.join(".codex-plugin/plugin.json");
    let claude_path = plugin_root.join(".claude-plugin/plugin.json");
    let cursor_path = plugin_root.join(".cursor-plugin/plugin.json");
    for path in [&codex_path, &claude_path, &cursor_path] {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, r#"{"name":"sample"}"#).expect("write manifest");
    }

    assert_eq!(
        find_plugin_manifest_path(&plugin_root),
        Some(codex_path.clone())
    );
    fs::remove_file(codex_path).expect("remove Codex manifest");
    assert_eq!(find_plugin_manifest_path(&plugin_root), Some(claude_path));
}

#[test]
fn schema_classification_does_not_validate_portable_fields() {
    assert_eq!(
        agent_plugin_schema_status("{broken"),
        AgentPluginSchemaStatus::Unrelated
    );
    assert_eq!(
        agent_plugin_schema_status("null"),
        AgentPluginSchemaStatus::Unrelated
    );
    assert_eq!(
        agent_plugin_schema_status(r#"{"$schema":42}"#),
        AgentPluginSchemaStatus::Unrelated
    );
    assert_eq!(
        agent_plugin_schema_status(r#"{"$schema":"https://example.com/schema"}"#),
        AgentPluginSchemaStatus::Unrelated
    );
    assert_eq!(
        agent_plugin_schema_status(&format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}"}}"#)),
        AgentPluginSchemaStatus::Supported
    );
    assert_eq!(
        agent_plugin_schema_status(
            r#"{"$schema":"https://agent-plugins.org/schemas/99/plugin.schema.json"}"#
        ),
        AgentPluginSchemaStatus::Unsupported
    );
}
