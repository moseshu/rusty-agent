//! Optional host hooks, following the Codex event surface rather than SDK lifecycle observers.
//!
//! Only pre-tool, permission-request, stop, and subagent-stop events accept control decisions.
//! The runtime validates a returned decision against its event; unsupported decisions are reported
//! and ignored. Hooks cannot override a permission-policy denial. No hooks are installed by default.
//!
//! This is an in-process contract. Command discovery, JSON/exit-code parsing, input rewrites, and
//! Codex-specific context injection belong to host adapters. Session events are explicitly emitted
//! by the session owner: a run is not a session. Subagent identity must likewise be supplied by the
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    /// A host session opens or resumes.
    SessionStart,
    /// A host session closes.
    SessionEnd,
    /// A host accepts a new user prompt.
    UserPromptSubmit,
    /// Before an admitted tool call executes.
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

/// Typed input for one host event. Runtime-only references never become model input implicitly.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum HookEvent<'a> {
    /// Emitted by the session owner, not for every runner invocation.
    SessionStart {
        /// Session identity supplied by its owner.
        session_id: &'a SessionId,
        /// Whether this opens a previously stored session.
        resumed: bool,
    },
    /// Emitted by the session owner when the session really ends.
    SessionEnd {
        /// Session identity supplied by its owner.
        session_id: &'a SessionId,
        /// Host-provided reason the session closed.
        reason: &'a str,
    },
    /// Emitted by the host for the new prompt alone, not a resumed transcript projection.
    UserPromptSubmit {
        /// Newly submitted input, excluding prior conversation history.
        input: &'a [ModelInputItem],
    },
    /// May deny this call, but may not grant permission.
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
    /// Grants this one pending permission request only.
    Allow,
    /// Denies a pre-tool or permission request, returning this message to the model.
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
        }
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
