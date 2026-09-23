//! Making, resuming and releasing local workspaces.
//!
//! The client is the only thing that decides a local workspace exists, and therefore the only thing
//! allowed to delete one. That is what makes "the run created this directory" and "the caller
//! handed us their project directory" two different situations rather than one guess.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::sandbox::{
    CreateRequest, DEFAULT_MANIFEST_ROOT, Dependencies, EnvValueResolver, ErrorCode, OpName,
    SandboxClient, SandboxConcurrencyLimits, SandboxError, SandboxResult, SandboxSession,
    SandboxSessionState, UnresolvableEnvValues, resolve_snapshot,
};
use uuid::Uuid;

use crate::snapshot::{BuiltinSnapshotStore, SnapshotStore};

use super::session::assert_host_path_grants_unsupported;
use super::{
    DEFAULT_WORKSPACE_PREFIX, UNIX_LOCAL_BACKEND_ID, UnixLocalSandboxClientOptions,
    UnixLocalSandboxSession, WORKSPACE_ROOT_OWNED_FIELD, workspace_root_owned,
};

/// Makes local workspaces, and releases the ones it made.
///
/// # The environment policy lives here, not in a session state
///
/// Whether a command inherits this host's environment is a decision the process running the SDK
/// makes about its own machine. It is held on the client and applied to every session the client
/// creates or resumes, so a state written by a host that inherited everything cannot re-open the
/// environment on a host that chose not to.
pub struct UnixLocalSandboxClient {
    host_environment_allowlist: Option<BTreeSet<String>>,
    env_values: Arc<dyn EnvValueResolver>,
    concurrency_limits: SandboxConcurrencyLimits,
    snapshot_store: Arc<dyn SnapshotStore>,
    /// The bindings every session this client makes starts from, or `None` for none.
    dependencies: Option<Dependencies>,
}

impl std::fmt::Debug for UnixLocalSandboxClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixLocalSandboxClient")
            .field(
                "host_environment_allowlist",
                &self.host_environment_allowlist,
            )
            .finish_non_exhaustive()
    }
}

impl Default for UnixLocalSandboxClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UnixLocalSandboxClient {
    /// Makes sessions that inherit this host's environment.
    ///
    /// The reference's default, and it is the permissive one: every variable this process has,
    /// including credentials it was started with, reaches every command. A host that does not want
    /// that uses [`Self::isolated_environment`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            host_environment_allowlist: None,
            env_values: Arc::new(UnresolvableEnvValues),
            concurrency_limits: SandboxConcurrencyLimits::default(),
            snapshot_store: Arc::new(BuiltinSnapshotStore),
            dependencies: None,
        }
    }

    /// Makes sessions that see only the standard set of host variables.
    ///
    /// See [`super::HOST_ENVIRONMENT_ALLOWLIST`] for what that is.
    #[must_use]
    pub fn isolated_environment() -> Self {
        Self::with_host_environment_allowlist(
            super::HOST_ENVIRONMENT_ALLOWLIST
                .iter()
                .map(|name| (*name).to_owned()),
        )
    }

    /// Makes sessions that see exactly these host variables.
    ///
    /// **The two invalid combinations the reference rejects at runtime cannot be written here.**
    /// There, an allowlist without disabling inheritance raises, and a bare string instead of a
    /// collection raises; here, naming an allowlist *is* disabling inheritance, and the argument is
    /// an iterator of names rather than something a string satisfies by accident.
    #[must_use]
    pub fn with_host_environment_allowlist(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            host_environment_allowlist: Some(names.into_iter().collect()),
            env_values: Arc::new(UnresolvableEnvValues),
            concurrency_limits: SandboxConcurrencyLimits::default(),
            snapshot_store: Arc::new(BuiltinSnapshotStore),
            dependencies: None,
        }
    }

    /// Fetches a manifest's non-literal environment values through `resolver`.
    #[must_use]
    pub fn with_env_value_resolver(mut self, resolver: Arc<dyn EnvValueResolver>) -> Self {
        self.env_values = resolver;
        self
    }

    /// Reads and writes snapshots through `store` in every session this client makes.
    ///
    /// Held here rather than taken from a session state, as the environment policy is: which
    /// storage exists is something the process running the SDK knows, and a state that arrived from
    /// another host must not be able to name one of its own.
    #[must_use]
    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = store;
        self
    }

    /// Gives every session this client makes its own copy of `dependencies`.
    ///
    /// A copy rather than the container itself, as the reference's client makes one: each session
    /// gets its own factory cache and its own owned resources, so closing one session does not
    /// close what another is still using.
    #[must_use]
    pub fn with_dependencies(mut self, dependencies: Dependencies) -> Self {
        self.dependencies = Some(dependencies);
        self
    }

    /// Paces manifest application in every session this client makes.
    ///
    /// Held here for the same reason the environment policy is: it is a decision the process
    /// running the SDK makes about its own machine, and a session state that travelled from a host
    /// with more capacity must not widen it on arrival.
    #[must_use]
    pub const fn with_concurrency_limits(mut self, limits: SandboxConcurrencyLimits) -> Self {
        self.concurrency_limits = limits;
        self
    }

    /// Which host variables a session sees, or `None` when it sees all of them.
    #[must_use]
    pub fn host_environment_allowlist(&self) -> Option<&BTreeSet<String>> {
        self.host_environment_allowlist.as_ref()
    }

    /// Builds a session over a state this client already vetted.
    fn open(&self, state: SandboxSessionState) -> Box<dyn SandboxSession> {
        let session = UnixLocalSandboxSession::new(
            state,
            self.host_environment_allowlist.clone(),
            Arc::clone(&self.env_values),
        )
        .with_concurrency_limits(self.concurrency_limits)
        .with_snapshot_store(Arc::clone(&self.snapshot_store));
        session.set_dependencies(self.resolve_dependencies());
        Box::new(session)
    }

    /// A fresh copy of the configured dependencies for one session, or `None` when there are none.
    fn resolve_dependencies(&self) -> Option<Arc<Dependencies>> {
        self.dependencies
            .as_ref()
            .map(|template| Arc::new(template.clone_bindings()))
    }

    /// Refuses a state another backend wrote.
    fn assert_own_state(state: &SandboxSessionState, operation: OpName) -> SandboxResult<()> {
        if state.state_type() == UNIX_LOCAL_BACKEND_ID {
            return Ok(());
        }
        Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            operation,
            format!(
                "a `{}` session state cannot be handled by the `{UNIX_LOCAL_BACKEND_ID}` backend",
                state.state_type()
            ),
        )
        .with_context("state_type", state.state_type())
        .with_context("backend", UNIX_LOCAL_BACKEND_ID))
    }
}

