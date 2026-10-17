// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's read-state storage tests: stale edits, live delivery dedupe and metadata isolation.

use ra_core::session::SessionId;
use ra_session::{
    CreateThreadParams, DeleteThreadParams, RolloutSessionMeta, RolloutThreadDirectory,
    ThreadStore,
    store::local::{ReadStateOperation, ReadStateUpdate, StateRuntime, ThreadReadState},
};
use rusqlite::{Connection, params};
use std::{path::PathBuf, sync::Arc};

fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir()
        .join("rusty_agent_read_states")
        .join(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn seed(db: &StateRuntime, id: &SessionId) {
    Connection::open(db.path()).unwrap().execute(
        "INSERT INTO threads(id,rollout_path,created_at_ms,updated_at_ms) VALUES(?1,'rollout',1,1)",
        [id.as_str()],
    ).unwrap();
}

async fn snapshot(db: &StateRuntime, id: &SessionId) -> ThreadReadState {
    db.thread_read_states(std::slice::from_ref(id))
        .await
        .unwrap()
        .remove(id)
        .unwrap()
}

async fn mark(
    db: &StateRuntime,
    id: &SessionId,
    state: &ThreadReadState,
    operation: ReadStateOperation,
) -> ThreadReadState {
    let ReadStateUpdate::Applied(state) = db
        .update_thread_read_state(id, state.revision(), operation)
        .await
        .unwrap()
    else {
        panic!("current mark must apply")
    };
    state
}

#[tokio::test]
async fn a_stale_read_cannot_erase_a_later_completion_or_another_mark() {
    let home = home("stale_marks");
    let db = StateRuntime::init(&home).await.unwrap();
    let id = SessionId::new("opaque-thread");
    let other = SessionId::new("another-thread");
    seed(&db, &id);
    seed(&db, &other);
    let baseline = snapshot(&db, &id).await;
    assert_eq!(baseline.first_unread_turn(), None);
    let count: i64 = Connection::open(db.path())
        .unwrap()
        .query_row("SELECT count(*) FROM thread_read_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0, "reads must not materialize receipts");
    assert!(matches!(
        db.update_thread_read_state(&other, baseline.revision(), ReadStateOperation::Unread)
            .await
            .unwrap(),
        ReadStateUpdate::Conflict(_)
    ));
    let marked = mark(&db, &id, &baseline, ReadStateOperation::Unread).await;
    assert_eq!(marked.first_unread_turn(), Some(""));
    assert_eq!(
        db.update_thread_read_state(&id, baseline.revision(), ReadStateOperation::Read)
            .await
            .unwrap(),
        ReadStateUpdate::Conflict(marked.clone())
    );
    let marked_again = mark(&db, &id, &marked, ReadStateOperation::Unread).await;
    assert_ne!(marked_again.revision(), marked.revision());
    assert_eq!(
        db.update_thread_read_state(&id, marked.revision(), ReadStateOperation::Read)
            .await
            .unwrap(),
        ReadStateUpdate::Conflict(marked_again.clone())
    );
    let read = mark(&db, &id, &marked_again, ReadStateOperation::Read).await;
    assert_eq!(read.first_unread_turn(), None);
    assert!(db.publish_thread_attention(&id, "turn-1").await.unwrap());
    let first = snapshot(&db, &id).await;
    assert_eq!(first.first_unread_turn(), Some("turn-1"));
    assert!(!db.publish_thread_attention(&id, "turn-1").await.unwrap());
    assert_eq!(snapshot(&db, &id).await, first);
    assert!(db.publish_thread_attention(&id, "turn-2").await.unwrap());
    let second = snapshot(&db, &id).await;
    assert_eq!(second.first_unread_turn(), Some("turn-1"));
    assert_ne!(first.revision(), second.revision());
    for operation in [ReadStateOperation::Read, ReadStateOperation::Unread] {
        assert_eq!(
            db.update_thread_read_state(&id, first.revision(), operation)
                .await
                .unwrap(),
            ReadStateUpdate::Conflict(second.clone())
        );
    }
    let read = mark(&db, &id, &second, ReadStateOperation::Read).await;
    assert!(!db.publish_thread_attention(&id, "turn-2").await.unwrap());
    assert_eq!(snapshot(&db, &id).await, read);
}

#[tokio::test]
async fn position_and_dedupe_survive_metadata_rebuild_restart_and_revert() {
    let home = home("metadata_isolation");
    let db = StateRuntime::init(&home).await.unwrap();
    let id = SessionId::new("opaque-thread");
    seed(&db, &id);
    let metadata = db.get_thread(&id).await.unwrap();
    assert!(
        db.publish_thread_attention(&id, "removed-turn")
            .await
            .unwrap()
    );
    let before = snapshot(&db, &id).await;
    // As in Codex's storage test, the caller retires a removed position at its observed revision.
    let removed = mark(&db, &id, &before, ReadStateOperation::Read).await;
    assert_eq!(removed.first_unread_turn(), None);
    assert_eq!(db.get_thread(&id).await.unwrap(), metadata);
    Connection::open(db.path())
        .unwrap()
        .execute("DELETE FROM threads WHERE id=?1", [id.as_str()])
        .unwrap();
    assert!(
        db.thread_read_states(std::slice::from_ref(&id))
            .await
            .unwrap()
            .is_empty()
    );
    seed(&db, &id);
    drop(db);
    let db = StateRuntime::init(&home).await.unwrap();
    assert_eq!(snapshot(&db, &id).await, removed);
    assert!(
        !db.publish_thread_attention(&id, "removed-turn")
            .await
            .unwrap()
    );
    assert_eq!(snapshot(&db, &id).await, removed);
    assert!(
        db.publish_thread_attention(&id, "new-live-turn")
            .await
            .unwrap()
    );
    assert_eq!(
        snapshot(&db, &id).await.first_unread_turn(),
        Some("new-live-turn")
    );
    // Real deletion, unlike index rebuilding, also removes the durable receipt.
    Connection::open(db.path())
        .unwrap()
        .execute("DELETE FROM threads WHERE id=?1", [id.as_str()])
        .unwrap();
    let store = RolloutThreadDirectory::new(&home).with_state_db(Arc::clone(&db));
    let recorder = store
        .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
            id.clone(),
        )))
        .await
        .unwrap();
    recorder
        .persist(ra_core::session::rollout::PersistContext::Standard)
        .await
        .unwrap();
    recorder.shutdown().await.unwrap();
    store
        .delete_thread(&DeleteThreadParams::new(id.clone()))
        .await
        .unwrap();
    assert!(!db.publish_thread_attention(&id, "late").await.unwrap());
    assert_eq!(
        db.update_thread_read_state(&id, removed.revision(), ReadStateOperation::Unread)
            .await
            .unwrap(),
        ReadStateUpdate::Unavailable
    );
    let count: i64 = Connection::open(db.path())
        .unwrap()
        .query_row("SELECT count(*) FROM thread_read_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn snapshots_chunk_large_pages_and_racing_marks_have_one_winner() {
    let home = home("batch_and_races");
    let db = StateRuntime::init(&home).await.unwrap();
    let mut connection = Connection::open(db.path()).unwrap();
    let tx = connection.transaction().unwrap();
    let ids: Vec<_> = (0..1100)
        .map(|index| SessionId::new(format!("thread-{index}")))
        .collect();
    for id in &ids {
        tx.execute("INSERT INTO threads(id,rollout_path,created_at_ms,updated_at_ms) VALUES(?1,'rollout',1,1)", params![id.as_str()]).unwrap();
    }
    tx.commit().unwrap();
    assert!(db.thread_read_states(&[]).await.unwrap().is_empty());
    assert_eq!(db.thread_read_states(&ids).await.unwrap().len(), ids.len());
    let second_handle = StateRuntime::init(&home).await.unwrap();
    let baseline = snapshot(&db, &ids[0]).await;
    let (left, right) = tokio::join!(
        db.update_thread_read_state(&ids[0], baseline.revision(), ReadStateOperation::Read),
        second_handle.update_thread_read_state(
            &ids[0],
            baseline.revision(),
            ReadStateOperation::Unread
        ),
    );
    match (left.unwrap(), right.unwrap()) {
        (ReadStateUpdate::Applied(winner), ReadStateUpdate::Conflict(current))
        | (ReadStateUpdate::Conflict(current), ReadStateUpdate::Applied(winner)) => {
            assert_eq!(winner, current)
        }
        outcomes => panic!("exactly one mark must win: {outcomes:?}"),
    }
}

#[tokio::test]
async fn receipt_migration_preserves_previous_metadata_and_reads_stay_lazy() {
    let home = home("receipt_migration");
    let db = StateRuntime::init(&home).await.unwrap();
    let id = SessionId::new("previous-thread");
    seed(&db, &id);
    let path = db.path().to_owned();
    drop(db);
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP TABLE thread_read_receipts;
         ALTER TABLE threads DROP COLUMN source;
         ALTER TABLE threads DROP COLUMN thread_source;
         PRAGMA user_version = 2;",
        )
        .unwrap();
    let db = StateRuntime::init(&home).await.unwrap();
    assert!(db.get_thread(&id).await.unwrap().is_some());
    let state = snapshot(&db, &id).await;
    assert_eq!(state.first_unread_turn(), None);
    assert_eq!(state.revision(), "initial:previous-thread");
    let sql = Connection::open(db.path()).unwrap();
    assert_eq!(
        sql.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM thread_read_receipts", [], |row| row
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
}
