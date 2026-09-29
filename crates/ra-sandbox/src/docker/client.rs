//! Making, resuming and releasing containers.
//!
//! The client is the only thing that decides a container exists, and therefore the only thing
//! allowed to remove one. It also owns the volumes generated for driver-backed mounts, which are
//! named after the session so that two sessions never share one and a delete can find them again.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::sandbox::{
    CreateRequest, Dependencies, EnvValueResolver, ErrorCode, InvalidSessionStatePayload, Manifest,
    ManifestRegistries, OpName, SandboxClient, SandboxConcurrencyLimits, SandboxError,
    SandboxResult, SandboxSession, SandboxSessionState, SnapshotSpec, TypeRegistry,
    UnresolvableEnvValues, invalid_state_payload, parse_session_state_for_backend,
    render_session_state_for_storage, resolve_snapshot,
};
use serde_json::Value;
use uuid::Uuid;

use crate::instrumentation::{Instrumentation, InstrumentedSession};
use crate::mounts::{BuiltinMountLifecycle, MountLifecycle};
use crate::snapshot::{BuiltinSnapshotStore, SnapshotStore};

use super::api::{DockerApi, DockerApiError, DockerApiErrorKind};
use super::container::{
    ContainerShape, assert_existing_container_labels_match,
    assert_existing_container_network_configuration_matches,
    assert_existing_container_path_grants_match, container_create_spec,
    docker_volume_names_for_manifest, parse_repository_tag, validate_docker_path_grants,
};
use super::session::DockerSandboxSession;
use super::{
    CONTAINER_ID_FIELD, DOCKER_BACKEND_ID, DockerNetworkMode, DockerSandboxClientOptions,
    DockerStateFields,
};

/// A daemon failure, as a sandbox error.
///
/// The reference lets `docker-py` exceptions out of its client as they are. Here every failure is a
/// [`SandboxError`], so a daemon failure travels as a transport error with the daemon's answer as
/// its cause, retryable when the daemon's status says it may pass.
fn daemon_failure(op: OpName, error: DockerApiError) -> SandboxError {
    let retryable = match error.kind() {
        DockerApiErrorKind::NotFound => Some(false),
        _ => error
            .status_code()
            .map(|status| matches!(status, 500 | 502 | 503 | 504)),
    };
    SandboxError::new(
        ErrorCode::ExecTransportError,
        op,
        "docker daemon request failed",
    )
    .with_context("backend", DOCKER_BACKEND_ID)
    .with_retryable(retryable)
    .with_cause(error)
}

/// Replaces a failure that may carry mount authority, when the manifest involved has some.
///
/// The reference's `@redact_mount_error_data` on `create`, `resume` and `delete`.
fn redact_for(manifest: &Manifest, error: SandboxError) -> SandboxError {
    crate::mounts::redact_for_manifest(manifest, error)
}

/// What to remove when acquiring a container did not produce a session.
///
/// Run explicitly on failure inside the acquisition task. If its result is never received, the
/// guard schedules the same removal on the runtime, as the reference's `BaseException` handler
/// cleans up after cancellation. The task keeps daemon acquisition and failure cleanup alive
/// when the caller drops its wait.
struct FailedCreateCleanup {
    api: Arc<dyn DockerApi>,
    container_id: Option<String>,
    volume_names: Vec<String>,
    armed: bool,
}

impl FailedCreateCleanup {
    fn new(api: Arc<dyn DockerApi>, volume_names: Vec<String>) -> Self {
        Self {
            api,
            container_id: None,
            volume_names,
            armed: true,
        }
    }

    /// The session exists now, and owns what was acquired.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Removes what was acquired, best effort, and disarms.
    async fn run(mut self) {
        self.armed = false;
        remove_acquired(
            self.api.as_ref(),
            self.container_id.take(),
            std::mem::take(&mut self.volume_names),
        )
        .await;
    }
}

impl Drop for FailedCreateCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let api = Arc::clone(&self.api);
        let container_id = self.container_id.take();
        let volume_names = std::mem::take(&mut self.volume_names);
        runtime.spawn(async move {
            remove_acquired(api.as_ref(), container_id, volume_names).await;
        });
    }
}

