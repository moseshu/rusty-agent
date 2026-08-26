//! Permission-policy contracts shared by hosts, runtime, and persisted state.
//!
//! This module deliberately defines policy vocabulary rather than tool-specific enforcement.
//! The runtime matches rules and turns a decision into an interruption or refusal; sandbox
//! enforcement and UI wording remain their owners' responsibility. Keeping the vocabulary in the
//! core lets those layers agree without making the core depend on any of them.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The host-selected policy for permission evaluation.
///
/// A mode is not itself an authorization decision. A matching rule, an explicit user decision,
/// or an enforcement boundary can still allow or deny a particular action. In particular,
/// [`Self::BypassPermissions`] does not bypass sandbox or host-enforced restrictions.
///
/// The variants align with the established permission-policy vocabulary while keeping their
/// concrete evaluation in the runtime: `Plan` and `AcceptEdits`, for example, require knowledge
/// of the requested action that this provider-neutral value type does not have.
///
/// **Deliberately not ordered.** A derived `Ord` would rank the variants by declaration order,
/// which is not a permissiveness order — [`Self::BypassPermissions`], the loosest mode, would sit
/// between two stricter ones. `mode >= BypassPermissions` and "take the stricter of the two" via
/// `max` would then compile and be wrong on a security policy, and inserting a variant under
/// `#[non_exhaustive]` would silently change every such comparison. Modes are matched, not
/// compared.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Evaluate actions under the host's normal approval policy.
    #[default]
    Default,
    /// Automatically accept edits while retaining approval checks for other actions.
    AcceptEdits,
    /// Automatically approve actions unless an explicit deny rule applies.
    BypassPermissions,
    /// Restrict actions to read-only ones: planning may inspect the workspace but neither modify
    /// it nor execute commands.
    Plan,
    /// Never prompt; actions that require approval are denied.
    DontAsk,
}

impl PermissionMode {
    /// Every mode currently defined by this version of the framework.
    ///
    /// This is not an exhaustive list for downstream matching because the enum may grow.
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::AcceptEdits,
        Self::BypassPermissions,
        Self::Plan,
        Self::DontAsk,
    ];

    /// Stable machine-readable identifier used by configuration and control protocols.
    ///
    /// The label matches this type's serde representation and the established public
    /// permission-mode vocabulary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
        }
    }
}

impl fmt::Display for PermissionMode {
    /// Renders [`Self::label`], so a mode reads the same in a log line, a trace field, and on the
    /// wire. A derived `Debug` spelling would give the same value a second name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

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

/// The capability class an action declares for permission evaluation.
///
/// This is a declaration about the action's effect, not its implementation. A tool that reads a
/// file is [`Read`](Self::Read), a workspace mutation is [`Edit`](Self::Edit), and a command or
/// external side effect is [`Execute`](Self::Execute). The conservative default belongs to the
/// tool declaration and is [`Execute`](Self::Execute), so adding an unclassified tool cannot make
/// plan mode more permissive.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    /// Observes data without changing the workspace or invoking an external action.
    Read,
    /// Changes workspace state without executing a command.
    Edit,
    /// Executes a command or can cause an external side effect.
    #[default]
    Execute,
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
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    decision: PermissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
}

impl PermissionRule {
    /// Creates a rule that matches every tool.
    #[must_use]
    pub const fn new(decision: PermissionDecision) -> Self {
        Self {
            decision,
            tool_name: None,
            namespace: None,
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

    /// Whether this rule matches a model-facing tool identity.
    #[must_use]
    pub fn matches(&self, tool_name: &str, namespace: Option<&str>) -> bool {
        self.tool_name
            .as_deref()
            .is_none_or(|name| name == tool_name)
            && self
                .namespace
                .as_deref()
                .is_none_or(|name| Some(name) == namespace)
    }
}
