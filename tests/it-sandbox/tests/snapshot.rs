//! `ra-sandbox::snapshot`: putting a workspace archive into storage, and getting it back.
//!
//! What a snapshot stores is the only copy of a paused session's workspace, so the refusals here
//! are about never being the reason it is lost: a persist that fails leaves the previous archive
//! exactly as it was, an id that could name something outside the snapshot directory is refused
//! before anything is written, and storage this build cannot reach says so rather than reporting an
//! empty workspace as a successful restore.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    CloseDependency, Dependencies, ErrorCode, FactoryOptions, SandboxResult, Snapshot,
    dependency_factory,
};
use ra_sandbox::snapshot::{
    BuiltinSnapshotStore, RemoteSnapshotClient, RemoteSnapshotError, SnapshotStore,
    closable_remote_snapshot_client_dependency, remote_snapshot_client_dependency,
};
use rstest::rstest;

/// The archive body these tests move around.
const ARCHIVE: &[u8] = b"workspace-tar";

/// The key the remote snapshots in these tests name their client by.
const CLIENT_KEY: &str = "tests.remote_snapshot_client";

/// A session's dependencies with nothing bound, which is all local and no-op storage needs.
fn no_dependencies() -> Arc<Dependencies> {
    Arc::new(Dependencies::new())
}

/// Remote storage in memory, recording every call.
#[derive(Default)]
struct FakeRemoteClient {
    stored: Mutex<BTreeMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeRemoteClient {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn record(&self, call: &str, snapshot_id: &str) {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("{call}:{snapshot_id}"));
    }
}

