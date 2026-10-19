//! One command queue and one owning task per server; lifecycle futures are never spawned.

use super::{LifecycleResult, Options};
use crate::McpServer;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use ra_core::error::{Error, ToolErrorKind};
use std::{
    error::Error as _,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

type Outcome = Option<LifecycleResult<()>>;
type Completion = Shared<BoxFuture<'static, LifecycleResult<()>>>;
type CleanupFailure = Arc<Mutex<Option<Arc<Error>>>>;

// Only the manager's worker supervisor can construct this marker. A server may return its own
// JoinError as an ordinary error source, which must retain the server's failure policy.
#[derive(Debug)]
struct WorkerPanic(tokio::task::JoinError);

impl std::fmt::Display for WorkerPanic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MCP lifecycle worker panicked")
    }
}

impl std::error::Error for WorkerPanic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

pub(super) struct ConnectWaiter {
    reply: oneshot::Receiver<LifecycleResult<()>>,
    completion: Completion,
}

pub(super) struct CleanupWaiter {
    outcome: watch::Receiver<Outcome>,
    completion: Completion,
    failure: CleanupFailure,
}

enum Command {
    Connect(Option<Duration>, oneshot::Sender<LifecycleResult<()>>),
    Cleanup(Option<Duration>, bool, watch::Sender<Outcome>),
}

pub(super) struct Worker {
    commands: mpsc::UnboundedSender<Command>,
    outcome: Option<watch::Receiver<Outcome>>,
    retire_after_cleanup: bool,
    stop: CancellationToken,
    completion: Completion,
    cleanup_failure: CleanupFailure,
}

impl Worker {
    pub(super) fn request_shutdown(&self) {
        self.stop.cancel();
    }

    pub(super) fn new(server: Arc<dyn McpServer>, options: Options) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(server, options, receiver, stop.clone()));
        let completion = async move {
            task.await.map_err(|error| {
                if error.is_cancelled() {
                    cancelled()
                } else {
                    Arc::new(
                        Error::caller("MCP lifecycle worker panicked")
                            .with_source(WorkerPanic(error)),
                    )
                }
            })
        }
        .boxed()
        .shared();
        Self {
            commands,
            outcome: None,
            retire_after_cleanup: false,
            stop,
            completion,
            cleanup_failure: Arc::default(),
        }
    }

    pub(super) fn is_stopping(&self) -> bool {
        self.outcome
            .as_ref()
            .is_some_and(|outcome| self.retire_after_cleanup || outcome.borrow().is_none())
    }

    pub(super) fn cleanup_result(&self) -> Outcome {
        if let Some(error) = super::lock(&self.cleanup_failure).as_ref() {
            return Some(Err(error.clone()));
        }
        self.outcome
            .as_ref()
            .and_then(|outcome| outcome.borrow().clone())
    }

    pub(super) fn is_retired(&self) -> bool {
        self.retire_after_cleanup && self.cleanup_result().is_some()
    }

    pub(super) fn is_done(&self) -> bool {
        self.commands.is_closed()
    }

    pub(super) fn connect(&mut self, timeout: Option<Duration>) -> ConnectWaiter {
        let (sender, receiver) = oneshot::channel();
        self.outcome = None;
        // A closed reply must be resolved through the task outcome, not assumed to be cancellation.
        let _ = self.commands.send(Command::Connect(timeout, sender));
        ConnectWaiter {
            reply: receiver,
            completion: self.completion.clone(),
        }
    }

    /// Reads an existing cleanup without changing its deadline or retirement policy.
    pub(super) fn stopping_cleanup(&self) -> Option<CleanupWaiter> {
        self.outcome
            .as_ref()
            .filter(|_| self.is_stopping())
            .map(|outcome| CleanupWaiter {
                outcome: outcome.clone(),
                completion: self.completion.clone(),
                failure: self.cleanup_failure.clone(),
            })
    }

    pub(super) fn cleanup(&mut self, timeout: Option<Duration>, retire: bool) -> CleanupWaiter {
        // A queued or retired cleanup has one shared outcome, even after waiter cancellation.
        if let Some(waiter) = self.stopping_cleanup() {
            return waiter;
        }
        let (sender, outcome) = watch::channel(None);
        self.retire_after_cleanup = retire;
        self.outcome = Some(outcome.clone());
        let _ = self
            .commands
            .send(Command::Cleanup(timeout, retire, sender));
        CleanupWaiter {
            outcome,
            completion: self.completion.clone(),
            failure: self.cleanup_failure.clone(),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Tokio abort cannot run async teardown. Signal the owning task instead.
        self.stop.cancel();
    }
}

