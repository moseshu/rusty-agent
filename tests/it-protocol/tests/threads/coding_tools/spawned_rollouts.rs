// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! A spawned agent is a thread of its own: Codex's `ThreadSpawn` session, with its own rollout and
//! its own hooks (`core/tests/suite/subagent_notifications.rs`,
//! `app-server/tests/suite/v2/thread_list.rs`), through the configured coding factory and a rollout
//! directory, so transcripts are real files.

use super::*;
use ra_session::rollout::RolloutThreadDirectory;

const SUBAGENT_START_CONTEXT: &str = "subagent start context reaches child";

/// A hook command that logs its input to `log`, and, when the input names a transcript, whether
/// that file exists while the hook runs.
fn logging_hook(log: &std::path::Path) -> String {
    format!(
        r#"input=$(cat); printf '%s\n' "$input" >> '{log}'; for key in transcript_path agent_transcript_path; do file=$(printf '%s' "$input" | sed -n "s/.*\"$key\":\"\([^\"]*\)\".*/\1/p"); if [ -n "$file" ]; then if [ -f "$file" ]; then echo "$key exists" >> '{log}.files'; else echo "$key missing" >> '{log}.files'; fi; fi; done"#,
        log = log.display()
    )
}

fn hook_section(event: &str, matcher: Option<&str>, command: &str) -> String {
    let matcher = matcher.map_or_else(String::new, |matcher| format!("matcher = '{matcher}'\n"));
    format!(
        "[[hooks.{event}]]\n{matcher}[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = '''{command}'''\n"
    )
}

fn read_log(log: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn read_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn eventually(what: &str, done: impl Fn() -> bool) {
    timeout(PATIENCE, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

struct Hooked {
    server: Server,
    connection: Connection,
    script: Arc<Script>,
    agents: Arc<Agents>,
    store: Arc<RolloutThreadDirectory>,
    home: std::path::PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

impl Hooked {
    fn log(&self, name: &str) -> std::path::PathBuf {
        self.home.join(format!("{name}.jsonl"))
    }
}

/// A server whose threads are the configured coding agent's, on a rollout directory, with the hooks
/// `sections` configure, every lifecycle event also logged.
async fn hooked(extra: &[String]) -> Hooked {
    hooked_with(extra, None).await
}

async fn hooked_with(
    extra: &[String],
    faults: Option<Arc<ra_session::rollout::faults::RolloutFaults>>,
) -> Hooked {
    hooked_unloading_after(extra, faults, Duration::ZERO, None, false).await
}

/// A hooked fixture whose server unloads a thread `unload_delay` after its last subscriber left,
/// and whose `thread/resume` waits `close_timeout` for a thread's close if one is given.
async fn hooked_unloading_after(
    extra: &[String],
    faults: Option<Arc<ra_session::rollout::faults::RolloutFaults>>,
    unload_delay: Duration,
    close_timeout: Option<Duration>,
    spawned_collaboration: bool,
) -> Hooked {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path().canonicalize().unwrap();
    let rollouts = tempfile::tempdir().unwrap();
    let mut sections = Vec::new();
    for event in [
        "SessionStart",
        "SubagentStart",
        "SubagentStop",
        "Stop",
        "SessionEnd",
        "Interrupt",
        "PreToolUse",
    ] {
        sections.push(hook_section(
            event,
            None,
            &logging_hook(&home.join(format!("{event}.jsonl"))),
        ));
    }
    sections.extend(extra.iter().cloned());
    std::fs::write(
        home.join("config.toml"),
        format!(
            "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n\n[features]\ncodex_hooks = true\nmulti_agent_v2 = true\n\n{}",
            sections.join("\n")
        ),
    )
    .unwrap();
    let builder = ra_coding::config::ConfigBuilder::default()
        .codex_home(home.clone())
        .loader_overrides(ra_config::LoaderOverrides {
            managed_config_path: Some(home.join("managed_config.toml")),
            system_config_path: Some(home.join("system_config.toml")),
            system_requirements_path: Some(home.join("requirements.toml")),
            ..ra_config::LoaderOverrides::without_managed_config_for_tests()
        })
        .harness_overrides(ra_coding::config::ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..ra_coding::config::ConfigOverrides::default()
        });
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    let services = Arc::new(configured_host::Services::default());
    let mut host = services.install(CodingHost::open(&root).unwrap());
    if spawned_collaboration {
        host = host.with_spawned_agent_collaboration();
    }
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap()
    .with_rules_home(&home);
    let factory = CodingThreadFactory::new(
        host,
        Arc::new(AgentsResolver(Arc::clone(&agents))),
        settings,
    )
    .with_profile(ra_coding::CodingProfile::CodexLike)
    .with_request_user_input()
    .with_product_config(builder);
    let mut directory = RolloutThreadDirectory::new(rollouts.path());
    if let Some(faults) = faults {
        directory = directory.with_faults(faults);
    }
    let store = Arc::new(directory);
    let (server, _) = build_server(
        Options {
            store: Some(store.clone()),
            factory: Some(Arc::new(factory)),
            unload_delay: Some(unload_delay),
        },
        None,
        None,
        None,
        &|server| {
            if let Some(timeout) = close_timeout {
                ra_protocol::server::test_api::set_thread_close_timeout(server, timeout);
            }
        },
    );
    let connection = server.connect(1).await;
    Hooked {
        server,
        connection,
        script,
        agents,
        store,
        home,
        _dirs: vec![workspace, home_dir, rollouts],
    }
}

/// Starts a thread whose turn spawns the worker with `spawn`'s extra arguments, waits for it and
/// ends; returns the thread's id once the worker has answered.
async fn delegate(fixture: &mut Hooked, spawn: Value) -> String {
    delegate_with_params(fixture, spawn, ThreadStartParams::new()).await
}

async fn delegate_with_params(
    fixture: &mut Hooked,
    spawn: Value,
    params: ThreadStartParams,
) -> String {
    let thread = fixture
        .server
        .start_thread(&mut fixture.connection, params)
        .await;
    let mut arguments =
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"});
    if let (Some(arguments), Some(extra)) = (arguments.as_object_mut(), spawn.as_object()) {
        arguments.extend(extra.clone());
    }
    fixture
        .script
        .push(Step::Respond(collab("spawn_agent", arguments)));
    fixture.script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms": 10000}),
    )));
    fixture.script.answer("delegated");
    fixture
        .server
        .run_turn(&mut fixture.connection, thread.id(), "delegate")
        .await;
    fixture.agents.wait_for_child_calls(1).await;
    thread.id().to_owned()
}

