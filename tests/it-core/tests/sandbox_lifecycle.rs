//! `ra-core::sandbox::session`: the order a session's lifecycle runs in, and the operations a
//! backend has to offer.
//!
//! The order is the contract, so it is asserted as an order: the backend here writes down every
//! hook it is asked for, and the tests read the transcript. What that catches is the class of
//! mistake a port makes silently — a step that moved, a branch that took the wrong arm, a hook that
//! ran on a path it was supposed to be kept off.
//!
//! The backend keeps its state behind a lock and really updates it, because several of these
//! decisions are only correct if a hook's recording survives into the next question the lifecycle
//! asks. A backend that only logged calls would pass while recording nothing.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, CreateRequest, DiscriminatedPayload, Entry, EntryContent, EntryKind, ErrorCode,
    ExecRequest, ExecResult, FileEntry, Manifest, MaterializationResult, Mount, MountPattern,
    MountProvider, MountStrategy, OpName, Permissions, PtyProcessId, PtyStartRequest,
    PtyWriteRequest, RcloneOptions, S3Mount, SandboxClient, SandboxError, SandboxResult,
    SandboxSession, SandboxSessionState, SessionPath, SessionResources, Snapshot,
};

/// Which hook a backend was asked for, in the order it was asked.
type Transcript = Arc<Mutex<Vec<&'static str>>>;

/// What the backend should do instead of succeeding.
#[derive(Debug, Clone, Default)]
struct Faults {
    prepare_workspace: bool,
    after_start: bool,
    persist_snapshot: bool,
    shutdown_backend: bool,
    pre_stop_hooks: bool,
    close_dependencies: bool,
    running: bool,
}

/// What the backend answers when the lifecycle asks it to choose a branch.
#[derive(Debug, Clone, Default)]
struct Answers {
    probe_finds_workspace: bool,
    workspace_preserved: bool,
    snapshot_restorable: bool,
    can_skip_restore: bool,
    provision_accounts: bool,
    state_root_ready: bool,
    exposed_ports: Vec<u16>,
}

/// What the backend has recorded about itself, which the lifecycle both reads and writes.
#[derive(Debug)]
struct Recorded {
    state: SandboxSessionState,
    pre_stop_hooks_ran: bool,
    pre_stop_hooks_failed: bool,
}

struct Backend {
    inner: Mutex<Recorded>,
    resources: SessionResources,
    log: Transcript,
    faults: Faults,
    answers: Answers,
}

impl Backend {
    fn new(answers: Answers, faults: Faults) -> Self {
        let state = SandboxSessionState::new("recording", Snapshot::noop(), Manifest::new())
            .with_workspace_root_ready(answers.state_root_ready)
            .with_exposed_ports(answers.exposed_ports.iter().copied())
            .expect("ports");
        Self {
            inner: Mutex::new(Recorded {
                state,
                pre_stop_hooks_ran: false,
                pre_stop_hooks_failed: false,
            }),
            resources: SessionResources::new(),
            log: Transcript::default(),
            faults,
            answers,
        }
    }

    fn with_answers(answers: Answers) -> Self {
        Self::new(answers, Faults::default())
    }

    fn with_faults(faults: Faults) -> Self {
        Self::new(Answers::default(), faults)
    }

    fn note(&self, step: &'static str) {
        self.log.lock().expect("transcript").push(step);
    }

    fn transcript(&self) -> Vec<&'static str> {
        self.log.lock().expect("transcript").clone()
    }

    fn clear_transcript(&self) {
        self.log.lock().expect("transcript").clear();
    }

    fn fail(code: ErrorCode, op: OpName, what: &str) -> SandboxError {
        SandboxError::new(code, op, format!("{what} failed"))
    }
}

#[async_trait]
impl SandboxSession for Backend {
    fn backend_id(&self) -> &str {
        "recording"
    }

