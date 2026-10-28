// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Shared product configuration and legacy policy wire values.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Codex's approval policy. These are independent of sandbox permissions.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AskForApproval {
    #[serde(rename = "untrusted")]
    /// Requests approval unless an explicit execution rule trusts the command.
    UnlessTrusted,
    #[default]
    /// Requests approval when execution requires a restricted filesystem boundary.
    OnRequest,
    /// Selects which approval categories may prompt.
    Granular {
        /// Whether sandbox escalation prompts are allowed.
        sandbox_approval: bool,
        /// Whether execution-rule prompts are allowed.
        rules: bool,
        #[serde(default)]
        /// Whether skill approval prompts are allowed.
        skill_approval: bool,
        #[serde(default)]
        /// Whether permission requests may prompt.
        request_permissions: bool,
        /// Whether MCP elicitations may prompt.
        mcp_elicitations: bool,
    },
    /// Never prompts; it does not grant sandbox or tool permissions.
    Never,
}

/// Codex's approval reviewer, including its legacy alias.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ApprovalsReviewer {
    #[default]
    #[serde(rename = "user")]
    /// Routes approval review to the user.
    User,
    #[serde(rename = "auto_review", alias = "guardian_subagent")]
    /// Routes approval review to the configured automatic reviewer. Codex's reviewer, Guardian,
    /// is not ported: the server refuses this reviewer wherever a request sets it.
    AutoReview,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
/// Network access under an external sandbox.
pub enum NetworkAccess {
    #[default]
    /// Network access is restricted.
    Restricted,
    /// Network access is enabled.
    Enabled,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
/// Codex filesystem and network policy, enforced by the embedding product.
pub enum SandboxPolicy {
    /// Requests unrestricted execution.
    DangerFullAccess,
    #[serde(rename_all = "camelCase")]
    /// Allows reads while confining writes.
    ReadOnly {
        #[serde(default)]
        /// Whether network access is available.
        network_access: bool,
    },
    #[serde(rename_all = "camelCase")]
    /// Delegates confinement to an external execution environment.
    ExternalSandbox {
        #[serde(default)]
        /// Whether network access is available.
        network_access: NetworkAccess,
    },
    #[serde(rename_all = "camelCase")]
    /// Allows writes to the workspace and selected additional roots.
    WorkspaceWrite {
        #[serde(default)]
        /// Additional absolute paths that may be written.
        writable_roots: Vec<PathBuf>,
        #[serde(default)]
        /// Whether network access is available.
        network_access: bool,
        #[serde(default)]
        /// Excludes the directory named by TMPDIR from writable roots.
        exclude_tmpdir_env_var: bool,
        #[serde(default)]
        /// Excludes /tmp from writable roots.
        exclude_slash_tmp: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum SandboxPolicyDeserialize {
    DangerFullAccess,
    #[serde(rename_all = "camelCase")]
    ReadOnly {
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        access: Option<LegacyReadOnlyAccess>,
    },
    #[serde(rename_all = "camelCase")]
    ExternalSandbox {
        #[serde(default)]
        network_access: NetworkAccess,
    },
    #[serde(rename_all = "camelCase")]
    WorkspaceWrite {
        #[serde(default)]
        writable_roots: Vec<PathBuf>,
        #[serde(default)]
        read_only_access: Option<LegacyReadOnlyAccess>,
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum LegacyReadOnlyAccess {
    FullAccess,
    Restricted,
}

impl<'de> Deserialize<'de> for SandboxPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match SandboxPolicyDeserialize::deserialize(deserializer)? {
            SandboxPolicyDeserialize::DangerFullAccess => Ok(SandboxPolicy::DangerFullAccess),
            SandboxPolicyDeserialize::ReadOnly {
                network_access,
                access,
            } => {
                if matches!(access, Some(LegacyReadOnlyAccess::Restricted)) {
                    return Err(serde::de::Error::custom(
                        "readOnly.access is no longer supported; use permissionProfile for restricted reads",
                    ));
                }
                Ok(SandboxPolicy::ReadOnly { network_access })
            }
            SandboxPolicyDeserialize::ExternalSandbox { network_access } => {
                Ok(SandboxPolicy::ExternalSandbox { network_access })
            }
            SandboxPolicyDeserialize::WorkspaceWrite {
                writable_roots,
                read_only_access,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            } => {
                if matches!(read_only_access, Some(LegacyReadOnlyAccess::Restricted)) {
                    return Err(serde::de::Error::custom(
                        "workspaceWrite.readOnlyAccess is no longer supported; use permissionProfile for restricted reads",
                    ));
                }
                Ok(SandboxPolicy::WorkspaceWrite {
                    writable_roots,
                    network_access,
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                })
            }
        }
    }
}

/// Codex's coarse sandbox selection for a new, resumed or forked thread: `SandboxMode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Reads anywhere, writes nowhere, no network.
    #[default]
    ReadOnly,
    /// Writes in the workspace, no network.
    WorkspaceWrite,
    /// Unrestricted.
    DangerFullAccess,
}

impl SandboxMode {
    /// The policy the mode selects, with Codex's defaults for what the mode leaves open: no
    /// network and no writable roots beyond the workspace.
    #[must_use]
    pub fn to_policy(self) -> SandboxPolicy {
        match self {
            Self::ReadOnly => SandboxPolicy::ReadOnly {
                network_access: false,
            },
            Self::WorkspaceWrite => SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            Self::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        }
    }
}
