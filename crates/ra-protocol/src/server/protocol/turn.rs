//! Starting, steering and interrupting a thread's turns, and changing the thread's settings:
//! Codex's v2 turn and thread-settings payloads (`protocol/v2/turn.rs`, `protocol/v2/thread.rs`,
//! `protocol/v2/thread_data.rs`).
//!
//! Every field Codex defines is read, so the experimental gate sees what Codex's sees. The fields
//! that set something a run here does not have — a working directory, approval and sandbox
//! policies, a turn's output schema, additional context — are refused rather than ignored, so a
//! client never believes a setting applied that did not; see
//! [`TurnStartParams::unsupported_field`]. The two Codex itself ignores, `personality` and
//! `multiAgentMode`, are ignored here too.

#![allow(
    clippy::option_option,
    reason = "Codex distinguishes omitted, null and supplied service tiers"
)]

use std::path::PathBuf;

use super::{AskForApproval, ReasoningSummary, ThreadItem, ThreadPolicyOverrides};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The state of a turn: Codex's `TurnStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    /// It ended without an error.
    Completed,
    /// It was aborted.
    Interrupted,
    /// It ended on an error.
    Failed,
    /// It is running.
    InProgress,
}

/// How much of a turn's items a payload carries: Codex's `TurnItemsView`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnItemsView {
    /// None of them; `items` is empty on purpose.
    NotLoaded,
    /// A display summary.
    Summary,
    /// All of them.
    #[default]
    Full,
}

impl TurnItemsView {
    /// The items of `items` this view includes, in order: Codex's `project_items`. A summary is
    /// the first user message and the last agent message, or whichever of them there is.
    #[must_use]
    pub fn project_items(self, items: &[ThreadItem]) -> Vec<ThreadItem> {
        match self {
            Self::NotLoaded => Vec::new(),
            Self::Full => items.to_vec(),
            Self::Summary => {
                let first_user_message = items
                    .iter()
                    .find(|item| matches!(item, ThreadItem::UserMessage { .. }));
                let final_agent_message = items
                    .iter()
                    .rev()
                    .find(|item| matches!(item, ThreadItem::AgentMessage { .. }));
                match (first_user_message, final_agent_message) {
                    (Some(user_message), Some(agent_message))
                        if user_message.id() != agent_message.id() =>
                    {
                        vec![user_message.clone(), agent_message.clone()]
                    }
                    (Some(user_message), _) => vec![user_message.clone()],
                    (None, Some(agent_message)) => vec![agent_message.clone()],
                    (None, None) => Vec::new(),
                }
            }
        }
    }
}

/// Why a turn failed: Codex's `TurnError`.
///
/// Codex classifies the error and explains a misalignment block; neither is produced here yet, so
/// both are carried as the JSON a newer server sends and written as `null`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    message: String,
    codex_error_info: Option<Value>,
    #[serde(default)]
    additional_details: Option<String>,
    #[serde(default)]
    misalignment: Option<Value>,
}

impl TurnError {
    /// An error described by `message` alone.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            codex_error_info: None,
            additional_details: None,
            misalignment: None,
        }
    }

    /// What went wrong.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Codex's classification of the error, as sent.
    #[must_use]
    pub const fn codex_error_info(&self) -> Option<&Value> {
        self.codex_error_info.as_ref()
    }

    /// More about it, if anything.
    #[must_use]
    pub fn additional_details(&self) -> Option<&str> {
        self.additional_details.as_deref()
    }

    /// The explanation of a misalignment block, as sent.
    #[must_use]
    pub const fn misalignment(&self) -> Option<&Value> {
        self.misalignment.as_ref()
    }
}

/// A turn of a thread: Codex's `Turn`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    id: String,
    items: Vec<ThreadItem>,
    #[serde(default)]
    items_view: TurnItemsView,
    status: TurnStatus,
    error: Option<TurnError>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
}