/// Force-removes a container and removes volumes, ignoring every failure.
async fn remove_acquired(api: &dyn DockerApi, container_id: Option<String>, volumes: Vec<String>) {
    if let Some(container_id) = container_id {
        let _ = api.remove_container(&container_id, true).await;
    }
    for volume in volumes {
        let _ = api.remove_volume(&volume).await;
    }
}

/// Keeps ownership attached to a completed acquisition until its caller receives the result.
/// Dropping a detached task's result therefore removes newly acquired resources as well.
struct AcquiredSession {
    session: DockerSandboxSession,
    cleanup: Option<FailedCreateCleanup>,
}

/// The reference's Docker calls run synchronously, so cancellation cannot interrupt resource
/// acquisition. Rust's daemon calls yield: let a task finish the request and retain its guard in
/// the result. Dropping this wait detaches the task; its discarded result then triggers cleanup.
async fn deliver_acquired(
    task: tokio::task::JoinHandle<SandboxResult<AcquiredSession>>,
    instrumentation: Arc<Instrumentation>,
) -> SandboxResult<Box<dyn SandboxSession>> {
    let mut acquired = task.await.map_err(|_| {
        SandboxError::new(
            ErrorCode::ExecTransportError,
            OpName::Start,
            "docker resource acquisition task did not complete",
        )
    })??;
    // Wrapped before the acquisition is disarmed: a sink that cannot bind means no session is
    // handed out, and what was acquired for it is removed rather than left behind.
    let manifest = acquired.session.state().manifest().clone();
    let session = InstrumentedSession::new(Arc::new(acquired.session), Some(instrumentation), None)
        .map_err(|error| redact_for(&manifest, error))?;
    if let Some(cleanup) = &mut acquired.cleanup {
        cleanup.disarm();
    }
    Ok(Box::new(session))
}

/// Makes containers, and releases the ones it made.
pub struct DockerSandboxClient {
    api: Arc<dyn DockerApi>,
    env_values: Arc<dyn EnvValueResolver>,
    concurrency_limits: SandboxConcurrencyLimits,
    snapshot_store: Arc<dyn SnapshotStore>,
    mount_lifecycle: Arc<dyn MountLifecycle>,
    dependencies: Option<Dependencies>,
    instrumentation: Arc<Instrumentation>,
}

impl std::fmt::Debug for DockerSandboxClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerSandboxClient")
            .finish_non_exhaustive()
    }
}

impl DockerSandboxClient {
    /// Copies the client's configuration for an acquisition that may outlive its caller.
    fn for_acquisition(&self) -> Self {
        Self {
            api: Arc::clone(&self.api),
            env_values: Arc::clone(&self.env_values),
            concurrency_limits: self.concurrency_limits,
            snapshot_store: Arc::clone(&self.snapshot_store),
            mount_lifecycle: Arc::clone(&self.mount_lifecycle),
            dependencies: self.dependencies.as_ref().map(Dependencies::clone_bindings),
            instrumentation: Arc::clone(&self.instrumentation),
        }
    }

    /// Makes containers through `api`.
    #[must_use]
    pub fn new(api: Arc<dyn DockerApi>) -> Self {
        Self {
            api,
            env_values: Arc::new(UnresolvableEnvValues),
            concurrency_limits: SandboxConcurrencyLimits::default(),
            snapshot_store: Arc::new(BuiltinSnapshotStore),
            mount_lifecycle: Arc::new(BuiltinMountLifecycle),
            dependencies: None,
            instrumentation: Arc::new(Instrumentation::new()),
        }
    }

    /// Fetches a manifest's non-literal environment values through `resolver`.
    #[must_use]
    pub fn with_env_value_resolver(mut self, resolver: Arc<dyn EnvValueResolver>) -> Self {
        self.env_values = resolver;
        self
    }

