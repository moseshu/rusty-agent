//! Listing a rollout directory's threads from the head of each rollout, ported from Codex's
//! file-backed listing (`rollout/src/list.rs`, `thread-store/src/local/list_threads.rs`).
//!
//! As Codex lists its rollouts when it has no state database, a listing reads no rollout whole: it
//! reads each file's head — the session metadata and, a little further, the first message from the
//! user, which is the thread's preview — and takes the update time from the file's modification
//! time. Names come from the directory's session index.
//!
//! A listing first reads the first record of every rollout, its session metadata, and the file's
//! modification time: that is what threads are sorted and filtered by. It then walks them in order
//! from the cursor, reading each head for its preview, and reads at most [`MAX_SCAN_FILES`] heads
//! per call. A call that reaches that cap before its page is full returns what it found with a
//! cursor at the last thread it examined, so the next call carries on from there; nothing is left
//! beyond the cap.
//!
//! Active listings show only threads with a preview, as Codex's do; archived listings show every
//! thread with session metadata. A rollout without session metadata is never listed.
//!
//! # Differences from Codex
//!
//! - Codex names its rollouts after their creation time and walks them newest first, so a listing
//!   by creation time stops once a page is full. Rollouts here are named after their session id,
//!   which a resume, a fork or a deletion needs to find a rollout without a database, so a listing
//!   reads the first record of every file to sort them; the creation time is the session
//!   metadata's, or when it records none, the time that record was written. Codex's cap counts
//!   every file its walk passes, skipped ones included, so a directory beyond the cap cannot be
//!   paged past it; here the cap counts the heads read for previews, and the cursor resumes the
//!   walk.
//! - Codex's cursor for creation and update times holds the time alone, and a page resumes after
//!   every thread of that time, which skips threads sharing it. The cursor here holds the time and
//!   the session id, as Codex's recency cursor does, and threads of one time are ordered by id.
//! - Codex treats a thread recorded without a model provider as one of its default provider; this
//!   framework has none, so such a thread matches no provider filter. Working directories are
//!   compared as paths, component by component, without consulting the file system.
//! - Codex's filters by session source, its search over titles held in its state database and its
//!   relation, section and project filters are not ported; search matches the session index's
//!   names, as Codex's file-backed fallback does.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::EventTimestamp,
    item::{ContentBlock, ModelInputItem},
};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    rollout::{
        RolloutPayload, RolloutRecord, RolloutSessionMeta, session_index,
        truncation::is_user_message,
    },
    store::{ListThreadsParams, SortDirection, StoredThread, ThreadPage, ThreadSortKey},
};

/// The most rollout files one listing scans: Codex's `MAX_SCAN_FILES`.
pub const MAX_SCAN_FILES: usize = 10_000;

/// How many records a head holds before the search for a preview: Codex's `HEAD_RECORD_LIMIT`.
const HEAD_RECORD_LIMIT: usize = 10;

/// How many more records are read looking for a preview: Codex's `USER_EVENT_SCAN_LIMIT`.
const USER_EVENT_SCAN_LIMIT: usize = 200;

/// What the head of a rollout says: Codex's `HeadTailSummary`, without its tail.
#[derive(Debug, Clone, Default)]
pub(crate) struct HeadSummary {
    meta: Option<RolloutSessionMeta>,
    created_at: Option<EventTimestamp>,
    first_user_message: Option<String>,
}

impl HeadSummary {
    /// The thread this head describes, if it records session metadata, with its preview.
    pub(crate) fn into_thread(self) -> Option<StoredThread> {
        let meta = self.meta?;
        let mut thread = StoredThread::new(meta.session_id().clone(), Some(&meta));
        if let Some(created_at) = self.created_at {
            thread.set_created_at(created_at);
        }
        thread.set_head(
            self.first_user_message.clone().unwrap_or_default(),
            self.first_user_message,
        );
        Some(thread)
    }
}