async fn spawned_children(
    store: &RolloutThreadDirectory,
    parent: &str,
) -> Vec<ra_session::RolloutSessionMeta> {
    store
        .children(&SessionId::new(parent))
        .await
        .unwrap()
        .into_iter()
        .map(|(_, meta)| meta)
        .collect()
}

async fn list(fixture: &mut Hooked, params: Value) -> Vec<String> {
    fixture
        .server
        .call(&mut fixture.connection, "thread/list", params)
        .await["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn multi_agent_child_delegation_requires_catalog_v2() {
    for (model, expected) in [("mock-model", false), ("catalog-child", true)] {
        let mut fixture = hooked(&[]).await;
        let catalog = fixture.home.join("collaboration-models.json");
        let entry = crate::product_models::multi_agent::catalog_model("catalog-child", Some("v2"));
        std::fs::write(&catalog, json!({"models": [entry]}).to_string()).unwrap();
        let config_path = fixture.home.join("config.toml");
        let config = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(
            &config_path,
            format!("model_catalog_json = {catalog:?}\n{config}"),
        )
        .unwrap();
        delegate_with_params(
            &mut fixture,
            json!({}),
            ThreadStartParams::new().with_model(model),
        )
        .await;

        let requests = fixture.agents.child_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .tools()
                .iter()
                .any(|tool| tool.namespace() == Some(COLLABORATION) && tool.name() == "spawn_agent"),
            expected,
        );
    }
}

#[tokio::test]
async fn multi_agent_legacy_host_with_collaboration_migrates_to_v2() {
    let mut fixture = hooked(&[]).await;
    let config_path = fixture.home.join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, config.replace("multi_agent_v2 = true", "")).unwrap();
    let id = "legacy-host-collaboration";
    let recorder = fixture.server.store_thread(id, "old turn").await;
    recorder.shutdown().await.unwrap();

    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": id}),
        )
        .await;
    fixture.script.answer("Done");
    fixture
        .server
        .run_turn(&mut fixture.connection, id, "Continue")
        .await;
    assert!(
        fixture.script.last_request().tools().iter().any(|tool| {
            tool.namespace() == Some(COLLABORATION) && tool.name() == "spawn_agent"
        })
    );
    unload(&fixture.server, &mut fixture.connection, id).await;
    let rollout =
        std::fs::read_to_string(fixture.store.rollout_path(&SessionId::new(id)).unwrap()).unwrap();
    assert!(
        rollout.contains(r#""multi_agent_version":"v2""#),
        "{rollout}"
    );
}

