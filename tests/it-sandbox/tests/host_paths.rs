//! `ra-sandbox::host_paths`: where a path really points once the filesystem has had its say.
//!
//! The lexical policy in the kernel already refuses the paths that *say* they are leaving the
//! workspace. What is tested here is the class it cannot see: a path that stays inside the
//! workspace as written and lands outside it once symlinks are followed. Every one of these tests
//! builds a real directory and a real link, because a resolving check that was tested against a
//! mock would be testing the mock.

use std::fs;
use std::path::Path;

use ra_core::sandbox::{ErrorCode, SandboxPathGrant};
use ra_sandbox::host_paths::{
    HostWorkspacePaths, expand_user, resolve_without_strictness, sandbox_path_grant_host_path,
};

/// A workspace root and somewhere outside it, both real.
struct Fixture {
    _root: tempfile::TempDir,
    workspace: std::path::PathBuf,
    outside: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("temp root");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("secret.txt"), b"classified").expect("secret");
    Fixture {
        _root: root,
        workspace,
        outside,
    }
}

fn policy(fixture: &Fixture, grants: Vec<SandboxPathGrant>) -> HostWorkspacePaths {
    HostWorkspacePaths::new(&fixture.workspace.to_string_lossy(), grants).expect("policy")
}

#[test]
fn resolution_follows_links_and_keeps_what_is_not_there_yet() {
    let fixture = fixture();
    let target = fixture.workspace.join("real");
    fs::create_dir(&target).expect("target");
    std::os::unix::fs::symlink(&target, fixture.workspace.join("link")).expect("link");

    // The link resolves; the file under it does not exist yet and is carried through unchanged,
    // which is what a write to a new file needs and what `canonicalize` refuses to do.
    let resolved =
        resolve_without_strictness(&fixture.workspace.join("link/new.txt")).expect("resolved path");
    assert_eq!(
        resolved,
        resolve_without_strictness(&target)
            .expect("resolved path")
            .join("new.txt")
    );
}

#[test]
fn resolution_climbs_from_where_the_link_landed_not_from_how_it_was_written() {
    let fixture = fixture();
    let nested = fixture.workspace.join("a/b");
    fs::create_dir_all(&nested).expect("nested");
    std::os::unix::fs::symlink(&nested, fixture.workspace.join("link")).expect("link");

    // Written as `link/..`, which looks like the workspace. Resolved, the link has already moved
    // the path to `a/b`, so the climb lands in `a`. Anything that folded `..` away first would get
    // this backwards.
    let resolved =
        resolve_without_strictness(&fixture.workspace.join("link/..")).expect("resolved path");
    assert_eq!(
        resolved,
        resolve_without_strictness(&fixture.workspace.join("a")).expect("resolved path")
    );
}

#[test]
fn a_home_relative_path_is_expanded_and_another_users_home_is_not() {
    let home = std::env::var("HOME").expect("HOME");
    assert_eq!(expand_user("~/bin"), Path::new(&home).join("bin"));
    assert_eq!(expand_user("~"), Path::new(&home));
    // `~someone` names an account this has no business guessing at.
    assert_eq!(expand_user("~other/bin"), Path::new("~other/bin"));
    assert_eq!(expand_user("/usr/bin"), Path::new("/usr/bin"));
}

#[test]
fn a_path_inside_the_workspace_resolves_to_itself() {
    let fixture = fixture();
    let paths = policy(&fixture, Vec::new());
    let resolved = paths.normalize_path("notes/today.md", false).expect("path");
    assert_eq!(
        resolved,
        paths.resolved_root().join("notes").join("today.md")
    );
}

#[test]
fn a_relative_path_that_climbs_out_is_refused_for_climbing_out() {
    let fixture = fixture();
    let paths = policy(&fixture, Vec::new());
    let error = paths
        .normalize_path("../outside/secret.txt", false)
        .expect_err("a path that leaves the workspace");
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("escape_root")
    );
}

#[test]
fn an_absolute_path_outside_the_workspace_is_refused_as_absolute() {
    let fixture = fixture();
    let paths = policy(&fixture, Vec::new());
    let error = paths
        .normalize_path(&fixture.outside.join("secret.txt").to_string_lossy(), false)
        .expect_err("a path outside every grant");
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("absolute")
    );
}

#[test]
fn a_symlink_inside_the_workspace_pointing_out_of_it_is_refused() {
    let fixture = fixture();
    std::os::unix::fs::symlink(&fixture.outside, fixture.workspace.join("escape"))
        .expect("escape link");
    let paths = policy(&fixture, Vec::new());

    // Lexically this is `<workspace>/escape/secret.txt` and passes every text-based check there is.
    // This is the whole reason the resolving half exists.
    let error = paths
        .normalize_path("escape/secret.txt", false)
        .expect_err("a link out of the workspace");
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
}

#[test]
fn a_grant_is_reachable_by_naming_it_outright() {
    let fixture = fixture();
    let grant = SandboxPathGrant::new(&fixture.outside.to_string_lossy()).expect("grant");
    let paths = policy(&fixture, vec![grant]);

    let resolved = paths
        .normalize_path(&fixture.outside.join("secret.txt").to_string_lossy(), false)
        .expect("granted path");
    assert_eq!(
        resolved,
        resolve_without_strictness(&fixture.outside)
            .expect("resolved path")
            .join("secret.txt")
    );
}

