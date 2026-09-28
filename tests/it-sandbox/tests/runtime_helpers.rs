//! `ra-sandbox::runtime_helpers`: the scripts a session runs inside its own sandbox.
//!
//! These run the scripts directly with `sh` rather than through a session, for two reasons. A shell
//! script is the one thing in this crate whose behaviour Rust cannot check by compiling it. And the
//! sandbox it is meant for cannot run it on this platform at all: the local backend's macOS fence
//! denies writes under `/tmp`, which is where helpers install, so a session on a Mac never gets
//! past installation. Running the script here is what keeps the *script* covered on a host that
//! cannot exercise the path that would use it.

use std::path::{Path, PathBuf};
use std::process::Command;

use ra_sandbox::runtime_helpers::{
    RuntimeHelperScript, resolve_workspace_path_helper, workspace_fingerprint_helper,
};
use ra_sandbox::snapshot::lifecycle::{SNAPSHOT_FINGERPRINT_VERSION, parse_fingerprint_record};

/// Runs the installer with a destination of this test's choosing.
///
/// The installed path is an argument rather than something the script decides, which is what lets
/// this check the installer without writing to the directory a real session would use.
fn install(destination: &Path) -> std::process::Output {
    install_helper(&workspace_fingerprint_helper(), destination)
}

/// Runs `helper`'s installer with a destination of this test's choosing.
fn install_helper(helper: &RuntimeHelperScript, destination: &Path) -> std::process::Output {
    let command = helper.install_command();
    let (program, arguments) = command.split_first().expect("a command");
    // The installed path is the script's last argument, which it reads as `$1`.
    let mut arguments: Vec<String> = arguments.to_vec();
    arguments.pop();
    arguments.push(destination.to_string_lossy().into_owned());
    Command::new(program)
        .args(&arguments)
        .output()
        .expect("the installer ran")
}

/// Runs the fingerprint helper, and answers what it printed and how it exited.
fn fingerprint(
    helper: &Path,
    root: &Path,
    cache: &Path,
    manifest_digest: &str,
    excludes: &[&str],
) -> std::process::Output {
    let mut invocation = Command::new("sh");
    invocation
        .arg(helper)
        .arg(root)
        .arg(SNAPSHOT_FINGERPRINT_VERSION)
        .arg(cache)
        .arg(manifest_digest);
    for exclude in excludes {
        invocation.arg(exclude);
    }
    invocation.output().expect("the helper ran")
}

/// A workspace with one file in it.
fn workspace(directory: &Path, body: &str) -> PathBuf {
    let root = directory.join("workspace");
    std::fs::create_dir_all(root.join("src")).expect("workspace");
    std::fs::write(root.join("src/main.rs"), body).expect("write");
    root
}

#[test]
fn the_installer_writes_the_script_and_leaves_an_identical_one_alone() {
    let directory = tempfile::tempdir().expect("temp");
    let destination = directory
        .path()
        .join("nested")
        .join("workspace-fingerprint");

    let first = install(&destination);
    assert!(first.status.success(), "{first:?}");
    assert!(
        std::fs::read_to_string(&destination)
            .expect("read")
            .contains("RUSTY_AGENT_WORKSPACE_FINGERPRINT_V1")
    );

    // Installing again is the idempotent check the reference's per-session cache exists to skip: a
    // destination whose content already matches is left exactly as it was.
    let before = std::fs::metadata(&destination).expect("metadata");
    let second = install(&destination);
    assert!(second.status.success(), "{second:?}");
    let after = std::fs::metadata(&destination).expect("metadata");
    assert_eq!(
        before.modified().expect("mtime"),
        after.modified().expect("mtime")
    );
    // And nothing is left beside it: the temporary the installer writes through is cleaned up.
    let beside: Vec<_> = std::fs::read_dir(destination.parent().expect("parent"))
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(beside, ["workspace-fingerprint"]);
}

#[test]
fn the_helper_hashes_the_workspace_and_leaves_the_answer_cached() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = workspace(directory.path(), "fn main() {}");
    let cache = directory.path().join("state").join("fingerprint.json");

    let output = fingerprint(&helper, &root, &cache, "manifest-digest", &[]);

    assert!(output.status.success(), "{output:?}");
    let record = parse_fingerprint_record(&output.stdout).expect("a record");
    assert_eq!(record.version(), SNAPSHOT_FINGERPRINT_VERSION);
    assert_eq!(record.fingerprint().len(), 64, "a sha256 in hex");
    // One run both answers and remembers, so a resume that asks twice does not tar the workspace
    // twice.
    assert_eq!(
        std::fs::read(&cache).expect("cached"),
        output.stdout,
        "the cached record is what was printed"
    );
}

