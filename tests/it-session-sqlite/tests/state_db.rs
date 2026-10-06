//! The state database a rollout directory keeps beside its rollouts, as Codex's local store keeps
//! its `SQLite` state database (`state/src/runtime/threads.rs`, `state/src/runtime/backfill.rs`,
//! `rollout/src/state_db_tests.rs`, `rollout/src/metadata_tests.rs`,
//! `thread-store/src/local/{list_threads,archive_thread,unarchive_thread,delete_thread}.rs`).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use ra_core::{
    agent::control::AgentPath,
    event::EventTimestamp,
    item::{AgentId, Message, ModelInputItem},
    session::{
        SessionId,
        rollout::{
            PersistContext, RolloutItem, RolloutRecorder, RolloutRunStarted, RolloutThreadSpawn,
            RolloutThreadStore, RolloutTurnContext,
        },
    },
    state::RunId,
};
use ra_session::{
    ArchiveThreadParams, CreateThreadParams, DeleteThreadParams, ListThreadsParams,
    ReadThreadParams, RolloutSessionMeta, RolloutThreadDirectory, RolloutWriter, SortDirection,
    StoredThread, ThreadMetadataPatch, ThreadSortKey, ThreadStore, UpdateThreadMetadataParams,
    store::local::{self, BackfillStatus, STATE_DB_FILENAME, StateRuntime},
};
use rusqlite::{Connection, params};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("state_db")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A directory with its state database opened and backfilled.
async fn indexed(dir: &Path) -> (RolloutThreadDirectory, Arc<StateRuntime>) {
    let db = local::try_init(dir).await.unwrap();
    (
        RolloutThreadDirectory::new(dir).with_state_db(Arc::clone(&db)),
        db,
    )
}

/// A thread as a session writes it, straight into a rollout file.
struct Thread<'a> {
    id: &'a str,
    created_at: u64,
    user: Option<&'a str>,
    provider: &'a str,
    cwd: &'a str,
    model: Option<&'a str>,
}

impl<'a> Thread<'a> {
    const fn new(id: &'a str, created_at: u64) -> Self {
        Self {
            id,
            created_at,
            user: Some("hello"),
            provider: "openai",
            cwd: "/work",
            model: None,
        }
    }

    async fn write(self, dir: &Path) -> PathBuf {
        let session_id = SessionId::new(self.id);
        let path = RolloutThreadDirectory::new(dir)
            .rollout_path(&session_id)
            .unwrap();
        let meta = RolloutSessionMeta::new(session_id.clone())
            .with_created_at(EventTimestamp::from_millis(self.created_at))
            .with_model_provider(self.provider)
            .with_cwd(self.cwd);
        let mut writer = RolloutWriter::open(&path, session_id).await.unwrap();
        writer.append_session_meta(meta).await.unwrap();
        if let Some(model) = self.model {
            writer
                .append_turn_context(
                    RolloutTurnContext::new(RunId::new("run-1"), 0)
                        .with_model(model)
                        .with_effort("high"),
                )
                .await
                .unwrap();
        }
        if let Some(user) = self.user {
            writer
                .append(
                    RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
                        .with_input(vec![ModelInputItem::Message(Message::user(user))]),
                )
                .await
                .unwrap();
        }
        writer.sync_all().await.unwrap();
        drop(writer);
        path
    }
}

fn set_modified(path: &Path, millis: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(millis))
        .unwrap();
}

fn meta(id: &str) -> RolloutSessionMeta {
    RolloutSessionMeta::new(SessionId::new(id))
        .with_created_at(EventTimestamp::from_millis(1_000))
        .with_model_provider("openai")
        .with_cwd("/work/./project/")
}

fn run(text: &str) -> RolloutItem {
    RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
            .with_input(vec![ModelInputItem::Message(Message::user(text))]),
    )
}

fn turn(model: &str, effort: &str) -> RolloutItem {
    RolloutItem::TurnContext(
        RolloutTurnContext::new(RunId::new("run-1"), 0)
            .with_model(model)
            .with_effort(effort),
    )
}

