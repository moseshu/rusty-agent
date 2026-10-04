//! Reading a run's history from its session and appending what the run adds to it.
//!
//! A port of `openai-agents-python`'s `run_internal/session_persistence.py`: the input a run starts
//! from is the session's history followed by the new input, or whatever a
//! [`SessionInputCallback`] makes of the two; and the session is appended to only with what is new
//! — the new input before the first model call, and the run's records as they are settled.
//!
//! # What differs from the reference, and why
//!
//! - **The session holds [`RunItem`]s, not model input**, as the `Session` port fixed. New input is
//!   given an [`ItemId`] here before anyone sees it, and "the same item" is decided by that id
//!   where the reference compares object identity; see [`SessionInputCallback`].
//! - **The cursor counts append-only run records**, not per-turn items. An uncertain append also
//!   retains the detached batch and the pre-append tail, as the reference's pending Session write
//!   does. Recovery verifies that exact tail before acknowledging or retrying the write.
//! - **Approval records are not appended.** The reference's session stores only model input, and
//!   its approval placeholders never reach it; this session could hold them, but a history read
//!   back would only filter them out again, and a resumed run answers them from its checkpoint.
//! - **Provider conversation sessions are not here.** The reference strips server item ids for its
//!   `OpenAIConversationsSession` in this module; that belongs to a provider session's own policy.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use ra_core::{
    cancel::CancelScope,
    error::{Error, Result},
    item::{
        CallId, InputItemDigest, InputItemNormalizer, ItemId, ModelInputItem, OrphanPolicy,
        RunItem, RunItemKind,
    },
    session::{Session, SessionInputCallback, SessionSettings},
    state::{PendingSessionWrite, RunId, RunState},
};

/// What a run starts from when it reads its history from a session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInputPlan {
    prepared_for_model: Vec<ModelInputItem>,
    append_for_turn: Vec<RunItem>,
}

impl SessionInputPlan {
    /// The run's model input: history and new input, merged, paired and deduplicated.
    #[must_use]
    pub fn prepared_for_model(&self) -> &[ModelInputItem] {
        &self.prepared_for_model
    }

    /// What the run appends to the session as this turn's input: only the items that are new.
    #[must_use]
    pub fn append_for_turn(&self) -> &[RunItem] {
        &self.append_for_turn
    }

    /// Both halves, by value.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ModelInputItem>, Vec<RunItem>) {
        (self.prepared_for_model, self.append_for_turn)
    }
}

/// Reads `session`'s history and merges it with a run's new `input`.
///
/// The reference's `prepare_input_with_session`. The returned plan's two halves answer different
/// questions: what the model is sent, and what the session is given. They differ whenever history
/// is involved, and with a callback they can differ in either direction — a callback that repeats
/// history sends more than it stores, and one that drops the new input stores nothing.
///
/// The history read is bounded by the session's own settings overlaid with `settings`. Calls in
/// the history without an output are dropped, as is reasoning tied to them; outputs in the history
/// whose call is gone are dropped only when the read was bounded and no callback was given, since
/// only then can the bound be what cut the call off. Calls in the new input are kept whatever they
/// pair with: the caller put them there.
///
/// # Errors
///
/// Returns the session's error when the history cannot be read, and the callback's when it fails.
pub async fn prepare_input_with_session(
    run_id: &RunId,
    input: &[ModelInputItem],
    session: &dyn Session,
    callback: Option<&dyn SessionInputCallback>,
    settings: Option<&SessionSettings>,
) -> Result<SessionInputPlan> {
    let resolved = session
        .session_settings()
        .copied()
        .unwrap_or_default()
        .resolve(settings);
    let history = session.get_items(resolved.limit()).await?;
    let new_input = new_input_items(run_id, input, &history);

    let (combined, history_positions, output_pruning, appended) = match callback {
        None => {
            let history_positions: BTreeSet<usize> = (0..history.len()).collect();
            let output_pruning = resolved.limit().map(|_| history_positions.clone());
            let mut combined = history;
            combined.extend(new_input.iter().cloned());
            (combined, history_positions, output_pruning, new_input)
        }
        Some(callback) => {
            let (combined, history_positions, appended) =
                combine_with_callback(callback, history, new_input).await?;
            (combined, history_positions, None, appended)
        }
    };

    let prepared = project_and_prune(&combined, &history_positions, output_pruning.as_ref());
    let prepared = InputItemNormalizer::new()
        .with_orphan_policy(OrphanPolicy::Preserve)
        .normalize_model_items(&prepared)
        .map_err(|error| {
            Error::caller("the session's history could not be prepared as model input")
                .with_source(error)
        })?
        .into_items();
    Ok(SessionInputPlan {
        prepared_for_model: prepared,
        append_for_turn: appended,
    })
}

