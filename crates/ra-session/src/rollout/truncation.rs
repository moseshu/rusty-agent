//! Cuts a rollout at a run boundary, to rebuild the session as it stood before or after that run,
//! or before a user message, to fork it there.
//!
//! Ported from Codex's `core/src/thread_rollout_truncation.rs`. Its `truncate_rollout_before_turn_id`
//! and `truncate_rollout_after_turn_id` are how Codex goes back to an earlier turn now that it has
//! removed live rollback: the records are cut, nothing is written into the rollout, and the history
//! is rebuilt from what is left. Codex's turn is a run here, and a run's boundary is the start of
//! its first segment.
//!
//! Its `user_message_positions_in_rollout` and `truncate_rollout_before_nth_user_message_from_start`
//! cut where a fork starts. A user message enters a rollout here as part of the input a run, or a
//! segment of it, starts on, and one start may carry several, so a position names the start record
//! and the message's place in its input. Cutting before a message that is not the first thing the
//! start carries keeps the start with the input before that message, as Codex keeps the items
//! recorded before the user message it cuts at; otherwise the cut falls before the record. Codex's
//! rollback markers are not applied: a rollout written here never holds one.

use ra_core::{
    agent::control::InterAgentMessageKind,
    error::{Error, Result},
    item::{MessageRole, ModelInputItem},
    session::interrupt::is_interrupted_turn_marker,
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

/// Where a user message sits in a rollout: the run start whose input carries it, and its place in
/// that input.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserMessagePosition {
    record: usize,
    input: usize,
}

impl UserMessagePosition {
    /// The index of the run start record in the rollout.
    #[must_use]
    pub const fn record(&self) -> usize {
        self.record
    }

    /// The index of the message in that start's input.
    #[must_use]
    pub const fn input(&self) -> usize {
        self.input
    }
}

/// Where the user messages in `records` are, in order: Codex's
/// `user_message_positions_in_rollout`.
///
/// A user message is a message with the user's role that a run, or a segment of it, starts on and
/// that is neither an inter-agent envelope nor an interrupted-run marker, as Codex counts only the
/// messages it parses as user messages: inter-agent communication has an item of its own there,
/// and the marker is a context fragment. A continuation base is the run's history supplied again
/// in place of what it recorded, not new input, and holds none.
///
/// # Errors
///
/// Returns an error if a run start cannot be read.
pub fn user_message_positions_in_rollout(
    records: &[RolloutRecord],
) -> Result<Vec<UserMessagePosition>> {
    let mut positions = Vec::new();
    for (record, rollout_record) in records.iter().enumerate() {
        if rollout_record.type_name() != "run_started" {
            continue;
        }
        if let RolloutPayload::RunStarted(started) = rollout_record.payload()?
            && !started.input_is_continuation_base()
        {
            positions.extend(
                started
                    .input()
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| is_user_message(item))
                    .map(|(input, _)| UserMessagePosition { record, input }),
            );
        }
    }
    Ok(positions)
}

/// The records strictly before the `n`th user message, counted from zero: Codex's
/// `truncate_rollout_before_nth_user_message_from_start`.
///
/// `n == 0` keeps only what precedes the first user message. When the rollout holds `n` or fewer
/// user messages, including for `usize::MAX`, it is returned whole. User messages are those
/// [`user_message_positions_in_rollout`] finds; a cut inside a run start's input keeps the start
/// with the input before the message.
///
/// # Errors
///
/// Returns an error if a run start cannot be read.
pub fn truncate_rollout_before_nth_user_message(
    mut records: Vec<RolloutRecord>,
    n: usize,
) -> Result<Vec<RolloutRecord>> {
    if n == usize::MAX {
        return Ok(records);
    }
    let Some(&position) = user_message_positions_in_rollout(&records)?.get(n) else {
        return Ok(records);
    };
    if position.input == 0 {
        records.truncate(position.record);
        return Ok(records);
    }
    let mut payload = records[position.record].payload_value().clone();
    let Some(input) = payload
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Err(Error::session(
            ra_core::error::SessionErrorKind::Corrupted,
            "a run start with user input recorded no input array",
        ));
    };
    input.truncate(position.input);
    let kept = records[position.record].with_payload_value(payload);
    records.truncate(position.record);
    records.push(kept);
    Ok(records)
}

/// Whether `item` is a message from the user, as [`user_message_positions_in_rollout`] counts them.
pub(crate) fn is_user_message(item: &ModelInputItem) -> bool {
    matches!(
        item,
        ModelInputItem::Message(message)
            if message.role() == MessageRole::User
                && InterAgentMessageKind::of(message).is_none()
                && !is_interrupted_turn_marker(message)
    )
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
