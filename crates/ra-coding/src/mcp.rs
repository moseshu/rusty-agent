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

mod startup;
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
        }
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
    filter: ToolFilter,
    operation: tokio::sync::Mutex<()>,
    state: Mutex<EntryState>,
    changes: broadcast::Sender<McpServerStatus>,
    lazy_startup: bool,
    automatic_retry: bool,
    startup_trigger: watch::Sender<bool>,
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
    entries: Vec<Arc<Entry>>,
    changes: broadcast::Sender<McpServerStatus>,
    discovery: McpDiscoveryOptions,
    registry: Arc<tokio::sync::Mutex<()>>,
}
impl CodingMcpRuntime {
    /// Creates a name-addressed product registry; duplicates fail before any connection opens.
    pub fn new(configs: Vec<McpServerConfig>, discovery: McpDiscoveryOptions) -> Result<Arc<Self>> {
        let mut names = BTreeSet::new();
        let (changes, _) = broadcast::channel(256);
        let mut entries = Vec::new();
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
            let server: Arc<dyn McpServer> = config.server.clone();
            let options = config
                .options
                .with_connect_in_parallel(true)
                .with_strict(false)
                .with_drop_failed_servers(true)
                .with_suppress_cancelled_error(false);
            entries.push(Arc::new(Entry {
                server: config.server,
                manager: McpServerManager::with_options([server], options),
                filter: config.filter,
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
                changes: changes.clone(),
                lazy_startup: config.lazy_startup,
                automatic_retry: config.automatic_retry,
                startup_trigger: watch::channel(false).0,
            }));
        }
        Ok(Arc::new(Self {
            entries,
            changes,
            discovery,
            registry: Arc::new(tokio::sync::Mutex::new(())),
        }))
    }

    /// Connects enabled servers concurrently, retaining each server's independent outcome.
    pub async fn connect_all(&self) -> Result<Vec<McpServerStatus>> {
        self.apply(None, Operation::Connect).await
    }

    /// Schedules shared startup without making thread preparation wait for network I/O.
    /// A usable catalog defers transport startup until invocation; otherwise discovery waits
    /// for this attempt. Preparing another thread reuses startup and respects recovery backoff.
    pub fn start_background(&self) {
        for entry in &self.entries {
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
        self.entries.iter().map(|entry| entry.snapshot()).collect()
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
        let entries = self.entries.clone();
        let registry = self.registry.clone();
        tokio::spawn(async move {
            let _registry = registry.lock().await;
            for entry in entries.into_iter().rev() {
                run(entry, Operation::Stop).await?;
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
        self.entries
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
            .entries
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

impl Entry {
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
                .is_some_and(|catalog| catalog.allows_deferred_startup(&self.filter));
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
        for entry in &self.entries {
            entry.invalidate_startup(true);
            entry.manager.request_shutdown();
        }
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
        let catalogs = join_all(self.entries.iter().map(|entry| async {
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
                    Ok(client) => client,
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
                            entry.filter.clone(),
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
                        catalog,
                        Arc::new(DeferredClient {
                            entry: entry.clone(),
                            epoch,
                            resolved: tokio::sync::OnceCell::new(),
                        }) as Arc<dyn McpToolClientSource>,
                        entry.filter.clone(),
                    )
                })
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<McpToolCatalogSource>>();
        Ok(discover_catalog_tools(
            &catalogs,
            &self.discovery,
            reserved,
            Some(context),
            Some(context.agent()),
        )
        .await?
        .into_iter()
        .map(|tool| tool as Arc<dyn Tool>)
        .collect())
    }
}

#[async_trait]
impl McpRuntimeControl for CodingMcpRuntime {
    fn get_mcp_status(&self) -> Vec<McpServerStatus> {
        self.get_mcp_status()
    }
    fn inventory(&self, name: &str, full: bool) -> BoxFuture<'static, McpServerStatus> {
        let Some(entry) = self
            .entries
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
