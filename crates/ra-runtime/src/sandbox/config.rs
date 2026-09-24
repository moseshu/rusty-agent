//! `SandboxRunConfig`: how one run reaches the sandboxes its sandbox agents work in.

use std::{fmt, sync::Arc};

use ra_core::sandbox::{
    CwdError, DiscriminatedPayload, Manifest, ManifestRegistries, PosixPath, SandboxArchiveLimits,
    SandboxClient, SandboxConcurrencyLimits, SandboxSession, SandboxSessionState, Snapshot,
    SnapshotSource, SnapshotSpec, TypeRegistry, builtin_snapshot_registry, normalize_sandbox_cwd,
};

/// How a run creates, resumes or borrows the sandbox sessions its sandbox agents use.
///
/// Carried on the run configuration rather than on the agent, as on the reference: the same agent
/// runs against a local directory in a test and a container in production without being
/// redeclared, and a session the host already holds is handed in here rather than baked into a
/// declaration that outlives it.
///
/// # Which session a sandbox agent gets
///
/// 1. A live [`Self::session`] is used as it is, and **never stopped, shut down or deleted** by
///    the run: whoever made it ends it.
/// 2. Otherwise, the continued run's own record for this agent, when there is one, is resumed —
///    ahead of an explicit [`Self::session_state`], which is the fallback for a run that does not
///    carry its sandboxes in its checkpoint.
/// 3. Otherwise, a fresh session is created from [`Self::manifest`], or the agent's default
///    manifest when this names none.
///
/// Sessions the run created or resumed are the run's, and are cleaned up when it ends.
///
/// # Additions for a host with no global registry
///
/// The reference reads persisted states through registries every import fills in. Here a host
/// assembles its registries explicitly, so this carries the ones checkpointed states are read
/// with — the built-in families by default.
///
/// # Where a fresh session's snapshot goes
///
/// [`Self::with_snapshot`] or [`Self::with_snapshot_spec`] when set. Otherwise the client's
/// [`SandboxClient::default_snapshot_spec`] — for the local backend, the per-user directory the
/// reference defaults to — and a snapshot that stores nothing when the client cannot settle on a
/// place, as the reference falls back when its directory cannot be made.
#[must_use]
#[non_exhaustive]
#[derive(Clone)]
pub struct SandboxRunConfig {
    client: Option<Arc<dyn SandboxClient>>,
    options: Option<DiscriminatedPayload>,
    session: Option<Arc<dyn SandboxSession>>,
    session_state: Option<SandboxSessionState>,
    manifest: Option<Manifest>,
    snapshot: Option<SnapshotSource>,
    concurrency_limits: SandboxConcurrencyLimits,
    archive_limits: Option<SandboxArchiveLimits>,
    cwd: Option<PosixPath>,
    snapshot_registry: Arc<TypeRegistry>,
    manifest_registries: Arc<ManifestRegistries>,
}

impl Default for SandboxRunConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxRunConfig {
    /// A configuration that names nothing yet.
    ///
    /// Not usable on its own: a sandbox agent needs either a client or a live session, and the run
    /// refuses one given neither when it first prepares a sandbox agent.
    pub fn new() -> Self {
        Self {
            client: None,
            options: None,
            session: None,
            session_state: None,
            manifest: None,
            snapshot: None,
            concurrency_limits: SandboxConcurrencyLimits::default(),
            archive_limits: None,
            cwd: None,
            snapshot_registry: Arc::new(builtin_snapshot_registry()),
            manifest_registries: Arc::new(ManifestRegistries::builtin()),
        }
    }

