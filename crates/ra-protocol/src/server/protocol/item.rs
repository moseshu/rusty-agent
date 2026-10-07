//! What the agent asks the client while an item of a turn runs: approving a command or a file
//! change, calling a dynamic tool, and asking the user questions — Codex's v2 item payloads
//! (`protocol/v2/item.rs`).
//!
//! Every request names the thread, the turn and the item it is for. Paths are carried as the
//! strings the server wrote, as Codex's `LegacyAppPathString` carries them.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::permissions::{
    AdditionalPermissionProfile, ExecPolicyAmendment, NetworkApprovalContext,
    NetworkPolicyAmendment,
};

/// How the client answers a command approval: Codex's `CommandExecutionApprovalDecision`.
///
/// The two amendments are written with their member names in `snake_case`, as Codex writes them.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionApprovalDecision {
    /// Run the command.
    Accept,
    /// Run the command, and run matching commands this session without asking.
    AcceptForSession,
    /// Run the command, and allow commands with the proposed prefix from now on.
    #[non_exhaustive]
    AcceptWithExecpolicyAmendment {
        /// The prefix to allow.
        execpolicy_amendment: ExecPolicyAmendment,
    },
    /// Apply a persistent rule for the host the command reaches.
    #[non_exhaustive]
    ApplyNetworkPolicyAmendment {
        /// The rule to apply.
        network_policy_amendment: NetworkPolicyAmendment,
    },
    /// Do not run the command; the turn goes on.
    Decline,
    /// Do not run the command, and interrupt the turn.
    Cancel,
}

impl CommandExecutionApprovalDecision {
    /// Run the command, and allow commands with `execpolicy_amendment`'s prefix from now on.
    #[must_use]
    pub const fn accept_with_execpolicy_amendment(
        execpolicy_amendment: ExecPolicyAmendment,
    ) -> Self {
        Self::AcceptWithExecpolicyAmendment {
            execpolicy_amendment,
        }
    }

    /// Apply `network_policy_amendment` to the host the command reaches.
    #[must_use]
    pub const fn apply_network_policy_amendment(
        network_policy_amendment: NetworkPolicyAmendment,
    ) -> Self {
        Self::ApplyNetworkPolicyAmendment {
            network_policy_amendment,
        }
    }
}

/// How the client answers a file change approval: Codex's `FileChangeApprovalDecision`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeApprovalDecision {
    /// Apply the changes.
    Accept,
    /// Apply the changes, and changes to the same files this session without asking.
    AcceptForSession,
    /// Do not apply the changes; the turn goes on.
    Decline,
    /// Do not apply the changes, and interrupt the turn.
    Cancel,
}

/// What a command approval is about: Codex's `CommandExecutionApprovalKind`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionApprovalKind {
    /// Running a command.
    #[default]
    Command,
    /// Writing to the input of a terminal already running.
    WriteStdin,
}

/// What a command does, parsed for display: Codex's v2 `CommandAction`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CommandAction {
    /// Reads a file.
    #[non_exhaustive]
    Read {
        /// The part of the command that reads it.
        command: String,
        /// The file's name.
        name: String,
        /// The file's path.
        path: String,
    },
    /// Lists files.
    #[non_exhaustive]
    ListFiles {
        /// The part of the command that lists them.
        command: String,
        /// Where, if the command says.
        path: Option<String>,
    },
    /// Searches files.
    #[non_exhaustive]
    Search {
        /// The part of the command that searches.
        command: String,
        /// What for, if the command says.
        query: Option<String>,
        /// Where, if the command says.
        path: Option<String>,
    },
    /// Anything else.
    #[non_exhaustive]
    Unknown {
        /// The command.
        command: String,
    },
}

impl CommandAction {
    /// `command` reads the file `name` at `path`.
    #[must_use]
    pub fn read(
        command: impl Into<String>,
        name: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self::Read {
            command: command.into(),
            name: name.into(),
            path: path.into(),
        }
    }

    /// `command` lists the files at `path`.
    #[must_use]
    pub fn list_files(command: impl Into<String>, path: Option<String>) -> Self {
        Self::ListFiles {
            command: command.into(),
            path,
        }
    }

    /// `command` searches for `query` at `path`.
    #[must_use]
    pub fn search(command: impl Into<String>, query: Option<String>, path: Option<String>) -> Self {
        Self::Search {
            command: command.into(),
            query,
            path,
        }
    }

