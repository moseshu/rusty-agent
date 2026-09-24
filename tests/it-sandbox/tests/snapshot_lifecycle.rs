//! `ra-sandbox::snapshot::lifecycle`: what a session does with its snapshot on the way down and on
//! the way back up.
//!
//! The decisions here are about not losing a workspace, and each has a way of going wrong quietly.
//! A persist that records a fingerprint for an archive that was never stored invites the next
//! resume to skip a restore it needed. A restore that extracts over what is already there leaves a
//! mixture of two workspaces. A fingerprint that could not be computed has to read as "unknown"
//! rather than "unchanged". The session and the storage underneath are recorders, so each decision
//! is visible without a real workspace.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, Dependencies, Entry, EntryKind, ErrorCode, ExecRequest, ExecResult, FileEntry,
    Manifest, Mount, MountPattern, MountProvider, MountStrategy, MountpointOptions, Permissions,
    S3Mount, SandboxError, SandboxResult, SandboxSession, SandboxSessionState, SessionResources,
    Snapshot, SnapshotFingerprint,
};
use ra_sandbox::runtime_helpers::workspace_fingerprint_helper;
use ra_sandbox::snapshot::SnapshotStore;
use ra_sandbox::snapshot::lifecycle::{
    SNAPSHOT_FINGERPRINT_VERSION, SnapshotLifecycle, fingerprint_skip_relpaths,
    parse_fingerprint_record, resume_manifest_digest,
};

/// The workspace these sessions describe.
const ROOT: &str = "/workspace";

/// What the recorded workspace archives to.
const ARCHIVE: &[u8] = b"workspace-tar";

/// A manifest rooted where these tests expect.
fn manifest() -> Manifest {
    Manifest::new().with_root(ROOT)
}

/// One thing the lifecycle asked the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Exec(Vec<String>),
    Ls(String),
    Rm(String),
    Hydrate(Vec<u8>),
    PersistWorkspace,
}

/// A session that records what it was asked to do and answers as configured.
struct RecordingSession {
    resources: SessionResources,
    state: Mutex<SandboxSessionState>,
    calls: Mutex<Vec<Call>>,
    /// What the fingerprint helper prints, or the exit code it fails with.
    fingerprint: Result<String, i32>,
    /// Whether the helper can be installed at all.
    helper_installs: bool,
    /// What each listed directory contains.
    listings: BTreeMap<String, Vec<FileEntry>>,
}

impl RecordingSession {
    fn new(snapshot: Snapshot, manifest: Manifest) -> Self {
        Self {
            resources: SessionResources::new(),
            state: Mutex::new(SandboxSessionState::new("recording", snapshot, manifest)),
            calls: Mutex::new(Vec::new()),
            fingerprint: Ok(record("workspace-hash")),
            helper_installs: true,
            listings: BTreeMap::new(),
        }
    }

    fn printing(mut self, payload: &str) -> Self {
        self.fingerprint = Ok(payload.to_owned());
        self
    }

    fn failing_fingerprint(mut self) -> Self {
        self.fingerprint = Err(2);
        self
    }

    const fn without_a_writable_helper_root(mut self) -> Self {
        self.helper_installs = false;
        self
    }

    fn already_fingerprinted(self, fingerprint: &str) -> Self {
        let updated = self
            .state
            .lock()
            .expect("state")
            .clone()
            .with_snapshot_fingerprint(fingerprint, SNAPSHOT_FINGERPRINT_VERSION);
        *self.state.lock().expect("state") = updated;
        self
    }

    fn listing(mut self, directory: &str, entries: Vec<FileEntry>) -> Self {
        self.listings.insert(directory.to_owned(), entries);
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Exec(command) => Some(command),
                _ => None,
            })
            .collect()
    }

    fn removals(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Rm(path) => Some(path),
                _ => None,
            })
            .collect()
    }

    fn recorded_fingerprint(&self) -> Option<(String, String)> {
        self.state
            .lock()
            .expect("state")
            .snapshot_fingerprint()
            .map(|(fingerprint, version)| (fingerprint.to_owned(), version.to_owned()))
    }
}

