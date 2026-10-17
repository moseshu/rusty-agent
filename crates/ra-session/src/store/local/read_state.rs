// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Durable receipts independent of rebuildable thread metadata. Only live terminal events publish
//! attention; snapshots never materialize receipts, and acknowledgements retain the delivery key.

use super::StateRuntime;
use ra_core::{error::Result, session::SessionId};
use rusqlite::{Connection, TransactionBehavior, params, params_from_iter};
use std::collections::HashMap;
use uuid::Uuid;

/// One local thread's receipt, ported from Codex's state runtime.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReadState {
    first_unread_turn: Option<String>,
    revision: String,
}

impl ThreadReadState {
    /// None means read; an empty string marks the whole thread unread, even without turns.
    #[must_use]
    pub fn first_unread_turn(&self) -> Option<&str> {
        self.first_unread_turn.as_deref()
    }
    /// The opaque revision a client must compare before an edit.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// An explicit read or unread mark, both guarded by the revision actually observed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum ReadStateOperation {
    /// Acknowledge the current activity.
    Read,
    /// Mark the whole thread unread.
    Unread,
}

/// The outcome of a revision-checked mark.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
pub enum ReadStateUpdate {
    /// The mark was committed.
    Applied(ThreadReadState),
    /// The caller's revision is stale; no write occurred.
    Conflict(ThreadReadState),
    /// There is no durable thread metadata row.
    Unavailable,
}

impl StateRuntime {
    /// Snapshots for the requested page's durable metadata rows, without baseline writes.
    ///
    /// # Errors
    /// Returns a storage error when the snapshot cannot be read.
    pub async fn thread_read_states(
        &self,
        ids: &[SessionId],
    ) -> Result<HashMap<SessionId, ThreadReadState>> {
        let ids = ids.to_vec();
        self.run(move |connection| read_states(connection, &ids))
            .await
    }

    /// Publishes a newly delivered terminal result once. Each newer result changes the revision,
    /// even when the earliest unread position stays unchanged.
    ///
    /// # Errors
    /// Returns a storage error when the publication cannot be committed.
    pub async fn publish_thread_attention(&self, id: &SessionId, turn_id: &str) -> Result<bool> {
        let id = id.as_str().to_owned();
        let turn_id = turn_id.to_owned();
        self.run(move |connection| {
            connection.execute(
                "INSERT INTO thread_read_receipts(thread_id,first_unread_turn,revision,last_published_turn)
                 SELECT id, ?1, ?2, ?1 FROM threads WHERE id=?3
                 ON CONFLICT(thread_id) DO UPDATE SET
                   first_unread_turn=COALESCE(thread_read_receipts.first_unread_turn,excluded.first_unread_turn),
                   revision=excluded.revision,
                   last_published_turn=excluded.last_published_turn
                 WHERE thread_read_receipts.last_published_turn IS NOT excluded.last_published_turn",
                params![turn_id, Uuid::new_v4().to_string(), id],
            ).map(|changed| changed != 0)
        }).await
    }

    /// Applies an explicit mark only at the observed revision. A stale edit is never retried.
    ///
    /// # Errors
    /// Returns a storage error when the comparison or write cannot complete.
    pub async fn update_thread_read_state(
        &self,
        id: &SessionId,
        expected_revision: &str,
        operation: ReadStateOperation,
    ) -> Result<ReadStateUpdate> {
        let id = id.clone();
        let expected_revision = expected_revision.to_owned();
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(before) = read_states(&tx, std::slice::from_ref(&id))?.remove(&id) else {
                return Ok(ReadStateUpdate::Unavailable);
            };
            if before.revision != expected_revision {
                return Ok(ReadStateUpdate::Conflict(before));
            }
            let first_unread = match operation {
                ReadStateOperation::Read => None,
                ReadStateOperation::Unread => Some(""),
            };
            let revision = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO thread_read_receipts(thread_id,first_unread_turn,revision)
                 VALUES(?1,?2,?3) ON CONFLICT(thread_id) DO UPDATE
                 SET first_unread_turn=excluded.first_unread_turn, revision=excluded.revision",
                params![id.as_str(), first_unread, revision],
            )?;
            tx.commit()?;
            Ok(ReadStateUpdate::Applied(ThreadReadState {
                first_unread_turn: first_unread.map(str::to_owned),
                revision,
            }))
        })
        .await
    }
}

fn read_states(
    connection: &Connection,
    ids: &[SessionId],
) -> rusqlite::Result<HashMap<SessionId, ThreadReadState>> {
    let mut result = HashMap::new();
    for ids in ids.chunks(998) {
        let placeholders = vec!["?"; ids.len()].join(", ");
        let mut query = connection.prepare(&format!(
            "SELECT t.id, r.first_unread_turn, r.revision FROM threads t
             LEFT JOIN thread_read_receipts r ON r.thread_id=t.id WHERE t.id IN ({placeholders})"
        ))?;
        let rows = query.query_map(params_from_iter(ids.iter().map(SessionId::as_str)), |row| {
            let id: String = row.get(0)?;
            let revision: Option<String> = row.get(2)?;
            Ok((
                SessionId::new(&id),
                ThreadReadState {
                    first_unread_turn: row.get(1)?,
                    revision: revision.unwrap_or_else(|| format!("initial:{id}")),
                },
            ))
        })?;
        for row in rows {
            let (id, state) = row?;
            result.insert(id, state);
        }
    }
    Ok(result)
}