impl Turn {
    /// The turn `id` with no items loaded, in `status`, and nothing else known about it yet.
    #[must_use]
    pub fn new(id: impl Into<String>, status: TurnStatus) -> Self {
        Self {
            id: id.into(),
            items: Vec::new(),
            items_view: TurnItemsView::NotLoaded,
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    /// Includes `items`, which are as much of the turn's items as `items_view` says.
    #[must_use]
    pub fn with_items(mut self, items: Vec<ThreadItem>, items_view: TurnItemsView) -> Self {
        self.items = items;
        self.items_view = items_view;
        self
    }

    /// Keeps only as much of its items as `items_view` includes: Codex's
    /// `apply_thread_turns_items_view` for one turn.
    #[must_use]
    pub fn in_items_view(mut self, items_view: TurnItemsView) -> Self {
        if items_view != TurnItemsView::Full && self.items_view != items_view {
            self.items = items_view.project_items(&self.items);
        }
        self.items_view = items_view;
        self
    }

    /// Records why it failed or was aborted.
    #[must_use]
    pub fn with_error(mut self, error: TurnError) -> Self {
        self.error = Some(error);
        self
    }

    /// Records when it started, in seconds since the Unix epoch.
    #[must_use]
    pub const fn with_started_at(mut self, started_at: Option<i64>) -> Self {
        self.started_at = started_at;
        self
    }

    /// Records when it ended, in seconds since the Unix epoch, and how long it ran.
    #[must_use]
    pub const fn with_completion(mut self, completed_at: i64, duration_ms: i64) -> Self {
        self.completed_at = Some(completed_at);
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Its identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn set_status(&mut self, status: TurnStatus) {
        self.status = status;
    }

    /// The items included.
    #[must_use]
    pub fn items(&self) -> &[ThreadItem] {
        &self.items
    }

    /// How much of its items are included.
    #[must_use]
    pub const fn items_view(&self) -> TurnItemsView {
        self.items_view
    }

    /// Its state.
    #[must_use]
    pub const fn status(&self) -> TurnStatus {
        self.status
    }

    /// Why it failed or was aborted, if it says.
    #[must_use]
    pub const fn error(&self) -> Option<&TurnError> {
        self.error.as_ref()
    }

    /// When it started, in seconds since the Unix epoch, if known.
    #[must_use]
    pub const fn started_at(&self) -> Option<i64> {
        self.started_at
    }

    /// When it ended, in seconds since the Unix epoch, if it has.
    #[must_use]
    pub const fn completed_at(&self) -> Option<i64> {
        self.completed_at
    }

    /// How long it ran, in milliseconds, if known.
    #[must_use]
    pub const fn duration_ms(&self) -> Option<i64> {
        self.duration_ms
    }
}

/// A byte range of a text input: Codex's v2 `ByteRange`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    start: usize,
    end: usize,
}

impl ByteRange {
    /// The bytes from `start` up to `end`.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Where it starts.
    #[must_use]
    pub const fn start(&self) -> usize {
        self.start
    }

    /// Where it ends.
    #[must_use]
    pub const fn end(&self) -> usize {
        self.end
    }
}

/// A span of a text input a client renders specially: Codex's v2 `TextElement`. The server keeps
/// it for display only; the model reads the text.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextElement {
    byte_range: ByteRange,
    placeholder: Option<String>,
}

impl TextElement {
    /// The span `byte_range`, shown as `placeholder` if one is given.
    #[must_use]
    pub const fn new(byte_range: ByteRange, placeholder: Option<String>) -> Self {
        Self {
            byte_range,
            placeholder,
        }
    }

    /// Where it is in the text.
    #[must_use]
    pub const fn byte_range(&self) -> ByteRange {
        self.byte_range
    }

    /// What to show in its place, if anything.
    #[must_use]
    pub fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }
}

/// How much vision detail to spend on an image: Codex's `ImageDetail`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    /// The provider's choice.
    Auto,
    /// Fewest tokens.
    Low,
    /// Most tokens the provider downsamples to.
    High,
    /// The image as it is, without downsampling.
    Original,
}

