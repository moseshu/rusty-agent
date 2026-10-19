//! The gate each connection's requests pass through: Codex's `ConnectionRpcGate`
//! (`app-server/src/connection_rpc_gate.rs`).
//!
//! A handler runs only while its connection's gate is open, and is counted until it finishes.
//! Closing the gate — the connection has gone — keeps handlers still waiting, such as those queued
//! behind another request of the same thread, from starting at all, while those already running
//! finish; shutting it down also waits for them.
//!
//! Codex also closes the gate of a connection whose login changed under it; logins are account
//! machinery and not ported.

use std::future::Future;

use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

/// Lets a connection's handlers start until the connection goes, and counts those that did.
#[derive(Debug)]
pub(crate) struct ConnectionRpcGate {
    accepting: Mutex<bool>,
    tasks: TaskTracker,
}

impl ConnectionRpcGate {
    pub(crate) fn new() -> Self {
        Self {
            accepting: Mutex::new(true),
            tasks: TaskTracker::new(),
        }
    }

    /// Runs `future` if the gate is open, counting it until it finishes; drops it unpolled if not.
    pub(crate) async fn run<F>(&self, future: F)
    where
        F: Future<Output = ()>,
    {
        let token = {
            let accepting = self.accepting.lock().await;
            if !*accepting || self.is_closed() {
                return;
            }
            self.tasks.token()
        };

        future.await;
        drop(token);
    }

    /// Transfers work from a running handler to its own task without losing drain ownership.
    /// The caller must already hold this gate's admission through `run`.
    pub(crate) fn spawn_admitted(&self, future: impl Future<Output = ()> + Send + 'static) {
        let token = self.tasks.token();
        tokio::spawn(async move {
            future.await;
            drop(token);
        });
    }

    /// Whether the gate has been closed.
    pub(crate) fn is_closed(&self) -> bool {
        self.tasks.is_closed()
    }

    /// Lets no more handlers start. Those running carry on.
    pub(crate) async fn close(&self) {
        let mut accepting = self.accepting.lock().await;
        *accepting = false;
        self.tasks.close();
    }

    /// Closes the gate and waits for the handlers running to finish.
    pub(crate) async fn shutdown(&self) {
        self.close().await;
        self.tasks.wait().await;
    }

    /// Whether handlers may still start.
    #[cfg(feature = "test-api")]
    pub(crate) async fn is_accepting(&self) -> bool {
        *self.accepting.lock().await
    }

    /// How many handlers are running.
    #[cfg(feature = "test-api")]
    pub(crate) fn inflight_count(&self) -> usize {
        self.tasks.len()
    }
}

impl Default for ConnectionRpcGate {
    fn default() -> Self {
        Self::new()
    }
}
