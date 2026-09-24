//! `ra-sandbox::unix_local`: a manifest becoming real files in a real directory.
//!
//! The recorded tests next door check the order things happen in; these check that the workspace
//! afterwards is the one the manifest declared — the bytes, the modes, and the checksums a caller
//! compares between runs. They also check the refusals that only a real filesystem can produce: a
//! source that is a symlink, a source outside what the manifest is allowed to read, and a source
//! that is not there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ra_core::sandbox::{
    CreateRequest, Entry, ErrorCode, Group, Manifest, MaterializationResult, Mount, MountPattern,
    MountProvider, MountStrategy, MountpointOptions, S3Mount, SandboxClient,
    SandboxConcurrencyLimits, SandboxPathGrant, SandboxSession, User,
};
use ra_sandbox::materialize::ManifestApplier;
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// `sha256("hello")`, so a receipt can be checked without recomputing it here.
const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

/// A workspace directory this test owns, and a manifest rooted at it.
fn workspace() -> (tempfile::TempDir, Manifest) {
    let directory = tempfile::tempdir().expect("a workspace directory");
    // Canonicalized on purpose. A temporary directory reaches this process through symlinked
    // parents on macOS, and a manifest root that still names the link describes a different path to
    // every check that resolves one.
    let root = std::fs::canonicalize(directory.path()).expect("resolve the workspace root");
    let manifest = Manifest::new().with_root(root.to_string_lossy().into_owned());
    (directory, manifest)
}

/// A session over that workspace, made by the client that owns the backend.
async fn session_for(manifest: Manifest) -> Arc<dyn SandboxSession> {
    UnixLocalSandboxClient::new()
        .create(CreateRequest::new().with_manifest(manifest))
        .await
        .expect("create")
        .into()
}

/// A directory of sources, canonicalized for the same reason the workspace root is.
fn sources() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("a source directory");
    let base = std::fs::canonicalize(directory.path()).expect("resolve the source directory");
    (directory, base)
}

