//! `ra-sandbox::unix_local`: moving a workspace out as a tar, and back in.
//!
//! An archive is how a workspace travels, which means extraction reads input that decides where its
//! own bytes land. Most of what is tested here is refusal: a member named `../..`, a symlink aimed
//! at `/etc`, a link left lying in the workspace for the extractor to write through. Each one is a
//! way of turning "extract into this directory" into "write anywhere", and each one is checked
//! before the first byte is written so a refusal leaves the workspace as it was.

use std::path::PathBuf;

use ra_core::sandbox::{
    CreateRequest, Entry, ErrorCode, Manifest, PosixPath, SandboxClient, SandboxSession,
    SessionPath,
};
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

/// The refusal's reason.
fn reason(error: &ra_core::sandbox::SandboxError) -> &str {
    error
        .context()
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no reason in {:?}", error.context()))
}

/// The member a refusal names.
fn member(error: &ra_core::sandbox::SandboxError) -> &str {
    error
        .context()
        .get("member")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no member in {:?}", error.context()))
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
async fn what_the_session_excluded_at_runtime_is_not_written_out() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::create_dir_all(root.join(".sandbox-rclone-config/session")).expect("config");
    std::fs::write(
        root.join(".sandbox-rclone-config/session/remote.conf"),
        b"[remote]\nsecret_access_key = sk\n",
    )
    .expect("config file");
    std::fs::write(root.join("keep.txt"), b"durable").expect("keep");
    session
        .register_persist_workspace_skip_path(".sandbox-rclone-config".into())
        .expect("inside the workspace");

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

    // Generated mount configuration carries credentials and is rebuilt on every mount; a snapshot
    // that kept it would be storing secrets nobody asked it to.
    assert!(names.iter().any(|name| name == "keep.txt"), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with(".sandbox-rclone-config")),
        "{names:?}"
    );
}

/// A skip path registered as a path keeps its backslash, and so does the name the directory
/// reports, as the reference's `should_skip_tar_member` reads both through `Path(...).parts`: the
/// file is left out, and a sibling whose name also holds a backslash is kept under that name.
#[tokio::test]
async fn a_skip_path_with_a_backslash_in_a_name_leaves_that_file_out() {
    let (_temp, _root, session) = fixture(Manifest::new()).await;
    let skipped = PosixPath::new("logs\\events.jsonl");
    let kept = PosixPath::new("logs\\kept.jsonl");
    for path in [&skipped, &kept] {
        session
            .write(SessionPath::Posix(path), b"line\n".to_vec(), None)
            .await
            .expect("write");
    }
    session
        .register_persist_workspace_skip_path(SessionPath::Posix(&skipped))
        .expect("inside the workspace");

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

    assert!(
        !names.iter().any(|name| name.contains("events.jsonl")),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| name.trim_start_matches("./") == "logs\\kept.jsonl"),
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
    assert_eq!(reason(&error), "parent traversal");
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
    assert_eq!(reason(&error), "absolute path");
}

#[tokio::test]
async fn a_symlink_member_aimed_out_of_the_workspace_is_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| {
        link_member(builder, "leak", "/etc/passwd");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a link out of the workspace");
    assert_eq!(
        reason(&error),
        "absolute symlink target not allowed: /etc/passwd"
    );
    assert_eq!(member(&error), "leak");
    assert!(root.join("leak").symlink_metadata().is_err());
}

