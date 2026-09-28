//! `ra-sandbox::docker`: the session — running commands in a container and moving files through them.
//!
//! Ports the session half of the reference's `tests/sandbox/test_docker.py`. The daemon is the fake
//! in `support/docker_fake.rs`, which, like the reference's `_HostBackedDockerSession`, answers the
//! session's commands from a host directory standing in for the container's filesystem.

#[path = "support/docker_fake.rs"]
mod docker_fake;

use std::sync::Arc;
use std::time::Duration;

use docker_fake::{
    FakeDocker, archive_member_names, docker_state, host_backed_session, tar_bytes,
    tar_symlink_bytes,
};
use ra_core::sandbox::{
    AzureBlobMount, Entry, ErrorCode, ExecRequest, Manifest, Mount, MountPattern, MountProvider,
    MountStrategy, MountpointOptions, PosixPath, S3Mount, SandboxPathGrant, SandboxSession,
    SessionPath, ShellInvocation, User,
};
use ra_sandbox::docker::{
    DockerSandboxSession, ExecRunOutput, LENGTH_FRAMED_STDIN_SCRIPT, manifest_requires_fuse,
};
use ra_sandbox::materialize::ManifestApplier;
use serde_json::json;

fn workspace_manifest() -> Manifest {
    Manifest::new().with_root("/workspace")
}

/// A mount attached from inside the container, standing in for the reference's `_RecordingMount`:
/// persisting never activates it, so only where it attaches matters.
fn in_container_mount() -> Mount {
    Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            ..S3Mount::default()
        }),
        MountStrategy::InContainer {
            pattern: MountPattern::Mountpoint(MountpointOptions::default()),
        },
    )
    .expect("a supported mount")
}

/// An S3 mount attached by the rclone volume driver, carrying credentials when `secret` is given.
fn volume_mount(secret: Option<&str>) -> Mount {
    Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            access_key_id: secret.map(|_| "access-key".to_owned()),
            secret_access_key: secret.map(str::to_owned),
            ..S3Mount::default()
        }),
        MountStrategy::docker_volume("rclone"),
    )
    .expect("a supported mount")
}

fn exec_calls(fake: &FakeDocker) -> Vec<docker_fake::ExecCall> {
    fake.execs.lock().expect("execs").clone()
}

// --- persisting the workspace -------------------------------------------------------------

#[tokio::test]
async fn persist_workspace_stages_a_copy_before_downloading_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("README.md"), "hello from workspace").expect("write");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let archive = session.persist_workspace().await.expect("persist");
    let names = archive_member_names(&archive);

    assert!(
        !fake
            .archive_calls
            .lock()
            .expect("calls")
            .contains(&"/workspace".to_owned())
    );
    assert!(names.contains(&".".to_owned()), "{names:?}");
    assert!(names.contains(&"README.md".to_owned()), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|name| name == "workspace" || name.starts_with("workspace/"))
    );
}

#[tokio::test]
async fn persist_workspace_removes_the_staged_copy_after_the_archive_is_read() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("README.md"), "hello").expect("write");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    session.persist_workspace().await.expect("persist");
    session.after_stop().await;

    let staging = tmp.path().join("tmp/sandbox-docker-archive");
    let left: Vec<_> = std::fs::read_dir(&staging)
        .map(|entries| {
            entries
                .map(|entry| entry.expect("entry").file_name())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "staged copies left behind: {left:?}");
    assert_eq!(session.pending_cleanup_count().await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_drains_deferred_cleanup_before_stopping_the_container() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    session.persist_workspace().await.expect("persist");
    session.shutdown().await.expect("shutdown");

    let events = fake.events.lock().expect("events").clone();
    let cleanup = events
        .iter()
        .position(|event| event.starts_with("exec:rm -rf -- /tmp/sandbox-docker-archive/"))
        .expect("the staged copy was removed");
    let stop = events
        .iter()
        .position(|event| event == "stop:container")
        .expect("the container was stopped");
    assert!(cleanup < stop, "{events:?}");
    assert_eq!(session.pending_cleanup_count().await, 0);
}

#[tokio::test]
async fn after_stop_bounds_the_wait_for_deferred_cleanup() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    let session = session.with_deferred_cleanup_timeout(Duration::from_millis(10));

    session.persist_workspace().await.expect("persist");
    fake.hanging_programs
        .lock()
        .expect("hanging")
        .insert("rm".to_owned());

    tokio::time::timeout(Duration::from_millis(500), session.after_stop())
        .await
        .expect("the wait is bounded");
}

