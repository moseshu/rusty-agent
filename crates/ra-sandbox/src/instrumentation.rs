//! Delivering a session's audit events to sinks, and the session wrapper that emits them.
//!
//! Ported from the reference's `session/manager.py` (`Instrumentation`) and
//! `session/sandbox_session.py` (the `SandboxSession` wrapper, here [`InstrumentedSession`]; the
//! protocol already owns the name `SandboxSession`). Every session a client in this crate creates or
//! resumes comes back wrapped, as it does from the reference's clients, so a caller gets the same
//! events and spans whichever backend made it.
//!
//! # Merge order
//!
//! A sink sees an event through one effective payload policy: the instrumentation's default, then
//! the policy registered for the event's operation, then the sink's own — each later one replacing
//! only what it set.
//!
//! # Delivery
//!
//! | Mode | `on_error` | What happens |
//! | --- | --- | --- |
//! | `Sync` | `Raise` | awaited; a failure fails the operation as `sandbox event sink failed: …` |
//! | `Sync` | `Log` / `Ignore` | awaited; a failure is logged or dropped |
//! | `Async` | `Raise` | awaited, so the failure can reach the caller — as the sink's own failure |
//! | `Async` | `Log` / `Ignore` | spawned; a failure is logged or dropped |
//! | `BestEffort` | any | spawned; a failure is logged if the policy is `Log`, and never raised |
//!
//! A group's members are each awaited in turn whatever their mode, which is the group's ordering
//! promise; a member's failure is handled as a `Sync` one, except that a `BestEffort` member's never
//! raises. [`Instrumentation::flush`] waits for everything spawned so far, and closing an
//! instrumented session calls it.
//!
//! # Logged failures say nothing about the sink
//!
//! The reference logs a swallowed sink failure through its tool-data logging policy, which by
//! default keeps only the fixed message: no sink type, no exception. That default is what is
//! logged here. The reference's switch for logging the details
//! (`OPENAI_AGENTS_DONT_LOG_TOOL_DATA=0`) is framework-wide and has not been carried over, so there
//! is no way to turn them on yet.

mod session;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ra_core::sandbox::{
    DeliveryMode, EventPayloadPolicy, EventSink, OnErrorPolicy, OpName, SandboxError,
    SandboxResult, SandboxSessionEvent, SinkError,
};
use tokio::task::JoinHandle;

pub use session::InstrumentedSession;

/// The fixed message a swallowed sink failure is logged with.
pub const SINK_FAILURE_LOG_MESSAGE: &str = "Sandbox event sink failed (ignored)";

/// Delivers audit events to configured sinks, each through its own payload policy.
pub struct Instrumentation {
    sinks: Mutex<Vec<Arc<dyn EventSink>>>,
    payload_policy: EventPayloadPolicy,
    payload_policy_by_op: HashMap<OpName, EventPayloadPolicy>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Instrumentation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Instrumentation")
            .field("payload_policy", &self.payload_policy)
            .field("payload_policy_by_op", &self.payload_policy_by_op)
            .finish_non_exhaustive()
    }
}

impl Default for Instrumentation {
    fn default() -> Self {
        Self::new()
    }
}

impl Instrumentation {
    /// No sinks, and a default policy that sets nothing.
    ///
    /// What a client wraps its sessions with when it was given nothing else: no events go anywhere,
    /// but the operations still open trace spans.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sinks: Mutex::new(Vec::new()),
            payload_policy: EventPayloadPolicy::new(),
            payload_policy_by_op: HashMap::new(),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Delivers to `sinks`, in order.
    #[must_use]
    pub fn with_sinks(sinks: impl IntoIterator<Item = Arc<dyn EventSink>>) -> Self {
        let instrumentation = Self::new();
        instrumentation.lock_sinks().extend(sinks);
        instrumentation
    }

    /// Uses `policy` as the default every other policy is merged over.
    #[must_use]
    pub const fn with_payload_policy(mut self, policy: EventPayloadPolicy) -> Self {
        self.payload_policy = policy;
        self
    }

    /// Merges `policy` over the default for events of `op`.
    #[must_use]
    pub fn with_payload_policy_for(mut self, op: OpName, policy: EventPayloadPolicy) -> Self {
        self.payload_policy_by_op.insert(op, policy);
        self
    }

    /// The sinks, in delivery order.
    #[must_use]
    pub fn sinks(&self) -> Vec<Arc<dyn EventSink>> {
        self.lock_sinks().clone()
    }

    /// Appends a sink.
    ///
    /// A session already wrapped with this instrumentation delivers to it from its next event, but
    /// does not bind it: binding happens when a session is wrapped.
    pub fn add_sink(&self, sink: Arc<dyn EventSink>) {
        self.lock_sinks().push(sink);
    }

    /// The default policy.
    #[must_use]
    pub const fn payload_policy(&self) -> &EventPayloadPolicy {
        &self.payload_policy
    }

