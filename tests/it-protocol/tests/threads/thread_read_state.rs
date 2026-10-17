// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Durable read-state protocol tests ported from Codex's v2 suite, on the local legacy store.

use super::*;
use ra_protocol::server::protocol::{ThreadReadState, ThreadReadStateUpdateResponse};
use ra_session::store::local::{ReadStateOperation, StateRuntime};

async fn durable_server(
    directory: &std::path::Path,
    script: &Arc<Script>,
) -> (Server, Arc<StateRuntime>) {
    durable_server_with_source(directory, script, None).await
}

async fn durable_server_with_source(
    directory: &std::path::Path,
    script: &Arc<Script>,
    source: Option<ra_protocol::server::SessionSource>,
) -> (Server, Arc<StateRuntime>) {
    let db = ra_session::store::local::try_init(directory).await.unwrap();
    let store = Arc::new(RolloutThreadDirectory::new(directory).with_state_db(Arc::clone(&db)));
    let server = build_with_session_source(
        Options {
            store: Some(store),
            factory: Some(Factory::new(script)),
            unload_delay: Some(Duration::from_millis(10)),
        },
        None,
        source,
    )
    .0;
    (server, db)
}

async fn receipt(server: &Server, connection: &mut Connection, id: &str) -> Value {
    server
        .call(connection, "thread/read", json!({"threadId": id}))
        .await["readState"]
        .clone()
}

async fn mark(
    server: &Server,
    connection: &mut Connection,
    id: &str,
    state: &Value,
    operation: &str,
) -> Value {
    let result = server.call(connection, "thread/readState/update", json!({
        "threadId": id, "expectedRevision": state["revision"], "operation": {"type": operation}
    })).await;
    let _: ThreadReadStateUpdateResponse = serde_json::from_value(result.clone()).unwrap();
    result["readState"].clone()
}

#[tokio::test]
async fn read_state_methods_require_experimental_capability() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut stable = server.connect_with(1, InitializeCapabilities::new()).await;
    let error = server
        .call_error(
            &mut stable,
            "thread/readState/update",
            json!({
                "threadId": "unknown", "expectedRevision": "none", "operation": {"type": "unread"}
            }),
        )
        .await;
    assert_eq!(error["code"], -32600);
    assert_eq!(
        error["message"],
        "thread/readState/update requires experimentalApi capability"
    );
    let mut experimental = server.connect(2).await;
    let error = server
        .call_error(
            &mut experimental,
            "thread/readState/update",
            json!({
                "threadId": "unknown", "expectedRevision": "none", "operation": {"type": "unread"}
            }),
        )
        .await;
    assert_eq!(error["code"], -32601);
    assert_eq!(error["message"], "durable thread read state unavailable");
}