/// A live thread created through `store`, with one turn recorded and persisted.
async fn live_thread(
    store: &RolloutThreadDirectory,
    id: &str,
    text: &str,
) -> Arc<dyn RolloutRecorder> {
    let recorder = store
        .create_thread_with(&CreateThreadParams::new(meta(id)))
        .await
        .unwrap();
    recorder.record(run(text));
    recorder.record(turn("gpt-5", "high"));
    recorder.persist(PersistContext::Standard).await.unwrap();
    recorder
}

fn ids(threads: &[StoredThread]) -> Vec<&str> {
    threads
        .iter()
        .map(|thread| thread.session_id().as_str())
        .collect()
}

/// Every page of a listing, followed through its cursors.
async fn all_pages(store: &RolloutThreadDirectory, params: &ListThreadsParams) -> Vec<String> {
    let mut found = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut page_params = params.clone();
        if let Some(cursor) = &cursor {
            page_params = page_params.with_cursor(cursor.clone());
        }
        let page = store.list_threads(&page_params).await.unwrap();
        found.extend(ids(page.items()).into_iter().map(str::to_owned));
        match page.next_cursor() {
            Some(next) => cursor = Some(next.to_owned()),
            None => return found,
        }
    }
}

fn db_connection(dir: &Path) -> Connection {
    let connection = Connection::open(dir.join(STATE_DB_FILENAME)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}

fn unix_seconds() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Opening and backfilling
// ---------------------------------------------------------------------------------------------

/// The database opens with Codex's settings — write-ahead logging, incremental auto-vacuum — and
/// Codex's thread indexes.
#[tokio::test]
async fn the_database_opens_with_codexs_settings_and_indexes() {
    let dir = temp_dir("settings");
    let db = local::try_init(&dir).await.unwrap();
    assert_eq!(db.path(), dir.join(STATE_DB_FILENAME));

    let connection = db_connection(&dir);
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    let auto_vacuum: i64 = connection
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto_vacuum, 2, "incremental");
    let mut indexes = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'threads' AND sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    indexes.sort();
    assert_eq!(
        indexes,
        [
            "idx_threads_archive_created_at_ms",
            "idx_threads_archive_updated_at_ms",
            "idx_threads_archived_cwd_created_at_ms",
            "idx_threads_archived_cwd_updated_at_ms",
            "idx_threads_created_at_ms",
            "idx_threads_provider",
            "idx_threads_updated_at_ms",
            "idx_threads_visible_created_at_ms",
            "idx_threads_visible_updated_at_ms",
        ]
    );
    // The listing of the database walks the partial index rather than sorting the table.
    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT id FROM threads WHERE archived = 0 AND preview <> '' \
             ORDER BY created_at_ms DESC, id DESC LIMIT 26",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("idx_threads_visible_created_at_ms") && !plan.contains("TEMP B-TREE"),
        "{plan}"
    );
}

/// A database a newer build migrated further still opens, as Codex's migrator ignores
/// migrations it does not know.
#[tokio::test]
async fn a_database_migrated_by_a_newer_build_still_opens() {
    let dir = temp_dir("newer");
    drop(local::try_init(&dir).await.unwrap());
    db_connection(&dir)
        .execute_batch("PRAGMA user_version = 7; ALTER TABLE threads ADD COLUMN later TEXT;")
        .unwrap();
    let (store, _db) = indexed(&dir).await;
    live_thread(&store, "after", "still works").await;
    let page = store
        .list_threads(&ListThreadsParams::new(10).with_state_db_only())
        .await
        .unwrap();
    assert_eq!(ids(page.items()), ["after"]);
}

