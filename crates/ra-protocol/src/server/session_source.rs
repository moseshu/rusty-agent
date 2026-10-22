// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Server-level session sources and startup parsing. The CLI field is reusable by embedding
//! executables because this crate exposes a server library rather than an app-server binary.

use ra_core::{agent::control::AgentPath, session::SessionId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Codex's core session source, kept in the server adapter rather than the framework core.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    /// An interactive CLI session.
    Cli,
    /// A VS Code session, also the upstream server default.
    #[default]
    VSCode,
    /// A noninteractive execution session.
    Exec,
    /// An app-server session; its API projection is `appServer`.
    Mcp,
    /// An explicitly selected product source.
    Custom(String),
    /// An internal product session, projected to API `unknown`.
    Internal(InternalSessionSource),
    /// A subagent session.
    SubAgent(SubAgentSource),
    /// Missing or unrecognized source metadata.
    #[serde(other)]
    Unknown,
}

/// Codex's internal session sources.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalSessionSource {
    /// Memory consolidation.
    MemoryConsolidation,
    /// Guardian review.
    Guardian,
}

/// Codex's subagent sources, with opaque framework session IDs for spawned parents.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentSource {
    /// A review agent.
    Review,
    /// A history compaction agent.
    Compact,
    /// A thread spawned by its parent agent.
    ThreadSpawn {
        /// The parent's opaque session ID, replacing Codex's UUID `ThreadId`.
        parent_thread_id: SessionId,
        /// The thread's depth below its root.
        depth: i32,
        /// The agent's path within its tree.
        #[serde(default)]
        agent_path: Option<AgentPath>,
        /// The product's nickname for the agent.
        #[serde(default)]
        agent_nickname: Option<String>,
        /// The registered agent role.
        #[serde(default, alias = "agent_type")]
        agent_role: Option<String>,
    },
    /// A memory consolidation agent.
    MemoryConsolidation,
    /// Another product-defined subagent.
    Other(String),
}

impl SessionSource {
    /// Whether this source represents an internal session or a subagent, as in Codex.
    #[must_use]
    pub const fn is_non_root_agent(&self) -> bool {
        matches!(self, Self::Internal(_) | Self::SubAgent(_))
    }

    /// Parses the server startup argument using Codex's aliases and normalization.
    ///
    /// # Errors
    /// Returns `session source must not be empty` for empty or whitespace-only input.
    pub fn from_startup_arg(value: &str) -> Result<Self, &'static str> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err("session source must not be empty");
        }
        let normalized = trimmed.to_ascii_lowercase();
        Ok(match normalized.as_str() {
            "cli" => Self::Cli,
            "vscode" => Self::VSCode,
            "exec" => Self::Exec,
            "mcp" | "appserver" | "app-server" | "app_server" => Self::Mcp,
            "unknown" => Self::Unknown,
            _ => Self::Custom(normalized),
        })
    }

    pub(crate) fn metadata_value(&self) -> Value {
        json!(self)
    }

    pub(crate) fn api_value(&self) -> Value {
        match self {
            Self::Mcp => json!("appServer"),
            Self::Internal(_) => json!("unknown"),
            Self::SubAgent(source) => json!({"subAgent": source}),
            Self::Cli | Self::VSCode | Self::Exec | Self::Custom(_) | Self::Unknown => {
                self.metadata_value()
            }
        }
    }

    pub(crate) fn from_metadata(value: Option<&Value>) -> Self {
        value
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or(Self::Unknown)
    }
}

/// The upstream `--session-source` field, flattenable into an embedding executable's CLI.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct SessionSourceArgs {
    /// Session source stamped into newly created thread metadata.
    #[arg(long, value_name = "SOURCE", default_value = "vscode", value_parser = SessionSource::from_startup_arg)]
    session_source: SessionSource,
}

impl SessionSourceArgs {
    /// The server-level source selected by startup arguments, defaulting to VS Code.
    #[must_use]
    pub const fn session_source(&self) -> &SessionSource {
        &self.session_source
    }
}
