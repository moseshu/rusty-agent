//! A product's per-thread runtime that lives as long as the loaded thread: what Codex keeps in its
//! session services beside the thread, such as its hooks.
//!
//! A factory that prepares one hands it over with [`super::PreparedThread::with_product_runtime`].
//! The server binds it once the thread is built, giving it what only a built thread has — the
//! sender of the thread's ordered events and a weak transcript source — and closes it once the
//! thread has shut down. It is a
//! separate port from [`super::ThreadSettingsAdapter`]: one applies settings, this one runs
//! product machinery for the thread's life.

use async_trait::async_trait;
use ra_core::session::SessionId;
use ra_runtime::thread::ThreadEvent;
use tokio::sync::mpsc;

/// What the server gives a product runtime once its thread is loaded.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ThreadProductBinding {
    thread_id: SessionId,
    events: mpsc::UnboundedSender<ThreadEvent>,
    thread: ra_runtime::thread::WeakAgentThread,
}

impl ThreadProductBinding {
    pub(crate) const fn new(
        thread_id: SessionId,
        events: mpsc::UnboundedSender<ThreadEvent>,
        thread: ra_runtime::thread::WeakAgentThread,
    ) -> Self {
        Self {
            thread_id,
            events,
            thread,
        }
    }

    /// A weak handle for querying and flushing the thread's transcript at dispatch time.
    #[must_use]
    pub fn thread_handle(&self) -> ra_runtime::thread::WeakAgentThread {
        self.thread.clone()
    }

    /// The thread.
    #[must_use]
    pub const fn thread_id(&self) -> &SessionId {
        &self.thread_id
    }

    /// The sender of the thread's events, in order with its turn events. Keeping it does not keep
    /// the thread alive.
    #[must_use]
    pub fn event_sender(&self) -> mpsc::UnboundedSender<ThreadEvent> {
        self.events.clone()
    }
}

/// A product's runtime for one loaded thread (see the module's documentation).
#[async_trait]
pub trait ThreadProductRuntime: Send + Sync + 'static {
    /// Called once, after the thread is built and before anything can start a turn on it.
    fn bind_thread(&self, binding: ThreadProductBinding);

    /// Reconciles product sources before each turn submission, including submissions without
    /// settings overrides.
    async fn activate_turn(&self) {}

    /// Observes ordered thread events after product callbacks have run, for terminal cleanup.
    fn on_thread_event(&self, _event: &ThreadEvent) {}

    /// Called after the thread has shut down. Must be idempotent: a product that already released
    /// what it owns during the shutdown confirms it here.
    async fn close(&self);
}