#[test]
fn a_read_only_grant_refuses_a_write_and_still_allows_a_read() {
    let fixture = fixture();
    let grant = SandboxPathGrant::new(&fixture.outside.to_string_lossy())
        .expect("grant")
        .read_only(true);
    let paths = policy(&fixture, vec![grant]);
    let target = fixture
        .outside
        .join("secret.txt")
        .to_string_lossy()
        .into_owned();

    paths.normalize_path(&target, false).expect("read");
    let error = paths
        .normalize_path(&target, true)
        .expect_err("a write through a read-only grant");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("read_only_extra_path_grant")
    );
}

#[test]
fn the_innermost_grant_decides() {
    let fixture = fixture();
    let inner = fixture.outside.join("locked");
    fs::create_dir_all(&inner).expect("inner");

    // A read-only grant nested inside a writable one is a narrowing, so the longest match has to
    // win. Taking the first match instead would let a write through the outer grant.
    let paths = policy(
        &fixture,
        vec![
            SandboxPathGrant::new(&fixture.outside.to_string_lossy()).expect("outer"),
            SandboxPathGrant::new(&inner.to_string_lossy())
                .expect("inner")
                .read_only(true),
        ],
    );

    paths
        .normalize_path(&fixture.outside.join("free.txt").to_string_lossy(), true)
        .expect("a write in the writable grant");
    let error = paths
        .normalize_path(&inner.join("held.txt").to_string_lossy(), true)
        .expect_err("a write in the read-only grant");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
}

#[test]
fn a_grant_whose_source_resolves_to_the_filesystem_root_is_refused() {
    let fixture = fixture();
    let link = fixture.outside.join("everything");
    std::os::unix::fs::symlink("/", &link).expect("root link");

    // The grant named a subdirectory and passed validation when it was written. By the time it is
    // used the link points at the whole filesystem, which is the substitution the second check is
    // there to catch.
    let grant = SandboxPathGrant::new(&link.to_string_lossy()).expect("grant");
    let error = sandbox_path_grant_host_path(&grant).expect_err("a grant that became everything");
    assert_eq!(
        error.to_string(),
        "sandbox path grant path must not resolve to filesystem root"
    );
}

#[test]
fn a_workspace_root_that_is_not_absolute_is_refused() {
    HostWorkspacePaths::new("workspace", Vec::new())
        .expect_err("a root measured from wherever the process happens to be");
}

/// Creates a chain longer than the kernel's usual per-operation symlink limit.
fn long_chain(directory: &Path, target: &Path) -> std::path::PathBuf {
    for index in 0..45 {
        let next = if index == 44 {
            target.to_owned()
        } else {
            directory.join(format!("link{}", index + 1))
        };
        std::os::unix::fs::symlink(next, directory.join(format!("link{index}")))
            .expect("chain link");
    }
    directory.join("link0")
}

#[test]
fn a_long_chain_resolves_fully_and_preserves_a_missing_leaf() {
    let fixture = fixture();
    let target = fixture.workspace.join("target");
    fs::create_dir(&target).expect("target");
    let link = long_chain(&fixture.workspace, &target);
    let paths = policy(&fixture, Vec::new());
    assert_eq!(
        paths
            .normalize_path(&link.join("new.txt").to_string_lossy(), true)
            .expect("new file"),
        fs::canonicalize(target)
            .expect("target path")
            .join("new.txt"),
    );
}

#[test]
fn a_long_chain_to_the_filesystem_root_cannot_become_a_grant() {
    let fixture = fixture();
    let link = long_chain(&fixture.outside, Path::new("/"));
    let grant = SandboxPathGrant::new(&link.to_string_lossy()).expect("grant");
    let error = sandbox_path_grant_host_path(&grant).expect_err("filesystem root grant");
    assert_eq!(
        error.to_string(),
        "sandbox path grant path must not resolve to filesystem root"
    );
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn symlink_cycles_are_errors_for_paths_roots_and_grants(#[case] indirect: bool) {
    let fixture = fixture();
    let link = fixture.workspace.join("loop");
    if indirect {
        std::os::unix::fs::symlink("other", &link).expect("first link");
        std::os::unix::fs::symlink("loop", fixture.workspace.join("other")).expect("second link");
    } else {
        std::os::unix::fs::symlink("loop", &link).expect("self link");
    }
    let error = resolve_without_strictness(&link).expect_err("cycle");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(
        policy(&fixture, Vec::new())
            .normalize_path("loop/file", false)
            .is_err()
    );
    assert!(HostWorkspacePaths::new(&link.to_string_lossy(), Vec::new()).is_err());
    let grant = SandboxPathGrant::new(&link.to_string_lossy()).expect("grant");
    assert!(sandbox_path_grant_host_path(&grant).is_err());
}

#[test]
fn revisiting_a_completed_symlink_is_not_a_cycle() {
    let fixture = fixture();
    fs::create_dir(fixture.workspace.join("target")).expect("target");
    std::os::unix::fs::symlink("target", fixture.workspace.join("link")).expect("link");
    let paths = policy(&fixture, Vec::new());
    assert_eq!(
        paths
            .normalize_path("link/../link/new.txt", true)
            .expect("repeated link"),
        paths.resolved_root().join("target/new.txt"),
    );
    assert_eq!(
        resolve_without_strictness(&fixture.workspace.join("link/../link/new.txt"))
            .expect("resolve repeated link"),
        paths.resolved_root().join("target/new.txt"),
    );
}
