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

use crate::{
    error::{Error, Result},
    item::{InputItemDigest, ModelInputItem, RunItem},
    session::{CompactionSnapshot, SessionCompaction, SessionId, SessionSettings},
};

/// The asynchronous contract for managing authoritative conversation session history.
///
/// Implementations may store history in memory, `SQLite`, JSONL rollout logs, or remote stores.
#[async_trait]
pub trait Session: Send + Sync + 'static {
    /// Returns the stable session identifier.
    fn session_id(&self) -> &SessionId;

    /// The session's own default settings, which a run's configuration may override.
    ///
    /// The reference's `session_settings` attribute. `None`, the default, sets nothing.
    fn session_settings(&self) -> Option<&SessionSettings> {
        None
    }

    /// Whether the backend stores items under identities of its own, so that an item read back
    /// cannot be recognized by the [`RunItem`] identity and envelope it was added with.
    ///
    /// The reference's `_ignore_ids_for_matching`, which it applies to its provider-managed
    /// conversation session. `true` compares the model input without item identities; `false`,
    /// the default, compares stored records exactly. This does not change callback matching.
    fn ignore_ids_for_matching(&self) -> bool {
        false
    }

    /// Selects the items this backend can persist, preserving their order.
    ///
    /// The default keeps every item. A backend may omit records its storage contract cannot
    /// retain; the runtime still counts those records as processed, but excludes them from the
    /// pending append. This is the reference's pre-append filtering, dispatched through the
    /// backend because the runtime cannot depend on provider implementations.
    fn prepare_items_for_persistence(&self, items: Vec<RunItem>) -> Vec<RunItem> {
        items
    }

    /// Fingerprints one item as this backend stores it, for pending-append reconciliation.
    ///
    /// The default follows [`Self::ignore_ids_for_matching`]. Backends that transform content
    /// must apply the same storage projection to a pending item and an item read back. The
    /// reference fingerprints wire items; this dispatch is necessary because Rust sessions
    /// exchange typed run records while provider lowering lives outside the runtime.
    async fn item_digest_for_persistence(&self, item: &RunItem) -> Result<InputItemDigest> {
        let digest = match item
            .to_model_input()
            .filter(|_| self.ignore_ids_for_matching())
        {
            Some(ModelInputItem::Reasoning(reasoning)) => {
                InputItemDigest::compute(&ModelInputItem::Reasoning(reasoning.without_id()))
            }
            Some(input) => InputItemDigest::compute(&input),
            None => InputItemDigest::compute_session_item(item),
        };
        digest.map_err(|error| {
            Error::caller("Session item could not be fingerprinted").with_source(error)
        })
    }

    /// Whether a callback's reconstructed item may match stored history by content alone.
    ///
    /// The default permits matching every kind. A backend whose history carries identities
    /// absent from the model projection may require identity for selected kinds instead. This
    /// is independent of the identity policy used to reconcile an append.
    fn matches_reconstructed_history_item(&self, _item: &RunItem) -> bool {
        true
    }

    /// Optional post-turn compaction; ordinary sessions need not implement it.
    fn compaction(&self) -> Option<&dyn SessionCompaction> {
        None
    }

    /// Whether history is managed by a provider rather than replaceable locally.
    fn manages_server_history(&self) -> bool {
        false
    }

    /// Reads history and its exact wrapper generation under one mutation boundary.
    async fn get_items_with_generation(
        &self,
        limit: Option<usize>,
    ) -> Result<(Vec<RunItem>, Option<u64>)> {
        Ok((self.get_items(limit).await?, None))
    }

    /// Appends a batch, retaining automatic-compaction ownership only if its read stayed current.
    async fn add_items_with_generation(
        &self,
        items: Vec<RunItem>,
        _expected: Option<u64>,
    ) -> Result<Option<u64>> {
        self.add_items(items).await?;
        Ok(None)
    }

    /// Optional bounded snapshot with backend-owned atomic suffix replacement.
    async fn get_compaction_snapshot(&self, _limit: usize) -> Result<Option<CompactionSnapshot>> {
        Ok(None)
    }

    /// Retrieves items from the session history.
    ///
    /// If `limit` is `Some(n)`, returns up to the most recent `n` items (tail read projection)
    /// in chronological order without mutating or deleting stored items.
    /// If `limit` is `None`, returns all items in chronological order — or, for a session whose
    /// own [`Self::session_settings`] set a limit, the most recent that many, as the reference's
    /// stores resolve an absent limit against their settings.
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>>;

    /// Appends authoritative run items to the session history.
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()>;

    /// Removes and returns the most recent item from the session history, if any.
    async fn pop_item(&self) -> Result<Option<RunItem>>;

    /// Clears all items from this session history.
    async fn clear(&self) -> Result<()>;
}