#[tokio::test]
async fn a_relative_symlink_member_that_climbs_above_the_archive_is_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    // `sub/../..` returns to the root and one step further; `sub/..` alone would be fine.
    let archive = archive_with(|builder| {
        link_member(builder, "sub/inside", "../other");
        link_member(builder, "sub/leak", "../../outside");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a relative link that leaves the archive");
    assert_eq!(
        reason(&error),
        "symlink target escapes archive root: ../../outside"
    );
    assert_eq!(member(&error), "sub/leak");
    assert!(
        !root.join("sub").exists(),
        "refused before anything was written"
    );
}

#[tokio::test]
async fn a_member_written_through_its_own_symlink_is_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let outside = tempfile::tempdir().expect("outside");
    let outside_root = std::fs::canonicalize(outside.path()).expect("canonical");

    // The classic pair: a link that is itself inside the workspace, followed by a member underneath
    // it. Each member's path looks contained; together they write wherever the link points. The
    // link aims somewhere legal, so it is the member under it that is refused.
    std::os::unix::fs::symlink(&outside_root, root.join("real")).expect("an existing link");
    let archive = archive_with(|builder| {
        link_member(builder, "./bridge", "real");
        file_member(builder, "./bridge/planted.txt", b"landed outside");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member under an archive symlink");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        reason(&error),
        "archive path descends through symlink: bridge"
    );
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
    // Caught by where the parent resolves to, before the walk looks for links, and reported with
    // the destination it would have been written to.
    assert_eq!(reason(&error), "path escapes root after resolution");
    assert_eq!(
        member(&error),
        root.join("bridge/planted.txt").to_string_lossy()
    );
    assert!(!outside_root.join("planted.txt").exists());
}

#[tokio::test]
async fn a_member_written_through_a_link_that_stays_inside_is_still_refused() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::create_dir(root.join("real")).expect("real");
    std::os::unix::fs::symlink("real", root.join("bridge")).expect("link");

    let archive = archive_with(|builder| {
        file_member(builder, "./bridge/planted.txt", b"through a link");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a member through a link");
    assert_eq!(reason(&error), "symlink in parent path");
    assert_eq!(member(&error), "bridge/planted.txt");
    assert!(!root.join("real/planted.txt").exists());
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
    assert_eq!(reason(&error), "hardlink member not allowed");
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

#[tokio::test]
async fn a_path_that_appears_twice_is_refused_unless_both_are_directories() {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let twice = archive_with(|builder| {
        file_member(builder, "notes.md", b"first");
        file_member(builder, "notes.md", b"second");
    });
    let error = session
        .hydrate_workspace(twice)
        .await
        .expect_err("the second copy would silently win");
    assert_eq!(reason(&error), "duplicate archive path: notes.md");
    assert!(!root.join("notes.md").exists());

    let directories = archive_with(|builder| {
        root_member(builder, "src/", tar::EntryType::Directory);
        root_member(builder, "src/", tar::EntryType::Directory);
        file_member(builder, "src/main.rs", b"fn main() {}");
    });
    session
        .hydrate_workspace(directories)
        .await
        .expect("a directory named twice is still one directory");
    assert!(root.join("src/main.rs").is_file());
}

#[tokio::test]
async fn a_member_beneath_a_file_member_is_refused() {
    let (_temp, _root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| {
        file_member(builder, "data", b"a file");
        file_member(builder, "data/inner.txt", b"under a file");
    });

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a path through a file");
    assert_eq!(
        reason(&error),
        "archive path descends through non-directory: data"
    );
    assert_eq!(member(&error), "data/inner.txt");
}

#[rstest::rstest]
#[case("C:secret.txt", "windows drive path")]
#[case("dir\\secret.txt", "windows path separator")]
#[tokio::test]
async fn a_member_named_in_windows_syntax_is_refused(#[case] name: &str, #[case] expected: &str) {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| file_member(builder, name, b"x"));

    let error = session
        .hydrate_workspace(archive)
        .await
        .expect_err("a name another host would read differently");
    assert_eq!(reason(&error), expected);
    assert_eq!(std::fs::read_dir(&root).expect("list").count(), 0);
}

#[rstest::rstest]
#[case::a_file(false)]
#[case::a_link(true)]
#[tokio::test]
async fn a_directory_member_replaces_what_is_at_its_path(#[case] link: bool) {
    let (_temp, root, session) = fixture(Manifest::new()).await;
    std::fs::create_dir(root.join("elsewhere")).expect("elsewhere");
    std::fs::write(root.join("elsewhere/keep"), b"kept").expect("keep");
    if link {
        std::os::unix::fs::symlink("elsewhere", root.join("data")).expect("link");
    } else {
        std::fs::write(root.join("data"), b"a file").expect("file");
    }
    let archive = archive_with(|builder| {
        root_member(builder, "data/", tar::EntryType::Directory);
        file_member(builder, "data/inner.txt", b"restored");
    });

    session.hydrate_workspace(archive).await.expect("hydrate");

    // The link itself is replaced, not followed: what it pointed at is left as it was.
    assert!(root.join("data").is_dir() && !root.join("data").is_symlink());
    assert_eq!(
        std::fs::read(root.join("data/inner.txt")).expect("inner"),
        b"restored"
    );
    assert_eq!(
        std::fs::read(root.join("elsewhere/keep")).expect("kept"),
        b"kept"
    );
    assert!(!root.join("elsewhere/inner.txt").exists());
}

/// A two-file tar split in the middle and compressed as two separate streams.
fn two_streams(compress: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let archive = archive_with(|builder| {
        file_member(builder, "a.txt", &[b'a'; 3000]);
        file_member(builder, "b.txt", &[b'b'; 3000]);
    });
    let half = archive.len() / 2;
    let mut joined = compress(&archive[..half]);
    joined.extend(compress(&archive[half..]));
    joined
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("gzip");
    encoder.finish().expect("gzip")
}

fn bzip2(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(data).expect("bzip2");
    encoder.finish().expect("bzip2")
}

fn xz(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).expect("xz");
    encoder.write_all(data).expect("xz");
    encoder.finish().expect("xz")
}

#[rstest::rstest]
#[case::gzip(gzip as fn(&[u8]) -> Vec<u8>)]
#[case::bzip2(bzip2 as fn(&[u8]) -> Vec<u8>)]
#[case::xz(xz as fn(&[u8]) -> Vec<u8>)]
#[tokio::test]
async fn a_compressed_workspace_is_read_through_every_stream(
    #[case] compress: fn(&[u8]) -> Vec<u8>,
) {
    let (_temp, root, session) = fixture(Manifest::new()).await;

    // The reference opens a restored workspace with `r:*`, whose readers continue into the next
    // gzip member, bzip2 stream or xz stream; a tar that only completes in the second one is whole.
    session
        .hydrate_workspace(two_streams(compress))
        .await
        .expect("hydrate");

    assert_eq!(std::fs::read(root.join("a.txt")).expect("a"), [b'a'; 3000]);
    assert_eq!(std::fs::read(root.join("b.txt")).expect("b"), [b'b'; 3000]);
}

#[tokio::test]
async fn a_restore_finishes_within_the_poll_that_starts_it() {
    use std::future::Future;

    let (_temp, root, session) = fixture(Manifest::new()).await;
    let archive = archive_with(|builder| file_member(builder, "notes.md", b"restored"));

    // The reference runs extraction on a worker thread and keeps awaiting it even when cancelled,
    // because a cancelled caller that returned early would release a stream the worker is still
    // reading and let resume clear a workspace the worker is still writing. Here the extraction runs
    // inside the future's own poll, so there is no point at which the caller can be gone while it
    // is still writing — which this pins down: moving it to a background thread without keeping
    // that guarantee would make this test fail.
    let mut restore = std::pin::pin!(session.hydrate_workspace(archive));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(matches!(
        restore.as_mut().poll(&mut context),
        std::task::Poll::Ready(Ok(()))
    ));
    assert_eq!(
        std::fs::read(root.join("notes.md")).expect("notes"),
        b"restored"
    );
}

#[rstest::rstest]
#[case::s3(ra_core::sandbox::MountProvider::S3(ra_core::sandbox::S3Mount {
    bucket: "bucket".to_owned(),
    ..ra_core::sandbox::S3Mount::default()
}))]
#[case::gcs(ra_core::sandbox::MountProvider::Gcs(ra_core::sandbox::GcsMount {
    bucket: "bucket".to_owned(),
    ..ra_core::sandbox::GcsMount::default()
}))]
#[tokio::test]
async fn a_mount_is_left_out_of_the_archive_where_it_is_declared_and_where_it_is_attached(
    #[case] provider: ra_core::sandbox::MountProvider,
) {
    use ra_core::sandbox::{MountPattern, MountStrategy, MountpointOptions};

    let mount = ra_core::sandbox::Mount::new(
        provider,
        MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
    )
    .expect("supported")
    .at("actual");
    let manifest = Manifest::new().with_entry("logical", Entry::mount(mount));
    let (_temp, root, session) = fixture(manifest).await;
    std::fs::create_dir_all(root.join("logical")).expect("logical");
    std::fs::write(root.join("logical/marker.txt"), b"logical").expect("marker");
    std::fs::create_dir_all(root.join("actual")).expect("actual");
    std::fs::write(root.join("actual/remote.txt"), b"remote").expect("remote");
    std::fs::write(root.join("keep.txt"), b"keep").expect("keep");

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

    // What lives behind a mount belongs to the remote it mounts, and the declared path is an
    // ephemeral entry; neither is workspace content to be stored.
    assert!(names.iter().any(|name| name == "keep.txt"), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("logical") || name.starts_with("actual")),
        "{names:?}"
    );
}

