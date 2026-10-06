//! Listing, naming, archiving and deleting threads, as Codex's local store does without its state
//! database (`rollout/src/tests.rs`, `rollout/src/session_index_tests.rs`,
//! `thread-store/src/local/{archive_thread,unarchive_thread,delete_thread}.rs`), and as its
//! in-memory store does (`thread-store/src/in_memory.rs`).

use std::{path::PathBuf, time::SystemTime};

use ra_core::{
    agent::control::AgentPath,
    error::{Error, SessionErrorKind},
    event::EventTimestamp,
    item::{AgentId, ContentBlock, ImageBlock, ImageSource, Message, MessageRole, ModelInputItem},
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutRunStarted, RolloutThreadSpawn, RolloutThreadStore,
            RolloutTurnContext,
        },
    },
    state::RunId,
};
use ra_session::{
    ArchiveThreadParams, ArchiveThreadsParams, CreateThreadParams, DeleteThreadParams,
    DeleteThreadsParams, InMemoryThreadStore, ListThreadsParams, LoadThreadHistoryParams,
    ReadThreadParams, ResumeThreadParams, RolloutSessionMeta, RolloutThreadDirectory,
    RolloutWriter, SortDirection, StoredThread, ThreadMetadataPatch, ThreadSortKey, ThreadStore,
    UpdateThreadMetadataParams,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn directory(name: &str) -> RolloutThreadDirectory {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("thread_listing")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    RolloutThreadDirectory::new(dir)
}

/// A thread as a session writes it.
struct Thread<'a> {
    id: &'a str,
    created_at: u64,
    user: Option<&'a str>,
    provider: Option<&'a str>,
    cwd: Option<&'a str>,
    records_before_user: usize,
}

impl<'a> Thread<'a> {
    const fn new(id: &'a str, created_at: u64) -> Self {
        Self {
            id,
            created_at,
            user: Some("hello"),
            provider: Some("openai"),
            cwd: Some("/work"),
            records_before_user: 0,
        }
    }