    /// `command` does something not parsed.
    #[must_use]
    pub fn unknown(command: impl Into<String>) -> Self {
        Self::Unknown {
            command: command.into(),
        }
    }
}

/// The parameters of `item/commandExecution/requestApproval`: Codex's
/// `CommandExecutionRequestApprovalParams`.
///
/// `additionalPermissions` and `availableDecisions` are experimental. Codex strips the first from
/// the request it sends a connection that did not opt into the experimental API, and sends the
/// second regardless; so does this server.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalParams {
    #[serde(default)]
    kind: CommandExecutionApprovalKind,
    thread_id: String,
    turn_id: String,
    item_id: String,
    started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_id: Option<String>,
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network_approval_context: Option<NetworkApprovalContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_actions: Option<Vec<CommandAction>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_permissions: Option<AdditionalPermissionProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposed_network_policy_amendments: Option<Vec<NetworkPolicyAmendment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    available_decisions: Option<Vec<CommandExecutionApprovalDecision>>,
}

impl CommandExecutionRequestApprovalParams {
    /// An approval of a command for the item `item_id` of `turn_id` in `thread_id`, asked at
    /// `started_at_ms` milliseconds after the Unix epoch.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        started_at_ms: i64,
    ) -> Self {
        Self {
            kind: CommandExecutionApprovalKind::default(),
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            started_at_ms,
            approval_id: None,
            environment_id: None,
            reason: None,
            network_approval_context: None,
            command: None,
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        }
    }

    /// Sets what the approval is about.
    #[must_use]
    pub const fn with_kind(mut self, kind: CommandExecutionApprovalKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets the id of this approval, for when one item has several.
    #[must_use]
    pub fn with_approval_id(mut self, approval_id: impl Into<String>) -> Self {
        self.approval_id = Some(approval_id.into());
        self
    }

    /// Sets the environment the command will run in.
    #[must_use]
    pub fn with_environment_id(mut self, environment_id: impl Into<String>) -> Self {
        self.environment_id = Some(environment_id.into());
        self
    }

    /// Sets why the approval is asked.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Sets the network access the approval is about.
    #[must_use]
    pub fn with_network_approval_context(mut self, context: NetworkApprovalContext) -> Self {
        self.network_approval_context = Some(context);
        self
    }

    /// Sets the command to run.
    #[must_use]
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    /// Sets the directory the command runs in.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets what the command does, parsed for display.
    #[must_use]
    pub fn with_command_actions(
        mut self,
        actions: impl IntoIterator<Item = CommandAction>,
    ) -> Self {
        self.command_actions = Some(actions.into_iter().collect());
        self
    }

    /// Sets the permissions the command asks for beyond the sandbox's. Experimental.
    #[must_use]
    pub fn with_additional_permissions(mut self, permissions: AdditionalPermissionProfile) -> Self {
        self.additional_permissions = Some(permissions);
        self
    }

    /// Proposes a prefix to allow from now on.
    #[must_use]
    pub fn with_proposed_execpolicy_amendment(mut self, amendment: ExecPolicyAmendment) -> Self {
        self.proposed_execpolicy_amendment = Some(amendment);
        self
    }

    /// Proposes rules for the hosts the command reaches.
    #[must_use]
    pub fn with_proposed_network_policy_amendments(
        mut self,
        amendments: impl IntoIterator<Item = NetworkPolicyAmendment>,
    ) -> Self {
        self.proposed_network_policy_amendments = Some(amendments.into_iter().collect());
        self
    }

    /// Sets the decisions the client may offer, in order. Experimental.
    #[must_use]
    pub fn with_available_decisions(
        mut self,
        decisions: impl IntoIterator<Item = CommandExecutionApprovalDecision>,
    ) -> Self {
        self.available_decisions = Some(decisions.into_iter().collect());
        self
    }

    /// What the approval is about.
    #[must_use]
    pub const fn kind(&self) -> CommandExecutionApprovalKind {
        self.kind
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// When the approval was asked, in milliseconds after the Unix epoch.
    #[must_use]
    pub const fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// The id of this approval, when one item has several.
    #[must_use]
    pub fn approval_id(&self) -> Option<&str> {
        self.approval_id.as_deref()
    }

    /// The environment the command will run in.
    #[must_use]
    pub fn environment_id(&self) -> Option<&str> {
        self.environment_id.as_deref()
    }

    /// Why the approval is asked.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The network access the approval is about.
    #[must_use]
    pub const fn network_approval_context(&self) -> Option<&NetworkApprovalContext> {
        self.network_approval_context.as_ref()
    }

    /// The command to run.
    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// The directory the command runs in.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// What the command does, parsed for display.
    #[must_use]
    pub fn command_actions(&self) -> Option<&[CommandAction]> {
        self.command_actions.as_deref()
    }

    /// The permissions the command asks for beyond the sandbox's.
    #[must_use]
    pub const fn additional_permissions(&self) -> Option<&AdditionalPermissionProfile> {
        self.additional_permissions.as_ref()
    }

    /// The prefix proposed to allow from now on.
    #[must_use]
    pub const fn proposed_execpolicy_amendment(&self) -> Option<&ExecPolicyAmendment> {
        self.proposed_execpolicy_amendment.as_ref()
    }

    /// The rules proposed for the hosts the command reaches.
    #[must_use]
    pub fn proposed_network_policy_amendments(&self) -> Option<&[NetworkPolicyAmendment]> {
        self.proposed_network_policy_amendments.as_deref()
    }

    /// The decisions the client may offer, in order.
    #[must_use]
    pub fn available_decisions(&self) -> Option<&[CommandExecutionApprovalDecision]> {
        self.available_decisions.as_deref()
    }

    /// Why the request needs the experimental API, or `None` if it does not: Codex's derived
    /// `experimental_reason`, the method and field of the first experimental field that is set.
    #[must_use]
    pub const fn experimental_reason(&self) -> Option<&'static str> {
        if self.additional_permissions.is_some() {
            Some("item/commandExecution/requestApproval.additionalPermissions")
        } else if self.available_decisions.is_some() {
            Some("item/commandExecution/requestApproval.availableDecisions")
        } else {
            None
        }
    }

    /// Removes what a connection without the experimental API is not sent: Codex's
    /// `strip_experimental_fields`, which removes `additionalPermissions` only.
    pub(crate) fn strip_experimental_fields(&mut self) {
        self.additional_permissions = None;
    }
}

