// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Hook trust metadata shared by product discovery and protocol consumers.

use serde::{Deserialize, Serialize};

/// Trust state of a discovered normalized handler identity.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HookTrustStatus {
    /// The handler is supplied by managed policy.
    Managed,
    /// No matching prior trust identity is available.
    Untrusted,
    /// The handler identity is trusted or admitted as a built-in handler.
    Trusted,
    /// A saved trust identity differs from the current identity.
    Modified,
}
