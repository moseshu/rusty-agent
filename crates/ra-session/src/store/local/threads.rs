//! The thread table: Codex's `ThreadMetadata` and the statements of `state/src/runtime/threads.rs`
//! that this framework's thread metadata uses.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use ra_core::{
    error::Result,
    event::EventTimestamp,
    session::{SessionId, rollout::RolloutThreadSpawn},
};
use rusqlite::{OptionalExtension, Row, params, types::Value};

use super::StateRuntime;
use crate::store::{SortDirection, ThreadSortKey};

/// What the state database keeps of a thread: Codex's `ThreadMetadata`, with the fields this
/// framework's threads have.
///
/// Codex's source, history mode, agent nickname and role, sandbox policy, approval mode, token
/// usage, Git facts, sections, project, recency and preferences are its product's and are not
/// kept. Its `title`, in its legacy history mode, is the title derived from the first user message
/// until the thread is given a name, which then replaces it; an empty preview means none.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMetadata {
    pub(crate) session_id: SessionId,
    pub(crate) rollout_path: PathBuf,
    pub(crate) created_at: EventTimestamp,
    pub(crate) updated_at: EventTimestamp,
    pub(crate) thread_spawn: Option<RolloutThreadSpawn>,
    pub(crate) forked_from_id: Option<SessionId>,
    pub(crate) model_provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) cli_version: Option<String>,
    pub(crate) originator: Option<String>,
    pub(crate) title: String,
    pub(crate) preview: String,
    pub(crate) first_user_message: Option<String>,
    pub(crate) archived_at: Option<EventTimestamp>,
}

impl ThreadMetadata {
    /// A thread of `session_id` kept in `rollout_path`, created and last updated at `created_at`,
    /// with nothing else known: what Codex's `ThreadMetadataBuilder::build` starts from.
    pub(crate) const fn new(
        session_id: SessionId,
        rollout_path: PathBuf,
        created_at: EventTimestamp,
    ) -> Self {
        Self {
            session_id,
            rollout_path,
            created_at,
            updated_at: created_at,
            thread_spawn: None,
            forked_from_id: None,
            model_provider: None,
            model: None,
            effort: None,
            cwd: None,
            cli_version: None,
            originator: None,
            title: String::new(),
            preview: String::new(),
            first_user_message: None,
            archived_at: None,
        }
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The file the thread's rollout is kept in.
    #[must_use]
    pub fn rollout_path(&self) -> &Path {
        &self.rollout_path
    }

    /// When the thread was created.
    #[must_use]
    pub const fn created_at(&self) -> EventTimestamp {
        self.created_at
    }

    /// When the thread was last written to.
    #[must_use]
    pub const fn updated_at(&self) -> EventTimestamp {
        self.updated_at
    }

    /// Where the thread was spawned from, for a spawned thread: Codex's `source`.
    #[must_use]
    pub const fn thread_spawn(&self) -> Option<&RolloutThreadSpawn> {
        self.thread_spawn.as_ref()
    }

    /// The thread this one was forked from, for a fork.
    #[must_use]
    pub const fn forked_from_id(&self) -> Option<&SessionId> {
        self.forked_from_id.as_ref()
    }

    /// The model provider.
    #[must_use]
    pub fn model_provider(&self) -> Option<&str> {
        self.model_provider.as_deref()
    }

    /// The latest model.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The latest effort: Codex's `reasoning_effort`.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// The working directory, as compared by a listing.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The version of the program that created the thread.
    #[must_use]
    pub fn cli_version(&self) -> Option<&str> {
        self.cli_version.as_deref()
    }

    /// The originator recorded at creation.
    #[must_use]
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    /// The title: derived from the first user message, or the name the thread was given.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The preview a listing shows; empty when there is none.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// The first user message.
    #[must_use]
    pub fn first_user_message(&self) -> Option<&str> {
        self.first_user_message.as_deref()
    }

    /// When the thread was archived, for an archived thread.
    #[must_use]
    pub const fn archived_at(&self) -> Option<EventTimestamp> {
        self.archived_at
    }

    /// The name the thread was given, if its title is one: Codex's
    /// `distinct_thread_metadata_title`, a title that is neither empty nor the first user message.
    pub(crate) fn distinct_title(&self) -> Option<&str> {
        let title = self.title.trim();
        (!title.is_empty() && self.first_user_message.as_deref().map(str::trim) != Some(title))
            .then_some(title)
    }

    /// Keeps the name the existing row was given over a title derived again from the rollout:
    /// Codex's `prefer_existing_explicit_title`.
    pub(crate) fn prefer_existing_explicit_title(&mut self, existing: &Self) {
        if existing.distinct_title().is_none() {
            return;
        }
        let title = self.title.trim();
        if title.is_empty() || self.first_user_message.as_deref().map(str::trim) == Some(title) {
            self.title.clone_from(&existing.title);
        }
    }
}

/// Which threads a listing selects and in what order: Codex's `ThreadFilterOptions`, without the
/// source, section and project filters.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThreadFilterOptions<'a> {
    pub(crate) archived_only: bool,
    pub(crate) model_providers: Option<&'a [String]>,
    pub(crate) cwd_filters: Option<&'a [String]>,
    /// Where the page starts: the sort time in milliseconds and the session id.
    pub(crate) anchor: Option<(u64, &'a str)>,
    pub(crate) sort_key: ThreadSortKey,
    pub(crate) sort_direction: SortDirection,
    pub(crate) search_term: Option<&'a str>,
}

/// A page of rows: Codex's `ThreadsPage`.
#[derive(Debug, Clone)]
pub(crate) struct ThreadsPage {
    pub(crate) items: Vec<ThreadMetadata>,
    /// The last row's sort time and id, when more rows follow.
    pub(crate) next_anchor: Option<(u64, SessionId)>,
}

const SELECT_COLUMNS: &str = "SELECT id, rollout_path, created_at_ms, updated_at_ms, thread_spawn, \
     forked_from_id, model_provider, model, reasoning_effort, cwd, cli_version, originator, title, \
     preview, first_user_message, archived_at_ms FROM threads";

impl StateRuntime {
    /// The row of `session_id`, if there is one: Codex's `get_thread`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read.
    pub async fn get_thread(&self, session_id: &SessionId) -> Result<Option<ThreadMetadata>> {
        let id = session_id.as_str().to_owned();
        self.run(move |connection| get_thread(connection, &id))
            .await
    }

