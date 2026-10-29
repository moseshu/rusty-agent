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
//! The event sink, output projector, sub-agent control plane and user-input port all travel this
//! way.
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

use crate::{
    agent::control::AgentControlPort, event::HostEventSink, tool::ToolOutputProjector,
    user_input::UserInputPort,
};

/// Framework-owned ports handed to a tool, each behind its own accessor.
///
/// An empty bag is the ordinary case: a run installs the ports its host actually provides, and a
/// tool that needs one it does not find answers the model that the capability is not enabled rather
/// than failing the turn.
#[must_use]
#[non_exhaustive]
#[derive(Clone, Default)]
pub struct ToolServices {
    event_sink: Option<Arc<dyn HostEventSink>>,
    output_projector: Option<Arc<dyn ToolOutputProjector>>,
    agent_control: Option<Arc<dyn AgentControlPort>>,
    user_input: Option<Arc<dyn UserInputPort>>,
    hook_context: Option<crate::hook::HookContextQueue>,
}

impl ToolServices {
    /// Creates a bag with no ports installed.
    pub const fn new() -> Self {
        Self {
            event_sink: None,
            output_projector: None,
            agent_control: None,
            user_input: None,
            hook_context: None,
        }
    }

    /// The empty set, as a borrow that outlives any caller.
    ///
    /// For a borrowed view whose services are optional: a default that allocated would make
    /// constructing one fallible, and a default that borrowed a local would not outlive the
    /// construction. Nothing is installed, so every port answers `None`.
    pub fn none() -> &'static Self {
        static NONE: ToolServices = ToolServices::new();
        &NONE
    }

    /// Installs the host event sink for granular observability and UI streaming.
    pub fn with_event_sink(mut self, event_sink: Arc<dyn HostEventSink>) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    /// Installs the context policy that projects completed tool output before it enters history.
    pub fn with_output_projector(mut self, output_projector: Arc<dyn ToolOutputProjector>) -> Self {
        self.output_projector = Some(output_projector);
        self
    }

    /// Installs the multi-agent control plane, bound to the agent this run is.
    pub fn with_agent_control(mut self, agent_control: Arc<dyn AgentControlPort>) -> Self {
        self.agent_control = Some(agent_control);
        self
    }

    /// Installs where the run's questions for the user go.
    pub fn with_user_input(mut self, user_input: Arc<dyn UserInputPort>) -> Self {
        self.user_input = Some(user_input);
        self
    }

    /// Installs the contextual history destination shared by hooks and their host.
    pub fn with_hook_context(mut self, output: crate::hook::HookContextQueue) -> Self {
        self.hook_context = Some(output);
        self
    }
    /// The contextual history destination, when bound to a run.
    #[must_use]
    pub const fn hook_context(&self) -> Option<&crate::hook::HookContextQueue> {
        self.hook_context.as_ref()
    }

    /// The host event sink for this run, when the host installed one.
    #[must_use]
    pub fn event_sink(&self) -> Option<&Arc<dyn HostEventSink>> {
        self.event_sink.as_ref()
    }

    /// The model-output projection policy, when the host configured one.
    #[must_use]
    pub fn output_projector(&self) -> Option<&Arc<dyn ToolOutputProjector>> {
        self.output_projector.as_ref()
    }
}

impl ToolServices {
    /// The multi-agent control plane as this run's agent sees it, when the host enabled one.
    ///
    /// `None` means the run takes no part in an agent tree; the collaboration tools answer the
    /// model that they are not enabled.
    #[must_use]
    pub fn agent_control(&self) -> Option<&Arc<dyn AgentControlPort>> {
        self.agent_control.as_ref()
    }

    /// Where the run's questions for the user go, when the host can ask one.
    ///
    /// `None` means nobody can be asked; `request_user_input` answers the model that it is not
    /// enabled.
    #[must_use]
    pub fn user_input(&self) -> Option<&Arc<dyn UserInputPort>> {
        self.user_input.as_ref()
    }
}

impl fmt::Debug for ToolServices {
    /// Reports which ports are installed, never what they hold.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolServices")
            .field("event_sink", &self.event_sink.is_some())
            .field("output_projector", &self.output_projector.is_some())
            .field("agent_control", &self.agent_control.is_some())
            .field("user_input", &self.user_input.is_some())
            .finish_non_exhaustive()
    }
}
