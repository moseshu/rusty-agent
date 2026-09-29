//! `ra-sandbox::mounts`: taking mounts down around an operation, and what the builtin strategies do.
//!
//! The first half follows the reference's `test_mount_lifecycle.py`: the order mounts come off and go
//! back on, which failure wins when several happen, and what happens to a transition that is under
//! way when its caller gives up. A caller giving up is a dropped future here — the task awaiting it
//! is aborted — and the assertions are about what the detached work still does afterwards.
//!
//! The second half is the strategy-level part of `test_mounts.py`: the credential boundary refusing
//! before anything runs, and a Docker-volume mount on a backend that does not attach volumes.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, DiscriminatedPayload, Entry, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest,
    MaterializedFile, Mount, MountPattern, MountProvider, MountStrategy, MountpointOptions, OpName,
    PosixPath, S3Mount, SandboxError, SandboxResult, SandboxSession, SandboxSessionState,
    SessionPath, SessionResources, Snapshot,
};
use ra_sandbox::materialize::ManifestApplier;
use ra_sandbox::mounts::{
    ArchiveErrorKind, BuiltinMountLifecycle, EphemeralMountRemoval, MountLifecycle,
    restore_detached_mounts, with_ephemeral_mounts_removed,
};
use serde_json::json;
use tokio::sync::Notify;

type Events = Arc<Mutex<Vec<String>>>;

fn push(events: &Events, event: impl Into<String>) {
    events.lock().expect("events").push(event.into());
}

fn snapshot_of(events: &Events) -> Vec<String> {
    events.lock().expect("events").clone()
}

/// Waits until `events` reads `expected`, for work a dropped caller left running.
async fn settles_to(events: &Events, expected: &[&str]) {
    let expected: Vec<String> = expected.iter().map(|event| (*event).to_owned()).collect();
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if snapshot_of(events) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "expected {expected:?}, saw {:?}",
        snapshot_of(events)
    );
}

// --- a session that counts shutdowns and records what it was asked to run -------------------------

struct Session {
    backend: &'static str,
    state: SandboxSessionState,
    resources: SessionResources,
    events: Events,
    commands: Mutex<Vec<Vec<String>>>,
    writes: Mutex<Vec<String>>,
    shutdowns: AtomicUsize,
    /// A shutdown waits for this before it finishes.
    shutdown_gate: Option<Arc<Notify>>,
    shutdown_started: Notify,
    shutdown_fails: bool,
    volume_mounts: bool,
}

impl Session {
    fn new(manifest: Manifest, events: &Events) -> Self {
        Self {
            backend: "recording",
            state: SandboxSessionState::new("recording", Snapshot::noop(), manifest),
            resources: SessionResources::new(),
            events: Arc::clone(events),
            commands: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            shutdowns: AtomicUsize::new(0),
            shutdown_gate: None,
            shutdown_started: Notify::new(),
            shutdown_fails: false,
            volume_mounts: false,
        }
    }

    /// A session of the backend that attaches Docker volumes, as far as the boundary can tell.
    fn docker(manifest: Manifest, events: &Events) -> Self {
        Self {
            backend: "docker",
            state: SandboxSessionState::new("docker", Snapshot::noop(), manifest),
            volume_mounts: true,
            ..Self::new(Manifest::new(), events)
        }
    }

    fn shutdowns(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }

    fn untouched(&self) -> bool {
        self.commands.lock().expect("commands").is_empty()
            && self.writes.lock().expect("writes").is_empty()
    }
}

