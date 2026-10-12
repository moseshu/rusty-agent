//! The typed requests, notifications and payloads of the app server's methods: Codex's
//! `app-server-protocol` (`protocol/common.rs`, `protocol/v1.rs`, `experimental_api.rs`).
//!
//! A client opens a connection with `initialize`, naming itself and declaring what it can take,
//! and then sends the `initialized` notification. Every other method is refused until `initialize`
//! has succeeded, and a method or notification marked experimental is withheld from a connection
//! that did not opt into the experimental API.
//!
//! A client starts a thread with `thread/start`, loads a stored one — or attaches to one already
//! loaded — with `thread/resume`, and starts a new thread on a copy of another's history with
//! `thread/fork`; every connection is sent `thread/started` for a new thread, and
//! `thread/status/changed` as a loaded thread's status changes. `thread/read` reads a thread, and
//! `thread/loaded/list` the ids of those loaded.
//!
//! A client starts a thread's turns with `turn/start`, adds input to the running one with
//! `turn/steer` and stops it with `turn/interrupt`; `thread/settings/update` changes the settings
//! of the turns the thread starts next. The thread's connections are sent `turn/started` and
//! `turn/completed` as its turns start and end.
//!
//! As a turn runs, the thread's connections are sent `item/started` and `item/completed` for what
//! it is made of — the user's input, the agent's messages and reasoning, the commands it runs —
//! and `turn/completed` carries the turn's answer. A client that comes back reads what it missed
//! with `thread/turns/list`, rebuilt from the thread's rollout.
//!
//! A connection leaves a thread with `thread/unsubscribe`; every connection is sent
//! `thread/closed` once a thread is unloaded. `thread/backgroundTerminals/list`, `terminate` and
//! `clean` list and stop the commands a thread's tools left running.
//!
//! While a turn runs, the server asks the client in turn with a [`ServerRequest`]: to approve a
//! command, file changes or extra permissions, to call a tool the client provides, or to put
//! questions to the user. The client answers with a response carrying the method's answer, and the
//! connections of the thread are sent `serverRequest/resolved` once the request no longer needs
//! one.
//!
//! Field names are Codex's, in camelCase. Members a type does not define are ignored when it is
//! read, so a client that sends what Codex's server reads and this one does not — such as the
//! capabilities listed on [`InitializeCapabilities`] — still connects. The one exception is
//! [`RequestPermissionProfile`], which refuses them as Codex does.

mod item;
mod permissions;
mod server_request;
mod settings;
mod thread;
mod thread_data;
mod thread_item;
mod thread_lifecycle;
mod thread_management;
mod turn;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, de::Error};

