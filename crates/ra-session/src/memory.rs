//! In-memory reference implementation of the [`Session`] contract.

use std::sync::{PoisonError, RwLock};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    item::RunItem,
    session::{Session, SessionId},
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
    items: RwLock<Vec<RunItem>>,
}

impl InMemorySession {
    /// Creates an empty in-memory session with the given identifier.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            items: RwLock::new(Vec::new()),
        }
    }

    /// Creates an in-memory session initialized with existing items.
    #[must_use]
    pub fn new_with_items(session_id: impl Into<SessionId>, items: Vec<RunItem>) -> Self {
        Self {
            session_id: session_id.into(),
            items: RwLock::new(items),
        }
    }
}

#[async_trait]
impl Session for InMemorySession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        let guard = self.items.read().unwrap_or_else(PoisonError::into_inner);

        let items = match limit {
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
