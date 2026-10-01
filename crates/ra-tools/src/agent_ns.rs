//! The collaboration tools: spawn, message, follow up, interrupt, wait for and list agents.
//!
//! Ported from Codex's `MultiAgentV2` tool surface (`core/src/tools/handlers/multi_agents_v2*` and
//! `multi_agents_spec.rs`): the same six names, arguments, descriptions, outputs and model-facing
//! refusals. Each is a thin projection of the [`AgentControlPort`] the run was given through
//! [`ToolServices`](ra_core::tool::ToolServices); the tree, its mailboxes and the background runs
//! live in the runtime. A run that takes part in no agent tree gets a model-visible answer that
//! collaboration is not enabled, not a failed turn.
//!
//! Together they are how a parent stops waiting on a stalled child: `wait_agent` returns when the
//! parent's mailbox receives something *or* when its timeout elapses, after which the parent can
//! redirect the child with `followup_task` or `send_message` — delivered at the child's next model
//! call — stop its current run with `interrupt_agent`, or carry on without it.
//!
//! # Deviations from Codex
//!
//! - `spawn_agent` takes no `model` or `reasoning_effort`; Codex also hides those behind
//!   configuration.
//! - The argument schemas are non-strict, as Codex declares them, so optional arguments may be
//!   omitted rather than sent as `null`.
//! - The tools are offered under their bare names; Codex's optional `collaboration` namespace is a
//!   product setting.
//! - In a tree given a depth limit, an agent at the limit is not offered the tools, as Codex's first
//!   multi-agent version hides them; the version these tools port has no depth limit.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use ra_core::{
    agent::{
        AgentId,
        control::{
            AgentControlError, AgentControlPort, AgentStatus, LiveAgent, MessageDeliveryMode,
            SpawnAgentForkMode, SpawnAgentRequest, WaitOutcome,
        },
    },
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    permission::PermissionScope,
    tool::{
        DecodedToolInput, FuncSchema, Tool, ToolAvailability, ToolContext, ToolFailureHandling,
        ToolInput, ToolOptions, ToolOrigin, ToolOutput, ToolSchema, ToolServices,
    },
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

/// Codex's default `wait_agent` timeout.
pub const DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
/// Codex's default floor on a `wait_agent` timeout; shorter requests are raised to it.
pub const MIN_WAIT_TIMEOUT_MS: i64 = 10_000;
/// Codex's default ceiling on a `wait_agent` timeout; longer requests are refused.
pub const MAX_WAIT_TIMEOUT_MS: i64 = 3_600_000;

/// Timeout policy of `wait_agent`, Codex's `WaitAgentTimeoutOptions`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitAgentTimeoutOptions {
    default: i64,
    min: i64,
    max: i64,
}

impl WaitAgentTimeoutOptions {
    /// Codex's defaults: 30 s, at least 10 s, at most one hour.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            default: DEFAULT_WAIT_TIMEOUT_MS,
            min: MIN_WAIT_TIMEOUT_MS,
            max: MAX_WAIT_TIMEOUT_MS,
        }
    }

    /// Replaces all three bounds.
    #[must_use]
    pub const fn with_bounds(mut self, default_ms: i64, min_ms: i64, max_ms: i64) -> Self {
        self.default = default_ms;
        self.min = min_ms;
        self.max = max_ms;
        self
    }

    /// Used when the call names no timeout.
    #[must_use]
    pub const fn default_timeout_ms(&self) -> i64 {
        self.default
    }

    /// Shorter requests are raised to this.
    #[must_use]
    pub const fn min_timeout_ms(&self) -> i64 {
        self.min
    }

    /// Longer requests are refused.
    #[must_use]
    pub const fn max_timeout_ms(&self) -> i64 {
        self.max
    }
}

impl Default for WaitAgentTimeoutOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The six collaboration tools, with Codex's default `wait_agent` timeouts.
///
/// # Errors
///
/// Returns a configuration error when a tool's identity or schema cannot be built.
pub fn collaboration_tools() -> Result<Vec<CollaborationTool>> {
    collaboration_tools_with(WaitAgentTimeoutOptions::new())
}

