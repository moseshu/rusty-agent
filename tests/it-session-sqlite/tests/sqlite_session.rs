//! `SqliteSession`, ported with the reference's `SQLiteSession` tests
//! (`tests/memory/test_session.py`): the `Session` contract, corrupt rows, limits, sessions that
//! share a file, and failed writes that must not keep the database's write lock.

#[path = "support/session_backend.rs"]
mod session_backend;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_core::{
    error::{Error, SessionErrorKind},
    session::{SessionId, SessionSettings},
};
use ra_session::{Session, SqliteSession};
use rusqlite::Connection;
use session_backend::{
    MALFORMED_ITEM_JSON, assert_limit_reads, assert_session_contract, assert_text_round_trip,
    sample_items, texts, user,
};

fn temp_test_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("sqlite_session")
        .join(test_name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp test directory");
    dir
}

/// A connection of the test's own, outside every session, configured like the reference's
/// Python connections: no busy wait, and foreign keys off.
fn probe(db_path: &Path) -> Connection {
    let connection = Connection::open(db_path).unwrap();
    connection.busy_timeout(std::time::Duration::ZERO).unwrap();
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    connection
}

fn insert_raw_row(db_path: &Path, session_id: &str, data: &str) {
    probe(db_path)
        .execute(
            "INSERT INTO agent_messages (session_id, message_data) VALUES (?1, ?2)",
            (session_id, data),
        )
        .unwrap();
}