pub(crate) use self::thread_lifecycle::ThreadPolicyFields;
pub use self::thread_management::*;
pub use self::{
    item::{
        CommandAction, CommandExecutionApprovalDecision, CommandExecutionApprovalKind,
        CommandExecutionRequestApprovalParams, CommandExecutionRequestApprovalResponse,
        DynamicToolCallOutputContentItem, DynamicToolCallParams, DynamicToolCallResponse,
        FileChangeApprovalDecision, FileChangeRequestApprovalParams,
        FileChangeRequestApprovalResponse, ToolRequestUserInputAnswer, ToolRequestUserInputOption,
        ToolRequestUserInputParams, ToolRequestUserInputQuestion, ToolRequestUserInputResponse,
    },
    permissions::{
        AdditionalFileSystemPermissions, AdditionalNetworkPermissions, AdditionalPermissionProfile,
        ExecPolicyAmendment, FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
        FileSystemSpecialPath, GrantedPermissionProfile, NetworkApprovalContext,
        NetworkApprovalProtocol, NetworkPolicyAmendment, NetworkPolicyRuleAction,
        PermissionGrantScope, PermissionsRequestApprovalParams, PermissionsRequestApprovalResponse,
        RequestPermissionProfile,
    },
    server_request::{
        SERVER_REQUEST_RESOLVED_METHOD, ServerRequest, ServerRequestPayload,
        ServerRequestResolvedNotification,
    },
    settings::{
        ApprovalsReviewer, AskForApproval, NetworkAccess, ReasoningSummary, SandboxPolicy,
        THREAD_SETTINGS_UPDATED_METHOD, ThreadPolicyOverrides, ThreadSettingsUpdatedNotification,
        TurnSettingsUpdateParams, TurnSettingsUpdateResponse, TurnSettingsUpdateStatus,
    },
    thread::{
        SortDirection, THREAD_CLOSED_METHOD, ThreadBackgroundTerminal,
        ThreadBackgroundTerminalsCleanParams, ThreadBackgroundTerminalsCleanResponse,
        ThreadBackgroundTerminalsListParams, ThreadBackgroundTerminalsListResponse,
        ThreadBackgroundTerminalsTerminateParams, ThreadBackgroundTerminalsTerminateResponse,
        ThreadClosedNotification, ThreadItemEntry, ThreadItemsListAnchor, ThreadItemsListCursor,
        ThreadItemsListParams, ThreadItemsListResponse, ThreadTurnsListParams,
        ThreadTurnsListResponse, ThreadUnsubscribeParams, ThreadUnsubscribeResponse,
        ThreadUnsubscribeStatus,
    },
    thread_data::{
        THREAD_STARTED_METHOD, THREAD_STATUS_CHANGED_METHOD, Thread, ThreadActiveFlag,
        ThreadHistoryMode, ThreadStartedNotification, ThreadStatus,
        ThreadStatusChangedNotification,
    },
    thread_item::{
        CommandExecutionItem, CommandExecutionSource, CommandExecutionStatus, FileUpdateChange,
        ITEM_COMPLETED_METHOD, ITEM_STARTED_METHOD, ItemCompletedNotification,
        ItemStartedNotification, MessagePhase, PatchApplyStatus, PatchChangeKind, ThreadItem,
    },
    thread_lifecycle::{
        SandboxMode, ThreadForkParams, ThreadForkResponse, ThreadLoadedListParams,
        ThreadLoadedListResponse, ThreadReadParams, ThreadReadResponse,
        ThreadResumeInitialTurnsPageParams, ThreadResumeParams, ThreadResumeResponse,
        ThreadSettingsRecord, ThreadStartParams, ThreadStartResponse, ThreadStartSource, TurnsPage,
    },
    turn::{
        ByteRange, ERROR_METHOD, ErrorNotification, ImageDetail, ImageReference,
        TURN_COMPLETED_METHOD, TURN_STARTED_METHOD, TextElement, ThreadSettingsUpdateParams,
        ThreadSettingsUpdateResponse, Turn, TurnCompletedNotification, TurnError,
        TurnInterruptParams, TurnInterruptResponse, TurnItemsView, TurnStartParams,
        TurnStartResponse, TurnStartedNotification, TurnStatus, TurnSteerParams, TurnSteerResponse,
        UserInput, WARNING_METHOD, WarningNotification,
    },
};
use crate::{
    absolute_path,
    frame::{JsonRpcRequest, RequestId},
};

/// The message a request that needs the experimental API gets without it: Codex's
/// `experimental_required_message`. `reason` names the method, or the method and field, that is
/// experimental.
#[must_use]
pub fn experimental_required_message(reason: &str) -> String {
    format!("{reason} requires experimentalApi capability")
}

