//! The call-level context: one resolved invocation, and the run it belongs to.

use core::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    context::RunContext,
    error::{Error, Result},
    event::HostEventEmitter,
    item::CallId,
    tool::{DecodedToolInput, Tool, ToolInput, ToolOrigin, ToolServices},
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
    decoded_input: Option<DecodedToolInput>,
    resume_state: Option<&'a Value>,
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
            decoded_input: None,
            resume_state: None,
        }
    }

    /// Hands the call the state its tool kept with the approval this call resumes from.
    ///
    /// The runtime sets it when it resumes an approved call whose approval carries a
    /// [`ToolResume`](crate::item::ToolResume); a direct caller may set it to drive a tool through
    /// that resumption in isolation.
    pub const fn with_resume_state(mut self, state: &'a Value) -> Self {
        self.resume_state = Some(state);
        self
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

    /// Adds the schema-bound input decoded by the common invocation entry.
    ///
    /// Tools that expose a [`FuncSchema`](super::FuncSchema) receive this value during runtime
    /// dispatch. Direct callers may still construct a context without it while testing a tool in
    /// isolation.
    pub fn with_decoded_input(mut self, decoded_input: DecodedToolInput) -> Self {
        self.decoded_input = Some(decoded_input);
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

    /// The state the tool kept with the approval this call resumes from, if any: see
    /// [`ToolResume`](crate::item::ToolResume).
    #[must_use]
    pub const fn resume_state(&self) -> Option<&Value> {
        self.resume_state
    }

    /// Borrows the schema-bound input when this context carries one of type `T`.
    ///
    /// A type mismatch is a framework wiring error, distinct from a context that simply carries
    /// no decoded input for an untyped tool.
    pub fn decoded_input<T: ToolInput>(&self) -> Result<Option<&T>> {
        let Some(decoded) = self.decoded_input.as_ref() else {
            return Ok(None);
        };
        decoded.downcast_ref::<T>().map(Some).ok_or_else(|| {
            Error::caller(format!(
                "tool context holds decoded input `{}`, not `{}`",
                decoded.type_name(),
                core::any::type_name::<T>()
            ))
        })
    }

    /// Takes the schema-bound input when this context carries one of type `T`.
    ///
    /// This avoids cloning a typed argument object merely because tool invocation owns its input.
    pub fn take_decoded_input<T: ToolInput>(&mut self) -> Result<Option<T>> {
        self.decoded_input
            .take()
            .map(DecodedToolInput::downcast::<T>)
            .transpose()
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
            .field("has_decoded_input", &self.decoded_input.is_some())
            .field("caller", &self.caller)
            .field("services", self.services)
            .finish()
    }
}

/// A tool's request, from inside its invocation, that its host approve the call before it goes on.
///
/// A tool returns it as the error of [`Tool::call`], through
/// [`Self::into_error`]. The runtime then records an approval for the call carrying `state` as a
/// [`ToolResume::during_call`](crate::item::ToolResume::during_call), instead of treating the call
/// as failed, and on approval invokes the tool again with that state in
/// [`ToolContext::resume_state`]. A rejected or cancelled call is not invoked again.
///
/// This adapts an approval that a tool would otherwise wait for inside its invocation, as Codex's
/// command execution does after its sandbox denies a first attempt: an interrupted run cannot keep
/// an invocation suspended while its host answers, possibly in another process.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolApprovalRequest {
    state: Value,
}

impl ToolApprovalRequest {
    /// A request that keeps `state` until the approved call resumes.
    #[must_use]
    pub const fn new(state: Value) -> Self {
        Self { state }
    }

    /// The state kept until the approved call resumes.
    #[must_use]
    pub const fn state(&self) -> &Value {
        &self.state
    }

    /// The error a tool named `tool` returns to make the request.
    #[must_use]
    pub fn into_error(self, tool: &str) -> Error {
        Error::tool(
            crate::error::ToolErrorKind::PermissionDenied,
            tool,
            "the call is waiting for its host to approve it",
        )
        .with_source(self)
    }

    /// The request `error` makes, if it is one.
    #[must_use]
    pub fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }
}

impl fmt::Display for ToolApprovalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the call is waiting for its host to approve it")
    }
}

impl std::error::Error for ToolApprovalRequest {}
