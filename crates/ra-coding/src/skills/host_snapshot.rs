// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain source skill metadata, loading and snapshot contracts."
)]
use std::io;
use std::sync::Arc;

use crate::skills::SkillLoadOutcome;
use crate::skills::SkillMetadata;

/// Immutable snapshot of host-owned skills and their source filesystems.
#[derive(Debug, Clone)]
pub struct HostSkillsSnapshot {
    outcome: Arc<SkillLoadOutcome>,
}

impl HostSkillsSnapshot {
    #[must_use]
    pub fn new(outcome: Arc<SkillLoadOutcome>) -> Self {
        Self { outcome }
    }

    #[must_use]
    pub fn outcome(&self) -> &SkillLoadOutcome {
        self.outcome.as_ref()
    }

    pub async fn read_skill_text(&self, skill: &SkillMetadata) -> io::Result<String> {
        self.outcome.read_skill_text(skill).await
    }
}