/// Migration settles a thread's own tools only. Its children follow Codex's V2 rule whatever the
/// host's spawned-agent setting: a child whose model declares V2 gains the tools without it, and a
/// child whose model declares nothing loses them with it. The rule holds across a second cold
/// resume, an empty fork, and a child resumed on its own.
#[tokio::test]
async fn multi_agent_migrated_thread_children_follow_catalog_v2() {
    for (host_spawned, model) in [(false, "catalog-child"), (true, "mock-model")] {
        let expected = model == "catalog-child";
        let mut fixture =
            hooked_unloading_after(&[], None, Duration::ZERO, None, host_spawned).await;
        let catalog = fixture.home.join("collaboration-models.json");
        let entry = crate::product_models::multi_agent::catalog_model("catalog-child", Some("v2"));
        std::fs::write(&catalog, json!({"models": [entry]}).to_string()).unwrap();
        let config_path = fixture.home.join("config.toml");
        let config = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(
            &config_path,
            format!(
                "model_catalog_json = {catalog:?}\n{}",
                config.replace("multi_agent_v2 = true", "")
            ),
        )
        .unwrap();
        let id = "legacy-host-children";
        let recorder = fixture.server.store_thread(id, "old turn").await;
        recorder.shutdown().await.unwrap();

        for call in 1..=2 {
            fixture
                .server
                .call(
                    &mut fixture.connection,
                    "thread/resume",
                    json!({"threadId": id, "model": model}),
                )
                .await;
            assert_child_delegation(&mut fixture, id, call, expected).await;
            unload(&fixture.server, &mut fixture.connection, id).await;
        }

        let fork = fixture
            .server
            .call(
                &mut fixture.connection,
                "thread/fork",
                json!({
                    "threadId": id, "beforeTurnId": "turn-1", "model": model,
                    "approvalPolicy": "never", "approvalsReviewer": "user", "sandbox": "read-only",
                }),
            )
            .await;
        assert_eq!(fork["thread"]["turns"], json!([]));
        let fork_id = fork["thread"]["id"].as_str().unwrap();
        unload(&fixture.server, &mut fixture.connection, fork_id).await;
        fixture
            .server
            .call(
                &mut fixture.connection,
                "thread/resume",
                json!({"threadId": fork_id, "model": model}),
            )
            .await;
        assert_child_delegation(&mut fixture, fork_id, 3, expected).await;
        unload(&fixture.server, &mut fixture.connection, fork_id).await;

        let children = spawned_children(&fixture.store, fork_id).await;
        assert_eq!(children.len(), 1);
        let child_id = children[0].session_id();
        let child = std::fs::read_to_string(fixture.store.rollout_path(child_id).unwrap()).unwrap();
        assert!(child.contains(r#""multi_agent_version":"v2""#), "{child}");
        fixture
            .server
            .call(
                &mut fixture.connection,
                "thread/resume",
                json!({"threadId": child_id, "model": model}),
            )
            .await;
        fixture
            .server
            .run_turn(
                &mut fixture.connection,
                child_id.as_str(),
                "continue worker task",
            )
            .await;
        assert_child_tools(&fixture, 4, expected).await;
    }
}

async fn assert_child_delegation(fixture: &mut Hooked, id: &str, call: usize, expected: bool) {
    fixture.script.push(Step::Respond(collab(
        "spawn_agent",
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
    )));
    fixture.script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms": 10000}),
    )));
    fixture.script.answer("delegated");
    fixture
        .server
        .run_turn(&mut fixture.connection, id, "delegate")
        .await;
    assert!(
        fixture
            .script
            .last_request()
            .tools()
            .iter()
            .any(|tool| tool.namespace() == Some(COLLABORATION))
    );
    assert_child_tools(fixture, call, expected).await;
}

async fn assert_child_tools(fixture: &Hooked, call: usize, expected: bool) {
    fixture.agents.wait_for_child_calls(call).await;
    let requests = fixture.agents.child_requests.lock().unwrap();
    assert_eq!(requests.len(), call);
    assert_eq!(
        requests[call - 1]
            .tools()
            .iter()
            .any(|tool| tool.namespace() == Some(COLLABORATION)),
        expected,
        "child request {call}",
    );
}

/// A product tree runs `max_concurrent_threads_per_session - 1` spawned agents at once, as Codex's
/// `effective_agent_max_threads` does; one thread per session leaves no room for a child.
#[tokio::test]
async fn multi_agent_v2_session_thread_limit_bounds_spawned_agents() {
    let mut fixture = hooked(&[]).await;
    let config_path = fixture.home.join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        format!(
            "{}\n[features.multi_agent_v2]\nenabled = true\nmax_concurrent_threads_per_session = 1\n",
            config.replace("multi_agent_v2 = true", "")
        ),
    )
    .unwrap();
    let thread = fixture
        .server
        .start_thread(&mut fixture.connection, ThreadStartParams::new())
        .await;
    fixture.script.push(Step::Respond(collab(
        "spawn_agent",
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
    )));
    fixture.script.answer("not delegated");
    fixture
        .server
        .run_turn(&mut fixture.connection, thread.id(), "delegate")
        .await;
    let last = format!("{:?}", fixture.script.last_request());
    assert!(last.contains("agent thread limit reached"), "{last}");
    assert!(fixture.agents.child_requests.lock().unwrap().is_empty());
}

/// The worker records into a rollout of its own, created through the server's store with Codex's
/// `ThreadSpawn` metadata, and the default `thread/list` leaves it out.
#[tokio::test]
async fn a_spawned_agent_records_its_own_rollout() {
    let mut fixture = hooked(&[]).await;
    let parent = delegate(&mut fixture, json!({})).await;
    unload(&fixture.server, &mut fixture.connection, &parent).await;

    let children = spawned_children(&fixture.store, &parent).await;
    assert_eq!(children.len(), 1, "{children:?}");
    let spawn = children[0].thread_spawn().unwrap();
    assert_eq!(spawn.parent_session_id().as_str(), parent);
    assert_eq!(spawn.root_session_id().as_str(), parent);
    assert_eq!(spawn.depth(), 1);
    assert_eq!(spawn.agent_path().name(), "worker");
    let child = children[0].session_id().as_str().to_owned();
    let child_rollout =
        std::fs::read_to_string(fixture.store.rollout_path(&SessionId::new(&child)).unwrap())
            .unwrap();
    assert!(child_rollout.contains("worker answer"), "{child_rollout}");

    // The default listing leaves spawned threads out. The worker, whose input is mail rather than a
    // user message, is not listed by any active listing of a rollout directory, as Codex's file
    // listing skips a thread without a user message; the filter rules are covered with stored
    // threads that have one.
    assert_eq!(list(&mut fixture, json!({})).await, [parent]);
}

