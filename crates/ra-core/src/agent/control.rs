//! Values and the port of the multi-agent control plane, ported from Codex's `MultiAgentV2`.
//!
//! An agent tree has one root and any number of spawned agents, each named by an [`AgentPath`]
//! under it. Agents talk through mailboxes: a message is queued on the recipient and reaches its
//! model at the next model-call boundary, or starts a new run when the recipient is idle and the
//! message asks for one ([`MessageDeliveryMode::TriggerTurn`]). A spawned agent reports its final
//! answer to its parent's mailbox when its run ends.
//!
//! This is what lets a parent stop waiting on a stalled child: `wait_agent` returns when the
//! parent's mailbox receives something *or* when its timeout elapses, after which the parent can
//! redirect the child with a follow-up, interrupt its current run, or carry on without it.
//!
//! Only values and the [`AgentControlPort`] trait live here; the live tree, the mailboxes and the
//! background runs belong to the runtime, and the model-facing tools to the tools crate. A tool
//! reaches the port through [`ToolServices`](crate::tool::ToolServices), already bound to the agent
//! the tool is running for, so every operation is attributed to that caller.

use std::{fmt, ops::Deref, str::FromStr, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    item::{AgentId, Message, MessageRole, ModelInputItem, OutputPhase},
    session::SessionId,
};

/// Canonical path of an agent in its tree: `/root` for the root, `/root/<name>/...` below it.
///
/// Ported from Codex's `AgentPath`. Names use lowercase ASCII letters, digits and underscores, and
/// `root`, `.` and `..` are reserved. A reference that does not start with `/` is resolved relative
/// to the agent using it, so a parent can name its own child by the bare task name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentPath(String);

impl AgentPath {
    /// The root agent's path.
    pub const ROOT: &'static str = "/root";
    const ROOT_SEGMENT: &'static str = "root";

    /// The root agent.
    #[must_use]
    pub fn root() -> Self {
        Self(Self::ROOT.to_owned())
    }

    /// Validates an absolute path.
    ///
    /// # Errors
    ///
    /// Returns the reason the path is not a canonical agent path.
    pub fn from_string(path: String) -> Result<Self, String> {
        validate_absolute_path(&path)?;
        Ok(Self(path))
    }

    /// The path as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the root agent.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == Self::ROOT
    }

    /// The last segment, `root` for the root.
    #[must_use]
    pub fn name(&self) -> &str {
        if self.is_root() {
            return Self::ROOT_SEGMENT;
        }
        self.0
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(Self::ROOT_SEGMENT)
    }

    /// The parent agent's path; `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        self.0
            .rsplit_once('/')
            .and_then(|(parent, _)| Self::try_from(parent).ok())
    }

    /// The path of a child named `agent_name`.
    ///
    /// # Errors
    ///
    /// Returns the reason `agent_name` is not a valid agent name.
    pub fn join(&self, agent_name: &str) -> Result<Self, String> {
        validate_agent_name(agent_name)?;
        Self::from_string(format!("{self}/{agent_name}"))
    }

    /// Resolves `reference` as seen from this agent: absolute paths as they are, anything else
    /// below this agent.
    ///
    /// # Errors
    ///
    /// Returns the reason `reference` is not a valid path or relative reference.
    pub fn resolve(&self, reference: &str) -> Result<Self, String> {
        if reference.is_empty() {
            return Err("agent path must not be empty".to_owned());
        }
        if reference == Self::ROOT {
            return Ok(Self::root());
        }
        if reference.starts_with('/') {
            return Self::try_from(reference);
        }
        validate_relative_reference(reference)?;
        Self::from_string(format!("{self}/{reference}"))
    }

    /// Whether this path is `prefix` or lies below it.
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self == prefix
            || self
                .0
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

impl TryFrom<String> for AgentPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_string(value)
    }
}

impl TryFrom<&str> for AgentPath {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_string(value.to_owned())
    }
}

impl From<AgentPath> for String {
    fn from(value: AgentPath) -> Self {
        value.0
    }
}

