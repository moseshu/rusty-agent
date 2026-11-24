// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The parent/child edges of the spawned threads of a rollout directory, ported from Codex's
//! `agent-graph-store` and the `thread_spawn_edges` table of its state database.
//!
//! An edge is kept apart from both rollouts, in `thread_spawn_edges.jsonl` beside them: one line
//! per change, appended, so closing or re-parenting a thread never needs its live writer. The
//! newest line for a child is its edge, as the table keys an edge by its child.
//!
//! # Differences from Codex
//!
//! Codex keeps the edges in its state database. That database is optional here, so the edges
//! follow the session index into a JSONL file every directory has. Codex backfills an open edge
//! from a thread's spawn source when it indexes the thread; here a spawned thread with no line
//! has that edge implicitly, read from its session metadata when the edges are listed.

use std::{
    collections::HashMap,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::{SessionId, rollout::ThreadSpawnEdgeStatus},
};
use serde::{Deserialize, Serialize};

const SPAWN_EDGES_FILE: &str = "thread_spawn_edges.jsonl";

/// Serializes this process's writers of every edge file.
static SPAWN_EDGES_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// The latest persisted edge of each child: its parent and status.
pub(crate) type SpawnEdges = HashMap<SessionId, (SessionId, ThreadSpawnEdgeStatus)>;

/// One line of the file: a row of Codex's `thread_spawn_edges`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SpawnEdgeEntry {
    parent_thread_id: SessionId,
    child_thread_id: SessionId,
    status: ThreadSpawnEdgeStatus,
}

/// Records the edge from `parent` to `child` with `status`, replacing any earlier edge of
/// `child`.
pub(crate) fn append_spawn_edge(
    dir: &Path,
    parent: &SessionId,
    child: &SessionId,
    status: ThreadSpawnEdgeStatus,
) -> Result<()> {
    let entry = SpawnEdgeEntry {
        parent_thread_id: parent.clone(),
        child_thread_id: child.clone(),
        status,
    };
    let mut line = serde_json::to_string(&entry).map_err(|error| {
        Error::session(
            SessionErrorKind::Corrupted,
            format!("failed to serialize a thread spawn edge: {error}"),
        )
    })?;
    line.push('\n');
    let _guard = lock();
    std::fs::create_dir_all(dir)
        .map_err(|error| io_error("create the thread spawn edges", &error))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(edges_path(dir))
        .map_err(|error| io_error("open the thread spawn edges", &error))?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_data())
        .map_err(|error| io_error("append to the thread spawn edges", &error))
}

/// Every child's latest edge.
pub(crate) async fn load_spawn_edges(dir: &Path) -> Result<SpawnEdges> {
    let contents = match tokio::fs::read_to_string(edges_path(dir)).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(io_error("read the thread spawn edges", &error)),
    };
    Ok(contents
        .lines()
        .filter_map(parse)
        .map(|entry| {
            (
                entry.child_thread_id,
                (entry.parent_thread_id, entry.status),
            )
        })
        .collect())
}

/// Removes every edge into or out of `session_id`, as Codex's thread deletion does.
pub(crate) fn remove_spawn_edges(dir: &Path, session_id: &SessionId) -> Result<()> {
    let _guard = lock();
    let path = edges_path(dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("read the thread spawn edges", &error)),
    };
    let mut removed = false;
    let mut remaining = String::with_capacity(contents.len());
    for line in contents.lines() {
        if parse(line).is_some_and(|entry| {
            entry.parent_thread_id == *session_id || entry.child_thread_id == *session_id
        }) {
            removed = true;
        } else {
            remaining.push_str(line);
            remaining.push('\n');
        }
    }
    if !removed {
        return Ok(());
    }
    let temp_path = path.with_extension("jsonl.tmp");
    std::fs::write(&temp_path, remaining)
        .and_then(|()| std::fs::rename(&temp_path, &path))
        .map_err(|error| io_error("rewrite the thread spawn edges", &error))
}

fn parse(line: &str) -> Option<SpawnEdgeEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

fn edges_path(dir: &Path) -> PathBuf {
    dir.join(SPAWN_EDGES_FILE)
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SPAWN_EDGES_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn io_error(action: &str, error: &std::io::Error) -> Error {
    Error::session(SessionErrorKind::Io, format!("failed to {action}: {error}"))
}