/// The answer to `item/commandExecution/requestApproval`: Codex's
/// `CommandExecutionRequestApprovalResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalResponse {
    decision: CommandExecutionApprovalDecision,
}

impl CommandExecutionRequestApprovalResponse {
    /// Answers with `decision`.
    #[must_use]
    pub const fn new(decision: CommandExecutionApprovalDecision) -> Self {
        Self { decision }
    }

    /// The decision.
    #[must_use]
    pub const fn decision(&self) -> &CommandExecutionApprovalDecision {
        &self.decision
    }

    pub(crate) fn into_decision(self) -> CommandExecutionApprovalDecision {
        self.decision
    }
}

/// The parameters of `item/fileChange/requestApproval`: Codex's `FileChangeRequestApprovalParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    started_at_ms: i64,
    reason: Option<String>,
    grant_root: Option<PathBuf>,
}

impl FileChangeRequestApprovalParams {
    /// An approval of the file changes of the item `item_id` of `turn_id` in `thread_id`, asked at
    /// `started_at_ms` milliseconds after the Unix epoch.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        started_at_ms: i64,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            started_at_ms,
            reason: None,
            grant_root: None,
        }
    }

    /// Sets why the approval is asked.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Asks to allow writes under `grant_root` for the rest of the session. Codex marks it
    /// unstable.
    #[must_use]
    pub fn with_grant_root(mut self, grant_root: impl Into<PathBuf>) -> Self {
        self.grant_root = Some(grant_root.into());
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// When the approval was asked, in milliseconds after the Unix epoch.
    #[must_use]
    pub const fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// Why the approval is asked.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The root under which writes are asked for, for the rest of the session.
    #[must_use]
    pub fn grant_root(&self) -> Option<&std::path::Path> {
        self.grant_root.as_deref()
    }
}

/// The answer to `item/fileChange/requestApproval`: Codex's `FileChangeRequestApprovalResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeRequestApprovalResponse {
    decision: FileChangeApprovalDecision,
}

