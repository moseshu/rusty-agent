// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
//! Model-owned Guardian coverage. Missing policy preserves legacy behavior;
//! unknown modes retain synchronous review and never enable the fast path.

use serde::Deserialize;
use serde::Serialize;

use super::ModelInfo;

/// How Guardian handles an action when the user selects automatic approval.
#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuardianReviewMode {
    #[default]
    Disabled,
    Synchronous,
    /// Use a current low-risk score; otherwise run synchronous review.
    Adaptive,
    #[serde(other)]
    Unknown,
}

/// How actions outside adaptive coverage affect cached classification evidence.
#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuardianUnscoredAction {
    Ignore,
    AgeScore,
    #[default]
    #[serde(other)]
    InvalidateScore,
}

/// A complete model policy. Omitted scopes are disabled; unknown fields are ignored.
/// Code Mode wrappers have no approval scope; their nested tools follow this policy.
#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct GuardianModelPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<GuardianReviewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<GuardianReviewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_changes: Option<GuardianReviewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<GuardianReviewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<GuardianReviewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<GuardianReviewMode>,
    /// Coverage for tools without an approval category.
    #[serde(default)]
    pub other_tools: GuardianReviewMode,
    #[serde(default)]
    pub unscored_action: GuardianUnscoredAction,
    /// Omission retains the existing allowance for adaptive computer use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_cua_call: Option<bool>,
    /// Whether adaptive shell coverage includes ordinary sandboxed commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandboxed_exec_commands: Option<bool>,
}

impl GuardianModelPolicy {
    #[must_use]
    pub fn review_mode(&self, scope: GuardianScope) -> GuardianReviewMode {
        match scope {
            GuardianScope::ComputerUse => self.computer_use,
            GuardianScope::Shell => self.shell,
            GuardianScope::FileChanges => self.file_changes,
            GuardianScope::Mcp => self.mcp,
            GuardianScope::Network => self.network,
            GuardianScope::Permissions => self.permissions,
        }
        .unwrap_or(GuardianReviewMode::Disabled)
    }

    #[must_use]
    pub fn scoring_enabled(&self) -> bool {
        [
            self.computer_use,
            self.shell,
            self.file_changes,
            self.mcp,
            self.network,
            self.permissions,
        ]
        .contains(&Some(GuardianReviewMode::Adaptive))
    }

    pub fn disable_scoring(&mut self) {
        for mode in [
            &mut self.computer_use,
            &mut self.shell,
            &mut self.file_changes,
            &mut self.mcp,
            &mut self.network,
            &mut self.permissions,
        ] {
            if *mode == Some(GuardianReviewMode::Adaptive) {
                *mode = Some(GuardianReviewMode::Synchronous);
            }
        }
        self.other_tools = GuardianReviewMode::Synchronous;
    }

    #[must_use]
    pub fn allows_initial_cua_call(&self) -> bool {
        self.initial_cua_call
            .unwrap_or(self.computer_use == Some(GuardianReviewMode::Adaptive))
    }
}

/// Approval categories understood by this client. Future catalog keys are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardianScope {
    ComputerUse,
    Shell,
    FileChanges,
    Mcp,
    Network,
    Permissions,
}

impl ModelInfo {
    /// An absent map uses legacy config; an omitted scope in a supplied map is disabled.
    #[must_use]
    pub fn guardian_review_mode(&self, scope: GuardianScope) -> Option<GuardianReviewMode> {
        self.guardian
            .as_ref()
            .map(|policy| policy.review_mode(scope))
    }

    /// The legacy metadata bit remains the transport understood by older CUA servers.
    #[must_use]
    pub fn computer_use_review_required(&self) -> bool {
        self.guardian_review_mode(GuardianScope::ComputerUse)
            .map_or(self.node_repl_auto_review_required, |mode| {
                mode != GuardianReviewMode::Disabled
            })
    }
}
