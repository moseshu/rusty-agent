//! `ra-sandbox::unix_local`: stopping a real workspace and starting it again from what was stored.
//!
//! The lifecycle tests cover the decisions; this covers the thing those decisions are for. A
//! session writes files, stops, and a second session over the same state comes up holding what the
//! first one had — including the part where the workspace is emptied first, so a file the stored
//! archive does not mention does not survive into the resumed workspace.

use std::path::Path;

use ra_core::sandbox::{
    CreateRequest, ExecRequest, Manifest, SandboxClient, ShellInvocation, SnapshotSpec,
};
use ra_sandbox::snapshot::lifecycle::SNAPSHOT_FINGERPRINT_VERSION;
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// A manifest rooted at a directory the caller owns.
fn manifest_at(root: &Path) -> Manifest {
    Manifest::new().with_root(root.to_string_lossy().into_owned())
}

/// Storage for snapshots, in a directory of its own.
fn spec(base_path: &Path) -> SnapshotSpec {
    SnapshotSpec::Local {
        base_path: base_path.to_path_buf(),
    }
}

#[tokio::test]
async fn a_workspace_comes_back_from_what_its_stop_stored() {
    let workspace = tempfile::tempdir().expect("temp");
    let snapshots = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();

    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(manifest_at(workspace.path()))
                .with_snapshot_spec(spec(snapshots.path())),
        )
        .await
        .expect("create");
    session.start().await.expect("start");
    session
        .write(
            "notes.md".into(),
            b"the first session's work".to_vec(),
            None,
        )
        .await
        .expect("write");

    session.stop().await.expect("stop");
    let state = session.state();
    assert!(
        snapshots
            .path()
            .join(format!("{}.tar", state.session_id()))
            .is_file(),
        "the stop stored an archive named after the session"
    );

    // What happens to the workspace between the stop and the resume is not the snapshot's business,
    // so make it obvious: one file is gone and another arrived.
    std::fs::remove_file(workspace.path().join("notes.md")).expect("remove");
    std::fs::write(
        workspace.path().join("stray.txt"),
        b"written after the stop",
    )
    .expect("write");

    let resumed = client.resume(state).await.expect("resume");
    resumed.start().await.expect("start");

    assert_eq!(
        resumed.read("notes.md".into(), None).await.expect("read"),
        b"the first session's work".to_vec()
    );
    // The workspace is emptied before the archive is extracted: a file the archive does not mention
    // would otherwise survive into a workspace that is supposed to be what was stored.
    assert!(!workspace.path().join("stray.txt").exists());
}

#[tokio::test]
async fn a_session_that_stores_nothing_leaves_its_workspace_where_it_is() {
    let workspace = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();

    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");
    session
        .write(
            "notes.md".into(),
            b"kept by the filesystem, not by a snapshot".to_vec(),
            None,
        )
        .await
        .expect("write");

    session.stop().await.expect("stop");

    // Nothing was stored, and nothing was restored over: the directory is simply still there, which
    // is what "the snapshot that stores nothing" means on a backend whose workspace is a directory.
    assert!(workspace.path().join("notes.md").is_file());
    assert_eq!(session.state().snapshot_fingerprint(), None);

    let resumed = client.resume(session.state()).await.expect("resume");
    resumed.start().await.expect("start");
    assert!(workspace.path().join("notes.md").is_file());
}

#[tokio::test]
async fn a_stop_stores_the_workspace_whether_or_not_it_could_hash_it() {
    let workspace = tempfile::tempdir().expect("temp");
    let snapshots = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();

    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(manifest_at(workspace.path()))
                .with_snapshot_spec(spec(snapshots.path())),
        )
        .await
        .expect("create");
    session.start().await.expect("start");
    session.stop().await.expect("stop");

    let state = session.state();
    assert!(
        snapshots
            .path()
            .join(format!("{}.tar", state.session_id()))
            .is_file()
    );
    // Whether the workspace could be hashed depends on the platform: the fingerprint helper
    // installs under `/tmp`, and this backend's macOS fence denies writes there — the reference's
    // fence denies them too. Either answer is correct; recording a hash without the scheme that
    // produced it would not be.
    match state.snapshot_fingerprint() {
        Some((fingerprint, version)) => {
            assert!(!fingerprint.is_empty());
            assert_eq!(version, SNAPSHOT_FINGERPRINT_VERSION);
        }
        None => {
            // A write, not only a `mkdir -p`: other tests install the helper from outside the
            // fence, and creating a directory that already exists writes nothing.
            let helper_root_writable = session
                .exec(
                    ExecRequest::new(vec![
                        "sh".to_owned(),
                        "-c".to_owned(),
                        "mkdir -p /tmp/rusty-agent/bin && probe=$(mktemp /tmp/rusty-agent/bin/.probe.XXXXXX) && rm -f \"$probe\"".to_owned(),
                    ])
                    .with_shell(ShellInvocation::None),
                )
                .await
                .expect("exec")
                .ok();
            assert!(
                !helper_root_writable,
                "a sandbox that can write the helper root should have produced a fingerprint"
            );
        }
    }
}

#[tokio::test]
async fn restoring_removes_stray_links_without_touching_their_targets() {
    let workspace = tempfile::tempdir().expect("workspace");
    let snapshots = tempfile::tempdir().expect("snapshots");
    let outside = tempfile::tempdir().expect("outside");
    let protected = outside.path().join("protected");
    std::fs::write(&protected, b"untouched").expect("outside file");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(manifest_at(workspace.path()))
                .with_snapshot_spec(spec(snapshots.path())),
        )
        .await
        .expect("create");
    session.start().await.expect("start");
    session
        .write("notes".into(), b"stored".to_vec(), None)
        .await
        .expect("write");
    session.stop().await.expect("stop");
    for (name, target) in [
        ("internal-link", workspace.path().join("notes")),
        ("external-link", protected.clone()),
        ("dangling-link", workspace.path().join("missing")),
    ] {
        std::os::unix::fs::symlink(target, workspace.path().join(name)).expect("link");
    }
    let resumed = client.resume(session.state()).await.expect("resume");
    resumed.start().await.expect("start restored");
    for name in ["internal-link", "external-link", "dangling-link"] {
        assert!(
            workspace.path().join(name).symlink_metadata().is_err(),
            "{name}"
        );
    }
    assert_eq!(
        std::fs::read(&protected).expect("outside retained"),
        b"untouched"
    );
    assert_eq!(
        resumed.read("notes".into(), None).await.expect("restored"),
        b"stored"
    );
}
