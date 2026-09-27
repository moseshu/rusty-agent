//! `ra-sandbox::unix_local`: unpacking an archive into a real workspace.
//!
//! The rules are covered against a recording session; this is the part that only a real filesystem
//! can answer — that the members land where the workspace says they land, and that a member whose
//! path leaves the workspace is refused by the same path checks an ordinary write gets.

use std::path::Path;

use ra_core::sandbox::{CreateRequest, Manifest, SandboxClient};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// A manifest rooted at a directory the caller owns.
fn manifest_at(root: &Path) -> Manifest {
    Manifest::new().with_root(root.to_string_lossy().into_owned())
}

/// A tar holding a directory and two files.
fn bundle() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    builder
        .append_data(&mut header, "src", std::io::empty())
        .expect("directory");
    let mut file = tar::Header::new_gnu();
    file.set_size(12);
    file.set_mode(0o644);
    builder
        .append_data(&mut file, "src/main.rs", &b"fn main() {}"[..])
        .expect("file");
    let mut notes = tar::Header::new_gnu();
    notes.set_size(8);
    notes.set_mode(0o644);
    builder
        .append_data(&mut notes, "README.md", &b"# bundle"[..])
        .expect("file");
    builder.into_inner().expect("archive")
}

/// A tar whose only member climbs out of wherever it is unpacked.
fn escaping() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let body = b"stolen";
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    let raw = header.as_gnu_mut().expect("a gnu header");
    let name = "../escape.txt";
    raw.name[..name.len()].copy_from_slice(name.as_bytes());
    header.set_cksum();
    builder.append(&header, &body[..]).expect("member");
    builder.into_inner().expect("archive")
}

#[tokio::test]
async fn an_archive_unpacks_beside_itself_in_the_workspace() {
    let workspace = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");

    session
        .extract("incoming/bundle.tar", bundle(), None, None)
        .await
        .expect("extract");

    let incoming = workspace.path().join("incoming");
    // The archive is written where it was asked for, and its members land in the directory holding
    // it rather than at the workspace root.
    assert!(incoming.join("bundle.tar").is_file());
    assert_eq!(
        std::fs::read(incoming.join("src/main.rs")).expect("read"),
        b"fn main() {}".to_vec()
    );
    assert_eq!(
        std::fs::read(incoming.join("README.md")).expect("read"),
        b"# bundle".to_vec()
    );
    assert!(incoming.join("src").is_dir());
}

#[tokio::test]
async fn a_zip_archive_unpacks_into_a_real_workspace() {
    let workspace = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("nested/hello.txt", zip::write::SimpleFileOptions::default())
        .expect("member");
    std::io::Write::write_all(&mut writer, b"hello from zip").expect("body");
    let archive = writer.finish().expect("archive").into_inner();

    session
        .extract("incoming/bundle.zip", archive, None, None)
        .await
        .expect("extract");
    assert_eq!(
        std::fs::read(workspace.path().join("incoming/nested/hello.txt")).expect("read"),
        b"hello from zip"
    );
}

#[tokio::test]
async fn a_member_that_climbs_out_of_the_workspace_is_refused() {
    let outside = tempfile::tempdir().expect("temp");
    let workspace = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");

    let error = session
        .extract("bundle.tar", escaping(), None, None)
        .await
        .expect_err("refused");

    assert!(error.message().contains("archive"), "{error}");
    // Refused by reading the member's name, before anything was written through it.
    assert!(!outside.path().join("escape.txt").exists());
    assert!(
        !workspace
            .path()
            .parent()
            .expect("a parent")
            .join("escape.txt")
            .exists()
    );
    // The archive itself is there, which is what the caller asked for.
    assert!(workspace.path().join("bundle.tar").is_file());
}

#[tokio::test]
async fn unpacking_the_same_archive_twice_leaves_the_same_workspace() {
    // Archives arrive more than once in practice — a retried download, a resumed run — and the
    // second unpacking overwrites rather than refusing.
    let workspace = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");

    for _ in 0..2 {
        session
            .extract("bundle.tar", bundle(), None, None)
            .await
            .expect("extract");
    }

    assert_eq!(
        std::fs::read(workspace.path().join("src/main.rs")).expect("read"),
        b"fn main() {}".to_vec()
    );
}

/// A PAX member whose effective size differs from its raw header.
fn pax_bundle() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_pax_extensions([("size", &b"5"[..])])
        .expect("pax size");
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_path("payload").expect("path");
    header.set_cksum();
    builder.append(&header, &b"hello"[..]).expect("payload");
    builder.into_inner().expect("archive")
}

