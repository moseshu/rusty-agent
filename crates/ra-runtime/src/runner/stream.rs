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

use ra_core::{cancel::CancelOnDrop, error::Result, item::AgentId, item::RunItem};
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
    task: JoinHandle<Result<RunResult>>,
    cancel: CancelOnDrop,
}

impl RunStream {
    pub(super) const fn new(
        events: mpsc::UnboundedReceiver<RunStreamEvent>,
        task: JoinHandle<Result<RunResult>>,
        cancel: CancelOnDrop,
    ) -> Self {
        Self {
            events,
            task,
            cancel,
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
        // Disarm before awaiting. Leaving the guard armed would cancel the very run being waited
        // on the moment this scope's locals drop.
        let _scope = self.cancel.disarm();
        drop(self.events);
        match self.task.await {
            Ok(result) => result,
            Err(error) => Err(ra_core::error::Error::caller(
                "the run task ended without producing a result",
            )
            .with_source(error)),
        }
    }
}
