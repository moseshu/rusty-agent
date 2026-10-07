//! The typed requests, notifications and payloads of the app server's methods: Codex's
//! `app-server-protocol` (`protocol/common.rs`, `protocol/v1.rs`, `experimental_api.rs`).
//!
//! A client opens a connection with `initialize`, naming itself and declaring what it can take,
//! and then sends the `initialized` notification. Every other method is refused until `initialize`
//! has succeeded, and a method or notification marked experimental is withheld from a connection
//! that did not opt into the experimental API.
//!
//! Field names are Codex's, in camelCase. Members a type does not define are ignored when it is
//! read, so a client that sends what Codex's server reads and this one does not — such as the
//! capabilities listed on [`InitializeCapabilities`] — still connects.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, de::Error};

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
            | Self::MockExperimentalMethod { request_id, .. } => request_id,
        }
    }

    /// The method called.
    #[must_use]
    pub const fn method_name(&self) -> &'static str {
        match self {
            Self::Initialize { .. } => "initialize",
            Self::MockExperimentalMethod { .. } => "mock/experimentalMethod",
        }
    }

    /// Why the request needs the experimental API, or `None` if it does not: Codex's
    /// `experimental_reason`, the method's name for a method that is experimental as a whole.
    ///
    /// Codex derives it from attributes on the methods and on the fields of their parameters; no
    /// method ported so far has an experimental field.
    #[must_use]
    pub const fn experimental_reason(&self) -> Option<&'static str> {
        match self {
            Self::Initialize { .. } => None,
            Self::MockExperimentalMethod { .. } => Some("mock/experimentalMethod"),
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
