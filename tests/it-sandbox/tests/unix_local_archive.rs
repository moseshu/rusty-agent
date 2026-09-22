//! `ra-sandbox::unix_local`: moving a workspace out as a tar, and back in.
//!
//! An archive is how a workspace travels, which means extraction reads input that decides where its
//! own bytes land. Most of what is tested here is refusal: a member named `../..`, a symlink aimed
//! at `/etc`, a link left lying in the workspace for the extractor to write through. Each one is a
//! way of turning "extract into this directory" into "write anywhere", and each one is checked
//! before the first byte is written so a refusal leaves the workspace as it was.

use std::path::PathBuf;

use ra_core::sandbox::{CreateRequest, Entry, ErrorCode, Manifest, SandboxClient, SandboxSession};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// A session over a symlink-free workspace. Not started: these operations do not need it, and a
/// manifest carrying entries cannot be materialized by this backend yet.
async fn fixture(manifest: Manifest) -> (tempfile::TempDir, PathBuf, Box<dyn SandboxSession>) {
    let directory = tempfile::tempdir().expect("temp");
    let root = std::fs::canonicalize(directory.path()).expect("canonical");
    let manifest = manifest.with_root(root.to_string_lossy().into_owned());
    let session = UnixLocalSandboxClient::new()
        .create(CreateRequest::new().with_manifest(manifest))
        .await
        .expect("create");
    (directory, root, session)
}

/// Builds a tar carrying one member of the caller's choosing.
fn archive_with(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    build(&mut builder);
    builder.into_inner().expect("archive")
}

/// Adds a regular file member under an arbitrary name.
///
/// The name is written into the header directly rather than through the archive writer, because the
/// writer refuses to produce an absolute or climbing member path. Refusing to *write* one is a
/// sensible thing for a writer to do and no help at all here: the archives that have to be refused
/// are exactly the ones a cooperative writer will not make.
fn file_member(builder: &mut tar::Builder<Vec<u8>>, name: &str, body: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    let raw = header.as_gnu_mut().expect("a gnu header");
    raw.name[..name.len()].copy_from_slice(name.as_bytes());
    header.set_cksum();
    builder.append(&header, body).expect("member");
}

/// Adds a symlink member aimed wherever the caller likes.
fn link_member(builder: &mut tar::Builder<Vec<u8>>, name: &str, target: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, name, target)
        .expect("member");
}

