//! A workspace that is a directory on this host.
//!
//! The session owns a directory, runs commands against it, and answers the protocol's lifecycle
//! hooks. What it does *not* do is tear the directory down: that belongs to the client that created
//! it, so a caller who handed in their own directory keeps it after the session is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, CompressionScheme, EnvValueResolver, ErrorCode, ExecRequest, ExecResult,
    ExposedPortEndpoint, FileEntry, Manifest, MaterializationResult, OpName, SandboxArchiveLimits,
    SandboxConcurrencyLimits, SandboxError, SandboxResult, SandboxSession, SandboxSessionState,
    SessionResources, SnapshotFingerprint, User,
};

use crate::archive::WorkspaceArchiveExtractor;
use crate::host_paths::HostWorkspacePaths;
use crate::listing::parse_ls_la;
use crate::materialize::{ManifestApplier, manifest_base_dir};
use crate::snapshot::lifecycle::SnapshotLifecycle;
use crate::snapshot::{BuiltinSnapshotStore, SnapshotStore};

use super::{UNIX_LOCAL_BACKEND_ID, archive, exec, files};

/// A local workspace, and the commands run against it.
///
/// Clones share lifecycle state so an owned materialization cleanup can retain the same session.
#[derive(Clone)]
pub struct UnixLocalSandboxSession {
    /// The durable half, which several hooks update as the lifecycle runs.
    state: Arc<Mutex<SandboxSessionState>>,
    /// Whether start has finished. Set only after the workspace is populated, because a session
    /// that reports itself running before then invites a resume to trust a workspace that is still
    /// being built.
    running: Arc<AtomicBool>,
    /// Which host variables reach a command, or all of them when there is no list.
    ///
    /// **Held by the session, never by the state.** A resumed session takes this from the client
    /// that resumed it, so a state travelling between hosts cannot re-open an environment the
    /// receiving host chose to close.
    host_environment_allowlist: Option<BTreeSet<String>>,
    /// How a manifest's non-literal environment values are fetched.
    env_values: Arc<dyn EnvValueResolver>,
    /// How much of a manifest application may be in flight at once.
    concurrency_limits: SandboxConcurrencyLimits,
    /// Where this session's snapshot is read from and written to.
    ///
    /// Held by the session for the same reason the environment policy is: which storage a host
    /// wired up is the host's decision, and a state that travelled from elsewhere must not be able
    /// to point this session at something else.
    snapshot_store: Arc<dyn SnapshotStore>,
    /// The dependency container, pre-stop callbacks and close lock every session holds. Shared by
    /// clones, as the state is: a clone is the same session.
    resources: Arc<SessionResources>,
}