#[tokio::test]
async fn pax_size_controls_content_and_extraction_limits() {
    use ra_core::sandbox::SandboxArchiveLimits;
    let workspace = tempfile::tempdir().expect("workspace");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    let limit = SandboxArchiveLimits::new()
        .with_max_extracted_bytes(Some(4))
        .expect("limit");
    let error = session
        .extract("rejected.tar", pax_bundle(), None, Some(limit))
        .await
        .expect_err("effective size exceeds limit");
    assert_eq!(error.context().get("actual"), Some(&serde_json::json!(5)));
    assert!(!workspace.path().join("payload").exists());
    let limit = limit
        .with_max_extracted_bytes(Some(5))
        .expect("exact limit");
    session
        .extract("accepted.tar", pax_bundle(), None, Some(limit))
        .await
        .expect("extract");
    assert_eq!(
        std::fs::read(workspace.path().join("payload")).expect("read"),
        b"hello"
    );
}

#[tokio::test]
async fn archive_leaf_link_uses_target_parent_and_original_format() {
    let workspace = tempfile::tempdir().expect("workspace");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    std::fs::create_dir(workspace.path().join("actual")).expect("directory");
    std::os::unix::fs::symlink("actual/bundle.data", workspace.path().join("alias.tar"))
        .expect("archive link");
    session
        .extract("alias.tar", bundle(), None, None)
        .await
        .expect("extract");
    assert!(workspace.path().join("actual/bundle.data").is_file());
    assert_eq!(
        std::fs::read(workspace.path().join("actual/README.md")).expect("member"),
        b"# bundle"
    );
    assert!(!workspace.path().join("README.md").exists());
}

#[tokio::test]
async fn reused_extractor_refreshes_listings_after_success_and_failure() {
    use ra_sandbox::archive::WorkspaceArchiveExtractor;
    let workspace = tempfile::tempdir().expect("workspace");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    let mut extractor = WorkspaceArchiveExtractor::new(session.as_ref());
    extractor
        .extract("first.tar", bundle(), None, None)
        .await
        .expect("extract");
    std::fs::rename(
        workspace.path().join("src"),
        workspace.path().join("target"),
    )
    .expect("move");
    std::fs::write(workspace.path().join("target/main.rs"), b"keep target").expect("target");
    std::os::unix::fs::symlink("target", workspace.path().join("src")).expect("link");
    let error = extractor
        .extract("second.tar", bundle(), None, None)
        .await
        .expect_err("new link");
    assert!(
        error
            .context()
            .get("reason")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("symlink"))
    );
    assert_eq!(
        std::fs::read(workspace.path().join("target/main.rs")).expect("target retained"),
        b"keep target"
    );
    std::fs::remove_file(workspace.path().join("src")).expect("unlink");
    std::fs::create_dir(workspace.path().join("src")).expect("directory");
    extractor
        .extract("third.tar", bundle(), None, None)
        .await
        .expect("refresh after failure");
    assert_eq!(
        std::fs::read(workspace.path().join("src/main.rs")).expect("member"),
        b"fn main() {}"
    );
}

#[tokio::test]
async fn an_archive_path_outside_the_workspace_is_refused_before_anything_is_written() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = UnixLocalSandboxClient::new()
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");

    let error = session
        .extract("/tmp/bundle.tar", bundle(), None, None)
        .await
        .expect_err("an absolute archive path");

    assert_eq!(
        error.error_code(),
        ra_core::sandbox::ErrorCode::InvalidManifestPath
    );
    assert!(
        error.message().contains("must be relative"),
        "{}",
        error.message()
    );
}

/// A limit on members only, the others off.
fn members_only(max_members: Option<usize>) -> ra_core::sandbox::SandboxArchiveLimits {
    ra_core::sandbox::SandboxArchiveLimits::new()
        .with_max_input_bytes(None)
        .and_then(|limits| limits.with_max_extracted_bytes(None))
        .and_then(|limits| limits.with_max_members(max_members))
        .expect("limits")
}

/// `test_extract_uses_session_default_archive_limits`,
/// `test_extract_archive_limits_per_call_override_session_default` and
/// `test_extract_archive_limits_object_with_all_none_overrides_session_default`: the session's limits
/// apply to a call that names none, and a call that names its own — even ones that limit nothing —
/// replaces them rather than being combined with them.
#[tokio::test]
async fn the_sessions_limits_apply_unless_the_call_names_its_own() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = UnixLocalSandboxClient::new()
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.set_archive_limits(Some(members_only(Some(1))));
    session.start().await.expect("start");

    let error = session
        .extract("default/bundle.tar", bundle(), None, None)
        .await
        .expect_err("the session allows one member");
    let context = |key: &str| error.context().get(key).cloned();
    assert_eq!(
        context("reason"),
        Some(serde_json::json!("archive member count exceeds limit"))
    );
    assert_eq!(context("limit"), Some(serde_json::json!(1)));
    assert_eq!(context("actual"), Some(serde_json::json!(2)));

    session
        .extract(
            "wider/bundle.tar",
            bundle(),
            None,
            Some(members_only(Some(3))),
        )
        .await
        .expect("the call allows three");
    session
        .extract(
            "unlimited/bundle.tar",
            bundle(),
            None,
            Some(members_only(None)),
        )
        .await
        .expect("the call allows any number");
    for directory in ["wider", "unlimited"] {
        assert_eq!(
            std::fs::read(workspace.path().join(directory).join("README.md")).expect("read"),
            b"# bundle"
        );
    }
}
