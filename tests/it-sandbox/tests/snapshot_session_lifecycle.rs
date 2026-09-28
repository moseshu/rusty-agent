//! A whole stop and start through the protocol's lifecycle, with the snapshot half wired to
//! `ra-sandbox::snapshot::lifecycle` and commands run for real on this machine.
//!
//! The reference's `test_snapshot.py` drives its base session's `stop()` and `start()` with a
//! session whose commands are real subprocesses, so the fingerprint helper actually installs, hashes
//! the workspace and writes its cache. This does the same: the only things recorded rather than
//! done are what the reference's tracking sessions record — archiving, extracting, clearing,
//! materializing and creating accounts — so each test reads which branch the start took.
//!
//! Unlike the local backend, nothing here is fenced, so the fingerprint really is computed: this is
//! the "a fingerprint exists and is compared" path end to end, which the local backend on macOS
//! cannot reach because its fence denies writes under `/tmp`.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, Dependencies, Entry, ExecRequest, ExecResult, FileEntry, Manifest,
    MaterializationResult, SandboxError, SandboxResult, SandboxSession, SandboxSessionState,
    SessionPath, SessionResources, Snapshot, SnapshotFingerprint, User,
};
use ra_sandbox::snapshot::lifecycle::{
    SNAPSHOT_FINGERPRINT_VERSION, SnapshotLifecycle, parse_fingerprint_record,
};
use ra_sandbox::snapshot::{BuiltinSnapshotStore, SnapshotStore};

/// A snapshot type the host registered, that stores nothing and has nothing to restore.
///
/// Named like the built-in one on purpose: only the built-in `noop` type skips persisting, and a
/// host type that merely stores nothing is still asked to.
const TEST_NOOP: &str = "test-noop";

/// A snapshot type the host registered, that always has this payload to restore.
const TEST_RESTORABLE: &str = "test-restorable";

/// What a restorable test snapshot hands back.
const RESTORED: &[u8] = b"restored-workspace";

/// Storage for the two host snapshot types, and the built-in storage for everything else.
struct TestStore;

