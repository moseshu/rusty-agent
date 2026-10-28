// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product MCP connection controls built on the Python-compatible lifecycle manager.
//!
//! Registry names are unique because UI commands address names. This does not constrain the
//! framework manager, whose identity remains the server instance. All lifecycle work keeps its
//! manager worker; cancelling a command waiter does not cancel the accepted operation.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use futures::{
    FutureExt,
    future::{BoxFuture, join_all},
};
use ra_core::{
    context::RunContext,
    error::{Error, Result},
    tool::{Tool, ToolResolver},
};
use ra_mcp::{
    McpServer, McpServerManager, McpServerManagerOptions,
    filter::ToolFilter,
    resolver::{McpClientSource, McpToolResolver},
    tool::{
        McpDiscoveryOptions, McpToolCatalog, McpToolCatalogSource, McpToolClientSource,
        discover_catalog_tools,
    },
};
use ra_protocol::{
    frame::JsonRpcErrorObject,
    server::{
        McpRuntimeControl,
        protocol::{McpServerConnectionStatus as Status, McpServerStatus},
    },
};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

mod approval;
mod product;
pub(crate) use approval::{McpApprovalKey, McpApprovalState};
pub use approval::{McpPermissionPromptAutoApproveContext, mcp_permission_prompt_is_auto_approved};

pub(crate) fn configure_approval_sources(
    servers: &mut [McpServerConfig],
    stack: &ra_config::ConfigLayerStack,
    user_file: Option<&ra_paths::AbsolutePathBuf>,
    elicitation: bool,
) {
    for server in servers {
        let name = server.server.name();
        let Some(policy) = &mut server.product_policy else {
            continue;
        };
        let policy = Arc::make_mut(policy);
        policy.elicitation = elicitation;
        let project_file = stack.layers_high_to_low().find_map(|layer| {
            if !matches!(layer.name, ra_config::ConfigLayerSource::Project { .. }) {
                return None;
            }
            let servers: std::collections::HashMap<String, ra_config::McpServerConfig> =
                layer.config.get("mcp_servers")?.clone().try_into().ok()?;
            if !servers.contains_key(name) {
                return None;
            }
            Some(layer.config_folder()?.join("config.toml").to_path_buf())
        });
        let user_configured = stack
            .effective_user_config()
            .as_ref()
            .and_then(|value| value.get("mcp_servers"))
            .cloned()
            .and_then(|servers| {
                servers
                    .try_into::<std::collections::HashMap<String, ra_config::McpServerConfig>>()
                    .ok()
            })
            .is_some_and(|servers| servers.contains_key(name));
        policy.persist_path = project_file.or_else(|| {
            (user_configured || policy.plugin_id.is_some())
                .then(|| user_file.map(ra_paths::AbsolutePathBuf::to_path_buf))
                .flatten()
        });
    }
}
mod startup;
pub use product::{ProductMcpServers, product_mcp_servers};
use startup::{DeferredClient, startup_error_display};

/// A configured server whose current initialized session can be captured for a model call.
pub trait McpRuntimeServer: McpServer + McpClientSource {}
impl<T: McpServer + McpClientSource> McpRuntimeServer for T {}

