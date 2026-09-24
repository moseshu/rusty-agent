//! `ra-sandbox::unix_local`: the workspace as a set of files.
//!
//! These are the operations a tool reaches for, so what matters is not only that they work but that
//! they refuse the same things the path policy refuses — a file API that quietly bypassed the
//! workspace boundary would make every other check decorative.

use std::path::PathBuf;

use ra_core::sandbox::{
    CreateRequest, EntryKind, ErrorCode, Manifest, SandboxClient, SandboxPathGrant, SandboxSession,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// A started session over a symlink-free workspace, plus that workspace's path.
async fn fixture(
    grants: Vec<SandboxPathGrant>,
) -> (tempfile::TempDir, PathBuf, Box<dyn SandboxSession>) {
    let directory = tempfile::tempdir().expect("temp");
    let root = std::fs::canonicalize(directory.path()).expect("canonical");
    let mut manifest = Manifest::new().with_root(root.to_string_lossy().into_owned());
    for grant in grants {
        manifest = manifest.with_path_grant(grant);
    }
    let session = UnixLocalSandboxClient::new()
        .create(CreateRequest::new().with_manifest(manifest))
        .await
        .expect("create");
    session.start().await.expect("start");
    (directory, root, session)
}

#[tokio::test]
async fn a_file_written_through_the_session_is_read_back_through_it() {
    let (_temp, root, session) = fixture(Vec::new()).await;

    session
        .write("notes/today.md", b"first line".to_vec(), None)
        .await
        .expect("write");

    // Parent directories are created rather than required: a caller writing a file should not have
    // to walk the tree first.
    assert!(root.join("notes").is_dir());
    assert_eq!(
        session.read("notes/today.md", None).await.expect("read"),
        b"first line"
    );
}

#[tokio::test]
async fn reading_a_file_that_is_not_there_says_so() {
    let (_temp, _root, session) = fixture(Vec::new()).await;
    let error = session
        .read("missing.md", None)
        .await
        .expect_err("a file nobody wrote");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
    // The path the caller named, not the host directory the provider picked.
    assert_eq!(
        error
            .context()
            .get("path")
            .and_then(serde_json::Value::as_str),
        Some("missing.md")
    );
}

#[tokio::test]
async fn a_listing_reports_what_each_entry_is() {
    let (_temp, root, session) = fixture(Vec::new()).await;
    std::fs::write(root.join("file.txt"), b"1234").expect("file");
    std::fs::create_dir(root.join("directory")).expect("directory");
    std::os::unix::fs::symlink(root.join("file.txt"), root.join("link")).expect("link");

    let mut listed = session.ls(".", None).await.expect("ls");
    listed.sort_by(|left, right| left.path.cmp(&right.path));
    let described: Vec<(String, EntryKind, u64)> = listed
        .into_iter()
        .map(|entry| {
            (
                PathBuf::from(&entry.path)
                    .file_name()
                    .expect("a name")
                    .to_string_lossy()
                    .into_owned(),
                entry.kind,
                entry.size,
            )
        })
        .collect();

    // A symlink is reported as a symlink rather than as whatever it points at: a caller deciding
    // what to do with an entry needs to know it is a link.
    assert_eq!(
        described,
        vec![
            ("directory".to_owned(), EntryKind::Directory, described[0].2),
            ("file.txt".to_owned(), EntryKind::File, 4),
            ("link".to_owned(), EntryKind::Symlink, described[2].2),
        ]
    );
}

#[tokio::test]
async fn a_directory_is_created_with_its_parents_only_when_asked() {
    let (_temp, root, session) = fixture(Vec::new()).await;

    session
        .mkdir("deep/nested", false, None)
        .await
        .expect_err("a directory whose parent is not there");
    session
        .mkdir("deep/nested", true, None)
        .await
        .expect("with parents");
    assert!(root.join("deep/nested").is_dir());

    // Creating one that already exists is success, so materializing the same manifest twice does
    // not fail the second time.
    session
        .mkdir("deep/nested", false, None)
        .await
        .expect("an existing directory");
}

#[tokio::test]
async fn removing_something_that_is_not_there_depends_on_whether_it_was_a_sweep() {
    let (_temp, _root, session) = fixture(Vec::new()).await;

    let error = session
        .rm("missing.txt", false, None)
        .await
        .expect_err("a named file that is not there");
    assert_eq!(error.error_code(), ErrorCode::ExecNonzero);

    // A recursive removal asked for the path to be gone, and it is.
    session
        .rm("missing.txt", true, None)
        .await
        .expect("a sweep of nothing");
}

#[tokio::test]
async fn a_directory_needs_a_sweep_and_a_link_is_followed_to_what_it_names() {
    let (_temp, root, session) = fixture(Vec::new()).await;
    std::fs::create_dir_all(root.join("tree/inner")).expect("tree");
    std::fs::write(root.join("tree/inner/file.txt"), b"x").expect("file");
    std::fs::write(root.join("target.txt"), b"delete me").expect("target");
    std::os::unix::fs::symlink(root.join("target.txt"), root.join("alias")).expect("link");

    session
        .rm("tree", false, None)
        .await
        .expect_err("a directory with contents");
    session.rm("tree", true, None).await.expect("a sweep");
    assert!(!root.join("tree").exists());

    // A local path is resolved all the way to its leaf before anything happens to it, so removing
    // a link removes what the link names and leaves the link dangling. This is worth stating
    // outright because the session protocol's remote form deliberately does the opposite: there the
    // resolved path is only checked for containment and the operation still lands on the link.
    session.rm("alias", false, None).await.expect("the link");
    assert!(!root.join("target.txt").exists());
    assert!(root.join("alias").symlink_metadata().is_ok());
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_by_every_operation() {
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.txt"), b"classified").expect("secret");
    let target = outside
        .path()
        .join("secret.txt")
        .to_string_lossy()
        .into_owned();
    let (_temp, _root, session) = fixture(Vec::new()).await;

    for error in [
        session.read(&target, None).await.err(),
        session.write(&target, b"x".to_vec(), None).await.err(),
        session.rm(&target, false, None).await.err(),
        session.mkdir(&target, false, None).await.err(),
        session.ls(&target, None).await.err(),
    ] {
        assert_eq!(
            error.expect("a path outside the workspace").error_code(),
            ErrorCode::InvalidManifestPath
        );
    }
    assert_eq!(
        std::fs::read(outside.path().join("secret.txt")).expect("secret"),
        b"classified"
    );
}

#[tokio::test]
async fn a_read_only_grant_can_be_read_and_not_written() {
    let granted = tempfile::tempdir().expect("granted");
    let granted_root = std::fs::canonicalize(granted.path()).expect("canonical");
    std::fs::write(granted_root.join("shared.txt"), b"reference").expect("shared");
    let grant = SandboxPathGrant::new(&granted_root.to_string_lossy())
        .expect("grant")
        .read_only(true);
    let (_temp, _root, session) = fixture(vec![grant]).await;

    let shared = granted_root
        .join("shared.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        session.read(&shared, None).await.expect("read"),
        b"reference"
    );

    let error = session
        .write(&shared, b"overwritten".to_vec(), None)
        .await
        .expect_err("a write through a read-only grant");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        std::fs::read(granted_root.join("shared.txt")).expect("shared"),
        b"reference"
    );
}

#[tokio::test]
async fn a_long_symlink_chain_cannot_read_or_write_outside_the_workspace() {
    let (_temp, root, session) = fixture(Vec::new()).await;
    let outside = tempfile::tempdir().expect("outside");
    let secret = outside.path().join("secret");
    std::fs::write(&secret, b"original").expect("secret");
    for index in 0..45 {
        let next = if index == 44 {
            secret.clone()
        } else {
            root.join(format!("link{}", index + 1))
        };
        std::os::unix::fs::symlink(next, root.join(format!("link{index}"))).expect("link");
    }
    for requested in [
        "link0".to_owned(),
        root.join("link0").to_string_lossy().into_owned(),
    ] {
        let read_error = session
            .read(&requested, None)
            .await
            .expect_err("external read");
        assert_eq!(read_error.error_code(), ErrorCode::InvalidManifestPath);
        let write_error = session
            .write(&requested, b"overwritten".to_vec(), None)
            .await
            .expect_err("external write");
        assert_eq!(write_error.error_code(), ErrorCode::InvalidManifestPath);
    }
    assert_eq!(
        std::fs::read(secret).expect("preserved secret"),
        b"original"
    );
}

/// The message of a refusal, asserted to be a path-policy one.
fn refused_path(error: Option<ra_core::sandbox::SandboxError>) -> String {
    let error = error.expect("refused");
    assert_eq!(
        error.error_code(),
        ErrorCode::InvalidManifestPath,
        "{error}"
    );
    error.message().to_owned()
}

#[tokio::test]
async fn a_path_that_climbs_out_is_refused_as_escaping_the_root_by_every_operation() {
    let (_temp, _root, session) = fixture(Vec::new()).await;

    for message in [
        refused_path(session.read("../secret.txt", None).await.err()),
        refused_path(
            session
                .write("../secret.txt", b"nope".to_vec(), None)
                .await
                .err(),
        ),
        refused_path(session.ls("../outside", None).await.err()),
        refused_path(session.mkdir("../outside", true, None).await.err()),
        refused_path(session.rm("../outside", false, None).await.err()),
    ] {
        assert!(message.contains("must not escape root"), "{message}");
    }
}

#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_refused_as_escaping_the_root() {
    let (_temp, root, session) = fixture(Vec::new()).await;
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), root.join("link")).expect("link");

    for message in [
        refused_path(session.mkdir("link/nested", true, None).await.err()),
        refused_path(session.ls("link", None).await.err()),
        refused_path(session.rm("link/file.txt", false, None).await.err()),
    ] {
        assert!(message.contains("must not escape root"), "{message}");
    }
    assert!(!outside.path().join("nested").exists());
}

