// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product cache routing through real server start/resume/fork entry points.
use super::*;
use ra_core::{agent::control::AgentPath, session::rollout::RolloutThreadSpawn};
use ra_protocol::server::{InternalSessionSource, SessionSource, SubAgentSource};

async fn coding_server() -> (Server, Arc<Script>, tempfile::TempDir) {
    coding_server_with_source(None).await
}

async fn coding_server_with_source(
    source: Option<SessionSource>,
) -> (Server, Arc<Script>, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let host = CodingHost::open(&root).unwrap();
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap();
    let script = Arc::new(Script::default());
    let factory = CodingThreadFactory::new(host, Arc::new(Resolver(script.clone())), settings);
    let (server, _) = build_with_session_source(
        Options {
            factory: Some(Arc::new(factory)),
            store: Some(Arc::new(InMemoryThreadStore::new())),
            unload_delay: None,
        },
        None,
        source,
    );
    (server, script, workspace)
}

#[tokio::test]
async fn coding_roots_remain_stable_across_turns_and_forks_choose_their_codex_keys() {
    let (server, script, _workspace) = coding_server().await;
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let root = started["thread"]["id"].as_str().unwrap();
    for text in ["first", "second"] {
        script.answer("done");
        server.run_turn(&mut connection, root, text).await;
        assert_eq!(script.last_request().prompt_cache_key(), Some(root));
    }
    for ephemeral in [false, true] {
        let params = ThreadForkParams::new(root);
        let fork = server
            .fork(
                &mut connection,
                if ephemeral {
                    params.ephemeral()
                } else {
                    params
                },
            )
            .await;
        script.answer("done");
        server
            .run_turn(&mut connection, fork.thread().id(), "fork")
            .await;
        assert_eq!(
            script.last_request().prompt_cache_key(),
            Some(if ephemeral { root } else { fork.thread().id() })
        );
    }
    server.store_thread("stored", "history").await;
    server
        .resume(&mut connection, ThreadResumeParams::new("stored"))
        .await;
    script.answer("done");
    server.run_turn(&mut connection, "stored", "resume").await;
    assert_eq!(script.last_request().prompt_cache_key(), Some("stored"));
}

#[tokio::test]
async fn resumed_spawn_and_ephemeral_fork_of_spawn_use_the_root_session() {
    let (server, script, _workspace) = coding_server().await;
    let spawn = RolloutThreadSpawn::new(
        SessionId::new("root-cache"),
        SessionId::new("parent"),
        2,
        AgentPath::root().join("child").unwrap(),
    );
    let recorder = server
        .store
        .create_thread_with(&CreateThreadParams::new(
            RolloutSessionMeta::new(SessionId::new("child")).with_thread_spawn(spawn),
        ))
        .await
        .unwrap();
    record(&recorder, completed_run("saved", "history", "Done")).await;
    let mut connection = server.connect(1).await;
    server
        .resume(&mut connection, ThreadResumeParams::new("child"))
        .await;
    script.answer("done");
    server.run_turn(&mut connection, "child", "resume").await;
    assert_eq!(script.last_request().prompt_cache_key(), Some("root-cache"));
    let fork = server
        .fork(&mut connection, ThreadForkParams::new("child").ephemeral())
        .await;
    script.answer("done");
    server
        .run_turn(&mut connection, fork.thread().id(), "fork")
        .await;
    assert_eq!(script.last_request().prompt_cache_key(), Some("root-cache"));
}

#[tokio::test]
async fn factory_receives_server_and_persisted_session_context() {
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let (server, _) = build_with_session_source(
        Options {
            factory: Some(factory.clone()),
            store: Some(Arc::new(InMemoryThreadStore::new())),
            unload_delay: None,
        },
        None,
        Some(SessionSource::Internal(
            InternalSessionSource::MemoryConsolidation,
        )),
    );
    let mut connection = server.connect(1).await;
    server
        .call(&mut connection, "thread/start", json!({"ephemeral":true}))
        .await;
    {
        let requests = factory.requests.lock().unwrap();
        assert_eq!(
            requests[0].session_source(),
            &SessionSource::Internal(InternalSessionSource::MemoryConsolidation)
        );
        assert!(requests[0].ephemeral());
        assert!(requests[0].thread_spawn().is_none());
    }
    server.store_thread("stored", "history").await;
    server
        .resume(&mut connection, ThreadResumeParams::new("stored"))
        .await;
    server
        .fork(&mut connection, ThreadForkParams::new("stored").ephemeral())
        .await;
    let requests = factory.requests.lock().unwrap();
    assert!(!requests[1].ephemeral());
    assert_eq!(
        requests[2].fork_session_id(),
        Some(&SessionId::new("stored"))
    );
    assert!(requests[2].ephemeral());
}

#[tokio::test]
async fn non_root_sources_do_not_reuse_the_ephemeral_fork_source_key() {
    for source in [
        SessionSource::SubAgent(SubAgentSource::Review),
        SessionSource::Internal(InternalSessionSource::Guardian),
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
    ] {
        let (server, script, _workspace) = coding_server_with_source(Some(source)).await;
        server.store_thread("source", "history").await;
        let mut connection = server.connect(1).await;
        let fork = server
            .fork(&mut connection, ThreadForkParams::new("source").ephemeral())
            .await;
        script.answer("done");
        server
            .run_turn(&mut connection, fork.thread().id(), "fork")
            .await;
        assert_eq!(
            script.last_request().prompt_cache_key(),
            Some(fork.thread().id())
        );
        assert_ne!(script.last_request().prompt_cache_key(), Some("source"));
    }
}