#[async_trait]
impl SnapshotStore for TestStore {
    async fn persist(
        &self,
        snapshot: &Snapshot,
        data: Vec<u8>,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<()> {
        match snapshot.snapshot_type() {
            TEST_NOOP | TEST_RESTORABLE => Ok(()),
            _ => {
                BuiltinSnapshotStore
                    .persist(snapshot, data, dependencies)
                    .await
            }
        }
    }

    async fn restore(
        &self,
        snapshot: &Snapshot,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<Vec<u8>> {
        match snapshot.snapshot_type() {
            TEST_RESTORABLE => Ok(RESTORED.to_vec()),
            TEST_NOOP => Err(SandboxError::snapshot_not_restorable(
                snapshot.id(),
                "<test-noop>",
            )),
            _ => BuiltinSnapshotStore.restore(snapshot, dependencies).await,
        }
    }

    async fn restorable(
        &self,
        snapshot: &Snapshot,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<bool> {
        match snapshot.snapshot_type() {
            TEST_RESTORABLE => Ok(true),
            TEST_NOOP => Ok(false),
            _ => {
                BuiltinSnapshotStore
                    .restorable(snapshot, dependencies)
                    .await
            }
        }
    }
}

/// How a session was asked to materialize its manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Applied {
    only_ephemeral: bool,
    provision_accounts: bool,
}

/// The reference's `_PersistTrackingSession` and `_ResumeTrackingSession` in one.
struct TrackingSession {
    resources: SessionResources,
    state: Mutex<SandboxSessionState>,
    store: TestStore,
    running: bool,
    workspace_preserved: bool,
    system_preserved: bool,
    persist_workspace_calls: AtomicUsize,
    clear_calls: AtomicUsize,
    hydrated: Mutex<Vec<Vec<u8>>>,
    applied: Mutex<Vec<Applied>>,
    provision_calls: AtomicUsize,
}

impl TrackingSession {
    /// A session over `root`, whose backend kept its workspace and its accounts as asked.
    ///
    /// The state starts with the root recorded as ready exactly when the workspace is preserved,
    /// which is the reference's default for these sessions.
    fn new(snapshot: Snapshot, root: &Path, workspace_preserved: bool) -> Self {
        let manifest = Manifest::new().with_root(root.to_string_lossy().into_owned());
        let state = SandboxSessionState::new("tracking", snapshot, manifest)
            .with_workspace_root_ready(workspace_preserved);
        Self {
            resources: SessionResources::new(),
            state: Mutex::new(state),
            store: TestStore,
            running: true,
            workspace_preserved,
            system_preserved: false,
            persist_workspace_calls: AtomicUsize::new(0),
            clear_calls: AtomicUsize::new(0),
            hydrated: Mutex::new(Vec::new()),
            applied: Mutex::new(Vec::new()),
            provision_calls: AtomicUsize::new(0),
        }
    }

    /// A resumable session: a restorable host snapshot over a preserved workspace.
    fn resumable(root: &Path) -> Self {
        Self::new(
            Snapshot::new(TEST_RESTORABLE, "resume-snapshot"),
            root,
            true,
        )
    }

    fn with_root_ready(self, ready: bool) -> Self {
        self.update_state(|state| state.with_workspace_root_ready(ready));
        self
    }

    const fn with_system_preserved(mut self, preserved: bool) -> Self {
        self.system_preserved = preserved;
        self
    }

    fn update_state(&self, change: impl FnOnce(SandboxSessionState) -> SandboxSessionState) {
        let mut guard = self.state.lock().expect("state");
        *guard = change(guard.clone());
    }

    fn lifecycle(&self) -> SnapshotLifecycle<'_> {
        SnapshotLifecycle::new(self, &self.store)
    }

    fn persist_workspace_calls(&self) -> usize {
        self.persist_workspace_calls.load(Ordering::SeqCst)
    }

    fn clear_calls(&self) -> usize {
        self.clear_calls.load(Ordering::SeqCst)
    }

    fn provision_calls(&self) -> usize {
        self.provision_calls.load(Ordering::SeqCst)
    }

    fn hydrated(&self) -> Vec<Vec<u8>> {
        self.hydrated.lock().expect("hydrated").clone()
    }

    fn applied(&self) -> Vec<Applied> {
        self.applied.lock().expect("applied").clone()
    }
}

#[async_trait]
impl SandboxSession for TrackingSession {
    fn backend_id(&self) -> &str {
        "tracking"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.lock().expect("state").clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    /// Runs the command on this machine, as the reference's tracking sessions do.
    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        let command = request.command.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(&command[0])
                .args(&command[1..])
                .output()
        })
        .await
        .expect("join")
        .map_err(|error| {
            SandboxError::exec_transport(request.command.clone(), Some(&error.to_string()))
        })?;
        Ok(ExecResult::new(
            output.stdout,
            output.stderr,
            output.status.code().unwrap_or(-1),
        ))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(self.running)
    }

    /// Counted as the workspace being cleared for a restore, which is the only thing that lists
    /// the root here; answering "not there" keeps the clear from removing anything real.
    async fn ls(&self, path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        let path = path.as_str();
        if path == self.state().manifest().root {
            self.clear_calls.fetch_add(1, Ordering::SeqCst);
        }
        Err(SandboxError::workspace_read_not_found(path))
    }

    async fn rm(
        &self,
        path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        panic!("rm({path}) should not be called in this test")
    }

    async fn mkdir(
        &self,
        path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        panic!("mkdir({path}) should not be called in this test")
    }

    async fn read(&self, path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        let path = path.as_str();
        panic!("read({path}) should not be called in this test")
    }

    async fn write(
        &self,
        path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        panic!("write({path}) should not be called in this test")
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        self.persist_workspace_calls.fetch_add(1, Ordering::SeqCst);
        Ok(b"tracked".to_vec())
    }

    async fn hydrate_workspace(&self, data: Vec<u8>) -> SandboxResult<()> {
        self.hydrated.lock().expect("hydrated").push(data);
        Ok(())
    }

    fn workspace_state_preserved_on_start(&self) -> bool {
        self.workspace_preserved
    }

    fn system_state_preserved_on_start(&self) -> bool {
        self.system_preserved
    }

    async fn snapshot_restorable(&self) -> SandboxResult<bool> {
        self.lifecycle().restorable().await
    }

    async fn can_skip_snapshot_restore(&self, is_running: bool) -> SandboxResult<bool> {
        self.lifecycle().can_skip_restore(is_running).await
    }

    async fn restore_snapshot(&self) -> SandboxResult<()> {
        self.lifecycle().restore_on_resume().await
    }

    async fn persist_snapshot(&self) -> SandboxResult<()> {
        self.lifecycle().persist().await
    }

    async fn record_snapshot_fingerprint(
        &self,
        fingerprint: Option<SnapshotFingerprint>,
    ) -> SandboxResult<()> {
        self.update_state(|state| match fingerprint {
            Some(fingerprint) => {
                state.with_snapshot_fingerprint(fingerprint.fingerprint(), fingerprint.version())
            }
            None => state.without_snapshot_fingerprint(),
        });
        Ok(())
    }

    async fn record_workspace_root_ready(&self) -> SandboxResult<()> {
        self.update_state(|state| state.with_workspace_root_ready(true));
        Ok(())
    }

    async fn provision_accounts(&self) -> SandboxResult<()> {
        self.provision_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn apply_manifest(
        &self,
        provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        self.applied.lock().expect("applied").push(Applied {
            only_ephemeral: false,
            provision_accounts,
        });
        Ok(MaterializationResult::new())
    }

    /// The reference's `apply_manifest(only_ephemeral=True)`, which never provisions accounts.
    async fn reapply_ephemeral_manifest(&self) -> SandboxResult<()> {
        self.applied.lock().expect("applied").push(Applied {
            only_ephemeral: true,
            provision_accounts: false,
        });
        Ok(())
    }
}

