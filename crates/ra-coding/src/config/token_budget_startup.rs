// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
//! Keeps configured token-budget preferences separate from session startup activation.
//! Fresh child sessions restore this snapshot before applying their starting model's defaults;
//! history forks retain their parent's effective activation and the original snapshot.

use super::product::Config;
use super::product::TokenBudgetConfig;
use ra_config::ConstraintResult;
use ra_config::features::Feature;

/// Token-budget preferences before a session applies experimental or model-owned activation.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenBudgetStartupConfig {
    enabled: bool,
    token_budget: Option<TokenBudgetConfig>,
}

impl Config {
    /// Captures configured preferences, removing any inherited startup activation first.
    pub fn prepare_token_budget_for_startup(&mut self) -> ConstraintResult<()> {
        if let Some(snapshot) = self.token_budget_startup_config.as_ref() {
            self.features
                .set_enabled(Feature::TokenBudget, snapshot.enabled)?;
            self.token_budget = snapshot.token_budget.clone();
        }
        self.token_budget_startup_config = Some(TokenBudgetStartupConfig {
            enabled: self.features.enabled(Feature::TokenBudget),
            token_budget: self.token_budget.clone(),
        });
        Ok(())
    }
}
