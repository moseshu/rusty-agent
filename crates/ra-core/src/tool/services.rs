//! The framework ports a running tool may reach, in one bag.
//!
//! # Why a bag and not fields
//!
//! A port reaches a tool through four layers — the run's request, turn settlement, batch execution,
//! then the dispatch of one call — and the last of those hands it to
//! [`Tool::call`](crate::tool::Tool::call), whose signature belongs to whoever wrote the tool. Every
//! port added as its own field therefore walks that whole chain again, and the walk ends in other
//! people's code. Carrying one value instead means the next port is a field on this struct and
//! nothing else moves.
//!
//! The task-state handle is the first port; a control plane for sub-agents, a workspace lease and a
//! budget reservation are the ones already visible. Migrating while there is exactly one is the
//! cheapest this will ever be.
//!
//! # Why named accessors and not a lookup
//!
//! A generic `service::<T>()` keyed by [`TypeId`](std::any::TypeId) cannot work in this language
//! without per-trait registration glue, because recovering a `dyn Trait` from a `dyn Any` is not
//! something the language does. What such an API degrades into is an application-context downcast
//! with extra layers — the very thing a typed port exists to remove. One accessor per port says in
//! the signature what is available, and a tool that asks for something the host did not install
//! gets `None` rather than a runtime type error.

use core::fmt;
use std::sync::Arc;

use crate::{event::HostEventSink, state::WorkStateHandle};

/// Framework-owned ports handed to a tool, each behind its own accessor.
///
/// An empty bag is the ordinary case: a run installs the ports its host actually provides, and a
/// tool that needs one it does not find answers the model that the capability is not enabled rather
/// than failing the turn.
#[must_use]
#[non_exhaustive]
#[derive(Clone, Default)]
pub struct ToolServices {
    work_state: Option<Arc<dyn WorkStateHandle>>,
    event_sink: Option<Arc<dyn HostEventSink>>,
}

impl ToolServices {
    /// Creates a bag with no ports installed.
    pub const fn new() -> Self {
        Self {
            work_state: None,
            event_sink: None,
        }
    }

    /// Installs the task state the run participates in.
    pub fn with_work_state(mut self, work_state: Arc<dyn WorkStateHandle>) -> Self {
        self.work_state = Some(work_state);
        self
    }

    /// Installs the host event sink for granular observability and UI streaming.
    pub fn with_event_sink(mut self, event_sink: Arc<dyn HostEventSink>) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    /// The task state spanning this run, when the host attached one.
    ///
    /// `None` is not a failure: a run belongs to a task only when something above it says so. A
    /// future milestone puts typed channel operations on [`WorkStateHandle`]; this accessor does not
    /// change when it does, which is the whole reason the port exists now.
    #[must_use]
    pub fn work_state(&self) -> Option<&dyn WorkStateHandle> {
        self.work_state.as_deref()
    }

    /// The host event sink for this run, when the host installed one.
    #[must_use]
    pub fn event_sink(&self) -> Option<&Arc<dyn HostEventSink>> {
        self.event_sink.as_ref()
    }
}

impl fmt::Debug for ToolServices {
    /// Reports which ports are installed, never what they hold.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolServices")
            .field("work_state", &self.work_state.is_some())
            .field("event_sink", &self.event_sink.is_some())
            .finish_non_exhaustive()
    }
}
