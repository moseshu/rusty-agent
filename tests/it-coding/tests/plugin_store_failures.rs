// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Staging, activation, rollback, and cleanup failures retain source side effects.

use ra_coding::plugins::{
    AbsolutePathBuf, PluginId,
    store::{PluginStore, PluginStoreError, test_api as api},
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("plugins/cache/debug/demo");
    fs::create_dir_all(source.join(".codex-plugin")).unwrap();
    fs::write(
        source.join(".codex-plugin/plugin.json"),
        r#"{"name":"demo"}"#,
    )
    .unwrap();
    fs::write(source.join("new"), "new").unwrap();
    fs::create_dir_all(target.join("local")).unwrap();
    fs::write(target.join("local/old"), "old").unwrap();
    (temp, source, target)
}
fn fault() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "injected filesystem failure",
    )
}
fn no_temporary_entries(target: &Path) {
    let names: Vec<_> = fs::read_dir(target.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["demo"]);
}

#[test]
fn failed_copy_keeps_old_cache_and_cleans_staging() {
    let (_temp, source, target) = fixture();
    let error = api::replace_with_ops(
        &source,
        &target,
        "local",
        None,
        |_, _| {
            Err(PluginStoreError::Io {
                context: "failed to copy plugin file",
                source: fault(),
            })
        },
        |a, b| fs::rename(a, b),
        |p| fs::remove_dir_all(p),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("failed to copy plugin file:"));
    assert_eq!(
        api::error_subtype(&error).as_deref(),
        Some("failed_to_copy_plugin_file")
    );
    assert_eq!(fs::read_to_string(target.join("local/old")).unwrap(), "old");
    no_temporary_entries(&target);
}

