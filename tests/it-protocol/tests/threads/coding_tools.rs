// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The configured coding factory reaches the app server's real control and user-input ports.

use super::*;

#[path = "../../../support/configured_coding_host.rs"]
mod configured_host;
#[path = "../../../support/marked_sleep.rs"]
mod marked_sleep;

use marked_sleep::MarkedSleep;

/// The namespace the collaboration tools are advertised in by default.
const COLLABORATION: &str = "collaboration";

fn call(name: &str, args: Value) -> ModelResponse {
    let id = format!("{name}-{}", uuid_like());
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(&id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, args)),
    )])
}

fn collab(name: &str, args: Value) -> ModelResponse {
    let id = format!("{name}-{}", uuid_like());
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(&id),
        RunItemKind::ToolCall(
            ToolCall::new(CallId::new(id), name, args).with_namespace(COLLABORATION),
        ),
    )])
}

/// Spawned agents answer from a queue of their own, told apart by their task; every other call
/// reads the shared script.
struct Agents {
    script: Arc<Script>,
    children: Mutex<VecDeque<ModelResponse>>,
    child_requests: Mutex<Vec<ModelRequest>>,
    called: Notify,
}

impl Agents {
    fn new(script: &Arc<Script>) -> Arc<Self> {
        Arc::new(Self {
            script: Arc::clone(script),
            children: Mutex::new(VecDeque::new()),
            child_requests: Mutex::new(Vec::new()),
            called: Notify::new(),
        })
    }

    async fn wait_for_child_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let called = self.called.notified();
                if self.child_requests.lock().unwrap().len() >= count {
                    return;
                }
                called.await;
            }
        })
        .await
        .expect("the child continues after its approval");
    }
}

#[async_trait]
impl Model for Agents {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let child = request.input().iter().any(|item| {
            matches!(item, ModelInputItem::Message(message) if message.text_content().contains("worker task"))
        });
        if !child {
            return ScriptedModel(Arc::clone(&self.script))
                .get_response(request)
                .await;
        }
        self.child_requests.lock().unwrap().push(request);
        self.called.notify_waiters();
        Ok(self
            .children
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| final_message("worker answer")))
    }
}

struct AgentsResolver(Arc<Agents>);