#[async_trait]
impl SandboxSession for RecordingSession {
    fn backend_id(&self) -> &str {
        "recording"
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    fn state(&self) -> SandboxSessionState {
        self.state.lock().expect("state").clone()
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::Exec(request.command.clone()));
        let program = request.command.first().cloned().unwrap_or_default();

        if program == "sh" {
            // Installing the helper.
            let exit_code = i32::from(!self.helper_installs);
            return Ok(ExecResult::new(Vec::new(), Vec::new(), exit_code));
        }
        if program == workspace_fingerprint_helper().install_path() {
            return Ok(match &self.fingerprint {
                Ok(payload) => ExecResult::new(payload.clone().into_bytes(), Vec::new(), 0),
                Err(exit_code) => ExecResult::new(Vec::new(), b"no tar".to_vec(), *exit_code),
            });
        }
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, path: &str, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::Ls(path.to_owned()));
        self.listings.get(path).cloned().ok_or_else(|| {
            SandboxError::workspace_read_not_found(path)
        })
    }

    async fn rm(&self, path: &str, _recursive: bool, _user: AsUser) -> SandboxResult<()> {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::Rm(path.to_owned()));
        Ok(())
    }

    async fn mkdir(&self, _path: &str, _parents: bool, _user: AsUser) -> SandboxResult<()> {
        Ok(())
    }

    async fn read(&self, path: &str, _user: AsUser) -> SandboxResult<Vec<u8>> {
        Err(SandboxError::workspace_read_not_found(path))
    }

    async fn write(&self, _path: &str, _data: Vec<u8>, _user: AsUser) -> SandboxResult<()> {
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::PersistWorkspace);
        Ok(ARCHIVE.to_vec())
    }

    async fn hydrate_workspace(&self, data: Vec<u8>) -> SandboxResult<()> {
        self.calls.lock().expect("calls").push(Call::Hydrate(data));
        Ok(())
    }

    async fn record_snapshot_fingerprint(
        &self,
        fingerprint: Option<SnapshotFingerprint>,
    ) -> SandboxResult<()> {
        let mut guard = self.state.lock().expect("state");
        let updated = match fingerprint {
            Some(fingerprint) => guard
                .clone()
                .with_snapshot_fingerprint(fingerprint.fingerprint(), fingerprint.version()),
            None => guard.clone().without_snapshot_fingerprint(),
        };
        *guard = updated;
        Ok(())
    }
}

/// Storage that records what it was handed, and answers as configured.
struct RecordingStore {
    persisted: Mutex<Vec<Vec<u8>>>,
    restores_with: Vec<u8>,
    refuses_to_persist: bool,
}

impl RecordingStore {
    fn new() -> Self {
        Self {
            persisted: Mutex::new(Vec::new()),
            restores_with: b"stored-workspace".to_vec(),
            refuses_to_persist: false,
        }
    }

    const fn refusing_to_persist(mut self) -> Self {
        self.refuses_to_persist = true;
        self
    }

    fn persisted(&self) -> Vec<Vec<u8>> {
        self.persisted.lock().expect("persisted").clone()
    }
}

#[async_trait]
impl SnapshotStore for RecordingStore {
    async fn persist(
        &self,
        _snapshot: &Snapshot,
        data: Vec<u8>,
        _dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<()> {
        if self.refuses_to_persist {
            return Err(SandboxError::snapshot_persist("snap-1", "/snapshots"));
        }
        self.persisted.lock().expect("persisted").push(data);
        Ok(())
    }

    async fn restore(
        &self,
        _snapshot: &Snapshot,
        _dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<Vec<u8>> {
        Ok(self.restores_with.clone())
    }

    async fn restorable(
        &self,
        _snapshot: &Snapshot,
        _dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<bool> {
        Ok(true)
    }
}

/// What the fingerprint helper prints for `hash`.
fn record(hash: &str) -> String {
    format!(r#"{{"fingerprint":"{hash}","version":"{SNAPSHOT_FINGERPRINT_VERSION}"}}"#)
}

/// A listed entry under the workspace.
fn entry(path: &str, kind: EntryKind) -> FileEntry {
    FileEntry::new(path, Permissions::default()).with_kind(kind)
}

#[tokio::test]
async fn persisting_stores_the_archive_and_records_what_it_hashed_to() {
    let session = RecordingSession::new(Snapshot::local("snap-1", "/snapshots").expect("named"), manifest());
    let store = RecordingStore::new();

    SnapshotLifecycle::new(&session, &store)
        .persist()
        .await
        .expect("persist");

    assert_eq!(store.persisted(), vec![ARCHIVE.to_vec()]);
    assert_eq!(
        session.recorded_fingerprint(),
        Some((
            "workspace-hash".to_owned(),
            SNAPSHOT_FINGERPRINT_VERSION.to_owned()
        ))
    );
    // Hashed before the archive was made, so the fingerprint describes what was stored rather than
    // a workspace that had moved on by the time it was read.
    let order: Vec<_> = session
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Exec(_) | Call::PersistWorkspace))
        .collect();
    assert!(matches!(order.last(), Some(Call::PersistWorkspace)), "{order:?}");
}

#[tokio::test]
async fn a_session_that_stores_nothing_does_not_hash_its_workspace() {
    // There is nothing to compare a fingerprint against later, so computing one would be a tar of
    // the whole workspace for an answer nobody reads.
    let session = RecordingSession::new(Snapshot::noop(), manifest());
    let store = RecordingStore::new();

    SnapshotLifecycle::new(&session, &store)
        .persist()
        .await
        .expect("persist");

    assert!(store.persisted().is_empty());
    assert!(session.calls().is_empty(), "{:?}", session.calls());
}

#[tokio::test]
async fn a_workspace_that_could_not_be_hashed_is_still_stored() {
    // The local backend's macOS fence denies writes under `/tmp`, where the helper installs, so
    // this is the ordinary outcome there rather than an exotic one.
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    )
    .without_a_writable_helper_root()
    .already_fingerprinted("from-an-earlier-persist");
    let store = RecordingStore::new();

    SnapshotLifecycle::new(&session, &store)
        .persist()
        .await
        .expect("persist");

    assert_eq!(store.persisted(), vec![ARCHIVE.to_vec()]);
    // And the fingerprint an earlier persist left behind is forgotten: comparing it against a
    // workspace this persist could not hash is how a resume skips a restore it needed.
    assert_eq!(session.recorded_fingerprint(), None);
}

#[tokio::test]
async fn a_store_that_refused_takes_the_cached_fingerprint_with_it() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    );
    let store = RecordingStore::new().refusing_to_persist();

    let error = SnapshotLifecycle::new(&session, &store)
        .persist()
        .await
        .expect_err("the store refused");

    assert_eq!(error.error_code(), ErrorCode::SnapshotPersistError);
    // The helper wrote the hash into the sandbox before the archive was stored. Nothing was stored,
    // so that cached value describes a snapshot that does not exist.
    let removed_cache = session
        .commands()
        .iter()
        .any(|command| command.first().is_some_and(|program| program == "rm"));
    assert!(removed_cache, "{:?}", session.commands());
    assert_eq!(session.recorded_fingerprint(), None);
}

