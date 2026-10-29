//! Optional host hooks, following the Codex event surface rather than SDK lifecycle observers.
//!
//! Session/prompt acceptance, tool, permission-request and stop events accept control decisions.
//! The runtime validates a returned decision against its event; unsupported decisions are reported
//! and ignored. Hooks cannot override a permission-policy denial. No hooks are installed by default.
//!
//! This is an in-process contract. Command discovery, JSON/exit-code parsing and provider-specific
//! input translation belong to host adapters. Context messages use a dedicated history queue and
//! never pass through input contributors. Session initialization/end belong to the host: a run is
//! not a session. After successful compaction the runtime re-establishes the identified session. Subagent identity must likewise be supplied by the
//! host, never guessed from an agent name. Stop continuation uses ordinary user-role history and
//! the existing run budget, not a separate retry budget.
//!
//! The SDK lifecycle observers this surface is *not* live next door, in
//! [`lifecycle`](crate::lifecycle). That family decides nothing at any of its moments, so an
//! observer never lands in the count kept of what changed a run; it also propagates a callback's
//! failure instead of reporting and ignoring it, because an observer that failed has no verdict
//! to fall back to. The two are installed separately and neither can stand in for the other.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    cancel::{CancelReason, CancelScope},
    context::RunContext,
    error::Result,
    finish::FinishReason,
    item::{CallId, Compaction, ItemId, Message, ModelInputItem, ToolCallOutput},
    session::SessionId,
    state::RunId,
    tool::{ToolOrigin, ToolOutput, ToolServices},
};

/// Lifecycle boundaries understood by host hooks.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    /// A host session opens, resumes, or re-establishes context after compaction.
    SessionStart,
    /// A host session closes.
    SessionEnd,
    /// A host accepts a new user prompt.
    UserPromptSubmit,
    /// Before native argument decoding and permission admission.
    PreToolUse,
    /// After a tool successfully produces its complete output.
    PostToolUse,
    /// A call would otherwise require host approval.
    PermissionRequest,
    /// Before delivering the run's own final output.
    Stop,
    /// Before an actual compaction attempt.
    PreCompact,
    /// After an actual compaction produces a summary.
    PostCompact,
    /// A run is interrupted by cancellation.
    Interrupt,
    /// A child run starts for the first time.
    SubagentStart,
    /// Before delivering a child run's own final output.
    SubagentStop,
}

impl HookEventName {
    /// Stable label for configuration and observability.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::Stop => "stop",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::Interrupt => "interrupt",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
        }
    }
}

/// Why the host is compacting history.
///
/// The framework's own compaction is threshold-driven and always reports `Automatic`. `Manual` is
/// reachable through [`ContextProcessorRequest::notify_pre_compact`], which is public precisely so
/// a host processor answering a user's explicit compact command can say so.
///
/// [`ContextProcessorRequest::notify_pre_compact`]: crate::capability::ContextProcessorRequest::notify_pre_compact
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    /// The context policy requires a smaller model-facing history.
    Automatic,
    /// The user or host explicitly requested compaction.
    Manual,
}

impl CompactTrigger {
    /// Stable label, so a host adapter forwarding this event does not invent its own.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }
}

/// Facts about the call being checked, before provider-specific serialization.
#[derive(Debug, Clone, Copy)]
pub struct ToolHookData<'a> {
    origin: &'a ToolOrigin,
    call_id: &'a CallId,
    arguments: &'a serde_json::Value,
}

impl<'a> ToolHookData<'a> {
    /// Creates a borrowed call view.
    #[must_use]
    pub const fn new(
        origin: &'a ToolOrigin,
        call_id: &'a CallId,
        arguments: &'a serde_json::Value,
    ) -> Self {
        Self {
            origin,
            call_id,
            arguments,
        }
    }
    /// Routing identity of the tool, including its namespace.
    #[must_use]
    pub const fn origin(&self) -> &'a ToolOrigin {
        self.origin
    }
    /// Identity of this invocation.
    #[must_use]
    pub const fn call_id(&self) -> &'a CallId {
        self.call_id
    }
    /// Arguments that will actually be passed to the tool.
    #[must_use]
    pub const fn arguments(&self) -> &'a serde_json::Value {
        self.arguments
    }
}

/// The candidate delivery a stop hook can ask the model to continue working on.
#[derive(Debug, Clone, Copy)]
pub struct StopHookData<'a> {
    reason: FinishReason,
    message: Option<&'a Message>,
    tool_outputs: &'a [ToolCallOutput],
    stop_hook_active: bool,
}