async fn run(
    server: Arc<dyn McpServer>,
    options: Options,
    mut commands: mpsc::UnboundedReceiver<Command>,
    stop: CancellationToken,
) {
    let mut needs_cleanup = false;
    loop {
        let command = tokio::select! {
            biased;
            () = stop.cancelled() => None,
            command = commands.recv() => command,
        };
        match command {
            Some(Command::Connect(timeout, reply)) => {
                needs_cleanup = true;
                let result = tokio::select! {
                    biased;
                    () = stop.cancelled() => {
                        emergency_cleanup(&server, &options).await;
                        let _ = reply.send(Err(cancelled()));
                        return;
                    }
                    result = in_task(server.connect(), timeout) => result,
                };
                let _ = reply.send(result);
            }
            Some(Command::Cleanup(timeout, retire, outcome)) => {
                let result = in_task(server.cleanup(), timeout).await;
                if result.is_ok() {
                    needs_cleanup = false;
                }
                // Keep the settled outcome even if every cleanup waiter has been dropped.
                outcome.send_replace(Some(result));
                if retire {
                    return;
                }
            }
            None => {
                if needs_cleanup {
                    emergency_cleanup(&server, &options).await;
                }
                return;
            }
        }
    }
}

async fn emergency_cleanup(server: &Arc<dyn McpServer>, options: &Options) {
    let timeout = super::lock(options).cleanup_timeout;
    if let Err(error) = in_task(server.cleanup(), timeout).await {
        tracing::warn!(server = %crate::catalog::safe_server_name(server.name()), code = error.code(), "MCP emergency cleanup failed");
    }
}

pub(super) async fn in_task<T>(
    future: impl Future<Output = ra_core::error::Result<T>>,
    timeout: Option<Duration>,
) -> LifecycleResult<T> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| {
                Arc::new(Error::tool(
                    ToolErrorKind::Timeout,
                    "mcp",
                    "MCP lifecycle operation timed out",
                ))
            })?
            .map_err(Arc::new),
        None => future.await.map_err(Arc::new),
    }
}

pub(super) fn cancelled() -> Arc<Error> {
    Arc::new(Error::cancelled("MCP lifecycle worker stopped"))
}

pub(super) fn is_panic(error: &Error) -> bool {
    error
        .source()
        .is_some_and(<dyn std::error::Error>::is::<WorkerPanic>)
}

async fn stopped(completion: Completion) -> LifecycleResult<()> {
    completion.await?;
    Err(Arc::new(Error::caller(
        "MCP lifecycle worker stopped before replying",
    )))
}

pub(super) async fn wait_connect(waiter: ConnectWaiter) -> LifecycleResult<()> {
    match waiter.reply.await {
        Ok(result) => result,
        Err(_) => stopped(waiter.completion).await,
    }
}

pub(super) async fn wait_cleanup(
    waiter: CleanupWaiter,
    timeout: Option<Duration>,
) -> LifecycleResult<()> {
    let CleanupWaiter {
        mut outcome,
        completion,
        failure,
    } = waiter;
    let wait = async {
        loop {
            if let Some(result) = outcome.borrow_and_update().clone() {
                return result;
            }
            if outcome.changed().await.is_err() {
                let result = stopped(completion).await;
                if let Err(error) = &result {
                    // A cleanup panic can close its channel before publishing an outcome.
                    // Keep that failure so a later caller cannot replace its owning task.
                    *super::lock(&failure) = Some(error.clone());
                }
                return result;
            }
        }
    };
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or_else(|_| {
                Err(Arc::new(Error::tool(
                    ToolErrorKind::Timeout,
                    "mcp",
                    "MCP cleanup wait timed out",
                )))
            }),
        None => wait.await,
    }
}
