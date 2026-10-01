//! Rebuilds a session's history from its rollout.
//!
//! Ported from Codex's `reconstruct_history_from_rollout` (`core/src/session/rollout_reconstruction.rs`):
//! replay the records a session's runs wrote and return the model-visible history a continuation
//! starts from, together with what a resume needs to know about the newest run — which run started
//! last and how it ended, and the turn context it ran in.
//!
//! # What the history is
//!
//! Codex's turn is a run here. Each run contributes, in rollout order, the new input it started on
//! and the records it added to the session — as Codex's history holds each turn's user message and
//! its response items, and as `openai-agents-python` saves a run's new input and new items to its
//! `Session`. A segment that continues a run from its checkpoint adds whatever new input it was
//! given, under the same run.
//!
//! A segment whose caller supplied the input it runs on, in place of the checkpoint's history, is
//! different: that input is the run's history as the caller projected it, followed by what the
//! caller added, and the segment's model calls start from it. Its start is recorded as a
//! continuation base, and the base replaces what the run had recorded before it; earlier runs are
//! kept.
//!
//! A record recorded again under its item id replaces the first copy in place. The runner does
//! this when settlement changes a record it had already recorded. Item ids are only unique within a
//! run, so this is decided per run.
//!
//! # Compaction
//!
//! Codex bounds replay at the newest compaction and takes its replacement history as the base. A
//! compaction here does not carry a replacement; it says what it replaces:
//!
//! - A [`Compaction`](ra_core::item::Compaction) names the records it stands for, which are records
//!   of the run that wrote it. They drop out of the history, as they drop out of the model-visible
//!   view while the run is live.
//! - A [`ProviderCompaction`](ra_core::item::ProviderCompaction) stands for everything before it,
//!   so the history starts at the last one — as Codex's remote compaction replaces the history
//!   before it, and as `openai-agents-python`'s compaction session replaces its stored items.
//!
//! # Runs that did not complete
//!
//! As in Codex, a run that was cancelled, failed, paused for approval or never recorded an end
//! keeps the records it wrote: they are what happened, and the model's next request is built from
//! them. A call it made whose output never arrived is kept as recorded; pairing calls with outputs
//! is left to input normalization when the next request is built, where an unpaired call is
//! dropped. [`RolloutReconstruction::last_run`] reports how the newest run ended, so a host can
//! tell a run to resume from its checkpoint from one that finished or one the process died in.
//!
//! # What is not ported
//!
//! Codex's rollback markers are not read: Codex removed live rollback, and a rollout written here
//! never holds one. Going back to an earlier run is done by truncating the records at a run
//! boundary before reconstructing; see [`truncate_rollout_before_run`](super::truncate_rollout_before_run).
//! Codex's context baseline (`reference_context_item`), world-state replay and context-window
//! numbering have no counterpart in this framework's records.
//!
//! # A new run's input is taken to be new
//!
//! The input a new run starts on is read as new to the session. A host that continues a
//! conversation by starting a new run on the whole transcript, rather than through a session or by
//! continuing the run from its checkpoint, records that transcript again with every run, and it is
//! repeated here.

use std::collections::{HashMap, HashSet};

use ra_core::{
    error::Result,
    item::{AgentId, ItemId, ModelInputItem, RunItem, RunItemKind},
    session::rollout::{RolloutRunEnded, RolloutRunStarted, RolloutTurnContext},
    state::RunId,
};

use super::writer::{RolloutPayload, RolloutRecord};

/// A session's history rebuilt from its rollout, and what a resume needs to know about its newest
/// run.
///
/// Codex's `RolloutReconstruction`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutReconstruction {
    history: Vec<ModelInputItem>,
    last_run: Option<ReconstructedRun>,
    turn_context: Option<RolloutTurnContext>,
}

impl RolloutReconstruction {
    /// The model-visible history, oldest first: the input to continue the session from.
    #[must_use]
    pub fn history(&self) -> &[ModelInputItem] {
        &self.history
    }

    /// Takes the history.
    #[must_use]
    pub fn into_history(self) -> Vec<ModelInputItem> {
        self.history
    }

    /// The run that started last, and how its latest segment ended: Codex's last started turn.
    #[must_use]
    pub const fn last_run(&self) -> Option<&ReconstructedRun> {
        self.last_run.as_ref()
    }

    /// The newest turn context recorded: the model and effort the session last ran with.
    #[must_use]
    pub const fn turn_context(&self) -> Option<&RolloutTurnContext> {
        self.turn_context.as_ref()
    }
}

/// The run that started last in a rollout.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ReconstructedRun {
    run_id: RunId,
    agent_id: AgentId,
    parent_run_id: Option<RunId>,
    end: Option<RolloutRunEnded>,
}

