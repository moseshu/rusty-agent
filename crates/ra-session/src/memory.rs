//! In-memory reference implementation of the [`Session`] contract.

use std::sync::{PoisonError, RwLock};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    item::RunItem,
    session::{Session, SessionId, SessionSettings, resolve_session_limit},
};

/// An in-memory, thread-safe implementation of [`Session`].
///
/// This implementation provides a lightweight reference session storage suitable for tests,
/// short-lived runs, and environments without filesystem or database access.
/// Like [`InMemoryHostEventSink`](ra_core::event::sink::InMemoryHostEventSink), poisoned lock
/// guards recover the underlying data via [`PoisonError::into_inner`] to allow inspection and
/// continued operation in test harnesses without panicking.
#[derive(Debug)]
pub struct InMemorySession {
    session_id: SessionId,
    session_settings: Option<SessionSettings>,
    items: RwLock<Vec<RunItem>>,
}

impl InMemorySession {
    /// Creates an empty in-memory session with the given identifier.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            session_settings: None,
            items: RwLock::new(Vec::new()),
        }
    }

    /// Creates an in-memory session initialized with existing items.
    #[must_use]
    pub fn new_with_items(session_id: impl Into<SessionId>, items: Vec<RunItem>) -> Self {
        Self {
            session_id: session_id.into(),
            session_settings: None,
            items: RwLock::new(items),
        }
    }

    /// Gives the session default settings: a read without an explicit limit uses theirs.
    #[must_use]
    pub const fn with_session_settings(mut self, settings: SessionSettings) -> Self {
        self.session_settings = Some(settings);
        self
    }
}

#[async_trait]
impl Session for InMemorySession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn session_settings(&self) -> Option<&SessionSettings> {
        self.session_settings.as_ref()
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        let guard = self.items.read().unwrap_or_else(PoisonError::into_inner);

        let items = match resolve_session_limit(limit, self.session_settings.as_ref()) {
            None => guard.clone(),
            Some(n) if n >= guard.len() => guard.clone(),
            Some(n) => guard[guard.len() - n..].to_vec(),
        };

        Ok(items)
    }

    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        let mut guard = self.items.write().unwrap_or_else(PoisonError::into_inner);
        guard.extend(items);
        Ok(())
    }

    async fn pop_item(&self) -> Result<Option<RunItem>> {
        let mut guard = self.items.write().unwrap_or_else(PoisonError::into_inner);
        Ok(guard.pop())
    }

    async fn clear(&self) -> Result<()> {
        let mut guard = self.items.write().unwrap_or_else(PoisonError::into_inner);
        guard.clear();
        Ok(())
    }
}