impl FromStr for AgentPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for AgentPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for AgentPath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn validate_agent_name(agent_name: &str) -> Result<(), String> {
    if agent_name.is_empty() {
        return Err("agent_name must not be empty".to_owned());
    }
    if agent_name == AgentPath::ROOT_SEGMENT {
        return Err("agent_name `root` is reserved".to_owned());
    }
    if agent_name == "." || agent_name == ".." {
        return Err(format!("agent_name `{agent_name}` is reserved"));
    }
    if agent_name.contains('/') {
        return Err("agent_name must not contain `/`".to_owned());
    }
    if !agent_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(
            "agent_name must use only lowercase letters, digits, and underscores".to_owned(),
        );
    }
    Ok(())
}

fn validate_absolute_path(path: &str) -> Result<(), String> {
    let Some(stripped) = path.strip_prefix('/') else {
        return Err("absolute agent paths must start with `/root`".to_owned());
    };
    let mut segments = stripped.split('/');
    if segments.next() != Some(AgentPath::ROOT_SEGMENT) {
        return Err("absolute agent paths must start with `/root`".to_owned());
    }
    if stripped.ends_with('/') {
        return Err("absolute agent path must not end with `/`".to_owned());
    }
    segments.try_for_each(validate_agent_name)
}

fn validate_relative_reference(reference: &str) -> Result<(), String> {
    if reference.ends_with('/') {
        return Err("relative agent path must not end with `/`".to_owned());
    }
    reference.split('/').try_for_each(validate_agent_name)
}

/// Lifecycle status of an agent in the tree, as Codex reports it to the model.
///
/// The wire form is Codex's: unit states are bare strings (`"running"`), the two carrying a payload
/// are single-key objects (`{"completed": "..."}`, `{"errored": "..."}`).
///
/// This is the control plane's own status and is distinct from
/// [`event::agent::AgentStatus`](crate::event::agent::AgentStatus), an open display label for host
/// events that carries no payload.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Spawned, and its first run has not started yet.
    #[default]
    PendingInit,
    /// A run is in progress.
    Running,
    /// The current run was stopped; the agent can still receive messages and follow-up tasks.
    Interrupted,
    /// The last run finished, with its final answer when it delivered one.
    Completed(Option<String>),
    /// The last run failed.
    Errored(String),
    /// The agent was shut down and accepts nothing more.
    Shutdown,
    /// No such agent.
    NotFound,
}

impl AgentStatus {
    /// Whether the agent has no run in progress and will not start one by itself.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Completed(_) | Self::Errored(_)
        )
    }
}

/// Whether a message to an idle agent starts a new run on it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDeliveryMode {
    /// Deliver to the mailbox without starting an idle agent.
    QueueOnly,
    /// Deliver to the running agent, or start a run if the agent is idle.
    TriggerTurn,
}

/// One message between agents, as queued on the recipient.
///
/// `content` is the model-facing text, already rendered in Codex's envelope
/// (`Message Type`/`Task name`/`Sender`/`Payload`), so the recipient's model reads who sent it and
/// why rather than a bare string it might take for user input.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterAgentCommunication {
    author: AgentPath,
    recipient: AgentPath,
    content: String,
    trigger_turn: bool,
}

impl InterAgentCommunication {
    /// Creates a message whose content is already rendered.
    #[must_use]
    pub fn new(
        author: AgentPath,
        recipient: AgentPath,
        content: impl Into<String>,
        trigger_turn: bool,
    ) -> Self {
        Self {
            author,
            recipient,
            content: content.into(),
            trigger_turn,
        }
    }

    /// A message sent by an agent, rendered as Codex renders plaintext agent messages: `NEW_TASK`
    /// for a follow-up task or a spawn, `MESSAGE` otherwise.
    #[must_use]
    pub fn from_agent(
        author: AgentPath,
        recipient: AgentPath,
        payload: &str,
        mode: MessageDeliveryMode,
    ) -> Self {
        let message_type = match mode {
            MessageDeliveryMode::QueueOnly => "MESSAGE",
            MessageDeliveryMode::TriggerTurn => "NEW_TASK",
        };
        let content = render_envelope(message_type, &recipient, &author, payload);
        Self::new(
            author,
            recipient,
            content,
            mode == MessageDeliveryMode::TriggerTurn,
        )
    }

    /// Sender.
    #[must_use]
    pub const fn author(&self) -> &AgentPath {
        &self.author
    }

