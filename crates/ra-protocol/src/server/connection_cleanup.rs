//! The cleanup of closed connections, run beside the server's reading loop: Codex's
//! `ConnectionCleanupTasks` (`app-server/src/connection_cleanup.rs`).
//!
//! Cleaning up after a connection waits for its handlers to finish, which may take a while; the
//! reading loop hands the work to a task of its own, reaps the tasks as they finish, and when the
//! server stops waits for those left — or, when the stop was forced, drops them.

use std::future::{Future, pending};

use tokio::task::{JoinError, JoinSet};

/// The cleanup tasks of the connections that have closed.
pub(crate) struct ConnectionCleanupTasks {
    tasks: JoinSet<()>,
}

impl ConnectionCleanupTasks {
    pub(crate) fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    pub(crate) fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(future);
    }

    /// Waits for the next task to finish; never returns while there is none.
    pub(crate) async fn reap_next(&mut self) {
        if self.tasks.is_empty() {
            pending::<()>().await;
        }
        if let Some(result) = self.tasks.join_next().await {
            log_cleanup_result(result);
        }
    }

    /// Waits for every task to finish.
    pub(crate) async fn drain(&mut self) {
        while let Some(result) = self.tasks.join_next().await {
            log_cleanup_result(result);
        }
    }

    /// Drops every task unfinished.
    pub(crate) fn abort(&mut self) {
        self.tasks.abort_all();
    }
}

fn log_cleanup_result(result: Result<(), JoinError>) {
    if let Err(err) = result
        && !err.is_cancelled()
    {
        tracing::warn!("connection cleanup task failed: {err}");
    }
}