#[async_trait]
impl SandboxSession for Session {
    fn backend_id(&self) -> &str {
        self.backend
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    fn supports_volume_mounts(&self) -> bool {
        self.volume_mounts
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        self.commands
            .lock()
            .expect("commands")
            .push(request.command().to_vec());
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Ok(Vec::new())
    }

    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn mkdir(
        &self,
        path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        self.writes
            .lock()
            .expect("writes")
            .push(format!("mkdir {path}"));
        Ok(())
    }

    async fn read(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn write(
        &self,
        path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        self.writes.lock().expect("writes").push(path.to_owned());
        push(&self.events, format!("write:{path}"));
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }

    async fn shutdown_backend(&self) -> SandboxResult<()> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        self.shutdown_started.notify_one();
        if let Some(gate) = &self.shutdown_gate {
            gate.notified().await;
            push(&self.events, "shutdown-complete");
        }
        if self.shutdown_fails {
            return Err(SandboxError::new(
                ErrorCode::WorkspaceStopError,
                OpName::Shutdown,
                "shutdown failed",
            ));
        }
        Ok(())
    }
}

// --- a lifecycle whose transitions record themselves, and fail or wait on request -----------------

#[derive(Default)]
struct Scripted {
    events: Events,
    fail_teardown: Vec<&'static str>,
    fail_restore: Vec<&'static str>,
    teardown_gate: Option<Arc<Notify>>,
    restore_gate: Option<Arc<Notify>>,
    started: Arc<Notify>,
    /// Aborts the caller from inside the teardown, so the teardown finishes in the same poll that
    /// the caller disappears.
    abort_during_teardown: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    panic_teardown: Vec<&'static str>,
}

/// Aborts `caller` and lets the runtime drop it before carrying on.
///
/// One yield exactly: the runtime drops the aborted caller while this task waits, and the next poll
/// finishes whatever called this. Waiting any longer would give a `select!` over "this finished"
/// and "the caller left" a poll in which only the second is ready, which is not the race under test.
async fn abort_and_let_it_drop(caller: Option<tokio::task::AbortHandle>) {
    if let Some(caller) = caller {
        caller.abort();
        tokio::task::yield_now().await;
    }
}

impl Scripted {
    fn new(events: &Events) -> Self {
        Self {
            events: Arc::clone(events),
            ..Self::default()
        }
    }
}

/// The name a test gave the mount, which is the last part of where it attaches.
fn name_of(path: &PosixPath) -> String {
    path.parts().last().copied().unwrap_or_default().to_owned()
}

fn transition_failed(what: &str, name: &str) -> SandboxError {
    SandboxError::new(
        ErrorCode::MountFailed,
        OpName::Materialize,
        format!("{what} failed: {name}"),
    )
}

#[async_trait]
impl MountLifecycle for Scripted {
    async fn activate(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        dest: &PosixPath,
        _base_dir: &Path,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        push(&self.events, format!("activate:{}", dest.as_str()));
        // Gives anything still in flight a chance to run in between, if the applier let it.
        tokio::task::yield_now().await;
        push(&self.events, format!("activated:{}", dest.as_str()));
        Ok(Vec::new())
    }

    async fn deactivate(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        dest: &PosixPath,
        _base_dir: &Path,
    ) -> SandboxResult<()> {
        push(&self.events, format!("deactivate:{}", dest.as_str()));
        Ok(())
    }

    async fn teardown_for_snapshot(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()> {
        let name = name_of(path);
        push(&self.events, format!("teardown:{name}"));
        assert!(
            !self.panic_teardown.contains(&name.as_str()),
            "teardown panicked: {name}"
        );
        let caller = self.abort_during_teardown.lock().expect("abort").take();
        abort_and_let_it_drop(caller).await;
        if let Some(gate) = &self.teardown_gate {
            self.started.notify_one();
            gate.notified().await;
            push(&self.events, format!("teardown-complete:{name}"));
        }
        if self.fail_teardown.contains(&name.as_str()) {
            return Err(transition_failed("teardown", &name));
        }
        Ok(())
    }

    async fn restore_after_snapshot(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()> {
        let name = name_of(path);
        push(&self.events, format!("restore:{name}"));
        if let Some(gate) = &self.restore_gate {
            self.started.notify_one();
            gate.notified().await;
            push(&self.events, format!("restore-complete:{name}"));
        }
        if self.fail_restore.contains(&name.as_str()) {
            return Err(transition_failed("restore", &name));
        }
        Ok(())
    }
}

fn s3_mount(strategy: MountStrategy) -> Mount {
    Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            ..S3Mount::default()
        }),
        strategy,
    )
    .expect("a supported provider and strategy")
}