/// The six collaboration tools, with `wait` as the `wait_agent` timeout policy.
///
/// # Errors
///
/// Returns a configuration error when a tool's identity or schema cannot be built.
pub fn collaboration_tools_with(wait: WaitAgentTimeoutOptions) -> Result<Vec<CollaborationTool>> {
    [
        CollaborationKind::SpawnAgent,
        CollaborationKind::SendMessage,
        CollaborationKind::FollowupTask,
        CollaborationKind::InterruptAgent,
        CollaborationKind::WaitAgent,
        CollaborationKind::ListAgents,
    ]
    .into_iter()
    .map(|kind| CollaborationTool::new(kind, wait))
    .collect()
}

/// Which collaboration tool.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollaborationKind {
    /// `spawn_agent`.
    SpawnAgent,
    /// `send_message`: queue a message, never start an idle agent.
    SendMessage,
    /// `followup_task`: queue a task and start the agent if it is idle.
    FollowupTask,
    /// `interrupt_agent`.
    InterruptAgent,
    /// `wait_agent`.
    WaitAgent,
    /// `list_agents`.
    ListAgents,
}

impl CollaborationKind {
    /// The model-facing name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SpawnAgent => "spawn_agent",
            Self::SendMessage => "send_message",
            Self::FollowupTask => "followup_task",
            Self::InterruptAgent => "interrupt_agent",
            Self::WaitAgent => "wait_agent",
            Self::ListAgents => "list_agents",
        }
    }
}

const SPAWN_AGENT_DESCRIPTION: &str = "Spawns an agent to work on the specified task. If your current task is `/root/task1` and you spawn_agent with task_name \"task_3\" the agent will have canonical task name `/root/task1/task_3`.
You are then able to refer to this agent as `task_3` or `/root/task1/task_3` interchangeably. However an agent `/root/task2/task_3` would only be able to communicate with this agent via its canonical name `/root/task1/task_3`.
The spawned agent will have the same tools as you and the ability to spawn its own subagents.
It will be able to send you and other running agents messages, and its final answer will be provided to you when it finishes.
The new agent's canonical task name will be provided to it along with the message.

Note that passing `fork_turns=\"none\"` will not pass any surrounding context to the spawned subagent, which may cause the agent to lack the context it needs to complete its task, whereas `fork_turns=\"all\"` will provide the subagent with all surrounding context.";

const SEND_MESSAGE_DESCRIPTION: &str = "Send a message to an existing agent. The message will be delivered promptly. Does not trigger a new turn.";

const FOLLOWUP_TASK_DESCRIPTION: &str = "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes.";

const INTERRUPT_AGENT_DESCRIPTION: &str = "Interrupt an agent's current turn, if any, and return its previous status. The agent remains available for messages and follow-up tasks.";

const WAIT_AGENT_DESCRIPTION: &str = "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. Does not return the content; returns either a summary of which agents have updates (if any) or a timeout summary if no activity arrives before the deadline.";

const LIST_AGENTS_DESCRIPTION: &str =
    "List live agents in the current root thread tree. Optionally filter by task-path prefix.";

const NOT_ENABLED: &str = "multi-agent collaboration is not enabled for this run";

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
/// Spawns an agent.
struct SpawnAgentInput {
    /// Task name for the new agent. Use lowercase letters, digits, and underscores.
    task_name: String,
    /// Initial plain-text task for the new agent.
    message: String,
    /// Agent type override for the new agent. Omit unless explicitly asked. The selected role
    /// applies regardless of how much parent history is inherited.
    agent_type: Option<String>,
    /// Optional number of turns to fork. Defaults to `all`. Use `none`, `all`, or a positive
    /// integer string such as `3` to fork only the most recent turns.
    fork_turns: Option<String>,
}

