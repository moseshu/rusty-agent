//! Which session each sandbox agent in a run uses, who owns it, and what resumes it next time.
//!
//! Ported from the reference's `sandbox/runtime_session_manager.py`. The decisions worth reading
//! before changing anything here:
//!
//! - **Ownership decides cleanup.** A session the host handed in is borrowed: the run starts it if
//!   it is not running and otherwise leaves it alone, and never stops, shuts down or deletes it. A
//!   session the run created or resumed is owned, and is cleaned up when the run ends.
//! - **Cleanup order is the reference's, failures included.** Pre-stop callbacks, then stop — only
//!   when the callbacks succeeded, now or on any earlier attempt — then shutdown, delete and the
//!   dependency close, each attempted whatever happened before it. The first failure is the one
//!   reported.
//! - **A failed cleanup produces no resume state.** Every agent's session is cleaned up even after
//!   one fails, and only when all of them succeeded is a new resume state written.
//!
//! # Where this differs from the reference
//!
//! - **Resume entries are keyed by agent id.** The reference identifies agents by object identity
//!   and derives display-name keys with `#2` suffixes to tell apart agents that share a name. An
//!   agent here already carries a stable [`AgentId`] its registry refuses to duplicate, so that is
//!   the key, and the suffix allocation has nothing to do. The name is still written beside each
//!   entry, as the reference writes it.
//! - **Only the current payload shape is read.** The reference also reads the shapes its earlier
//!   releases wrote — a payload keyed by process-local object id, or by agent name without an
//!   entry map. No release of this framework wrote those.
//! - **The run-as branch of the preparation cache is not here**: an agent's configuration cannot
//!   change between turns, so the cached preparation is keyed on the session alone.

use std::{collections::BTreeMap, sync::Arc};

use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Result},
    sandbox::{
        CreateRequest, Entry, Manifest, PosixPath, SandboxAgentConfig, SandboxAgentRunLease,
        SandboxClient, SandboxError, SandboxPathGrant, SandboxResult, SandboxSession,
        SandboxSessionState, manifest_with_run_as_user, process_manifest, resolve_workspace_path,
        validate_manifest_mount_credential_boundaries,
    },
};
use serde_json::{Map, Value};
use tracing::Instrument;

use super::{SandboxRunConfig, sandbox_error};

/// One session a sandbox agent in this run uses.
struct RunSession {
    session: Arc<dyn SandboxSession>,
    /// The client that made or resumed it, which is who deletes it; `None` for a borrowed one.
    client: Option<Arc<dyn SandboxClient>>,
    owns_session: bool,
    started: bool,
    cleaned: bool,
}

impl RunSession {
    /// Starts the session unless it is already running.
    ///
    /// A borrowed session that is running is taken as it is; an owned one is started once and
    /// restarted only if it stopped since.
    async fn ensure_started(&mut self) -> SandboxResult<()> {
        if self.started && self.session.running().await? {
            return Ok(());
        }
        if !self.owns_session && self.session.running().await? {
            self.started = true;
            return Ok(());
        }
        self.session.start().await?;
        self.started = true;
        Ok(())
    }

