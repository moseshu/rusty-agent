//! The call-level context: one resolved invocation, and the run it belongs to.

use core::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    context::RunContext,
    event::HostEventEmitter,
    item::CallId,
    tool::{Tool, ToolOrigin, ToolServices},
};

/// How a tool invocation entered the runtime.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCaller {
    /// The model called the tool directly.
    Direct,
    /// Model-generated code called the tool through a programmatic tool runtime.
    Programmatic,
}

/// The empty bag a call that installs no ports is given.
///
/// A borrowed empty value rather than an `Option`, so `context.services().work_state()` reads the
/// same whether or not the host installed anything.
static NO_SERVICES: ToolServices = ToolServices::new();

/// One invocation after the registry has resolved a [`ToolLookupKey`](super::ToolLookupKey).
///
/// This is the whole of what a tool is handed, and it is the lower of the framework's two context
/// layers — [`RunContext`] is the other. There is no third: the run is reached through
/// [`Self::run`], and application state through
/// [`RunContext::app_context`](crate::context::RunContext::app_context) beyond it, so a tool has one
/// path to host state instead of one per accessor that happens to return something type-erased.
///
/// # Identity is the origin, not a name
///
/// [`Self::origin`] is the single authoritative identity of the tool being called; its
/// [`lookup_key`](ToolOrigin::lookup_key) is what routing and persistence use. No string name and
/// no second key are carried beside it, because two identities for one call are two things that can
/// disagree, and reconstructing one by splitting the other's dotted display name is ambiguous the
/// moment a namespace or a tool name contains a dot.
#[must_use]
#[non_exhaustive]
pub struct ToolContext<'a> {
    run: &'a RunContext,
    origin: &'a ToolOrigin,
    call_id: &'a CallId,
    arguments: &'a Value,
    caller: ToolCaller,
    services: &'a ToolServices,
}

impl<'a> ToolContext<'a> {
    /// Creates the context of one direct call, with no ports installed.
    ///
    /// The origin is always derived from `tool`, so callers cannot invoke one implementation while
    /// attributing the call to another tool's identity.
    pub fn new(
        run: &'a RunContext,
        tool: &'a dyn Tool,
        call_id: &'a CallId,
        arguments: &'a Value,
    ) -> Self {
        Self {
            run,
            origin: tool.origin(),
            call_id,
            arguments,
            caller: ToolCaller::Direct,
            services: &NO_SERVICES,
        }
    }

    /// Sets the caller class.
    pub const fn with_caller(mut self, caller: ToolCaller) -> Self {
        self.caller = caller;
        self
    }

    /// Installs the framework ports this call may reach.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run this call is part of: its identity, its public agent, and the host's own state.
    #[must_use]
    pub const fn run(&self) -> &RunContext {
        self.run
    }

    /// Stable identity of the tool being called.
    #[must_use]
    pub const fn origin(&self) -> &ToolOrigin {
        self.origin
    }

    /// Provider call ID paired with the eventual result.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        self.call_id
    }

    /// Parsed model arguments.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        self.arguments
    }

    /// Caller class used by [`ToolOptions`](super::ToolOptions) admission checks.
    #[must_use]
    pub const fn caller(&self) -> ToolCaller {
        self.caller
    }

    /// Framework ports available to this call.
    pub const fn services(&self) -> &ToolServices {
        self.services
    }

    /// Constructs a [`HostEventEmitter`] if both an event allocator and sink are present.
    #[must_use]
    pub fn event_emitter(&self) -> Option<HostEventEmitter> {
        let sink = self.services.event_sink()?;
        self.run.event_emitter(Arc::clone(sink))
    }
}

impl fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("run", self.run)
            .field("tool", &self.origin.qualified_name())
            .field("call_id", self.call_id)
            .field("arguments", &"<redacted>")
            .field("caller", &self.caller)
            .field("services", self.services)
            .finish()
    }
}
