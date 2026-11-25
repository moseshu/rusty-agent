// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The threads the server has loaded: the part of Codex's `ThreadManager` (`core/src/thread_manager.rs`)
//! the thread and turn methods look threads up in.
//!
//! A thread is loaded by `thread/start`, `thread/resume` or `thread/fork`, which have the host's
//! [`ThreadFactory`](super::ThreadFactory) prepare it, and is then reachable by its id until it is
//! unloaded. Alongside the live thread the manager keeps what a client is told of it that the
//! thread itself does not know: whether it keeps a rollout, when it was created, what it was
//! forked from and the client's classification of it.

use std::{
    collections::HashMap,
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    background_terminals::ThreadBackgroundTerminals, dynamic_tool_calls::DynamicToolCalls,
    settings_adapter::ThreadSettingsAdapter,
};
use ra_core::session::SessionId;
use ra_runtime::thread::AgentThread;

/// What a client is told of a loaded thread that the live thread does not know.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoadedThreadInfo {
    /// The thread keeps no rollout: Codex's `ephemeral`.
    pub(crate) ephemeral: bool,
    /// When it was created, in seconds since the Unix epoch.
    pub(crate) created_at: i64,
    /// The thread it was forked from, if it is a fork.
    pub(crate) forked_from_id: Option<SessionId>,
    /// The client's classification of where it came from.
    pub(crate) thread_source: Option<String>,
    /// The persisted product source; missing values stay unknown.
    pub(crate) source: Option<serde_json::Value>,
    /// Its preview, for a thread that keeps no rollout to read one from.
    pub(crate) preview: String,
}

