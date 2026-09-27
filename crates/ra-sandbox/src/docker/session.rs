//! A workspace inside a container, and the commands run against it.
//!
//! Everything the session does to the workspace it does by running something inside the container:
//! reading a file is `cat`, writing one is `cat >` fed through standard input, persisting the
//! workspace copies it to a staging directory first. The reference avoids the daemon's archive
//! upload for writes and reads because, with volume-driver mounts attached, some drivers reject the
//! daemon re-running their mount setup; the one archive call left is the download of a staged copy,
//! which is outside every mount.
//!
//! The session does not remove its container. That belongs to the client, which is the only party
//! that knows whether this session created it.

mod pty;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, CompressionScheme, ErrorCode, ExecRequest, ExecResult, ExposedPortEndpoint, FileEntry,
    Manifest, MaterializationResult, OpName, PosixPath, PtyExecUpdate, PtyStartRequest,
    PtyWriteRequest, SandboxArchiveLimits, SandboxError, SandboxResult, SandboxSession,
    SandboxSessionState, SessionResources, ShellInvocation, SnapshotFingerprint,
    manifest_has_configured_mount_authority, replace_protected_mount_error,
};
#[cfg(unix)]
use ra_core::sandbox::{Entry, MaterializedFile};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::archive::WorkspaceArchiveExtractor;
use crate::mounts::{BuiltinMountLifecycle, MountLifecycle};
use crate::remote;
use crate::runtime_helpers::{ensure_installed, resolve_workspace_path_helper};
use crate::snapshot::lifecycle::SnapshotLifecycle;
use crate::snapshot::{BuiltinSnapshotStore, SnapshotStore};
use crate::tar_utils::{
    StripPrefixError, TarValidation, strip_tar_member_prefix, validate_tar_bytes,
};

use super::api::{DockerApi, DockerApiError, DockerApiErrorKind, ExecRunRequest};
use super::container::docker_port_key;
use super::stream::stream_into_exec;
use super::{CONTAINER_ID_FIELD, DOCKER_BACKEND_ID, DockerStateFields};

/// Where writes and workspace copies are staged inside the container.
const ARCHIVE_STAGING_DIR: &str = "/tmp/sandbox-docker-archive";

/// How long stop and shutdown wait for staged copies to be cleaned up before giving up on them.
const DEFERRED_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP statuses that mark a daemon failure as worth another try.
const TRANSIENT_HTTP_STATUS_CODES: [u16; 4] = [500, 502, 503, 504];

/// How many times a transient archive failure is tried in all.
const TRANSIENT_RETRY_MAX_ATTEMPT: u32 = 3;

/// The delay before the first retry, doubled for each one after.
const TRANSIENT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Which archive error a staging command failure is reported as.
#[derive(Debug, Clone, Copy)]
enum ArchiveDirection {
    Read,
    Write,
}

impl ArchiveDirection {
    fn error(self, path: &str) -> SandboxError {
        match self {
            Self::Read => SandboxError::workspace_archive_read(path),
            Self::Write => SandboxError::workspace_archive_write(path),
        }
    }
}

/// Cleanups started after a workspace archive was read, which stop and shutdown wait for.
///
/// The reference removes a staged copy when the archive stream it produced is closed, which happens
/// after `persist_workspace` has returned, so the removal is scheduled rather than awaited and the
/// session keeps track of it. Here the archive is read in full before returning; the removal is
/// still deferred so persisting does not wait on it, and still drained before the container stops.
#[derive(Default)]
struct DeferredCleanup {
    tasks: AsyncMutex<JoinSet<()>>,
}

impl DeferredCleanup {
    /// Waits for every scheduled cleanup, up to `timeout` in all, and abandons the rest.
    async fn drain(&self, timeout: Duration) {
        let mut tasks = self.tasks.lock().await;
        let deadline = tokio::time::Instant::now() + timeout;
        while !tasks.is_empty() {
            if tokio::time::timeout_at(deadline, tasks.join_next())
                .await
                .is_err()
            {
                break;
            }
        }
        tasks.abort_all();
    }

    /// How many cleanups are still tracked.
    async fn pending(&self) -> usize {
        self.tasks.lock().await.len()
    }
}

/// A container workspace, and the commands run against it.
///
/// Clones share everything — the state, the flags, the cleanup set — so a clone is the same session,
/// which is what lets manifest application hold one.
#[derive(Clone)]
pub struct DockerSandboxSession {
    api: Arc<dyn DockerApi>,
    /// The durable half, which several hooks update as the lifecycle runs.
    state: Arc<Mutex<SandboxSessionState>>,
    /// Whether commands may run in the workspace root. Starts as the state says, and set once start
    /// has created the root or a probe has found it.
    workspace_root_ready: Arc<AtomicBool>,
    /// Whether the first command after a resume should first look for the workspace root.
    resume_workspace_probe_pending: Arc<AtomicBool>,
    /// Whether this start reconnected to a container that was already there.
    start_state_preserved: Arc<AtomicBool>,
    /// The container status last read, answered when a fresh read fails.
    last_status: Arc<Mutex<String>>,
    snapshot_store: Arc<dyn SnapshotStore>,
    mount_lifecycle: Arc<dyn MountLifecycle>,
    resources: Arc<SessionResources>,
    cleanup: Arc<DeferredCleanup>,
    cleanup_timeout: Duration,
    /// The interactive processes this session started.
    pty: Arc<pty::DockerPtyProcesses>,
}

