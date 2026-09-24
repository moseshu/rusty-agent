//! Taking a workspace's ephemeral mounts down around an operation, and putting them back.
//!
//! A snapshot has to see the workspace without anybody else's storage attached to it, so the
//! mounts come off first and go back on afterwards — in reverse order, because a mount nested
//! inside another has to be reattached after the one it sits in.
//!
//! # A transition that started is finished
//!
//! Detaching or reattaching a mount is not something that can be abandoned half way: a mount
//! command interrupted mid-flight leaves the path in a state nobody can describe. So once a
//! transition starts it runs to completion even when the caller gives up, and a caller that gave
//! up during teardown still gets whatever was detached reattached.
//!
//! The reference does this by shielding each transition from its caller's cancellation. Dropping a
//! Rust future cannot wait for anything, so the whole sequence runs in a task of its own on the
//! Tokio runtime, and the caller's future only waits for it. Dropping the caller's future tells the
//! task the caller is gone: the task stops before starting the operation, or drops the operation if
//! it is already running, and still reattaches what it detached.
//!
//! A caller that leaves in the same instant a transition or the operation finishes is still seen
//! as having left: whichever of the two is read first, the other is checked before the sequence
//! moves on, so the operation does not start for a caller that is gone and mounts are put back
//! even where a successful operation asked for them to be left off.
//!
//! **A caller that gave up does not hear how it ended.** Where the reference raises an error past
//! the cancellation — a teardown that failed, a remount that failed — there is nobody left here to
//! return it to, so it is logged instead.
//!
//! # A panic is cleaned up after before it goes on
//!
//! The reference catches every exception, cancellation included, and puts mounts back before
//! re-raising. A panic in the operation or in a transition is caught the same way, handled as that
//! step failing — the mounts that came off go back on, a session left in an unknown state is
//! terminated — and then resumed. This holds where panics unwind; under `panic = "abort"` nothing
//! runs after one.
//!
//! # A transition whose outcome is unknown ends the session
//!
//! When a mount could not be detached, or could not be put back, whether it is still attached is
//! not known. The session is then terminated through
//! [`SandboxSession::terminate_ambiguous_mount_transition`] rather than left running over a
//! workspace that might be recording somebody else's storage.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use ra_core::sandbox::{
    ErrorCode, Mount, OpName, PosixPath, SandboxError, SandboxResult, SandboxSession,
};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::MountLifecycle;

/// Which failure a mount transition is reported as.
///
/// The operation being wrapped decides it: a snapshot that could not take its mounts down failed to
/// read the workspace, a hydration that could not put them back failed to write it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveErrorKind {
    /// [`ra_core::sandbox::ErrorCode::WorkspaceArchiveReadError`].
    ArchiveRead,
    /// [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`].
    ArchiveWrite,
    /// [`ra_core::sandbox::ErrorCode::WorkspaceStartError`].
    Start,
}

impl ArchiveErrorKind {
    fn raise(self, path: &str) -> SandboxError {
        match self {
            Self::ArchiveRead => SandboxError::workspace_archive_read(path),
            Self::ArchiveWrite => SandboxError::workspace_archive_write(path),
            Self::Start => SandboxError::workspace_start(path, None),
        }
    }
}

/// How [`with_ephemeral_mounts_removed`] reports what goes wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralMountRemoval {
    /// The path a transition failure is reported against.
    pub error_path: String,
    /// What a transition failure is reported as.
    pub error_kind: ArchiveErrorKind,
    /// Where a remount failure records the operation's own failure, when there was one.
    ///
    /// The remount failure is what is returned, because it is what leaves the workspace in an
    /// unknown state; the operation's failure is kept under this key so it is not lost. Only a
    /// workspace IO failure is recorded, and only its message.
    pub operation_error_context_key: Option<String>,
    /// Whether mounts are put back after the operation succeeds.
    ///
    /// `false` is for an operation that is about to replace the session, where reattaching to a
    /// workspace that is being thrown away would be wasted work. Mounts are always put back after a
    /// failure.
    pub restore_on_success: bool,
}

impl EphemeralMountRemoval {
    /// Reports failures as `error_kind` against `error_path`, and puts mounts back afterwards.
    #[must_use]
    pub fn new(error_path: impl Into<String>, error_kind: ArchiveErrorKind) -> Self {
        Self {
            error_path: error_path.into(),
            error_kind,
            operation_error_context_key: None,
            restore_on_success: true,
        }
    }