#[async_trait]
impl RemoteSnapshotClient for FakeRemoteClient {
    async fn upload(&self, snapshot_id: &str, data: Vec<u8>) -> Result<(), RemoteSnapshotError> {
        self.record("upload", snapshot_id);
        self.stored
            .lock()
            .expect("stored")
            .insert(snapshot_id.to_owned(), data);
        Ok(())
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>, RemoteSnapshotError> {
        self.record("download", snapshot_id);
        self.stored
            .lock()
            .expect("stored")
            .get(snapshot_id)
            .cloned()
            .ok_or_else(|| RemoteSnapshotError::new(format!("no snapshot {snapshot_id}")))
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool, RemoteSnapshotError> {
        self.record("exists", snapshot_id);
        Ok(self
            .stored
            .lock()
            .expect("stored")
            .contains_key(snapshot_id))
    }
}

/// A client that can store and fetch but cannot say whether anything is stored.
#[derive(Default)]
struct UploadDownloadOnlyClient {
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
}

#[async_trait]
impl RemoteSnapshotClient for UploadDownloadOnlyClient {
    async fn upload(&self, snapshot_id: &str, data: Vec<u8>) -> Result<(), RemoteSnapshotError> {
        self.uploads
            .lock()
            .expect("uploads")
            .push((snapshot_id.to_owned(), data));
        Ok(())
    }

    async fn download(&self, _snapshot_id: &str) -> Result<Vec<u8>, RemoteSnapshotError> {
        Ok(b"downloaded".to_vec())
    }
}

/// Dependencies with `client` bound as a value under [`CLIENT_KEY`].
fn with_client(client: Arc<dyn RemoteSnapshotClient>) -> Arc<Dependencies> {
    let dependencies = Dependencies::new();
    dependencies
        .bind_value(CLIENT_KEY, remote_snapshot_client_dependency(client), false)
        .expect("bound");
    Arc::new(dependencies)
}

/// The built-in store with no dependencies bound, which is all local and no-op storage needs.
struct Store;

impl Store {
    async fn persist(&self, snapshot: &Snapshot, data: Vec<u8>) -> SandboxResult<()> {
        BuiltinSnapshotStore
            .persist(snapshot, data, &no_dependencies())
            .await
    }

    async fn restore(&self, snapshot: &Snapshot) -> SandboxResult<Vec<u8>> {
        BuiltinSnapshotStore
            .restore(snapshot, &no_dependencies())
            .await
    }

    async fn restorable(&self, snapshot: &Snapshot) -> SandboxResult<bool> {
        BuiltinSnapshotStore
            .restorable(snapshot, &no_dependencies())
            .await
    }
}

#[tokio::test]
async fn an_archive_comes_back_out_of_local_storage_as_it_went_in() {
    let directory = tempfile::tempdir().expect("temp");
    let snapshot = Snapshot::local("snap-1", directory.path()).expect("named");
    let store = Store;

    assert!(!store.restorable(&snapshot).await.expect("restorable"));

    store
        .persist(&snapshot, ARCHIVE.to_vec())
        .await
        .expect("persist");

    assert!(store.restorable(&snapshot).await.expect("restorable"));
    assert_eq!(store.restore(&snapshot).await.expect("restore"), ARCHIVE);
    // Named after the id, so that a directory holding many sessions' snapshots can be read by
    // whoever holds the states rather than only by the process that wrote them.
    assert!(directory.path().join("snap-1.tar").is_file());
}

#[tokio::test]
async fn storage_that_is_not_a_file_holds_nothing_to_restore() {
    let directory = tempfile::tempdir().expect("temp");
    let snapshot = Snapshot::local("snap-1", directory.path()).expect("named");
    let store = Store;
    let path = directory.path().join("snap-1.tar");

    assert!(!store.restorable(&snapshot).await.expect("restorable"));

    // A directory where the archive belongs is not half a snapshot; it is not one at all.
    std::fs::create_dir(&path).expect("directory");
    assert!(!store.restorable(&snapshot).await.expect("restorable"));

    std::fs::remove_dir(&path).expect("remove");
    std::fs::write(&path, ARCHIVE).expect("write");
    assert!(store.restorable(&snapshot).await.expect("restorable"));
}

#[tokio::test]
async fn the_directory_a_snapshot_belongs_in_is_created_on_the_way() {
    let directory = tempfile::tempdir().expect("temp");
    let base_path = directory.path().join("state").join("snapshots");
    let snapshot = Snapshot::local("snap-1", &base_path).expect("named");

    Store
        .persist(&snapshot, ARCHIVE.to_vec())
        .await
        .expect("persist");

    assert!(base_path.join("snap-1.tar").is_file());
}

#[rstest]
#[case("../escape")]
#[case("..\\escape")]
#[case("nested/escape")]
#[case("../")]
#[case("..//")]
#[case("..\\")]
#[case("nested/")]
#[case("nested//")]
#[case("nested\\")]
#[case(".")]
#[case("..")]
#[case("")]
// A drive prefix makes the rest of the id a path relative to that drive, on the host that reads
// this state back even if not on the one that wrote it.
#[case("c:snap")]
#[tokio::test]
async fn an_id_that_is_not_one_path_segment_is_refused(#[case] id: &str) {
    let directory = tempfile::tempdir().expect("temp");
    let snapshot = Snapshot::local(id, directory.path()).expect("named");
    let store = Store;

    for error in [
        store
            .persist(&snapshot, ARCHIVE.to_vec())
            .await
            .expect_err("refused"),
        store.restore(&snapshot).await.expect_err("refused"),
        store.restorable(&snapshot).await.expect_err("refused"),
    ] {
        assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
        assert!(error.message().contains("single path segment"), "{error}");
    }

    // Refused before anything is written: the point of the check is that the path was never used.
    assert_eq!(std::fs::read_dir(directory.path()).expect("list").count(), 0);
}

#[tokio::test]
async fn an_id_that_only_looks_like_a_drive_is_an_ordinary_name() {
    // A drive needs a single letter before the colon. `:a` and `ab:c` are names, and refusing them
    // would refuse ids the reference stores.
    let directory = tempfile::tempdir().expect("temp");
    let store = Store;

    for id in [":a", "ab:c", "...", ".hidden"] {
        let snapshot = Snapshot::local(id, directory.path()).expect("named");
        store
            .persist(&snapshot, ARCHIVE.to_vec())
            .await
            .expect("persist");
        assert_eq!(store.restore(&snapshot).await.expect("restore"), ARCHIVE);
    }
}

#[tokio::test]
async fn a_failed_persist_leaves_the_stored_snapshot_alone() {
    let directory = tempfile::tempdir().expect("temp");
    let snapshot = Snapshot::local("atomic", directory.path()).expect("named");
    let path = directory.path().join("atomic.tar");
    std::fs::write(&path, b"previous-snapshot").expect("write");

    // The archive is written beside the stored one and renamed over it, so the failure has to
    // happen where that write does: a directory nothing may add a file to.
    read_only(directory.path());
    let error = Store
        .persist(&snapshot, ARCHIVE.to_vec())
        .await
        .expect_err("refused");
    writable(directory.path());

    assert_eq!(error.error_code(), ErrorCode::SnapshotPersistError);
    assert_eq!(
        std::fs::read(&path).expect("read"),
        b"previous-snapshot".to_vec()
    );
    // And no half-written file left behind for the next run to find.
    let remaining: Vec<_> = std::fs::read_dir(directory.path())
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(remaining, ["atomic.tar"]);
}

#[tokio::test]
async fn the_snapshot_that_stores_nothing_accepts_an_archive_and_returns_none() {
    let store = Store;
    let snapshot = Snapshot::noop();

    // Persisting is not an error: a session with no storage still stops, and the stop path asks
    // its snapshot to take the archive.
    store
        .persist(&snapshot, ARCHIVE.to_vec())
        .await
        .expect("persist");

    assert!(!store.restorable(&snapshot).await.expect("restorable"));
    let error = store.restore(&snapshot).await.expect_err("refused");
    assert_eq!(error.error_code(), ErrorCode::SnapshotNotRestorable);
    assert_eq!(error.retryable(), Some(false));
}

#[tokio::test]
async fn a_local_snapshot_with_no_directory_names_nothing_to_open() {
    // Not reachable through `Snapshot::local`, and refused when parsing stored state — but a
    // payload assembled in process can still say `local` and stop there.
    let snapshot = Snapshot::new("local", "snap-1");
    let error = Store
        .restore(&snapshot)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

#[tokio::test]
async fn storage_this_build_cannot_reach_is_refused_rather_than_ignored() {
    let snapshot = Snapshot::new("host-object-store", "snap-1");
    let store = Store;

    for error in [
        store
            .persist(&snapshot, ARCHIVE.to_vec())
            .await
            .expect_err("refused"),
        store.restore(&snapshot).await.expect_err("refused"),
        store.restorable(&snapshot).await.expect_err("refused"),
    ] {
        assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
        assert_eq!(
            error.context().get("snapshot_type").and_then(|v| v.as_str()),
            Some(snapshot.snapshot_type())
        );
    }
}

/// Takes away the permission to add a file to a directory.
fn read_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500)).expect("permissions");
}