/// Ports `subagent_start_replaces_session_start_and_injects_context` and
/// `subagent_stop_replaces_stop_and_skips_internal_subagents` for the v2 tools: the worker runs
/// `SubagentStart` once, as its own session, with context that reaches only its own request, and
/// ends with `SubagentStop` naming the parent's transcript and its own, both on disk while the hook
/// runs; the parent's `SessionStart` runs once and no `Stop` runs for the worker.
#[tokio::test]
async fn a_spawned_agent_runs_its_own_start_and_stop_hooks() {
    let start_context = format!(
        "printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"SubagentStart\",\"additionalContext\":\"{SUBAGENT_START_CONTEXT}\"}}}}'"
    );
    let mut fixture = hooked(&[hook_section("SubagentStart", None, &start_context)]).await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker's turn ended with SubagentStop", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    let children = spawned_children(&fixture.store, &parent).await;
    let child = children[0].session_id().as_str().to_owned();

    let starts = read_log(&fixture.log("SubagentStart"));
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0]["agent_type"], "default");
    assert_eq!(starts[0]["agent_id"], child);
    assert_eq!(starts[0]["session_id"], child);
    assert_eq!(
        read_lines(&fixture.home.join("SubagentStart.jsonl.files")),
        ["transcript_path exists"]
    );
    let child_requests = fixture.agents.child_requests.lock().unwrap().clone();
    assert!(
        serde_json::to_string(child_requests[0].input())
            .unwrap()
            .contains(SUBAGENT_START_CONTEXT)
    );
    assert!(
        !serde_json::to_string(fixture.script.last_request().input())
            .unwrap()
            .contains(SUBAGENT_START_CONTEXT)
    );

    let session_starts = read_log(&fixture.log("SessionStart"));
    assert_eq!(session_starts.len(), 1, "{session_starts:?}");
    assert_eq!(session_starts[0]["source"], "startup");
    assert_eq!(session_starts[0]["session_id"], parent);

    let stops = read_log(&fixture.log("SubagentStop"));
    assert_eq!(stops.len(), 1, "{stops:?}");
    let parent_path = fixture
        .store
        .rollout_path(&SessionId::new(&parent))
        .unwrap();
    let child_path = fixture.store.rollout_path(&SessionId::new(&child)).unwrap();
    assert_eq!(stops[0]["transcript_path"], parent_path.to_str().unwrap());
    assert_eq!(
        stops[0]["agent_transcript_path"],
        child_path.to_str().unwrap()
    );
    assert_eq!(stops[0]["agent_id"], child);
    assert_eq!(stops[0]["session_id"], child);
    assert_eq!(stops[0]["last_assistant_message"], "worker answer");
    assert_eq!(
        read_lines(&fixture.home.join("SubagentStop.jsonl.files")),
        ["transcript_path exists", "agent_transcript_path exists"]
    );
    assert!(
        read_log(&fixture.log("Stop"))
            .iter()
            .all(|stop| stop["session_id"] == parent.as_str()
                && stop["last_assistant_message"] != "worker answer")
    );

    // Woken again by mail, the worker starts nothing more; its turn ends with SubagentStop again.
    fixture
        .agents
        .children
        .lock()
        .unwrap()
        .push_back(final_message("worker again"));
    fixture.script.push(Step::Respond(collab(
        "followup_task",
        json!({"target": "worker", "message": "worker task again"}),
    )));
    fixture.script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms": 10000}),
    )));
    fixture.script.answer("delegated again");
    fixture
        .server
        .run_turn(&mut fixture.connection, &parent, "again")
        .await;
    eventually("the worker's second turn ended", || {
        read_log(&fixture.log("SubagentStop")).len() == 2
    })
    .await;
    assert_eq!(read_log(&fixture.log("SubagentStart")).len(), 1);
    unload(&fixture.server, &mut fixture.connection, &parent).await;
    let ends = read_log(&fixture.log("SessionEnd"));
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["session_id"], parent);
}