    async fn write(self, directory: &RolloutThreadDirectory) -> PathBuf {
        let session_id = SessionId::new(self.id);
        let path = directory.rollout_path(&session_id).unwrap();
        let mut meta = RolloutSessionMeta::new(session_id.clone())
            .with_created_at(EventTimestamp::from_millis(self.created_at));
        if let Some(provider) = self.provider {
            meta = meta.with_model_provider(provider);
        }
        if let Some(cwd) = self.cwd {
            meta = meta.with_cwd(cwd);
        }
        let mut writer = RolloutWriter::open(&path, session_id).await.unwrap();
        writer.append_session_meta(meta).await.unwrap();
        for turn in 0..self.records_before_user {
            writer
                .append_turn_context(RolloutTurnContext::new(
                    RunId::new("run-0"),
                    u32::try_from(turn).unwrap(),
                ))
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

fn set_modified(path: &PathBuf, millis: u64) {
    let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(millis);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

fn ids(threads: &[StoredThread]) -> Vec<&str> {
    threads
        .iter()
        .map(|thread| thread.session_id().as_str())
        .collect()
}

fn is_kind(error: &Error, kind: SessionErrorKind) -> bool {
    matches!(error, Error::Session { kind: found, .. } if *found == kind)
}

fn failure<T>(result: ra_core::error::Result<T>) -> Error {
    match result {
        Ok(_) => panic!("expected the call to fail"),
        Err(error) => error,
    }
}

async fn rename(directory: &RolloutThreadDirectory, id: &str, name: Option<&str>) {
    directory
        .update_thread_metadata(&UpdateThreadMetadataParams::new(
            SessionId::new(id),
            ThreadMetadataPatch::new().with_name(name.map(str::to_owned)),
        ))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------------

/// Codex's `test_list_conversations_latest_first` and `test_pagination_cursor`.
#[tokio::test]
async fn threads_list_newest_first_and_page_with_the_cursor() {
    let directory = directory("latest_first");
    for (id, created_at) in [("a", 1_000), ("b", 2_000), ("c", 3_000), ("d", 4_000)] {
        Thread::new(id, created_at).write(&directory).await;
    }

    let first = directory
        .list_threads(&ListThreadsParams::new(3))
        .await
        .unwrap();
    assert_eq!(ids(first.items()), ["d", "c", "b"]);
    let thread = &first.items()[0];
    assert_eq!(thread.preview(), "hello");
    assert_eq!(thread.first_user_message(), Some("hello"));
    assert_eq!(
        thread.created_at(),
        Some(EventTimestamp::from_millis(4_000))
    );
    assert_eq!(thread.model_provider(), Some("openai"));
    assert_eq!(
        thread.rollout_path(),
        Some(
            directory
                .rollout_path(&SessionId::new("d"))
                .unwrap()
                .as_path()
        )
    );
    assert!(thread.updated_at().is_some());
    assert_eq!(thread.archived_at(), None);

    let second = directory
        .list_threads(&ListThreadsParams::new(3).with_cursor(first.next_cursor().unwrap()))
        .await
        .unwrap();
    assert_eq!(ids(second.items()), ["a"]);
    assert_eq!(second.next_cursor(), None);

    let oldest_first = directory
        .list_threads(&ListThreadsParams::new(2).with_sort_direction(SortDirection::Asc))
        .await
        .unwrap();
    assert_eq!(ids(oldest_first.items()), ["a", "b"]);
    let rest = directory
        .list_threads(
            &ListThreadsParams::new(2)
                .with_sort_direction(SortDirection::Asc)
                .with_cursor(oldest_first.next_cursor().unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(ids(rest.items()), ["c", "d"]);
    assert_eq!(rest.next_cursor(), None);
}

/// Codex's `test_timestamp_only_cursor_skips_same_second_filesystem_ties` shows its cursor
/// skipping threads that share the last one's time; the cursor here holds the id as well.
#[tokio::test]
async fn threads_of_the_same_time_are_ordered_by_id_and_none_is_skipped() {
    let directory = directory("ties");
    for id in ["t1", "t2", "t3"] {
        Thread::new(id, 5_000).write(&directory).await;
    }
    let first = directory
        .list_threads(&ListThreadsParams::new(2))
        .await
        .unwrap();
    assert_eq!(ids(first.items()), ["t3", "t2"]);
    let second = directory
        .list_threads(&ListThreadsParams::new(2).with_cursor(first.next_cursor().unwrap()))
        .await
        .unwrap();
    assert_eq!(ids(second.items()), ["t1"]);
}

/// Codex's `test_updated_at_uses_file_mtime` and `test_created_at_sort_uses_file_mtime_for_updated_at`.
#[tokio::test]
async fn the_update_time_is_the_files_and_sorts_threads_by_last_write() {
    let directory = directory("updated_at");
    let old = Thread::new("old", 1_000).write(&directory).await;
    let new = Thread::new("new", 2_000).write(&directory).await;
    set_modified(&old, 9_000);
    set_modified(&new, 8_000);

    let by_created = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    assert_eq!(ids(by_created.items()), ["new", "old"]);
    assert_eq!(
        by_created.items()[0].updated_at(),
        Some(EventTimestamp::from_millis(8_000))
    );

    let by_updated = directory
        .list_threads(&ListThreadsParams::new(10).with_sort_key(ThreadSortKey::UpdatedAt))
        .await
        .unwrap();
    assert_eq!(ids(by_updated.items()), ["old", "new"]);
}

/// Codex's `test_list_threads_scans_past_head_for_user_event`.
#[tokio::test]
async fn the_preview_is_found_past_the_head() {
    let directory = directory("past_head");
    Thread {
        records_before_user: 12,
        ..Thread::new("late", 1_000)
    }
    .write(&directory)
    .await;
    let page = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    assert_eq!(ids(page.items()), ["late"]);
    assert_eq!(page.items()[0].preview(), "hello");
}

#[tokio::test]
async fn active_listings_need_a_preview_and_no_listing_shows_a_rollout_without_metadata() {
    let directory = directory("preview_required");
    Thread {
        user: None,
        ..Thread::new("silent", 1_000)
    }
    .write(&directory)
    .await;
    Thread::new("spoken", 2_000).write(&directory).await;
    // A rollout without session metadata.
    let bare = SessionId::new("bare");
    let mut writer = RolloutWriter::open(directory.rollout_path(&bare).unwrap(), bare)
        .await
        .unwrap();
    writer
        .append(
            RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
                .with_input(vec![ModelInputItem::Message(Message::user("hi"))]),
        )
        .await
        .unwrap();
    writer.sync_all().await.unwrap();
    drop(writer);

    let page = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    assert_eq!(ids(page.items()), ["spoken"]);

    // Archived listings show threads without a preview, as Codex's flat archived listing does.
    for id in ["silent", "spoken"] {
        directory
            .archive_thread(&ArchiveThreadParams::new(SessionId::new(id)))
            .await
            .unwrap();
    }
    let archived = directory
        .list_threads(&ListThreadsParams::new(10).archived())
        .await
        .unwrap();
    assert_eq!(ids(archived.items()), ["spoken", "silent"]);
    assert_eq!(archived.items()[1].preview(), "");
}

#[tokio::test]
async fn an_image_without_text_previews_as_an_image() {
    let directory = directory("image_preview");
    let session_id = SessionId::new("pic");
    let mut writer = RolloutWriter::open(
        directory.rollout_path(&session_id).unwrap(),
        session_id.clone(),
    )
    .await
    .unwrap();
    writer
        .append_session_meta(RolloutSessionMeta::new(session_id))
        .await
        .unwrap();
    writer
        .append(
            RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead")).with_input(vec![
                ModelInputItem::Message(Message::new(
                    MessageRole::User,
                    vec![ContentBlock::Image(ImageBlock::new(ImageSource::url(
                        "https://example.com/a.png",
                    )))],
                )),
            ]),
        )
        .await
        .unwrap();
    writer.sync_all().await.unwrap();
    drop(writer);
    let page = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    assert_eq!(page.items()[0].preview(), "[Image]");
}

/// Codex's `test_model_provider_filter_selects_only_matching_sessions`, and its cwd filter.
#[tokio::test]
async fn provider_and_cwd_filters_select_threads() {
    let directory = directory("filters");
    Thread::new("openai", 1_000).write(&directory).await;
    Thread {
        provider: Some("anthropic"),
        cwd: Some("/other"),
        ..Thread::new("anthropic", 2_000)
    }
    .write(&directory)
    .await;
    Thread {
        provider: None,
        ..Thread::new("unknown", 3_000)
    }
    .write(&directory)
    .await;

    let list = |params: ListThreadsParams| {
        let directory = &directory;
        async move { directory.list_threads(&params).await.unwrap().into_items() }
    };
    assert_eq!(
        ids(&list(ListThreadsParams::new(10).with_model_providers(vec!["openai".into()])).await),
        ["openai"]
    );
    assert_eq!(
        ids(&list(ListThreadsParams::new(10).with_model_providers(Vec::new())).await),
        ["unknown", "anthropic", "openai"],
        "an empty provider list matches every provider"
    );
    assert_eq!(
        ids(&list(ListThreadsParams::new(10).with_cwd_filters(vec!["/other/".into()])).await),
        ["anthropic"]
    );
    assert!(
        list(ListThreadsParams::new(10).with_cwd_filters(Vec::new()))
            .await
            .is_empty(),
        "an empty cwd list matches no thread"
    );
}

#[tokio::test]
async fn a_cursor_no_listing_returned_is_refused() {
    let directory = directory("bad_cursor");
    for cursor in ["", "nope", "12|", "x|id"] {
        let error = failure(
            directory
                .list_threads(&ListThreadsParams::new(10).with_cursor(cursor))
                .await,
        );
        assert!(
            !is_kind(&error, SessionErrorKind::NotFound),
            "{cursor}: {error}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

/// Codex's `find_thread_names_by_ids_prefers_latest_entry`, its search over names and its legacy
/// rule that a name repeating the preview is not shown.
#[tokio::test]
async fn names_are_kept_in_the_index_and_the_latest_wins() {
    let directory = directory("names");
    Thread::new("a", 1_000).write(&directory).await;
    Thread::new("b", 2_000).write(&directory).await;
    rename(&directory, "a", Some("first")).await;
    rename(&directory, "b", Some("other")).await;
    rename(&directory, "a", Some("latest")).await;

    let page = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    let names: Vec<_> = page.items().iter().map(StoredThread::name).collect();
    assert_eq!(names, [Some("other"), Some("latest")]);

    let searched = directory
        .list_threads(&ListThreadsParams::new(10).with_search_term("lat"))
        .await
        .unwrap();
    assert_eq!(ids(searched.items()), ["a"]);

    let read = directory
        .read_thread(&ReadThreadParams::new(SessionId::new("a")))
        .await
        .unwrap();
    assert_eq!(read.name(), Some("latest"));
    assert_eq!(read.preview(), "hello");

    // A name that only repeats the preview is not shown.
    rename(&directory, "b", Some("hello")).await;
    let read = directory
        .read_thread(&ReadThreadParams::new(SessionId::new("b")))
        .await
        .unwrap();
    assert_eq!(read.name(), None);
}

/// Codex appends an empty name to clear one; its batch lookup then keeps the older name, while
/// the clear holds here.
#[tokio::test]
async fn a_cleared_name_stays_cleared() {
    let directory = directory("clear_name");
    Thread::new("a", 1_000).write(&directory).await;
    rename(&directory, "a", Some("named")).await;
    rename(&directory, "a", None).await;
    let read = directory
        .read_thread(&ReadThreadParams::new(SessionId::new("a")))
        .await
        .unwrap();
    assert_eq!(read.name(), None);
    let page = directory
        .list_threads(&ListThreadsParams::new(10).with_search_term("named"))
        .await
        .unwrap();
    assert!(page.items().is_empty());
}

#[tokio::test]
async fn only_the_name_of_a_patch_is_kept_and_an_unknown_thread_is_not_found() {
    let directory = directory("patch");
    Thread::new("a", 1_000).write(&directory).await;
    let updated = directory
        .update_thread_metadata(&UpdateThreadMetadataParams::new(
            SessionId::new("a"),
            ThreadMetadataPatch::new()
                .with_name(Some("named".into()))
                .with_model("gpt-5")
                .with_preview("ignored"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name(), Some("named"));
    assert_eq!(updated.model(), None);
    assert_eq!(updated.preview(), "hello");

    let error = failure(
        directory
            .update_thread_metadata(&UpdateThreadMetadataParams::new(
                SessionId::new("missing"),
                ThreadMetadataPatch::new().with_name(Some("x".into())),
            ))
            .await,
    );
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

// ---------------------------------------------------------------------------------------------
// Archiving
// ---------------------------------------------------------------------------------------------

/// Codex's `unarchive_thread_restores_rollout_and_returns_updated_thread`.
#[tokio::test]
async fn archiving_moves_the_rollout_aside_and_unarchiving_brings_it_back() {
    let directory = directory("archive");
    let active = Thread::new("a", 1_000).write(&directory).await;
    Thread::new("b", 2_000).write(&directory).await;
    std::fs::write(sidecar(&active), "{}").unwrap();
    rename(&directory, "a", Some("kept")).await;
    let session_id = SessionId::new("a");

    directory
        .archive_thread(&ArchiveThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    let archived_path = directory
        .path()
        .join("archived_sessions")
        .join("rollout-a.jsonl");
    assert!(!active.exists());
    assert!(archived_path.exists());
    assert!(
        sidecar(&archived_path).exists(),
        "the sidecar moves with it"
    );

    let page = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    assert_eq!(ids(page.items()), ["b"]);
    let archived = directory
        .list_threads(&ListThreadsParams::new(10).archived())
        .await
        .unwrap();
    assert_eq!(ids(archived.items()), ["a"]);
    assert!(archived.items()[0].archived_at().is_some());
    assert_eq!(archived.items()[0].name(), Some("kept"));

    let read = failure(
        directory
            .read_thread(&ReadThreadParams::new(session_id.clone()))
            .await,
    );
    assert!(is_kind(&read, SessionErrorKind::NotFound), "{read}");
    let read = directory
        .read_thread(&ReadThreadParams::new(session_id.clone()).including_archived())
        .await
        .unwrap();
    assert!(read.archived_at().is_some());
    assert_eq!(read.rollout_path(), Some(archived_path.as_path()));
    let history = directory
        .load_history(&LoadThreadHistoryParams::new(session_id.clone()).including_archived())
        .await
        .unwrap();
    assert_eq!(history.records().len(), 2);
    assert!(
        directory
            .resume_thread(&ResumeThreadParams::new(session_id.clone()))
            .await
            .is_err(),
        "a resume reopens active threads only"
    );

    set_modified(&archived_path, 1_000);
    let before = SystemTime::now();
    let restored = directory
        .unarchive_thread(&ArchiveThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    assert!(active.exists());
    assert!(!archived_path.exists());
    assert!(sidecar(&active).exists());
    assert_eq!(restored.archived_at(), None);
    assert_eq!(restored.rollout_path(), Some(active.as_path()));
    assert_eq!(restored.preview(), "hello");
    assert!(
        restored.updated_at().unwrap().to_system_time()
            >= before - std::time::Duration::from_secs(2),
        "unarchiving marks the rollout just written"
    );
}

#[tokio::test]
async fn a_thread_a_live_writer_holds_is_neither_archived_nor_deleted() {
    let directory = directory("live_writer");
    Thread::new("a", 1_000).write(&directory).await;
    let session_id = SessionId::new("a");
    let writer = directory
        .resume_thread(&ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();

    for error in [
        failure(
            directory
                .archive_thread(&ArchiveThreadParams::new(session_id.clone()))
                .await,
        ),
        failure(
            directory
                .delete_thread(&DeleteThreadParams::new(session_id.clone()))
                .await,
        ),
    ] {
        assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    }
    assert!(directory.rollout_path(&session_id).unwrap().exists());

    writer.shutdown().await.unwrap();
    directory
        .archive_thread(&ArchiveThreadParams::new(session_id))
        .await
        .unwrap();
}

#[tokio::test]
async fn unarchiving_never_overwrites_an_active_rollout() {
    let directory = directory("unarchive_shadowed");
    Thread::new("a", 1_000).write(&directory).await;
    let session_id = SessionId::new("a");
    directory
        .archive_thread(&ArchiveThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    let active = Thread::new("a", 2_000).write(&directory).await;
    let before = std::fs::read(&active).unwrap();

    let error = failure(
        directory
            .unarchive_thread(&ArchiveThreadParams::new(session_id.clone()))
            .await,
    );
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert_eq!(std::fs::read(&active).unwrap(), before);

    let missing = failure(
        directory
            .unarchive_thread(&ArchiveThreadParams::new(SessionId::new("missing")))
            .await,
    );
    assert!(is_kind(&missing, SessionErrorKind::NotFound), "{missing}");
    let missing = failure(
        directory
            .archive_thread(&ArchiveThreadParams::new(SessionId::new("missing")))
            .await,
    );
    assert!(is_kind(&missing, SessionErrorKind::NotFound), "{missing}");
}

/// Codex's default `archive_threads`: the first must be archived, a later failure is skipped.
#[tokio::test]
async fn archiving_threads_requires_the_first_and_skips_later_failures() {
    let directory = directory("archive_many");
    Thread::new("a", 1_000).write(&directory).await;
    Thread::new("c", 3_000).write(&directory).await;
    let archived = directory
        .archive_threads(&ArchiveThreadsParams::new(vec![
            SessionId::new("a"),
            SessionId::new("missing"),
            SessionId::new("c"),
        ]))
        .await
        .unwrap();
    assert_eq!(archived, [SessionId::new("a"), SessionId::new("c")]);

    let error = failure(
        directory
            .archive_threads(&ArchiveThreadsParams::new(vec![SessionId::new("missing")]))
            .await,
    );
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

#[tokio::test]
async fn a_new_thread_cannot_take_the_id_of_an_archived_one() {
    let directory = directory("create_over_archived");
    Thread::new("a", 1_000).write(&directory).await;
    directory
        .archive_thread(&ArchiveThreadParams::new(SessionId::new("a")))
        .await
        .unwrap();
    let error = failure(
        directory
            .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
                SessionId::new("a"),
            )))
            .await,
    );
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

// ---------------------------------------------------------------------------------------------
// Deleting
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn deleting_removes_the_rollouts_their_sidecars_and_the_names() {
    let directory = directory("delete");
    let active = Thread::new("a", 1_000).write(&directory).await;
    std::fs::write(sidecar(&active), "{}").unwrap();
    rename(&directory, "a", Some("gone")).await;
    Thread::new("b", 2_000).write(&directory).await;
    directory
        .archive_thread(&ArchiveThreadParams::new(SessionId::new("b")))
        .await
        .unwrap();

    directory
        .delete_thread(&DeleteThreadParams::new(SessionId::new("a")))
        .await
        .unwrap();
    directory
        .delete_thread(&DeleteThreadParams::new(SessionId::new("b")))
        .await
        .unwrap();
    assert!(!active.exists());
    assert!(!sidecar(&active).exists());
    let index = std::fs::read_to_string(directory.path().join("session_index.jsonl")).unwrap();
    assert!(!index.contains("gone"), "{index}");
    assert!(
        directory
            .list_threads(&ListThreadsParams::new(10).archived())
            .await
            .unwrap()
            .items()
            .is_empty()
    );

    let error = failure(
        directory
            .delete_thread(&DeleteThreadParams::new(SessionId::new("a")))
            .await,
    );
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
    // Codex's default `delete_threads`: a thread already gone counts as deleted.
    Thread::new("c", 3_000).write(&directory).await;
    directory
        .delete_threads(&DeleteThreadsParams::new(vec![
            SessionId::new("a"),
            SessionId::new("c"),
        ]))
        .await
        .unwrap();
    assert!(
        !directory
            .rollout_path(&SessionId::new("c"))
            .unwrap()
            .exists()
    );
}

#[tokio::test]
async fn spawned_children_are_still_found_beside_listed_threads() {
    let directory = directory("children");
    Thread::new("root", 1_000).write(&directory).await;
    let spawn = RolloutThreadSpawn::new(
        SessionId::new("root"),
        SessionId::new("root"),
        1,
        AgentPath::root().join("worker").unwrap(),
    );
    let child = directory
        .create_thread(&SessionId::new("child"), &spawn)
        .await
        .unwrap();
    child.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("worker"))
            .with_input(vec![ModelInputItem::Message(Message::user("task"))]),
    ));
    child.shutdown().await.unwrap();
    rename(&directory, "root", Some("named")).await;

    let page = directory
        .list_threads(&ListThreadsParams::new(10))
        .await
        .unwrap();
    // The child was created now, after the root's fixed creation time.
    assert_eq!(ids(page.items()), ["child", "root"]);
    assert_eq!(
        page.items()[0].parent_session_id(),
        Some(&SessionId::new("root"))
    );
    assert_eq!(
        directory
            .children(&SessionId::new("root"))
            .await
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------------------------
// In memory
// ---------------------------------------------------------------------------------------------

/// Codex's in-memory store: a listing returns every thread ordered by id, a patch is merged and
/// read back, archiving keeps nothing, deleting forgets.
#[tokio::test]
async fn the_in_memory_store_manages_threads_as_codexs_does() {
    let store = InMemoryThreadStore::new();
    for id in ["b", "a"] {
        store
            .create_thread_with(&CreateThreadParams::new(
                RolloutSessionMeta::new(SessionId::new(id)).with_model_provider("openai"),
            ))
            .await
            .unwrap();
    }
    let page = store
        .list_threads(&ListThreadsParams::new(1).with_sort_direction(SortDirection::Desc))
        .await
        .unwrap();
    assert_eq!(ids(page.items()), ["a", "b"]);
    assert_eq!(page.next_cursor(), None);

    let session_id = SessionId::new("a");
    store
        .update_thread_metadata(&UpdateThreadMetadataParams::new(
            session_id.clone(),
            ThreadMetadataPatch::new()
                .with_name(Some("named".into()))
                .with_preview("preview")
                .with_model("gpt-5")
                .with_effort(Some("high".into())),
        ))
        .await
        .unwrap();
    let updated = store
        .update_thread_metadata(&UpdateThreadMetadataParams::new(
            session_id.clone(),
            ThreadMetadataPatch::new()
                .with_effort(None)
                .with_model_provider("anthropic"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name(), Some("named"));
    assert_eq!(updated.preview(), "preview");
    assert_eq!(updated.model(), Some("gpt-5"));
    assert_eq!(updated.effort(), None, "a cleared field stays cleared");
    assert_eq!(updated.model_provider(), Some("anthropic"));

    store
        .archive_thread(&ArchiveThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    assert_eq!(
        store
            .unarchive_thread(&ArchiveThreadParams::new(session_id.clone()))
            .await
            .unwrap()
            .name(),
        Some("named")
    );

    store
        .delete_thread(&DeleteThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    let error = failure(
        store
            .delete_thread(&DeleteThreadParams::new(session_id.clone()))
            .await,
    );
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
    let error = failure(
        store
            .update_thread_metadata(&UpdateThreadMetadataParams::new(
                session_id,
                ThreadMetadataPatch::new(),
            ))
            .await,
    );
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

fn sidecar(path: &PathBuf) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".sidecar.json");
    PathBuf::from(sidecar)
}

// ---------------------------------------------------------------------------------------------
// Review regressions
// ---------------------------------------------------------------------------------------------

/// Archiving never replaces an archived rollout, and neither way of creating a thread can take an
/// id whose rollout is archived, so an earlier archived history cannot be overwritten.
#[tokio::test]
async fn an_archived_history_is_never_overwritten() {
    let directory = directory("no_overwrite");
    let spawn = RolloutThreadSpawn::new(
        SessionId::new("root"),
        SessionId::new("root"),
        1,
        AgentPath::root().join("worker").unwrap(),
    );
    let child = SessionId::new("child");
    let first = directory.create_thread(&child, &spawn).await.unwrap();
    first.persist().await.unwrap();
    first.shutdown().await.unwrap();
    directory
        .archive_thread(&ArchiveThreadParams::new(child.clone()))
        .await
        .unwrap();
    let archived_path = directory
        .path()
        .join("archived_sessions")
        .join("rollout-child.jsonl");
    let archived = std::fs::read(&archived_path).unwrap();

    // The spawn entry shares the conflict check of `create_thread_with`.
    let error = failure(directory.create_thread(&child, &spawn).await);
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");

    // A rollout put back under the same id by other means is not archived over the old one.
    Thread::new("child", 2_000).write(&directory).await;
    let error = failure(
        directory
            .archive_thread(&ArchiveThreadParams::new(child.clone()))
            .await,
    );
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert_eq!(std::fs::read(&archived_path).unwrap(), archived);
    assert!(directory.rollout_path(&child).unwrap().exists());

    let store = InMemoryThreadStore::new();
    store.create_thread(&child, &spawn).await.unwrap();
    let error = failure(store.create_thread(&child, &spawn).await);
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

/// Polls `future` up to `polls` times, then drops it.
async fn poll_then_drop<F: std::future::Future>(future: F, polls: usize) {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    for _ in 0..polls {
        if future.as_mut().poll(&mut context).is_ready() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// Where a thread's rollout and sidecar are once its file work has settled: `Some(true)` when both
/// are archived, `Some(false)` when both are active, `None` when they are apart.
async fn settled(directory: &RolloutThreadDirectory, id: &str) -> Option<bool> {
    let active = directory.rollout_path(&SessionId::new(id)).unwrap();
    let archived = directory
        .path()
        .join("archived_sessions")
        .join(format!("rollout-{id}.jsonl"));
    // A blocking task still running finishes well within this.
    for _ in 0..200 {
        let state = (
            active.exists(),
            sidecar(&active).exists(),
            archived.exists(),
            sidecar(&archived).exists(),
        );
        match state {
            (true, true, false, false) => return Some(false),
            (false, false, true, true) => return Some(true),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    None
}

/// A dropped archive, unarchive or delete still completes its file work, under its writer lock.
#[tokio::test]
async fn a_dropped_file_operation_still_completes_as_a_whole() {
    let directory = directory("cancelled");
    for polls in 0..12 {
        let id = format!("t{polls}");
        let session_id = SessionId::new(id.as_str());
        let path = Thread::new(&id, 1_000).write(&directory).await;
        std::fs::write(sidecar(&path), "{}").unwrap();

        poll_then_drop(
            directory.archive_thread(&ArchiveThreadParams::new(session_id.clone())),
            polls,
        )
        .await;
        let archived = settled(&directory, &id)
            .await
            .unwrap_or_else(|| panic!("archive dropped after {polls} polls split the files"));
        if archived {
            poll_then_drop(
                directory.unarchive_thread(&ArchiveThreadParams::new(session_id.clone())),
                polls,
            )
            .await;
            assert!(
                settled(&directory, &id).await.is_some(),
                "unarchive dropped after {polls} polls split the files"
            );
        }

        poll_then_drop(
            directory.delete_thread(&DeleteThreadParams::new(session_id.clone())),
            polls,
        )
        .await;
        // Whatever was done, the lock is let go once the work completes.
        let mut deleted = false;
        for _ in 0..200 {
            match directory
                .delete_thread(&DeleteThreadParams::new(session_id.clone()))
                .await
            {
                Ok(()) => deleted = true,
                Err(error) if is_kind(&error, SessionErrorKind::NotFound) => deleted = true,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
            if deleted {
                break;
            }
        }
        assert!(deleted, "the writer lock was let go after {polls} polls");
        assert!(!path.exists() && !sidecar(&path).exists());
    }
}

/// A batch takes every writer lock before it moves or deletes anything, as Codex's local store
/// does, so a busy thread anywhere in it leaves the rest untouched.
#[tokio::test]
async fn a_batch_with_a_busy_thread_touches_nothing() {
    let directory = directory("batch_locks");
    let parent = Thread::new("parent", 1_000).write(&directory).await;
    let child = Thread::new("child", 2_000).write(&directory).await;
    let writer = directory
        .resume_thread(&ResumeThreadParams::new(SessionId::new("child")))
        .await
        .unwrap();

    let error = failure(
        directory
            .archive_threads(&ArchiveThreadsParams::new(vec![
                SessionId::new("parent"),
                SessionId::new("child"),
            ]))
            .await,
    );
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert!(parent.exists(), "nothing was archived");

    let error = failure(
        directory
            .delete_threads(&DeleteThreadsParams::new(vec![
                SessionId::new("parent"),
                SessionId::new("child"),
            ]))
            .await,
    );
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert!(parent.exists() && child.exists(), "nothing was deleted");

    writer.shutdown().await.unwrap();
    let archived = directory
        .archive_threads(&ArchiveThreadsParams::new(vec![
            SessionId::new("parent"),
            SessionId::new("child"),
        ]))
        .await
        .unwrap();
    assert_eq!(
        archived,
        [SessionId::new("parent"), SessionId::new("child")]
    );
}

/// More threads than one call reads heads of: the cursor carries the walk past the cap, so every
/// thread is listed.
#[tokio::test]
async fn paging_reaches_past_the_scan_cap() {
    use ra_session::{RolloutPayload, RolloutRecord, lite::MAX_SCAN_FILES};

    let directory = directory("past_cap");
    let total = MAX_SCAN_FILES + 1;
    for index in 0..total {
        let session_id = SessionId::new(format!("t{index:05}"));
        let records = [
            RolloutPayload::SessionMeta(
                RolloutSessionMeta::new(session_id.clone())
                    .with_created_at(EventTimestamp::from_millis(index as u64)),
            ),
            RolloutPayload::RunStarted(
                RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
                    .with_input(vec![ModelInputItem::Message(Message::user("hi"))]),
            ),
        ];
        let mut text = String::new();
        for (seq, payload) in records.into_iter().enumerate() {
            let record =
                RolloutRecord::new(seq as u64, EventTimestamp::from_millis(0), payload).unwrap();
            text.push_str(&serde_json::to_string(&record).unwrap());
            text.push('\n');
        }
        std::fs::write(directory.rollout_path(&session_id).unwrap(), text).unwrap();
    }

    let first = directory
        .list_threads(&ListThreadsParams::new(total))
        .await
        .unwrap();
    assert_eq!(first.items().len(), MAX_SCAN_FILES);
    let second = directory
        .list_threads(&ListThreadsParams::new(total).with_cursor(first.next_cursor().unwrap()))
        .await
        .unwrap();
    assert_eq!(ids(second.items()), ["t00000"]);
    assert_eq!(second.next_cursor(), None);
}
