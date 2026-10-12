//! The coding host's approval requests, as Codex's app-server sends them for its shell and patch
//! tools (`app-server/src/bespoke_event_handling.rs`, `core/src/tools/approvals.rs`): what a
//! command or patch approval says, how each answer reaches the coding runtime — its session cache
//! and its command-prefix rules — and how the call's item ends. They port Codex's `turn_start`
//! approval cases (`turn_start_exec_approval_*`, `turn_start_file_change_approval_*`) where the
//! coding host carries what they exercise.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_coding::{
    exec_policy::{ExecPolicyManager, ExecPolicySources, default_policy_path},
    host::CodingHost,
    thread_items::CodingThreadToolItems,
    thread_settings::CodingThreadSettingsAdapter,
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
    tool::ToolOutput,
};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, RequestId},
    server::{
        AppServer, ThreadSettingsAdapter,
        protocol::{
            ApprovalsReviewer, AskForApproval, ClientInfo, InitializeCapabilities,
            InitializeParams, SandboxPolicy, ThreadItem, ThreadPolicyOverrides,
            ThreadTurnsListParams, ThreadTurnsListResponse, TurnItemsView, TurnStartParams,
            UserInput,
        },
        test_api::ThreadLoader,
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{agent::AgentBinding, thread::AgentThread};
use ra_session::{
    RolloutSessionMeta,
    store::{CreateThreadParams, InMemoryThreadStore, ThreadStore},
};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};
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
    called: Notify,
}

impl Script {
    fn push(&self, response: ModelResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    /// The text of the output of `call_id` the model was last given.
    fn last_output(&self, call_id: &str) -> Option<String> {
        let inputs = self.inputs.lock().unwrap();
        inputs.last()?.iter().find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                Some(output_text(output))
            }
            _ => None,
        })
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
        self.0.called.notify_waiters();
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

fn patch(body: &str) -> Value {
    json!({"patch": format!("*** Begin Patch\n{body}*** End Patch\n")})
}

// ---------------------------------------------------------------------------------------------
// The server in this process, with a coding thread
// ---------------------------------------------------------------------------------------------

/// Writes only the workspace: the temporary directories are excluded, so another temporary
/// directory is outside every writable root.
fn workspace_only() -> SandboxPolicy {
    SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    }
}

fn granular_sandbox_approval() -> AskForApproval {
    AskForApproval::Granular {
        sandbox_approval: true,
        rules: true,
        skill_approval: true,
        request_permissions: true,
        mcp_elicitations: true,
    }
}

struct Server {
    events: mpsc::Sender<TransportEvent>,
    loader: ThreadLoader,
    store: Arc<InMemoryThreadStore>,
    _task: JoinHandle<()>,
    workspace: tempfile::TempDir,
    outside: tempfile::TempDir,
    rules_home: tempfile::TempDir,
    host: CodingHost,
}

