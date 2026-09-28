//! The lifecycle defaults a session gets from its `SessionResources`: dependencies, pre-stop
//! callbacks, runtime persistence exclusions and the close lock, as the reference's base session
//! provides them.
//!
//! The session here overrides nothing those defaults touch, so every assertion is about the default
//! itself. Stop is observed through `persist_snapshot` and shutdown through `shutdown_backend`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, CloseDependency, Dependencies, DependencyValue, Entry, ErrorCode, ExecRequest,
    ExecResult, FactoryOptions, FileEntry, GcsMount, Manifest, Mount, MountPattern, MountProvider,
    MountStrategy, MountpointOptions, OpName, SandboxError, SandboxResult, SandboxSession,
    SandboxSessionState, SessionPath, SessionResources, Snapshot, dependency_factory,
    pre_stop_hook,
};

type Transcript = Arc<Mutex<Vec<String>>>;

struct Plain {
    state: SandboxSessionState,
    resources: SessionResources,
    log: Transcript,
    fail_persist: AtomicBool,
}

impl Plain {
    fn new() -> Self {
        Self::with_manifest(Manifest::new())
    }

    fn with_manifest(manifest: Manifest) -> Self {
        Self {
            state: SandboxSessionState::new("plain", Snapshot::noop(), manifest),
            resources: SessionResources::new(),
            log: Transcript::default(),
            fail_persist: AtomicBool::new(false),
        }
    }

    fn failing_persist() -> Self {
        let session = Self::new();
        session.fail_persist.store(true, Ordering::SeqCst);
        session
    }

    fn note(&self, step: &str) {
        note(&self.log, step);
    }

    fn transcript(&self) -> Vec<String> {
        self.log.lock().expect("transcript").clone()
    }

    /// A callback that records its name and succeeds, or fails naming itself.
    fn hook(&self, name: &'static str, fails: bool) -> ra_core::sandbox::PreStopHook {
        let log = Arc::clone(&self.log);
        pre_stop_hook(move || {
            let log = Arc::clone(&log);
            async move {
                note(&log, name);
                if fails {
                    return Err(SandboxError::new(
                        ErrorCode::WorkspaceStopError,
                        OpName::Stop,
                        format!("{name} failed"),
                    ));
                }
                Ok(())
            }
        })
    }
}

fn note(log: &Transcript, step: &str) {
    log.lock().expect("transcript").push(step.to_owned());
}