/// A request from the client: Codex's `ClientRequest`.
///
/// It is read from a [`JsonRpcRequest`] by its method, and its parameters must fit that method's.
/// Codex's server has one variant per method; the methods of later protocol work are added here
/// as they are ported.
// A request is read once and handed to its method; boxing the larger parameters would only add an
// allocation and change how each variant is built and matched.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum ClientRequest {
    /// Opens the connection; nothing else is answered before it succeeds.
    #[serde(rename = "initialize")]
    Initialize {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// Who the client is and what it can take.
        params: InitializeParams,
    },
    /// Starts a thread and subscribes the connection to it.
    #[serde(rename = "thread/start")]
    ThreadStart {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread's settings.
        params: ThreadStartParams,
    },
    /// Loads a stored thread, or attaches to the loaded one, and subscribes the connection to it.
    #[serde(rename = "thread/resume")]
    ThreadResume {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and the settings it resumes with.
        params: ThreadResumeParams,
    },
    /// Starts a new thread on a copy of another's history.
    #[serde(rename = "thread/fork")]
    ThreadFork {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread forked, where to cut it, and the new thread's settings.
        params: ThreadForkParams,
    },
    /// The `thread/archive` request.
    #[serde(rename = "thread/archive")]
    ThreadArchive {
        /// The response correlation id.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The method parameters.
        params: ThreadArchiveParams,
    },
    /// The `thread/delete` request.
    #[serde(rename = "thread/delete")]
    ThreadDelete {
        /// The response correlation id.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The method parameters.
        params: ThreadDeleteParams,
    },
    /// The `thread/unarchive` request.
    #[serde(rename = "thread/unarchive")]
    ThreadUnarchive {
        /// The response correlation id.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The method parameters.
        params: ThreadUnarchiveParams,
    },
    /// The `thread/name/set` request.
    #[serde(rename = "thread/name/set")]
    ThreadSetName {
        /// The response correlation id.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The method parameters.
        params: ThreadSetNameParams,
    },
    /// The `thread/metadata/update` request.
    #[serde(rename = "thread/metadata/update")]
    ThreadMetadataUpdate {
        /// The response correlation id.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The method parameters.
        params: ThreadMetadataUpdateParams,
    },
    /// The `thread/list` request.
    #[serde(rename = "thread/list")]
    ThreadList {
        /// The response correlation id.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The method parameters.
        params: ThreadListParams,
    },
    /// Removes the connection's subscription to a thread.
    #[serde(rename = "thread/unsubscribe")]
    ThreadUnsubscribe {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread.
        params: ThreadUnsubscribeParams,
    },
    /// Reads the ids of the loaded threads.
    #[serde(rename = "thread/loaded/list")]
    ThreadLoadedList {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The page.
        params: ThreadLoadedListParams,
    },
    /// Reads a thread, and if asked its turns.
    #[serde(rename = "thread/read")]
    ThreadRead {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread.
        params: ThreadReadParams,
    },
    /// Reads a page of a thread's turns.
    #[serde(rename = "thread/turns/list")]
    ThreadTurnsList {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and the page.
        params: ThreadTurnsListParams,
    },
    /// Reads a page of a thread's items.
    #[serde(rename = "thread/items/list")]
    ThreadItemsList {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and the page.
        params: ThreadItemsListParams,
    },
    /// Stops every command a thread's tools left running. Experimental, as in Codex.
    #[serde(rename = "thread/backgroundTerminals/clean")]
    ThreadBackgroundTerminalsClean {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread.
        params: ThreadBackgroundTerminalsCleanParams,
    },
    /// Lists the commands a thread's tools left running. Experimental, as in Codex.
    #[serde(rename = "thread/backgroundTerminals/list")]
    ThreadBackgroundTerminalsList {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and the page.
        params: ThreadBackgroundTerminalsListParams,
    },
    /// Stops one command a thread's tools left running. Experimental, as in Codex.
    #[serde(rename = "thread/backgroundTerminals/terminate")]
    ThreadBackgroundTerminalsTerminate {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and the terminal.
        params: ThreadBackgroundTerminalsTerminateParams,
    },
    /// Changes the settings of the turns a thread starts next. Experimental, as in Codex.
    #[serde(rename = "thread/settings/update")]
    ThreadSettingsUpdate {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and its new settings.
        params: ThreadSettingsUpdateParams,
    },
    /// Publishes settings for subsequent steps of one running turn.
    #[serde(rename = "turn/settings/update")]
    TurnSettingsUpdate {
        /// Correlates the response with this request.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// Settings published for subsequent steps of the targeted turn.
        params: TurnSettingsUpdateParams,
    },
    /// Starts a turn of a thread, or steers the one running.
    #[serde(rename = "turn/start")]
    TurnStart {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread, the input and the settings it carries.
        params: TurnStartParams,
    },
    /// Adds input to the running turn of a thread.
    #[serde(rename = "turn/steer")]
    TurnSteer {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread, the turn and the input.
        params: TurnSteerParams,
    },
    /// Stops the running turn of a thread; answered once it has ended.
    #[serde(rename = "turn/interrupt")]
    TurnInterrupt {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The thread and the turn.
        params: TurnInterruptParams,
    },
    /// Echoes its value back. It exists to exercise the experimental gate, as Codex's does.
    #[serde(rename = "mock/experimentalMethod")]
    MockExperimentalMethod {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The value to echo.
        params: MockExperimentalMethodParams,
    },
}