const EPHEMERAL_ONLY: Applied = Applied {
    only_ephemeral: true,
    provision_accounts: false,
};

/// Where the fingerprint helper caches its answer for `state`, on this machine.
fn cached_fingerprint_path(state: &SandboxSessionState) -> String {
    format!(
        "/tmp/rusty-agent/session-state/{}/fingerprint.json",
        state.session_id().simple()
    )
}

// --- stop ------------------------------------------------------------------------------------

/// `test_noop_snapshot_stop_skips_workspace_persist`
#[tokio::test]
async fn stopping_with_the_built_in_noop_snapshot_archives_nothing() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::new(Snapshot::noop(), workspace.path(), false);

    session.stop().await.expect("stop");

    assert_eq!(session.persist_workspace_calls(), 0);
}

/// `test_non_noop_snapshot_stop_persists_workspace`
#[tokio::test]
async fn stopping_with_a_host_snapshot_that_stores_nothing_still_archives() {
    // Only the built-in type is known to store nothing. A host type is asked to persist whatever
    // it does with the archive, so the archive is made.
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::new(
        Snapshot::new(TEST_NOOP, "custom-snapshot"),
        workspace.path(),
        false,
    );

    session.stop().await.expect("stop");

    assert_eq!(session.persist_workspace_calls(), 1);
}

/// `test_non_noop_snapshot_stop_records_snapshot_fingerprint`
#[tokio::test]
async fn a_stop_records_the_fingerprint_the_helper_cached() {
    let workspace = tempfile::tempdir().expect("temp");
    std::fs::write(workspace.path().join("tracked.txt"), b"tracked").expect("write");
    let session = TrackingSession::new(
        Snapshot::new(TEST_NOOP, "custom-snapshot"),
        workspace.path(),
        false,
    );

    session.stop().await.expect("stop");

    let state = session.state();
    let (fingerprint, version) = state
        .snapshot_fingerprint()
        .expect("the helper ran on this machine, so a fingerprint was recorded");
    assert_eq!(version, SNAPSHOT_FINGERPRINT_VERSION);
    let cached = parse_fingerprint_record(
        &std::fs::read(cached_fingerprint_path(&state)).expect("the helper left its answer"),
    )
    .expect("a record");
    assert_eq!(cached.fingerprint(), fingerprint);
    assert_eq!(cached.version(), version);
}

// --- start with a restorable snapshot --------------------------------------------------------

/// `test_start_skips_snapshot_restore_when_live_workspace_fingerprint_matches`
#[tokio::test]
async fn a_preserved_workspace_that_still_matches_its_snapshot_is_kept() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::resumable(workspace.path());
    std::fs::write(workspace.path().join("tracked.txt"), b"tracked").expect("write");

    session.stop().await.expect("stop");
    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 0);
    assert!(session.hydrated().is_empty());
    assert_eq!(session.provision_calls(), 0);
    assert_eq!(session.applied(), [EPHEMERAL_ONLY]);
}

/// `test_start_restores_snapshot_when_live_workspace_fingerprint_mismatches`
#[tokio::test]
async fn a_preserved_workspace_that_drifted_is_restored_over() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::resumable(workspace.path());
    let tracked = workspace.path().join("tracked.txt");
    std::fs::write(&tracked, b"tracked").expect("write");

    session.stop().await.expect("stop");
    std::fs::write(&tracked, b"drifted").expect("write");
    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 1);
    assert_eq!(session.hydrated(), [RESTORED.to_vec()]);
    assert_eq!(session.provision_calls(), 1);
    assert_eq!(session.applied(), [EPHEMERAL_ONLY]);
}

/// A change made to a stopped session's manifest.
type ManifestChange = fn(Manifest) -> Manifest;

