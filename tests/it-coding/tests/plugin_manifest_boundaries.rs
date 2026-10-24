// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Public host loading preserves format selection and field-level parsing boundaries.

use ra_coding::plugins::{
    discovery::AGENT_PLUGIN_SCHEMA_URI,
    manifest::{
        PluginManifest, PluginManifestHooks, PluginManifestMcpServers, is_agent_plugin_manifest,
        load_plugin_manifest,
    },
};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn write(root: &Path, relative: &str, value: &Value) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn legacy(root: &Path, value: &Value) -> PluginManifest {
    write(root, ".codex-plugin/plugin.json", value);
    load_plugin_manifest(root).unwrap()
}

#[test]
fn selected_invalid_manifest_does_not_fall_back_but_unrelated_root_does() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        ".codex-plugin/plugin.json",
        &json!({"name":"legacy"}),
    );
    fs::write(temp.path().join("plugin.json"), "{broken").unwrap();
    assert_eq!(load_plugin_manifest(temp.path()).unwrap().name, "legacy");
    write(
        temp.path(),
        "plugin.json",
        &json!({"$schema":AGENT_PLUGIN_SCHEMA_URI,"name":42}),
    );
    assert!(load_plugin_manifest(temp.path()).is_none());
    assert!(!is_agent_plugin_manifest(temp.path()));
    write(
        temp.path(),
        "plugin.json",
        &json!({"$schema":"https://agent-plugins.org/schemas/99/plugin.schema.json","name":"portable"}),
    );
    assert!(load_plugin_manifest(temp.path()).is_none());
    fs::remove_file(temp.path().join("plugin.json")).unwrap();
    fs::write(temp.path().join(".codex-plugin/plugin.json"), "{broken").unwrap();
    write(
        temp.path(),
        ".claude-plugin/plugin.json",
        &json!({"name":"alternate"}),
    );
    assert!(load_plugin_manifest(temp.path()).is_none());
}

#[test]
fn legacy_name_fallback_and_optional_values_keep_upstream_meanings() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = legacy(
        temp.path(),
        &json!({"name":"  ","version":"  ","description":"  raw text  ","unknown":true}),
    );
    assert_eq!(
        manifest.name,
        temp.path().file_name().unwrap().to_string_lossy()
    );
    assert_eq!(manifest.version, None);
    assert_eq!(manifest.description.as_deref(), Some("  raw text  "));
    assert_eq!(
        legacy(temp.path(), &json!({"name":" name "})).name,
        " name "
    );
    assert_eq!(
        legacy(temp.path(), &json!({"interface":{}})).interface,
        None
    );
    assert_eq!(
        legacy(temp.path(), &json!({"interface":{"displayName":""}}))
            .interface
            .unwrap()
            .display_name
            .as_deref(),
        Some("")
    );
    write(
        temp.path(),
        ".codex-plugin/plugin.json",
        &json!({"name":42}),
    );
    assert!(load_plugin_manifest(temp.path()).is_none());
}

#[test]
fn manifest_paths_reject_parent_segments_before_normalization() {
    let temp = tempfile::tempdir().unwrap();
    for path in [
        "relative",
        "../escape",
        "./",
        "./../escape",
        "./inside/../inside",
        ".//absolute",
        "./bad\0name",
    ] {
        let manifest = legacy(
            temp.path(),
            &json!({"skills":[path],"apps":path,"hooks":path,"mcpServers":path,"interface":{"logo":path}}),
        );
        assert!(manifest.paths.skills.is_empty(), "{path}");
        assert!(manifest.paths.apps.is_none(), "{path}");
        assert!(manifest.paths.hooks.is_none(), "{path}");
        assert!(manifest.paths.mcp_servers.is_none(), "{path}");
        assert!(manifest.interface.is_none(), "{path}");
    }
    let manifest = legacy(
        temp.path(),
        &json!({"skills":["./space # %2e%2e/child","./nested/./skill"]}),
    );
    assert_eq!(
        manifest.paths.skills[0].as_path(),
        temp.path().join("space # %2e%2e/child")
    );
    assert_eq!(
        manifest.paths.skills[1].as_path(),
        temp.path().join("nested/skill")
    );
}

#[cfg(not(windows))]
#[test]
fn posix_manifest_paths_do_not_apply_windows_parent_rules() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = legacy(
        temp.path(),
        &json!({"skills":["./..\\literal","./C:/literal"]}),
    );
    assert_eq!(manifest.paths.skills.len(), 2);
    assert_eq!(
        manifest.paths.skills[0].as_path(),
        temp.path().join("..\\literal")
    );
}

#[cfg(windows)]
#[test]
fn windows_manifest_paths_reject_drive_roots_and_backslash_parents() {
    let temp = tempfile::tempdir().unwrap();
    for path in ["./..\\escape", "./C:/escape", "./\\escape"] {
        assert!(
            legacy(temp.path(), &json!({"skills":[path]}))
                .paths
                .skills
                .is_empty()
        );
    }
}

