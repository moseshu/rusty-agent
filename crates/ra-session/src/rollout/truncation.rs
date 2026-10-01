//! Cuts a rollout at a run boundary, to rebuild the session as it stood before or after that run.
//!
//! Ported from Codex's `truncate_rollout_before_turn_id` and `truncate_rollout_after_turn_id`
//! (`core/src/thread_rollout_truncation.rs`), which is how Codex goes back to an earlier turn now
//! that it has removed live rollback: the records are cut, nothing is written into the rollout, and
//! the history is rebuilt from what is left. Codex's turn is a run here, and a run's boundary is
//! the start of its first segment.

use ra_core::{
    error::{Error, Result},
    state::RunId,
};

use super::writer::{RolloutPayload, RolloutRecord};

/// The records before the run `before_run` started: the rollout as it stood before that run.
///
/// # Errors
///
/// Returns an error if the rollout records no start of `before_run`, or if a run start cannot be
/// read.
pub fn truncate_rollout_before_run(
    mut records: Vec<RolloutRecord>,
    before_run: &RunId,
) -> Result<Vec<RolloutRecord>> {
    let mut cut = None;
    for (index, record) in records.iter().enumerate() {
        if starts(record, Some(before_run))? {
            cut = Some(index);
            break;
        }
    }
    let Some(cut) = cut else {
        return Err(Error::caller(format!(
            "run `{before_run}` was not found in the rollout"
        )));
    };
    records.truncate(cut);
    Ok(records)
}

/// The records up to the start of the run after `last_run`: the rollout as it stood once that run
/// ended.
///
/// Everything recorded before the next run started is kept, including events of `last_run`
/// recorded after it ended.
///
/// # Errors
///
/// Returns an error if the rollout records no start of `last_run`, if its latest segment recorded
/// no end — it is still in progress, or the process running it stopped — or if a run record cannot
/// be read.
pub fn truncate_rollout_after_run(
    mut records: Vec<RolloutRecord>,
    last_run: &RunId,
) -> Result<Vec<RolloutRecord>> {
    let mut last_start = None;
    for (index, record) in records.iter().enumerate() {
        if starts(record, Some(last_run))? {
            last_start = Some(index);
        }
    }
    let Some(last_start) = last_start else {
        return Err(Error::caller(format!(
            "run `{last_run}` was not found in the rollout"
        )));
    };

    let mut ended = false;
    let mut cut = records.len();
    for (index, record) in records.iter().enumerate().skip(last_start + 1) {
        if starts(record, None)? {
            cut = index;
            break;
        }
        if !ended && record.type_name() == "run_ended" {
            ended = matches!(
                record.payload()?,
                RolloutPayload::RunEnded(end) if end.run_id() == last_run
            );
        }
    }
    if !ended {
        return Err(Error::caller(format!(
            "run `{last_run}` is in progress in the rollout"
        )));
    }
    records.truncate(cut);
    Ok(records)
}

/// Whether `record` starts a run segment, of `run` if given.
fn starts(record: &RolloutRecord, run: Option<&RunId>) -> Result<bool> {
    if record.type_name() != "run_started" {
        return Ok(false);
    }
    Ok(match record.payload()? {
        RolloutPayload::RunStarted(started) => run.is_none_or(|run| started.run_id() == run),
        _ => false,
    })
}
