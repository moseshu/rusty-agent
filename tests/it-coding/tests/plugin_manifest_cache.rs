// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Cache revision, ownership, diagnostics, capacity, and concurrency contracts.

use ra_coding::plugins::{
    manifest::test_api::Cache,
    store::{PluginStore, test_api as store_api},
};
use std::{
    cell::Cell,
    io::{self, Write},
    sync::{Arc, Mutex},
};

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
fn capture() -> (Arc<Mutex<Vec<u8>>>, impl tracing::Subscriber) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(buffer.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (buffer, subscriber)
}

#[test]
fn repeated_revision_runs_parser_once_and_changes_run_it_again() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join(".codex-plugin/plugin.json");
    let first = r#"{"name":"demo","description":"first"}"#;
    let changed = r#"{"name":"demo","description":"changed"}"#;
    let cache = Cache::default();
    let parses = Cell::new(0);
    let results: Vec<_> = std::iter::repeat_n(first, 220)
        .chain([changed, first])
        .map(|contents| {
            cache
                .parse_with(root, &path, contents, None, || parses.set(parses.get() + 1))
                .unwrap()
        })
        .collect();
    let original = results[0].clone();
    let mut revised = original.clone();
    revised.description = Some("changed".into());
    assert_eq!(
        results,
        std::iter::repeat_n(original.clone(), 220)
            .chain([revised, original])
            .collect::<Vec<_>>()
    );
    assert_eq!(parses.get(), 3);
}

#[test]
fn invalid_fields_warn_once_until_the_manifest_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(".codex-plugin/plugin.json");
    let (logs, subscriber) = capture();
    let _guard = tracing::subscriber::set_default(subscriber);
    let original =
        r#"{"name":"demo","interface":{"defaultPrompt":" ","composerIcon":"../outside.svg"}}"#;
    let changed = r#"{"name":"demo","description":"changed","interface":{"defaultPrompt":" ","composerIcon":"../outside.svg"}}"#;
    let cache = Cache::default();
    let parse = |contents| {
        cache
            .parse_with(directory.path(), &path, contents, None, || {})
            .unwrap()
    };
    let first = parse(original);
    for _ in 0..220 {
        assert_eq!(parse(original), first);
    }
    let mut expected = first.clone();
    expected.description = Some("changed".into());
    assert_eq!(parse(changed), expected);
    assert_eq!(parse(original), first);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(logs.matches("ignoring interface.defaultPrompt").count(), 3);
    assert_eq!(logs.matches("ignoring interface.composerIcon").count(), 3);
}

#[test]
fn overlay_revision_and_path_are_part_of_cached_parse() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join("plugin.json");
    let overlay_path = root.join(".codex-plugin/plugin.json");
    let other_path = root.join("overlay.json");
    let contents = serde_json::json!({"$schema":ra_coding::plugins::discovery::AGENT_PLUGIN_SCHEMA_URI,"name":"demo"}).to_string();
    let cache = Cache::default();
    let parses = Cell::new(0);
    for (overlay, expected) in [
        (
            Some((
                overlay_path.as_path(),
                r#"{"interface":{"defaultPrompt":"first"}}"#,
            )),
            Some("first"),
        ),
        (
            Some((
                overlay_path.as_path(),
                r#"{"interface":{"defaultPrompt":"first"}}"#,
            )),
            Some("first"),
        ),
        (
            Some((
                overlay_path.as_path(),
                r#"{"interface":{"defaultPrompt":"changed"}}"#,
            )),
            Some("changed"),
        ),
        (
            Some((
                other_path.as_path(),
                r#"{"interface":{"defaultPrompt":"changed"}}"#,
            )),
            Some("changed"),
        ),
        (None, None),
    ] {
        let manifest = cache
            .parse_with(root, &path, &contents, overlay, || {
                parses.set(parses.get() + 1)
            })
            .unwrap();
        assert_eq!(
            manifest.interface.unwrap().default_prompt,
            expected.map(|s| vec![s.to_owned()])
        );
    }
    assert_eq!(parses.get(), 4);
}

#[test]
fn resource_root_and_manifest_path_are_independent_cache_keys() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let other = root.join("other");
    let path = root.join("manifest.json");
    let another_path = root.join("another.json");
    let cache = Cache::default();
    let parses = Cell::new(0);
    for (root, path) in [
        (root, &path),
        (other.as_path(), &path),
        (other.as_path(), &another_path),
    ] {
        let manifest = cache
            .parse_with(
                root,
                path,
                r#"{"name":"demo","skills":"./skills"}"#,
                None,
                || parses.set(parses.get() + 1),
            )
            .unwrap();
        assert_eq!(manifest.paths.skills[0].as_path(), root.join("skills"));
    }
    assert_eq!(parses.get(), 3);
}

#[test]
fn failed_revision_is_retried_and_repair_is_visible() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join(".codex-plugin/plugin.json");
    let cache = Cache::default();
    let parses = Cell::new(0);
    for contents in ["{", "{"] {
        assert!(
            cache
                .parse_with(root, &path, contents, None, || parses.set(parses.get() + 1))
                .is_err()
        );
    }
    assert_eq!(
        cache
            .parse_with(root, &path, r#"{"name":"repaired"}"#, None, || parses
                .set(parses.get() + 1))
            .unwrap()
            .name,
        "repaired"
    );
    assert_eq!(parses.get(), 3);
}