/// `test_start_restores_snapshot_when_resume_manifest_changes`, both parameters.
#[tokio::test]
async fn a_workspace_declared_differently_since_the_stop_is_restored_over() {
    // The bytes on disk are unchanged; what changed is what the session is supposed to have, and
    // that is half of what the fingerprint answers.
    let changes: [(&str, ManifestChange); 2] = [
        ("ephemeral_entry", |manifest| {
            manifest.with_entry(
                "ephemeral.txt",
                Entry::file(b"temp".to_vec()).ephemeral(true),
            )
        }),
        ("user", |manifest| {
            manifest.with_user(User::new("sandbox-user"))
        }),
    ];

    for (name, change) in changes {
        let workspace = tempfile::tempdir().expect("temp");
        let session = TrackingSession::resumable(workspace.path());
        std::fs::write(workspace.path().join("tracked.txt"), b"tracked").expect("write");

        session.stop().await.expect("stop");
        session.update_state(|state| {
            let manifest = change(state.manifest().clone());
            state.with_manifest(manifest)
        });
        session.start().await.expect("start");

        assert_eq!(session.clear_calls(), 1, "{name}");
        assert_eq!(session.hydrated(), [RESTORED.to_vec()], "{name}");
        assert_eq!(session.provision_calls(), 1, "{name}");
        assert_eq!(session.applied(), [EPHEMERAL_ONLY], "{name}");
    }
}

// --- start without a restorable snapshot -----------------------------------------------------

/// `test_start_applies_full_manifest_for_fresh_non_restorable_backend`
#[tokio::test]
async fn a_fresh_backend_with_nothing_to_restore_materializes_everything() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::new(Snapshot::noop(), workspace.path(), false);

    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 0);
    assert!(session.hydrated().is_empty());
    assert_eq!(session.provision_calls(), 0);
    assert_eq!(
        session.applied(),
        [Applied {
            only_ephemeral: false,
            provision_accounts: true
        }]
    );
}

/// `test_start_reapplies_only_ephemeral_manifest_for_preserved_non_restorable_backend`
#[tokio::test]
async fn a_preserved_backend_with_nothing_to_restore_rebuilds_only_the_ephemeral_parts() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::new(Snapshot::noop(), workspace.path(), true);

    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 0);
    assert!(session.hydrated().is_empty());
    assert_eq!(session.provision_calls(), 0);
    assert_eq!(session.applied(), [EPHEMERAL_ONLY]);
}

/// `test_start_reapplies_only_ephemeral_manifest_when_preserved_probe_succeeds`
#[tokio::test]
async fn a_preserved_root_the_probe_finds_is_reused_and_recorded() {
    // Nothing recorded the root as ready, so the protocol's own probe — `test -d`, run for real —
    // is what proves it.
    let workspace = tempfile::tempdir().expect("temp");
    let session =
        TrackingSession::new(Snapshot::noop(), workspace.path(), true).with_root_ready(false);

    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 0);
    assert!(session.hydrated().is_empty());
    assert_eq!(session.provision_calls(), 0);
    assert_eq!(session.applied(), [EPHEMERAL_ONLY]);
    assert!(session.state().workspace_root_ready());
}

/// `test_start_applies_full_manifest_when_preserved_non_restorable_workspace_unproven`
#[tokio::test]
async fn a_preserved_root_the_probe_cannot_find_is_materialized_in_full() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::new(
        Snapshot::noop(),
        &workspace.path().join("missing-workspace"),
        true,
    )
    .with_root_ready(false);

    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 0);
    assert!(session.hydrated().is_empty());
    assert_eq!(session.provision_calls(), 0);
    assert_eq!(
        session.applied(),
        [Applied {
            only_ephemeral: false,
            provision_accounts: true
        }]
    );
}

/// `test_start_applies_full_manifest_without_accounts_when_system_state_preserved`
#[tokio::test]
async fn a_backend_that_kept_its_accounts_materializes_without_creating_them_again() {
    let workspace = tempfile::tempdir().expect("temp");
    let session = TrackingSession::new(
        Snapshot::noop(),
        &workspace.path().join("missing-workspace"),
        true,
    )
    .with_root_ready(false)
    .with_system_preserved(true);

    session.start().await.expect("start");

    assert_eq!(session.clear_calls(), 0);
    assert!(session.hydrated().is_empty());
    assert_eq!(session.provision_calls(), 0);
    assert_eq!(
        session.applied(),
        [Applied {
            only_ephemeral: false,
            provision_accounts: false
        }]
    );
}