/// Reads a workspace file back as text.
fn read(root: &str, relative: &str) -> String {
    std::fs::read_to_string(Path::new(root).join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

/// The permission bits on a workspace path.
fn mode_of(root: &str, relative: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(Path::new(root).join(relative))
        .unwrap_or_else(|error| panic!("stat {relative}: {error}"))
        .permissions()
        .mode()
        & 0o7777
}

/// The paths a receipt accounted for.
fn receipted(receipt: &MaterializationResult) -> Vec<&str> {
    receipt
        .files()
        .iter()
        .map(|file| file.path().as_str())
        .collect()
}

#[tokio::test]
async fn starting_a_session_puts_the_declared_content_in_the_workspace() {
    let (_directory, manifest) = workspace();
    let manifest = manifest
        .with_entry("README.md", Entry::file("hello"))
        .with_entry(
            "src",
            Entry::dir().with_child("main.rs", Entry::file("fn main() {}")),
        );
    let root = manifest.root.clone();
    let session = session_for(manifest).await;

    session.start().await.expect("start");

    assert_eq!(read(&root, "README.md"), "hello");
    assert_eq!(read(&root, "src/main.rs"), "fn main() {}");
    // The default entry permissions: the owner may do anything, everyone else may read and traverse.
    assert_eq!(mode_of(&root, "src"), 0o755);
    assert_eq!(mode_of(&root, "README.md"), 0o755);
}

#[tokio::test]
async fn an_entry_carries_the_mode_it_declared_onto_the_disk() {
    use ra_core::sandbox::{FileMode, Permissions};

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry(
        "id_rsa",
        Entry::file("private").with_permissions(Permissions::default().owner_can(FileMode::All)),
    );
    let root = manifest.root.clone();
    let session = session_for(manifest).await;

    session.start().await.expect("start");

    assert_eq!(mode_of(&root, "id_rsa"), 0o700);
}

#[tokio::test]
async fn a_copied_host_file_arrives_with_its_bytes_and_its_checksum() {
    let (_sources, base) = sources();
    std::fs::write(base.join("notes.txt"), "hello").expect("write the source");

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("docs/notes.txt", Entry::local_file("notes.txt"));
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    let receipt = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(read(&root, "docs/notes.txt"), "hello");
    assert_eq!(receipt.files().len(), 1);
    assert_eq!(receipt.files()[0].sha256(), HELLO_SHA256);
    assert_eq!(
        receipt.files()[0].path().as_str(),
        format!("{root}/docs/notes.txt")
    );
}

#[tokio::test]
async fn a_copied_host_directory_arrives_whole_and_accounts_for_every_file() {
    let (_sources, base) = sources();
    std::fs::create_dir_all(base.join("assets/img")).expect("make the source tree");
    std::fs::write(base.join("assets/index.html"), "hello").expect("write");
    std::fs::write(base.join("assets/img/logo.svg"), "<svg/>").expect("write");

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("public", Entry::local_dir(Some("assets".to_owned())));
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    let receipt = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(read(&root, "public/index.html"), "hello");
    assert_eq!(read(&root, "public/img/logo.svg"), "<svg/>");
    assert_eq!(
        receipted(&receipt),
        vec![
            format!("{root}/public/img/logo.svg").as_str(),
            format!("{root}/public/index.html").as_str(),
        ]
    );
}

#[tokio::test]
async fn a_copied_directory_with_no_source_is_just_a_directory() {
    let (_sources, base) = sources();
    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("build", Entry::local_dir(None));
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    let receipt = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert!(Path::new(&root).join("build").is_dir());
    assert!(receipt.is_empty());
}

#[tokio::test]
async fn a_symlink_inside_a_copied_directory_is_refused_rather_than_followed() {
    let (_sources, base) = sources();
    std::fs::create_dir(base.join("assets")).expect("make the source tree");
    std::fs::write(base.join("secret.txt"), "not yours").expect("write");
    std::os::unix::fs::symlink(base.join("secret.txt"), base.join("assets/link.txt"))
        .expect("link");

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("public", Entry::local_dir(Some("assets".to_owned())));
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    let error = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("a link out of the source tree is not content the manifest named");

    assert_eq!(error.error_code(), ErrorCode::LocalDirReadError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("symlink_not_supported")
    );
    assert!(
        !Path::new(&root).join("public/link.txt").exists(),
        "nothing is copied once the tree is refused"
    );
}

#[tokio::test]
async fn a_source_that_is_itself_a_symlink_is_refused() {
    let (_sources, base) = sources();
    std::fs::create_dir(base.join("real")).expect("make the source tree");
    std::fs::write(base.join("real/a.txt"), "hello").expect("write");
    std::os::unix::fs::symlink(base.join("real"), base.join("assets")).expect("link");

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("public", Entry::local_dir(Some("assets".to_owned())));
    let session = session_for(manifest.clone()).await;

    let error = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::LocalDirReadError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("symlink_not_supported")
    );
    assert_eq!(
        error
            .context()
            .get("child")
            .and_then(|value| value.as_str()),
        Some("assets"),
        "the refusal names the component that is a link"
    );
}

#[tokio::test]
async fn a_source_that_is_not_there_says_so_rather_than_failing_at_the_open() {
    let (_sources, base) = sources();
    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("public", Entry::local_dir(Some("missing".to_owned())));
    let session = session_for(manifest.clone()).await;

    let error = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::LocalDirReadError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("path_not_found")
    );
    assert_eq!(error.retryable(), Some(false));
}

#[tokio::test]
async fn a_missing_source_file_is_reported_as_a_file_failure_keeping_why_it_failed() {
    let (_sources, base) = sources();
    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("docs/notes.txt", Entry::local_file("notes.txt"));
    let session = session_for(manifest.clone()).await;

    let error = ManifestApplier::new(session.clone(), base.clone())
        .apply_manifest(&manifest, false)
        .await
        .expect_err("refused");

    // A manifest that declared one file is told about that file — but the reason the directory walk
    // gave is what distinguishes one kind of missing source from another, so it survives the
    // rewrapping.
    //
    // The reason is `path_changed_during_copy` rather than `path_not_found`, which reads oddly for
    // a file that was never there and is what the reference reports: a single file is copied as the
    // one child of its parent directory, so the walk finds the parent present and only discovers
    // the leaf is missing at the open. `path_not_found` is reserved for the source *root*.
    assert_eq!(error.error_code(), ErrorCode::LocalFileReadError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("path_changed_during_copy")
    );
    assert_eq!(
        error.context().get("src").and_then(|value| value.as_str()),
        Some(base.join("notes.txt").to_string_lossy().as_ref()),
        "the refusal names the source absolutely, not as the manifest happened to write it"
    );
}