/// Gives it back, so the directory can be cleaned up.
fn writable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("permissions");
}

#[tokio::test]
async fn a_remote_snapshot_goes_through_the_client_its_dependency_names() {
    let client = Arc::new(FakeRemoteClient::default());
    let dependencies = with_client(client.clone());
    let snapshot = Snapshot::remote("snap-123", CLIENT_KEY);
    let store = BuiltinSnapshotStore;

    assert!(
        !store
            .restorable(&snapshot, &dependencies)
            .await
            .expect("restorable")
    );
    store
        .persist(&snapshot, ARCHIVE.to_vec(), &dependencies)
        .await
        .expect("persist");
    assert!(
        store
            .restorable(&snapshot, &dependencies)
            .await
            .expect("restorable")
    );
    let restored = store
        .restore(&snapshot, &dependencies)
        .await
        .expect("restore");

    assert_eq!(restored, ARCHIVE);
    assert_eq!(
        client.calls(),
        [
            "exists:snap-123",
            "upload:snap-123",
            "exists:snap-123",
            "download:snap-123"
        ]
    );
}

#[tokio::test]
async fn a_remote_client_without_exists_can_store_but_cannot_say_whether_it_has() {
    let client = Arc::new(UploadDownloadOnlyClient::default());
    let dependencies = with_client(client.clone());
    let snapshot = Snapshot::remote("snap-123", CLIENT_KEY);
    let store = BuiltinSnapshotStore;
    let expected = "Remote snapshot client must implement `exists(snapshot_id, ...)`";

    let error = store
        .restorable(&snapshot, &dependencies)
        .await
        .expect_err("no exists");
    assert_eq!(
        error.context().get("reason").and_then(|v| v.as_str()),
        Some(expected)
    );

    store
        .persist(&snapshot, ARCHIVE.to_vec(), &dependencies)
        .await
        .expect("persist");
    assert_eq!(
        *client.uploads.lock().expect("uploads"),
        [("snap-123".to_owned(), ARCHIVE.to_vec())]
    );

    let error = store
        .restorable(&snapshot, &dependencies)
        .await
        .expect_err("still no exists");
    assert_eq!(
        error.context().get("reason").and_then(|v| v.as_str()),
        Some(expected)
    );
}

