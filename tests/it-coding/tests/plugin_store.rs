// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Non-account Store tests ported through the public and source-internal test entries.

use api::old_version_stays_active as old_plugin_version_would_stay_active;
use ra_coding::plugins::{
    AbsolutePathBuf, PluginId,
    store::{DEFAULT_PLUGIN_VERSION, PluginInstallResult, PluginStore, test_api as api},
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::tempdir;

fn write_plugin_with_version(
    root: &Path,
    dir_name: &str,
    manifest_name: &str,
    manifest_version: Option<&str>,
) {
    let plugin_root = root.join(dir_name);
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::create_dir_all(plugin_root.join("skills")).unwrap();
    let version = manifest_version
        .map(|manifest_version| format!(r#","version":"{manifest_version}""#))
        .unwrap_or_default();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{manifest_name}"{version}}}"#),
    )
    .unwrap();
    fs::write(plugin_root.join("skills/SKILL.md"), "skill").unwrap();
    fs::write(plugin_root.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
}

fn write_plugin(root: &Path, dir_name: &str, manifest_name: &str) {
    write_plugin_with_version(
        root,
        dir_name,
        manifest_name,
        /*manifest_version*/ None,
    );
}

#[test]
fn try_new_rejects_relative_codex_home() {
    let err = PluginStore::try_new(PathBuf::from("relative"))
        .expect_err("relative codex home should fail");
    let err = err.to_string().replace('\\', "/");

    assert_eq!(
        err,
        "failed to resolve plugin cache root: path is not absolute: relative/plugins/cache"
    );
}

#[test]
fn install_copies_plugin_into_default_marketplace() {
    let tmp = tempdir().unwrap();
    write_plugin(tmp.path(), "sample-plugin", "sample-plugin");
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("sample-plugin")).unwrap(),
            plugin_id.clone(),
        )
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/sample-plugin/local");
    assert_eq!(
        result,
        PluginInstallResult {
            plugin_id,
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
        }
    );
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
    assert!(installed_path.join("skills/SKILL.md").is_file());
}

#[test]
fn install_accepts_manifest_mcp_server_objects() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("counter-sample");
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "counter-sample",
  "version": "1.1.1",
  "mcpServers": {
    "counter": {
      "type": "http",
      "url": "https://sample.example/counter/mcp"
    }
  }
}"#,
    )
    .unwrap();
    let plugin_id = PluginId::new("counter-sample".to_string(), "debug".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(
            AbsolutePathBuf::try_from(plugin_root).unwrap(),
            plugin_id.clone(),
        )
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/counter-sample/1.1.1");
    assert_eq!(
        result,
        PluginInstallResult {
            plugin_id,
            plugin_version: "1.1.1".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
        }
    );
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
}

