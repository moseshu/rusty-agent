//! What consumes audit events: the sink protocol, without any sink.
//!
//! Ported from the protocol half of the reference's `session/sinks.py`. A sink receives each event
//! a session's instrumentation emits, after that sink's payload policy has been applied. The five
//! sinks the reference ships — a callback, a host JSONL file, a JSONL file inside the workspace, an
//! HTTP proxy, and an ordered group — are implementations and live in the service crate beside the
//! instrumentation that drives them.
//!
//! # Delivery mode and failure policy are two separate answers
//!
//! [`DeliveryMode`] says whether the operation waits for the sink, and [`OnErrorPolicy`] what a
//! failure does. They combine the way the reference combines them, which is not orthogonal: an
//! `async` sink that raises is awaited inline so its failure can reach the caller, and a
//! `best_effort` sink never fails anything whatever its policy says. The instrumentation holds that
//! table; a sink only answers the two questions.

use std::sync::Arc;

use async_trait::async_trait;

use super::events::{EventPayloadPolicy, SandboxSessionEvent};
use super::session::{SandboxResult, SandboxSession};

/// What a sink's delivery failed with.
pub type SinkError = Box<dyn std::error::Error + Send + Sync>;

/// Whether an operation waits for a sink.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryMode {
    /// Delivered before the operation continues.
    Sync,
    /// Delivered in the background, unless the sink raises, in which case it is awaited so its
    /// failure can reach the caller.
    Async,
    /// Delivered in the background, and never allowed to fail the operation.
    BestEffort,
}

/// What a sink's failure does.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnErrorPolicy {
    /// Fails the operation that emitted the event.
    Raise,
    /// Logged and ignored.
    Log,
    /// Ignored without a trace.
    Ignore,
}

/// Something audit events are delivered to.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// A name the host gave this sink, if any.
    fn name(&self) -> Option<&str> {
        None
    }

    /// The sink's type, as failures and logs name it.
    ///
    /// The reference names a failing sink by its class; the default here is the Rust type's name.
    fn type_name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    /// Whether the operation waits for this sink.
    fn mode(&self) -> DeliveryMode;

    /// What this sink's failure does.
    fn on_error(&self) -> OnErrorPolicy;

    /// This sink's own payload policy, which overrides the instrumentation's in the fields it sets.
    fn payload_policy(&self) -> Option<&EventPayloadPolicy> {
        None
    }

    /// Gives the sink the session whose events it will receive.
    ///
    /// The reference's optional `SandboxSessionBoundSink` protocol: a sink that reads or writes the
    /// session — one that appends into the workspace, one that hands the session to a callback —
    /// keeps it; the default ignores it. The session given is always the undecorated one, so a sink
    /// that writes through it does not emit events about its own writes.
    ///
    /// # Errors
    ///
    /// Returns what the sink could not set up on the session — an ephemeral workspace sink whose
    /// path cannot be excluded from snapshots, for one. The reference raises it from the wrapper's
    /// construction, so a session whose sinks cannot bind is not handed out.
    fn bind(&self, session: Arc<dyn SandboxSession>) -> SandboxResult<()> {
        let _ = session;
        Ok(())
    }

    /// The sinks this one groups, in order, when it is a group.
    ///
    /// The instrumentation delivers to each member itself, with that member's own policy, and waits
    /// for one before starting the next whatever its mode says — which is the group's promise.
    fn grouped_sinks(&self) -> Option<&[Arc<dyn EventSink>]> {
        None
    }

    /// Consumes one event.
    ///
    /// # Errors
    ///
    /// Returns whatever the sink failed with; the instrumentation decides what that does.
    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError>;
}

/// The session a sink bound to `session` should keep.
///
/// The reference's `_unwrap_session_wrapper`, done defensively on every bind: a sink handed the
/// instrumented wrapper by mistake would write through it and emit an event for every write it
/// makes, each of which it would then be delivered.
#[must_use]
pub fn undecorated_session(session: Arc<dyn SandboxSession>) -> Arc<dyn SandboxSession> {
    session.inner_session().unwrap_or(session)
}