    /// Reads and writes snapshots through `store` in every session this client makes.
    #[must_use]
    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = store;
        self
    }

    /// Attaches in-container mounts with `lifecycle` in every session this client makes.
    #[must_use]
    pub fn with_mount_lifecycle(mut self, lifecycle: Arc<dyn MountLifecycle>) -> Self {
        self.mount_lifecycle = lifecycle;
        self
    }

    /// Gives every session this client makes its own copy of `dependencies`.
    #[must_use]
    pub fn with_dependencies(mut self, dependencies: Dependencies) -> Self {
        self.dependencies = Some(dependencies);
        self
    }

    /// Delivers the audit events of every session this client makes through `instrumentation`.
    ///
    /// Every session comes back wrapped whether or not this is called, as the reference's do; the
    /// default instrumentation has no sinks.
    #[must_use]
    pub fn with_instrumentation(mut self, instrumentation: Arc<Instrumentation>) -> Self {
        self.instrumentation = instrumentation;
        self
    }

    /// Paces manifest application in every session this client makes.
    #[must_use]
    pub const fn with_concurrency_limits(mut self, limits: SandboxConcurrencyLimits) -> Self {
        self.concurrency_limits = limits;
        self
    }

    /// The Docker operations this client runs through.
    #[must_use]
    pub fn api(&self) -> &Arc<dyn DockerApi> {
        &self.api
    }

    /// Whether an image is present on the daemon.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure, other than "no such image".
    pub async fn image_exists(&self, image: &str) -> SandboxResult<bool> {
        match self.api.inspect_image(image).await {
            Ok(()) => Ok(true),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(daemon_failure(OpName::Start, error)),
        }
    }

    /// A container's attribute document, or `None` when it is gone.
    ///
    /// An empty id is taken as gone without asking: it is what a scrubbed state carries, and it
    /// names no container.
    ///
    /// # Errors
    ///
    /// Returns the daemon's failure, other than "no such container".
    pub async fn get_container(&self, container_id: &str) -> SandboxResult<Option<Value>> {
        if container_id.is_empty() {
            return Ok(None);
        }
        match self.api.inspect_container(container_id).await {
            Ok(attrs) => Ok(Some(attrs)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(daemon_failure(OpName::Start, error)),
        }
    }

    /// Creates a container, pulling its image first when the daemon does not have it.
    ///
    /// The reference's `_create_container`: path grants are checked before the image is looked up,
    /// so a configuration that cannot be honoured costs no pull. Returns the container's id.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] for grants a container cannot bind, or an image
    /// still missing after the pull; the manifest's failure to resolve its mounts or environment;
    /// and the daemon's failure to pull or create.
    pub async fn create_container(
        &self,
        image: &str,
        manifest: Option<&Manifest>,
        exposed_ports: &[u16],
        network_mode: Option<DockerNetworkMode>,
        session_id: Option<Uuid>,
        labels: &BTreeMap<String, String>,
    ) -> SandboxResult<String> {
        if let Some(manifest) = manifest {
            validate_docker_path_grants(manifest)?;
        }
        if !self.image_exists(image).await? {
            let (repository, tag) = parse_repository_tag(image);
            self.api
                .pull_image(&repository, tag.as_deref())
                .await
                .map_err(|error| daemon_failure(OpName::Start, error))?;
        }
        if !self.image_exists(image).await? {
            return Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Start,
                format!("docker image `{image}` is still missing after pulling it"),
            )
            .with_context("backend", DOCKER_BACKEND_ID));
        }

        let environment = match manifest {
            Some(manifest) => Some(
                manifest
                    .environment
                    .resolve(self.env_values.as_ref())
                    .await?,
            ),
            None => None,
        };
        let spec = container_create_spec(
            &ContainerShape {
                image,
                manifest,
                exposed_ports,
                network_mode,
                session_id,
                labels,
            },
            environment,
        )?;
        self.api
            .create_container(&spec)
            .await
            .map_err(|error| daemon_failure(OpName::Start, error))
    }

    /// Builds a session over a state this client already vetted.
    fn open(
        &self,
        state: SandboxSessionState,
        container_status: &str,
    ) -> SandboxResult<DockerSandboxSession> {
        let session = DockerSandboxSession::new(Arc::clone(&self.api), state)?
            .with_concurrency_limits(self.concurrency_limits)
            .with_snapshot_store(Arc::clone(&self.snapshot_store))
            .with_mount_lifecycle(Arc::clone(&self.mount_lifecycle))
            .with_container_status(container_status);
        session.set_dependencies(
            self.dependencies
                .as_ref()
                .map(|template| Arc::new(template.clone_bindings())),
        );
        Ok(session)
    }

    /// Refuses a state another backend wrote.
    fn assert_own_state(state: &SandboxSessionState, operation: OpName) -> SandboxResult<()> {
        if state.state_type() == DOCKER_BACKEND_ID {
            return Ok(());
        }
        Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            operation,
            format!(
                "a `{}` session state cannot be handled by the `{DOCKER_BACKEND_ID}` backend",
                state.state_type()
            ),
        )
        .with_context("state_type", state.state_type())
        .with_context("backend", DOCKER_BACKEND_ID))
    }

    /// The part of `create` that acquires resources, cleaned up by the guard on failure.
    async fn create_session(
        &self,
        request: CreateRequest,
        options: DockerSandboxClientOptions,
        manifest: Manifest,
        session_id: Uuid,
        cleanup: &mut FailedCreateCleanup,
    ) -> SandboxResult<DockerSandboxSession> {
        let container_id = self
            .create_container(
                options.image(),
                Some(&manifest),
                options.exposed_ports(),
                options.network_mode(),
                Some(session_id),
                options.labels(),
            )
            .await?;
        cleanup.container_id = Some(container_id.clone());
        self.api
            .start_container(&container_id)
            .await
            .map_err(|error| daemon_failure(OpName::Start, error))?;
        let snapshot =
            resolve_snapshot(request.snapshot(), &session_id.to_string()).map_err(|error| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    error.to_string(),
                )
                .with_cause(error)
            })?;
        let state = SandboxSessionState::new(DOCKER_BACKEND_ID, snapshot, manifest)
            .with_session_id(session_id)
            .with_exposed_ports(options.exposed_ports().iter().copied())
            .map_err(|error| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    error.to_string(),
                )
            })?;
        let state = DockerStateFields {
            image: options.image().to_owned(),
            container_id,
            network_mode: options.network_mode(),
            labels: options.labels().clone(),
        }
        .apply(state);
        self.open(state, "created")
    }

    /// The part of `resume` that may acquire a replacement, cleaned up by the guard on failure.
    async fn resume_session(
        &self,
        state: SandboxSessionState,
        fields: DockerStateFields,
        replacement: Option<(Uuid, &mut FailedCreateCleanup)>,
        existing_status: Option<String>,
    ) -> SandboxResult<DockerSandboxSession> {
        let reused_existing_container = replacement.is_none();
        let (state, status) = match replacement {
            None => (state, existing_status.unwrap_or_default()),
            Some((session_id, cleanup)) => {
                let container_id = self
                    .create_container(
                        &fields.image,
                        Some(state.manifest()),
                        state.exposed_ports(),
                        fields.network_mode,
                        Some(session_id),
                        &fields.labels,
                    )
                    .await?;
                cleanup.container_id = Some(container_id.clone());
                let state = state
                    .with_session_id(session_id)
                    .with_field(CONTAINER_ID_FIELD, container_id)
                    .with_workspace_root_ready(false);
                (state, "created".to_owned())
            }
        };
        let session = self.open(state, &status)?;
        session.mark_resumed(reused_existing_container);
        Ok(session)
    }
}