#[tokio::test]
async fn persist_workspace_leaves_out_ephemeral_entries() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("keep.txt"), "keep").expect("write");
    std::fs::write(workspace.join("skip.txt"), "skip").expect("write");
    let manifest =
        workspace_manifest().with_entry("skip.txt", Entry::file(b"skip".to_vec()).ephemeral(true));
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    assert!(names.contains(&"keep.txt".to_owned()));
    assert!(!names.contains(&"skip.txt".to_owned()));
}

#[tokio::test]
async fn persist_workspace_prunes_mount_paths_without_touching_the_mount() {
    let tmp = tempfile::tempdir().expect("tmp");
    let mount_dir = tmp.path().join("workspace/repo/mount");
    std::fs::create_dir_all(&mount_dir).expect("mkdir");
    std::fs::write(mount_dir.join("remote.txt"), "remote").expect("write");
    let manifest = workspace_manifest().with_entry(
        "repo",
        Entry::dir().with_child("mount", Entry::mount(in_container_mount())),
    );
    let (fake, session) = host_backed_session(tmp.path(), manifest);

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    assert!(
        !names
            .iter()
            .any(|name| name.ends_with("repo/mount/remote.txt"))
    );
    // Nothing was unmounted or remounted: only the copy was pruned.
    assert!(mount_dir.join("remote.txt").exists());
    assert!(
        !exec_calls(&fake)
            .iter()
            .any(|call| call.cmd.iter().any(|arg| arg.contains("mount-s3")))
    );
}

#[tokio::test]
async fn persist_workspace_skips_a_workspace_root_mount_without_reading_through_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("remote.txt"), "remote").expect("write");
    let manifest = workspace_manifest().with_entry(
        "root-mount",
        Entry::mount(in_container_mount().at("/workspace")),
    );
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    assert!(names.contains(&".".to_owned()));
    assert!(!names.contains(&"remote.txt".to_owned()));
}

#[tokio::test]
async fn persist_workspace_copies_the_siblings_of_a_pruned_mount() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = tmp.path().join("workspace/repo");
    std::fs::create_dir_all(repo.join("mount")).expect("mkdir");
    std::fs::write(repo.join("keep.txt"), "keep").expect("write");
    std::fs::write(repo.join("mount/remote.txt"), "remote").expect("write");
    let manifest = workspace_manifest().with_entry(
        "repo",
        Entry::dir().with_child("mount", Entry::mount(in_container_mount())),
    );
    let (fake, session) = host_backed_session(tmp.path(), manifest);

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    let copies = fake.copies.lock().expect("copies").clone();
    assert!(
        copies.contains(&"/workspace/repo/keep.txt".to_owned()),
        "{copies:?}"
    );
    assert!(
        !copies
            .iter()
            .any(|path| path.starts_with("/workspace/repo/mount"))
    );
    assert!(names.contains(&"repo/keep.txt".to_owned()));
    assert!(!names.contains(&"repo/mount/remote.txt".to_owned()));
}

#[tokio::test]
async fn persist_workspace_leaves_out_paths_registered_at_runtime() {
    let tmp = tempfile::tempdir().expect("tmp");
    let logs = tmp.path().join("workspace/logs");
    std::fs::create_dir_all(&logs).expect("mkdir");
    std::fs::write(logs.join("keep.txt"), "keep").expect("write");
    std::fs::write(logs.join("events.jsonl"), "skip").expect("write");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    session
        .register_persist_workspace_skip_path("logs/events.jsonl".into())
        .expect("register");

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    assert!(names.contains(&"logs/keep.txt".to_owned()));
    assert!(!names.contains(&"logs/events.jsonl".to_owned()));
}

#[tokio::test]
async fn persist_workspace_leaves_out_an_explicit_mount_path() {
    let tmp = tempfile::tempdir().expect("tmp");
    let actual = tmp.path().join("workspace/actual");
    std::fs::create_dir_all(&actual).expect("mkdir");
    std::fs::write(actual.join("remote.txt"), "remote").expect("write");
    let manifest =
        workspace_manifest().with_entry("logical", Entry::mount(in_container_mount().at("actual")));
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    assert!(!names.contains(&"actual/remote.txt".to_owned()));
    assert_eq!(
        std::fs::read_to_string(actual.join("remote.txt")).expect("read"),
        "remote"
    );
}

