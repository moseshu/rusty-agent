//! Provider-neutral permission decisions and ordered tool rules.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    item::AgentId,
    tool::{ToolLookupKey, ToolNamespace, ToolOrigin},
};

/// The result of applying a permission policy to one action.
///
/// `Ask` is an explicit control-flow result, not a synonym for denial: the runtime turns it into
/// a pending approval record, whereas `Deny` produces a model-visible refusal without running the
/// tool. A mode and a rule use the same three values so a host cannot accidentally translate an
/// "ask" rule into a permanent denial while merging its policy.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Run the action without asking the host.
    Allow,
    /// Do not run the action.
    Deny,
    /// Suspend the run until the host explicitly approves or rejects the action.
    Ask,
}

impl PermissionDecision {
    /// Stable machine-readable identifier used by configuration and control protocols.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

impl fmt::Display for PermissionDecision {
    /// Renders the same stable spelling serde uses.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// An open, provider-neutral class assigned to an action.
///
/// Classes describe declared effects for an optional runtime policy. The core does not prescribe
/// a vocabulary, and an action with no declared class remains unclassified.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionClass(String);

impl ActionClass {
    /// Creates an action class identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Stable class identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ActionClass {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ActionClass {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ActionClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A host-owned exception to the selected permission mode.
///
/// A rule matches one model-facing tool name and, optionally, one tool namespace. A missing tool
/// name matches every tool; a missing namespace matches that name in every namespace. Rules do
/// not inspect arguments: argument-specific safety policy belongs to the relevant product or
/// sandbox, where the argument has a meaningful schema and enforcement point.
///
/// When several rules match, the runtime uses the last one. This makes an appended rule an
/// intentional override of a broad earlier rule, while retaining deterministic, serializable
/// policy order.
///
/// # Written policy and answered questions are matched differently
///
/// The wildcard reading above is right for policy a person writes down: `write_file` in a config
/// file means the capability, wherever it is served from. It is wrong for a rule minted from one
/// approval click, where the host saw a specific action and said "always" about *that* one —
/// [`Self::with_lookup_key`] pins such a rule to the exact executable, so a same-named tool in a
/// namespace the host never saw is not covered by an answer it never gave.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    decision: PermissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lookup_key: Option<ToolLookupKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejection_message: Option<String>,
}

impl PermissionRule {
    /// Creates a rule that matches every tool.
    #[must_use]
    pub const fn new(decision: PermissionDecision) -> Self {
        Self {
            decision,
            tool_name: None,
            namespace: None,
            lookup_key: None,
            agent_id: None,
            rejection_message: None,
        }
    }

    /// Restricts this rule to one model-facing tool name.
    #[must_use]
    pub fn with_tool_name(mut self, tool_name: impl Into<String>) -> Self {
        self.tool_name = Some(tool_name.into());
        self
    }

    /// Restricts this rule to one provider or host namespace.
    ///
    /// A namespace restriction can be used without a tool-name restriction to set a default for
    /// an entire remote tool family.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Pins this rule to one exact executable.
    ///
    /// A pinned rule stops being a statement about a name and becomes a statement about a
    /// specific tool: it matches that lookup key and nothing else, whatever
    /// [`Self::with_tool_name`] and [`Self::with_namespace`] also say.
    #[must_use]
    pub fn with_lookup_key(mut self, lookup_key: ToolLookupKey) -> Self {
        self.lookup_key = Some(lookup_key);
        self
    }

    /// Restricts a session answer to the agent that owned the approval.
    #[must_use]
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    /// Sets the text a call this deny rule refuses returns to the model.
    ///
    /// Only rules minted from a host rejection carry one: it is the reference's sticky rejection
    /// message, kept with an `always` rejection so later calls it refuses read the same text.
    #[must_use]
    pub(crate) fn with_rejection_message(mut self, message: Option<String>) -> Self {
        self.rejection_message = message;
        self
    }

    /// The decision this rule contributes.
    #[must_use]
    pub const fn decision(&self) -> PermissionDecision {
        self.decision
    }

    /// Model-facing tool name the rule restricts to, if any.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    /// Tool namespace the rule restricts to, if any.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Exact executable the rule is pinned to, if any.
    #[must_use]
    pub const fn lookup_key(&self) -> Option<&ToolLookupKey> {
        self.lookup_key.as_ref()
    }

    /// Agent that owns this rule when it came from a sticky approval answer.
    #[must_use]
    pub const fn agent_id(&self) -> Option<&AgentId> {
        self.agent_id.as_ref()
    }

    /// The text a call this deny rule refuses returns to the model, verbatim, if the host supplied
    /// one with the `always` rejection that minted the rule. An empty string is a message too.
    #[must_use]
    pub fn rejection_message(&self) -> Option<&str> {
        self.rejection_message.as_deref()
    }

    /// Whether this rule matches the tool being dispatched.
    ///
    /// The origin is asked as a whole rather than taken apart by the caller, because a pinned rule
    /// and an unpinned one read different parts of it. [`Tool::validate`](crate::tool::Tool::validate)
    /// holds the origin name and the schema name equal, so the name matched here is the one the
    /// model called. Agent-owned rules return false without an agent identity; use
    /// [`Self::matches_origin_for_agent`] to evaluate session rules.
    #[must_use]
    pub fn matches_origin(&self, origin: &ToolOrigin) -> bool {
        self.agent_id.is_none() && self.matches_tool_origin(origin)
    }

    /// Whether this rule matches an action and its owning agent.
    #[must_use]
    pub fn matches_origin_for_agent(&self, origin: &ToolOrigin, agent_id: &AgentId) -> bool {
        self.agent_id.as_ref().is_none_or(|owner| owner == agent_id)
            && self.matches_tool_origin(origin)
    }

    fn matches_tool_origin(&self, origin: &ToolOrigin) -> bool {
        match &self.lookup_key {
            Some(pinned) => pinned == origin.lookup_key(),
            None => {
                self.matches_tool_name(origin.name(), origin.namespace().map(ToolNamespace::as_str))
            }
        }
    }

    /// Whether this rule matches a model-facing tool identity.
    ///
    /// A rule pinned by [`Self::with_lookup_key`] never matches here: a bare name and namespace
    /// cannot show that the caller holds the exact tool the rule was written about, and answering
    /// "yes" on that evidence is how a pinned rule would silently widen back into a name rule.
    /// Agent-owned rules also return false here because a name does not prove ownership.
    /// Dispatch uses [`Self::matches_origin_for_agent`].
    #[must_use]
    pub fn matches(&self, tool_name: &str, namespace: Option<&str>) -> bool {
        self.agent_id.is_none() && self.matches_tool_name(tool_name, namespace)
    }

    fn matches_tool_name(&self, tool_name: &str, namespace: Option<&str>) -> bool {
        self.lookup_key.is_none()
            && self
                .tool_name
                .as_deref()
                .is_none_or(|name| name == tool_name)
            && self
                .namespace
                .as_deref()
                .is_none_or(|name| Some(name) == namespace)
    }
}
