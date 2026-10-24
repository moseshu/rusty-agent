//! The coding host's `request_permissions`, as Codex's app-server carries it
//! (`app-server/tests/suite/v2/request_permissions.rs`, the core suite's `request_permissions.rs`):
//! the `item/permissions/requestApproval` request a call becomes, the thread waiting on approval
//! meanwhile, what the model is given of the grant, and where the grant is kept.
//!
//! Codex's round trip runs with project roots above its cwd; a coding thread's one project root is
//! its cwd, so its second request is checked against that root.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_coding::{
    host::CodingHost,
    permissions::{AdditionalPermissionProfile, FileSystemPermissions},
    thread_settings::{CodingThreadSettings, CodingThreadSettingsAdapter},
};
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{SessionId, rollout::RolloutRecorder},
    state::RunId,
    tool::{Tool, ToolOutput},
};
use ra_protocol::{
    frame::{
        JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
        RequestId,
    },
    server::{
        AppServer, ThreadSettingsAdapter,
        protocol::{
            AskForApproval, ClientInfo, InitializeCapabilities, InitializeParams, SandboxPolicy,
            ThreadPolicyOverrides, TurnStartParams, UserInput,
        },
        test_api::ThreadLoader,
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{
    agent::{AgentBinding, tool::AgentAsTool},
    runner::RunConfig,
    thread::AgentThread,
};
use ra_session::{
    RolloutSessionMeta,
    store::{CreateThreadParams, InMemoryThreadStore, ThreadStore},
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

const PATIENCE: Duration = Duration::from_secs(10);

const THREAD: &str = "thread-1";

// ---------------------------------------------------------------------------------------------
// A scripted model
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
}

impl Script {
    fn push(&self, response: ModelResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    /// What the model was last given of `call_id`'s output, parsed as JSON when it is.
    fn last_output(&self, call_id: &str) -> Option<Value> {
        let inputs = self.inputs.lock().unwrap();
        inputs.iter().rev().flatten().find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                let text = output_text(output);
                Some(serde_json::from_str(&text).unwrap_or(Value::String(text)))
            }
            _ => None,
        })
    }

    fn calls(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
}

struct Resolver(Arc<Script>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some(model_name.unwrap_or("mock-model").to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedModel(Arc<Script>);

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0.inputs.lock().unwrap().push(request.input().to_vec());
        self.0
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("the model ran out of responses"))
    }
}

fn output_text(output: &ToolCallOutput) -> String {
    ToolOutput::from_stored(output.output())
        .ok()
        .flatten()
        .and_then(|output| output.as_text().map(str::to_owned))
        .unwrap_or_else(|| match output.output() {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
}

fn final_message(text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("answer-{text}")),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn request_permissions(call_id: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("item-{call_id}")),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "request_permissions",
            arguments,
        )),
    )])
}

/// Codex's `create_request_permissions_sse_response` arguments.
fn select_a_workspace_root() -> Value {
    json!({
        "reason": "Select a workspace root",
        "permissions": {"file_system": {"write": [".", "../shared"]}}
    })
}

/// What a call granted nothing is given: Codex's empty turn grant.
fn nothing_granted() -> Value {
    json!({"permissions": {"network": null, "file_system": null}, "scope": "turn"})
}

fn on_request() -> AskForApproval {
    AskForApproval::OnRequest
}

fn granular(request_permissions: bool) -> AskForApproval {
    AskForApproval::Granular {
        sandbox_approval: true,
        rules: true,
        skill_approval: true,
        request_permissions,
        mcp_elicitations: true,
    }
}

// ---------------------------------------------------------------------------------------------
// The server in this process, with a coding thread that may ask for permissions
// ---------------------------------------------------------------------------------------------

struct Server {
    events: mpsc::Sender<TransportEvent>,
    loader: ThreadLoader,
    store: Arc<InMemoryThreadStore>,
    _task: JoinHandle<()>,
    _workspace: tempfile::TempDir,
    host: CodingHost,
}

impl Server {
    fn start() -> Self {
        Self::with_workspace(tempfile::tempdir().unwrap())
    }

    fn with_workspace(workspace: tempfile::TempDir) -> Self {
        let host = CodingHost::open(workspace.path()).unwrap();
        let store = Arc::new(InMemoryThreadStore::new());
        let server = AppServer::new(std::env::temp_dir().join("rusty-agent-request-permissions"))
            .unwrap()
            .with_thread_store(Arc::clone(&store) as Arc<dyn ThreadStore>);
        let loader = ThreadLoader::new(&server);
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            loader,
            store,
            _task: task,
            _workspace: workspace,
            host,
        }
    }

    fn root(&self) -> PathBuf {
        self.host.workspace_root().to_path_buf()
    }

    async fn connect(&self, id: u64) -> Connection {
        let id = ConnectionId::new(id);
        let (writer, written) = mpsc::channel(CHANNEL_CAPACITY);
        self.events
            .send(connection_opened(
                id,
                ConnectionOrigin::WebSocket,
                writer,
                Some(CancellationToken::new()),
            ))
            .await
            .unwrap();
        let mut connection = Connection {
            id,
            written,
            passed: VecDeque::new(),
            next_id: 1,
        };
        let params = InitializeParams::new(ClientInfo::new("client", "0.1.0"))
            .with_capabilities(InitializeCapabilities::new().with_experimental_api(true));
        for _ in 0..2 {
            self.send(
                &connection,
                JsonRpcRequest::new(0, "initialize")
                    .with_params(serde_json::to_value(&params).unwrap()),
            )
            .await;
            connection.next().await;
        }
        connection
    }

    /// Loads a coding thread on `script` under `approval`, whose agent may ask for permissions,
    /// and returns its configuration, whose settings keep the grants.
    async fn load(
        &self,
        script: &Arc<Script>,
        approval: AskForApproval,
        connection: &Connection,
    ) -> RunConfig {
        self.load_with_tools(
            script,
            approval,
            connection,
            vec![self.host.request_permissions_tool().unwrap()],
        )
        .await
    }

    async fn load_with_tools(
        &self,
        script: &Arc<Script>,
        approval: AskForApproval,
        connection: &Connection,
        tools: Vec<Arc<dyn Tool>>,
    ) -> RunConfig {
        self.load_in_sandbox(
            script,
            approval,
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            },
            connection,
            tools,
        )
        .await
    }

    /// Loads a coding thread whose commands run in `sandbox`.
    async fn load_in_sandbox(
        &self,
        script: &Arc<Script>,
        approval: AskForApproval,
        sandbox: SandboxPolicy,
        connection: &Connection,
        tools: Vec<Arc<dyn Tool>>,
    ) -> RunConfig {
        let adapter =
            CodingThreadSettingsAdapter::new(self.host.workspace_root(), sandbox).unwrap();
        let config = adapter
            .prepare(
                &adapter.initialize(self.host.build_run_config()),
                &ThreadPolicyOverrides::new().with_approval_policy(approval),
            )
            .unwrap();
        let recorder: Arc<dyn RolloutRecorder> = self
            .store
            .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
                SessionId::new(THREAD),
            )))
            .await
            .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("coder")
            .instructions("code")
            .tools(tools)
            .build()
            .unwrap();
        let thread = AgentThread::builder(
            SessionId::new(THREAD),
            AgentBinding::direct(agent),
            Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        )
        .with_config(config.clone())
        .with_services(self.host.build_tool_services())
        .with_rollout_recorder(recorder)
        .build();
        self.loader
            .load_with_adapter(
                thread,
                &[connection.id],
                Some(Arc::new(adapter) as Arc<dyn ThreadSettingsAdapter>),
            )
            .await;
        config
    }

    async fn send(&self, connection: &Connection, message: impl Into<JsonRpcMessage>) {
        self.events
            .send(TransportEvent::IncomingMessage {
                connection_id: connection.id,
                message: message.into(),
            })
            .await
            .unwrap();
    }

    async fn request(&self, connection: &mut Connection, method: &str, params: Value) -> i64 {
        let id = connection.next_id;
        connection.next_id += 1;
        self.send(
            connection,
            JsonRpcRequest::new(id, method).with_params(params),
        )
        .await;
        id
    }

    async fn turn_start(&self, connection: &mut Connection, text: &str) -> String {
        let params = TurnStartParams::new(THREAD, vec![UserInput::text(text)]);
        let id = self
            .request(
                connection,
                "turn/start",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        connection.response(id).await["turn"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn answer(&self, connection: &Connection, request: &Value, result: Value) {
        let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
        self.send(connection, JsonRpcResponse::new(id, result))
            .await;
    }

    async fn answer_error(&self, connection: &Connection, request: &Value) {
        let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
        self.send(
            connection,
            JsonRpcError::new(id, JsonRpcErrorObject::internal_error("the user went away")),
        )
        .await;
    }
}

struct Connection {
    id: ConnectionId,
    written: mpsc::Receiver<QueuedOutgoingMessage>,
    passed: VecDeque<Value>,
    next_id: i64,
}

impl Connection {
    async fn next(&mut self) -> Value {
        if let Some(message) = self.passed.pop_front() {
            return message;
        }
        let queued = timeout(PATIENCE, self.written.recv())
            .await
            .expect("the server writes a message")
            .expect("the server keeps the write queue");
        serde_json::to_value(queued.message()).unwrap()
    }

    async fn until(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.passed.iter().position(&matches) {
            return self.passed.remove(index).unwrap();
        }
        loop {
            let queued = timeout(PATIENCE, self.written.recv())
                .await
                .expect("the server writes the message waited for")
                .expect("the server keeps the write queue");
            let message = serde_json::to_value(queued.message()).unwrap();
            if matches(&message) {
                return message;
            }
            self.passed.push_back(message);
        }
    }

    async fn response(&mut self, id: i64) -> Value {
        let message = self
            .until(|message| message["id"] == id && message.get("method").is_none())
            .await;
        assert!(message.get("result").is_some(), "{message}");
        message["result"].clone()
    }

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }

    async fn permissions_request(&mut self) -> Value {
        self.until(|message| {
            message["method"] == "item/permissions/requestApproval" && message.get("id").is_some()
        })
        .await
    }

    async fn status(&mut self, status: &Value) {
        self.until(|message| {
            message["method"] == "thread/status/changed" && message["params"]["status"] == *status
        })
        .await;
    }

    /// Everything written to it so far that a case has not read, once the server has had a moment
    /// to write what it was going to.
    async fn drain(&mut self) -> Vec<Value> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut messages: Vec<Value> = self.passed.drain(..).collect();
        while let Ok(queued) = self.written.try_recv() {
            messages.push(serde_json::to_value(queued.message()).unwrap());
        }
        messages
    }
}

