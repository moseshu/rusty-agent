//! Runtime permission evaluation for tool dispatch.
//!
//! The core owns the provider-neutral approval and rule vocabulary. This module preserves tool
//! approval behavior by default and optionally applies open action-class policy before it.

use std::{collections::BTreeSet, sync::Arc};

use ra_core::{
    item::AgentId,
    permission::{ActionClass, PermissionDecision, PermissionRule},
    tool::ToolOrigin,
};
use serde::{Deserialize, Serialize};

/// Optional runtime policy over the open action classes tools declare.
///
/// This is a Rusty extension. With no policy installed, tools retain their normal approval
/// behavior. When installed, `allow_only` is a hard boundary, ordered rules can refine the allowed
/// surface, and `auto_allow` can suppress framework-generated approval prompts for matching
/// classes. It does not bypass approval requested from inside a tool or any sandbox check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionClassPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_only: Option<BTreeSet<ActionClass>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    auto_allow: BTreeSet<ActionClass>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    auto_allow_all: bool,
    #[serde(default = "default_prompts_enabled")]
    prompts: bool,
}

const fn default_prompts_enabled() -> bool {
    true
}

impl ActionClassPolicy {
    /// Creates an unrestricted policy that preserves normal tool approval behavior.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restricts all actions to the supplied classes.
    ///
    /// Every class declared by a tool must be present. A tool with no declared classes is denied
    /// while this restriction is active.
    #[must_use]
    pub fn with_allow_only(
        mut self,
        classes: impl IntoIterator<Item = impl Into<ActionClass>>,
    ) -> Self {
        self.allow_only = Some(classes.into_iter().map(Into::into).collect());
        self
    }

    /// Automatically approves framework-level requests for the supplied classes after rules.
    ///
    /// Every class declared by a tool must be listed. Unclassified tools do not match.
    #[must_use]
    pub fn with_auto_allow(
        mut self,
        classes: impl IntoIterator<Item = impl Into<ActionClass>>,
    ) -> Self {
        self.auto_allow = classes.into_iter().map(Into::into).collect();
        self
    }

    /// Automatically approves every action after rules, including unclassified actions.
    ///
    /// This is intended for explicit product modes such as a coding product's full-bypass mode.
    /// Sandbox enforcement and approval requests made by the tool itself remain active.
    #[must_use]
    pub const fn with_auto_allow_all(mut self, enabled: bool) -> Self {
        self.auto_allow_all = enabled;
        self
    }

    /// Enables or disables framework approval prompts without changing class restrictions or
    /// automatic approvals. Disabling prompts converts `Ask` to `Deny` after policy evaluation.
    #[must_use]
    pub const fn with_prompts(mut self, enabled: bool) -> Self {
        self.prompts = enabled;
        self
    }

    /// Classes that are the only permitted classes, if restricted.
    #[must_use]
    pub const fn allow_only(&self) -> Option<&BTreeSet<ActionClass>> {
        self.allow_only.as_ref()
    }

    /// Classes that bypass framework-generated approval prompts.
    #[must_use]
    pub const fn auto_allow(&self) -> &BTreeSet<ActionClass> {
        &self.auto_allow
    }

    /// Whether every action bypasses framework-generated approval prompts.
    #[must_use]
    pub const fn auto_allow_all(&self) -> bool {
        self.auto_allow_all
    }

    /// Whether framework-generated approval prompts are enabled.
    #[must_use]
    pub const fn prompts(&self) -> bool {
        self.prompts
    }

    fn permits_classes(&self, classes: &[ActionClass]) -> bool {
        self.allow_only.as_ref().is_none_or(|allowed| {
            !classes.is_empty() && classes.iter().all(|class| allowed.contains(class))
        })
    }

    fn automatically_allows(&self, classes: &[ActionClass]) -> bool {
        self.auto_allow_all
            || (!classes.is_empty() && classes.iter().all(|class| self.auto_allow.contains(class)))
    }

    const fn normalize(&self, decision: PermissionDecision) -> PermissionDecision {
        if !self.prompts && matches!(decision, PermissionDecision::Ask) {
            PermissionDecision::Deny
        } else {
            decision
        }
    }
}

impl Default for ActionClassPolicy {
    fn default() -> Self {
        Self {
            allow_only: None,
            auto_allow: BTreeSet::new(),
            auto_allow_all: false,
            prompts: true,
        }
    }
}

/// Immutable rule evaluator applied to every tool call in a run.
///
/// An optional action-class policy runs before ordered rules. Rules are evaluated in reverse
/// declaration order, and a matching rule takes precedence over automatic approval. Without a
/// class policy, the engine resolves only configured or session rules and otherwise returns the
/// tool's normal approval decision.
#[derive(Debug, Clone, Default)]
pub struct PermissionEngine {
    policy: Option<ActionClassPolicy>,
    rules: Arc<[PermissionRule]>,
    answered_from: Option<usize>,
}

