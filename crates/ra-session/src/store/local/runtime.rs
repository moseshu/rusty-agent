//! Opening the state database and running statements on it: Codex's `StateRuntime`
//! (`state/src/runtime.rs`, `state/src/sqlite.rs`) as far as its thread table goes.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::EventTimestamp,
};
use rusqlite::{Connection, TransactionBehavior};

/// The file the state database is kept in, inside its home directory. Codex names its own
/// `state_5.sqlite` and changes the number when a schema change cannot be migrated; this schema is
/// not Codex's, so it starts at its own first version.
pub const STATE_DB_FILENAME: &str = "state_1.sqlite";

/// The most connections the database is open on at once: Codex's `max_connections(5)`.
const MAX_CONNECTIONS: usize = 5;

/// How long a statement waits for another connection's write lock: Codex's `busy_timeout`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The schema, version by version: Codex's migrations, folded into the one this port starts from.
///
/// The thread table keeps the columns of Codex's that this framework's thread metadata has, under
/// Codex's names, with two of this framework's: `thread_spawn`, the JSON of where a spawned thread
/// was spawned from, which Codex keeps as its `source`, and `forked_from_id`, which Codex reads
/// from the rollout instead. Codex's legacy history mode keeps an explicit name in `title`, as
/// here; its `name` column belongs to its paginated mode, which is not ported. Its indexes are Codex's:
/// creation and update order, with the archived flag and working directory in front for filtered
/// listings, and partial indexes for the active listing, which skips threads without a preview, and
/// for the archived one. Every index ends in the id, which breaks ties between threads of one time.
const MIGRATIONS: &[&str] = &[r"
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    rollout_path TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    thread_spawn TEXT,
    forked_from_id TEXT,
    model_provider TEXT,
    model TEXT,
    reasoning_effort TEXT,
    cwd TEXT,
    cli_version TEXT,
    originator TEXT,
    title TEXT NOT NULL DEFAULT '',
    preview TEXT NOT NULL DEFAULT '',
    first_user_message TEXT NOT NULL DEFAULT '',
    archived INTEGER NOT NULL DEFAULT 0,
    archived_at_ms INTEGER
);

CREATE INDEX idx_threads_created_at_ms ON threads(created_at_ms DESC, id DESC);
CREATE INDEX idx_threads_updated_at_ms ON threads(updated_at_ms DESC, id DESC);
CREATE INDEX idx_threads_provider ON threads(model_provider);
CREATE INDEX idx_threads_archived_cwd_created_at_ms
    ON threads(archived, cwd, created_at_ms DESC, id DESC);
CREATE INDEX idx_threads_archived_cwd_updated_at_ms
    ON threads(archived, cwd, updated_at_ms DESC, id DESC);
CREATE INDEX idx_threads_visible_created_at_ms
    ON threads(archived, created_at_ms DESC, id DESC)
    WHERE preview <> '';
CREATE INDEX idx_threads_visible_updated_at_ms
    ON threads(archived, updated_at_ms DESC, id DESC)
    WHERE preview <> '';
CREATE INDEX idx_threads_archive_created_at_ms
    ON threads(archived, created_at_ms DESC, id DESC)
    WHERE archived = 1;
CREATE INDEX idx_threads_archive_updated_at_ms
    ON threads(archived, updated_at_ms DESC, id DESC)
    WHERE archived = 1;

CREATE TABLE backfill_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    status TEXT NOT NULL,
    last_watermark TEXT,
    last_success_at INTEGER,
    updated_at INTEGER NOT NULL
);

INSERT INTO backfill_state (id, status, last_watermark, last_success_at, updated_at)
VALUES (1, 'pending', NULL, NULL, CAST(strftime('%s', 'now') AS INTEGER))
ON CONFLICT(id) DO NOTHING;
"];

/// The state database a rollout directory keeps beside its rollouts: Codex's `StateRuntime`, with
/// its thread table and the state of its first backfill.
///
/// It indexes what a listing shows of each thread, so a listing can read one page of rows instead
/// of the head of every rollout. The rollouts stay the record: the database is filled from them
/// once, kept in step as threads are written, archived and deleted, and repaired from them when a
/// listing finds it behind. It is shared by every handle of the directory it was attached to, and
/// is safe to use from several processes at once.
///
/// Statements run on the blocking pool over at most five connections, as Codex's pool allows. A
/// statement whose caller stops waiting still runs to completion.
#[derive(Debug)]
pub struct StateRuntime {
    path: PathBuf,
    pool: Arc<Pool>,
    /// The latest update time handed out, in milliseconds: Codex's `thread_updated_at_millis`.
    pub(super) thread_updated_at_millis: Arc<AtomicI64>,
}

