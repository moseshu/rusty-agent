//! `SQLite`-backed [`Session`]: a port of `openai-agents-python`'s `memory/sqlite_session.py`.
//!
//! The layout is the reference's: a sessions table keyed by session id, a messages table whose
//! `AUTOINCREMENT` id orders a session's items, one JSON document per item, and an index on
//! `(session_id, id)`. Several sessions may share one database file; within a process they share
//! one lock per file, so their statements never contend for the database's write lock.
//!
//! # Deviations from the reference
//!
//! - **One connection per session.** The reference opens a connection per worker thread for a
//!   file database and reaps the connections of exited threads. Every statement already runs
//!   under the per-file lock, so the extra connections bought no concurrency; here each session
//!   owns one connection and runs its statements on the blocking pool.
//! - **Items are typed.** The reference stores whatever JSON it is given and skips rows that are
//!   not JSON. That is kept: a row that is not JSON is skipped by reads and dropped by
//!   [`Session::pop_item`]. A row that is JSON but not a [`RunItem`] this build understands — the
//!   shape a record written by a newer build takes — is reported as
//!   [`SessionErrorKind::Corrupted`] instead, and `pop_item` leaves it in place: the reference
//!   never meets that case, since every JSON value is an item to it, and skipping or dropping such
//!   a row would lose history silently.
//! - **Table names are quoted.** The reference interpolates them into its SQL as written; here
//!   they are quoted identifiers, so any name is a single table name.
//! - **Cancellation.** The reference waits for a mutation's outcome before re-raising a caller's
//!   cancellation. A dropped Rust future cannot wait, so the statement runs to completion on the
//!   blocking pool and its outcome is not reported to anyone; it is never left half applied.
//! - The private compaction snapshot the reference offers its compaction session is not ported
//!   with this store.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    item::RunItem,
    session::{Session, SessionId, SessionSettings, resolve_session_limit},
};
use rusqlite::{Connection, ErrorCode, OpenFlags, params, types::ValueRef};

/// Default name of the table holding one row per session.
pub const DEFAULT_SESSIONS_TABLE: &str = "agent_sessions";

/// Default name of the table holding one row per item.
pub const DEFAULT_MESSAGES_TABLE: &str = "agent_messages";

/// A file database's process-local lock and the number of sessions holding it.
type FileLockEntry = (Arc<Mutex<()>>, usize);

/// Process-local locks for file databases, keyed by resolved path. The entry is dropped when the
/// last session on that file closes.
static FILE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, FileLockEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A [`Session`] stored in a `SQLite` database, in a file or in memory.
///
/// An in-memory database lives as long as the session and is not shared with any other. A file
/// database persists, and any number of sessions — with the same or different ids — may open it.
///
/// ```no_run
/// # async fn demo() -> ra_core::error::Result<()> {
/// use ra_session::{Session, SqliteSession};
///
/// let session = SqliteSession::open("sess-demo", "conversations.db")?;
/// let history = session.get_items(None).await?;
/// # let _ = history;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SqliteSession {
    session_id: SessionId,
    session_settings: Option<SessionSettings>,
    sessions_table: String,
    messages_table: String,
    inner: Arc<Inner>,
}

/// Options for opening a [`SqliteSession`], mirroring the reference constructor's parameters.
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `open` is called"]
pub struct SqliteSessionBuilder {
    session_id: SessionId,
    db_path: Option<PathBuf>,
    sessions_table: String,
    messages_table: String,
    session_settings: Option<SessionSettings>,
}

impl SqliteSessionBuilder {
    /// Stores the session in the database file at `db_path`, creating it if absent.
    ///
    /// Without a path the database is in memory.
    pub fn db_path(mut self, db_path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(db_path.into());
        self
    }

    /// Names the table holding one row per session. Defaults to [`DEFAULT_SESSIONS_TABLE`].
    pub fn sessions_table(mut self, name: impl Into<String>) -> Self {
        self.sessions_table = name.into();
        self
    }