    /// Inserts or replaces the row of `metadata`: Codex's `upsert_thread`.
    ///
    /// The update time goes through [`StateRuntime::allocate_thread_updated_at`]. An existing
    /// originator is kept, as Codex keeps creation-time facts, and an empty preview does not
    /// replace one already kept.
    pub(crate) async fn upsert_thread(&self, metadata: &ThreadMetadata) -> Result<()> {
        let mut metadata = metadata.clone();
        metadata.updated_at = self.allocate_thread_updated_at(metadata.updated_at);
        self.run(move |connection| upsert_thread(connection, &metadata))
            .await
    }

    /// Marks the thread archived at `archived_at`, now kept at `rollout_path`, with the update
    /// time of that file: Codex's `mark_archived`. A thread without a row is left without one.
    pub(crate) fn mark_archived_blocking(
        &self,
        session_id: &SessionId,
        rollout_path: &Path,
        archived_at: EventTimestamp,
    ) -> Result<()> {
        self.move_blocking(session_id, rollout_path, Some(archived_at))
    }

    /// Marks the thread active, kept at `rollout_path` again: Codex's `mark_unarchived`.
    pub(crate) fn mark_unarchived_blocking(
        &self,
        session_id: &SessionId,
        rollout_path: &Path,
    ) -> Result<()> {
        self.move_blocking(session_id, rollout_path, None)
    }

    fn move_blocking(
        &self,
        session_id: &SessionId,
        rollout_path: &Path,
        archived_at: Option<EventTimestamp>,
    ) -> Result<()> {
        let id = session_id.as_str().to_owned();
        let Some(mut metadata) = self.run_blocking(|connection| get_thread(connection, &id))?
        else {
            return Ok(());
        };
        metadata.archived_at = archived_at;
        rollout_path.clone_into(&mut metadata.rollout_path);
        if let Some(modified) = modified_time(rollout_path) {
            metadata.updated_at = modified;
        }
        metadata.updated_at = self.allocate_thread_updated_at(metadata.updated_at);
        self.run_blocking(|connection| upsert_thread(connection, &metadata))
    }

