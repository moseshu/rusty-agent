// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! List exclusions port Codex's thread-list tests; unavailable receipts preserve its wire shape.

use super::*;
use ra_core::session::rollout::{RolloutThreadSpawn, RolloutThreadStore};
use ra_protocol::server::protocol::{ThreadListParams, ThreadListResponse, ThreadReadState};
use ra_session::store::{
    DeleteThreadParams, ListThreadsParams, LoadThreadHistoryParams, ResumeThreadParams,
    StoredThread, StoredThreadHistory, ThreadPage,
};

fn list_server(store: Arc<dyn ThreadStore>) -> Server {
    build(
        Options {
            store: Some(store),
            factory: None,
            unload_delay: None,
        },
        None,
    )
    .0
}

fn ids(page: &Value) -> Vec<String> {
    page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn seed_list(store: &Arc<dyn ThreadStore>) -> Vec<String> {
    use ra_core::{agent::control::AgentPath, session::rollout::RolloutThreadSpawn};

    let mut ids = Vec::new();
    for index in 0..12 {
        let id = format!("thread-{index:02}");
        let mut meta = RolloutSessionMeta::new(SessionId::new(&id))
            .with_created_at(EventTimestamp::from_millis(1000 + index / 2))
            .with_model_provider(if index == 5 { "other" } else { "one" })
            .with_cwd(if index == 6 { "/work/b" } else { "/work/a" });
        if index >= 10 {
            meta = meta.with_thread_spawn(RolloutThreadSpawn::new(
                SessionId::new("thread-00"),
                SessionId::new("thread-00"),
                1,
                AgentPath::root().join(&format!("child_{index}")).unwrap(),
            ));
        }
        let recorder = store
            .create_thread_with(&CreateThreadParams::new(meta))
            .await
            .unwrap();
        record(&recorder, completed_run(&id, &id, "Done")).await;
        recorder.shutdown().await.unwrap();
        ids.push(id);
    }
    ids
}

/// Codex's exclusions refill across scans, DB-only reads, filters, sorts and archived listings.
#[tokio::test]
async fn exclusions_refill_pages_across_storage_paths_and_preserve_both_cursors() {
    for indexed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RolloutThreadDirectory::new(directory.path());
        if indexed {
            store = store.with_state_db(
                ra_session::store::local::try_init(directory.path())
                    .await
                    .unwrap(),
            );
        }
        let store: Arc<dyn ThreadStore> = Arc::new(store);
        let all_ids = seed_list(&store).await;
        let server = list_server(Arc::clone(&store));
        let mut connection = server.connect(1).await;
        for archived in [false, true] {
            if archived {
                for id in &all_ids {
                    store
                        .archive_thread(&ArchiveThreadParams::new(SessionId::new(id)))
                        .await
                        .unwrap();
                }
            }
            for db_only in [false, true] {
                if db_only && !indexed {
                    continue;
                }
                for sort in ["created_at", "updated_at", "recency_at"] {
                    for direction in ["asc", "desc"] {
                        let mut query = json!({
                            "limit": 100, "archived": archived, "useStateDbOnly": db_only,
                            "sortKey": sort, "sortDirection": direction,
                            "modelProviders": ["one"], "cwd": "/work/a",
                            "sourceKinds": ["unknown"]
                        });
                        let baseline = ids(&server
                            .call(&mut connection, "thread/list", query.clone())
                            .await);
                        assert_eq!(baseline.len(), 8, "{query}");
                        let excluded = vec![
                            baseline[0].clone(),
                            baseline[1].clone(),
                            baseline[2].clone(),
                            baseline[4].clone(),
                            baseline[0].clone(),
                            "unknown-thread".into(),
                        ];
                        let expected: Vec<_> = baseline
                            .iter()
                            .filter(|id| !excluded.contains(id))
                            .cloned()
                            .collect();
                        query["excludedThreadIds"] = json!(excluded);
                        query["limit"] = json!(2);
                        let mut next_query = query.clone();
                        let mut actual = Vec::new();
                        let mut reverse_cursor = Value::Null;
                        for _ in 0..=baseline.len() {
                            let page = server
                                .call(&mut connection, "thread/list", next_query.clone())
                                .await;
                            assert_eq!(
                                page.get("readStates"),
                                Some(&if indexed { json!({}) } else { Value::Null })
                            );
                            let page_ids = ids(&page);
                            assert_eq!(
                                page_ids.len(),
                                2.min(expected.len() - actual.len()),
                                "{next_query}"
                            );
                            if actual.len() == 2 {
                                reverse_cursor = page["backwardsCursor"].clone();
                            }
                            actual.extend(page_ids);
                            next_query["cursor"] = page["nextCursor"].clone();
                            if page["nextCursor"].is_null() {
                                break;
                            }
                        }
                        assert_eq!(actual, expected, "{query}");
                        assert!(
                            next_query["cursor"].is_null(),
                            "listing must terminate: {next_query}"
                        );
                        let mut reverse = query.clone();
                        reverse["sortDirection"] =
                            json!(if direction == "asc" { "desc" } else { "asc" });
                        reverse["cursor"] = reverse_cursor;
                        let page = server.call(&mut connection, "thread/list", reverse).await;
                        assert_eq!(
                            ids(&page),
                            expected[..2].iter().rev().cloned().collect::<Vec<_>>()
                        );

                        query["excludedThreadIds"] = json!(baseline);
                        let page = server.call(&mut connection, "thread/list", query).await;
                        assert!(ids(&page).is_empty());
                        assert!(page["nextCursor"].is_null());
                        assert!(page["backwardsCursor"].is_null());
                    }
                }
            }
        }
    }
}