    /// Names the table holding one row per item. Defaults to [`DEFAULT_MESSAGES_TABLE`].
    pub fn messages_table(mut self, name: impl Into<String>) -> Self {
        self.messages_table = name.into();
        self
    }

    /// Gives the session default settings: a read without an explicit limit uses theirs.
    pub const fn session_settings(mut self, settings: SessionSettings) -> Self {
        self.session_settings = Some(settings);
        self
    }

    /// Opens the database and creates the tables and index if they do not exist.
    ///
    /// This runs a few statements synchronously; call it from a blocking context, or accept the
    /// short block, as with any constructor that touches the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`SessionErrorKind::Io`] if the database cannot be opened, switched to WAL
    /// journaling, or given its schema.
    pub fn open(self) -> Result<SqliteSession> {
        let sql = Statements::new(&self.sessions_table, &self.messages_table);
        let (connection, lock, lock_key) = match &self.db_path {
            None => {
                let connection = Connection::open_in_memory().map_err(sqlite_error)?;
                configure_connection(&connection).map_err(sqlite_error)?;
                init_schema(&connection, &sql).map_err(sqlite_error)?;
                (connection, Arc::new(Mutex::new(())), None)
            }
            Some(path) => {
                let (lock_key, lock) = acquire_file_lock(path);
                // The schema is created once under the shared lock, since it persists with the
                // file. A failure gives the lock back before reporting.
                let opened = {
                    let _guard = lock_ignoring_poison(&lock);
                    open_file_connection(path).and_then(|connection| {
                        init_schema(&connection, &sql).map_err(sqlite_error)?;
                        Ok(connection)
                    })
                };
                match opened {
                    Ok(connection) => (connection, lock, Some(lock_key)),
                    Err(error) => {
                        release_file_lock(&lock_key);
                        return Err(error);
                    }
                }
            }
        };

        Ok(SqliteSession {
            session_id: self.session_id.clone(),
            session_settings: self.session_settings,
            sessions_table: self.sessions_table,
            messages_table: self.messages_table,
            inner: Arc::new(Inner {
                session_id: self.session_id.as_str().to_owned(),
                db_path: self.db_path,
                sql,
                lock,
                lock_key,
                closed: AtomicBool::new(false),
                state: Mutex::new(State {
                    connection: Some(connection),
                    quarantined: Vec::new(),
                    lock_released: false,
                }),
            }),
        })
    }
}

impl SqliteSession {
    /// Starts options for a session with the given id, defaulting to an in-memory database and
    /// the default table names.
    pub fn builder(session_id: impl Into<SessionId>) -> SqliteSessionBuilder {
        SqliteSessionBuilder {
            session_id: session_id.into(),
            db_path: None,
            sessions_table: DEFAULT_SESSIONS_TABLE.to_owned(),
            messages_table: DEFAULT_MESSAGES_TABLE.to_owned(),
            session_settings: None,
        }
    }

    /// Opens the session `session_id` in the database file at `db_path`, with the default table
    /// names.
    ///
    /// # Errors
    ///
    /// See [`SqliteSessionBuilder::open`].
    pub fn open(session_id: impl Into<SessionId>, db_path: impl Into<PathBuf>) -> Result<Self> {
        Self::builder(session_id).db_path(db_path).open()
    }

    /// Opens the session `session_id` in a new in-memory database that is lost when the session
    /// is dropped.
    ///
    /// # Errors
    ///
    /// See [`SqliteSessionBuilder::open`].
    pub fn open_in_memory(session_id: impl Into<SessionId>) -> Result<Self> {
        Self::builder(session_id).open()
    }

    /// The database file, or `None` for an in-memory database.
    #[must_use]
    pub fn db_path(&self) -> Option<&Path> {
        self.inner.db_path.as_deref()
    }

    /// Name of the table holding one row per session.
    #[must_use]
    pub fn sessions_table(&self) -> &str {
        &self.sessions_table
    }

    /// Name of the table holding one row per item.
    #[must_use]
    pub fn messages_table(&self) -> &str {
        &self.messages_table
    }

