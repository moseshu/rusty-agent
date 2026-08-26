//! Permission-policy contracts shared by hosts, runtime, and persisted state.
//!
//! This module deliberately defines policy vocabulary rather than tool-specific enforcement.
//! The runtime matches rules and turns a decision into an interruption or refusal; sandbox
//! enforcement and UI wording remain their owners' responsibility. Keeping the vocabulary in the
//! core lets those layers agree without making the core depend on any of them.
//!
//! # Wire spelling
//!
//! One payload here mixes two casings, and the split is a rule rather than an accident:
//!
//! - **Field names are this crate's own**, so they use `snake_case` like every other core type:
//!   `updated_input`, `updated_permissions`, `tool_name`.
//! - **Label values that reproduce the established public permission vocabulary keep that
//!   vocabulary's spelling verbatim**: `acceptEdits`, `bypassPermissions`, `localSettings`,
//!   `addRules`. These are the values a host already has in its configuration and control
//!   protocol; respelling them would silently reject a policy a user has been writing for
//!   years, which is the failure this module refuses everywhere else.
//!
//! Values this crate invented are unaffected, because they are single words either way
//! (`allow`, `deny`, `ask`, `read`, `edit`, `execute`, `session`).
//!
//! The alternative — camelCase throughout, matching the reference SDK's `updatedInput` — would
//! also be coherent, but it would respell the already-published [`PermissionRule`] fields for a
//! consistency that only a reader, not a program, can observe. New types in this module follow
//! the rule above rather than the nearest neighbour.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{context::RunContext, error::Result, item::ToolApproval};

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

/// Where a host should persist a permission-policy update.
///
/// The core names the destination but does not implement its storage. In particular, a host must
/// not silently choose a destination on behalf of a caller: `Session` and `UserSettings` have
/// materially different lifetimes and scopes.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// Applies only to the current session or run.
    Session,
    /// Applies to settings local to the current workspace.
    LocalSettings,
    /// Applies to settings shared by the current project.
    ProjectSettings,
    /// Applies to the user's settings across projects.
    UserSettings,
}

impl PermissionUpdateDestination {
    /// Stable machine-readable identifier used by control protocols.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::LocalSettings => "localSettings",
            Self::ProjectSettings => "projectSettings",
            Self::UserSettings => "userSettings",
        }
    }
}

impl fmt::Display for PermissionUpdateDestination {
    /// Renders the same stable spelling serde uses.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A requested mutation of host-owned permission policy.
///
/// This intentionally uses [`PermissionRule`] rather than the upstream provider's tool-name and
/// matcher fields. The core's rule type is provider-neutral and has already narrowed matching to
/// tool names and namespaces; argument-specific rules belong at the tool or sandbox boundary.
///
/// Every update requires an explicit destination. The reference SDK permits an omitted
/// destination and lets its process choose one, but this core cannot safely infer whether an
/// approval should survive the session. Hosts own persistence and apply these values.
///
/// Each variant is `#[non_exhaustive]` and built through this type's constructors. A permission
/// update is expected to grow qualifiers — a rule scope, an expiry — and a literally constructed
/// variant would turn each one into a breaking change for every host, which is the same reason
/// public structs in this crate keep their fields private.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum PermissionUpdate {
    /// Appends rules after the existing ordered rule set.
    #[non_exhaustive]
    AddRules {
        /// Destination that owns the changed rule set.
        destination: PermissionUpdateDestination,
        /// Rules to append. Later matching rules take precedence.
        rules: Vec<PermissionRule>,
    },
    /// Replaces the destination's complete ordered rule set.
    #[non_exhaustive]
    ReplaceRules {
        /// Destination that owns the changed rule set.
        destination: PermissionUpdateDestination,
        /// Complete replacement rule set. An empty list clears all rules.
        rules: Vec<PermissionRule>,
    },
    /// Removes exact rules from the destination's ordered rule set.
    #[non_exhaustive]
    RemoveRules {
        /// Destination that owns the changed rule set.
        destination: PermissionUpdateDestination,
        /// Exact rules to remove.
        rules: Vec<PermissionRule>,
    },
    /// Changes the permission mode at the requested destination.
    #[non_exhaustive]
    SetMode {
        /// Destination that owns the changed mode.
        destination: PermissionUpdateDestination,
        /// Mode to store at that destination.
        mode: PermissionMode,
    },
}

impl PermissionUpdate {
    /// Appends rules after the destination's existing ordered rule set.
    #[must_use]
    pub fn add_rules(
        destination: PermissionUpdateDestination,
        rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        Self::AddRules {
            destination,
            rules: rules.into_iter().collect(),
        }
    }

