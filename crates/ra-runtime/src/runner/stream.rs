//! The run channel and the streaming wrapper (R3-7).
//!
//! # Why this is not the model channel
//!
//! [`ModelStreamEvent`](ra_core::model::ModelStreamEvent) is what a provider adapter emits about
//! **one wire call**. This is what the runner emits about **a run**: which turn started, which
//! public agent is speaking, what the turn produced. An adapter cannot express the second — it has
//! never heard of agents or turns — so the two are separate enums and this one wraps rather than
//! extends the other.
//!
//! Forwarding provider deltas into this channel is R1-7's, and its insertion point is the model
//! call in [`super::run_loop`]. Until then a subscriber sees a turn's records the moment that turn
//! settles, which is already incremental across a multi-turn run.

use ra_core::{
    cancel::{CancelOnDrop, CancelReason, DRAIN_GRACE},
    error::Result,
    item::AgentId,
    item::RunItem,
};
use tokio::{sync::mpsc, task::JoinHandle};

use super::result::{RunOutcome, RunResult};

/// One thing that happened during a run.
///
/// The size spread between variants is accepted rather than boxed away: [`Self::Item`] is the one
/// that actually flows, so paying an allocation on every record to shrink the rare bookkeeping
/// variants would make the common path worse to make a number look better.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum RunStreamEvent {
    /// A turn began. `agent` is the public identity (R3-12), which is the one a host displays.
    TurnStarted {
        /// 1-based index of this turn within the run.
        turn: u32,
        /// The public agent speaking.
        agent: AgentId,
    },
    /// One record the turn produced, already attributed.
    Item(RunItem),
    /// The run reached its end. Also available from [`RunStream::finish`]; emitted here so a
    /// subscriber watching only events does not have to infer why the stream stopped.
    Finished(RunOutcome),
}

/// A run in progress, with its events and its eventual outcome.
///
/// **Draining the events does not consume the result.** A stream that only yielded events would
/// force a host to reconstruct the outcome from whatever it happened to observe, and a host that
/// stopped reading early would lose it entirely. Events and [`Self::finish`] are two independent
/// views of the same run.
///
/// Dropping the stream cancels the run. The alternative — a detached task still calling a provider
/// after the host walked away — spends money nobody is waiting for.
#[derive(Debug)]
pub struct RunStream {
    events: mpsc::UnboundedReceiver<RunStreamEvent>,
    task: Option<JoinHandle<Result<RunResult>>>,
    cancel: Option<CancelOnDrop>,
}

impl RunStream {
    pub(super) const fn new(
        events: mpsc::UnboundedReceiver<RunStreamEvent>,
        task: JoinHandle<Result<RunResult>>,
        cancel: CancelOnDrop,
    ) -> Self {
        Self {
            events,
            task: Some(task),
            cancel: Some(cancel),
        }
    }

    /// Waits for the next event, or `None` once the run has emitted its last one.
    pub async fn next_event(&mut self) -> Option<RunStreamEvent> {
        self.events.recv().await
    }

    /// Waits for the run to end and returns its result.
    ///
    /// Any events still buffered are dropped, which is the point of them being a separate view: a
    /// caller that only wants the outcome does not have to read the narration first.
    pub async fn finish(self) -> Result<RunResult> {
        let mut stream = self;
        stream.events.close();

        // Keep `task` in `self` while awaiting. If this future is dropped, `RunStream::drop` still
        // owns the handle and the armed guard, so it can cancel and reap the background run instead
        // of silently detaching it.
        // Both `None` arms below are unreachable by construction: the two slots are emptied only by
        // a completed `finish` — which consumed the stream — or by `Drop`, which runs after this
        // function returns. They answer rather than unwrap because an `Option` that can only be
        // `Some` today is exactly the kind of assumption a later refactor invalidates quietly.
        let joined = match stream.task.as_mut() {
            Some(task) => task.await,
            None => {
                return Err(ra_core::error::Error::caller(
                    "the run task was already handed to cancellation cleanup",
                ));
            }
        };
        drop(stream.task.take());

        // The task has reached a terminal state, so dropping the stream must not retroactively
        // cancel its completed run scope.
        let _scope = match stream.cancel.take() {
            Some(cancel) => cancel.disarm(),
            None => {
                return Err(ra_core::error::Error::caller(
                    "the run cancellation guard was already handed to cleanup",
                ));
            }
        };
        match joined {
            Ok(result) => result,
            Err(error) => Err(ra_core::error::Error::caller(
                "the run task ended without producing a result",
            )
            .with_source(error)),
        }
    }
}

impl Drop for RunStream {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };

        // Signal before detaching from the caller. The guard remains armed as a backstop for every
        // early-return path in this destructor.
        if let Some(cancel) = &self.cancel {
            cancel.scope().cancel(CancelReason::UserInterrupt);
        }
        reap_cancelled_task(task);
    }
}

/// Waits briefly for a cancelled run, then stops a task that did not cooperate with cancellation.
///
/// `Drop` cannot await. Handing the join handle to this short-lived reaper keeps the spawned task
/// supervised instead of making its lifetime depend on a receiver the host has already discarded.
///
/// # The grace period needs a timer
///
/// [`DRAIN_GRACE`] is enforced with [`tokio::time::timeout`], which requires the ambient runtime to
/// have its time driver enabled — see [`Runner::run_streamed`](super::Runner::run_streamed). On a
/// runtime without one this reaper panics, the panic dies with the detached reaper, and an
/// uncooperative run degrades to the plain detach this function exists to prevent. It is the
/// **backstop** that is lost, not the cancellation: the signal was already sent before this call,
/// and a run that observes its scope stops at its next checkpoint regardless.
fn reap_cancelled_task(mut task: JoinHandle<Result<RunResult>>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // `run_streamed` required a Tokio runtime to create this handle. A stream can nevertheless
        // be moved and dropped after that runtime has gone away; aborting is the only synchronous
        // cleanup available in that situation.
        task.abort();
        return;
    };

    drop(runtime.spawn(async move {
        if tokio::time::timeout(DRAIN_GRACE, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }));
}