impl Server {
    fn start() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let host = CodingHost::open(workspace.path()).unwrap();
        let store = Arc::new(InMemoryThreadStore::new());
        let server = AppServer::new(std::env::temp_dir().join("rusty-agent-coding-approvals"))
            .unwrap()
            .with_thread_store(Arc::clone(&store) as Arc<dyn ThreadStore>)
            .with_thread_tool_items(Arc::new(CodingThreadToolItems::new(host.workspace_root())));
        let loader = ThreadLoader::new(&server);
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            loader,
            store,
            _task: task,
            workspace,
            outside: tempfile::tempdir().unwrap(),
            rules_home: tempfile::tempdir().unwrap(),
            host,
        }
    }

    fn root(&self) -> &Path {
        self.workspace.path()
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

    /// Loads a coding thread on `script` under `approval`, its amendments written under the rules
    /// home if `writes_amendments`.
    async fn load(
        &self,
        script: &Arc<Script>,
        approval: AskForApproval,
        writes_amendments: bool,
        connection: &Connection,
    ) {
        self.load_with(
            script,
            &ThreadPolicyOverrides::new().with_approval_policy(approval),
            writes_amendments,
            connection,
        )
        .await;
    }

    /// Loads a coding thread on `script` with `policy` applied by the adapter itself, as an
    /// embedding host may apply it without the server.
    async fn load_with(
        &self,
        script: &Arc<Script>,
        policy: &ThreadPolicyOverrides,
        writes_amendments: bool,
        connection: &Connection,
    ) {
        let manager = ExecPolicyManager::load(
            &ExecPolicySources::new().with_config_folder(self.rules_home.path()),
        )
        .await
        .unwrap();
        let mut adapter =
            CodingThreadSettingsAdapter::new(self.host.workspace_root(), workspace_only())
                .unwrap()
                .with_exec_policy(Arc::new(manager));
        if writes_amendments {
            adapter = adapter.with_rules_home(self.rules_home.path());
        }
        let config = adapter
            .prepare(&adapter.initialize(self.host.build_run_config()), policy)
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
            .tools(vec![
                self.host.exec_command_tool().unwrap(),
                self.host.write_stdin_tool().unwrap(),
                self.host.apply_patch_tool().unwrap(),
            ])
            .build()
            .unwrap();
        let thread = AgentThread::builder(
            SessionId::new(THREAD),
            AgentBinding::direct(agent),
            Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        )
        .with_config(config)
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

    /// The items of the thread's turns, oldest first, read back from its rollout.
    async fn items_read_back(&self, connection: &mut Connection) -> Vec<Vec<ThreadItem>> {
        let id = self
            .request(
                connection,
                "thread/turns/list",
                serde_json::to_value(
                    ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
                )
                .unwrap(),
            )
            .await;
        let page: ThreadTurnsListResponse =
            serde_json::from_value(connection.response(id).await).unwrap();
        let mut turns: Vec<_> = page
            .data()
            .iter()
            .map(|turn| turn.items().to_vec())
            .collect();
        turns.reverse();
        turns
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

    async fn server_request(&mut self) -> Value {
        self.until(|message| message.get("id").is_some() && message.get("method").is_some())
            .await
    }

    /// The single `item/completed` of `call_id`'s item once its turn has completed.
    async fn turn_end_item(&mut self, call_id: &str) -> Value {
        self.notification("turn/completed").await;
        let items: Vec<_> = self
            .passed
            .iter()
            .filter(|message| {
                message["method"] == "item/completed" && message["params"]["item"]["id"] == call_id
            })
            .map(|message| message["params"]["item"].clone())
            .collect();
        assert_eq!(items.len(), 1, "the call must complete once: {items:?}");
        items.into_iter().next().unwrap()
    }

    fn requests_seen(&self) -> usize {
        self.passed
            .iter()
            .filter(|message| message.get("id").is_some() && message.get("method").is_some())
            .count()
    }
}

fn item_status(items: &[ThreadItem], call_id: &str) -> Value {
    let item = items
        .iter()
        .find(|item| item.id() == call_id)
        .expect("the call's item is read back");
    serde_json::to_value(item).unwrap()["status"].clone()
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_start_exec_approval_toggle_v2`: the command is asked about in the form it is
/// decided in — its shell, `-c` or `-lc`, and the command line — with where it runs, the
/// command-prefix rule its decision proposes and the decisions Codex offers by default. Accepted,
/// it runs.
#[tokio::test]
async fn a_command_approval_says_what_will_run_and_where() {
    for (login, shown) in [
        (false, "/bin/sh -c 'touch made.txt'"),
        (true, "/bin/sh -lc 'touch made.txt'"),
    ] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        server
            .load(&script, AskForApproval::UnlessTrusted, true, &connection)
            .await;
        script.push(call(
            "exec_command",
            "call-1",
            command(json!({"cmd": "touch made.txt", "login": login})),
        ));
        script.push(final_message("done"));

        let turn_id = server.turn_start(&mut connection, "make it").await;
        let request = connection.server_request().await;
        assert_eq!(request["method"], "item/commandExecution/requestApproval");
        let params = &request["params"];
        assert_eq!(params["threadId"], THREAD);
        assert_eq!(params["turnId"], turn_id.as_str());
        assert_eq!(params["itemId"], "call-1");
        assert_eq!(params["environmentId"], "local");
        assert_eq!(params["command"], shown);
        assert_eq!(
            params["cwd"],
            server.host.workspace_root().display().to_string()
        );
        assert_eq!(
            params["commandActions"],
            json!([{"type": "unknown", "command": "touch made.txt"}])
        );
        assert_eq!(
            params["proposedExecpolicyAmendment"],
            json!(["touch", "made.txt"])
        );
        assert_eq!(
            params["availableDecisions"],
            json!([
                "accept",
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["touch", "made.txt"]}},
                "cancel"
            ])
        );

        server
            .answer(&connection, &request, json!({"decision": "accept"}))
            .await;
        let item = connection.turn_end_item("call-1").await;
        assert_eq!(item["status"], "completed", "{item}");
        assert!(server.root().join("made.txt").exists());
    }
}

/// Codex's `turn_start_exec_approval_decline_v2` and `_invalid_response_v2`: the command does not
/// run, the model is told Codex's rejection, and the item ends declined — or failed when no answer
/// could be read — with nothing it ran, live and read back.
#[tokio::test]
async fn a_refused_command_ends_as_its_answer_says() {
    for (answer, status, rejection) in [
        (
            json!({"decision": "decline"}),
            "declined",
            "rejected by user",
        ),
        (
            json!({"unexpected": "response"}),
            "failed",
            "approval request failed",
        ),
    ] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        server
            .load(&script, AskForApproval::UnlessTrusted, true, &connection)
            .await;
        script.push(call(
            "exec_command",
            "call-1",
            command(json!({"cmd": "touch made.txt"})),
        ));
        script.push(final_message("done"));

        server.turn_start(&mut connection, "make it").await;
        let request = connection.server_request().await;
        server.answer(&connection, &request, answer).await;
        let item = connection.turn_end_item("call-1").await;
        assert_eq!(item["status"], status);
        assert!(item["exitCode"].is_null() && item["aggregatedOutput"].is_null());
        assert!(!server.root().join("made.txt").exists());
        assert_eq!(script.last_output("call-1").as_deref(), Some(rejection));
        let turns = server.items_read_back(&mut connection).await;
        assert_eq!(item_status(&turns[0], "call-1"), status);
    }
}

/// A thread whose settings name the automatic reviewer, which the server refuses but an embedding
/// host can still apply through the adapter, asks the client nothing: its call is refused as one
/// whose approval could not be requested, rather than put to the client instead.
#[tokio::test]
async fn a_thread_set_to_auto_review_refuses_its_approvals() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load_with(
            &script,
            &ThreadPolicyOverrides::new()
                .with_approval_policy(AskForApproval::UnlessTrusted)
                .with_approvals_reviewer(ApprovalsReviewer::AutoReview),
            true,
            &connection,
        )
        .await;
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({"cmd": "touch made.txt"})),
    ));
    script.push(final_message("done"));

    server.turn_start(&mut connection, "make it").await;
    let item = connection.turn_end_item("call-1").await;
    assert_eq!(item["status"], "failed");
    assert_eq!(connection.requests_seen(), 0);
    assert!(!server.root().join("made.txt").exists());
    assert_eq!(
        script.last_output("call-1").as_deref(),
        Some("approval request failed")
    );
}

/// An approval for the session is remembered by the thread: the same command later runs without
/// asking, as Codex's session cache has it.
#[tokio::test]
async fn a_command_accepted_for_the_session_is_not_asked_again() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, AskForApproval::UnlessTrusted, true, &connection)
        .await;
    let arguments = command(json!({"cmd": "echo ran >> ran.log"}));
    script.push(call("exec_command", "call-1", arguments.clone()));
    script.push(final_message("first"));
    script.push(call("exec_command", "call-2", arguments));
    script.push(final_message("second"));

    server.turn_start(&mut connection, "first").await;
    let request = connection.server_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"decision": "acceptForSession"}),
        )
        .await;
    connection.notification("turn/completed").await;
    let seen = connection.requests_seen();

    server.turn_start(&mut connection, "second").await;
    connection.notification("turn/completed").await;
    assert_eq!(connection.requests_seen(), seen, "nothing is asked again");
    let log = std::fs::read_to_string(server.root().join("ran.log")).unwrap();
    assert_eq!(log.lines().count(), 2);
}

/// Codex's `approving_execpolicy_amendment_persists_policy_and_skips_future_prompts`: the
/// approved prefix is written to the rules home and allows matching commands from then on.
/// Without a rules home the approval stands and the user is warned, as Codex warns of an
/// amendment it could not save.
#[tokio::test]
async fn an_accepted_amendment_is_saved_or_warned_about() {
    for writes_amendments in [true, false] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        server
            .load(
                &script,
                AskForApproval::UnlessTrusted,
                writes_amendments,
                &connection,
            )
            .await;
        script.push(call(
            "exec_command",
            "call-1",
            command(json!({"cmd": "touch first.txt"})),
        ));
        script.push(final_message("done"));

        server.turn_start(&mut connection, "make it").await;
        let request = connection.server_request().await;
        let amendment = request["params"]["proposedExecpolicyAmendment"].clone();
        server
            .answer(
                &connection,
                &request,
                json!({"decision": {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": amendment}}}),
            )
            .await;
        connection.notification("turn/completed").await;
        assert!(server.root().join("first.txt").exists());

        let rules = default_policy_path(server.rules_home.path());
        if writes_amendments {
            let written = std::fs::read_to_string(&rules).unwrap();
            assert!(written.contains("first.txt"), "{written}");
            assert!(
                !connection
                    .passed
                    .iter()
                    .any(|message| message["method"] == "warning")
            );
        } else {
            assert!(!rules.exists());
            let warning = connection.notification("warning").await;
            assert_eq!(warning["threadId"], THREAD);
            assert_eq!(
                warning["message"],
                "Failed to apply execpolicy amendment: no rules home is configured for this thread"
            );
        }
    }
}

/// Codex's reason for asking: the call's justification when its decision gives none.
#[tokio::test]
async fn an_escalation_is_asked_with_its_justification() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, AskForApproval::OnRequest, true, &connection)
        .await;
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({
            "cmd": "touch made.txt",
            "sandbox_permissions": "require_escalated",
            "justification": "needs the real file system"
        })),
    ));
    script.push(final_message("done"));

    server.turn_start(&mut connection, "make it").await;
    let request = connection.server_request().await;
    assert_eq!(request["params"]["reason"], "needs the real file system");
    server
        .answer(&connection, &request, json!({"decision": "cancel"}))
        .await;
    let turn = connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted");
}

/// After the sandbox refuses a command, the retry is asked with Codex's retry reason. Declined,
/// the command's item — already driven by the refused attempt's own events — ends declined, live
/// and read back.
#[tokio::test]
async fn a_declined_sandbox_retry_ends_its_command_declined() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, granular_sandbox_approval(), true, &connection)
        .await;
    let target = server.outside.path().join("written.txt");
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({"cmd": format!("touch '{}'", target.display())})),
    ));
    script.push(final_message("done"));

    server.turn_start(&mut connection, "write outside").await;
    let request = connection.server_request().await;
    assert_eq!(request["params"]["itemId"], "call-1");
    assert_eq!(
        request["params"]["reason"],
        "command failed; retry without sandbox?"
    );
    server
        .answer(&connection, &request, json!({"decision": "decline"}))
        .await;
    let item = connection.turn_end_item("call-1").await;
    assert_eq!(item["status"], "declined", "{item}");
    assert!(
        item["exitCode"].is_null() && item["aggregatedOutput"].is_null(),
        "the refused attempt's result is not the declined command's: {item}"
    );
    assert!(!target.exists());
    let turns = server.items_read_back(&mut connection).await;
    assert_eq!(item_status(&turns[0], "call-1"), "declined");
}

/// Whether the first prompt approved the command or a later prompt approved the sandbox retry,
/// the refused attempt is not a terminal item. The final successful attempt completes it once.
#[tokio::test]
async fn an_accepted_sandbox_retry_completes_its_command_once() {
    for approval in [granular_sandbox_approval(), AskForApproval::UnlessTrusted] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        server.load(&script, approval, true, &connection).await;
        let target = server.outside.path().join("written.txt");
        script.push(call(
            "exec_command",
            "call-1",
            command(json!({"cmd": format!("touch '{}'", target.display())})),
        ));
        script.push(final_message("done"));
        server.turn_start(&mut connection, "write outside").await;
        let request = connection.server_request().await;
        assert!(
            !connection
                .passed
                .iter()
                .any(|message| message["method"] == "item/completed"
                    && message["params"]["item"]["id"] == "call-1")
        );
        server
            .answer(&connection, &request, json!({"decision": "accept"}))
            .await;
        let item = connection.turn_end_item("call-1").await;
        assert_eq!(item["status"], "completed");
        assert!(target.exists());
    }
}

/// A retry can yield a live session; its eventual process exit still completes the item once.
#[tokio::test]
async fn a_background_sandbox_retry_keeps_its_eventual_completion() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, granular_sandbox_approval(), true, &connection)
        .await;
    let target = server.outside.path().join("written.txt");
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({
            "cmd": format!("touch '{}' && sleep 2", target.display()),
            "yield_time_ms": 200
        })),
    ));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "write outside").await;
    let request = connection.server_request().await;
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
    assert!(target.exists());
    assert!(!connection.passed.iter().any(|message| {
        message["method"] == "item/completed" && message["params"]["item"]["id"] == "call-1"
    }));
    let completed = connection
        .until(|message| {
            message["method"] == "item/completed" && message["params"]["item"]["id"] == "call-1"
        })
        .await;
    assert_eq!(completed["params"]["item"]["status"], "completed");
    connection.passed.push_back(completed);
    let turns = server.items_read_back(&mut connection).await;
    assert_eq!(item_status(&turns[0], "call-1"), "completed");
    assert_eq!(
        connection
            .passed
            .iter()
            .filter(|message| {
                message["method"] == "item/completed" && message["params"]["item"]["id"] == "call-1"
            })
            .count(),
        1
    );
}

/// An accepted retry that cannot spawn still closes the item using the saved failed attempt.
#[cfg(unix)]
#[tokio::test]
async fn a_sandbox_retry_that_cannot_spawn_completes_failed() {
    use std::os::unix::fs::PermissionsExt;

    let server = Server::start();
    let shell = server.root().join("shell");
    std::fs::write(&shell, "#!/bin/sh\nexec /bin/sh \"$@\"\n").unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Warm the executable before measuring its sandboxed launch grace period.
    assert!(
        std::process::Command::new(&shell)
            .args(["-c", "true"])
            .status()
            .unwrap()
            .success()
    );
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, granular_sandbox_approval(), true, &connection)
        .await;
    let target = server.outside.path().join("written.txt");
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({"cmd": format!("touch '{}'", target.display()), "shell": shell})),
    ));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "write outside").await;
    let request = connection.server_request().await;
    assert_eq!(
        request["params"]["reason"],
        "command failed; retry without sandbox?"
    );
    std::fs::remove_file(&shell).unwrap();
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    let item = connection.turn_end_item("call-1").await;
    assert_eq!(item["status"], "failed");
    assert!(!target.exists());
    assert!(
        script
            .last_output("call-1")
            .unwrap()
            .contains("could not be started")
    );
    let turns = server.items_read_back(&mut connection).await;
    assert_eq!(item_status(&turns[0], "call-1"), "failed");
}

/// A policy that does not retry still publishes the refused execution's terminal event.
#[tokio::test]
async fn a_sandbox_denial_without_a_retry_completes_once() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, AskForApproval::OnRequest, true, &connection)
        .await;
    let target = server.outside.path().join("written.txt");
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({"cmd": format!("touch '{}'", target.display())})),
    ));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "write outside").await;
    let item = connection.turn_end_item("call-1").await;
    assert_eq!(item["status"], "failed");
    assert!(!target.exists());
    assert_eq!(connection.requests_seen(), 0);
}

/// A connection attached while approval waits receives the eventual warning, as it receives the
/// replayed request and its resolution. Unsubscribed connections must not receive that warning.
#[tokio::test]
async fn amendment_warnings_follow_current_thread_subscriptions() {
    let server = Server::start();
    let mut first = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, AskForApproval::UnlessTrusted, false, &first)
        .await;
    script.push(call(
        "exec_command",
        "call-1",
        command(json!({"cmd": "touch first.txt"})),
    ));
    script.push(final_message("done"));
    server.turn_start(&mut first, "make it").await;
    let request = first.server_request().await;
    let mut second = server.connect(2).await;
    let id = server
        .request(&mut second, "thread/resume", json!({"threadId": THREAD}))
        .await;
    second.response(id).await;
    let replay = second.server_request().await;
    assert_eq!(replay["id"], request["id"]);
    let unsubscribe = server
        .request(
            &mut first,
            "thread/unsubscribe",
            json!({"threadId": THREAD}),
        )
        .await;
    first.response(unsubscribe).await;
    let amendment = replay["params"]["proposedExecpolicyAmendment"].clone();
    server
        .answer(
            &second,
            &replay,
            json!({"decision": {
                "acceptWithExecpolicyAmendment": {"execpolicy_amendment": amendment}
            }}),
        )
        .await;
    second.notification("turn/completed").await;
    let warning = second.notification("warning").await;
    assert_eq!(warning["threadId"], THREAD);
    assert!(
        warning["message"]
            .as_str()
            .unwrap()
            .starts_with("Failed to apply execpolicy amendment:")
    );
    assert!(server.root().join("first.txt").exists());
    // This response is an ordering barrier behind the warning in the outgoing queue.
    let id = server
        .request(
            &mut first,
            "mock/experimentalMethod",
            json!({"value": "barrier"}),
        )
        .await;
    first.response(id).await;
    assert!(
        !first
            .passed
            .iter()
            .any(|message| message["method"] == "warning")
    );
}

// ---------------------------------------------------------------------------------------------
// Patches
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_start_file_change_approval_v2`, `_decline_v2` and `_invalid_response_v2`: a
/// patch is asked about for its item with no reason; declined or answered with something
/// unreadable, it is not applied and its item ends declined either way, live and read back.
#[tokio::test]
async fn a_refused_patch_ends_declined() {
    for answer in [
        json!({"decision": "decline"}),
        json!({"unexpected": "response"}),
    ] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        server
            .load(&script, AskForApproval::OnRequest, true, &connection)
            .await;
        script.push(call(
            "apply_patch",
            "patch-1",
            patch("*** Add File: added.txt\n+new\n"),
        ));
        script.push(final_message("done"));

        server.turn_start(&mut connection, "patch it").await;
        let request = connection.server_request().await;
        assert_eq!(request["method"], "item/fileChange/requestApproval");
        assert_eq!(request["params"]["itemId"], "patch-1");
        assert!(request["params"].get("reason").is_none_or(Value::is_null));
        server.answer(&connection, &request, answer).await;
        let item = connection.turn_end_item("patch-1").await;
        assert_eq!(item["status"], "declined", "{item}");
        assert!(!server.root().join("added.txt").exists());
        let turns = server.items_read_back(&mut connection).await;
        assert_eq!(item_status(&turns[0], "patch-1"), "declined");
    }
}

/// Codex's `approving_apply_patch_for_session_skips_future_prompts_for_same_file`.
#[tokio::test]
async fn a_patch_accepted_for_the_session_is_not_asked_again_for_its_file() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, AskForApproval::OnRequest, true, &connection)
        .await;
    script.push(call(
        "apply_patch",
        "patch-1",
        patch("*** Add File: notes.txt\n+before\n"),
    ));
    script.push(final_message("first"));
    script.push(call(
        "apply_patch",
        "patch-2",
        patch("*** Update File: notes.txt\n@@\n-before\n+after\n"),
    ));
    script.push(final_message("second"));

    server.turn_start(&mut connection, "first").await;
    let request = connection.server_request().await;
    server
        .answer(
            &connection,
            &request,
            json!({"decision": "acceptForSession"}),
        )
        .await;
    connection.notification("turn/completed").await;
    let seen = connection.requests_seen();

    server.turn_start(&mut connection, "second").await;
    connection.notification("turn/completed").await;
    assert_eq!(connection.requests_seen(), seen, "nothing is asked again");
    assert_eq!(
        std::fs::read_to_string(server.root().join("notes.txt")).unwrap(),
        "after\n"
    );
}