impl std::fmt::Debug for UnixLocalSandboxSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixLocalSandboxSession")
            .field("root", &self.workspace_root())
            .field("running", &self.running.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl UnixLocalSandboxSession {
    /// Opens a session over the workspace a state describes.
    #[must_use]
    pub fn new(
        state: SandboxSessionState,
        host_environment_allowlist: Option<BTreeSet<String>>,
        env_values: Arc<dyn EnvValueResolver>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            running: Arc::new(AtomicBool::new(false)),
            host_environment_allowlist,
            env_values,
            concurrency_limits: SandboxConcurrencyLimits::default(),
            snapshot_store: Arc::new(BuiltinSnapshotStore),
            resources: Arc::new(SessionResources::new()),
        }
    }

    /// Reads and writes this session's snapshot through `store` instead of the built-in one.
    #[must_use]
    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = store;
        self
    }

    /// The snapshot half of this session's lifecycle.
    fn snapshots(&self) -> SnapshotLifecycle<'_> {
        SnapshotLifecycle::new(self, self.snapshot_store.as_ref())
    }

    /// Paces manifest application with these limits instead of the defaults.
    ///
    /// **The reference sets these on a session that already exists**, from the run configuration,
    /// which has not been carried over yet. Until it has, the client that makes a session is where
    /// a host says how much work may be in flight.
    #[must_use]
    pub const fn with_concurrency_limits(mut self, limits: SandboxConcurrencyLimits) -> Self {
        self.concurrency_limits = limits;
        self
    }

    /// The applier that puts this session's manifest in its workspace.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when this process has no readable working
    /// directory, which is what a manifest's relative sources are measured from.
    fn applier(&self) -> SandboxResult<ManifestApplier> {
        Ok(
            ManifestApplier::new(Arc::new(self.clone()), manifest_base_dir()?)
                .with_limits(self.concurrency_limits),
        )
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

    /// Where the workspace lives on this host.
    #[must_use]
    pub fn workspace_root(&self) -> PathBuf {
        PathBuf::from(&self.state_now().manifest().root)
    }

    /// The path policy this session decides access with.
    ///
    /// Rebuilt per question rather than cached. The reference caches it against the root and the
    /// grants; here the manifest lives behind a lock that the lifecycle writes through, and a cache
    /// keyed on a value that can change under it would be answering an older manifest's question.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the manifest root is not absolute or
    /// cannot be resolved.
    pub fn paths(&self) -> SandboxResult<HostWorkspacePaths> {
        let manifest = self.manifest();
        HostWorkspacePaths::new(&manifest.root, manifest.extra_path_grants.clone()).map_err(
            |error| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    error.to_string(),
                )
                .with_context("root", manifest.root.clone())
                .with_sandbox_cause(error)
            },
        )
    }

    /// Validates a path against the workspace and its grants, following symlinks.
    ///
    /// **The leaf is followed too**, so an operation on a link lands on what the link names: a
    /// removal deletes the target and leaves the link dangling. The reference's remote form does
    /// the opposite — there the resolved path is used only to check containment and the operation
    /// still lands on the link — and the difference is kept rather than reconciled, because a local
    /// workspace is a real directory and quietly acting on something other than the path that was
    /// resolved is how a check ends up guarding a different file than the one that gets written.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidManifestPath`] for a path that resolves outside everything this
    /// session may reach, and [`ErrorCode::WorkspaceArchiveWriteError`] for a write through a
    /// read-only grant.
    pub fn normalize_path(&self, path: &str, for_write: bool) -> SandboxResult<PathBuf> {
        self.paths()?.normalize_path(path, for_write)
    }

    /// The environment a command runs with, and the directory it runs in.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::WorkspaceRootNotFound`] when the workspace directory is gone, and
    /// whatever resolving a manifest environment value failed with.
    async fn exec_context(&self) -> SandboxResult<(BTreeMap<String, String>, PathBuf)> {
        let manifest = self.manifest();
        let mut env: BTreeMap<String, String> = std::env::vars()
            .filter(|(name, _)| {
                self.host_environment_allowlist
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name))
            })
            .collect();
        // The manifest wins over the host: it is the configuration written for this workspace, and
        // a host variable of the same name is an accident of where the SDK happens to run.
        env.extend(
            manifest
                .environment
                .resolve(self.env_values.as_ref())
                .await?,
        );

        let workspace = PathBuf::from(&manifest.root);
        if !workspace.exists() {
            return Err(SandboxError::new(
                ErrorCode::WorkspaceRootNotFound,
                OpName::Exec,
                format!("workspace root not found: {}", workspace.to_string_lossy()),
            )
            .with_context("path", workspace.to_string_lossy().as_ref()));
        }
        // A local command's home is the workspace, so a tool that writes dotfiles writes them where
        // the session can clean them up rather than into the developer's own home.
        env.insert("HOME".to_owned(), workspace.to_string_lossy().into_owned());
        Ok((env, workspace))
    }

    /// Runs an already-shaped argument vector.
    async fn run_prepared(
        &self,
        command: &[String],
        timeout_s: Option<f64>,
        stdin: Option<Vec<u8>>,
    ) -> SandboxResult<ExecResult> {
        let (env, cwd) = self.exec_context().await?;
        let grants = self.manifest().extra_path_grants;
        exec::run(command, timeout_s, &env, &cwd, &grants, stdin).await
    }

    /// Asks, as another account, whether an operation would be permitted.
    ///
    /// The command is deliberately `sh -lc`: the check runs under the other account's login
    /// environment, which is the environment the operation itself would run under.
    async fn check_as_user(
        &self,
        script: &str,
        arguments: &[String],
        user: &User,
    ) -> SandboxResult<ExecResult> {
        let mut command = vec![
            "sh".to_owned(),
            "-lc".to_owned(),
            script.to_owned(),
            "sh".to_owned(),
        ];
        command.extend_from_slice(arguments);
        let request = ExecRequest::new(command)
            .with_shell(ra_core::sandbox::ShellInvocation::None)
            .as_user(user.clone());
        self.exec(request).await
    }

    /// Refuses an operation the other account would not have been allowed to perform.
    fn refuse_denied_access(
        label: &str,
        path: &Path,
        arguments: &[String],
        result: &ExecResult,
    ) -> SandboxError {
        let mut command = vec!["sh".to_owned(), "-lc".to_owned(), format!("<{label}>")];
        command.extend_from_slice(arguments);
        SandboxError::workspace_archive_write(&path.to_string_lossy())
            .with_context("command", command)
            .with_context(
                "stdout",
                String::from_utf8_lossy(&result.stdout).into_owned(),
            )
            .with_context(
                "stderr",
                String::from_utf8_lossy(&result.stderr).into_owned(),
            )
    }
}

