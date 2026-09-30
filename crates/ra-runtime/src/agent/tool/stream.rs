//! Forwarding a nested run's events to the host: the reference's `on_stream`.
//!
//! With a handler installed, the nested agent runs through
//! [`Runner::run_streamed`](crate::runner::Runner::run_streamed) instead of
//! [`Runner::run`](crate::runner::Runner::run), and every event it emits reaches the handler,
//! wrapped with the agent that emitted it and the call that started the run. The call's output is
//! decided exactly as without a handler; forwarding only adds a view of the run in progress.
//!
//! # Reading the nested run is decoupled from handling its events
//!
//! As in the reference, the events wait in a backlog between the nested run and the handler, so a
//! slow handler does not hold the nested run back. The backlog is bounded by
//! `on_stream_max_pending_events`, which counts the events waiting, not the one being handled.
//! When a new event finds the backlog full, the handler is given one chance to catch up; if the
//! backlog is still full after that, the handler is abandoned, the nested run is cancelled, and the
//! call fails through the tool's failure handling. The limit caps a handler that has stopped
//! keeping up, not a burst a ready handler can absorb.
//!
//! When the nested run ends — however it ends — every event it emitted is handled before the call
//! returns or fails.
//!
//! # A handler cannot fail the call, but it can stop it
//!
//! An error from the handler is logged and forwarding continues, as in the reference. The one
//! exception is a cancellation, which the reference lets escape the handler because it is not an
//! ordinary exception there: a handler that reports one stops the nested run, and the call ends
//! with that cancellation.
//!
//! # Rust adaptations
//!
//! - The reference runs the reader and the handler as two tasks. Here both are polled by the call's
//!   own future, the handler first each time, so the chance to catch up the reference gives with a
//!   scheduler yield is a real turn of the handler rather than a guess at task ordering. Dropping
//!   the call — a parent cancellation — drops the handler with it, without waiting for it.
//! - `agent` is the public identity the run stream announces each turn under, not an agent object:
//!   [`RunStreamEvent::TurnStarted`] carries the identity, and a handoff inside the nested run is
//!   seen here as a turn started under another one.
//! - `tool_call` is the [`AgentToolInvocation`] the nested result also carries. The reference's
//!   `tool_call` can be absent for a tool invoked without one; here an agent tool only runs inside
//!   a dispatched call, so it is always present.

use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
};

use async_trait::async_trait;
use ra_core::{
    cancel::{CancelReason, CancelScope, DRAIN_GRACE},
    error::{Error, Result},
    item::AgentId,
};
use tokio::sync::mpsc;
use tracing::warn;

use crate::runner::{AgentToolInvocation, RunRequest, RunResult, RunStreamEvent, Runner};

/// The default for `on_stream_max_pending_events`, as in the reference.
pub const DEFAULT_ON_STREAM_MAX_PENDING_EVENTS: usize = 1024;

/// One event from a nested agent run, as an agent tool's stream handler receives it.
///
/// The reference's `AgentToolStreamEvent`.
#[derive(Debug, Clone)]
pub struct AgentToolStreamEvent {
    event: RunStreamEvent,
    agent: AgentId,
    tool_call: Arc<AgentToolInvocation>,
}

impl AgentToolStreamEvent {
    /// The event from the nested run, unchanged.
    #[must_use]
    pub const fn event(&self) -> &RunStreamEvent {
        &self.event
    }

    /// Takes the event from the nested run.
    #[must_use]
    pub fn into_event(self) -> RunStreamEvent {
        self.event
    }

    /// The public agent of the nested run that emitted the event.
    ///
    /// The agent the tool wraps until the nested run hands off; from then on the agent it handed
    /// off to.
    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The call that started the nested run.
    #[must_use]
    pub fn tool_call(&self) -> &AgentToolInvocation {
        &self.tool_call
    }
}

/// Receives the events of the nested runs an agent tool starts.
#[async_trait]
pub trait AgentToolStreamHandler: Send + Sync + 'static {
    /// Handles one event.
    ///
    /// An error is logged and does not fail the call — unless it is a
    /// [cancellation](Error::is_cancelled), which stops the nested run and ends the call with it.
    async fn on_event(&self, event: AgentToolStreamEvent) -> Result<()>;
}

#[async_trait]
impl<F> AgentToolStreamHandler for F
where
    F: Fn(AgentToolStreamEvent) -> Result<()> + Send + Sync + 'static,
{
    async fn on_event(&self, event: AgentToolStreamEvent) -> Result<()> {
        self(event)
    }
}