    /// Whether [`Self::close`] has run, or the connection had to be abandoned.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Closes the database connection; every later operation fails.
    ///
    /// Waits for an operation in flight on the same database file. Once no connection is left
    /// open, the session gives up its share of the file's lock. Closing again retries a connection
    /// whose earlier close failed and is otherwise a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`SessionErrorKind::Io`] if a connection fails to close. It is kept and retried by
    /// the next call.
    pub fn close(&self) -> Result<()> {
        self.inner.close()
    }

    /// Runs `operation` on the blocking pool and reports its result.
    async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        match tokio::task::spawn_blocking(move || operation(&inner)).await {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(Error::session(
                SessionErrorKind::Io,
                format!("the SQLite session operation did not run: {error}"),
            )),
        }
    }
}

#[async_trait]
impl Session for SqliteSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn session_settings(&self) -> Option<&SessionSettings> {
        self.session_settings.as_ref()
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        let limit = resolve_session_limit(limit, self.session_settings.as_ref());
        self.run(move |inner| inner.get_items(limit)).await
    }

    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        // Checked before the empty-list fast path, which would otherwise succeed on a closed
        // session.
        self.inner.check_not_closed()?;
        if items.is_empty() {
            return Ok(());
        }
        let rows = items.iter().map(encode_item).collect::<Result<Vec<_>>>()?;
        self.run(move |inner| inner.add_rows(&rows)).await
    }

    async fn pop_item(&self) -> Result<Option<RunItem>> {
        self.run(Inner::pop_item).await
    }

    async fn clear(&self) -> Result<()> {
        self.run(Inner::clear).await
    }
}

/// The part of a session its blocking operations share.
#[derive(Debug)]
struct Inner {
    session_id: String,
    db_path: Option<PathBuf>,
    sql: Statements,
    /// Shared by every session on the same file; an in-memory database has its own.
    lock: Arc<Mutex<()>>,
    lock_key: Option<PathBuf>,
    closed: AtomicBool,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// `None` once closed, or after a file connection was abandoned and before it is reopened.
    connection: Option<Connection>,
    /// Connections that failed to close; [`Inner::close`] retries them.
    quarantined: Vec<Connection>,
    lock_released: bool,
}