    fn state(&self) -> SandboxSessionState {
        self.inner.lock().expect("state").state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
        self.note("exec");
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        self.note("running");
        if self.faults.running {
            return Err(Self::fail(
                ErrorCode::ExecTransportError,
                OpName::Running,
                "running probe",
            ));
        }
        Ok(true)
    }

    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        self.note("ls");
        Ok(vec![
            FileEntry::new("/workspace/README.md", Permissions::from_mode(0o644))
                .with_ownership("agent", "agent")
                .with_size(12),
        ])
    }

    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        self.note("rm");
        Ok(())
    }

    async fn mkdir(
        &self,
        _path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        self.note("mkdir");
        Ok(())
    }

    async fn read(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        self.note("read");
        Ok(b"contents".to_vec())
    }

    async fn write(
        &self,
        _path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        self.note("write");
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        self.note("persist_workspace");
        Ok(b"archive".to_vec())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        self.note("hydrate_workspace");
        Ok(())
    }

    async fn probe_workspace_root(&self) -> SandboxResult<bool> {
        self.note("probe_workspace_root");
        Ok(self.answers.probe_finds_workspace)
    }

    async fn prepare_backend_workspace(&self) -> SandboxResult<()> {
        self.note("prepare_backend_workspace");
        if self.faults.prepare_workspace {
            return Err(Self::fail(
                ErrorCode::WorkspaceStartError,
                OpName::Start,
                "prepare",
            ));
        }
        Ok(())
    }

    async fn ensure_runtime_helpers(&self) -> SandboxResult<()> {
        self.note("ensure_runtime_helpers");
        Ok(())
    }

    async fn snapshot_restorable(&self) -> SandboxResult<bool> {
        self.note("snapshot_restorable");
        Ok(self.answers.snapshot_restorable)
    }

    fn workspace_state_preserved_on_start(&self) -> bool {
        self.answers.workspace_preserved
    }

    async fn can_skip_snapshot_restore(&self, _is_running: bool) -> SandboxResult<bool> {
        self.note("can_skip_snapshot_restore");
        Ok(self.answers.can_skip_restore)
    }

    fn should_provision_accounts(&self) -> bool {
        self.answers.provision_accounts
    }

    async fn provision_accounts(&self) -> SandboxResult<()> {
        self.note("provision_accounts");
        Ok(())
    }

    async fn restore_snapshot(&self) -> SandboxResult<()> {
        self.note("restore_snapshot");
        Ok(())
    }

    async fn reapply_ephemeral_manifest(&self) -> SandboxResult<()> {
        self.note("reapply_ephemeral_manifest");
        Ok(())
    }

    async fn apply_manifest(
        &self,
        _provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        self.note("apply_manifest");
        Ok(MaterializationResult::new())
    }

    async fn after_start(&self) -> SandboxResult<()> {
        self.note("after_start");
        if self.faults.after_start {
            return Err(Self::fail(
                ErrorCode::WorkspaceStartError,
                OpName::Start,
                "after start",
            ));
        }
        Ok(())
    }

    async fn record_workspace_root_ready(&self) -> SandboxResult<()> {
        self.note("record_workspace_root_ready");
        let mut inner = self.inner.lock().expect("state");
        inner.state = inner.state.clone().with_workspace_root_ready(true);
        Ok(())
    }

    async fn after_start_failed(&self) {
        self.note("after_start_failed");
    }

    async fn before_stop(&self) -> SandboxResult<()> {
        self.note("before_stop");
        // A real stop waits on something. Yielding here gives a second, concurrent close the chance
        // to interleave, which is what the close lock is there to prevent.
        tokio::task::yield_now().await;
        Ok(())
    }

    async fn persist_snapshot(&self) -> SandboxResult<()> {
        self.note("persist_snapshot");
        if self.faults.persist_snapshot {
            return Err(Self::fail(
                ErrorCode::SnapshotPersistError,
                OpName::SnapshotPersist,
                "persist",
            ));
        }
        let mut inner = self.inner.lock().expect("state");
        inner.state = inner
            .state
            .clone()
            .with_snapshot_fingerprint("abc123", "v2");
        Ok(())
    }

    async fn after_stop(&self) {
        self.note("after_stop");
    }

    async fn before_shutdown(&self) -> SandboxResult<()> {
        self.note("before_shutdown");
        Ok(())
    }

    async fn shutdown_backend(&self) -> SandboxResult<()> {
        self.note("shutdown_backend");
        if self.faults.shutdown_backend {
            return Err(Self::fail(
                ErrorCode::WorkspaceStopError,
                OpName::Shutdown,
                "shutdown",
            ));
        }
        Ok(())
    }

    async fn after_shutdown(&self) -> SandboxResult<()> {
        self.note("after_shutdown");
        Ok(())
    }

    async fn run_pre_stop_hooks(&self) -> SandboxResult<()> {
        // Run-once, exactly as the reference: a second close is told nothing went wrong.
        {
            let mut inner = self.inner.lock().expect("state");
            if inner.pre_stop_hooks_ran {
                self.note("run_pre_stop_hooks:already_ran");
                return Ok(());
            }
            inner.pre_stop_hooks_ran = true;
            if self.faults.pre_stop_hooks {
                inner.pre_stop_hooks_failed = true;
            }
        }
        self.note("run_pre_stop_hooks");
        if self.faults.pre_stop_hooks {
            return Err(Self::fail(
                ErrorCode::WorkspaceStopError,
                OpName::Stop,
                "pre-stop hook",
            ));
        }
        Ok(())
    }

    fn pre_stop_hooks_failed(&self) -> bool {
        self.inner.lock().expect("state").pre_stop_hooks_failed
    }

    async fn close_dependencies(&self) -> SandboxResult<()> {
        self.note("close_dependencies");
        if self.faults.close_dependencies {
            return Err(Self::fail(
                ErrorCode::WorkspaceStopError,
                OpName::Stop,
                "dependency close",
            ));
        }
        Ok(())
    }
}