#[cfg(unix)]
#[test]
fn install_rejects_symlinked_manifest_that_hides_lower_precedence_mcp_server() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("manifest-switch");
    let codex_path = plugin_root.join(".codex-plugin/plugin.json");
    let claude_path = plugin_root.join(".claude-plugin/plugin.json");
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
    fs::write(
        plugin_root.join("benign.json"),
        r#"{"name":"manifest-switch","version":"1.2.3"}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink("../benign.json", &codex_path).unwrap();
    fs::write(
        &claude_path,
        r#"{"name":"manifest-switch","version":"1.2.3","mcpServers":{"hidden":{"command":"/bin/sh"}}}"#,
    )
    .unwrap();
    let plugin_id = PluginId::new("manifest-switch".to_string(), "debug".to_string()).unwrap();

    let err = PluginStore::new(tmp.path().to_path_buf())
        .install(AbsolutePathBuf::try_from(plugin_root).unwrap(), plugin_id)
        .expect_err("a symlinked manifest must not conceal a different installed manifest");

    assert_eq!(err.to_string(), "missing plugin.json");
    assert!(
        !tmp.path()
            .join("plugins/cache/debug/manifest-switch")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn install_rejects_symlinked_manifest_directory() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("manifest-switch");
    let manifest_directory = tmp.path().join("manifest-directory");
    let claude_path = plugin_root.join(".claude-plugin/plugin.json");
    fs::create_dir_all(&manifest_directory).unwrap();
    fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
    fs::write(
        manifest_directory.join("plugin.json"),
        r#"{"name":"manifest-switch"}"#,
    )
    .unwrap();
    fs::write(&claude_path, r#"{"name":"manifest-switch"}"#).unwrap();
    std::os::unix::fs::symlink(&manifest_directory, plugin_root.join(".codex-plugin")).unwrap();
    let plugin_id = PluginId::new("manifest-switch".to_string(), "debug".to_string()).unwrap();

    let err = PluginStore::new(tmp.path().to_path_buf())
        .install(AbsolutePathBuf::try_from(plugin_root).unwrap(), plugin_id)
        .expect_err("a symlinked manifest directory must not conceal a different manifest");

    assert_eq!(err.to_string(), "missing plugin.json");
    assert!(
        !tmp.path()
            .join("plugins/cache/debug/manifest-switch")
            .exists()
    );
}

#[test]
fn install_uses_manifest_name_for_destination_and_key() {
    let tmp = tempdir().unwrap();
    write_plugin(tmp.path(), "source-dir", "manifest-name");
    let plugin_id = PluginId::new("manifest-name".to_string(), "market".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("source-dir")).unwrap(),
            plugin_id.clone(),
        )
        .unwrap();

    assert_eq!(
        result,
        PluginInstallResult {
            plugin_id,
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(
                tmp.path().join("plugins/cache/market/manifest-name/local"),
            )
            .unwrap(),
        }
    );
}

#[test]
fn plugin_root_derives_path_from_key_and_version() {
    let tmp = tempdir().unwrap();
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample".to_string(), "debug".to_string()).unwrap();

    assert_eq!(
        store.plugin_root(&plugin_id, "local").as_path(),
        tmp.path().join("plugins/cache/debug/sample/local")
    );
}

#[test]
fn plugin_data_root_derives_path_from_key() {
    let tmp = tempdir().unwrap();
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample".to_string(), "debug".to_string()).unwrap();

    assert_eq!(
        store.plugin_data_root(&plugin_id).as_path(),
        tmp.path().join("plugins/data/sample-debug")
    );
}

#[test]
fn agent_plugin_data_root_is_stable_and_unambiguous() {
    let tmp = tempdir().unwrap();
    let store = PluginStore::new(tmp.path().to_path_buf());
    let first = PluginId::new("a-b".to_string(), "c".to_string()).unwrap();
    let second = PluginId::new("a".to_string(), "b-c".to_string()).unwrap();

    let first_root = api::agent_data_root(&store, &first);
    let second_root = api::agent_data_root(&store, &second);
    let expected_parent = tmp.path().join("plugins/data/agent-plugins");

    assert_ne!(first_root, second_root);
    assert_eq!(
        first_root.as_path(),
        expected_parent.join("6920dd17774030852d11d1b94758fcaae4f894c7b2f36301ed174bc3b33e0743")
    );
    assert_eq!(
        second_root.as_path(),
        expected_parent.join("fa89b988ebbe54a68fdcbeb87fb913a5238d482084a3cee49a86288c2d45fa90")
    );
}

#[test]
fn install_with_version_uses_requested_cache_version() {
    let tmp = tempdir().unwrap();
    write_plugin(tmp.path(), "sample-plugin", "sample-plugin");
    let plugin_id =
        PluginId::new("sample-plugin".to_string(), "openai-curated".to_string()).unwrap();
    let plugin_version = "0123456789abcdef".to_string();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install_with_version(
            AbsolutePathBuf::try_from(tmp.path().join("sample-plugin")).unwrap(),
            plugin_id.clone(),
            plugin_version.clone(),
        )
        .unwrap();

    let installed_path = tmp.path().join(format!(
        "plugins/cache/openai-curated/sample-plugin/{plugin_version}"
    ));
    assert_eq!(
        result,
        PluginInstallResult {
            plugin_id,
            plugin_version,
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
        }
    );
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
}

#[test]
fn install_prefers_on_disk_manifest_version_over_fallback() {
    let tmp = tempdir().unwrap();
    write_plugin_with_version(
        tmp.path(),
        "sample-plugin",
        "sample-plugin",
        Some("1.2.3-beta+7"),
    );
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    let result = api::install_with_fallback(
        &PluginStore::new(tmp.path().to_path_buf()),
        AbsolutePathBuf::try_from(tmp.path().join("sample-plugin")).unwrap(),
        plugin_id.clone(),
        r#"{"name":"sample-plugin","version":"9.9.9"}"#,
    )
    .unwrap();

    let installed_path = tmp
        .path()
        .join("plugins/cache/debug/sample-plugin/1.2.3-beta+7");
    assert_eq!(
        result,
        PluginInstallResult {
            plugin_id,
            plugin_version: "1.2.3-beta+7".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path.clone()).unwrap(),
        }
    );
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
}