/// Explicit product registration, enabled by default with Python's ten-second deadlines.
pub struct McpServerConfig {
    server: Arc<dyn McpRuntimeServer>,
    enabled: bool,
    filter: ToolFilter,
    options: McpServerManagerOptions,
    cached_catalog: Option<McpToolCatalog>,
    lazy_startup: bool,
    automatic_retry: bool,
    identity: Option<String>,
    binding_options: Option<ra_mcp::client::McpClientOptions>,
    product_policy: Option<Arc<approval::ProductMcpPolicy>>,
}
impl McpServerConfig {
    /// Registers a reconnectable server without opening its transport.
    pub fn new(server: Arc<dyn McpRuntimeServer>) -> Self {
        Self {
            server,
            enabled: true,
            filter: ToolFilter::default(),
            options: McpServerManagerOptions::default(),
            cached_catalog: None,
            lazy_startup: false,
            automatic_retry: false,
            identity: None,
            binding_options: None,
            product_policy: None,
        }
    }
    /// The connection identity this registration carries, if any.
    #[must_use]
    pub fn connection_identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }
    /// The registered server's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.server.name()
    }
    /// Names the connection inputs this registration was built from. A later
    /// [`CodingMcpRuntime::replace_mcp_servers`] keeps the live entry of a same-named server whose
    /// identity is unchanged; registrations without an identity are always replaced.
    #[must_use]
    pub fn with_connection_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }
    /// Seeds a complete cache with the configured server's approval, guardrails and filters.
    #[must_use]
    pub fn with_cached_catalog(mut self, catalog: McpToolCatalog) -> Self {
        self.cached_catalog = Some(catalog);
        self
    }
    /// Opts into cached, model-visible lazy startup, disabled by default.
    /// Deferring a first startup from a host-provided cache is a product extension.
    #[must_use]
    pub const fn with_lazy_startup(mut self, enabled: bool) -> Self {
        self.lazy_startup = enabled;
        self
    }
    /// Opts into shared startup recovery on discovery or invocation, disabled by default.
    /// This product extension adapts Codex Apps' one-to-thirty-second exponential backoff.
    #[must_use]
    pub const fn with_automatic_retry(mut self, enabled: bool) -> Self {
        self.automatic_retry = enabled;
        self
    }
    /// Selects its initial enabled state.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    /// Selects a per-run tool filter.
    #[must_use]
    pub fn with_filter(mut self, filter: ToolFilter) -> Self {
        self.filter = filter;
        self
    }
    /// Captures product request deadlines and tool approval policy independently of the
    /// reusable connection. Previously discovered tools retain their captured policy.
    #[must_use]
    pub fn with_binding_options(mut self, options: ra_mcp::client::McpClientOptions) -> Self {
        self.binding_options = Some(options);
        self
    }
    /// Selects independent connect and cleanup deadlines. Scheduling and failure isolation are
    /// product-owned; the remaining manager policy switches are not used by this registry.
    #[must_use]
    pub fn with_lifecycle_options(mut self, options: McpServerManagerOptions) -> Self {
        self.options = options;
        self
    }
}

struct Entry {
    server: Arc<dyn McpRuntimeServer>,
    manager: McpServerManager,
    view: Mutex<BindingView>,
    identity: Option<String>,
    configured_enabled: bool,
    operation: tokio::sync::Mutex<()>,
    state: Mutex<EntryState>,
    changes: broadcast::Sender<McpServerStatus>,
    lazy_startup: bool,
    automatic_retry: bool,
    startup_trigger: watch::Sender<bool>,
}