impl<'a> StopHookData<'a> {
    /// Creates the delivery view, including tool-stop outputs when there is no assistant message.
    #[must_use]
    pub const fn new(
        reason: FinishReason,
        message: Option<&'a Message>,
        tool_outputs: &'a [ToolCallOutput],
        stop_hook_active: bool,
    ) -> Self {
        Self {
            reason,
            message,
            tool_outputs,
            stop_hook_active,
        }
    }
    /// Why the run proposes to deliver this output.
    #[must_use]
    pub const fn reason(&self) -> FinishReason {
        self.reason
    }
    /// The last assistant message in the concluding turn, if any.
    #[must_use]
    pub const fn message(&self) -> Option<&'a Message> {
        self.message
    }
    /// Tool outputs from the concluding turn.
    #[must_use]
    pub const fn tool_outputs(&self) -> &'a [ToolCallOutput] {
        self.tool_outputs
    }
    /// Whether a previous stop hook already requested continuation in this run.
    /// This is information for the hook, not an automatic limit on further blocks.
    #[must_use]
    pub const fn stop_hook_active(&self) -> bool {
        self.stop_hook_active
    }
}

/// Why a host session starts or its context is re-established, following Codex.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartSource {
    /// A new conversation.
    Startup,
    /// Previously stored or explicitly supplied history.
    Resume,
    /// A fresh conversation created by clearing another conversation.
    Clear,
    /// Context re-establishment after successful compaction.
    Compact,
    /// History copied from a parent thread.
    Fork,
}
impl SessionStartSource {
    /// Stable hook matcher label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
            Self::Fork => "fork",
        }
    }
}

/// Typed input for one host event. Runtime-only references never become model input implicitly.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum HookEvent<'a> {
    /// Emitted by the session owner at initialization and by the runtime after compaction.
    /// The owner identity must be supplied; an ordinary runner invocation is not session startup.
    SessionStart {
        /// Session identity supplied by its owner.
        session_id: &'a SessionId,
        /// Whether the source is Resume; retained for legacy observers.
        resumed: bool,
        /// The actual lifecycle source; use this for hook selection.
        source: SessionStartSource,
    },
    /// Emitted by the session owner when the session really ends.
    SessionEnd {
        /// Session identity supplied by its owner.
        session_id: &'a SessionId,
        /// Host-provided reason the session closed.
        reason: &'a str,
    },
    /// Inspects newly submitted user input, excluding prior history and contextual messages.
    /// The session host emits this for initial input. The runtime emits it once per steered
    /// message when delivering `PendingInput`, including for standalone `Runner` requests.
    /// Hosts must not also emit this event when enqueueing steered messages.
    UserPromptSubmit {
        /// Newly submitted input, excluding prior conversation history.
        input: &'a [ModelInputItem],
    },
    /// May rewrite or deny this call, but may not grant permission.
    PreToolUse(ToolHookData<'a>),
    /// Observes complete successful output before output guardrails or context projection.
    PostToolUse {
        /// The executed call.
        call: ToolHookData<'a>,
        /// Complete successful tool result.
        output: &'a ToolOutput,
    },
    /// May resolve an otherwise pending approval, without overriding policy denial.
    PermissionRequest(ToolHookData<'a>),
    /// May request continuation with a non-empty prompt.
    Stop(StopHookData<'a>),
    /// Emitted only after the context processor chooses to attempt compaction.
    PreCompact {
        /// Identity reserved for the resulting compaction record.
        record_id: &'a ItemId,
        /// Why compaction was requested.
        trigger: CompactTrigger,
    },
    /// Emitted only after the context processor successfully creates a compaction.
    PostCompact {
        /// Identity reserved for the resulting compaction record.
        record_id: &'a ItemId,
        /// Why compaction was requested.
        trigger: CompactTrigger,
        /// The newly created summary and covered record identities.
        compaction: &'a Compaction,
    },
    /// Notification after cancellation. Its callback runs with a separate bounded cleanup scope.
    Interrupt {
        /// Cancellation's recorded root cause.
        reason: &'a CancelReason,
    },
    /// Emitted once for a child run, never again for approval resume.
    SubagentStart {
        /// Explicit parent run identity.
        parent_run_id: &'a RunId,
    },
    /// A child delivery uses this instead of the root Stop event.
    SubagentStop {
        /// Explicit parent run identity.
        parent_run_id: &'a RunId,
        /// Candidate child output and continuation state.
        delivery: StopHookData<'a>,
    },
}

impl HookEvent<'_> {
    /// Event name used to select registered callbacks.
    #[must_use]
    pub const fn name(&self) -> HookEventName {
        match self {
            Self::SessionStart { .. } => HookEventName::SessionStart,
            Self::SessionEnd { .. } => HookEventName::SessionEnd,
            Self::UserPromptSubmit { .. } => HookEventName::UserPromptSubmit,
            Self::PreToolUse(_) => HookEventName::PreToolUse,
            Self::PostToolUse { .. } => HookEventName::PostToolUse,
            Self::PermissionRequest(_) => HookEventName::PermissionRequest,
            Self::Stop(_) => HookEventName::Stop,
            Self::PreCompact { .. } => HookEventName::PreCompact,
            Self::PostCompact { .. } => HookEventName::PostCompact,
            Self::Interrupt { .. } => HookEventName::Interrupt,
            Self::SubagentStart { .. } => HookEventName::SubagentStart,
            Self::SubagentStop { .. } => HookEventName::SubagentStop,
        }
    }
    /// Call identity for tool events; other events are attributed to their run.
    #[must_use]
    pub const fn call_id(&self) -> Option<&CallId> {
        match self {
            Self::PreToolUse(call)
            | Self::PermissionRequest(call)
            | Self::PostToolUse { call, .. } => Some(call.call_id),
            _ => None,
        }
    }
}

/// Explicit control result. The runtime rejects effects unsupported by the current event.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookDecision {
    /// No control effect; an approval remains pending.
    #[default]
    Continue,
    /// Replaces native tool arguments before decoding, permission and input guardrails.
    UpdateInput {
        /// Complete replacement arguments, supported only for pre-tool events.
        input: serde_json::Value,
    },
    /// Replaces the model-facing successful tool result while retaining the original output.
    ReplaceOutput {
        /// Replacement text, supported only for post-tool events.
        message: String,
    },
    /// Grants this one pending permission request only.
    Allow,
    /// Rejects a tool result/request or host input. At stop, ends processing without continuation.
    Deny {
        /// Model-facing explanation of the refused call.
        message: String,
    },
    /// Blocks a stop or subagent-stop delivery, appending this prompt before continuing.
    Block {
        /// Model-facing request for further work; must contain non-whitespace text.
        prompt: String,
    },
}