#[test]
fn install_stages_fallback_manifest_when_source_has_no_manifest() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("fallback-plugin");
    fs::create_dir_all(plugin_root.join("skills")).unwrap();
    let manifest = r#"{"name":"fallback-plugin","version":"1.2.3"}"#;
    let plugin_id = PluginId::new("fallback-plugin".to_string(), "debug".to_string()).unwrap();

    let result = api::install_with_fallback(
        &PluginStore::new(tmp.path().to_path_buf()),
        AbsolutePathBuf::try_from(plugin_root).unwrap(),
        plugin_id,
        manifest,
    )
    .expect("install plugin with fallback manifest");

    assert_eq!(
        fs::read_to_string(
            result
                .installed_path
                .as_path()
                .join(".codex-plugin/plugin.json")
        )
        .unwrap(),
        manifest,
    );
}

#[test]
fn install_rejects_blank_manifest_version() {
    let tmp = tempdir().unwrap();
    write_plugin_with_version(tmp.path(), "sample-plugin", "sample-plugin", Some("   "));
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    let err = PluginStore::new(tmp.path().to_path_buf())
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("sample-plugin")).unwrap(),
            plugin_id,
        )
        .expect_err("blank manifest version should be rejected");
    let err = err.to_string().replace('\\', "/");

    assert_eq!(
        err,
        "invalid plugin version in plugin.json: must not be blank"
    );
}

#[test]
fn agent_plugin_blank_version_uses_default_version() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("agent-plugin");
    fs::create_dir_all(&plugin_root).unwrap();
    fs::write(
        plugin_root.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"agent-plugin","version":"   "}"#,
    )
    .unwrap();
    let plugin_id = PluginId::new("agent-plugin".to_string(), "debug".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(AbsolutePathBuf::try_from(plugin_root).unwrap(), plugin_id)
        .expect("install Agent Plugin");

    assert_eq!(result.plugin_version, "1.0.0");
}