#[test]
fn staging_rechecks_manifest_presence_path_and_bytes_before_activation() {
    for case in ["missing", "bytes", "path", "source_changed"] {
        let (_temp, source, target) = fixture();
        if case == "path" {
            fs::rename(source.join(".codex-plugin"), source.join(".claude-plugin")).unwrap();
        }
        let error = api::replace_with_ops(
            &source,
            &target,
            "local",
            None,
            |a, b| {
                if case == "source_changed" {
                    fs::write(a.join(".codex-plugin/plugin.json"), r#"{"name":"changed"}"#)
                        .unwrap();
                }
                api::copy_directory(a, b)?;
                match case {
                    "missing" => fs::remove_file(b.join(".codex-plugin/plugin.json")).unwrap(),
                    "bytes" => {
                        fs::write(b.join(".codex-plugin/plugin.json"), r#"{"name":"changed"}"#)
                            .unwrap()
                    }
                    "path" => {
                        fs::create_dir_all(b.join(".codex-plugin")).unwrap();
                        fs::write(b.join(".codex-plugin/plugin.json"), r#"{"name":"demo"}"#)
                            .unwrap();
                    }
                    _ => {}
                }
                Ok(())
            },
            |_, _| panic!("activation must not run"),
            |p| fs::remove_dir_all(p),
        )
        .unwrap_err();
        let expected = match case {
            "missing" => "plugin manifest is missing after installation staging",
            "path" => "plugin manifest changed during installation staging",
            _ => "plugin manifest contents changed during installation staging",
        };
        assert_eq!(error.to_string(), expected);
        assert!(target.join("local/old").is_file());
        no_temporary_entries(&target);
    }
}

#[test]
fn failed_backup_preserves_old_root() {
    let (_temp, source, target) = fixture();
    let error = api::replace_with_ops(
        &source,
        &target,
        "local",
        None,
        api::copy_directory,
        |_, _| Err(fault()),
        |p| fs::remove_dir_all(p),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("failed to back up plugin cache entry:")
    );
    assert!(target.join("local/old").is_file());
    no_temporary_entries(&target);
}

#[test]
fn failed_same_version_activation_restores_the_old_root() {
    let (_temp, source, target) = fixture();
    let mut renames = 0;
    let error = api::replace_with_ops(
        &source,
        &target,
        "local",
        None,
        api::copy_directory,
        |a, b| {
            renames += 1;
            if renames == 2 {
                Err(fault())
            } else {
                fs::rename(a, b)
            }
        },
        |p| fs::remove_dir_all(p),
    )
    .unwrap_err();
    assert_eq!(renames, 3);
    assert!(
        error
            .to_string()
            .starts_with("failed to activate updated plugin cache entry:")
    );
    assert!(target.join("local/old").is_file());
    assert!(!target.join("local/new").exists());
    no_temporary_entries(&target);
}

#[test]
fn failed_restore_keeps_the_backup_and_reports_its_location() {
    let (_temp, source, target) = fixture();
    let mut renames = 0;
    let mut backup = None;
    let error = api::replace_with_ops(
        &source,
        &target,
        "local",
        None,
        api::copy_directory,
        |a, b| {
            renames += 1;
            if renames == 1 {
                fs::rename(a, b)
            } else {
                if renames == 3 {
                    backup = Some(a.to_path_buf());
                }
                Err(fault())
            }
        },
        |p| fs::remove_dir_all(p),
    )
    .unwrap_err();
    let backup = backup.unwrap();
    let message = error.to_string();
    assert!(matches!(error, PluginStoreError::Invalid(_)));
    assert!(message.contains("failed to restore previous cache entry (left at"));
    assert!(message.contains(backup.to_str().unwrap()));
    assert!(backup.join("local/old").is_file());
    assert!(!target.exists());
    assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn fresh_activation_failure_cleans_staging_without_creating_an_installation() {
    let (_temp, source, target) = fixture();
    fs::remove_dir_all(&target).unwrap();
    let error = api::replace_with_ops(
        &source,
        &target,
        "local",
        None,
        api::copy_directory,
        |_, _| Err(fault()),
        |p| fs::remove_dir_all(p),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("failed to activate plugin cache entry:")
    );
    assert!(!target.exists());
    assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 0);
}

#[test]
fn failed_new_version_activation_leaves_the_previous_version_selected() {
    let (_temp, source, target) = fixture();
    let error = api::replace_with_ops(
        &source,
        &target,
        "2.0.0",
        None,
        api::copy_directory,
        |_, _| Err(fault()),
        |p| fs::remove_dir_all(p),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("failed to activate updated plugin cache version:")
    );
    assert!(target.join("local/old").is_file());
    assert!(!target.join("2.0.0").exists());
    no_temporary_entries(&target);
}

#[test]
fn cleanup_failure_reports_errors_only_when_an_old_version_remains_active() {
    for (old, new, should_error) in [
        ("local", "2.0.0", true),
        ("local", "zeta", true),
        ("10.0.0", "9.0.0", true),
        ("1.0.0", "2.0.0", false),
    ] {
        let (temp, source, target) = fixture();
        if old != "local" {
            fs::rename(target.join("local"), target.join(old)).unwrap();
        }
        let result = api::replace_with_ops(
            &source,
            &target,
            new,
            None,
            api::copy_directory,
            |a, b| fs::rename(a, b),
            |_| Err(fault()),
        );
        assert!(target.join(format!("{new}/new")).is_file());
        assert!(target.join(old).is_dir());
        if should_error {
            assert_eq!(
                result.unwrap_err().to_string(),
                format!(
                    "failed to activate updated plugin cache version `{new}` while `{old}` remains active"
                )
            );
        } else {
            result.unwrap();
        }
        let store = PluginStore::new(temp.path().to_path_buf());
        let id = PluginId::parse("demo@debug").unwrap();
        assert_eq!(
            store.active_plugin_version(&id).as_deref(),
            Some(if should_error { old } else { new })
        );
    }
}

#[test]
fn same_version_replacement_and_uninstall_preserve_source_and_persistent_data() {
    let (temp, source, target) = fixture();
    let store = PluginStore::new(temp.path().to_path_buf());
    let id = PluginId::parse("demo@debug").unwrap();
    let data = store.plugin_data_root(&id);
    fs::create_dir_all(data.as_path()).unwrap();
    fs::write(data.as_path().join("kept"), "user data").unwrap();
    store
        .install(
            AbsolutePathBuf::try_from(source.clone()).unwrap(),
            id.clone(),
        )
        .unwrap();
    assert!(!target.join("local/old").exists());
    assert!(target.join("local/new").is_file());
    no_temporary_entries(&target);
    assert!(source.join("new").is_file());
    assert!(store.is_installed(&id));
    store.uninstall(&id).unwrap();
    store.uninstall(&id).unwrap();
    assert!(!target.exists());
    assert!(!store.is_installed(&id));
    assert!(source.join("new").is_file());
    assert_eq!(
        fs::read_to_string(data.as_path().join("kept")).unwrap(),
        "user data"
    );
}

#[test]
fn invalid_selected_manifest_does_not_use_a_fallback_or_replace_old_cache() {
    let (temp, source, target) = fixture();
    fs::write(source.join(".codex-plugin/plugin.json"), "{broken").unwrap();
    let error = api::install_with_fallback(
        &PluginStore::new(temp.path().to_path_buf()),
        AbsolutePathBuf::try_from(source).unwrap(),
        PluginId::parse("demo@debug").unwrap(),
        r#"{"name":"demo"}"#,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("failed to parse plugin.json:")
    );
    assert!(target.join("local/old").is_file());
}
