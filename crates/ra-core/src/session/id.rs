//! Opaque identifier for an authoritative conversation session.
//!
//! A session identifier distinguishes conversation histories across runs and agents.
//! It is distinct from command execution sessions ([`ExecSessionId`](crate::event::exec::ExecSessionId))
//! and provider-managed remote conversations ([`ProviderConversationId`](crate::model::ProviderConversationId)).

use core::fmt;
use std::borrow::Borrow;

use serde::{Deserialize, Serialize};

/// An opaque identifier for a local authoritative conversation session.
///
/// This is the identity of the authoritative local conversation history. It is distinct from
/// execution and PTY session identifiers ([`ExecSessionId`](crate::event::exec::ExecSessionId))
/// and provider-managed remote conversation identifiers ([`ProviderConversationId`](crate::model::ProviderConversationId)).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Creates a session identifier from a string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generates a new unique session identifier using a timestamp-sortable UUID v7.
    ///
    /// The `sess-` prefix keeps conversation history identifiers distinguishable in logs from
    /// command execution session identifiers, which carry an `exec-` prefix. UUID v7 also gives
    /// generated identifiers monotonic temporal ordering, which rollout file naming depends on.
    #[must_use]
    pub fn generate() -> Self {
        Self(format!("sess-{}", uuid::Uuid::now_v7()))
    }

    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        &self.0
    }
}
