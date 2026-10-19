//! MCP lifecycle management ported from Python's `MCPServerManager` and `_ServerWorker`.
//!
//! Hosts explicitly await cleanup. Each server keeps one worker through connect and cleanup,
//! including sequential mode, so different host tasks can safely share a manager. Dropping a
//! waiter never aborts that worker. Dropping the manager signals same-task emergency cleanup;
//! the Tokio runtime must remain alive for asynchronous teardown to finish.

mod options;
mod worker;

pub use options::McpServerManagerOptions;

use crate::McpServer;
use futures::future::join_all;
use ra_core::error::Error;
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use worker::Worker;

/// Lifecycle errors retain the original error through shared ownership for diagnostic snapshots.
pub type McpLifecycleResult<T> = std::result::Result<T, Arc<Error>>;
use McpLifecycleResult as LifecycleResult;
type Options = Arc<Mutex<McpServerManagerOptions>>;

/// Owns configured servers and their lifecycle workers, independently of the agent runner.
pub struct McpServerManager {
    state: Arc<Mutex<State>>,
    options: Options,
    lifecycle: tokio::sync::Mutex<()>,
}

struct State {
    entries: Vec<Entry>,
    active: Vec<usize>,
    // Preserve failure occurrence order, including parallel completion order.
    failed: Vec<usize>,
}

struct Entry {
    server: Arc<dyn McpServer>,
    worker: Option<Worker>,
    connected: bool,
    error: Option<Arc<Error>>,
}

impl McpServerManager {
    /// Owns each distinct server instance once. Same-named instances remain distinct.
    pub fn new(servers: impl IntoIterator<Item = Arc<dyn McpServer>>) -> Self {
        Self::with_options(servers, McpServerManagerOptions::default())
    }

    /// Creates a manager without connecting or requiring a running Tokio runtime.
    pub fn with_options(
        servers: impl IntoIterator<Item = Arc<dyn McpServer>>,
        options: McpServerManagerOptions,
    ) -> Self {
        let mut entries: Vec<Entry> = Vec::new();
        for server in servers {
            if !entries
                .iter()
                .any(|entry| Arc::ptr_eq(&entry.server, &server))
            {
                entries.push(Entry {
                    server,
                    worker: None,
                    connected: false,
                    error: None,
                });
            }
        }
        Self {
            state: Arc::new(Mutex::new(State {
                active: (0..entries.len()).collect(),
                entries,
                failed: Vec::new(),
            })),
            options: Arc::new(Mutex::new(options)),
            lifecycle: tokio::sync::Mutex::new(()),
        }
    }

    /// All configured servers, in their original order, as a detached snapshot.
    pub fn all_servers(&self) -> Vec<Arc<dyn McpServer>> {
        lock(&self.state)
            .entries
            .iter()
            .map(|entry| entry.server.clone())
            .collect()
    }

    /// The last published active snapshot, refreshed at Python's lifecycle boundaries.
    /// Before the first attempt it includes all configured servers.
    pub fn active_servers(&self) -> Vec<Arc<dyn McpServer>> {
        let state = lock(&self.state);
        state
            .active
            .iter()
            .map(|&index| state.entries[index].server.clone())
            .collect()
    }

    /// Servers with recorded connection failures, in failure occurrence order.
    pub fn failed_servers(&self) -> Vec<Arc<dyn McpServer>> {
        let state = lock(&self.state);
        state
            .failed
            .iter()
            .map(|&index| state.entries[index].server.clone())
            .collect()
    }

    /// Detached server/error pairs keyed by server instance identity, never by name.
    /// Errors remain shared so the original classification and source are preserved.
    #[allow(clippy::type_complexity)]
    pub fn errors(&self) -> Vec<(Arc<dyn McpServer>, Arc<Error>)> {
        lock(&self.state)
            .entries
            .iter()
            .filter_map(|entry| {
                entry
                    .error
                    .as_ref()
                    .map(|error| (entry.server.clone(), error.clone()))
            })
            .collect()
    }