#[tokio::test]
async fn read_state_tracks_terminal_activity_and_explicit_marks_across_reload() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, db) = durable_server(directory.path(), &script).await;
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let id = thread.id();
    script.answer("Ready");
    let turn_id = server
        .run_turn(&mut connection, id, "Finish this task.")
        .await;
    let first = receipt(&server, &mut connection, id).await;
    let _: ThreadReadState = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(
        first["firstUnread"],
        json!({"type": "turn", "turnId": turn_id})
    );
    assert_eq!(
        receipt(&server, &mut connection, id).await,
        first,
        "reading does not acknowledge"
    );
    for experimental in [false, true] {
        let mut reader = server
            .connect_with(
                10 + u64::from(experimental),
                InitializeCapabilities::new().with_experimental_api(experimental),
            )
            .await;
        let listed = server
            .call(
                &mut reader,
                "thread/list",
                json!({"limit": 1, "useStateDbOnly": true}),
            )
            .await;
        assert_eq!(listed["data"][0]["id"], id);
        assert_eq!(listed["readStates"], json!({id: first}));
        assert_eq!(receipt(&server, &mut reader, id).await, first);
    }
    let changed = connection.notification("thread/readState/changed").await;
    assert_eq!(changed, json!({"threadId": id, "readState": first}));
    let marked = mark(&server, &mut connection, id, &first, "unread").await;
    assert_eq!(marked["firstUnread"], json!({"type": "threadStart"}));
    assert_ne!(marked["revision"], first["revision"]);
    assert_eq!(
        connection.notification("thread/readState/changed").await["readState"],
        marked
    );
    for operation in ["read", "unread"] {
        let error = server.call_error(&mut connection, "thread/readState/update", json!({
            "threadId": id, "expectedRevision": first["revision"], "operation": {"type": operation}
        })).await;
        assert_eq!(error["code"], -32600);
        assert_eq!(error["message"], "read state changed");
        assert_eq!(
            error["data"],
            json!({"reason": "readStateConflict", "readState": marked})
        );
    }
    let fresh = mark(&server, &mut connection, id, &marked, "read").await;
    assert_eq!(fresh["firstUnread"], Value::Null);
    assert_eq!(
        connection.notification("thread/readState/changed").await["readState"],
        fresh
    );
    assert_eq!(receipt(&server, &mut connection, id).await, fresh);
    server
        .call(
            &mut connection,
            "thread/unsubscribe",
            json!({"threadId": id}),
        )
        .await;
    connection.notification("thread/closed").await;
    assert_eq!(receipt(&server, &mut connection, id).await, fresh);
    server._task.abort();
    let (reopened, _) = durable_server(directory.path(), &script).await;
    let mut reader = reopened.connect(1).await;
    assert_eq!(receipt(&reopened, &mut reader, id).await, fresh);
    reopened
        .resume(&mut reader, ThreadResumeParams::new(id))
        .await;
    assert_eq!(
        receipt(&reopened, &mut reader, id).await,
        fresh,
        "history replay must not publish attention"
    );
    assert!(
        !db.publish_thread_attention(&SessionId::new(id), &turn_id)
            .await
            .unwrap()
    );
    script.answer("Another result");
    let later = reopened.run_turn(&mut reader, id, "Continue").await;
    let after = receipt(&reopened, &mut reader, id).await;
    assert_eq!(after["firstUnread"], json!({"type":"turn", "turnId":later}));
    assert_ne!(after["revision"], fresh["revision"]);
    reopened
        .call(&mut reader, "thread/delete", json!({"threadId":id}))
        .await;
    assert!(
        db.thread_read_states(&[SessionId::new(id)])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !db.publish_thread_attention(&SessionId::new(id), "late")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn only_terminal_attention_is_published_and_completion_is_last() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, _) = durable_server(directory.path(), &script).await;
    let mut connection = server.connect(1).await;
    for (answer, visible) in [
        ("", false),
        ("  \n ", false),
        ("<decision>DONT_NOTIFY</decision>", false),
        ("Ready", true),
    ] {
        let thread = server
            .start_thread(&mut connection, ThreadStartParams::new())
            .await;
        script.answer(answer);
        server
            .turn_start(&mut connection, thread.id(), "Finish")
            .await;
        let mut changes = 0;
        loop {
            let message = connection.next().await;
            if message["method"] == "thread/readState/changed" {
                assert_eq!(message["params"]["threadId"], thread.id());
                changes += 1;
            }
            if message["method"] == "turn/completed" {
                break;
            }
        }
        assert_eq!(changes, usize::from(visible), "{answer}");
        let state = receipt(&server, &mut connection, thread.id()).await;
        assert_eq!(!state["firstUnread"].is_null(), visible);
    }
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.push(Step::Fail);
    server.run_turn(&mut connection, thread.id(), "Fail").await;
    assert!(
        !receipt(&server, &mut connection, thread.id()).await["firstUnread"].is_null(),
        "failure needs attention without final text"
    );
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let gate = Arc::new(Notify::new());
    script.push(Step::After(gate, final_message("Not delivered")));
    let calls = script.requests.lock().unwrap().len();
    let interrupted_turn = server
        .turn_start(&mut connection, thread.id(), "Wait")
        .await;
    script.wait_for_calls(calls + 1).await;
    server
        .call(
            &mut connection,
            "turn/interrupt",
            json!({"threadId": thread.id(), "turnId": interrupted_turn}),
        )
        .await;
    connection.notification("turn/completed").await;
    assert!(
        receipt(&server, &mut connection, thread.id()).await["firstUnread"].is_null(),
        "interrupt without text or error needs no attention"
    );
}

