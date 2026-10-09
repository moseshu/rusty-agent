//! The threads the server has loaded: the part of Codex's `ThreadManager` (`core/src/thread_manager.rs`)
//! the thread and turn methods look threads up in.
//!
//! A thread is loaded by the host, which builds it, and is then reachable by its id until it is
//! unloaded. Codex's manager also creates, resumes and forks threads; those come with the thread
//! methods.

use std::{
    collections::HashMap,
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    background_terminals::ThreadBackgroundTerminals, settings_adapter::ThreadSettingsAdapter,
};
use ra_core::session::SessionId;
use ra_runtime::thread::AgentThread;

#[derive(Clone)]
pub(crate) struct LoadedThread {
    pub(crate) thread: AgentThread,
    pub(crate) adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
    /// The commands the thread's tools left running, if its host handed them over.
    pub(crate) terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
    pub(crate) settings_lock: Arc<tokio::sync::Mutex<()>>,
    /// Tells this loading of the thread from a later one under the same id.
    generation: u64,
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
    /// Shuts the thread down and stops what its tools left running: Codex's
    /// `shutdown_session_runtime`, which aborts the thread's tasks and then terminates its
    /// unified-exec processes.
    pub(crate) async fn shutdown(&self) {
        self.thread.shutdown().await;
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
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn add_thread(
        &self,
        thread: AgentThread,
        adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
        terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
    ) -> LoadedThread {
        let loaded = LoadedThread {
            thread,
            adapter,
            terminals,
            settings_lock: Arc::new(tokio::sync::Mutex::new(())),
            generation: self.next_generation.fetch_add(1, Ordering::Relaxed),
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

    /// Whether any loaded thread is running a turn.
    pub(crate) fn any_running(&self) -> bool {
        self.threads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|loaded| loaded.is_running())
    }
}