/// A manifest with one mount at each of `names`, all at the same depth.
fn mounted(names: &[&str]) -> Manifest {
    names.iter().fold(Manifest::new(), |manifest, name| {
        manifest.with_entry(
            *name,
            Entry::mount(s3_mount(MountStrategy::docker_volume("rclone"))),
        )
    })
}

fn removal() -> EphemeralMountRemoval {
    EphemeralMountRemoval::new("/workspace", ArchiveErrorKind::ArchiveRead)
}

fn persist_failed() -> SandboxError {
    SandboxError::workspace_archive_read("/workspace").with_context("reason", "persist_failed")
}

// --- the reference's test_mount_lifecycle.py --------------------------------------------------

#[tokio::test]
async fn mounts_come_off_in_order_and_go_back_on_in_reverse_around_the_operation() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["left", "right"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));

    let recorded = Arc::clone(&events);
    let result = with_ephemeral_mounts_removed(
        session,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Ok("persisted")
        },
        removal().recording_operation_error_as("snapshot_error_before_remount_corruption"),
    )
    .await
    .expect("every transition succeeds");

    assert_eq!(result, "persisted");
    assert_eq!(
        snapshot_of(&events),
        [
            "teardown:left",
            "teardown:right",
            "operation",
            "restore:right",
            "restore:left"
        ]
    );
}

#[tokio::test]
async fn a_remount_failure_is_reported_over_the_operation_failure_and_keeps_its_message() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted {
        fail_restore: vec!["mount"],
        ..Scripted::new(&events)
    });

    let recorded = Arc::clone(&events);
    let error = with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Err::<(), _>(persist_failed())
        },
        removal().recording_operation_error_as("snapshot_error_before_remount_corruption"),
    )
    .await
    .expect_err("the remount failed");

    assert_eq!(
        snapshot_of(&events),
        ["teardown:mount", "operation", "restore:mount"]
    );
    // The remount failure is what leaves the workspace in an unknown state, so it is what is
    // returned; the operation's own failure survives as a message under the key it was asked for.
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert_eq!(
        error
            .context()
            .get("snapshot_error_before_remount_corruption"),
        Some(&json!({ "message": persist_failed().message() }))
    );
    let cause = std::error::Error::source(&error)
        .and_then(|cause| cause.downcast_ref::<SandboxError>())
        .expect("the strategy's failure is the cause");
    assert_eq!(cause.message(), "restore failed: mount");
    assert_eq!(session.shutdowns(), 1);
}

#[tokio::test]
async fn an_operation_failure_is_returned_as_it_was_once_the_mounts_are_back() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));

    let recorded = Arc::clone(&events);
    let error = with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Err::<(), _>(SandboxError::exec_transport(
                vec!["tar".to_owned()],
                Some("unexpected persistence failure"),
            ))
        },
        removal(),
    )
    .await
    .expect_err("the operation failed");

    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(
        snapshot_of(&events),
        ["teardown:mount", "operation", "restore:mount"]
    );
    // Every mount went back on, so nothing about the session is in doubt.
    assert_eq!(session.shutdowns(), 0);
}

#[tokio::test]
async fn a_teardown_failure_puts_back_what_came_off_and_then_ends_the_session() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["left", "right"]), &events));
    let lifecycle = Arc::new(Scripted {
        fail_teardown: vec!["right"],
        ..Scripted::new(&events)
    });

    let error = with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        || async { panic!("the operation must not run after a teardown failure") },
        removal(),
    )
    .await
    .map(|()| ())
    .expect_err("a teardown failed");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    // `right` may or may not still be attached, so it is not "restored"; `left` is.
    assert_eq!(
        snapshot_of(&events),
        ["teardown:left", "teardown:right", "restore:left"]
    );
    assert_eq!(session.shutdowns(), 1);
}