#[tokio::test]
async fn persist_workspace_prunes_nested_mount_paths() {
    let tmp = tempfile::tempdir().expect("tmp");
    let child = tmp.path().join("workspace/repo/sub");
    std::fs::create_dir_all(&child).expect("mkdir");
    std::fs::write(child.join("remote.txt"), "remote").expect("write");
    let manifest = workspace_manifest()
        .with_entry("repo", Entry::mount(in_container_mount()))
        .with_entry("child", Entry::mount(in_container_mount().at("repo/sub")));
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let names = archive_member_names(&session.persist_workspace().await.expect("persist"));

    assert!(!names.contains(&"repo/remote.txt".to_owned()));
    assert!(!names.contains(&"repo/sub/remote.txt".to_owned()));
}

#[tokio::test]
async fn a_direct_persist_failure_under_protected_mounts_is_redacted() {
    let sentinel = "direct-docker-persist-secret";
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let manifest =
        workspace_manifest().with_entry("data", Entry::mount(volume_mount(Some(sentinel))));
    let (fake, session) = host_backed_session(tmp.path(), manifest);
    *fake.archive_error.lock().expect("error") = Some(ra_sandbox::docker::DockerApiError::api(
        400,
        format!("provider echoed {sentinel}"),
    ));

    let error = session
        .persist_workspace()
        .await
        .expect_err("the download fails");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert!(
        error.message().contains("protected mount configuration"),
        "{error}"
    );
    assert!(error.context().is_empty(), "{:?}", error.context());
    assert!(!format!("{error:?}").contains(sentinel));
    assert!(std::error::Error::source(&error).is_none());
}

// --- reading and writing files -------------------------------------------------------------

#[tokio::test]
async fn read_and_write_refuse_paths_outside_the_workspace_root() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let read = session
        .read("../secret.txt".into(), None)
        .await
        .expect_err("refused");
    assert!(read.message().contains("must not escape root"), "{read}");
    let write = session
        .write("../secret.txt".into(), b"nope".to_vec(), None)
        .await
        .expect_err("refused");
    assert!(write.message().contains("must not escape root"), "{write}");
}

#[tokio::test]
async fn read_returns_the_file_bytes_without_the_archive_api() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("hello.bin"), b"hello\x00world").expect("write");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let data = session.read("hello.bin".into(), None).await.expect("read");

    assert_eq!(data, b"hello\x00world");
    assert!(fake.archive_calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn read_of_a_missing_path_is_not_found() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let error = session
        .read("missing.txt".into(), None)
        .await
        .expect_err("missing");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
}

#[tokio::test]
async fn read_of_an_existing_unreadable_path_is_an_archive_error() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace/directory")).expect("mkdir");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let error = session
        .read("directory".into(), None)
        .await
        .expect_err("unreadable");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
}

#[tokio::test]
async fn read_with_an_undecided_probe_is_an_archive_error() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    *fake.read_probe_exit_code.lock().expect("probe") = Some(2);

    let error = session
        .read("inaccessible/missing.txt".into(), None)
        .await
        .expect_err("undecided");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
}

#[tokio::test]
async fn read_probe_runs_as_the_requested_user() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let error = session
        .read("missing.txt".into(), Some(User::new("sandbox-user")))
        .await
        .expect_err("missing");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
    assert_eq!(
        *fake.read_probe_users.lock().expect("users"),
        vec![Some("sandbox-user".to_owned())]
    );
}

#[tokio::test]
async fn path_validation_keeps_a_safe_leaf_symlink_as_written() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("target.txt"), "hello").expect("write");
    std::os::unix::fs::symlink(workspace.join("target.txt"), workspace.join("link.txt"))
        .expect("symlink");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let normalized = session
        .validate_path_access("link.txt".into(), false)
        .await
        .expect("inside the workspace");

    assert_eq!(normalized.as_str(), "/workspace/link.txt");
}