/// An agent tool's stream handler with its backlog limit.
#[derive(Clone)]
pub(super) struct StreamForwarding {
    handler: Arc<dyn AgentToolStreamHandler>,
    max_pending_events: Option<usize>,
}

impl StreamForwarding {
    pub(super) fn new(
        handler: Arc<dyn AgentToolStreamHandler>,
        max_pending_events: Option<usize>,
    ) -> Self {
        Self {
            handler,
            max_pending_events,
        }
    }

    /// Runs the nested request streamed, handing every event to the handler, and returns its
    /// result once the run has ended and all of its events have been handled.
    ///
    /// `scope` is the request's own cancellation scope, which a failed forwarding cancels.
    pub(super) async fn run(
        &self,
        request: RunRequest,
        scope: &CancelScope,
        agent: AgentId,
        tool_call: Arc<AgentToolInvocation>,
    ) -> Result<RunResult> {
        // Spawned under the call's span, so the nested run's spans nest under it as they do for a
        // nested run that is not streamed.
        let mut stream = Runner::run_streamed_in(request, tracing::Span::current());
        let forwarded = self.forward(&mut stream, agent, tool_call).await;
        match forwarded {
            Ok(()) => stream.finish().await,
            Err(error) => {
                // Stop the nested run and wait for it to end, as the reference does before the
                // call fails. A run that does not stop within the grace period is abandoned to the
                // stream's own cleanup, which aborts it.
                scope.cancel(CancelReason::PeerFailure);
                drop(tokio::time::timeout(DRAIN_GRACE, stream.finish()).await);
                Err(error)
            }
        }
    }

    async fn forward(
        &self,
        stream: &mut crate::runner::RunStream,
        mut agent: AgentId,
        tool_call: Arc<AgentToolInvocation>,
    ) -> Result<()> {
        let pending = AtomicUsize::new(0);
        let (sender, mut receiver) = mpsc::unbounded_channel::<AgentToolStreamEvent>();
        let limit = self.max_pending_events;

        let read = async {
            // Owned here, so the backlog closes — and the handler can finish — once reading ends.
            let sender = sender;
            while let Some(event) = stream.next_event().await {
                if let RunStreamEvent::TurnStarted { agent: next, .. } = &event {
                    agent = next.clone();
                }
                if let Some(limit) = limit
                    && pending.load(Ordering::Acquire) >= limit
                {
                    // Let a ready handler take from the backlog before deciding it is overrun.
                    tokio::task::yield_now().await;
                    if pending.load(Ordering::Acquire) >= limit {
                        return Err(overflow(limit));
                    }
                }
                pending.fetch_add(1, Ordering::AcqRel);
                // The receiver lives until both halves are dropped, so this cannot fail.
                let _ = sender.send(AgentToolStreamEvent {
                    event,
                    agent: agent.clone(),
                    tool_call: Arc::clone(&tool_call),
                });
            }
            Ok(())
        };
        let handle = async {
            while let Some(event) = receiver.recv().await {
                pending.fetch_sub(1, Ordering::AcqRel);
                let agent = event.agent.clone();
                if let Err(error) = self.handler.on_event(event).await {
                    if error.is_cancelled() {
                        return Err(error);
                    }
                    warn!(
                        agent.id = %agent,
                        error.code = error.code(),
                        "Error while handling an agent tool on_stream event"
                    );
                }
            }
            Ok(())
        };

        let mut read = pin!(read);
        let mut handle = pin!(handle);
        let mut reading = true;
        poll_fn(|cx| {
            // The handler is polled first on every wake-up. That is what gives it its chance to
            // catch up between the reader finding the backlog full and checking it again.
            if let Poll::Ready(result) = handle.as_mut().poll(cx) {
                // Before reading has ended the backlog is still open, so the handler can only have
                // finished by failing.
                return Poll::Ready(result);
            }
            if reading {
                match read.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => reading = false,
                }
                // Reading has just closed the backlog; let the handler drain what is left.
                return handle.as_mut().poll(cx);
            }
            Poll::Pending
        })
        .await
    }
}

impl fmt::Debug for StreamForwarding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamForwarding")
            .field("max_pending_events", &self.max_pending_events)
            .finish_non_exhaustive()
    }
}

fn overflow(limit: usize) -> Error {
    Error::caller(format!(
        "Agent tool on_stream backlog exceeded on_stream_max_pending_events={limit}. Use a faster \
         handler, increase the limit, or set it to None."
    ))
}
