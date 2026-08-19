//! Asynchronous session port contract for conversation history storage.
//!
//! A [`Session`] is the provider-neutral, storage-layout-independent contract for reading and
//! appending authoritative session records ([`RunItem`]).
//!
//! # Authority and separation
//!
//! A session manages only conversation history. It does not own live host context
//! ([`RunContext`](crate::context::RunContext)), recoverable run state
//! ([`RunState`](crate::state::RunState)), or cross-run task state
//! ([`WorkState`](crate::state::WorkStateHandle)).
//!
//! Model requests must not read this history directly; model input items are projected from
//! authoritative [`RunItem`] records via input normalization.

use async_trait::async_trait;

use crate::{error::Result, item::RunItem, session::SessionId};

/// The asynchronous contract for managing authoritative conversation session history.
///
/// Implementations may store history in memory, `SQLite`, JSONL rollout logs, or remote stores.
#[async_trait]
pub trait Session: Send + Sync + 'static {
    /// Returns the stable session identifier.
    fn session_id(&self) -> &SessionId;

    /// Retrieves items from the session history.
    ///
    /// If `limit` is `Some(n)`, returns up to the most recent `n` items (tail read projection)
    /// in chronological order without mutating or deleting stored items.
    /// If `limit` is `None`, returns all items in chronological order.
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>>;

    /// Appends authoritative run items to the session history.
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()>;

    /// Removes and returns the most recent item from the session history, if any.
    async fn pop_item(&self) -> Result<Option<RunItem>>;

    /// Clears all items from this session history.
    async fn clear(&self) -> Result<()>;
}