#[test]
fn the_same_workspace_hashes_the_same_and_a_changed_one_does_not() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = workspace(directory.path(), "fn main() {}");
    let cache = directory.path().join("fingerprint.json");

    let first = fingerprint(&helper, &root, &cache, "manifest-digest", &[]);
    let again = fingerprint(&helper, &root, &cache, "manifest-digest", &[]);
    assert_eq!(first.stdout, again.stdout);

    // The manifest is the other half of what a resumed session is supposed to have, so the same
    // workspace under a different declaration is not the same answer.
    let redeclared = fingerprint(&helper, &root, &cache, "another-digest", &[]);
    assert_ne!(first.stdout, redeclared.stdout);

    std::fs::write(root.join("src/main.rs"), "fn main() { todo!() }").expect("write");
    let changed = fingerprint(&helper, &root, &cache, "manifest-digest", &[]);
    assert_ne!(first.stdout, changed.stdout);
}

#[test]
fn the_answer_is_about_the_archive_not_only_the_bytes_in_it() {
    // Worth knowing before trusting a match: the fingerprint hashes a tar of the workspace, and a
    // tar carries each file's metadata as well as its content. Writing the same bytes back gives a
    // new modification time and therefore a new fingerprint, so a resume that would have skipped
    // restoring goes ahead and restores. That is the safe direction, and it is the reference's
    // behaviour too — it hashes the same archive.
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = workspace(directory.path(), "fn main() {}");
    let cache = directory.path().join("fingerprint.json");

    let before = fingerprint(&helper, &root, &cache, "digest", &[]);
    let source = root.join("src/main.rs");
    std::fs::write(&source, "fn main() {}").expect("write the same bytes");
    // Stamped rather than left to the clock, so the test says what it means on a filesystem whose
    // timestamps are coarse.
    let file = std::fs::File::options()
        .write(true)
        .open(&source)
        .expect("open");
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)),
    )
    .expect("times");
    let after = fingerprint(&helper, &root, &cache, "digest", &[]);

    assert_ne!(before.stdout, after.stdout);
}

#[test]
fn what_is_excluded_does_not_change_the_answer() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = workspace(directory.path(), "fn main() {}");
    let cache = directory.path().join("fingerprint.json");
    std::fs::write(root.join("scratch.log"), "first").expect("write");

    let before = fingerprint(&helper, &root, &cache, "digest", &["scratch.log"]);
    std::fs::write(root.join("scratch.log"), "second, and longer").expect("write");
    let after = fingerprint(&helper, &root, &cache, "digest", &["scratch.log"]);

    assert!(before.status.success(), "{before:?}");
    assert_eq!(
        before.stdout, after.stdout,
        "an excluded path is not part of what the fingerprint describes"
    );
    // And it really was the exclusion doing that: without it the same change is visible.
    let unexcluded = fingerprint(&helper, &root, &cache, "digest", &[]);
    assert_ne!(before.stdout, unexcluded.stdout);
}

#[test]
fn a_nested_exclusion_is_matched_where_it_sits() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = workspace(directory.path(), "fn main() {}");
    let cache = directory.path().join("fingerprint.json");
    std::fs::create_dir_all(root.join("target/debug")).expect("directory");
    std::fs::write(root.join("target/debug/out"), "built").expect("write");

    let before = fingerprint(&helper, &root, &cache, "digest", &["target/debug"]);
    std::fs::write(root.join("target/debug/out"), "rebuilt, differently").expect("write");
    let after = fingerprint(&helper, &root, &cache, "digest", &["target/debug"]);

    assert!(before.status.success(), "{before:?}");
    assert_eq!(before.stdout, after.stdout);
}

#[test]
fn an_exclusion_that_could_reach_outside_the_workspace_is_refused() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = workspace(directory.path(), "fn main() {}");
    let cache = directory.path().join("fingerprint.json");

    for exclude in ["..", "../escape", "nested/../..", "/etc", ".", ""] {
        let output = fingerprint(&helper, &root, &cache, "digest", &[exclude]);
        assert_eq!(
            output.status.code(),
            Some(65),
            "expected a refusal for {exclude:?}: {output:?}"
        );
    }
}

#[test]
fn a_workspace_that_is_not_there_is_refused_before_anything_is_hashed() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let cache = directory.path().join("fingerprint.json");

    let output = fingerprint(
        &helper,
        &directory.path().join("never-created"),
        &cache,
        "digest",
        &[],
    );

    assert_eq!(output.status.code(), Some(66), "{output:?}");
    assert!(
        !cache.exists(),
        "nothing is cached for a workspace that is gone"
    );
}

