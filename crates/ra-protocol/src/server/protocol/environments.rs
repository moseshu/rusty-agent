// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Environment selection at the client boundary; products validate supported execution routes.

use ra_paths::LegacyAppPathString;
use serde::{Deserialize, Serialize};

/// An environment selected for a thread or turn, with its native working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEnvironmentParams {
    /// Registered execution environment identity.
    pub environment_id: String,
    /// Native absolute path or file URI in that environment.
    pub cwd: LegacyAppPathString,
    /// Runtime workspace roots; omission uses cwd.
    pub runtime_workspace_roots: Option<Vec<LegacyAppPathString>>,
}