    /// Records the operation's failure under `key` on a remount failure.
    #[must_use]
    pub fn recording_operation_error_as(mut self, key: impl Into<String>) -> Self {
        self.operation_error_context_key = Some(key.into());
        self
    }

    /// Leaves mounts detached after the operation succeeds.
    #[must_use]
    pub const fn without_restore_on_success(mut self) -> Self {
        self.restore_on_success = false;
        self
    }
}

/// Runs `operation` with the session's ephemeral mounts detached, and reattaches them afterwards.
///
/// The mounts come off deepest first and go back on in reverse. If one cannot be detached the
/// operation does not run; the ones already detached are put back and the session is terminated,
/// because whether the one that failed is still attached is not known.
///
/// The operation is started by calling `operation` once every mount is off. It runs in a task of
/// its own, so it must own what it uses.
///
/// # Errors
///
/// In order of precedence: a remount failure (with the session terminated, and the operation's own
/// failure recorded as [`EphemeralMountRemoval::operation_error_context_key`] says), a teardown
/// failure, then the operation's own failure. A transition failure is reported as
/// [`EphemeralMountRemoval::error_kind`] with the strategy's failure as its cause; the session is
/// marked `terminal_cleanup_failed` when terminating it failed too.
///
/// # Panics
///
/// Re-raises a panic from the operation or from a mount transition, once the mounts that came off
/// are back on and a session left in an unknown state has been terminated.
pub async fn with_ephemeral_mounts_removed<T, F, Fut>(
    session: Arc<dyn SandboxSession>,
    lifecycle: Arc<dyn MountLifecycle>,
    operation: F,
    removal: EphemeralMountRemoval,
) -> SandboxResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = SandboxResult<T>> + Send + 'static,
{
    let targets = ephemeral_targets(session.as_ref())?;
    let (_cancel_on_drop, cancelled) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let mut transitions = Transitions::new(session, lifecycle, cancelled);
        let result = transitions.remove_around(targets, operation, removal).await;
        transitions.resume_panic(result.as_ref().err());
        result
    });
    join(task).await
}

/// Reattaches mounts that were detached, last detached first.
///
/// For a caller that detached mounts itself and needs them back. Every mount is attempted even
/// after one fails; the first failure is returned with the rest summarized under
/// `additional_remount_errors`, and the session is terminated.
///
/// # Panics
///
/// Re-raises a panic from a mount transition, once every other mount has been attempted and the
/// session terminated.
pub async fn restore_detached_mounts(
    session: Arc<dyn SandboxSession>,
    lifecycle: Arc<dyn MountLifecycle>,
    detached: Vec<(Mount, PosixPath)>,
    error_path: String,
    error_kind: ArchiveErrorKind,
) -> Option<SandboxError> {
    let (_cancel_on_drop, cancelled) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let mut transitions = Transitions::new(session, lifecycle, cancelled);
        let error = transitions
            .restore(&detached, &error_path, error_kind)
            .await;
        transitions.resume_panic(error.as_ref());
        if transitions.caller_cancelled
            && let Some(error) = &error
        {
            log_unreported(error);
        }
        error
    });
    join(task).await
}

/// A transition failure as a record another failure can carry.
///
/// The message, and when the failure wraps another one, that one's code (for a sandbox failure)
/// and text.
#[must_use]
pub fn workspace_archive_error_summary(error: &SandboxError) -> Value {
    let mut summary = Map::new();
    summary.insert("message".to_owned(), Value::from(error.message()));
    if let Some(cause) = std::error::Error::source(error) {
        if let Some(sandbox) = cause.downcast_ref::<SandboxError>() {
            summary.insert(
                "cause_type".to_owned(),
                Value::from(sandbox.error_code().as_str()),
            );
        }
        summary.insert("cause".to_owned(), Value::from(cause.to_string()));
    }
    Value::Object(summary)
}

/// The session's ephemeral mounts, owned, in teardown order.
fn ephemeral_targets(session: &dyn SandboxSession) -> SandboxResult<Vec<(Mount, PosixPath)>> {
    let state = session.state();
    Ok(state
        .manifest()
        .ephemeral_mount_targets()?
        .into_iter()
        .map(|(mount, path)| (mount.clone(), path))
        .collect())
}

