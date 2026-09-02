//! Retention facts for completed tool outputs in one resumable run.
//!
//! The runtime records which outputs were produced in each completed turn, while a product may
//! report typed references it observed in a model response. The ledger deliberately never infers a
//! reference from free text: a provider-neutral response has no structured meaning for a string
//! that happens to contain a call ID.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
    item::CallId,
    state::RunId,
};

/// Current schema version of the persisted tool-output reference ledger.
pub const TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(2);

const fn tool_output_reference_schema_version() -> SchemaVersion {
    TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION
}

const TOOL_OUTPUT_REFERENCE_RECORD_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn tool_output_reference_record_schema_version() -> SchemaVersion {
    TOOL_OUTPUT_REFERENCE_RECORD_SCHEMA_VERSION
}

/// Durable per-run record of when individual tool outputs were last explicitly referenced.
///
/// The ledger is append-only for the life of its run. A result record cannot be discarded after
/// its complete content has been projected away: an unknown call ID is deliberately retained by
/// context policies, so forgetting it would restore the complete result to later model requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolOutputReferenceTracker {
    schema_version: SchemaVersion,
    run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_completed_turn: Option<u64>,
    records: BTreeMap<CallId, ReferenceRecord>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

#[derive(Deserialize)]
struct ToolOutputReferenceTrackerWire {
    #[serde(default = "tool_output_reference_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    #[serde(default)]
    last_completed_turn: Option<u64>,
    #[serde(default)]
    records: BTreeMap<CallId, ReferenceRecord>,
    #[serde(flatten, default)]
    unknown: Unknown,
}

/// One output's first and most recent observed reference turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReferenceRecord {
    #[serde(default = "tool_output_reference_record_schema_version")]
    schema_version: SchemaVersion,
    created_turn: u64,
    last_referenced_turn: u64,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ReferenceRecord {
    fn produced_in(turn: u64) -> Self {
        Self {
            schema_version: TOOL_OUTPUT_REFERENCE_RECORD_SCHEMA_VERSION,
            created_turn: turn,
            last_referenced_turn: turn,
            unknown: Unknown::new(),
        }
    }
}

impl<'de> Deserialize<'de> for ToolOutputReferenceTracker {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolOutputReferenceTrackerWire::deserialize(deserializer)?;
        if let Some((call_id, record)) = wire
            .records
            .iter()
            .find(|(_, record)| record.last_referenced_turn < record.created_turn)
        {
            return Err(D::Error::custom(format!(
                "tool-output reference `{call_id}` was last referenced on turn {}, before it was \
                 produced on turn {}",
                record.last_referenced_turn, record.created_turn
            )));
        }
        let record_high_water = wire
            .records
            .values()
            .map(|record| record.last_referenced_turn)
            .max();
        let last_completed_turn = wire.last_completed_turn.or(record_high_water);
        if let (Some(last_completed_turn), Some(record_high_water)) =
            (last_completed_turn, record_high_water)
            && last_completed_turn < record_high_water
        {
            return Err(D::Error::custom(format!(
                "tool-output reference ledger completed turn {last_completed_turn}, before a \
                 recorded reference from turn {record_high_water}"
            )));
        }
        let schema_version = if wire.schema_version <= TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION {
            TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION
        } else {
            wire.schema_version
        };
        Ok(Self {
            schema_version,
            run_id: wire.run_id,
            last_completed_turn,
            records: wire.records,
            unknown: wire.unknown,
        })
    }
}