    /// Cleans up an owned session, once. A borrowed one is left alone.
    async fn cleanup(&mut self) -> SandboxResult<()> {
        if !self.owns_session || self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        let mut first_error = self.session.run_pre_stop_hooks().await.err();
        // Stop persists the workspace, and a failed pre-stop callback is exactly the signal that it
        // must not be persisted — on this attempt or any earlier one, which is why the sticky flag
        // is read rather than only this call's result.
        if first_error.is_none()
            && !self.session.pre_stop_hooks_failed()
            && let Err(error) = self.session.stop().await
        {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.session.shutdown().await {
            first_error.get_or_insert(error);
        }
        if let Some(client) = &self.client
            && let Err(error) = client.delete(self.session.as_ref()).await
        {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.session.close_dependencies().await {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// A sandbox agent this run has claimed.
struct AcquiredAgent {
    id: AgentId,
    name: String,
    _lease: SandboxAgentRunLease,
}

/// What a capability changed about a live session's manifest.
struct LiveSessionManifestUpdate {
    processed_manifest: Option<Manifest>,
    entries_to_apply: Vec<(PosixPath, Entry)>,
}

/// The sessions one run's sandbox agents use.
pub(crate) struct SessionManager {
    config: SandboxRunConfig,
    /// The resume payload the run was continued with, as its checkpoint carried it.
    resumed: Option<Value>,
    /// In the order the agents first ran, which is the order they are cleaned up in.
    resources: Vec<(AgentId, RunSession)>,
    current_agent: Option<AgentId>,
    acquired: Vec<AcquiredAgent>,
    /// The key each agent's resumed entry was read from.
    resume_source_keys: BTreeMap<AgentId, String>,
}

impl SessionManager {
    pub(crate) fn new(config: SandboxRunConfig, resumed: Option<Value>) -> Self {
        Self {
            config,
            resumed,
            resources: Vec::new(),
            current_agent: None,
            acquired: Vec::new(),
            resume_source_keys: BTreeMap::new(),
        }
    }

    /// Claims a sandbox agent for this run, once.
    pub(crate) fn acquire_agent(
        &mut self,
        agent: &AgentSpec,
        sandbox: &SandboxAgentConfig,
    ) -> Result<()> {
        if self
            .acquired
            .iter()
            .any(|acquired| &acquired.id == agent.id())
        {
            return Ok(());
        }
        let lease = sandbox.acquire_run(agent.name())?;
        self.acquired.push(AcquiredAgent {
            id: agent.id().clone(),
            name: agent.name().to_owned(),
            _lease: lease,
        });
        Ok(())
    }

    /// The session `agent` runs against, created, resumed or borrowed the first time it is asked
    /// for, and started if it is not running.
    pub(crate) async fn ensure_session(
        &mut self,
        agent: &AgentSpec,
        sandbox: &SandboxAgentConfig,
    ) -> Result<Arc<dyn SandboxSession>> {
        if !self.resources.iter().any(|(id, _)| id == agent.id()) {
            let resources = self.create_resources(agent, sandbox).await?;
            self.resources.push((agent.id().clone(), resources));
        }
        self.current_agent = Some(agent.id().clone());
        let resources = self
            .resources
            .iter_mut()
            .find(|(id, _)| id == agent.id())
            .map(|(_, resources)| resources)
            .ok_or_else(|| Error::caller("sandbox resources vanished while being prepared"))?;
        resources.ensure_started().await.map_err(sandbox_error)?;
        Ok(Arc::clone(&resources.session))
    }

    /// Whether any agent has a session yet.
    pub(crate) fn has_sessions(&self) -> bool {
        !self.resources.is_empty()
    }

    /// Cleans up every owned session and returns what resumes them.
    ///
    /// Every session is attempted even after one fails; the first failure is returned and no
    /// resume state is produced, because a state describing sessions whose cleanup failed would
    /// resume a workspace nobody can vouch for. The run's claims are released either way.
    pub(crate) async fn cleanup(&mut self) -> Result<Option<Value>> {
        let mut first_error = None;
        for (_, resources) in &mut self.resources {
            if let Err(error) = resources.cleanup().await {
                first_error.get_or_insert(error);
            }
        }
        let result = match first_error {
            Some(error) => Err(sandbox_error(error)),
            None => self.serialize_resume_state(),
        };
        self.resources.clear();
        self.current_agent = None;
        self.acquired.clear();
        self.resume_source_keys.clear();
        result
    }

    /// What resumes this run's sessions, merged over what the run was continued with.
    ///
    /// A borrowed session is not the run's to resume, so a run that used one records nothing; a run
    /// that never prepared a sandbox agent passes on what it was given unchanged.
    pub(crate) fn serialize_resume_state(&self) -> Result<Option<Value>> {
        let existing = self.resumed.clone();
        if self.config.session().is_some() {
            return Ok(None);
        }
        let Some(current_id) = &self.current_agent else {
            return Ok(existing);
        };
        let Some(client) = self.config.client() else {
            return Ok(existing);
        };
        let Some((_, current)) = self.resources.iter().find(|(id, _)| id == current_id) else {
            return Ok(existing);
        };
        let Some(current_agent) = self.acquired.iter().find(|agent| &agent.id == current_id) else {
            return Ok(existing);
        };

        let mut sessions_by_agent = self.legacy_session_entries();
        for (agent_id, resources) in &self.resources {
            let Some(agent) = self.acquired.iter().find(|agent| &agent.id == agent_id) else {
                continue;
            };
            let key = resume_key(agent_id);
            if let Some(source) = self.resume_source_keys.get(agent_id)
                && source != &key
            {
                sessions_by_agent.remove(source);
            }
            let state = client
                .serialize_session_state(&resources.session.state())
                .map_err(sandbox_error)?;
            let mut entry = Map::new();
            entry.insert("agent_name".to_owned(), Value::from(agent.name.clone()));
            entry.insert("session_state".to_owned(), state);
            sessions_by_agent.insert(key, Value::Object(entry));
        }

        let mut payload = Map::new();
        payload.insert("backend_id".to_owned(), Value::from(client.backend_id()));
        payload.insert(
            "current_agent_key".to_owned(),
            Value::from(resume_key(current_id)),
        );
        payload.insert(
            "current_agent_name".to_owned(),
            Value::from(current_agent.name.clone()),
        );
        payload.insert(
            "session_state".to_owned(),
            client
                .serialize_session_state(&current.session.state())
                .map_err(sandbox_error)?,
        );
        payload.insert(
            "sessions_by_agent".to_owned(),
            Value::Object(sessions_by_agent),
        );
        Ok(Some(Value::Object(payload)))
    }

    /// The entries the run was continued with, kept for agents that did not run this time.
    fn legacy_session_entries(&self) -> Map<String, Value> {
        let Some(Value::Object(resumed)) = &self.resumed else {
            return Map::new();
        };
        if let Some(Value::Object(sessions)) = resumed.get("sessions_by_agent") {
            return sessions.clone();
        }
        let mut entries = Map::new();
        if let (Some(payload @ Value::Object(_)), Some(Value::String(key))) = (
            resumed.get("session_state"),
            resumed.get("current_agent_key"),
        ) {
            entries.insert(key.clone(), payload.clone());
        }
        entries
    }

    async fn create_resources(
        &mut self,
        agent: &AgentSpec,
        sandbox: &SandboxAgentConfig,
    ) -> Result<RunSession> {
        if let Some(session) = self.config.session().cloned() {
            return self.borrow_live_session(session, sandbox).await;
        }

        let client = Arc::clone(self.config.client().ok_or_else(|| {
            Error::config(
                "Sandbox execution requires `run_config.sandbox.client` unless a live session is \
                 provided",
            )
        })?);
        let mut explicit_state = self.config.session_state().cloned();
        if let Some(payload) = self.resume_payload_for_agent(client.as_ref(), agent)? {
            explicit_state = Some(
                client
                    .deserialize_session_state(
                        payload,
                        self.config.snapshot_registry(),
                        self.config.manifest_registries(),
                    )
                    .map_err(sandbox_error)?,
            );
        }
        match explicit_state {
            Some(state) => self.resume_session(client, state, agent, sandbox).await,
            None => self.create_session(client, agent, sandbox).await,
        }
    }

    /// Sets the run's limits on a session it is about to use.
    fn configure(&self, session: &dyn SandboxSession) {
        session.set_concurrency_limits(self.config.concurrency_limits());
        session.set_archive_limits(self.config.archive_limits());
    }

    /// Uses the session the host handed in, applying what the agent's capabilities change.
    async fn borrow_live_session(
        &self,
        session: Arc<dyn SandboxSession>,
        sandbox: &SandboxAgentConfig,
    ) -> Result<RunSession> {
        self.configure(session.as_ref());
        let update = process_live_session_manifest(sandbox, session.as_ref())
            .await
            .map_err(sandbox_error)?;
        if !update.entries_to_apply.is_empty() {
            session
                .apply_manifest_entries(update.entries_to_apply)
                .await
                .map_err(sandbox_error)?;
        }
        if let Some(manifest) = update.processed_manifest {
            session.replace_manifest(manifest).map_err(sandbox_error)?;
        }
        Ok(RunSession {
            session,
            client: None,
            owns_session: false,
            started: false,
            cleaned: false,
        })
    }

    /// Resumes `state`, after rebinding what was never persisted from the manifest trusted now.
    async fn resume_session(
        &self,
        client: Arc<dyn SandboxClient>,
        state: SandboxSessionState,
        agent: &AgentSpec,
        sandbox: &SandboxAgentConfig,
    ) -> Result<RunSession> {
        let trusted = self
            .config
            .manifest()
            .or_else(|| sandbox.default_manifest());
        let state = process_resumed_state_manifest(sandbox, state, trusted, client.backend_id())
            .map_err(sandbox_error)?;
        let session = client
            .resume(state)
            .instrument(tracing::info_span!(
                "sandbox.resume_session",
                agent.name = %agent.name(),
                backend_id = %client.backend_id(),
            ))
            .await
            .map_err(sandbox_error)?;
        let session: Arc<dyn SandboxSession> = Arc::from(session);
        self.configure(session.as_ref());
        Ok(RunSession {
            session,
            client: Some(client),
            owns_session: true,
            started: false,
            cleaned: false,
        })
    }

    /// Creates a fresh session from the configured manifest, or the agent's default.
    async fn create_session(
        &self,
        client: Arc<dyn SandboxClient>,
        agent: &AgentSpec,
        sandbox: &SandboxAgentConfig,
    ) -> Result<RunSession> {
        let run_as = sandbox.run_as();
        let mut manifest = self
            .config
            .manifest()
            .or_else(|| sandbox.default_manifest())
            .cloned();
        if manifest.is_some() || run_as.is_some() {
            let declared = manifest.unwrap_or_default();
            manifest = Some(
                process_manifest(sandbox.capabilities(), &declared, run_as)
                    .map_err(sandbox_error)?,
            );
        }

        let options = self.config.options().cloned();
        match &options {
            None if !client.supports_default_options() => {
                return Err(Error::config(
                    "Sandbox execution requires `run_config.sandbox.options` when creating a \
                     session",
                ));
            }
            Some(options) if options.type_name() != client.backend_id() => {
                return Err(Error::config(format!(
                    "sandbox.options type `{}` does not match selected sandbox client backend \
                     `{}`",
                    options.type_name(),
                    client.backend_id()
                )));
            }
            _ => {}
        }

        let request = CreateRequest {
            snapshot: Some(self.config.resolve_snapshot(client.as_ref())),
            manifest,
            options,
        };
        let session = client
            .create(request)
            .instrument(tracing::info_span!(
                "sandbox.create_session",
                agent.name = %agent.name(),
                backend_id = %client.backend_id(),
            ))
            .await
            .map_err(sandbox_error)?;
        let session: Arc<dyn SandboxSession> = Arc::from(session);
        self.configure(session.as_ref());
        ensure_session_manifest_has_run_as_user(session.as_ref(), run_as).map_err(sandbox_error)?;
        Ok(RunSession {
            session,
            client: Some(client),
            owns_session: true,
            started: false,
            cleaned: false,
        })
    }

    /// The checkpointed session state for `agent`, if the run was continued with one.
    ///
    /// "The run was continued" is not the same as "this agent has something to resume": a run
    /// whose checkpoint only knows other agents creates this one fresh.
    fn resume_payload_for_agent(
        &mut self,
        client: &dyn SandboxClient,
        agent: &AgentSpec,
    ) -> Result<Option<Value>> {
        let Some(resumed) = &self.resumed else {
            return Ok(None);
        };
        let resumed = resumed.as_object().ok_or_else(invalid_envelope)?;
        if resumed.get("backend_id").and_then(Value::as_str) != Some(client.backend_id()) {
            return Err(Error::config(
                "RunState sandbox backend does not match the configured sandbox client",
            ));
        }
        let key = resume_key(agent.id());

        if let Some(Value::Object(sessions)) = resumed.get("sessions_by_agent")
            && let Some(payload) = session_payload_from_entry(sessions.get(&key))?
        {
            self.resume_source_keys.insert(agent.id().clone(), key);
            return Ok(Some(payload));
        }

        let payload = match resumed.get("session_state") {
            None | Some(Value::Null) => return Ok(None),
            Some(payload @ Value::Object(_)) => payload.clone(),
            Some(_) => {
                return Err(Error::config(
                    "RunState sandbox payload is missing `session_state`",
                ));
            }
        };
        match resumed.get("current_agent_key") {
            Some(Value::String(current)) if current == &key => {
                self.resume_source_keys.insert(agent.id().clone(), key);
                Ok(Some(payload))
            }
            _ => Ok(None),
        }
    }
}

/// The key an agent's entry is filed under in the resume payload.
fn resume_key(agent: &AgentId) -> String {
    agent.as_str().to_owned()
}

fn invalid_envelope() -> Error {
    Error::config("RunState sandbox resume state has an invalid envelope")
}

/// The session state inside one `sessions_by_agent` entry.
fn session_payload_from_entry(entry: Option<&Value>) -> Result<Option<Value>> {
    match entry {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(fields)) => match fields.get("session_state") {
            Some(state @ Value::Object(_)) => Ok(Some(state.clone())),
            _ => Ok(Some(Value::Object(fields.clone()))),
        },
        Some(_) => Err(Error::config(
            "RunState sandbox payload has an invalid `sessions_by_agent` item",
        )),
    }
}

/// Adds the agent's user to a session a client created, when the client's manifest dropped it.
fn ensure_session_manifest_has_run_as_user(
    session: &dyn SandboxSession,
    run_as: Option<&ra_core::sandbox::User>,
) -> SandboxResult<()> {
    let manifest = session.state().manifest().clone();
    let processed = manifest_with_run_as_user(manifest.clone(), run_as);
    if processed != manifest {
        session.replace_manifest(processed)?;
    }
    Ok(())
}

/// The state a session is resumed from, with its manifest processed and its authority rebound.
///
/// Path grants and mount authority are never persisted, so both come from the manifest the host
/// trusts now: the run configuration's, or the agent's default.
fn process_resumed_state_manifest(
    sandbox: &SandboxAgentConfig,
    state: SandboxSessionState,
    trusted: Option<&Manifest>,
    provider_backend_id: &str,
) -> SandboxResult<SandboxSessionState> {
    let mut resume_manifest = state.manifest().clone();
    if !state.path_grants_require_rebind().is_empty()
        && let Some(trusted) = trusted
    {
        resume_manifest
            .extra_path_grants
            .clone_from(&trusted.extra_path_grants);
    }
    let processed = process_manifest(sandbox.capabilities(), &resume_manifest, sandbox.run_as())?;
    let state = state
        .with_manifest(processed.clone())
        .rebind_persisted_path_grants(Some(&processed))?;
    if !state.mount_authority_redacted() {
        return Ok(state);
    }
    let trusted = trusted
        .map(|trusted| process_manifest(sandbox.capabilities(), trusted, sandbox.run_as()))
        .transpose()?;
    state.rebind_persisted_mount_authority(trusted.as_ref(), provider_backend_id)
}

/// What the agent's capabilities change about a session the host handed in.
///
/// A stopped session takes any change, since starting it materializes the whole manifest. A
/// running one takes only what can be added without undoing anything: new entries and changed
/// ones, never a removal, a replaced entry type, a mount, or a different root, environment or set
/// of accounts.
async fn process_live_session_manifest(
    sandbox: &SandboxAgentConfig,
    session: &dyn SandboxSession,
) -> SandboxResult<LiveSessionManifestUpdate> {
    let state = session.state();
    let current = state.manifest();
    let processed = process_manifest(sandbox.capabilities(), current, sandbox.run_as())?;
    if &processed == current {
        validate_manifest_mount_credential_boundaries(current, Some(state.state_type()))?;
        let running = session.running().await?;
        session
            .validate_manifest_application(current, running)
            .await?;
        return Ok(LiveSessionManifestUpdate {
            processed_manifest: None,
            entries_to_apply: Vec::new(),
        });
    }

    validate_live_session_host_path_grants(current, &processed)?;
    validate_manifest_mount_credential_boundaries(&processed, Some(state.state_type()))?;
    let running = session.running().await?;
    session
        .validate_manifest_application(&processed, running)
        .await?;

    let mut entries_to_apply = Vec::new();
    if running {
        validate_running_live_session_manifest_update(current, &processed)?;
        for (relative, entry) in
            diff_live_session_entries(&current.entries, &processed.entries, &PosixPath::coerce(""))?
        {
            entries_to_apply.push((
                resolve_workspace_path(&processed.root, relative.as_str())?,
                entry,
            ));
        }
    }
    Ok(LiveSessionManifestUpdate {
        processed_manifest: Some(processed),
        entries_to_apply,
    })
}

fn live_session_refusal(message: impl Into<String>) -> SandboxError {
    SandboxError::new(
        ra_core::sandbox::ErrorCode::SandboxConfigInvalid,
        ra_core::sandbox::OpName::Start,
        message,
    )
}

/// The grants that mount a host path, as the part of a manifest a capability may not change on a
/// session it was handed.
fn host_path_grant_topology(manifest: &Manifest) -> Vec<(&str, bool, Option<&str>)> {
    let mounted: Vec<&str> = manifest
        .extra_path_grants
        .iter()
        .filter(|grant| grant.host_path().is_some())
        .map(SandboxPathGrant::path)
        .collect();
    manifest
        .extra_path_grants
        .iter()
        .filter(|grant| mounted.contains(&grant.path()))
        .map(|grant| (grant.path(), grant.is_read_only(), grant.host_path()))
        .collect()
}

fn validate_live_session_host_path_grants(
    current: &Manifest,
    processed: &Manifest,
) -> SandboxResult<()> {
    if host_path_grant_topology(current) != host_path_grant_topology(processed) {
        return Err(live_session_refusal(
            "Injected sandbox sessions do not support capability changes to host-backed \
             `manifest.extra_path_grants`; use a fresh session or a session_state resume flow.",
        ));
    }
    Ok(())
}

fn validate_running_live_session_manifest_update(
    current: &Manifest,
    processed: &Manifest,
) -> SandboxResult<()> {
    if processed.root != current.root {
        return Err(live_session_refusal(
            "Running injected sandbox sessions do not support capability changes to \
             `manifest.root`; use a fresh session or a session_state resume flow.",
        ));
    }
    if processed.environment != current.environment {
        return Err(live_session_refusal(
            "Running injected sandbox sessions do not support capability changes to \
             `manifest.environment`; use a fresh session or a session_state resume flow.",
        ));
    }
    if processed.users != current.users || processed.groups != current.groups {
        return Err(live_session_refusal(
            "Running injected sandbox sessions do not support capability changes to \
             `manifest.users` or `manifest.groups`; use a fresh session or a session_state \
             resume flow.",
        ));
    }
    Ok(())
}

/// The entries a running session needs written for `processed` to hold, relative to `parent`.
fn diff_live_session_entries(
    current: &BTreeMap<String, Entry>,
    processed: &BTreeMap<String, Entry>,
    parent: &PosixPath,
) -> SandboxResult<Vec<(PosixPath, Entry)>> {
    let current_by_name: BTreeMap<PosixPath, &Entry> = current
        .iter()
        .map(|(name, entry)| (PosixPath::coerce(name), entry))
        .collect();
    let processed_by_name: BTreeMap<PosixPath, &Entry> = processed
        .iter()
        .map(|(name, entry)| (PosixPath::coerce(name), entry))
        .collect();

    let removed: Vec<String> = current_by_name
        .keys()
        .filter(|name| !processed_by_name.contains_key(*name))
        .map(|name| parent.join(name.as_str()).as_str().to_owned())
        .collect();
    if !removed.is_empty() {
        return Err(live_session_refusal(format!(
            "Running injected sandbox sessions do not support removing manifest entries: {}.",
            removed.join(", ")
        )));
    }

    let mut entries_to_apply = Vec::new();
    for (name, processed_entry) in processed_by_name {
        let relative = parent.join(name.as_str());
        match current_by_name.get(&name) {
            None => {
                if entry_contains_mount(processed_entry) {
                    return Err(live_session_refusal(format!(
                        "Running injected sandbox sessions do not support capability-added mount \
                         entries at {}; use a fresh session or a session_state resume flow.",
                        relative.as_str()
                    )));
                }
                entries_to_apply.push((relative, processed_entry.clone()));
            }
            Some(current_entry) => {
                if let Some(delta) =
                    diff_live_session_entry(&relative, current_entry, processed_entry)?
                {
                    entries_to_apply.push((relative, delta));
                }
            }
        }
    }
    Ok(entries_to_apply)
}

/// What of one changed entry a running session needs written, or `None` when nothing changed.
fn diff_live_session_entry(
    relative: &PosixPath,
    current: &Entry,
    processed: &Entry,
) -> SandboxResult<Option<Entry>> {
    if current == processed {
        return Ok(None);
    }
    if current.entry_type() != processed.entry_type() || current.is_dir() != processed.is_dir() {
        return Err(live_session_refusal(format!(
            "Running injected sandbox sessions do not support replacing manifest entry types at \
             {}; use a fresh session or a session_state resume flow.",
            relative.as_str()
        )));
    }
    if matches!(current.content(), ra_core::sandbox::EntryContent::Mount(_)) {
        return Err(live_session_refusal(format!(
            "Running injected sandbox sessions do not support capability changes to mount \
             entries at {}; use a fresh session or a session_state resume flow.",
            relative.as_str()
        )));
    }
    if let (Some(current_children), Some(processed_children)) =
        (current.children(), processed.children())
    {
        let changed: BTreeMap<String, Entry> = diff_live_session_entries(
            current_children,
            processed_children,
            &PosixPath::coerce(""),
        )?
        .into_iter()
        .map(|(path, entry)| (path.as_str().to_owned(), entry))
        .collect();
        let metadata_changed = current.clone().with_children(BTreeMap::new())
            != processed.clone().with_children(BTreeMap::new());
        if !metadata_changed && changed.is_empty() {
            return Ok(None);
        }
        return Ok(Some(processed.clone().with_children(changed)));
    }
    Ok(Some(processed.clone()))
}

fn entry_contains_mount(entry: &Entry) -> bool {
    match entry.content() {
        ra_core::sandbox::EntryContent::Mount(_) => true,
        _ => entry
            .children()
            .is_some_and(|children| children.values().any(entry_contains_mount)),
    }
}