impl Inner {
    fn check_not_closed(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::caller("SqliteSession is closed"));
        }
        Ok(())
    }

    /// Runs `operation` on the session's connection while holding the file's lock.
    fn with_connection<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _guard = lock_ignoring_poison(&self.lock);
        let mut state = lock_ignoring_poison(&self.state);
        operation(self.connection(&mut state)?)
    }

    /// Runs `operation` in a transaction: committed when it succeeds, rolled back otherwise.
    ///
    /// A connection that cannot roll back is abandoned, so it cannot keep holding the database's
    /// write lock for the sessions that come after: a file connection is reopened by the next
    /// operation, and losing the in-memory one closes the session.
    fn with_transaction<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _guard = lock_ignoring_poison(&self.lock);
        let mut state = lock_ignoring_poison(&self.state);
        let connection = self.connection(&mut state)?;

        connection.execute_batch("BEGIN").map_err(sqlite_error)?;
        let outcome = operation(connection).and_then(|value| {
            connection.execute_batch("COMMIT").map_err(sqlite_error)?;
            Ok(value)
        });
        let rollback_failed = outcome.is_err()
            && !connection.is_autocommit()
            && connection.execute_batch("ROLLBACK").is_err();
        if rollback_failed {
            self.abandon_connection(&mut state);
        }
        outcome
    }

    /// The open connection, reopening a file connection an earlier failure abandoned.
    fn connection<'a>(&self, state: &'a mut State) -> Result<&'a Connection> {
        self.check_not_closed()?;
        if state.connection.is_none() {
            // Only a file connection is ever replaced; losing the in-memory one closes the
            // session, which the check above has already reported.
            let path = self
                .db_path
                .as_deref()
                .ok_or_else(|| Error::caller("SqliteSession is closed"))?;
            state.connection = Some(open_file_connection(path)?);
        }
        state
            .connection
            .as_ref()
            .ok_or_else(|| Error::caller("SqliteSession is closed"))
    }

    fn abandon_connection(&self, state: &mut State) {
        if let Some(connection) = state.connection.take()
            && let Err((connection, _)) = connection.close()
        {
            state.quarantined.push(connection);
            self.closed.store(true, Ordering::Release);
        }
        if self.db_path.is_none() {
            self.closed.store(true, Ordering::Release);
        }
    }

    fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        let sql = &self.sql;
        self.with_connection(|connection| {
            let Some(limit) = limit else {
                let rows = query_rows(connection, &sql.select_all, params![self.session_id])?;
                return decode_rows(rows);
            };
            if limit == 0 {
                return Ok(Vec::new());
            }
            // Widen the window while corrupt rows sit among the newest, so the limit counts
            // valid items, as `pop_item` does.
            let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut window = limit_sql;
            loop {
                let mut rows =
                    query_rows(connection, &sql.select_newest, params![self.session_id, window])?;
                let fetched = rows.len();
                rows.reverse();
                let mut items = decode_rows(rows)?;
                if items.len() >= limit {
                    return Ok(items.split_off(items.len() - limit));
                }
                if i64::try_from(fetched).unwrap_or(i64::MAX) < window || window == i64::MAX {
                    return Ok(items);
                }
                window = window.saturating_mul(2);
            }
        })
    }

    fn add_rows(&self, rows: &[String]) -> Result<()> {
        let sql = &self.sql;
        self.with_transaction(|connection| {
            connection
                .execute(&sql.ensure_session, params![self.session_id])
                .map_err(sqlite_error)?;
            let mut insert = connection.prepare(&sql.insert_item).map_err(sqlite_error)?;
            for row in rows {
                insert
                    .execute(params![self.session_id, row])
                    .map_err(sqlite_error)?;
            }
            connection
                .execute(&sql.touch_session, params![self.session_id])
                .map_err(sqlite_error)?;
            Ok(())
        })
    }

    fn pop_item(&self) -> Result<Option<RunItem>> {
        let sql = &self.sql;
        loop {
            // Each step claims the newest row: `Some(popped)` ends the search, with `None` for an
            // empty session; `None` means the row was not JSON and was dropped. Each corrupt row
            // goes in a transaction of its own, so a later failure does not bring it back.
            let step = self.with_transaction(|connection| {
                let mut statement = connection.prepare(&sql.pop_newest).map_err(sqlite_error)?;
                let mut rows = statement
                    .query(params![self.session_id])
                    .map_err(sqlite_error)?;
                let Some(row) = rows.next().map_err(sqlite_error)? else {
                    return Ok(Some(None));
                };
                let decoded = decode_value(row.get_ref(0).map_err(sqlite_error)?)?;
                // Drain the statement so the deletion completes before the commit.
                while rows.next().map_err(sqlite_error)?.is_some() {}
                Ok(decoded.map(|item| Some(Box::new(item))))
            })?;
            if let Some(popped) = step {
                return Ok(popped.map(|item| *item));
            }
        }
    }

    fn clear(&self) -> Result<()> {
        let sql = &self.sql;
        self.with_transaction(|connection| {
            connection
                .execute(&sql.delete_items, params![self.session_id])
                .map_err(sqlite_error)?;
            connection
                .execute(&sql.delete_session, params![self.session_id])
                .map_err(sqlite_error)?;
            Ok(())
        })
    }

    fn close(&self) -> Result<()> {
        let _guard = lock_ignoring_poison(&self.lock);
        let mut state = lock_ignoring_poison(&self.state);
        self.closed.store(true, Ordering::Release);

        let mut first_error = None;
        let mut remaining = Vec::new();
        let open = state.connection.take().into_iter();
        for connection in open.chain(std::mem::take(&mut state.quarantined)) {
            if let Err((connection, error)) = connection.close() {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                remaining.push(connection);
            }
        }
        state.quarantined = remaining;

        if state.quarantined.is_empty() {
            self.release_lock(&mut state);
        }
        first_error.map_or(Ok(()), |error| Err(sqlite_error(error)))
    }

    fn release_lock(&self, state: &mut State) {
        if !state.lock_released {
            state.lock_released = true;
            if let Some(key) = &self.lock_key {
                release_file_lock(key);
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if !state.lock_released {
            state.lock_released = true;
            if let Some(key) = &self.lock_key {
                release_file_lock(key);
            }
        }
    }
}

/// The SQL a session runs, with its table names quoted in.
#[derive(Debug)]
struct Statements {
    create_sessions: String,
    create_messages: String,
    create_index: String,
    ensure_session: String,
    insert_item: String,
    touch_session: String,
    select_all: String,
    select_newest: String,
    pop_newest: String,
    delete_items: String,
    delete_session: String,
}

impl Statements {
    fn new(sessions_table: &str, messages_table: &str) -> Self {
        let sessions = quote_identifier(sessions_table);
        let messages = quote_identifier(messages_table);
        let index = quote_identifier(&format!("idx_{messages_table}_session_id"));
        Self {
            create_sessions: format!(
                "CREATE TABLE IF NOT EXISTS {sessions} (
                    session_id TEXT PRIMARY KEY,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
                )"
            ),
            create_messages: format!(
                "CREATE TABLE IF NOT EXISTS {messages} (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    message_data TEXT NOT NULL,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY (session_id) REFERENCES {sessions} (session_id)
                        ON DELETE CASCADE
                )"
            ),
            create_index: format!(
                "CREATE INDEX IF NOT EXISTS {index} ON {messages} (session_id, id)"
            ),
            ensure_session: format!("INSERT OR IGNORE INTO {sessions} (session_id) VALUES (?1)"),
            insert_item: format!(
                "INSERT INTO {messages} (session_id, message_data) VALUES (?1, ?2)"
            ),
            touch_session: format!(
                "UPDATE {sessions} SET updated_at = CURRENT_TIMESTAMP WHERE session_id = ?1"
            ),
            select_all: format!(
                "SELECT message_data FROM {messages} WHERE session_id = ?1 ORDER BY id ASC"
            ),
            select_newest: format!(
                "SELECT message_data FROM {messages} WHERE session_id = ?1 \
                 ORDER BY id DESC LIMIT ?2"
            ),
            pop_newest: format!(
                "DELETE FROM {messages} WHERE id = (
                    SELECT id FROM {messages} WHERE session_id = ?1 ORDER BY id DESC LIMIT 1
                ) RETURNING message_data"
            ),
            delete_items: format!("DELETE FROM {messages} WHERE session_id = ?1"),
            delete_session: format!("DELETE FROM {sessions} WHERE session_id = ?1"),
        }
    }
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn init_schema(connection: &Connection, sql: &Statements) -> rusqlite::Result<()> {
    connection.execute_batch("BEGIN")?;
    let created = connection
        .execute(&sql.create_sessions, [])
        .and_then(|_| connection.execute(&sql.create_messages, []))
        .and_then(|_| connection.execute(&sql.create_index, []))
        .and_then(|_| connection.execute_batch("COMMIT"));
    if created.is_err() && !connection.is_autocommit() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    created
}