    /// Current connect timeout; defaults to ten seconds.
    pub fn connect_timeout(&self) -> Option<Duration> {
        lock(&self.options).connect_timeout
    }

    /// Changes the timeout used by subsequent connect commands.
    pub fn set_connect_timeout(&self, timeout: Option<Duration>) -> ra_core::error::Result<()> {
        options::validate_timeout(timeout)?;
        lock(&self.options).connect_timeout = timeout;
        Ok(())
    }

    /// Current cleanup timeout; defaults to ten seconds.
    pub fn cleanup_timeout(&self) -> Option<Duration> {
        lock(&self.options).cleanup_timeout
    }

    /// Changes subsequent cleanup commands, including emergency cleanup after manager drop.
    pub fn set_cleanup_timeout(&self, timeout: Option<Duration>) -> ra_core::error::Result<()> {
        options::validate_timeout(timeout)?;
        lock(&self.options).cleanup_timeout = timeout;
        Ok(())
    }

    /// Whether failed servers are excluded from the active view.
    pub fn drop_failed_servers(&self) -> bool {
        lock(&self.options).drop_failed_servers
    }

    /// Selects filtering for the next lifecycle refresh without changing the published snapshot.
    pub fn set_drop_failed_servers(&self, enabled: bool) {
        lock(&self.options).drop_failed_servers = enabled;
    }

    /// Whether connection failures propagate after being recorded.
    pub fn strict(&self) -> bool {
        lock(&self.options).strict
    }

    /// Changes connection failure propagation for subsequent lifecycle operations.
    pub fn set_strict(&self, enabled: bool) {
        lock(&self.options).strict = enabled;
    }

    /// Whether server-returned cancellation errors are suppressed.
    pub fn suppress_cancelled_error(&self) -> bool {
        lock(&self.options).suppress_cancelled_error
    }

    /// Changes cancellation error propagation; dropping a Rust future cannot be suppressed.
    pub fn set_suppress_cancelled_error(&self, enabled: bool) {
        lock(&self.options).suppress_cancelled_error = enabled;
    }

    /// Whether subsequent servers connect concurrently.
    pub fn connect_in_parallel(&self) -> bool {
        lock(&self.options).connect_in_parallel
    }

    /// Changes connection scheduling while keeping existing workers and their task affinity.
    pub fn set_connect_in_parallel(&self, enabled: bool) {
        lock(&self.options).connect_in_parallel = enabled;
    }

