// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Consumer boundaries for catalog policies, metadata-only remote entries, and cache ownership.

use ra_coding::plugins::{
    AbsolutePathBuf,
    marketplace::{
        MarketplaceError, MarketplacePluginAuthPolicy, MarketplacePluginInstallPolicy,
        MarketplacePluginSource, find_installable_marketplace_plugin, find_marketplace_plugin,
        list_marketplaces_with_home, load_marketplace, test_api::list_with_store_cache,
    },
    marketplace_add::{MarketplaceAddError, test_api as source_api},
    store::PluginStore,
};
use ra_protocol::server::protocol::Product;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs,
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
};

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn catalog(root: &Path, plugins: Value) -> AbsolutePathBuf {
    let path = root.join(".agents/plugins/marketplace.json");
    write_json(&path, &json!({"name":"test-market", "plugins":plugins}));
    AbsolutePathBuf::try_from(path).unwrap()
}

#[test]
fn installation_policy_filters_selection_without_hiding_catalog_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog(
        dir.path(),
        json!([
            {"name":"blocked", "source":"./missing", "policy":{"installation":"NOT_AVAILABLE"}},
            {"name":"defaulted", "source":"./missing", "policy":{"installation":"INSTALLED_BY_DEFAULT", "authentication":"ON_USE"}}
        ]),
    );
    let listed = load_marketplace(&path).unwrap();
    assert_eq!(listed.plugins.len(), 2);
    assert_eq!(
        listed.plugins[0].policy.installation,
        MarketplacePluginInstallPolicy::NotAvailable
    );
    assert!(find_marketplace_plugin(&path, "blocked").is_ok());
    assert!(matches!(
        find_installable_marketplace_plugin(&path, "blocked", None),
        Err(MarketplaceError::PluginNotAvailable { .. })
    ));
    let selected = find_installable_marketplace_plugin(&path, "defaulted", None).unwrap();
    assert_eq!(
        selected.policy.installation,
        MarketplacePluginInstallPolicy::InstalledByDefault
    );
    assert_eq!(
        selected.policy.authentication,
        MarketplacePluginAuthPolicy::OnUse
    );
    assert!(!dir.path().join("missing").exists());
    assert!(!dir.path().join("plugins").exists());
}

#[test]
fn invalid_source_duplicates_are_skipped_but_policy_does_not_select_later_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog(
        dir.path(),
        json!([
            {"name":"demo", "source":{"source":"unknown"}},
            {"name":"demo", "source":"../escape"},
            {"name":"demo", "source":"./first", "policy":{"installation":"NOT_AVAILABLE"}},
            {"name":"demo", "source":"./second"}
        ]),
    );
    let selected = find_marketplace_plugin(&path, "demo").unwrap();
    assert_eq!(
        selected.source,
        MarketplacePluginSource::Local {
            path: AbsolutePathBuf::try_from(dir.path().join("first")).unwrap(),
        }
    );
    assert!(matches!(
        find_installable_marketplace_plugin(&path, "demo", None),
        Err(MarketplaceError::PluginNotAvailable { .. })
    ));
    assert_eq!(load_marketplace(&path).unwrap().plugins.len(), 2);
}

#[test]
fn malformed_typed_policy_invalidates_the_catalog_instead_of_skipping_one_entry() {
    let dir = tempfile::tempdir().unwrap();
    for policy in [
        json!({"installation":"available"}),
        json!({"authentication":"later"}),
        json!({"products":["unknown-product"]}),
        json!({"products":"codex"}),
    ] {
        let path = catalog(
            dir.path(),
            json!([
                {"name":"valid", "source":"./valid"},
                {"name":"bad", "source":"./bad", "policy":policy}
            ]),
        );
        assert!(matches!(
            load_marketplace(&path),
            Err(MarketplaceError::InvalidMarketplaceFile { .. })
        ));
        let outcome = list_marketplaces_with_home(std::slice::from_ref(&path), None).unwrap();
        assert!(outcome.marketplaces.is_empty());
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, path);
    }
}

