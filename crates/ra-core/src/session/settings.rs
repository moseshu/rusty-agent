//! Settings for reading a session's history.
//!
//! A port of `openai-agents-python`'s `memory/session_settings.py`. A session may carry its own
//! defaults ([`Session::session_settings`](super::Session::session_settings)); a run may override
//! them through its configuration. Values the override leaves unset keep the session's.

use serde::{Deserialize, Serialize};

/// Settings for session operations.
///
/// `limit` is a read projection: it bounds how many of the most recent items a run reads as
/// history and never deletes anything from the store.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionSettings {
    /// Maximum number of items to retrieve. `None` retrieves all items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
}

impl SessionSettings {
    /// Settings that leave every value unset.
    #[must_use]
    pub const fn new() -> Self {
        Self { limit: None }
    }

    /// Bounds the history a run reads to the most recent `limit` items.
    #[must_use]
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Maximum number of items to retrieve, if bounded.
    #[must_use]
    pub const fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// Overlays every value `override_settings` sets on top of these settings.
    ///
    /// Unset values in the override keep this instance's, as the reference's `resolve` overlays
    /// only non-`None` fields.
    #[must_use]
    pub fn resolve(&self, override_settings: Option<&Self>) -> Self {
        let Some(override_settings) = override_settings else {
            return *self;
        };
        Self {
            limit: override_settings.limit.or(self.limit),
        }
    }
}

/// The limit a session read uses: an explicit one, else the settings' own.
///
/// The reference's `resolve_session_limit`, which its stores use so a read without an explicit
/// limit honors the session's configured default.
#[must_use]
pub fn resolve_session_limit(
    explicit_limit: Option<usize>,
    settings: Option<&SessionSettings>,
) -> Option<usize> {
    explicit_limit.or_else(|| settings.and_then(SessionSettings::limit))
}