#[test]
fn agent_plugin_install_does_not_migrate_commands() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("agent-plugin");
    fs::create_dir_all(plugin_root.join("commands")).unwrap();
    fs::write(
        plugin_root.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"agent-plugin","commands":"./commands"}"#,
    )
    .unwrap();
    fs::write(plugin_root.join("commands/demo.md"), "# Demo").unwrap();
    let plugin_id = PluginId::new("agent-plugin".to_string(), "debug".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(AbsolutePathBuf::try_from(plugin_root).unwrap(), plugin_id)
        .expect("install Agent Plugin");

    assert!(
        !result
            .installed_path
            .as_path()
            .join(".codex-plugin/migrated-command-skills")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn agent_plugin_install_skips_symlinked_skill_file() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("agent-plugin");
    let skill_root = plugin_root.join("skills/greet");
    fs::create_dir_all(&skill_root).unwrap();
    fs::write(
        plugin_root.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"agent-plugin"}"#,
    )
    .unwrap();
    let outside_skill = tmp.path().join("outside-SKILL.md");
    fs::write(&outside_skill, "---\nname: greet\n---\n").unwrap();
    std::os::unix::fs::symlink(&outside_skill, skill_root.join("SKILL.md")).unwrap();
    let plugin_id = PluginId::new("agent-plugin".to_string(), "debug".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(AbsolutePathBuf::try_from(plugin_root).unwrap(), plugin_id)
        .expect("install Agent Plugin");

    assert!(
        result
            .installed_path
            .as_path()
            .join("plugin.json")
            .is_file()
    );
    assert!(
        !result
            .installed_path
            .as_path()
            .join("skills/greet/SKILL.md")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn agent_plugin_install_skips_symlinked_executable() {
    let tmp = tempdir().unwrap();
    let plugin_root = tmp.path().join("agent-plugin");
    let bin_root = plugin_root.join("bin");
    fs::create_dir_all(&bin_root).unwrap();
    fs::write(
        plugin_root.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"agent-plugin"}"#,
    )
    .unwrap();
    let outside_executable = tmp.path().join("outside-tool");
    fs::write(&outside_executable, "#!/bin/sh\n").unwrap();
    std::os::unix::fs::symlink(&outside_executable, bin_root.join("tool")).unwrap();
    let plugin_id = PluginId::new("agent-plugin".to_string(), "debug".to_string()).unwrap();

    let result = PluginStore::new(tmp.path().to_path_buf())
        .install(AbsolutePathBuf::try_from(plugin_root).unwrap(), plugin_id)
        .expect("install Agent Plugin");

    assert!(
        result
            .installed_path
            .as_path()
            .join("plugin.json")
            .is_file()
    );
    assert!(!result.installed_path.as_path().join("bin/tool").exists());
}

#[test]
fn active_plugin_version_reads_version_directory_name() {
    let tmp = tempdir().unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/local",
        "sample-plugin",
    );
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    assert_eq!(
        store.active_plugin_version(&plugin_id),
        Some("local".to_string())
    );
    assert_eq!(
        store.active_plugin_root(&plugin_id).unwrap().as_path(),
        tmp.path().join("plugins/cache/debug/sample-plugin/local")
    );
}

#[test]
fn active_plugin_version_prefers_default_local_version_when_multiple_versions_exist() {
    let tmp = tempdir().unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/0123456789abcdef",
        "sample-plugin",
    );
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/local",
        "sample-plugin",
    );
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    assert_eq!(
        store.active_plugin_version(&plugin_id),
        Some("local".to_string())
    );
}

#[test]
fn active_plugin_version_returns_latest_version_when_default_is_missing() {
    let tmp = tempdir().unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/0123456789abcdef",
        "sample-plugin",
    );
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/fedcba9876543210",
        "sample-plugin",
    );
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    assert_eq!(
        store.active_plugin_version(&plugin_id),
        Some("fedcba9876543210".to_string())
    );
}

#[test]
fn active_plugin_version_compares_semver_versions_semantically() {
    let tmp = tempdir().unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/9.0.0",
        "sample-plugin",
    );
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/10.0.0",
        "sample-plugin",
    );
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    assert_eq!(
        store.active_plugin_version(&plugin_id),
        Some("10.0.0".to_string())
    );
}

#[test]
fn install_with_new_version_keeps_existing_plugin_root_and_prunes_old_versions() {
    let tmp = tempdir().unwrap();
    let store = PluginStore::new(tmp.path().to_path_buf());
    let plugin_id = PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap();

    write_plugin_with_version(tmp.path(), "v1", "sample-plugin", Some("1.0.0"));
    store
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("v1")).unwrap(),
            plugin_id.clone(),
        )
        .unwrap();

    write_plugin_with_version(tmp.path(), "v2", "sample-plugin", Some("2.0.0"));
    store
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("v2")).unwrap(),
            plugin_id.clone(),
        )
        .unwrap();

    assert_eq!(
        store.active_plugin_version(&plugin_id),
        Some("2.0.0".to_string())
    );
    assert!(
        tmp.path()
            .join("plugins/cache/debug/sample-plugin/2.0.0")
            .is_dir()
    );
    assert!(
        !tmp.path()
            .join("plugins/cache/debug/sample-plugin/1.0.0")
            .exists()
    );
}