/// Refuses a manifest whose grants name a separate host source.
///
/// A split grant says "this path inside the sandbox comes from that path on the host", which needs
/// something that can map one onto the other. A local session has one filesystem and no way to make
/// two paths be the same place, so the configuration is refused rather than quietly honoured as
/// whichever half happened to be read.
pub(crate) fn assert_host_path_grants_unsupported(manifest: &Manifest) -> SandboxResult<()> {
    let Some(grant) = manifest
        .extra_path_grants
        .iter()
        .find(|grant| grant.host_path().is_some())
    else {
        return Ok(());
    };
    Err(SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Start,
        format!(
            "the `{UNIX_LOCAL_BACKEND_ID}` backend does not support sandbox path grant host_path \
             for `{}`; omit host_path when both paths are the same, or use a container backend",
            grant.path()
        ),
    )
    .with_context("grant_path", grant.path())
    .with_context("backend", UNIX_LOCAL_BACKEND_ID))
}

/// Refuses a manifest that asks for accounts to be created.
///
/// Provisioning a user would create one on the developer's machine. The reference refuses for the
/// same reason, and refusing is the only honest answer: a workspace whose files are supposed to
/// belong to `build` cannot be materialized correctly without that account existing.
pub(crate) fn assert_accounts_unsupported(manifest: &Manifest) -> SandboxResult<()> {
    if manifest.users.is_empty() && manifest.groups.is_empty() {
        return Ok(());
    }
    Err(SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Materialize,
        format!(
            "the `{UNIX_LOCAL_BACKEND_ID}` backend does not support manifest users or groups \
             because provisioning would run on the host machine"
        ),
    )
    .with_context("backend", UNIX_LOCAL_BACKEND_ID))
}

#[async_trait]
impl SandboxSession for UnixLocalSandboxSession {
    fn backend_id(&self) -> &str {
        UNIX_LOCAL_BACKEND_ID
    }

    fn state(&self) -> SandboxSessionState {
        self.state_now()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    /// Whether this session can allocate a terminal.
    ///
    /// **`false`, where the reference says `true`.** The terminal implementation is a separate
    /// task; until it lands, claiming a terminal would have the tool surface offer an interactive
    /// write tool backed by nothing. Answering honestly costs an absent capability; answering
    /// aspirationally costs a tool that fails when a model uses it.
    fn supports_pty(&self) -> bool {
        false
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        let command = exec::prepare_exec_command(&request);
        self.run_prepared(&command, request.timeout_s, None).await
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }

    async fn validate_path_access(&self, path: &str, for_write: bool) -> SandboxResult<String> {
        let normalized = self.normalize_path(path, for_write)?;
        normalized.to_str().map(str::to_owned).ok_or_else(|| {
            SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Write,
                "resolved workspace path is not valid UTF-8",
            )
        })
    }

    async fn ls(&self, path: &str, user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        let normalized = self.normalize_path(path, false)?;
        let Some(user) = user else {
            return files::list_directory(&normalized);
        };

        // As another account the listing has to come from that account's own view, so it is read
        // out of `ls` rather than out of this process's directory handle.
        let rendered = normalized.to_string_lossy().into_owned();
        let command = vec![
            "ls".to_owned(),
            "-la".to_owned(),
            "--".to_owned(),
            rendered.clone(),
        ];
        let request = ExecRequest::new(command.clone())
            .with_shell(ra_core::sandbox::ShellInvocation::None)
            .as_user(user);
        let result = self.exec(request).await?;
        if !result.ok() {
            return Err(SandboxError::exec_nonzero(result, command));
        }
        Ok(parse_ls_la(
            &String::from_utf8_lossy(&result.stdout),
            &rendered,
        ))
    }

    async fn rm(&self, path: &str, recursive: bool, user: AsUser) -> SandboxResult<()> {
        let normalized = self.normalize_path(path, true)?;
        if let Some(user) = user {
            let arguments = vec![
                normalized.to_string_lossy().into_owned(),
                if recursive { "1" } else { "0" }.to_owned(),
            ];
            let result = self
                .check_as_user(files::RM_ACCESS_CHECK_SCRIPT, &arguments, &user)
                .await?;
            if !result.ok() {
                return Err(Self::refuse_denied_access(
                    "rm_access_check",
                    &normalized,
                    &arguments,
                    &result,
                ));
            }
        }
        files::remove(&normalized, recursive)
    }

