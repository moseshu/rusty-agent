// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! URI parsing follows the owning root convention without host filesystem access.

use ra_coding::plugins::{
    manifest::parse_plugin_manifest_uri,
    model::{
        PluginManifest as GenericPluginManifest, PluginManifestHooks, PluginManifestInterface,
        PluginManifestMcpServers, PluginManifestPaths,
    },
};
use ra_paths::PathUri;

#[test]
fn uri_manifest_uses_the_root_path_convention() {
    let windows_root =
        PathUri::parse("file:///C:/plugins/demo-plugin").expect("Windows plugin root URI");
    let posix_root = PathUri::parse("file:///plugins/demo-plugin").expect("POSIX plugin root URI");
    let composer_icon = r"./assets\..\icon.svg";

    assert_eq!(parse_uri_composer_icon(&windows_root, composer_icon), None);
    assert_eq!(
        parse_uri_composer_icon(&posix_root, composer_icon),
        Some(
            posix_root
                .join(r"assets\..\icon.svg")
                .expect("composer icon URI")
        )
    );
}

fn parse_uri_composer_icon(plugin_root: &PathUri, composer_icon: &str) -> Option<PathUri> {
    let manifest_path = plugin_root
        .join(".codex-plugin/plugin.json")
        .expect("manifest URI");
    let composer_icon_json = serde_json::to_string(composer_icon).expect("serialize composer icon");
    let contents = format!(
        r#"{{
  "name": "demo-plugin",
  "interface": {{
"displayName": "Demo Plugin",
"composerIcon": {composer_icon_json}
  }}
}}"#
    );
    parse_plugin_manifest_uri(plugin_root, &manifest_path, &contents)
        .expect("URI manifest")
        .interface
        .and_then(|interface| interface.composer_icon)
}

#[test]
fn uri_manifest_resolves_resources_below_foreign_root() {
    let plugin_root = PathUri::parse("file:///C:/plugins/demo-plugin").expect("plugin root URI");
    let manifest_path = plugin_root
        .join(".codex-plugin/plugin.json")
        .expect("manifest URI");
    let manifest = parse_plugin_manifest_uri(
        &plugin_root,
        &manifest_path,
        r#"{
  "name": "demo-plugin",
  "skills": "./skills",
  "mcpServers": "./.mcp.json",
  "apps": "./apps",
  "hooks": "./hooks.json",
  "interface": {
"displayName": "Demo Plugin",
"composerIcon": "./assets/icon.svg"
  }
}"#,
    )
    .expect("URI manifest");

    assert_eq!(
        manifest,
        GenericPluginManifest {
            name: "demo-plugin".to_string(),
            version: None,
            description: None,
            keywords: Vec::new(),
            paths: PluginManifestPaths {
                onboarding_skill: None,
                skills: vec![plugin_root.join("skills").expect("skills URI")],
                mcp_servers: Some(PluginManifestMcpServers::Path(
                    plugin_root.join(".mcp.json").expect("MCP URI"),
                )),
                apps: Some(plugin_root.join("apps").expect("apps URI")),
                hooks: Some(PluginManifestHooks::Paths(vec![
                    plugin_root.join("hooks.json").expect("hooks URI"),
                ])),
            },
            interface: Some(PluginManifestInterface {
                display_name: Some("Demo Plugin".to_string()),
                composer_icon: Some(
                    plugin_root
                        .join("assets/icon.svg")
                        .expect("composer icon URI"),
                ),
                ..PluginManifestInterface::default()
            }),
        }
    );
}

#[test]
fn host_and_uri_entry_points_share_the_same_parser() {
    let directory = tempfile::tempdir().expect("temporary plugin");
    let root = directory.path().join("demo-plugin");
    std::fs::create_dir_all(root.join(".codex-plugin")).expect("manifest directory");
    let text = r#"{
        "name":"demo-plugin","version":" 1.2.3 ",
        "skills":["./skills","./../escape"],
        "interface":{"displayName":"Demo Plugin","composerIcon":"./assets/icon.svg"}
    }"#;
    let manifest_path = root.join(".codex-plugin/plugin.json");
    std::fs::write(&manifest_path, text).expect("manifest contents");
    let uri = parse_plugin_manifest_uri(
        &PathUri::from_host_native_path(&root).expect("root URI"),
        &PathUri::from_host_native_path(&manifest_path).expect("manifest URI"),
        text,
    )
    .expect("URI manifest");
    let native = uri
        .try_map_resources(|path| path.to_abs_path())
        .expect("native resources");
    assert_eq!(
        ra_coding::plugins::manifest::load_plugin_manifest(&root),
        Some(native)
    );
}

#[test]
fn foreign_portable_manifest_keeps_fixed_components_and_inline_extension() {
    let root = PathUri::parse("file://executor-share/plugins/demo").expect("foreign root");
    let manifest = parse_plugin_manifest_uri(
        &root,
        &root.join("plugin.json").expect("manifest URI"),
        &serde_json::json!({
            "$schema":ra_coding::plugins::discovery::AGENT_PLUGIN_SCHEMA_URI,
            "name":"demo",
            "skills":["./ignored"],
            "mcpServers":"./ignored.json",
            "extensions":{"com.openai":{
                "skills":["./ignored-extension"],
                "hooks":{"hooks":{"Stop":[{"hooks":[{"type":"mcp_tool","server":"node_repl","tool":"turn_ended"}]}]}},
                "onboardingSkill":"skills/setup/SKILL.md",
                "interface":{"composerIcon":"./assets/icon.svg"}
            }}
        }).to_string(),
    ).expect("portable URI manifest");
    assert_eq!(
        manifest.paths.skills,
        vec![root.join("skills").expect("skills URI")]
    );
    assert_eq!(
        manifest.paths.mcp_servers,
        Some(PluginManifestMcpServers::Path(
            root.join("mcp.json").expect("MCP URI")
        ))
    );
    assert_eq!(
        manifest.paths.onboarding_skill,
        Some(root.join("skills/setup/SKILL.md").expect("onboarding URI"))
    );
    assert!(matches!(
        manifest.paths.hooks,
        Some(PluginManifestHooks::Inline(_))
    ));
    assert_eq!(
        manifest.interface.expect("interface").composer_icon,
        Some(root.join("assets/icon.svg").expect("icon URI"))
    );
}