impl ModelResolver for AgentsResolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("mock-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// A server on `store` whose threads are the configured coding agent's, unloaded as soon as the
/// last connection lets go of them.
fn coding_server(
    agents: &Arc<Agents>,
    store: Arc<dyn ThreadStore>,
    workspace: &std::path::Path,
) -> (Server, Arc<configured_host::Services>) {
    let services = Arc::new(configured_host::Services::default());
    let host = services.install(CodingHost::open(workspace).unwrap());
    let settings = CodingThreadSettingsAdapter::new(
        workspace,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap();
    let factory =
        CodingThreadFactory::new(host, Arc::new(AgentsResolver(Arc::clone(agents))), settings)
            .with_profile(ra_coding::CodingProfile::CodexLike)
            .with_request_user_input();
    let (server, _) = build(
        Options {
            store: Some(store),
            factory: Some(Arc::new(factory)),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    );
    (server, services)
}

/// Runs a turn that lists the thread's live tree and asks the user a question, and returns what
/// the model saw after the answer.
async fn ask_on_thread(
    server: &Server,
    connection: &mut Connection,
    script: &Script,
    thread: &str,
) -> String {
    script.push(Step::Respond(collab("list_agents", json!({}))));
    script.push(Step::Respond(call(
        "request_user_input",
        json!({"questions": [{
            "id": "next", "header": "Next", "question": "Continue?", "options": [
                {"label": "Proceed", "description": "Continue the work"}
            ]
        }]}),
    )));
    script.answer("answered");
    server
        .turn_start(connection, thread, "inspect the tree and ask the user")
        .await;
    let question = connection
        .until(|message| message["method"] == "item/tool/requestUserInput")
        .await;
    assert_eq!(question["params"]["threadId"], thread);
    assert_eq!(question["params"]["isBlocking"], false);
    server
        .send(
            connection,
            ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<ra_protocol::frame::RequestId>(question["id"].clone())
                    .unwrap(),
                json!({"answers": {"next": {"answers": ["client answer"]}}}),
            ),
        )
        .await;
    connection.notification("turn/completed").await;
    let request = script.last_request();
    // Twelve functions and the six collaboration tools, the latter in their namespace.
    assert_eq!(request.tools().len(), 18);
    assert!(
        request
            .tools()
            .iter()
            .any(|tool| tool.qualified_name() == "collaboration.spawn_agent")
    );
    let input = serde_json::to_string(request.input()).unwrap();
    assert!(
        input.contains("/root"),
        "the live tree was not bound: {input}"
    );
    assert!(
        input.contains("client answer"),
        "the client's answer did not reach the model: {input}"
    );
    assert!(!input.contains("not enabled for this run"));
    input
}

#[tokio::test]
async fn configured_coding_factory_routes_controls_and_questions_after_start_fork_and_resume() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    let (server, services) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    ask_on_thread(&server, &mut connection, &script, thread.id()).await;
    let fork = server
        .fork(&mut connection, ThreadForkParams::new(thread.id()))
        .await;
    ask_on_thread(&server, &mut connection, &script, fork.thread().id()).await;

    // A cold resume prepares the thread through the factory again.
    unload(&server, &mut connection, thread.id()).await;
    let resumed = server
        .resume(&mut connection, ThreadResumeParams::new(thread.id()))
        .await;
    ask_on_thread(&server, &mut connection, &script, resumed.thread().id()).await;
    assert!(
        services.calls.lock().unwrap().is_empty(),
        "the server must replace the standalone question port"
    );
}

/// Each thread owns its agent tree, as each Codex session owns its spawned agents: another
/// thread's tree does not list them, and unloading the thread stops them and their processes.
#[tokio::test]
async fn each_coding_thread_owns_its_tree_and_unloading_it_stops_the_agents_below() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
    let mut connection = server.connect(1).await;
    let first = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let second = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;

    // The worker starts a command that outlives its turn, then finishes.
    let sleep = MarkedSleep::new();
    agents.children.lock().unwrap().push_back(call(
        "exec_command",
        json!({
            "cmd": format!("echo worker-ready-$((6 * 7)); {}", sleep.command()),
            "justification": null,
            "prefix_rule": null,
            "sandbox_permissions": null,
            "workdir": null,
            "shell": null,
            "tty": false,
            "login": null,
            "yield_time_ms": 500,
            "timeout_ms": null
        }),
    ));
    script.push(Step::Respond(collab(
        "spawn_agent",
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
    )));
    script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms": 10000}),
    )));
    script.answer("delegated");
    server
        .run_turn(&mut connection, first.id(), "delegate a command")
        .await;
    // The worker's second call carries its command's output; the arithmetic keeps the marker out
    // of the call's own arguments, so only the output can match.
    timeout(PATIENCE, async {
        loop {
            let started = agents.child_requests.lock().unwrap().iter().any(|request| {
                serde_json::to_string(request.input())
                    .unwrap()
                    .contains("worker-ready-42")
            });
            if started {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let requests = agents.child_requests.lock().unwrap();
        panic!(
            "the worker did not run its command: {}",
            requests
                .last()
                .map(|request| serde_json::to_string(request.input()).unwrap())
                .unwrap_or_default()
        )
    });
    assert!(sleep.is_alive());

    script.push(Step::Respond(collab("list_agents", json!({}))));
    script.answer("listed");
    server
        .run_turn(&mut connection, second.id(), "list the tree")
        .await;
    let second_view = serde_json::to_string(script.last_request().input()).unwrap();
    assert!(second_view.contains("/root"), "{second_view}");
    assert!(
        !second_view.contains("/root/worker"),
        "another thread's tree listed the worker: {second_view}"
    );

    script.push(Step::Respond(collab("list_agents", json!({}))));
    script.answer("listed");
    server
        .run_turn(&mut connection, first.id(), "list the tree")
        .await;
    let first_view = serde_json::to_string(script.last_request().input()).unwrap();
    assert!(first_view.contains("/root/worker"), "{first_view}");

    unload(&server, &mut connection, first.id()).await;
    timeout(PATIENCE, async {
        while sleep.is_alive() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("unloading the thread stops the worker's processes");
}

fn child_patch(filename: &str) -> ModelResponse {
    call(
        "apply_patch",
        json!({"patch": format!(
            "*** Begin Patch\n*** Add File: {filename}\n+child work\n*** End Patch"
        )}),
    )
}

async fn spawn_child(
    server: &Server,
    connection: &mut Connection,
    script: &Script,
    thread: &str,
    name: &str,
) {
    script.push(Step::Respond(collab(
        "spawn_agent",
        json!({
            "task_name": name, "message": "worker task", "fork_turns": "none"
        }),
    )));
    script.answer("delegated");
    server
        .run_turn(connection, thread, "delegate the task")
        .await;
}

async fn answer_approval(
    server: &Server,
    connection: &Connection,
    request: &Value,
    decision: &str,
) {
    server
        .send(
            connection,
            ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                    .unwrap(),
                json!({"decision": decision}),
            ),
        )
        .await;
}

/// A child keeps its checkpoint across parent turns and replays its request to new subscribers.
#[tokio::test]
async fn spawned_patch_approvals_survive_parent_turns_and_reconnects() {
    for decision in ["accept", "decline"] {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let script = Arc::new(Script::default());
        let agents = Agents::new(&script);
        agents
            .children
            .lock()
            .unwrap()
            .push_back(child_patch("child.txt"));
        let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
        let mut connection = server.connect(1).await;
        let thread = server
            .start_thread(&mut connection, ThreadStartParams::new())
            .await;
        spawn_child(&server, &mut connection, &script, thread.id(), "worker").await;
        let request = connection
            .until(|message| message["method"] == "item/fileChange/requestApproval")
            .await;
        assert_ne!(request["params"]["threadId"], thread.id());

        script.push(Step::Respond(collab("list_agents", json!({}))));
        script.answer("listed");
        server
            .run_turn(&mut connection, thread.id(), "inspect the tree")
            .await;
        let mut reconnected = server.connect(2).await;
        server
            .resume(&mut reconnected, ThreadResumeParams::new(thread.id()))
            .await;
        let replay = reconnected
            .until(|message| message["method"] == "item/fileChange/requestApproval")
            .await;
        assert_eq!(replay, request);
        answer_approval(&server, &reconnected, &replay, decision).await;
        agents.wait_for_child_calls(2).await;
        assert_eq!(root.join("child.txt").exists(), decision == "accept");
        let resolved = connection.notification("serverRequest/resolved").await;
        assert_eq!(resolved["threadId"], request["params"]["threadId"]);
        assert_eq!(resolved["requestId"], request["id"]);
        let input = serde_json::to_string(
            agents
                .child_requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .input(),
        )
        .unwrap();
        assert!(
            input.contains(if decision == "accept" {
                "Applied patch"
            } else {
                "rejected"
            }),
            "{input}"
        );
        server
            .call(
                &mut reconnected,
                "thread/unsubscribe",
                json!({"threadId": thread.id()}),
            )
            .await;
        unload(&server, &mut connection, thread.id()).await;
    }
}

#[tokio::test]
async fn spawned_command_approval_resumes_the_child() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    agents.children.lock().unwrap().push_back(call(
        "exec_command",
        json!({
            "cmd": "printf approved-child", "sandbox_permissions": "require_escalated",
            "justification": "test the child approval", "prefix_rule": null,
            "workdir": null, "shell": null, "tty": false, "login": null,
            "yield_time_ms": 1000, "timeout_ms": null
        }),
    ));
    let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    spawn_child(&server, &mut connection, &script, thread.id(), "worker").await;
    let request = connection
        .until(|message| message["method"] == "item/commandExecution/requestApproval")
        .await;
    assert_ne!(request["params"]["threadId"], thread.id());
    answer_approval(&server, &connection, &request, "accept").await;
    agents.wait_for_child_calls(2).await;
    let input = serde_json::to_string(
        agents
            .child_requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .input(),
    )
    .unwrap();
    assert!(input.contains("approved-child"), "{input}");
    unload(&server, &mut connection, thread.id()).await;
}

#[tokio::test]
async fn interrupting_a_spawned_agent_withdraws_its_approval() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    agents
        .children
        .lock()
        .unwrap()
        .push_back(child_patch("child.txt"));
    let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    spawn_child(&server, &mut connection, &script, thread.id(), "worker").await;
    let request = connection
        .until(|message| message["method"] == "item/fileChange/requestApproval")
        .await;
    script.push(Step::Respond(collab(
        "interrupt_agent",
        json!({"target": "worker"}),
    )));
    script.answer("interrupted");
    server
        .run_turn(&mut connection, thread.id(), "interrupt the worker")
        .await;
    let resolved = connection.notification("serverRequest/resolved").await;
    assert_eq!(resolved["requestId"], request["id"]);
    answer_approval(&server, &connection, &request, "accept").await;
    unload(&server, &mut connection, thread.id()).await;
    assert!(!root.join("child.txt").exists());
    assert_eq!(agents.child_requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn simultaneous_children_own_independent_approval_batches() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    agents
        .children
        .lock()
        .unwrap()
        .extend([child_patch("first.txt"), child_patch("second.txt")]);
    let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    spawn_child(&server, &mut connection, &script, thread.id(), "first").await;
    let first = connection
        .until(|message| message["method"] == "item/fileChange/requestApproval")
        .await;
    spawn_child(&server, &mut connection, &script, thread.id(), "second").await;
    let second = connection
        .until(|message| message["method"] == "item/fileChange/requestApproval")
        .await;
    assert_ne!(first["params"]["threadId"], second["params"]["threadId"]);
    answer_approval(&server, &connection, &first, "accept").await;
    answer_approval(&server, &connection, &second, "decline").await;
    agents.wait_for_child_calls(4).await;
    assert!(root.join("first.txt").exists());
    assert!(!root.join("second.txt").exists());
    unload(&server, &mut connection, thread.id()).await;
}

#[tokio::test]
async fn spawned_batches_wait_for_all_answers_and_withdraw_on_cancel() {
    for cancel in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let script = Arc::new(Script::default());
        let agents = Agents::new(&script);
        let output = [child_patch("first.txt"), child_patch("second.txt")]
            .into_iter()
            .flat_map(|response| response.output().to_vec())
            .collect();
        agents
            .children
            .lock()
            .unwrap()
            .extend([ModelResponse::new(output), child_patch("later.txt")]);
        let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
        let mut connection = server.connect(1).await;
        let thread = server
            .start_thread(&mut connection, ThreadStartParams::new())
            .await;
        spawn_child(&server, &mut connection, &script, thread.id(), "worker").await;
        let first = connection
            .until(|message| message["method"] == "item/fileChange/requestApproval")
            .await;
        let second = connection
            .until(|message| message["method"] == "item/fileChange/requestApproval")
            .await;
        assert_eq!(first["params"]["threadId"], second["params"]["threadId"]);
        answer_approval(
            &server,
            &connection,
            &first,
            if cancel { "cancel" } else { "accept" },
        )
        .await;
        connection.notification("serverRequest/resolved").await;
        assert!(!root.join("first.txt").exists());
        assert!(!root.join("second.txt").exists());
        if cancel {
            let resolved = connection.notification("serverRequest/resolved").await;
            assert_eq!(resolved["requestId"], second["id"]);
        } else {
            answer_approval(&server, &connection, &second, "accept").await;
            let later = connection
                .until(|message| message["method"] == "item/fileChange/requestApproval")
                .await;
            assert_eq!(later["params"]["turnId"], first["params"]["turnId"]);
            assert!(root.join("first.txt").exists());
            assert!(root.join("second.txt").exists());
            answer_approval(&server, &connection, &later, "accept").await;
            agents.wait_for_child_calls(3).await;
            assert!(root.join("later.txt").exists());
        }
        unload(&server, &mut connection, thread.id()).await;
        if cancel {
            assert_eq!(agents.child_requests.lock().unwrap().len(), 1);
            assert!(!root.join("first.txt").exists());
            assert!(!root.join("second.txt").exists());
        }
    }
}

