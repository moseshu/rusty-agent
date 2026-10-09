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
    background_terminals::ThreadBackgroundTerminals, settings_adapter::ThreadSettingsAdapter,
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
    pub(crate) fn add_thread(
        &self,
        thread: AgentThread,
        info: LoadedThreadInfo,
        adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
        terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
    ) -> LoadedThread {
        let loaded = LoadedThread {
            thread,
            info,
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