    async fn mkdir(&self, path: &str, parents: bool, user: AsUser) -> SandboxResult<()> {
        let normalized = self.normalize_path(path, true)?;
        if let Some(user) = user {
            let arguments = vec![
                normalized.to_string_lossy().into_owned(),
                if parents { "1" } else { "0" }.to_owned(),
            ];
            let result = self
                .check_as_user(files::MKDIR_ACCESS_CHECK_SCRIPT, &arguments, &user)
                .await?;
            if !result.ok() {
                return Err(Self::refuse_denied_access(
                    "mkdir_access_check",
                    &normalized,
                    &arguments,
                    &result,
                ));
            }
        }
        files::make_directory(&normalized, parents)
    }

    async fn read(&self, path: &str, user: AsUser) -> SandboxResult<Vec<u8>> {
        let normalized = self.normalize_path(path, false)?;
        if let Some(user) = user {
            let arguments = vec![normalized.to_string_lossy().into_owned()];
            let result = self
                .check_as_user(files::READ_ACCESS_CHECK_SCRIPT, &arguments, &user)
                .await?;
            if !result.ok() {
                // The reference runs a shell probe here that resolves the path itself in order to
                // separate "is not there" from "may not be read". This asks the shell the same
                // question more simply, which differs only for a path whose parent directory the
                // account cannot search: that is reported as a read failure rather than as missing.
                // The full probe arrives with the runtime helper scripts.
                let exists = self
                    .check_as_user(r#"[ -e "$1" ]"#, &arguments, &user)
                    .await?;
                return Err(if exists.exit_code == 1 {
                    SandboxError::workspace_read_not_found(path)
                } else {
                    SandboxError::workspace_archive_read(path)
                }
                .with_context(
                    "stderr",
                    String::from_utf8_lossy(&result.stderr).into_owned(),
                ));
            }
        }
        files::read_file(&normalized, path)
    }

    async fn write(&self, path: &str, data: Vec<u8>, user: AsUser) -> SandboxResult<()> {
        let normalized = self.normalize_path(path, true)?;
        let Some(user) = user else {
            return files::write_file(&normalized, &data);
        };

        // Written *by* the other account rather than merely checked against it: a file the session
        // creates itself would belong to whoever runs the SDK, and a workspace whose files the
        // sandbox user cannot write is not the workspace that was asked for.
        let rendered = normalized.to_string_lossy().into_owned();
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"mkdir -p "$(dirname "$1")" && cat > "$1""#.to_owned(),
            "sh".to_owned(),
            rendered.clone(),
        ];
        let request = ExecRequest::new(command.clone())
            .with_shell(ra_core::sandbox::ShellInvocation::None)
            .as_user(user);
        let prepared = exec::prepare_exec_command(&request);
        let result = self.run_prepared(&prepared, None, Some(data)).await?;
        if result.ok() {
            return Ok(());
        }
        Err(SandboxError::workspace_archive_write(&rendered)
            .with_context("command", command)
            .with_context(
                "stdout",
                String::from_utf8_lossy(&result.stdout).into_owned(),
            )
            .with_context(
                "stderr",
                String::from_utf8_lossy(&result.stderr).into_owned(),
            ))
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        let manifest = self.manifest();
        archive::persist(Path::new(&manifest.root), &manifest)
    }

    async fn hydrate_workspace(&self, data: Vec<u8>) -> SandboxResult<()> {
        archive::hydrate(&self.workspace_root(), &data)
    }