fn settings(config: &RunConfig) -> &CodingThreadSettings {
    config
        .app_context()
        .and_then(|value| value.downcast_ref::<CodingThreadSettings>())
        .expect("a coding thread")
}

fn writes(paths: Vec<PathBuf>) -> AdditionalPermissionProfile {
    AdditionalPermissionProfile::new().with_file_system(
        FileSystemPermissions::from_read_write_roots(None, Some(paths)),
    )
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A thread asking for permissions once with `arguments`, the request it sent, and its turn.
struct Asked {
    server: Server,
    connection: Connection,
    script: Arc<Script>,
    config: RunConfig,
    turn_id: String,
    request: Value,
}

async fn ask(arguments: Value) -> Asked {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions("call1", arguments));
    script.push(final_message("done"));
    let config = server.load(&script, on_request(), &connection).await;
    let turn_id = server.turn_start(&mut connection, "pick a directory").await;
    let request = connection.permissions_request().await;
    Asked {
        server,
        connection,
        script,
        config,
        turn_id,
        request,
    }
}

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

fn nested_permission_tool(server: &Server, depth: usize) -> Arc<dyn Tool> {
    let mut tool = server.host.request_permissions_tool().unwrap();
    for level in (0..depth).rev() {
        let name = format!("helper{level}");
        let agent = AgentSpec::builder()
            .id(AgentId::new(&name))
            .name(&name)
            .instructions("delegate or request permissions")
            .tool(tool)
            .build()
            .unwrap();
        tool = Arc::new(agent.as_tool().build().unwrap());
    }
    tool
}

fn nested_permission_calls(script: &Script, depth: usize) {
    for level in 0..depth {
        script.push(ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("helper-item{level}")),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(format!("helper-call{level}")),
                format!("helper{level}"),
                json!({"input": "request permissions"}),
            )),
        )]));
    }
    script.push(request_permissions(
        "nested-grant",
        json!({"permissions": {"file_system": {"write": ["out"]}}}),
    ));
    script.push(request_permissions(
        "nested-wait",
        json!({"permissions": {"network": {"enabled": true}}}),
    ));
    for level in (0..=depth).rev() {
        script.push(final_message(&format!("done{level}")));
    }
}

/// Agent tools share the application's permission state, as they share the reference's context.
/// The RPC and its grant belong to the thread turn, at any nesting depth.
#[tokio::test]
async fn nested_permissions_belong_to_the_thread_turn() {
    for depth in [1, 2] {
        for scope in ["turn", "session"] {
            let server = Server::start();
            let mut connection = server.connect(1).await;
            let script = Arc::new(Script::default());
            nested_permission_calls(&script, depth);
            let config = server
                .load_with_tools(
                    &script,
                    on_request(),
                    &connection,
                    vec![nested_permission_tool(&server, depth)],
                )
                .await;
            let turn_id = server.turn_start(&mut connection, "delegate").await;
            let request = connection.permissions_request().await;
            assert_eq!(request["params"]["turnId"], turn_id);
            assert_eq!(request["params"]["itemId"], "nested-grant");
            let out = server.root().join("out");
            server
                .answer(
                    &connection,
                    &request,
                    json!({"permissions": {"fileSystem": {"write": [text(&out)]}}, "scope": scope}),
                )
                .await;

            let waiting = connection.permissions_request().await;
            assert_eq!(waiting["params"]["turnId"], turn_id);
            let state = settings(&config);
            if scope == "turn" {
                assert_eq!(
                    state.granted_turn_permissions(&RunId::new(&turn_id)),
                    Some(writes(vec![out.clone()]))
                );
                assert_eq!(state.granted_session_permissions(), None);
            } else {
                assert_eq!(
                    state.granted_session_permissions(),
                    Some(writes(vec![out.clone()]))
                );
                assert_eq!(state.granted_turn_permissions(&RunId::new(&turn_id)), None);
            }
            server
                .answer(&connection, &waiting, json!({"permissions": {}}))
                .await;
            connection.notification("turn/completed").await;
            assert_eq!(
                script.last_output("nested-grant"),
                Some(
                    json!({"permissions": {"network": null, "file_system": {"write": [text(&out)]}}, "scope": scope})
                )
            );
            assert_eq!(state.granted_turn_permissions(&RunId::new(&turn_id)), None);

            script.push(final_message("next-turn"));
            let next_turn = server.turn_start(&mut connection, "continue").await;
            connection.notification("turn/completed").await;
            assert_eq!(
                state.granted_turn_permissions(&RunId::new(&next_turn)),
                None
            );
            assert_eq!(
                state.granted_session_permissions(),
                (scope == "session").then(|| writes(vec![out]))
            );
        }
    }
}

/// Cancelling the enclosing turn withdraws a nested request and rejects a late grant.
#[tokio::test]
async fn interrupting_a_nested_permission_request_withdraws_it() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    nested_permission_calls(&script, 2);
    let config = server
        .load_with_tools(
            &script,
            on_request(),
            &connection,
            vec![nested_permission_tool(&server, 2)],
        )
        .await;
    let turn_id = server.turn_start(&mut connection, "delegate").await;
    let request = connection.permissions_request().await;
    let interrupted = server
        .request(
            &mut connection,
            "turn/interrupt",
            json!({"threadId": THREAD, "turnId": turn_id}),
        )
        .await;
    connection.response(interrupted).await;
    let resolved = connection.notification("serverRequest/resolved").await;
    assert_eq!(resolved["requestId"], request["id"]);
    let turn = connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted");
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"network": {"enabled": true}}, "scope": "session"}),
        )
        .await;
    connection.drain().await;
    assert_eq!(script.calls(), 3);
    assert_eq!(settings(&config).granted_session_permissions(), None);
    assert_eq!(
        settings(&config).granted_turn_permissions(&RunId::new(turn_id)),
        None
    );
}

/// Codex rejects clearly foreign absolute paths instead of reinterpreting them as POSIX names.
#[tokio::test]
async fn foreign_absolute_permission_paths_are_refused() {
    for raw in [r"C:\outside", "C:/outside", r"\\server\share\outside"] {
        for file_system in [
            json!({"write": [raw]}),
            json!({"entries": [{"path": {"type": "path", "path": raw}, "access": "read"}]}),
        ] {
            let server = Server::start();
            let mut connection = server.connect(1).await;
            let script = Arc::new(Script::default());
            script.push(request_permissions(
                "call1",
                json!({"permissions": {"file_system": file_system}}),
            ));
            script.push(final_message("done"));
            let config = server.load(&script, on_request(), &connection).await;
            server
                .turn_start(&mut connection, "request a foreign path")
                .await;
            let completed = connection
                .until(|message| {
                    message["method"] == "turn/completed"
                        || message["method"] == "item/permissions/requestApproval"
                })
                .await;
            assert_eq!(completed["method"], "turn/completed", "{completed}");
            assert_eq!(
                script.last_output("call1"),
                Some(Value::String(format!(
                    "path {raw} uses Windows paths, but executor cwd {} uses POSIX paths",
                    server.root().display()
                )))
            );
            assert_eq!(settings(&config).granted_session_permissions(), None);
        }
    }
}

/// A drive-like relative name and a POSIX absolute name remain native POSIX paths.
#[tokio::test]
async fn ambiguous_permission_paths_keep_posix_semantics() {
    let asked = ask(json!({"permissions": {"file_system": {"write": ["C:relative", "/C:/outside", "//server/share"]}}})).await;
    assert_eq!(
        asked.request["params"]["permissions"]["fileSystem"]["write"],
        json!([
            text(&asked.server.root().join("C:relative")),
            "/C:/outside",
            "/server/share"
        ])
    );
}

