//! How the server builds the threads `thread/start`, `thread/resume` and `thread/fork` load: the
//! host's part of Codex's `ThreadManager::start_thread` and its resume and fork variants
//! (`core/src/thread_manager.rs`).
//!
//! Codex's server builds every session itself, from configuration it loads: the model, the tools,
//! the instructions. A thread here runs an agent and models only its host has, so the host hands
//! the server a [`ThreadFactory`], and the server asks it for each thread it loads. The factory
//! prepares the thread — its agent, model resolver, run configuration and services, with the
//! settings adapter and background terminals it is loaded with — and the server does the rest, as
//! Codex's thread manager does: it applies the settings the request and the thread's history
//! carry, gives the thread its history and the recorder of its rollout, and builds it.
//!
//! A factory is asked once per thread loaded, with the thread's id and why it is loaded; one that
//! fails leaves nothing loaded, and the request fails with its error.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::session::SessionId;
use ra_runtime::thread::AgentThreadBuilder;

use super::{
    background_terminals::ThreadBackgroundTerminals, settings_adapter::ThreadSettingsAdapter,
};

/// Why a thread is being loaded.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadOrigin {
    /// A new thread, for `thread/start`.
    Start,
    /// A stored thread loaded again, for `thread/resume`.
    Resume,
    /// A new thread on a copy of another's history, for `thread/fork`.
    Fork {
        /// The thread it was forked from.
        source: SessionId,
    },
}

/// The thread a factory is asked to prepare.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFactoryRequest {
    thread_id: SessionId,
    origin: ThreadOrigin,
}

impl ThreadFactoryRequest {
    pub(crate) const fn new(thread_id: SessionId, origin: ThreadOrigin) -> Self {
        Self { thread_id, origin }
    }

    /// The thread's id, which its builder must carry.
    #[must_use]
    pub const fn thread_id(&self) -> &SessionId {
        &self.thread_id
    }

    /// Why it is loaded.
    #[must_use]
    pub const fn origin(&self) -> &ThreadOrigin {
        &self.origin
    }
}

/// A thread a factory prepared, not yet built.
///
/// The server gives the builder the thread's history and rollout recorder before building it,
/// replacing any the factory set.
#[must_use]
pub struct PreparedThread {
    builder: AgentThreadBuilder,
    adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
    terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
}

impl std::fmt::Debug for PreparedThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedThread")
            .field("thread_id", self.builder.thread_id())
            .field("has_adapter", &self.adapter.is_some())
            .field("has_terminals", &self.terminals.is_some())
            .finish()
    }
}

impl PreparedThread {
    /// The thread `builder` builds, with no product settings and no background terminals.
    pub const fn new(builder: AgentThreadBuilder) -> Self {
        Self {
            builder,
            adapter: None,
            terminals: None,
        }
    }

    /// Applies product settings — approval and sandbox policy, named permissions — through
    /// `adapter`, which the thread is then loaded with. Without one, a request that sets them is
    /// refused.
    pub fn with_settings_adapter(mut self, adapter: Arc<dyn ThreadSettingsAdapter>) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// Hands over the commands the thread's tools leave running, for
    /// `thread/backgroundTerminals/*` and for stopping them with the thread.
    pub fn with_background_terminals(
        mut self,
        terminals: Arc<dyn ThreadBackgroundTerminals>,
    ) -> Self {
        self.terminals = Some(terminals);
        self
    }

    pub(crate) fn into_parts(self) -> PreparedParts {
        (self.builder, self.adapter, self.terminals)
    }
}

/// A prepared thread taken apart: its builder, settings adapter and background terminals.
pub(crate) type PreparedParts = (
    AgentThreadBuilder,
    Option<Arc<dyn ThreadSettingsAdapter>>,
    Option<Arc<dyn ThreadBackgroundTerminals>>,
);

/// Prepares the threads the server loads (see the module's documentation).
#[async_trait]
pub trait ThreadFactory: Send + Sync + 'static {
    /// Prepares the thread `request` describes. Its builder must be for
    /// [`ThreadFactoryRequest::thread_id`].
    ///
    /// # Errors
    ///
    /// Returns an error if the thread cannot be prepared; the request that asked for it fails with
    /// the error, and nothing is loaded.
    async fn prepare_thread(
        &self,
        request: &ThreadFactoryRequest,
    ) -> ra_core::error::Result<PreparedThread>;
}
