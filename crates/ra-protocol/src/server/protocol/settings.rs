//! Codex thread policy and live-settings payloads. Policy execution belongs to the host adapter.

#![allow(
    clippy::option_option,
    reason = "Codex distinguishes omitted, null and supplied service tiers"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Codex's approval policy. These are independent of sandbox permissions.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
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

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
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

/// The product policy fields shared by thread updates and turn starts.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPolicyOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approval_policy: Option<AskForApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permissions: Option<String>,
}

impl ThreadPolicyOverrides {
    /// Changes no policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the approval prompt policy.
    #[must_use]
    pub const fn with_approval_policy(mut self, policy: AskForApproval) -> Self {
        self.approval_policy = Some(policy);
        self
    }

    /// Sets the approval reviewer.
    #[must_use]
    pub const fn with_approvals_reviewer(mut self, reviewer: ApprovalsReviewer) -> Self {
        self.approvals_reviewer = Some(reviewer);
        self
    }

    /// Sets the sandbox policy; it cannot be combined with a permissions profile.
    #[must_use]
    pub fn with_sandbox_policy(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox_policy = Some(policy);
        self
    }

    /// Selects a configured permissions profile by its identifier.
    #[must_use]
    pub fn with_permissions(mut self, profile: impl Into<String>) -> Self {
        self.permissions = Some(profile.into());
        self
    }

    /// Approval prompt policy; `None` leaves it unchanged.
    #[must_use]
    pub const fn approval_policy(&self) -> Option<AskForApproval> {
        self.approval_policy
    }

    /// Approval reviewer; `None` leaves it unchanged.
    #[must_use]
    pub const fn approvals_reviewer(&self) -> Option<ApprovalsReviewer> {
        self.approvals_reviewer
    }

    /// Sandbox policy; mutually exclusive with a permissions profile.
    #[must_use]
    pub const fn sandbox_policy(&self) -> Option<&SandboxPolicy> {
        self.sandbox_policy.as_ref()
    }

    /// Configured permission profile identifier.
    #[must_use]
    pub fn permissions(&self) -> Option<&str> {
        self.permissions.as_deref()
    }

    /// Whether no policy fields were supplied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.approval_policy.is_none()
            && self.approvals_reviewer.is_none()
            && self.sandbox_policy.is_none()
            && self.permissions.is_none()
    }
    /// Validates policy conflicts and absolute writable roots.
    pub fn validate(&self) -> Result<(), String> {
        if self.sandbox_policy.is_some() && self.permissions.is_some() {
            return Err("`permissions` cannot be combined with `sandboxPolicy`".into());
        }
        if let Some(SandboxPolicy::WorkspaceWrite { writable_roots, .. }) = &self.sandbox_policy
            && writable_roots.iter().any(|root| !root.is_absolute())
        {
            return Err("sandboxPolicy.writableRoots must be absolute".into());
        }
        Ok(())
    }
}

/// Experimental settings for one running turn. Unsupported members are rejected by Codex.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnSettingsUpdateParams {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<ReasoningSummary>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) service_tier: Option<Option<String>>,
}

impl TurnSettingsUpdateParams {
    /// Creates an update targeting one turn.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            ..Self::default()
        }
    }

    /// Routes the turn's later approvals to `reviewer`.
    #[must_use]
    pub const fn with_approvals_reviewer(mut self, reviewer: ApprovalsReviewer) -> Self {
        self.approvals_reviewer = Some(reviewer);
        self
    }

    /// Runs the turn's later steps on `model`.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Runs the turn's later steps with reasoning `effort`, by its wire name.
    #[must_use]
    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    /// Requests a provider-side reasoning summary preference.
    #[must_use]
    pub const fn with_summary(mut self, summary: ReasoningSummary) -> Self {
        self.summary = Some(summary);
        self
    }

    /// Sets the service tier, or clears it with `None`.
    #[must_use]
    pub fn with_service_tier(mut self, tier: Option<String>) -> Self {
        self.service_tier = Some(tier);
        self
    }

    /// Loaded thread identifier.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Active turn identifier.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// Approval reviewer; `None` leaves it unchanged.
    #[must_use]
    pub const fn approvals_reviewer(&self) -> Option<ApprovalsReviewer> {
        self.approvals_reviewer
    }

    /// Model selector; `None` leaves it unchanged.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Reasoning effort by its wire name; `None` leaves it unchanged.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Provider-side reasoning summary preference.
    #[must_use]
    pub const fn summary(&self) -> Option<ReasoningSummary> {
        self.summary
    }

    /// `None` leaves the tier unchanged; `Some(None)` clears it.
    #[must_use]
    pub fn service_tier(&self) -> Option<Option<&str>> {
        self.service_tier.as_ref().map(Option::as_deref)
    }
}

pub(super) fn double_option<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(de).map(Some)
}

/// Codex's provider-side reasoning-summary preference.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReasoningSummary {
    /// Uses the provider default summary behavior.
    Auto,
    /// Requests a concise reasoning summary.
    Concise,
    /// Requests a detailed reasoning summary.
    Detailed,
    /// Requests no reasoning summary.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
/// Whether a live turn accepted the settings publication.
pub enum TurnSettingsUpdateStatus {
    /// The target accepted the update for subsequent model steps.
    Applied,
    /// The target ended, was cancelled, or no longer matches.
    TargetUnavailable,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Acknowledgement for a live turn settings update.
pub struct TurnSettingsUpdateResponse {
    pub(crate) status: TurnSettingsUpdateStatus,
}

impl TurnSettingsUpdateResponse {
    /// Result of attempting the update.
    #[must_use]
    pub const fn status(&self) -> TurnSettingsUpdateStatus {
        self.status
    }
}

/// Notification method for an effective persistent settings snapshot.
pub const THREAD_SETTINGS_UPDATED_METHOD: &str = "thread/settings/updated";

/// Codex's notification shape. Host-owned policy fields remain in the wire record rather than
/// entering provider-neutral runtime types.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettingsUpdatedNotification {
    pub(crate) thread_id: String,
    pub(crate) thread_settings: Value,
}

impl ThreadSettingsUpdatedNotification {
    /// Loaded thread identifier.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Effective settings snapshot provided by the host and runtime.
    #[must_use]
    pub const fn thread_settings(&self) -> &Value {
        &self.thread_settings
    }
}