impl DockerSandboxClient {
    async fn create_owned(&self, request: CreateRequest) -> SandboxResult<AcquiredSession> {
        self.check_options(request.options())?;
        let options = match request.options() {
            Some(payload) => DockerSandboxClientOptions::from_payload(payload)?,
            None => {
                return Err(SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::Start,
                    "the `docker` backend needs options naming an image",
                ));
            }
        };
        let manifest = request.manifest().cloned().unwrap_or_default();
        let outcome = async {
            self.validate_manifest_for_create(&manifest)?;
            validate_docker_path_grants(&manifest)?;
            let session_id = Uuid::new_v4();
            let volume_names = docker_volume_names_for_manifest(&manifest, Some(session_id))?;
            let mut cleanup = FailedCreateCleanup::new(Arc::clone(&self.api), volume_names);
            match self
                .create_session(request, options, manifest.clone(), session_id, &mut cleanup)
                .await
            {
                Ok(session) => Ok(AcquiredSession {
                    session,
                    cleanup: Some(cleanup),
                }),
                Err(error) => {
                    cleanup.run().await;
                    Err(error)
                }
            }
        }
        .await;
        outcome.map_err(|error| redact_for(&manifest, error))
    }

    /// Reconnects to the container a state names, or replaces it.
    ///
    /// The existing container is reused only when nothing about the state asks for a fresh one:
    /// mount authority that was rebound from a trusted manifest, or that the manifest carries now,
    /// has to be attached by a container created with it. A reused container must still bind exactly
    /// the trusted grants, be as isolated as the state says, and carry its labels.
    ///
    /// A replacement gets a new session id when authority is involved, so its generated volumes are
    /// new ones rather than the volumes the state's own id selects — those are never removed here.
    /// A replacement starts with its workspace not ready and its accounts not provisioned.
    async fn resume_owned(&self, state: SandboxSessionState) -> SandboxResult<AcquiredSession> {
        Self::assert_own_state(&state, OpName::Start)?;
        let manifest = state.manifest().clone();
        let outcome = async {
            state.assert_path_grants_rebound()?;
            validate_docker_path_grants(state.manifest())?;
            let fields = DockerStateFields::read(&state)?;
            let configured_authority =
                ra_core::sandbox::manifest_has_configured_mount_authority(state.manifest());
            let requires_fresh_resource = state.mount_authority_rebound() || configured_authority;
            let existing = if requires_fresh_resource {
                None
            } else {
                self.get_container(&fields.container_id).await?
            };
            if let Some(attrs) = &existing {
                assert_existing_container_path_grants_match(attrs, state.manifest())?;
                assert_existing_container_network_configuration_matches(
                    attrs,
                    fields.network_mode,
                )?;
                assert_existing_container_labels_match(attrs, &fields.labels)?;
            }

            if let Some(attrs) = existing {
                let status = attrs
                    .get("State")
                    .and_then(|state| state.get("Status"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let session = self
                    .resume_session(state, fields, None, Some(status))
                    .await?;
                return Ok(AcquiredSession {
                    session,
                    cleanup: None,
                });
            }

            // Owned from here on: a failure removes the replacement and its volumes, never the
            // container or volumes the state itself names.
            let replacement_session_id = if requires_fresh_resource {
                Uuid::new_v4()
            } else {
                state.session_id()
            };
            let volume_names =
                docker_volume_names_for_manifest(state.manifest(), Some(replacement_session_id))?;
            let mut cleanup = FailedCreateCleanup::new(Arc::clone(&self.api), volume_names);
            match self
                .resume_session(
                    state,
                    fields,
                    Some((replacement_session_id, &mut cleanup)),
                    None,
                )
                .await
            {
                Ok(session) => Ok(AcquiredSession {
                    session,
                    cleanup: Some(cleanup),
                }),
                Err(error) => {
                    cleanup.run().await;
                    Err(error)
                }
            }
        }
        .await;
        outcome.map_err(|error| redact_for(&manifest, error))
    }
}

#[async_trait]
impl SandboxClient for DockerSandboxClient {
    fn backend_id(&self) -> &str {
        DOCKER_BACKEND_ID
    }

    /// The directory the reference's runner uses when a run names no snapshot.
    ///
    /// A container this client created is removed when its run ends, so without a snapshot a
    /// paused run would come back to an empty workspace.
    fn default_snapshot_spec(&self) -> SandboxResult<SnapshotSpec> {
        crate::snapshot::defaults::resolve_current_default_local_snapshot_spec()
    }

    /// Creates and starts a container, and opens a session over it.
    ///
    /// Everything acquired on the way — the container, and the volumes Docker creates for
    /// driver-backed mounts when it creates the container — is removed again when this does not end
    /// in a session, including when the caller stops waiting for it.
    async fn create(&self, request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>> {
        let client = self.for_acquisition();
        deliver_acquired(
            tokio::spawn(async move { client.create_owned(request).await }),
            Arc::clone(&self.instrumentation),
        )
        .await
    }

    /// Reconnects to a vetted existing container or acquires a replacement that remains owned
    /// until the caller receives it, even if the caller stops waiting during creation.
    async fn resume(&self, state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>> {
        let client = self.for_acquisition();
        deliver_acquired(
            tokio::spawn(async move { client.resume_owned(state).await }),
            Arc::clone(&self.instrumentation),
        )
        .await
    }

    /// Shuts the session down, removes its container, and removes its generated volumes.
    ///
    /// Every step runs even when an earlier one failed, so one failure does not leave the rest
    /// behind; the first failure is what is reported. Something already gone is not a failure.
    async fn delete(&self, session: &dyn SandboxSession) -> SandboxResult<()> {
        let state = session.state();
        Self::assert_own_state(&state, OpName::Shutdown)?;
        let manifest = state.manifest().clone();
        let outcome = async {
            let volume_names =
                docker_volume_names_for_manifest(state.manifest(), Some(state.session_id()))?;
            let mut first_error: Option<SandboxError> = None;
            if let Err(error) = session.shutdown().await {
                first_error = Some(error);
            }

            let container_id = state
                .field(CONTAINER_ID_FIELD)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            match self.get_container(&container_id).await {
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
                Ok(Some(_)) => match self.api.remove_container(&container_id, false).await {
                    Ok(()) => {}
                    Err(error) if error.is_not_found() => {}
                    Err(error) => {
                        first_error.get_or_insert(daemon_failure(OpName::Shutdown, error));
                    }
                },
            }

            for volume in volume_names {
                match self.api.inspect_volume(&volume).await {
                    Ok(()) => {}
                    Err(error) if error.is_not_found() => continue,
                    Err(error) => {
                        first_error.get_or_insert(daemon_failure(OpName::Shutdown, error));
                        continue;
                    }
                }
                match self.api.remove_volume(&volume).await {
                    Ok(()) => {}
                    Err(error) if error.is_not_found() => {}
                    Err(error) => {
                        first_error.get_or_insert(daemon_failure(OpName::Shutdown, error));
                    }
                }
            }
            first_error.map_or(Ok(()), Err)
        }
        .await;
        outcome.map_err(|error| redact_for(&manifest, error))
    }

    /// Renders a state for storage, dropping the container's identity when mount authority was.
    ///
    /// A state whose mount credentials were stripped cannot reconnect to the container those
    /// credentials were attached to: whoever holds the stored state would otherwise reach a live
    /// mount without the authority it needs. So the container id is emptied, the session id is
    /// replaced by one derived from it — the same derivation as the reference's, so a replacement's
    /// volumes are named alike wherever it is resumed — and the workspace is marked not ready.
    fn serialize_session_state(
        &self,
        state: &SandboxSessionState,
    ) -> SandboxResult<serde_json::Value> {
        let state = DockerStateFields::read(state)?.apply(state.clone());
        render_session_state_for_storage(&state)
    }

    /// Reads a stored state back, refusing one whose Docker fields do not read.
    ///
    /// Refused with the same fixed message as any unreadable payload — a network mode other than
    /// `none`, or published ports without a network, included — before anything asks the daemon.
    fn deserialize_session_state(
        &self,
        payload: serde_json::Value,
        snapshots: &TypeRegistry,
        manifests: &ManifestRegistries,
    ) -> SandboxResult<SandboxSessionState> {
        let state =
            parse_session_state_for_backend(DOCKER_BACKEND_ID, payload, snapshots, manifests)?;
        let fields = DockerStateFields::read(&state)
            .map_err(|_| invalid_state_payload(InvalidSessionStatePayload::Invalid))?;
        Ok(fields.apply(state))
    }
}
