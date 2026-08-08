//! Internal preparation of provider-neutral model input.
//!
//! This module is public only so provider/runtime crates can share one implementation. It is not
//! a stable extension point. The authoritative record remains [`RunItem`]; normalization works on
//! its [`ModelInputItem`] projection and never sends provenance, raw provider payloads, or session
//! data back to a model.
//!
//! Provider conversation identifiers are deliberately not recognized by string key here. They
//! belong to an adapter/session policy and must be removed while lifting a provider payload into
//! typed items. Generic removal from forward-compatible `unknown` fields would silently destroy
//! data owned by a newer schema.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CallId, ItemId, ModelInputItem, RunItem};

/// Fixed-size digest of the exact model-facing projection of an item.
///
/// The digest is calculated after normalization policy (including reasoning-ID omission) has
/// been applied. It therefore identifies what was actually sent, not the richer session record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputItemDigest(String);

impl InputItemDigest {
    /// Computes a deterministic SHA-256 digest of an input item.
    pub fn compute(item: &ModelInputItem) -> Result<Self, serde_json::Error> {
        let encoded = serde_json::to_vec(item)?;
        let digest = Sha256::digest(encoded);
        Ok(Self(format!("{digest:x}")))
    }

    /// Lowercase hexadecimal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for InputItemDigest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Durable coordinates for one authoritative session-item occurrence.
///
/// `ItemId` distinguishes two intentional equal-content occurrences; the digest verifies that a
/// callback, resume, or rewrite still refers to the same model-facing value. Neither coordinate
/// depends on an array index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InputItemOccurrenceKey {
    item_id: ItemId,
    digest: InputItemDigest,
}

impl InputItemOccurrenceKey {
    /// Creates coordinates from a stable session item ID and its exact outbound digest.
    #[must_use]
    pub const fn new(item_id: ItemId, digest: InputItemDigest) -> Self {
        Self { item_id, digest }
    }

    /// Stable identity of the authoritative session record.
    #[must_use]
    pub const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// Digest of the exact outbound projection.
    #[must_use]
    pub const fn digest(&self) -> &InputItemDigest {
        &self.digest
    }
}

/// Policy for provider reasoning item IDs.
///
/// This policy never drops encrypted content or provider replay data. Those fields are required
/// to replay reasoning when server-side storage is disabled; only the provider item ID is
/// optional because some endpoints reject IDs issued by another conversation.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReasoningIdPolicy {
    /// Preserve provider reasoning item IDs.
    #[default]
    Preserve,
    /// Omit provider reasoning item IDs while retaining all replay material.
    Omit,
}

/// How locally unmatched call/output pairs are handled.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum OrphanPolicy {
    /// Keep every item. Use this for caller-owned input that the runner must not reinterpret.
    Preserve,
    /// Drop calls without local outputs, retaining output-only items for server continuations.
    #[default]
    DropCallsWithoutOutputs,
    /// Require both sides locally and drop unmatched calls and outputs.
    DropUnpaired,
}

/// One normalized item and, when projected from a session record, its durable coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedInputItem {
    item: ModelInputItem,
    occurrence_key: Option<InputItemOccurrenceKey>,
}

impl NormalizedInputItem {
    /// Model-facing item.
    #[must_use]
    pub const fn item(&self) -> &ModelInputItem {
        &self.item
    }

    /// Durable coordinates, absent for caller-owned model input without a session record.
    #[must_use]
    pub const fn occurrence_key(&self) -> Option<&InputItemOccurrenceKey> {
        self.occurrence_key.as_ref()
    }
}

/// A normalized model-input sequence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NormalizedInput {
    entries: Vec<NormalizedInputItem>,
}

impl NormalizedInput {
    /// Entries with their optional occurrence coordinates.
    #[must_use]
    pub fn entries(&self) -> &[NormalizedInputItem] {
        &self.entries
    }

    /// Removes reconciliation coordinates and returns only the model request payload.
    #[must_use]
    pub fn into_items(self) -> Vec<ModelInputItem> {
        self.entries.into_iter().map(|entry| entry.item).collect()
    }
}

/// Shared input preparation for provider adapters and the runtime.
///
/// The type is intentionally internal API (`#[doc(hidden)]` at its module boundary). It performs
/// only provider-neutral work: stable-identity dedupe, call/output pairing, dangling-reasoning
/// cleanup, reasoning-ID policy, and session-to-model projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputItemNormalizer {
    reasoning_id_policy: ReasoningIdPolicy,
    orphan_policy: OrphanPolicy,
}