impl ClientRequest {
    /// The id its answer will carry.
    #[must_use]
    pub const fn id(&self) -> &RequestId {
        match self {
            Self::Initialize { request_id, .. }
            | Self::ThreadArchive { request_id, .. }
            | Self::ThreadDelete { request_id, .. }
            | Self::ThreadUnarchive { request_id, .. }
            | Self::ThreadSetName { request_id, .. }
            | Self::ThreadMetadataUpdate { request_id, .. }
            | Self::ThreadList { request_id, .. }
            | Self::ThreadStart { request_id, .. }
            | Self::ThreadResume { request_id, .. }
            | Self::ThreadFork { request_id, .. }
            | Self::ThreadLoadedList { request_id, .. }
            | Self::ThreadRead { request_id, .. }
            | Self::ThreadUnsubscribe { request_id, .. }
            | Self::ThreadTurnsList { request_id, .. }
            | Self::ThreadItemsList { request_id, .. }
            | Self::ThreadBackgroundTerminalsClean { request_id, .. }
            | Self::ThreadBackgroundTerminalsList { request_id, .. }
            | Self::ThreadBackgroundTerminalsTerminate { request_id, .. }
            | Self::ThreadSettingsUpdate { request_id, .. }
            | Self::TurnSettingsUpdate { request_id, .. }
            | Self::TurnStart { request_id, .. }
            | Self::TurnSteer { request_id, .. }
            | Self::TurnInterrupt { request_id, .. }
            | Self::MockExperimentalMethod { request_id, .. } => request_id,
        }
    }

    /// The method called.
    #[must_use]
    pub const fn method_name(&self) -> &'static str {
        match self {
            Self::Initialize { .. } => "initialize",
            Self::ThreadArchive { .. } => "thread/archive",
            Self::ThreadDelete { .. } => "thread/delete",
            Self::ThreadUnarchive { .. } => "thread/unarchive",
            Self::ThreadSetName { .. } => "thread/name/set",
            Self::ThreadMetadataUpdate { .. } => "thread/metadata/update",
            Self::ThreadList { .. } => "thread/list",
            Self::ThreadStart { .. } => "thread/start",
            Self::ThreadResume { .. } => "thread/resume",
            Self::ThreadFork { .. } => "thread/fork",
            Self::ThreadLoadedList { .. } => "thread/loaded/list",
            Self::ThreadRead { .. } => "thread/read",
            Self::ThreadUnsubscribe { .. } => "thread/unsubscribe",
            Self::ThreadTurnsList { .. } => "thread/turns/list",
            Self::ThreadItemsList { .. } => "thread/items/list",
            Self::ThreadBackgroundTerminalsClean { .. } => "thread/backgroundTerminals/clean",
            Self::ThreadBackgroundTerminalsList { .. } => "thread/backgroundTerminals/list",
            Self::ThreadBackgroundTerminalsTerminate { .. } => {
                "thread/backgroundTerminals/terminate"
            }
            Self::ThreadSettingsUpdate { .. } => "thread/settings/update",
            Self::TurnSettingsUpdate { .. } => "turn/settings/update",
            Self::TurnStart { .. } => "turn/start",
            Self::TurnSteer { .. } => "turn/steer",
            Self::TurnInterrupt { .. } => "turn/interrupt",
            Self::MockExperimentalMethod { .. } => "mock/experimentalMethod",
        }
    }

    /// Why the request needs the experimental API, or `None` if it does not: Codex's
    /// `experimental_reason` — the method's name for a method that is experimental as a whole,
    /// and otherwise the reason of the first experimental field its parameters carry.
    ///
    /// Codex derives it from attributes on the methods and on the fields of their parameters;
    /// here each method's parameters say it.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        match self {
            Self::Initialize { .. }
            | Self::ThreadArchive { .. }
            | Self::ThreadDelete { .. }
            | Self::ThreadUnarchive { .. }
            | Self::ThreadSetName { .. }
            | Self::ThreadLoadedList { .. }
            | Self::ThreadRead { .. }
            | Self::ThreadUnsubscribe { .. }
            | Self::ThreadTurnsList { .. }
            | Self::ThreadItemsList { .. }
            | Self::TurnInterrupt { .. } => None,
            Self::ThreadMetadataUpdate { params, .. } => params.experimental_reason(),
            Self::ThreadList { params, .. } => params.experimental_reason(),
            Self::ThreadStart { params, .. } => params.experimental_reason(),
            Self::ThreadResume { params, .. } => params.experimental_reason(),
            Self::ThreadFork { params, .. } => params.experimental_reason(),
            Self::ThreadBackgroundTerminalsClean { .. } => Some("thread/backgroundTerminals/clean"),
            Self::ThreadBackgroundTerminalsList { .. } => Some("thread/backgroundTerminals/list"),
            Self::ThreadBackgroundTerminalsTerminate { .. } => {
                Some("thread/backgroundTerminals/terminate")
            }
            Self::ThreadSettingsUpdate { .. } => Some("thread/settings/update"),
            Self::TurnSettingsUpdate { .. } => Some("turn/settings/update"),
            Self::TurnStart { params, .. } => params.experimental_reason(),
            Self::TurnSteer { params, .. } => params.experimental_reason(),
            Self::MockExperimentalMethod { .. } => Some("mock/experimentalMethod"),
        }
    }
}