#[tokio::test]
async fn a_restore_empties_the_workspace_before_extracting_into_it() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    )
    .listing(
        ROOT,
        vec![
            entry("/workspace/src", EntryKind::Directory),
            entry("/workspace/notes.md", EntryKind::File),
        ],
    );
    let store = RecordingStore::new();

    SnapshotLifecycle::new(&session, &store)
        .restore_on_resume()
        .await
        .expect("restore");

    assert_eq!(
        session.removals(),
        vec!["/workspace/src", "/workspace/notes.md"]
    );
    // Cleared first, then extracted: anything left behind would survive into a workspace that is
    // supposed to be what was stored.
    assert_eq!(
        session.calls().last(),
        Some(&Call::Hydrate(b"stored-workspace".to_vec()))
    );
}

#[tokio::test]
async fn a_restore_leaves_what_an_ephemeral_mount_owns_alone() {
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
    let manifest = manifest().with_entry("cache/mounted", Entry::mount(mount).ephemeral(true));
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest,
    )
    .listing(
        ROOT,
        vec![
            entry("/workspace/cache", EntryKind::Directory),
            entry("/workspace/notes.md", EntryKind::File),
        ],
    )
    .listing(
        "/workspace/cache",
        vec![
            entry("/workspace/cache/mounted", EntryKind::Directory),
            entry("/workspace/cache/scratch", EntryKind::File),
        ],
    );
    let store = RecordingStore::new();

    SnapshotLifecycle::new(&session, &store)
        .restore_on_resume()
        .await
        .expect("restore");

    // The directory on the way to the mount is descended into rather than removed, and the mount
    // point itself is left: removing it would reach through to whatever is mounted there.
    // Depth first, in listing order: the directory on the way to the mount is cleared out before
    // its sibling, because it is reached first.
    assert_eq!(
        session.removals(),
        vec!["/workspace/cache/scratch", "/workspace/notes.md"]
    );
}

#[tokio::test]
async fn a_resume_keeps_a_workspace_that_still_hashes_to_the_stored_value() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    )
    .already_fingerprinted("workspace-hash");
    let store = RecordingStore::new();

    assert!(
        SnapshotLifecycle::new(&session, &store)
            .can_skip_restore(true)
            .await
            .expect("compare")
    );
}

#[tokio::test]
async fn a_resume_restores_over_a_workspace_that_has_drifted() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    )
    .already_fingerprinted("what-was-stored")
    .printing(&record("what-is-there-now"));
    let store = RecordingStore::new();

    assert!(
        !SnapshotLifecycle::new(&session, &store)
            .can_skip_restore(true)
            .await
            .expect("compare")
    );
}

#[tokio::test]
async fn a_workspace_that_cannot_be_hashed_is_not_assumed_to_match() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    )
    .already_fingerprinted("what-was-stored")
    .failing_fingerprint();
    let store = RecordingStore::new();

    assert!(
        !SnapshotLifecycle::new(&session, &store)
            .can_skip_restore(true)
            .await
            .expect("compare")
    );
}

