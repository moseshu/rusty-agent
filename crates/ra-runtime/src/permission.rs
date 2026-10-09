//! Runtime permission evaluation for tool dispatch.
//!
//! The core owns the serializable policy vocabulary. This module combines that vocabulary with a
//! tool's declared effect and its normal approval result; it neither parses provider payloads nor
//! enforces a sandbox boundary.

use std::sync::Arc;

use ra_core::{
    permission::{PermissionDecision, PermissionMode, PermissionRule, PermissionScope},
    tool::ToolOrigin,
};

/// Immutable rule evaluator applied to every tool call in a run.
///
/// Rules are evaluated in reverse declaration order: the last matching rule is the most specific
/// configuration the host supplied. Mode restrictions still apply around that result. In
/// particular, plan mode cannot be opened up to edits or commands by a rule, and `DontAsk` turns
/// any would-be prompt into a denial.
#[derive(Debug, Clone, Default)]
pub struct PermissionEngine {
    mode: PermissionMode,
    rules: Arc<[PermissionRule]>,
    // Where the rules a run's own answers left begin, after the configured ones.
    answered_from: Option<usize>,
}

/// What a call the host rejected returns to the model when the host gave no text of its own: the
/// reference's `DEFAULT_APPROVAL_REJECTION_MESSAGE`.
pub const DEFAULT_APPROVAL_REJECTION_MESSAGE: &str = "Tool execution was not approved.";

impl PermissionEngine {
    /// Creates an evaluator under `mode` with no rule exceptions.
    #[must_use]
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            rules: Arc::from([]),
            answered_from: None,
        }
    }

    /// Switches the base mode without copying the rule table.
    #[must_use]
    pub fn with_mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Replaces the ordered rule set.
    #[must_use]
    pub fn with_rules(mut self, rules: impl IntoIterator<Item = PermissionRule>) -> Self {
        self.rules = rules.into_iter().collect::<Vec<_>>().into();
        self.answered_from = None;
        self
    }

    /// Appends the session rules a run's `always` answers left (see
    /// [`RunState::permission_rules`](ra_core::state::RunState::permission_rules)) after the
    /// configured ones, so they take precedence.
    ///
    /// They decide like any rule. What differs is what a call one of their deny rules refuses
    /// returns: the host's rejection text, or [`DEFAULT_APPROVAL_REJECTION_MESSAGE`] when it gave
    /// none, as the reference answers a call its sticky rejection covers. A configured deny rule
    /// keeps the runtime's own refusal.
    #[must_use]
    pub fn with_answered_rules(mut self, rules: impl IntoIterator<Item = PermissionRule>) -> Self {
        let mut all = self.rules.to_vec();
        let start = all.len();
        all.extend(rules);
        self.rules = all.into();
        self.answered_from = Some(self.answered_from.unwrap_or(start));
        self
    }

    /// Selected base mode.
    #[must_use]
    pub const fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Ordered rule set. Later matching entries take precedence.
    #[must_use]
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Resolves a call after the tool's declaration supplied its normal decision.
    ///
    /// The fallback is normally `Allow` for a tool that does not need approval and `Ask` for one
    /// that does. Keeping the dynamic tool callback outside this pure evaluator means matching is
    /// deterministic and testable without handing the policy engine executable tool objects.
    #[must_use]
    pub fn evaluate(
        &self,
        scope: PermissionScope,
        origin: &ToolOrigin,
        fallback: PermissionDecision,
    ) -> PermissionDecision {
        self.fixed_decision(scope, origin)
            .unwrap_or_else(|| self.normalize(fallback))
    }

    /// Resolves a call when mode or a matching rule already decides it.
    ///
    /// Dispatch uses this before asking a dynamically-configured tool whether it needs approval,
    /// so an explicitly denied call does not invoke third-party policy code just to be refused.
    #[must_use]
    pub fn fixed_decision(
        &self,
        scope: PermissionScope,
        origin: &ToolOrigin,
    ) -> Option<PermissionDecision> {
        if matches!(self.mode, PermissionMode::Plan) && !matches!(scope, PermissionScope::Read) {
            return Some(PermissionDecision::Deny);
        }

        if let Some(decision) = self.matching_rule(origin) {
            return Some(self.normalize(decision));
        }

        if matches!(self.mode, PermissionMode::BypassPermissions)
            || (matches!(self.mode, PermissionMode::AcceptEdits)
                && matches!(scope, PermissionScope::Edit))
        {
            Some(PermissionDecision::Allow)
        } else {
            None
        }
    }

    /// The text a call refused by a matching deny rule returns to the model: the rule's own text
    /// (see [`PermissionRule::rejection_message`]), or [`DEFAULT_APPROVAL_REJECTION_MESSAGE`] for
    /// a rule a run's answer left without one (see [`Self::with_answered_rules`]).
    ///
    /// `None` for a configured deny rule without text, and whenever something other than a rule
    /// decides the call: plan mode refuses before any rule is consulted, and `DontAsk` turns an
    /// `Ask` rule into a denial of its own.
    #[must_use]
    pub fn rejection_message(&self, scope: PermissionScope, origin: &ToolOrigin) -> Option<&str> {
        if matches!(self.mode, PermissionMode::Plan) && !matches!(scope, PermissionScope::Read) {
            return None;
        }
        let (index, rule) = self
            .rules
            .iter()
            .enumerate()
            .rev()
            .find(|(_, rule)| rule.matches_origin(origin))
            .filter(|(_, rule)| matches!(rule.decision(), PermissionDecision::Deny))?;
        let answered = self.answered_from.is_some_and(|start| index >= start);
        rule.rejection_message()
            .or_else(|| answered.then_some(DEFAULT_APPROVAL_REJECTION_MESSAGE))
    }

    fn matching_rule(&self, origin: &ToolOrigin) -> Option<PermissionDecision> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches_origin(origin))
            .map(PermissionRule::decision)
    }

    const fn normalize(&self, decision: PermissionDecision) -> PermissionDecision {
        if matches!(self.mode, PermissionMode::DontAsk)
            && matches!(decision, PermissionDecision::Ask)
        {
            PermissionDecision::Deny
        } else {
            decision
        }
    }
}