    /// Deletes the rows of `session_ids`, in one transaction: Codex's `delete_threads_strict`.
    pub(crate) fn delete_threads_blocking(&self, session_ids: &[SessionId]) -> Result<()> {
        if session_ids.is_empty() {
            return Ok(());
        }
        self.run_blocking(|connection| {
            let transaction = connection.transaction()?;
            for session_id in session_ids {
                transaction.execute(
                    "DELETE FROM threads WHERE id = ?1",
                    params![session_id.as_str()],
                )?;
            }
            transaction.commit()
        })
    }

    /// One page of the rows `filters` selects, and where the next starts: Codex's
    /// `list_threads`, which reads one row past the page to know whether more follow.
    pub(crate) async fn list_threads(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
    ) -> Result<ThreadsPage> {
        let (sql, values) = list_threads_query(page_size.saturating_add(1), filters);
        let sort_key = filters.sort_key;
        self.run(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows =
                statement.query_map(rusqlite::params_from_iter(values), metadata_from_row)?;
            let mut items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let next_anchor = if items.len() > page_size {
                items.truncate(page_size);
                items
                    .last()
                    .map(|item| (sort_time(item, sort_key), item.session_id.clone()))
            } else {
                None
            };
            Ok(ThreadsPage { items, next_anchor })
        })
        .await
    }
}

/// The time a row sorts by under `sort_key`, in milliseconds.
pub(crate) fn sort_time(item: &ThreadMetadata, sort_key: ThreadSortKey) -> u64 {
    match sort_key {
        ThreadSortKey::UpdatedAt => item.updated_at.as_millis(),
        _ => item.created_at.as_millis(),
    }
}

/// The statement and its values for a page of `filters`: Codex's `push_list_threads_query`.
///
/// Codex breaks ties by id only for its recency sort; here every sort does, as the listing of
/// rollouts does, so a page never skips threads sharing a time.
fn list_threads_query(limit: usize, filters: ThreadFilterOptions<'_>) -> (String, Vec<Value>) {
    let mut sql = String::from(SELECT_COLUMNS);
    let mut values = Vec::new();
    if filters.archived_only {
        sql.push_str(" WHERE archived = 1");
    } else {
        // As Codex's: an active listing shows only threads with a preview, which the partial
        // indexes match.
        sql.push_str(" WHERE archived = 0 AND preview <> ''");
    }
    if let Some(providers) = filters.model_providers
        && !providers.is_empty()
    {
        sql.push_str(" AND model_provider IN (");
        push_placeholders(&mut sql, &mut values, providers);
        sql.push(')');
    }
    match filters.cwd_filters {
        Some([]) => sql.push_str(" AND 1 = 0"),
        Some(cwds) => {
            sql.push_str(" AND cwd IN (");
            let normalized = cwds
                .iter()
                .map(|cwd| normalize_cwd(cwd))
                .collect::<Vec<_>>();
            push_placeholders(&mut sql, &mut values, &normalized);
            sql.push(')');
        }
        None => {}
    }
    if let Some(term) = filters.search_term {
        sql.push_str(" AND (instr(title, ?) > 0 OR instr(preview, ?) > 0)");
        values.push(Value::Text(term.to_owned()));
        values.push(Value::Text(term.to_owned()));
    }
    let column = match filters.sort_key {
        ThreadSortKey::UpdatedAt => "updated_at_ms",
        _ => "created_at_ms",
    };
    let (operator, direction) = match filters.sort_direction {
        SortDirection::Asc => (">", "ASC"),
        _ => ("<", "DESC"),
    };
    if let Some((at, id)) = filters.anchor {
        let at = i64::try_from(at).unwrap_or(i64::MAX);
        let _ = write!(
            sql,
            " AND ({column} {operator} ? OR ({column} = ? AND id {operator} ?))"
        );
        values.push(Value::Integer(at));
        values.push(Value::Integer(at));
        values.push(Value::Text(id.to_owned()));
    }
    // As Codex's: with several working directories, the ordered column is kept off the index so
    // the planner does not walk the time index instead of the more selective one.
    let unindexed = if filters.cwd_filters.is_some_and(|cwds| cwds.len() > 1) {
        "+"
    } else {
        ""
    };
    let _ = write!(
        sql,
        " ORDER BY {unindexed}{column} {direction}, id {direction} LIMIT ?"
    );
    values.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    (sql, values)
}

fn push_placeholders(sql: &mut String, values: &mut Vec<Value>, items: &[String]) {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
        values.push(Value::Text(item.clone()));
    }
}