/// Writes PAX metadata using the same encoding for global and per-member records.
fn pax_member(builder: &mut tar::Builder<Vec<u8>>, global: bool, fields: &[(&str, &[u8])]) {
    let mut encoded = tar::Builder::new(Vec::new());
    encoded
        .append_pax_extensions(fields.iter().copied())
        .expect("pax");
    let encoded = encoded.into_inner().expect("pax bytes");
    let mut header = tar::Header::new_ustar();
    header.as_mut_bytes().copy_from_slice(&encoded[..512]);
    if global {
        header.set_entry_type(tar::EntryType::XGlobalHeader);
        header.set_cksum();
    }
    let length = usize::try_from(header.size().expect("size")).expect("size fits");
    builder
        .append(&header, &encoded[512..512 + length])
        .expect("pax member");
}

#[tokio::test]
async fn global_pax_paths_are_inherited_and_local_overrides_do_not_leak() {
    let data = archive_with(|builder| {
        pax_member(builder, true, &[("path", b"inherited.txt")]);
        pax_member(builder, false, &[("path", b"local.txt")]);
        file_member(builder, "unused-one", b"local");
        file_member(builder, "unused-two", b"inherited");
        pax_member(builder, true, &[("path", b"updated.txt")]);
        file_member(builder, "unused-three", b"updated");
    });
    let (_temp, root, session) = fixture(Manifest::new()).await;
    session.hydrate_workspace(data).await.expect("hydrate");
    for (name, body) in [
        ("local.txt", b"local".as_slice()),
        ("inherited.txt", b"inherited"),
        ("updated.txt", b"updated"),
    ] {
        assert_eq!(std::fs::read(root.join(name)).expect("file"), body);
    }
    assert_eq!(std::fs::read_dir(root).expect("entries").count(), 3);
}