#[tokio::test]
async fn notifications_follow_subscriptions_experimental_capability_and_opt_out() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, _) = durable_server(directory.path(), &script).await;
    let mut owner = server.connect(1).await;
    let mut other = server.connect(2).await;
    let mut stable = server.connect_with(3, InitializeCapabilities::new()).await;
    let mut opted_out = server
        .connect_with(
            4,
            InitializeCapabilities::new()
                .with_experimental_api(true)
                .with_opt_out_notification_methods(["thread/readState/changed"]),
        )
        .await;
    let thread = server
        .start_thread(&mut owner, ThreadStartParams::new())
        .await;
    script.answer("<decision>DONT_NOTIFY</decision>");
    server
        .run_turn(&mut owner, thread.id(), "Materialize")
        .await;
    for connection in [&mut other, &mut stable, &mut opted_out] {
        server
            .resume(connection, ThreadResumeParams::new(thread.id()))
            .await;
    }
    let mut outsider = server.connect(5).await;
    script.answer("Ready");
    server.run_turn(&mut owner, thread.id(), "Finish").await;
    let first = owner.notification("thread/readState/changed").await;
    assert_eq!(other.notification("thread/readState/changed").await, first);
    for connection in [&mut stable, &mut opted_out, &mut outsider] {
        assert!(
            !connection
                .drain()
                .await
                .iter()
                .any(|message| message["method"] == "thread/readState/changed")
        );
    }
    let state = mark(
        &server,
        &mut other,
        thread.id(),
        &first["readState"],
        "read",
    )
    .await;
    assert_eq!(
        owner.notification("thread/readState/changed").await["readState"],
        state
    );
    assert_eq!(
        other.notification("thread/readState/changed").await["readState"],
        state
    );
    let stale = first["readState"].clone();
    let error = server.call_error(&mut other, "thread/readState/update", json!({"threadId":thread.id(),"expectedRevision":stale["revision"],"operation":{"type":"unread"}})).await;
    assert_eq!(error["data"]["reason"], "readStateConflict");
    assert!(
        !owner
            .drain()
            .await
            .iter()
            .any(|message| message["method"] == "thread/readState/changed")
    );
    server
        .call(
            &mut other,
            "thread/unsubscribe",
            json!({"threadId":thread.id()}),
        )
        .await;
    mark(&server, &mut owner, thread.id(), &state, "unread").await;
    assert!(
        !other
            .drain()
            .await
            .iter()
            .any(|message| message["method"] == "thread/readState/changed")
    );
}