    /// Recipient.
    #[must_use]
    pub const fn recipient(&self) -> &AgentPath {
        &self.recipient
    }

    /// Rendered message text.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Whether the message starts a run on an idle recipient.
    #[must_use]
    pub const fn trigger_turn(&self) -> bool {
        self.trigger_turn
    }

    /// The model input the recipient's run receives for this message.
    ///
    /// A user-role message. Codex records agent messages as assistant-side items in its own wire
    /// format; in a provider-neutral history an assistant message placed last would be read by
    /// some providers as a prefill of the next answer, so the message takes the role a run's other
    /// injected text (hook continuations) already uses, and the envelope says who wrote it.
    #[must_use]
    pub fn to_message(&self) -> Message {
        Message::text(MessageRole::User, self.content.clone())
    }
}

/// Renders Codex's inter-agent envelope. `task_name` is the recipient's path, as in Codex.
fn render_envelope(
    message_type: &str,
    task_name: &AgentPath,
    sender: &AgentPath,
    payload: &str,
) -> String {
    format!(
        "Message Type: {message_type}\nTask name: {task_name}\nSender: {sender}\nPayload:\n{payload}"
    )
}

/// Codex's budget for an error in a completion message: 1,000 tokens less 100 for the envelope.
const ERROR_MAX_TOKENS: usize = 900;
/// Bytes per token in the approximation Codex's truncation also uses.
const APPROX_BYTES_PER_TOKEN: usize = 4;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task.";

/// The `FINAL_ANSWER` message a child's terminal status sends its parent, or `None` for a status
/// that reports nothing (an interrupted or still-running agent).
///
/// Ported from Codex's `format_inter_agent_completion_message`: the final answer verbatim, an
/// error truncated with a hint at what to do next, or a one-line notice.
#[must_use]
pub fn completion_message(
    parent: &AgentPath,
    child: &AgentPath,
    status: &AgentStatus,
) -> Option<InterAgentCommunication> {
    let payload = match status {
        AgentStatus::Completed(Some(message)) => message.clone(),
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::Errored(error) => {
            let error = truncate_to_bytes(error, ERROR_MAX_TOKENS * APPROX_BYTES_PER_TOKEN);
            format!("Agent errored: {error}\n\n{ERROR_NEXT_ACTION}")
        }
        AgentStatus::Shutdown => "Agent shut down.".to_owned(),
        AgentStatus::NotFound => "Agent was not found.".to_owned(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    let content = render_envelope("FINAL_ANSWER", parent, child, &payload);
    Some(InterAgentCommunication::new(
        child.clone(),
        parent.clone(),
        content,
        false,
    ))
}

fn truncate_to_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Identity and status of one agent in the tree.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgent {
    agent_path: AgentPath,
    agent_id: Option<AgentId>,
    status: AgentStatus,
    session_id: Option<SessionId>,
}

impl LiveAgent {
    /// Creates a snapshot.
    #[must_use]
    pub const fn new(
        agent_path: AgentPath,
        agent_id: Option<AgentId>,
        status: AgentStatus,
    ) -> Self {
        Self {
            agent_path,
            agent_id,
            status,
            session_id: None,
        }
    }

    /// Sets the session of the agent's own rollout: the id of Codex's thread.
    #[must_use]
    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Canonical path.
    #[must_use]
    pub const fn agent_path(&self) -> &AgentPath {
        &self.agent_path
    }

    /// The declaration the agent runs; unknown for a root that has not run yet.
    #[must_use]
    pub const fn agent_id(&self) -> Option<&AgentId> {
        self.agent_id.as_ref()
    }

    /// Current status.
    #[must_use]
    pub const fn status(&self) -> &AgentStatus {
        &self.status
    }

    /// The session of the agent's own rollout, which names this life of the agent: a path freed by
    /// a close can be spawned again, with a session of its own. Unknown for a root whose tree was
    /// not told its session.
    #[must_use]
    pub const fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }
}

/// What to spawn.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnAgentRequest {
    task_name: String,
    message: String,
    agent_type: Option<AgentId>,
    fork_mode: Option<SpawnAgentForkMode>,
}