    /// Replaces the destination's complete ordered rule set.
    #[must_use]
    pub fn replace_rules(
        destination: PermissionUpdateDestination,
        rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        Self::ReplaceRules {
            destination,
            rules: rules.into_iter().collect(),
        }
    }

    /// Removes exact rules from the destination's ordered rule set.
    #[must_use]
    pub fn remove_rules(
        destination: PermissionUpdateDestination,
        rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        Self::RemoveRules {
            destination,
            rules: rules.into_iter().collect(),
        }
    }

    /// Changes the permission mode at the requested destination.
    #[must_use]
    pub const fn set_mode(destination: PermissionUpdateDestination, mode: PermissionMode) -> Self {
        Self::SetMode { destination, mode }
    }

    /// Destination whose policy the update changes.
    #[must_use]
    pub const fn destination(&self) -> PermissionUpdateDestination {
        match self {
            Self::AddRules { destination, .. }
            | Self::ReplaceRules { destination, .. }
            | Self::RemoveRules { destination, .. }
            | Self::SetMode { destination, .. } => *destination,
        }
    }
}

/// Whether a host preserved a call's input or supplied a replacement.
///
/// This stays private because it exists solely to distinguish a missing `updated_input` field
/// from an explicit JSON `null`. `Option<Value>` cannot preserve that distinction through serde:
/// both forms deserialize as `None`.
#[derive(Debug, Clone, Default, PartialEq)]
enum UpdatedToolInput {
    #[default]
    Original,
    Replacement(Value),
}

impl UpdatedToolInput {
    const fn is_original(&self) -> bool {
        matches!(self, Self::Original)
    }

    const fn as_ref(&self) -> Option<&Value> {
        match self {
            Self::Original => None,
            Self::Replacement(value) => Some(value),
        }
    }
}

fn serialize_updated_tool_input<S>(
    input: &UpdatedToolInput,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match input {
        UpdatedToolInput::Original => serializer.serialize_none(),
        UpdatedToolInput::Replacement(value) => value.serialize(serializer),
    }
}

fn deserialize_updated_tool_input<'de, D>(
    deserializer: D,
) -> std::result::Result<UpdatedToolInput, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(UpdatedToolInput::Replacement)
}

/// The approving answer to a pending tool approval.
///
/// Carrying the approval's payload in its own type is what makes an impossible answer impossible
/// to write: policy updates ride along with an approval, and only an approval, so a host cannot
/// build "deny, and also add this rule" and have the rule quietly disappear.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolApprovalAllow {
    #[serde(
        default,
        skip_serializing_if = "UpdatedToolInput::is_original",
        serialize_with = "serialize_updated_tool_input",
        deserialize_with = "deserialize_updated_tool_input"
    )]
    updated_input: UpdatedToolInput,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    updated_permissions: Vec<PermissionUpdate>,
}

impl ToolApprovalAllow {
    /// Creates an approval that preserves the requested input and changes no policy.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            updated_input: UpdatedToolInput::Original,
            updated_permissions: Vec::new(),
        }
    }

    /// Replaces the requested input for this one invocation.
    ///
    /// An explicit JSON `null` is a replacement just like any other JSON value. The private
    /// representation distinguishes it from an absent field, which means a host never has a
    /// requested replacement silently changed back to the original input.
    #[must_use]
    pub fn with_updated_input(mut self, updated_input: Value) -> Self {
        self.updated_input = UpdatedToolInput::Replacement(updated_input);
        self
    }

    /// Adds policy changes to apply alongside this approval.
    #[must_use]
    pub fn with_updated_permissions(
        mut self,
        updated_permissions: impl IntoIterator<Item = PermissionUpdate>,
    ) -> Self {
        self.updated_permissions.extend(updated_permissions);
        self
    }

    /// Replacement input for this invocation, if the host supplied one.
    #[must_use]
    pub const fn updated_input(&self) -> Option<&Value> {
        self.updated_input.as_ref()
    }

    /// Policy updates requested alongside the approval.
    #[must_use]
    pub fn updated_permissions(&self) -> &[PermissionUpdate] {
        &self.updated_permissions
    }
}

/// The refusing answer to a pending tool approval.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolApprovalDeny {
    message: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    interrupt: bool,
}

impl ToolApprovalDeny {
    /// Creates a refusal that leaves the rest of the run active.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            interrupt: false,
        }
    }

    /// Requests termination of the current run after this refusal.
    #[must_use]
    pub const fn with_interrupt(mut self, interrupt: bool) -> Self {
        self.interrupt = interrupt;
        self
    }

    /// Model-visible explanation for the refusal.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the host also requests termination of the current run.
    #[must_use]
    pub const fn interrupts_run(&self) -> bool {
        self.interrupt
    }
}