impl std::fmt::Debug for DockerSandboxSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerSandboxSession")
            .field("container_id", &self.container_id())
            .field(
                "workspace_root_ready",
                &self.workspace_root_ready.load(Ordering::SeqCst),
            )
            .finish_non_exhaustive()
    }
}

impl DockerSandboxSession {
    /// Opens a session over the container a state names.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the state's Docker fields do not read.
    pub fn new(api: Arc<dyn DockerApi>, state: SandboxSessionState) -> SandboxResult<Self> {
        let state = DockerStateFields::read(&state)?.apply(state);
        let ready = state.workspace_root_ready();
        Ok(Self {
            api,
            state: Arc::new(Mutex::new(state)),
            workspace_root_ready: Arc::new(AtomicBool::new(ready)),
            resume_workspace_probe_pending: Arc::new(AtomicBool::new(false)),
            start_state_preserved: Arc::new(AtomicBool::new(false)),
            last_status: Arc::new(Mutex::new("created".to_owned())),
            snapshot_store: Arc::new(BuiltinSnapshotStore),
            mount_lifecycle: Arc::new(BuiltinMountLifecycle),
            resources: Arc::new(SessionResources::new()),
            cleanup: Arc::new(DeferredCleanup::default()),
            cleanup_timeout: DEFERRED_CLEANUP_TIMEOUT,
            pty: Arc::new(pty::DockerPtyProcesses::default()),
        })
    }