/// Appends the records the run has settled since it last did so, and moves the run's count past
/// them.
///
/// The reference's `save_result_to_session` for a turn's items. A run not bound to a session has
/// nothing to append; one whose records are all accounted for appends nothing and does not call
/// the session.
///
/// # Errors
///
/// Returns the session's error when the append fails. The count is not moved then, so the records
/// are still owed.
pub(crate) async fn save_session_items(
    session: &dyn Session,
    state: &mut RunState,
    cancel: &CancelScope,
) -> Result<()> {
    let Some(start) = state.session_persisted_item_count() else {
        return Ok(());
    };
    let end = state.generated_items().len();
    let items: Vec<RunItem> = state.generated_items()[start..]
        .iter()
        .filter(|item| item.is_model_input())
        .cloned()
        .collect();
    if items.is_empty() {
        return state.mark_session_persisted(end);
    }
    append_session_items(session, state, items, end, cancel).await
}

/// Captures a batch before reading or writing the backend, so any failure retains the work.
pub(crate) async fn append_session_items(
    session: &dyn Session,
    state: &mut RunState,
    items: Vec<RunItem>,
    persisted_count: usize,
    cancel: &CancelScope,
) -> Result<()> {
    state.begin_session_write(PendingSessionWrite::new(
        session.session_id().clone(),
        items,
        persisted_count,
    ))?;
    resume_pending_session_write(Some(session), state, cancel).await
}

/// Settles an uncertain append before allowing any further model or tool work.
///
/// The reference's `resume_pending_session_write`: an unchanged tail allows a retry, the exact
/// appended tail acknowledges a lost reply, and a changed or partially committed tail is refused.
/// The host must provide the original backend and serialize access, including restored copies of
/// the checkpoint. There is no backend identity or distributed compare-and-swap contract.
pub(crate) async fn resume_pending_session_write(
    session: Option<&dyn Session>,
    state: &mut RunState,
    cancel: &CancelScope,
) -> Result<()> {
    let Some(pending) = state.pending_session_write().cloned() else {
        return Ok(());
    };
    let session = session
        .filter(|session| session.session_id() == pending.session_id())
        .ok_or_else(|| {
            Error::caller(
                "resume the pending Session write with the original Session and session ID",
            )
        })?;
    let append = match pending.before() {
        None => {
            let tail = cancel
                .run(session.get_items(Some(pending.items().len() + 1)))
                .await??;
            let before = item_digests(&tail)?;
            state
                .pending_session_write_mut()
                .ok_or_else(|| {
                    Error::caller("the pending Session batch disappeared before acknowledgement")
                })?
                .set_before(before);
            true
        }
        Some(before) => {
            let mut expected = before.to_vec();
            expected.extend(item_digests(pending.items())?);
            let tail = cancel
                .run(session.get_items(Some(expected.len())))
                .await??;
            let observed = item_digests(&tail)?;
            let committed = observed == expected;
            let unchanged = if before.is_empty() {
                observed.is_empty()
            } else {
                observed.ends_with(before)
            };
            if committed == unchanged {
                return Err(Error::caller(
                    "cannot reconcile the pending Session write: history changed or is ambiguous; repair the original Session before resuming and do not rerun the completed tool",
                ));
            }
            !committed
        }
    };
    if append {
        cancel
            .run(session.add_items(pending.items().to_vec()))
            .await??;
    }
    state.finish_session_write()
}