#[tokio::test]
async fn a_writable_grant_outside_the_workspace_can_be_written_and_read_back() {
    let granted = tempfile::tempdir().expect("granted");
    let granted_root = std::fs::canonicalize(granted.path()).expect("canonical");
    let grant = SandboxPathGrant::new(&granted_root.to_string_lossy()).expect("grant");
    let (_temp, _root, session) = fixture(vec![grant]).await;
    let result = granted_root
        .join("result.txt")
        .to_string_lossy()
        .into_owned();

    session
        .write(&result, b"scratch output".to_vec(), None)
        .await
        .expect("write");

    assert_eq!(
        session.read(&result, None).await.expect("read"),
        b"scratch output"
    );
}

#[tokio::test]
async fn a_write_under_a_read_only_grant_names_the_grant_that_refused_it() {
    let granted = tempfile::tempdir().expect("granted");
    let granted_root = std::fs::canonicalize(granted.path()).expect("canonical");
    let grant = SandboxPathGrant::new(&granted_root.to_string_lossy())
        .expect("grant")
        .read_only(true);
    let (_temp, _root, session) = fixture(vec![grant]).await;
    let result = granted_root
        .join("result.txt")
        .to_string_lossy()
        .into_owned();

    let error = session
        .write(&result, b"scratch output".to_vec(), None)
        .await
        .expect_err("read-only");

    assert_eq!(
        error.message(),
        format!("failed to write archive for path: {result}")
    );
    assert_eq!(
        serde_json::to_value(error.context()).expect("context"),
        serde_json::json!({
            "path": result,
            "reason": "read_only_extra_path_grant",
            "grant_path": granted_root.to_string_lossy(),
        })
    );
}