#[test]
fn failed_archive_or_hash_commands_never_publish_a_fingerprint() {
    use std::os::unix::fs::PermissionsExt;

    for (program, script) in [
        (
            "tar",
            "#!/bin/sh\nif [ \"$1\" = --help ]; then echo --no-wildcards; exit 0; fi\nexit 2\n",
        ),
        (
            "tar",
            "#!/bin/sh\nif [ \"$1\" = --help ]; then echo --no-wildcards; exit 0; fi\nprintf partial-archive\nexit 2\n",
        ),
        ("sha256sum", "#!/bin/sh\ncat >/dev/null\nexit 2\n"),
    ] {
        let directory = tempfile::tempdir().expect("temp");
        let helper = directory.path().join("workspace-fingerprint");
        assert!(install(&helper).status.success());
        let root = workspace(directory.path(), "persisted content");
        let cache = directory.path().join("fingerprint.json");
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).expect("bin");
        let executable = bin.join(program);
        std::fs::write(&executable, script).expect("script");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH"),
        ));
        let path = std::env::join_paths(paths).expect("search path");
        for existing_cache in [false, true] {
            if existing_cache {
                std::fs::write(&cache, b"previous record").expect("cache");
            }
            let output = Command::new("sh")
                .arg(&helper)
                .arg(&root)
                .arg(SNAPSHOT_FINGERPRINT_VERSION)
                .arg(&cache)
                .arg("digest")
                .env("PATH", &path)
                .output()
                .expect("helper ran");
            assert!(!output.status.success(), "{program}: {output:?}");
            assert!(output.stdout.is_empty(), "no valid record: {output:?}");
            if existing_cache {
                assert_eq!(
                    std::fs::read(&cache).expect("cache retained"),
                    b"previous record"
                );
            } else {
                assert!(!cache.exists());
            }
        }
    }
}

// --- the reference's `test_runtime_helpers.py` ----------------------------------------------------