fn item_digests(items: &[RunItem]) -> Result<Vec<InputItemDigest>> {
    items
        .iter()
        .map(|item| {
            InputItemDigest::compute_session_item(item).map_err(|error| {
                Error::caller("Session item could not be fingerprinted").with_source(error)
            })
        })
        .collect()
}

/// Gives each item of a run's new input an identity no history item has.
///
/// Derived from the run so it is stable for one run, and checked against the history because a
/// host may reuse a run id across runs of one session: an input item sharing an id with history
/// would be taken for the history item by the callback reconciliation.
fn new_input_items(run_id: &RunId, input: &[ModelInputItem], history: &[RunItem]) -> Vec<RunItem> {
    let mut taken: HashSet<ItemId> = history.iter().map(|item| item.id().clone()).collect();
    input
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mut suffix = 0_u32;
            let id = loop {
                let candidate = if suffix == 0 {
                    ItemId::new(format!("{run_id}.input.{index}"))
                } else {
                    ItemId::new(format!("{run_id}.input.{index}.{suffix}"))
                };
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                suffix = suffix.saturating_add(1);
            };
            RunItem::new(id, RunItemKind::from(item.clone()))
        })
        .collect()
}

/// Runs the callback and decides which of its output items are history and which are new.
///
/// Returns the callback's output, the positions in it that are history, and the new items in
/// output order. The decision follows the reference item by item, in this order: an item matched
/// to an unconsumed new-input item by identity is new (unless its identity is a history item's);
/// one matched to an unconsumed history item by identity is history; any other item whose identity
/// was handed in as history is history; then an item equal to a history item not yet matched is
/// history, one equal to a new-input item not yet matched is new, and anything left is new.
async fn combine_with_callback(
    callback: &dyn SessionInputCallback,
    history: Vec<RunItem>,
    new_input: Vec<RunItem>,
) -> Result<(Vec<RunItem>, BTreeSet<usize>, Vec<RunItem>)> {
    let original_history_ids: HashSet<ItemId> =
        history.iter().map(|item| item.id().clone()).collect();
    let mut history_for_callback = history;
    let mut new_for_callback = new_input;
    let combined = callback
        .combine(&mut history_for_callback, &mut new_for_callback)
        .await?;

    // Built from the lists as the callback left them: what it wrote into either list counts as
    // belonging to that list.
    let mut history_refs = reference_map(&history_for_callback);
    let mut new_refs = reference_map(&new_for_callback);
    let mut history_counts = frequency_map(&history_for_callback);
    let mut new_counts = frequency_map(&new_for_callback);

    let mut history_positions = BTreeSet::new();
    let mut appended = Vec::new();
    for (position, item) in combined.iter().enumerate() {
        let key = item_key(item);
        if consume_reference(&mut new_refs, &key, item.id()) {
            decrement(&mut new_counts, &key);
            if original_history_ids.contains(item.id()) {
                history_positions.insert(position);
            } else {
                appended.push(item.clone());
            }
            continue;
        }
        if consume_reference(&mut history_refs, &key, item.id()) {
            decrement(&mut history_counts, &key);
            history_positions.insert(position);
            continue;
        }
        if original_history_ids.contains(item.id()) {
            history_positions.insert(position);
            continue;
        }
        if history_counts.get(&key).copied().unwrap_or(0) > 0 {
            decrement(&mut history_counts, &key);
            history_positions.insert(position);
            continue;
        }
        if new_counts.get(&key).copied().unwrap_or(0) > 0 {
            decrement(&mut new_counts, &key);
        }
        appended.push(item.clone());
    }
    Ok((combined, history_positions, appended))
}

/// What two items are compared by: the digest of what the model would be sent, or, for a record
/// that is not model input, its serialized payload.
fn item_key(item: &RunItem) -> String {
    let key = match item.to_model_input() {
        Some(input) => InputItemDigest::compute(&input).map(|digest| digest.as_str().to_owned()),
        None => serde_json::to_string(item.kind()),
    };
    key.unwrap_or_else(|_| format!("{:?}", item.kind()))
}

fn reference_map(items: &[RunItem]) -> BTreeMap<String, Vec<ItemId>> {
    let mut refs: BTreeMap<String, Vec<ItemId>> = BTreeMap::new();
    for item in items {
        refs.entry(item_key(item))
            .or_default()
            .push(item.id().clone());
    }
    refs
}