#[tokio::test]
async fn a_source_outside_the_base_directory_is_refused_when_nothing_grants_it() {
    let (_sources, base) = sources();
    let (_elsewhere, outside) = sources();
    std::fs::write(outside.join("secret.txt"), "not yours").expect("write");

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry(
        "copied.txt",
        Entry::local_file(outside.join("secret.txt").to_string_lossy().into_owned()),
    );
    let session = session_for(manifest.clone()).await;

    let error = ManifestApplier::new(session.clone(), base.clone())
        .apply_manifest(&manifest, false)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::LocalFileReadError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("outside_base_dir")
    );
    assert_eq!(
        error
            .context()
            .get("base_dir")
            .and_then(|value| value.as_str()),
        Some(base.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn a_source_outside_the_base_directory_is_copied_when_a_grant_names_it() {
    let (_sources, base) = sources();
    let (_elsewhere, outside) = sources();
    std::fs::write(outside.join("shared.txt"), "hello").expect("write");

    let (_directory, manifest) = workspace();
    let grant = SandboxPathGrant::new(outside.to_string_lossy().as_ref()).expect("a grant");
    let manifest = manifest.with_path_grant(grant).with_entry(
        "copied.txt",
        Entry::local_file(outside.join("shared.txt").to_string_lossy().into_owned()),
    );
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    let receipt = ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(read(&root, "copied.txt"), "hello");
    assert_eq!(receipt.files()[0].sha256(), HELLO_SHA256);
}

#[tokio::test]
async fn only_the_ephemeral_entries_are_rebuilt_and_the_persisted_ones_are_left_alone() {
    let (_sources, base) = sources();
    let (_directory, manifest) = workspace();
    let manifest = manifest
        .with_entry("keep.txt", Entry::file("as materialized"))
        .with_entry("token", Entry::file("rebuilt").ephemeral(true));
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;
    let applier = ManifestApplier::new(session.clone(), base);

    applier
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");
    // Stand in for a workspace that survived a stop and was written to since.
    std::fs::write(Path::new(&root).join("keep.txt"), "edited since").expect("edit");
    std::fs::remove_file(Path::new(&root).join("token")).expect("remove");

    applier.apply_ephemeral(&manifest).await.expect("reapply");

    assert_eq!(read(&root, "token"), "rebuilt");
    assert_eq!(
        read(&root, "keep.txt"),
        "edited since",
        "an ephemeral reapply must not overwrite persisted workspace content"
    );
}

#[tokio::test]
async fn a_manifest_that_names_an_account_is_refused_because_it_would_be_created_on_this_host() {
    let (_directory, manifest) = workspace();
    let manifest = manifest
        .with_user(User::new("builder"))
        .with_entry("README.md", Entry::file("hello"));
    let root = manifest.root.clone();
    let session = session_for(manifest).await;

    let error = session.start().await.expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(
        !Path::new(&root).join("README.md").exists(),
        "nothing is materialized into a workspace whose ownership cannot be honoured"
    );
}

#[tokio::test]
async fn a_manifest_that_declares_a_group_is_refused_for_the_same_reason() {
    let (_directory, manifest) = workspace();
    let manifest = manifest.with_group(Group::new("web", vec![User::new("nginx")]));
    let session = session_for(manifest).await;

    let error = session.start().await.expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

#[tokio::test]
async fn a_manifest_that_declares_a_mount_is_refused_until_the_mount_lifecycle_lands() {
    let (_directory, manifest) = workspace();
    let mount = Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "artifacts".to_owned(),
            ..Default::default()
        }),
        MountStrategy::InContainer {
            pattern: MountPattern::Mountpoint(MountpointOptions::default()),
        },
    )
    .expect("a supported provider and strategy");
    let manifest = manifest.with_entry("mounted", Entry::mount(mount));
    let root = manifest.root.clone();
    let session = session_for(manifest).await;

    let error = session.start().await.expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    // Refused rather than left as an ordinary empty directory, which is what a workspace would come
    // up with if the entry were quietly skipped.
    assert!(!Path::new(&root).join("mounted").exists());
}

#[tokio::test]
async fn a_large_copied_directory_is_paced_by_the_limit_it_was_given() {
    let (_sources, base) = sources();
    std::fs::create_dir(base.join("many")).expect("make the source tree");
    for index in 0..25 {
        std::fs::write(base.join(format!("many/file-{index:02}.txt")), "hello").expect("write");
    }

    let (_directory, manifest) = workspace();
    let manifest = manifest.with_entry("copied", Entry::local_dir(Some("many".to_owned())));
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;
    let limits = SandboxConcurrencyLimits::default()
        .with_local_dir_files(Some(3))
        .expect("a positive limit");

    let receipt = ManifestApplier::new(session.clone(), base)
        .with_limits(limits)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(receipt.files().len(), 25);
    assert_eq!(read(&root, "copied/file-07.txt"), "hello");
    // Ordered by source path whatever order the copies finished in.
    let listed = receipted(&receipt);
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    assert_eq!(listed, sorted);
}