/// Codex's `request_permissions_round_trip`: the request names the call, its turn and cwd, carries
/// the requested paths resolved against the cwd, and the thread waits on approval meanwhile; the
/// grant is kept for the turn and the model is given it, narrowed to what was requested; the last
/// request is reported resolved before the turn completes, and the turn's grant goes with it.
#[tokio::test]
async fn request_permissions_round_trip() {
    let server = Server::start();
    let root = server.root();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let project_root_entry = json!({
        "path": {"type": "special", "value": {"kind": "project_roots", "subpath": "output"}},
        "access": "write"
    });
    script.push(request_permissions("call1", select_a_workspace_root()));
    script.push(request_permissions(
        "call2",
        json!({"permissions": {"file_system": {"entries": [project_root_entry]}}}),
    ));
    script.push(final_message("done"));
    let config = server.load(&script, on_request(), &connection).await;
    let turn_id = server.turn_start(&mut connection, "pick a directory").await;

    let request = connection.permissions_request().await;
    let shared = root.parent().unwrap().join("shared");
    let params = &request["params"];
    assert_eq!(params["threadId"], THREAD);
    assert_eq!(params["turnId"], turn_id.as_str());
    assert_eq!(params["itemId"], "call1");
    assert_eq!(params["environmentId"], "local");
    assert_eq!(params["cwd"], text(&root));
    assert_eq!(params["reason"], "Select a workspace root");
    assert!(params["startedAtMs"].is_i64());
    assert_eq!(
        params["permissions"]["fileSystem"],
        json!({
            "read": null,
            "write": [text(&root), text(&shared)],
            "entries": [
                {"path": {"type": "path", "path": text(&root)}, "access": "write"},
                {"path": {"type": "path", "path": text(&shared)}, "access": "write"},
            ],
        })
    );
    connection
        .status(&json!({"type": "active", "activeFlags": ["waitingOnApproval"]}))
        .await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"fileSystem": {"write": [text(&root)]}}, "scope": "turn"}),
        )
        .await;

    let request = connection.permissions_request().await;
    assert_eq!(request["params"]["itemId"], "call2");
    assert_eq!(
        settings(&config).granted_turn_permissions(&RunId::new(&turn_id)),
        Some(writes(vec![root.clone()])),
        "the first grant is kept for the turn"
    );
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"fileSystem": {"entries": [
                project_root_entry,
                {"path": {"type": "path", "path": text(&root.join("output"))}, "access": "write"},
            ]}}}),
        )
        .await;
    let mut saw_resolved = false;
    loop {
        let message = connection.next().await;
        match message["method"].as_str() {
            Some("serverRequest/resolved") => {
                saw_resolved |= message["params"]["requestId"] == request["id"];
            }
            Some("turn/completed") => {
                assert!(saw_resolved, "serverRequest/resolved should arrive first");
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        script.last_output("call1"),
        Some(
            json!({"permissions": {"network": null, "file_system": {"write": [text(&root)]}}, "scope": "turn"})
        )
    );
    assert_eq!(
        script.last_output("call2"),
        Some(
            json!({"permissions": {"network": null, "file_system": {"write": [text(&root.join("output"))]}}, "scope": "turn"})
        )
    );
    assert_eq!(
        settings(&config).granted_turn_permissions(&RunId::new(&turn_id)),
        None,
        "a turn's grant ends with it"
    );
    assert_eq!(settings(&config).granted_session_permissions(), None);
}

/// A grant for the session outlives its turn, and network access is given only when both asked
/// for and granted.
#[tokio::test]
async fn a_session_grant_outlives_its_turn() {
    let mut asked = ask(json!({"permissions": {
        "network": {"enabled": true},
        "file_system": {"read": ["data"]},
    }}))
    .await;
    let root = asked.server.root();
    asked
        .server
        .answer(
            &asked.connection,
            &asked.request,
            json!({
                "permissions": {"network": {"enabled": true}, "fileSystem": {"read": [text(&root.join("data"))]}},
                "scope": "session",
            }),
        )
        .await;
    asked.connection.notification("turn/completed").await;

    let granted = AdditionalPermissionProfile::new()
        .with_network(ra_coding::permissions::NetworkPermissions::new(Some(true)))
        .with_file_system(FileSystemPermissions::from_read_write_roots(
            Some(vec![root.join("data")]),
            None,
        ));
    assert_eq!(
        settings(&asked.config).granted_session_permissions(),
        Some(granted)
    );
    assert_eq!(
        settings(&asked.config).granted_turn_permissions(&RunId::new(&asked.turn_id)),
        None
    );
    assert_eq!(
        asked.script.last_output("call1"),
        Some(json!({
            "permissions": {"network": {"enabled": true}, "file_system": {"read": [text(&root.join("data"))]}},
            "scope": "session",
        }))
    );
}

/// Codex's `request_permissions_tool_is_auto_denied_when_granular_request_permissions_is_disabled`,
/// and the same under `never`: nobody is asked, and nothing is granted.
#[tokio::test]
async fn request_permissions_is_auto_denied_when_its_prompts_are_off() {
    for approval in [granular(false), AskForApproval::Never] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(request_permissions("call1", select_a_workspace_root()));
        script.push(final_message("done"));
        let config = server.load(&script, approval, &connection).await;
        server.turn_start(&mut connection, "pick a directory").await;
        connection.notification("turn/completed").await;

        assert!(
            connection
                .drain()
                .await
                .iter()
                .all(|message| message["method"] != "item/permissions/requestApproval"),
            "nobody is asked under {approval:?}"
        );
        assert_eq!(script.last_output("call1"), Some(nothing_granted()));
        assert_eq!(settings(&config).granted_session_permissions(), None);
    }
}

/// Granular approval that lets permission requests prompt asks as `on-request` does.
#[tokio::test]
async fn granular_approval_with_permission_prompts_asks() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions("call1", select_a_workspace_root()));
    script.push(final_message("done"));
    let _ = server.load(&script, granular(true), &connection).await;
    server.turn_start(&mut connection, "pick a directory").await;

    let request = connection.permissions_request().await;
    assert_eq!(request["params"]["itemId"], "call1");
}

/// A request the client answers with an error grants nothing, for the turn, and is reported
/// resolved.
#[tokio::test]
async fn an_error_answer_grants_nothing() {
    let mut asked = ask(select_a_workspace_root()).await;
    asked
        .server
        .answer_error(&asked.connection, &asked.request)
        .await;
    let resolved = asked
        .connection
        .notification("serverRequest/resolved")
        .await;
    assert_eq!(resolved["requestId"], asked.request["id"]);
    asked.connection.notification("turn/completed").await;

    assert_eq!(asked.script.last_output("call1"), Some(nothing_granted()));
}

/// A grant naming a path that is not an absolute local path fails the turn, as Codex's handler
/// does when it cannot localize a granted path: the error is reported, the turn is interrupted,
/// and the model is not called again.
#[tokio::test]
async fn an_unlocalizable_grant_fails_the_turn() {
    let mut asked = ask(select_a_workspace_root()).await;
    asked
        .server
        .answer(
            &asked.connection,
            &asked.request,
            json!({"permissions": {"fileSystem": {"write": ["relative/out"]}}, "scope": "turn"}),
        )
        .await;

    let error = asked.connection.notification("error").await;
    assert_eq!(error["turnId"], asked.turn_id.as_str());
    assert!(
        error["error"]["message"].as_str().unwrap().starts_with(
            "failed to localize granted filesystem paths: invalid native path `relative/out`"
        ),
        "{error}"
    );
    let turn = asked.connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted", "{turn}");
    assert_eq!(asked.script.calls(), 1, "the model is not called again");
}

/// A grant asking for strict automatic review grants nothing: for a session, as Codex refuses it,
/// and for a turn, since Codex's reviewer is not ported.
#[tokio::test]
async fn a_grant_asking_for_strict_review_grants_nothing() {
    for scope in ["turn", "session"] {
        let mut asked = ask(select_a_workspace_root()).await;
        let root = asked.server.root();
        asked
            .server
            .answer(
                &asked.connection,
                &asked.request,
                json!({
                    "permissions": {"fileSystem": {"write": [text(&root)]}},
                    "scope": scope,
                    "strictAutoReview": true,
                }),
            )
            .await;
        asked.connection.notification("turn/completed").await;

        assert_eq!(
            asked.script.last_output("call1"),
            Some(nothing_granted()),
            "{scope}"
        );
        assert_eq!(settings(&asked.config).granted_session_permissions(), None);
    }
}

/// A grant the request does not cover is dropped: the model is given only the part requested.
#[tokio::test]
async fn a_grant_beyond_the_request_is_narrowed() {
    let mut asked = ask(json!({"permissions": {"file_system": {"write": ["out"]}}})).await;
    let root = asked.server.root();
    asked
        .server
        .answer(
            &asked.connection,
            &asked.request,
            json!({"permissions": {
                "network": {"enabled": true},
                "fileSystem": {"write": [text(&root), text(&root.join("out/child"))]},
            }}),
        )
        .await;
    asked.connection.notification("turn/completed").await;

    assert_eq!(
        asked.script.last_output("call1"),
        Some(
            json!({"permissions": {"network": null, "file_system": {"write": [text(&root.join("out/child"))]}}, "scope": "turn"})
        )
    );
}