impl SpawnAgentRequest {
    /// Spawns the caller's own declaration under `task_name` with `message` as its task.
    #[must_use]
    pub fn new(task_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            task_name: task_name.into(),
            message: message.into(),
            agent_type: None,
            fork_mode: None,
        }
    }

    /// Starts the new agent from the caller's history, projected by `fork_mode`. Without one the
    /// agent starts from its task message alone.
    #[must_use]
    pub const fn with_fork_mode(mut self, fork_mode: SpawnAgentForkMode) -> Self {
        self.fork_mode = Some(fork_mode);
        self
    }

    /// How much of the caller's history the new agent starts from; `None` for none of it.
    #[must_use]
    pub const fn fork_mode(&self) -> Option<&SpawnAgentForkMode> {
        self.fork_mode.as_ref()
    }

    /// Runs the registered declaration `agent_type` instead of the caller's.
    #[must_use]
    pub fn with_agent_type(mut self, agent_type: AgentId) -> Self {
        self.agent_type = Some(agent_type);
        self
    }

    /// Name of the new agent below the caller.
    #[must_use]
    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    /// Its initial task.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Which registered agent declaration to run; the caller's own when absent.
    #[must_use]
    pub const fn agent_type(&self) -> Option<&AgentId> {
        self.agent_type.as_ref()
    }
}

/// How much of the spawning agent's history a spawned agent starts from, Codex's
/// `SpawnAgentForkMode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnAgentForkMode {
    /// The whole conversation so far.
    FullHistory,
    /// The conversation from the start of the last `n` turns.
    LastNTurns(usize),
}

/// The part of `history` a spawned agent inherits under `mode`.
///
/// Ported from Codex's fork: the history is first cut to the last `n` turns, where a turn starts
/// at a user message or at an agent message that started a run (`NEW_TASK`), then only the
/// conversation is kept — user and system messages and final answers. Tool calls and their
/// outputs, reasoning, commentary and every inter-agent message are left behind; the child gets
/// the context of the task, not the parent's working state or its mail. Compaction summaries stay,
/// since they are what an earlier stretch of conversation was reduced to.
///
/// Codex tells inter-agent messages apart by their own item type. A provider-neutral history
/// records them as user messages, so here the envelope they carry is what marks them.
#[must_use]
pub fn fork_history(history: &[ModelInputItem], mode: SpawnAgentForkMode) -> Vec<ModelInputItem> {
    let start = match mode {
        SpawnAgentForkMode::FullHistory => 0,
        SpawnAgentForkMode::LastNTurns(0) => return Vec::new(),
        SpawnAgentForkMode::LastNTurns(n) => {
            let boundaries: Vec<usize> = history
                .iter()
                .enumerate()
                .filter(|(_, item)| is_fork_turn_boundary(item))
                .map(|(index, _)| index)
                .collect();
            match boundaries
                .len()
                .checked_sub(n)
                .map(|first| boundaries[first])
                .or_else(|| boundaries.first().copied())
            {
                Some(start) => start,
                None => return Vec::new(),
            }
        }
    };
    history[start..]
        .iter()
        .filter(|item| keep_forked_item(item))
        .cloned()
        .collect()
}

fn is_fork_turn_boundary(item: &ModelInputItem) -> bool {
    let ModelInputItem::Message(message) = item else {
        return false;
    };
    if message.role() != MessageRole::User {
        return false;
    }
    match InterAgentMessageKind::of(message) {
        None => true,
        Some(kind) => kind == InterAgentMessageKind::NewTask,
    }
}

fn keep_forked_item(item: &ModelInputItem) -> bool {
    match item {
        ModelInputItem::Message(message) => match message.role() {
            MessageRole::System => true,
            MessageRole::User => InterAgentMessageKind::of(message).is_none(),
            MessageRole::Assistant => message.phase() == Some(OutputPhase::Final),
        },
        ModelInputItem::Compaction(_) => true,
        _ => false,
    }
}

/// Which inter-agent envelope a message carries.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterAgentMessageKind {
    /// A message that does not start a run (`MESSAGE`).
    Message,
    /// A task that starts a run on an idle recipient (`NEW_TASK`).
    NewTask,
    /// A child's report of how its run ended (`FINAL_ANSWER`).
    FinalAnswer,
}