/// Codex's `backfill_sessions_resumes_from_watermark_and_marks_complete`, with the archive read
/// as well: a backfill claimed by a process that stopped, past its lease, is taken over and
/// resumed after its last checkpoint.
#[tokio::test]
async fn the_backfill_resumes_from_its_watermark_reads_the_archive_and_completes() {
    let dir = temp_dir("backfill");
    Thread::new("a-first", 1_000).write(&dir).await;
    Thread::new("b-second", 2_000).write(&dir).await;
    Thread::new("c-archived", 3_000).write(&dir).await;
    let plain = RolloutThreadDirectory::new(&dir);
    for id in ["a-first", "c-archived"] {
        plain
            .archive_thread(&ArchiveThreadParams::new(SessionId::new(id)))
            .await
            .unwrap();
    }

    // Watermarks are paths relative to the directory, so the archive comes first.
    drop(StateRuntime::init(&dir).await.unwrap());
    db_connection(&dir)
        .execute(
            "UPDATE backfill_state SET status = 'running', last_watermark = ?1, updated_at = ?2",
            params![
                "archived_sessions/rollout-a-first.jsonl",
                unix_seconds() - 3_600
            ],
        )
        .unwrap();

    let db = local::try_init(&dir).await.unwrap();
    assert_eq!(
        db.get_thread(&SessionId::new("a-first")).await.unwrap(),
        None
    );
    let second = db
        .get_thread(&SessionId::new("b-second"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.preview(), "hello");
    assert_eq!(second.created_at(), EventTimestamp::from_millis(2_000));
    let archived = db
        .get_thread(&SessionId::new("c-archived"))
        .await
        .unwrap()
        .unwrap();
    assert!(archived.archived_at().is_some());
    assert!(
        archived
            .rollout_path()
            .ends_with("archived_sessions/rollout-c-archived.jsonl")
    );

    let state = db.get_backfill_state().await.unwrap();
    assert_eq!(state.status(), BackfillStatus::Complete);
    assert_eq!(state.last_watermark(), Some("rollout-b-second.jsonl"));
    assert!(state.last_success_at().is_some());
}

/// Codex's `try_init_waits_for_concurrent_startup_backfill`: a backfill another process holds
/// within its lease is waited for, not run twice.
#[tokio::test]
async fn startup_waits_for_a_backfill_another_process_is_running() {
    let dir = temp_dir("backfill_wait");
    Thread::new("waiting", 1_000).write(&dir).await;
    drop(StateRuntime::init(&dir).await.unwrap());
    db_connection(&dir)
        .execute(
            "UPDATE backfill_state SET status = 'running', updated_at = ?1",
            params![unix_seconds()],
        )
        .unwrap();
    let completing = dir.clone();
    let other_process = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        db_connection(&completing)
            .execute("UPDATE backfill_state SET status = 'complete'", [])
            .unwrap();
    });

    let db = local::try_init(&dir).await.unwrap();
    other_process.await.unwrap();
    assert_eq!(
        db.get_backfill_state().await.unwrap().status(),
        BackfillStatus::Complete
    );
    // The other process's backfill was trusted, so this one read nothing.
    assert_eq!(
        db.get_thread(&SessionId::new("waiting")).await.unwrap(),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// Keeping it in step
// ---------------------------------------------------------------------------------------------

/// What a live thread derives reaches its row, which is created from the first patch, before the
/// rollout is read by anything: Codex's `ThreadMetadataSync` writing through
/// `record_thread_metadata`.
#[tokio::test]
async fn a_live_thread_writes_its_derived_metadata_to_its_row() {
    let dir = temp_dir("live");
    let (store, db) = indexed(&dir).await;
    let recorder = live_thread(&store, "live", "  derive me  ").await;

    let row = db
        .get_thread(&SessionId::new("live"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.preview(), "derive me");
    assert_eq!(row.first_user_message(), Some("derive me"));
    assert_eq!(row.title(), "derive me");
    assert_eq!(row.model(), Some("gpt-5"));
    assert_eq!(row.effort(), Some("high"));
    assert_eq!(row.model_provider(), Some("openai"));
    assert_eq!(
        row.cwd(),
        Some("/work/project"),
        "normalized as a listing compares it"
    );
    assert_eq!(row.created_at(), EventTimestamp::from_millis(1_000));
    assert_eq!(
        row.rollout_path(),
        store.rollout_path(&SessionId::new("live")).unwrap()
    );
    assert_eq!(row.archived_at(), None);
    recorder.shutdown().await.unwrap();

    // A spawned thread's row says where it was spawned from, and a fork's what it was forked
    // from, as the rollout's session metadata does.
    let spawn = RolloutThreadSpawn::new(
        SessionId::new("live"),
        SessionId::new("live"),
        1,
        AgentPath::root().join("worker").unwrap(),
    );
    let child = store
        .create_thread(&SessionId::new("child"), &spawn)
        .await
        .unwrap();
    child.record(run("work"));
    child.flush().await.unwrap();
    let row = db
        .get_thread(&SessionId::new("child"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.thread_spawn(), Some(&spawn));

    let fork = store
        .create_thread_with(&CreateThreadParams::new(
            meta("fork").with_forked_from_id(SessionId::new("live")),
        ))
        .await
        .unwrap();
    fork.record(run("forked"));
    fork.flush().await.unwrap();
    let row = db
        .get_thread(&SessionId::new("fork"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.forked_from_id(), Some(&SessionId::new("live")));
}

/// Reading a thread adds what only the database knows — the model and effort, which the rollout
/// directory keeps nowhere else — as Codex's `read_thread` does in its legacy history mode.
#[tokio::test]
async fn reading_a_thread_adds_the_model_and_effort_its_row_holds() {
    let dir = temp_dir("read");
    let (store, _db) = indexed(&dir).await;
    live_thread(&store, "read", "hello")
        .await
        .shutdown()
        .await
        .unwrap();

    let read = store
        .read_thread(&ReadThreadParams::new(SessionId::new("read")))
        .await
        .unwrap();
    assert_eq!(read.model(), Some("gpt-5"));
    assert_eq!(read.effort(), Some("high"));

    let without = RolloutThreadDirectory::new(&dir)
        .read_thread(&ReadThreadParams::new(SessionId::new("read")))
        .await
        .unwrap();
    assert_eq!(without.model(), None);
}

/// Codex's `reconcile_rollout_preserves_existing_explicit_title`: a name is kept in the row's
/// title and survives the row being rebuilt from the rollout; clearing it brings back the title
/// derived from the first user message.
#[tokio::test]
async fn a_name_is_kept_in_the_title_and_survives_a_rebuild() {
    let dir = temp_dir("name");
    let (store, db) = indexed(&dir).await;
    live_thread(&store, "named", "Hey")
        .await
        .shutdown()
        .await
        .unwrap();
    let rename = |name: Option<&str>| {
        UpdateThreadMetadataParams::new(
            SessionId::new("named"),
            ThreadMetadataPatch::new().with_name(name.map(str::to_owned)),
        )
    };

    let renamed = store
        .update_thread_metadata(&rename(Some("math")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.name(), Some("math"));
    let row = db
        .get_thread(&SessionId::new("named"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.title(), "math");
    assert_eq!(row.first_user_message(), Some("Hey"));
    let page = store
        .list_threads(&ListThreadsParams::new(5).with_state_db_only())
        .await
        .unwrap();
    assert_eq!(page.items()[0].name(), Some("math"));

    store.update_thread_metadata(&rename(None)).await.unwrap();
    let row = db
        .get_thread(&SessionId::new("named"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.title(), "Hey");
    let page = store
        .list_threads(&ListThreadsParams::new(5).with_state_db_only())
        .await
        .unwrap();
    assert_eq!(page.items()[0].name(), None);
}

/// Codex's archive and unarchive tests that update its state database, and its deletion of a
/// thread's state: the row follows the rollout, and goes with it.
#[tokio::test]
async fn archiving_unarchiving_and_deleting_carry_the_row_along() {
    let dir = temp_dir("archive");
    let (store, db) = indexed(&dir).await;
    live_thread(&store, "moved", "hello")
        .await
        .shutdown()
        .await
        .unwrap();
    let id = SessionId::new("moved");
    let active = ListThreadsParams::new(5).with_state_db_only();
    let archived = ListThreadsParams::new(5).with_state_db_only().archived();

    store
        .archive_thread(&ArchiveThreadParams::new(id.clone()))
        .await
        .unwrap();
    let row = db.get_thread(&id).await.unwrap().unwrap();
    assert!(row.archived_at().is_some());
    assert!(
        row.rollout_path()
            .ends_with("archived_sessions/rollout-moved.jsonl")
    );
    assert!(
        store
            .list_threads(&active)
            .await
            .unwrap()
            .items()
            .is_empty()
    );
    assert_eq!(
        ids(store.list_threads(&archived).await.unwrap().items()),
        ["moved"]
    );

    store
        .unarchive_thread(&ArchiveThreadParams::new(id.clone()))
        .await
        .unwrap();
    let row = db.get_thread(&id).await.unwrap().unwrap();
    assert_eq!(row.archived_at(), None);
    assert_eq!(row.rollout_path(), store.rollout_path(&id).unwrap());
    assert_eq!(
        ids(store.list_threads(&active).await.unwrap().items()),
        ["moved"]
    );

    store
        .delete_thread(&DeleteThreadParams::new(id.clone()))
        .await
        .unwrap();
    assert_eq!(db.get_thread(&id).await.unwrap(), None);
}

/// As Codex's local store restores the moves its database cannot follow: an archive whose row
/// cannot be written leaves the rollout where it was, and reports why. A deletion it cannot follow
/// is reported once the files are gone, as Codex deletes the rows last.
#[tokio::test]
async fn a_move_the_database_cannot_follow_is_undone() {
    let dir = temp_dir("undo");
    let (store, db) = indexed(&dir).await;
    live_thread(&store, "stuck", "hello")
        .await
        .shutdown()
        .await
        .unwrap();
    let id = SessionId::new("stuck");
    db_connection(&dir)
        .execute_batch(
            "CREATE TRIGGER refuse_update BEFORE UPDATE ON threads BEGIN SELECT RAISE(FAIL, 'refused'); END;
             CREATE TRIGGER refuse_delete BEFORE DELETE ON threads BEGIN SELECT RAISE(FAIL, 'refused'); END;",
        )
        .unwrap();

    let error = store
        .archive_thread(&ArchiveThreadParams::new(id.clone()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
    assert!(store.rollout_path(&id).unwrap().is_file());
    assert!(!dir.join("archived_sessions/rollout-stuck.jsonl").exists());
    assert_eq!(
        db.get_thread(&id).await.unwrap().unwrap().archived_at(),
        None
    );

    let error = store
        .delete_thread(&DeleteThreadParams::new(id.clone()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
    assert!(!store.rollout_path(&id).unwrap().exists());
}

/// Metadata a thread writes before its rollout exists gives it a row at once, which a listing of
/// the database leaves out while the rollout is not there, as Codex leaves out a row with a stale
/// path.
#[tokio::test]
async fn a_row_without_its_rollout_is_not_listed() {
    let dir = temp_dir("unwritten");
    let (store, db) = indexed(&dir).await;
    store
        .record_thread_metadata(&UpdateThreadMetadataParams::new(
            SessionId::new("ghost"),
            ThreadMetadataPatch::new()
                .with_preview("boo")
                .with_created_at(EventTimestamp::from_millis(5_000)),
        ))
        .await
        .unwrap();
    let row = db
        .get_thread(&SessionId::new("ghost"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.created_at(), EventTimestamp::from_millis(5_000));
    assert_eq!(
        row.rollout_path(),
        store.rollout_path(&SessionId::new("ghost")).unwrap()
    );

    // Created before the row without a rollout, so it sorts after it.
    live_thread(&store, "real", "hello").await;
    let page = store
        .list_threads(&ListThreadsParams::new(1).with_state_db_only())
        .await
        .unwrap();
    // The page is filled from the rows after the one left out.
    assert_eq!(ids(page.items()), ["real"]);
}

// ---------------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------------

/// A listing of the database alone returns what a listing of the rollouts returns, in the same
/// order, page by page, for both sorts and directions, active and archived, filtered or not.
#[tokio::test]
async fn the_database_lists_what_the_rollouts_list() {
    let dir = temp_dir("parity");
    for (index, id) in ["t-a", "t-b", "t-c", "t-d", "t-e"].into_iter().enumerate() {
        let mut thread = Thread::new(id, 1_000 * (u64::try_from(index).unwrap() % 3));
        if index == 3 {
            thread.provider = "other";
            thread.cwd = "/elsewhere";
        }
        let path = thread.write(&dir).await;
        set_modified(
            &path,
            10_000 - 1_000 * u64::try_from(index).unwrap() % 2_000,
        );
    }
    let mut silent = Thread::new("t-silent", 500);
    silent.user = None;
    silent.write(&dir).await;
    Thread::new("t-old", 1).write(&dir).await;
    RolloutThreadDirectory::new(&dir)
        .archive_thread(&ArchiveThreadParams::new(SessionId::new("t-old")))
        .await
        .unwrap();

    let plain = RolloutThreadDirectory::new(&dir);
    let (store, _db) = indexed(&dir).await;
    for sort_key in [ThreadSortKey::CreatedAt, ThreadSortKey::UpdatedAt] {
        for direction in [SortDirection::Desc, SortDirection::Asc] {
            for params in [
                ListThreadsParams::new(2),
                ListThreadsParams::new(2).archived(),
                ListThreadsParams::new(2).with_model_providers(vec!["openai".to_owned()]),
                ListThreadsParams::new(2).with_cwd_filters(vec!["/work/".to_owned()]),
            ] {
                let params = params
                    .with_sort_key(sort_key)
                    .with_sort_direction(direction);
                let expected = all_pages(&plain, &params).await;
                assert_eq!(
                    all_pages(&store, &params.clone().with_state_db_only()).await,
                    expected,
                    "{params:?}"
                );
                assert_eq!(all_pages(&store, &params).await, expected, "{params:?}");
            }
        }
    }
}

/// A rollout the database does not know is found by the scan of rollouts, which repairs its row
/// so the database lists it from then on: Codex's `read_repair_rollout_path`. A row whose rollout
/// was moved without the database is pointed at where the scan finds it; one naming a rollout that
/// is still there is left alone, as Codex never repoints a row during a repair.
#[tokio::test]
async fn the_scan_of_rollouts_repairs_the_rows_it_finds() {
    let dir = temp_dir("repair");
    let (store, db) = indexed(&dir).await;
    Thread::new("behind-its-back", 1_000).write(&dir).await;
    let db_only = ListThreadsParams::new(10).with_state_db_only();
    assert!(
        store
            .list_threads(&db_only)
            .await
            .unwrap()
            .items()
            .is_empty()
    );

    assert_eq!(
        ids(store
            .list_threads(&ListThreadsParams::new(10))
            .await
            .unwrap()
            .items()),
        ["behind-its-back"]
    );
    assert_eq!(
        ids(store.list_threads(&db_only).await.unwrap().items()),
        ["behind-its-back"]
    );

    // Archived by a handle without the database: the row still says active.
    RolloutThreadDirectory::new(&dir)
        .archive_thread(&ArchiveThreadParams::new(SessionId::new("behind-its-back")))
        .await
        .unwrap();
    assert!(
        store
            .list_threads(&db_only)
            .await
            .unwrap()
            .items()
            .is_empty()
    );
    store
        .list_threads(&ListThreadsParams::new(10).archived())
        .await
        .unwrap();
    let row = db
        .get_thread(&SessionId::new("behind-its-back"))
        .await
        .unwrap()
        .unwrap();
    assert!(row.archived_at().is_some());
    assert_eq!(
        ids(store
            .list_threads(&db_only.clone().archived())
            .await
            .unwrap()
            .items()),
        ["behind-its-back"]
    );

    // Unarchived the same way: the scan of active rollouts sets the row active again.
    RolloutThreadDirectory::new(&dir)
        .unarchive_thread(&ArchiveThreadParams::new(SessionId::new("behind-its-back")))
        .await
        .unwrap();
    store
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    let row = db
        .get_thread(&SessionId::new("behind-its-back"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.archived_at(), None);
    assert_eq!(
        ids(store.list_threads(&db_only).await.unwrap().items()),
        ["behind-its-back"]
    );

    // A stray copy beside a row whose rollout is still there does not move the row.
    live_thread(&store, "kept", "hello")
        .await
        .shutdown()
        .await
        .unwrap();
    let kept = store.rollout_path(&SessionId::new("kept")).unwrap();
    std::fs::copy(&kept, dir.join("archived_sessions/rollout-kept.jsonl")).unwrap();
    store
        .list_threads(&ListThreadsParams::new(10).archived())
        .await
        .unwrap();
    let row = db
        .get_thread(&SessionId::new("kept"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.rollout_path(), kept);
    assert_eq!(row.archived_at(), None);
}

/// Codex's `list_threads_preserves_sqlite_title_search_results`: a search of the database
/// matches a thread's title and preview.
#[tokio::test]
async fn a_search_of_the_database_matches_titles_and_previews() {
    let dir = temp_dir("search");
    let (store, _db) = indexed(&dir).await;
    live_thread(&store, "by-preview", "find the needle")
        .await
        .shutdown()
        .await
        .unwrap();
    live_thread(&store, "by-title", "unrelated")
        .await
        .shutdown()
        .await
        .unwrap();
    live_thread(&store, "neither", "unrelated")
        .await
        .shutdown()
        .await
        .unwrap();
    // Named, so its title no longer holds the matching first message.
    for (id, name) in [("by-title", "needle work"), ("by-preview", "renamed")] {
        store
            .update_thread_metadata(&UpdateThreadMetadataParams::new(
                SessionId::new(id),
                ThreadMetadataPatch::new().with_name(Some(name.to_owned())),
            ))
            .await
            .unwrap();
    }

    let params = ListThreadsParams::new(10)
        .with_search_term("needle")
        .with_sort_direction(SortDirection::Asc);
    let mut found = ids(store
        .list_threads(&params.clone().with_state_db_only())
        .await
        .unwrap()
        .items())
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    found.sort();
    assert_eq!(found, ["by-preview", "by-title"]);
    let mut found = all_pages(&store, &params).await;
    found.sort();
    assert_eq!(found, ["by-preview", "by-title"]);
}

/// A search whose first database page is empty retains filesystem matches, as Codex's
/// metadata-filter fallback does for names written before the database was attached.
#[tokio::test]
async fn a_search_keeps_names_written_before_the_database_was_attached() {
    let dir = temp_dir("search_existing_name");
    Thread::new("named", 1_000).write(&dir).await;
    let plain = RolloutThreadDirectory::new(&dir);
    plain
        .update_thread_metadata(&UpdateThreadMetadataParams::new(
            SessionId::new("named"),
            ThreadMetadataPatch::new().with_name(Some("needle name".to_owned())),
        ))
        .await
        .unwrap();
    let (store, _db) = indexed(&dir).await;
    for direction in [SortDirection::Asc, SortDirection::Desc] {
        let query = ListThreadsParams::new(10)
            .with_search_term("needle")
            .with_sort_direction(direction);
        assert_eq!(
            ids(plain.list_threads(&query).await.unwrap().items()),
            ["named"]
        );
        let page = store.list_threads(&query).await.unwrap();
        assert_eq!(ids(page.items()), ["named"]);
        assert_eq!(page.items()[0].name(), Some("needle name"));
        assert!(
            store
                .list_threads(&query.with_state_db_only())
                .await
                .unwrap()
                .items()
                .is_empty()
        );
    }
}

/// A listing of the database alone lists nothing without a database, as Codex's does.
#[tokio::test]
async fn a_directory_without_a_database_lists_nothing_from_one() {
    let dir = temp_dir("no_db");
    Thread::new("unindexed", 1_000).write(&dir).await;
    let plain = RolloutThreadDirectory::new(&dir);
    assert!(
        plain
            .list_threads(&ListThreadsParams::new(10).with_state_db_only())
            .await
            .unwrap()
            .items()
            .is_empty()
    );
    assert_eq!(
        ids(plain
            .list_threads(&ListThreadsParams::new(10))
            .await
            .unwrap()
            .items()),
        ["unindexed"]
    );
}

/// Codex's `allocate_thread_updated_at`: threads written within one second keep distinct,
/// increasing update times, so a listing by update time orders them as they were written.
#[tokio::test]
async fn updates_within_a_second_keep_distinct_increasing_times() {
    let dir = temp_dir("allocate");
    let (store, db) = indexed(&dir).await;
    let at = EventTimestamp::now();
    for id in ["first", "second", "third"] {
        store
            .record_thread_metadata(&UpdateThreadMetadataParams::new(
                SessionId::new(id),
                ThreadMetadataPatch::new()
                    .with_preview(id)
                    .with_updated_at(at),
            ))
            .await
            .unwrap();
    }
    let mut times = Vec::new();
    for id in ["first", "second", "third"] {
        times.push(
            db.get_thread(&SessionId::new(id))
                .await
                .unwrap()
                .unwrap()
                .updated_at(),
        );
    }
    assert_eq!(times[0], at);
    assert!(times[0] < times[1] && times[1] < times[2], "{times:?}");
}

/// Codex's `init_restores_independent_thread_timestamp_maxima`: reopening seeds the allocator
/// from stored update times, while historical repairs still retain their original timestamps.
#[tokio::test]
async fn reopening_restores_the_update_time_high_water_mark() {
    let dir = temp_dir("allocate_reopened");
    let (store, db) = indexed(&dir).await;
    for id in ["first", "second"] {
        store
            .record_thread_metadata(&UpdateThreadMetadataParams::new(
                SessionId::new(id),
                ThreadMetadataPatch::new()
                    .with_preview(id)
                    .with_updated_at(EventTimestamp::from_millis(1_000)),
            ))
            .await
            .unwrap();
    }
    db_connection(&dir)
        .execute(
            "UPDATE threads SET updated_at_ms = 3000 WHERE id = 'first'",
            [],
        )
        .unwrap();
    drop(store);
    drop(db);

    let (store, db) = indexed(&dir).await;
    for (id, candidate, expected) in [("latest", 3_000, 3_001), ("historical", 1_000, 1_000)] {
        store
            .record_thread_metadata(&UpdateThreadMetadataParams::new(
                SessionId::new(id),
                ThreadMetadataPatch::new()
                    .with_preview(id)
                    .with_updated_at(EventTimestamp::from_millis(candidate)),
            ))
            .await
            .unwrap();
        assert_eq!(
            db.get_thread(&SessionId::new(id))
                .await
                .unwrap()
                .unwrap()
                .updated_at(),
            EventTimestamp::from_millis(expected),
            "{id}"
        );
    }
}

/// Two handles on one database — two processes, as far as `SQLite` is concerned — and many
/// statements at once on each: the pool shares its connections and nothing is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_and_readers_share_the_database() {
    let dir = temp_dir("concurrent");
    let (store, _db) = indexed(&dir).await;
    let other =
        RolloutThreadDirectory::new(&dir).with_state_db(StateRuntime::init(&dir).await.unwrap());
    let mut tasks = Vec::new();
    for index in 0..24 {
        let store = if index % 2 == 0 {
            store.clone()
        } else {
            other.clone()
        };
        tasks.push(tokio::spawn(async move {
            let id = format!("c-{index:02}");
            live_thread(&store, &id, "parallel")
                .await
                .shutdown()
                .await
                .unwrap();
            store
                .list_threads(&ListThreadsParams::new(50).with_state_db_only())
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let listed = all_pages(&store, &ListThreadsParams::new(7).with_state_db_only()).await;
    assert_eq!(listed.len(), 24);
}

/// Disjoint patches of one thread preserve unspecified fields, both through one runtime and
/// through independently opened handles of the same database.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_patches_of_one_thread_preserve_unset_fields() {
    for separate_runtime in [false, true] {
        let dir = temp_dir(&format!("concurrent_patches_{separate_runtime}"));
        let (store, db) = indexed(&dir).await;
        live_thread(&store, "shared", "hello")
            .await
            .shutdown()
            .await
            .unwrap();
        let other_db = if separate_runtime {
            StateRuntime::init(&dir).await.unwrap()
        } else {
            Arc::clone(&db)
        };
        let other = RolloutThreadDirectory::new(&dir).with_state_db(Arc::clone(&other_db));

        // Open pooled connections before taking the external write lock. Without an atomic
        // read/merge/write, WAL lets both patches read the old row before their writes wait.
        let mut warming = Vec::new();
        for runtime in [&db, &other_db] {
            for _ in 0..30 {
                let runtime = Arc::clone(runtime);
                warming.push(tokio::spawn(async move {
                    runtime.get_thread(&SessionId::new("shared")).await.unwrap()
                }));
            }
        }
        for task in warming {
            task.await.unwrap();
        }
        let connection = db_connection(&dir);
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        let model = tokio::spawn(async move {
            store
                .record_thread_metadata(&UpdateThreadMetadataParams::new(
                    SessionId::new("shared"),
                    ThreadMetadataPatch::new().with_model("new-model"),
                ))
                .await
                .unwrap();
        });
        let cwd = tokio::spawn(async move {
            other
                .record_thread_metadata(&UpdateThreadMetadataParams::new(
                    SessionId::new("shared"),
                    ThreadMetadataPatch::new().with_cwd("/new-cwd"),
                ))
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pending = !model.is_finished() && !cwd.is_finished();
        connection.execute_batch("COMMIT").unwrap();
        model.await.unwrap();
        cwd.await.unwrap();
        assert!(pending, "both patches must contend with the write lock");

        let row = db
            .get_thread(&SessionId::new("shared"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.model(), row.cwd()),
            (Some("new-model"), Some("/new-cwd")),
            "separate_runtime: {separate_runtime}, row: {row:?}"
        );
    }
}