impl ReconstructedRun {
    fn started(started: &RolloutRunStarted) -> Self {
        Self {
            run_id: started.run_id().clone(),
            agent_id: started.agent_id().clone(),
            parent_run_id: started.parent_run_id().cloned(),
            end: None,
        }
    }

    /// The run.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The agent its latest segment started with.
    #[must_use]
    pub const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// The run that started it, if any.
    #[must_use]
    pub const fn parent_run_id(&self) -> Option<&RunId> {
        self.parent_run_id.as_ref()
    }

    /// How its latest segment ended, or `None` if no end was recorded: the run is still going, or
    /// the process running it stopped before it could say.
    #[must_use]
    pub const fn end(&self) -> Option<&RolloutRunEnded> {
        self.end.as_ref()
    }
}

/// One entry of the history being rebuilt, with the run it belongs to.
struct Entry {
    run: Option<RunId>,
    recorded: Recorded,
}

enum Recorded {
    Input(ModelInputItem),
    Item(Box<RunItem>),
}

/// Rebuilds the history `records` describe; see the [module documentation](self).
///
/// Records of kinds this build does not know are skipped, as are child anchors, checkpoints, host
/// events and usage, none of which is history.
///
/// # Errors
///
/// Returns an error if a record of a kind this build knows cannot be read: leaving it out would
/// hand the model a history with a hole in it.
pub fn reconstruct_history(records: &[RolloutRecord]) -> Result<RolloutReconstruction> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut positions: HashMap<(Option<RunId>, ItemId), usize> = HashMap::new();
    let mut current: Option<RunId> = None;
    let mut last_run: Option<ReconstructedRun> = None;
    let mut turn_context = None;

    for record in records {
        match record.payload()? {
            RolloutPayload::RunStarted(started) => {
                if started.input_is_continuation_base() {
                    entries.retain(|entry| entry.run.as_ref() != Some(started.run_id()));
                    positions = item_positions(&entries);
                }
                current = Some(started.run_id().clone());
                entries.extend(started.input().iter().cloned().map(|item| Entry {
                    run: current.clone(),
                    recorded: Recorded::Input(item),
                }));
                last_run = Some(ReconstructedRun::started(&started));
            }
            RolloutPayload::Item(item) => {
                let key = (current.clone(), item.id().clone());
                if let Some(&position) = positions.get(&key) {
                    entries[position].recorded = Recorded::Item(Box::new(item));
                } else {
                    positions.insert(key, entries.len());
                    entries.push(Entry {
                        run: current.clone(),
                        recorded: Recorded::Item(Box::new(item)),
                    });
                }
            }
            RolloutPayload::RunEnded(ended) => {
                if let Some(run) = last_run
                    .as_mut()
                    .filter(|run| run.run_id == *ended.run_id())
                {
                    run.end = Some(ended);
                }
            }
            RolloutPayload::TurnContext(context) => turn_context = Some(context),
            _ => {}
        }
    }

    Ok(RolloutReconstruction {
        history: model_visible(&entries),
        last_run,
        turn_context,
    })
}

/// Where each run's records sit in `entries`, keyed by run and item id.
fn item_positions(entries: &[Entry]) -> HashMap<(Option<RunId>, ItemId), usize> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| match &entry.recorded {
            Recorded::Item(item) => Some(((entry.run.clone(), item.id().clone()), position)),
            Recorded::Input(_) => None,
        })
        .collect()
}

/// Applies the compactions that survive in `entries` and projects the rest to model input.
fn model_visible(entries: &[Entry]) -> Vec<ModelInputItem> {
    let replaced: HashSet<(Option<&RunId>, &ItemId)> = entries
        .iter()
        .filter_map(|entry| match &entry.recorded {
            Recorded::Item(item) => match item.kind() {
                RunItemKind::Compaction(compaction) => Some(
                    compaction
                        .compacted_items()
                        .iter()
                        .map(move |id| (entry.run.as_ref(), id)),
                ),
                _ => None,
            },
            Recorded::Input(_) => None,
        })
        .flatten()
        .collect();
    let surviving: Vec<&Entry> = entries
        .iter()
        .filter(|entry| match &entry.recorded {
            Recorded::Item(item) => !replaced.contains(&(entry.run.as_ref(), item.id())),
            Recorded::Input(_) => true,
        })
        .collect();
    let start = surviving
        .iter()
        .rposition(|entry| {
            matches!(
                &entry.recorded,
                Recorded::Item(item) if matches!(item.kind(), RunItemKind::ProviderCompaction(_))
            )
        })
        .unwrap_or(0);
    surviving[start..]
        .iter()
        .filter_map(|entry| match &entry.recorded {
            Recorded::Input(item) => Some(item.clone()),
            Recorded::Item(item) => item.to_model_input(),
        })
        .collect()
}