    /// The policy `sink` sees events of `op` through.
    #[must_use]
    pub fn policy_for(&self, op: OpName, sink: &dyn EventSink) -> EventPayloadPolicy {
        let mut effective = self.payload_policy;
        if let Some(op_policy) = self.payload_policy_by_op.get(&op) {
            effective = effective.overridden_by(op_policy);
        }
        if let Some(sink_policy) = sink.payload_policy() {
            effective = effective.overridden_by(sink_policy);
        }
        effective
    }

    /// Delivers `event` to every sink.
    ///
    /// # Errors
    ///
    /// Returns [`ra_core::sandbox::ErrorCode::EventSinkFailed`] for the first sink configured to
    /// raise that failed; sinks after it are not delivered to, as they are not in the reference.
    pub async fn emit(&self, event: &SandboxSessionEvent) -> SandboxResult<()> {
        for sink in self.sinks() {
            if let Some(members) = sink.grouped_sinks() {
                for member in members {
                    let per_sink =
                        event.with_policy_applied(&self.policy_for(event.op(), &**member));
                    self.deliver_in_order(member, per_sink).await?;
                }
            } else {
                let per_sink = event.with_policy_applied(&self.policy_for(event.op(), &*sink));
                self.deliver(&sink, per_sink).await?;
            }
        }
        Ok(())
    }

    /// Waits for every delivery spawned so far.
    ///
    /// Their failures were already logged or dropped where they happened; waiting does not surface
    /// them.
    pub async fn flush(&self) {
        let pending: Vec<JoinHandle<()>> = std::mem::take(&mut *self.lock_tasks());
        for task in pending {
            let _ = task.await;
        }
    }

    async fn deliver(
        &self,
        sink: &Arc<dyn EventSink>,
        event: SandboxSessionEvent,
    ) -> SandboxResult<()> {
        match sink.mode() {
            DeliveryMode::Sync => {
                let op = event.op();
                let event_id = event.event_id();
                match sink.handle(event).await {
                    Ok(()) => Ok(()),
                    Err(error) => handle_sink_error(&**sink, op, event_id, error, false),
                }
            }
            DeliveryMode::Async if sink.on_error() == OnErrorPolicy::Raise => {
                // Awaited inline, and the failure let out as the sink's own: the reference awaits
                // the sink directly here, without the wrapping synchronous delivery applies.
                let op = event.op();
                sink.handle(event).await.map_err(|error| {
                    SandboxError::event_sink_failed(op, error.to_string()).with_boxed_cause(error)
                })
            }
            DeliveryMode::Async => {
                self.spawn(sink, event, false);
                Ok(())
            }
            // `BestEffort`, and any mode added later: delivered the one way that can neither hold the
            // operation up nor fail it, until a new mode is given a meaning here.
            _ => {
                self.spawn(sink, event, true);
                Ok(())
            }
        }
    }

    async fn deliver_in_order(
        &self,
        sink: &Arc<dyn EventSink>,
        event: SandboxSessionEvent,
    ) -> SandboxResult<()> {
        let op = event.op();
        let event_id = event.event_id();
        match sink.handle(event).await {
            Ok(()) => Ok(()),
            Err(error) => handle_sink_error(
                &**sink,
                op,
                event_id,
                error,
                sink.mode() == DeliveryMode::BestEffort,
            ),
        }
    }

    fn spawn(&self, sink: &Arc<dyn EventSink>, event: SandboxSessionEvent, force_no_raise: bool) {
        let sink = Arc::clone(sink);
        let task = tokio::spawn(async move {
            let op = event.op();
            let event_id = event.event_id();
            if let Err(error) = sink.handle(event).await {
                // A spawned delivery has nobody to raise to. `Async` spawns only sinks that do not
                // raise, and `BestEffort` never does whatever it was configured with.
                let _ = handle_sink_error(&*sink, op, event_id, error, force_no_raise);
            }
        });
        let mut tasks = self.lock_tasks();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    fn lock_sinks(&self) -> std::sync::MutexGuard<'_, Vec<Arc<dyn EventSink>>> {
        self.sinks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_tasks(&self) -> std::sync::MutexGuard<'_, Vec<JoinHandle<()>>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What a sink's failure does: logged, dropped, or turned into the operation's failure.
fn handle_sink_error(
    sink: &dyn EventSink,
    op: OpName,
    event_id: uuid::Uuid,
    error: SinkError,
    force_no_raise: bool,
) -> SandboxResult<()> {
    let policy = sink.on_error();
    if force_no_raise || matches!(policy, OnErrorPolicy::Log | OnErrorPolicy::Ignore) {
        if policy == OnErrorPolicy::Log {
            tracing::error!("{SINK_FAILURE_LOG_MESSAGE}");
        }
        return Ok(());
    }
    Err(SandboxError::event_sink_failed(
        op,
        format!(
            "sandbox event sink failed: {} while handling event {event_id}",
            sink.type_name()
        ),
    )
    .with_boxed_cause(error))
}