#[tokio::test]
async fn read_uses_the_sandbox_side_of_a_split_path_grant() {
    let tmp = tempfile::tempdir().expect("tmp");
    let container = tmp.path().join("container");
    std::fs::create_dir_all(container.join("workspace")).expect("mkdir");
    std::fs::create_dir_all(container.join("tmp")).expect("mkdir");
    let native_source = tmp.path().join("native-source");
    std::fs::create_dir_all(&native_source).expect("mkdir");
    std::fs::write(container.join("tmp/result.txt"), "scratch output").expect("write");
    let manifest = workspace_manifest().with_path_grant(
        SandboxPathGrant::new("/tmp")
            .expect("grant")
            .with_host_path(&native_source.to_string_lossy())
            .expect("host path"),
    );
    let (_fake, session) = host_backed_session(&container, manifest);

    let data = session
        .read("/tmp/result.txt".into(), None)
        .await
        .expect("read");

    assert_eq!(data, b"scratch output");
}

#[tokio::test]
async fn write_refuses_a_read_only_extra_path_grant() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    std::fs::create_dir_all(tmp.path().join("tmp")).expect("mkdir");
    let manifest = workspace_manifest().with_path_grant(
        SandboxPathGrant::new("/tmp")
            .expect("grant")
            .read_only(true),
    );
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let error = session
        .write("/tmp/result.txt".into(), b"scratch output".to_vec(), None)
        .await
        .expect_err("read-only");

    assert_eq!(
        error.message(),
        "failed to write archive for path: /tmp/result.txt"
    );
    assert_eq!(
        serde_json::to_value(error.context()).expect("context"),
        json!({
            "path": "/tmp/result.txt",
            "reason": "read_only_extra_path_grant",
            "grant_path": "/tmp",
        })
    );
}

#[tokio::test]
async fn write_refuses_a_workspace_symlink_into_a_read_only_grant() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    let extra = tmp.path().join("tmp");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(&extra).expect("mkdir");
    std::os::unix::fs::symlink(&extra, workspace.join("tmp-link")).expect("symlink");
    let manifest = workspace_manifest().with_path_grant(
        SandboxPathGrant::new("/tmp")
            .expect("grant")
            .read_only(true),
    );
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let error = session
        .write(
            "tmp-link/result.txt".into(),
            b"scratch output".to_vec(),
            None,
        )
        .await
        .expect_err("read-only");

    assert_eq!(
        error.message(),
        "failed to write archive for path: /workspace/tmp-link/result.txt"
    );
    assert_eq!(
        serde_json::to_value(error.context()).expect("context"),
        json!({
            "path": "/workspace/tmp-link/result.txt",
            "reason": "read_only_extra_path_grant",
            "grant_path": "/tmp",
            "resolved_path": "/tmp/result.txt",
        })
    );
}

#[tokio::test]
async fn write_refuses_a_workspace_symlink_into_a_nested_read_only_grant() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    let extra = tmp.path().join("tmp");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(extra.join("protected")).expect("mkdir");
    std::os::unix::fs::symlink(&extra, workspace.join("tmp-link")).expect("symlink");
    let manifest = workspace_manifest()
        .with_path_grant(SandboxPathGrant::new("/tmp").expect("grant"))
        .with_path_grant(
            SandboxPathGrant::new("/tmp/protected")
                .expect("grant")
                .read_only(true),
        );
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    let error = session
        .write(
            "tmp-link/protected/result.txt".into(),
            b"scratch output".to_vec(),
            None,
        )
        .await
        .expect_err("read-only");

    assert_eq!(
        serde_json::to_value(error.context()).expect("context"),
        json!({
            "path": "/workspace/tmp-link/protected/result.txt",
            "reason": "read_only_extra_path_grant",
            "grant_path": "/tmp/protected",
            "resolved_path": "/tmp/protected/result.txt",
        })
    );
}

#[tokio::test]
async fn rm_unlinks_a_safe_leaf_symlink_rather_than_its_target() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("target.txt"), "hello").expect("write");
    std::os::unix::fs::symlink(workspace.join("target.txt"), workspace.join("link.txt"))
        .expect("symlink");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    session
        .rm("link.txt".into(), false, None)
        .await
        .expect("rm");

    assert_eq!(
        std::fs::read_to_string(workspace.join("target.txt")).expect("read"),
        "hello"
    );
    assert!(std::fs::symlink_metadata(workspace.join("link.txt")).is_err());
}

