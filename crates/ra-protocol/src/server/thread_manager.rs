//! The threads the server has loaded: the part of Codex's `ThreadManager` (`core/src/thread_manager.rs`)
//! the turn methods look threads up in.
//!
//! A thread is loaded by the host, which builds it, and is then reachable by its id. Codex's
//! manager also creates, resumes, forks and unloads threads; those come with the thread methods.

use std::{
    collections::HashMap,
    sync::{PoisonError, RwLock},
};

use super::settings_adapter::ThreadSettingsAdapter;
use ra_core::session::SessionId;
use ra_runtime::thread::AgentThread;

#[derive(Clone)]
pub(crate) struct LoadedThread {
    pub(crate) thread: AgentThread,
    pub(crate) adapter: Option<std::sync::Arc<dyn ThreadSettingsAdapter>>,
    pub(crate) settings_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
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

/// The loaded threads, by id.
#[derive(Debug, Default)]
pub(crate) struct ThreadManager {
    threads: RwLock<HashMap<SessionId, LoadedThread>>,
}

impl ThreadManager {
    /// Makes `thread` reachable by its id, in place of any thread loaded under the same id.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn add_thread(
        &self,
        thread: AgentThread,
        adapter: Option<std::sync::Arc<dyn ThreadSettingsAdapter>>,
    ) {
        self.threads
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                thread.thread_id().clone(),
                LoadedThread {
                    thread,
                    adapter,
                    settings_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                },
            );
    }

    /// The thread loaded under `thread_id`, if there is one: Codex's `get_thread`.
    pub(crate) fn get_thread(&self, thread_id: &SessionId) -> Option<LoadedThread> {
        self.threads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(thread_id)
            .cloned()
    }
}