#[test]
fn old_plugin_version_would_stay_active_for_local_or_later_versions() {
    assert!(old_plugin_version_would_stay_active(
        DEFAULT_PLUGIN_VERSION,
        "1.0.0"
    ));
    assert!(old_plugin_version_would_stay_active(
        DEFAULT_PLUGIN_VERSION,
        "zeta"
    ));
    assert!(old_plugin_version_would_stay_active("10.0.0", "9.0.0"));
    assert!(!old_plugin_version_would_stay_active("1.0.0", "2.0.0"));
}

#[test]
fn plugin_root_rejects_path_separators_in_key_segments() {
    let err = PluginId::parse("../../etc@debug").unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid plugin name: dots must separate non-empty name segments in `../../etc@debug`"
    );

    let err = PluginId::parse("sample@../../etc").unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid marketplace name: only ASCII letters, digits, `_`, and `-` are allowed in `sample@../../etc`"
    );
}

#[test]
fn install_rejects_manifest_names_with_path_separators() {
    let tmp = tempdir().unwrap();
    write_plugin(tmp.path(), "source-dir", "../../etc");

    let err = PluginStore::new(tmp.path().to_path_buf())
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("source-dir")).unwrap(),
            PluginId::new("source-dir".to_string(), "debug".to_string()).unwrap(),
        )
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "invalid plugin name: dots must separate non-empty name segments"
    );
}

#[test]
fn install_rejects_marketplace_names_with_path_separators() {
    let err = PluginId::new("sample-plugin".to_string(), "../../etc".to_string()).unwrap_err();

    assert_eq!(
        err.to_string(),
        "invalid marketplace name: only ASCII letters, digits, `_`, and `-` are allowed"
    );
}

#[test]
fn install_rejects_manifest_names_that_do_not_match_marketplace_plugin_name() {
    let tmp = tempdir().unwrap();
    write_plugin(tmp.path(), "source-dir", "manifest-name");

    let err = PluginStore::new(tmp.path().to_path_buf())
        .install(
            AbsolutePathBuf::try_from(tmp.path().join("source-dir")).unwrap(),
            PluginId::new("different-name".to_string(), "debug".to_string()).unwrap(),
        )
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "plugin.json name `manifest-name` does not match marketplace plugin name `different-name`"
    );
}

#[test]
fn store_construction_uses_scoped_home_and_creates_no_directories() {
    use ra_coding::permissions::AbsolutePathBufGuard;
    let temp = tempdir().unwrap();
    AbsolutePathBufGuard::with_home_directory(temp.path(), || {
        let store = PluginStore::try_new(PathBuf::from("~/product")).unwrap();
        assert_eq!(api::home(&store).as_path(), temp.path().join("product"));
        assert_eq!(
            store.root().as_path(),
            temp.path().join("product/plugins/cache")
        );
        let id = PluginId::parse("demo@debug").unwrap();
        assert_eq!(
            store.plugin_root(&id, "~/outside").as_path(),
            temp.path().join("outside")
        );
        assert!(!temp.path().join("product").exists());
    });
}

#[test]
fn portable_source_version_hashes_unsafe_directory_names_without_changing_manifest_metadata() {
    use ra_coding::plugins::{
        discovery::AGENT_PLUGIN_SCHEMA_URI, manifest::load_plugin_manifest,
        store::plugin_version_for_source,
    };
    let temp = tempdir().unwrap();
    let source = temp.path().join("portable");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("plugin.json"), serde_json::json!({"$schema":AGENT_PLUGIN_SCHEMA_URI,"name":"portable","version":" release/2026 beta "}).to_string()).unwrap();
    let expected = "agent-plugins-afbf9486675af30c8db589fc";
    assert_eq!(plugin_version_for_source(&source).unwrap(), expected);
    let result = PluginStore::new(temp.path().join("home"))
        .install(
            AbsolutePathBuf::try_from(source).unwrap(),
            PluginId::parse("portable@debug").unwrap(),
        )
        .unwrap();
    assert_eq!(result.plugin_version, expected);
    assert_eq!(
        load_plugin_manifest(result.installed_path.as_path())
            .unwrap()
            .version
            .as_deref(),
        Some("release/2026 beta")
    );
}