/// A turn interrupted while the request waits withdraws it, and it is reported resolved.
#[tokio::test]
async fn interrupting_the_turn_withdraws_the_request() {
    let mut asked = ask(select_a_workspace_root()).await;
    let interrupted = asked
        .server
        .request(
            &mut asked.connection,
            "turn/interrupt",
            json!({"threadId": THREAD, "turnId": asked.turn_id}),
        )
        .await;
    asked.connection.response(interrupted).await;
    let resolved = asked
        .connection
        .notification("serverRequest/resolved")
        .await;
    assert_eq!(resolved["requestId"], asked.request["id"]);
    let turn = asked.connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted", "{turn}");
    assert_eq!(asked.script.calls(), 1, "the model is not called again");
}

/// Calls Codex's handler refuses are refused with its words, and nobody is asked.
#[tokio::test]
async fn malformed_calls_are_refused() {
    for (arguments, refusal) in [
        (
            json!({"environment_id": "remote", "permissions": {"network": {"enabled": true}}}),
            "unknown turn environment id `remote`".to_owned(),
        ),
        (
            json!({"permissions": {}}),
            "request_permissions requires at least one permission".to_owned(),
        ),
        (
            json!({"permissions": {"network": {}}}),
            "request_permissions requires at least one permission".to_owned(),
        ),
        (
            json!({"permissions": {"file_system": {"entries": [
                {"path": {"type": "glob_pattern", "pattern": "**/*.env"}, "access": "read"}
            ]}}}),
            "glob file system permissions only support deny-read entries".to_owned(),
        ),
    ] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(request_permissions("call1", arguments.clone()));
        script.push(final_message("done"));
        let _ = server.load(&script, on_request(), &connection).await;
        server.turn_start(&mut connection, "pick a directory").await;
        connection.notification("turn/completed").await;

        assert!(
            connection
                .drain()
                .await
                .iter()
                .all(|message| message["method"] != "item/permissions/requestApproval"),
            "nobody is asked for {arguments}"
        );
        assert_eq!(
            script.last_output("call1"),
            Some(Value::String(refusal)),
            "{arguments}"
        );
    }
}

/// Arguments that do not parse are refused with the parser's words.
#[tokio::test]
async fn unparseable_arguments_are_refused() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "call1",
        json!({"permissions": {"file_system": {"write": ["out"]}, "camera": true}}),
    ));
    script.push(final_message("done"));
    let _ = server.load(&script, on_request(), &connection).await;
    server.turn_start(&mut connection, "pick a directory").await;
    connection.notification("turn/completed").await;

    let Some(Value::String(refusal)) = script.last_output("call1") else {
        panic!("the call is refused");
    };
    assert!(
        refusal.starts_with("failed to parse function arguments: "),
        "{refusal}"
    );
}

// ---------------------------------------------------------------------------------------------
// Grants in later commands
// ---------------------------------------------------------------------------------------------

fn call(tool: &str, call_id: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("item-{call_id}")),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), tool, arguments)),
    )])
}

/// `exec_command`'s arguments, every field the strict schema lists given, `fields` over them.
fn command(fields: Value) -> Value {
    let mut arguments = json!({
        "cmd": null,
        "workdir": null,
        "shell": null,
        "tty": null,
        "login": null,
        "yield_time_ms": 5000,
        "timeout_ms": null,
        "sandbox_permissions": null,
        "justification": null,
        "prefix_rule": null
    });
    for (key, value) in fields.as_object().unwrap() {
        arguments[key] = value.clone();
    }
    arguments
}

/// `write_stdin`'s arguments writing `chars` to `session`, every field the strict schema lists
/// given.
fn stdin(session: &str, chars: &str) -> Value {
    json!({
        "session_id": session,
        "chars": chars,
        "until": null,
        "match_text": null,
        "control": null,
        "yield_time_ms": 500
    })
}

/// A command writing `text` to `path`, then printing it back.
fn write_and_cat(path: &Path, text: &str) -> Value {
    command(json!({"cmd": format!("printf {text} > '{0}' && cat '{0}'", path.display())}))
}

/// Codex's `requested_directory_write_permissions`.
fn directory_write(path: &Path) -> Value {
    json!({"reason": "Allow writing outside the workspace", "permissions": {"file_system": {"write": [text(path)]}}})
}

fn grant_directory_write(path: &Path, scope: &str) -> Value {
    json!({"permissions": {"fileSystem": {"write": [text(path)]}}, "scope": scope})
}

fn command_tools(server: &Server) -> Vec<Arc<dyn Tool>> {
    vec![
        server.host.request_permissions_tool().unwrap(),
        server.host.exec_command_tool().unwrap(),
        server.host.write_stdin_tool().unwrap(),
    ]
}

impl Connection {
    async fn command_request(&mut self) -> Value {
        self.until(|message| {
            message["method"] == "item/commandExecution/requestApproval"
                && message.get("id").is_some()
        })
        .await
    }
}

/// Codex's `request_permissions_grants_apply_to_later_exec_command_calls`: a turn's grant opens
/// its directory to the turn's later commands, which do not ask for it again.
#[tokio::test]
async fn request_permissions_grants_apply_to_later_exec_command_calls() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("sticky-write.txt");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(call(
        "exec_command",
        "exec-call",
        write_and_cat(&written, "sticky-grant-ok"),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    server
        .turn_start(&mut connection, "write outside the workspace")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "turn"),
        )
        .await;
    connection.notification("turn/completed").await;

    let output = script.last_output("exec-call").unwrap();
    assert!(output.to_string().contains("sticky-grant-ok"), "{output}");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "sticky-grant-ok"
    );
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| { message["method"] != "item/commandExecution/requestApproval" }),
        "a granted command does not ask"
    );
}

/// Codex's `request_permissions_grants_do_not_carry_across_turns`: a turn's grant ends with it,
/// and the next turn's command is confined to the thread's sandbox again.
#[tokio::test]
async fn request_permissions_grants_do_not_carry_across_turns() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("later.txt");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(final_message("granted"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    server.turn_start(&mut connection, "ask for later").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "turn"),
        )
        .await;
    connection.notification("turn/completed").await;

    script.push(call(
        "exec_command",
        "exec-call",
        write_and_cat(&written, "should-not-run"),
    ));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "reuse it").await;
    connection.notification("turn/completed").await;

    assert!(!written.exists(), "{:?}", script.last_output("exec-call"));
}

/// Codex's `request_permissions_session_grants_carry_across_turns`: a session's grant reaches the
/// commands of later turns.
#[tokio::test]
async fn request_permissions_session_grants_carry_across_turns() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("session.txt");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(final_message("granted"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    server.turn_start(&mut connection, "ask for later").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "session"),
        )
        .await;
    connection.notification("turn/completed").await;

    script.push(call(
        "exec_command",
        "exec-call",
        write_and_cat(&written, "session-grant-ok"),
    ));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "use it").await;
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "session-grant-ok"
    );
}

/// A command that asks anyway carries the grant in its approval request, as Codex's
/// `request_permissions_grants_apply_to_later_exec_command_calls` expects of one, and is offered
/// only approving once or cancelling.
#[tokio::test]
async fn a_granted_command_that_asks_shows_its_grant() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("asked.txt");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(call(
        "exec_command",
        "exec-call",
        write_and_cat(&written, "asked-ok"),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(
            &script,
            AskForApproval::UnlessTrusted,
            &connection,
            command_tools(&server),
        )
        .await;
    server
        .turn_start(&mut connection, "write outside the workspace")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "turn"),
        )
        .await;

    let request = connection.command_request().await;
    let params = &request["params"];
    assert_eq!(params["itemId"], "exec-call");
    assert_eq!(
        params["additionalPermissions"],
        json!({
            "network": null,
            "fileSystem": {
                "read": null,
                "write": [text(outside.path())],
                "entries": [{"path": {"type": "path", "path": text(outside.path())}, "access": "write"}],
            },
        })
    );
    assert_eq!(params["availableDecisions"], json!(["accept", "cancel"]));
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;

    assert_eq!(std::fs::read_to_string(&written).unwrap(), "asked-ok");
}

/// The executor's sandbox cannot exclude a path from a root it opens, so while a grant denies a
/// path, commands are refused rather than run with it open.
#[tokio::test]
async fn a_grant_that_denies_a_path_refuses_later_commands() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("open.txt");
    let entries = json!([
        {"path": {"type": "path", "path": text(outside.path())}, "access": "write"},
        {"path": {"type": "path", "path": text(&outside.path().join("secret"))}, "access": "deny"},
    ]);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        json!({"permissions": {"file_system": {"entries": entries}}}),
    ));
    script.push(call(
        "exec_command",
        "exec-call",
        write_and_cat(&written, "should-not-run"),
    ));
    script.push(final_message("done"));
    let config = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    let turn_id = server
        .turn_start(&mut connection, "write but not there")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"fileSystem": {"entries": entries}}, "scope": "turn"}),
        )
        .await;
    connection.notification("turn/completed").await;

    let output = script.last_output("exec-call").unwrap().to_string();
    assert!(
        output.contains("the permissions granted to this turn or session deny paths the command sandbox cannot exclude, so the command was not run"),
        "{output}"
    );
    assert!(!written.exists());
    assert_eq!(
        settings(&config).granted_turn_permissions(&RunId::new(&turn_id)),
        None
    );
}