#[tokio::test]
async fn ineligible_scopes_are_absent_from_available_maps_and_cannot_be_marked() {
    use ra_core::{agent::control::AgentPath, session::rollout::RolloutThreadSpawn};
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, db) = durable_server(directory.path(), &script).await;
    let mut connection = server.connect(1).await;
    let cases = [
        (
            "user",
            Some(json!({"custom":"host"})),
            Some("user"),
            false,
            true,
        ),
        ("unset_scope", Some(json!("cli")), None, false, true),
        ("unknown", None, None, false, false),
        (
            "internal",
            Some(json!({"internal":"guardian"})),
            None,
            false,
            false,
        ),
        (
            "feature",
            Some(json!("exec")),
            Some("memory_consolidation"),
            false,
            false,
        ),
        ("spawned", Some(json!("cli")), Some("user"), true, false),
    ];
    for (id, source, scope, spawned, eligible) in cases {
        let mut meta = RolloutSessionMeta::new(SessionId::new(id));
        if let Some(source) = source {
            meta = meta.with_source(source);
        }
        if let Some(scope) = scope {
            meta = meta.with_thread_source(scope);
        }
        if spawned {
            meta = meta.with_thread_spawn(RolloutThreadSpawn::new(
                SessionId::new("user"),
                SessionId::new("user"),
                1,
                AgentPath::root().join("child").unwrap(),
            ));
        }
        let recorder = server
            .store
            .create_thread_with(&CreateThreadParams::new(meta))
            .await
            .unwrap();
        record(&recorder, completed_run("old-turn", "Saved", "History")).await;
        recorder.shutdown().await.unwrap();
        let state = receipt(&server, &mut connection, id).await;
        assert_eq!(!state.is_null(), eligible, "{id}");
        if eligible {
            assert!(
                state["firstUnread"].is_null(),
                "reading historical results does not publish"
            );
            mark(&server, &mut connection, id, &state, "unread").await;
        } else {
            let error = server.call_error(&mut connection, "thread/readState/update", json!({"threadId": id, "expectedRevision": "any", "operation":{"type":"unread"}})).await;
            assert_eq!(error["message"], "thread read state unavailable");
        }
    }
    for db_only in [false, true] {
        let page = server
            .call(
                &mut connection,
                "thread/list",
                json!({"useStateDbOnly": db_only}),
            )
            .await;
        let states = page["readStates"].as_object().unwrap();
        assert_eq!(states.len(), 2);
        assert!(states.contains_key("user") && states.contains_key("unset_scope"));
    }
    // A spawned thread's lineage takes precedence over a contradictory raw root source.
    for (kind, expected) in [
        ("cli", "unset_scope"),
        ("exec", "feature"),
        ("unknown", "unknown"),
        ("subAgentThreadSpawn", "spawned"),
    ] {
        let page = server
            .call(
                &mut connection,
                "thread/list",
                json!({"sourceKinds":[kind], "useStateDbOnly":true}),
            )
            .await;
        assert_eq!(page["data"].as_array().unwrap().len(), 1, "{kind}");
        assert_eq!(page["data"][0]["id"], expected, "{kind}");
    }
    let before = db
        .thread_read_states(&[SessionId::new("feature")])
        .await
        .unwrap();
    server
        .resume(&mut connection, ThreadResumeParams::new("feature"))
        .await;
    script.answer("Feature result");
    server
        .run_turn(&mut connection, "feature", "Continue")
        .await;
    assert_eq!(
        db.thread_read_states(&[SessionId::new("feature")])
            .await
            .unwrap(),
        before,
        "live feature scope must remain excluded after resume"
    );
    let ephemeral = server
        .start_thread(
            &mut connection,
            ThreadStartParams::new().with_ephemeral(true),
        )
        .await;
    assert_eq!(
        receipt(&server, &mut connection, ephemeral.id()).await,
        Value::Null
    );
    let error = server
        .call_error(
            &mut connection,
            "thread/readState/update",
            json!({"threadId":ephemeral.id(),"expectedRevision":"any","operation":{"type":"read"}}),
        )
        .await;
    assert_eq!(error["message"], "thread read state unavailable");
    let user = receipt(&server, &mut connection, "user").await;
    let result = db
        .update_thread_read_state(
            &SessionId::new("user"),
            user["revision"].as_str().unwrap(),
            ReadStateOperation::Read,
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ra_session::store::local::ReadStateUpdate::Applied(_)
    ));
    server
        .call(
            &mut connection,
            "thread/archive",
            json!({"threadId":"user"}),
        )
        .await;
    let archived = server
        .call(
            &mut connection,
            "thread/list",
            json!({"archived":true,"useStateDbOnly":true}),
        )
        .await;
    assert!(archived["readStates"].get("user").is_some());
}

