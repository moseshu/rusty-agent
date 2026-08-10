//! Runtime-neutral invocation envelope.

use core::{any::Any, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{item::CallId, state::WorkStateHandle};

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

/// Type-erased host context made available to tool implementations.
///
/// `ra-runtime` will provide its concrete `ToolContext` in R3-9. Keeping this tiny trait in core
/// avoids a reverse dependency while still allowing business tools to downcast to their host
/// context today.
pub trait ToolRuntimeContext: Any + Send + Sync {
    /// Enables checked downcasting to an application-owned context type.
    fn as_any(&self) -> &(dyn Any + Send + Sync);
}

impl<T> ToolRuntimeContext for T
where
    T: Any + Send + Sync,
{
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[derive(Debug)]
struct EmptyToolRuntimeContext;

static EMPTY_CONTEXT: EmptyToolRuntimeContext = EmptyToolRuntimeContext;

/// One invocation after the registry has resolved a [`ToolLookupKey`](super::ToolLookupKey).
#[non_exhaustive]
pub struct ToolInvocation<'a> {
    call_id: &'a CallId,
    arguments: &'a Value,
    caller: ToolCaller,
    context: &'a dyn ToolRuntimeContext,
    work_state: Option<&'a dyn WorkStateHandle>,
}

impl<'a> ToolInvocation<'a> {
    /// Creates a direct invocation without application context.
    #[must_use]
    pub const fn new(call_id: &'a CallId, arguments: &'a Value) -> Self {
        Self {
            call_id,
            arguments,
            caller: ToolCaller::Direct,
            context: &EMPTY_CONTEXT,
            work_state: None,
        }
    }

    /// Sets the caller class.
    #[must_use]
    pub const fn with_caller(mut self, caller: ToolCaller) -> Self {
        self.caller = caller;
        self
    }

    /// Attaches an application-owned runtime context.
    #[must_use]
    pub const fn with_context(mut self, context: &'a dyn ToolRuntimeContext) -> Self {
        self.context = context;
        self
    }

    /// Attaches the task state this run participates in (R3-13).
    #[must_use]
    pub const fn with_work_state(mut self, work_state: &'a dyn WorkStateHandle) -> Self {
        self.work_state = Some(work_state);
        self
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

    /// Type-erased host context.
    #[must_use]
    pub const fn context(&self) -> &dyn ToolRuntimeContext {
        self.context
    }

    /// The task state spanning this run, when the host attached one.
    ///
    /// `None` is the ordinary case, not a failure: a run belongs to a task only when something
    /// above it says so. R17-1 puts the typed channel operations on [`WorkStateHandle`]; this
    /// accessor does not change when it does, which is the whole reason the slot exists now
    /// (R3-13).
    #[must_use]
    pub const fn work_state(&self) -> Option<&dyn WorkStateHandle> {
        self.work_state
    }
}

impl fmt::Debug for ToolInvocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInvocation")
            .field("call_id", self.call_id)
            .field("arguments", &"<redacted>")
            .field("caller", &self.caller)
            .field("context", &"<runtime-context>")
            .field("work_state", &self.work_state.map(|_| "<work-state>"))
            .finish()
    }
}