/// The source's per-step view is separate from connection identity and lifecycle ownership.
#[derive(Clone)]
struct BindingView {
    filter: ToolFilter,
    options: Option<ra_mcp::client::McpClientOptions>,
    product_policy: Option<Arc<approval::ProductMcpPolicy>>,
}
impl BindingView {
    fn client(&self, client: ra_mcp::client::McpClient) -> ra_mcp::client::McpClient {
        match &self.options {
            Some(options) => client.with_binding_options(options.clone()),
            None => client,
        }
    }
    fn catalog(&self, catalog: McpToolCatalog) -> McpToolCatalog {
        match &self.options {
            Some(options) => catalog.with_binding_options(options.clone()),
            None => catalog,
        }
    }
}
type StartupResult = std::result::Result<ra_mcp::client::McpClient, Arc<Error>>;
type StartupAttempt = watch::Receiver<Option<StartupResult>>;
struct EntryState {
    enabled: bool,
    generation: u64,
    status: Status,
    failure: Option<(&'static str, Arc<Error>)>,
    cleanup_failed: bool,
    tools_error: Option<Arc<Error>>,
    cached_catalog: Option<McpToolCatalog>,
    stopped: bool,
    epoch: u64,
    pending: Option<StartupAttempt>,
    startup_cancel: CancellationToken,
    consecutive_failures: u32,
    retry_not_before: Option<tokio::time::Instant>,
}

/// A shared, explicitly selected coding-product MCP runtime and per-model-call tool resolver.
/// The host must await shutdown while its Tokio runtime is still alive.
pub struct CodingMcpRuntime {
    // Readers capture one published set; replacement publishes a new one under `registry`.
    entries: std::sync::RwLock<Arc<Vec<Arc<Entry>>>>,
    changes: broadcast::Sender<McpServerStatus>,
    discovery: McpDiscoveryOptions,
    registry: Arc<tokio::sync::Mutex<()>>,
    // Whether the host has asked for startup; replacement starts new entries only after that.
    started: std::sync::atomic::AtomicBool,
}
impl CodingMcpRuntime {
    /// Creates a name-addressed product registry; duplicates fail before any connection opens.
    pub fn new(configs: Vec<McpServerConfig>, discovery: McpDiscoveryOptions) -> Result<Arc<Self>> {
        validate_configs(&configs)?;
        let (changes, _) = broadcast::channel(256);
        let entries = configs
            .into_iter()
            .map(|config| Entry::new(config, changes.clone()))
            .collect();
        Ok(Arc::new(Self {
            entries: std::sync::RwLock::new(Arc::new(entries)),
            changes,
            discovery,
            registry: Arc::new(tokio::sync::Mutex::new(())),
            started: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    fn entries(&self) -> Arc<Vec<Arc<Entry>>> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reconciles the registry with a new set of registrations, as the source's connection set
    /// does on refresh. A same-named enabled server with an unchanged connection identity keeps
    /// its healthy connection or compatible pending startup and takes the new binding policy; every
    /// other previous entry is stopped and released, and new entries start in the background
    /// once the host has started the runtime. Order follows `configs`. Each server's failure
    /// stays its own; the accepted replacement finishes even if the caller stops waiting.
    pub async fn replace_mcp_servers(
        &self,
        configs: Vec<McpServerConfig>,
    ) -> Result<Vec<McpServerStatus>> {
        validate_configs(&configs)?;
        // Hold the registry from reading the current set until the replaced entries are stopped,
        // so concurrent replacements and lifecycle commands see one consistent set.
        let registry = Arc::clone(&self.registry).lock_owned().await;
        let previous = self.entries();
        let mut retained = std::collections::HashSet::new();
        let mut next = Vec::with_capacity(configs.len());
        let mut added = Vec::new();
        let mut view_updates = Vec::new();
        for config in configs {
            // A registration enabled by configuration again gets a fresh entry; a runtime toggle
            // of a retained entry survives the refresh.
            let existing = (config.enabled && config.identity.is_some())
                .then(|| {
                    previous.iter().find(|entry| {
                        entry.configured_enabled
                            && entry.server.name() == config.server.name()
                            && entry.identity == config.identity
                            && entry.can_reuse(&config)
                    })
                })
                .flatten();
            if let Some(entry) = existing {
                view_updates.push((
                    entry.clone(),
                    BindingView {
                        filter: config.filter,
                        options: config.binding_options,
                        product_policy: config.product_policy,
                    },
                ));
                retained.insert(Arc::as_ptr(entry).addr());
                next.push(entry.clone());
            } else {
                let entry = Entry::new(config, self.changes.clone());
                added.push(entry.clone());
                next.push(entry);
            }
        }
        let removed = previous
            .iter()
            .filter(|entry| !retained.contains(&Arc::as_ptr(entry).addr()))
            .cloned()
            .collect::<Vec<_>>();
        let started = self.started.load(std::sync::atomic::Ordering::Acquire);
        let published = Arc::new(next);
        {
            // Publish before the old entries are stopped so no model call captures a removed one.
            let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
            for (entry, view) in view_updates {
                *entry.view.lock().unwrap_or_else(PoisonError::into_inner) = view;
            }
            *entries = published;
        }
        tokio::spawn(async move {
            let _registry = registry;
            let mut failure = None;
            for result in
                join_all(removed.into_iter().map(|entry| run(entry, Operation::Stop))).await
            {
                if let Err(error) = result {
                    failure.get_or_insert(error);
                }
            }
            if started {
                for entry in &added {
                    entry.start_background();
                }
            }
            failure.map_or(Ok(()), Err)
        })
        .await
        .map_err(|error| Error::config("MCP registry replacement failed").with_source(error))??;
        Ok(self.get_mcp_status())
    }

    /// Connects enabled servers concurrently, retaining each server's independent outcome.
    pub async fn connect_all(&self) -> Result<Vec<McpServerStatus>> {
        self.started
            .store(true, std::sync::atomic::Ordering::Release);
        self.apply(None, Operation::Connect).await
    }

    /// Schedules shared startup without making thread preparation wait for network I/O.
    /// A usable catalog defers transport startup until invocation; otherwise discovery waits
    /// for this attempt. Preparing another thread reuses startup and respects recovery backoff.
    pub fn start_background(&self) {
        self.started
            .store(true, std::sync::atomic::Ordering::Release);
        for entry in self.entries().iter() {
            entry.start_background();
        }
    }

    /// Reconciles the installed registry, retaining healthy and pending connections. A host with
    /// immutable registrations has no changed transport identity to replace on reload.
    pub async fn reload_mcp_servers(&self) -> Result<Vec<McpServerStatus>> {
        self.apply(None, Operation::Reload).await
    }

    /// Reads detached connection snapshots without network I/O or lifecycle locks.
    #[must_use]
    pub fn get_mcp_status(&self) -> Vec<McpServerStatus> {
        self.entries()
            .iter()
            .map(|entry| entry.snapshot())
            .collect()
    }

    /// Retries one or all enabled servers; failed-only defaults belong to the caller's API.
    pub async fn reconnect_mcp_server(
        &self,
        name: Option<&str>,
        failed_only: bool,
    ) -> Result<Vec<McpServerStatus>> {
        self.apply(name, Operation::Reconnect(failed_only)).await
    }

    /// Enables and connects a server, or hides its tools and cleans its owning worker.
    pub async fn toggle_mcp_server(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<Vec<McpServerStatus>> {
        self.apply(Some(name), Operation::Toggle(enabled)).await
    }

    /// Cleans connections in reverse registry order, recording failures and keeping ownership.
    pub async fn shutdown(&self) -> Result<Vec<McpServerStatus>> {
        let entries = self.entries();
        let registry = self.registry.clone();
        tokio::spawn(async move {
            let _registry = registry.lock().await;
            for entry in entries.iter().rev() {
                run(entry.clone(), Operation::Stop).await?;
            }
            Ok::<(), Error>(())
        })
        .await
        .map_err(|error| Error::config("MCP shutdown task failed").with_source(error))??;
        Ok(self.get_mcp_status())
    }

    /// Original lifecycle failures for host diagnostics, including their phase and source.
    #[must_use]
    pub fn errors(&self) -> Vec<(String, &'static str, Arc<Error>)> {
        self.entries()
            .iter()
            .filter_map(|entry| {
                let state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
                state
                    .failure
                    .as_ref()
                    .map(|(phase, error)| (entry.server.name().to_owned(), *phase, error.clone()))
                    .or_else(|| {
                        state.tools_error.as_ref().map(|error| {
                            (entry.server.name().to_owned(), "tools/list", error.clone())
                        })
                    })
            })
            .collect()
    }

    async fn apply(
        &self,
        name: Option<&str>,
        operation: Operation,
    ) -> Result<Vec<McpServerStatus>> {
        let targets: Vec<_> = self
            .entries()
            .iter()
            .filter(|entry| name.is_none_or(|name| entry.server.name() == name))
            .cloned()
            .collect();
        if name.is_some() && targets.is_empty() {
            return Err(Error::config("No MCP server with the requested name found"));
        }
        let registry = self.registry.clone();
        tokio::spawn(async move {
            let _registry = registry.lock().await;
            for result in join_all(targets.into_iter().map(|entry| run(entry, operation))).await {
                result?;
            }
            Ok::<(), Error>(())
        })
        .await
        .map_err(|error| Error::config("MCP registry operation failed").with_source(error))??;
        Ok(self.get_mcp_status())
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Connect,
    Reload,
    Reconnect(bool),
    Toggle(bool),
    Stop,
}

async fn run(entry: Arc<Entry>, operation: Operation) -> Result<()> {
    // The task owns the accepted command even if its caller disconnects or drops its future.
    tokio::spawn(async move {
        let operation_guard = entry.operation.lock().await;
        let outcome = std::panic::AssertUnwindSafe(entry.operate(operation))
            .catch_unwind()
            .await;
        let pending = if let Ok(pending) = outcome {
            pending
        } else {
            entry.fail(
                "runtime",
                Arc::new(Error::config("MCP runtime operation panicked")),
            );
            None
        };
        // Startup uses the same operation lock. Release command admission before joining it.
        drop(operation_guard);
        if let Some(mut pending) = pending {
            while pending.borrow().is_none() {
                if pending.changed().await.is_err() {
                    break;
                }
            }
        }
    })
    .await
    .map_err(|error| Error::config("MCP control task failed").with_source(error))
}

fn validate_configs(configs: &[McpServerConfig]) -> Result<()> {
    let mut names = BTreeSet::new();
    for config in configs {
        if !names.insert(config.server.name().to_owned()) {
            return Err(Error::config("MCP runtime server names must be unique"));
        }
        if config
            .cached_catalog
            .as_ref()
            .is_some_and(|catalog| catalog.server_name() != config.server.name())
        {
            return Err(Error::config(
                "cached MCP catalog belongs to a different server",
            ));
        }
    }
    Ok(())
}

impl Entry {
    fn new(config: McpServerConfig, changes: broadcast::Sender<McpServerStatus>) -> Arc<Self> {
        let server: Arc<dyn McpServer> = config.server.clone();
        let options = config
            .options
            .with_connect_in_parallel(true)
            .with_strict(false)
            .with_drop_failed_servers(true)
            .with_suppress_cancelled_error(false);
        Arc::new(Self {
            server: config.server,
            manager: McpServerManager::with_options([server], options),
            view: Mutex::new(BindingView {
                filter: config.filter,
                options: config.binding_options,
                product_policy: config.product_policy,
            }),
            identity: config.identity,
            configured_enabled: config.enabled,
            operation: tokio::sync::Mutex::new(()),
            state: Mutex::new(EntryState {
                enabled: config.enabled,
                generation: 0,
                status: if config.enabled {
                    Status::NotStarted
                } else {
                    Status::Disabled
                },
                failure: None,
                cleanup_failed: false,
                tools_error: None,
                cached_catalog: config.cached_catalog,
                stopped: false,
                epoch: 0,
                pending: None,
                startup_cancel: CancellationToken::new(),
                consecutive_failures: 0,
                retry_not_before: None,
            }),
            changes,
            lazy_startup: config.lazy_startup,
            automatic_retry: config.automatic_retry,
            startup_trigger: watch::channel(false).0,
        })
    }
    fn filter(&self) -> ToolFilter {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .filter
            .clone()
    }
    fn binding_view(&self) -> BindingView {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn can_reuse(&self, desired: &McpServerConfig) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Runtime disable survives config refresh. A failed cleanup must keep its manager
        // worker so refresh cannot open another transport over unconfirmed cleanup.
        if !state.enabled || state.cleanup_failed {
            return true;
        }
        if state.stopped {
            return false;
        }
        if state.status == Status::Connected {
            return self
                .server
                .current_client()
                .is_ok_and(|client| !client.is_closed());
        }
        if let Some(pending) = &state.pending {
            return pending.borrow().is_none()
                && !self.startup_is_dormant()
                && !state.startup_cancel.is_cancelled()
                && self.manager.connect_timeout() == desired.options.connect_timeout();
        }
        state.status == Status::NotStarted
            && self.manager.connect_timeout() == desired.options.connect_timeout()
    }
    fn snapshot(&self) -> McpServerStatus {
        self.snapshot_with_generation().0
    }
    fn snapshot_with_generation(&self) -> (McpServerStatus, u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let disconnected = state.status == Status::Connected
            && self
                .server
                .current_client()
                .map_or(true, |client| client.is_closed());
        if disconnected {
            state.status = Status::Failed;
            state.generation = state.generation.wrapping_add(1);
            state.failure = Some((
                "session",
                Arc::new(Error::config("MCP transport disconnected")),
            ));
        }
        let mut status = McpServerStatus::new(self.server.name(), state.status);
        if let Some((phase, error)) = &state.failure {
            status = status
                .with_lifecycle_failure(*phase, error.code())
                .with_startup_error(startup_error_display(
                    self.server.name(),
                    self.manager.connect_timeout(),
                    phase,
                    error,
                ));
        }
        if let Some(error) = &state.tools_error {
            status = status.with_tools(BTreeMap::new(), Some(error.to_string()));
        }
        let generation = state.generation;
        drop(state);
        if disconnected {
            let _ = self.changes.send(status.clone());
        }
        (status, generation)
    }
    fn publish(&self, status: Status) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.status = status;
            state.generation = state.generation.wrapping_add(1);
        }
        let _ = self.changes.send(self.snapshot());
    }
    fn fail(&self, phase: &'static str, error: Arc<Error>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let status = if !state.enabled {
            Status::Disabled
        } else if error.is_cancelled() {
            Status::Cancelled
        } else {
            Status::Failed
        };
        state.failure = Some((phase, error));
        drop(state);
        self.publish(status);
    }
    fn clear_diagnostics(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.failure = None;
        state.tools_error = None;
    }
    fn ready(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.enabled && state.status == Status::Connected
    }
    async fn cleanup(&self) -> bool {
        let retained_cleanup_failure = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cleanup_failed;
        let previous = self
            .manager
            .errors()
            .first()
            .map(|(_, error)| error.clone());
        let result = self.manager.cleanup_all().await;
        let error = result.err().or_else(|| {
            self.manager
                .errors()
                .into_iter()
                .map(|(_, error)| error)
                .find(|error| {
                    retained_cleanup_failure
                        || previous.as_ref().is_none_or(|old| !Arc::ptr_eq(old, error))
                })
        });
        if let Some(error) = error {
            self.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cleanup_failed = true;
            self.fail("cleanup", error);
            false
        } else {
            self.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cleanup_failed = false;
            true
        }
    }
    async fn operate(self: &Arc<Self>, operation: Operation) -> Option<StartupAttempt> {
        let _ = self.snapshot();
        let (enabled, status, pending) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if let Operation::Toggle(enabled) = operation {
                state.enabled = enabled;
            }
            if matches!(operation, Operation::Connect | Operation::Toggle(true)) {
                state.stopped = false;
            }
            (state.enabled, state.status, state.pending.clone())
        };
        if matches!(operation, Operation::Stop | Operation::Toggle(false)) {
            self.invalidate_startup(true);
            self.clear_diagnostics();
            self.publish(if enabled {
                Status::NotStarted
            } else {
                Status::Disabled
            });
            if self.cleanup().await {
                self.clear_diagnostics();
                self.publish(if enabled {
                    Status::NotStarted
                } else {
                    Status::Disabled
                });
            }
            return None;
        }
        if !enabled
            || (matches!(operation, Operation::Reconnect(true))
                && !matches!(status, Status::Failed | Status::Cancelled))
        {
            return None;
        }
        if matches!(operation, Operation::Connect | Operation::Reload) {
            if status == Status::Connected {
                return None;
            }
            if pending.is_some() {
                return if self.startup_is_dormant() {
                    None
                } else {
                    pending
                };
            }
        }
        if matches!(operation, Operation::Toggle(true)) && status == Status::Connected {
            return None;
        }
        if matches!(operation, Operation::Reconnect(_) | Operation::Toggle(true)) {
            self.invalidate_startup(false);
        }
        if matches!(operation, Operation::Connect | Operation::Reload)
            && status == Status::NotStarted
            && self.lazy_startup
        {
            let can_defer = self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cached_catalog
                .as_ref()
                .is_some_and(|catalog| catalog.allows_deferred_startup(&self.filter()));
            if can_defer {
                let pending = self.launch_startup(true, false);
                return if self.startup_is_dormant() {
                    None
                } else {
                    pending
                };
            }
        }
        self.launch_explicit_startup(matches!(operation, Operation::Reconnect(_)))
    }

    async fn connect_now(&self, force: bool) {
        let status = self.snapshot().runtime_status();
        if status == Some(Status::Connected) && !force {
            return;
        }
        self.clear_diagnostics();
        self.publish(Status::Starting);
        let cleanup_failed = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cleanup_failed;
        if (cleanup_failed || !matches!(status, Some(Status::NotStarted | Status::Disabled)))
            && !self.cleanup().await
        {
            return;
        }
        let result = self.manager.connect_all().await;
        if let Some(error) = result.err().or_else(|| {
            self.manager
                .errors()
                .first()
                .map(|(_, error)| error.clone())
        }) {
            self.fail("connect", error);
        } else {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.consecutive_failures = 0;
            state.retry_not_before = None;
            drop(state);
            self.publish(Status::Connected);
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .startup_cancel
            .cancel();
    }
}

impl Drop for CodingMcpRuntime {
    fn drop(&mut self) {
        // Deferred bindings and recovery tasks may still retain an entry. The runtime's owner
        // must nevertheless cancel startup and release its connections, as Codex's owner does.
        for entry in self.entries().iter() {
            entry.invalidate_startup(true);
            entry.manager.request_shutdown();
        }
    }
}

impl CodingMcpRuntime {
    fn product_tool(
        bindings: &[(Arc<Entry>, BindingView)],
        tool: Arc<ra_mcp::tool::McpTool>,
    ) -> Arc<dyn Tool> {
        let policy = bindings
            .iter()
            .find(|(entry, _)| entry.server.name() == tool.info().server_name())
            .and_then(|(_, view)| view.product_policy.clone());
        match policy {
            Some(policy) => Arc::new(approval::ProductMcpTool::new(tool, policy)),
            None => tool,
        }
    }
    fn bindings(&self) -> Vec<(Arc<Entry>, BindingView)> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|entry| (entry.clone(), entry.binding_view()))
            .collect()
    }
    /// Resource-helper sources for one model call.
    fn resource_sources(
        bindings: &[(Arc<Entry>, BindingView)],
    ) -> Vec<(String, Arc<dyn McpToolClientSource>)> {
        // Codex advertises resource helpers whenever MCP servers are configured and lets each
        // request wait for its connection. A connected session is captured as is; any other
        // enabled server gets the same deferred binding as cached function tools.
        bindings
            .iter()
            .filter_map(|(entry, view)| {
                let state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
                if !state.enabled || state.stopped {
                    return None;
                }
                let connected = (state.status == Status::Connected)
                    .then(|| entry.server.current_client().ok())
                    .flatten();
                let source = match connected {
                    Some(client) => Arc::new(view.client(client)) as Arc<dyn McpToolClientSource>,
                    None => Arc::new(DeferredClient {
                        entry: entry.clone(),
                        epoch: state.epoch,
                        resolved: tokio::sync::OnceCell::new(),
                        view: view.clone(),
                    }),
                };
                Some((entry.server.name().to_owned(), source))
            })
            .collect()
    }
}

#[async_trait]
impl ToolResolver for CodingMcpRuntime {
    fn requires_reserved_tool_names(&self) -> bool {
        McpToolResolver::new(Vec::new(), self.discovery.clone()).requires_reserved_tool_names()
    }
    async fn get_tools(
        &self,
        context: &RunContext,
        reserved: &BTreeSet<String>,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        let _ = self.get_mcp_status();
        let bindings = self.bindings();
        let resource_sources = Self::resource_sources(&bindings);
        let catalogs = join_all(bindings.iter().map(|(entry, view)| async {
            entry.wait_for_catalog().await;
            let (enabled, stopped, status, cached, epoch, generation) = {
                let state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
                (
                    state.enabled,
                    state.stopped,
                    state.status,
                    state.cached_catalog.clone().filter(|_| {
                        matches!(state.status, Status::NotStarted | Status::Starting)
                            || (entry.automatic_retry
                                && matches!(state.status, Status::Failed | Status::Cancelled)
                                && !state.cleanup_failed)
                    }),
                    state.epoch,
                    state.generation,
                )
            };
            if !enabled || stopped {
                return None;
            }
            if status == Status::Connected {
                let client = match entry.server.current_client() {
                    Ok(client) => view.client(client),
                    Err(error) => {
                        entry.fail("session", Arc::new(error));
                        return None;
                    }
                };
                match McpToolCatalog::fetch(&client).await {
                    Ok(catalog) => {
                        let mut state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
                        if state.generation != generation {
                            return None;
                        }
                        state.cached_catalog = Some(catalog.clone());
                        state.tools_error = None;
                        Some((
                            catalog,
                            Arc::new(client) as Arc<dyn McpToolClientSource>,
                            view.filter.clone(),
                        ))
                    }
                    Err(error) => {
                        let mut state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
                        if state.generation == generation {
                            state.tools_error = Some(Arc::new(error));
                        }
                        drop(state);
                        let _ = entry.changes.send(entry.snapshot());
                        None
                    }
                }
            } else {
                cached.map(|catalog| {
                    (
                        view.catalog(catalog),
                        Arc::new(DeferredClient {
                            entry: entry.clone(),
                            epoch,
                            resolved: tokio::sync::OnceCell::new(),
                            view: view.clone(),
                        }) as Arc<dyn McpToolClientSource>,
                        view.filter.clone(),
                    )
                })
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<McpToolCatalogSource>>();
        let mut tools = discover_catalog_tools(
            &catalogs,
            &self.discovery,
            reserved,
            Some(context),
            Some(context.agent()),
        )
        .await?
        .into_iter()
        .map(|tool| Self::product_tool(&bindings, tool))
        .collect::<Vec<_>>();
        // Resource helpers are ordinary advertised tools, independent of server tool filtering,
        // tools/list failures and deferred function discovery. Their sessions share the same owner.
        if !resource_sources.is_empty() {
            tools.extend(ra_tools::mcp_ns::resource_tools_from_sources(
                resource_sources,
            )?);
        }
        Ok(tools)
    }
}

#[async_trait]
impl McpRuntimeControl for CodingMcpRuntime {
    fn get_mcp_status(&self) -> Vec<McpServerStatus> {
        self.get_mcp_status()
    }
    fn inventory(&self, name: &str, full: bool) -> BoxFuture<'static, McpServerStatus> {
        let Some(entry) = self
            .entries()
            .iter()
            .find(|entry| entry.server.name() == name)
            .cloned()
        else {
            return futures::future::ready(McpServerStatus::new(name, Status::NotStarted)).boxed();
        };
        let (status, generation) = entry.snapshot_with_generation();
        let client = entry
            .ready()
            .then(|| entry.server.current_client().ok())
            .flatten();
        async move {
            if entry.snapshot_with_generation().1 != generation {
                return entry.snapshot();
            }
            let Some(client) = client else {
                return status;
            };
            let info = client.server_initialize_result();
            let status = status.with_server_info(
                serde_json::to_value(&info.server_info).unwrap_or_default(),
                serde_json::to_value(&info.capabilities).unwrap_or_default(),
            );
            let tools = client.list_tools().await;
            let status = match tools {
                Ok(tools) => status.with_tools(
                    tools
                        .into_iter()
                        .map(|tool| {
                            (
                                tool.name.to_string(),
                                serde_json::to_value(tool).unwrap_or_default(),
                            )
                        })
                        .collect(),
                    None,
                ),
                Err(error) => status.with_tools(BTreeMap::new(), Some(error.to_string())),
            };
            let status = if full {
                let resources = catalogs(&client).await.unwrap_or_default();
                status.with_resources(resources.0, resources.1)
            } else {
                status
            };
            let current_generation = entry
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .generation;
            if current_generation == generation {
                status
            } else {
                entry.snapshot()
            }
        }
        .boxed()
    }
    async fn reload_mcp_servers(&self) -> std::result::Result<(), JsonRpcErrorObject> {
        self.reload_mcp_servers()
            .await
            .map(|_| ())
            .map_err(|error| JsonRpcErrorObject::internal_error(error.to_string()))
    }
    async fn reconnect_mcp_server(
        &self,
        name: Option<&str>,
        failed_only: bool,
    ) -> std::result::Result<(), JsonRpcErrorObject> {
        self.reconnect_mcp_server(name, failed_only)
            .await
            .map(|_| ())
            .map_err(|_| {
                JsonRpcErrorObject::invalid_request(
                    "MCP reconnect failed: check the configured server name",
                )
            })
    }
    async fn toggle_mcp_server(
        &self,
        name: &str,
        enabled: bool,
    ) -> std::result::Result<(), JsonRpcErrorObject> {
        self.toggle_mcp_server(name, enabled)
            .await
            .map(|_| ())
            .map_err(|_| {
                JsonRpcErrorObject::invalid_request(
                    "MCP toggle failed: check the configured server name",
                )
            })
    }
    async fn shutdown(&self) {
        let _ = self.shutdown().await;
    }
    fn subscribe(&self) -> broadcast::Receiver<McpServerStatus> {
        self.changes.subscribe()
    }
}

async fn catalogs(
    client: &ra_mcp::client::McpClient,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
    let mut resources = Vec::new();
    let mut templates = Vec::new();
    if client
        .server_initialize_result()
        .capabilities
        .resources
        .is_some()
    {
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        loop {
            let page = client.list_resources(cursor).await?;
            resources.extend(
                page.resources
                    .into_iter()
                    .map(|item| serde_json::to_value(item).unwrap_or_default()),
            );
            cursor = page.next_cursor;
            let Some(next) = &cursor else {
                break;
            };
            if !seen.insert(next.clone()) {
                return Err(Error::config("resources/list returned a repeated cursor"));
            }
        }
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        loop {
            let page = client.list_resource_templates(cursor).await?;
            templates.extend(
                page.resource_templates
                    .into_iter()
                    .map(|item| serde_json::to_value(item).unwrap_or_default()),
            );
            cursor = page.next_cursor;
            let Some(next) = &cursor else {
                break;
            };
            if !seen.insert(next.clone()) {
                return Err(Error::config(
                    "resources/templates/list returned a repeated cursor",
                ));
            }
        }
    }
    Ok((resources, templates))
}