#[tokio::test]
async fn global_pax_link_targets_are_applied_and_validated() {
    for (target, accepted) in [("target.txt", true), ("../outside", false)] {
        let data = archive_with(|builder| {
            file_member(builder, "target.txt", b"target");
            pax_member(builder, true, &[("linkpath", target.as_bytes())]);
            link_member(builder, "link", "ignored");
        });
        let (_temp, root, session) = fixture(Manifest::new()).await;
        let result = session.hydrate_workspace(data).await;
        if accepted {
            result.expect("hydrate");
            assert_eq!(
                std::fs::read_link(root.join("link")).expect("link"),
                PathBuf::from(target)
            );
        } else {
            assert_eq!(
                reason(&result.expect_err("escaping link")),
                "symlink target escapes archive root: ../outside"
            );
            assert_eq!(std::fs::read_dir(root).expect("entries").count(), 0);
        }
    }
}

#[tokio::test]
async fn a_global_pax_path_cannot_bypass_archive_validation() {
    let data = archive_with(|builder| {
        pax_member(builder, true, &[("path", b"../outside")]);
        file_member(builder, "safe.txt", b"content");
    });
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let error = session
        .hydrate_workspace(data)
        .await
        .expect_err("escaping path");
    assert_eq!(reason(&error), "parent traversal");
    assert_eq!(member(&error), "../outside");
    assert_eq!(std::fs::read_dir(root).expect("entries").count(), 0);
}

#[tokio::test]
async fn pax_sizes_control_payload_boundaries_and_local_sizes_do_not_leak() {
    let data = archive_with(|builder| {
        pax_member(builder, true, &[("size", b"600")]);
        for (name, body, local) in [
            ("large-one", vec![b'a'; 600], false),
            ("small", b"small".to_vec(), true),
            ("large-two", vec![b'b'; 600], false),
        ] {
            if local {
                pax_member(builder, false, &[("size", b"5")]);
            }
            let mut header = tar::Header::new_ustar();
            header.set_path(name).expect("path");
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, body.as_slice()).expect("member");
        }
    });
    let (_temp, root, session) = fixture(Manifest::new()).await;
    session.hydrate_workspace(data).await.expect("hydrate");
    assert_eq!(
        std::fs::read(root.join("large-one")).expect("file"),
        vec![b'a'; 600]
    );
    assert_eq!(std::fs::read(root.join("small")).expect("file"), b"small");
    assert_eq!(
        std::fs::read(root.join("large-two")).expect("file"),
        vec![b'b'; 600]
    );
}

