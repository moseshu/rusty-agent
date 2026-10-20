// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Deterministic reduction of local observations. Replay performs no model or tool calls.

use crate::trace::{RolloutTraceRecord, TraceError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Reconstructed span topology and final safe metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayedSpan {
    /// Stable trace identity.
    trace_id: String,
    /// Stable span identity.
    span_id: String,
    /// Captured parent span identity.
    parent_id: Option<String>,
    /// Span creation sequence.
    started_seq: u64,
    /// Span close sequence, absent for captures taken while it was active.
    ended_seq: Option<u64>,
    /// Safe fields after ordered updates.
    fields: BTreeMap<String, Value>,
}

/// Read-only local replay result. Content is present only where explicitly captured.
///
/// `events` retains the exact request/response/item/guard/NextStep evidence in sequence order.
/// Guard spans remain ordinary span observations; tool items retain call IDs in metadata. This
/// reducer checks artifact integrity, leaving task-contract assertions to the assertion layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayedTrace {
    /// Reconstructed spans indexed by export identity.
    spans: BTreeMap<String, ReplayedSpan>,
    /// Original ordered observations, including unknown event kinds.
    events: Vec<RolloutTraceRecord>,
}

impl ReplayedTrace {
    /// Iterates observations of the requested stable kind in capture order.
    pub fn events_of_kind<'a>(
        &'a self,
        kind: &'a str,
    ) -> impl Iterator<Item = &'a RolloutTraceRecord> {
        self.events
            .iter()
            .filter(move |record| record.event().kind() == kind)
    }
}

/// Replays capture streams in persisted order, preserving topology without payloads.
///
/// Sequence gaps, unsupported schemas, unknown span references, inconsistent parent/trace IDs,
/// duplicate starts and updates after close fail explicitly. Open spans are retained as open,
/// because a snapshot or a recovered rollout can end while work was in progress. Unknown event
/// kinds are preserved without guessing their meaning. No code in the artifact is executed.
pub fn replay(records: &[RolloutTraceRecord]) -> Result<ReplayedTrace, TraceError> {
    let mut spans: BTreeMap<String, ReplayedSpan> = BTreeMap::new();
    let mut sequences: BTreeMap<&str, u64> = BTreeMap::new();
    for record in records {
        let expected = sequences.entry(record.capture_id()).or_insert(1);
        if record.schema_version() != 1
            || record.seq() != *expected
            || record.trace_id().is_empty()
            || record.capture_id().is_empty()
        {
            return Err(invalid(
                record,
                "unsupported schema, sequence gap or empty trace ID",
            ));
        }
        *expected += 1;
        let fields = record
            .event()
            .fields()
            .as_object()
            .ok_or_else(|| invalid(record, "metadata must be an object"))?;
        let identity = record.span_id();
        if record.event().kind() == "span_started" {
            let id = identity.ok_or_else(|| invalid(record, "span start without ID"))?;
            if id.is_empty() || spans.contains_key(id) {
                return Err(invalid(record, "duplicate or empty span ID"));
            }
            if let Some(parent) = record.parent_id() {
                let parent = spans
                    .get(parent)
                    .ok_or_else(|| invalid(record, "unknown parent span"))?;
                if parent.trace_id != record.trace_id() {
                    return Err(invalid(record, "parent belongs to another trace"));
                }
            }
            spans.insert(
                id.to_owned(),
                ReplayedSpan {
                    trace_id: record.trace_id().to_owned(),
                    span_id: id.to_owned(),
                    parent_id: record.parent_id().map(str::to_owned),
                    started_seq: record.seq(),
                    ended_seq: None,
                    fields: fields.clone().into_iter().collect(),
                },
            );
        } else if let Some(id) = identity {
            let span = spans
                .get_mut(id)
                .ok_or_else(|| invalid(record, "unknown span reference"))?;
            if span.trace_id != record.trace_id() || span.parent_id.as_deref() != record.parent_id()
            {
                return Err(invalid(record, "inconsistent span envelope"));
            }
            match record.event().kind() {
                "span_updated" => {
                    if span.ended_seq.is_some() {
                        return Err(invalid(record, "update after span close"));
                    }
                    span.fields.extend(fields.clone());
                }
                "span_ended" if span.ended_seq.replace(record.seq()).is_some() => {
                    return Err(invalid(record, "duplicate span close"));
                }
                _ => {}
            }
        } else if record.parent_id().is_some()
            || matches!(record.event().kind(), "span_updated" | "span_ended")
        {
            return Err(invalid(record, "span observation without span ID"));
        }
    }
    Ok(ReplayedTrace {
        spans,
        events: records.to_vec(),
    })
}

fn invalid(record: &RolloutTraceRecord, reason: &str) -> TraceError {
    TraceError::Invalid(format!("sequence {}: {reason}", record.seq()))
}

impl ReplayedSpan {
    /// Returns the trace id recorded during capture.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }
    /// Returns the span id recorded during capture.
    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }
    /// Returns the parent id recorded during capture.
    #[must_use]
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }
    /// Returns the started seq recorded during capture.
    #[must_use]
    pub fn started_seq(&self) -> u64 {
        self.started_seq
    }
    /// Returns the ended seq recorded during capture.
    #[must_use]
    pub fn ended_seq(&self) -> Option<u64> {
        self.ended_seq
    }
    /// Returns the fields recorded during capture.
    #[must_use]
    pub fn fields(&self) -> &BTreeMap<String, Value> {
        &self.fields
    }
}

impl ReplayedTrace {
    /// Returns the spans recorded during capture.
    #[must_use]
    pub fn spans(&self) -> &BTreeMap<String, ReplayedSpan> {
        &self.spans
    }
    /// Returns the events recorded during capture.
    #[must_use]
    pub fn events(&self) -> &[RolloutTraceRecord] {
        &self.events
    }
}