async fn run_with_file_system_grant(
    server: &Server,
    file_system: Value,
    arguments: Value,
) -> Arc<Script> {
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "grant",
        json!({"permissions": {"file_system": file_system}}),
    ));
    script.push(call("exec_command", "exec-call", arguments));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(server))
        .await;
    server
        .turn_start(&mut connection, "use the granted file permissions")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"fileSystem": file_system}, "scope": "turn"}),
        )
        .await;
    connection.notification("turn/completed").await;
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| message["method"] != "item/commandExecution/requestApproval")
    );
    script
}

fn assert_read_only_refusal(script: &Script, written: &Path) {
    let output = script.last_output("exec-call").unwrap().to_string();
    assert!(output.contains("the granted permissions require read-only paths within writable roots that the command sandbox cannot enforce, so the command was not run"), "{output}");
    assert!(!written.exists(), "the read-only path was written");
}

/// Codex preserves read-only children of writable roots. A root-list sandbox cannot do so and
/// must refuse the command, including when the read-only child does not exist yet.
#[tokio::test]
async fn read_only_children_of_granted_writable_roots_refuse_commands() {
    for existing in [true, false] {
        let server = Server::start();
        let outside = tempfile::tempdir().unwrap();
        let child = outside.path().join("readonly");
        if existing {
            std::fs::create_dir(&child).unwrap();
        }
        let written = child.join("must-not-exist");
        let file_system = json!({"entries": [
            {"path": {"type": "path", "path": text(outside.path())}, "access": "write"},
            {"path": {"type": "path", "path": text(&child)}, "access": "read"},
        ]});
        let script = run_with_file_system_grant(&server, file_system, command(json!({"cmd": format!("mkdir -p '{}' && printf unauthorized > '{}'", child.display(), written.display())}))).await;
        assert_read_only_refusal(&script, &written);
    }
}

/// A read grant can constrain the thread's baseline writable root, even with no write grant.
#[tokio::test]
async fn read_only_children_of_baseline_writable_roots_refuse_commands() {
    let server = Server::start();
    let child = server.root().join("readonly");
    std::fs::create_dir(&child).unwrap();
    let written = child.join("must-not-exist");
    let script = run_with_file_system_grant(
        &server,
        json!({"read": [text(&child)]}),
        write_and_cat(&written, "unauthorized"),
    )
    .await;
    assert_read_only_refusal(&script, &written);
}

/// A more specific read survives session/turn merging whichever scope supplied it.
#[tokio::test]
async fn read_only_carveouts_survive_session_and_turn_merging() {
    for session_writes in [true, false] {
        let server = Server::start();
        let outside = tempfile::tempdir().unwrap();
        let child = outside.path().join("readonly");
        std::fs::create_dir(&child).unwrap();
        let written = child.join("must-not-exist");
        let write = json!({"write": [text(outside.path())]});
        let read = json!({"read": [text(&child)]});
        let (session_permissions, turn_permissions) = if session_writes {
            (write, read)
        } else {
            (read, write)
        };
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(request_permissions(
            "session-grant",
            json!({"permissions": {"file_system": session_permissions}}),
        ));
        script.push(final_message("session granted"));
        let config = server
            .load_with_tools(&script, on_request(), &connection, command_tools(&server))
            .await;
        server
            .turn_start(&mut connection, "grant the session")
            .await;
        let request = connection.permissions_request().await;
        server
            .answer(
                &connection,
                &request,
                json!({"permissions": {"fileSystem": session_permissions}, "scope": "session"}),
            )
            .await;
        connection.notification("turn/completed").await;

        script.push(request_permissions(
            "turn-grant",
            json!({"permissions": {"file_system": turn_permissions}}),
        ));
        script.push(call(
            "exec_command",
            "exec-call",
            write_and_cat(&written, "unauthorized"),
        ));
        script.push(final_message("done"));
        let turn_id = server
            .turn_start(&mut connection, "grant the turn, then write")
            .await;
        let request = connection.permissions_request().await;
        server
            .answer(
                &connection,
                &request,
                json!({"permissions": {"fileSystem": turn_permissions}, "scope": "turn"}),
            )
            .await;
        connection.notification("turn/completed").await;
        assert_read_only_refusal(&script, &written);
        assert!(settings(&config).granted_session_permissions().is_some());
        assert_eq!(
            settings(&config).granted_turn_permissions(&RunId::new(&turn_id)),
            None
        );
    }
}

/// Shadowed reads do not create carveouts: an equally specific write wins, including a baseline
/// write, and a more specific writable root under a readable ancestor remains writable.
#[tokio::test]
async fn effective_write_overrides_still_allow_commands() {
    for case in ["equal-grant", "more-specific-grant", "equal-baseline"] {
        let server = Server::start();
        let outside = tempfile::tempdir().unwrap();
        let child = outside.path().join("writable");
        std::fs::create_dir(&child).unwrap();
        let (file_system, written) = match case {
            "equal-grant" => (
                json!({"read": [text(&child)], "write": [text(outside.path()), text(&child)]}),
                child.join("allowed"),
            ),
            "more-specific-grant" => (
                json!({"read": [text(outside.path())], "write": [text(&child)]}),
                child.join("allowed"),
            ),
            _ => (
                json!({"read": [text(&server.root())]}),
                server.root().join("allowed"),
            ),
        };
        let script =
            run_with_file_system_grant(&server, file_system, write_and_cat(&written, "permitted"))
                .await;
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "permitted",
            "{case}: {:?}",
            script.last_output("exec-call")
        );
    }
}

/// Seatbelt matches system aliases in their resolved namespace; spelling a parent through /tmp
/// and its child through /private/tmp must not bypass the read-only carveout check.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn read_only_carveouts_match_system_aliases() {
    for alias_writes in [true, false] {
        let server = Server::start();
        let outside = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = outside.path().canonicalize().unwrap();
        let child = canonical.join("readonly");
        std::fs::create_dir(&child).unwrap();
        let written = child.join("must-not-exist");
        let (write, read) = if alias_writes {
            (outside.path().to_path_buf(), child)
        } else {
            (canonical, outside.path().join("readonly"))
        };
        let script = run_with_file_system_grant(
            &server,
            json!({"read": [text(&read)], "write": [text(&write)]}),
            write_and_cat(&written, "unauthorized"),
        )
        .await;
        assert_read_only_refusal(&script, &written);
    }
}

/// A network grant opens the network to the turn's later commands, which reach a listener on this
/// host only once it is given.
#[tokio::test]
async fn a_network_grant_opens_the_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let probe = command(json!({
        "cmd": format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo network-reached"),
        "shell": "/bin/bash",
    }));
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(call("exec_command", "before", probe.clone()));
    script.push(request_permissions(
        "permissions-call",
        json!({"permissions": {"network": {"enabled": true}}}),
    ));
    script.push(call("exec_command", "after", probe));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    server
        .turn_start(&mut connection, "reach the network")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"}),
        )
        .await;
    connection.notification("turn/completed").await;

    let before = script.last_output("before").unwrap().to_string();
    let after = script.last_output("after").unwrap().to_string();
    assert!(!before.contains("network-reached"), "{before}");
    assert!(after.contains("network-reached"), "{after}");
}

/// A command a nested agent runs gets the grant of the turn that owns it.
#[tokio::test]
async fn a_nested_command_runs_with_its_turns_grant() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("nested.txt");
    let helper = AgentSpec::builder()
        .id(AgentId::new("helper"))
        .name("helper")
        .instructions("run commands")
        .tool(server.host.exec_command_tool().unwrap())
        .build()
        .unwrap();
    let helper: Arc<dyn Tool> = Arc::new(helper.as_tool().build().unwrap());
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(call(
        "helper",
        "helper-call",
        json!({"input": "write the file"}),
    ));
    script.push(call(
        "exec_command",
        "exec-call",
        write_and_cat(&written, "nested-grant-ok"),
    ));
    script.push(final_message("helper done"));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(
            &script,
            on_request(),
            &connection,
            vec![server.host.request_permissions_tool().unwrap(), helper],
        )
        .await;
    server.turn_start(&mut connection, "delegate").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "turn"),
        )
        .await;
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "nested-grant-ok"
    );
}

