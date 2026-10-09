//! The three decisions a rule can make: Codex's `execpolicy/src/decision.rs`.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// What a policy decides about a command. Ordered from least to most strict, so the strictest of
/// several decisions is their maximum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    /// Command may run without further approval.
    Allow,
    /// Request explicit user approval; rejected outright when running with `approval_policy="never"`.
    Prompt,
    /// Command is blocked without further consideration.
    Forbidden,
}

impl Decision {
    /// Parses the spelling a rules file uses: `allow`, `prompt` or `forbidden`.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "allow" => Ok(Self::Allow),
            "prompt" => Ok(Self::Prompt),
            "forbidden" => Ok(Self::Forbidden),
            other => Err(Error::InvalidDecision(other.to_string())),
        }
    }
}
