// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Account-independent startup diagnostics from Codex's `mcp_init_error_display`.

use std::{
    sync::{Arc, PoisonError, Weak},
    time::Duration,
};

use async_trait::async_trait;
use futures::FutureExt;
use ra_core::error::Result;
use ra_mcp::tool::McpToolClientSource;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{Entry, EntryState, StartupAttempt, StartupResult, Status};

use ra_core::error::{Error, ProtocolErrorKind, ToolErrorKind};

#[derive(Clone, Copy)]
enum StartupIntent {
    Initial,
    Recovery,
    Explicit { force: bool },
}

pub(super) fn startup_error_display(
    server_name: &str,
    timeout: Option<Duration>,
    phase: &str,
    error: &Error,
) -> String {
    let message = error.to_string();
    let timed_out = matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::Timeout,
            ..
        } | Error::Protocol {
            kind: ProtocolErrorKind::Timeout,
            ..
        }
    ) || message.contains("request timed out")
        || message.contains("timed out handshaking with MCP server")
        || message.contains("MCP client startup timed out");
    if phase == "connect" && timed_out {
        let elapsed = timeout.map_or_else(
            || "during startup".to_owned(),
            |timeout| format!("after {} seconds", timeout.as_secs_f64()),
        );
        // This host takes typed registrations rather than Codex's config.toml.
        format!(
            "MCP client for `{server_name}` timed out {elapsed}. \
             Increase the connection timeout with \
             McpServerManagerOptions::with_connect_timeout and \
             McpServerConfig::with_lifecycle_options."
        )
    } else if phase == "connect" {
        // Transport display is already sanitized. The original source chain stays with the host.
        format!("MCP client for `{server_name}` failed to start: {message}")
    } else {
        format!("MCP client for `{server_name}` failed during {phase}: {message}")
    }
}

impl Entry {
    pub(super) fn startup_is_dormant(&self) -> bool {
        !*self.startup_trigger.borrow()
    }

    pub(super) fn invalidate_startup(&self, stopped: bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.epoch = state.epoch.wrapping_add(1);
        state.startup_cancel.cancel();
        state.startup_cancel = CancellationToken::new();
        state.pending = None;
        state.stopped = stopped;
        state.consecutive_failures = 0;
        state.retry_not_before = None;
        self.startup_trigger.send_replace(false);
    }

    pub(super) fn retry_background(self: &Arc<Self>) -> Option<StartupAttempt> {
        self.launch_startup(false, true)
    }

    fn can_defer(&self, state: &EntryState) -> bool {
        self.lazy_startup
            && state
                .cached_catalog
                .as_ref()
                .is_some_and(|catalog| catalog.allows_deferred_startup(&self.filter()))
    }

    pub(super) fn start_background(self: &Arc<Self>) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let initial = state.status == Status::NotStarted;
        let deferred = self.can_defer(&state);
        drop(state);
        if initial {
            self.launch_startup(deferred, false);
        }
    }

    pub(super) async fn wait_for_catalog(self: &Arc<Self>) {
        self.retry_background();
        self.start_background();
        let pending = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.enabled || state.stopped || state.cached_catalog.is_some() {
                return;
            }
            state.pending.clone()
        };
        if let Some(mut pending) = pending {
            while pending.borrow().is_none() {
                if pending.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    pub(super) fn launch_startup(
        self: &Arc<Self>,
        deferred: bool,
        recovery: bool,
    ) -> Option<StartupAttempt> {
        let intent = if recovery {
            StartupIntent::Recovery
        } else {
            StartupIntent::Initial
        };
        self.reserve_startup(deferred, intent)
    }

    pub(super) fn launch_explicit_startup(self: &Arc<Self>, force: bool) -> Option<StartupAttempt> {
        self.reserve_startup(false, StartupIntent::Explicit { force })
    }

    fn reserve_startup(
        self: &Arc<Self>,
        deferred: bool,
        intent: StartupIntent,
    ) -> Option<StartupAttempt> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.enabled || state.stopped {
            return None;
        }
        if let Some(pending) = &state.pending {
            return Some(pending.clone());
        }
        if matches!(intent, StartupIntent::Initial) && state.status != Status::NotStarted {
            return None;
        }
        if matches!(intent, StartupIntent::Recovery)
            && (!self.automatic_retry
                || !matches!(state.status, Status::Failed | Status::Cancelled)
                || state.cleanup_failed
                || state
                    .retry_not_before
                    .is_some_and(|deadline| tokio::time::Instant::now() < deadline))
        {
            return None;
        }
        let (sender, receiver) = watch::channel(None);
        state.pending = Some(receiver.clone());
        let epoch = state.epoch;
        let cancel = state.startup_cancel.clone();
        self.startup_trigger.send_replace(!deferred);
        let trigger = deferred.then(|| self.startup_trigger.subscribe());
        drop(state);
        tokio::spawn(run_startup(
            Arc::downgrade(self),
            epoch,
            intent,
            cancel,
            trigger,
            sender,
        ));
        Some(receiver)
    }
    fn finish_startup(&self, epoch: u64, recovery: bool) -> StartupResult {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.epoch != epoch {
            return Err(Arc::new(Error::cancelled("MCP startup replaced")));
        }
        state.pending = None;
        let result = if state.status == Status::Connected {
            self.server.current_client().map_err(Arc::new)
        } else {
            Err(state.failure.as_ref().map_or_else(
                || Arc::new(Error::config("MCP startup failed")),
                |(_, error)| error.clone(),
            ))
        };
        if recovery && result.is_err() {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            let backoff =
                Duration::from_secs(1_u64 << state.consecutive_failures.saturating_sub(1).min(5))
                    .min(Duration::from_secs(30));
            state.retry_not_before = Some(tokio::time::Instant::now() + backoff);
        }
        result
    }
}