/// What a call the host rejected returns to the model when the host gave no text of its own.
pub const DEFAULT_APPROVAL_REJECTION_MESSAGE: &str = "Tool execution was not approved.";

impl PermissionEngine {
    /// Creates an evaluator with no class policy and no rule exceptions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs or replaces the optional action-class policy.
    #[must_use]
    pub fn with_policy(mut self, policy: ActionClassPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Replaces the ordered rule set while preserving the installed policy.
    #[must_use]
    pub fn with_rules(mut self, rules: impl IntoIterator<Item = PermissionRule>) -> Self {
        self.rules = rules.into_iter().collect::<Vec<_>>().into();
        self.answered_from = None;
        self
    }

    /// Appends the session rules a run's `always` answers left after configured rules.
    ///
    /// A sticky rejection carries the host's rejection text, or
    /// [`DEFAULT_APPROVAL_REJECTION_MESSAGE`] when it supplied none. Configured deny rules retain
    /// the normal runtime refusal.
    #[must_use]
    pub fn with_answered_rules(mut self, rules: impl IntoIterator<Item = PermissionRule>) -> Self {
        let mut all = self.rules.to_vec();
        let start = all.len();
        all.extend(rules);
        self.rules = all.into();
        self.answered_from = Some(self.answered_from.unwrap_or(start));
        self
    }

    /// Installed action-class policy, if any.
    #[must_use]
    pub const fn policy(&self) -> Option<&ActionClassPolicy> {
        self.policy.as_ref()
    }

    /// Returns this evaluator with framework prompts enabled or disabled.
    ///
    /// Turning prompts off creates an otherwise unrestricted policy if none was installed. It
    /// does not remove an existing class restriction or automatic approval list.
    #[must_use]
    pub fn with_prompts(mut self, enabled: bool) -> Self {
        let policy = self.policy.take().unwrap_or_default().with_prompts(enabled);
        self.policy = Some(policy);
        self
    }

    /// Ordered rule set. Later matching entries take precedence.
    #[must_use]
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Resolves a call after the tool's declaration supplied its normal decision.
    #[must_use]
    pub fn evaluate(
        &self,
        classes: &[ActionClass],
        origin: &ToolOrigin,
        agent_id: &AgentId,
        fallback: PermissionDecision,
    ) -> PermissionDecision {
        self.fixed_decision(classes, origin, agent_id)
            .unwrap_or_else(|| self.normalize(fallback))
    }

    /// Resolves a call when its classes, a matching rule, or automatic approval already decides it.
    ///
    /// Dispatch calls this before consulting a tool's dynamic approval callback, so a hard
    /// classification denial does not invoke product code for a call that cannot proceed.
    #[must_use]
    pub fn fixed_decision(
        &self,
        classes: &[ActionClass],
        origin: &ToolOrigin,
        agent_id: &AgentId,
    ) -> Option<PermissionDecision> {
        if self
            .policy
            .as_ref()
            .is_some_and(|policy| !policy.permits_classes(classes))
        {
            return Some(PermissionDecision::Deny);
        }

        if let Some(rule) = self.matching_rule(origin, agent_id) {
            return Some(self.normalize(rule));
        }

        if self
            .policy
            .as_ref()
            .is_some_and(|policy| policy.automatically_allows(classes))
        {
            return Some(PermissionDecision::Allow);
        }
        None
    }

    /// Rejection text from a matching sticky deny rule, if that rule decides this call.
    #[must_use]
    pub fn rejection_message(
        &self,
        classes: &[ActionClass],
        origin: &ToolOrigin,
        agent_id: &AgentId,
    ) -> Option<&str> {
        if self
            .policy
            .as_ref()
            .is_some_and(|policy| !policy.permits_classes(classes))
        {
            return None;
        }
        let (index, rule) = self
            .rules
            .iter()
            .enumerate()
            .rev()
            .find(|(_, rule)| rule.matches_origin_for_agent(origin, agent_id))
            .filter(|(_, rule)| matches!(rule.decision(), PermissionDecision::Deny))?;
        let answered = self.answered_from.is_some_and(|start| index >= start);
        rule.rejection_message()
            .or_else(|| answered.then_some(DEFAULT_APPROVAL_REJECTION_MESSAGE))
    }

    fn matching_rule(&self, origin: &ToolOrigin, agent_id: &AgentId) -> Option<PermissionDecision> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches_origin_for_agent(origin, agent_id))
            .map(PermissionRule::decision)
    }

    fn normalize(&self, decision: PermissionDecision) -> PermissionDecision {
        self.policy
            .as_ref()
            .map_or(decision, |policy| policy.normalize(decision))
    }
}