fn frequency_map(items: &[RunItem]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        *counts.entry(item_key(item)).or_default() += 1;
    }
    counts
}

/// Removes the reference `id` holds under `key`, if it holds one.
fn consume_reference(refs: &mut BTreeMap<String, Vec<ItemId>>, key: &str, id: &ItemId) -> bool {
    let Some(candidates) = refs.get_mut(key) else {
        return false;
    };
    let Some(index) = candidates.iter().position(|candidate| candidate == id) else {
        return false;
    };
    candidates.remove(index);
    if candidates.is_empty() {
        refs.remove(key);
    }
    true
}

fn decrement(counts: &mut BTreeMap<String, usize>, key: &str) {
    if let Some(count) = counts.get_mut(key) {
        *count = count.saturating_sub(1);
    }
}

/// Which pairing a call or an output takes part in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PairKey {
    Tool(CallId),
    Handoff(CallId),
}

fn call_key(item: &ModelInputItem) -> Option<PairKey> {
    match item {
        ModelInputItem::ToolCall(call) => Some(PairKey::Tool(call.call_id().clone())),
        ModelInputItem::HandoffCall(call) => Some(PairKey::Handoff(call.call_id().clone())),
        _ => None,
    }
}

fn output_key(item: &ModelInputItem) -> Option<PairKey> {
    match item {
        ModelInputItem::ToolCallOutput(output) => Some(PairKey::Tool(output.call_id().clone())),
        ModelInputItem::HandoffOutput(output) => Some(PairKey::Handoff(output.call_id().clone())),
        _ => None,
    }
}

/// Projects the merged records to model input and drops the unpaired items history brought in.
///
/// The reference's `drop_orphan_function_calls` with its index sets: only calls at `history`
/// positions may be dropped for lacking an output, and only outputs at `output_pruning` positions
/// for lacking a call that survived. Reasoning immediately ahead of a dropped call goes with it,
/// since a provider rejects reasoning without the item it was tied to.
fn project_and_prune(
    combined: &[RunItem],
    history: &BTreeSet<usize>,
    output_pruning: Option<&BTreeSet<usize>>,
) -> Vec<ModelInputItem> {
    // Positions are carried through the projection: an approval record has no model input, and
    // dropping it must not shift which items count as history.
    let projected: Vec<(usize, ModelInputItem)> = combined
        .iter()
        .enumerate()
        .filter_map(|(position, item)| item.to_model_input().map(|input| (position, input)))
        .collect();

    let completed: BTreeSet<PairKey> = projected
        .iter()
        .filter_map(|(_, item)| output_key(item))
        .collect();
    let mut dropped = BTreeSet::new();
    for (index, (position, item)) in projected.iter().enumerate() {
        if history.contains(position) && call_key(item).is_some_and(|key| !completed.contains(&key))
        {
            dropped.insert(index);
        }
    }
    let triggers = dropped.clone();

    if let Some(output_pruning) = output_pruning {
        let available: BTreeSet<PairKey> = projected
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .filter_map(|(_, (_, item))| call_key(item))
            .collect();
        for (index, (position, item)) in projected.iter().enumerate() {
            if output_pruning.contains(position)
                && output_key(item).is_some_and(|key| !available.contains(&key))
            {
                dropped.insert(index);
            }
        }
    }

    let mut dropped_reasoning = BTreeSet::new();
    for index in (0..projected.len()).rev() {
        if dropped.contains(&index) || !matches!(projected[index].1, ModelInputItem::Reasoning(_)) {
            continue;
        }
        let follower = ((index + 1)..projected.len()).find(|next| {
            !dropped_reasoning.contains(next)
                && !matches!(projected[*next].1, ModelInputItem::Reasoning(_))
        });
        if follower.is_some_and(|next| triggers.contains(&next)) {
            dropped_reasoning.insert(index);
        }
    }

    projected
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !dropped.contains(index) && !dropped_reasoning.contains(index))
        .map(|(_, (_, item))| item)
        .collect()
}