/// The worker's tool hooks name its own session and carry Codex's sub-agent fields; the parent's
/// carry none.
#[tokio::test]
async fn a_spawned_agents_tool_hooks_name_its_session() {
    let mut fixture = hooked(&[]).await;
    fixture
        .agents
        .children
        .lock()
        .unwrap()
        .push_back(call("exec_command", json!({"cmd": "echo child"})));
    let parent = delegate(&mut fixture, json!({"agent_type": null})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    let child = spawned_children(&fixture.store, &parent).await[0]
        .session_id()
        .as_str()
        .to_owned();
    let pre = read_log(&fixture.log("PreToolUse"));
    let of_child: Vec<_> = pre
        .iter()
        .filter(|input| input["session_id"] == child.as_str())
        .collect();
    assert!(!of_child.is_empty(), "{pre:?}");
    for input in of_child {
        assert_eq!(input["agent_id"], child);
        assert_eq!(input["agent_type"], "default");
    }
    let of_parent: Vec<_> = pre
        .iter()
        .filter(|input| input["session_id"] == parent.as_str())
        .collect();
    assert!(!of_parent.is_empty(), "{pre:?}");
    for input in of_parent {
        assert!(input.get("agent_id").is_none(), "{input}");
        assert!(input.get("agent_type").is_none(), "{input}");
    }
}

/// An ephemeral thread records nothing, its agents included, and their hooks run as their own
/// sessions with no transcript.
#[tokio::test]
async fn an_ephemeral_threads_agents_record_nothing() {
    let mut fixture = hooked(&[]).await;
    let thread = fixture
        .server
        .start_thread(
            &mut fixture.connection,
            ThreadStartParams::new().with_ephemeral(true),
        )
        .await;
    fixture.script.push(Step::Respond(collab(
        "spawn_agent",
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
    )));
    fixture.script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms": 10000}),
    )));
    fixture.script.answer("delegated");
    fixture
        .server
        .run_turn(&mut fixture.connection, thread.id(), "delegate")
        .await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    assert!(
        spawned_children(&fixture.store, thread.id())
            .await
            .is_empty()
    );
    let starts = read_log(&fixture.log("SubagentStart"));
    assert_eq!(starts.len(), 1);
    assert!(starts[0]["transcript_path"].is_null(), "{}", starts[0]);
    assert_ne!(starts[0]["session_id"], thread.id());
    assert_eq!(starts[0]["agent_id"], starts[0]["session_id"]);
    let stops = read_log(&fixture.log("SubagentStop"));
    assert!(stops[0]["transcript_path"].is_null(), "{}", stops[0]);
    assert!(stops[0]["agent_transcript_path"].is_null(), "{}", stops[0]);
}

/// Archiving or deleting the root takes the real spawned rollouts with it, and archiving the worker
/// by its own id closes it in the parent's live tree while the parent goes on.
#[tokio::test]
async fn archiving_and_deleting_reach_spawned_rollouts() {
    let mut fixture = hooked(&[]).await;
    let parent = delegate(&mut fixture, json!({})).await;
    let child = spawned_children(&fixture.store, &parent).await[0]
        .session_id()
        .as_str()
        .to_owned();
    let child_path = fixture.store.rollout_path(&SessionId::new(&child)).unwrap();

    // The worker is still in the parent's tree, holding its writer.
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/archive",
            json!({"threadId": child}),
        )
        .await;
    assert!(!child_path.exists());
    assert_eq!(
        list(
            &mut fixture,
            json!({"archived": true, "sourceKinds": ["subAgentThreadSpawn"]})
        )
        .await,
        std::slice::from_ref(&child)
    );
    fixture.script.answer("parent goes on");
    fixture
        .server
        .run_turn(&mut fixture.connection, &parent, "go on")
        .await;

    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/delete",
            json!({"threadId": parent}),
        )
        .await;
    assert!(spawned_children(&fixture.store, &parent).await.is_empty());
    assert!(
        list(
            &mut fixture,
            json!({"archived": true, "sourceKinds": ["subAgentThreadSpawn"]})
        )
        .await
        .is_empty()
    );
}