#[tokio::test]
async fn failed_storage_returns_null_snapshots_and_never_announces_a_failed_edit() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, db) = durable_server(directory.path(), &script).await;
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("Ready");
    server
        .run_turn(&mut connection, thread.id(), "Finish")
        .await;
    connection.notification("thread/readState/changed").await;
    let before = receipt(&server, &mut connection, thread.id()).await;
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON thread_read_receipts BEGIN SELECT RAISE(FAIL, 'receipt write failed'); END;").unwrap();
    let error = server.call_error(&mut connection, "thread/readState/update", json!({"threadId":thread.id(),"expectedRevision":before["revision"],"operation":{"type":"read"}})).await;
    assert_eq!(error["code"], -32603);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("read state was not saved:")
    );
    assert_eq!(receipt(&server, &mut connection, thread.id()).await, before);
    assert!(
        !connection
            .drain()
            .await
            .iter()
            .any(|message| message["method"] == "thread/readState/changed")
    );
    sql.execute_batch("DROP TRIGGER fail_receipt; DROP TABLE thread_read_receipts;")
        .unwrap();
    let read = server
        .call(
            &mut connection,
            "thread/read",
            json!({"threadId":thread.id()}),
        )
        .await;
    assert_eq!(read.get("readState"), Some(&Value::Null));
    let page = server
        .call(
            &mut connection,
            "thread/list",
            json!({"useStateDbOnly":true}),
        )
        .await;
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page.get("readStates"), Some(&Value::Null));
}

#[tokio::test]
async fn available_empty_pages_and_excluded_threads_have_bounded_receipt_maps() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, _) = durable_server(directory.path(), &script).await;
    let mut connection = server.connect(1).await;
    assert_eq!(
        server.call(&mut connection, "thread/list", json!({})).await["readStates"],
        json!({})
    );
    let mut ids = Vec::new();
    for _ in 0..3 {
        let thread = server
            .start_thread(&mut connection, ThreadStartParams::new())
            .await;
        script.answer("Ready");
        server
            .run_turn(&mut connection, thread.id(), "Finish")
            .await;
        ids.push(thread.id().to_owned());
    }
    let page = server
        .call(
            &mut connection,
            "thread/list",
            json!({"limit":1,"excludedThreadIds":[ids[2]],"useStateDbOnly":true}),
        )
        .await;
    let id = page["data"][0]["id"].as_str().unwrap();
    assert_ne!(id, ids[2]);
    assert_eq!(page["readStates"].as_object().unwrap().len(), 1);
    assert_eq!(
        page["readStates"][id],
        receipt(&server, &mut connection, id).await
    );
    let empty = server
        .call(
            &mut connection,
            "thread/list",
            json!({"excludedThreadIds":ids}),
        )
        .await;
    assert_eq!(empty["data"], json!([]));
    assert_eq!(empty["readStates"], json!({}));
}

