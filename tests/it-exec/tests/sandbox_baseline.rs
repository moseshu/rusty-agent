//! The baseline backend's environment derivation and resource ceilings, and the scratch directory
//! a run owns.

use std::{fs, sync::Arc, time::Duration};

use ra_exec::{
    command::{ExecLimits, ExecRequest},
    sandbox::{
        ExecEnvironment,
        unix_local::{EnvInherit, EnvPattern, EnvPolicy, ResourceLimits},
    },
    session::{ExecError, ExecExecutionResult, ProcessManager},
    tmpdir::{RunTempDir, TMPDIR_ENV_VAR, TempDirError},
};
use tempfile::TempDir;

/// A parent environment with one of each interesting shape, used instead of this process's own —
/// no test may mutate the process environment while its neighbours are reading it.
fn parent_env() -> Vec<(String, String)> {
    [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/tester"),
        ("LANG", "en_US.UTF-8"),
        ("CARGO_HOME", "/home/tester/.cargo"),
        ("AWS_SECRET_ACCESS_KEY", "shh"),
        ("GITHUB_TOKEN", "ghp_shh"),
        ("openai_api_key", "sk-shh"),
        ("MY_PASSWORD", "hunter2"),
        ("DATABASE_URL", "postgres://user:pw@host/db"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

/// Runs a command to completion and returns its stdout, trimmed.
async fn stdout_of(manager: &ProcessManager, command: &str) -> String {
    let result = manager
        .execute(ExecRequest::new(command), None)
        .await
        .expect("execute");
    match result {
        ExecExecutionResult::Completed(summary) => summary.stdout().trim().to_owned(),
        other => panic!("command did not finish: {command} ({other:?})"),
    }
}

#[test]
fn test_credential_looking_variables_are_dropped_by_default() {
    let env = EnvPolicy::new().derive_from(parent_env());

    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    assert_eq!(
        env.get("CARGO_HOME").map(String::as_str),
        Some("/home/tester/.cargo"),
        "inheriting everything is what keeps arbitrary toolchains working"
    );
    for dropped in [
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "openai_api_key",
        "MY_PASSWORD",
    ] {
        assert!(
            !env.contains_key(dropped),
            "{dropped} should not reach a command the model wrote"
        );
    }
    assert!(
        env.contains_key("DATABASE_URL"),
        "the heuristic is three words on a name, and this is the documented miss: a password \
         inside a URL is not caught, which is why a host names its own secrets in `exclude`"
    );
}

#[test]
fn test_core_inherits_only_the_core_list() {
    let env = EnvPolicy::new()
        .with_inherit(EnvInherit::Core)
        .derive_from(parent_env());

    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    assert!(env.contains_key("HOME"));
    assert!(env.contains_key("LANG"));
    assert!(
        !env.contains_key("CARGO_HOME"),
        "a toolchain variable is the host's to add, not the core list's to guess"
    );
}

#[test]
fn test_none_starts_empty_and_set_still_lands() {
    let env = EnvPolicy::new()
        .with_inherit(EnvInherit::None)
        .with_set("MARKER", "value")
        .derive_from(parent_env());

    assert_eq!(env.len(), 1);
    assert_eq!(env.get("MARKER").map(String::as_str), Some("value"));
}

#[test]
fn test_set_survives_an_include_only_allowlist() {
    // The deliberate difference from upstream, which applies `set` before `include_only` and so
    // lets one half of a policy silently delete what the other half just asked for.
    let env = EnvPolicy::new()
        .with_include_only([EnvPattern::new("PATH")])
        .with_set("MARKER", "value")
        .derive_from(parent_env());

    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    assert_eq!(env.get("MARKER").map(String::as_str), Some("value"));
    assert!(!env.contains_key("HOME"));
}

#[test]
fn test_host_excludes_catch_what_the_heuristic_misses() {
    let env = EnvPolicy::new()
        .with_exclude([EnvPattern::new("DATABASE_*")])
        .derive_from(parent_env());

    assert!(!env.contains_key("DATABASE_URL"));
    assert!(env.contains_key("PATH"));
}

#[test]
fn test_patterns_match_without_case_and_only_on_star() {
    assert!(EnvPattern::new("AWS_*").matches("AWS_REGION"));
    assert!(EnvPattern::new("aws_*").matches("AWS_REGION"));
    assert!(EnvPattern::new("*_TOKEN").matches("github_token"));
    assert!(EnvPattern::new("*").matches("ANYTHING"));
    assert!(EnvPattern::new("A*B*C").matches("AxxBxxC"));
    assert!(EnvPattern::new("EXACT").matches("exact"));

    assert!(!EnvPattern::new("AWS_*").matches("MY_AWS_REGION"));
    assert!(!EnvPattern::new("A*B*C").matches("AxxBxx"));
    assert!(!EnvPattern::new("EXACT").matches("EXACTLY"));
}

#[tokio::test]
async fn test_a_spawned_command_sees_only_the_derived_environment() {
    let manager = ProcessManager::default().with_environment(
        ExecEnvironment::new().with_env_policy(
            EnvPolicy::new()
                .with_inherit(EnvInherit::None)
                .with_set("PATH", "/usr/bin:/bin")
                .with_set("MARKER", "present"),
        ),
    );

    assert_eq!(stdout_of(&manager, "echo \"$MARKER\"").await, "present");
    assert_eq!(
        stdout_of(&manager, "echo \"[$HOME]\"").await,
        "[]",
        "the child's environment is cleared before the policy's answer is installed, so a \
         variable the policy did not name is absent rather than inherited"
    );
}

#[tokio::test]
async fn test_resource_ceilings_reach_the_child() {
    // `ulimit` reports what the child inherited, which is the only way to observe that the hook
    // between fork and exec actually ran.
    let manager = ProcessManager::default().with_environment(
        ExecEnvironment::new()
            .with_resource_limits(ResourceLimits::new().with_open_files(Some(64))),
    );

    assert_eq!(
        stdout_of(&manager, "ulimit -c").await,
        "0",
        "core dumps are off by default: a crash otherwise writes the process's memory to disk"
    );
    assert_eq!(stdout_of(&manager, "ulimit -n").await, "64");
}

#[tokio::test]
async fn test_a_ceiling_above_the_hard_limit_is_taken_down_to_it() {
    // Asking for more than the host allows yields what the host allows, rather than failing the
    // spawn: the request is a ceiling, not a grant.
    let manager = ProcessManager::default().with_environment(
        ExecEnvironment::new()
            .with_resource_limits(ResourceLimits::new().with_open_files(Some(u64::MAX / 2))),
    );

    let reported = stdout_of(&manager, "ulimit -n").await;
    assert!(
        !reported.is_empty(),
        "the command ran rather than failing to spawn"
    );
}

#[test]
fn test_the_scratch_directory_is_named_from_the_run_key_and_reopens() {
    let parent = TempDir::new().expect("parent");
    let first = RunTempDir::open_in(parent.path(), "run-1").expect("create");

    assert!(first.path().is_dir());
    assert_eq!(
        first.path().parent(),
        Some(
            parent
                .path()
                .canonicalize()
                .expect("canonical parent")
                .as_path()
        )
    );
    assert!(
        first
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("run-1"))
    );

    fs::write(first.path().join("left-behind"), b"x").expect("write");

    // A resumed run addresses the same directory, which is the whole reason the name is derived
    // from the key rather than randomised.
    let second = RunTempDir::open_in(parent.path(), "run-1").expect("reopen");
    assert_eq!(first.path(), second.path());
    assert!(second.path().join("left-behind").is_file());
}

#[test]
fn test_keys_that_cannot_be_one_directory_name_are_refused() {
    let parent = TempDir::new().expect("parent");
    for key in ["", "a/b", "..", "."] {
        assert!(
            matches!(
                RunTempDir::open_in(parent.path(), key),
                Err(TempDirError::InvalidKey { .. })
            ),
            "key {key:?} should not become a directory name"
        );
    }
}

#[test]
fn test_cleanup_waits_for_its_users_and_is_idempotent() {
    let parent = TempDir::new().expect("parent");
    let scratch = RunTempDir::open_in(parent.path(), "run-2").expect("create");

    let user = scratch.use_handle().expect("register user");
    assert_eq!(scratch.users(), 1);
    let refused = scratch.cleanup().expect_err("a live user blocks removal");
    assert!(matches!(refused, TempDirError::InUse { users: 1, .. }));
    assert!(
        scratch.path().is_dir(),
        "refusing to remove must leave the directory alone, not half-remove it"
    );

    drop(user);
    assert_eq!(scratch.users(), 0);
    scratch.cleanup().expect("removal after the last user left");
    assert!(!scratch.path().exists());

    scratch
        .cleanup()
        .expect("removing what is already gone satisfies the caller's intent");
}

#[test]
fn test_cleanup_refuses_a_path_that_is_no_longer_a_directory() {
    let parent = TempDir::new().expect("parent");
    let elsewhere = TempDir::new().expect("elsewhere");
    fs::write(elsewhere.path().join("precious"), b"keep me").expect("write");

    let scratch = RunTempDir::open_in(parent.path(), "run-3").expect("create");
    fs::remove_dir_all(scratch.path()).expect("remove");
    #[cfg(unix)]
    std::os::unix::fs::symlink(elsewhere.path(), scratch.path()).expect("symlink");

    let refused = scratch
        .cleanup()
        .expect_err("a symlink is not our directory");
    assert!(matches!(refused, TempDirError::Remove { .. }));
    assert!(
        elsewhere.path().join("precious").is_file(),
        "whatever the link pointed at must survive"
    );
}

#[tokio::test]
async fn test_a_backgrounded_command_keeps_the_scratch_directory_alive() {
    let parent = TempDir::new().expect("parent");
    let scratch = Arc::new(RunTempDir::open_in(parent.path(), "run-4").expect("create"));
    let manager = ProcessManager::default()
        .with_environment(ExecEnvironment::new().with_temp_dir(Arc::clone(&scratch)));

    let expected = scratch.path().display().to_string();
    assert_eq!(
        stdout_of(&manager, &format!("echo \"${TMPDIR_ENV_VAR}\"")).await,
        expected,
        "a command is told where its run's scratch directory is"
    );

    // A command that outlives its call is exactly the case a drop-based cleanup gets wrong.
    let request = ExecRequest::new("sleep 30")
        .with_limits(ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(100)));
    let result = manager.execute(request, None).await.expect("execute");
    let session_id = match result {
        ExecExecutionResult::Yielded { session_id, .. } => session_id,
        other => panic!("sleep should have yielded, got {other:?}"),
    };

    assert_eq!(
        scratch.users(),
        1,
        "the backgrounded process still counts as a user after the call returned"
    );
    let refused = scratch
        .cleanup()
        .expect_err("removal must not pull the floor out from under a live process");
    assert!(matches!(refused, TempDirError::InUse { .. }));
    assert!(scratch.path().is_dir());

    manager.cancel(&session_id).await;
    // The handle is released by the supervisor, which is reaped shortly after the signal.
    for _ in 0..50 {
        if scratch.users() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(scratch.users(), 0, "the handle is released once reaped");
    scratch
        .cleanup()
        .expect("cleanup after the process is gone");
}

#[test]
fn reopened_handles_share_cleanup_state() {
    let parent = TempDir::new().expect("parent");
    let first = RunTempDir::open_in(parent.path(), "shared").expect("create");
    let user = first.use_handle().expect("user");
    let second = RunTempDir::open_in(parent.path(), "shared").expect("share");
    assert!(matches!(second.cleanup(), Err(TempDirError::InUse { .. })));
    drop(user);
    second.cleanup().expect("cleanup");
    assert!(first.use_handle().is_err());
}

#[test]
fn reopening_after_cleanup_gives_a_usable_directory() {
    let parent = TempDir::new().expect("parent");
    let first = RunTempDir::open_in(parent.path(), "recycled").expect("create");
    first.cleanup().expect("cleanup");

    // The cleaned state is still reachable through `first`, but it must not be handed to a caller
    // asking for a directory: an `Ok` naming a path that no longer exists refuses every handle.
    let second = RunTempDir::open_in(parent.path(), "recycled").expect("create again");
    assert!(second.path().is_dir());
    let user = second.use_handle().expect("the new directory is usable");

    assert!(
        first.use_handle().is_err(),
        "the value holding the cleaned state keeps refusing, which is what it is for"
    );
    drop(user);
    second.cleanup().expect("cleanup again");
}

#[tokio::test]
async fn a_cleaned_scratch_directory_stops_the_command_before_it_starts() {
    let parent = TempDir::new().expect("parent");
    let scratch = Arc::new(RunTempDir::open_in(parent.path(), "gone").expect("create"));
    scratch.cleanup().expect("cleanup");

    let manager = ProcessManager::default()
        .with_environment(ExecEnvironment::new().with_temp_dir(Arc::clone(&scratch)));
    let error = manager
        .execute(ExecRequest::new("echo hi"), None)
        .await
        .expect_err("no process may start without the directory it was promised");

    assert!(
        matches!(error, ExecError::Scratch { .. }),
        "a scratch failure is not a failure to start the command: {error:?}"
    );
    assert!(
        std::error::Error::source(&error).is_some(),
        "the underlying refusal stays on the chain rather than being flattened into a message"
    );
    assert!(
        manager.active_sessions().await.is_empty(),
        "a session that never got a process must not be left behind in the registry"
    );
}

#[test]
fn unowned_directory_is_not_adopted() {
    let parent = TempDir::new().expect("parent");
    let path = parent.path().join("rusty-agent-existing");
    fs::create_dir(&path).expect("create");
    fs::write(path.join("keep"), "data").expect("write");
    assert!(RunTempDir::open_in(parent.path(), "existing").is_err());
    assert!(path.join("keep").exists());
    assert!(RunTempDir::open_in(parent.path().join("missing"), "run").is_err());
}

#[tokio::test]
async fn child_cannot_raise_core_dump_limit() {
    let manager = ProcessManager::default();
    assert_eq!(
        stdout_of(&manager, "ulimit -c 1 2>/dev/null; ulimit -c").await,
        "0"
    );
}

#[test]
fn exec_lints_match_workspace_except_unsafe() {
    let root = include_str!("../../../Cargo.toml");
    let local = include_str!("../../../crates/ra-exec/Cargo.toml");
    let expected = root
        .split("[workspace.lints.rust]")
        .nth(1)
        .expect("lints")
        .split("[workspace.dependencies]")
        .next()
        .expect("end")
        .replace("[workspace.lints.clippy]", "[lints.clippy]")
        .replace(r#"unsafe_code = "forbid""#, r#"unsafe_code = "deny""#);
    let actual = local
        .split("[lints.rust]")
        .nth(1)
        .expect("lints")
        .split("[features]")
        .next()
        .expect("end");
    assert_eq!(actual.trim(), expected.trim());
}

#[test]
fn cleanup_and_registration_are_mutually_exclusive() {
    for _ in 0..50 {
        let parent = TempDir::new().expect("parent");
        let dir = Arc::new(RunTempDir::open_in(parent.path(), "race").expect("create"));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_dir = Arc::clone(&dir);
        let worker_barrier = Arc::clone(&barrier);
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            worker_dir.use_handle()
        });
        barrier.wait();
        let cleanup = dir.cleanup();
        let handle = worker.join().expect("join");
        match handle {
            Ok(handle) => {
                assert!(matches!(cleanup, Err(TempDirError::InUse { .. })));
                assert!(dir.path().is_dir());
                drop(handle);
                dir.cleanup().expect("cleanup");
            }
            Err(_) => assert!(cleanup.is_ok()),
        }
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_environment_does_not_panic() {
    use std::os::unix::ffi::OsStringExt;
    if std::env::var_os("EXEC_ENV_CHILD").is_some() {
        let env = EnvPolicy::new().derive();
        assert!(!env.contains_key("EXEC_INVALID_BYTES"));
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "non_unicode_environment_does_not_panic"])
        .env("EXEC_ENV_CHILD", "1")
        .env(
            "EXEC_INVALID_BYTES",
            std::ffi::OsString::from_vec(vec![255]),
        )
        .status()
        .expect("child");
    assert!(status.success());
}

#[test]
fn replaced_directory_is_not_removed() {
    let parent = TempDir::new().expect("parent");
    let dir = RunTempDir::open_in(parent.path(), "replace").expect("create");
    fs::rename(dir.path(), parent.path().join("original")).expect("move");
    fs::create_dir(dir.path()).expect("replacement");
    fs::write(dir.path().join("keep"), "data").expect("write");
    assert!(dir.cleanup().is_err());
    assert!(dir.path().join("keep").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn shell_exit_with_live_descendant_requires_scratch_recovery() {
    let parent = TempDir::new().expect("parent");
    let scratch = Arc::new(RunTempDir::open_in(parent.path(), "descendant").expect("scratch"));
    let manager = ProcessManager::default()
        .with_environment(ExecEnvironment::new().with_temp_dir(Arc::clone(&scratch)));
    // Redirect every pipe so completion of pipe draining cannot accidentally hide the bug.
    // The descendant waits for the test's release file, making the live interval deterministic.
    let release = parent.path().join("release");
    let finished = parent.path().join("finished");
    let command = format!(
        "(for i in $(seq 1 100); do if [ -f '{}' ]; then printf alive > '{}'; exit; fi; sleep 0.05; done) </dev/null >/dev/null 2>&1 &",
        release.display(),
        finished.display()
    );
    let result = manager
        .execute(ExecRequest::new(command).with_login(false), None)
        .await;
    let cleanup = scratch.cleanup();
    // Always release the descendant before asserting, including when the regression reappears.
    fs::write(&release, "release").expect("release");
    for _ in 0..100 {
        if finished.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(result.is_ok());
    assert!(finished.exists(), "the descendant survived its shell");
    assert!(matches!(
        cleanup,
        Err(TempDirError::RecoveryRequired { .. })
    ));
    assert!(scratch.path().is_dir());
    // A completed command and a reused process-group ID must not clear an earlier recovery mark.
    stdout_of(&manager, "true").await;
    assert!(matches!(
        scratch.cleanup(),
        Err(TempDirError::RecoveryRequired { .. })
    ));

    // The mark is reportable rather than only discoverable by attempting a removal and reading the
    // error back.
    assert!(scratch.recovery_required());

    // A host's verification answers for descendants this executor never tracked. It says nothing
    // about a process the executor is still holding a handle for, so that gate stays shut.
    let user = scratch
        .use_handle()
        .expect("a marked directory still serves its run");
    assert!(matches!(
        scratch.cleanup_after_recovery(),
        Err(TempDirError::InUse { users: 1, .. })
    ));
    drop(user);

    // Having verified, the host reclaims through the same identity check and open-handle removal
    // that ordinary cleanup uses — which is the whole reason this method exists rather than leaving
    // the host to `remove_dir_all` the path itself.
    scratch
        .cleanup_after_recovery()
        .expect("a verified host may reclaim");
    assert!(!scratch.path().exists());
}