#[tokio::test]
async fn non_utf8_names_are_preserved_or_fail_without_overwriting_a_different_name() {
    use std::os::unix::ffi::OsStrExt;

    let (_temp, root, session) = fixture(Manifest::new()).await;
    let raw = std::ffi::OsStr::from_bytes(b"name_\xff");
    // Some Unix filesystems reject these bytes. Match the host's behavior without silently
    // replacing bytes, and keep a distinct valid UTF-8 name safe in either case.
    let supported = std::fs::write(root.join(raw), b"probe").is_ok();
    if supported {
        std::fs::remove_file(root.join(raw)).expect("remove probe");
    }
    let replacement = "name_\u{fffd}";
    std::fs::write(root.join(replacement), b"keep").expect("existing file");
    let data = archive_with(|builder| {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().expect("gnu").name[..6].copy_from_slice(raw.as_bytes());
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &b"data"[..]).expect("member");
    });
    let result = session.hydrate_workspace(data).await;
    if supported {
        result.expect("hydrate");
        assert_eq!(
            std::fs::read(root.join(raw)).expect("original name"),
            b"data"
        );
    } else {
        assert_eq!(
            result.expect_err("unsupported filename").error_code(),
            ErrorCode::WorkspaceArchiveWriteError
        );
    }
    assert_eq!(
        std::fs::read(root.join(replacement)).expect("unchanged file"),
        b"keep"
    );
}

#[tokio::test]
async fn non_utf8_symlink_targets_keep_their_original_bytes() {
    use std::os::unix::ffi::OsStrExt;

    let (_temp, root, session) = fixture(Manifest::new()).await;
    let target = std::ffi::OsStr::from_bytes(b"target_\xff");
    let supported = std::os::unix::fs::symlink(target, root.join("probe")).is_ok();
    if supported {
        std::fs::remove_file(root.join("probe")).expect("remove probe");
    }
    let data = archive_with(|builder| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "link", target)
            .expect("link");
    });
    let result = session.hydrate_workspace(data).await;
    if supported {
        result.expect("hydrate");
        assert_eq!(
            std::fs::read_link(root.join("link"))
                .expect("target")
                .as_os_str()
                .as_bytes(),
            target.as_bytes()
        );
    } else {
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn gnu_long_paths_and_link_targets_survive_the_metadata_reader() {
    let name = "n".repeat(160);
    let data = archive_with(|builder| {
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, &name, &b"data"[..])
            .expect("long name");
        link_member(builder, "link", &name);
        file_member(builder, "following", b"next");
    });
    let (_temp, root, session) = fixture(Manifest::new()).await;
    session.hydrate_workspace(data).await.expect("hydrate");
    assert_eq!(
        std::fs::read(root.join("link")).expect("linked file"),
        b"data"
    );
    assert_eq!(
        std::fs::read(root.join("following")).expect("following file"),
        b"next"
    );
}

#[tokio::test]
async fn gnu_sparse_payloads_leave_the_reader_at_the_next_header() {
    let data = archive_with(|builder| {
        let mut header = tar::Header::new_gnu();
        header.set_path("sparse").expect("path");
        header.set_entry_type(tar::EntryType::GNUSparse);
        header.set_mode(0o644);
        header.set_size(4);
        let gnu = header.as_gnu_mut().expect("gnu");
        gnu.set_real_size(1028);
        gnu.sparse[0].set_offset(1024);
        gnu.sparse[0].set_length(4);
        header.set_cksum();
        builder
            .append(&header, &b"data"[..])
            .expect("sparse member");
        file_member(builder, "following", b"next");
    });
    let (_temp, root, session) = fixture(Manifest::new()).await;
    session.hydrate_workspace(data).await.expect("hydrate");
    let mut expected = vec![0; 1024];
    expected.extend_from_slice(b"data");
    assert_eq!(
        std::fs::read(root.join("sparse")).expect("sparse"),
        expected
    );
    assert_eq!(
        std::fs::read(root.join("following")).expect("next"),
        b"next"
    );
}

#[rstest::rstest]
#[case::local(false)]
#[case::global(true)]
#[tokio::test]
async fn a_pax_header_without_a_following_member_is_not_a_complete_archive(#[case] global: bool) {
    let data = archive_with(|builder| {
        pax_member(builder, global, &[("path", b"missing.txt")]);
    });
    let (_temp, root, session) = fixture(Manifest::new()).await;
    let error = session
        .hydrate_workspace(data)
        .await
        .expect_err("missing member");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(std::fs::read_dir(root).expect("entries").count(), 0);
}