/// The entry bound applies before deduplication; the framework keeps opaque session IDs.
#[tokio::test]
async fn exclusions_validate_the_bound_and_accept_generated_and_host_supplied_ids() {
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::new());
    let known = [
        "host-custom-id".to_owned(),
        SessionId::generate().to_string(),
    ];
    for id in &known {
        let recorder = store
            .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
                SessionId::new(id),
            )))
            .await
            .unwrap();
        record(&recorder, completed_run(id, id, "Done")).await;
    }
    let server = list_server(store);
    let mut connection = server.connect_with(1, InitializeCapabilities::new()).await;
    let baseline = ids(&server.call(&mut connection, "thread/list", json!({})).await);
    for excluded in [Value::Null, json!([]), json!(["unknown", "not-a-uuid"])] {
        let page = server
            .call(
                &mut connection,
                "thread/list",
                json!({"excludedThreadIds": excluded}),
            )
            .await;
        assert_eq!(ids(&page), baseline);
    }
    for id in &known {
        let mut excluded = vec!["unknown-thread".to_owned(); 99];
        excluded.push(id.clone());
        let page = server
            .call(
                &mut connection,
                "thread/list",
                json!({"excludedThreadIds": excluded}),
            )
            .await;
        assert_eq!(
            ids(&page),
            baseline
                .iter()
                .filter(|other| *other != id)
                .cloned()
                .collect::<Vec<_>>()
        );
        excluded.push(id.clone());
        for values in [excluded, vec![id.clone(); 101]] {
            let error = server
                .call_error(
                    &mut connection,
                    "thread/list",
                    json!({"excludedThreadIds": values}),
                )
                .await;
            assert_eq!(error["code"], -32602);
            assert_eq!(
                error["message"],
                "excludedThreadIds accepts at most 100 entries"
            );
        }
    }
}

#[tokio::test]
async fn unavailable_read_receipts_are_explicit_null_for_stable_and_experimental_clients() {
    let server = list_server(Arc::new(InMemoryThreadStore::new()));
    for experimental in [false, true] {
        let mut connection = server
            .connect_with(
                u64::from(experimental) + 1,
                InitializeCapabilities::new().with_experimental_api(experimental),
            )
            .await;
        let page = server.call(&mut connection, "thread/list", json!({})).await;
        assert_eq!(page.get("readStates"), Some(&Value::Null));
        let typed: ThreadListResponse = serde_json::from_value(page).unwrap();
        assert!(typed.read_states.is_none());
    }
}