impl SpawnAgentInput {
    /// Codex's `SpawnAgentArgs::fork_mode`.
    fn fork_mode(&self) -> std::result::Result<Option<SpawnAgentForkMode>, &'static str> {
        let fork_turns = self
            .fork_turns
            .as_deref()
            .map(str::trim)
            .filter(|fork_turns| !fork_turns.is_empty())
            .unwrap_or("all");
        if fork_turns.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        if fork_turns.eq_ignore_ascii_case("all") {
            return Ok(Some(SpawnAgentForkMode::FullHistory));
        }
        match fork_turns.parse::<usize>() {
            Ok(turns) if turns > 0 => Ok(Some(SpawnAgentForkMode::LastNTurns(turns))),
            _ => Err("fork_turns must be `none`, `all`, or a positive integer string"),
        }
    }
}

// Field docs are Codex's model-facing wording, kept verbatim.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
/// Sends a message to an agent.
struct SendMessageInput {
    /// Relative or canonical task name to message (from spawn_agent).
    target: String,
    /// Message text to queue on the target agent.
    message: String,
}

// Field docs are Codex's model-facing wording, kept verbatim.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
/// Sends a follow-up task to an agent.
struct FollowupTaskInput {
    /// Agent id or canonical task name to send a follow-up task to (from spawn_agent).
    target: String,
    /// Message text to send to the target agent.
    message: String,
}

// Field docs are Codex's model-facing wording, kept verbatim.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
/// Interrupts an agent.
struct InterruptAgentInput {
    /// Agent id or canonical task name to interrupt (from spawn_agent).
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
/// Waits for mailbox activity.
struct WaitAgentInput {
    /// Timeout in milliseconds.
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
/// Lists agents.
struct ListAgentsInput {
    /// Task-path prefix filter without a trailing slash. Omit to list all live agents.
    path_prefix: Option<String>,
}

/// One collaboration tool.
#[derive(Debug)]
pub struct CollaborationTool {
    kind: CollaborationKind,
    origin: ToolOrigin,
    func_schema: FuncSchema,
    schema: ToolSchema,
    options: ToolOptions,
    wait: WaitAgentTimeoutOptions,
}

impl CollaborationTool {
    /// Creates `kind`; `wait` only matters to `wait_agent`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(kind: CollaborationKind, wait: WaitAgentTimeoutOptions) -> Result<Self> {
        let name = kind.name();
        let (func_schema, description) = match kind {
            CollaborationKind::SpawnAgent => (
                FuncSchema::for_input::<SpawnAgentInput>(name)?,
                SPAWN_AGENT_DESCRIPTION.to_owned(),
            ),
            CollaborationKind::SendMessage => (
                FuncSchema::for_input::<SendMessageInput>(name)?,
                SEND_MESSAGE_DESCRIPTION.to_owned(),
            ),
            CollaborationKind::FollowupTask => (
                FuncSchema::for_input::<FollowupTaskInput>(name)?,
                FOLLOWUP_TASK_DESCRIPTION.to_owned(),
            ),
            CollaborationKind::InterruptAgent => (
                FuncSchema::for_input::<InterruptAgentInput>(name)?,
                INTERRUPT_AGENT_DESCRIPTION.to_owned(),
            ),
            CollaborationKind::WaitAgent => (
                FuncSchema::for_input::<WaitAgentInput>(name)?,
                format!(
                    "{WAIT_AGENT_DESCRIPTION} Timeout in milliseconds defaults to {}, min {}, max {}.",
                    wait.default, wait.min, wait.max
                ),
            ),
            CollaborationKind::ListAgents => (
                FuncSchema::for_input::<ListAgentsInput>(name)?,
                LIST_AGENTS_DESCRIPTION.to_owned(),
            ),
        };
        let schema = func_schema
            .tool_schema()
            .clone()
            .with_description(description);
        // Observing the tree changes nothing; starting or steering an agent sets work in motion
        // that can run any of that agent's tools.
        let scope = match kind {
            CollaborationKind::WaitAgent | CollaborationKind::ListAgents => PermissionScope::Read,
            _ => PermissionScope::Execute,
        };
        Ok(Self {
            kind,
            origin: ToolOrigin::new(name)?,
            func_schema,
            schema,
            // Dynamic so a run whose agent is at the tree's depth limit is not offered the tools.
            options: ToolOptions::new()
                .with_availability(ToolAvailability::Dynamic)
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_permission_scope(scope),
            wait,
        })
    }

