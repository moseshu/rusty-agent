// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Requirements carried by an individual environment or plugin.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Skills required from one environment or plugin before inference.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopedSkillsConfig {
    /// Ordered required skill identifiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}