#[cfg(unix)]
#[test]
fn opaque_host_roots_reject_legacy_manifests_before_resolving_resources() {
    let root = std::path::Path::new("/C:/pkg");
    let path = root.join(".codex-plugin/plugin.json");
    for cache in [Cache::default(), Cache::disabled()] {
        let parses = Cell::new(0);
        for _ in 0..2 {
            let result = cache.parse_with(root, &path, r#"{"name":"demo"}"#, None, || {
                parses.set(parses.get() + 1);
            });
            assert!(
                result.is_err(),
                "an opaque root must reject the entire manifest"
            );
        }
        assert_eq!(parses.get(), 2);
    }
}

#[cfg(windows)]
#[test]
fn equivalent_windows_paths_share_a_cached_revision() {
    let cache = Cache::default();
    let parses = Cell::new(0);
    let mut manifests = Vec::new();
    for value in [r"C:\plugins\Demo", r"c:\plugins\demo"] {
        let root = std::path::Path::new(value);
        let path = root.join(".codex-plugin/plugin.json");
        manifests.push(
            cache
                .parse_with(root, &path, r#"{"name":"demo"}"#, None, || {
                    parses.set(parses.get() + 1)
                })
                .unwrap(),
        );
    }
    assert_eq!(manifests[0], manifests[1]);
    assert_eq!(parses.get(), 1);
}

#[test]
fn oversized_revision_invalidates_previous_result_without_being_cached() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join("manifest.json");
    let small = r#"{"name":"demo"}"#;
    let large = format!("{}{small}", " ".repeat(64 * 1024));
    let cache = Cache::default();
    let parses = Cell::new(0);
    for contents in [small, small, &large, &large, small] {
        cache
            .parse_with(root, &path, contents, None, || parses.set(parses.get() + 1))
            .unwrap();
    }
    assert_eq!(parses.get(), 4);
}

#[test]
fn oversized_parse_does_not_block_unrelated_manifest() {
    use std::{sync::mpsc, time::Duration};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let large_path = root.join("large.json");
    let small_path = root.join("small.json");
    let large = format!("{}{{}}", " ".repeat(64 * 1024));
    let cache = Cache::default();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    let cache = &cache;
    let large_path = &large_path;
    let small_path = &small_path;
    let large = &large;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            cache
                .parse_with(root, large_path, large, None, || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        scope.spawn(move || {
            cache
                .parse_with(root, small_path, "{}", None, || {})
                .unwrap();
            completed_tx.send(()).unwrap();
        });
        let completed = completed_rx.recv_timeout(Duration::from_secs(10));
        release_tx.send(()).unwrap();
        assert_eq!(completed, Ok(()));
    });
}

#[test]
fn store_clones_reuse_validation_but_independent_stores_warn_again() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("plugin");
    std::fs::create_dir_all(root.join(".codex-plugin")).unwrap();
    std::fs::write(
        root.join(".codex-plugin/plugin.json"),
        r#"{"name":"demo","interface":{"defaultPrompt":" "}}"#,
    )
    .unwrap();
    let (logs, subscriber) = capture();
    let _guard = tracing::subscriber::set_default(subscriber);
    let store = PluginStore::new(directory.path().to_path_buf());
    let cloned = store.clone();
    let first = store_api::load_cached(&store, &root).unwrap();
    for _ in 0..220 {
        assert_eq!(store_api::load_cached(&cloned, &root).unwrap(), first);
    }
    let independent = PluginStore::new(directory.path().to_path_buf());
    assert_eq!(store_api::load_cached(&independent, &root).unwrap(), first);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("ignoring interface.defaultPrompt: prompt must not be empty")
            .count(),
        2
    );
}

#[test]
fn disabled_cache_parses_each_load() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join("manifest.json");
    let cache = Cache::disabled();
    let parses = Cell::new(0);
    for _ in 0..3 {
        assert_eq!(
            cache
                .parse_with(root, &path, r#"{"name":"demo"}"#, None, || parses
                    .set(parses.get() + 1))
                .unwrap()
                .name,
            "demo"
        );
    }
    assert_eq!(parses.get(), 3);
}

#[test]
fn bounded_cache_evicts_the_least_recently_used_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cache = Cache::default();
    let parses = Cell::new(0);
    let parse = |index| {
        cache
            .parse_with(
                root,
                &root.join(format!("{index}.json")),
                "{}",
                None,
                || parses.set(parses.get() + 1),
            )
            .unwrap()
    };
    for index in 0..128 {
        parse(index);
    }
    parse(0);
    parse(128);
    assert_eq!(parses.get(), 129);
    parse(0);
    assert_eq!(parses.get(), 129);
    parse(1);
    assert_eq!(parses.get(), 130);
}

#[test]
fn concurrent_cache_misses_validate_one_revision_once() {
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cache = Cache::default();
    let count = AtomicUsize::new(0);
    let start = Barrier::new(16);
    std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for _ in 0..16 {
            joins.push(scope.spawn(|| {
                start.wait();
                cache
                    .parse_with(
                        root,
                        &root.join("manifest.json"),
                        r#"{"name":"demo"}"#,
                        None,
                        || {
                            count.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .unwrap()
            }));
        }
        for join in joins {
            assert_eq!(join.join().unwrap().name, "demo");
        }
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