    /// Writes an archive into the workspace and unpacks it beside itself.
    ///
    /// Through the shared extractor rather than this backend's own filesystem code, because every
    /// member it writes goes through this session's `mkdir` and `write` — the same path checks an
    /// ordinary write gets, applied to input that chose its own paths.
    async fn extract(
        &self,
        path: &str,
        data: Vec<u8>,
        scheme: Option<CompressionScheme>,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<()> {
        WorkspaceArchiveExtractor::new(self)
            .extract(path, data, scheme, limits)
            .await
    }

    async fn resolve_exposed_port(&self, port: u16) -> SandboxResult<ExposedPortEndpoint> {
        let state = self.state_now();
        if !state.exposed_ports().contains(&port) {
            return Err(SandboxError::exposed_port_unavailable(
                port,
                state.exposed_ports(),
                "not_configured",
            ));
        }
        // The sandbox and the host are the same machine, so a published port is already reachable;
        // there is nothing to forward and nothing to look up.
        Ok(ExposedPortEndpoint::new("127.0.0.1", port))
    }

    async fn prepare_backend_workspace(&self) -> SandboxResult<()> {
        let root = self.workspace_root();
        std::fs::create_dir_all(&root).map_err(|error| {
            SandboxError::workspace_start(&root.to_string_lossy(), Some(&error.to_string()))
                .with_cause(error)
        })
    }

    /// Whether accounts named by the manifest still need creating.
    ///
    /// Always, because nothing about a local host is preserved between sessions in the way a
    /// container's user database is. In practice this backend has no accounts to create — a
    /// manifest that names one is refused — but the answer has to be the truthful one so the
    /// lifecycle does not skip a step on the strength of a preservation that never happened.
    fn should_provision_accounts(&self) -> bool {
        true
    }

    async fn provision_accounts(&self) -> SandboxResult<()> {
        assert_accounts_unsupported(&self.manifest())
    }

    async fn apply_manifest(
        &self,
        provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        let manifest = self.manifest();
        assert_host_path_grants_unsupported(&manifest)?;
        // Refused whatever `provision_accounts` says. The flag asks whether the accounts still need
        // creating; this backend cannot create one at all, and materializing content that is meant
        // to belong to a missing account would hand it to whoever runs the SDK instead.
        assert_accounts_unsupported(&manifest)?;
        self.applier()?
            .apply_manifest(&manifest, provision_accounts)
            .await
    }

    /// Rebuilds the entries that were deliberately never persisted.
    ///
    /// **Not reached on this backend yet**: it is only called for a workspace whose content
    /// survived a stop, and nothing here reports one — snapshots are not carried over, so every
    /// start materializes the whole manifest. It is implemented anyway because that is the entry
    /// point the snapshot lifecycle calls, and because an ephemeral application is a different
    /// answer from a full one rather than a cheaper one.
    async fn reapply_ephemeral_manifest(&self) -> SandboxResult<()> {
        let manifest = self.manifest();
        assert_host_path_grants_unsupported(&manifest)?;
        self.applier()?.apply_ephemeral(&manifest).await.map(|_| ())
    }

    /// Whether the snapshot this session carries has something stored.
    async fn snapshot_restorable(&self) -> SandboxResult<bool> {
        self.snapshots().restorable().await
    }

    /// Replaces the workspace with what the snapshot holds.
    async fn restore_snapshot(&self) -> SandboxResult<()> {
        self.snapshots().restore_on_resume().await
    }

    async fn remove_workspace_entry_on_resume(&self, path: &str) -> SandboxResult<()> {
        let entry = std::path::Path::new(path);
        let name = entry.file_name().ok_or_else(|| {
            SandboxError::workspace_archive_write(path)
                .with_context("reason", "resume cleanup requires a child entry")
        })?;
        let parent = entry.parent().unwrap_or_else(|| std::path::Path::new("."));
        let parent = self.normalize_path(parent.to_str().unwrap_or("."), true)?;
        // Resolve and authorize the parent, but never follow the entry being removed. A stray
        // link may point outside the workspace or be dangling; neither target is ours to remove.
        files::remove(&parent.join(name), true)
    }

    /// Whether the workspace already matches the snapshot closely enough to keep it.
    ///
    /// **Not reached on this backend.** The lifecycle only asks when workspace content survived a
    /// stop, and a local directory is never reported as preserved — the same answer the reference's
    /// local backend gives. Wired up anyway so that the fingerprint this session records on persist
    /// has the reader it was recorded for.
    async fn can_skip_snapshot_restore(&self, is_running: bool) -> SandboxResult<bool> {
        self.snapshots().can_skip_restore(is_running).await
    }

    /// Writes the workspace into the snapshot's storage.
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

    /// Runs after a successful start.
    ///
    /// The running flag is set here and not earlier, because a resumed session may have just
    /// recreated an empty workspace where a previous one was deleted. A session that called itself
    /// running before the workspace was populated would let a later resume trust a fingerprint that
    /// describes content no longer on disk.
    async fn after_start(&self) -> SandboxResult<()> {
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn after_start_failed(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    async fn record_workspace_root_ready(&self) -> SandboxResult<()> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let updated = guard.clone().with_workspace_root_ready(true);
        *guard = updated;
        Ok(())
    }

    fn wrap_stop_error(&self, error: SandboxError) -> SandboxError {
        SandboxError::workspace_stop(&self.workspace_root().to_string_lossy())
            .with_sandbox_cause(error)
    }

    /// Runs after the backend is torn down.
    ///
    /// The workspace directory is deliberately left alone: deleting it belongs to the client, which
    /// is the only party that knows whether this session created it.
    async fn after_shutdown(&self) -> SandboxResult<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}