fn count(db_path: &Path, sql: &str) -> i64 {
    probe(db_path).query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Whether another connection can take the write lock without waiting.
fn write_lock_is_free(db_path: &Path) -> bool {
    let connection = probe(db_path);
    let free = connection.execute_batch("BEGIN IMMEDIATE").is_ok();
    if free {
        connection.execute_batch("ROLLBACK").unwrap();
    }
    free
}

fn is_kind(error: &Error, expected: SessionErrorKind) -> bool {
    matches!(error, Error::Session { kind, .. } if *kind == expected)
}

#[tokio::test]
async fn test_sqlite_session_honours_the_session_contract_in_a_file() {
    let dir = temp_test_dir("contract_file");
    let session = SqliteSession::open(SessionId::generate(), dir.join("contract.db")).unwrap();
    assert_session_contract(&session).await;
}

#[tokio::test]
async fn test_sqlite_session_honours_the_session_contract_in_memory() {
    let session = SqliteSession::open_in_memory(SessionId::generate()).unwrap();
    assert_eq!(session.db_path(), None);
    assert_session_contract(&session).await;
}

#[tokio::test]
async fn test_sqlite_session_memory_direct() {
    let dir = temp_test_dir("direct");
    let session = SqliteSession::open("direct_test", dir.join("test_direct.db")).unwrap();

    session
        .add_items(vec![user("u", "Hello"), user("a", "Hi there!")])
        .await
        .unwrap();
    assert_eq!(
        texts(&session.get_items(None).await.unwrap()),
        ["Hello", "Hi there!"]
    );

    session.clear().await.unwrap();
    assert!(session.get_items(None).await.unwrap().is_empty());
    session.close().unwrap();
}

#[tokio::test]
async fn test_sqlite_session_persists_across_sessions_and_in_memory_databases_do_not() {
    let dir = temp_test_dir("persist");
    let db_path = dir.join("persist.db");
    let items = sample_items();

    let session = SqliteSession::open("persist", &db_path).unwrap();
    session.add_items(items.clone()).await.unwrap();
    drop(session);
    let reopened = SqliteSession::open("persist", &db_path).unwrap();
    assert_eq!(reopened.get_items(None).await.unwrap(), items);

    // The schema is the reference's: two tables, an autoincrement id, JSON per row.
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM agent_sessions"), 1);
    assert_eq!(
        count(&db_path, "SELECT COUNT(*) FROM agent_messages"),
        i64::try_from(items.len()).unwrap()
    );
    let journal_mode: String = probe(&db_path)
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");

    let first = SqliteSession::open_in_memory("same").unwrap();
    let second = SqliteSession::open_in_memory("same").unwrap();
    first.add_items(vec![user("u", "only here")]).await.unwrap();
    assert!(second.get_items(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_sqlite_session_closed_rejects_every_operation() {
    let dir = temp_test_dir("closed");
    let session = SqliteSession::open("closed_test", dir.join("closed.db")).unwrap();
    session.close().unwrap();
    assert!(session.is_closed());

    // `add_items([])` must not bypass the closed check through the empty-list fast path.
    let error = session.add_items(Vec::new()).await.unwrap_err();
    assert!(matches!(error, Error::Caller { .. }), "{error}");
    assert!(error.to_string().contains("SqliteSession is closed"));
    assert!(session.get_items(None).await.is_err());
    assert!(session.pop_item().await.is_err());
    assert!(session.clear().await.is_err());
    session.close().unwrap();
}

#[tokio::test]
async fn test_sqlite_session_memory_pop_item() {
    let dir = temp_test_dir("pop");
    let session = SqliteSession::open("pop_test", dir.join("test_pop.db")).unwrap();

    assert_eq!(session.pop_item().await.unwrap(), None);
    let items = vec![
        user("1", "Hello"),
        user("2", "Hi there!"),
        user("3", "How are you?"),
    ];
    session.add_items(items.clone()).await.unwrap();

    assert_eq!(session.pop_item().await.unwrap(), Some(items[2].clone()));
    assert_eq!(
        texts(&session.get_items(None).await.unwrap()),
        ["Hello", "Hi there!"]
    );
    assert_eq!(session.pop_item().await.unwrap(), Some(items[1].clone()));
    assert_eq!(session.pop_item().await.unwrap(), Some(items[0].clone()));
    assert_eq!(session.pop_item().await.unwrap(), None);
    assert!(session.get_items(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_sqlite_session_pop_and_clear_affect_only_their_session() {
    let dir = temp_test_dir("pop_sessions");
    let db_path = dir.join("test_pop_sessions.db");
    let session_1 = SqliteSession::open("session_1", &db_path).unwrap();
    let session_2 = SqliteSession::open("session_2", &db_path).unwrap();

    session_1
        .add_items(vec![user("1", "Session 1 message")])
        .await
        .unwrap();
    session_2
        .add_items(vec![
            user("1", "Session 2 message 1"),
            user("2", "Session 2 message 2"),
        ])
        .await
        .unwrap();

    let popped = session_2.pop_item().await.unwrap().unwrap();
    assert_eq!(texts(&[popped]), ["Session 2 message 2"]);
    assert_eq!(
        texts(&session_1.get_items(None).await.unwrap()),
        ["Session 1 message"]
    );
    assert_eq!(
        texts(&session_2.get_items(None).await.unwrap()),
        ["Session 2 message 1"]
    );

    session_2.clear().await.unwrap();
    assert_eq!(session_1.get_items(None).await.unwrap().len(), 1);
    assert_eq!(
        count(
            &db_path,
            "SELECT COUNT(*) FROM agent_sessions WHERE session_id = 'session_2'"
        ),
        0
    );
}

#[tokio::test]
async fn test_sqlite_session_pop_item_skips_corrupt_most_recent() {
    let dir = temp_test_dir("pop_corrupt");
    let db_path = dir.join("test_pop_corrupt.db");
    let session = SqliteSession::open("pop_corrupt", &db_path).unwrap();
    let valid = user("v", "valid");
    session.add_items(vec![valid.clone()]).await.unwrap();
    insert_raw_row(&db_path, "pop_corrupt", "not valid json {{{");

    assert_eq!(session.pop_item().await.unwrap(), Some(valid));
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM agent_messages"), 0);
}

#[tokio::test]
async fn test_sqlite_session_pop_item_returns_none_after_dropping_only_corrupt_rows() {
    let dir = temp_test_dir("pop_only_corrupt");
    let db_path = dir.join("test_pop_only_corrupt.db");
    let session = SqliteSession::open("pop_only_corrupt", &db_path).unwrap();
    insert_raw_row(&db_path, "pop_only_corrupt", "not valid json {{{");

    assert_eq!(session.pop_item().await.unwrap(), None);
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM agent_messages"), 0);
}

#[tokio::test]
async fn test_sqlite_session_skips_malformed_json_that_has_a_typed_data_error() {
    let dir = temp_test_dir("malformed_typed_data");
    let db_path = dir.join("malformed.db");
    let session = SqliteSession::open("malformed", &db_path).unwrap();
    let valid = user("valid", "kept");
    session.add_items(vec![valid.clone()]).await.unwrap();
    for malformed in MALFORMED_ITEM_JSON {
        assert!(serde_json::from_str::<serde_json::Value>(malformed).is_err());
        insert_raw_row(&db_path, "malformed", malformed);
    }

    assert_eq!(session.get_items(None).await.unwrap(), vec![valid.clone()]);
    assert_eq!(
        session.get_items(Some(1)).await.unwrap(),
        vec![valid.clone()]
    );
    assert_eq!(session.pop_item().await.unwrap(), Some(valid));
    assert_eq!(session.pop_item().await.unwrap(), None);
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM agent_messages"), 0);
    assert!(write_lock_is_free(&db_path));
}

#[tokio::test]
async fn test_sqlite_session_get_items_with_limit() {
    let dir = temp_test_dir("limit");
    let session = SqliteSession::open("count_test", dir.join("test_count.db")).unwrap();
    assert_limit_reads(&session).await;
}

#[tokio::test]
async fn test_sqlite_session_get_items_limit_skips_corrupt_newest_rows() {
    let dir = temp_test_dir("limit_corrupt");
    let db_path = dir.join("test_limit_corrupt.db");
    let session = SqliteSession::open("limit_corrupt", &db_path).unwrap();
    session
        .add_items(vec![
            user("0", "valid 0"),
            user("1", "valid 1"),
            user("2", "valid 2"),
        ])
        .await
        .unwrap();
    // More corrupt rows than the first window holds, so the window has to grow twice.
    for _ in 0..5 {
        insert_raw_row(&db_path, "limit_corrupt", "not valid json {{{");
    }

    assert_eq!(
        texts(&session.get_items(Some(2)).await.unwrap()),
        ["valid 1", "valid 2"]
    );
    assert_eq!(
        texts(&session.get_items(None).await.unwrap()),
        ["valid 0", "valid 1", "valid 2"]
    );
    assert_eq!(session.get_items(Some(5)).await.unwrap().len(), 3);
}

#[tokio::test]
async fn test_sqlite_session_get_items_session_settings_limit_skips_corrupt_rows() {
    let dir = temp_test_dir("settings_limit");
    let db_path = dir.join("test_settings_limit_corrupt.db");
    let session = SqliteSession::builder("settings_limit_corrupt")
        .db_path(&db_path)
        .session_settings(SessionSettings::new().with_limit(2))
        .open()
        .unwrap();
    session
        .add_items(vec![
            user("0", "valid 0"),
            user("1", "valid 1"),
            user("2", "valid 2"),
        ])
        .await
        .unwrap();
    insert_raw_row(&db_path, "settings_limit_corrupt", "not valid json {{{");

    assert_eq!(
        texts(&session.get_items(None).await.unwrap()),
        ["valid 1", "valid 2"]
    );
    assert_eq!(session.get_items(Some(3)).await.unwrap().len(), 3);
}

#[tokio::test]
async fn test_sqlite_session_keeps_json_it_cannot_read_as_an_item() {
    let dir = temp_test_dir("unreadable_item");
    let db_path = dir.join("newer.db");
    let session = SqliteSession::open("newer", &db_path).unwrap();
    session.add_items(vec![user("v", "valid")]).await.unwrap();
    // What an item of a kind this build does not know looks like from here.
    insert_raw_row(
        &db_path,
        "newer",
        r#"{"schema_version":1,"id":"n","kind":{"type":"from_the_future"}}"#,
    );
    insert_raw_row(&db_path, "newer", "not valid json {{{");

    let error = session.get_items(None).await.unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::Corrupted), "{error}");

    // Pop drops the corrupt row above it, then refuses to remove the item it cannot read.
    let error = session.pop_item().await.unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::Corrupted), "{error}");
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM agent_messages"), 2);
    assert!(session.pop_item().await.is_err());
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM agent_messages"), 2);
    assert!(write_lock_is_free(&db_path));
}

#[tokio::test]
async fn test_sqlite_session_unicode_and_special_characters() {
    let dir = temp_test_dir("text");
    let session = SqliteSession::open("text_test", dir.join("test_text.db")).unwrap();
    assert_text_round_trip(&session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sqlite_session_concurrent_access() {
    let dir = temp_test_dir("concurrent");
    let session: Arc<dyn Session> =
        Arc::new(SqliteSession::open("concurrent_test", dir.join("test_concurrent.db")).unwrap());

    let handles: Vec<_> = (0..10)
        .map(|i| {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                session
                    .add_items(vec![user(&format!("m{i}"), &format!("Message {i}"))])
                    .await
                    .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.await.unwrap();
    }

    let mut contents = texts(&session.get_items(None).await.unwrap());
    contents.sort();
    let mut expected: Vec<_> = (0..10).map(|i| format!("Message {i}")).collect();
    expected.sort();
    assert_eq!(contents, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sqlite_session_sessions_sharing_a_file_do_not_contend() {
    let dir = temp_test_dir("shared_file");
    let db_path = dir.join("test_shared_lock.db");
    let sessions: Vec<Arc<SqliteSession>> = (0..4)
        .map(|i| Arc::new(SqliteSession::open(format!("session_{i}"), &db_path).unwrap()))
        .collect();

    // Every session writes at once through its own connection. The shared process-local lock
    // serializes them, so none waits out the busy timeout or fails with "database is locked".
    let handles: Vec<_> = sessions
        .iter()
        .enumerate()
        .flat_map(|(i, session)| {
            (0..25).map(move |n| {
                let session = Arc::clone(session);
                tokio::spawn(async move {
                    session
                        .add_items(vec![user(&format!("{n}"), &format!("session_{i} {n}"))])
                        .await
                        .unwrap();
                    session.get_items(Some(1)).await.unwrap();
                })
            })
        })
        .collect();
    for handle in handles {
        handle.await.unwrap();
    }

    for (i, session) in sessions.iter().enumerate() {
        let items = texts(&session.get_items(None).await.unwrap());
        assert_eq!(items.len(), 25);
        assert!(
            items
                .iter()
                .all(|t| t.starts_with(&format!("session_{i} ")))
        );
    }

    // Closing one session leaves the others working on the same file.
    sessions[0].close().unwrap();
    sessions[1]
        .add_items(vec![user("late", "late")])
        .await
        .unwrap();
    assert_eq!(sessions[1].get_items(None).await.unwrap().len(), 26);
}

#[tokio::test]
async fn test_sqlite_session_failed_add_items_releases_write_lock() {
    let dir = temp_test_dir("add_rollback");
    let db_path = dir.join("test_rollback.db");
    let session = SqliteSession::open("rollback_test", &db_path).unwrap();
    // The sessions-table upsert succeeds and opens the write transaction; the item insert fails.
    probe(&db_path)
        .execute_batch("DROP TABLE agent_messages")
        .unwrap();

    let error = session
        .add_items(vec![user("u", "lost")])
        .await
        .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::Io), "{error}");

    assert!(write_lock_is_free(&db_path));
    assert_eq!(
        count(
            &db_path,
            "SELECT COUNT(*) FROM agent_sessions WHERE session_id = 'rollback_test'"
        ),
        0
    );
}

#[tokio::test]
async fn test_sqlite_session_failed_clear_rolls_back() {
    let dir = temp_test_dir("clear_rollback");
    let db_path = dir.join("clear_rollback.db");
    let session = SqliteSession::open("clear_rollback", &db_path).unwrap();
    let kept = user("k", "kept");
    session.add_items(vec![kept.clone()]).await.unwrap();
    // Deleting the items succeeds; deleting the session row then fails.
    probe(&db_path)
        .execute_batch("DROP TABLE agent_sessions")
        .unwrap();

    assert!(session.clear().await.is_err());
    assert!(write_lock_is_free(&db_path));
    // The earlier deletion was rolled back with it.
    assert_eq!(session.get_items(None).await.unwrap(), vec![kept]);
}

#[tokio::test]
async fn test_sqlite_session_failed_pop_item_releases_write_lock() {
    let dir = temp_test_dir("pop_rollback");
    let db_path = dir.join("pop_rollback.db");
    let session = SqliteSession::open("pop_rollback", &db_path).unwrap();
    session.add_items(vec![user("k", "kept")]).await.unwrap();
    probe(&db_path)
        .execute_batch("DROP TABLE agent_messages")
        .unwrap();

    assert!(session.pop_item().await.is_err());
    assert!(write_lock_is_free(&db_path));
}

#[tokio::test]
async fn test_sqlite_session_custom_table_names_are_single_quoted_identifiers() {
    let dir = temp_test_dir("custom_tables");
    let db_path = dir.join("custom.db");
    let session = SqliteSession::builder("custom")
        .db_path(&db_path)
        .sessions_table("my sessions")
        .messages_table("my \"messages\"; DROP TABLE x")
        .open()
        .unwrap();
    assert_eq!(session.sessions_table(), "my sessions");
    assert_eq!(session.messages_table(), "my \"messages\"; DROP TABLE x");
    assert_eq!(session.db_path(), Some(db_path.as_path()));

    assert_session_contract(&session).await;
    session.add_items(sample_items()).await.unwrap();
    assert_eq!(
        count(
            &db_path,
            "SELECT COUNT(*) FROM \"my \"\"messages\"\"; DROP TABLE x\""
        ),
        i64::try_from(sample_items().len()).unwrap()
    );
    assert_eq!(count(&db_path, "SELECT COUNT(*) FROM \"my sessions\""), 1);
    let default_tables = count(
        &db_path,
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('agent_sessions', 'agent_messages')",
    );
    assert_eq!(default_tables, 0);
}