#[tokio::test]
async fn ephemeral_fork_children_use_the_fork_prompt_cache_key() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    let (server, _) = coding_server(&agents, Arc::new(InMemoryThreadStore::new()), &root);
    let mut connection = server.connect(1).await;
    let parent = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("parent answer");
    server
        .run_turn(&mut connection, parent.id(), "parent")
        .await;
    let fork = server
        .fork(
            &mut connection,
            ThreadForkParams::new(parent.id()).ephemeral(),
        )
        .await;
    assert_ne!(fork.thread().id(), parent.id());

    script.push(Step::Respond(collab(
        "spawn_agent",
        json!({
            "task_name": "worker", "message": "worker task", "fork_turns": "none"
        }),
    )));
    script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms":10000}),
    )));
    script.answer("delegated");
    server
        .run_turn(&mut connection, fork.thread().id(), "delegate")
        .await;
    agents.wait_for_child_calls(1).await;
    assert_eq!(script.last_request().prompt_cache_key(), Some(parent.id()));
    let children = agents.child_requests.lock().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].prompt_cache_key(), Some(fork.thread().id()));
    assert_ne!(children[0].prompt_cache_key(), Some(parent.id()));
}

/// Codex runs SessionEnd only for the session the user started: a spawned agent's thread ends
/// without it (`session_end_skips_subagents`). Ending the root thread here shuts its whole agent
/// tree down, which must run the root's SessionEnd once and none for the worker below it.
#[tokio::test]
async fn ending_a_thread_with_spawned_agents_runs_session_end_once_for_the_root() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let log = home_path.join("session-end.jsonl");
    let stops = home_path.join("subagent-stop.jsonl");
    let section = |event: &str, log: &std::path::Path| {
        format!(
            "[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = '''cat >> '{0}'; echo >> '{0}' '''\n",
            log.display()
        )
    };
    std::fs::write(
        home_path.join("config.toml"),
        format!(
            "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n\n[features]\ncodex_hooks = true\n\n{}{}",
            section("SessionEnd", &log),
            section("SubagentStop", &stops)
        ),
    )
    .unwrap();
    let read = |log: &std::path::Path| -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let builder = ra_coding::config::ConfigBuilder::default()
        .codex_home(home_path.clone())
        .loader_overrides(ra_config::LoaderOverrides {
            managed_config_path: Some(home_path.join("managed_config.toml")),
            system_config_path: Some(home_path.join("system_config.toml")),
            system_requirements_path: Some(home_path.join("requirements.toml")),
            ..ra_config::LoaderOverrides::without_managed_config_for_tests()
        })
        .harness_overrides(ra_coding::config::ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..ra_coding::config::ConfigOverrides::default()
        });
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    let services = Arc::new(configured_host::Services::default());
    let host = services.install(CodingHost::open(&root).unwrap());
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap()
    .with_rules_home(&home_path);
    let factory = CodingThreadFactory::new(
        host,
        Arc::new(AgentsResolver(Arc::clone(&agents))),
        settings,
    )
    .with_profile(ra_coding::CodingProfile::CodexLike)
    .with_request_user_input()
    .with_product_config(builder);
    let (server, _) = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: Some(Arc::new(factory)),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.push(Step::Respond(collab(
        "spawn_agent",
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
    )));
    script.push(Step::Respond(collab(
        "wait_agent",
        json!({"timeout_ms": 10000}),
    )));
    script.answer("delegated");
    server
        .run_turn(&mut connection, thread.id(), "delegate")
        .await;
    agents.wait_for_child_calls(1).await;
    // The worker ran under the thread's hooks: its stop was reported as SubagentStop.
    timeout(PATIENCE, async {
        while read(&stops).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the worker's stop runs SubagentStop");
    assert!(
        !log.exists(),
        "a spawned agent finishing its task must not run SessionEnd"
    );

    unload(&server, &mut connection, thread.id()).await;
    assert_eq!(read(&stops).len(), 1);
    let inputs = read(&log);
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert_eq!(inputs[0]["hook_event_name"], "SessionEnd");
    assert_eq!(inputs[0]["session_id"], thread.id());
}

/// Codex hooks a namespaced function tool under `flat_tool_name`, the namespace and the name
/// joined with nothing between them, and gives `spawn_agent` the `Agent` alias only in the default
/// namespace and `multi_agent_v1`: the collaboration tools are `collaborationlist_agents` and
/// `collaborationspawn_agent`, matched by neither their dotted name nor `Agent`.
#[tokio::test]
async fn collaboration_tools_are_hooked_under_codexs_flat_names() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let flat = home_path.join("flat.jsonl");
    let other = home_path.join("other.jsonl");
    let section = |matcher: &str, log: &std::path::Path| {
        format!(
            "[[hooks.PreToolUse]]\nmatcher = '{matcher}'\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = '''cat >> '{0}'; echo >> '{0}' '''\n",
            log.display()
        )
    };
    std::fs::write(
        home_path.join("config.toml"),
        format!(
            "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n\n[features]\ncodex_hooks = true\n\n{}{}{}",
            section("^collaboration(list_agents|spawn_agent)$", &flat),
            section("^collaboration\\.", &other),
            section("^Agent$", &other)
        ),
    )
    .unwrap();
    let read = |log: &std::path::Path| -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let builder = ra_coding::config::ConfigBuilder::default()
        .codex_home(home_path.clone())
        .loader_overrides(ra_config::LoaderOverrides {
            managed_config_path: Some(home_path.join("managed_config.toml")),
            system_config_path: Some(home_path.join("system_config.toml")),
            system_requirements_path: Some(home_path.join("requirements.toml")),
            ..ra_config::LoaderOverrides::without_managed_config_for_tests()
        })
        .harness_overrides(ra_coding::config::ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..ra_coding::config::ConfigOverrides::default()
        });
    let script = Arc::new(Script::default());
    let agents = Agents::new(&script);
    let services = Arc::new(configured_host::Services::default());
    let host = services.install(CodingHost::open(&root).unwrap());
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap()
    .with_rules_home(&home_path);
    let factory = CodingThreadFactory::new(
        host,
        Arc::new(AgentsResolver(Arc::clone(&agents))),
        settings,
    )
    .with_profile(ra_coding::CodingProfile::CodexLike)
    .with_product_config(builder);
    let (server, _) = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: Some(Arc::new(factory)),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.push(Step::Respond(collab("list_agents", json!({}))));
    script.push(Step::Respond(collab(
        "spawn_agent",
        json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
    )));
    script.answer("listed");
    server.run_turn(&mut connection, thread.id(), "list").await;
    agents.wait_for_child_calls(1).await;

    let names: Vec<Value> = read(&flat)
        .iter()
        .map(|input| input["tool_name"].clone())
        .collect();
    assert_eq!(
        names,
        [
            json!("collaborationlist_agents"),
            json!("collaborationspawn_agent")
        ]
    );
    assert!(read(&other).is_empty(), "{:?}", read(&other));
}
