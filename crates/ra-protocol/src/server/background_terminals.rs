//! The host boundary for the commands a thread's tools leave running: what Codex's thread reaches
//! through its unified-exec process manager (`CodexThread::list_background_terminals`,
//! `terminate_background_terminal`, `Op::CleanBackgroundTerminals`).
//!
//! A turn's end does not stop them; they keep running until they end, the client stops them with
//! `thread/backgroundTerminals/terminate` or `clean`, or the thread is shut down. Commands run in
//! the host's executor, not in the provider-neutral thread, so the host that loads a thread hands
//! the server the thread's terminals alongside it; a thread loaded without them has none.

use futures::future::BoxFuture;

/// A command a thread's tools left running: Codex's `BackgroundTerminalInfo`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTerminalInfo {
    pub(crate) item_id: String,
    pub(crate) process_id: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
}

impl BackgroundTerminalInfo {
    /// The command `command`, running in `cwd`, started by the tool call `item_id` and named
    /// `process_id`.
    #[must_use]
    pub fn new(
        item_id: impl Into<String>,
        process_id: impl Into<String>,
        command: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            item_id: item_id.into(),
            process_id: process_id.into(),
            command: command.into(),
            cwd: cwd.into(),
        }
    }

    /// The tool call that started it.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// What the client names it by.
    #[must_use]
    pub fn process_id(&self) -> &str {
        &self.process_id
    }

    /// The command line.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The directory it runs in.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }
}

/// Lists and stops the commands one thread's tools left running.
pub trait ThreadBackgroundTerminals: Send + Sync + 'static {
    /// The commands still running, in the order they started.
    fn list(&self) -> BoxFuture<'_, Vec<BackgroundTerminalInfo>>;

    /// Stops the command `process_id`. Returns whether a running command of that id was stopped.
    fn terminate<'a>(&'a self, process_id: &'a str) -> BoxFuture<'a, bool>;

    /// Stops every command still running.
    fn clean(&self) -> BoxFuture<'_, ()>;
}