    /// Reads and writes this session's snapshot through `store` instead of the built-in one.
    #[must_use]
    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = store;
        self
    }

    /// Attaches the manifest's mounts with `lifecycle` instead of the built-in one.
    #[must_use]
    pub fn with_mount_lifecycle(mut self, lifecycle: Arc<dyn MountLifecycle>) -> Self {
        self.mount_lifecycle = lifecycle;
        self
    }

    /// Bounds how long stop and shutdown wait for staged copies to be removed.
    #[must_use]
    pub const fn with_deferred_cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.cleanup_timeout = timeout;
        self
    }

    /// Paces manifest application with these limits instead of the defaults.
    #[must_use]
    pub fn with_concurrency_limits(
        self,
        limits: ra_core::sandbox::SandboxConcurrencyLimits,
    ) -> Self {
        self.resources.set_concurrency_limits(limits);
        self
    }

    /// Records the container status the client last saw, for when a fresh read fails.
    #[must_use]
    pub(crate) fn with_container_status(self, status: impl Into<String>) -> Self {
        *self
            .last_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status.into();
        self
    }

    /// Marks the session as resumed: the first command looks for the workspace root, and whether
    /// the container was reconnected says whether its workspace and accounts survived.
    pub(crate) fn mark_resumed(&self, reused_existing_container: bool) {
        self.resume_workspace_probe_pending
            .store(true, Ordering::SeqCst);
        self.start_state_preserved
            .store(reused_existing_container, Ordering::SeqCst);
    }

    /// The state as it stands, copied out from under the lock.
    fn state_now(&self) -> SandboxSessionState {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the workspace is supposed to contain.
    fn manifest(&self) -> Manifest {
        self.state_now().manifest().clone()
    }

    /// The container this session runs in.
    #[must_use]
    pub fn container_id(&self) -> String {
        self.state_now()
            .field(CONTAINER_ID_FIELD)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    /// Whether commands run in the workspace root yet.
    #[must_use]
    pub fn workspace_root_ready_now(&self) -> bool {
        self.workspace_root_ready.load(Ordering::SeqCst)
    }

    /// Whether the container still exists.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure, other than "no such container", as a transport error.
    pub async fn exists(&self) -> SandboxResult<bool> {
        match self.api.inspect_container(&self.container_id()).await {
            Ok(_) => Ok(true),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(docker_failure(OpName::Start, error)),
        }
    }

    /// How many staged-copy cleanups are still tracked.
    pub async fn pending_cleanup_count(&self) -> usize {
        self.cleanup.pending().await
    }

    /// The workspace root as a sandbox path.
    fn workspace_root(&self) -> SandboxResult<PosixPath> {
        Ok(self.workspace_path_policy()?.sandbox_root().clone())
    }

    /// Runs an argument vector in the container, bounded by `timeout_s`.
    ///
    /// On a timeout the command is left running in the container, as the reference leaves it, so a
    /// best-effort `pkill` for its command line follows when `kill_on_timeout` asks for one.
    async fn exec_run(
        &self,
        cmd: Vec<String>,
        workdir: Option<String>,
        user: Option<String>,
        timeout_s: Option<f64>,
        command_for_errors: Vec<String>,
        kill_on_timeout: bool,
    ) -> SandboxResult<ExecResult> {
        let container_id = self.container_id();
        let request = ExecRunRequest::new(cmd)
            .in_dir(workdir.clone())
            .as_user(user.clone());
        let run = self.api.exec_run(&container_id, &request);
        let outcome = match timeout_s {
            Some(timeout_s) => {
                let limit = Duration::try_from_secs_f64(timeout_s).unwrap_or(Duration::ZERO);
                let Ok(outcome) = tokio::time::timeout(limit, run).await else {
                    if kill_on_timeout {
                        let pattern = command_for_errors.join(" ").replace('\'', "'\\''");
                        let _ = self
                            .api
                            .exec_run(
                                &container_id,
                                &ExecRunRequest::new(vec![
                                    "sh".to_owned(),
                                    "-lc".to_owned(),
                                    format!("pkill -f -- '{pattern}' >/dev/null 2>&1 || true"),
                                ])
                                .as_user(user),
                            )
                            .await;
                    }
                    return Err(SandboxError::exec_timeout(
                        command_for_errors,
                        Some(timeout_s),
                    ));
                };
                outcome
            }
            None => run.await,
        };
        let output = outcome.map_err(|error| {
            SandboxError::exec_transport(command_for_errors.clone(), None).with_cause(error)
        })?;
        let (stdout, stderr, exit_code) = output.into_parts();
        let Some(exit_code) = exit_code else {
            return Err(SandboxError::exec_transport(command_for_errors, None)
                .with_context("reason", "missing_exit_code")
                .with_context("stdout", String::from_utf8_lossy(&stdout).into_owned())
                .with_context("stderr", String::from_utf8_lossy(&stderr).into_owned())
                .with_context("workdir", workdir)
                .with_context("retry_safe", true));
        };
        Ok(ExecResult::new(
            stdout,
            stderr,
            i32::try_from(exit_code).unwrap_or(i32::MAX),
        ))
    }

    /// Looks for the workspace root once after a resume, before the first command needs it.
    ///
    /// A resumed session does not know whether the container it reconnected to still has its
    /// workspace, and running a command there before knowing would fail on a missing directory. A
    /// probe that cannot run answers nothing and is not retried.
    async fn recover_workspace_root_ready(&self, timeout_s: Option<f64>) {
        if self.workspace_root_ready.load(Ordering::SeqCst)
            || !self.resume_workspace_probe_pending.load(Ordering::SeqCst)
        {
            return;
        }
        let root = self.manifest().root;
        let probe = vec!["test".to_owned(), "-d".to_owned(), root];
        let result = self
            .exec_run(probe.clone(), None, None, timeout_s, probe, false)
            .await;
        self.resume_workspace_probe_pending
            .store(false, Ordering::SeqCst);
        if matches!(result, Ok(result) if result.ok()) {
            self.mark_workspace_root_ready();
        }
    }

    /// Records the workspace root as present, in the session and in its state.
    fn mark_workspace_root_ready(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = state.clone().with_workspace_root_ready(true);
        self.workspace_root_ready.store(true, Ordering::SeqCst);
    }

    /// Runs a shaped command as `user`, in the workspace root once it exists.
    async fn exec_internal_for_user(
        &self,
        command: Vec<String>,
        timeout_s: Option<f64>,
        user: Option<String>,
    ) -> SandboxResult<ExecResult> {
        self.recover_workspace_root_ready(timeout_s).await;
        // The root is created while the manifest is applied, so the first commands of a start must
        // not ask Docker to change into it yet.
        let workdir = self
            .workspace_root_ready
            .load(Ordering::SeqCst)
            .then(|| self.manifest().root);
        self.exec_run(command.clone(), workdir, user, timeout_s, command, true)
            .await
    }

    /// Runs a command as written and turns a failure into the archive error for `error_path`.
    async fn exec_checked(
        &self,
        command: Vec<String>,
        direction: ArchiveDirection,
        error_path: &str,
    ) -> SandboxResult<ExecResult> {
        let result = self.exec(no_shell(command.clone())).await?;
        if !result.ok() {
            return Err(direction
                .error(error_path)
                .with_context("command", command)
                .with_context(
                    "stdout",
                    String::from_utf8_lossy(&result.stdout).into_owned(),
                )
                .with_context(
                    "stderr",
                    String::from_utf8_lossy(&result.stderr).into_owned(),
                ));
        }
        Ok(result)
    }

    /// Removes a path inside the container, and says nothing if it cannot.
    async fn rm_best_effort(&self, path: &str) {
        let _ = self
            .exec(no_shell(vec![
                "rm".to_owned(),
                "-rf".to_owned(),
                "--".to_owned(),
                path.to_owned(),
            ]))
            .await;
    }

    /// Schedules a staged path's removal without waiting for it.
    async fn schedule_rm_best_effort(&self, path: String) {
        let session = self.clone();
        self.cleanup.tasks.lock().await.spawn(async move {
            session.rm_best_effort(&path).await;
        });
    }

    /// A fresh staging path, unique so concurrent reads and writes do not collide.
    fn archive_stage_path(name_hint: &str) -> String {
        format!(
            "{ARCHIVE_STAGING_DIR}/{}_{name_hint}",
            Uuid::new_v4().simple()
        )
    }

    /// Copies the workspace into a staging directory, leaving out `skip`.
    ///
    /// Returns the staging parent, which is what gets removed afterwards, and the copy inside it,
    /// which carries the workspace root's own name. A mount at the workspace root leaves nothing
    /// that can be copied without reaching into the mount, so the copy is then an empty root.
    async fn stage_workspace_copy(
        &self,
        skip: &BTreeSet<PosixPath>,
    ) -> SandboxResult<(String, String)> {
        let root = self.workspace_root()?;
        let root_name = root
            .as_str()
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("workspace")
            .to_owned();
        let staging_parent = Self::archive_stage_path("workspace");
        let staging_workspace = format!("{staging_parent}/{root_name}");
        let skip_workspace_root = self
            .manifest()
            .ephemeral_mount_targets()?
            .iter()
            .any(|(_, target)| PosixPath::coerce(target.as_str()) == root);

        let mkdir = |path: &str| vec!["mkdir".to_owned(), "-p".to_owned(), path.to_owned()];
        self.exec_checked(
            mkdir(&staging_parent),
            ArchiveDirection::Read,
            root.as_str(),
        )
        .await?;
        if skip_workspace_root {
            self.exec_checked(
                mkdir(&staging_workspace),
                ArchiveDirection::Read,
                root.as_str(),
            )
            .await?;
        } else if !skip.is_empty() {
            self.exec_checked(
                mkdir(&staging_workspace),
                ArchiveDirection::Read,
                root.as_str(),
            )
            .await?;
            let skip: BTreeSet<String> = skip.iter().map(|path| path.as_str().to_owned()).collect();
            self.copy_workspace_tree_pruned(
                root.as_str().to_owned(),
                staging_workspace.clone(),
                String::new(),
                &skip,
            )
            .await?;
        } else {
            self.exec_checked(
                vec![
                    "cp".to_owned(),
                    "-R".to_owned(),
                    "--".to_owned(),
                    root.as_str().to_owned(),
                    staging_workspace.clone(),
                ],
                ArchiveDirection::Read,
                root.as_str(),
            )
            .await?;
        }
        Ok((staging_parent, staging_workspace))
    }

    /// Copies one directory into the staging copy, descending only where something below is left
    /// out, so everything else is copied whole.
    fn copy_workspace_tree_pruned<'a>(
        &'a self,
        source: String,
        destination: String,
        relative: String,
        skip: &'a BTreeSet<String>,
    ) -> futures::future::BoxFuture<'a, SandboxResult<()>> {
        Box::pin(async move {
            for entry in self.ls(&source, None).await? {
                let name = entry.path.rsplit('/').next().unwrap_or_default().to_owned();
                let child_relative = if relative.is_empty() {
                    name.clone()
                } else {
                    format!("{relative}/{name}")
                };
                if skip.contains(&child_relative) {
                    continue;
                }
                let child_destination = format!("{destination}/{name}");
                let nested_skip = skip
                    .iter()
                    .any(|path| path.starts_with(&format!("{child_relative}/")));
                if entry.is_dir() && nested_skip {
                    self.exec_checked(
                        vec![
                            "mkdir".to_owned(),
                            "-p".to_owned(),
                            child_destination.clone(),
                        ],
                        ArchiveDirection::Read,
                        &entry.path,
                    )
                    .await?;
                    self.copy_workspace_tree_pruned(
                        entry.path.clone(),
                        child_destination,
                        child_relative,
                        skip,
                    )
                    .await?;
                    continue;
                }
                self.exec_checked(
                    vec![
                        "cp".to_owned(),
                        "-R".to_owned(),
                        "--".to_owned(),
                        entry.path.clone(),
                        child_destination,
                    ],
                    ArchiveDirection::Read,
                    &entry.path,
                )
                .await?;
            }
            Ok(())
        })
    }

    /// One attempt at reading the workspace out as a portable archive.
    async fn persist_workspace_once(&self) -> SandboxResult<Vec<u8>> {
        let root = self.workspace_root()?;
        let skip = self.persist_workspace_skip_relpaths()?;
        let (staging_parent, staging_workspace) = self.stage_workspace_copy(&skip).await?;
        let archive = self
            .api
            .get_archive(&self.container_id(), &staging_workspace)
            .await;
        // Removed once the archive has been read, as the reference removes it when the stream is
        // closed; deferred so persisting does not wait on it.
        self.schedule_rm_best_effort(staging_parent).await;
        let archive = archive.map_err(|error| archive_read_failure(root.as_str(), error))?;
        let root_name = staging_workspace.rsplit('/').next().unwrap_or("workspace");
        strip_tar_member_prefix(&archive, root_name).map_err(|error| {
            let failure = SandboxError::workspace_archive_read(root.as_str());
            match error {
                StripPrefixError::Unsafe(member) => failure
                    .with_context("reason", member.reason().to_owned())
                    .with_context("member", member.member().to_owned()),
                other => failure.with_cause(other),
            }
        })
    }

    /// Replaces a failure that may carry mount authority, when this session's manifest has some.
    ///
    /// The reference's `@redact_mount_error_data` on the persist and hydrate methods.
    fn redact_if_protected(&self, error: SandboxError) -> SandboxError {
        if error.is_data_redacted() || manifest_has_configured_mount_authority(&self.manifest()) {
            replace_protected_mount_error(&error)
        } else {
            error
        }
    }

    /// The applier that puts this session's manifest in its workspace.
    #[cfg(unix)]
    fn applier(&self) -> SandboxResult<crate::materialize::ManifestApplier> {
        Ok(crate::materialize::ManifestApplier::new(
            Arc::new(self.clone()),
            crate::materialize::manifest_base_dir()?,
        )
        .with_limits(self.resources.concurrency_limits())
        .with_mount_lifecycle(Arc::clone(&self.mount_lifecycle)))
    }

    /// The snapshot half of this session's lifecycle.
    fn snapshots(&self) -> SnapshotLifecycle<'_> {
        SnapshotLifecycle::new(self, self.snapshot_store.as_ref())
    }
}

