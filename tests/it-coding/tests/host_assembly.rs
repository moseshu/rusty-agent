//! Real host assembly, backend dispatch and collaboration across live thread turns.

#[path = "../../support/configured_coding_host.rs"]
mod configured_host;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_coding::{
    CodingHost, CodingProfile, build_agent_with_control, build_agent_with_profile,
    host_backed_surface,
};
use ra_core::{
    agent::{
        AgentId, AgentSpec,
        control::{AgentPath, AgentStatus},
    },
    error::Result,
    item::{
        CallId, ContentBlock, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    prompt::PromptRole,
    session::SessionId,
};
use ra_runtime::{
    agent::{AgentBinding, control::AgentControl},
    thread::{AgentThread, ThreadEvent, TurnInputRequest},
};
use serde_json::{Value, json};
use tokio::{sync::Notify, time::timeout};

const PATIENCE: Duration = Duration::from_secs(10);

/// The namespace the collaboration tools are advertised in by default.
const COLLABORATION: &str = "collaboration";

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    /// What spawned agents answer before their final message.
    child_responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    child_requests: Mutex<Vec<ModelRequest>>,
    hang_child: std::sync::atomic::AtomicBool,
    child_started: Notify,
    child_cancelled: Arc<std::sync::atomic::AtomicBool>,
}

struct ChildCall(Arc<std::sync::atomic::AtomicBool>);

impl Drop for ChildCall {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl Model for Script {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        // Fork-free children answer independently of the parent's scripted turn sequence.
        let child = request.input().iter().any(|item| {
            matches!(item, ModelInputItem::Message(message) if message.text_content().contains("worker task"))
        });
        if child {
            self.child_requests.lock().unwrap().push(request);
            if self.hang_child.load(std::sync::atomic::Ordering::SeqCst) {
                let _call = ChildCall(self.child_cancelled.clone());
                self.child_started.notify_one();
                return std::future::pending().await;
            }
            if let Some(response) = self.child_responses.lock().unwrap().pop_front() {
                return Ok(response);
            }
            return Ok(answer("worker answer"));
        }
        self.requests.lock().unwrap().push(request);
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted response"))
    }
}

struct Resolver(Arc<Script>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        next_id(),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn call(name: &str, args: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        next_id(),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(name), name, args)),
    )])
}

/// A call of a collaboration tool in its default namespace.
fn collab(name: &str, args: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        next_id(),
        RunItemKind::ToolCall(
            ToolCall::new(CallId::new(name), name, args).with_namespace(COLLABORATION),
        ),
    )])
}

fn qualified(name: &str) -> String {
    format!("{COLLABORATION}.{name}")
}

/// The model-facing names of the tools a request carries.
fn request_tools(request: &ModelRequest) -> Vec<String> {
    request
        .tools()
        .iter()
        .map(ra_core::model::ModelToolDefinition::qualified_name)
        .collect()
}

