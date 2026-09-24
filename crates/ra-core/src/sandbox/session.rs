//! What a workspace execution environment offers, and the order its lifecycle runs in.
//!
//! A session is a workspace that outlives a command: it starts, holds files, runs things, persists
//! what it was, and stops.
//!
//! # The lifecycle is a default, not a sealed sequence
//!
//! The reference puts start, stop, shutdown and close on its base class as concrete methods that
//! call overridable hooks, and here they are default trait methods that do the same. A backend
//! implements the hooks and inherits the order; one that overrides the order can, exactly as a
//! subclass there can.
//!
//! That order is worth reading before replacing: the workspace probe runs *before* the backend
//! prepares the workspace, so a directory this start created is never mistaken for evidence of a
//! resume; the after-start hook sits outside the guarded block, so its failure is not reported as a
//! failed start; stop persists and runs its after-hook either way, and is not teardown.
//!
//! # What is not here
//!
//! This is the protocol. The decorating layer the reference wraps around a session — events,
//! tracing, concurrency limits, path validation — is an implementation and belongs in the service
//! crate, as does every backend. The owner-driven cleanup the runtime performs instead of
//! [`SandboxSession::close`] lands with the task that ports it; the difference between the two
//! closing paths is recorded on `close` so that whoever ports the second does not assume it is the
//! same sequence.
//!
//! What the reference keeps as attributes of its base class — the dependency container, the
//! pre-stop callbacks, the close lock — a backend embeds as one [`SessionResources`] and hands out
//! from [`SandboxSession::resources`].

use std::sync::Arc;

use async_trait::async_trait;

use super::archive::{CompressionScheme, SandboxArchiveLimits};
use super::dependencies::Dependencies;
use super::error::{ErrorCode, OpName, SandboxError};
use super::files::FileEntry;
use super::manifest::{Manifest, ManifestRegistries};
use super::materialization::MaterializationResult;
use super::mount_security::validate_manifest_mount_credential_boundaries;
use super::pty::{PtyExecUpdate, PtyStartRequest, PtyWriteRequest};
use super::registry::{DiscriminatedPayload, TypeRegistry};
use super::resources::{PreStopHook, SessionResources};
use super::snapshot::{Snapshot, SnapshotFingerprint, SnapshotSource, SnapshotSpec};
use super::state::{
    InvalidSessionStatePayload, REDACTED_HOST_PATH_GRANT_PATHS_KEY, SandboxSessionState,
};
use super::types::{ExecResult, ExposedPortEndpoint, User};

/// What a sandbox operation returns when it can fail.
pub type SandboxResult<T> = std::result::Result<T, SandboxError>;

/// How a command's arguments reach the shell, when they do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ShellInvocation {
    /// Run through the backend's default login shell.
    ///
    /// The reference spells this `sh -lc`, which is where `exec_command`'s login default comes
    /// from: a command run this way sees whatever the profile files set up.
    #[default]
    Login,
    /// Run the argument vector directly, with no shell between.
    None,
    /// Run through the given prefix instead of the default one.
    Prefix(Vec<String>),
}

/// What a caller is asking a session to run.
#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    /// The command and its arguments.
    pub command: Vec<String>,
    /// How long to wait before giving up, in seconds.
    pub timeout_s: Option<f64>,
    /// Whether a shell sits between the session and the command.
    pub shell: ShellInvocation,
    /// The account to run as, or the session's default.
    pub user: Option<User>,
}