/// Opens a file database without URI interpretation, as the reference's `sqlite3.connect` does.
fn open_file_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sqlite_error)?;
    configure_connection(&connection).map_err(sqlite_error)?;
    Ok(connection)
}

/// Enables WAL journaling, retrying the transient lock another process can hold while it
/// initializes WAL on the same file, for as long as the connection's busy timeout allows.
///
/// Foreign keys are turned off, as they are on the reference's connections: the bundled `SQLite`
/// enables them by default, which would make the schema's `ON DELETE CASCADE` act where the
/// reference's never does.
fn configure_connection(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "foreign_keys", false)?;
    let timeout_ms: i64 = connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    let deadline = Instant::now() + Duration::from_millis(u64::try_from(timeout_ms).unwrap_or(0));
    loop {
        match connection.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())) {
            Ok(()) => return Ok(()),
            Err(error) if is_locked(&error) && Instant::now() < deadline => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// The key two paths to the same file share: the path with symlinks and relative components
/// resolved, as far as they exist.
fn lock_key(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map_or_else(|_| absolute.clone(), |parent| parent.join(name)),
        _ => absolute,
    }
}

fn acquire_file_lock(path: &Path) -> (PathBuf, Arc<Mutex<()>>) {
    let key = lock_key(path);
    let mut locks = lock_ignoring_poison(&FILE_LOCKS);
    let entry = locks
        .entry(key.clone())
        .or_insert_with(|| (Arc::new(Mutex::new(())), 0));
    entry.1 += 1;
    (key, Arc::clone(&entry.0))
}