/// A worker closed with its parent and then resumed on its own keeps its spawned identity, as
/// Codex's resumed sub-agent session does: no `SessionStart`, its turns end with `SubagentStop`
/// naming the parent's transcript, and unloading it runs no `SessionEnd`.
#[tokio::test]
async fn a_resumed_spawned_thread_keeps_its_spawned_identity() {
    let mut fixture = hooked(&[]).await;
    let parent = delegate(&mut fixture, json!({})).await;
    unload(&fixture.server, &mut fixture.connection, &parent).await;
    let child = spawned_children(&fixture.store, &parent).await[0]
        .session_id()
        .as_str()
        .to_owned();
    let before = read_log(&fixture.log("SessionStart")).len();

    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": child}),
        )
        .await;
    fixture.script.answer("resumed answer");
    fixture
        .server
        .run_turn(&mut fixture.connection, &child, "continue alone")
        .await;
    eventually("the resumed turn ended with SubagentStop", || {
        read_log(&fixture.log("SubagentStop")).len() == 2
    })
    .await;
    assert_eq!(read_log(&fixture.log("SessionStart")).len(), before);
    let stop = read_log(&fixture.log("SubagentStop")).pop().unwrap();
    assert_eq!(stop["session_id"], child);
    assert_eq!(stop["agent_id"], child);
    assert_eq!(
        stop["transcript_path"],
        fixture
            .store
            .rollout_path(&SessionId::new(&parent))
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(
        stop["agent_transcript_path"],
        fixture
            .store
            .rollout_path(&SessionId::new(&child))
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert!(
        read_log(&fixture.log("Stop"))
            .iter()
            .all(|stop| stop["session_id"] != child.as_str())
    );
    let ends_before = read_log(&fixture.log("SessionEnd")).len();
    unload(&fixture.server, &mut fixture.connection, &child).await;
    assert_eq!(read_log(&fixture.log("SessionEnd")).len(), ends_before);
}

/// Codex restores the last sampling total from a sub-agent session's rollout after a restart.
#[tokio::test]
async fn a_resumed_spawned_thread_compacts_before_sampling_when_its_rollout_is_over_limit() {
    const MODEL: &str = "gpt-5.4";
    let mut fixture = hooked(&[]).await;
    fixture.agents.children.lock().unwrap().push_back(
        final_message("obsolete worker answer").with_usage(ra_core::usage::Usage::from_request(
            ra_core::usage::RequestUsage::new(5_000_000, 100),
        )),
    );
    let parent = delegate_with_params(
        &mut fixture,
        json!({}),
        ThreadStartParams::new().with_model(MODEL),
    )
    .await;
    unload(&fixture.server, &mut fixture.connection, &parent).await;
    let child = spawned_children(&fixture.store, &parent).await[0]
        .session_id()
        .as_str()
        .to_owned();
    let starts = read_log(&fixture.log("SessionStart")).len();

    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": child, "model": MODEL}),
        )
        .await;
    // The old task identifies the child to the mock during compaction. The new task identifies it
    // again once the compacted history replaces the old mail with a summary.
    fixture.agents.children.lock().unwrap().extend([
        final_message(
            &json!({
                "primary_request_and_intent": "intent",
                "key_technical_concepts": "concepts",
                "files_and_code_sections": "files",
                "errors_and_fixes": "errors",
                "problem_solving": "solutions",
                "pending_tasks": "tasks",
                "current_work": "cold worker summary",
                "optional_next_step": "continue"
            })
            .to_string(),
        ),
        final_message("cold worker answer"),
    ]);
    fixture
        .server
        .run_turn(
            &mut fixture.connection,
            &child,
            "continue worker task alone",
        )
        .await;

    let requests = fixture.agents.child_requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let before_compaction = serde_json::to_string(requests[1].input()).unwrap();
    assert!(before_compaction.contains("obsolete worker answer"));
    assert!(!before_compaction.contains("continue worker task alone"));
    let after_compaction = serde_json::to_string(requests[2].input()).unwrap();
    assert!(after_compaction.contains("cold worker summary"));
    assert!(after_compaction.contains("continue worker task alone"));
    assert!(!after_compaction.contains("obsolete worker answer"));
    assert_eq!(read_log(&fixture.log("SessionStart")).len(), starts);
}

/// When a spawned agent's rollout cannot be let go of, archiving its parent fails without moving
/// anything — though the parent's own writer was already closed — and asking again finishes the
/// close from where it stopped, without writing to the closed writer again.
#[tokio::test]
async fn an_archive_retry_finishes_a_close_that_left_a_child_writer_open() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let faults = RolloutFaults::new();
    let mut fixture = hooked_with(&[], Some(Arc::clone(&faults))).await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    let child = spawned_children(&fixture.store, &parent).await[0]
        .session_id()
        .as_str()
        .to_owned();
    faults.fail(RolloutFaultPoint::Shutdown, 1);
    fixture.connection.passed.clear();
    let error = fixture
        .server
        .call_error(
            &mut fixture.connection,
            "thread/archive",
            json!({"threadId": parent}),
        )
        .await;
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("nothing was changed"),
        "{error}"
    );
    let parent_path = fixture
        .store
        .rollout_path(&SessionId::new(&parent))
        .unwrap();
    let child_path = fixture.store.rollout_path(&SessionId::new(&child)).unwrap();
    assert!(parent_path.exists() && child_path.exists());
    assert!(
        !fixture
            .connection
            .passed
            .iter()
            .any(|message| message["method"] == "thread/archived")
    );
    // The retry is refused nothing: the closed parent writer is not written to again.
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/archive",
            json!({"threadId": parent}),
        )
        .await;
    assert!(!parent_path.exists() && !child_path.exists());
    assert_eq!(
        faults.calls(RolloutFaultPoint::Shutdown),
        3,
        "the child's writer twice, the parent's once"
    );
}

/// An idle unload whose close leaves a spawned agent's writer open keeps the thread loaded, so a
/// later archive finds what is still open and finishes the close.
#[tokio::test]
async fn an_idle_unload_that_left_a_child_open_keeps_it_for_the_archive() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let faults = RolloutFaults::new();
    let mut fixture = hooked_with(&[], Some(Arc::clone(&faults))).await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    let child = spawned_children(&fixture.store, &parent).await[0]
        .session_id()
        .as_str()
        .to_owned();
    faults.fail(RolloutFaultPoint::Shutdown, 1);
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/unsubscribe",
            json!({"threadId": parent}),
        )
        .await;
    eventually("the idle unload tried to close everything", || {
        faults.calls(RolloutFaultPoint::Shutdown) == 2
    })
    .await;
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/archive",
            json!({"threadId": parent}),
        )
        .await;
    assert!(
        !fixture
            .store
            .rollout_path(&SessionId::new(&child))
            .unwrap()
            .exists()
    );
    assert_eq!(faults.calls(RolloutFaultPoint::Shutdown), 3);
}

