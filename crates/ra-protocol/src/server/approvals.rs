//! Decisions passed from the control protocol to a coding runtime: Codex's `ReviewDecision`
//! (`protocol/src/protocol.rs`). The scope and policy payloads stay intact until the runtime
//! applies its command/path approval cache or persists the requested rule. They are not the
//! provider-neutral framework's whole-tool approvals.

use serde::{Deserialize, Serialize};

use super::protocol::{ExecPolicyAmendment, NetworkPolicyAmendment, NetworkPolicyRuleAction};

/// A coding approval decision, retaining the scope and amendments selected by the client.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// Execute this invocation.
    Approved,
    /// Execute this invocation and persist the command-prefix rule.
    ApprovedExecpolicyAmendment {
        /// The command prefix the client approved.
        proposed_execpolicy_amendment: ExecPolicyAmendment,
    },
    /// Cache approval for the requested command or paths for the rest of the session.
    ApprovedForSession,
    /// Persist approval for matching MCP calls across sessions.
    ApprovedMcpPolicyAmendment,
    /// Persist an allow or deny rule for the specified host.
    NetworkPolicyAmendment {
        /// The host and action selected by the client, including persistent denials.
        network_policy_amendment: NetworkPolicyAmendment,
    },
    /// Refuse this invocation and let the session continue.
    Denied {
        /// The model-visible explanation.
        rejection: String,
    },
    /// Automatic approval review timed out.
    TimedOut,
    /// Stop the turn until the user's next command.
    Abort,
}

impl Default for ReviewDecision {
    fn default() -> Self {
        Self::denied("denied")
    }
}

impl ReviewDecision {
    /// Refuses the invocation with the supplied explanation.
    #[must_use]
    pub fn denied(rejection: impl Into<String>) -> Self {
        Self::Denied {
            rejection: rejection.into(),
        }
    }

    /// Returns Codex's opaque label without the command, host or rejection text.
    #[must_use]
    pub const fn to_opaque_string(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ApprovedExecpolicyAmendment { .. } => "approved_with_amendment",
            Self::ApprovedForSession => "approved_for_session",
            Self::ApprovedMcpPolicyAmendment => "approved_mcp_policy_amendment",
            Self::NetworkPolicyAmendment {
                network_policy_amendment,
            } => match network_policy_amendment.action() {
                NetworkPolicyRuleAction::Allow => "approved_with_network_policy_allow",
                NetworkPolicyRuleAction::Deny => "denied_with_network_policy_deny",
            },
            Self::Denied { .. } => "denied",
            Self::TimedOut => "timed_out",
            Self::Abort => "abort",
        }
    }
}