impl ExecRequest {
    /// Asks for a command, run through the default login shell.
    #[must_use]
    pub fn new(command: impl IntoIterator<Item = String>) -> Self {
        Self {
            command: command.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Gives up after `timeout_s` seconds.
    #[must_use]
    pub const fn with_timeout_s(mut self, timeout_s: f64) -> Self {
        self.timeout_s = Some(timeout_s);
        self
    }

    /// Puts a different shell, or none, between the session and the command.
    #[must_use]
    pub fn with_shell(mut self, shell: ShellInvocation) -> Self {
        self.shell = shell;
        self
    }

    /// Runs as `user` instead of the session's default.
    #[must_use]
    pub fn as_user(mut self, user: User) -> Self {
        self.user = Some(user);
        self
    }
}

/// Which account a workspace operation acts as.
pub type AsUser = Option<User>;

/// A backend's workspace execution environment.
///
/// Two methods have no default: running a command, and saying what the session's durable state is.
/// Everything else describes something a backend may or may not do, and the defaults are what a
/// backend that does not do it should answer — a local session has nothing to tear down, a session
/// without a terminal refuses to start one.
#[async_trait]
pub trait SandboxSession: Send + Sync {
    /// Which backend this session belongs to.
    fn backend_id(&self) -> &str;

    /// The durable half of this session, as it stands now.
    ///
    /// **Returns a value rather than a borrow.** The lifecycle hooks take `&self` and several of
    /// them have to record something — that the workspace root now exists, what fingerprint the
    /// snapshot was taken at — so a real backend keeps its state behind a lock. A borrow would
    /// either forbid that or have to escape the lock that guards it, so the caller gets a copy of
    /// what the state was when it asked.
    fn state(&self) -> SandboxSessionState;

    /// The dependency container, pre-stop callbacks and close lock this session holds.
    ///
    /// Every session has them, as every session on the reference does; the lifecycle defaults below
    /// are written against them.
    fn resources(&self) -> &SessionResources;

    // --- dependencies and pre-stop callbacks -----------------------------------------------

    /// The session's dependency container, created empty the first time it is asked for.
    ///
    /// Snapshot storage and manifest materialization resolve what they need from here — a remote
    /// snapshot's storage client, for one.
    fn dependencies(&self) -> Arc<Dependencies> {
        self.resources().dependencies()
    }

    /// Replaces the session's dependency container; `None` leaves the current one in place.
    ///
    /// A client calls this with its own copy of the template it was configured with, so each
    /// session gets its own factory cache and owned-resource lifecycle.
    fn set_dependencies(&self, dependencies: Option<Arc<Dependencies>>) {
        self.resources().set_dependencies(dependencies);
    }

    /// Registers a callback to run once before the workspace is persisted.
    fn register_pre_stop_hook(&self, hook: PreStopHook) {
        self.resources().register_pre_stop_hook(hook);
    }

    // --- capability probes ----------------------------------------------------------------

    /// Whether this session can allocate a terminal.
    ///
    /// The tool surface reads this: the interactive write tool is offered only where a terminal can
    /// exist, because a caller handed that tool by a session without one has been given something
    /// that cannot work.
    fn supports_pty(&self) -> bool {
        false
    }

    /// Whether this session can attach volume-driver mounts when it is created.
    fn supports_volume_mounts(&self) -> bool {
        false
    }

    // --- execution ------------------------------------------------------------------------

    /// Runs a command to completion.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure, which for a command that ran and exited non-zero carries the
    /// original streams rather than only a rendered message.
    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult>;

    /// Whether the session is alive.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when it cannot tell. Not knowing is not the same as "no": a
    /// caller that reads an error as "stopped" will delete a workspace that is still running.
    async fn running(&self) -> SandboxResult<bool>;

    /// Starts a command that keeps running, and returns what it produced so far.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::PtySessionNotFound`] shaped refusal when this session has no terminal
    /// to offer, or the backend's failure to start one.
    async fn pty_start(&self, request: PtyStartRequest) -> SandboxResult<PtyExecUpdate> {
        let _ = request;
        Err(self.pty_unsupported())
    }

    /// Sends input to a running interactive process, or waits for more of its output.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::PtySessionNotFound`] when the process is gone or this session never had
    /// terminals, or the backend's failure to write.
    async fn pty_write(&self, request: PtyWriteRequest) -> SandboxResult<PtyExecUpdate> {
        let _ = request;
        Err(self.pty_unsupported())
    }

    /// Ends every interactive process this session started.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to end them.
    async fn pty_terminate_all(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// The refusal a session without terminals gives.
    fn pty_unsupported(&self) -> SandboxError {
        SandboxError::new(
            ErrorCode::PtySessionNotFound,
            OpName::Exec,
            "PTY execution is not supported by this sandbox session",
        )
        .with_context("backend", self.backend_id().to_owned())
    }

    // --- the workspace --------------------------------------------------------------------

    /// Validates access and returns the path the backend's file operations use.
    ///
    /// The default applies the manifest's lexical path policy. Backends that resolve filesystem
    /// links must override this so callers can derive destinations from the same resolved path.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid root, or the path policy's access refusal.
    async fn validate_path_access(&self, path: &str, for_write: bool) -> SandboxResult<String> {
        let state = self.state();
        let manifest = state.manifest();
        let policy = super::workspace_paths::WorkspacePathPolicy::new(
            &manifest.root,
            manifest.extra_path_grants.clone(),
        )
        .map_err(|error| {
            SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Write,
                error.to_string(),
            )
        })?;
        Ok(policy
            .normalize_sandbox_path(path, for_write)?
            .as_str()
            .to_owned())
    }

    /// Lists a directory.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::WorkspaceReadNotFound`] for a path that is not there, or the backend's
    /// failure to read it.
    async fn ls(&self, path: &str, user: AsUser) -> SandboxResult<Vec<FileEntry>>;

    /// Removes a path, optionally with everything under it.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to remove it.
    async fn rm(&self, path: &str, recursive: bool, user: AsUser) -> SandboxResult<()>;

    /// Creates a directory, optionally creating its parents.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to create it.
    async fn mkdir(&self, path: &str, parents: bool, user: AsUser) -> SandboxResult<()>;

    /// Reads a file out of the workspace.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::WorkspaceReadNotFound`] for a path that is not there, or the backend's
    /// failure to read it.
    async fn read(&self, path: &str, user: AsUser) -> SandboxResult<Vec<u8>>;

    /// Writes a file into the workspace.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to write it.
    async fn write(&self, path: &str, data: Vec<u8>, user: AsUser) -> SandboxResult<()>;

    /// Writes an archive into the workspace and unpacks it beside itself.
    ///
    /// The archive lands at `path` and its members are written into the directory that holds it, so
    /// a caller ends up with both. `scheme` says which format it is in; `None` reads it from the
    /// archive's own extension. `limits` bound what unpacking may cost, and **`None` means no
    /// bounds at all** — the reference's default, where a caller opts into the built-in ceilings by
    /// passing them.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidCompressionScheme`] when the format is unstated and cannot be
    /// read from the name, [`ErrorCode::WorkspaceArchiveWriteError`] for an archive that is
    /// malformed, that would write outside the workspace, or that exceeds a limit, and the
    /// backend's own failure to write.
    async fn extract(
        &self,
        path: &str,
        data: Vec<u8>,
        scheme: Option<CompressionScheme>,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<()> {
        let _ = (path, data, scheme, limits);
        Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::Write,
            "unpacking an archive is not supported by this sandbox session",
        )
        .with_context("backend", self.backend_id().to_owned()))
    }

    /// Streams the whole workspace out, so it can be moved or kept.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::WorkspaceArchiveReadError`] or the backend's own failure.
    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>>;

    /// Streams a workspace in, replacing what is there.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::WorkspaceArchiveWriteError`] or the backend's own failure.
    async fn hydrate_workspace(&self, data: Vec<u8>) -> SandboxResult<()>;

    /// Resolves a forwarded port to an address reachable from the host.
    ///
    /// The default separates the two ways this fails, because they call for different things. A
    /// port nobody configured will not become configured by trying again, so that is refused
    /// outright. A configured port the backend cannot map right now might work later, and the
    /// backend is the only party that can say — so it is left unclassified rather than declared
    /// permanent.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ExposedPortUnavailable`] either way.
    async fn resolve_exposed_port(&self, port: u16) -> SandboxResult<ExposedPortEndpoint> {
        let state = self.state();
        let ports = state.exposed_ports();
        if ports.contains(&port) {
            Err(
                SandboxError::exposed_port_unavailable(port, ports, "backend_unavailable")
                    .with_context("backend", self.backend_id().to_owned()),
            )
        } else {
            Err(SandboxError::exposed_port_unavailable(
                port,
                ports,
                "not_configured",
            ))
        }
    }

    // --- lifecycle hooks ------------------------------------------------------------------
    //
    // Called by the lifecycle defaults below in a fixed order. A backend implements the ones it
    // needs and leaves the rest, which is how a local session and a container session share one
    // sequence.

    /// Brings the backend up, if it is not already.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to start.
    async fn ensure_backend_started(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Looks for evidence that a previous workspace survived.
    ///
    /// **Runs before the workspace is prepared**, which is the whole point: afterwards, a directory
    /// this session just created is indistinguishable from one a previous session left behind.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to probe.
    async fn probe_workspace_root(&self) -> SandboxResult<bool> {
        Ok(false)
    }

    /// Creates whatever the workspace needs before content goes into it.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to prepare.
    async fn prepare_backend_workspace(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Installs the helper scripts the backend runs things through.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to install them.
    async fn ensure_runtime_helpers(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Whether the snapshot this session carries can be restored from.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to tell.
    async fn snapshot_restorable(&self) -> SandboxResult<bool> {
        Ok(false)
    }

    /// Whether workspace content from a previous run is known to have survived.
    fn workspace_state_preserved_on_start(&self) -> bool {
        false
    }

    /// Whether a restore can be skipped because the live workspace already matches the snapshot.
    ///
    /// Only asked when the workspace was preserved; a fresh one has nothing to compare against.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to compare them.
    async fn can_skip_snapshot_restore(&self, _is_running: bool) -> SandboxResult<bool> {
        Ok(false)
    }

    /// Whether accounts named by the manifest still need creating on this start.
    fn should_provision_accounts(&self) -> bool {
        false
    }

    /// Creates the accounts the manifest names.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to create them.
    async fn provision_accounts(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Restores the snapshot into the workspace.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SnapshotRestoreError`] or the backend's own failure.
    async fn restore_snapshot(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Removes an entry while clearing the workspace for snapshot restoration.
    ///
    /// Backends whose ordinary removal follows the final symlink must override this to remove
    /// the entry itself after validating its parent, leaving the link target untouched.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to validate or remove the entry.
    async fn remove_workspace_entry_on_resume(&self, path: &str) -> SandboxResult<()> {
        self.rm(path, true, None).await
    }

    /// Rebuilds the manifest state that was deliberately never persisted.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to rebuild it.
    async fn reapply_ephemeral_manifest(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Materializes the whole manifest.
    ///
    /// Returns a receipt of what was written. An empty one does not mean nothing was written — an
    /// entry materialized by a command inside the sandbox cannot hash its own files without reading
    /// them back out — so a caller reads it as "these files, for certain" rather than "only these".
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to materialize it.
    async fn apply_manifest(
        &self,
        _provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        Ok(MaterializationResult::new())
    }

    /// Runs after a successful start.
    ///
    /// **Outside the guarded block**: a failure here is not reported as a failed start, and does
    /// not run the failure hook. By then the session really is running, and reporting it as failed
    /// would have a caller tear down something that came up.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure.
    async fn after_start(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Records that the workspace root now exists, so a later start can tell a resume from a fresh
    /// one.
    ///
    /// Called last on a successful start. A backend that persists its state has to write this
    /// down; one that does not can ignore it and will simply probe again next time.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to record it.
    async fn record_workspace_root_ready(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Runs after a start that failed inside the guarded block.
    ///
    /// Never returns an error: it is cleanup for a start that already failed, and a failure here
    /// would replace the reason the caller needs to see.
    async fn after_start_failed(&self) {}

    /// Gives a start failure the backend's own wording.
    fn wrap_start_error(&self, error: SandboxError) -> SandboxError {
        error
    }

    /// Runs before the workspace is persisted, to settle transient processes.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure.
    async fn before_stop(&self) -> SandboxResult<()> {
        self.pty_terminate_all().await
    }

    /// Persists the workspace into a snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SnapshotPersistError`] or the backend's own failure.
    async fn persist_snapshot(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Records what the workspace hashed to when the snapshot was taken, or that nothing did.
    ///
    /// Called by the snapshot lifecycle as part of persisting, and `None` is as meaningful as a
    /// value: it says this persist produced no fingerprint, so the one an earlier persist left
    /// behind must not be compared against a later workspace.
    ///
    /// A backend that does not keep its state across a stop can ignore this, as it ignores
    /// [`Self::record_workspace_root_ready`], and will simply have nothing to compare next time.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to record it.
    async fn record_snapshot_fingerprint(
        &self,
        fingerprint: Option<SnapshotFingerprint>,
    ) -> SandboxResult<()> {
        let _ = fingerprint;
        Ok(())
    }

    /// Runs after a stop, whether or not persistence succeeded.
    async fn after_stop(&self) {}

    /// Gives a stop failure the backend's own wording.
    fn wrap_stop_error(&self, error: SandboxError) -> SandboxError {
        error
    }

    /// Runs before the backend is torn down.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure.
    async fn before_shutdown(&self) -> SandboxResult<()> {
        self.pty_terminate_all().await
    }

    /// Tears the backend down.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to tear down.
    async fn shutdown_backend(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Runs after the backend is torn down.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure.
    async fn after_shutdown(&self) -> SandboxResult<()> {
        Ok(())
    }

    /// Runs the callbacks registered to fire before this session stops, **once**.
    ///
    /// A second call after the first has run returns success without running anything again. That
    /// is not the same as saying they succeeded — see [`Self::pre_stop_hooks_failed`].
    ///
    /// # Errors
    ///
    /// Returns the first callback failure, on the call that actually ran them.
    async fn run_pre_stop_hooks(&self) -> SandboxResult<()> {
        self.resources().run_pre_stop_hooks().await
    }

    /// Whether the pre-stop callbacks have ever failed.
    ///
    /// **Sticky, and that is the point.** The callbacks run once, so a second close finds them
    /// already run and is told nothing went wrong. Without a flag that outlives the call, that
    /// second close would persist a workspace the first one deliberately refused to persist —
    /// exactly the state the failed callback was there to prevent.
    fn pre_stop_hooks_failed(&self) -> bool {
        self.resources().pre_stop_hooks_failed()
    }

    /// Releases whatever the session was holding on behalf of its caller.
    ///
    /// Called at most once in effect: a backend that has already released them returns success.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to release them.
    async fn close_dependencies(&self) -> SandboxResult<()> {
        self.resources().close_dependencies().await;
        Ok(())
    }

    /// Checks the session's manifest at the mount credential boundary of its own backend.
    ///
    /// Run again at each lifecycle step that could act on a mount, because the manifest a session
    /// holds can change between them.
    ///
    /// # Errors
    ///
    /// Returns the boundary failure.
    fn validate_mount_credential_boundaries(&self) -> SandboxResult<()> {
        let state = self.state();
        validate_manifest_mount_credential_boundaries(state.manifest(), Some(state.state_type()))
    }

    // --- lifecycle ------------------------------------------------------------------------
    //
    // Defaults, as they are on the reference's base class. Overridable for the same reason they
    // are there, and worth reading before being replaced: the comments say which orderings are
    // decisions rather than convenience.

    /// Brings the session up.
    ///
    /// Answers whether the workspace was already there when this start began, which is what
    /// distinguishes a resume from a fresh start for every branch below it.
    ///
    /// # Errors
    ///
    /// Returns the first failure from the guarded block, given the backend's wording. The failure
    /// hook has already run by then.
    async fn start(&self) -> SandboxResult<bool> {
        // Before the guarded block, as the reference has it: a manifest refused at the credential
        // boundary never started anything, so there is no failed start to clean up after.
        self.validate_mount_credential_boundaries()?;
        match self.start_guarded().await {
            Ok(preserved) => {
                self.after_start().await?;
                // Last, and only on success: the root is proven to exist now, and a start that did
                // not finish has proven nothing.
                self.record_workspace_root_ready().await?;
                Ok(preserved)
            }
            Err(error) => {
                self.after_start_failed().await;
                Err(self.wrap_start_error(error))
            }
        }
    }

    /// The part of a start whose failure runs the failure hook.
    ///
    /// # Errors
    ///
    /// Returns the first failure, unwrapped.
    async fn start_guarded(&self) -> SandboxResult<bool> {
        self.ensure_backend_started().await?;

        // Read before anything is created: once the workspace has been prepared, "the root exists"
        // no longer tells a resume from a fresh start. A state written by a previous run is the
        // other way to know, and either is enough.
        let root_ready_at_start =
            self.state().workspace_root_ready() || self.probe_workspace_root().await?;

        self.prepare_backend_workspace().await?;
        self.ensure_runtime_helpers().await?;
        self.start_workspace(root_ready_at_start).await?;
        Ok(root_ready_at_start)
    }

    /// Chooses what the workspace needs, out of the four things it can need.
    ///
    /// # Errors
    ///
    /// Returns the failure of whichever branch ran.
    async fn start_workspace(&self, root_ready_at_start: bool) -> SandboxResult<()> {
        self.validate_mount_credential_boundaries()?;
        let preserved = self.workspace_state_preserved_on_start() && root_ready_at_start;

        if self.snapshot_restorable().await? {
            // Only asked when there is a preserved workspace to compare against. A fresh one has
            // nothing to match, and asking a backend whether it is running — which can fail — would
            // abandon a restore that was going to happen regardless of the answer.
            let can_skip = if preserved {
                let is_running = self.running().await?;
                self.can_skip_snapshot_restore(is_running).await?
            } else {
                false
            };

            if can_skip {
                // The workspace already matches the snapshot, so restoring would replace live
                // content with an identical copy and lose anything written since.
                self.reapply_ephemeral_manifest().await
            } else {
                self.restore_snapshot().await?;
                if self.should_provision_accounts() {
                    self.provision_accounts().await?;
                }
                self.reapply_ephemeral_manifest().await
            }
        } else if preserved {
            // Nothing durable to restore, but a reconnected backend still needs the mounts and
            // files that were never meant to survive.
            self.reapply_ephemeral_manifest().await
        } else {
            self.apply_manifest(self.should_provision_accounts())
                .await
                .map(|_receipt| ())
        }
    }

    /// Persists the session's workspace.
    ///
    /// **This is not teardown.** A session that has stopped has written down what it was; releasing
    /// what it was running is [`Self::shutdown`]. The after-stop hook runs whether or not
    /// persistence succeeded, because it settles state this session will otherwise leave behind.
    ///
    /// # Errors
    ///
    /// Returns the persistence failure, given the backend's wording.
    async fn stop(&self) -> SandboxResult<()> {
        // Outside the persist-and-settle sequence: a refusal here means nothing was attempted, so
        // there is nothing for the after-stop hook to settle.
        self.validate_mount_credential_boundaries()?;
        let outcome = async {
            self.before_stop().await?;
            self.persist_snapshot().await
        }
        .await;

        self.after_stop().await;
        outcome.map_err(|error| self.wrap_stop_error(error))
    }

    /// Tears the session's backend down.
    ///
    /// Sequential, and a failure stops the rest: running the later steps over a backend that failed
    /// to shut down would act on a session whose state nobody knows.
    ///
    /// # Errors
    ///
    /// Returns the first failure.
    async fn shutdown(&self) -> SandboxResult<()> {
        self.before_shutdown().await?;
        self.shutdown_backend().await?;
        self.after_shutdown().await
    }

    /// Closes a session the caller holds directly.
    ///
    /// Held under the session's close lock so two closes cannot interleave, then: pre-stop callbacks,
    /// stop, shutdown, and dependencies released whatever happened. Three refusals are deliberate
    /// and easy to smooth away by accident:
    ///
    /// - **Callbacks that failed keep the workspace from being persisted, on this close and every
    ///   later one.** They run once, so a second close is told nothing went wrong; the sticky flag
    ///   is what stops it from saving the state the failure was there to prevent.
    /// - **A failed callback still shuts down.** The workspace is not persisted, but the backend is
    ///   still released, because it would otherwise leak.
    /// - **A failed stop skips the shutdown.** A session that could not persist is left for a
    ///   caller to look at rather than torn down underneath them.
    ///
    /// **The owner-driven cleanup the runtime performs is a different sequence** — there, a failed
    /// stop still attempts shutdown, delete and dependency release. That is a fact about the
    /// reference at the pinned commit, not an accident to unify here.
    ///
    /// # Errors
    ///
    /// Returns the first failure recorded, after every step that was still eligible has run.
    async fn close(&self) -> SandboxResult<()> {
        let _closing = self.resources().lock_close().await;
        self.close_guarded().await
    }

    /// The close itself, with the guard already held.
    ///
    /// # Errors
    ///
    /// Returns the first failure recorded.
    async fn close_guarded(&self) -> SandboxResult<()> {
        let mut first_error: Option<SandboxError> = None;

        if let Err(error) = self.run_pre_stop_hooks().await {
            first_error = Some(error);
        }

        // Consulted rather than inferred from this call: the callbacks run once, so a later close
        // gets a clean return from a session whose callbacks failed the first time.
        let hooks_failed = first_error.is_some() || self.pre_stop_hooks_failed();

        let stop_failed = if hooks_failed {
            false
        } else {
            match self.stop().await {
                Ok(()) => false,
                Err(error) => {
                    first_error.get_or_insert(error);
                    true
                }
            }
        };

        if !stop_failed && let Err(error) = self.shutdown().await {
            first_error.get_or_insert(error);
        }

        if let Err(error) = self.close_dependencies().await {
            first_error.get_or_insert(error);
        }

        first_error.map_or(Ok(()), Err)
    }
}

/// How a client was asked to make a session.
#[derive(Debug, Clone, Default)]
pub struct CreateRequest {
    /// What to start the workspace from and persist back to, or nothing.
    ///
    /// Either a snapshot that already names stored content, or a spec saying where to put one; the
    /// client settles it with [`resolve_snapshot`](super::snapshot::resolve_snapshot) once it has
    /// chosen the session's id.
    pub snapshot: Option<SnapshotSource>,
    /// What the workspace should contain.
    pub manifest: Option<Manifest>,
    /// The backend's own settings: an image, a template, an endpoint.
    ///
    /// Carried as a routed payload rather than a concrete type, for the reason the registry exists:
    /// a third-party backend's options are its own, and a closed set here would shut it out. A
    /// client checks the discriminator names its own backend before reading anything.
    pub options: Option<DiscriminatedPayload>,
}

impl CreateRequest {
    /// Asks for a session with nothing restored and nothing materialized.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the workspace from `snapshot`, which already names stored content.
    #[must_use]
    pub fn with_snapshot(mut self, snapshot: Snapshot) -> Self {
        self.snapshot = Some(SnapshotSource::Snapshot(snapshot));
        self
    }

    /// Stores the workspace where `spec` says, under the id the client gives the session.
    #[must_use]
    pub fn with_snapshot_spec(mut self, spec: SnapshotSpec) -> Self {
        self.snapshot = Some(SnapshotSource::Spec(spec));
        self
    }

    /// Materializes `manifest` into the workspace.
    #[must_use]
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    /// Configures the backend with its own settings.
    #[must_use]
    pub fn with_options(mut self, options: DiscriminatedPayload) -> Self {
        self.options = Some(options);
        self
    }
}

/// What makes, resumes and releases sessions for one backend.
///
/// A client is the only thing that decides a session exists. That is what lets a host hand in a
/// session it made itself and have the run leave it alone: whoever created it is who ends it.
#[async_trait]
pub trait SandboxClient: Send + Sync {
    /// Which backend this client speaks for.
    ///
    /// Also the discriminator its options and its sessions' states carry, which is how a payload
    /// finds its way back to a client that can read it.
    fn backend_id(&self) -> &str;

    /// Whether this client can make a session with no options supplied.
    ///
    /// A backend with a usable default answers yes; one that needs an image, a template or an
    /// endpoint answers no, so a host is told to configure it rather than handed a session that
    /// cannot start.
    fn supports_default_options(&self) -> bool {
        false
    }

    /// Checks that options, if given, were meant for this backend.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the options name a different backend, or
    /// when none were given and this client has no usable default.
    fn check_options(&self, options: Option<&DiscriminatedPayload>) -> SandboxResult<()> {
        match options {
            Some(options) if options.type_name() != self.backend_id() => Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Start,
                format!(
                    "sandbox client options for `{}` cannot configure the `{}` backend",
                    options.type_name(),
                    self.backend_id()
                ),
            )
            .with_context("options_type", options.type_name().to_owned())
            .with_context("backend", self.backend_id().to_owned())),
            None if !self.supports_default_options() => Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Start,
                format!(
                    "the `{}` backend needs options and has no usable default",
                    self.backend_id()
                ),
            )
            .with_context("backend", self.backend_id().to_owned())),
            _ => Ok(()),
        }
    }

    /// Makes a new session.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to create one, a refusal of the manifest it was given, or a
    /// refusal of options meant for something else.
    async fn create(&self, request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>>;

    /// Reattaches to the sandbox a state describes, or replaces it.
    ///
    /// Reattaching is tried first, including after a process died without releasing anything: a
    /// sandbox that is still there should be picked up and eventually cleaned up rather than
    /// abandoned. When it is gone, a replacement is made and its workspace is hydrated from the
    /// state's snapshot during start.
    ///
    /// The returned session is owned by whoever asked for it. A host that wants a session left
    /// alone passes the live one instead of resuming.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to reattach or replace.
    async fn resume(&self, state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>>;

    /// Releases a session's sandbox resources.
    ///
    /// **Borrows the session rather than consuming it.** The reference hands the session back, and
    /// a caller whose delete failed still holds the same object; taking ownership here would leave
    /// a failed cleanup with nothing to inspect and nothing to retry against.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure to release them, with the session still the caller's.
    async fn delete(&self, session: &dyn SandboxSession) -> SandboxResult<()>;

    /// Checks a manifest before anything is created for it.
    ///
    /// Runs the mount credential boundary for this backend, so a manifest that would expose
    /// credentials it should not is refused while nothing exists yet to clean up.
    ///
    /// # Errors
    ///
    /// Returns the boundary failure.
    fn validate_manifest_for_create(&self, manifest: &Manifest) -> SandboxResult<()> {
        validate_manifest_mount_credential_boundaries(manifest, Some(self.backend_id()))
    }

    /// Renders a session state into something a host can store.
    ///
    /// Mount authority is stripped (see [`SandboxSessionState::to_json`]), and so is every path
    /// grant with a host source: a host path is authority on this host, and the paths that were
    /// dropped are listed under [`REDACTED_HOST_PATH_GRANT_PATHS_KEY`] so a reader knows to ask a
    /// trusted manifest for them.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::MountConfigInvalid`] for a manifest holding a custom mount or mount
    /// strategy, or one carrying authority that cannot be rendered safely.
    fn serialize_session_state(
        &self,
        state: &SandboxSessionState,
    ) -> SandboxResult<serde_json::Value> {
        let (persistable, dropped) = state.without_host_path_grants();
        let mut payload = persistable.to_json()?;
        if !dropped.is_empty()
            && let serde_json::Value::Object(fields) = &mut payload
        {
            fields.insert(
                REDACTED_HOST_PATH_GRANT_PATHS_KEY.to_owned(),
                serde_json::Value::from(dropped.into_iter().collect::<Vec<_>>()),
            );
        }
        Ok(payload)
    }

    /// Reads back a state this backend wrote.
    ///
    /// Sanitizes before reading (see [`SandboxSessionState::parse`]), and refuses a state some
    /// other backend wrote. The result may still need its authority rebound from a trusted manifest
    /// before [`Self::resume`] accepts it.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] with a fixed message for any payload that does
    /// not read, including one written by a different backend; the message never quotes it.
    fn deserialize_session_state(
        &self,
        payload: serde_json::Value,
        snapshots: &TypeRegistry,
        manifests: &ManifestRegistries,
    ) -> SandboxResult<SandboxSessionState> {
        let invalid = |error: InvalidSessionStatePayload| {
            SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Start,
                error.to_string(),
            )
        };
        let state = SandboxSessionState::parse(payload, snapshots, manifests).map_err(invalid)?;
        if state.state_type() != self.backend_id() {
            return Err(invalid(InvalidSessionStatePayload::Invalid));
        }
        Ok(state)
    }
}