impl FileChangeRequestApprovalResponse {
    /// Answers with `decision`.
    #[must_use]
    pub const fn new(decision: FileChangeApprovalDecision) -> Self {
        Self { decision }
    }

    /// The decision.
    #[must_use]
    pub const fn decision(&self) -> FileChangeApprovalDecision {
        self.decision
    }
}

/// The parameters of `item/tool/call`, a call of a tool the client provides: Codex's
/// `DynamicToolCallParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallParams {
    thread_id: String,
    turn_id: String,
    call_id: String,
    namespace: Option<String>,
    tool: String,
    arguments: Value,
}

impl DynamicToolCallParams {
    /// The call `call_id` of `tool` with `arguments`, in `turn_id` of `thread_id`.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            call_id: call_id.into(),
            namespace: None,
            tool: tool.into(),
            arguments,
        }
    }

    /// Sets the namespace of the tool.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The call.
    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The namespace of the tool, if it has one.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// The tool.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The arguments, as the model wrote them.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }
}

/// What a dynamic tool returned: Codex's `DynamicToolCallResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallResponse {
    content_items: Vec<DynamicToolCallOutputContentItem>,
    success: bool,
}

impl DynamicToolCallResponse {
    /// The tool returned `content_items`, having succeeded or not.
    #[must_use]
    pub fn new(
        content_items: impl IntoIterator<Item = DynamicToolCallOutputContentItem>,
        success: bool,
    ) -> Self {
        Self {
            content_items: content_items.into_iter().collect(),
            success,
        }
    }

    /// What the tool returned.
    #[must_use]
    pub fn content_items(&self) -> &[DynamicToolCallOutputContentItem] {
        &self.content_items
    }

    /// Whether it succeeded.
    #[must_use]
    pub const fn success(&self) -> bool {
        self.success
    }
}

/// One part of what a dynamic tool returned: Codex's `DynamicToolCallOutputContentItem`.
///
/// Images and audio are inline `data:` URLs; the server refuses a response with a remote image or
/// audio that is not inline, as Codex does.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolCallOutputContentItem {
    /// Text.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    InputText {
        /// The text.
        text: String,
    },
    /// An image.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    InputImage {
        /// The image, as a `data:` URL.
        image_url: String,
    },
    /// Audio.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    InputAudio {
        /// The audio, as a `data:` URL.
        audio_url: String,
    },
}

impl DynamicToolCallOutputContentItem {
    /// Text.
    #[must_use]
    pub fn input_text(text: impl Into<String>) -> Self {
        Self::InputText { text: text.into() }
    }

    /// An image at `image_url`.
    #[must_use]
    pub fn input_image(image_url: impl Into<String>) -> Self {
        Self::InputImage {
            image_url: image_url.into(),
        }
    }

    /// Audio at `audio_url`.
    #[must_use]
    pub fn input_audio(audio_url: impl Into<String>) -> Self {
        Self::InputAudio {
            audio_url: audio_url.into(),
        }
    }
}

/// One option a question offers: Codex's `ToolRequestUserInputOption`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputOption {
    label: String,
    description: String,
}

impl ToolRequestUserInputOption {
    /// The option `label`, explained by `description`.
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }

    /// The label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The explanation.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// One question for the user: Codex's `ToolRequestUserInputQuestion`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputQuestion {
    id: String,
    header: String,
    question: String,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
    options: Option<Vec<ToolRequestUserInputOption>>,
}

impl ToolRequestUserInputQuestion {
    /// The question `id`, headed `header`, asking `question`, without options.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        header: impl Into<String>,
        question: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            header: header.into(),
            question: question.into(),
            is_other: false,
            is_secret: false,
            options: None,
        }
    }

    /// Lets the user answer other than with an option.
    #[must_use]
    pub const fn with_is_other(mut self, is_other: bool) -> Self {
        self.is_other = is_other;
        self
    }

    /// Marks the answer secret, so the client hides it.
    #[must_use]
    pub const fn with_is_secret(mut self, is_secret: bool) -> Self {
        self.is_secret = is_secret;
        self
    }

    /// Sets the options offered.
    #[must_use]
    pub fn with_options(
        mut self,
        options: impl IntoIterator<Item = ToolRequestUserInputOption>,
    ) -> Self {
        self.options = Some(options.into_iter().collect());
        self
    }

    /// The question's id, which its answer is keyed by.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The header.
    #[must_use]
    pub fn header(&self) -> &str {
        &self.header
    }

    /// The question.
    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    /// Whether the user may answer other than with an option.
    #[must_use]
    pub const fn is_other(&self) -> bool {
        self.is_other
    }

    /// Whether the answer is secret.
    #[must_use]
    pub const fn is_secret(&self) -> bool {
        self.is_secret
    }

    /// The options offered, if any.
    #[must_use]
    pub fn options(&self) -> Option<&[ToolRequestUserInputOption]> {
        self.options.as_deref()
    }
}

