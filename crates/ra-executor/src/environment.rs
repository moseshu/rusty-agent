// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Concrete filesystem environments and their trusted startup selection.
#![allow(
    unreachable_pub,
    reason = "Private implementation items are exposed only by the feature-gated test facade."
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "Preserve the source ownership and public argument contract."
)]
#![allow(
    clippy::unnecessary_wraps,
    reason = "Keep the source fallible API shared across platforms."
)]
#![allow(
    clippy::unused_async,
    reason = "Preserve the source asynchronous operation contract."
)]
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use crate::environment_toml::validate_environment_id;
use crate::{
    CapabilityRootsDiscoverParams, CapabilityRootsDiscoverResponse, EnvironmentInfo,
    ExecServerError, ExecServerRuntimeOptions, ExecutorFileSystem, LocalFileSystem,
    client::LazyRemoteExecServerClient,
    client_api::{DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT, ExecServerTransportParams},
    environment_provider::{
        DefaultEnvironmentProvider, EnvironmentDefault, EnvironmentProvider,
        EnvironmentProviderEntry, EnvironmentProviderSnapshot, normalize_exec_server_url,
    },
    remote_file_system::RemoteFileSystem,
};
use arc_swap::ArcSwapOption;
use ra_config::ScopedSkillsConfig;
use ra_executor_protocol::capabilities::SelectedCapabilityRoot;
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use std::result::Result;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;
mod connect_options;
mod discovery_recovery;
pub use connect_options::RemoteEnvironmentOptions;
pub const CODEX_EXEC_SERVER_URL_ENV_VAR: &str = "CODEX_EXEC_SERVER_URL";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentConnectionState {
    Connected,
    Disconnected,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionAttemptOutcome {
    Success,
    Failure,
    Cancelled,
}
#[derive(Debug)]
pub struct EnvironmentManager {
    default_environment: Option<String>,
    pub(super) environments: RwLock<HashMap<String, Arc<Environment>>>,
    local_environment: Option<Arc<Environment>>,
    local_runtime_paths: Option<ExecServerRuntimeOptions>,
    http_client_factory: HttpClientFactory,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentReadyInfo {
    pub selected_capability_roots: Vec<SelectedCapabilityRoot>,
}
pub const MAX_SELECTED_CAPABILITY_ROOTS: usize = 256;
pub const LOCAL_ENVIRONMENT_ID: &str = "local";
pub const REMOTE_ENVIRONMENT_ID: &str = "remote";
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentObservedStatus {
    Ready,
    Pending,
    Disconnected { error: String },
}
#[derive(Clone)]
pub struct Environment {
    skills: ScopedSkillsConfig,
    remote_client: Option<LazyRemoteExecServerClient>,
    ready_info: Arc<ArcSwapOption<EnvironmentReadyInfo>>,
    startup_task: Arc<Mutex<Option<AbortOnDropHandle<()>>>>,
    filesystem: Arc<dyn ExecutorFileSystem>,
    local_runtime_paths: Option<ExecServerRuntimeOptions>,
}
impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment").finish_non_exhaustive()
    }
}
impl EnvironmentManager {
    #[must_use]
    pub fn default_for_tests() -> Self {
        Self {
            default_environment: Some(LOCAL_ENVIRONMENT_ID.to_string()),
            environments: RwLock::new(HashMap::from([(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Arc::new(Environment::default_for_tests()),
            )])),
            local_environment: Some(Arc::new(Environment::default_for_tests())),
            local_runtime_paths: None,
            // Test-only construction has no application config from which to resolve proxy policy.
            http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        }
    }
    #[must_use]
    pub fn without_environments(http_client_factory: HttpClientFactory) -> Self {
        Self {
            default_environment: None,
            environments: RwLock::new(HashMap::new()),
            local_environment: None,
            local_runtime_paths: None,
            http_client_factory,
        }
    }
    pub async fn create_for_tests(
        exec_server_url: Option<String>,
        local_runtime_paths: Option<ExecServerRuntimeOptions>,
    ) -> Self {
        let provider = DefaultEnvironmentProvider::new(exec_server_url);
        match Self::from_snapshot(
            provider.snapshot_inner(),
            local_runtime_paths,
            // Test-only construction has no application config from which to resolve proxy policy.
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ) {
            Ok(manager) => manager,
            Err(err) => panic!("default provider should create valid environments: {err}"),
        }
    }
    pub(crate) fn from_snapshot(
        snapshot: EnvironmentProviderSnapshot,
        local_runtime_paths: Option<ExecServerRuntimeOptions>,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let mut environment_map =
            HashMap::with_capacity(environments.len() + usize::from(include_local));
        let local_environment = if include_local {
            let local_runtime_paths = local_runtime_paths.clone().ok_or_else(|| {
                ExecServerError::Protocol(
                    "local environment requires configured runtime paths".to_string(),
                )
            })?;
            let local_environment = Arc::new(Environment::local(
                local_runtime_paths,
                http_client_factory.clone(),
            ));
            environment_map.insert(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Arc::clone(&local_environment),
            );
            Some(local_environment)
        } else {
            None
        };
        for EnvironmentProviderEntry {
            id,
            transport,
            skills,
        } in environments
        {
            if id.is_empty() {
                return Err(ExecServerError::Protocol(
                    "environment id cannot be empty".to_string(),
                ));
            }
            if id == LOCAL_ENVIRONMENT_ID {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
                )));
            }
            let environment = Environment::remote_with_transport(
                transport,
                /*local_runtime_paths*/ None,
                http_client_factory.clone(),
                skills,
            );
            if environment_map
                .insert(id.clone(), Arc::new(environment))
                .is_some()
            {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{id}` is duplicated"
                )));
            }
        }
        let default_environment = match default {
            EnvironmentDefault::Disabled => None,
            EnvironmentDefault::EnvironmentId(environment_id) => {
                if !environment_map.contains_key(&environment_id) {
                    return Err(ExecServerError::Protocol(format!(
                        "default environment `{environment_id}` is not configured"
                    )));
                }
                Some(environment_id)
            }
        };
        // The snapshot is valid; start connecting its remote environments in the background.
        for environment in environment_map.values() {
            environment.start_connecting();
        }
        Ok(Self {
            default_environment,
            environments: RwLock::new(environment_map),
            local_environment,
            local_runtime_paths,
            http_client_factory,
        })
    }
    pub fn default_environment(&self) -> Option<Arc<Environment>> {
        self.default_environment
            .as_deref()
            .and_then(|environment_id| self.get_environment(environment_id))
    }
    pub fn default_environment_id(&self) -> Option<&str> {
        self.default_environment.as_deref()
    }
    pub fn default_environment_ids(&self) -> Vec<String> {
        let Some(default_environment_id) = self.default_environment.as_ref() else {
            return Vec::new();
        };
        let environments = self
            .environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut environment_ids = Vec::with_capacity(environments.len());
        environment_ids.push(default_environment_id.clone());
        environment_ids.extend(
            environments
                .keys()
                .filter(|environment_id| *environment_id != default_environment_id)
                .cloned(),
        );
        environment_ids
    }
    pub fn try_local_environment(&self) -> Option<Arc<Environment>> {
        self.local_environment.as_ref().map(Arc::clone)
    }
    pub fn get_environment(&self, environment_id: &str) -> Option<Arc<Environment>> {
        self.environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(environment_id)
            .cloned()
    }
    pub async fn get_environment_status(
        &self,
        environment_id: &str,
    ) -> Option<EnvironmentObservedStatus> {
        let environment = self.get_environment(environment_id)?;
        Some(environment.status().await)
    }
    pub fn upsert_environment(
        &self,
        environment_id: String,
        exec_server_url: String,
        connect_timeout: Option<std::time::Duration>,
    ) -> Result<(), ExecServerError> {
        self.upsert_environment_with_options(
            environment_id,
            RemoteEnvironmentOptions {
                exec_server_url,
                connect_timeout,
                http_headers: HashMap::new(),
            },
            ScopedSkillsConfig::default(),
        )
    }
    pub fn upsert_environment_with_options(
        &self,
        environment_id: String,
        options: RemoteEnvironmentOptions,
        skills: ScopedSkillsConfig,
    ) -> Result<(), ExecServerError> {
        validate_environment_id(&environment_id)?;
        let transport = options.into_transport_params()?;
        let environment = Environment::remote_with_transport(
            transport,
            self.local_runtime_paths.clone(),
            self.http_client_factory.clone(),
            skills,
        );
        self.insert_environment(environment_id, Arc::new(environment))
    }
    fn insert_environment(
        &self,
        environment_id: String,
        environment: Arc<Environment>,
    ) -> Result<(), ExecServerError> {
        let replaced = {
            let mut environments = self
                .environments
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            environments.insert(environment_id, Arc::clone(&environment))
        };
        drop(replaced);
        environment.start_connecting();
        Ok(())
    }
    pub fn http_client_factory(&self) -> &HttpClientFactory {
        &self.http_client_factory
    }

    pub async fn prepare_from_codex_home(
        home: impl AsRef<std::path::Path>,
    ) -> Result<PreparedEnvironmentManager, ExecServerError> {
        let provider =
            crate::environment_toml::environment_provider_from_codex_home(home.as_ref())?;
        Ok(PreparedEnvironmentManager {
            snapshot: provider.snapshot().await?,
        })
    }
    pub async fn from_codex_home(
        home: impl AsRef<std::path::Path>,
        runtime: Option<ExecServerRuntimeOptions>,
        factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        Self::prepare_from_codex_home(home)
            .await?
            .build(runtime, factory)
    }
    pub async fn prepare_from_env() -> Result<PreparedEnvironmentManager, ExecServerError> {
        Ok(PreparedEnvironmentManager {
            snapshot: DefaultEnvironmentProvider::from_env().snapshot().await?,
        })
    }
    pub async fn from_env(
        runtime: Option<ExecServerRuntimeOptions>,
        factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        Self::prepare_from_env().await?.build(runtime, factory)
    }
}
impl Environment {
    #[cfg(feature = "test-api")]
    #[doc(hidden)]
    pub fn startup_abort_handle_for_test(&self) -> Option<tokio::task::AbortHandle> {
        self.startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(tokio_util::task::AbortOnDropHandle::abort_handle)
    }

    #[must_use]
    pub fn default_for_tests() -> Self {
        Self {
            remote_client: None,
            skills: ScopedSkillsConfig::default(),
            ready_info: Arc::new(ArcSwapOption::empty()),
            startup_task: Arc::new(Mutex::new(None)),
            filesystem: Arc::new(LocalFileSystem::unsandboxed()),
            local_runtime_paths: None,
        }
    }
    pub fn create(
        exec_server_url: Option<String>,
        local_runtime_paths: ExecServerRuntimeOptions,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        Self::create_inner(
            exec_server_url,
            Some(local_runtime_paths),
            http_client_factory,
        )
    }
    pub fn create_for_tests(exec_server_url: Option<String>) -> Result<Self, ExecServerError> {
        Self::create_inner(
            exec_server_url,
            /*local_runtime_paths*/ None,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
    }
    fn create_inner(
        exec_server_url: Option<String>,
        local_runtime_paths: Option<ExecServerRuntimeOptions>,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let (exec_server_url, disabled) = normalize_exec_server_url(exec_server_url);
        if disabled {
            return Err(ExecServerError::Protocol(
                "disabled mode does not create an Environment".to_string(),
            ));
        }

        Ok(match exec_server_url {
            Some(exec_server_url) => Self::remote_with_transport(
                ExecServerTransportParams::websocket_url(
                    exec_server_url,
                    DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
                ),
                local_runtime_paths,
                http_client_factory,
                ScopedSkillsConfig::default(),
            ),
            None => match local_runtime_paths {
                Some(local_runtime_paths) => Self::local(local_runtime_paths, http_client_factory),
                None => Self::default_for_tests(),
            },
        })
    }
    #[must_use]
    pub fn local(
        local_runtime_paths: ExecServerRuntimeOptions,
        _http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            remote_client: None,
            skills: ScopedSkillsConfig::default(),
            ready_info: Arc::new(ArcSwapOption::empty()),
            startup_task: Arc::new(Mutex::new(None)),
            filesystem: Arc::new(LocalFileSystem::with_runtime_paths(
                local_runtime_paths.clone(),
            )),
            local_runtime_paths: Some(local_runtime_paths),
        }
    }
    #[must_use]
    pub fn remote_with_transport(
        remote_transport: ExecServerTransportParams,
        local_runtime_paths: Option<ExecServerRuntimeOptions>,
        http_client_factory: HttpClientFactory,
        skills: ScopedSkillsConfig,
    ) -> Self {
        let client = LazyRemoteExecServerClient::new(remote_transport, http_client_factory);
        Self::remote_with_client(client, local_runtime_paths, skills)
    }
    #[must_use]
    pub fn remote_with_client(
        client: LazyRemoteExecServerClient,
        local_runtime_paths: Option<ExecServerRuntimeOptions>,
        skills: ScopedSkillsConfig,
    ) -> Self {
        let filesystem: Arc<dyn ExecutorFileSystem> =
            Arc::new(RemoteFileSystem::new(client.clone()));

        Self {
            remote_client: Some(client.clone()),
            skills,
            ready_info: Arc::new(ArcSwapOption::empty()),
            startup_task: Arc::new(Mutex::new(None)),
            filesystem,
            local_runtime_paths,
        }
    }
    #[must_use]
    pub fn required_skills(&self) -> &[String] {
        &self.skills.required
    }
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.remote_client.is_some()
    }
    #[must_use]
    pub fn last_ready_info(&self) -> Option<Arc<EnvironmentReadyInfo>> {
        self.ready_info.load_full()
    }
    pub fn selected_capability_roots(&self) -> Vec<SelectedCapabilityRoot> {
        self.ready_info
            .load()
            .as_ref()
            .map_or_else(Vec::new, |ready_info| {
                ready_info.selected_capability_roots.clone()
            })
    }
    pub fn observe_connection_attempts(
        &self,
        observer: impl Fn(std::time::Duration, ConnectionAttemptOutcome) + Send + Sync + 'static,
    ) {
        if let Some(client) = &self.remote_client {
            let _ = client.connection_observer.set(Arc::new(observer));
        }
    }
    pub fn subscribe_connection_state(
        &self,
    ) -> Option<watch::Receiver<EnvironmentConnectionState>> {
        self.remote_client
            .as_ref()
            .map(LazyRemoteExecServerClient::subscribe_connection_state)
    }
    #[must_use]
    pub fn local_runtime_paths(&self) -> Option<&ExecServerRuntimeOptions> {
        self.local_runtime_paths.as_ref()
    }
    pub async fn info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        match &self.remote_client {
            Some(client) => client.environment_info().await,
            None => Ok(crate::local_environment_info::local_environment_info()),
        }
    }
    pub fn cached_executor_registration_id(&self) -> Option<String> {
        self.remote_client
            .as_ref()
            .and_then(LazyRemoteExecServerClient::cached_executor_registration_id)
    }
    pub async fn refresh_connection(&self) -> Result<(), ExecServerError> {
        let client = self.remote_client.as_ref().ok_or_else(|| {
            ExecServerError::Protocol(
                "connection refresh requires a remote environment".to_string(),
            )
        })?;
        client.refresh_connection().await
    }
    pub async fn discover_capability_roots(
        &self,
        params: CapabilityRootsDiscoverParams,
    ) -> Result<CapabilityRootsDiscoverResponse, ExecServerError> {
        match &self.remote_client {
            Some(client) => {
                let connection_state = client.subscribe_connection_state();
                let client = client.get().await?;
                let discover = || async {
                    if params.roots.iter().any(|root| {
                        root.sandbox
                            .as_ref()
                            .is_some_and(crate::FileSystemSandboxContext::should_read_from_sandbox)
                    }) && !client
                        .environment_info()
                        .await?
                        .capabilities
                        .capability_discovery_sandbox
                    {
                        return Err(ExecServerError::Protocol(
                            "exec-server does not support sandboxed capability discovery"
                                .to_string(),
                        ));
                    }
                    client.discover_capability_roots(params.clone()).await
                };
                self.discover_with_recovery(connection_state, discover)
                    .await
            }
            None => crate::discover_capability_roots(self.filesystem.as_ref(), params)
                .await
                .map_err(|error| ExecServerError::Protocol(error.to_string())),
        }
    }
    pub fn start_connecting(&self) {
        let Some(client) = &self.remote_client else {
            return;
        };
        let mut startup_task = self
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            *startup_task = client.start_connecting();
        }
    }
    pub fn startup_finished(&self) -> bool {
        self.remote_client
            .as_ref()
            .is_none_or(LazyRemoteExecServerClient::startup_finished)
    }
    pub async fn wait_until_ready(&self) -> Result<(), ExecServerError> {
        match &self.remote_client {
            Some(client) => client.wait_until_ready().await,
            None => Ok(()),
        }
    }
    #[must_use]
    pub fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        match &self.remote_client {
            Some(client) => client.readiness_result(),
            None => Some(Ok(())),
        }
    }
    pub async fn status(&self) -> EnvironmentObservedStatus {
        match &self.remote_client {
            Some(client) => client.status().await,
            None => EnvironmentObservedStatus::Ready,
        }
    }
    #[must_use]
    pub fn get_filesystem(&self) -> Arc<dyn ExecutorFileSystem> {
        Arc::clone(&self.filesystem)
    }
    #[must_use]
    pub fn filesystem_ref(&self) -> &Arc<dyn ExecutorFileSystem> {
        &self.filesystem
    }
    #[must_use]
    pub fn get_filesystem_without_reconnect(&self) -> Arc<dyn ExecutorFileSystem> {
        match &self.remote_client {
            Some(client) => Arc::new(RemoteFileSystem::new(client.fail_fast())),
            None => Arc::clone(&self.filesystem),
        }
    }
}

/// Startup environments prepared before the effective network policy is chosen.
#[derive(Debug)]
pub struct PreparedEnvironmentManager {
    snapshot: EnvironmentProviderSnapshot,
}
impl PreparedEnvironmentManager {
    #[must_use]
    pub fn default_environment_is_remote(&self) -> bool {
        match &self.snapshot.default {
            EnvironmentDefault::Disabled => false,
            EnvironmentDefault::EnvironmentId(id) => self
                .snapshot
                .environments
                .iter()
                .any(|entry| &entry.id == id),
        }
    }
    pub fn build(
        self,
        runtime: Option<ExecServerRuntimeOptions>,
        factory: HttpClientFactory,
    ) -> Result<EnvironmentManager, ExecServerError> {
        EnvironmentManager::from_snapshot(self.snapshot, runtime, factory)
    }
}

#[cfg(feature = "test-api")]
pub fn from_snapshot_for_test(
    snapshot: EnvironmentProviderSnapshot,
    local_runtime_paths: Option<ExecServerRuntimeOptions>,
    http_client_factory: HttpClientFactory,
) -> Result<EnvironmentManager, ExecServerError> {
    EnvironmentManager::from_snapshot(snapshot, local_runtime_paths, http_client_factory)
}