#[test]
fn selected_invalid_local_manifest_does_not_fall_back_to_catalog_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog(
        dir.path(),
        json!([{
            "name":"demo", "source":"./demo", "version":"fallback-version",
            "description":"fallback description", "category":"catalog category"
        }]),
    );
    let plugin_path = dir.path().join("demo/.codex-plugin/plugin.json");
    fs::create_dir_all(plugin_path.parent().unwrap()).unwrap();
    fs::write(&plugin_path, "{").unwrap();
    assert!(
        find_marketplace_plugin(&path, "demo")
            .unwrap()
            .manifest
            .is_none()
    );
    fs::remove_file(plugin_path).unwrap();
    let fallback = find_marketplace_plugin(&path, "demo")
        .unwrap()
        .manifest
        .unwrap();
    assert_eq!(fallback.version.as_deref(), Some("fallback-version"));
    assert_eq!(
        fallback.description.as_deref(),
        Some("fallback description")
    );
    assert_eq!(
        fallback.interface.unwrap().category.as_deref(),
        Some("catalog category")
    );
}

#[test]
fn remote_fallback_removes_all_resolved_paths_but_retains_raw_install_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let entry = |name, source| {
        json!({
            "name":name, "source":source, "version":"1.2.3", "keywords":["search"],
            "skills":["./skills"], "mcpServers":"./mcp.json", "apps":"./apps.json",
            "hooks":"./hooks.json", "category":"catalog-category",
        "extensions":{"com.openai":{"onboardingSkill":"./onboard/SKILL.md"}},
            "interface":{
            "displayName":"Demo", "category":"manifest-category",
                "composerIcon":"./icon.svg", "logo":"./logo.svg", "logoDark":"./dark.svg", "screenshots":["./shot.png"]
            }
        })
    };
    let path = catalog(
        dir.path(),
        json!([
            entry(
                "git-demo",
                json!({"source":"url", "url":"https://unreachable.invalid/repo.git"})
            ),
            entry(
                "npm-demo",
                json!({"source":"npm", "package":"@demo/plugin"})
            )
        ]),
    );
    for name in ["git-demo", "npm-demo"] {
        let resolved = find_marketplace_plugin(&path, name).unwrap();
        let manifest = resolved.manifest.unwrap();
        assert_eq!(manifest.version.as_deref(), Some("1.2.3"));
        assert_eq!(manifest.keywords, ["search"]);
        assert!(manifest.paths.skills.is_empty());
        assert!(manifest.paths.onboarding_skill.is_none());
        assert!(manifest.paths.mcp_servers.is_none());
        assert!(manifest.paths.apps.is_none());
        assert!(manifest.paths.hooks.is_none());
        let interface = manifest.interface.unwrap();
        assert!(interface.composer_icon.is_none());
        assert!(interface.logo.is_none());
        assert!(interface.logo_dark.is_none());
        assert!(interface.screenshots.is_empty());
        assert_eq!(
            resolved.interface.unwrap().category.as_deref(),
            Some("catalog-category")
        );
        let raw: Value = serde_json::from_str(resolved.manifest_fallback.contents()).unwrap();
        assert_eq!(raw["skills"], json!(["./skills"]));
        assert_eq!(raw["interface"]["logo"], "./logo.svg");
    }
    assert!(!dir.path().join("plugins").exists());
}