fn release_file_lock(key: &Path) {
    let mut locks = lock_ignoring_poison(&FILE_LOCKS);
    if let Some(entry) = locks.get_mut(key) {
        if entry.1 <= 1 {
            locks.remove(key);
        } else {
            entry.1 -= 1;
        }
    }
}

/// Poisoning is recovered, as [`InMemorySession`](crate::InMemorySession) does: the guarded data
/// is a lock token or a connection whose open transaction rolled back as the panic unwound.
fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One stored `message_data` value, still undecoded.
enum RawRow {
    Bytes(Vec<u8>),
    NotText,
}

fn query_rows(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<RawRow>> {
    let mut statement = connection.prepare_cached(sql).map_err(sqlite_error)?;
    let rows = statement
        .query_map(params, |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => RawRow::Bytes(bytes.to_vec()),
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => RawRow::NotText,
            })
        })
        .map_err(sqlite_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sqlite_error)
}

/// Decodes rows in order, skipping the ones that are not JSON.
fn decode_rows(rows: Vec<RawRow>) -> Result<Vec<RunItem>> {
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let decoded = match row {
            RawRow::Bytes(bytes) => decode_item(&bytes)?,
            RawRow::NotText => None,
        };
        items.extend(decoded);
    }
    Ok(items)
}

fn decode_value(value: ValueRef<'_>) -> Result<Option<RunItem>> {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => decode_item(bytes),
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => Ok(None),
    }
}

fn sqlite_error(error: rusqlite::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("SQLite session storage failed: {error}"),
    )
    .with_source(error)
}

/// Decodes one stored item.
///
/// `None` for bytes that are not JSON: the reference's stores skip those on read and drop them on
/// pop. JSON that is not a [`RunItem`] is an error rather than another skip, because that is what
/// an item written by a newer build looks like here, and skipping or dropping it would lose
/// history silently.
fn decode_item(bytes: &[u8]) -> Result<Option<RunItem>> {
    match serde_json::from_slice::<RunItem>(bytes) {
        Ok(item) => Ok(Some(item)),
        // A typed decoder can stop before the bad syntax, such as at `null` in
        // `null trailing-garbage`. Preserve an incompatible item only after validating the
        // entire JSON document; malformed JSON keeps the reference's skip/drop behavior.
        Err(error)
            if error.is_data() && serde_json::from_slice::<serde_json::Value>(bytes).is_ok() =>
        {
            Err(Error::session(
                SessionErrorKind::Corrupted,
                format!(
                    "a stored session item is JSON but not an item this build can read; it may have \
                     been written by a newer version: {error}"
                ),
            ))
        }
        Err(_) => Ok(None),
    }
}

/// Encodes one item as the JSON document stored in its row.
fn encode_item(item: &RunItem) -> Result<String> {
    serde_json::to_string(item).map_err(|e| {
        Error::session(
            SessionErrorKind::Corrupted,
            format!(
                "failed to serialize session item {}: {e}",
                item.id().as_str()
            ),
        )
    })
}