impl ClientRequest {
    /// The thread whose requests this one runs in order with, or `None` if it runs alongside any:
    /// Codex's `serialization_scope`, for the thread scope its methods declare.
    pub(crate) fn serialization_thread(&self) -> Option<&str> {
        match self {
            // Codex reads history alongside anything, as it reads append-only rollouts.
            Self::Initialize { .. }
            | Self::ThreadStart { .. }
            | Self::ThreadList { .. }
            | Self::ThreadLoadedList { .. }
            | Self::ThreadTurnsList { .. }
            | Self::ThreadItemsList { .. }
            | Self::MockExperimentalMethod { .. } => None,
            // Codex keys these by path when one is given; a path is not supported here.
            Self::ThreadArchive { params, .. } => Some(params.thread_id()),
            Self::ThreadDelete { params, .. } => Some(params.thread_id()),
            Self::ThreadUnarchive { params, .. } => Some(params.thread_id()),
            Self::ThreadSetName { params, .. } => Some(params.thread_id()),
            Self::ThreadMetadataUpdate { params, .. } => Some(params.thread_id()),
            Self::ThreadResume { params, .. } => Some(params.thread_id()),
            Self::ThreadFork { params, .. } => Some(params.thread_id()),
            Self::ThreadRead { params, .. } => Some(params.thread_id()),
            Self::ThreadUnsubscribe { params, .. } => Some(params.thread_id()),
            Self::ThreadBackgroundTerminalsClean { params, .. } => Some(params.thread_id()),
            Self::ThreadBackgroundTerminalsList { params, .. } => Some(params.thread_id()),
            Self::ThreadBackgroundTerminalsTerminate { params, .. } => Some(params.thread_id()),
            Self::ThreadSettingsUpdate { params, .. } => Some(params.thread_id()),
            Self::TurnSettingsUpdate { params, .. } => Some(params.thread_id()),
            Self::TurnStart { params, .. } => Some(params.thread_id()),
            Self::TurnSteer { params, .. } => Some(params.thread_id()),
            Self::TurnInterrupt { params, .. } => Some(params.thread_id()),
        }
    }
}

impl TryFrom<JsonRpcRequest> for ClientRequest {
    type Error = serde_json::Error;

    /// Reads the request by its method, as Codex's `TryFrom<JSONRPCRequest>` does: an unknown
    /// method or parameters that do not fit fail with serde's description of the mismatch.
    fn try_from(request: JsonRpcRequest) -> Result<Self, Self::Error> {
        let mut object = serde_json::Map::new();
        object.insert("id".to_owned(), serde_json::to_value(request.id())?);
        object.insert(
            "method".to_owned(),
            serde_json::Value::String(request.method().to_owned()),
        );
        if let Some(params) = request.into_params() {
            object.insert("params".to_owned(), params);
        }
        serde_json::from_value(serde_json::Value::Object(object))
    }
}

/// A notification from the client: Codex's `ClientNotification`.
///
/// As in Codex, the server reads no notification from a client: it logs them. `initialized`
/// completes the handshake from the client's side, but the connection is open once `initialize`
/// has been answered.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum ClientNotification {
    /// Sent after the answer to `initialize`.
    Initialized,
}

/// The parameters of `initialize`: Codex's `InitializeParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    client_info: ClientInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<InitializeCapabilities>,
}

impl InitializeParams {
    /// The parameters of a client that declares no capabilities.
    #[must_use]
    pub fn new(client_info: ClientInfo) -> Self {
        Self {
            client_info,
            capabilities: None,
        }
    }

    /// Declares the client's capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: InitializeCapabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    /// Who the client is.
    #[must_use]
    pub const fn client_info(&self) -> &ClientInfo {
        &self.client_info
    }

    /// What the client can take, if it said.
    #[must_use]
    pub const fn capabilities(&self) -> Option<&InitializeCapabilities> {
        self.capabilities.as_ref()
    }

    pub(crate) fn into_parts(self) -> (ClientInfo, Option<InitializeCapabilities>) {
        (self.client_info, self.capabilities)
    }
}

/// Who the client is: Codex's `ClientInfo`.
///
/// The name must be a valid HTTP header value, or `initialize` is refused, as in Codex.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    name: String,
    title: Option<String>,
    version: String,
}

