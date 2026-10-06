//! The names given to the threads of a rollout directory, ported from Codex's
//! `rollout/src/session_index.rs`.
//!
//! A name is kept apart from its rollout, in `session_index.jsonl` beside the rollouts: one line
//! per rename, appended, so naming a thread never rewrites its history. The newest line for a
//! thread is its name. Deleting a thread removes its lines.
//!
//! # Differences from Codex
//!
//! A name is cleared by appending an empty one, as Codex does, and here the empty line then wins.
//! Codex's batch lookup, `find_thread_names_by_ids`, skips empty lines without dropping an older
//! name, so a cleared name comes back wherever its state database does not answer instead; that
//! database is not ported, so the clear is honored here. Looking threads up by name is not ported:
//! no store operation needs it.

use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::EventTimestamp,
    session::SessionId,
};
use serde::{Deserialize, Serialize};

const SESSION_INDEX_FILE: &str = "session_index.jsonl";

/// Serializes this process's writers of every index, as Codex's `SESSION_INDEX_LOCK` does.
static SESSION_INDEX_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// One line of the index: Codex's `SessionIndexEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionIndexEntry {
    id: SessionId,
    thread_name: String,
    updated_at: String,
}

/// Records `name` as the name of the thread of `session_id`; an empty name clears it: Codex's
/// `append_thread_name`.
pub(crate) fn append_thread_name(dir: &Path, session_id: &SessionId, name: &str) -> Result<()> {
    let entry = SessionIndexEntry {
        id: session_id.clone(),
        thread_name: name.to_owned(),
        updated_at: rfc3339(EventTimestamp::now()),
    };
    let mut line = serde_json::to_string(&entry).map_err(|error| {
        Error::session(
            SessionErrorKind::Corrupted,
            format!("failed to serialize a session index entry: {error}"),
        )
    })?;
    line.push('\n');
    let _guard = lock();
    std::fs::create_dir_all(dir).map_err(|error| io_error("create the session index", &error))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path(dir))
        .map_err(|error| io_error("open the session index", &error))?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| io_error("append to the session index", &error))
}

/// Removes every line naming the thread of `session_id`: Codex's `remove_thread_name_entries`.
pub(crate) fn remove_thread_name_entries(dir: &Path, session_id: &SessionId) -> Result<()> {
    let _guard = lock();
    let path = index_path(dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("read the session index", &error)),
    };
    let mut removed = false;
    let mut remaining = String::with_capacity(contents.len());
    for line in contents.lines() {
        if parse(line).is_some_and(|entry| entry.id == *session_id) {
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
        .map_err(|error| io_error("rewrite the session index", &error))
}

/// The names of the threads of `session_ids` that have one: Codex's `find_thread_names_by_ids`,
/// with the newest line for a thread winning even when it clears the name.
pub(crate) async fn find_thread_names(
    dir: &Path,
    session_ids: &HashSet<SessionId>,
) -> Result<HashMap<SessionId, String>> {
    if session_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let contents = match tokio::fs::read_to_string(index_path(dir)).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(io_error("read the session index", &error)),
    };
    let mut names = HashMap::new();
    for entry in contents.lines().filter_map(parse) {
        if !session_ids.contains(&entry.id) {
            continue;
        }
        let name = entry.thread_name.trim();
        if name.is_empty() {
            names.remove(&entry.id);
        } else {
            names.insert(entry.id, name.to_owned());
        }
    }
    Ok(names)
}

fn parse(line: &str) -> Option<SessionIndexEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join(SESSION_INDEX_FILE)
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SESSION_INDEX_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rfc3339(at: EventTimestamp) -> String {
    let nanos = i128::from(at.as_millis()) * 1_000_000;
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn io_error(action: &str, error: &std::io::Error) -> Error {
    Error::session(SessionErrorKind::Io, format!("failed to {action}: {error}"))
}