/// Codex's `stdin_reviews_retained_grants_after_turn_permissions_expire`: a terminal keeps the
/// grant it was launched with after its turn's grants end, so input to it asks, showing the grant,
/// and runs with the grant once approved.
#[tokio::test]
async fn stdin_reviews_retained_grants_after_turn_permissions_expire() {
    let server = Server::start();
    let allowed = tempfile::tempdir().unwrap();
    let result = allowed.path().join("result");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "grant",
        directory_write(allowed.path()),
    ));
    script.push(call(
        "exec_command",
        "open",
        command(json!({"cmd": "sh", "yield_time_ms": 300})),
    ));
    script.push(final_message("opened"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    server.turn_start(&mut connection, "open terminal").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(allowed.path(), "turn"),
        )
        .await;
    connection.notification("turn/completed").await;
    let opened = script.last_output("open").unwrap().to_string();
    let start = opened
        .find("Session `")
        .expect("the terminal keeps running")
        + "Session `".len();
    let session = opened[start..][..opened[start..].find('`').unwrap()].to_owned();

    let grants = format!(
        "{{\"network\":null,\"file_system\":{{\"write\":[{}]}}}}",
        serde_json::to_string(&text(allowed.path())).unwrap()
    );
    for (call_id, chars, answer) in [
        (
            "denied",
            "REJECTED=1\n".to_owned(),
            json!({"decision": "decline"}),
        ),
        (
            "allowed",
            format!(
                "test -z \"${{REJECTED-}}\" && printf allowed > '{}'; exit\n",
                result.display()
            ),
            json!({"decision": "accept"}),
        ),
    ] {
        script.push(call("write_stdin", call_id, stdin(&session, &chars)));
        script.push(final_message(call_id));
        server.turn_start(&mut connection, call_id).await;
        let request = connection.command_request().await;
        let params = &request["params"];
        assert_eq!(params["kind"], "writeStdin");
        assert_eq!(params["itemId"], "open");
        assert_eq!(params["approvalId"], call_id);
        assert_eq!(
            params["reason"],
            format!(
                "Send input to an existing terminal. This terminal retains additional permissions. The cwd is its launch directory; the terminal's current directory and state may have changed. Retained grants: {grants}."
            )
        );
        assert_eq!(
            params["additionalPermissions"]["fileSystem"]["write"],
            json!([text(allowed.path())])
        );
        assert_eq!(params["availableDecisions"], json!(["accept", "cancel"]));
        server.answer(&connection, &request, answer).await;
        connection.notification("turn/completed").await;
    }

    assert_eq!(std::fs::read_to_string(&result).unwrap(), "allowed");
}

/// Codex keys a command's session approval by the permissions it runs with: one approved before a
/// grant asks again once the grant applies, and one approved with the grant does not ask again
/// while the grant holds.
#[tokio::test]
async fn session_approvals_are_kept_per_grant() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let marker = command(json!({"cmd": "touch marker.txt"}));
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let _ = server
        .load_with_tools(
            &script,
            AskForApproval::UnlessTrusted,
            &connection,
            command_tools(&server),
        )
        .await;

    script.push(call("exec_command", "before-grant", marker.clone()));
    script.push(final_message("approved"));
    server.turn_start(&mut connection, "touch").await;
    let request = connection.command_request().await;
    assert_eq!(request["params"]["additionalPermissions"], Value::Null);
    server
        .answer(
            &connection,
            &request,
            json!({"decision": "acceptForSession"}),
        )
        .await;
    connection.notification("turn/completed").await;

    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(call("exec_command", "with-grant", marker.clone()));
    script.push(final_message("approved again"));
    server
        .turn_start(&mut connection, "grant, then touch")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "session"),
        )
        .await;
    let request = connection.command_request().await;
    assert_eq!(request["params"]["itemId"], "with-grant");
    server
        .answer(
            &connection,
            &request,
            json!({"decision": "acceptForSession"}),
        )
        .await;
    connection.notification("turn/completed").await;

    script.push(call("exec_command", "cached", marker.clone()));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "touch again").await;
    connection.notification("turn/completed").await;
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| { message["method"] != "item/commandExecution/requestApproval" }),
        "the approval kept with the grant covers the command"
    );
    assert!(server.root().join("marker.txt").exists());

    let another = tempfile::tempdir().unwrap();
    script.push(request_permissions(
        "more-permissions",
        directory_write(another.path()),
    ));
    script.push(call("exec_command", "wider-grant", marker));
    script.push(final_message("asked once more"));
    server
        .turn_start(&mut connection, "grant more, then touch")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(another.path(), "session"),
        )
        .await;
    let request = connection.command_request().await;
    assert_eq!(request["params"]["itemId"], "wider-grant");
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
}

/// Codex's `apply_granted_turn_permissions` leaves a call that leaves the sandbox alone: it asks
/// for its escalation as it would without a grant, and its request carries no grant.
#[tokio::test]
async fn an_escalated_command_takes_no_grant() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(call(
        "exec_command",
        "escalated",
        command(json!({
            "cmd": "touch escalated.txt",
            "sandbox_permissions": "require_escalated",
            "justification": "Do you want to leave the sandbox?",
        })),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, command_tools(&server))
        .await;
    server.turn_start(&mut connection, "escalate").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "turn"),
        )
        .await;

    let request = connection.command_request().await;
    let params = &request["params"];
    assert_eq!(params["itemId"], "escalated");
    assert_eq!(params["reason"], "Do you want to leave the sandbox?");
    assert_eq!(params["additionalPermissions"], Value::Null);
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
    assert!(server.root().join("escalated.txt").exists());
}

// ---------------------------------------------------------------------------------------------
// Permissions a command asks for itself, and patches the grants cover
// ---------------------------------------------------------------------------------------------

fn read_only() -> SandboxPolicy {
    SandboxPolicy::ReadOnly {
        network_access: false,
    }
}

/// The tools of a host that turned on Codex's `exec_permission_approvals`.
fn inline_tools(server: &Server) -> Vec<Arc<dyn Tool>> {
    let host = server.host.clone().with_exec_permission_approvals();
    vec![
        host.request_permissions_tool().unwrap(),
        host.exec_command_tool().unwrap(),
        host.apply_patch_tool().unwrap(),
    ]
}

/// `exec_command`'s arguments once additional permissions are offered.
fn inline_command(fields: Value) -> Value {
    let mut arguments = command(json!({"additional_permissions": null}));
    for (key, value) in fields.as_object().unwrap() {
        arguments[key] = value.clone();
    }
    arguments
}

/// A command asking for `write` with `with_additional_permissions`.
fn asking_to_write(cmd: &str, write: &[String]) -> Value {
    inline_command(json!({
        "cmd": cmd,
        "sandbox_permissions": "with_additional_permissions",
        "additional_permissions": {"network": null, "file_system": {"read": null, "write": write}},
    }))
}

fn printf_into(path: &Path, text: &str) -> String {
    format!("printf {text} > '{0}' && cat '{0}'", path.display())
}

/// The paths a command approval request says the command may write.
fn requested_writes(request: &Value) -> Vec<String> {
    let mut writes: Vec<String> = request["params"]["additionalPermissions"]["fileSystem"]["write"]
        .as_array()
        .expect("the request carries the permissions")
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    writes.sort();
    writes
}

/// One turn running `call`, whose command asks for approval, answered with `decision`. Returns
/// the approval request.
async fn run_asking(
    sandbox: SandboxPolicy,
    call: Value,
    decision: &str,
) -> (Server, Arc<Script>, Value) {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(self::call("exec_command", "exec-call", call));
    script.push(final_message("done"));
    let _ = server
        .load_in_sandbox(
            &script,
            on_request(),
            sandbox,
            &connection,
            inline_tools(&server),
        )
        .await;
    server.turn_start(&mut connection, "run it").await;
    let request = connection.command_request().await;
    server
        .answer(&connection, &request, json!({"decision": decision}))
        .await;
    connection.notification("turn/completed").await;
    (server, script, request)
}

