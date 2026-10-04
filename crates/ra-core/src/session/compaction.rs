//! Optional session compaction and atomic history replacement contracts.
//!
//! Provider request modes stay in adapters. These ports carry only lifecycle evidence:
//! the latest successful exchange, wrapper mutation ownership, and backend snapshots.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    cancel::CancelScope,
    error::{Error, Result},
    item::{InputItemDigest, ModelInputItem, RunItem},
    model::ModelRequest,
    usage::Usage,
};

/// Evidence authorizing automatic compaction of the latest successful model exchange.
///
/// Digests preserve order and repeated occurrences without persisting plaintext requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCompactionContext {
    // A wrapper's generation counter is local to that instance and process. A restored
    // checkpoint may meet a different wrapper whose counter happens to hold the same value,
    // so ownership is never serialized; a resumed write regains it only from a reconciled read.
    #[serde(skip)]
    generation: Option<u64>,
    model_exchange: Vec<InputItemDigest>,
    response_id: Option<String>,
    response_stored: Option<bool>,
}

impl SessionCompactionContext {
    /// Captures wrapper ownership and the latest successful model exchange.
    #[must_use]
    pub fn new(
        generation: Option<u64>,
        model_exchange: Vec<InputItemDigest>,
        response_id: Option<String>,
        response_stored: Option<bool>,
    ) -> Self {
        Self {
            generation,
            model_exchange,
            response_id,
            response_stored,
        }
    }
    /// Wrapper generation owned by the run's latest history read or append.
    ///
    /// Process-local: it is not part of a serialized checkpoint and restores as `None`.
    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }
    /// Digests of the actual request followed by its successful response items.
    #[must_use]
    pub fn model_exchange(&self) -> &[InputItemDigest] {
        &self.model_exchange
    }
    /// Identity of that successful response, when supplied by the model.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }
    /// Whether that response was retained by the provider for subsequent retrieval.
    #[must_use]
    pub const fn response_stored(&self) -> Option<bool> {
        self.response_stored
    }
    /// Replaces mutation ownership after a reconciled read or append.
    pub fn set_generation(&mut self, generation: Option<u64>) {
        self.generation = generation;
    }
    /// Invalidates evidence before a model request, so failure cannot reuse an earlier exchange.
    pub fn reset_exchange(&mut self) {
        self.model_exchange.clear();
        self.response_id = None;
        self.response_stored = None;
    }
    /// Records one model-visible occurrence in request/response order.
    pub fn push_model_item_digest(&mut self, digest: InputItemDigest) {
        self.model_exchange.push(digest);
    }
    /// Records response identity and retention after successful generation.
    pub fn set_response(&mut self, response_id: Option<String>, stored: Option<bool>) {
        self.response_id = response_id;
        self.response_stored = stored;
    }
}

/// Compaction settlement, including billed usage even if normalization or replacement failed.
pub struct SessionCompactionOutcome {
    usage: Usage,
    result: Result<()>,
}

impl SessionCompactionOutcome {
    /// Reports both billed usage and the operation's final result.
    #[must_use]
    pub fn new(usage: Usage, result: Result<()>) -> Self {
        Self { usage, result }
    }
    /// Usage to be settled by the run's ledger owner.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }
    /// Usage and settlement by value, retaining billing when settlement failed.
    pub fn into_parts(self) -> (Usage, Result<()>) {
        (self.usage, self.result)
    }
    /// Settlement by value for callers without a usage ledger.
    pub fn into_result(self) -> Result<()> {
        self.result
    }
}

/// Optional compaction behavior discovered through [`super::Session::compaction`].
#[async_trait]
pub trait SessionCompaction: Send + Sync {
    /// Fingerprints a model item under the adapter's wire and identity policy.
    async fn model_item_digest(&self, item: &ModelInputItem) -> Result<InputItemDigest> {
        InputItemDigest::compute(item).map_err(|error| {
            Error::caller("Compaction item could not be fingerprinted").with_source(error)
        })
    }
    /// Fingerprints the request after the adapter's own input normalization and lowering.
    ///
    /// The default records each prepared item. Adapters which prune or deduplicate wire input
    /// must override this so automatic compaction cannot remove items the model never received.
    async fn model_request_digests(&self, request: &ModelRequest) -> Result<Vec<InputItemDigest>> {
        let mut digests = Vec::with_capacity(request.input().len());
        for item in request.input() {
            digests.push(self.model_item_digest(item).await?);
        }
        Ok(digests)
    }
    /// Whether the request's response will be retrievable by the provider, when known.
    fn response_stored(&self, _request: &ModelRequest) -> Option<bool> {
        None
    }
    /// Applies the post-append decision: defer for local outputs, otherwise compact when due.
    ///
    /// Cancellation may interrupt reads and the provider request. Once replacement starts,
    /// implementations must settle replacement or rollback before returning its outcome.
    async fn after_turn(
        &self,
        context: &SessionCompactionContext,
        has_local_tool_outputs: bool,
        cancel: &CancelScope,
    ) -> SessionCompactionOutcome;
}

/// Backend-owned compare-and-replace operation for one bounded history snapshot.
#[async_trait]
pub trait CompactionSnapshotReplacement: Send + Sync {
    /// Replaces the snapshot suffix starting at `start` only if it is still authoritative.
    ///
    /// Returns `false` when a concurrent backend mutation revoked the snapshot. Failure must
    /// leave history unchanged; cancellation must not leave a half-applied transaction.
    async fn replace_suffix(&self, start: usize, items: Vec<RunItem>) -> Result<bool>;
}

/// Bounded history and its atomic replacement operation.
pub struct CompactionSnapshot {
    items: Vec<RunItem>,
    complete: bool,
    replacement: Arc<dyn CompactionSnapshotReplacement>,
}

impl CompactionSnapshot {
    /// Captures bounded logical history and a backend-owned conditional replacement.
    #[must_use]
    pub fn new(
        items: Vec<RunItem>,
        complete: bool,
        replacement: Arc<dyn CompactionSnapshotReplacement>,
    ) -> Self {
        Self {
            items,
            complete,
            replacement,
        }
    }
    /// Logical items in chronological order.
    #[must_use]
    pub fn items(&self) -> &[RunItem] {
        &self.items
    }
    /// Whether this snapshot covers all retained history.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }
    /// Atomically replaces an unchanged snapshot suffix; otherwise returns `false`.
    pub async fn replace_suffix(&self, start: usize, items: Vec<RunItem>) -> Result<bool> {
        self.replacement.replace_suffix(start, items).await
    }
}