impl InputItemNormalizer {
    /// Creates the default normalizer.
    ///
    /// Provider reasoning IDs are preserved. Calls without outputs are removed, while output-only
    /// items are retained because they may target a call stored behind a server continuation ID.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reasoning_id_policy: ReasoningIdPolicy::Preserve,
            orphan_policy: OrphanPolicy::DropCallsWithoutOutputs,
        }
    }

    /// Sets reasoning item ID handling.
    #[must_use]
    pub const fn with_reasoning_id_policy(mut self, policy: ReasoningIdPolicy) -> Self {
        self.reasoning_id_policy = policy;
        self
    }

    /// Sets local call/output orphan handling.
    #[must_use]
    pub const fn with_orphan_policy(mut self, policy: OrphanPolicy) -> Self {
        self.orphan_policy = policy;
        self
    }

    /// Projects authoritative session records, normalizes them, and attaches durable coordinates.
    pub fn normalize_run_items(
        &self,
        items: &[RunItem],
    ) -> Result<NormalizedInput, serde_json::Error> {
        let projected = items
            .iter()
            .filter_map(|run_item| {
                run_item
                    .to_model_input()
                    .map(|item| PendingInputItem::new(item, Some(run_item.id().clone())))
            })
            .collect();
        self.normalize(projected)
    }

    /// Normalizes already-projected model input.
    ///
    /// These items have no authoritative session occurrence key. Callers that need durable
    /// reconciliation must assign `RunItem::id` values before history can be rewritten.
    pub fn normalize_model_items(
        &self,
        items: &[ModelInputItem],
    ) -> Result<NormalizedInput, serde_json::Error> {
        self.normalize(
            items
                .iter()
                .cloned()
                .map(|item| PendingInputItem::new(item, None))
                .collect(),
        )
    }

    fn normalize(self, items: Vec<PendingInputItem>) -> Result<NormalizedInput, serde_json::Error> {
        let deduplicated = deduplicate_preferring_latest(items);
        let paired = prune_orphans(deduplicated, self.orphan_policy);

        let entries = paired
            .into_iter()
            .map(|entry| {
                let item = apply_reasoning_id_policy(entry.item, self.reasoning_id_policy);
                let digest = InputItemDigest::compute(&item)?;
                let occurrence_key = entry
                    .item_id
                    .map(|item_id| InputItemOccurrenceKey::new(item_id, digest));
                Ok(NormalizedInputItem {
                    item,
                    occurrence_key,
                })
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;

        Ok(NormalizedInput { entries })
    }
}

#[derive(Debug, Clone)]
struct PendingInputItem {
    item: ModelInputItem,
    item_id: Option<ItemId>,
}

impl PendingInputItem {
    const fn new(item: ModelInputItem, item_id: Option<ItemId>) -> Self {
        Self { item, item_id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DedupeKey {
    Reasoning(String),
    ToolCall(CallId),
    ToolCallOutput(CallId),
    HandoffCall(CallId),
    HandoffOutput(CallId),
    McpApprovalRequest(String),
    McpApprovalResponse(String),
}

fn dedupe_key(item: &ModelInputItem) -> Option<DedupeKey> {
    match item {
        ModelInputItem::Reasoning(reasoning) => {
            reasoning.id().map(|id| DedupeKey::Reasoning(id.to_owned()))
        }
        ModelInputItem::ToolCall(call) => Some(DedupeKey::ToolCall(call.call_id().clone())),
        ModelInputItem::ToolCallOutput(output) => {
            Some(DedupeKey::ToolCallOutput(output.call_id().clone()))
        }
        ModelInputItem::HandoffCall(call) => Some(DedupeKey::HandoffCall(call.call_id().clone())),
        ModelInputItem::HandoffOutput(output) => {
            Some(DedupeKey::HandoffOutput(output.call_id().clone()))
        }
        ModelInputItem::McpApprovalRequest(request) => Some(DedupeKey::McpApprovalRequest(
            request.request_id().to_owned(),
        )),
        ModelInputItem::McpApprovalResponse(response) => Some(DedupeKey::McpApprovalResponse(
            response.request_id().to_owned(),
        )),
        ModelInputItem::Message(_)
        | ModelInputItem::McpListTools(_)
        | ModelInputItem::Compaction(_) => None,
    }
}

fn keeps_earliest_anchor(item: &ModelInputItem) -> bool {
    matches!(
        item,
        ModelInputItem::Reasoning(_)
            | ModelInputItem::ToolCall(_)
            | ModelInputItem::HandoffCall(_)
            | ModelInputItem::McpApprovalRequest(_)
    )
}

fn deduplicate_preferring_latest(items: Vec<PendingInputItem>) -> Vec<PendingInputItem> {
    let mut latest = BTreeMap::<DedupeKey, PendingInputItem>::new();
    let mut anchors = BTreeMap::<DedupeKey, usize>::new();

    for (index, entry) in items.iter().enumerate() {
        let Some(key) = dedupe_key(&entry.item) else {
            continue;
        };
        latest.insert(key.clone(), entry.clone());
        if !anchors.contains_key(&key) || !keeps_earliest_anchor(&entry.item) {
            anchors.insert(key, index);
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let Some(key) = dedupe_key(&entry.item) else {
                return Some(entry);
            };
            (anchors.get(&key) == Some(&index)).then(|| {
                // Every anchored key was inserted in `latest` in the pass above.
                latest.remove(&key).unwrap_or(entry)
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PairKey {
    Tool(CallId),
    Handoff(CallId),
}

fn call_pair_key(item: &ModelInputItem) -> Option<PairKey> {
    match item {
        ModelInputItem::ToolCall(call) => Some(PairKey::Tool(call.call_id().clone())),
        ModelInputItem::HandoffCall(call) => Some(PairKey::Handoff(call.call_id().clone())),
        _ => None,
    }
}

fn output_pair_key(item: &ModelInputItem) -> Option<PairKey> {
    match item {
        ModelInputItem::ToolCallOutput(output) => Some(PairKey::Tool(output.call_id().clone())),
        ModelInputItem::HandoffOutput(output) => Some(PairKey::Handoff(output.call_id().clone())),
        _ => None,
    }
}

fn prune_orphans(items: Vec<PendingInputItem>, policy: OrphanPolicy) -> Vec<PendingInputItem> {
    if matches!(policy, OrphanPolicy::Preserve) {
        return items;
    }

    let calls = items
        .iter()
        .filter_map(|entry| call_pair_key(&entry.item))
        .collect::<BTreeSet<_>>();
    let outputs = items
        .iter()
        .filter_map(|entry| output_pair_key(&entry.item))
        .collect::<BTreeSet<_>>();

    let mut dropped = BTreeSet::new();
    for (index, entry) in items.iter().enumerate() {
        if call_pair_key(&entry.item).is_some_and(|key| !outputs.contains(&key))
            || (matches!(policy, OrphanPolicy::DropUnpaired)
                && output_pair_key(&entry.item).is_some_and(|key| !calls.contains(&key)))
        {
            dropped.insert(index);
        }
    }

    // A reasoning item belongs to the next non-reasoning model-emitted item. Responses-compatible
    // endpoints reject a reasoning item that is not followed by that item, so it must survive too.
    // Both failing shapes are handled: the follower was pruned above, or there is no follower at
    // all because history was truncated by a rewind or a compaction. Consecutive reasoning items
    // are all tied to the same follower, so dropping one never changes another one's verdict.
    for index in (0..items.len()).rev() {
        if !matches!(items[index].item, ModelInputItem::Reasoning(_)) {
            continue;
        }
        let follower_survives = ((index + 1)..items.len())
            .find(|next| !matches!(items[*next].item, ModelInputItem::Reasoning(_)))
            .is_some_and(|next| !dropped.contains(&next));
        if !follower_survives {
            dropped.insert(index);
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| (!dropped.contains(&index)).then_some(entry))
        .collect()
}

fn apply_reasoning_id_policy(item: ModelInputItem, policy: ReasoningIdPolicy) -> ModelInputItem {
    match (item, policy) {
        (ModelInputItem::Reasoning(reasoning), ReasoningIdPolicy::Omit) => {
            ModelInputItem::Reasoning(reasoning.without_id())
        }
        (item, ReasoningIdPolicy::Preserve | ReasoningIdPolicy::Omit) => item,
    }
}