/// Startup configuration is shared by thread creation, durable metadata, listing and publication.
#[tokio::test]
async fn server_session_source_defaults_and_overrides_reach_storage_filters_and_receipts() {
    use ra_protocol::server::{InternalSessionSource, SessionSource, SubAgentSource};
    let cases = [
        (None, json!("vscode"), Some("vscode"), true),
        (Some(SessionSource::Cli), json!("cli"), Some("cli"), true),
        (Some(SessionSource::Exec), json!("exec"), Some("exec"), true),
        (
            Some(SessionSource::Mcp),
            json!("appServer"),
            Some("appServer"),
            true,
        ),
        (
            Some(SessionSource::Custom("Rusty-Agent".into())),
            json!({"custom":"Rusty-Agent"}),
            None,
            true,
        ),
        (
            Some(SessionSource::Unknown),
            json!("unknown"),
            Some("unknown"),
            false,
        ),
        (
            Some(SessionSource::Internal(InternalSessionSource::Guardian)),
            json!("unknown"),
            None,
            false,
        ),
        (
            Some(SessionSource::SubAgent(SubAgentSource::Review)),
            json!({"subAgent":"review"}),
            Some("subAgentReview"),
            false,
        ),
    ];
    for (source, api_source, filter, eligible) in cases {
        let directory = tempfile::tempdir().unwrap();
        let script = Arc::new(Script::default());
        let metadata_source = serde_json::to_value(source.clone().unwrap_or_default()).unwrap();
        let (server, db) = durable_server_with_source(directory.path(), &script, source).await;
        let mut connection = server.connect(1).await;
        let thread = server
            .start_thread(&mut connection, ThreadStartParams::new())
            .await;
        assert_eq!(thread.source(), &api_source);
        script.answer("Ready");
        server
            .run_turn(&mut connection, thread.id(), "Finish")
            .await;
        let read = server
            .call(
                &mut connection,
                "thread/read",
                json!({"threadId":thread.id()}),
            )
            .await;
        assert_eq!(read["thread"]["source"], api_source);
        assert_eq!(!read["readState"].is_null(), eligible, "{metadata_source}");
        let stored = server
            .store
            .read_thread(&ReadThreadParams::new(SessionId::new(thread.id())))
            .await
            .unwrap();
        assert_eq!(stored.source(), Some(&metadata_source));
        let state = db
            .thread_read_states(&[SessionId::new(thread.id())])
            .await
            .unwrap();
        assert_eq!(
            state[&SessionId::new(thread.id())]
                .first_unread_turn()
                .is_some(),
            eligible,
            "{metadata_source}"
        );
        if eligible {
            assert_eq!(
                connection.notification("thread/readState/changed").await["readState"],
                read["readState"]
            );
        } else {
            assert!(
                !connection
                    .passed
                    .iter()
                    .any(|message| message["method"] == "thread/readState/changed")
            );
        }
        for db_only in [false, true] {
            let page = server
                .call(
                    &mut connection,
                    "thread/list",
                    json!({"useStateDbOnly":db_only}),
                )
                .await;
            assert_eq!(
                page["data"].as_array().unwrap().len(),
                1,
                "existing default-list behavior stays unchanged"
            );
            assert_eq!(page["data"][0]["source"], api_source);
            assert_eq!(
                page["readStates"]
                    .as_object()
                    .unwrap()
                    .contains_key(thread.id()),
                eligible
            );
            let kind = filter.unwrap_or("vscode");
            let filtered = server
                .call(
                    &mut connection,
                    "thread/list",
                    json!({"sourceKinds":[kind], "useStateDbOnly":db_only}),
                )
                .await;
            assert_eq!(
                filtered["data"].as_array().unwrap().len(),
                usize::from(filter.is_some()),
                "{metadata_source}"
            );
        }
        server
            .call(
                &mut connection,
                "thread/delete",
                json!({"threadId":thread.id()}),
            )
            .await;
    }
}

#[tokio::test]
async fn server_source_changes_apply_to_new_forks_and_preserve_resumed_source() {
    use ra_protocol::server::SessionSource;
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, _) =
        durable_server_with_source(directory.path(), &script, Some(SessionSource::Mcp)).await;
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let id = thread.id().to_owned();
    script.answer("Ready");
    server.run_turn(&mut connection, &id, "Finish").await;
    let before = receipt(&server, &mut connection, &id).await;
    server
        .call(
            &mut connection,
            "thread/unsubscribe",
            json!({"threadId":id}),
        )
        .await;
    connection.notification("thread/closed").await;
    server._task.abort();

    let (server, _) = durable_server(directory.path(), &script).await;
    let mut connection = server.connect(1).await;
    let resumed = server
        .resume(&mut connection, ThreadResumeParams::new(&id))
        .await;
    assert_eq!(resumed.thread().source(), &json!("appServer"));
    assert_eq!(receipt(&server, &mut connection, &id).await, before);
    script.answer("Later");
    server.run_turn(&mut connection, &id, "Continue").await;
    assert_ne!(
        receipt(&server, &mut connection, &id).await["revision"],
        before["revision"]
    );
    assert_eq!(
        server
            .read(&mut connection, ThreadReadParams::new(&id))
            .await
            .source(),
        &json!("appServer")
    );

    let fork = server
        .fork(&mut connection, ThreadForkParams::new(&id))
        .await;
    assert_eq!(fork.thread().source(), &json!("vscode"));
    let stored = server
        .store
        .read_thread(&ReadThreadParams::new(SessionId::new(fork.thread().id())))
        .await
        .unwrap();
    assert_eq!(stored.source(), Some(&json!("vscode")));
    let ephemeral = server
        .fork(&mut connection, ThreadForkParams::new(&id).ephemeral())
        .await;
    assert_eq!(ephemeral.thread().source(), &json!("vscode"));
    assert_eq!(
        receipt(&server, &mut connection, ephemeral.thread().id()).await,
        Value::Null
    );
}
