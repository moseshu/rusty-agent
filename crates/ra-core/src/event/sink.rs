//! Event sinks for host observability.

use std::sync::{Arc, Mutex, PoisonError};

use super::HostEvent;

/// A consumer of host events.
///
/// Implementations must be thread-safe and non-blocking. Event emission occurs synchronously
/// on tool and runtime execution paths.
pub trait HostEventSink: Send + Sync + 'static {
    /// Emits a host event.
    fn emit(&self, event: HostEvent);
}

/// An event sink that discards all emitted events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHostEventSink;

impl HostEventSink for NoopHostEventSink {
    fn emit(&self, _event: HostEvent) {}
}

/// An in-memory event sink that accumulates emitted events in a thread-safe list.
///
/// Primarily used in tests, mock hosts, and inspectable debug harnesses.
#[derive(Debug, Default, Clone)]
pub struct InMemoryHostEventSink {
    events: Arc<Mutex<Vec<HostEvent>>>,
}

impl InMemoryHostEventSink {
    /// Creates an empty in-memory sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a snapshot of all events emitted so far.
    #[must_use]
    pub fn events(&self) -> Vec<HostEvent> {
        let guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    }

    /// Clears all stored events.
    pub fn clear(&self) {
        let mut guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clear();
    }

    /// Returns the number of events emitted so far.
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        guard.len()
    }

    /// Returns true if no events have been emitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl HostEventSink for InMemoryHostEventSink {
    fn emit(&self, event: HostEvent) {
        let mut guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        guard.push(event);
    }
}

/// An event sink that invokes a closure for each emitted event.
pub struct FnHostEventSink<F> {
    callback: F,
}

impl<F> FnHostEventSink<F>
where
    F: Fn(HostEvent) + Send + Sync + 'static,
{
    /// Creates a sink backed by the given callback closure.
    pub const fn new(callback: F) -> Self {
        Self { callback }
    }
}

impl<F> HostEventSink for FnHostEventSink<F>
where
    F: Fn(HostEvent) + Send + Sync + 'static,
{
    fn emit(&self, event: HostEvent) {
        (self.callback)(event);
    }
}