impl LoadedThreadInfo {
    /// A thread loaded from outside the thread methods, whose rollout the server does not know of.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn unknown() -> Self {
        Self {
            ephemeral: true,
            created_at: super::outgoing::now_unix_timestamp_ms() / 1000,
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct LoadedThread {
    pub(crate) thread: AgentThread,
    pub(crate) info: LoadedThreadInfo,
    pub(crate) adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
    /// The commands the thread's tools left running, if its host handed them over.
    pub(crate) terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
    /// The product's runtime for the thread, if its host prepared one.
    pub(crate) product: Option<Arc<dyn super::ThreadProductRuntime>>,
    /// The calls of the tools its client provides that wait for the client's answer.
    pub(crate) dynamic_tool_calls: Arc<DynamicToolCalls>,
    pub(crate) settings_lock: Arc<tokio::sync::Mutex<()>>,
    /// Tells this loading of the thread from a later one under the same id.
    generation: u64,
    /// The thread's close, shared by everyone closing it.
    closing: Arc<std::sync::Mutex<Option<SharedThreadClose>>>,
}

type SharedThreadClose = futures::future::Shared<futures::future::BoxFuture<'static, ThreadClose>>;

/// How a loaded thread's close ended.
#[derive(Debug, Clone, Default)]
pub(crate) struct ThreadClose {
    /// The agents of its tree confirmed closed.
    pub(crate) closed_agents: Vec<ra_runtime::agent::control::AgentInstance>,
    /// What is still open, and why.
    pub(crate) failures: Vec<String>,
}

impl ThreadClose {
    pub(crate) fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

impl std::fmt::Debug for LoadedThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedThread")
            .field("thread", &self.thread)
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for LoadedThread {
    type Target = AgentThread;
    fn deref(&self) -> &AgentThread {
        &self.thread
    }
}

impl LoadedThread {
    /// Which load of its id this is: a thread loaded again under the same id is another.
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Closes the thread completely: shuts it down — its `SessionEnd` hooks first — then lets go
    /// of every agent of its tree, waits for the spawns its tree froze, and closes its rollout.
    ///
    /// Every caller — an idle unload, a reload, an archive or delete and their retries — shares
    /// one close: a caller that arrives while it runs waits for it and gets its outcome. Only a
    /// close that ended with something left open is attempted again, continuing where the last
    /// one stopped; the thread stays loaded until a close succeeds, so a retry can find what is
    /// still open.
    pub(crate) async fn close(&self) -> ThreadClose {
        use futures::FutureExt;
        let closing = {
            let mut closing = self
                .closing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let reusable = closing
                .as_ref()
                .filter(|close| close.peek().is_none_or(ThreadClose::is_complete));
            if let Some(close) = reusable {
                close.clone()
            } else {
                let thread = self.clone();
                let task = tokio::spawn(async move { thread.close_once().await });
                let close = async move {
                    task.await.unwrap_or_else(|error| ThreadClose {
                        closed_agents: Vec::new(),
                        failures: vec![format!("the thread's close failed: {error}")],
                    })
                }
                .boxed()
                .shared();
                *closing = Some(close.clone());
                close
            }
        };
        closing.await
    }

    /// Whether a close of the thread has started: its writer may have been let go of already.
    pub(crate) fn close_started(&self) -> bool {
        self.closing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    async fn close_once(self) -> ThreadClose {
        let mut close = ThreadClose::default();
        self.shutdown().await;
        if let Some(control) = self.thread.agent_control() {
            // The tree's shutdown ran within the thread's own; its report is read, not retried, so
            // what it left open fails this close and a later close tries again.
            for report in [
                control.shutdown_report().await,
                control.settle_spawns().await,
            ] {
                close.closed_agents.extend(report.closed().iter().cloned());
                close.failures.extend(
                    report
                        .failed()
                        .iter()
                        .map(|(agent, error)| format!("agent {}: {error}", agent.session_id())),
                );
            }
        }
        if let Err(error) = self.thread.close_rollout().await {
            close
                .failures
                .push(format!("thread {}: {error}", self.thread_id()));
        }
        close
    }

    /// Shuts the thread down and stops what its tools left running: Codex's
    /// `shutdown_session_runtime`, which aborts the thread's tasks and then terminates its
    /// unified-exec processes.
    pub(crate) async fn shutdown(&self) {
        self.thread.shutdown().await;
        if let Some(product) = &self.product {
            product.close().await;
        }
        if let Some(terminals) = &self.terminals {
            terminals.clean().await;
        }
    }
}

/// The loaded threads, by id.
#[derive(Debug, Default)]
pub(crate) struct ThreadManager {
    threads: RwLock<HashMap<SessionId, LoadedThread>>,
    next_generation: AtomicU64,
}

impl ThreadManager {
    /// Makes `thread` reachable by its id, in place of any thread loaded under the same id.
    pub(crate) fn add_thread(
        self: &Arc<Self>,
        thread: AgentThread,
        info: LoadedThreadInfo,
        adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
        terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
        product: Option<Arc<dyn super::ThreadProductRuntime>>,
        dynamic_tool_calls: Arc<DynamicToolCalls>,
    ) -> LoadedThread {
        if let Some(control) = thread.agent_control() {
            let manager = Arc::downgrade(self);
            control.set_live_agent_lookup(move |session_id| {
                let manager = manager.upgrade()?;
                manager
                    .threads()
                    .iter()
                    .find_map(|loaded| loaded.thread.agent_control()?.live_agent(session_id))
            });
        }
        let loaded = LoadedThread {
            thread,
            info,
            adapter,
            terminals,
            product,
            dynamic_tool_calls,
            settings_lock: Arc::new(tokio::sync::Mutex::new(())),
            generation: self.next_generation.fetch_add(1, Ordering::Relaxed),
            closing: Arc::default(),
        };
        self.threads
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(loaded.thread_id().clone(), loaded.clone());
        loaded
    }

    /// The thread loaded under `thread_id`, if there is one: Codex's `get_thread`.
    pub(crate) fn get_thread(&self, thread_id: &SessionId) -> Option<LoadedThread> {
        self.threads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(thread_id)
            .cloned()
    }

    /// Unloads `thread` unless another thread has been loaded under its id since: Codex's
    /// `remove_thread_if_matches`. Returns whether it did.
    pub(crate) fn remove_thread_if_matches(&self, thread: &LoadedThread) -> bool {
        let mut threads = self.threads.write().unwrap_or_else(PoisonError::into_inner);
        if threads
            .get(thread.thread_id())
            .is_some_and(|loaded| loaded.generation == thread.generation)
        {
            threads.remove(thread.thread_id());
            true
        } else {
            false
        }
    }

    /// Every loaded thread.
    pub(crate) fn threads(&self) -> Vec<LoadedThread> {
        self.threads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// The ids of the loaded threads: Codex's `list_thread_ids`.
    pub(crate) fn list_thread_ids(&self) -> Vec<SessionId> {
        self.threads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Whether any loaded thread is running a turn.
    pub(crate) fn any_running(&self) -> bool {
        self.threads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|loaded| loaded.is_running())
    }
}