// ---------------------------------------------------------------------------------------------
// Terminal input
// ---------------------------------------------------------------------------------------------

/// Codex's stdin review through the server: once the thread's sandbox changes, input to a terminal
/// launched under the old one is asked about as a command approval of kind `writeStdin` for the
/// launching call's item, naming the input call as its approval, offering approving once or
/// cancelling. Declined, it is not written and the model is told so; accepted, it is written.
#[tokio::test]
async fn input_to_a_terminal_whose_sandbox_changed_is_asked_about() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load(&script, AskForApproval::OnRequest, true, &connection)
        .await;
    script.push(call(
        "exec_command",
        "open",
        command(json!({"cmd": "cat", "yield_time_ms": 300})),
    ));
    script.push(final_message("opened"));
    server.turn_start(&mut connection, "open a terminal").await;
    connection.notification("turn/completed").await;
    let opened = script.last_output("open").unwrap();
    let start = opened
        .find("Session `")
        .expect("the terminal keeps running")
        + "Session `".len();
    let session = opened[start..][..opened[start..].find('`').unwrap()].to_owned();

    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD, "sandboxPolicy": {"type": "readOnly"}}),
        )
        .await;
    connection.response(id).await;

    // Codex's oversized escaped input is refused before a request or any terminal interaction.
    let oversized = format!("#{}\nREJECTED=1\n", "\"".repeat(4_100));
    script.push(call(
        "write_stdin",
        "oversized",
        stdin(&session, &oversized),
    ));
    script.push(final_message("refused"));
    server
        .turn_start(&mut connection, "refuse oversized input")
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(connection.requests_seen(), 0);
    assert_eq!(
        script.last_output("oversized").as_deref(),
        Some(
            "terminal input and permission details are too large to review safely; use a smaller input or start a new terminal with fewer grants"
        )
    );

    for (call_id, chars, answer) in [
        ("declined", "declined\n", json!({"decision": "decline"})),
        ("accepted", "accepted\n", json!({"decision": "accept"})),
    ] {
        script.push(call("write_stdin", call_id, stdin(&session, chars)));
        script.push(final_message(call_id));
        server.turn_start(&mut connection, call_id).await;
        let request = connection.server_request().await;
        assert_eq!(request["method"], "item/commandExecution/requestApproval");
        let params = &request["params"];
        assert_eq!(params["kind"], "writeStdin");
        assert_eq!(params["itemId"], "open");
        assert_eq!(params["approvalId"], call_id);
        assert_eq!(params["environmentId"], "local");
        let shown = params["command"].as_str().unwrap();
        assert!(
            shown.starts_with(&format!("write_stdin --session-id {session} "))
                && shown.contains(call_id),
            "{shown}"
        );
        assert_eq!(
            params["reason"],
            "Send input to an existing terminal. This terminal retains sandbox or network settings that differ from the current permissions. The cwd is its launch directory; the terminal's current directory and state may have changed."
        );
        assert_eq!(params["availableDecisions"], json!(["accept", "cancel"]));
        server.answer(&connection, &request, answer).await;
        connection.notification("turn/completed").await;
    }
    assert_eq!(
        script
            .inputs
            .lock()
            .unwrap()
            .iter()
            .rev()
            .nth(2)
            .and_then(|input| {
                input.iter().find_map(|item| match item {
                    ModelInputItem::ToolCallOutput(output)
                        if output.call_id().as_str() == "declined" =>
                    {
                        Some(output_text(output))
                    }
                    _ => None,
                })
            })
            .as_deref(),
        Some("rejected by user")
    );
    let accepted = script.last_output("accepted").unwrap();
    assert!(
        accepted.contains("accepted")
            && !accepted.contains("declined")
            && !accepted.contains("REJECTED"),
        "{accepted}"
    );
}
