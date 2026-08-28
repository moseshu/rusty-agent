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

use crate::{
    compat::Unknown,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId, ToolApproval},
    tool::{ToolLookupKey, ToolNamespace, ToolOrigin},
};

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

    /// Whether this rule matches the tool being dispatched.
    ///
    /// The origin is asked as a whole rather than taken apart by the caller, because a pinned rule
    /// and an unpinned one read different parts of it. [`Tool::validate`](crate::tool::Tool::validate)
    /// holds the origin name and the schema name equal, so the name matched here is the one the
    /// model called.
    #[must_use]
    pub fn matches_origin(&self, origin: &ToolOrigin) -> bool {
        match &self.lookup_key {
            Some(pinned) => pinned == origin.lookup_key(),
            None => self.matches(origin.name(), origin.namespace().map(ToolNamespace::as_str)),
        }
    }

    /// Whether this rule matches a model-facing tool identity.
    ///
    /// A rule pinned by [`Self::with_lookup_key`] never matches here: a bare name and namespace
    /// cannot show that the caller holds the exact tool the rule was written about, and answering
    /// "yes" on that evidence is how a pinned rule would silently widen back into a name rule.
    /// Dispatch uses [`Self::matches_origin`].
    #[must_use]
    pub fn matches(&self, tool_name: &str, namespace: Option<&str>) -> bool {
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

/// Presentation and correlation data for one pending tool approval.
///
/// This is a data contract between the approval producer and a host UI. The UI may render the
/// title, display name, and description as supplied, but none of these strings authorizes an
/// action: the host returns a [`ToolApprovalDecision`] and the runtime applies that typed result.
/// Keeping the renderable data with the approval request prevents every host from inventing a
/// different prompt from a tool name and JSON arguments.
///
/// `tool_use_id` is always copied from the [`ToolApproval::call_id`] that owns this context, so a
/// host can correlate a UI response without inventing a second identifier. `agent_id` is present
/// only when a child agent requested approval, and is a different fact from
/// [`RunContext::agent_id`]: that one always names the public agent currently speaking, and a
/// handoff replaces it mid-run. The remaining optional text fields are absent when the approval
/// producer has no applicable value; a UI must not manufacture a replacement that changes the
/// meaning of a supplied field.
///
/// When a later runtime stage persists this alongside a [`ToolApproval`] record, that record's
/// schema version covers the enclosing pending-approval state. Unknown fields are retained and
/// written back rather than rejected: a newer renderer hint must survive a resume through an
/// older runtime even when that runtime cannot use the hint itself.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPermissionContext {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    suggestions: Vec<PermissionUpdate>,
    /// Deliberately without `#[serde(default)]`, unlike every field around it: a context whose
    /// correlation ID is absent must fail to load rather than deserialize into an empty `CallId`
    /// that matches no pending call.
    tool_use_id: CallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blocked_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolPermissionContext {
    /// Creates context correlated to one pending approval.
    ///
    /// This is the only constructor so the UI correlation ID has one source of truth. Callers
    /// that later load a context from storage can use [`Self::matches_approval`] before acting on
    /// it to check that the record was paired with the same pending call.
    #[must_use]
    pub fn for_approval(approval: &ToolApproval) -> Self {
        Self {
            suggestions: Vec::new(),
            tool_use_id: approval.call_id().clone(),
            agent_id: None,
            blocked_path: None,
            decision_reason: None,
            title: None,
            display_name: None,
            description: None,
            unknown: Unknown::new(),
        }
    }

    /// Attaches host-suggested, typed permission updates for the user to accept or ignore.
    #[must_use]
    pub fn with_suggestions(
        mut self,
        suggestions: impl IntoIterator<Item = PermissionUpdate>,
    ) -> Self {
        self.suggestions = suggestions.into_iter().collect();
        self
    }

    /// Identifies the exact tool call awaiting approval.
    ///
    /// There is deliberately no setter: this value is derived by [`Self::for_approval`] from the
    /// pending record's canonical call ID.
    #[must_use]
    pub const fn tool_use_id(&self) -> &CallId {
        &self.tool_use_id
    }

    /// Whether this context belongs to the given pending approval.
    #[must_use]
    pub fn matches_approval(&self, approval: &ToolApproval) -> bool {
        self.tool_use_id == *approval.call_id()
    }

    /// Identifies the child agent that requested approval.
    #[must_use]
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    /// Records the path that was blocked, when the approval concerns a filesystem boundary.
    #[must_use]
    pub fn with_blocked_path(mut self, blocked_path: impl Into<String>) -> Self {
        self.blocked_path = Some(blocked_path.into());
        self
    }

    /// Records why the approval producer requested a decision.
    #[must_use]
    pub fn with_decision_reason(mut self, decision_reason: impl Into<String>) -> Self {
        self.decision_reason = Some(decision_reason.into());
        self
    }

    /// Sets the complete primary prompt text supplied to the UI.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Sets the compact action name supplied to the UI.
    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    /// Sets the human-readable subtitle supplied to the UI.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Typed policy updates suggested for this approval.
    #[must_use]
    pub fn suggestions(&self) -> &[PermissionUpdate] {
        &self.suggestions
    }

    /// ID of the child agent that requested approval, when applicable.
    #[must_use]
    pub const fn agent_id(&self) -> Option<&AgentId> {
        self.agent_id.as_ref()
    }

    /// Blocked filesystem path, when applicable.
    #[must_use]
    pub fn blocked_path(&self) -> Option<&str> {
        self.blocked_path.as_deref()
    }

    /// Reason the approval producer requested a decision, when supplied.
    #[must_use]
    pub fn decision_reason(&self) -> Option<&str> {
        self.decision_reason.as_deref()
    }

    /// Primary permission prompt text, when supplied.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Compact action name for permission UI controls, when supplied.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Human-readable permission UI subtitle, when supplied.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Unknown fields retained while reading a newer approval context.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
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
/// The handler receives the exact [`ToolApproval`] record that caused the interruption, its
/// directly renderable [`ToolPermissionContext`], and the same [`RunContext`] dynamic prompts and
/// tools receive.
///
/// A handler returns a typed core value, so the host or UI collects a choice without interpreting
/// display text as permission semantics. The paused-run logic and persistence remain
/// responsibilities of their respective runtime and state layers.
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
        permission: &ToolPermissionContext,
        context: &RunContext,
    ) -> Result<ToolApprovalDecision>;
}