impl ToolOutputReferenceTracker {
    /// Starts an empty reference ledger for one run.
    #[must_use]
    pub fn new(run_id: RunId) -> Self {
        Self {
            schema_version: TOOL_OUTPUT_REFERENCE_SCHEMA_VERSION,
            run_id,
            last_completed_turn: None,
            records: BTreeMap::new(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Run whose call IDs this tracker may describe.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Records outputs produced and outputs explicitly referenced by one completed turn.
    ///
    /// New outputs are registered before same-turn references. Replaying an existing reference
    /// never moves its timestamp backwards, while registering a new output in an older turn is
    /// rejected because it would make a live result appear stale immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when a reference names no registered output, when a reference predates its
    /// output, when a call ID is registered in two different turns, or when a new output is added
    /// to a turn the ledger has already passed.
    pub fn record_turn(
        &mut self,
        turn: u64,
        new_outputs: impl IntoIterator<Item = CallId>,
        referenced_outputs: impl IntoIterator<Item = CallId>,
    ) -> Result<()> {
        let new_outputs: BTreeSet<_> = new_outputs.into_iter().collect();
        let referenced_outputs: BTreeSet<_> = referenced_outputs.into_iter().collect();

        for call_id in &new_outputs {
            if let Some(record) = self.records.get(call_id)
                && record.created_turn != turn
            {
                return Err(Error::caller(format!(
                    "tool output `{call_id}` was already registered for turn {}; it cannot be \
                     registered again for turn {turn}",
                    record.created_turn
                )));
            }
        }
        if let Some(highest) = self.last_completed_turn
            && turn < highest
            && let Some(call_id) = new_outputs
                .iter()
                .find(|call_id| !self.records.contains_key(*call_id))
        {
            return Err(Error::caller(format!(
                "tool output `{call_id}` cannot be registered for turn {turn}: run `{}` has \
                 already recorded turn {highest}",
                self.run_id
            )));
        }
        for call_id in &referenced_outputs {
            let created_turn = new_outputs
                .contains(call_id)
                .then_some(turn)
                .or_else(|| self.records.get(call_id).map(|record| record.created_turn));
            let Some(created_turn) = created_turn else {
                return Err(Error::caller(format!(
                    "tool-output reference `{call_id}` does not name an output registered in run `{}`",
                    self.run_id
                )));
            };
            if turn < created_turn {
                return Err(Error::caller(format!(
                    "tool-output reference `{call_id}` at turn {turn} predates its output from \
                     turn {created_turn}"
                )));
            }
        }

        for call_id in new_outputs {
            self.records
                .entry(call_id)
                .or_insert_with(|| ReferenceRecord::produced_in(turn));
        }
        for call_id in referenced_outputs {
            let record = self
                .records
                .entry(call_id)
                .or_insert_with(|| ReferenceRecord::produced_in(turn));
            record.last_referenced_turn = record.last_referenced_turn.max(turn);
        }
        self.last_completed_turn = Some(
            self.last_completed_turn
                .map_or(turn, |last_completed_turn| last_completed_turn.max(turn)),
        );
        Ok(())
    }

    /// Most recent completed turn this ledger has observed.
    ///
    /// A resumed segment reads this to continue the run's turn axis instead of restarting its
    /// count. Staleness is measured in completed turns of the whole run, so a counter that resets
    /// per segment would both re-open a turn already recorded and make every earlier result look
    /// newer than the segment now running.
    ///
    /// It is stored rather than derived from output records because a completed turn may have no
    /// tool output or typed reference at all. Older checkpoints did not carry this field, so they
    /// conservatively recover their high-water mark from the latest retained reference.
    #[must_use]
    pub const fn last_completed_turn(&self) -> Option<u64> {
        self.last_completed_turn
    }

    /// Most recent turn that explicitly referenced this output, including its producing turn.
    #[must_use]
    pub fn last_referenced_turn(&self, call_id: &CallId) -> Option<u64> {
        self.records
            .get(call_id)
            .map(|record| record.last_referenced_turn)
    }

    /// Whether this output has completed at least `max_unreferenced_turns` turns unreferenced.
    #[must_use]
    pub fn is_unreferenced_for(
        &self,
        call_id: &CallId,
        current_turn: u64,
        max_unreferenced_turns: u64,
    ) -> bool {
        self.last_referenced_turn(call_id)
            .and_then(|last| current_turn.checked_sub(last)?.checked_sub(1))
            .is_some_and(|completed_unreferenced| completed_unreferenced >= max_unreferenced_turns)
    }
}