/// A request for an argument vector run as written.
fn no_shell(command: Vec<String>) -> ExecRequest {
    ExecRequest::new(command).with_shell(ShellInvocation::None)
}

/// A daemon failure where no more specific error applies.
fn docker_failure(op: OpName, error: DockerApiError) -> SandboxError {
    SandboxError::new(
        ErrorCode::ExecTransportError,
        op,
        "docker daemon request failed",
    )
    .with_context("backend", DOCKER_BACKEND_ID)
    .with_cause(error)
}

/// Whether a daemon status is one worth trying again.
fn is_transient_status(status: Option<u16>) -> bool {
    status.is_some_and(|status| TRANSIENT_HTTP_STATUS_CODES.contains(&status))
}

/// An archive download failure, classified as the reference classifies it.
///
/// Not-found is final: the container is gone. An error status the daemon may recover from is
/// marked retryable, and any other is left unclassified.
fn archive_read_failure(root: &str, error: DockerApiError) -> SandboxError {
    let retryable = match error.kind() {
        DockerApiErrorKind::NotFound => Some(false),
        _ if is_transient_status(error.status_code()) => Some(true),
        _ => None,
    };
    SandboxError::workspace_archive_read(root)
        .with_retryable(retryable)
        .with_cause(error)
}

/// Whether anything in an error's chain is a daemon answer with a transient status.
///
/// The reference's `exception_chain_has_status_code`, over this crate's error chain.
fn chain_has_transient_status(error: &SandboxError) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(candidate) = current {
        if let Some(docker) = candidate.downcast_ref::<DockerApiError>()
            && is_transient_status(docker.status_code())
        {
            return true;
        }
        current = candidate.source();
    }
    false
}