    /// Connects servers in configuration order, or concurrently when selected.
    /// Already connected servers are retained without another connect call.
    pub async fn connect_all(&self) -> LifecycleResult<Vec<Arc<dyn McpServer>>> {
        let _lifecycle = self.lifecycle.lock().await;
        let (targets, previous_connected, previous_active) = {
            let mut state = lock(&self.state);
            state.reconcile_workers();
            let previous_connected = state
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| entry.connected.then_some(index))
                .collect::<Vec<_>>();
            let previous_active = state.active.clone();
            state.failed.clear();
            for entry in &mut state.entries {
                entry.error = None;
            }
            (
                state.unconnected(0..state.entries.len()),
                previous_connected,
                previous_active,
            )
        };
        if let Err(error) = self.connect_servers(&targets).await {
            let attempted = if self.connect_in_parallel() {
                targets
            } else {
                let state = lock(&self.state);
                // Python cleans this round's successes followed by its recorded failures.
                // An idle worker from an earlier round is not evidence of an attempted connect.
                let mut attempted: Vec<_> = targets
                    .into_iter()
                    .filter(|&index| state.entries[index].connected)
                    .collect();
                for &index in &state.failed {
                    if !attempted.contains(&index) {
                        attempted.push(index);
                    }
                }
                attempted
            };
            self.cleanup_servers(&attempted).await?;
            lock(&self.state).active = if self.drop_failed_servers() {
                previous_connected
            } else {
                previous_active
            };
            return Err(error);
        }
        self.refresh_active();
        Ok(self.active_servers())
    }

    /// Retries failed servers after cleanup, or cleans and reconnects every server.
    /// In parallel mode, a failed cleanup is retained and prevents a second connection.
    pub async fn reconnect(&self, failed_only: bool) -> LifecycleResult<Vec<Arc<dyn McpServer>>> {
        let _lifecycle = self.lifecycle.lock().await;
        let targets = if failed_only {
            let failed = lock(&self.state).failed.clone();
            self.cleanup_servers(&failed).await?
        } else {
            let all: Vec<_> = (0..lock(&self.state).entries.len()).collect();
            {
                let _refresh = self.refresh_on_exit();
                self.cleanup_servers(&all).await?;
            }
            let mut state = lock(&self.state);
            state.failed.clear();
            for entry in &mut state.entries {
                entry.error = None;
            }
            all
        };
        let refresh = self.refresh_on_exit();
        let targets = lock(&self.state).unconnected(targets);
        self.connect_servers(&targets).await?;
        drop(refresh);
        Ok(self.active_servers())
    }

    /// Cleans servers in reverse configuration order. Cancellation is always recorded.
    /// Suppression applies to server-returned errors; dropping this future cannot be suppressed.
    pub async fn cleanup_all(&self) -> LifecycleResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let _refresh = self.refresh_on_exit();
        let all: Vec<_> = (0..lock(&self.state).entries.len()).collect();
        self.cleanup_servers(&all).await?;
        Ok(())
    }

    fn refresh_on_exit(&self) -> RefreshActive {
        RefreshActive {
            state: self.state.clone(),
            options: self.options.clone(),
        }
    }

    fn refresh_active(&self) {
        drop(self.refresh_on_exit());
    }

    async fn connect_servers(&self, targets: &[usize]) -> LifecycleResult<()> {
        let options = lock(&self.options).clone();
        if options.connect_in_parallel {
            let results = join_all(targets.iter().map(|&index| self.attempt_connect(index))).await;
            if !options.suppress_cancelled_error
                && let Some(error) = results
                    .iter()
                    .filter_map(|result| result.as_ref().err())
                    .find(|error| error.is_cancelled())
            {
                return Err(error.clone());
            }
            if let Some(error) = results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .find(|error| worker::is_panic(error))
            {
                return Err(error.clone());
            }
            if options.strict {
                let state = lock(&self.state);
                for &index in &state.failed {
                    if let Some(error) = &state.entries[index].error
                        && (!error.is_cancelled() || !options.suppress_cancelled_error)
                    {
                        return Err(error.clone());
                    }
                }
            }
        } else {
            for &index in targets {
                self.attempt_connect(index).await?;
            }
        }
        Ok(())
    }

    async fn attempt_connect(&self, index: usize) -> LifecycleResult<()> {
        let mut guard = AttemptGuard::new(self.state.clone(), index, true);
        let result = self.run_connect(index).await;
        guard.finished = true;
        let options = lock(&self.options).clone();
        match result {
            Ok(()) => {
                let mut state = lock(&self.state);
                state.entries[index].connected = true;
                if state.failed.contains(&index) {
                    state.failed.retain(|&failed| failed != index);
                    state.entries[index].error = None;
                }
                Ok(())
            }
            Err(error) => {
                lock(&self.state).record(index, error.clone(), true);
                if worker::is_panic(&error)
                    || (error.is_cancelled() && !options.suppress_cancelled_error)
                    || (!error.is_cancelled() && options.strict && !options.connect_in_parallel)
                {
                    Err(error)
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn run_connect(&self, index: usize) -> LifecycleResult<()> {
        let cleanup_timeout = self.cleanup_timeout();
        let stopping = {
            let mut state = lock(&self.state);
            state.reconcile_workers();
            state.entries[index]
                .worker
                .as_ref()
                .and_then(Worker::stopping_cleanup)
        };
        if let Some(outcome) = stopping {
            worker::wait_cleanup(outcome, cleanup_timeout).await?;
            lock(&self.state).reconcile_workers();
        }
        let connect_timeout = self.connect_timeout();
        let reply = {
            let mut state = lock(&self.state);
            let entry = &mut state.entries[index];
            let worker = entry
                .worker
                .get_or_insert_with(|| Worker::new(entry.server.clone(), self.options.clone()));
            worker.connect(connect_timeout)
        };
        worker::wait_connect(reply).await
    }

    async fn cleanup_servers(&self, targets: &[usize]) -> LifecycleResult<Vec<usize>> {
        let mut cleaned = Vec::new();
        for &index in targets.iter().rev() {
            let mut guard = AttemptGuard::new(self.state.clone(), index, false);
            let timeout = self.cleanup_timeout();
            let parallel = self.connect_in_parallel();
            let outcome = {
                let mut state = lock(&self.state);
                state.reconcile_workers();
                let entry = &mut state.entries[index];
                entry.connected = false;
                // Python's sequential path calls cleanup even before connect and on repeated calls.
                if entry.worker.is_none() && !parallel {
                    entry.worker = Some(Worker::new(entry.server.clone(), self.options.clone()));
                }
                entry
                    .worker
                    .as_mut()
                    .map(|worker| worker.cleanup(timeout, parallel))
            };
            let result = match outcome {
                Some(outcome) => worker::wait_cleanup(outcome, timeout).await,
                None => Ok(()),
            };
            guard.finished = true;
            match result {
                Ok(()) => {
                    lock(&self.state).reconcile_workers();
                    cleaned.push(index);
                }
                Err(error) => {
                    let propagate = worker::is_panic(&error)
                        || (error.is_cancelled() && !lock(&self.options).suppress_cancelled_error);
                    lock(&self.state).record(index, error.clone(), false);
                    if propagate {
                        return Err(error);
                    }
                }
            }
        }
        cleaned.reverse();
        Ok(cleaned)
    }
}

impl State {
    fn reconcile_workers(&mut self) {
        for entry in &mut self.entries {
            let result = entry.worker.as_ref().and_then(Worker::cleanup_result);
            let done = entry.worker.as_ref().is_some_and(Worker::is_done);
            if result.is_some() || done {
                entry.connected = false;
                let cleanup_failed = result.as_ref().is_some_and(Result::is_err);
                let cleanup_pending =
                    result.is_none() && entry.worker.as_ref().is_some_and(Worker::is_stopping);
                if !cleanup_failed
                    && !cleanup_pending
                    && (done || entry.worker.as_ref().is_some_and(Worker::is_retired))
                {
                    entry.worker = None;
                }
            }
        }
    }

    fn unconnected(&self, targets: impl IntoIterator<Item = usize>) -> Vec<usize> {
        targets
            .into_iter()
            .filter(|&index| !self.entries[index].connected)
            .collect()
    }

    fn record(&mut self, index: usize, error: Arc<Error>, connect: bool) {
        let phase = if connect { "connect" } else { "cleanup" };
        // Server errors can contain credentials; retain them for the host, never log their display.
        tracing::warn!(server = %crate::catalog::safe_server_name(self.entries[index].server.name()), phase, code = error.code(), "MCP lifecycle operation failed");
        if connect && !self.failed.contains(&index) {
            self.failed.push(index);
        }
        self.entries[index].error = Some(error);
    }
}

struct RefreshActive {
    state: Arc<Mutex<State>>,
    options: Options,
}

impl Drop for RefreshActive {
    fn drop(&mut self) {
        let drop_failed = lock(&self.options).drop_failed_servers;
        let mut state = lock(&self.state);
        state.reconcile_workers();
        state.active = state
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (!drop_failed || entry.connected).then_some(index))
            .collect();
    }
}

struct AttemptGuard {
    state: Arc<Mutex<State>>,
    index: usize,
    connect: bool,
    finished: bool,
}

impl AttemptGuard {
    fn new(state: Arc<Mutex<State>>, index: usize, connect: bool) -> Self {
        Self {
            state,
            index,
            connect,
            finished: false,
        }
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if !self.finished {
            lock(&self.state).record(
                self.index,
                Arc::new(Error::cancelled("MCP lifecycle waiter dropped")),
                self.connect,
            );
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