/// Context available only to the host callback.
pub struct UserHookContext<'a> {
    run: &'a RunContext,
    cancel: &'a CancelScope,
    services: &'a ToolServices,
    context_output: Option<&'a HookContextQueue>,
}

impl<'a> UserHookContext<'a> {
    /// Binds the callback to a run and its own cancellation scope.
    #[must_use]
    pub const fn new(
        run: &'a RunContext,
        cancel: &'a CancelScope,
        services: &'a ToolServices,
    ) -> Self {
        Self {
            run,
            cancel,
            services,
            context_output: services.hook_context(),
        }
    }
    /// Selects the run-owned context destination without changing callback construction.
    #[must_use]
    pub const fn with_context_output(mut self, output: &'a HookContextQueue) -> Self {
        self.context_output = Some(output);
        self
    }
    /// Context destination shared with the host for background results.
    #[must_use]
    pub const fn context_output(&self) -> Option<&'a HookContextQueue> {
        self.context_output
    }
    /// The public agent and current run accounting.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }
    /// Cancelled when the callback times out or the enclosing operation stops.
    #[must_use]
    pub const fn cancel(&self) -> &'a CancelScope {
        self.cancel
    }
    /// Optional host ports; none of these are serialized into model input.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }
}

/// A host-supplied callback, installed for specific events by the runtime.
///
/// Names are display labels and may repeat. Errors and timeouts are reported as failed hook
/// executions and have no control effect, following Codex. Permission policy remains authoritative.
/// Callbacks must own cleanup for any tasks or child processes they spawn: cancellation drops
/// their future. The runtime itself does not detach callback tasks.
#[async_trait]
pub trait UserHook: Send + Sync + 'static {
    /// Display label for reports. It is not a registry lookup key.
    fn name(&self) -> &str;
    /// Binds background context delivery to the active run without executing an event.
    /// Hosts retain results rejected by a closed destination for the next run.
    fn bind_context(&self, _run: &RunContext, _output: &HookContextQueue) {}

    /// Handles a selected event. Returning Continue observes it without changing control flow.
    async fn call(
        &self,
        context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision>;
}

/// A run-bound dispatcher port for lower layers and host-owned lifecycle boundaries.
///
/// The runtime implements execution, validation, timeouts and reporting. A context processor uses
/// this same port for compact events instead of depending upward on the runtime crate.
#[async_trait]
pub trait UserHookDispatcher: Send + Sync {
    /// Runs the callbacks registered for the event and returns their validated aggregate decision.
    async fn dispatch(&self, event: HookEvent<'_>) -> Result<HookDecision>;
}

/// Status of one callback, reported even when it produced no control effect.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRunStatus {
    /// Returned a supported decision.
    Completed,
    /// Returned an error.
    Failed,
    /// Exceeded its callback deadline.
    TimedOut,
    /// Interrupted by the enclosing cancellation scope.
    Cancelled,
    /// Returned an unsupported or empty control effect, which was ignored.
    Ignored,
}

