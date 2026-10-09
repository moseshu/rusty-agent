//! The coding host's background terminals, handed to the app server with a thread: what Codex's
//! thread reaches through its unified-exec process manager.
//!
//! The commands `exec_command` leaves running live in the host's [`ProcessManager`], which a turn's
//! end does not touch. A thread loaded with [`CodingBackgroundTerminals`] lets its client list and
//! stop them, and the server stops them all when it shuts the thread down.

use std::{future::Future, pin::Pin, sync::Arc};
use ra_exec::session::{ExecSessionId, ProcessManager};
use ra_protocol::server::{BackgroundTerminalInfo, ThreadBackgroundTerminals};

/// The background terminals of the commands one process manager runs.
#[derive(Clone)]
pub struct CodingBackgroundTerminals {
    process_manager: Arc<ProcessManager>,
}

impl std::fmt::Debug for CodingBackgroundTerminals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodingBackgroundTerminals")
            .finish_non_exhaustive()
    }
}

impl CodingBackgroundTerminals {
    /// The terminals of the commands `process_manager` runs.
    #[must_use]
    pub const fn new(process_manager: Arc<ProcessManager>) -> Self {
        Self { process_manager }
    }
}

/// The future each method returns: `futures`' `BoxFuture`, spelled out.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl ThreadBackgroundTerminals for CodingBackgroundTerminals {
    fn list(&self) -> BoxFuture<'_, Vec<BackgroundTerminalInfo>> {
        Box::pin(async move {
            self.process_manager
                .background_terminals()
                .await
                .into_iter()
                .map(|terminal| {
                    BackgroundTerminalInfo::new(
                        terminal.call_id().unwrap_or_default(),
                        terminal.session_id().as_str(),
                        terminal.command(),
                        terminal
                            .cwd()
                            .map(|cwd| cwd.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    )
                })
                .collect()
        })
    }

    fn terminate<'a>(&'a self, process_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.process_manager
                .terminate_background_terminal(&ExecSessionId::new(process_id))
                .await
        })
    }

    fn clean(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.process_manager.terminate_background_terminals().await;
        })
    }
}