impl StateRuntime {
    /// Opens the state database in `home`, creating the directory and the database if they are
    /// missing and migrating its schema: Codex's `StateRuntime::init`.
    ///
    /// A database a newer build has migrated further opens as it is, as Codex's migrator ignores
    /// migrations it does not know; its schema only ever grows.
    /// The latest stored update time seeds the allocator, preserving ordering across reopenings.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the database cannot be opened or
    /// migrated.
    pub async fn init(home: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let home = home.into();
        let path = home.join(STATE_DB_FILENAME);
        let pool = Arc::new(Pool::new(path.clone()));
        let migrating = Arc::clone(&pool);
        let opened = path.clone();
        let latest_updated_at = tokio::task::spawn_blocking(move || -> Result<i64> {
            std::fs::create_dir_all(&home).map_err(|error| {
                Error::session(
                    SessionErrorKind::Io,
                    format!(
                        "failed to create the state database directory `{}`: {error}",
                        home.display()
                    ),
                )
            })?;
            let mut connection = migrating
                .checkout()
                .map_err(|error| open_error(&opened, &error))?;
            migrate(&mut connection).map_err(|error| open_error(&opened, &error))?;
            let latest: Option<i64> = connection
                .query_row("SELECT MAX(updated_at_ms) FROM threads", [], |row| {
                    row.get(0)
                })
                .map_err(|error| open_error(&opened, &error))?;
            Ok(latest.unwrap_or(0))
        })
        .await
        .map_err(|error| {
            Error::session(
                SessionErrorKind::Io,
                format!("opening the state database did not complete: {error}"),
            )
        })??;
        Ok(Arc::new(Self {
            path,
            pool,
            thread_updated_at_millis: Arc::new(AtomicI64::new(latest_updated_at)),
        }))
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `statement` on a pooled connection on the blocking pool.
    pub(crate) async fn run<T, F>(&self, statement: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || pool.run(statement))
            .await
            .map_err(|error| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("a state database statement did not complete: {error}"),
                )
            })?
    }

    /// Runs `statement` on a pooled connection on this thread, for work already on the blocking
    /// pool.
    pub(crate) fn run_blocking<T>(
        &self,
        statement: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T> {
        self.pool.run(statement)
    }

    /// An update time to store for `updated_at`: Codex's `allocate_thread_updated_at`.
    ///
    /// Times newer than any handed out are kept and become the latest. Times within a second of
    /// the latest move just past it, so threads written in quick succession keep distinct,
    /// increasing update times and order the way they were written. Older times — a backfill or a
    /// repair restoring a rollout's modification time — are kept as they are.
    pub(crate) fn allocate_thread_updated_at(&self, updated_at: EventTimestamp) -> EventTimestamp {
        allocate_thread_updated_at(&self.thread_updated_at_millis, updated_at)
    }
}

/// Allocates an update time from the shared counter, including inside a blocking transaction.
pub(super) fn allocate_thread_updated_at(
    latest: &AtomicI64,
    updated_at: EventTimestamp,
) -> EventTimestamp {
    let candidate = i64::try_from(updated_at.as_millis()).unwrap_or(i64::MAX);
    let allocated = loop {
        let current = latest.load(Ordering::Relaxed);
        if candidate > current {
            if latest
                .compare_exchange(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break candidate;
            }
            continue;
        }
        if candidate.saturating_add(1000) <= current {
            break candidate;
        }
        let bumped = current.saturating_add(1);
        if latest
            .compare_exchange(current, bumped, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break bumped;
        }
    };
    EventTimestamp::from_millis(u64::try_from(allocated).unwrap_or_default())
}

/// Brings the schema up to the newest version this build knows, in one immediate transaction so
/// two processes opening a new database do not both create it.
fn migrate(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: usize = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
        transaction.execute_batch(migration)?;
        transaction.pragma_update(None, "user_version", index + 1)?;
    }
    transaction.commit()
}

/// Up to [`MAX_CONNECTIONS`] connections to the database, opened as they are needed.
#[derive(Debug)]
struct Pool {
    path: PathBuf,
    state: Mutex<PoolState>,
    released: Condvar,
}

#[derive(Debug, Default)]
struct PoolState {
    idle: Vec<Connection>,
    open: usize,
}

impl Pool {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(PoolState::default()),
            released: Condvar::new(),
        }
    }

    fn run<T>(&self, statement: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut connection = self
            .checkout()
            .map_err(|error| open_error(&self.path, &error))?;
        statement(&mut connection).map_err(|error| statement_error(&self.path, &error))
    }

    /// An idle connection, or a new one while fewer than [`MAX_CONNECTIONS`] are open; otherwise
    /// waits for one to be returned.
    fn checkout(&self) -> rusqlite::Result<Pooled<'_>> {
        let mut state = self.lock();
        loop {
            if let Some(connection) = state.idle.pop() {
                return Ok(Pooled {
                    pool: self,
                    connection: Some(connection),
                });
            }
            if state.open < MAX_CONNECTIONS {
                state.open += 1;
                drop(state);
                return match open_connection(&self.path) {
                    Ok(connection) => Ok(Pooled {
                        pool: self,
                        connection: Some(connection),
                    }),
                    Err(error) => {
                        self.lock().open -= 1;
                        self.released.notify_one();
                        Err(error)
                    }
                };
            }
            state = self
                .released
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection checked out of the pool, returned to it when dropped.
struct Pooled<'a> {
    pool: &'a Pool,
    connection: Option<Connection>,
}

impl std::ops::Deref for Pooled<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection
            .as_ref()
            .unwrap_or_else(|| unreachable!("a pooled connection is held until dropped"))
    }
}

impl std::ops::DerefMut for Pooled<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection
            .as_mut()
            .unwrap_or_else(|| unreachable!("a pooled connection is held until dropped"))
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.lock().idle.push(connection);
            self.pool.released.notify_one();
        }
    }
}

/// Opens a connection as Codex's pool opens each of its own: a busy timeout, auto-vacuum,
/// write-ahead logging
/// — retried, as the `SQLite` session retries it, while another process sets it up on the same
/// file — and normal synchronization. Foreign keys stay off, as the tables have none.
fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    // Incremental auto-vacuum, as Codex's pool sets it. It takes effect only on a database that
    // is still empty, so it is set before anything — even the journal mode — is written to it.
    connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    crate::sqlite::configure_connection(&connection)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(connection)
}

fn open_error(path: &Path, error: &rusqlite::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!(
            "failed to open the state database `{}`: {error}",
            path.display()
        ),
    )
}

fn statement_error(path: &Path, error: &rusqlite::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!(
            "a statement on the state database `{}` failed: {error}",
            path.display()
        ),
    )
}