// --- start ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_workspace_probe_runs_before_the_workspace_is_prepared() {
    // Afterwards a directory this start created is indistinguishable from one a previous session
    // left behind, and every resume decision below reads that answer.
    let session = Backend::with_answers(Answers {
        workspace_preserved: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    let transcript = session.transcript();
    let probe = transcript
        .iter()
        .position(|step| *step == "probe_workspace_root")
        .expect("probed");
    let prepare = transcript
        .iter()
        .position(|step| *step == "prepare_backend_workspace")
        .expect("prepared");
    assert!(
        probe < prepare,
        "probe must precede prepare: {transcript:?}"
    );
}

#[tokio::test]
async fn a_backend_that_kept_nothing_is_not_probed() {
    // Only a backend that says its workspace survived has anything a probe could prove; for any
    // other, a directory that happens to be there is not a workspace this session can reuse.
    let session = Backend::with_answers(Answers {
        probe_finds_workspace: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    let transcript = session.transcript();
    assert!(
        !transcript.contains(&"probe_workspace_root"),
        "{transcript:?}"
    );
    assert!(transcript.contains(&"apply_manifest"), "{transcript:?}");
}

#[tokio::test]
async fn a_root_an_earlier_start_recorded_is_not_probed_again() {
    let session = Backend::with_answers(Answers {
        workspace_preserved: true,
        state_root_ready: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    let transcript = session.transcript();
    assert!(
        !transcript.contains(&"probe_workspace_root"),
        "{transcript:?}"
    );
    assert!(
        transcript.contains(&"reapply_ephemeral_manifest"),
        "{transcript:?}"
    );
}

#[tokio::test]
async fn a_root_the_probe_proves_is_recorded_before_anything_is_prepared() {
    // Written down as soon as it is proven, as the reference writes it, rather than only when the
    // whole start has succeeded.
    let session = Backend::with_answers(Answers {
        workspace_preserved: true,
        probe_finds_workspace: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    assert_eq!(
        &session.transcript()[..3],
        [
            "probe_workspace_root",
            "record_workspace_root_ready",
            "prepare_backend_workspace"
        ]
    );
}

#[tokio::test]
async fn a_fresh_start_materializes_the_whole_manifest() {
    let session = Backend::with_answers(Answers::default());

    session.start().await.expect("start");

    assert_eq!(
        session.transcript(),
        [
            "prepare_backend_workspace",
            "ensure_runtime_helpers",
            "snapshot_restorable",
            "apply_manifest",
            "after_start",
            "record_workspace_root_ready",
        ]
    );
}

#[tokio::test]
async fn a_successful_start_records_that_the_root_now_exists() {
    // Not just that the hook was called: the recording has to survive, because the next start
    // reads it to tell a resume from a fresh workspace.
    let session = Backend::with_answers(Answers::default());
    assert!(!session.state().workspace_root_ready());

    session.start().await.expect("start");

    assert!(session.state().workspace_root_ready());
}

#[tokio::test]
async fn a_failed_start_records_nothing() {
    let session = Backend::with_faults(Faults {
        prepare_workspace: true,
        ..Faults::default()
    });

    session.start().await.expect_err("fail");

    assert!(!session.state().workspace_root_ready());
}

#[tokio::test]
async fn a_preserved_workspace_with_no_snapshot_only_rebuilds_the_ephemeral_parts() {
    let session = Backend::with_answers(Answers {
        probe_finds_workspace: true,
        workspace_preserved: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    let transcript = session.transcript();
    assert!(
        transcript.contains(&"reapply_ephemeral_manifest"),
        "{transcript:?}"
    );
    assert!(!transcript.contains(&"apply_manifest"), "{transcript:?}");
    assert!(!transcript.contains(&"restore_snapshot"), "{transcript:?}");
}

#[tokio::test]
async fn a_matching_live_workspace_is_not_overwritten_by_its_own_snapshot() {
    let session = Backend::with_answers(Answers {
        probe_finds_workspace: true,
        workspace_preserved: true,
        snapshot_restorable: true,
        can_skip_restore: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    let transcript = session.transcript();
    assert!(
        transcript.contains(&"can_skip_snapshot_restore"),
        "{transcript:?}"
    );
    assert!(!transcript.contains(&"restore_snapshot"), "{transcript:?}");
}

#[tokio::test]
async fn a_drifted_workspace_restores_the_snapshot_then_rebuilds_the_ephemeral_parts() {
    let session = Backend::with_answers(Answers {
        probe_finds_workspace: true,
        workspace_preserved: true,
        snapshot_restorable: true,
        can_skip_restore: false,
        provision_accounts: true,
        ..Answers::default()
    });

    session.start().await.expect("start");

    let transcript = session.transcript();
    let restore = transcript.iter().position(|s| *s == "restore_snapshot");
    let accounts = transcript.iter().position(|s| *s == "provision_accounts");
    let ephemeral = transcript
        .iter()
        .position(|s| *s == "reapply_ephemeral_manifest");
    assert!(
        restore < accounts && accounts < ephemeral,
        "restore, then accounts, then ephemeral: {transcript:?}"
    );
}

#[tokio::test]
async fn a_fresh_workspace_restores_its_snapshot_without_asking_whether_anything_is_running() {
    // There is nothing preserved to compare against, so the answer cannot change the outcome. A
    // probe that can fail would abandon a restore that was going to happen either way.
    let session = Backend::new(
        Answers {
            snapshot_restorable: true,
            ..Answers::default()
        },
        Faults {
            running: true,
            ..Faults::default()
        },
    );

    session.start().await.expect("start");

    let transcript = session.transcript();
    assert!(!transcript.contains(&"running"), "{transcript:?}");
    assert!(
        !transcript.contains(&"can_skip_snapshot_restore"),
        "{transcript:?}"
    );
    assert!(transcript.contains(&"restore_snapshot"), "{transcript:?}");
}

#[tokio::test]
async fn a_start_that_fails_inside_the_guarded_block_runs_the_failure_hook() {
    let session = Backend::with_faults(Faults {
        prepare_workspace: true,
        ..Faults::default()
    });

    let error = session.start().await.expect_err("fail");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceStartError);
    let transcript = session.transcript();
    assert!(transcript.contains(&"after_start_failed"), "{transcript:?}");
    assert!(!transcript.contains(&"after_start"), "{transcript:?}");
    assert!(
        !transcript.contains(&"ensure_runtime_helpers"),
        "{transcript:?}"
    );
}

#[tokio::test]
async fn a_failing_after_start_hook_is_not_a_failed_start() {
    let session = Backend::with_faults(Faults {
        after_start: true,
        ..Faults::default()
    });

    session.start().await.expect_err("fail");

    assert!(
        !session.transcript().contains(&"after_start_failed"),
        "the failure hook belongs to the guarded block only: {:?}",
        session.transcript()
    );
}

// --- stop and shutdown ----------------------------------------------------------------------

#[tokio::test]
async fn stop_persists_and_is_not_teardown() {
    let session = Backend::with_answers(Answers::default());

    session.stop().await.expect("stop");

    assert_eq!(
        session.transcript(),
        ["before_stop", "persist_snapshot", "after_stop"]
    );
    // What it recorded is readable afterwards, with the scheme that produced it.
    assert_eq!(
        session.state().snapshot_fingerprint(),
        Some(("abc123", "v2"))
    );
}

#[tokio::test]
async fn the_after_stop_hook_runs_even_when_persistence_fails() {
    let session = Backend::with_faults(Faults {
        persist_snapshot: true,
        ..Faults::default()
    });

    let error = session.stop().await.expect_err("fail");

    assert_eq!(error.error_code(), ErrorCode::SnapshotPersistError);
    assert_eq!(
        session.transcript(),
        ["before_stop", "persist_snapshot", "after_stop"]
    );
}

#[tokio::test]
async fn shutdown_stops_at_the_first_failure() {
    let session = Backend::with_faults(Faults {
        shutdown_backend: true,
        ..Faults::default()
    });

    session.shutdown().await.expect_err("fail");

    assert_eq!(
        session.transcript(),
        ["before_shutdown", "shutdown_backend"]
    );
}

// --- close ----------------------------------------------------------------------------------

#[tokio::test]
async fn closing_a_session_stops_it_then_shuts_it_down() {
    let session = Backend::with_answers(Answers::default());

    session.close().await.expect("close");

    assert_eq!(
        session.transcript(),
        [
            "run_pre_stop_hooks",
            "before_stop",
            "persist_snapshot",
            "after_stop",
            "before_shutdown",
            "shutdown_backend",
            "after_shutdown",
            "close_dependencies",
        ]
    );
}

#[tokio::test]
async fn a_failed_pre_stop_hook_skips_the_stop_but_still_shuts_down() {
    let session = Backend::with_faults(Faults {
        pre_stop_hooks: true,
        ..Faults::default()
    });

    let error = session.close().await.expect_err("fail");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceStopError);
    let transcript = session.transcript();
    assert!(!transcript.contains(&"persist_snapshot"), "{transcript:?}");
    assert!(transcript.contains(&"shutdown_backend"), "{transcript:?}");
    assert!(transcript.contains(&"close_dependencies"), "{transcript:?}");
}

#[tokio::test]
async fn a_second_close_does_not_persist_what_the_first_one_refused_to() {
    // The callbacks run once, so the second close is told nothing went wrong. Only a flag that
    // outlives the call stops it from saving the state the failure existed to prevent.
    let session = Backend::with_faults(Faults {
        pre_stop_hooks: true,
        ..Faults::default()
    });

    session.close().await.expect_err("first close fails");
    session.clear_transcript();
    session
        .close()
        .await
        .expect("second close reports nothing new");

    let transcript = session.transcript();
    assert!(
        transcript.contains(&"run_pre_stop_hooks:already_ran"),
        "{transcript:?}"
    );
    assert!(
        !transcript.contains(&"persist_snapshot"),
        "the second close must not persist what the first refused: {transcript:?}"
    );
    assert_eq!(session.state().snapshot_fingerprint(), None);
}

#[tokio::test]
async fn a_failed_stop_skips_the_shutdown_and_still_releases_dependencies() {
    let session = Backend::with_faults(Faults {
        persist_snapshot: true,
        ..Faults::default()
    });

    let error = session.close().await.expect_err("fail");

    assert_eq!(error.error_code(), ErrorCode::SnapshotPersistError);
    let transcript = session.transcript();
    assert!(!transcript.contains(&"shutdown_backend"), "{transcript:?}");
    assert!(transcript.contains(&"close_dependencies"), "{transcript:?}");
}

#[tokio::test]
async fn the_first_failure_is_the_one_reported() {
    let session = Backend::with_faults(Faults {
        pre_stop_hooks: true,
        close_dependencies: true,
        ..Faults::default()
    });

    let error = session.close().await.expect_err("fail");

    assert_eq!(error.op(), OpName::Stop);
    assert!(error.message().contains("pre-stop hook"), "{error}");
}

#[tokio::test]
async fn two_closes_run_one_after_the_other() {
    // The reference holds a lock across a whole close, so a second close waits and then runs its
    // own pass — it is not turned away. The pre-stop callbacks have already run by then, so that
    // pass is told nothing went wrong and goes on to stop and shut down again.
    let session = Backend::with_answers(Answers::default());

    let (first, second) = tokio::join!(session.close(), session.close());
    first.expect("first close");
    second.expect("second close");

    let pass = [
        "before_stop",
        "persist_snapshot",
        "after_stop",
        "before_shutdown",
        "shutdown_backend",
        "after_shutdown",
        "close_dependencies",
    ];
    let mut expected = vec!["run_pre_stop_hooks"];
    expected.extend(pass);
    expected.push("run_pre_stop_hooks:already_ran");
    expected.extend(pass);
    assert_eq!(session.transcript(), expected);
}

// --- the workspace surface -------------------------------------------------------------------

#[tokio::test]
async fn the_workspace_operations_are_reachable_through_the_protocol() {
    // Reachable through `dyn`, which is how a host holds one.
    let session: Box<dyn SandboxSession> = Box::new(Backend::with_answers(Answers::default()));

    let listing = session.ls("/workspace".into(), None).await.expect("ls");
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].kind, EntryKind::File);
    assert!(!listing[0].is_dir());

    assert_eq!(
        session
            .read("/workspace/a".into(), None)
            .await
            .expect("read"),
        b"contents"
    );
    session
        .write("/workspace/a".into(), b"x".to_vec(), None)
        .await
        .expect("write");
    session
        .mkdir("/workspace/d".into(), true, None)
        .await
        .expect("mkdir");
    session
        .rm("/workspace/d".into(), true, None)
        .await
        .expect("rm");

    let archive = session.persist_workspace().await.expect("persist");
    session.hydrate_workspace(archive).await.expect("hydrate");
}

#[tokio::test]
async fn a_session_without_a_terminal_refuses_to_start_one() {
    let session = Backend::with_answers(Answers::default());
    assert!(!session.supports_pty());

    let error = session
        .pty_start(PtyStartRequest::new(["top".to_owned()]))
        .await
        .expect_err("refuse");
    assert_eq!(error.error_code(), ErrorCode::PtySessionNotFound);

    let error = session
        .pty_write(PtyWriteRequest::new(PtyProcessId(7), "q"))
        .await
        .expect_err("refuse");
    assert_eq!(error.error_code(), ErrorCode::PtySessionNotFound);

    // Terminating nothing is not an error.
    session.pty_terminate_all().await.expect("terminate");
}

#[tokio::test]
async fn an_unconfigured_port_is_refused_permanently() {
    let session = Backend::with_answers(Answers::default());

    let error = session
        .resolve_exposed_port(8080)
        .await
        .expect_err("refuse");

    assert_eq!(error.error_code(), ErrorCode::ExposedPortUnavailable);
    assert_eq!(
        error.message(),
        "port 8080 is not configured for host exposure"
    );
    // It will not become configured by trying again.
    assert_eq!(error.retryable(), Some(false));
}

#[tokio::test]
async fn a_configured_port_the_backend_cannot_map_is_not_reported_as_unconfigured() {
    // Different cause, different advice: the port is set up, the backend just cannot map it right
    // now, and only the backend can say whether that clears.
    let session = Backend::with_answers(Answers {
        exposed_ports: vec![8080],
        ..Answers::default()
    });

    let error = session
        .resolve_exposed_port(8080)
        .await
        .expect_err("refuse");

    assert_eq!(
        error.message(),
        "port 8080 could not be resolved for host exposure"
    );
    assert_eq!(
        error.context().get("reason"),
        Some(&serde_json::json!("backend_unavailable"))
    );
    assert_eq!(error.retryable(), None, "unclassified, not permanent");
}

// --- the client -------------------------------------------------------------------------------

struct StubClient {
    needs_options: bool,
}

#[async_trait]
impl SandboxClient for StubClient {
    fn backend_id(&self) -> &str {
        "recording"
    }

    fn supports_default_options(&self) -> bool {
        !self.needs_options
    }

    async fn create(&self, request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>> {
        self.check_options(request.options.as_ref())?;
        Ok(Box::new(Backend::with_answers(Answers::default())))
    }

    async fn resume(&self, _state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>> {
        Ok(Box::new(Backend::with_answers(Answers::default())))
    }

    async fn delete(&self, _session: &dyn SandboxSession) -> SandboxResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_backend_that_needs_configuring_is_not_handed_a_session_it_cannot_start() {
    let client = StubClient {
        needs_options: true,
    };

    // `Box<dyn SandboxSession>` is not `Debug`, so the success arm is discarded by hand rather
    // than through `expect_err`.
    let Err(error) = client.create(CreateRequest::new()).await else {
        panic!("a backend needing options must refuse");
    };

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(error.message().contains("needs options"), "{error}");
}

#[tokio::test]
async fn options_reach_the_client_and_are_checked_against_its_backend() {
    let client = StubClient {
        needs_options: true,
    };

    // Options routed to this backend are accepted.
    let mine = DiscriminatedPayload::new("recording").with_field("image", "python:3.14-slim");
    client
        .create(CreateRequest::new().with_options(mine))
        .await
        .expect("create");

    // Options meant for another backend are not silently ignored: a session started from the
    // wrong configuration is worse than one that refused to start.
    let theirs = DiscriminatedPayload::new("docker").with_field("image", "python:3.14-slim");
    let Err(error) = client
        .create(CreateRequest::new().with_options(theirs))
        .await
    else {
        panic!("options for another backend must be refused");
    };
    assert_eq!(
        error.context().get("options_type"),
        Some(&serde_json::json!("docker"))
    );
}

#[tokio::test]
async fn deleting_a_session_leaves_the_caller_holding_it() {
    // A failed cleanup has to leave something to inspect and retry against.
    let client = StubClient {
        needs_options: false,
    };
    let session = client.create(CreateRequest::new()).await.expect("create");

    client.delete(session.as_ref()).await.expect("delete");

    // Still ours afterwards.
    assert_eq!(session.backend_id(), "recording");
}

#[tokio::test]
async fn a_client_drives_a_session_through_the_same_lifecycle() {
    let client = StubClient {
        needs_options: false,
    };
    let session = client
        .create(CreateRequest::new().with_manifest(Manifest::new()))
        .await
        .expect("create");

    session.start().await.expect("start");
    session.close().await.expect("close");
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_client_refuses_to_serialize_a_forged_mount() {
    let forged = SandboxSessionState::new(
        "recording",
        Snapshot::noop(),
        Manifest::new().with_entry(
            "data",
            Entry::new(EntryContent::Extension(
                DiscriminatedPayload::new("s3_mount").with_field("secret_access_key", "AKIA"),
            )),
        ),
    );
    let client = StubClient {
        needs_options: false,
    };

    let error = client.serialize_session_state(&forged).expect_err("refuse");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(!format!("{error:?}").contains("AKIA"), "leaked: {error}");

    let plain = SandboxSessionState::new("recording", Snapshot::noop(), Manifest::new());
    let rendered = client.serialize_session_state(&plain).expect("serialize");
    assert_eq!(rendered["type"], serde_json::json!("recording"));
}

/// A manifest the credential boundary refuses: bucket keys handed to a helper inside the sandbox,
/// with no acknowledgement for the path.
fn refused_at_the_credential_boundary() -> SandboxSessionState {
    let provider = MountProvider::S3(S3Mount {
        bucket: "bucket".to_owned(),
        access_key_id: Some("access-key".to_owned()),
        secret_access_key: Some("boundary-secret".to_owned()),
        ..S3Mount::default()
    });
    let strategy = MountStrategy::in_container(MountPattern::Rclone(RcloneOptions::default()));
    let manifest = Manifest::new().with_entry(
        "data",
        Entry::mount(Mount::new(provider, strategy).expect("supported")),
    );
    SandboxSessionState::new("recording", Snapshot::noop(), manifest)
}

#[tokio::test]
async fn a_start_refused_at_the_credential_boundary_runs_nothing() {
    let backend = Backend::with_answers(Answers::default());
    backend.inner.lock().expect("state").state = refused_at_the_credential_boundary();

    let error = backend.start().await.expect_err("refuse");

    // Refused before the guarded block, so not even the failure hook runs: nothing started.
    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(
        backend.transcript().is_empty(),
        "{:?}",
        backend.transcript()
    );
    assert!(!format!("{error:?}").contains("boundary-secret"));
}

#[tokio::test]
async fn a_stop_refused_at_the_credential_boundary_persists_nothing() {
    let backend = Backend::with_answers(Answers::default());
    backend.inner.lock().expect("state").state = refused_at_the_credential_boundary();

    let error = backend.stop().await.expect_err("refuse");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(
        backend.transcript().is_empty(),
        "{:?}",
        backend.transcript()
    );
}

#[tokio::test]
async fn a_client_refuses_a_manifest_for_create_at_the_credential_boundary() {
    let client = StubClient {
        needs_options: false,
    };

    let error = client
        .validate_manifest_for_create(refused_at_the_credential_boundary().manifest())
        .expect_err("refuse");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    client
        .validate_manifest_for_create(&Manifest::new())
        .expect("nothing to refuse");
}