#[tokio::test]
async fn file_operations_refuse_a_symlink_that_escapes_the_workspace() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(&outside).expect("mkdir");
    std::fs::write(outside.join("secret.txt"), "secret").expect("write");
    std::os::unix::fs::symlink(&outside, workspace.join("link")).expect("symlink");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let escaped = |error: ra_core::sandbox::SandboxError| {
        assert!(error.message().contains("must not escape root"), "{error}");
    };
    escaped(
        session
            .read("link/secret.txt".into(), None)
            .await
            .expect_err("read"),
    );
    escaped(
        session
            .write("link/secret.txt".into(), b"overwrite".to_vec(), None)
            .await
            .expect_err("write"),
    );
    escaped(session.ls("link".into(), None).await.expect_err("ls"));
    escaped(
        session
            .mkdir("link/newdir".into(), true, None)
            .await
            .expect_err("mkdir"),
    );
    escaped(
        session
            .rm("link/secret.txt".into(), false, None)
            .await
            .expect_err("rm"),
    );
}

#[tokio::test]
async fn write_streams_through_a_staging_file_and_lands_in_place() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    session
        .write("nested/out.txt".into(), b"payload".to_vec(), None)
        .await
        .expect("write");

    assert_eq!(
        std::fs::read(tmp.path().join("workspace/nested/out.txt")).expect("read"),
        b"payload"
    );
    let attached = fake.attached.lock().expect("attached").clone();
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].stdin, b"payload");
    assert_eq!(attached[0].request.workdir(), None);
}

/// Text reads a backslash as a separator and a path keeps it, as the reference's remote session
/// reads a `str` and a `Path`: the path lands as one file whose name holds the backslash.
#[tokio::test]
async fn a_path_keeps_its_backslash_where_text_reads_a_separator() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (_fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    let typed = PosixPath::new("notes\\draft.txt");

    assert_eq!(
        session
            .validate_path_access(SessionPath::Posix(&typed), true)
            .await
            .expect("path")
            .as_str(),
        "/workspace/notes\\draft.txt"
    );
    assert_eq!(
        session
            .validate_path_access("notes\\draft.txt".into(), true)
            .await
            .expect("text")
            .as_str(),
        "/workspace/notes/draft.txt"
    );

    session
        .write(SessionPath::Posix(&typed), b"typed".to_vec(), None)
        .await
        .expect("write the path");
    session
        .write("notes\\draft.txt".into(), b"text".to_vec(), None)
        .await
        .expect("write the text");

    let workspace = tmp.path().join("workspace");
    assert_eq!(
        std::fs::read(workspace.join("notes\\draft.txt")).expect("one file"),
        b"typed"
    );
    assert_eq!(
        std::fs::read(workspace.join("notes/draft.txt")).expect("a nested file"),
        b"text"
    );
}

/// What materialization writes is a path, as the reference's `dest / child` is: a host file whose
/// name holds a backslash arrives as one file, on a backend that would read the same characters as
/// text as a separator.
#[tokio::test]
async fn a_copied_host_file_with_a_backslash_in_its_name_arrives_as_one_file() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let sources = tempfile::tempdir().expect("sources");
    std::fs::create_dir_all(sources.path().join("assets")).expect("mkdir");
    std::fs::write(sources.path().join("assets/a\\b.txt"), b"one file").expect("write");
    let manifest =
        workspace_manifest().with_entry("public", Entry::local_dir(Some("assets".to_owned())));
    let (_fake, session) = host_backed_session(tmp.path(), manifest.clone());

    ManifestApplier::new(Arc::new(session), sources.path().to_path_buf())
        .apply_manifest(&manifest, false)
        .await
        .expect("materialize");

    let public = tmp.path().join("workspace/public");
    assert_eq!(
        std::fs::read(public.join("a\\b.txt")).expect("one file"),
        b"one file"
    );
    assert!(!public.join("a").exists());
}

