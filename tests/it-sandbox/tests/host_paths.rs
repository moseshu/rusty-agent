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

// --- the reference's `test_workspace_paths.py`, host half -------------------------------------

#[test]
fn a_workspace_reached_through_an_alias_resolves_the_way_the_reference_resolves_it() {
    let fixture = fixture();
    let target = fixture.workspace.join("target.txt");
    fs::write(&target, "hello").expect("target");
    std::os::unix::fs::symlink(&target, fixture.workspace.join("link.txt")).expect("leaf link");
    std::os::unix::fs::symlink(&fixture.outside, fixture.workspace.join("outside-link"))
        .expect("escape link");
    let alias = fixture
        .workspace
        .parent()
        .expect("parent")
        .join("workspace-alias");
    std::os::unix::fs::symlink(&fixture.workspace, &alias).expect("root alias");
    let paths = HostWorkspacePaths::new(&alias.to_string_lossy(), Vec::new()).expect("policy");
    let resolved_target = fs::canonicalize(&target).expect("resolve");

    for path in [
        "target.txt".to_owned(),
        "nested/../target.txt".to_owned(),
        "link.txt".to_owned(),
        alias.join("target.txt").to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    ] {
        assert_eq!(
            paths.normalize_path(&path, false).expect(&path),
            resolved_target,
            "{path}"
        );
    }

    let error = paths
        .normalize_path("outside-link/secret.txt", false)
        .expect_err("a link out of the workspace");
    assert_eq!(
        error.message(),
        "manifest path must not escape root: outside-link/secret.txt"
    );
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("escape_root")
    );

    let outside = fixture.outside.join("secret.txt");
    let error = paths
        .normalize_path(&outside.to_string_lossy(), false)
        .expect_err("outside the workspace");
    assert_eq!(
        error.message(),
        format!(
            "manifest path must be relative: {}",
            outside.to_string_lossy()
        )
    );
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("absolute")
    );
}

#[test]
fn a_drive_path_is_refused_as_absolute_against_a_host_root() {
    // On the hosts this backend runs on a drive path is not absolute, so without the check it
    // would be anchored under the workspace as though it were relative.
    let fixture = fixture();
    let paths = policy(&fixture, Vec::new());

    for path in ["C:/tmp/secret.txt", "C:\\tmp\\secret.txt"] {
        for for_write in [false, true] {
            let error = paths
                .normalize_path(path, for_write)
                .expect_err("a drive path");
            assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
            assert_eq!(
                error.message(),
                "manifest path must be relative: C:/tmp/secret.txt"
            );
            assert_eq!(
                error
                    .context()
                    .get("rel")
                    .and_then(serde_json::Value::as_str),
                Some("C:/tmp/secret.txt")
            );
        }
    }
}

#[test]
fn a_read_only_grant_named_through_an_alias_refuses_a_write_to_its_real_path() {
    let fixture = fixture();
    let alias = fixture
        .outside
        .parent()
        .expect("parent")
        .join("allowed-alias");
    std::os::unix::fs::symlink(&fixture.outside, &alias).expect("grant alias");
    let grant = SandboxPathGrant::new(&alias.to_string_lossy())
        .expect("grant")
        .read_only(true);
    let paths = policy(&fixture, vec![grant]);
    let target = fs::canonicalize(&fixture.outside)
        .expect("resolve")
        .join("cache.db");

    let error = paths
        .normalize_path(&target.to_string_lossy(), true)
        .expect_err("a write through a read-only grant");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error.message(),
        format!(
            "failed to write archive for path: {}",
            target.to_string_lossy()
        )
    );
    // The grant is named the way it was written, not the way it resolved.
    assert_eq!(
        error
            .context()
            .get("grant_path")
            .and_then(serde_json::Value::as_str),
        Some(alias.to_string_lossy().as_ref())
    );
}

#[test]
fn a_split_grant_resolves_its_host_source_once_and_keeps_that_answer() {
    let fixture = fixture();
    let alias = fixture
        .outside
        .parent()
        .expect("parent")
        .join("source-alias");
    std::os::unix::fs::symlink(&fixture.outside, &alias).expect("source alias");
    let grant = SandboxPathGrant::new("/mnt/shared-data")
        .expect("grant")
        .with_host_path(&alias.to_string_lossy())
        .expect("host path");

    let resolved = sandbox_path_grant_host_path(&grant).expect("resolved source");
    // Repointing the alias afterwards does not move what was already resolved.
    fs::remove_file(&alias).expect("unlink");
    std::os::unix::fs::symlink("/", &alias).expect("repoint");

    assert_eq!(
        resolved,
        fs::canonicalize(&fixture.outside).expect("resolve")
    );
    assert_eq!(
        sandbox_path_grant_host_path(&grant)
            .expect_err("the repointed source is the whole filesystem")
            .to_string(),
        "sandbox path grant path must not resolve to filesystem root"
    );
}