#[test]
fn list_and_read_receipt_types_preserve_codex_wire_shapes() {
    for value in [json!({}), json!({"excludedThreadIds": null})] {
        assert!(
            serde_json::from_value::<ThreadListParams>(value)
                .unwrap()
                .excluded_thread_ids
                .is_none()
        );
    }
    for invalid in [json!("thread"), json!([3]), json!([null])] {
        assert!(
            serde_json::from_value::<ThreadListParams>(json!({"excludedThreadIds": invalid}))
                .is_err()
        );
    }
    for position in [
        Value::Null,
        json!({"type": "threadStart"}),
        json!({"type": "turn", "turnId": "turn-1"}),
    ] {
        let value = json!({"firstUnread": position, "revision": "opaque-revision"});
        let typed: ThreadReadState = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(typed).unwrap(), value);
        let page = json!({"data": [], "readStates": {"thread": value}, "nextCursor": null, "backwardsCursor": null});
        let typed: ThreadListResponse = serde_json::from_value(page.clone()).unwrap();
        assert_eq!(serde_json::to_value(typed).unwrap(), page);
    }
}

/// A host store can return an invalid continuation; only listing is scripted here.
struct ScriptedListing {
    inner: InMemoryThreadStore,
    pages: Mutex<VecDeque<ThreadPage>>,
}

#[async_trait]
impl RolloutThreadStore for ScriptedListing {
    async fn create_thread(
        &self,
        id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        self.inner.create_thread(id, spawn).await
    }
}

#[async_trait]
impl ThreadStore for ScriptedListing {
    async fn create_thread_with(
        &self,
        params: &CreateThreadParams,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        self.inner.create_thread_with(params).await
    }
    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>> {
        self.inner.resume_thread(params).await
    }
    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory> {
        self.inner.load_history(params).await
    }
    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread> {
        self.inner.read_thread(params).await
    }
    async fn list_threads(&self, _params: &ListThreadsParams) -> Result<ThreadPage> {
        self.pages
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("scripted listing exhausted"))
    }
    async fn update_thread_metadata(
        &self,
        params: &UpdateThreadMetadataParams,
    ) -> Result<Option<StoredThread>> {
        self.inner.update_thread_metadata(params).await
    }
    async fn archive_thread(&self, params: &ArchiveThreadParams) -> Result<()> {
        self.inner.archive_thread(params).await
    }
    async fn unarchive_thread(&self, params: &ArchiveThreadParams) -> Result<StoredThread> {
        self.inner.unarchive_thread(params).await
    }
    async fn delete_thread(&self, params: &DeleteThreadParams) -> Result<()> {
        self.inner.delete_thread(params).await
    }
}

#[tokio::test]
async fn repeated_cursors_are_internal_errors_including_full_pages_and_filtered_cycles() {
    let inner = InMemoryThreadStore::new();
    let id = SessionId::new("host-thread");
    let recorder = inner
        .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
            id.clone(),
        )))
        .await
        .unwrap();
    record(&recorder, completed_run("turn", "Hello", "Done")).await;
    let thread = inner.read_thread(&ReadThreadParams::new(id)).await.unwrap();
    for (cursors, params) in [
        (
            vec!["1000|first", "2000|second", "1000|first"],
            json!({"excludedThreadIds": ["host-thread"]}),
        ),
        (
            vec!["1000|first"],
            json!({"limit": 1, "cursor": "1000|first"}),
        ),
    ] {
        let pages = cursors
            .into_iter()
            .map(|cursor| ThreadPage::new(vec![thread.clone()], Some(cursor.into())))
            .collect();
        let server = list_server(Arc::new(ScriptedListing {
            inner: inner.clone(),
            pages: Mutex::new(pages),
        }));
        let mut connection = server.connect(1).await;
        let error = server
            .call_error(&mut connection, "thread/list", params)
            .await;
        assert_eq!(error["code"], -32603);
        assert_eq!(
            error["message"],
            "thread listing returned a repeated cursor"
        );
    }
}
