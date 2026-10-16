// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Child-process transport; protocol operations stay in the shared session.

use super::{McpClient, McpClientOptions, safe_error};
use ra_core::error::{Result, ToolErrorKind};
use std::{collections::BTreeMap, path::PathBuf};

/// Explicit process launch parameters, corresponding to `MCPServerStdioParams`.
#[derive(Clone)]
pub struct McpStdioParams {
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: Option<BTreeMap<String, String>>,
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
    /// Overlays explicit values onto the MCP SDK default environment allowlist.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }
}
impl McpClient {
    /// Spawns and initializes one stdio MCP server.
    pub async fn connect_stdio(
        server_name: impl Into<String>,
        params: McpStdioParams,
        options: McpClientOptions,
    ) -> Result<Self> {
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
        let transport = rmcp::transport::TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| safe_error(ToolErrorKind::ExecutionFailed, "MCP process spawn failed"))?
            .0;
        Self::connect(server_name, transport, options).await
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