/// Where an image input is: Codex's v2 `ImageReference`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageReference {
    /// A URL; the server takes only inline `data:` URLs.
    Inline {
        /// The URL.
        url: String,
    },
    /// A file already uploaded to the provider.
    File {
        /// Its identifier.
        #[serde(rename = "fileId")]
        file_id: String,
    },
}

/// One piece of user input: Codex's v2 `UserInput`.
///
/// Text and images reach the model. Audio, skills and mentions are read, so a request carrying
/// them is understood, and refused when a turn is given them: they belong to Codex's realtime,
/// skills and plugin machinery.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    /// Text.
    #[non_exhaustive]
    Text {
        /// The text.
        text: String,
        /// Spans of it the client renders specially.
        #[serde(default)]
        text_elements: Vec<TextElement>,
    },
    /// An image given by URL or provider file.
    #[non_exhaustive]
    Image {
        /// Where it is.
        #[serde(flatten)]
        image: ImageReference,
        /// How much detail to spend on it.
        #[serde(default)]
        detail: Option<ImageDetail>,
    },
    /// An image in a file the server can read.
    #[non_exhaustive]
    LocalImage {
        /// How much detail to spend on it.
        #[serde(default)]
        detail: Option<ImageDetail>,
        /// The file.
        path: PathBuf,
    },
    /// Audio given by URL.
    #[non_exhaustive]
    Audio {
        /// The URL.
        url: String,
    },
    /// Audio in a file the server can read.
    #[non_exhaustive]
    LocalAudio {
        /// The file.
        path: PathBuf,
    },
    /// A skill to use.
    #[non_exhaustive]
    Skill {
        /// Its name.
        name: String,
        /// Where it is.
        path: PathBuf,
    },
    /// Something mentioned by name.
    #[non_exhaustive]
    Mention {
        /// Its name.
        name: String,
        /// Where it is.
        path: String,
    },
}

impl UserInput {
    /// Plain text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }

    /// An image at `url`.
    #[must_use]
    pub fn image_url(url: impl Into<String>) -> Self {
        Self::Image {
            image: ImageReference::Inline { url: url.into() },
            detail: None,
        }
    }

    /// An image in the file at `path`.
    #[must_use]
    pub fn local_image(path: impl Into<PathBuf>) -> Self {
        Self::LocalImage {
            detail: None,
            path: path.into(),
        }
    }

    /// Audio at `url`.
    #[must_use]
    pub fn audio(url: impl Into<String>) -> Self {
        Self::Audio { url: url.into() }
    }

    /// How many characters of text it carries: Codex's `text_char_count`.
    #[must_use]
    pub fn text_char_count(&self) -> usize {
        match self {
            Self::Text { text, .. } => text.chars().count(),
            Self::Image { .. }
            | Self::LocalImage { .. }
            | Self::Audio { .. }
            | Self::LocalAudio { .. }
            | Self::Skill { .. }
            | Self::Mention { .. } => 0,
        }
    }
}

/// Shared settings fields of turn admission and persistent thread updates. Unsupported fields
/// retain their JSON shape; service tier distinguishes omission from an explicit null.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadSettingsFields {
    #[serde(flatten)]
    policy: ThreadPolicyOverrides,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled_plugin_ids: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<Value>,
    #[serde(
        default,
        deserialize_with = "super::settings::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    service_tier: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<ReasoningSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collaboration_mode: Option<Value>,
}

impl ThreadSettingsFields {
    /// The first field sent, in the order Codex declares them on `turn/start`.
    fn first_sent(&self) -> Option<&'static str> {
        [
            ("disabledPluginIds", &self.disabled_plugin_ids),
            ("cwd", &self.cwd),
            ("collaborationMode", &self.collaboration_mode),
        ]
        .into_iter()
        .find_map(|(name, value)| value.is_some().then_some(name))
    }

    /// Whether the approval policy sent is Codex's experimental granular one.
    fn granular_approval_policy(&self) -> bool {
        matches!(
            self.policy.approval_policy,
            Some(AskForApproval::Granular { .. })
        )
    }
}