#[async_trait]
impl SandboxSession for DockerSandboxSession {
    fn backend_id(&self) -> &str {
        DOCKER_BACKEND_ID
    }

    fn state(&self) -> SandboxSessionState {
        self.state_now()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    /// Docker attaches volume-driver mounts when it creates the container.
    fn supports_volume_mounts(&self) -> bool {
        true
    }

    /// Commands can be started on a terminal through an attached exec.
    fn supports_pty(&self) -> bool {
        true
    }

    /// Runs a command, switching accounts through Docker rather than `sudo`.
    ///
    /// The account is handed to the daemon, which starts the command as that account directly; the
    /// image does not need `sudo` for a caller to name one.
    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        let user = request.user.as_ref().map(|user| user.name.clone());
        let command = remote::prepare_exec_command(&ExecRequest {
            user: None,
            ..request.clone()
        });
        self.exec_internal_for_user(command, request.timeout_s, user)
            .await
    }

    /// Starts a command with its streams attached, as the account named, through the daemon.
    ///
    /// The request's timeout bounds setting the command up and starting it, not the process, which
    /// runs until it exits or is ended.
    async fn pty_start(&self, request: PtyStartRequest) -> SandboxResult<PtyExecUpdate> {
        self.pty_exec_start(request).await
    }

    async fn pty_write(&self, request: PtyWriteRequest) -> SandboxResult<PtyExecUpdate> {
        self.pty_write_stdin(request).await
    }