/// Reads the head of the rollout at `path`: Codex's `read_head_summary`.
///
/// The first [`HEAD_RECORD_LIMIT`] records are read; once session metadata has been seen, up to
/// [`USER_EVENT_SCAN_LIMIT`] more are read looking for the first user message. Records that cannot
/// be read are skipped, and so is a last line still being written.
pub(crate) async fn read_head_summary(path: &Path) -> Result<HeadSummary> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| io_error(path, &error))?;
    let mut lines = BufReader::new(file);
    let mut summary = HeadSummary::default();
    let mut scanned = 0usize;
    let mut line = String::new();
    while scanned < HEAD_RECORD_LIMIT
        || (summary.meta.is_some()
            && summary.first_user_message.is_none()
            && scanned < HEAD_RECORD_LIMIT + USER_EVENT_SCAN_LIMIT)
    {
        line.clear();
        let read = lines
            .read_line(&mut line)
            .await
            .map_err(|error| io_error(path, &error))?;
        if read == 0 || !line.ends_with('\n') {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        scanned += 1;
        let Ok(record) = serde_json::from_str::<RolloutRecord>(trimmed) else {
            continue;
        };
        match record.type_name() {
            "session_meta" if summary.meta.is_none() => {
                if let Ok(RolloutPayload::SessionMeta(meta)) = record.payload() {
                    summary.created_at = Some(meta.created_at().unwrap_or_else(|| record.at()));
                    summary.meta = Some(meta);
                }
            }
            "run_started" if summary.first_user_message.is_none() => {
                if let Ok(RolloutPayload::RunStarted(started)) = record.payload()
                    && !started.input_is_continuation_base()
                {
                    summary.first_user_message =
                        started.input().iter().find_map(user_message_preview);
                }
            }
            _ => {}
        }
        if summary.meta.is_some() && summary.first_user_message.is_some() {
            break;
        }
    }
    Ok(summary)
}

/// What a listing shows of a user message: its text, or a placeholder for what has none: Codex's
/// `user_message_preview`.
pub(crate) fn user_message_preview(item: &ModelInputItem) -> Option<String> {
    if !is_user_message(item) {
        return None;
    }
    let ModelInputItem::Message(message) = item else {
        return None;
    };
    let text = message.text_content();
    let text = text.trim();
    if !text.is_empty() {
        return Some(text.to_owned());
    }
    message
        .content()
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_) | ContentBlock::File(_)))
        .then(|| "[Image]".to_owned())
}

/// A rollout found by a listing, with what it sorts by.
struct Candidate {
    key: (u64, String),
    path: PathBuf,
    meta: RolloutSessionMeta,
    created_at: EventTimestamp,
    updated_at: EventTimestamp,
}

/// Lists the threads whose rollouts are kept in `dir`, archived when `archived`, with names from
/// the session index in `index_dir`.
pub(crate) async fn list_threads(
    dir: &Path,
    index_dir: &Path,
    archived: bool,
    params: &ListThreadsParams,
) -> Result<ThreadPage> {
    let cursor = params.cursor().map(parse_cursor).transpose()?;
    if params.cwd_filters().is_some_and(<[String]>::is_empty) {
        return Ok(ThreadPage::new(Vec::new(), None));
    }

    // What every rollout sorts and filters by: its session metadata and modification time.
    let mut candidates = Vec::new();
    for path in rollout_files(dir).await? {
        let Some((meta, created_at)) = first_session_meta(&path).await else {
            continue;
        };
        let probe = StoredThread::new(meta.session_id().clone(), Some(&meta));
        if !matches_filters(&probe, params) {
            continue;
        }
        let updated_at = modified_time(&path).await.unwrap_or(created_at);
        let at = match params.sort_key() {
            ThreadSortKey::UpdatedAt => updated_at,
            _ => created_at,
        };
        candidates.push(Candidate {
            key: (at.as_millis(), meta.session_id().as_str().to_owned()),
            path,
            meta,
            created_at,
            updated_at,
        });
    }

    let ids = candidates
        .iter()
        .map(|candidate| candidate.meta.session_id().clone())
        .collect::<HashSet<_>>();
    let names = session_index::find_thread_names(index_dir, &ids).await?;
    if let Some(term) = params.search_term() {
        candidates.retain(|candidate| {
            names
                .get(candidate.meta.session_id())
                .is_some_and(|name| name.contains(term))
        });
    }

    candidates.sort_by(|left, right| left.key.cmp(&right.key));
    if params.sort_direction() == SortDirection::Desc {
        candidates.reverse();
    }
    if let Some(anchor) = &cursor {
        candidates.retain(|candidate| match params.sort_direction() {
            SortDirection::Asc => candidate.key > *anchor,
            _ => candidate.key < *anchor,
        });
    }

    // Walk them in order, reading heads for previews, until the page is full or the cap is hit.
    let mut items = Vec::new();
    let mut last_examined = None;
    let mut remaining = candidates.into_iter().peekable();
    let mut heads_read = 0usize;
    while items.len() < params.page_size() && heads_read < MAX_SCAN_FILES {
        let Some(candidate) = remaining.next() else {
            break;
        };
        heads_read += 1;
        last_examined = Some(candidate.key.clone());
        let summary = match read_head_summary(&candidate.path).await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!(path = %candidate.path.display(), %error, "a rollout's head could not be read; it is not listed");
                continue;
            }
        };
        if !archived && summary.first_user_message.is_none() {
            continue;
        }
        let mut thread =
            StoredThread::new(candidate.meta.session_id().clone(), Some(&candidate.meta));
        thread.set_created_at(candidate.created_at);
        thread.set_head(
            summary.first_user_message.clone().unwrap_or_default(),
            summary.first_user_message,
        );
        thread.set_updated_at(candidate.updated_at);
        if archived {
            thread.set_archived_at(candidate.updated_at);
        }
        if let Some(name) = names.get(thread.session_id()) {
            thread.set_name(name.clone());
        }
        items.push(thread.with_rollout_path(candidate.path));
    }
    let next_cursor = remaining
        .peek()
        .is_some()
        .then(|| last_examined.map(|(at, id)| format!("{at}|{id}")))
        .flatten();
    Ok(ThreadPage::new(items, next_cursor))
}

