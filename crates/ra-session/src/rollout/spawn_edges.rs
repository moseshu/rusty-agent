// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The parent/child edges of the spawned threads of a rollout directory, ported from Codex's
//! `agent-graph-store` and the `thread_spawn_edges` table of its state database.
//!
//! An edge is kept apart from both rollouts, in `thread_spawn_edges.jsonl` beside them: one line
//! per change, appended, so closing or re-parenting a thread never needs its live writer.
//! Replaying the operations reconstructs the current edge keyed by each child.
//!
//! # Differences from Codex
//!
//! Codex keeps the edges in its state database. That database is optional here, so the edges
//! follow the session index into a JSONL file every directory has.
//!
//! Codex updates a row in place; this file is an operation log instead. A status update applies,
//! on replay, to whatever edge the child has at that point, so it keeps a parent recorded after
//! the update was prepared. A deletion appends a removal of the thread's incoming and outgoing
//! edges rather than deleting rows. The log is never compacted.
//!
//! Codex backfills an open edge from a thread's spawn source when it indexes the thread. Here a
//! spawned thread with no recorded edge has that edge implicitly, read from its session metadata
//! when the edges are listed, unless that parent has since been deleted.

use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::{SessionId, rollout::ThreadSpawnEdgeStatus},
};
use serde::{Deserialize, Serialize};

const SPAWN_EDGES_FILE: &str = "thread_spawn_edges.jsonl";
const SPAWN_EDGES_LOCK_FILE: &str = "thread_spawn_edges.lock";

/// The current graph, including removals that suppress fallback to spawn metadata.
#[derive(Clone, Debug, Default)]
pub(crate) struct SpawnEdges {
    edges: HashMap<SessionId, Option<(SessionId, ThreadSpawnEdgeStatus)>>,
    removed_threads: HashSet<SessionId>,
}

impl SpawnEdges {
    pub(crate) fn children(
        &self,
        parent: &SessionId,
        filter: Option<ThreadSpawnEdgeStatus>,
    ) -> impl Iterator<Item = &SessionId> {
        self.edges.iter().filter_map(move |(child, edge)| {
            let (current_parent, status) = edge.as_ref()?;
            (current_parent == parent && filter.is_none_or(|filter| filter == *status))
                .then_some(child)
        })
    }

    pub(crate) fn needs_metadata(&self, child: &SessionId) -> bool {
        !self.edges.contains_key(child)
    }

    pub(crate) fn can_use_metadata(&self, parent: &SessionId, child: &SessionId) -> bool {
        self.needs_metadata(child) && !self.removed_threads.contains(parent)
    }

    pub(crate) fn upsert(
        &mut self,
        parent: SessionId,
        child: SessionId,
        status: ThreadSpawnEdgeStatus,
    ) {
        self.edges.insert(child, Some((parent, status)));
    }

    pub(crate) fn set_status(
        &mut self,
        child: SessionId,
        status: ThreadSpawnEdgeStatus,
        implicit_parent: Option<SessionId>,
    ) {
        if let Some(edge) = self.edges.get_mut(&child) {
            if let Some((_, current)) = edge {
                *current = status;
            }
        } else if let Some(parent) =
            implicit_parent.filter(|parent| self.can_use_metadata(parent, &child))
        {
            self.upsert(parent, child, status);
        }
    }

    pub(crate) fn remove_thread(&mut self, thread: &SessionId) {
        self.removed_threads.insert(thread.clone());
        self.edges.insert(thread.clone(), None);
        for edge in self.edges.values_mut() {
            if edge.as_ref().is_some_and(|(parent, _)| parent == thread) {
                *edge = None;
            }
        }
    }
}

/// Replay applies a status change to the edge present at that position in the log. The original
/// parent is only a fallback when no prior operation established or removed the edge.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum SpawnEdgeEntry {
    Upsert {
        parent_thread_id: SessionId,
        child_thread_id: SessionId,
        status: ThreadSpawnEdgeStatus,
    },
    SetStatus {
        child_thread_id: SessionId,
        status: ThreadSpawnEdgeStatus,
        implicit_parent: Option<SessionId>,
    },
    RemoveThread {
        removed_thread_id: SessionId,
    },
}

impl SpawnEdgeEntry {
    fn apply(self, graph: &mut SpawnEdges) {
        match self {
            Self::Upsert {
                parent_thread_id,
                child_thread_id,
                status,
            } => {
                graph.upsert(parent_thread_id, child_thread_id, status);
            }
            Self::SetStatus {
                child_thread_id,
                status,
                implicit_parent,
            } => {
                graph.set_status(child_thread_id, status, implicit_parent);
            }
            Self::RemoveThread { removed_thread_id } => {
                graph.remove_thread(&removed_thread_id);
            }
        }
    }
}

pub(crate) fn append_spawn_edge(dir: &Path, entry: &SpawnEdgeEntry) -> Result<()> {
    let mut line = serde_json::to_vec(entry).map_err(|error| {
        Error::session(
            SessionErrorKind::Corrupted,
            format!("failed to serialize a thread spawn edge: {error}"),
        )
    })?;
    line.push(b'\n');
    std::fs::create_dir_all(dir)
        .map_err(|error| io_error("create the thread spawn edges", &error))?;
    // Keep the writer lock separate so readers can replay complete records during an append.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(SPAWN_EDGES_LOCK_FILE))
        .map_err(|error| io_error("open the thread spawn edges lock", &error))?;
    lock.lock()
        .map_err(|error| io_error("lock the thread spawn edges", &error))?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(edges_path(dir))
        .map_err(|error| io_error("open the thread spawn edges", &error))?;
    append_record(&mut file, &line)
        .and_then(|()| file.sync_data())
        .map_err(|error| io_error("append to the thread spawn edges", &error))
}

fn append_record(file: &mut std::fs::File, line: &[u8]) -> std::io::Result<()> {
    if file.metadata()?.len() != 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        // Separate a partial final record left by an interrupted write from the next operation.
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    file.write_all(line)
}

pub(crate) async fn write_spawn_edge(dir: &Path, entry: SpawnEdgeEntry) -> Result<()> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || append_spawn_edge(&dir, &entry))
        .await
        .map_err(|error| {
            Error::session(
                SessionErrorKind::Io,
                format!("thread spawn edge write failed: {error}"),
            )
        })?
}

/// Replays the complete records of the edge log.
pub(crate) async fn load_spawn_edges(dir: &Path) -> Result<SpawnEdges> {
    let contents = match tokio::fs::read(edges_path(dir)).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(SpawnEdges::default()),
        Err(error) => return Err(io_error("read the thread spawn edges", &error)),
    };
    let mut graph = SpawnEdges::default();
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        if let Some(line) = line.strip_suffix(b"\n")
            && let Ok(entry) = serde_json::from_slice::<SpawnEdgeEntry>(line)
        {
            entry.apply(&mut graph);
        }
    }
    Ok(graph)
}

/// Removes the thread's current edges and prevents older edges from being backfilled.
pub(crate) fn remove_spawn_edges(dir: &Path, session_id: &SessionId) -> Result<()> {
    append_spawn_edge(
        dir,
        &SpawnEdgeEntry::RemoveThread {
            removed_thread_id: session_id.clone(),
        },
    )
}

fn edges_path(dir: &Path) -> PathBuf {
    dir.join(SPAWN_EDGES_FILE)
}

fn io_error(action: &str, error: &std::io::Error) -> Error {
    Error::session(SessionErrorKind::Io, format!("failed to {action}: {error}"))
}