#[test]
fn malformed_optional_component_shapes_are_ignored_without_losing_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = legacy(
        temp.path(),
        &json!({"name":"kept","skills":["./ok",42],"hooks":{"hooks":{"Stop":[{"hooks":[{"type":"unknown"}]}]}},"mcpServers":["wrong"]}),
    );
    assert_eq!(manifest.name, "kept");
    assert!(manifest.paths.skills.is_empty());
    assert!(manifest.paths.hooks.is_none());
    assert!(manifest.paths.mcp_servers.is_none());
    let manifest = legacy(temp.path(), &json!({"mcpServers":{"z":false,"a":null}}));
    assert_eq!(
        manifest.paths.mcp_servers,
        Some(PluginManifestMcpServers::Object(
            r#"{"a":null,"z":false}"#.into()
        ))
    );
}

#[test]
fn hooks_accept_paths_and_typed_inline_lists_without_executing_them() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = legacy(temp.path(), &json!({"hooks":["./one.json","./two.json"]}));
    let Some(PluginManifestHooks::Paths(paths)) = manifest.paths.hooks else {
        panic!("path declarations")
    };
    assert_eq!(paths.len(), 2);
    let declaration = json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"this-command-must-never-run"}]}]}});
    let manifest = legacy(
        temp.path(),
        &json!({"hooks":[declaration, {"description":"empty"}]}),
    );
    let Some(PluginManifestHooks::Inline(files)) = manifest.paths.hooks else {
        panic!("typed declarations")
    };
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].hooks.handler_count(), 1);
    assert!(files[1].hooks.is_empty());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    let manifest = legacy(
        temp.path(),
        &json!({"hooks":{"hooks":{"Stop":[{"hooks":[{"type":"mcp_tool","server":"s","tool":"t","input":{"bad":null}}]}]}}}),
    );
    assert!(manifest.paths.hooks.is_none());
}

#[test]
fn portable_extension_owns_only_its_permitted_fields_and_onboarding_resource() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "plugin.json",
        &json!({"$schema":AGENT_PLUGIN_SCHEMA_URI,"name":"portable","skills":[],"mcpServers":null,"extensions":{"com.openai":{"name":"ignored","skills":["./ignored"],"mcpServers":{},"onboardingSkill":"skills/start/SKILL.md","apps":"./apps.json","hooks":{"hooks":{"Stop":[]}},"interface":{"displayName":"Shown"}}}}),
    );
    write(
        temp.path(),
        ".codex-plugin/plugin.json",
        &json!({"interface":{"displayName":"Lower priority"}}),
    );
    let manifest = load_plugin_manifest(temp.path()).unwrap();
    assert!(is_agent_plugin_manifest(temp.path()));
    assert_eq!(manifest.name, "portable");
    assert_eq!(
        manifest.paths.skills[0].as_path(),
        temp.path().join("skills")
    );
    assert_eq!(
        manifest.paths.onboarding_skill.unwrap().as_path(),
        temp.path().join("skills/start/SKILL.md")
    );
    assert_eq!(
        manifest.paths.apps.unwrap().as_path(),
        temp.path().join("apps.json")
    );
    assert!(matches!(
        manifest.paths.hooks,
        Some(PluginManifestHooks::Inline(_))
    ));
    assert_eq!(
        manifest.interface.unwrap().display_name.as_deref(),
        Some("Shown")
    );
}

#[test]
fn invalid_selected_overlay_fails_the_portable_load_and_inline_extension_bypasses_it() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "plugin.json",
        &json!({"$schema":AGENT_PLUGIN_SCHEMA_URI,"name":"portable"}),
    );
    write(
        temp.path(),
        ".codex-plugin/plugin.json",
        &json!({"interface":42}),
    );
    assert!(load_plugin_manifest(temp.path()).is_none());
    write(
        temp.path(),
        "plugin.json",
        &json!({"$schema":AGENT_PLUGIN_SCHEMA_URI,"name":"portable","extensions":{"com.openai":{}}}),
    );
    assert!(load_plugin_manifest(temp.path()).is_some());
}

#[test]
fn relative_host_roots_are_not_silently_resolved() {
    let cwd = std::env::current_dir().unwrap();
    let temp = tempfile::tempdir_in(&cwd).unwrap();
    write(
        temp.path(),
        ".codex-plugin/plugin.json",
        &json!({"name":"valid"}),
    );
    let relative = temp.path().strip_prefix(&cwd).unwrap();
    assert!(load_plugin_manifest(temp.path()).is_some());
    assert!(load_plugin_manifest(relative).is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn host_resource_paths_retain_non_utf8_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'p', 0xff]));
    let manifest = legacy(&root, &json!({"skills":["./child"]}));
    assert_eq!(manifest.paths.skills[0].as_path(), root.join("child"));
    assert_eq!(manifest.name, "p%FF");
}