#[tokio::test]
async fn a_session_with_no_stored_fingerprint_has_nothing_to_compare() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    );
    let store = RecordingStore::new();

    assert!(
        !SnapshotLifecycle::new(&session, &store)
            .can_skip_restore(true)
            .await
            .expect("compare")
    );
    // Not even hashed: there is nothing the answer could be compared against.
    assert!(session.calls().is_empty(), "{:?}", session.calls());
}

#[tokio::test]
async fn a_backend_that_is_not_running_cannot_vouch_for_its_workspace() {
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest(),
    )
    .already_fingerprinted("workspace-hash");
    let store = RecordingStore::new();

    assert!(
        !SnapshotLifecycle::new(&session, &store)
            .can_skip_restore(false)
            .await
            .expect("compare")
    );
    assert!(session.calls().is_empty(), "{:?}", session.calls());
}

#[tokio::test]
async fn the_hashing_command_names_the_workspace_the_scheme_and_what_to_leave_out() {
    let manifest = manifest()
        .with_entry("cache.txt", Entry::file("scratch").ephemeral(true))
        .with_entry("notes.md", Entry::file("kept"));
    let session = RecordingSession::new(
        Snapshot::local("snap-1", "/snapshots").expect("named"),
        manifest.clone(),
    );
    let store = RecordingStore::new();

    SnapshotLifecycle::new(&session, &store)
        .compute_and_cache_fingerprint()
        .await
        .expect("hash");

    let helper = workspace_fingerprint_helper();
    let session_id = session.state().session_id().simple().to_string();
    let run = session
        .commands()
        .into_iter()
        .find(|command| command.first().is_some_and(|program| *program == *helper.install_path()))
        .expect("the helper ran");
    assert_eq!(
        run,
        vec![
            helper.install_path().to_owned(),
            ROOT.to_owned(),
            SNAPSHOT_FINGERPRINT_VERSION.to_owned(),
            format!("/tmp/rusty-agent/session-state/{session_id}/fingerprint.json"),
            resume_manifest_digest(&manifest).expect("digest"),
            // What the manifest said not to persist is not hashed either: a fingerprint over
            // content the archive leaves out would never match the archive it describes.
            "cache.txt".to_owned(),
        ]
    );
}

#[test]
fn what_is_left_out_of_a_hash_is_what_is_left_out_of_the_archive() {
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
    let manifest = manifest()
        .with_entry("cache.txt", Entry::file("scratch").ephemeral(true))
        .with_entry("mounted", Entry::mount(mount).ephemeral(true));
    let session = RecordingSession::new(Snapshot::noop(), manifest);
    // What the session created at runtime and excluded from its archives is excluded from the hash
    // for the same reason.
    session
        .register_persist_workspace_skip_path(".sandbox-rclone-config/session")
        .expect("outside every mount");

    let skipped: Vec<String> = fingerprint_skip_relpaths(&session)
        .expect("skip paths")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();

    assert_eq!(
        skipped,
        vec![
            ".sandbox-rclone-config/session".to_owned(),
            "cache.txt".to_owned(),
            "mounted".to_owned()
        ]
    );
}

#[test]
fn a_manifest_that_says_something_different_hashes_differently() {
    let declared = manifest().with_entry("notes.md", Entry::file("kept"));
    let same = manifest().with_entry("notes.md", Entry::file("kept"));
    let other = manifest().with_entry("notes.md", Entry::file("changed"));

    assert_eq!(
        resume_manifest_digest(&declared).expect("digest"),
        resume_manifest_digest(&same).expect("digest")
    );
    // What is on disk is only half of what a resumed session is supposed to have; the declaration
    // is the other half, so a workspace that matches byte for byte under a different manifest is
    // not a workspace this session can keep.
    assert_ne!(
        resume_manifest_digest(&declared).expect("digest"),
        resume_manifest_digest(&other).expect("digest")
    );
}

#[test]
fn a_record_that_is_not_the_agreed_shape_is_refused() {
    let refused = [
        &b"not json"[..],
        b"[]",
        br#"{"version":"workspace_tar_sha256_v1"}"#,
        br#"{"fingerprint":"","version":"workspace_tar_sha256_v1"}"#,
        br#"{"fingerprint":"hash"}"#,
        br#"{"fingerprint":"hash","version":7}"#,
    ];

    for payload in refused {
        let error = parse_fingerprint_record(payload).expect_err("refused");
        assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    }

    let parsed = parse_fingerprint_record(record("hash").as_bytes()).expect("parsed");
    assert_eq!(parsed.fingerprint(), "hash");
    assert_eq!(parsed.version(), SNAPSHOT_FINGERPRINT_VERSION);
}