// --- sources, against the reference's `test_entries.py` -------------------------------------

/// A base directory and a sibling outside it, under one canonicalized parent.
fn base_and_outside() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let top = std::fs::canonicalize(directory.path()).expect("resolve");
    let base = top.join("base");
    let outside = top.join("outside");
    std::fs::create_dir(&base).expect("base");
    std::fs::create_dir(&outside).expect("outside");
    std::fs::write(outside.join("secret.txt"), "secret").expect("write");
    (directory, base, outside)
}

/// Applies one entry against `base`, returning the refusal it is expected to produce.
async fn refusal(manifest: Manifest, base: PathBuf) -> ra_core::sandbox::SandboxError {
    let session = session_for(manifest.clone()).await;
    ManifestApplier::new(session, base)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("refused")
}

fn context<'a>(error: &'a ra_core::sandbox::SandboxError, key: &str) -> Option<&'a str> {
    error.context().get(key).and_then(|value| value.as_str())
}

#[tokio::test]
async fn a_source_that_climbs_out_of_the_base_directory_is_refused_either_way_it_is_written() {
    let (_directory, base, outside) = base_and_outside();
    for (entry, code) in [
        (
            Entry::local_file("../outside/secret.txt"),
            ErrorCode::LocalFileReadError,
        ),
        (
            Entry::local_file(outside.join("secret.txt").to_string_lossy().into_owned()),
            ErrorCode::LocalFileReadError,
        ),
        (
            Entry::local_dir(Some("../outside".to_owned())),
            ErrorCode::LocalDirReadError,
        ),
        (
            Entry::local_dir(Some(outside.to_string_lossy().into_owned())),
            ErrorCode::LocalDirReadError,
        ),
    ] {
        let (_workspace, manifest) = workspace();
        let root = manifest.root.clone();
        let error = refusal(manifest.with_entry("copied", entry), base.clone()).await;

        assert_eq!(error.error_code(), code);
        assert_eq!(context(&error, "reason"), Some("outside_base_dir"));
        assert_eq!(
            context(&error, "base_dir"),
            Some(base.to_string_lossy().as_ref())
        );
        assert!(!Path::new(&root).join("copied").exists());
    }
}

#[tokio::test]
async fn a_source_no_grant_covers_names_the_grants_that_were_checked() {
    let (_directory, base, outside) = base_and_outside();
    let other = base.parent().expect("a parent").join("other");
    std::fs::create_dir(&other).expect("other");
    let (_workspace, manifest) = workspace();
    let manifest = manifest
        .with_path_grant(SandboxPathGrant::new(&other.to_string_lossy()).expect("grant"))
        .with_entry(
            "copied.txt",
            Entry::local_file(outside.join("secret.txt").to_string_lossy().into_owned()),
        );

    let error = refusal(manifest, base).await;

    assert_eq!(context(&error, "reason"), Some("outside_base_dir"));
    assert_eq!(
        error.context().get("extra_path_grants"),
        Some(&serde_json::json!([other.to_string_lossy()]))
    );
}