/// `test_runtime_helper_from_content_uses_posix_install_path`: a POSIX path under the helper root,
/// named after the helper and its content.
///
/// The root is this framework's `/tmp/rusty-agent/bin` rather than the reference's
/// `/tmp/openai-agents/bin` (a recorded choice, §3.30 of the R8-P0 checklist): the directory names
/// the product that installs into it, and nothing reads it but the commands built from it.
#[test]
fn a_helper_installs_under_the_helper_root_named_by_its_content() {
    let helper = RuntimeHelperScript::from_content("test-helper", "#!/bin/sh\nprintf 'ok\\n'");
    let name = helper
        .install_path()
        .strip_prefix("/tmp/rusty-agent/bin/test-helper-")
        .unwrap_or_else(|| panic!("{}", helper.install_path()));
    assert_eq!(name.len(), 12, "{name}");
    assert!(name.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
    assert_ne!(
        RuntimeHelperScript::from_content("test-helper", "#!/bin/sh\n").install_path(),
        helper.install_path(),
        "a different script installs under a different name"
    );
}

/// `test_workspace_fingerprint_helper_treats_exclusions_as_literal`: an exclusion is a path, not a
/// pattern, so `cache[1]` leaves out that directory and not `cache1`.
#[test]
fn an_exclusion_is_a_literal_path_not_a_pattern() {
    let directory = tempfile::tempdir().expect("temp");
    let helper = directory.path().join("workspace-fingerprint");
    assert!(install(&helper).status.success());
    let root = directory.path().join("workspace");
    std::fs::create_dir_all(root.join("cache[1]")).expect("excluded");
    std::fs::create_dir_all(root.join("cache1")).expect("durable");
    std::fs::write(root.join("cache[1]/remote.txt"), "remote-one").expect("write");
    std::fs::write(root.join("cache1/durable.txt"), "durable-one").expect("write");
    let cache = directory.path().join("fingerprint.json");
    let answer = || {
        let output = fingerprint(&helper, &root, &cache, "manifest-digest", &["cache[1]"]);
        assert!(output.status.success(), "{output:?}");
        parse_fingerprint_record(&output.stdout)
            .expect("a record")
            .fingerprint()
            .to_owned()
    };

    let first = answer();
    std::fs::write(root.join("cache[1]/remote.txt"), "remote-two").expect("write");
    assert_eq!(answer(), first);
    std::fs::write(root.join("cache1/durable.txt"), "durable-two").expect("write");
    assert_ne!(answer(), first);
}

/// A workspace, a directory beside it, and a link from inside the workspace to that directory, all
/// under a resolved temporary directory; and the resolve helper installed there.
struct Resolve {
    _directory: tempfile::TempDir,
    top: PathBuf,
    helper: PathBuf,
    workspace: PathBuf,
}

impl Resolve {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp");
        let top = std::fs::canonicalize(directory.path()).expect("resolve");
        let helper = top.join("resolve-workspace-path");
        let installed = install_helper(&resolve_workspace_path_helper(), &helper);
        assert!(installed.status.success(), "{installed:?}");
        let workspace = top.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        Self {
            _directory: directory,
            top,
            helper,
            workspace,
        }
    }

    /// `tmp/result.txt` beside the workspace, and `workspace/tmp-link` pointing at `tmp`; with
    /// `protected`, the file sits in `tmp/protected` instead.
    fn linked_scratch(&self, protected: bool) -> PathBuf {
        let extra = self.top.join("tmp");
        let holder = if protected {
            extra.join("protected")
        } else {
            extra.clone()
        };
        std::fs::create_dir_all(&holder).expect("extra root");
        std::fs::write(holder.join("result.txt"), "scratch output").expect("write");
        std::os::unix::fs::symlink(&extra, self.workspace.join("tmp-link")).expect("link");
        holder.join("result.txt")
    }

    fn run(&self, arguments: &[&str]) -> (i32, String, String) {
        let output = Command::new("sh")
            .arg(&self.helper)
            .arg(&self.workspace)
            .args(arguments)
            .output()
            .expect("the helper ran");
        (
            output.status.code().expect("an exit code"),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn path(&self, relative: &str) -> String {
        self.top.join(relative).to_string_lossy().into_owned()
    }
}

/// `test_resolve_workspace_path_helper_allows_extra_root_symlink_target`.
#[test]
fn a_link_into_a_granted_root_resolves_to_its_target() {
    let resolve = Resolve::new();
    let target = resolve.linked_scratch(false);

    let answer = resolve.run(&[
        &resolve.path("workspace/tmp-link/result.txt"),
        "0",
        &resolve.path("tmp"),
        "0",
    ]);

    assert_eq!(
        answer,
        (0, format!("{}\n", target.display()), String::new())
    );
}

/// `test_resolve_workspace_path_helper_rejects_extra_root_when_not_allowed`.
#[test]
fn a_link_out_of_the_workspace_without_a_grant_is_an_escape() {
    let resolve = Resolve::new();
    let target = resolve.linked_scratch(false);

    let answer = resolve.run(&[&resolve.path("workspace/tmp-link/result.txt"), "0"]);

    assert_eq!(
        answer,
        (
            111,
            String::new(),
            format!("workspace escape: {}\n", target.display())
        )
    );
}

/// `test_resolve_workspace_path_helper_rejects_extra_root_symlink_to_root`.
#[test]
fn a_grant_that_resolves_to_the_filesystem_root_is_refused() {
    let resolve = Resolve::new();
    let root_alias = resolve.path("root-alias");
    std::os::unix::fs::symlink("/", &root_alias).expect("link");

    let answer = resolve.run(&["/etc/passwd", "0", &root_alias, "0"]);

    assert_eq!(
        answer,
        (
            113,
            String::new(),
            format!("extra path grant must not resolve to filesystem root: {root_alias}\n")
        )
    );
}

/// `test_resolve_workspace_path_helper_rejects_nested_read_only_extra_grant_on_write`.
#[test]
fn a_write_into_a_nested_read_only_grant_is_refused() {
    let resolve = Resolve::new();
    let target = resolve.linked_scratch(true);
    let protected = resolve.path("tmp/protected");

    let answer = resolve.run(&[
        &resolve.path("workspace/tmp-link/protected/result.txt"),
        "1",
        &resolve.path("tmp"),
        "0",
        &protected,
        "1",
    ]);

    assert_eq!(
        answer,
        (
            114,
            String::new(),
            format!(
                "read-only extra path grant: {protected}\nresolved path: {}\n",
                target.display()
            )
        )
    );
}

/// `test_resolve_workspace_path_helper_allows_nested_read_only_extra_grant_on_read`.
#[test]
fn a_read_from_a_nested_read_only_grant_is_allowed() {
    let resolve = Resolve::new();
    let target = resolve.linked_scratch(true);

    let answer = resolve.run(&[
        &resolve.path("workspace/tmp-link/protected/result.txt"),
        "0",
        &resolve.path("tmp"),
        "0",
        &resolve.path("tmp/protected"),
        "1",
    ]);

    assert_eq!(
        answer,
        (0, format!("{}\n", target.display()), String::new())
    );
}