#[tokio::test]
async fn a_caller_that_gives_up_during_the_ambiguous_shutdown_still_sees_it_finish() {
    let events = Events::default();
    let gate = Arc::new(Notify::new());
    let session = Arc::new(Session {
        shutdown_gate: Some(Arc::clone(&gate)),
        ..Session::new(mounted(&["mount"]), &events)
    });
    let lifecycle = Arc::new(Scripted {
        fail_teardown: vec!["mount"],
        ..Scripted::new(&events)
    });

    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        || async {
            panic!("the operation must not run after a teardown failure") as SandboxResult<()>
        },
        removal(),
    ));
    session.shutdown_started.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    gate.notify_one();

    // The reference still raises the teardown failure to the cancelled caller; here nobody is
    // waiting for it, so what can be checked is that the shutdown ran once and finished.
    settles_to(&events, &["teardown:mount", "shutdown-complete"]).await;
    assert_eq!(session.shutdowns(), 1);
}

#[tokio::test]
async fn a_caller_that_gives_up_during_the_operation_still_gets_its_mounts_back() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));
    let operation_started = Arc::new(Notify::new());

    let recorded = Arc::clone(&events);
    let started = Arc::clone(&operation_started);
    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            started.notify_one();
            std::future::pending::<SandboxResult<()>>().await
        },
        removal(),
    ));
    operation_started.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());

    settles_to(&events, &["teardown:mount", "operation", "restore:mount"]).await;
    assert_eq!(session.shutdowns(), 0);
}

#[tokio::test]
async fn a_teardown_under_way_when_the_caller_gives_up_is_finished_and_then_undone() {
    let events = Events::default();
    let gate = Arc::new(Notify::new());
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted {
        teardown_gate: Some(Arc::clone(&gate)),
        ..Scripted::new(&events)
    });
    let started = Arc::clone(&lifecycle.started);

    let recorded = Arc::clone(&events);
    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Ok(())
        },
        removal(),
    ));
    started.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    gate.notify_one();

    // The operation never starts once the caller is gone, but what was detached goes back on.
    settles_to(
        &events,
        &["teardown:mount", "teardown-complete:mount", "restore:mount"],
    )
    .await;
}

#[tokio::test]
async fn a_remount_under_way_when_the_caller_gives_up_is_finished() {
    let events = Events::default();
    let gate = Arc::new(Notify::new());
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted {
        restore_gate: Some(Arc::clone(&gate)),
        ..Scripted::new(&events)
    });
    let started = Arc::clone(&lifecycle.started);

    let recorded = Arc::clone(&events);
    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Ok(())
        },
        removal(),
    ));
    started.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    gate.notify_one();

    settles_to(
        &events,
        &[
            "teardown:mount",
            "operation",
            "restore:mount",
            "restore-complete:mount",
        ],
    )
    .await;
}

// --- the rest of the helper's surface ---------------------------------------------------------

#[tokio::test]
async fn mounts_can_be_left_off_after_an_operation_that_succeeds() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));

    let recorded = Arc::clone(&events);
    with_ephemeral_mounts_removed(
        session,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Ok(())
        },
        removal().without_restore_on_success(),
    )
    .await
    .expect("succeeds");

    assert_eq!(snapshot_of(&events), ["teardown:mount", "operation"]);
}

#[tokio::test]
async fn every_detached_mount_is_attempted_and_the_later_failures_ride_on_the_first() {
    let events = Events::default();
    let session = Arc::new(Session {
        shutdown_fails: true,
        ..Session::new(Manifest::new(), &events)
    });
    let lifecycle = Arc::new(Scripted {
        fail_restore: vec!["left", "right"],
        ..Scripted::new(&events)
    });
    let detached = ["left", "right"]
        .iter()
        .map(|name| {
            (
                s3_mount(MountStrategy::docker_volume("rclone")),
                PosixPath::new(format!("/workspace/{name}")),
            )
        })
        .collect();

    let error = restore_detached_mounts(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        detached,
        "/workspace".to_owned(),
        ArchiveErrorKind::ArchiveWrite,
    )
    .await
    .expect("both remounts failed");

    assert_eq!(snapshot_of(&events), ["restore:right", "restore:left"]);
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error.context().get("additional_remount_errors"),
        Some(&json!([{
            "message": "failed to write archive for path: /workspace",
            "cause_type": "mount_failed",
            "cause": "restore failed: left",
        }]))
    );
    // Terminating the session was attempted once, and its failure is recorded rather than
    // replacing the remount failure.
    assert_eq!(session.shutdowns(), 1);
    assert_eq!(
        error.context().get("terminal_cleanup_failed"),
        Some(&json!(true))
    );
}