impl InterAgentMessageKind {
    /// The envelope `message` carries, if it is a delivered inter-agent message.
    #[must_use]
    pub fn of(message: &Message) -> Option<Self> {
        if message.role() != MessageRole::User {
            return None;
        }
        let text = message.text_content();
        let (header, rest) = text.split_once('\n')?;
        let kind = match header.strip_prefix("Message Type: ")? {
            "MESSAGE" => Self::Message,
            "NEW_TASK" => Self::NewTask,
            "FINAL_ANSWER" => Self::FinalAnswer,
            _ => return None,
        };
        let rest = rest.strip_prefix("Task name: ")?;
        let (_, rest) = rest.split_once("\nSender: ")?;
        rest.contains("\nPayload:\n").then_some(kind)
    }
}

/// Why a wait returned.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The caller's mailbox has something for it.
    MailboxActivity,
    /// Nothing arrived before the timeout.
    TimedOut,
}

/// Why a control-plane operation was refused or failed.
///
/// The messages are the ones Codex hands back to the model, so a model reading a refusal learns
/// what to change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentControlError {
    /// The operation is not allowed as asked; the message says why.
    Unsupported(String),
    /// The request itself is malformed.
    InvalidRequest(String),
    /// No capacity is left to start another agent run.
    AgentLimitReached {
        /// Ceiling on concurrently running spawned agents.
        max_threads: usize,
    },
    /// The control plane is gone, for instance after shutdown.
    Unavailable,
}

impl fmt::Display for AgentControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message) | Self::InvalidRequest(message) => f.write_str(message),
            Self::AgentLimitReached { .. } => f.write_str("agent thread limit reached"),
            Self::Unavailable => f.write_str("collab manager unavailable"),
        }
    }
}

impl std::error::Error for AgentControlError {}

/// The control plane as one agent sees it.
///
/// An implementation is bound to its caller: every operation acts as, and resolves relative
/// references against, [`Self::caller`]. That is how a tool, which only holds this port, cannot
/// act on behalf of another agent.
#[async_trait]
pub trait AgentControlPort: Send + Sync {
    /// The agent this port acts for.
    fn caller(&self) -> &AgentPath;

    /// Spawns an agent below the caller and starts its first run on `request.message`.
    async fn spawn(&self, request: SpawnAgentRequest) -> Result<LiveAgent, AgentControlError>;

    /// Queues `message` on `target`. With [`MessageDeliveryMode::TriggerTurn`] an idle target
    /// starts a run on it; such a follow-up cannot target the root. Returns the target's path.
    async fn send(
        &self,
        target: &str,
        message: String,
        mode: MessageDeliveryMode,
    ) -> Result<AgentPath, AgentControlError>;

    /// Stops `target`'s current run, if any, and returns the status it had before. The agent keeps
    /// its history and can still be given messages and follow-up tasks. The root and the caller
    /// itself cannot be interrupted.
    async fn interrupt(&self, target: &str) -> Result<AgentStatus, AgentControlError>;

    /// Agents in the tree, the root first, optionally only those at or below `path_prefix`.
    async fn list(&self, path_prefix: Option<&str>) -> Result<Vec<LiveAgent>, AgentControlError>;

    /// Waits until the caller's mailbox has something, or `timeout` elapses. Returns at once when
    /// mail is already pending. The mail itself reaches the caller's model at its next model call.
    async fn wait_for_mailbox(&self, timeout: Duration) -> WaitOutcome;

    /// Whether an agent the caller spawned would be deeper than the tree allows.
    ///
    /// Codex's `exceeds_thread_spawn_depth_limit` for the caller's next spawn. A caller for which
    /// this holds is refused every spawn, and Codex does not offer it the collaboration tools at
    /// all. The default answers `false`: a tree with no depth limit.
    fn spawn_depth_exceeded(&self) -> bool {
        false
    }

    /// The session of the live agent at `path`, if the tree has one there and knows it.
    ///
    /// What Codex's collaboration handlers record as the agent's thread id once an operation on it
    /// has succeeded. The default knows none.
    fn agent_session_id(&self, path: &AgentPath) -> Option<SessionId> {
        let _ = path;
        None
    }
}
