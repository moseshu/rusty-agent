//! Runtime permission evaluation for tool dispatch.
//!
//! The core owns the serializable policy vocabulary. This module combines that vocabulary with a
//! tool's declared effect and its normal approval result; it neither parses provider payloads nor
//! enforces a sandbox boundary.

use std::sync::Arc;

use ra_core::permission::{PermissionDecision, PermissionMode, PermissionRule, PermissionScope};

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
}

impl PermissionEngine {
    /// Creates an evaluator under `mode` with no rule exceptions.
    #[must_use]
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            rules: Arc::from([]),
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
        tool_name: &str,
        namespace: Option<&str>,
        fallback: PermissionDecision,
    ) -> PermissionDecision {
        self.fixed_decision(scope, tool_name, namespace)
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
        tool_name: &str,
        namespace: Option<&str>,
    ) -> Option<PermissionDecision> {
        if matches!(self.mode, PermissionMode::Plan) && !matches!(scope, PermissionScope::Read) {
            return Some(PermissionDecision::Deny);
        }

        if let Some(decision) = self.matching_rule(tool_name, namespace) {
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

    fn matching_rule(
        &self,
        tool_name: &str,
        namespace: Option<&str>,
    ) -> Option<PermissionDecision> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches(tool_name, namespace))
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