/// Codex's `with_additional_permissions_requires_approval_under_on_request`: a read-only thread's
/// command asking to write a directory asks, showing what it asks for, and once approved writes
/// there.
#[tokio::test]
async fn with_additional_permissions_requires_approval_under_on_request() {
    let server = Server::start();
    let requested_dir = server.root().join("requested-dir");
    std::fs::create_dir_all(&requested_dir).unwrap();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(call(
        "exec_command",
        "request_permissions_skip_approval",
        asking_to_write(
            "touch requested-dir/requested-but-unused.txt",
            &[text(&requested_dir)],
        ),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_in_sandbox(
            &script,
            on_request(),
            read_only(),
            &connection,
            inline_tools(&server),
        )
        .await;
    server.turn_start(&mut connection, "touch it").await;
    let request = connection.command_request().await;
    assert_eq!(
        request["params"]["itemId"],
        "request_permissions_skip_approval"
    );
    assert_eq!(requested_writes(&request), vec![text(&requested_dir)]);
    assert_eq!(
        request["params"]["availableDecisions"],
        json!(["accept", "cancel"])
    );
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;

    assert!(requested_dir.join("requested-but-unused.txt").exists());
}

/// Codex's `relative_additional_permissions_resolve_against_tool_workdir`.
#[tokio::test]
async fn relative_additional_permissions_resolve_against_tool_workdir() {
    let server = Server::start();
    let nested = server.root().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let mut arguments = asking_to_write("touch relative-write.txt", &[".".to_owned()]);
    arguments["workdir"] = json!("nested");
    script.push(call("exec_command", "exec-call", arguments));
    script.push(final_message("done"));
    let _ = server
        .load_in_sandbox(
            &script,
            on_request(),
            read_only(),
            &connection,
            inline_tools(&server),
        )
        .await;
    server.turn_start(&mut connection, "touch it").await;
    let request = connection.command_request().await;
    assert_eq!(requested_writes(&request), vec![text(&nested)]);
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;

    assert!(nested.join("relative-write.txt").exists());
}

/// Codex's `read_only_with_additional_permissions_does_not_widen_to_unrequested_cwd_write` and
/// `..._tmp_write`: approving what a command asks for opens nothing else.
#[tokio::test]
async fn read_only_with_additional_permissions_does_not_widen_to_unrequested_writes() {
    let tmp = tempfile::tempdir().unwrap();
    for (requested, unrequested) in [
        ("requested-only-cwd.txt", None),
        (
            "requested-only-tmp.txt",
            Some(tmp.path().join("tmp-widening.txt")),
        ),
    ] {
        let server = Server::start();
        let requested = server.root().join(requested);
        let unrequested =
            unrequested.unwrap_or_else(|| server.root().join("unrequested-cwd-write.txt"));
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(call(
            "exec_command",
            "exec-call",
            asking_to_write(&printf_into(&unrequested, "widened"), &[text(&requested)]),
        ));
        script.push(final_message("done"));
        let _ = server
            .load_in_sandbox(
                &script,
                on_request(),
                read_only(),
                &connection,
                inline_tools(&server),
            )
            .await;
        server.turn_start(&mut connection, "write elsewhere").await;
        let request = connection.command_request().await;
        assert_eq!(requested_writes(&request), vec![text(&requested)]);
        server
            .answer(&connection, &request, json!({"decision": "accept"}))
            .await;
        connection.notification("turn/completed").await;

        assert!(
            !unrequested.exists(),
            "{:?}",
            script.last_output("exec-call")
        );
    }
}

/// Codex's `workspace_write_with_additional_permissions_can_write_outside_cwd` and
/// `with_additional_permissions_denied_approval_blocks_execution`.
#[tokio::test]
async fn approving_what_a_command_asks_for_decides_whether_it_runs() {
    for (decision, runs) in [("accept", true), ("decline", false)] {
        let outside = tempfile::tempdir().unwrap();
        let written = outside.path().join("outside-write.txt");
        let (_server, script, request) = run_asking(
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            },
            asking_to_write(
                &printf_into(&written, "outside-ok"),
                &[text(outside.path())],
            ),
            decision,
        )
        .await;
        assert_eq!(requested_writes(&request), vec![text(outside.path())]);
        assert_eq!(
            written.exists(),
            runs,
            "{decision}: {:?}",
            script.last_output("exec-call")
        );
    }
}

/// Codex's `request_permissions_preapprove_explicit_exec_permissions_outside_on_request`: a
/// command asking for what the turn was granted does not ask again.
#[tokio::test]
async fn request_permissions_preapprove_explicit_exec_permissions() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let written = outside.path().join("sticky-explicit-write.txt");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(outside.path()),
    ));
    script.push(call(
        "exec_command",
        "exec-call",
        asking_to_write(
            &printf_into(&written, "sticky-explicit-grant-ok"),
            &[text(outside.path())],
        ),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, inline_tools(&server))
        .await;
    server.turn_start(&mut connection, "write outside").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(outside.path(), "turn"),
        )
        .await;
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "sticky-explicit-grant-ok"
    );
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| { message["method"] != "item/commandExecution/requestApproval" }),
        "a preapproved command does not ask"
    );
}

/// Codex's `partial_request_permissions_grants_do_not_preapprove_new_permissions`: a command
/// asking for more than the turn was granted asks, for the grant and its request together.
#[tokio::test]
async fn partial_request_permissions_grants_do_not_preapprove_new_permissions() {
    let server = Server::start();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let written = second.path().join("partial-grant-write.txt");
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        json!({"permissions": {"file_system": {"write": [text(first.path()), text(second.path())]}}}),
    ));
    script.push(call(
        "exec_command",
        "exec-call",
        asking_to_write(
            &printf_into(&written, "partial-grant-ok"),
            &[text(second.path())],
        ),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, inline_tools(&server))
        .await;
    server.turn_start(&mut connection, "write both").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(first.path(), "turn"),
        )
        .await;
    let request = connection.command_request().await;
    let mut expected = vec![text(first.path()), text(second.path())];
    expected.sort();
    assert_eq!(requested_writes(&request), expected);
    assert_eq!(
        request["params"]["additionalPermissions"]["network"],
        Value::Null
    );
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "partial-grant-ok"
    );
}

/// A command asking for permissions where Codex's handler refuses it is refused with Codex's text
/// and does not run: under `never`, and under any policy but `on-request`.
#[tokio::test]
async fn inline_permissions_are_refused_outside_on_request() {
    for (approval, refusal) in [
        (
            AskForApproval::Never,
            "approval policy is Never; reject command — you cannot ask for escalated permissions if the approval policy is Never",
        ),
        (
            AskForApproval::UnlessTrusted,
            "approval policy is UnlessTrusted; reject command — you cannot request additional permissions unless the approval policy is OnRequest",
        ),
    ] {
        let server = Server::start();
        let outside = tempfile::tempdir().unwrap();
        let written = outside.path().join("refused.txt");
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(call(
            "exec_command",
            "exec-call",
            asking_to_write(&printf_into(&written, "refused"), &[text(outside.path())]),
        ));
        script.push(final_message("done"));
        let _ = server
            .load_with_tools(&script, approval, &connection, inline_tools(&server))
            .await;
        server.turn_start(&mut connection, "write outside").await;
        connection.notification("turn/completed").await;

        let output = script.last_output("exec-call").unwrap().to_string();
        assert!(output.contains(refusal), "{output}");
        assert!(!written.exists());
    }
}

/// `path` spelled through macOS's `/var` alias when it lies below `/private/var`, as a client may
/// name it; elsewhere as it is.
fn aliased(path: &Path) -> String {
    let path = text(path);
    path.strip_prefix("/private/var/")
        .map_or(path.clone(), |rest| format!("/var/{rest}"))
}

fn add_file(path: &str, content: &str) -> Value {
    json!({"patch": format!("*** Begin Patch\n*** Add File: {path}\n+{content}\n*** End Patch\n")})
}

/// Codex's `approved_folder_write_request_permissions_unblocks_later_apply_patch`, inside the
/// workspace this patch tool is confined to: once a read-only turn is granted a directory, a patch
/// writing only there does not ask, while one writing elsewhere, or a path the grant keeps
/// read-only, still does. The grant names its paths through a system alias where there is one.
#[tokio::test]
async fn approved_folder_write_request_permissions_unblocks_later_apply_patch() {
    let server = Server::start();
    let granted = server.root().join("granted");
    std::fs::create_dir_all(&granted).unwrap();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(call(
        "apply_patch",
        "before-grant",
        add_file("granted/early.txt", "early"),
    ));
    script.push(request_permissions(
        "permissions-call",
        json!({"permissions": {"file_system": {"entries": [
            {"path": {"type": "path", "path": aliased(&granted)}, "access": "write"},
            {"path": {"type": "path", "path": aliased(&granted.join("locked.txt"))}, "access": "read"},
        ]}}}),
    ));
    script.push(call(
        "apply_patch",
        "granted-patch",
        add_file("granted/new.txt", "patched"),
    ));
    script.push(call(
        "apply_patch",
        "locked-patch",
        add_file("granted/locked.txt", "locked"),
    ));
    script.push(call(
        "apply_patch",
        "outside-patch",
        add_file("other.txt", "other"),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_in_sandbox(
            &script,
            on_request(),
            read_only(),
            &connection,
            inline_tools(&server),
        )
        .await;
    server.turn_start(&mut connection, "patch").await;

    let file_change = |message: &Value| {
        message["method"] == "item/fileChange/requestApproval" && message.get("id").is_some()
    };
    let request = connection.until(file_change).await;
    assert_eq!(request["params"]["itemId"], "before-grant");
    server
        .answer(&connection, &request, json!({"decision": "decline"}))
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"fileSystem": {"entries": [
                {"path": {"type": "path", "path": aliased(&granted)}, "access": "write"},
                {"path": {"type": "path", "path": aliased(&granted.join("locked.txt"))}, "access": "read"},
            ]}}, "scope": "turn"}),
        )
        .await;
    for item in ["locked-patch", "outside-patch"] {
        let request = connection.until(file_change).await;
        assert_eq!(request["params"]["itemId"], item);
        server
            .answer(&connection, &request, json!({"decision": "decline"}))
            .await;
    }
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(granted.join("new.txt")).unwrap(),
        "patched\n"
    );
    for refused in ["granted/early.txt", "granted/locked.txt", "other.txt"] {
        assert!(!server.root().join(refused).exists(), "{refused}");
    }
}

/// A preapproved update changes only the authorized directory entry, leaving an ungranted file
/// sharing its old inode unchanged. The in-process patch writer cannot rely on a kernel sandbox.
#[tokio::test]
async fn preapproved_patches_do_not_modify_ungranted_hardlinks() {
    let server = Server::start();
    let granted = server.root().join("granted");
    std::fs::create_dir(&granted).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let protected = outside.path().join("protected.txt");
    let alias = granted.join("alias.txt");
    std::fs::write(&protected, "original\n").unwrap();
    std::fs::hard_link(&protected, &alias).unwrap();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions("grant", directory_write(&granted)));
    script.push(call("apply_patch", "patch", json!({"patch": "*** Begin Patch\n*** Update File: granted/alias.txt\n@@\n-original\n+updated\n*** End Patch\n"})));
    script.push(final_message("done"));
    let _ = server
        .load_in_sandbox(
            &script,
            on_request(),
            read_only(),
            &connection,
            inline_tools(&server),
        )
        .await;
    server
        .turn_start(&mut connection, "patch only the granted directory")
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(&granted, "turn"),
        )
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "original\n");
    assert_eq!(std::fs::read_to_string(&alias).unwrap(), "updated\n");
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| message["method"] != "item/fileChange/requestApproval")
    );
}