#[test]
fn nested_discovery_uses_valid_git_markers_and_deduplicates_explicit_catalog_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog(dir.path(), json!([]));
    let nested = dir.path().join("nested/deep");
    fs::create_dir_all(&nested).unwrap();
    let roots = [AbsolutePathBuf::try_from(nested.clone()).unwrap()];
    assert!(
        list_marketplaces_with_home(&roots, None)
            .unwrap()
            .marketplaces
            .is_empty()
    );
    fs::create_dir(dir.path().join(".git")).unwrap();
    assert!(
        list_marketplaces_with_home(&roots, None)
            .unwrap()
            .marketplaces
            .is_empty()
    );
    fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(
        list_marketplaces_with_home(&roots, None)
            .unwrap()
            .marketplaces[0]
            .path,
        path
    );
    fs::remove_dir_all(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git"), "gitdir: /unused/worktrees/demo\n").unwrap();
    let outcome =
        list_marketplaces_with_home(&[roots[0].clone(), path.clone()], Some(dir.path())).unwrap();
    assert_eq!(outcome.marketplaces.len(), 1);
    assert_eq!(outcome.marketplaces[0].path, path);
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn owner_cache_is_shared_by_store_clones_and_refreshes_manifest_and_catalog_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("catalog");
    let path = catalog(
        &root,
        json!([{"name":"demo", "source":"./demo", "category":"first"}]),
    );
    let manifest_path = root.join("demo/.codex-plugin/plugin.json");
    write_json(
        &manifest_path,
        &json!({"name":"demo", "version":"1.0.0", "interface":{"defaultPrompt":" "}}),
    );
    let owner = PluginStore::new(dir.path().join("home"));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let read = |store: &PluginStore| {
        list_with_store_cache(std::slice::from_ref(&path), None, store)
            .unwrap()
            .marketplaces
            .remove(0)
            .plugins
            .remove(0)
    };
    let first = read(&owner);
    assert_eq!(read(&owner.clone()), first);
    assert_eq!(
        String::from_utf8_lossy(&logs.lock().unwrap())
            .matches("ignoring interface.defaultPrompt")
            .count(),
        1
    );
    catalog(
        &root,
        json!([{"name":"demo", "source":"./demo", "category":"changed"}]),
    );
    assert_eq!(
        read(&owner).interface.unwrap().category.as_deref(),
        Some("changed")
    );
    write_json(
        &manifest_path,
        &json!({"name":"demo", "version":"2.0.0", "interface":{"defaultPrompt":" "}}),
    );
    assert_eq!(read(&owner).local_version.as_deref(), Some("2.0.0"));
    assert_eq!(
        read(&PluginStore::new(dir.path().join("home")))
            .local_version
            .as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        list_marketplaces_with_home(&[path], None)
            .unwrap()
            .marketplaces[0]
            .plugins[0]
            .local_version
            .as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        String::from_utf8_lossy(&logs.lock().unwrap())
            .matches("ignoring interface.defaultPrompt")
            .count(),
        4
    );
    assert!(!dir.path().join("home").exists());
}

#[test]
fn product_aliases_are_metadata_and_do_not_override_explicit_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog(
        dir.path(),
        json!([{
            "name":"demo", "source":"./missing", "policy":{"products":["CODEX", "atlas"]}
        }]),
    );
    let resolved = find_marketplace_plugin(&path, "demo").unwrap();
    assert_eq!(
        resolved.policy.products,
        Some(vec![Product::Codex, Product::Atlas])
    );
    for product in [None, Some(Product::Chatgpt)] {
        assert!(matches!(
            find_installable_marketplace_plugin(&path, "demo", product),
            Err(MarketplaceError::PluginNotAvailable { .. })
        ));
    }
    for product in [Product::Codex, Product::Atlas] {
        assert!(find_installable_marketplace_plugin(&path, "demo", Some(product)).is_ok());
    }
}

#[test]
fn origin_parser_keeps_its_distinct_ref_and_git_suffix_rules() {
    for (source, explicit_ref, expected_url, expected_ref) in [
        (
            " owner/repo.git# main ",
            None,
            "https://github.com/owner/repo.git.git",
            Some("main"),
        ),
        (
            "owner/repo@release",
            Some(" ".into()),
            "https://github.com/owner/repo.git",
            Some(" "),
        ),
        (
            "https://github.com/owner/repo.GIT/",
            None,
            "https://github.com/owner/repo.GIT.git",
            None,
        ),
    ] {
        let parsed = source_api::parse_marketplace_source(source, explicit_ref).unwrap();
        assert_eq!(
            parsed,
            source_api::MarketplaceSource::Git {
                url: expected_url.into(),
                ref_name: expected_ref.map(String::from),
            }
        );
    }
    assert!(
        matches!(source_api::parse_marketplace_source(" ", None), Err(MarketplaceAddError::InvalidRequest(message)) if message == "marketplace source must not be empty")
    );
}

#[test]
fn staging_admission_passes_git_parameters_and_preserves_callback_errors() {
    let source = source_api::MarketplaceSource::Git {
        url: "https://unreachable.invalid/repo".into(),
        ref_name: Some("release".into()),
    };
    let calls = Cell::new(0);
    let sparse = ["catalog".to_owned()];
    let stage = Path::new("unused-stage");
    let result = source_api::stage_marketplace_source(
        &source,
        &sparse,
        stage,
        |url, reference, paths, target| {
            calls.set(calls.get() + 1);
            assert_eq!(url, "https://unreachable.invalid/repo");
            assert_eq!(reference, Some("release"));
            assert_eq!(paths, sparse);
            assert_eq!(target, stage);
            Err(MarketplaceAddError::Internal(
                "offline clone failure".into(),
            ))
        },
    );
    assert_eq!(calls.get(), 1);
    assert!(
        matches!(result, Err(MarketplaceAddError::Internal(message)) if message == "offline clone failure")
    );
}