    /// Which tool this is.
    #[must_use]
    pub const fn kind(&self) -> CollaborationKind {
        self.kind
    }

    fn input<T: ToolInput + DeserializeOwned>(&self, context: &mut ToolContext<'_>) -> Result<T> {
        if let Some(input) = context.take_decoded_input::<T>()? {
            return Ok(input);
        }
        serde_json::from_value(context.arguments().clone())
            .map_err(|error| self.refuse(format!("failed to parse function arguments: {error}")))
    }

    fn refuse(&self, message: impl Into<String>) -> Error {
        let failure = CollaborationFailure(message.into());
        Error::tool(
            ToolErrorKind::InvalidInput,
            self.kind.name(),
            failure.0.clone(),
        )
        .with_source(failure)
    }

    /// Codex's `collab_spawn_error`, used by `spawn_agent` and `list_agents`.
    fn spawn_error(&self, error: &AgentControlError) -> Error {
        match error {
            AgentControlError::Unsupported(message) => self.refuse(message.clone()),
            AgentControlError::Unavailable => self.refuse(error.to_string()),
            _ => self.refuse(format!("collab spawn failed: {error}")),
        }
    }

    /// Codex's `collab_v2_agent_error`, used by the tools that act on an existing agent.
    fn agent_error(&self, error: &AgentControlError) -> Error {
        match error {
            AgentControlError::Unsupported(message) => self.refuse(message.clone()),
            AgentControlError::Unavailable => self.refuse(error.to_string()),
            _ => self.refuse(format!("collab tool failed: {error}")),
        }
    }

    fn message_content(&self, message: String) -> Result<String> {
        if message.trim().is_empty() {
            return Err(self.refuse("Empty message can't be sent to an agent"));
        }
        Ok(message)
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        port: &dyn AgentControlPort,
    ) -> Result<ToolOutput> {
        match self.kind {
            CollaborationKind::SpawnAgent => {
                let input: SpawnAgentInput = self.input(context)?;
                let fork_mode = input.fork_mode().map_err(|message| self.refuse(message))?;
                let message = self.message_content(input.message)?;
                let mut request = SpawnAgentRequest::new(input.task_name, message);
                if let Some(fork_mode) = fork_mode {
                    request = request.with_fork_mode(fork_mode);
                }
                if let Some(agent_type) = input
                    .agent_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|agent_type| !agent_type.is_empty())
                {
                    request = request.with_agent_type(AgentId::new(agent_type));
                }
                let agent = port
                    .spawn(request)
                    .await
                    .map_err(|error| self.spawn_error(&error))?;
                json_output(&json!({ "task_name": agent.agent_path() }))
            }
            CollaborationKind::SendMessage | CollaborationKind::FollowupTask => {
                let (target, message, mode) = if self.kind == CollaborationKind::SendMessage {
                    let input: SendMessageInput = self.input(context)?;
                    (input.target, input.message, MessageDeliveryMode::QueueOnly)
                } else {
                    let input: FollowupTaskInput = self.input(context)?;
                    (
                        input.target,
                        input.message,
                        MessageDeliveryMode::TriggerTurn,
                    )
                };
                let message = self.message_content(message)?;
                port.send(&target, message, mode)
                    .await
                    .map_err(|error| self.agent_error(&error))?;
                Ok(ToolOutput::text(String::new()))
            }
            CollaborationKind::InterruptAgent => {
                let input: InterruptAgentInput = self.input(context)?;
                let previous_status = port
                    .interrupt(&input.target)
                    .await
                    .map_err(|error| self.agent_error(&error))?;
                json_output(&InterruptAgentResult { previous_status })
            }
            CollaborationKind::WaitAgent => {
                let input: WaitAgentInput = self.input(context)?;
                let timeout_ms = match input.timeout_ms {
                    Some(ms) if ms > self.wait.max => {
                        return Err(
                            self.refuse(format!("timeout_ms must be at most {}", self.wait.max))
                        );
                    }
                    Some(ms) => ms.max(self.wait.min),
                    None => self.wait.default,
                };
                let timeout = Duration::from_millis(u64::try_from(timeout_ms).unwrap_or(0));
                let outcome = port.wait_for_mailbox(timeout).await;
                json_output(&WaitAgentResult::from_outcome(
                    outcome,
                    input.timeout_ms,
                    timeout_ms,
                ))
            }
            CollaborationKind::ListAgents => {
                let input: ListAgentsInput = self.input(context)?;
                let agents = port
                    .list(input.path_prefix.as_deref())
                    .await
                    .map_err(|error| self.spawn_error(&error))?;
                json_output(&ListAgentsResult {
                    agents: agents.into_iter().map(ListedAgent::from).collect(),
                })
            }
        }
    }
}