/// An aliased deny glob must match a patch's canonical target while permitting other files.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn patch_deny_globs_match_system_aliases() {
    for use_alias in [true, false] {
        let server = Server::start();
        let granted = server.root().join("granted");
        std::fs::create_dir(&granted).unwrap();
        let root = if use_alias {
            aliased(&granted)
        } else {
            text(&granted)
        };
        let entries = json!([
            {"path": {"type": "path", "path": root}, "access": "write"},
            {"path": {"type": "glob_pattern", "pattern": format!("{root}/*.env")}, "access": "deny"},
        ]);
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(request_permissions(
            "grant",
            json!({"permissions": {"file_system": {"entries": entries}}}),
        ));
        script.push(call(
            "apply_patch",
            "allowed",
            add_file("granted/public.txt", "allowed"),
        ));
        script.push(call(
            "apply_patch",
            "denied",
            add_file("granted/private.env", "secret"),
        ));
        script.push(final_message("done"));
        let config = server
            .load_in_sandbox(
                &script,
                on_request(),
                read_only(),
                &connection,
                inline_tools(&server),
            )
            .await;
        server
            .turn_start(&mut connection, "keep environment files denied")
            .await;
        let request = connection.permissions_request().await;
        server
            .answer(
                &connection,
                &request,
                json!({"permissions": {"fileSystem": {"entries": entries}}, "scope": "session"}),
            )
            .await;
        let request = connection
            .until(|message| {
                message["method"] == "item/fileChange/requestApproval"
                    || message["method"] == "turn/completed"
            })
            .await;
        assert_eq!(
            request["method"], "item/fileChange/requestApproval",
            "{request}"
        );
        assert_eq!(request["params"]["itemId"], "denied");
        server
            .answer(&connection, &request, json!({"decision": "decline"}))
            .await;
        connection.notification("turn/completed").await;
        assert_eq!(
            std::fs::read_to_string(granted.join("public.txt")).unwrap(),
            "allowed\n"
        );
        assert!(!granted.join("private.env").exists());
        assert!(
            settings(&config)
                .granted_session_permissions()
                .unwrap()
                .file_system()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| entry.access() == ra_coding::permissions::FileSystemAccessMode::Deny)
        );
    }
}

/// A restriction that cannot be translated into the sandbox namespace prevents preapproval;
/// neither an unresolvable deny path nor an ambiguous glob root can be dropped.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn unresolvable_patch_restrictions_prevent_preapproval() {
    for case in ["symlink-deny", "nonliteral-glob-root"] {
        let server = Server::start();
        let granted = server.root().join("granted");
        std::fs::create_dir(&granted).unwrap();
        let restriction = if case == "symlink-deny" {
            std::fs::create_dir(granted.join("secret")).unwrap();
            std::os::unix::fs::symlink("secret", granted.join("link")).unwrap();
            json!({"path": {"type": "path", "path": text(&granted.join("link"))}, "access": "deny"})
        } else {
            json!({"path": {"type": "glob_pattern", "pattern": "/v*/**/*.env"}, "access": "deny"})
        };
        let root = if case == "symlink-deny" {
            text(&granted)
        } else {
            aliased(&granted)
        };
        let entries = json!([
            {"path": {"type": "path", "path": root}, "access": "write"}, restriction,
        ]);
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        script.push(request_permissions(
            "grant",
            json!({"permissions": {"file_system": {"entries": entries}}}),
        ));
        script.push(call(
            "apply_patch",
            "patch",
            add_file("granted/public.txt", "unreviewed"),
        ));
        script.push(final_message("done"));
        let _ = server
            .load_in_sandbox(
                &script,
                on_request(),
                read_only(),
                &connection,
                inline_tools(&server),
            )
            .await;
        server
            .turn_start(&mut connection, "check the complete restrictions")
            .await;
        let request = connection.permissions_request().await;
        server
            .answer(
                &connection,
                &request,
                json!({"permissions": {"fileSystem": {"entries": entries}}, "scope": "turn"}),
            )
            .await;
        let request = connection
            .until(|message| {
                message["method"] == "item/fileChange/requestApproval"
                    || message["method"] == "turn/completed"
            })
            .await;
        assert_eq!(
            request["method"], "item/fileChange/requestApproval",
            "{case}: {request}"
        );
        server
            .answer(&connection, &request, json!({"decision": "decline"}))
            .await;
        connection.notification("turn/completed").await;
        assert!(!granted.join("public.txt").exists());
    }
}

/// Preapproved permissions skip Codex's refusal of sandbox overrides where sandbox prompts are
/// rejected: a granular policy without sandbox approval still runs a granted command, and one
/// asking inline for what was granted, while a command asking for more is refused.
#[tokio::test]
async fn grants_apply_where_sandbox_prompts_are_rejected() {
    let server = Server::start();
    let granted = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(request_permissions(
        "permissions-call",
        directory_write(granted.path()),
    ));
    script.push(call(
        "exec_command",
        "implicit",
        inline_command(
            json!({"cmd": printf_into(&granted.path().join("implicit.txt"), "implicit")}),
        ),
    ));
    script.push(call(
        "exec_command",
        "explicit",
        asking_to_write(
            &printf_into(&granted.path().join("explicit.txt"), "explicit"),
            &[text(granted.path())],
        ),
    ));
    script.push(call(
        "exec_command",
        "beyond",
        asking_to_write(
            &printf_into(&other.path().join("beyond.txt"), "beyond"),
            &[text(other.path())],
        ),
    ));
    script.push(final_message("done"));
    let approval = AskForApproval::Granular {
        sandbox_approval: false,
        rules: true,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: true,
    };
    let _ = server
        .load_with_tools(&script, approval, &connection, inline_tools(&server))
        .await;
    server.turn_start(&mut connection, "write").await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            grant_directory_write(granted.path(), "turn"),
        )
        .await;
    connection.notification("turn/completed").await;

    for name in ["implicit", "explicit"] {
        assert_eq!(
            std::fs::read_to_string(granted.path().join(format!("{name}.txt"))).unwrap(),
            name
        );
    }
    let beyond = script.last_output("beyond").unwrap().to_string();
    assert!(
        beyond.contains("you cannot ask for escalated permissions"),
        "{beyond}"
    );
    assert!(!other.path().join("beyond.txt").exists());
}

/// As Codex's patch handler preapproves it, a patch inside a workspace-write thread's workspace
/// runs without asking once the turn holds any grant, here one for a path elsewhere.
#[tokio::test]
async fn any_grant_preapproves_patches_the_sandbox_already_allows() {
    let server = Server::start();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = text(outside.path());
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(call(
        "apply_patch",
        "before-grant",
        add_file("early.txt", "early"),
    ));
    script.push(request_permissions(
        "permissions-call",
        json!({"permissions": {"file_system": {"write": [outside_path]}}}),
    ));
    script.push(call(
        "apply_patch",
        "after-grant",
        add_file("late.txt", "late"),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_with_tools(&script, on_request(), &connection, inline_tools(&server))
        .await;
    server.turn_start(&mut connection, "patch").await;
    let request = connection
        .until(|message| {
            message["method"] == "item/fileChange/requestApproval" && message.get("id").is_some()
        })
        .await;
    assert_eq!(request["params"]["itemId"], "before-grant");
    server
        .answer(&connection, &request, json!({"decision": "decline"}))
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"fileSystem": {"write": [outside_path]}}, "scope": "turn"}),
        )
        .await;
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(server.root().join("late.txt")).unwrap(),
        "late\n"
    );
    assert!(!server.root().join("early.txt").exists());
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| { message["method"] != "item/fileChange/requestApproval" }),
        "the preapproved patch does not ask"
    );
}

/// A thread whose commands run unconfined can write anywhere, so under `unless-trusted`, where its
/// patches ask, any grant preapproves them, as Codex's patch handler does.
#[tokio::test]
async fn any_grant_preapproves_patches_of_an_unconfined_thread() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    script.push(call(
        "apply_patch",
        "before-grant",
        add_file("early.txt", "early"),
    ));
    script.push(request_permissions(
        "permissions-call",
        json!({"permissions": {"network": {"enabled": true}}}),
    ));
    script.push(call(
        "apply_patch",
        "after-grant",
        add_file("late.txt", "late"),
    ));
    script.push(final_message("done"));
    let _ = server
        .load_in_sandbox(
            &script,
            AskForApproval::UnlessTrusted,
            SandboxPolicy::DangerFullAccess,
            &connection,
            inline_tools(&server),
        )
        .await;
    server.turn_start(&mut connection, "patch").await;
    let request = connection
        .until(|message| {
            message["method"] == "item/fileChange/requestApproval" && message.get("id").is_some()
        })
        .await;
    assert_eq!(request["params"]["itemId"], "before-grant");
    server
        .answer(&connection, &request, json!({"decision": "decline"}))
        .await;
    let request = connection.permissions_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"}),
        )
        .await;
    connection.notification("turn/completed").await;

    assert_eq!(
        std::fs::read_to_string(server.root().join("late.txt")).unwrap(),
        "late\n"
    );
    assert!(!server.root().join("early.txt").exists());
}

#[path = "request_permissions/skills.rs"]
mod skills;