    /// Creates and resumes sessions with `client`.
    pub fn with_client(mut self, client: Arc<dyn SandboxClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// Configures a fresh session with the client's own settings.
    pub fn with_options(mut self, options: DiscriminatedPayload) -> Self {
        self.options = Some(options);
        self
    }

    /// Uses a session the host already holds, and leaves its lifecycle to the host.
    pub fn with_session(mut self, session: Arc<dyn SandboxSession>) -> Self {
        self.session = Some(session);
        self
    }

    /// Resumes this state when the run's own checkpoint has nothing for the agent.
    pub fn with_session_state(mut self, state: SandboxSessionState) -> Self {
        self.session_state = Some(state);
        self
    }

    /// Creates fresh sessions with this manifest instead of the agent's default.
    ///
    /// Also the trusted manifest a resumed session's path grants and mount authority are rebound
    /// from, since those are never persisted.
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    /// Starts fresh sessions from a snapshot that already names stored content.
    pub fn with_snapshot(mut self, snapshot: Snapshot) -> Self {
        self.snapshot = Some(SnapshotSource::Snapshot(snapshot));
        self
    }

    /// Persists fresh sessions where `spec` says.
    pub fn with_snapshot_spec(mut self, spec: SnapshotSpec) -> Self {
        self.snapshot = Some(SnapshotSource::Spec(spec));
        self
    }

    /// Paces manifest application in every session the run uses.
    pub const fn with_concurrency_limits(mut self, limits: SandboxConcurrencyLimits) -> Self {
        self.concurrency_limits = limits;
        self
    }

    /// Bounds archive extraction in every session the run uses.
    ///
    /// Unset by default, and unset means **no** bounds — the reference's default. A host that wants
    /// the built-in ceilings passes [`SandboxArchiveLimits::default`] explicitly.
    pub const fn with_archive_limits(mut self, limits: SandboxArchiveLimits) -> Self {
        self.archive_limits = Some(limits);
        self
    }

    /// Measures relative paths the built-in tools receive from `cwd`, relative to the workspace
    /// root.
    ///
    /// This changes path resolution and nothing else: it does not confine the run to the
    /// directory, does not change the manifest's root, and does not change how a session resolves
    /// paths it is handed directly. The directory must exist and be reachable by the agent's user;
    /// the run checks that when it prepares each sandbox agent, after a fresh session has
    /// materialized its manifest.
    ///
    /// # Errors
    ///
    /// Returns [`CwdError`] for a path that is absolute or leaves the workspace.
    pub fn with_cwd(mut self, cwd: &str) -> Result<Self, CwdError> {
        self.cwd = Some(normalize_sandbox_cwd(cwd)?);
        Ok(self)
    }

    /// Reads checkpointed session states with these registries instead of the built-in ones.
    pub fn with_registries(
        mut self,
        snapshots: TypeRegistry,
        manifests: ManifestRegistries,
    ) -> Self {
        self.snapshot_registry = Arc::new(snapshots);
        self.manifest_registries = Arc::new(manifests);
        self
    }

    /// The client that creates and resumes sessions.
    #[must_use]
    pub fn client(&self) -> Option<&Arc<dyn SandboxClient>> {
        self.client.as_ref()
    }

    /// The client's own settings for a fresh session.
    #[must_use]
    pub const fn options(&self) -> Option<&DiscriminatedPayload> {
        self.options.as_ref()
    }

    /// The live session the host handed in.
    #[must_use]
    pub fn session(&self) -> Option<&Arc<dyn SandboxSession>> {
        self.session.as_ref()
    }

    /// The explicit state to resume from.
    #[must_use]
    pub const fn session_state(&self) -> Option<&SandboxSessionState> {
        self.session_state.as_ref()
    }

    /// The manifest fresh sessions are created with.
    #[must_use]
    pub const fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// The snapshot fresh sessions start from or persist to.
    #[must_use]
    pub const fn snapshot(&self) -> Option<&SnapshotSource> {
        self.snapshot.as_ref()
    }

    /// The pacing of manifest application.
    #[must_use]
    pub const fn concurrency_limits(&self) -> SandboxConcurrencyLimits {
        self.concurrency_limits
    }

    /// The bounds on archive extraction, if any.
    #[must_use]
    pub const fn archive_limits(&self) -> Option<SandboxArchiveLimits> {
        self.archive_limits
    }

    /// The model-facing working directory, relative to the workspace root.
    #[must_use]
    pub const fn cwd(&self) -> Option<&PosixPath> {
        self.cwd.as_ref()
    }

    /// The registry checkpointed snapshots are read with.
    #[must_use]
    pub fn snapshot_registry(&self) -> &TypeRegistry {
        &self.snapshot_registry
    }

    /// The registries checkpointed manifests are read with.
    #[must_use]
    pub fn manifest_registries(&self) -> &ManifestRegistries {
        &self.manifest_registries
    }

    /// The snapshot a fresh session is created with: the configured one, else the client's
    /// default, else one that stores nothing.
    ///
    /// A client that cannot settle on a place falls back to storing nothing, as the reference falls
    /// back when its managed directory cannot be made: a run is not refused for lacking somewhere to
    /// persist.
    pub(crate) fn resolve_snapshot(&self, client: &dyn SandboxClient) -> SnapshotSource {
        if let Some(snapshot) = &self.snapshot {
            return snapshot.clone();
        }
        SnapshotSource::Spec(client.default_snapshot_spec().unwrap_or(SnapshotSpec::Noop))
    }
}

impl fmt::Debug for SandboxRunConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxRunConfig")
            .field(
                "client",
                &self.client.as_ref().map(|client| client.backend_id()),
            )
            .field(
                "options",
                &self.options.as_ref().map(DiscriminatedPayload::type_name),
            )
            .field(
                "session",
                &self.session.as_ref().map(|session| session.backend_id()),
            )
            .field("session_state", &self.session_state.is_some())
            .field("manifest", &self.manifest.is_some())
            .field("snapshot", &self.snapshot)
            .field("concurrency_limits", &self.concurrency_limits)
            .field("archive_limits", &self.archive_limits)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}