/// Host-visible evidence for one callback. This never enters model history automatically.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookReport {
    hook: String,
    event: HookEventName,
    call_id: Option<CallId>,
    status: HookRunStatus,
    decision: HookDecision,
    warning: Option<String>,
}

impl HookReport {
    /// Creates a completed callback report with its validated effect.
    #[must_use]
    pub fn new(
        hook: impl Into<String>,
        event: HookEventName,
        status: HookRunStatus,
        decision: HookDecision,
    ) -> Self {
        Self {
            hook: hook.into(),
            event,
            call_id: None,
            status,
            decision,
            warning: None,
        }
    }
    /// Attaches tool-call attribution.
    #[must_use]
    pub fn with_call_id(mut self, call_id: Option<CallId>) -> Self {
        self.call_id = call_id;
        self
    }
    /// Attaches a host-facing diagnostic, never a model-facing prompt.
    #[must_use]
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(warning.into());
        self
    }
    /// Callback display label.
    #[must_use]
    pub fn hook(&self) -> &str {
        &self.hook
    }
    /// Boundary at which this callback ran.
    #[must_use]
    pub const fn event(&self) -> HookEventName {
        self.event
    }
    /// Tool-call attribution when applicable.
    #[must_use]
    pub const fn call_id(&self) -> Option<&CallId> {
        self.call_id.as_ref()
    }
    /// Callback completion status.
    #[must_use]
    pub const fn status(&self) -> HookRunStatus {
        self.status
    }
    /// Validated effect; failed or ignored callbacks report Continue.
    #[must_use]
    pub const fn decision(&self) -> &HookDecision {
        &self.decision
    }
    /// Diagnostic for host UI and logs.
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
}

/// Run-owned contextual history, separate from steered user input.
///
/// Accepted entries are drained into authoritative history after tool outputs. Closing and
/// accepting are atomic, so a host can retain late background results for the next run.
#[derive(Debug, Clone, Default)]
pub struct HookContextQueue {
    state: std::sync::Arc<std::sync::Mutex<HookContextState>>,
}

#[derive(Debug, Default)]
struct HookContextState {
    closed: bool,
    messages: Vec<Message>,
    follow_up: bool,
}

impl HookContextQueue {
    /// Records synchronous context without requesting follow-up sampling.
    /// Returns ownership when the run no longer accepts messages.
    pub fn push(&self, messages: Vec<Message>) -> std::result::Result<(), Vec<Message>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(messages);
        }
        state.messages.extend(messages);
        Ok(())
    }
    /// Queues a completed background result and requests follow-up sampling when necessary.
    pub fn push_background(&self, messages: Vec<Message>) -> std::result::Result<(), Vec<Message>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(messages);
        }
        if !messages.is_empty() {
            state.follow_up = true;
        }
        state.messages.extend(messages);
        Ok(())
    }
    /// Whether pending background context requires another model request.
    #[must_use]
    pub fn requires_follow_up(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .follow_up
    }
    /// Takes accepted context without closing the destination.
    #[must_use]
    pub fn take(&self) -> Vec<Message> {
        self.take_with_follow_up().0
    }
    /// Atomically drains context and its background follow-up intent.
    #[must_use]
    pub fn take_with_follow_up(&self) -> (Vec<Message>, bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let follow_up = std::mem::take(&mut state.follow_up);
        (std::mem::take(&mut state.messages), follow_up)
    }
    /// Whether the run no longer accepts context through this handle.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }

    /// Whether context is waiting for delivery.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .is_empty()
    }
    /// Closes atomically when no accepted context remains.
    #[must_use]
    pub fn close_if_empty(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.messages.is_empty() {
            state.closed = true;
        }
        state.closed
    }
    /// Closes this lane and another input lane under one acceptance boundary.
    /// The other lane must close only when empty and return whether it closed.
    #[must_use]
    pub fn close_if_empty_with(&self, close_other: impl FnOnce() -> bool) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.messages.is_empty() {
            return false;
        }
        if close_other() {
            state.closed = true;
        }
        state.closed
    }

    /// Closes and takes any undelivered entries for checkpoint persistence.
    #[must_use]
    pub fn close(&self) -> Vec<Message> {
        self.close_with_follow_up().0
    }
    /// Closes acceptance and atomically drains messages with their persisted wakeup intent.
    #[must_use]
    pub fn close_with_follow_up(&self) -> (Vec<Message>, bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        let follow_up = std::mem::take(&mut state.follow_up);
        (std::mem::take(&mut state.messages), follow_up)
    }
}