    /// Kills every interactive process through its pid file and closes its attachment.
    async fn pty_terminate_all(&self) -> SandboxResult<()> {
        self.pty_terminate_all_processes().await;
        Ok(())
    }

    /// Whether the container is running, read fresh from the daemon.
    ///
    /// A read the daemon answers with an error falls back to the last status seen, as the reference
    /// falls back to its cached one; a daemon that cannot be reached at all is reported.
    async fn running(&self) -> SandboxResult<bool> {
        match self.api.inspect_container(&self.container_id()).await {
            Ok(attrs) => {
                if let Some(status) = attrs
                    .get("State")
                    .and_then(|state| state.get("Status").or(Some(state)))
                    .and_then(Value::as_str)
                {
                    status.clone_into(
                        &mut self
                            .last_status
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner),
                    );
                }
            }
            Err(error) if error.kind() == DockerApiErrorKind::Transport => {
                return Err(docker_failure(OpName::Start, error));
            }
            Err(_) => {}
        }
        Ok(*self
            .last_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            == "running")
    }

    async fn validate_path_access(&self, path: &str, for_write: bool) -> SandboxResult<String> {
        remote::validate_remote_path_access(self, path, for_write).await
    }

    async fn ls(&self, path: &str, user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        remote::ls(self, path, user).await
    }

    async fn rm(&self, path: &str, recursive: bool, user: AsUser) -> SandboxResult<()> {
        remote::rm(self, path, recursive, user).await
    }

    async fn mkdir(&self, path: &str, parents: bool, user: AsUser) -> SandboxResult<()> {
        remote::mkdir(self, path, parents, user).await
    }

    /// Reads a file with `cat`, inside the container, as the account named.
    async fn read(&self, path: &str, user: AsUser) -> SandboxResult<Vec<u8>> {
        let workspace_path = self.validate_path_access(path, false).await?;
        let command = vec!["cat".to_owned(), "--".to_owned(), workspace_path.clone()];
        let mut request = no_shell(command.clone());
        if let Some(user) = user.clone() {
            request = request.as_user(user);
        }
        let result = self.exec(request).await?;
        if !result.ok() {
            return Err(remote::read_error_from_exec(
                self,
                path,
                &workspace_path,
                command,
                &result,
                user,
            )
            .await);
        }
        Ok(result.stdout)
    }

    /// Writes a file by streaming it into the container.
    ///
    /// As another account, the account writes it itself, creating the parent directories. Otherwise
    /// the bytes go to a staging file first and are copied into place by a process inside the
    /// container, which sees the mounts the daemon's archive upload would not.
    async fn write(&self, path: &str, data: Vec<u8>, user: AsUser) -> SandboxResult<()> {
        let path = self.validate_path_access(path, true).await?;
        let container_id = self.container_id();

        if let Some(user) = user {
            return stream_into_exec(
                self.api.as_ref(),
                &container_id,
                &[
                    "sh".to_owned(),
                    "-lc".to_owned(),
                    r#"mkdir -p "$(dirname "$1")" && cat > "$1""#.to_owned(),
                    "sh".to_owned(),
                    path.clone(),
                ],
                &data,
                &path,
                Some(&user.name),
            )
            .await;
        }

        let parent = PosixPath::coerce(&path)
            .as_str()
            .rsplit_once('/')
            .map_or_else(
                || "/".to_owned(),
                |(parent, _)| {
                    if parent.is_empty() {
                        "/".to_owned()
                    } else {
                        parent.to_owned()
                    }
                },
            );
        self.mkdir(&parent, true, None).await?;

        let name = path.rsplit('/').next().unwrap_or("file");
        let staging_path = Self::archive_stage_path(name);
        self.exec_checked(
            vec![
                "mkdir".to_owned(),
                "-p".to_owned(),
                ARCHIVE_STAGING_DIR.to_owned(),
            ],
            ArchiveDirection::Write,
            ARCHIVE_STAGING_DIR,
        )
        .await?;
        stream_into_exec(
            self.api.as_ref(),
            &container_id,
            &[
                "sh".to_owned(),
                "-lc".to_owned(),
                r#"cat > "$1""#.to_owned(),
                "sh".to_owned(),
                staging_path.clone(),
            ],
            &data,
            &staging_path,
            None,
        )
        .await?;

        let copy = vec![
            "cp".to_owned(),
            "--".to_owned(),
            staging_path.clone(),
            path.clone(),
        ];
        let copied = self.exec(no_shell(copy.clone())).await?;
        if !copied.ok() {
            return Err(SandboxError::workspace_archive_write(&parent)
                .with_context("command", copy)
                .with_context(
                    "stdout",
                    String::from_utf8_lossy(&copied.stdout).into_owned(),
                )
                .with_context(
                    "stderr",
                    String::from_utf8_lossy(&copied.stderr).into_owned(),
                ));
        }
        self.rm_best_effort(&staging_path).await;
        Ok(())
    }

    /// Reads the workspace out as a portable archive, retrying a transient daemon failure.
    ///
    /// The workspace is copied to a staging directory first — with the persistence exclusions and
    /// every mount pruned from the copy — and the copy is downloaded, so the daemon never archives a
    /// path a volume driver is attached under. Members are renamed from the copy's root to `.`.
    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        let mut attempt = 1;
        loop {
            match self.persist_workspace_once().await {
                Ok(archive) => return Ok(archive),
                Err(error)
                    if attempt < TRANSIENT_RETRY_MAX_ATTEMPT
                        && chain_has_transient_status(&error) =>
                {
                    tokio::time::sleep(TRANSIENT_RETRY_INTERVAL * 2_u32.pow(attempt - 1)).await;
                    attempt += 1;
                }
                Err(error) => return Err(self.redact_if_protected(error)),
            }
        }
    }

    /// Replaces the workspace with an archive, checked in full before anything is written.
    ///
    /// The archive is validated here — no absolute or climbing member, no symlink pointing outside
    /// it, no symlink at the root — and then streamed to `tar -x` inside the container, so the
    /// extraction itself happens where the files live.
    async fn hydrate_workspace(&self, data: Vec<u8>) -> SandboxResult<()> {
        let outcome = async {
            let root = self.workspace_root()?;
            validate_tar_bytes(
                &data,
                &TarValidation::new().with_external_symlink_targets(false),
            )
            .map_err(|member| {
                let refused = SandboxError::workspace_archive_write(root.as_str());
                if member.member() == "<tar>" {
                    // A stream that is not an archive at all, which the reference reports with the
                    // parser's error as the cause and no member.
                    refused.with_cause(member)
                } else {
                    refused
                        .with_context("reason", member.reason().to_owned())
                        .with_context("member", member.member().to_owned())
                }
            })?;
            self.exec_checked(
                vec![
                    "mkdir".to_owned(),
                    "-p".to_owned(),
                    root.as_str().to_owned(),
                ],
                ArchiveDirection::Write,
                root.as_str(),
            )
            .await?;
            stream_into_exec(
                self.api.as_ref(),
                &self.container_id(),
                &[
                    "tar".to_owned(),
                    "-x".to_owned(),
                    "-C".to_owned(),
                    root.as_str().to_owned(),
                ],
                &data,
                root.as_str(),
                None,
            )
            .await
        }
        .await;
        outcome.map_err(|error| self.redact_if_protected(error))
    }

    /// Writes an archive into the workspace and unpacks it beside itself, through this session's
    /// own `mkdir` and `write`.
    async fn extract(
        &self,
        path: &str,
        data: Vec<u8>,
        scheme: Option<CompressionScheme>,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<()> {
        WorkspaceArchiveExtractor::new(self)
            .extract(path, data, scheme, limits.or_else(|| self.archive_limits()))
            .await
    }

    /// Resolves a published port to where the daemon bound it on the host.
    ///
    /// Read from the container's current port map. A missing binding, a binding that is not an
    /// object, or a host port that is not a number is `backend_unavailable` with the detail named;
    /// a binding without a host address is taken to be on loopback.
    async fn resolve_exposed_port(&self, port: u16) -> SandboxResult<ExposedPortEndpoint> {
        let state = self.state_now();
        let ports = state.exposed_ports();
        if !ports.contains(&port) {
            return Err(SandboxError::exposed_port_unavailable(
                port,
                ports,
                "not_configured",
            ));
        }
        let unavailable = |detail: &str| {
            SandboxError::exposed_port_unavailable(port, ports, "backend_unavailable")
                .with_context("backend", DOCKER_BACKEND_ID)
                .with_context("detail", detail.to_owned())
        };
        let attrs = self
            .api
            .inspect_container(&self.container_id())
            .await
            .map_err(|error| unavailable("container_reload_failed").with_cause(error))?;
        let port_key = docker_port_key(port);
        let bindings = attrs
            .get("NetworkSettings")
            .and_then(|settings| settings.get("Ports"))
            .and_then(|ports| ports.get(&port_key))
            .and_then(Value::as_array)
            .filter(|bindings| !bindings.is_empty())
            .ok_or_else(|| {
                unavailable("port_not_published").with_context("port_key", port_key.clone())
            })?;
        let binding = bindings[0].as_object().ok_or_else(|| {
            unavailable("invalid_port_binding").with_context("port_key", port_key.clone())
        })?;
        let host_ip = binding
            .get("HostIp")
            .and_then(Value::as_str)
            .filter(|ip| !ip.is_empty())
            .unwrap_or("127.0.0.1");
        let host_port = binding
            .get("HostPort")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<u16>().ok())
            .ok_or_else(|| {
                unavailable("invalid_host_port").with_context("port_key", port_key.clone())
            })?;
        Ok(ExposedPortEndpoint::new(host_ip, host_port).with_tls(false))
    }

    /// Starts the container if it is not running.
    async fn ensure_backend_started(&self) -> SandboxResult<()> {
        let container_id = self.container_id();
        self.api
            .inspect_container(&container_id)
            .await
            .map_err(|error| docker_failure(OpName::Start, error))?;
        if !self.running().await? {
            self.api
                .start_container(&container_id)
                .await
                .map_err(|error| docker_failure(OpName::Start, error))?;
        }
        Ok(())
    }

    /// Installs the path resolver every file operation runs through.
    async fn ensure_runtime_helpers(&self) -> SandboxResult<()> {
        ensure_installed(self, &resolve_workspace_path_helper()).await
    }

    fn workspace_state_preserved_on_start(&self) -> bool {
        self.start_state_preserved.load(Ordering::SeqCst)
    }

    #[cfg(unix)]
    async fn provision_accounts(&self) -> SandboxResult<()> {
        self.applier()?.provision_accounts(&self.manifest()).await
    }

    #[cfg(unix)]
    async fn apply_manifest(
        &self,
        provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        let manifest = self.manifest();
        ra_core::sandbox::validate_manifest_mount_credential_boundaries(
            &manifest,
            Some(DOCKER_BACKEND_ID),
        )?;
        self.applier()?
            .apply_manifest(&manifest, provision_accounts)
            .await
    }

    #[cfg(not(unix))]
    async fn apply_manifest(
        &self,
        _provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::Materialize,
            "manifest materialization is not available on this platform yet",
        )
        .with_context("backend", DOCKER_BACKEND_ID))
    }

    fn replace_manifest(&self, manifest: Manifest) -> SandboxResult<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = state.clone().with_manifest(manifest);
        Ok(())
    }

    #[cfg(unix)]
    async fn apply_manifest_entries(
        &self,
        entries: Vec<(PosixPath, Entry)>,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        self.applier()?.apply_entry_list(&entries).await
    }

    #[cfg(unix)]
    async fn reapply_ephemeral_manifest(&self) -> SandboxResult<()> {
        self.applier()?
            .apply_ephemeral(&self.manifest())
            .await
            .map(|_| ())
    }

    async fn snapshot_restorable(&self) -> SandboxResult<bool> {
        self.snapshots().restorable().await
    }

    async fn restore_snapshot(&self) -> SandboxResult<()> {
        self.snapshots().restore_on_resume().await
    }

    async fn can_skip_snapshot_restore(&self, is_running: bool) -> SandboxResult<bool> {
        self.snapshots().can_skip_restore(is_running).await
    }

    async fn persist_snapshot(&self) -> SandboxResult<()> {
        self.snapshots().persist().await
    }

    async fn record_snapshot_fingerprint(
        &self,
        fingerprint: Option<SnapshotFingerprint>,
    ) -> SandboxResult<()> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let updated = match fingerprint {
            Some(fingerprint) => guard
                .clone()
                .with_snapshot_fingerprint(fingerprint.fingerprint(), fingerprint.version()),
            None => guard.clone().without_snapshot_fingerprint(),
        };
        *guard = updated;
        Ok(())
    }

    /// The workspace root exists once start has finished, and no probe is needed any more.
    async fn after_start(&self) -> SandboxResult<()> {
        self.workspace_root_ready.store(true, Ordering::SeqCst);
        self.resume_workspace_probe_pending
            .store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn record_workspace_root_ready(&self) -> SandboxResult<()> {
        self.mark_workspace_root_ready();
        Ok(())
    }

    /// Waits, boundedly, for staged copies to be removed.
    async fn after_stop(&self) {
        self.cleanup.drain(self.cleanup_timeout).await;
    }

    /// Ends interactive processes, then waits, boundedly, for staged copies to be removed — before
    /// the container is stopped underneath them.
    async fn before_shutdown(&self) -> SandboxResult<()> {
        self.pty_terminate_all().await?;
        self.cleanup.drain(self.cleanup_timeout).await;
        Ok(())
    }

    /// Stops the container if it is running, best effort.
    ///
    /// A container that is already gone or stopped is not an error: shutdown's job is done.
    async fn shutdown_backend(&self) -> SandboxResult<()> {
        if matches!(self.running().await, Ok(true)) {
            let _ = self.api.stop_container(&self.container_id()).await;
        }
        Ok(())
    }
}