/// Waits for the task that owns the transitions, re-raising its panic.
async fn join<T>(task: tokio::task::JoinHandle<T>) -> T {
    match task.await {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            // The task is never aborted, and the runtime shutting down underneath it would have
            // taken this future with it.
            Err(error) => unreachable!("mount transition task ended without finishing: {error}"),
        },
    }
}

/// Logs a failure the reference would have raised past a cancellation.
fn log_unreported(error: &SandboxError) {
    tracing::warn!(
        error = %error,
        code = %error.error_code(),
        "mount transition failed after its caller was cancelled"
    );
}

/// What running the operation came to.
enum Ran<T> {
    Finished(SandboxResult<T>),
    Panicked,
    /// The caller left before it finished, and it was dropped.
    Abandoned,
}

/// The state one run of transitions shares.
struct Transitions {
    session: Arc<dyn SandboxSession>,
    lifecycle: Arc<dyn MountLifecycle>,
    /// Resolves when the caller's future is dropped.
    cancelled: oneshot::Receiver<()>,
    caller_cancelled: bool,
    /// The first panic caught, to resume once cleanup is done.
    panicked: Option<Box<dyn Any + Send>>,
}

impl Transitions {
    fn new(
        session: Arc<dyn SandboxSession>,
        lifecycle: Arc<dyn MountLifecycle>,
        cancelled: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            session,
            lifecycle,
            cancelled,
            caller_cancelled: false,
            panicked: None,
        }
    }

    /// Resumes a panic caught along the way, now that cleanup has run.
    ///
    /// The failure the sequence would otherwise have returned is logged, since the panic takes its
    /// place.
    fn resume_panic(&mut self, failure: Option<&SandboxError>) {
        let Some(panic) = self.panicked.take() else {
            return;
        };
        if let Some(error) = failure {
            log_unreported(error);
        }
        std::panic::resume_unwind(panic);
    }

    /// Keeps a caught panic for later, and describes it as a failure of the step that panicked.
    fn caught(&mut self, panic: Box<dyn Any + Send>) -> SandboxError {
        let message = panic
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic with no message".to_owned());
        if self.panicked.is_none() {
            self.panicked = Some(panic);
        }
        SandboxError::new(
            ErrorCode::MountFailed,
            OpName::Materialize,
            format!("mount transition panicked: {message}"),
        )
    }

    /// Notes a departure the last `select!` did not read because the other side was ready first.
    fn observe_cancellation(&mut self) {
        if !self.caller_cancelled
            && matches!(
                self.cancelled.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            )
        {
            self.caller_cancelled = true;
        }
    }

    async fn remove_around<T, F, Fut>(
        &mut self,
        targets: Vec<(Mount, PosixPath)>,
        operation: F,
        removal: EphemeralMountRemoval,
    ) -> SandboxResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = SandboxResult<T>>,
    {
        let EphemeralMountRemoval {
            error_path,
            error_kind,
            operation_error_context_key,
            restore_on_success,
        } = removal;

        let mut detached = Vec::new();
        let mut detach_error = None;
        for (mount, path) in targets {
            let session = Arc::clone(&self.session);
            let lifecycle = Arc::clone(&self.lifecycle);
            let teardown = async {
                lifecycle
                    .teardown_for_snapshot(&mount, mount.strategy(), session.as_ref(), &path)
                    .await
            };
            if let Err(error) = self.settle(teardown).await {
                detach_error = Some(error_kind.raise(&error_path).with_sandbox_cause(error));
                break;
            }
            detached.push((mount, path));
            if self.caller_cancelled {
                break;
            }
        }
        let detach_ambiguous = detach_error.is_some();

        let mut outcome = None;
        let mut operation_error = None;
        let mut operation_panicked = false;
        if detach_error.is_none() && !self.caller_cancelled {
            match self.run(operation).await {
                Ran::Finished(Ok(value)) => outcome = Some(value),
                Ran::Finished(Err(error)) => operation_error = Some(error),
                Ran::Panicked => operation_panicked = true,
                Ran::Abandoned => {}
            }
        }

        let should_restore = (outcome.is_some() && restore_on_success)
            || detach_error.is_some()
            || operation_error.is_some()
            || operation_panicked
            || self.caller_cancelled;
        let restore_error = if should_restore {
            self.restore(&detached, &error_path, error_kind).await
        } else {
            None
        };
        if detach_ambiguous && restore_error.is_none() && self.terminate().await.is_err() {
            detach_error =
                detach_error.map(|error| error.with_context("terminal_cleanup_failed", true));
        }

        let failure = if let Some(mut error) = restore_error {
            if let (Some(operation_error), Some(key)) =
                (&operation_error, &operation_error_context_key)
                && operation_error.error_code().is_workspace_io()
            {
                error = error
                    .with_context(key.clone(), json!({ "message": operation_error.message() }));
            }
            Some(error)
        } else {
            detach_error.or(operation_error)
        };

        match (failure, outcome) {
            (Some(error), _) => {
                if self.caller_cancelled {
                    log_unreported(&error);
                }
                Err(error)
            }
            (None, Some(value)) => Ok(value),
            // Only a dropped caller leaves no outcome and no failure, and it is not waiting for
            // one. Something has to be returned all the same.
            (None, None) => Err(error_kind
                .raise(&error_path)
                .with_context("reason", "caller_cancelled")),
        }
    }

    /// Reattaches what was detached, last first, carrying on past failures.
    async fn restore(
        &mut self,
        detached: &[(Mount, PosixPath)],
        error_path: &str,
        error_kind: ArchiveErrorKind,
    ) -> Option<SandboxError> {
        let mut first: Option<SandboxError> = None;
        let mut additional = Vec::new();
        for (mount, path) in detached.iter().rev() {
            let session = Arc::clone(&self.session);
            let lifecycle = Arc::clone(&self.lifecycle);
            let restore = async {
                lifecycle
                    .restore_after_snapshot(mount, mount.strategy(), session.as_ref(), path)
                    .await
            };
            if let Err(error) = self.settle(restore).await {
                let current = error_kind.raise(error_path).with_sandbox_cause(error);
                if first.is_none() {
                    first = Some(current);
                } else {
                    additional.push(workspace_archive_error_summary(&current));
                }
            }
        }
        let mut error = first?;
        if !additional.is_empty() {
            error = error.with_context("additional_remount_errors", additional);
        }
        if self.terminate().await.is_err() {
            error = error.with_context("terminal_cleanup_failed", true);
        }
        Some(error)
    }

    /// Terminates a session a transition left in an unknown state.
    async fn terminate(&mut self) -> SandboxResult<()> {
        let session = Arc::clone(&self.session);
        self.settle(async move { session.terminate_ambiguous_mount_transition().await })
            .await
    }

    /// Runs a transition to completion, noting whether the caller gave up meanwhile.
    ///
    /// A panic is caught and returned as the transition failing; it is resumed once cleanup is done.
    async fn settle<F>(&mut self, transition: F) -> SandboxResult<()>
    where
        F: Future<Output = SandboxResult<()>>,
    {
        let transition = AssertUnwindSafe(transition).catch_unwind();
        tokio::pin!(transition);
        let settled = if self.caller_cancelled {
            transition.await
        } else {
            tokio::select! {
                biased;
                settled = &mut transition => settled,
                _ = &mut self.cancelled => {
                    self.caller_cancelled = true;
                    transition.await
                }
            }
        };
        self.observe_cancellation();
        settled.unwrap_or_else(|panic| Err(self.caught(panic)))
    }

    /// Runs the operation until it finishes or the caller gives up, whichever is first.
    async fn run<T, F, Fut>(&mut self, operation: F) -> Ran<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = SandboxResult<T>>,
    {
        let operation = match std::panic::catch_unwind(AssertUnwindSafe(operation)) {
            Ok(operation) => AssertUnwindSafe(operation).catch_unwind(),
            Err(panic) => {
                self.caught(panic);
                return Ran::Panicked;
            }
        };
        let ran = tokio::select! {
            biased;
            finished = operation => match finished {
                Ok(result) => Ran::Finished(result),
                Err(panic) => {
                    self.caught(panic);
                    Ran::Panicked
                }
            },
            _ = &mut self.cancelled => {
                self.caller_cancelled = true;
                Ran::Abandoned
            }
        };
        self.observe_cancellation();
        ran
    }
}