fn next_id() -> ItemId {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    ItemId::new(format!("response-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

async fn complete_turn(thread: &AgentThread, text: &str) {
    thread
        .start_or_steer_turn(TurnInputRequest::new(vec![ContentBlock::text(text)]))
        .await;
    timeout(PATIENCE, async {
        loop {
            match thread.next_event().await.expect("a live thread") {
                ThreadEvent::TurnComplete { .. } => break,
                ThreadEvent::Error { message, .. } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the turn completes");
}

async fn configured_agent(host: &CodingHost, role: &PromptRole) -> (Arc<AgentSpec>, AgentControl) {
    let (agent, control) = build_agent_with_control(
        AgentId::new("coding"),
        "Coding",
        role,
        host,
        CodingProfile::CodexLike,
    )
    .await
    .expect("the configured standard tier builds")
    .into_parts();
    (
        agent,
        control.expect("the standard tier offers collaboration"),
    )
}

fn thread(
    id: &str,
    host: &CodingHost,
    agent: Arc<AgentSpec>,
    control: AgentControl,
    script: &Arc<Script>,
) -> AgentThread {
    AgentThread::builder(
        SessionId::new(id),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(script))),
    )
    .with_config(host.build_run_config())
    .with_services(host.build_tool_services())
    .with_agent_control(control)
    .build()
}

#[tokio::test]
async fn configured_profiles_preserve_independent_tools_and_role_boundaries() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    for tier in CodingProfile::SHIPPED {
        for role in [
            PromptRole::Main,
            PromptRole::Coordinator,
            PromptRole::ReadOnlySpecialist,
            PromptRole::Planner,
            PromptRole::OneOffAnswer,
        ] {
            let assembled = host_backed_surface(&role, &host, tier).await.unwrap();
            let built =
                build_agent_with_control(AgentId::new("coding"), "Coding", &role, &host, tier)
                    .await
                    .unwrap();
            let agent = built.agent();
            let names: Vec<_> = assembled.tool_surface().advertised_names().collect();
            assert!(
                !names
                    .iter()
                    .any(|name| ["agent", "mcp", "tool_search"].contains(name))
            );
            assert_eq!(agent.tools().len(), names.len());
            let spawn = qualified("spawn_agent");
            // The tree exists exactly when the surface carries the tools that act on it.
            assert_eq!(
                built.agent_control().is_some(),
                names.contains(&spawn.as_str()),
                "{tier:?} / {role:?}"
            );
            if role.is_one_off() {
                assert!(names.is_empty());
            } else if tier != CodingProfile::Core && !role.is_read_only() {
                assert_eq!(names.len(), 18);
                for name in ["web_search", "web_fetch", "skill", "request_user_input"] {
                    assert!(names.contains(&name), "{tier:?}: missing {name}");
                }
                for name in [
                    "spawn_agent",
                    "send_message",
                    "followup_task",
                    "interrupt_agent",
                    "wait_agent",
                    "list_agents",
                ] {
                    let name = qualified(name);
                    assert!(names.contains(&name.as_str()), "{tier:?}: missing {name}");
                    // Codex's default places them in a namespace; none is offered bare.
                    assert!(!names.contains(&&name[COLLABORATION.len() + 1..]));
                }
            }
            if role.is_read_only() {
                assert!(
                    agent
                        .tools()
                        .iter()
                        .all(|tool| tool.options().permission_scope()
                            == ra_core::permission::PermissionScope::Read)
                );
                assert!(!names.contains(&spawn.as_str()));
            }
        }
    }
}

#[tokio::test]
async fn an_agent_offered_collaboration_is_built_with_its_tree_only() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let error = build_agent_with_profile(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("build_agent_with_control"),
        "{error}"
    );
    // Without collaboration tools on the surface there is no tree to own, and the plain builder
    // still serves.
    build_agent_with_profile(
        AgentId::new("coding"),
        "Coding",
        &PromptRole::Main,
        &host,
        CodingProfile::Core,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_bare_collaboration_namespace_serves_providers_without_namespaces() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_collaboration_namespace(None);
    let assembled = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap();
    let names: Vec<_> = assembled.tool_surface().advertised_names().collect();
    assert_eq!(names.len(), 18);
    assert!(names.contains(&"spawn_agent"));
    assert!(!names.iter().any(|name| name.starts_with("collaboration.")));
}

#[tokio::test]
async fn a_narrow_tier_does_not_load_an_unselected_catalog() {
    let workspace = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    services
        .fail_listing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let host = services.install(CodingHost::open(workspace.path()).unwrap());
    let core = host_backed_surface(&PromptRole::Main, &host, CodingProfile::Core)
        .await
        .unwrap();
    assert_eq!(core.tool_surface().advertised_count(), 6);
    let error = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("catalog unavailable"));
}

#[tokio::test]
async fn configured_backends_receive_real_calls_and_results_reach_history() {
    let workspace = tempfile::tempdir().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = services.install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        call("web_search", json!({"query": "Rust", "max_results": null})),
        call("web_fetch", json!({"url": "https://example.test/guide"})),
        call("skill", json!({"skill": "rust-guide", "offset": null})),
        call("request_user_input", json!({"questions": [{"id": "next", "header": "Next", "question": "Continue?", "options": [{"label": "Proceed", "description": "Continue the work"}]}]})),
        answer("finished"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("backends", &host, agent, control, &script);
    complete_turn(&thread, "use the installed services").await;
    assert_eq!(
        *services.calls.lock().unwrap(),
        [
            "search:Rust",
            "fetch:https://example.test/guide",
            "skill:rust-guide",
            "user_input"
        ]
    );
    let history = serde_json::to_string(&thread.history()).unwrap();
    for text in [
        "Rust guide",
        "backend document body",
        "skill instructions from the catalog",
        "Proceed",
    ] {
        assert!(
            history.contains(text),
            "missing backend result {text}: {history}"
        );
    }
    assert_eq!(script.requests.lock().unwrap()[0].tools().len(), 18);
    thread.shutdown().await;
}

#[tokio::test]
async fn thread_control_keeps_children_across_turns_and_isolates_other_threads() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        collab("wait_agent", json!({})),
        answer("first turn"),
        collab("list_agents", json!({})),
        answer("second turn"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let (_, other) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("parent", &host, agent, control.clone(), &script);
    complete_turn(&thread, "delegate work").await;
    assert_eq!(control.agents().len(), 2);
    assert_eq!(other.agents().len(), 1);
    complete_turn(&thread, "inspect the live tree").await;
    let history = serde_json::to_string(&thread.history()).unwrap();
    assert!(history.contains("/root/worker"));
    assert!(history.contains("worker answer"));
    thread.shutdown().await;
    assert_eq!(
        control.status(&AgentPath::from_string("/root/worker".to_owned()).unwrap()),
        Some(AgentStatus::Shutdown)
    );
    assert_eq!(other.agents().len(), 1);
    other.shutdown().await;
}

/// Whether process `pid` is still running.
fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Codex starts a spawned agent as a session of its own, with its own unified-exec process manager
/// and its own tool plan: the child's commands are not the parent's sessions, they stop when the
/// tree shuts down, and the child is not offered the collaboration tools unless its model takes
/// them.
#[tokio::test]
async fn a_spawned_agent_is_assembled_with_its_own_processes_and_tool_plan() {
    use ra_core::permission::{PermissionDecision, PermissionMode, PermissionRule};
    use ra_runtime::permission::PermissionEngine;
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script.child_responses.lock().unwrap().push_back(call(
        "exec_command",
        json!({
            "cmd": "echo $$ > child.pid; exec sleep 30",
            "justification": null,
            "prefix_rule": null,
            "sandbox_permissions": null,
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 500,
            "timeout_ms": null
        }),
    ));
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        answer("delegated"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    // The worker runs on the configuration of the run that spawned it, which lets the command
    // through without asking.
    let config = host.build_run_config().with_permission_engine(
        PermissionEngine::new(PermissionMode::Default)
            .with_rules([
                PermissionRule::new(PermissionDecision::Allow).with_tool_name("exec_command")
            ]),
    );
    let thread = AgentThread::builder(
        SessionId::new("processes"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(&script))),
    )
    .with_config(config)
    .with_services(host.build_tool_services())
    .with_agent_control(control.clone())
    .build();
    complete_turn(&thread, "delegate a command").await;
    let worker = AgentPath::from_string("/root/worker".to_owned()).unwrap();
    timeout(PATIENCE, async {
        while !matches!(control.status(&worker), Some(AgentStatus::Completed(_))) {
            assert!(
                control.paused_runs().is_empty(),
                "the worker stopped for approval"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the worker completes");

    let pid: u32 = std::fs::read_to_string(workspace.path().join("child.pid"))
        .unwrap_or_else(|error| {
            let requests = script.child_requests.lock().unwrap();
            panic!(
                "the worker did not run its command ({error}): {}",
                serde_json::to_string(requests.last().unwrap().input()).unwrap()
            )
        })
        .trim()
        .parse()
        .unwrap();
    assert!(is_alive(pid));
    // The worker's command lives in the worker's manager, not in the one the parent's tools use.
    assert!(host.process_manager().active_sessions().await.is_empty());
    let child_tools = request_tools(&script.child_requests.lock().unwrap()[0]);
    assert_eq!(child_tools.len(), 12, "{child_tools:?}");
    assert!(child_tools.contains(&"exec_command".to_owned()));
    assert!(
        !child_tools
            .iter()
            .any(|name| name.starts_with("collaboration."))
    );
    let parent_tools = request_tools(&script.requests.lock().unwrap()[0]);
    assert!(parent_tools.contains(&qualified("spawn_agent")));

    timeout(PATIENCE, thread.shutdown()).await.unwrap();
    timeout(PATIENCE, async {
        while is_alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("shutting the tree down stops the worker's processes");
}

/// A host that states its model takes the collaboration tools offers them to spawned agents too,
/// as Codex does for a model declaring its second multi-agent version.
#[tokio::test]
async fn spawned_agents_are_offered_collaboration_when_the_host_says_so() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap())
        .with_spawned_agent_collaboration();
    let script = Arc::new(Script::default());
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        collab("wait_agent", json!({"timeout_ms": 10000})),
        answer("delegated"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("nested", &host, agent, control, &script);
    complete_turn(&thread, "delegate").await;
    let child_tools = request_tools(&script.child_requests.lock().unwrap()[0]);
    assert_eq!(child_tools.len(), 18, "{child_tools:?}");
    assert!(child_tools.contains(&qualified("spawn_agent")));
    thread.shutdown().await;
}

#[tokio::test]
async fn thread_shutdown_cancels_and_waits_for_a_running_child() {
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script
        .hang_child
        .store(true, std::sync::atomic::Ordering::SeqCst);
    script.responses.lock().unwrap().extend([
        collab(
            "spawn_agent",
            json!({"task_name": "worker", "message": "worker task", "fork_turns": "none"}),
        ),
        answer("parent complete"),
    ]);
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = thread("shutdown", &host, agent, control.clone(), &script);
    complete_turn(&thread, "start background work").await;
    timeout(PATIENCE, script.child_started.notified())
        .await
        .unwrap();
    let worker = AgentPath::from_string("/root/worker".to_owned()).unwrap();
    assert_eq!(control.status(&worker), Some(AgentStatus::Running));
    timeout(PATIENCE, thread.shutdown()).await.unwrap();
    assert!(
        script
            .child_cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(control.status(&worker), Some(AgentStatus::Shutdown));
}

#[tokio::test]
async fn an_approval_resume_keeps_the_thread_bound_to_its_control_tree() {
    use ra_core::permission::{PermissionDecision, PermissionMode, PermissionRule};
    use ra_runtime::permission::PermissionEngine;
    let workspace = tempfile::tempdir().unwrap();
    let host = Arc::new(configured_host::Services::default())
        .install(CodingHost::open(workspace.path()).unwrap());
    let script = Arc::new(Script::default());
    script
        .responses
        .lock()
        .unwrap()
        .extend([collab("list_agents", json!({})), answer("resumed")]);
    let config = host.build_run_config().with_permission_engine(
        PermissionEngine::new(PermissionMode::Default).with_rules([PermissionRule::new(
            PermissionDecision::Ask,
        )
        .with_tool_name("list_agents")
        .with_namespace(COLLABORATION)]),
    );
    let (agent, control) = configured_agent(&host, &PromptRole::Main).await;
    let thread = AgentThread::builder(
        SessionId::new("approval"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(script)),
    )
    .with_config(config)
    .with_services(host.build_tool_services())
    .with_agent_control(control)
    .build();
    thread
        .start_or_steer_turn(TurnInputRequest::new(vec![ContentBlock::text(
            "list the live tree",
        )]))
        .await;
    timeout(PATIENCE, async {
        while !matches!(
            thread.next_event().await,
            Some(ThreadEvent::TurnAwaitingApproval { .. })
        ) {}
    })
    .await
    .unwrap();
    let mut state = thread.paused_run().unwrap();
    let item = state.pending_interruption_items().next().unwrap().clone();
    state.approve(&item, false).unwrap();
    thread.resume_turn(state).unwrap();
    timeout(PATIENCE, async {
        while !matches!(
            thread.next_event().await,
            Some(ThreadEvent::TurnComplete { .. })
        ) {}
    })
    .await
    .unwrap();
    let history = serde_json::to_string(&thread.history()).unwrap();
    assert!(history.contains("/root"), "{history}");
    assert!(!history.contains("not enabled for this run"));
    thread.shutdown().await;
}
