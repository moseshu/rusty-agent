// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Child-process transport; protocol operations stay in the shared session.

use super::{McpClient, McpClientOptions};
use ra_core::error::Result;
use std::{collections::BTreeMap, path::PathBuf};

mod encoding;

/// Explicit process launch parameters, corresponding to `MCPServerStdioParams`.
#[derive(Clone)]
pub struct McpStdioParams {
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: Option<BTreeMap<String, String>>,
    encoding: String,
    encoding_error_handler: McpEncodingErrorHandler,
}
impl McpStdioParams {
    /// Selects an executable. Arguments are passed literally without a shell.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: vec![],
            cwd: None,
            env: None,
            encoding: "utf-8".to_owned(),
            encoding_error_handler: McpEncodingErrorHandler::Strict,
        }
    }
    /// Sets process arguments.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args = args.into_iter().collect();
        self
    }
    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    /// Overlays explicit values onto the Codex core environment allowlist.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }
    /// Selects the text codec. Defaults to UTF-8; labels are resolved before spawning.
    #[must_use]
    pub fn with_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.encoding = encoding.into();
        self
    }
    /// Selects Python's strict, ignore or replace text error behavior.
    #[must_use]
    pub const fn with_encoding_error_handler(mut self, handler: McpEncodingErrorHandler) -> Self {
        self.encoding_error_handler = handler;
        self
    }
}

/// Text codec error policy, matching Python's stdio parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpEncodingErrorHandler {
    /// Fail on malformed input or unrepresentable output.
    #[default]
    Strict,
    /// Discard malformed input and unrepresentable characters.
    Ignore,
    /// Replace malformed input with U+FFFD and unrepresentable output with a question mark.
    Replace,
}
impl McpClient {
    /// Spawns and initializes one stdio MCP server.
    pub async fn connect_stdio(
        server_name: impl Into<String>,
        params: McpStdioParams,
        options: McpClientOptions,
    ) -> Result<Self> {
        let server_name = server_name.into();
        super::validate_server_name(&server_name)?;
        let codec = encoding::TextCodec::new(&params.encoding, params.encoding_error_handler)?;
        let mut command = tokio::process::Command::new(params.command);
        command.args(params.args).kill_on_drop(true).env_clear();
        for name in DEFAULT_ENV_VARS {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(cwd) = params.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = params.env {
            command.envs(env);
        }
        let (transport, failed) = encoding::spawn(command, codec)?;
        let mut client = Self::connect(server_name, transport, options).await?;
        client.transport_failed = failed;
        Ok(client)
    }
}

// From Codex rmcp-client/utils.rs and protocol/shell_environment.rs. Explicit env overrides
// are applied afterward, preserving PATH and normal runtime settings without ambient secrets.
#[cfg(not(windows))]
const DEFAULT_ENV_VARS: &[&str] = &[
    "HOME",
    "LOGNAME",
    "PATH",
    "SHELL",
    "USER",
    "__CF_USER_TEXT_ENCODING",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "TZ",
];
#[cfg(windows)]
const DEFAULT_ENV_VARS: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SHELL",
    "COMSPEC",
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "USERNAME",
    "USERDOMAIN",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PROGRAMDATA",
    "LOCALAPPDATA",
    "APPDATA",
    "TEMP",
    "TMP",
    "TMPDIR",
    "POWERSHELL",
    "PWSH",
];

/// A reusable configured stdio server, corresponding to Python's `MCPServerStdio`.
/// Existing client handles stay bound to their original session after a reconnect.
pub struct McpServerStdio {
    name: String,
    params: McpStdioParams,
    options: McpClientOptions,
    connection: crate::server::ServerConnection,
}
impl McpServerStdio {
    /// Configures a disconnected server. The default name is `stdio: <command>`.
    #[must_use]
    pub fn new(params: McpStdioParams) -> Self {
        Self {
            name: format!("stdio: {}", params.command),
            params,
            options: McpClientOptions::default(),
            connection: crate::server::ServerConnection::default(),
        }
    }
    /// Sets the host-assigned name before connecting.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
    /// Sets shared session options before connecting.
    #[must_use]
    pub fn with_options(mut self, options: McpClientOptions) -> Self {
        self.connection
            .tools
            .configure(options.cache_tools_list, options.tool_filter.clone());
        self.options = options;
        self
    }
    /// Replaces the active filter, including before connection and across reconnects.
    pub fn set_tool_filter(&self, filter: crate::filter::ToolFilter) {
        self.connection.tools.set_filter(filter);
    }
    /// Changes cache reuse without invalidating the last successful listing.
    pub fn set_cache_tools_list(&self, enabled: bool) {
        self.connection.tools.set_cache_tools_list(enabled);
    }
    /// Returns the connected snapshot used by tool discovery. A reconnect never retargets it.
    pub fn client(&self) -> Result<McpClient> {
        self.connection.client()
    }
    /// Reports the current session's liveness without starting a process.
    pub async fn is_connected(&self) -> bool {
        self.connection.is_connected().await
    }
}
#[async_trait::async_trait]
impl crate::server::McpServer for McpServerStdio {
    fn name(&self) -> &str {
        &self.name
    }
    async fn connect(&self) -> Result<()> {
        self.connection
            .connect(
                &self.name,
                Box::pin(McpClient::connect_stdio(
                    &self.name,
                    self.params.clone(),
                    self.options.clone(),
                )),
            )
            .await
    }
    /// Always clears the session. As in Python, stdio teardown failures are logged, not raised;
    /// a retained [`McpClient`] handle still reports them from [`McpClient::cleanup`].
    async fn cleanup(&self) -> Result<()> {
        self.connection.cleanup(&self.name).await
    }
    async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>> {
        self.client()?.list_tools().await
    }
    async fn list_tools_with_context(
        &self,
        run_context: Option<&ra_core::context::RunContext>,
        agent: Option<&ra_core::context::RunAgent>,
    ) -> Result<Vec<rmcp::model::Tool>> {
        self.client()?
            .list_tools_with_context(run_context, agent)
            .await
    }
    fn cached_tools(&self) -> Option<Vec<rmcp::model::Tool>> {
        self.connection.tools.snapshot()
    }
    fn invalidate_tools_cache(&self) {
        self.connection.tools.invalidate();
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult> {
        self.client()?.call_tool(name, arguments, meta).await
    }
    async fn list_prompts(&self) -> Result<rmcp::model::ListPromptsResult> {
        self.client()?.list_prompts().await
    }
    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::GetPromptResult> {
        self.client()?.get_prompt(name, arguments).await
    }
    async fn list_resources(
        &self,
        cursor: Option<String>,
    ) -> Result<rmcp::model::ListResourcesResult> {
        self.client()?.list_resources(cursor).await
    }
    async fn list_resource_templates(
        &self,
        cursor: Option<String>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult> {
        self.client()?.list_resource_templates(cursor).await
    }
    async fn read_resource(&self, uri: &str) -> Result<rmcp::model::ReadResourceResult> {
        self.client()?.read_resource(uri).await
    }
}