#[tokio::test]
async fn write_as_another_user_is_done_by_that_user() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    session
        .write(
            "deep/dir/out.txt".into(),
            b"payload".to_vec(),
            Some(User::new("sandbox-user")),
        )
        .await
        .expect("write");

    let attached = fake.attached.lock().expect("attached").clone();
    assert_eq!(attached[0].request.user(), Some("sandbox-user"));
    assert_eq!(
        attached[0].request.cmd()[5..],
        [
            "sh",
            "-lc",
            r#"mkdir -p "$(dirname "$1")" && cat > "$1""#,
            "sh",
            "/workspace/deep/dir/out.txt"
        ]
    );
    assert_eq!(
        std::fs::read(tmp.path().join("workspace/deep/dir/out.txt")).expect("read"),
        b"payload"
    );
}

#[test]
fn manifest_requires_fuse_finds_nested_mounts() {
    let azure = Mount::new(
        MountProvider::AzureBlob(AzureBlobMount {
            account: "account".to_owned(),
            container: "container".to_owned(),
            ..AzureBlobMount::default()
        }),
        MountStrategy::InContainer {
            pattern: MountPattern::Fuse(ra_core::sandbox::FuseOptions::default()),
        },
    )
    .expect("a supported mount");
    let manifest = Manifest::new().with_entry(
        "workspace",
        Entry::dir().with_child("mount", Entry::mount(azure)),
    );

    assert!(manifest_requires_fuse(&manifest));
}

// --- hydrating the workspace ---------------------------------------------------------------

#[tokio::test]
async fn hydrate_workspace_refuses_unsafe_members() {
    for (member, reason) in [
        ("/etc/passwd", "absolute path"),
        ("../escape.txt", "parent traversal"),
    ] {
        let tmp = tempfile::tempdir().expect("tmp");
        let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

        let error = session
            .hydrate_workspace(tar_bytes(&[member]))
            .await
            .expect_err("unsafe");

        assert_eq!(
            error.message(),
            "failed to write archive for path: /workspace"
        );
        assert_eq!(
            serde_json::to_value(error.context()).expect("context"),
            json!({"path": "/workspace", "reason": reason, "member": member})
        );
        assert!(fake.attached.lock().expect("attached").is_empty());
    }
}

#[tokio::test]
async fn hydrate_workspace_refuses_a_symlink_at_the_workspace_root() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());

    let error = session
        .hydrate_workspace(tar_symlink_bytes(".", "/tmp/outside"))
        .await
        .expect_err("unsafe");

    assert_eq!(
        serde_json::to_value(error.context()).expect("context"),
        json!({"path": "/workspace", "reason": "archive root symlink", "member": "."})
    );
    assert!(
        fake.attached.lock().expect("attached").is_empty(),
        "an unsafe archive must be refused before tar runs"
    );
}