#[async_trait]
impl SandboxSession for Plain {
    fn backend_id(&self) -> &str {
        "plain"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
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
        _path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn read(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn write(
        &self,
        _path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }

    async fn persist_snapshot(&self) -> SandboxResult<()> {
        self.note("persist");
        if self.fail_persist.load(Ordering::SeqCst) {
            return Err(SandboxError::snapshot_persist("plain", "<test>"));
        }
        Ok(())
    }

    async fn shutdown_backend(&self) -> SandboxResult<()> {
        self.note("shutdown");
        Ok(())
    }
}

/// Counts how many times it was closed.
#[derive(Default)]
struct Closable {
    calls: AtomicUsize,
}

#[async_trait]
impl CloseDependency for Closable {
    async fn close(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// Dependencies holding one owned, closable resource under `tests.owned`.
fn owned_resource_dependencies() -> Arc<Dependencies> {
    let dependencies = Dependencies::new();
    dependencies
        .bind_factory(
            "tests.owned",
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(|_| async {
                Ok(DependencyValue::closable(Arc::new(Closable::default())))
            }),
        )
        .expect("bound");
    Arc::new(dependencies)
}

#[tokio::test]
async fn pre_stop_callbacks_run_once_before_the_workspace_is_persisted() {
    let session = Plain::new();
    session.register_pre_stop_hook(session.hook("flush", false));

    session.close().await.expect("first close");
    session.close().await.expect("second close");

    assert_eq!(
        session.transcript(),
        ["flush", "persist", "shutdown", "persist", "shutdown"]
    );
}

#[tokio::test]
async fn every_callback_runs_and_the_first_failure_is_reported_and_remembered() {
    let session = Plain::new();
    session.register_pre_stop_hook(session.hook("first", true));
    session.register_pre_stop_hook(session.hook("second", true));
    session.register_pre_stop_hook(session.hook("third", false));

    let error = session.close().await.expect_err("callback failed");

    assert!(error.message().contains("first failed"), "{error}");
    assert!(session.pre_stop_hooks_failed());
    // Every callback ran; the workspace was not persisted; the backend was still shut down.
    assert_eq!(
        session.transcript(),
        ["first", "second", "third", "shutdown"]
    );

    // The callbacks do not run again, and the failure still keeps this close from persisting.
    session.close().await.expect("second close");
    assert_eq!(
        session.transcript(),
        ["first", "second", "third", "shutdown", "shutdown"]
    );
}

#[tokio::test]
async fn registering_after_a_run_arms_every_callback_again() {
    let session = Plain::new();
    session.register_pre_stop_hook(session.hook("first", false));
    session.run_pre_stop_hooks().await.expect("first run");
    session.run_pre_stop_hooks().await.expect("already ran");

    session.register_pre_stop_hook(session.hook("second", false));
    session.run_pre_stop_hooks().await.expect("second run");

    assert_eq!(session.transcript(), ["first", "first", "second"]);
}

#[tokio::test]
async fn close_releases_the_session_dependencies_once() {
    let session = Plain::new();
    session.set_dependencies(Some(owned_resource_dependencies()));
    let resource = session
        .dependencies()
        .require_as::<Closable>("tests.owned", None)
        .await
        .expect("resource");

    session.close().await.expect("first close");
    session.close().await.expect("second close");

    assert_eq!(resource.calls.load(Ordering::SeqCst), 1);
    assert!(session.dependencies().is_closed());
}

#[tokio::test]
async fn a_failed_stop_still_releases_the_session_dependencies() {
    let session = Plain::failing_persist();
    session.set_dependencies(Some(owned_resource_dependencies()));
    let resource = session
        .dependencies()
        .require_as::<Closable>("tests.owned", None)
        .await
        .expect("resource");

    let error = session.close().await.expect_err("stop failed");

    assert_eq!(error.error_code(), ErrorCode::SnapshotPersistError);
    // A failed stop skips the shutdown, and still releases the dependencies.
    assert_eq!(session.transcript(), ["persist"]);
    assert_eq!(resource.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_session_has_one_container_until_another_is_set() {
    let session = Plain::new();

    let created = session.dependencies();
    assert!(Arc::ptr_eq(&created, &session.dependencies()));

    session.set_dependencies(None);
    assert!(Arc::ptr_eq(&created, &session.dependencies()));

    let replacement = Arc::new(Dependencies::new());
    session.set_dependencies(Some(Arc::clone(&replacement)));
    assert!(Arc::ptr_eq(&replacement, &session.dependencies()));
}

#[tokio::test]
async fn a_close_cancelled_during_its_callbacks_never_persists_on_a_later_close() {
    // A callback that never finishes on its own, so the close running it can be cancelled there.
    let session = Arc::new(Plain::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let hook_started = Arc::clone(&started);
    let log = Arc::clone(&session.log);
    session.register_pre_stop_hook(pre_stop_hook(move || {
        let (started, log) = (Arc::clone(&hook_started), Arc::clone(&log));
        async move {
            note(&log, "hook");
            started.notify_one();
            futures::future::pending::<()>().await;
            Ok(())
        }
    }));
    session.set_dependencies(Some(owned_resource_dependencies()));
    let resource = session
        .dependencies()
        .require_as::<Closable>("tests.owned", None)
        .await
        .expect("resource");

    let closing = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.close().await })
    };
    started.notified().await;
    closing.abort();
    assert!(closing.await.expect_err("cancelled").is_cancelled());

    // Cancelled callbacks count as failed ones: the next close does not run them again, does not
    // persist the workspace they were protecting, and still shuts down and releases everything.
    assert!(session.pre_stop_hooks_failed());
    session.close().await.expect("second close");
    assert_eq!(session.transcript(), ["hook", "shutdown"]);
    assert_eq!(resource.calls.load(Ordering::SeqCst), 1);
}

// --- paths a session excludes from its snapshots at runtime ------------------------------------

/// A session whose manifest mounts a bucket at `mount_path`, as the reference's fixture does.
fn mounted_at(mount_path: &str) -> Plain {
    let mount = Mount::new(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            ..GcsMount::default()
        }),
        MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
    )
    .expect("supported")
    .at(mount_path);
    Plain::with_manifest(Manifest::new().with_entry("remote", Entry::mount(mount)))
}

#[test]
fn a_runtime_skip_path_may_not_overlap_where_a_mount_attaches() {
    for (skip_path, mount_path) in [
        ("data", "data"),
        ("logs", "logs/remote"),
        ("data/tmp", "data"),
    ] {
        let session = mounted_at(mount_path);

        let error = session
            .register_persist_workspace_skip_path(skip_path.into())
            .expect_err("overlaps the mount");

        assert_eq!(
            error.error_code(),
            ErrorCode::MountConfigInvalid,
            "{skip_path}"
        );
        assert_eq!(
            error.to_string(),
            "persist workspace skip path must not overlap mount path"
        );
        assert_eq!(
            error.context().get("mount_path"),
            Some(&serde_json::json!(mount_path))
        );
        assert!(
            session
                .resources()
                .persist_workspace_skip_paths()
                .is_empty()
        );
    }
}

#[test]
fn a_runtime_skip_path_beside_a_mount_is_recorded_and_left_out_of_snapshots() {
    let session = mounted_at("data");

    let registered = session
        .register_persist_workspace_skip_path("logs/events.jsonl".into())
        .expect("does not overlap");

    assert_eq!(registered.as_str(), "logs/events.jsonl");
    let skipped: Vec<String> = session
        .persist_workspace_skip_relpaths()
        .expect("paths")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    // Beside what the manifest itself leaves out, which here is the mount under both the name it was
    // declared at and the path it attaches to.
    assert_eq!(skipped, ["data", "logs/events.jsonl", "remote"]);
}

#[test]
fn a_runtime_skip_path_names_somewhere_inside_the_workspace() {
    let session = Plain::new();

    for (path, reason) in [
        ("/tmp/x", "absolute"),
        ("../x", "escape_root"),
        ("C:/x", "absolute"),
    ] {
        let error = session
            .register_persist_workspace_skip_path(path.into())
            .expect_err("not a workspace-relative path");
        assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath, "{path}");
        assert_eq!(
            error.context().get("reason"),
            Some(&serde_json::json!(reason)),
            "{path}"
        );
    }
    // The root itself is not a concrete path: excluding it would exclude everything.
    for path in ["", "."] {
        let error = session
            .register_persist_workspace_skip_path(path.into())
            .expect_err("the workspace root");
        assert_eq!(
            error.error_code(),
            ErrorCode::SandboxConfigInvalid,
            "{path:?}"
        );
    }
}