/// The session metadata the rollout at `path` opens with, and when the thread was created: the
/// metadata's time, or when it records none, the time its record was written.
async fn first_session_meta(path: &Path) -> Option<(RolloutSessionMeta, EventTimestamp)> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).await.ok()?;
    if !line.ends_with('\n') {
        return None;
    }
    let record = serde_json::from_str::<RolloutRecord>(line.trim()).ok()?;
    match record.payload().ok()? {
        RolloutPayload::SessionMeta(meta) => {
            let created_at = meta.created_at().unwrap_or_else(|| record.at());
            Some((meta, created_at))
        }
        _ => None,
    }
}

/// The rollout files in `dir`.
pub(crate) async fn rollout_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(dir, &error)),
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| io_error(dir, &error))?
    {
        let is_rollout = entry.file_name().to_str().is_some_and(|name| {
            name.starts_with("rollout-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
        });
        // Only regular files: anything else may never end when read.
        if is_rollout
            && entry
                .file_type()
                .await
                .is_ok_and(|file_type| file_type.is_file())
        {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn matches_filters(thread: &StoredThread, params: &ListThreadsParams) -> bool {
    if let Some(providers) = params.model_providers()
        && !providers.is_empty()
        && !thread
            .model_provider()
            .is_some_and(|provider| providers.iter().any(|candidate| candidate == provider))
    {
        return false;
    }
    if let Some(cwds) = params.cwd_filters()
        && !thread.cwd().is_some_and(|cwd| {
            cwds.iter().any(|filter| {
                Path::new(cwd)
                    .components()
                    .eq(Path::new(filter).components())
            })
        })
    {
        return false;
    }
    true
}

/// When the file at `path` was last modified.
pub(crate) async fn modified_time(path: &Path) -> Option<EventTimestamp> {
    tokio::fs::metadata(path)
        .await
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(EventTimestamp::from_system_time)
}

/// A cursor a listing returned: the sort time in milliseconds and the session id.
pub(crate) fn parse_cursor(cursor: &str) -> Result<(u64, String)> {
    cursor
        .split_once('|')
        .and_then(|(at, id)| Some((at.parse().ok()?, id.to_owned())))
        .filter(|(_, id)| !id.is_empty())
        .ok_or_else(|| Error::caller(format!("invalid cursor: {cursor}")))
}

fn io_error(path: &Path, error: &std::io::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("failed to read {}: {error}", path.display()),
    )
}