// --- the builtin strategies, against the reference's test_mounts.py ---------------------------

fn credentialed_in_container() -> Mount {
    Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            access_key_id: Some("access-key".to_owned()),
            secret_access_key: Some("direct-mount-apply-secret".to_owned()),
            ..S3Mount::default()
        }),
        MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
    )
    .expect("supported")
}

#[tokio::test]
async fn a_mount_carrying_credentials_is_refused_before_anything_runs() {
    let events = Events::default();
    let session = Session::new(Manifest::new(), &events);
    let mount = credentialed_in_container();

    let error = BuiltinMountLifecycle
        .apply(
            &mount,
            &session,
            &PosixPath::new("/workspace/data"),
            Path::new("/workspace"),
        )
        .await
        .expect_err("the credentials were never acknowledged for this path");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(session.untouched());
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(
            !rendered.contains("direct-mount-apply-secret"),
            "{rendered}"
        );
    }
}

#[tokio::test]
async fn a_mount_carrying_credentials_is_refused_before_it_is_put_back_after_a_snapshot() {
    let events = Events::default();
    let session = Session::new(Manifest::new(), &events);
    let mount = credentialed_in_container();

    let error = BuiltinMountLifecycle
        .restore_after_snapshot(
            &mount,
            mount.strategy(),
            &session,
            &PosixPath::new("/workspace/data"),
        )
        .await
        .expect_err("reattaching is attaching");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(session.untouched());
}

#[tokio::test]
async fn a_docker_volume_mount_is_refused_by_a_backend_that_does_not_attach_volumes() {
    let events = Events::default();
    let session = Session::new(Manifest::new(), &events);
    let mount = s3_mount(MountStrategy::docker_volume("rclone"));
    let dest = PosixPath::new("/workspace/data");

    // The credential boundary is the first to say so: the strategy belongs to another backend.
    let error = BuiltinMountLifecycle
        .apply(&mount, &session, &dest, Path::new("/ignored"))
        .await
        .expect_err("nobody attaches it");
    assert_eq!(
        error.to_string(),
        "docker-volume mounts are not supported by this sandbox backend"
    );

    // The strategy refuses for itself too, for a lifecycle that runs it without that check.
    let lifecycle = BuiltinMountLifecycle;
    let error = lifecycle
        .activate(
            &mount,
            mount.strategy(),
            &session,
            &dest,
            Path::new("/ignored"),
        )
        .await
        .expect_err("the session does not attach volumes");
    assert_eq!(
        error.to_string(),
        "docker-volume mounts are not supported by this sandbox backend"
    );
    assert_eq!(
        error.context().get("session_type"),
        Some(&json!("recording"))
    );

    // Detaching is refused the same way; around a snapshot there is nothing to do either way,
    // because the volume stays where it is and the snapshot leaves its path out.
    assert!(
        lifecycle
            .unmount(&mount, &session, &dest, Path::new("/"))
            .await
            .is_err()
    );
    lifecycle
        .teardown_for_snapshot(&mount, mount.strategy(), &session, &dest)
        .await
        .expect("nothing to detach");
    lifecycle
        .restore_after_snapshot(&mount, mount.strategy(), &session, &dest)
        .await
        .expect("nothing to reattach");
}

#[tokio::test]
async fn a_docker_volume_mount_is_already_attached_on_a_backend_that_attaches_volumes() {
    let events = Events::default();
    let session = Session::docker(Manifest::new(), &events);
    let mount = s3_mount(MountStrategy::docker_volume("rclone"));
    let dest = PosixPath::new("/workspace/data");

    let files = BuiltinMountLifecycle
        .apply(&mount, &session, &dest, Path::new("/ignored"))
        .await
        .expect("the runtime attached it before the session started");
    BuiltinMountLifecycle
        .unmount(&mount, &session, &dest, Path::new("/"))
        .await
        .expect("and detaches it with the container");

    assert!(files.is_empty());
    assert!(session.untouched());
}