async fn run_startup(
    entry: Weak<Entry>,
    epoch: u64,
    intent: StartupIntent,
    cancel: CancellationToken,
    trigger: Option<watch::Receiver<bool>>,
    sender: watch::Sender<Option<StartupResult>>,
) {
    if let Some(mut trigger) = trigger {
        let started = tokio::select! {
            result = trigger.wait_for(|started| *started) => result.is_ok(),
            () = cancel.cancelled() => false,
        };
        if !started {
            sender.send_replace(Some(Err(Arc::new(Error::cancelled(
                "MCP deferred startup cancelled",
            )))));
            return;
        }
    }
    let Some(entry) = entry.upgrade() else {
        return;
    };
    let _operation = tokio::select! {
        guard = entry.operation.lock() => guard,
        () = cancel.cancelled() => {
            sender.send_replace(Some(Err(Arc::new(Error::cancelled("MCP startup cancelled")))));
            return;
        }
    };
    if cancel.is_cancelled() {
        sender.send_replace(Some(Err(Arc::new(Error::cancelled(
            "MCP startup cancelled",
        )))));
        return;
    }
    let force = matches!(intent, StartupIntent::Explicit { force: true });
    let outcome = std::panic::AssertUnwindSafe(entry.connect_now(force))
        .catch_unwind()
        .await;
    if outcome.is_err() {
        entry.fail(
            "runtime",
            Arc::new(Error::config("MCP startup task panicked")),
        );
    }
    sender.send_replace(Some(
        entry.finish_startup(epoch, matches!(intent, StartupIntent::Recovery)),
    ));
}

pub(super) struct DeferredClient {
    pub(super) entry: Arc<Entry>,
    pub(super) epoch: u64,
    pub(super) resolved: tokio::sync::OnceCell<ra_mcp::client::McpClient>,
}

// Adapter primary messages are already sanitized. Keep host display labels and the original
// source chain out of the model's diagnostic, while retaining that chain for the host.
fn startup_failure(error: Arc<Error>) -> Error {
    let mut primary = error.as_ref();
    while let Error::Run { error, .. } = primary {
        primary = error;
    }
    let message = match primary {
        Error::Config { message, .. }
        | Error::Caller { message, .. }
        | Error::Provider { message, .. }
        | Error::Tool { message, .. }
        | Error::Sandbox { message, .. }
        | Error::Session { message, .. }
        | Error::Protocol { message, .. }
        | Error::Budget { message, .. }
        | Error::Guardrail { message, .. }
        | Error::InvalidFinalOutput { message, .. } => format!("MCP startup failed: {message}"),
        Error::Cancelled { .. } => "MCP startup cancelled".to_owned(),
        _ => "MCP startup failed".to_owned(),
    };
    Error::config(message).with_source(error)
}

#[async_trait]
impl McpToolClientSource for DeferredClient {
    async fn client(&self) -> Result<ra_mcp::client::McpClient> {
        let client = self
            .resolved
            .get_or_try_init(|| async {
                {
                    let state = self
                        .entry
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if !state.enabled || state.stopped || state.epoch != self.epoch {
                        return Err(Error::tool(
                            ra_core::error::ToolErrorKind::ExecutionFailed,
                            "mcp",
                            "MCP tool binding was replaced",
                        ));
                    }
                }
                self.entry.startup_trigger.send_replace(true);
                let (pending, status) = {
                    let state = self
                        .entry
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    (state.pending.clone(), state.status)
                };
                if status == Status::Connected {
                    return self.entry.server.current_client();
                }
                let recovery = matches!(status, Status::Failed | Status::Cancelled);
                let mut attempt = pending
                    .or_else(|| self.entry.launch_startup(false, recovery))
                    .ok_or_else(|| {
                        let state = self
                            .entry
                            .state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        state.failure.as_ref().map_or_else(
                            || Error::config("MCP startup recovery is unavailable or cooling down"),
                            |(_, error)| startup_failure(error.clone()),
                        )
                    })?;
                let result = loop {
                    if let Some(result) = attempt.borrow().clone() {
                        break result;
                    }
                    attempt.changed().await.map_err(|error| {
                        Error::config("MCP startup worker closed").with_source(error)
                    })?;
                };
                let state = self
                    .entry
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if state.epoch != self.epoch || !state.enabled || state.stopped {
                    return Err(Error::tool(
                        ra_core::error::ToolErrorKind::ExecutionFailed,
                        "mcp",
                        "MCP tool binding was replaced",
                    ));
                }
                result.map_err(startup_failure)
            })
            .await?;
        Ok(client.clone())
    }
}
