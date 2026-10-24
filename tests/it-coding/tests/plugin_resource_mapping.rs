// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Resource mapping preserves metadata and inline declarations across representations.

use ra_coding::plugins::{
    hook_config::HooksFile,
    model::{
        PluginManifest, PluginManifestHooks, PluginManifestInterface, PluginManifestMcpServers,
        PluginManifestPaths,
    },
};

fn manifest() -> PluginManifest<&'static str> {
    PluginManifest {
        name: "package".into(),
        version: Some("1.2.3".into()),
        description: Some("description".into()),
        keywords: vec!["keyword".into()],
        paths: PluginManifestPaths {
            skills: vec!["skill-one", "skill-two"],
            onboarding_skill: Some("onboarding"),
            mcp_servers: Some(PluginManifestMcpServers::Path("mcp")),
            apps: Some("apps"),
            hooks: Some(PluginManifestHooks::Paths(vec!["hook-one", "hook-two"])),
        },
        interface: Some(PluginManifestInterface {
            display_name: Some("  Display  ".into()),
            default_prompt: Some(vec!["prompt".into()]),
            composer_icon: Some("icon"),
            logo: Some("logo"),
            logo_dark: Some("dark"),
            screenshots: vec!["shot-one", "shot-two"],
            ..Default::default()
        }),
    }
}

#[test]
fn mapping_visits_all_path_bearing_resources_in_upstream_order() {
    let original = manifest();
    let mut seen = Vec::new();
    let mapped = original
        .clone()
        .try_map_resources(|resource| {
            seen.push(resource);
            Ok::<_, ()>(format!("owner:{resource}"))
        })
        .unwrap();
    assert_eq!(
        seen,
        [
            "hook-one",
            "hook-two",
            "mcp",
            "icon",
            "logo",
            "dark",
            "shot-one",
            "shot-two",
            "skill-one",
            "skill-two",
            "onboarding",
            "apps"
        ]
    );
    assert_eq!(mapped.paths.skills, ["owner:skill-one", "owner:skill-two"]);
    assert_eq!(
        mapped.paths.onboarding_skill.as_deref(),
        Some("owner:onboarding")
    );
    assert_eq!(mapped.paths.apps.as_deref(), Some("owner:apps"));
    assert_eq!(
        mapped.paths.mcp_servers,
        Some(PluginManifestMcpServers::Path("owner:mcp".into()))
    );
    assert_eq!(
        mapped.paths.hooks,
        Some(PluginManifestHooks::Paths(vec![
            "owner:hook-one".into(),
            "owner:hook-two".into()
        ]))
    );
    let interface = mapped.interface.as_ref().unwrap();
    assert_eq!(interface.composer_icon.as_deref(), Some("owner:icon"));
    assert_eq!(interface.logo.as_deref(), Some("owner:logo"));
    assert_eq!(interface.logo_dark.as_deref(), Some("owner:dark"));
    assert_eq!(interface.screenshots, ["owner:shot-one", "owner:shot-two"]);
    assert_eq!(
        interface.default_prompt,
        original.interface.unwrap().default_prompt
    );
    assert_eq!(mapped.version, original.version);
    assert_eq!(mapped.description, original.description);
    assert_eq!(mapped.keywords, original.keywords);
    assert_eq!(mapped.display_name(), "Display");
}

#[test]
fn mapping_preserves_inline_declarations_and_stops_at_the_first_error() {
    let mut original = manifest();
    let inline = vec![HooksFile::default()];
    original.paths.hooks = Some(PluginManifestHooks::Inline(inline.clone()));
    original.paths.mcp_servers = Some(PluginManifestMcpServers::Object("{}".into()));
    let mapped = original
        .clone()
        .try_map_resources(|resource| Ok::<_, ()>(resource.len()))
        .unwrap();
    assert_eq!(
        mapped.paths.hooks,
        Some(PluginManifestHooks::Inline(inline))
    );
    assert_eq!(
        mapped.paths.mcp_servers,
        Some(PluginManifestMcpServers::Object("{}".into()))
    );
    let mut seen = Vec::new();
    let error = original
        .try_map_resources(|resource| {
            seen.push(resource);
            if resource == "logo" {
                Err("unavailable resource")
            } else {
                Ok(resource)
            }
        })
        .unwrap_err();
    assert_eq!(error, "unavailable resource");
    assert_eq!(seen, ["icon", "logo"]);
}

#[test]
fn display_name_falls_back_to_the_untrimmed_manifest_name() {
    let mut manifest = manifest();
    manifest.name = " raw name ".into();
    manifest.interface.as_mut().unwrap().display_name = Some("  ".into());
    assert_eq!(manifest.display_name(), " raw name ");
    manifest.interface = None;
    assert_eq!(manifest.display_name(), " raw name ");
}