#[tokio::test]
async fn hydrate_workspace_length_frames_the_archive_into_tar() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    let archive = tar_bytes(&["hello.txt"]);

    session
        .hydrate_workspace(archive.clone())
        .await
        .expect("hydrate");

    let attached = fake.attached.lock().expect("attached").clone();
    assert_eq!(attached.len(), 1);
    let framed = attached[0].request.cmd();
    assert_eq!(
        framed,
        [
            "sh".to_owned(),
            "-c".to_owned(),
            LENGTH_FRAMED_STDIN_SCRIPT.to_owned(),
            "sh".to_owned(),
            archive.len().to_string(),
            "tar".to_owned(),
            "-x".to_owned(),
            "-C".to_owned(),
            "/workspace".to_owned(),
        ]
    );
    assert!(framed[2].contains(r#"head -c "$n""#));
    assert!(framed[2].contains("head -c 1") && framed[2].contains("exit 98"));
    assert_eq!(attached[0].stdin, archive);
    assert!(attached[0].request.stdin());
    assert_eq!(
        std::fs::read(tmp.path().join("workspace/hello.txt")).expect("read"),
        b"pwned"
    );
}

// --- running commands ----------------------------------------------------------------------

/// A session over a fake with no host directory: every command succeeds.
fn command_session(root_ready: bool) -> (Arc<FakeDocker>, DockerSandboxSession) {
    let fake = Arc::new(
        FakeDocker::new().with_container("container", json!({"State": {"Status": "running"}})),
    );
    let state =
        docker_state(workspace_manifest(), "container").with_workspace_root_ready(root_ready);
    let session = DockerSandboxSession::new(fake.clone(), state).expect("a docker state");
    (fake, session)
}

fn no_shell(command: &[&str]) -> ExecRequest {
    ExecRequest::new(command.iter().map(|part| (*part).to_owned()))
        .with_shell(ShellInvocation::None)
}

#[tokio::test]
async fn a_timed_out_command_is_killed_by_its_command_line() {
    let (fake, session) = command_session(false);
    fake.hanging_programs
        .lock()
        .expect("hanging")
        .insert("sleep".to_owned());

    for seconds in ["10", "20"] {
        let error = session
            .exec(no_shell(&["sleep", seconds]).with_timeout_s(0.01))
            .await
            .expect_err("timed out");
        assert_eq!(error.error_code(), ErrorCode::ExecTimeout);
    }

    let kills: Vec<_> = exec_calls(&fake)
        .into_iter()
        .filter(|call| call.cmd[0] == "sh")
        .collect();
    assert_eq!(
        kills,
        vec![
            docker_fake::ExecCall {
                cmd: vec![
                    "sh".to_owned(),
                    "-lc".to_owned(),
                    "pkill -f -- 'sleep 10' >/dev/null 2>&1 || true".to_owned(),
                ],
                workdir: None,
                user: None,
            },
            docker_fake::ExecCall {
                cmd: vec![
                    "sh".to_owned(),
                    "-lc".to_owned(),
                    "pkill -f -- 'sleep 20' >/dev/null 2>&1 || true".to_owned(),
                ],
                workdir: None,
                user: None,
            },
        ]
    );
}

#[tokio::test]
async fn commands_run_without_a_working_directory_until_the_workspace_is_ready() {
    let (fake, session) = command_session(false);

    let result = session
        .exec(no_shell(&["find", "."]).with_timeout_s(0.01))
        .await
        .expect("exec");

    assert!(result.ok());
    assert_eq!(
        exec_calls(&fake),
        vec![docker_fake::ExecCall {
            cmd: vec!["find".to_owned(), ".".to_owned()],
            workdir: None,
            user: None,
        }]
    );
}

#[tokio::test]
async fn a_missing_exit_code_is_a_retry_safe_transport_error() {
    let (fake, session) = command_session(false);
    fake.on_exec(|_| {
        Ok(ExecRunOutput::new(
            b"partial stdout".to_vec(),
            b"partial stderr".to_vec(),
            None,
        ))
    });

    let error = session
        .exec(no_shell(&["find", "."]).with_timeout_s(0.01))
        .await
        .expect_err("no exit code");

    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(
        serde_json::to_value(error.context()).expect("context"),
        json!({
            "command": ["find", "."],
            "command_str": "find .",
            "reason": "missing_exit_code",
            "stdout": "partial stdout",
            "stderr": "partial stderr",
            "workdir": null,
            "retry_safe": true,
        })
    );
}

#[tokio::test]
async fn commands_run_in_the_manifest_root_once_the_workspace_is_ready() {
    let (fake, session) = command_session(true);

    session
        .exec(no_shell(&["find", "."]).with_timeout_s(0.01))
        .await
        .expect("exec");

    assert_eq!(
        exec_calls(&fake),
        vec![docker_fake::ExecCall {
            cmd: vec!["find".to_owned(), ".".to_owned()],
            workdir: Some("/workspace".to_owned()),
            user: None,
        }]
    );
}

#[tokio::test]
async fn another_user_is_handed_to_docker_rather_than_to_sudo() {
    let (fake, session) = command_session(false);

    let result = session
        .exec(
            ExecRequest::new(["whoami".to_owned()])
                .with_timeout_s(0.01)
                .as_user(User::new("sandbox-user")),
        )
        .await
        .expect("exec");

    assert!(result.ok());
    assert_eq!(
        exec_calls(&fake),
        vec![docker_fake::ExecCall {
            cmd: vec!["sh".to_owned(), "-lc".to_owned(), "whoami".to_owned()],
            workdir: None,
            user: Some("sandbox-user".to_owned()),
        }]
    );
}

#[tokio::test]
async fn a_published_port_resolves_to_its_host_binding() {
    let fake = Arc::new(FakeDocker::new().with_container(
        "container",
        json!({
            "State": {"Status": "running"},
            "NetworkSettings": {"Ports": {"8765/tcp": [{"HostIp": "127.0.0.1", "HostPort": "45123"}]}},
        }),
    ));
    let state = docker_state(workspace_manifest(), "container")
        .with_exposed_ports([8765])
        .expect("ports");
    let session = DockerSandboxSession::new(fake, state).expect("a docker state");

    let endpoint = session.resolve_exposed_port(8765).await.expect("resolved");

    assert_eq!(endpoint.host, "127.0.0.1");
    assert_eq!(endpoint.port, 45123);
    assert!(!endpoint.tls);
}

#[tokio::test]
async fn exists_is_false_for_a_container_that_is_gone() {
    let fake = Arc::new(FakeDocker::new());
    let session = DockerSandboxSession::new(fake, docker_state(workspace_manifest(), "missing"))
        .expect("a docker state");

    assert!(!session.exists().await.expect("asked"));
}

#[tokio::test]
async fn clearing_the_workspace_on_resume_keeps_nested_volume_mounts() {
    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("a/b")).expect("mkdir");
    std::fs::write(workspace.join("a/b/remote.txt"), "remote").expect("write");
    std::fs::write(workspace.join("a/local.txt"), "local").expect("write");
    std::fs::write(workspace.join("root.txt"), "root").expect("write");
    let manifest = workspace_manifest().with_entry("a/b", Entry::mount(volume_mount(None)));
    let (_fake, session) = host_backed_session(tmp.path(), manifest);

    // Restoring clears the workspace first; the snapshot that stores nothing then has nothing to
    // restore, which is not what this test is about.
    let _ = session.restore_snapshot().await;

    assert!(workspace.join("a/b/remote.txt").exists());
    assert!(!workspace.join("a/local.txt").exists());
    assert!(!workspace.join("root.txt").exists());
}