#[tokio::test]
async fn an_in_container_mount_runs_its_patterns_tool_once_the_boundary_has_passed() {
    let events = Events::default();
    let session = Session::new(Manifest::new(), &events);
    let mount = s3_mount(MountStrategy::in_container(MountPattern::Mountpoint(
        MountpointOptions::default(),
    )));

    BuiltinMountLifecycle
        .apply(
            &mount,
            &session,
            &PosixPath::new("/workspace/data"),
            Path::new("/"),
        )
        .await
        .expect("mounts");

    let commands = session.commands.lock().expect("commands").clone();
    assert_eq!(
        commands.first().map(|command| command.join(" ")),
        Some("command -v mount-s3 >/dev/null 2>&1".to_owned())
    );
    assert_eq!(
        commands.last().map(|command| command.join(" ")),
        Some("sh -lc mount-s3 --no-sign-request --read-only bucket /workspace/data".to_owned())
    );
}

#[tokio::test]
async fn a_strategy_or_mount_type_a_host_registered_has_no_builtin_lifecycle() {
    let events = Events::default();
    let session = Session::new(Manifest::new(), &events);
    let dest = PosixPath::new("/workspace/data");

    let custom_strategy = s3_mount(MountStrategy::docker_volume("rclone"));
    let strategy = MountStrategy::Extension(DiscriminatedPayload::new("host_attach"));
    let error = BuiltinMountLifecycle
        .activate(&custom_strategy, &strategy, &session, &dest, Path::new("/"))
        .await
        .expect_err("no lifecycle for it here");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(
        error.context().get("strategy_type"),
        Some(&json!("host_attach"))
    );

    let custom_type = Mount::new(
        MountProvider::Extension(DiscriminatedPayload::new("host_mount")),
        MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
    )
    .expect("an extension type is not checked against a matrix");
    let error = BuiltinMountLifecycle
        .teardown_for_snapshot(&custom_type, custom_type.strategy(), &session, &dest)
        .await
        .expect_err("no in-container adapter for it");
    // A mount type this crate cannot read may hide authority, so the refusal leaves the boundary
    // replaced: still a configuration failure, with nothing of the mount in it.
    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert_eq!(error.to_string(), "sandbox mount configuration is invalid");
    assert!(error.context().is_empty());
}

// --- manifest application hands mounts to the lifecycle ----------------------------------------

#[tokio::test]
async fn a_manifest_application_attaches_each_mount_through_the_lifecycle_it_was_given() {
    let events = Events::default();
    let manifest = mounted(&["data"]);
    let session = Arc::new(Session::docker(manifest.clone(), &events));
    let lifecycle = Arc::new(Scripted::new(&events));

    ManifestApplier::new(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        std::path::PathBuf::from("/"),
    )
    .with_mount_lifecycle(lifecycle)
    .apply_manifest(&manifest, false)
    .await
    .expect("applies");

    assert_eq!(
        snapshot_of(&events),
        ["activate:/workspace/data", "activated:/workspace/data"]
    );
    // No ownership or mode is applied to a mount afterwards: its permissions are the provider's.
    assert!(
        session
            .commands
            .lock()
            .expect("commands")
            .iter()
            .all(|command| command.first().is_none_or(|program| program != "chmod")),
        "{:?}",
        session.commands.lock().expect("commands")
    );
}

