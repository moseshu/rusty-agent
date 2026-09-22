//! `ra-sandbox::snapshot`: putting a workspace archive into storage, and getting it back.
//!
//! What a snapshot stores is the only copy of a paused session's workspace, so the refusals here
//! are about never being the reason it is lost: a persist that fails leaves the previous archive
//! exactly as it was, an id that could name something outside the snapshot directory is refused
//! before anything is written, and storage this build cannot reach says so rather than reporting an
//! empty workspace as a successful restore.

use ra_core::sandbox::{ErrorCode, Snapshot};
use ra_sandbox::snapshot::{BuiltinSnapshotStore, SnapshotStore};
use rstest::rstest;

/// The archive body these tests move around.
const ARCHIVE: &[u8] = b"workspace-tar";

#[tokio::test]
async fn an_archive_comes_back_out_of_local_storage_as_it_went_in() {
    let directory = tempfile::tempdir().expect("temp");
    let snapshot = Snapshot::local("snap-1", directory.path()).expect("named");
    let store = BuiltinSnapshotStore;

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
    let store = BuiltinSnapshotStore;
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

    BuiltinSnapshotStore
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
    let store = BuiltinSnapshotStore;

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
    let store = BuiltinSnapshotStore;

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
    let error = BuiltinSnapshotStore
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
    let store = BuiltinSnapshotStore;
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
    let error = BuiltinSnapshotStore
        .restore(&snapshot)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

#[rstest]
#[case(Snapshot::remote("snap-1", "tests.client"))]
#[case(Snapshot::new("host-object-store", "snap-1"))]
#[tokio::test]
async fn storage_this_build_cannot_reach_is_refused_rather_than_ignored(
    #[case] snapshot: Snapshot,
) {
    let store = BuiltinSnapshotStore;

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