#[tokio::test]
async fn a_remote_snapshot_whose_client_is_not_bound_says_which_key_is_missing() {
    let snapshot = Snapshot::remote("snap-123", CLIENT_KEY);
    let store = BuiltinSnapshotStore;

    // Storing and fetching wrap the lookup failure as their own, with the remote path, as the
    // reference does.
    let persist = store
        .persist(&snapshot, ARCHIVE.to_vec(), &no_dependencies())
        .await
        .expect_err("no client");
    assert_eq!(persist.error_code(), ErrorCode::SnapshotPersistError);
    assert_eq!(
        persist.context().get("path").and_then(|v| v.as_str()),
        Some("<remote:tests.remote_snapshot_client>")
    );
    let restore = store
        .restore(&snapshot, &no_dependencies())
        .await
        .expect_err("no client");
    assert_eq!(restore.error_code(), ErrorCode::SnapshotRestoreError);

    // Asking whether anything is stored reports the missing binding itself.
    let restorable = store
        .restorable(&snapshot, &no_dependencies())
        .await
        .expect_err("no client");
    assert_eq!(restorable.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(
        restorable
            .message()
            .contains("Missing dependency `tests.remote_snapshot_client` for RemoteSnapshot"),
        "{restorable}"
    );
}

#[tokio::test]
async fn a_remote_snapshot_refuses_a_dependency_that_is_not_a_client() {
    let dependencies = Dependencies::new();
    dependencies
        .bind_value(
            CLIENT_KEY,
            ra_core::sandbox::DependencyValue::new(Arc::new("not a client".to_owned())),
            false,
        )
        .expect("bound");
    let snapshot = Snapshot::remote("snap-123", CLIENT_KEY);

    let error = BuiltinSnapshotStore
        .restorable(&snapshot, &Arc::new(dependencies))
        .await
        .expect_err("wrong type");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(error.message().contains("is not a"), "{error}");
}

/// Remote storage that holds a connection, and counts how often it was released.
#[derive(Default)]
struct ConnectedRemoteClient {
    closes: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl RemoteSnapshotClient for ConnectedRemoteClient {
    async fn upload(&self, _snapshot_id: &str, _data: Vec<u8>) -> Result<(), RemoteSnapshotError> {
        Ok(())
    }

    async fn download(&self, _snapshot_id: &str) -> Result<Vec<u8>, RemoteSnapshotError> {
        Ok(ARCHIVE.to_vec())
    }
}

#[async_trait]
impl CloseDependency for ConnectedRemoteClient {
    async fn close(&self) {
        self.closes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_remote_client_an_owning_factory_built_is_closed_with_the_session_dependencies() {
    let client = Arc::new(ConnectedRemoteClient::default());
    let factory_client = Arc::clone(&client);
    let dependencies = Arc::new(Dependencies::new());
    dependencies
        .bind_factory(
            CLIENT_KEY,
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(move |_| {
                let client = Arc::clone(&factory_client);
                async move { Ok(closable_remote_snapshot_client_dependency(client)) }
            }),
        )
        .expect("bound");
    let snapshot = Snapshot::remote("snap-123", CLIENT_KEY);

    // Read through the client trait, as the snapshot store reads it.
    BuiltinSnapshotStore
        .persist(&snapshot, ARCHIVE.to_vec(), &dependencies)
        .await
        .expect("persist");
    dependencies.close().await;

    assert_eq!(client.closes.load(std::sync::atomic::Ordering::SeqCst), 1);
}