impl ClientInfo {
    /// The client `name` at `version`, without a display title.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            version: version.into(),
        }
    }

    /// Sets the title to display.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The client's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The title to display, if any.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The client's version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn into_name_and_version(self) -> (String, String) {
        (self.name, self.version)
    }
}

/// What the client declares it can take: Codex's `InitializeCapabilities`.
///
/// Codex's other capabilities are not read here: `explicitGatewayOauth` and `requestAttestation`
/// belong to its account machinery, and `mcpServerOpenaiFormElicitation` and `extensions` are MCP
/// client settings, which come with the MCP methods.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    #[serde(default)]
    experimental_api: bool,
    opt_out_notification_methods: Option<Vec<String>>,
}

impl InitializeCapabilities {
    /// No experimental API and no notification left out.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opts into the experimental methods and notifications, or out of them.
    #[must_use]
    pub const fn with_experimental_api(mut self, experimental_api: bool) -> Self {
        self.experimental_api = experimental_api;
        self
    }

    /// The notification methods, by exact name, that this connection is not sent.
    #[must_use]
    pub fn with_opt_out_notification_methods(
        mut self,
        methods: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.opt_out_notification_methods = Some(methods.into_iter().map(Into::into).collect());
        self
    }

    /// Whether the client opted into the experimental API.
    #[must_use]
    pub const fn experimental_api(&self) -> bool {
        self.experimental_api
    }

    /// The notification methods the client opted out of, if it named any.
    #[must_use]
    pub fn opt_out_notification_methods(&self) -> Option<&[String]> {
        self.opt_out_notification_methods.as_deref()
    }

    pub(crate) fn into_parts(self) -> (bool, Vec<String>) {
        (
            self.experimental_api,
            self.opt_out_notification_methods.unwrap_or_default(),
        )
    }
}

/// The answer to `initialize`: Codex's `InitializeResponse`.
///
/// `codexHome` is the host's configuration and state directory. The wire name remains Codex's;
/// the host supplies the directory when building [`super::AppServer`]. Codex stores it as an
/// `AbsolutePathBuf`; here it is a `PathBuf`, normalized at construction and deserialization.
/// Responses carry an absolute path, so no thread-local deserialization base is needed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    user_agent: String,
    #[serde(deserialize_with = "deserialize_absolute_path")]
    codex_home: PathBuf,
    platform_family: String,
    platform_os: String,
}

impl InitializeResponse {
    pub(crate) fn new(user_agent: impl Into<String>, codex_home: PathBuf) -> Self {
        Self {
            user_agent: user_agent.into(),
            codex_home,
            platform_family: std::env::consts::FAMILY.to_owned(),
            platform_os: std::env::consts::OS.to_owned(),
        }
    }

    /// The `User-Agent` the agent sends to model providers.
    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// The absolute path to the server's configuration and state directory.
    #[must_use]
    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    /// The platform family the server runs on, such as `unix` or `windows`.
    #[must_use]
    pub fn platform_family(&self) -> &str {
        &self.platform_family
    }

    /// The operating system the server runs on, such as `macos`, `linux` or `windows`.
    #[must_use]
    pub fn platform_os(&self) -> &str {
        &self.platform_os
    }
}

/// Reads an absolute response path as Codex's `AbsolutePathBuf` does without a base override.
fn deserialize_absolute_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    let path = PathBuf::deserialize(deserializer)?;
    if !path.is_absolute() {
        return Err(D::Error::custom(
            "AbsolutePathBuf deserialized without a base path",
        ));
    }
    absolute_path::from_absolute_path(&path).map_err(D::Error::custom)
}

/// The parameters of `mock/experimentalMethod`: Codex's `MockExperimentalMethodParams`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MockExperimentalMethodParams {
    value: Option<String>,
}

impl MockExperimentalMethodParams {
    /// Parameters without a value.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the value to echo.
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// The value to echo, if any.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub(crate) fn into_value(self) -> Option<String> {
        self.value
    }
}

/// The answer to `mock/experimentalMethod`: Codex's `MockExperimentalMethodResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MockExperimentalMethodResponse {
    echoed: Option<String>,
}

impl MockExperimentalMethodResponse {
    pub(crate) const fn new(echoed: Option<String>) -> Self {
        Self { echoed }
    }

    /// The value the request carried.
    #[must_use]
    pub fn echoed(&self) -> Option<&str> {
        self.echoed.as_deref()
    }
}