/// The parameters of `turn/start`: Codex's `TurnStartParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_user_message_id: Option<String>,
    input: Vec<UserInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(flatten)]
    settings: ThreadSettingsFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_trigger: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    responsesapi_client_metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_workspace_roots: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier_for_turn: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    personality: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    multi_agent_mode: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cyber_access_program: Option<Value>,
}

impl TurnStartParams {
    /// Host-owned permission and approval overrides.
    #[must_use]
    pub fn policy(&self) -> &ThreadPolicyOverrides {
        &self.settings.policy
    }

    /// Provider-side reasoning-summary override.
    #[must_use]
    pub fn summary(&self) -> Option<ReasoningSummary> {
        self.settings.summary
    }

    /// Provider-side service tier. Omission and null keep the thread selection.
    #[must_use]
    pub fn service_tier(&self) -> Option<Option<&str>> {
        self.settings
            .service_tier
            .as_ref()
            .map(|tier| tier.as_deref())
    }

    /// Starts a turn of `thread_id` on `input`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, input: Vec<UserInput>) -> Self {
        Self {
            thread_id: thread_id.into(),
            client_user_message_id: None,
            input,
            model: None,
            effort: None,
            settings: ThreadSettingsFields::default(),
            turn_trigger: None,
            tool_output: None,
            responsesapi_client_metadata: None,
            additional_context: None,
            environments: None,
            runtime_workspace_roots: None,
            service_tier_for_turn: None,
            output_schema: None,
            personality: None,
            multi_agent_mode: None,
            cyber_access_program: None,
        }
    }

    /// Runs this turn and the thread's later ones on `model`.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Runs this turn and the thread's later ones with reasoning `effort`, by its wire name.
    #[must_use]
    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    /// Names the user message the client will show for this input.
    #[must_use]
    pub fn with_client_user_message_id(mut self, id: impl Into<String>) -> Self {
        self.client_user_message_id = Some(id.into());
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The input.
    #[must_use]
    pub fn input(&self) -> &[UserInput] {
        &self.input
    }

    /// The model asked for, if any.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The reasoning effort asked for, by its wire name, if any.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// The client's identifier for the user message, if it gave one.
    #[must_use]
    pub fn client_user_message_id(&self) -> Option<&str> {
        self.client_user_message_id.as_deref()
    }

    /// Why the request needs the experimental API, if it does: the reason Codex gives for the
    /// first experimental field sent, in the order it declares them.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        [
            (
                "turn/start.responsesapiClientMetadata",
                self.responsesapi_client_metadata.is_some(),
            ),
            (
                "turn/start.additionalContext",
                self.additional_context.is_some(),
            ),
            ("turn/start.environments", self.environments.is_some()),
            (
                "turn/start.runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            (
                "askForApproval.granular",
                self.settings.granular_approval_policy(),
            ),
            (
                "turn/start.permissions",
                self.settings.policy.permissions.is_some(),
            ),
            (
                "turn/start.collaborationMode",
                self.settings.collaboration_mode.is_some(),
            ),
            ("turn/start.multiAgentMode", self.multi_agent_mode.is_some()),
            (
                "turn/start.cyberAccessProgram",
                self.cyber_access_program.is_some(),
            ),
        ]
        .into_iter()
        .find_map(|(reason, sent)| sent.then_some(reason))
    }

    /// The first field sent that sets something a run here does not have, by its wire name.
    ///
    /// Such a request is refused. Codex applies these: a working directory and runtime workspace
    /// roots, environments, approval and sandbox policies and permission profiles, a service tier
    /// and reasoning summary, a collaboration mode, disabled plugins, a tool output to answer in
    /// place of input, a turn trigger and Responses API metadata, additional context, a cyber
    /// access program, and an output schema for the turn's final answer.
    #[must_use]
    pub fn unsupported_field(&self) -> Option<&'static str> {
        [
            ("turnTrigger", self.turn_trigger.is_some()),
            ("toolOutput", self.tool_output.is_some()),
            (
                "responsesapiClientMetadata",
                self.responsesapi_client_metadata.is_some(),
            ),
            ("additionalContext", self.additional_context.is_some()),
            ("environments", self.environments.is_some()),
            (
                "runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            ("serviceTierForTurn", self.service_tier_for_turn.is_some()),
            ("outputSchema", self.output_schema.is_some()),
            ("cyberAccessProgram", self.cyber_access_program.is_some()),
        ]
        .into_iter()
        .find_map(|(name, sent)| sent.then_some(name))
        .or_else(|| self.settings.first_sent())
    }

    pub(crate) fn into_parts(self) -> (String, Vec<UserInput>, Option<String>, Option<String>) {
        (self.thread_id, self.input, self.model, self.effort)
    }
}