#[derive(Serialize)]
struct InterruptAgentResult {
    previous_status: AgentStatus,
}

#[derive(Serialize)]
struct WaitAgentResult {
    message: String,
    timed_out: bool,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let timed_out = outcome == WaitOutcome::TimedOut;
        let message = if timed_out {
            "Wait timed out."
        } else {
            "Wait completed."
        };
        let message = match requested_timeout_ms {
            Some(requested) if requested < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_owned(),
        };
        Self { message, timed_out }
    }
}

#[derive(Serialize)]
struct ListedAgent {
    agent_name: String,
    agent_status: AgentStatus,
}

impl From<LiveAgent> for ListedAgent {
    fn from(agent: LiveAgent) -> Self {
        Self {
            agent_name: agent.agent_path().to_string(),
            agent_status: agent.status().clone(),
        }
    }
}

#[derive(Serialize)]
struct ListAgentsResult {
    agents: Vec<ListedAgent>,
}

fn json_output(value: &impl Serialize) -> Result<ToolOutput> {
    serde_json::to_string(value)
        .map(ToolOutput::text)
        .map_err(|error| Error::caller(format!("failed to serialize tool output: {error}")))
}

/// A refusal whose text is what the model is shown, as Codex's `RespondToModel`.
#[derive(Debug)]
struct CollaborationFailure(String);

impl fmt::Display for CollaborationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CollaborationFailure {}

#[async_trait]
impl Tool for CollaborationTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema
            .decode_value_diagnostic(arguments.clone())
            .map(Some)
            .map_err(|error| self.refuse(format!("failed to parse function arguments: {error}")))
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let Some(port) = context.services().agent_control().cloned() else {
            return Err(self.refuse(NOT_ENABLED));
        };
        self.run(&mut context, port.as_ref()).await
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    /// Hidden from an agent that may spawn nothing more, as Codex's first multi-agent version
    /// leaves the collaboration tools out once the next spawn would exceed the depth limit. A run
    /// with no control plane still sees them, and is told collaboration is not enabled.
    async fn is_enabled(&self, _context: &RunContext) -> Result<bool> {
        Ok(true)
    }

    async fn is_enabled_with_services(
        &self,
        _context: &RunContext,
        services: &ToolServices,
    ) -> Result<bool> {
        Ok(services
            .agent_control()
            .is_none_or(|port| !port.spawn_depth_exceeded()))
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Control signals stop or close out the run rather than becoming an observation.
        if error.is_cancelled() || matches!(error, Error::Budget { .. } | Error::Guardrail { .. }) {
            return Ok(None);
        }
        Ok(std::error::Error::source(error)
            .and_then(<dyn std::error::Error + 'static>::downcast_ref::<CollaborationFailure>)
            .map(|failure| ToolOutput::text(failure.0.clone())))
    }
}