#[tokio::test]
async fn a_mount_is_attached_alone_between_what_was_queued_before_it_and_what_comes_after() {
    // Entries are held by name, so the names are chosen to sort in the order the reference declares
    // them: two files, the mount, one more file.
    let events = Events::default();
    let manifest = Manifest::new()
        .with_entry("a.txt", Entry::file("a"))
        .with_entry("b.txt", Entry::file("b"))
        .with_entry(
            "m",
            Entry::mount(s3_mount(MountStrategy::docker_volume("rclone"))),
        )
        .with_entry("z.txt", Entry::file("c"));
    let session = Arc::new(Session::docker(manifest.clone(), &events));

    ManifestApplier::new(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        std::path::PathBuf::from("/"),
    )
    .with_mount_lifecycle(Arc::new(Scripted::new(&events)))
    .apply_manifest(&manifest, false)
    .await
    .expect("applies");

    let seen = snapshot_of(&events);
    let at = |event: &str| {
        seen.iter()
            .position(|seen| seen == event)
            .unwrap_or_else(|| panic!("{event} in {seen:?}"))
    };
    let mount_start = at("activate:/workspace/m");
    assert!(at("write:/workspace/a.txt") < mount_start, "{seen:?}");
    assert!(at("write:/workspace/b.txt") < mount_start, "{seen:?}");
    assert!(
        at("activated:/workspace/m") < at("write:/workspace/z.txt"),
        "{seen:?}"
    );
}

// --- cancellation that arrives together with a result, and panics ------------------------------

#[tokio::test]
async fn a_caller_that_leaves_just_as_a_teardown_finishes_still_gets_its_mounts_back() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));
    let abort = Arc::clone(&lifecycle.abort_during_teardown);

    let recorded = Arc::clone(&events);
    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            Ok(())
        },
        removal().without_restore_on_success(),
    ));
    *abort.lock().expect("abort") = Some(caller.abort_handle());
    assert!(caller.await.expect_err("aborted").is_cancelled());

    // The teardown's result and the caller's departure were both ready at once. Whichever is read
    // first, the departure is not lost: the operation does not start, and the mount goes back on
    // even though a successful operation was told to leave it off — nobody is left to put it back.
    settles_to(&events, &["teardown:mount", "restore:mount"]).await;
}

#[tokio::test]
async fn a_caller_that_leaves_just_as_the_operation_finishes_still_gets_its_mounts_back() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));
    let abort: Arc<Mutex<Option<tokio::task::AbortHandle>>> = Arc::default();

    let recorded = Arc::clone(&events);
    let operation_abort = Arc::clone(&abort);
    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            let caller = operation_abort.lock().expect("abort").take();
            abort_and_let_it_drop(caller).await;
            Ok(())
        },
        removal().without_restore_on_success(),
    ));
    *abort.lock().expect("abort") = Some(caller.abort_handle());
    assert!(caller.await.expect_err("aborted").is_cancelled());

    settles_to(&events, &["teardown:mount", "operation", "restore:mount"]).await;
}

#[tokio::test]
async fn an_operation_that_panics_still_gets_its_mounts_back_before_the_panic_goes_on() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["mount"]), &events));
    let lifecycle = Arc::new(Scripted::new(&events));

    let recorded = Arc::clone(&events);
    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        move || async move {
            push(&recorded, "operation");
            panic!("the operation panicked") as SandboxResult<()>
        },
        removal(),
    ));

    let error = caller.await.expect_err("the panic reaches the caller");
    assert!(error.is_panic());
    assert_eq!(
        snapshot_of(&events),
        ["teardown:mount", "operation", "restore:mount"]
    );
    assert_eq!(session.shutdowns(), 0);
}

#[tokio::test]
async fn a_teardown_that_panics_is_cleaned_up_after_like_one_that_failed() {
    let events = Events::default();
    let session = Arc::new(Session::new(mounted(&["left", "right"]), &events));
    let lifecycle = Arc::new(Scripted {
        panic_teardown: vec!["right"],
        ..Scripted::new(&events)
    });

    let caller = tokio::spawn(with_ephemeral_mounts_removed(
        Arc::clone(&session) as Arc<dyn SandboxSession>,
        lifecycle,
        || async {
            panic!("the operation must not run after a teardown panic") as SandboxResult<()>
        },
        removal(),
    ));

    let error = caller.await.expect_err("the panic reaches the caller");
    assert!(error.is_panic());
    // Whether `right` is still attached is unknown, exactly as after a teardown that returned an
    // error: `left` goes back on and the session is ended.
    assert_eq!(
        snapshot_of(&events),
        ["teardown:left", "teardown:right", "restore:left"]
    );
    assert_eq!(session.shutdowns(), 1);
}