/// The answer to `turn/start`: Codex's `TurnStartResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    turn: Turn,
}

impl TurnStartResponse {
    pub(crate) const fn new(turn: Turn) -> Self {
        Self { turn }
    }

    /// The turn started, or the one steered.
    #[must_use]
    pub const fn turn(&self) -> &Turn {
        &self.turn
    }
}

/// The parameters of `turn/steer`: Codex's `TurnSteerParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_user_message_id: Option<String>,
    input: Vec<UserInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    responsesapi_client_metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_context: Option<Value>,
    expected_turn_id: String,
}

impl TurnSteerParams {
    /// Steers the turn `expected_turn_id` of `thread_id` with `input`.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        input: Vec<UserInput>,
        expected_turn_id: impl Into<String>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            client_user_message_id: None,
            input,
            responsesapi_client_metadata: None,
            additional_context: None,
            expected_turn_id: expected_turn_id.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The input.
    #[must_use]
    pub fn input(&self) -> &[UserInput] {
        &self.input
    }

    /// The turn that must be running.
    #[must_use]
    pub fn expected_turn_id(&self) -> &str {
        &self.expected_turn_id
    }

    /// Why the request needs the experimental API, if it does, as Codex says.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        if self.responsesapi_client_metadata.is_some() {
            Some("turn/steer.responsesapiClientMetadata")
        } else if self.additional_context.is_some() {
            Some("turn/steer.additionalContext")
        } else {
            None
        }
    }

    /// The first field sent that sets something a run here does not have, by its wire name.
    #[must_use]
    pub fn unsupported_field(&self) -> Option<&'static str> {
        if self.responsesapi_client_metadata.is_some() {
            Some("responsesapiClientMetadata")
        } else if self.additional_context.is_some() {
            Some("additionalContext")
        } else {
            None
        }
    }

    pub(crate) fn into_parts(self) -> (String, Vec<UserInput>, String) {
        (self.thread_id, self.input, self.expected_turn_id)
    }
}

/// The answer to `turn/steer`: Codex's `TurnSteerResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
    turn_id: String,
}

impl TurnSteerResponse {
    pub(crate) const fn new(turn_id: String) -> Self {
        Self { turn_id }
    }

    /// The turn steered.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

/// The parameters of `turn/interrupt`: Codex's `TurnInterruptParams`.
///
/// An empty `turnId` interrupts the thread's startup, which has no turn; the answer then comes at
/// once rather than when a turn ends.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    thread_id: String,
    turn_id: String,
}

impl TurnInterruptParams {
    /// Interrupts the turn `turn_id` of `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
        }
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

    pub(crate) fn into_parts(self) -> (String, String) {
        (self.thread_id, self.turn_id)
    }
}

/// The answer to `turn/interrupt`, sent once the turn has ended: Codex's `TurnInterruptResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInterruptResponse {}