#[tokio::test]
async fn a_workspace_survives_being_written_out_and_read_back_somewhere_else() {
    let (_source_temp, source_root, source) = fixture(Manifest::new()).await;
    std::fs::create_dir_all(source_root.join("src")).expect("dir");
    std::fs::write(source_root.join("src/main.rs"), b"fn main() {}").expect("file");
    std::fs::write(source_root.join("run.sh"), b"#!/bin/sh\n").expect("script");
    std::fs::set_permissions(
        source_root.join("run.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("mode");
    std::os::unix::fs::symlink("src/main.rs", source_root.join("entry")).expect("link");

    let archive = source.persist_workspace().await.expect("persist");

    let (_target_temp, target_root, target) = fixture(Manifest::new()).await;
    target.hydrate_workspace(archive).await.expect("hydrate");

    assert_eq!(
        std::fs::read(target_root.join("src/main.rs")).expect("file"),
        b"fn main() {}"
    );
    // The executable bit survives, because a script that arrives unrunnable has not survived.
    let mode: u32 = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(target_root.join("run.sh"))
            .expect("script")
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
        std::fs::read_link(target_root.join("entry")).expect("link"),
        PathBuf::from("src/main.rs")
    );
}

#[tokio::test]
async fn what_the_manifest_called_ephemeral_is_not_written_out() {
    let manifest = Manifest::new().with_entry("cache", Entry::dir().ephemeral(true));
    let (_temp, root, session) = fixture(manifest).await;
    std::fs::create_dir_all(root.join("cache/objects")).expect("cache");
    std::fs::write(root.join("cache/objects/blob"), b"rebuildable").expect("blob");
    std::fs::write(root.join("keep.txt"), b"durable").expect("keep");

    let archive = session.persist_workspace().await.expect("persist");
    let names: Vec<String> = tar::Archive::new(archive.as_slice())
        .entries()
        .expect("entries")
        .map(|entry| {
            entry
                .expect("entry")
                .path()
                .expect("path")
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // An entry declared ephemeral is rebuilt on the next start, so carrying it would only make the
    // archive bigger and the restored workspace staler.
    assert!(names.iter().any(|name| name == "keep.txt"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("cache")),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_workspace_that_is_not_there_cannot_be_written_out() {
    let (temp, _root, session) = fixture(Manifest::new()).await;
    drop(temp);

    let error = session
        .persist_workspace()
        .await
        .expect_err("a workspace that is gone");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("workspace_root_not_found")
    );
}

#[tokio::test]
async fn a_member_that_climbs_out_of_the_workspace_is_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| {
        file_member(builder, "../escaped.txt", b"landed outside");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member aimed above the workspace");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert!(!root.parent().expect("parent").join("escaped.txt").exists());
}

#[tokio::test]
async fn an_absolute_member_is_refused() {
    let (_temp, _root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| {
        file_member(builder, "/tmp/absolute.txt", b"landed outside");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member that names its own destination");
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("path_escapes_root")
    );
}

#[tokio::test]
async fn a_symlink_member_aimed_out_of_the_workspace_is_refused() {
    let (_temp, _root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| {
        link_member(builder, "./passwd", "/etc/passwd");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a link out of the workspace");
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("symlink_target_escapes_root")
    );
}

#[tokio::test]
async fn a_member_written_through_its_own_symlink_is_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let outside = tempfile::tempdir().expect("outside");
    let outside_root = std::fs::canonicalize(outside.path()).expect("canonical");

    // The classic pair: a link that is itself inside the workspace, followed by a member underneath
    // it. Each member's path looks contained; together they write wherever the link points.
    let archive = archive_with(|builder| {
        link_member(builder, "./bridge", &outside_root.to_string_lossy());
        file_member(builder, "./bridge/planted.txt", b"landed outside");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member under an archive symlink");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert!(!outside_root.join("planted.txt").exists());
    // Refused before anything was written, so the link never got created either.
    assert!(!root.join("bridge").symlink_metadata().is_ok());
}

#[tokio::test]
async fn a_member_written_through_a_symlink_already_in_the_workspace_is_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let outside = tempfile::tempdir().expect("outside");
    let outside_root = std::fs::canonicalize(outside.path()).expect("canonical");
    std::os::unix::fs::symlink(&outside_root, root.join("bridge")).expect("link");

    let archive = archive_with(|builder| {
        file_member(builder, "./bridge/planted.txt", b"landed outside");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member through a link that was already there");
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("parent_is_symlink")
    );
    assert!(!outside_root.join("planted.txt").exists());
}

#[tokio::test]
async fn a_member_that_is_neither_a_file_a_directory_nor_a_link_is_refused() {
    let (_temp, _root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o644);
        builder
            .append_link(&mut header, "./hard", "/etc/passwd")
            .expect("member");
    });

    // A hardlink names a destination the archive never had to declare, and a device or a fifo is
    // not workspace content at all.
    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member kind a workspace has no use for");
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("unsupported_member_kind")
    );
}

#[tokio::test]
async fn hydrating_replaces_what_is_at_a_path_rather_than_writing_through_it() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::write(root.join("notes.md"), b"stale").expect("stale");

    let archive = archive_with(|builder| {
        file_member(builder, "./notes.md", b"fresh");
    });
    session.hydrate_workspace(archive).await.expect("hydrate");

    assert_eq!(
        std::fs::read(root.join("notes.md")).expect("read"),
        b"fresh"
    );
}

/// Adds a root member without letting the tar writer normalize its spelling.
fn root_member(builder: &mut tar::Builder<Vec<u8>>, name: &str, kind: tar::EntryType) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(0);
    header.set_mode(0o755);
    header.as_gnu_mut().expect("gnu header").name[..name.len()].copy_from_slice(name.as_bytes());
    if kind.is_symlink() || kind.is_hard_link() {
        header.set_link_name(".").expect("link target");
    }
    header.set_cksum();
    builder
        .append(&header, std::io::empty())
        .expect("root member");
}

#[rstest::rstest]
#[case("", tar::EntryType::Regular)]
#[case(".", tar::EntryType::Regular)]
#[case("./", tar::EntryType::Regular)]
#[case("", tar::EntryType::Symlink)]
#[case(".", tar::EntryType::Symlink)]
#[case("./", tar::EntryType::Symlink)]
#[case("", tar::EntryType::Link)]
#[case(".", tar::EntryType::Link)]
#[case("./", tar::EntryType::Link)]
#[tokio::test]
async fn a_non_directory_root_member_is_rejected_before_any_write(
    #[case] name: &str,
    #[case] kind: tar::EntryType,
) {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::write(root.join("keep"), b"original").expect("keep");
    let archive = archive_with(|builder| {
        file_member(builder, "first.txt", b"must not be written");
        root_member(builder, name, kind);
    });
    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("invalid root");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    let reason = if kind.is_symlink() {
        "archive root symlink"
    } else if kind.is_hard_link() {
        "archive root hardlink"
    } else {
        "archive root member must be directory"
    };
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some(reason)
    );
    assert_eq!(
        std::fs::read(root.join("keep")).expect("preserved file"),
        b"original"
    );
    assert!(!root.join("first.txt").exists());
}

#[rstest::rstest]
#[case("")]
#[case(".")]
#[case("./")]
#[tokio::test]
async fn a_directory_root_member_is_skipped_and_its_children_are_restored(#[case] name: &str) {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::write(root.join("keep"), b"original").expect("keep");
    let archive = archive_with(|builder| {
        root_member(builder, name, tar::EntryType::Directory);
        file_member(builder, "child.txt", b"restored");
    });
    session
        .hydrate_workspace(archive)
        .await
        .expect("directory root");
    assert_eq!(
        std::fs::read(root.join("keep")).expect("preserved file"),
        b"original"
    );
    assert_eq!(
        std::fs::read(root.join("child.txt")).expect("restored file"),
        b"restored"
    );
}

#[rstest::rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
#[tokio::test]
async fn an_existing_directory_is_preserved_on_a_leaf_conflict(
    #[case] symlink: bool,
    #[case] nonempty: bool,
) {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::create_dir(root.join("data")).expect("directory");
    if nonempty {
        std::fs::write(root.join("data/keep"), b"original").expect("keep");
    }
    let archive = archive_with(|builder| {
        if symlink {
            link_member(builder, "data", "target");
        } else {
            file_member(builder, "data", b"replacement");
        }
    });
    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("directory conflict");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("destination directory already exists: data")
    );
    assert!(root.join("data").is_dir());
    assert!(!root.join("data").is_symlink());
    if nonempty {
        assert_eq!(
            std::fs::read(root.join("data/keep")).expect("preserved file"),
            b"original"
        );
    }
}

#[tokio::test]
async fn a_file_replaces_a_symlink_without_touching_the_target_directory() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::create_dir(root.join("target")).expect("target");
    std::fs::write(root.join("target/keep"), b"original").expect("keep");
    std::os::unix::fs::symlink("target", root.join("data")).expect("link");
    let archive = archive_with(|builder| file_member(builder, "data", b"replacement"));
    session
        .hydrate_workspace(archive)
        .await
        .expect("replace symlink");
    assert_eq!(
        std::fs::read(root.join("data")).expect("new file"),
        b"replacement"
    );
    assert_eq!(
        std::fs::read(root.join("target/keep")).expect("preserved target"),
        b"original"
    );
}
