// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Environment ownership is preserved across every manifest resource.

use ra_coding::plugins::PluginResourceLocator;
use ra_coding::plugins::ResolvedPlugin;
use ra_coding::plugins::ResolvedPluginError;
use ra_coding::plugins::model::PluginManifest;
use ra_coding::plugins::model::PluginManifestHooks;
use ra_coding::plugins::model::PluginManifestInterface;
use ra_coding::plugins::model::PluginManifestMcpServers;
use ra_coding::plugins::model::PluginManifestPaths;
use ra_paths::AbsolutePathBuf;
use ra_paths::PathUri;

fn absolute(path: impl AsRef<std::path::Path>) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path_checked(path.as_ref()).expect("absolute test path")
}

fn path_uri(path: &AbsolutePathBuf) -> PathUri {
    PathUri::from_abs_path(path)
}

fn resource(environment_id: &str, path: &AbsolutePathBuf) -> PluginResourceLocator {
    PluginResourceLocator::Environment {
        environment_id: environment_id.to_string(),
        path: path_uri(path),
    }
}

#[test]
fn environment_descriptor_binds_every_manifest_resource() {
    let root = absolute(std::env::current_dir().expect("cwd").join("plugin-root"));
    let root_uri = path_uri(&root);
    let manifest_path = root.join(".codex-plugin/plugin.json");
    let skills = root.join("skills");
    let mcp_servers = root.join(".mcp.json");
    let apps = root.join(".app.json");
    let hooks = root.join("hooks/hooks.json");
    let composer_icon = root.join("assets/composer.svg");
    let logo = root.join("assets/logo.svg");
    let screenshot = root.join("assets/screenshot.png");
    let manifest = PluginManifest {
        name: "demo".to_string(),
        version: None,
        description: None,
        keywords: Vec::new(),
        paths: PluginManifestPaths {
            onboarding_skill: None,
            skills: vec![path_uri(&skills)],
            mcp_servers: Some(PluginManifestMcpServers::Path(path_uri(&mcp_servers))),
            apps: Some(path_uri(&apps)),
            hooks: Some(PluginManifestHooks::Paths(vec![path_uri(&hooks)])),
        },
        interface: Some(PluginManifestInterface {
            composer_icon: Some(path_uri(&composer_icon)),
            logo: Some(path_uri(&logo)),
            screenshots: vec![path_uri(&screenshot)],
            ..PluginManifestInterface::default()
        }),
    };

    let plugin = ResolvedPlugin::from_environment(
        "selected-demo".to_string(),
        "executor-1".to_string(),
        root_uri,
        path_uri(&manifest_path),
        manifest,
    )
    .expect("valid descriptor");

    assert_eq!(
        plugin.manifest_path(),
        &resource("executor-1", &manifest_path)
    );
    assert_eq!(
        plugin.manifest(),
        &PluginManifest {
            name: "demo".to_string(),
            version: None,
            description: None,
            keywords: Vec::new(),
            paths: PluginManifestPaths {
                onboarding_skill: None,
                skills: vec![resource("executor-1", &skills)],
                mcp_servers: Some(PluginManifestMcpServers::Path(resource(
                    "executor-1",
                    &mcp_servers,
                ))),
                apps: Some(resource("executor-1", &apps)),
                hooks: Some(PluginManifestHooks::Paths(vec![resource(
                    "executor-1",
                    &hooks
                )])),
            },
            interface: Some(PluginManifestInterface {
                composer_icon: Some(resource("executor-1", &composer_icon)),
                logo: Some(resource("executor-1", &logo)),
                screenshots: vec![resource("executor-1", &screenshot)],
                ..PluginManifestInterface::default()
            }),
        }
    );
}

#[test]
fn environment_descriptor_rejects_resources_outside_package_root() {
    let cwd = std::env::current_dir().expect("cwd");
    let root = absolute(cwd.join("plugin-root"));
    let outside = absolute(cwd.join("outside/.mcp.json"));
    let manifest = PluginManifest {
        name: "demo".to_string(),
        version: None,
        description: None,
        keywords: Vec::new(),
        paths: PluginManifestPaths {
            onboarding_skill: None,
            skills: Vec::new(),
            mcp_servers: Some(PluginManifestMcpServers::Path(path_uri(&outside))),
            apps: None,
            hooks: None,
        },
        interface: None,
    };

    let err = ResolvedPlugin::from_environment(
        "selected-demo".to_string(),
        "executor-1".to_string(),
        path_uri(&root),
        path_uri(&root.join(".codex-plugin/plugin.json")),
        manifest,
    )
    .expect_err("outside resource should fail");

    assert_eq!(
        err,
        ResolvedPluginError::ResourceOutsideRoot {
            root: path_uri(&root),
            path: path_uri(&outside),
        }
    );
}

#[test]
fn foreign_resources_keep_authority_and_reject_nearby_roots() {
    let root = PathUri::parse("file://executor-a/plugins/demo").expect("root URI");
    let manifest_path = root
        .join(".codex-plugin/plugin.json")
        .expect("manifest URI");
    let text = r#"{"name":"demo","skills":["./skills"],"extensions":{"com.openai":{"onboardingSkill":"skills/setup/SKILL.md"}},"interface":{"logoDark":"./assets/dark.svg"}}"#;
    let manifest =
        ra_coding::plugins::manifest::parse_plugin_manifest_uri(&root, &manifest_path, text)
            .expect("URI manifest");
    let plugin = ResolvedPlugin::from_environment(
        "opaque-selection-id".into(),
        "executor-42".into(),
        root.clone(),
        manifest_path.clone(),
        manifest.clone(),
    )
    .expect("bound descriptor");
    assert_eq!(plugin.selected_root_id(), "opaque-selection-id");
    assert_eq!(
        plugin.location(),
        &ra_coding::plugins::ResolvedPluginLocation::Environment {
            environment_id: "executor-42".into(),
            root: root.clone()
        }
    );
    assert_eq!(
        plugin.manifest().paths.onboarding_skill,
        Some(PluginResourceLocator::Environment {
            environment_id: "executor-42".into(),
            path: root.join("skills/setup/SKILL.md").expect("onboarding URI")
        })
    );
    assert_eq!(
        plugin
            .manifest()
            .interface
            .as_ref()
            .expect("interface")
            .logo_dark,
        Some(PluginResourceLocator::Environment {
            environment_id: "executor-42".into(),
            path: root.join("assets/dark.svg").expect("logo URI")
        })
    );
    for outside in [
        "file://executor-b/plugins/demo/skills",
        "file://executor-a/plugins/demo-sibling/skills",
    ] {
        let mut invalid = manifest.clone();
        let path = PathUri::parse(outside).expect("foreign URI");
        invalid.paths.skills = vec![path.clone()];
        assert_eq!(
            ResolvedPlugin::from_environment(
                "opaque-selection-id".into(),
                "executor-42".into(),
                root.clone(),
                manifest_path.clone(),
                invalid
            )
            .expect_err("outside resource"),
            ResolvedPluginError::ResourceOutsideRoot {
                root: root.clone(),
                path
            }
        );
    }
    let outside_manifest = root
        .parent()
        .expect("parent URI")
        .join("other.json")
        .expect("outside manifest URI");
    assert_eq!(
        ResolvedPlugin::from_environment(
            "opaque-selection-id".into(),
            "executor-42".into(),
            root.clone(),
            outside_manifest.clone(),
            manifest
        )
        .expect_err("outside manifest"),
        ResolvedPluginError::ResourceOutsideRoot {
            root,
            path: outside_manifest
        }
    );
}