/// A working directory as the database keeps and compares it: Codex's
/// `normalize_cwd_for_state_db`, lexically, component by component, as the listing of rollouts
/// compares working directories, without consulting the file system.
pub(crate) fn normalize_cwd(cwd: &str) -> String {
    Path::new(cwd)
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

pub(super) fn get_thread(
    connection: &rusqlite::Connection,
    id: &str,
) -> rusqlite::Result<Option<ThreadMetadata>> {
    connection
        .query_row(
            &format!("{SELECT_COLUMNS} WHERE id = ?1"),
            params![id],
            metadata_from_row,
        )
        .optional()
}

/// Codex's upsert, over this table's columns: every column takes the new value except the
/// originator, which keeps the one recorded first, and the preview, which an empty one does not
/// clear.
pub(crate) fn upsert_thread(
    connection: &rusqlite::Connection,
    metadata: &ThreadMetadata,
) -> rusqlite::Result<()> {
    let thread_spawn = metadata
        .thread_spawn
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    connection.execute(
        r"
INSERT INTO threads (
    id, rollout_path, created_at_ms, updated_at_ms, thread_spawn, forked_from_id, model_provider,
    model, reasoning_effort, cwd, cli_version, originator, title, preview, first_user_message,
    archived, archived_at_ms
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
ON CONFLICT(id) DO UPDATE SET
    rollout_path = excluded.rollout_path,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms,
    thread_spawn = excluded.thread_spawn,
    forked_from_id = excluded.forked_from_id,
    model_provider = excluded.model_provider,
    model = excluded.model,
    reasoning_effort = excluded.reasoning_effort,
    cwd = excluded.cwd,
    cli_version = excluded.cli_version,
    originator = COALESCE(threads.originator, excluded.originator),
    title = excluded.title,
    preview = COALESCE(NULLIF(excluded.preview, ''), threads.preview),
    first_user_message = excluded.first_user_message,
    archived = excluded.archived,
    archived_at_ms = excluded.archived_at_ms
",
        params![
            metadata.session_id.as_str(),
            metadata.rollout_path.to_string_lossy(),
            millis(metadata.created_at),
            millis(metadata.updated_at),
            thread_spawn,
            metadata.forked_from_id.as_ref().map(SessionId::as_str),
            metadata.model_provider,
            metadata.model,
            metadata.effort,
            metadata.cwd.as_deref().map(normalize_cwd),
            metadata.cli_version,
            metadata.originator,
            metadata.title,
            metadata.preview,
            metadata.first_user_message.as_deref().unwrap_or_default(),
            metadata.archived_at.is_some(),
            metadata.archived_at.map(millis),
        ],
    )?;
    Ok(())
}

fn metadata_from_row(row: &Row<'_>) -> rusqlite::Result<ThreadMetadata> {
    let thread_spawn = row
        .get::<_, Option<String>>(4)?
        .map(|json| serde_json::from_str::<RolloutThreadSpawn>(&json))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let first_user_message = row.get::<_, String>(14)?;
    Ok(ThreadMetadata {
        session_id: SessionId::new(row.get::<_, String>(0)?),
        rollout_path: PathBuf::from(row.get::<_, String>(1)?),
        created_at: timestamp(row.get(2)?),
        updated_at: timestamp(row.get(3)?),
        thread_spawn,
        forked_from_id: row.get::<_, Option<String>>(5)?.map(SessionId::new),
        model_provider: row.get(6)?,
        model: row.get(7)?,
        effort: row.get(8)?,
        cwd: row.get(9)?,
        cli_version: row.get(10)?,
        originator: row.get(11)?,
        title: row.get(12)?,
        preview: row.get(13)?,
        first_user_message: (!first_user_message.is_empty()).then_some(first_user_message),
        archived_at: row.get::<_, Option<i64>>(15)?.map(timestamp),
    })
}

fn millis(at: EventTimestamp) -> i64 {
    i64::try_from(at.as_millis()).unwrap_or(i64::MAX)
}

fn timestamp(millis: i64) -> EventTimestamp {
    EventTimestamp::from_millis(u64::try_from(millis).unwrap_or_default())
}

/// When the file at `path` was last modified.
pub(crate) fn modified_time(path: &Path) -> Option<EventTimestamp> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(EventTimestamp::from_system_time)
}