/// A host's answer to one pending tool approval.
///
/// This is deliberately separate from [`PermissionDecision`]. The latter is the small,
/// serializable vocabulary used by rules and mode evaluation; this value is a one-time host reply
/// that can rewrite an invocation, request policy changes, or stop the run. Combining them would
/// change the rule wire format and let a transient answer leak into persistent policy.
///
/// Each branch owns its payload type, so the builders that belong to one branch cannot be reached
/// from the other. A single enum with shared builders would let `deny(..).with_updated_permissions(..)`
/// compile and then discard a security policy the user asked for; the compiler rejects it here.
/// The consequence is worth stating: **a refusal cannot carry policy updates.** When "deny, and do
/// not ask again" becomes reachable, it has to arrive as a field on [`ToolApprovalDeny`] rather
/// than by reusing the approval's, which is why that type is `#[non_exhaustive]` with private
/// fields.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum ToolApprovalDecision {
    /// Permit the call, optionally after replacing its parsed JSON input and updating policy.
    Allow(ToolApprovalAllow),
    /// Refuse the call, optionally ending the entire run rather than continuing with a refusal.
    Deny(ToolApprovalDeny),
}

impl ToolApprovalDecision {
    /// Creates an approval that preserves the requested input and policy.
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow(ToolApprovalAllow::new())
    }

    /// Creates an approval that replaces the requested input for this invocation only.
    #[must_use]
    pub fn allow_with_input(updated_input: Value) -> Self {
        Self::Allow(ToolApprovalAllow::new().with_updated_input(updated_input))
    }

    /// Creates a refusal that leaves the rest of the run active.
    #[must_use]
    pub fn deny(message: impl Into<String>) -> Self {
        Self::Deny(ToolApprovalDeny::new(message))
    }

    /// Stable machine-readable identifier used by control protocols.
    ///
    /// This is the `behavior` tag serde writes, so a decision reads the same in a log line and on
    /// the wire. It deliberately says nothing about the payload.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Allow(_) => "allow",
            Self::Deny(_) => "deny",
        }
    }

    /// Whether this decision permits execution.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow(_))
    }

    /// Replacement input for an allowed call, if the host supplied one.
    #[must_use]
    pub const fn updated_input(&self) -> Option<&Value> {
        match self {
            Self::Allow(allow) => allow.updated_input(),
            Self::Deny(_) => None,
        }
    }

    /// Policy updates requested by an approval.
    #[must_use]
    pub fn updated_permissions(&self) -> &[PermissionUpdate] {
        match self {
            Self::Allow(allow) => allow.updated_permissions(),
            Self::Deny(_) => &[],
        }
    }

    /// Model-visible refusal text, if this decision denies the call.
    #[must_use]
    pub fn denial_message(&self) -> Option<&str> {
        match self {
            Self::Allow(_) => None,
            Self::Deny(deny) => Some(deny.message()),
        }
    }

    /// Whether this decision asks the runtime to terminate the run.
    #[must_use]
    pub const fn interrupts_run(&self) -> bool {
        match self {
            Self::Allow(_) => false,
            Self::Deny(deny) => deny.interrupts_run(),
        }
    }
}

impl From<ToolApprovalAllow> for ToolApprovalDecision {
    fn from(allow: ToolApprovalAllow) -> Self {
        Self::Allow(allow)
    }
}

impl From<ToolApprovalDeny> for ToolApprovalDecision {
    fn from(deny: ToolApprovalDeny) -> Self {
        Self::Deny(deny)
    }
}

impl fmt::Display for ToolApprovalDecision {
    /// Renders [`Self::label`] — the behavior, not the refusal message, which is model-visible
    /// text rather than a stable identifier.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Resolves one pending tool approval from the live run context.
///
/// The handler receives the exact [`ToolApproval`] record that caused the interruption and the
/// same [`RunContext`] dynamic prompts and tools receive. It returns a typed core value, so the
/// host or UI may render and collect a choice without reinterpreting display text as permission
/// semantics. The paused-run logic and persistence remain responsibilities of their respective
/// runtime and state layers.
#[async_trait]
pub trait ToolApprovalHandler: Send + Sync + 'static {
    /// Produces the host's decision for this pending approval.
    ///
    /// # Errors
    ///
    /// An `Err` is a failure to obtain a decision — a broken UI channel, a timed-out prompt — and
    /// **never an answer**. The call does not run and the error propagates; the runtime must not
    /// read it as an approval, and must not quietly turn it into a model-visible refusal either,
    /// because a model that sees "denied" will try a different route while the host still believes
    /// it was asked. A handler that wants the model to see a refusal returns
    /// [`ToolApprovalDecision::deny`].
    async fn decide(
        &self,
        approval: &ToolApproval,
        context: &RunContext,
    ) -> Result<ToolApprovalDecision>;
}