#[async_trait]
impl SandboxClient for UnixLocalSandboxClient {
    fn backend_id(&self) -> &str {
        UNIX_LOCAL_BACKEND_ID
    }

    /// Whether a session can be made with no options supplied.
    ///
    /// Yes: a local workspace needs no image, endpoint or template, so the options exist only to
    /// publish ports and a caller that publishes none has nothing to say.
    fn supports_default_options(&self) -> bool {
        true
    }

    async fn create(&self, request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>> {
        self.check_options(request.options.as_ref())?;
        let options = match &request.options {
            Some(payload) => UnixLocalSandboxClientOptions::from_payload(payload)?,
            None => UnixLocalSandboxClientOptions::new(),
        };

        let mut manifest = request.manifest.unwrap_or_default();
        // Checked before anything is created. A configuration this backend cannot honour should
        // leave nothing behind, and a temporary directory made first would outlive the refusal.
        //
        // A manifest that asks for accounts is deliberately *not* refused here. The reference
        // refuses it when the manifest is materialized, and moving the refusal earlier would make
        // this client reject a configuration the reference lets a caller hold — the same manifest
        // can be handed to a container backend that can honour it.
        assert_host_path_grants_unsupported(&manifest)?;

        let mut workspace_root_owned = false;
        if manifest.root == DEFAULT_MANIFEST_ROOT {
            // The default root is a path inside a container. On this host it would be a directory
            // at the filesystem root, shared by every session that ever ran, so a session that was
            // not told where to work gets a private directory instead.
            let directory = tempfile::Builder::new()
                .prefix(DEFAULT_WORKSPACE_PREFIX)
                .tempdir()
                .map_err(|error| {
                    SandboxError::workspace_start(DEFAULT_MANIFEST_ROOT, Some(&error.to_string()))
                        .with_cause(error)
                })?;
            manifest.root = directory.keep().to_string_lossy().into_owned();
            workspace_root_owned = true;
        }

        let session_id = Uuid::new_v4();
        // A caller that named storage gets it as it stands; one that only said where to put a
        // snapshot has it named after this session, and one that said nothing gets the snapshot
        // that stores nothing — under the same id, so that turning storage on later does not
        // change what the session is called.
        let snapshot = resolve_snapshot(request.snapshot.as_ref(), &session_id.to_string())
            .map_err(|error| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    error.to_string(),
                )
                .with_cause(error)
            })?;
        let state = SandboxSessionState::new(UNIX_LOCAL_BACKEND_ID, snapshot, manifest)
            .with_session_id(session_id)
            .with_exposed_ports(options.exposed_ports().iter().copied())
            .map_err(|error| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    error.to_string(),
                )
            })?
            .with_field(WORKSPACE_ROOT_OWNED_FIELD, workspace_root_owned);
        Ok(self.open(state))
    }

    /// Reattaches to the workspace a state describes.
    ///
    /// There is nothing to reconnect to — the directory is either still on disk or it is not — so
    /// resuming is opening a session over the same root. A workspace that is gone is recreated
    /// empty by start, and restoring its contents from the state's snapshot is the snapshot
    /// lifecycle's job rather than this one's.
    async fn resume(&self, state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>> {
        Self::assert_own_state(&state, OpName::Start)?;
        assert_host_path_grants_unsupported(state.manifest())?;
        Ok(self.open(state))
    }

    /// Removes the workspace directory, when this backend is the one that created it.
    ///
    /// Best effort, as the reference is: a directory that is already gone, or that cannot be
    /// removed, does not fail a cleanup that has nothing else left to do. A root the caller
    /// supplied is never touched.
    async fn delete(&self, session: &dyn SandboxSession) -> SandboxResult<()> {
        let state = session.state();
        Self::assert_own_state(&state, OpName::Shutdown)?;
        if !workspace_root_owned(&state) {
            return Ok(());
        }

        // The reference unmounts the workspace's ephemeral mounts before removing the root, and
        // leaves the root alone when an unmount fails, because deleting through a live mount would
        // delete what is on the other side of it. Mount lifecycle is not ported yet, so a manifest
        // that declares one takes the same branch as a failed unmount.
        let mounted = state
            .manifest()
            .mount_targets()
            .is_ok_and(|targets| !targets.is_empty());
        if mounted {
            tracing::warn!(
                backend = UNIX_LOCAL_BACKEND_ID,
                "leaving the workspace root in place: it declares mounts, and unmounting is not \
                 implemented yet"
            );
            return Ok(());
        }

        let root = std::path::PathBuf::from(&state.manifest().root);
        if let Err(error) = std::fs::remove_dir_all(&root)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                backend = UNIX_LOCAL_BACKEND_ID,
                error = %error,
                "failed to remove the workspace root"
            );
        }
        Ok(())
    }
}
