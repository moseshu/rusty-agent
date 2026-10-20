// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit local export and import, outside subscriber callbacks.

use super::RolloutTraceRecord;
use ra_session::rollout::{RolloutPayload, RolloutReader};
use std::{
    io::{BufRead, Write},
    path::Path,
};

/// Local artifact I/O, format or replay failure.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    /// Reading or writing the local artifact failed.
    #[error("local trace I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error("local trace JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    /// A schema, sequence or topology invariant is invalid.
    #[error("invalid local trace: {0}")]
    Invalid(String),
    /// Reading an existing persisted session rollout failed.
    #[error("local rollout read failed: {0}")]
    Rollout(#[from] ra_core::error::Error),
}

/// Exports ordered records as JSONL and flushes the supplied writer.
///
/// The host chooses its file, subscriber and shutdown policy. Records contain inline payloads,
/// following Codex's payload-before-reference rule without separate file references. This function
/// never opens a network connection and does not install a subscriber.
pub fn write_jsonl(
    records: &[RolloutTraceRecord],
    mut writer: impl Write,
) -> Result<(), TraceError> {
    for record in records {
        serde_json::to_writer(&mut writer, record)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

/// Reads an exported JSONL artifact. Invalid or partial lines are errors, with line context.
///
/// For a crash-recoverable persisted rollout, use [`read_rollout`], whose reader accepts an
/// unterminated tail. Exported artifacts are expected to be complete and are read strictly.
pub fn read_jsonl(reader: impl BufRead) -> Result<Vec<RolloutTraceRecord>, TraceError> {
    let mut records = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line)
            .map_err(|error| TraceError::Invalid(format!("line {}: {error}", index + 1)))?;
        records.push(record);
    }
    Ok(records)
}

/// Reads trace envelopes captured into an existing local session rollout.
///
/// Legacy rollouts contain no attempt-level trace observations. Missing requests or topology are
/// not fabricated from their conversation history. A session may contain several capture streams;
/// each has its own capture identity and sequence.
pub async fn read_rollout(path: impl AsRef<Path>) -> Result<Vec<RolloutTraceRecord>, TraceError> {
    let records = RolloutReader::open(path.as_ref()).read_all().await?;
    let mut trace = Vec::new();
    for record in records {
        if let RolloutPayload::Trace(record) = record.payload()? {
            trace.push(record);
        }
    }
    Ok(trace)
}