#[cfg(unix)]
#[test]
fn component_paths_remain_lexical_and_do_not_require_existing_targets() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), temp.path().join("linked")).unwrap();
    let manifest = legacy(
        temp.path(),
        &json!({"skills":["./linked/skill"],"apps":"./missing.json"}),
    );
    assert_eq!(
        manifest.paths.skills[0].as_path(),
        temp.path().join("linked/skill")
    );
    assert_eq!(
        manifest.paths.apps.unwrap().as_path(),
        temp.path().join("missing.json")
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn basename_decodes_filename_text_without_treating_plus_as_query_whitespace() {
    let temp = tempfile::tempdir().unwrap();
    let name = "package +%# snow-\u{2603}";
    let root = temp.path().join(name);
    let manifest = legacy(&root, &json!({"name":"","skills":["./child +%#"]}));
    assert_eq!(manifest.name, name);
    assert_eq!(manifest.paths.skills[0].as_path(), root.join("child +%#"));
}

#[test]
fn resource_resolution_uses_the_scoped_home_without_accepting_relative_roots() {
    use ra_coding::{
        permissions::AbsolutePathBufGuard, plugins::manifest::test_api::resolve_resource,
    };
    let outer_home = tempfile::tempdir().unwrap();
    let inner_home = tempfile::tempdir().unwrap();
    let original_home = AbsolutePathBufGuard::home_directory();
    AbsolutePathBufGuard::with_home_directory(outer_home.path(), || {
        let expected = outer_home.path().join("package/skills");
        assert_eq!(
            resolve_resource(Path::new("~/package"), "./skills")
                .unwrap()
                .as_path(),
            expected
        );
        AbsolutePathBufGuard::with_home_directory(inner_home.path(), || {
            assert_eq!(
                resolve_resource(Path::new("~/package"), "./skills")
                    .unwrap()
                    .as_path(),
                inner_home.path().join("package/skills")
            );
        });
        assert_eq!(
            resolve_resource(Path::new("~/package"), "./skills")
                .unwrap()
                .as_path(),
            expected
        );
        let _base = AbsolutePathBufGuard::new(inner_home.path());
        assert!(resolve_resource(Path::new("relative"), "./skills").is_none());
        #[cfg(windows)]
        assert_eq!(
            resolve_resource(Path::new(r"~\package"), "./skills")
                .unwrap()
                .as_path(),
            expected
        );
    });
    assert_eq!(AbsolutePathBufGuard::home_directory(), original_home);
}

#[cfg(not(windows))]
#[test]
fn resolved_drive_like_segments_follow_the_upstream_path_convention_check() {
    use ra_coding::plugins::manifest::test_api::resolve_resource;
    for path in ["./c:", "./Z:", "./C:/x"] {
        assert!(resolve_resource(Path::new("/"), path).is_none(), "{path}");
        assert_eq!(
            resolve_resource(Path::new("/tmp/package"), path)
                .unwrap()
                .as_path(),
            Path::new("/tmp/package").join(path.strip_prefix("./").unwrap())
        );
    }
    for path in ["./c%3A/x", "./a:b", "./C:x", "./a/./c:"] {
        assert!(resolve_resource(Path::new("/"), path).is_some(), "{path}");
    }
}

async fn assert_manifest_and_skill_namespace_fallback(
    workspace: &Path,
    package: &Path,
    expected: &str,
) {
    let manifest = legacy(package, &json!({"name":"  "}));
    assert_eq!(manifest.name, expected);
    fs::create_dir_all(package.join("review")).unwrap();
    fs::write(
        package.join("review/SKILL.md"),
        "---\nname: review\ndescription: Review changes.\n---\n",
    )
    .unwrap();
    let catalog = ra_coding::skills::SkillDiscoveryConfig::new()
        .discover(workspace)
        .await
        .unwrap();
    assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(catalog.entries[0].name, format!("{expected}:review"));
}

#[tokio::test]
async fn empty_manifest_name_and_skill_namespace_share_the_directory_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp
        .path()
        .join(".rusty-agent/skills/package +%# snow-\u{2603}");
    assert_manifest_and_skill_namespace_fallback(
        temp.path(),
        &package,
        "package +%# snow-\u{2603}",
    )
    .await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_utf8_manifest_and_skill_namespace_use_the_same_encoded_fallback() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let package = temp
        .path()
        .join(".rusty-agent/skills")
        .join(std::ffi::OsString::from_vec(vec![b'p', 0xff]));
    assert_manifest_and_skill_namespace_fallback(temp.path(), &package, "p%FF").await;
}
