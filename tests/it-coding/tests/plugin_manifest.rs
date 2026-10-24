// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::plugins::{
    AbsolutePathBuf,
    manifest::{PluginManifest, load_plugin_manifest},
};
use std::{fs, path::Path};
use tempfile::tempdir;
const MAX_DEFAULT_PROMPT_LEN: usize = 128;
const ALTERNATE_PLUGIN_MANIFEST_RELATIVE_PATH: &str = ".claude-plugin/plugin.json";

fn write_manifest(plugin_root: &Path, version: Option<&str>, interface: &str) {
    fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create manifest dir");
    let version = version
        .map(|version| format!("  \"version\": \"{version}\",\n"))
        .unwrap_or_default();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(
            r#"{{
  "name": "demo-plugin",
{version}
  "interface": {interface}
}}"#
        ),
    )
    .expect("write manifest");
}

fn write_alternate_plugin_manifest(plugin_root: &Path, contents: &str) {
    let manifest_path = plugin_root.join(ALTERNATE_PLUGIN_MANIFEST_RELATIVE_PATH);
    fs::create_dir_all(manifest_path.parent().expect("manifest parent"))
        .expect("create manifest dir");
    fs::write(manifest_path, contents).expect("write manifest");
}

fn load_manifest(plugin_root: &Path) -> PluginManifest {
    load_plugin_manifest(plugin_root).expect("load plugin manifest")
}

#[test]
fn plugin_interface_accepts_legacy_default_prompt_string() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    write_manifest(
        &plugin_root,
        /*version*/ None,
        r#"{
"displayName": "Demo Plugin",
"defaultPrompt": "  Summarize   my inbox  "
  }"#,
    );

    let manifest = load_manifest(&plugin_root);
    let interface = manifest.interface.expect("plugin interface");

    assert_eq!(
        interface.default_prompt,
        Some(vec!["Summarize my inbox".to_string()])
    );
}

#[test]
fn plugin_interface_normalizes_default_prompt_array() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    let too_long = "x".repeat(MAX_DEFAULT_PROMPT_LEN + 1);
    write_manifest(
        &plugin_root,
        /*version*/ None,
        &format!(
            r#"{{
"displayName": "Demo Plugin",
"defaultPrompt": [
  " Summarize my inbox ",
  123,
  "{too_long}",
  "   ",
  "Draft the reply  ",
  "Find   my next action",
  "Archive old mail"
]
  }}"#
        ),
    );

    let manifest = load_manifest(&plugin_root);
    let interface = manifest.interface.expect("plugin interface");

    assert_eq!(
        interface.default_prompt,
        Some(vec![
            "Summarize my inbox".to_string(),
            "Draft the reply".to_string(),
            "Find my next action".to_string(),
        ])
    );
}

#[test]
fn plugin_interface_ignores_invalid_default_prompt_shape() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    write_manifest(
        &plugin_root,
        /*version*/ None,
        r#"{
"displayName": "Demo Plugin",
"defaultPrompt": { "text": "Summarize my inbox" }
  }"#,
    );

    let manifest = load_manifest(&plugin_root);
    let interface = manifest.interface.expect("plugin interface");

    assert_eq!(interface.default_prompt, None);
}

#[test]
fn plugin_interface_reads_dark_logo_path() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    write_manifest(
        &plugin_root,
        /*version*/ None,
        r#"{
"logoDark": "./assets/logo-dark.svg"
  }"#,
    );

    let manifest = load_manifest(&plugin_root);
    let interface = manifest.interface.expect("plugin interface");

    assert_eq!(
        interface.logo_dark,
        Some(
            AbsolutePathBuf::from_absolute_path(plugin_root.join("assets/logo-dark.svg"),)
                .expect("absolute dark logo path")
        )
    );
}

#[test]
fn plugin_manifest_reads_trimmed_version() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    write_manifest(
        &plugin_root,
        Some(" 1.2.3-beta+7 "),
        r#"{
"displayName": "Demo Plugin"
  }"#,
    );

    let manifest = load_manifest(&plugin_root);

    assert_eq!(manifest.version, Some("1.2.3-beta+7".to_string()));
}

#[test]
fn plugin_manifest_reads_keywords() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create manifest dir");
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "demo-plugin",
  "keywords": ["api-key", "developer tools"]
}"#,
    )
    .expect("write manifest");

    let manifest = load_manifest(&plugin_root);

    assert_eq!(
        manifest.keywords,
        vec!["api-key".to_string(), "developer tools".to_string()]
    );
}

#[test]
fn plugin_manifest_uses_alternate_discoverable_path() {
    let tmp = tempdir().expect("tempdir");
    let plugin_root = tmp.path().join("demo-plugin");
    write_alternate_plugin_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "version": " 2.0.0 ",
  "interface": {
"displayName": "Fallback Plugin"
  }
}"#,
    );

    let manifest = load_manifest(&plugin_root);

    assert_eq!(manifest.version, Some("2.0.0".to_string()));
    assert_eq!(
        manifest
            .interface
            .as_ref()
            .and_then(|interface| interface.display_name.as_deref()),
        Some("Fallback Plugin")
    );
}