#[tokio::test]
async fn a_transient_daemon_failure_while_persisting_is_retried() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    *fake.archive_error.lock().expect("error") =
        Some(ra_sandbox::docker::DockerApiError::api(503, "daemon busy"));

    let error = session
        .persist_workspace()
        .await
        .expect_err("never recovers");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert_eq!(error.retryable(), Some(true));
    assert_eq!(fake.archive_calls.lock().expect("calls").len(), 3);
}

#[tokio::test]
async fn a_permanent_daemon_failure_while_persisting_is_not_retried() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("workspace")).expect("mkdir");
    let (fake, session) = host_backed_session(tmp.path(), workspace_manifest());
    *fake.archive_error.lock().expect("error") = Some(
        ra_sandbox::docker::DockerApiError::not_found("no such container"),
    );

    let error = session.persist_workspace().await.expect_err("gone");

    assert_eq!(error.retryable(), Some(false));
    assert_eq!(fake.archive_calls.lock().expect("calls").len(), 1);
}

/// `test_parse_ls_la_rejects_special_permission_bits_in_wrong_position`, through a session: a row
/// whose mode does not read fails the listing, as the reference's `ls` lets the parser's error out,
/// rather than leaving the entry out of an answer that would then look complete.
#[tokio::test]
async fn a_listing_row_whose_mode_does_not_read_fails_the_listing() {
    let (fake, session) = command_session(true);
    fake.on_exec(|request| {
        let listing = request.cmd().first().is_some_and(|program| program == "ls");
        let stdout = if listing {
            b"-rw-r--r-- 1 root root 1 Jan 1 00:00 fine.txt\n-rwTr--r-- 1 root root 1 Jan 1 00:00 odd\n"
                .to_vec()
        } else {
            // What the path resolver prints for a path it accepts: the path, resolved.
            format!("{}\n", request.cmd().get(2).map_or("", String::as_str)).into_bytes()
        };
        Ok(ExecRunOutput::new(stdout, Vec::new(), Some(0)))
    });

    let error = session
        .ls("docs".into(), None)
        .await
        .expect_err("the listing does not read");

    // The command got through and succeeded; its output did not read. That is neither a transport
    // failure nor one that another try would change, and it is recorded as the reference records
    // its parser's `ValueError`: no reference code, that type name.
    assert_eq!(error.error_code(), ErrorCode::ListingUnreadable);
    assert_eq!(error.retryable(), Some(false));
    assert_eq!(error.error_code().reference_code(), None);
    assert_eq!(error.error_code().reference_type_name(), "ValueError");
    assert!(error.message().starts_with("invalid exec flag"), "{error}");
    assert_eq!(error.context()["path"], "/workspace/docs");
}
