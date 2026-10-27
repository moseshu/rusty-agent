//! Discovery materializes files on their owning filesystem and freezes one step's snapshot.
use ra_executor::capabilities::{CapabilityRootLocation, SelectedCapabilityRoot};
use ra_executor::{EnvironmentManager, ExecutorCapabilityDiscoveryCache};
use ra_paths::PathUri;
use std::{collections::HashMap, sync::Arc};

#[tokio::test]
async fn real_discovery_snapshot_retains_cached_files_and_root_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    std::fs::create_dir_all(root.join(".codex-plugin")).unwrap();
    std::fs::create_dir_all(root.join("skills/demo")).unwrap();
    std::fs::write(
        root.join(".codex-plugin/plugin.json"),
        r#"{"name":"demo","skills":"./skills"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: test\n---\ncontent",
    )
    .unwrap();
    let selected = SelectedCapabilityRoot {
        id: "demo@local".into(),
        location: CapabilityRootLocation::Environment {
            environment_id: "local".into(),
            path: PathUri::from_host_native_path(&root).unwrap(),
        },
    };
    let cache =
        ExecutorCapabilityDiscoveryCache::new(Arc::new(EnvironmentManager::default_for_tests()));
    let snapshot = cache
        .snapshot(std::slice::from_ref(&selected), &HashMap::new())
        .await;
    let entry = &snapshot.roots()[0];
    assert_eq!(entry.selected_root, selected);
    let discovery = entry.result.as_ref().unwrap();
    assert!(discovery.plugin.is_some());
    assert_eq!(discovery.skills.len(), 1);
    std::fs::remove_file(root.join(".codex-plugin/plugin.json")).unwrap();
    let later = cache.snapshot(&[selected], &HashMap::new()).await;
    assert!(Arc::ptr_eq(
        discovery,
        later.roots()[0].result.as_ref().unwrap()
    ));
    assert!(!cache.take_recovered_discovery());
}

#[tokio::test]
async fn unavailable_environment_never_reads_an_existing_host_directory() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".codex-plugin")).unwrap();
    std::fs::write(temp.path().join(".codex-plugin/plugin.json"), "{}").unwrap();
    let selected = SelectedCapabilityRoot {
        id: "missing@local".into(),
        location: CapabilityRootLocation::Environment {
            environment_id: "remote-unavailable".into(),
            path: PathUri::from_host_native_path(temp.path()).unwrap(),
        },
    };
    let cache =
        ExecutorCapabilityDiscoveryCache::new(Arc::new(EnvironmentManager::default_for_tests()));
    let snapshot = cache.snapshot(&[selected], &HashMap::new()).await;
    assert!(
        snapshot.roots()[0]
            .result
            .as_ref()
            .unwrap_err()
            .contains("unavailable")
    );
}