/// Archiving a thread lets go of its agents' rollouts only after its own `SessionEnd` hooks have
/// run, as the design orders the close: nothing is shut down while the hook runs.
#[tokio::test]
async fn an_archive_closes_child_writers_after_the_session_end_hook() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let faults = RolloutFaults::new();
    let marker = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::remove_file(&marker).unwrap();
    let slow_end = format!(
        "echo started >> '{}'; sleep 1; cat > /dev/null",
        marker.display()
    );
    let mut fixture = hooked_with(
        &[hook_section("SessionEnd", None, &slow_end)],
        Some(Arc::clone(&faults)),
    )
    .await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    let request = fixture
        .server
        .request(
            &mut fixture.connection,
            "thread/archive",
            json!({"threadId": parent}),
        )
        .await;
    let marker_path = marker.to_path_buf();
    eventually("the SessionEnd hook started", || marker_path.exists()).await;
    assert_eq!(
        faults.calls(RolloutFaultPoint::Shutdown),
        0,
        "no writer is let go of while SessionEnd runs"
    );
    fixture.connection.response(request).await;
    assert_eq!(faults.calls(RolloutFaultPoint::Shutdown), 2);
    assert_eq!(read_log(&fixture.log("SessionEnd")).len(), 1);
}

/// A thread left loaded by an idle unload that could not close everything is not handed back as
/// it is: resuming it finishes the close and loads it again from the store, so its next turn runs.
#[tokio::test]
async fn resuming_after_a_failed_idle_unload_finishes_the_close_and_runs() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let faults = RolloutFaults::new();
    let mut fixture = hooked_with(&[], Some(Arc::clone(&faults))).await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    faults.fail(RolloutFaultPoint::Shutdown, 1);
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/unsubscribe",
            json!({"threadId": parent}),
        )
        .await;
    eventually("the idle unload tried to close everything", || {
        faults.calls(RolloutFaultPoint::Shutdown) == 2
    })
    .await;
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": parent}),
        )
        .await;
    assert_eq!(faults.calls(RolloutFaultPoint::Shutdown), 3);
    fixture.script.answer("after resume");
    fixture
        .server
        .run_turn(&mut fixture.connection, &parent, "go on")
        .await;
    assert!(
        serde_json::to_string(fixture.script.last_request().input())
            .unwrap()
            .contains("go on")
    );
}

#[tokio::test]
async fn resuming_with_other_settings_after_a_failed_close_reports_it_and_the_next_resume_runs() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let faults = RolloutFaults::new();
    let mut fixture = hooked_unloading_after(
        &[],
        Some(Arc::clone(&faults)),
        Duration::from_secs(3600),
        None,
        false,
    )
    .await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/unsubscribe",
            json!({"threadId": parent}),
        )
        .await;
    assert_eq!(faults.calls(RolloutFaultPoint::Shutdown), 0);
    // The other model makes the resume close the idle thread to reload it; one writer fails to
    // close, so the thread is shut down and cannot be handed back as it is.
    faults.fail(RolloutFaultPoint::Shutdown, 1);
    let refused = fixture
        .server
        .call_error(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": parent, "model": "another-model"}),
        )
        .await;
    assert_eq!(faults.calls(RolloutFaultPoint::Shutdown), 2);
    let message = refused["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("could not be closed yet"),
        "expected the unfinished close to be reported, got {refused}"
    );
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": parent}),
        )
        .await;
    assert_eq!(faults.calls(RolloutFaultPoint::Shutdown), 3);
    fixture.script.answer("after resume");
    fixture
        .server
        .run_turn(&mut fixture.connection, &parent, "go on")
        .await;
    assert!(
        serde_json::to_string(fixture.script.last_request().input())
            .unwrap()
            .contains("go on")
    );
}

/// A resume that meets a close still in progress waits for it no longer than a reload does, and
/// fails: it holds the lock every thread's start, resume and removal take, so a writer that does
/// not let go must not hold up every other thread. Once the writer lets go, a resume finishes the
/// close and the thread runs.
#[tokio::test]
async fn a_resume_waits_for_a_stuck_close_only_so_long_and_other_threads_go_on() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let faults = RolloutFaults::new();
    let mut fixture = hooked_unloading_after(
        &[],
        Some(Arc::clone(&faults)),
        Duration::from_secs(3600),
        Some(Duration::from_millis(300)),
        false,
    )
    .await;
    let parent = delegate(&mut fixture, json!({})).await;
    eventually("the worker finished", || {
        !read_log(&fixture.log("SubagentStop")).is_empty()
    })
    .await;
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/unsubscribe",
            json!({"threadId": parent}),
        )
        .await;
    let hold = faults.hold(RolloutFaultPoint::Shutdown);
    // The reload for another model starts the close, which a writer holds up.
    let refused = fixture
        .server
        .call_error(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": parent, "model": "another-model"}),
        )
        .await;
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|message| message.contains("timed out")),
        "{refused}"
    );
    // The next resume meets the same close still held up, and fails as well rather than wait.
    let refused = fixture
        .server
        .call_error(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": parent}),
        )
        .await;
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|message| message.contains("timed out")),
        "{refused}"
    );
    // Another thread starts meanwhile.
    fixture
        .server
        .call(&mut fixture.connection, "thread/start", json!({}))
        .await;

    hold.release();
    fixture
        .server
        .call(
            &mut fixture.connection,
            "thread/resume",
            json!({"threadId": parent}),
        )
        .await;
    fixture.script.answer("after resume");
    fixture
        .server
        .run_turn(&mut fixture.connection, &parent, "go on")
        .await;
    assert!(
        serde_json::to_string(fixture.script.last_request().input())
            .unwrap()
            .contains("go on")
    );
}

