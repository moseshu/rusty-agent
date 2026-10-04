//! How a run merges a session's history with its new input.
//!
//! A port of `openai-agents-python`'s `SessionInputCallback`. Without one, a run's model input is
//! the session's history followed by the new input. A callback may filter, reorder, duplicate or
//! replace items, and may add items of its own.
//!
//! # What the run stores afterwards
//!
//! Not everything the callback returns: the run appends to the session only what belongs to the
//! new turn, so a callback that repeats history does not grow the session. After the callback
//! returns, an item is history when it is one of the history list's items — by [`ItemId`] and
//! content, else when the history list still holds an equal item it has not been matched to — and
//! everything else is new. The two lists are handed over by mutable reference because what they
//! hold **after** the callback is what counts, as the reference builds its reference maps from the
//! lists the callback was given once it has returned: an item the callback writes into the history
//! list stays history, and one it moves into the new-input list from history is still history.
//!
//! # Identity
//!
//! The reference tells "this exact item" from "an equal copy" by object identity. Here the
//! identity is the item's [`ItemId`]: a clone of a history item is the same item, and an item
//! the callback builds from scratch is a new one unless the history list holds an equal item that
//! no other output item was matched to.
//!
//! [`ItemId`]: crate::item::ItemId

use async_trait::async_trait;

use crate::{error::Result, item::RunItem};

/// Combines a session's history with a run's new input into the run's model input.
#[async_trait]
pub trait SessionInputCallback: Send + Sync {
    /// Returns the items the run starts from.
    ///
    /// `history` is the session's history as read for this run, with its settings' limit applied;
    /// `new_input` is the run's new input, each item under an identity the runner assigned.
    async fn combine(
        &self,
        history: &mut Vec<RunItem>,
        new_input: &mut Vec<RunItem>,
    ) -> Result<Vec<RunItem>>;
}

#[async_trait]
impl<F> SessionInputCallback for F
where
    F: Fn(&mut Vec<RunItem>, &mut Vec<RunItem>) -> Result<Vec<RunItem>> + Send + Sync,
{
    async fn combine(
        &self,
        history: &mut Vec<RunItem>,
        new_input: &mut Vec<RunItem>,
    ) -> Result<Vec<RunItem>> {
        self(history, new_input)
    }
}