#[test]
fn explicit_fallback_version_injects_only_into_staging() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("fallback");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload"), "keep").unwrap();
    let manifest = r#"{"name":"fallback","version":"3.0.0"}"#;
    assert_eq!(api::fallback_version(&source, manifest).unwrap(), "3.0.0");
    let store = PluginStore::new(temp.path().join("home"));
    let result = api::install_version_with_fallback(
        &store,
        AbsolutePathBuf::try_from(source.clone()).unwrap(),
        PluginId::parse("fallback@debug").unwrap(),
        "fixed-revision".into(),
        manifest,
    )
    .unwrap();
    assert_eq!(result.plugin_version, "fixed-revision");
    assert_eq!(
        fs::read_to_string(
            result
                .installed_path
                .as_path()
                .join(".codex-plugin/plugin.json")
        )
        .unwrap(),
        manifest
    );
    assert!(!source.join(".codex-plugin").exists());
    assert_eq!(fs::read_to_string(source.join("payload")).unwrap(), "keep");
}

#[test]
fn raw_version_errors_remain_distinct_from_full_manifest_validation() {
    use ra_coding::plugins::store::plugin_version_for_source;
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join(".codex-plugin")).unwrap();
    for (version, expected) in [
        (
            serde_json::json!(42),
            "invalid plugin version in plugin.json: expected string",
        ),
        (
            serde_json::json!(" "),
            "invalid plugin version in plugin.json: must not be blank",
        ),
    ] {
        fs::write(
            source.join(".codex-plugin/plugin.json"),
            serde_json::json!({"name":"source","version":version}).to_string(),
        )
        .unwrap();
        assert_eq!(
            plugin_version_for_source(&source).unwrap_err().to_string(),
            expected
        );
    }
    fs::write(
        source.join(".codex-plugin/plugin.json"),
        r#"{"name":42,"version":"1.2.3"}"#,
    )
    .unwrap();
    assert_eq!(plugin_version_for_source(&source).unwrap(), "1.2.3");
    assert!(
        PluginStore::new(temp.path().join("home"))
            .install(
                AbsolutePathBuf::try_from(source).unwrap(),
                PluginId::parse("source@debug").unwrap()
            )
            .is_err()
    );
}

#[test]
fn mcp_data_root_preserves_format_specific_persistent_layouts() {
    let temp = tempdir().unwrap();
    let store = PluginStore::new(temp.path().to_path_buf());
    let id = PluginId::parse("sample@debug").unwrap();
    assert_eq!(
        api::mcp_data_root(&store, &id, false),
        store.plugin_data_root(&id)
    );
    assert_eq!(
        api::mcp_data_root(&store, &id, true),
        api::agent_data_root(&store, &id)
    );
    assert_ne!(
        api::mcp_data_root(&store, &id, false),
        api::mcp_data_root(&store, &id, true)
    );
    assert!(!temp.path().join("plugins").exists());
}

#[test]
fn active_installation_retains_local_identity_and_root_without_account_metadata() {
    let temp = tempdir().unwrap();
    let store = PluginStore::new(temp.path().join("home"));
    let id = PluginId::parse("sample@debug").unwrap();
    assert!(api::active_installation(&store, &id).is_none());
    write_plugin(temp.path(), "source", "sample");
    let result = store
        .install(
            AbsolutePathBuf::try_from(temp.path().join("source")).unwrap(),
            id.clone(),
        )
        .unwrap();
    assert_eq!(
        api::active_installation(&store, &id),
        Some((id.clone(), result.installed_path))
    );
    store.uninstall(&id).unwrap();
    assert!(api::active_installation(&store, &id).is_none());
}