/// Spawns whose preparation never finishes, recording each spawn's session as it starts.
#[derive(Default)]
struct HeldSpawns {
    entered: tokio::sync::Notify,
    sessions: Mutex<Vec<SessionId>>,
}

#[async_trait]
impl ra_runtime::agent::control::SpawnedAgentFactory for HeldSpawns {
    async fn prepare(
        &self,
        request: ra_runtime::agent::control::SpawnedAgentRequest<'_>,
    ) -> Result<ra_runtime::agent::control::PreparedSpawnedAgent> {
        self.sessions
            .lock()
            .unwrap()
            .push(request.session_id().clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

/// Threads whose agent spawns through [`HeldSpawns`].
struct SpawningFactory {
    script: Arc<Script>,
    spawns: Arc<HeldSpawns>,
}

#[async_trait]
impl ThreadFactory for SpawningFactory {
    async fn prepare_thread(&self, request: &ThreadFactoryRequest) -> Result<PreparedThread> {
        let tools: Vec<Arc<dyn ra_core::tool::Tool>> =
            ra_tools::agent_ns::collaboration_tools_with(
                ra_tools::agent_ns::WaitAgentTimeoutOptions::new().with_bounds(2_000, 0, 60_000),
            )
            .unwrap()
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn ra_core::tool::Tool>)
            .collect();
        let lead = AgentSpec::builder()
            .id(AgentId::new("lead"))
            .name("Lead")
            .instructions("lead")
            .tools(tools)
            .build()
            .unwrap();
        let worker = AgentSpec::builder()
            .id(AgentId::new("worker"))
            .name("Worker")
            .instructions("worker")
            .build()
            .unwrap();
        let registry = ra_runtime::agent::AgentRegistry::builder()
            .register(worker)
            .build()
            .unwrap();
        // The server gives the tree the thread's rollout store when it loads the thread.
        let control = ra_runtime::agent::control::AgentControl::new()
            .with_spawned_agent_factory(Arc::clone(&self.spawns) as _)
            .unwrap();
        let builder = AgentThread::builder(
            request.thread_id().clone(),
            AgentBinding::direct(lead),
            Arc::new(Resolver(Arc::clone(&self.script))) as Arc<dyn ModelResolver>,
        )
        .with_config(RunConfig::new().with_agent_registry(registry))
        .with_agent_control(control);
        Ok(PreparedThread::new(builder))
    }
}

/// Archiving a thread while one of its agents is still being spawned freezes that spawn with the
/// thread; once the spawn has cleaned up within the thread's close, the archive lets go of it, so
/// its session is not left held as closing.
#[tokio::test]
async fn archiving_lets_go_of_a_spawn_that_cleaned_up_within_the_close() {
    let rollouts = tempfile::tempdir().unwrap();
    let store = Arc::new(RolloutThreadDirectory::new(rollouts.path()));
    let script = Arc::new(Script::default());
    let spawns = Arc::new(HeldSpawns::default());
    let (server, _) = build(
        Options {
            store: Some(store.clone()),
            factory: Some(Arc::new(SpawningFactory {
                script: Arc::clone(&script),
                spawns: Arc::clone(&spawns),
            })),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let parent = started["thread"]["id"].as_str().unwrap().to_owned();
    script.push(Step::Respond(ModelResponse::new(vec![RunItem::new(
        ItemId::new("spawn"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("spawn"),
            "spawn_agent",
            json!({"task_name": "worker", "message": "work", "agent_type": "worker"}),
        )),
    )])));
    server
        .call(
            &mut connection,
            "turn/start",
            json!({"threadId": parent, "input": [{"type": "text", "text": "delegate"}]}),
        )
        .await;
    timeout(PATIENCE, spawns.entered.notified())
        .await
        .expect("the spawn starts preparing");
    let child = spawns.sessions.lock().unwrap()[0].clone();

    server
        .call(
            &mut connection,
            "thread/archive",
            json!({"threadId": parent}),
        )
        .await;
    let refused = server
        .call_error(
            &mut connection,
            "thread/resume",
            json!({"threadId": child.as_str()}),
        )
        .await;
    let message = refused["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("is closing"),
        "the archive left the spawn held: {refused}"
    );
}