impl TurnInterruptResponse {
    pub(crate) const fn new() -> Self {
        Self {}
    }
}

/// The parameters of `thread/settings/update`: Codex's `ThreadSettingsUpdateParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettingsUpdateParams {
    thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(flatten)]
    settings: ThreadSettingsFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    personality: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    multi_agent_mode: Option<Value>,
}

impl ThreadSettingsUpdateParams {
    /// Host-owned permission and approval overrides.
    #[must_use]
    pub fn policy(&self) -> &ThreadPolicyOverrides {
        &self.settings.policy
    }

    /// Provider-side reasoning-summary override.
    #[must_use]
    pub fn summary(&self) -> Option<ReasoningSummary> {
        self.settings.summary
    }

    /// Provider-side service tier. Omission and null keep the thread selection.
    #[must_use]
    pub fn service_tier(&self) -> Option<Option<&str>> {
        self.settings
            .service_tier
            .as_ref()
            .map(|tier| tier.as_deref())
    }

    /// Changes nothing of `thread_id` yet.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            model: None,
            effort: None,
            settings: ThreadSettingsFields::default(),
            personality: None,
            multi_agent_mode: None,
        }
    }

    /// Runs the thread's later turns on `model`.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Runs the thread's later turns with reasoning `effort`, by its wire name.
    #[must_use]
    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The model asked for, if any.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The reasoning effort asked for, by its wire name, if any.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Why the request needs the experimental API beyond the method's own, if it does, as Codex
    /// says.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        if self.settings.granular_approval_policy() {
            Some("askForApproval.granular")
        } else if self.settings.policy.permissions.is_some() {
            Some("thread/settings/update.permissions")
        } else if self.settings.collaboration_mode.is_some() {
            Some("thread/settings/update.collaborationMode")
        } else if self.multi_agent_mode.is_some() {
            Some("thread/settings/update.multiAgentMode")
        } else {
            None
        }
    }

    /// The first field sent that sets something a run here does not have, by its wire name.
    #[must_use]
    pub fn unsupported_field(&self) -> Option<&'static str> {
        self.settings.first_sent()
    }

    pub(crate) fn into_parts(self) -> (String, Option<String>, Option<String>) {
        (self.thread_id, self.model, self.effort)
    }
}

/// The answer to `thread/settings/update`: Codex's `ThreadSettingsUpdateResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSettingsUpdateResponse {}

impl ThreadSettingsUpdateResponse {
    pub(crate) const fn new() -> Self {
        Self {}
    }
}

/// The method of the notification that a turn started.
pub const TURN_STARTED_METHOD: &str = "turn/started";

/// The method of the notification that a turn ended.
pub const TURN_COMPLETED_METHOD: &str = "turn/completed";

/// That a turn started: Codex's `TurnStartedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    thread_id: String,
    turn: Turn,
}

impl TurnStartedNotification {
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn new(thread_id: impl Into<String>, turn: Turn) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub const fn turn(&self) -> &Turn {
        &self.turn
    }
}

/// That a turn ended, however it did: Codex's `TurnCompletedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    thread_id: String,
    turn: Turn,
}

impl TurnCompletedNotification {
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn new(thread_id: impl Into<String>, turn: Turn) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub const fn turn(&self) -> &Turn {
        &self.turn
    }
}

/// The method of the notification that a turn ran into an error.
pub const ERROR_METHOD: &str = "error";

/// That a turn ran into an error: Codex's `ErrorNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    error: TurnError,
    will_retry: bool,
    thread_id: String,
    turn_id: String,
}

impl ErrorNotification {
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn new(
        error: TurnError,
        will_retry: bool,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            error,
            will_retry,
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
        }
    }

    /// What went wrong.
    #[must_use]
    pub const fn error(&self) -> &TurnError {
        &self.error
    }

    /// Whether the server retries on its own, so the turn goes on.
    #[must_use]
    pub const fn will_retry(&self) -> bool {
        self.will_retry
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
}