#[tokio::test]
async fn an_absolute_source_inside_the_base_directory_needs_no_grant() {
    let (_sources, base) = sources();
    std::fs::create_dir(base.join("source")).expect("source");
    std::fs::write(base.join("source/safe.txt"), "safe").expect("write");
    let (_workspace, manifest) = workspace();
    let manifest = manifest
        .with_entry(
            "file.txt",
            Entry::local_file(base.join("source/safe.txt").to_string_lossy().into_owned()),
        )
        .with_entry(
            "dir",
            Entry::local_dir(Some(base.join("source").to_string_lossy().into_owned())),
        );
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    ManifestApplier::new(session, base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(read(&root, "file.txt"), "safe");
    assert_eq!(read(&root, "dir/safe.txt"), "safe");
}

#[tokio::test]
async fn a_granted_directory_outside_the_base_is_copied_even_when_the_grant_is_read_only() {
    // Read-only limits what the sandbox may write there, not whether the manifest may read it.
    let (_directory, base, outside) = base_and_outside();
    let (_workspace, manifest) = workspace();
    let manifest = manifest
        .with_path_grant(
            SandboxPathGrant::new(&outside.to_string_lossy())
                .expect("grant")
                .read_only(true),
        )
        .with_entry(
            "copied",
            Entry::local_dir(Some(outside.to_string_lossy().into_owned())),
        );
    let root = manifest.root.clone();
    let session = session_for(manifest.clone()).await;

    ManifestApplier::new(session, base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(read(&root, "copied/secret.txt"), "secret");
}

#[tokio::test]
async fn a_grant_read_back_from_a_manifest_payload_authorizes_its_source() {
    let (_directory, base, outside) = base_and_outside();
    let (_workspace, manifest) = workspace();
    let root = manifest.root.clone();
    let parsed = Manifest::parse(
        &ra_core::sandbox::ManifestRegistries::builtin(),
        &serde_json::json!({
            "root": root,
            "extra_path_grants": [{"path": outside.to_string_lossy()}],
            "entries": {"copied.txt": {
                "type": "local_file",
                "src": outside.join("secret.txt").to_string_lossy(),
            }},
        }),
    )
    .expect("parse");
    let session = session_for(parsed.clone()).await;

    ManifestApplier::new(session, base)
        .apply_manifest(&parsed, false)
        .await
        .expect("apply");

    assert_eq!(read(&root, "copied.txt"), "secret");
}

#[tokio::test]
async fn every_symlink_on_the_way_to_a_source_is_refused_by_name() {
    let (_sources, base) = sources();
    std::fs::create_dir_all(base.join("secret-dir/sub")).expect("target tree");
    std::fs::write(base.join("secret-dir/sub/secret.txt"), "secret").expect("write");
    std::fs::write(base.join("secret.txt"), "secret").expect("write");
    std::os::unix::fs::symlink(base.join("secret-dir"), base.join("link")).expect("dir link");
    std::os::unix::fs::symlink(base.join("secret.txt"), base.join("link.txt")).expect("file link");
    std::os::unix::fs::symlink(base.join("secret-dir"), base.join("dir-link.txt"))
        .expect("leaf link to a directory");

    for (entry, code, child) in [
        // An ancestor of the source.
        (
            Entry::local_file("link/sub/secret.txt"),
            ErrorCode::LocalFileReadError,
            "link",
        ),
        (
            Entry::local_dir(Some("link/sub".to_owned())),
            ErrorCode::LocalDirReadError,
            "link",
        ),
        // The source file itself, whether it points at a file or a directory.
        (
            Entry::local_file("link.txt"),
            ErrorCode::LocalFileReadError,
            "link.txt",
        ),
        (
            Entry::local_file("dir-link.txt"),
            ErrorCode::LocalFileReadError,
            "dir-link.txt",
        ),
    ] {
        let (_workspace, manifest) = workspace();
        let root = manifest.root.clone();
        let error = refusal(manifest.with_entry("copied", entry), base.clone()).await;

        assert_eq!(error.error_code(), code, "{child}");
        assert_eq!(context(&error, "reason"), Some("symlink_not_supported"));
        assert_eq!(context(&error, "child"), Some(child));
        assert!(!Path::new(&root).join("copied").exists(), "{child}");
    }
}

#[tokio::test]
async fn a_link_inside_a_copied_directory_is_refused_by_name_whatever_it_points_at() {
    let (_sources, base) = sources();
    std::fs::create_dir_all(base.join("src")).expect("source tree");
    std::fs::create_dir_all(base.join("secret-dir")).expect("secret tree");
    std::fs::write(base.join("src/safe.txt"), "safe").expect("write");
    std::fs::write(base.join("secret.txt"), "secret").expect("write");
    std::fs::write(base.join("secret-dir/secret.txt"), "secret").expect("write");

    for (link, target) in [("link.txt", "secret.txt"), ("linked-dir", "secret-dir")] {
        let link_path = base.join("src").join(link);
        std::os::unix::fs::symlink(base.join(target), &link_path).expect("link");
        let (_workspace, manifest) = workspace();
        let manifest = manifest.with_entry("copied", Entry::local_dir(Some("src".to_owned())));

        let error = refusal(manifest, base.clone()).await;

        assert_eq!(error.error_code(), ErrorCode::LocalDirReadError);
        assert_eq!(context(&error, "reason"), Some("symlink_not_supported"));
        assert_eq!(context(&error, "child"), Some(link));
        std::fs::remove_file(&link_path).expect("unlink");
    }
}
