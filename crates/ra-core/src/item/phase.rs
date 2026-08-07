//! User-visible channels for assistant output.

use serde::{Deserialize, Serialize};

/// Output phase for an assistant message.
///
/// Progress updates belong to [`Commentary`](Self::Commentary). Only the closing delivery of a
/// completed run belongs to [`Final`](Self::Final). User and system messages do not carry a phase.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputPhase {
    /// Progress and context around tool calls.
    Commentary,
    /// Final delivery after the run has completed.
    Final,
}

impl OutputPhase {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::Final => "final",
        }
    }
}

impl core::fmt::Display for OutputPhase {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}
