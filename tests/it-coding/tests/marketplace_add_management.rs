// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use anyhow::Result;
use ra_coding::plugins::installed_marketplaces::marketplace_install_root;
use ra_coding::plugins::marketplace_add::management_test_api::*;
use ra_coding::plugins::marketplace_add::*;
use ra_config::ConfigRequirements;
use ra_config::RequirementSource;
use ra_config::RequirementsLayerEntry;
use ra_config::compose_requirements;
use ra_paths::AbsolutePathBuf;
use std::cell::Cell;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn requirements(requirements_toml: &str) -> ConfigRequirements {
    let with_sources = compose_requirements([RequirementsLayerEntry::from_toml(
        RequirementSource::Unknown,
        requirements_toml,
    )])
    .expect("compose requirements")
    .expect("requirements should be present");
    ConfigRequirements::try_from(with_sources).expect("normalize requirements")
}

#[test]
fn add_marketplace_sync_installs_marketplace_and_updates_config() -> Result<()> {
    let codex_home = TempDir::new()?;
    let source_root = TempDir::new()?;
    write_marketplace_source(source_root.path(), "remote copy")?;

    let result = add_marketplace_sync_with_cloner(
        codex_home.path(),
        &ConfigRequirements::default(),
        MarketplaceAddRequest {
            source: "https://github.com/owner/repo.git".to_string(),
            ref_name: None,
            sparse_paths: Vec::new(),
        },
        |_url, _ref_name, _sparse_paths, destination| {
            copy_dir_all(source_root.path(), destination)
                .map_err(|err| MarketplaceAddError::Internal(err.to_string()))
        },
    )?;

    assert_eq!(result.marketplace_name, "debug");
    assert_eq!(result.source_display, "https://github.com/owner/repo.git");
    assert!(!result.already_added);
    assert!(
        result
            .installed_root
            .as_path()
            .join(".agents/plugins/marketplace.json")
            .is_file()
    );

    let config = fs::read_to_string(codex_home.path().join(ra_config::CONFIG_TOML_FILE))?;
    assert!(config.contains("[marketplaces.debug]"));
    assert!(config.contains("source_type = \"git\""));
    assert!(config.contains("source = \"https://github.com/owner/repo.git\""));
    Ok(())
}

#[test]
fn denied_git_marketplace_does_not_clone_or_create_install_root() {
    let codex_home = TempDir::new().expect("create Codex home");
    let requirements = requirements(
        r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.company]
source = "git"
url = "https://github.com/example/allowed.git"
"#,
    );
    let cloner_called = Cell::new(false);

    let err = add_marketplace_sync_with_cloner(
        codex_home.path(),
        &requirements,
        MarketplaceAddRequest {
            source: "https://github.com/example/blocked.git".to_string(),
            ref_name: None,
            sparse_paths: Vec::new(),
        },
        |_url, _ref_name, _sparse_paths, _destination| {
            cloner_called.set(true);
            Ok(())
        },
    )
    .expect_err("blocked marketplace should fail");

    assert!(err.to_string().contains("is not allowed by requirements"));
    assert!(!cloner_called.get());
    assert!(!marketplace_install_root(codex_home.path()).exists());
    assert!(!codex_home.path().join(ra_config::CONFIG_TOML_FILE).exists());
}

#[test]
fn add_marketplace_sync_installs_local_directory_source_and_updates_config() -> Result<()> {
    let codex_home = TempDir::new()?;
    let source_root = TempDir::new()?;
    write_marketplace_source(source_root.path(), "local copy")?;

    let result = add_marketplace_sync_with_cloner(
        codex_home.path(),
        &ConfigRequirements::default(),
        MarketplaceAddRequest {
            source: source_root.path().display().to_string(),
            ref_name: None,
            sparse_paths: Vec::new(),
        },
        |_url, _ref_name, _sparse_paths, _destination| {
            panic!("git cloner should not be called for local marketplace sources")
        },
    )?;

    let expected_source = source_root.path().canonicalize()?.display().to_string();
    assert_eq!(result.marketplace_name, "debug");
    assert_eq!(result.source_display, expected_source);
    let expected_installed_root =
        AbsolutePathBuf::from_absolute_path(source_root.path().canonicalize()?)?;
    assert_eq!(result.installed_root, expected_installed_root);
    assert!(!result.already_added);
    assert!(
        !marketplace_install_root(codex_home.path())
            .join("debug")
            .exists()
    );

    let config = fs::read_to_string(codex_home.path().join(ra_config::CONFIG_TOML_FILE))?;
    let config: toml::Value = toml::from_str(&config)?;
    assert_eq!(
        config["marketplaces"]["debug"]["source_type"].as_str(),
        Some("local")
    );
    assert_eq!(
        config["marketplaces"]["debug"]["source"].as_str(),
        Some(expected_source.as_str())
    );
    Ok(())
}

#[test]
fn add_marketplace_sync_rejects_sparse_checkout_for_local_directory_source() -> Result<()> {
    let codex_home = TempDir::new()?;
    let source_root = TempDir::new()?;
    write_marketplace_source(source_root.path(), "local copy")?;

    let err = add_marketplace_sync_with_cloner(
        codex_home.path(),
        &ConfigRequirements::default(),
        MarketplaceAddRequest {
            source: source_root.path().display().to_string(),
            ref_name: None,
            sparse_paths: vec![".agents".to_string()],
        },
        |_url, _ref_name, _sparse_paths, _destination| {
            panic!("git cloner should not be called for local marketplace sources")
        },
    )
    .unwrap_err();

    assert_eq!(
        err.to_string(),
        "--sparse is only supported for git marketplace sources"
    );
    assert!(!codex_home.path().join(ra_config::CONFIG_TOML_FILE).exists());
    Ok(())
}

#[test]
fn add_marketplace_sync_treats_existing_local_directory_source_as_already_added() -> Result<()> {
    let codex_home = TempDir::new()?;
    let source_root = TempDir::new()?;
    write_marketplace_source(source_root.path(), "local copy")?;

    let request = MarketplaceAddRequest {
        source: source_root.path().display().to_string(),
        ref_name: None,
        sparse_paths: Vec::new(),
    };
    let requirements = ConfigRequirements::default();
    let first_result = add_marketplace_sync_with_cloner(
        codex_home.path(),
        &requirements,
        request.clone(),
        |_url, _ref_name, _sparse_paths, _destination| {
            panic!("git cloner should not be called for local marketplace sources")
        },
    )?;
    let second_result = add_marketplace_sync_with_cloner(
        codex_home.path(),
        &requirements,
        request,
        |_url, _ref_name, _sparse_paths, _destination| {
            panic!("git cloner should not be called for local marketplace sources")
        },
    )?;

    assert!(!first_result.already_added);
    assert!(second_result.already_added);
    assert_eq!(second_result.installed_root, first_result.installed_root);

    Ok(())
}

fn write_marketplace_source(source: &Path, marker: &str) -> std::io::Result<()> {
    fs::create_dir_all(source.join(".agents/plugins"))?;
    fs::create_dir_all(source.join("plugins/sample/.codex-plugin"))?;
    fs::write(
        source.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample",
      "source": {
        "source": "local",
        "path": "./plugins/sample"
      }
    }
  ]
}"#,
    )?;
    fs::write(
        source.join("plugins/sample/.codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )?;
    fs::write(source.join("plugins/sample/marker.txt"), marker)?;
    Ok(())
}

fn copy_dir_all(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_all(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}