/// The parameters of `item/tool/requestUserInput`: Codex's `ToolRequestUserInputParams`, which it
/// documents as experimental without gating the method.
///
/// A request without `isBlocking`, from a server older than the member, is read as blocking, as
/// Codex reads it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    questions: Vec<ToolRequestUserInputQuestion>,
    is_blocking: bool,
    auto_resolution_ms: Option<u64>,
}

impl ToolRequestUserInputParams {
    /// Asks `questions` for the item `item_id` of `turn_id` in `thread_id`; the turn waits for the
    /// answer.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        questions: impl IntoIterator<Item = ToolRequestUserInputQuestion>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            questions: questions.into_iter().collect(),
            is_blocking: true,
            auto_resolution_ms: None,
        }
    }

    /// Sets whether the turn waits for the answer.
    #[must_use]
    pub const fn with_is_blocking(mut self, is_blocking: bool) -> Self {
        self.is_blocking = is_blocking;
        self
    }

    /// Sets after how long the request resolves by itself. Codex deprecates it in favour of
    /// `isBlocking`.
    #[must_use]
    pub const fn with_auto_resolution_ms(mut self, auto_resolution_ms: u64) -> Self {
        self.auto_resolution_ms = Some(auto_resolution_ms);
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// The questions.
    #[must_use]
    pub fn questions(&self) -> &[ToolRequestUserInputQuestion] {
        &self.questions
    }

    /// Whether the turn waits for the answer.
    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        self.is_blocking
    }

    /// After how long the request resolves by itself, if it does.
    #[must_use]
    pub const fn auto_resolution_ms(&self) -> Option<u64> {
        self.auto_resolution_ms
    }
}

impl<'de> Deserialize<'de> for ToolRequestUserInputParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WireToolRequestUserInputParams {
            thread_id: String,
            turn_id: String,
            item_id: String,
            questions: Vec<ToolRequestUserInputQuestion>,
            is_blocking: Option<bool>,
            auto_resolution_ms: Option<u64>,
        }

        let wire = WireToolRequestUserInputParams::deserialize(deserializer)?;
        Ok(Self {
            thread_id: wire.thread_id,
            turn_id: wire.turn_id,
            item_id: wire.item_id,
            questions: wire.questions,
            is_blocking: wire.is_blocking.unwrap_or(true),
            auto_resolution_ms: wire.auto_resolution_ms,
        })
    }
}

/// The user's answer to one question: Codex's `ToolRequestUserInputAnswer`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputAnswer {
    answers: Vec<String>,
}

impl ToolRequestUserInputAnswer {
    /// The answer made of `answers`.
    #[must_use]
    pub fn new(answers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            answers: answers.into_iter().map(Into::into).collect(),
        }
    }

    /// What the user answered.
    #[must_use]
    pub fn answers(&self) -> &[String] {
        &self.answers
    }
}

/// The answer to `item/tool/requestUserInput`, keyed by question id: Codex's
/// `ToolRequestUserInputResponse`.
///
/// Codex keeps the answers in a `HashMap`; a `BTreeMap` writes the same JSON object, in a stable
/// order.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputResponse {
    answers: BTreeMap<String, ToolRequestUserInputAnswer>,
}

impl ToolRequestUserInputResponse {
    /// No answers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the answer to the question `id`.
    #[must_use]
    pub fn with_answer(
        mut self,
        id: impl Into<String>,
        answer: ToolRequestUserInputAnswer,
    ) -> Self {
        self.answers.insert(id.into(), answer);
        self
    }

    /// The answers, by question id.
    #[must_use]
    pub const fn answers(&self) -> &BTreeMap<String, ToolRequestUserInputAnswer> {
        &self.answers
    }
}
