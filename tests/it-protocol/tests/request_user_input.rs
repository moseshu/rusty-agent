//! Questions for the user, as Codex's app-server carries them
//! (`app-server/tests/suite/v2/request_user_input.rs`, the core suite's `request_user_input.rs`,
//! and `thread_status.rs`'s waiting flags): a call of `request_user_input` becomes an
//! `item/tool/requestUserInput` request, the thread waits on the user's input until it is
//! answered, the request is reported resolved, and the model is given the answers.
//!
//! The server runs in this process, and the thread's agent runs on the Responses adapter against a
//! mock endpoint, so the cases read the request bodies the model is sent, as Codex's do. Turns here
//! have no collaboration mode, so the agent's tool is enabled in Default mode, as Codex's
//! `default_mode_request_user_input` feature enables it.

use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::Result,
    model::{
        ApiProtocol, Model, ModelResolver, ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
    },
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_protocol::{
    frame::{
        JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
        RequestId,
    },
    server::{
        AppServer, PreparedThread, ThreadFactory, ThreadFactoryRequest, ThreadSettingsAdapter,
        protocol::{
            ClientInfo, InitializeCapabilities, InitializeParams, ThreadPolicyOverrides,
            TurnStartParams, UserInput,
        },
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{
    agent::{AgentBinding, tool::AgentTool},
    runner::RunConfig,
    thread::{AgentThread, ThreadEvent},
};
use ra_session::store::{InMemoryThreadStore, ThreadStore};
use ra_tools::request_user_input::{RequestUserInputTool, request_user_input_available_modes};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

/// How long a case waits for any one message.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// The model: the Responses adapter against a mock endpoint
// ---------------------------------------------------------------------------------------------

/// A Responses endpoint answering each request with the next scripted stream, and with a final
/// message once the script runs out.
struct Endpoint {
    server: wiremock::MockServer,
    script: Arc<Mutex<VecDeque<String>>>,
}

struct Scripted(Arc<Mutex<VecDeque<String>>>);

impl wiremock::Respond for Scripted {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let body = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| message_stream("done"));
        wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }
}

impl Endpoint {
    async fn start() -> Self {
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/responses"))
            .respond_with(Scripted(Arc::clone(&script)))
            .mount(&server)
            .await;
        Self { server, script }
    }

    /// Answers the next request with a call `call_id` of `tool` with `arguments`.
    fn call_next(&self, call_id: &str, tool: &str, arguments: &Value) {
        let item = json!({
            "id": format!("fc-{call_id}"),
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": tool,
            "arguments": arguments.to_string(),
        });
        self.script.lock().unwrap().push_back(stream(&item));
    }

    /// The bodies of the requests the endpoint has received, in order.
    async fn bodies(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    /// What the model was given for `call_id`, from the first request that carries it.
    async fn output_of(&self, call_id: &str) -> Value {
        self.bodies()
            .await
            .iter()
            .find_map(|body| function_call_output(body, call_id))
            .expect("the call's output reaches the model")
    }
}

fn message_stream(text: &str) -> String {
    stream(&json!({
        "id": "msg-1",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text}]
    }))
}

/// A response whose one output item is `item`, as Codex's `ev_response_created`, an
/// `output_item.done` and `ev_completed` stream it.
fn stream(item: &Value) -> String {
    let frames = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {"id": "resp-1", "status": "in_progress"}
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 1,
            "output_index": 0,
            "item": item
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp-1",
                "object": "response",
                "status": "completed",
                "output": [item],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }
        }),
    ];
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

fn function_call_output(body: &Value, call_id: &str) -> Option<Value> {
    body["input"]
        .as_array()?
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .map(|item| item["output"].clone())
}

/// The text the model was given as a tool's output.
fn output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        other => panic!("unexpected output {other}"),
    }
}

/// Resolves every model to the Responses adapter talking to `base_url`.
struct ResponsesResolver(String);

impl ModelResolver for ResponsesResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        let model = ra_model::openai::responses::OpenAiResponsesModel::new(
            model_name.unwrap_or("mock-model"),
            ra_model::openai::auth::OpenAiAuth::new("test-secret")
                .with_base_url(format!("{}/v1/", self.0)),
        )?;
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                Some(model_name.unwrap_or("mock-model").to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(model) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// The threads and the server in this process
// ---------------------------------------------------------------------------------------------

/// The tool, available in the modes Codex offers it in once Default mode is enabled, or in Plan
/// mode alone.
fn request_user_input(default_mode_enabled: bool) -> Arc<dyn Tool> {
    Arc::new(
        RequestUserInputTool::new(request_user_input_available_modes(default_mode_enabled))
            .unwrap(),
    )
}

fn agent(id: &str, tools: Vec<Arc<dyn Tool>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .instructions(id)
        .tools(tools)
        .build()
        .unwrap()
}

struct Factory {
    base_url: String,
    agent: Arc<AgentSpec>,
    adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
}

#[async_trait]
impl ThreadFactory for Factory {
    async fn prepare_thread(&self, request: &ThreadFactoryRequest) -> Result<PreparedThread> {
        let prepared = PreparedThread::new(
            AgentThread::builder(
                request.thread_id().clone(),
                AgentBinding::direct(Arc::clone(&self.agent)),
                Arc::new(ResponsesResolver(self.base_url.clone())) as Arc<dyn ModelResolver>,
            )
            .with_config(RunConfig::new()),
        );
        Ok(match &self.adapter {
            Some(adapter) => prepared.with_settings_adapter(Arc::clone(adapter)),
            None => prepared,
        })
    }
}

struct Server {
    events: mpsc::Sender<TransportEvent>,
    _task: JoinHandle<()>,
}

impl Server {
    /// A server running threads of `agent` on `endpoint`.
    fn start(endpoint: &Endpoint, agent: Arc<AgentSpec>) -> Self {
        Self::start_with_adapter(endpoint, agent, None)
    }

    fn start_with_adapter(
        endpoint: &Endpoint,
        agent: Arc<AgentSpec>,
        adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
    ) -> Self {
        let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::new());
        let server =
            AppServer::new(std::env::temp_dir().join("rusty-agent-request-user-input-test"))
                .unwrap()
                .with_thread_store(store)
                .with_thread_factory(Arc::new(Factory {
                    base_url: endpoint.server.uri(),
                    agent,
                    adapter,
                }));
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            _task: task,
        }
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
        // Asked twice: the second answer, `Already initialized`, arrives once the router knows.
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

    async fn call(&self, connection: &mut Connection, method: &str, params: Value) -> Value {
        let id = self.request(connection, method, params).await;
        connection.response(id).await
    }

    async fn start_thread(&self, connection: &mut Connection) -> String {
        let started = self.call(connection, "thread/start", json!({})).await;
        started["thread"]["id"].as_str().unwrap().to_owned()
    }

    async fn turn_start(&self, connection: &mut Connection, thread_id: &str, text: &str) -> String {
        let params = TurnStartParams::new(thread_id, vec![UserInput::text(text)]);
        let result = self
            .call(
                connection,
                "turn/start",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        result["turn"]["id"].as_str().unwrap().to_owned()
    }

    /// Answers the server's request `request` with `result`.
    async fn answer(&self, connection: &Connection, request: &Value, result: Value) {
        let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
        self.send(connection, JsonRpcResponse::new(id, result))
            .await;
    }

    /// Answers the server's request `request` with an error.
    async fn answer_error(&self, connection: &Connection, request: &Value) {
        let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
        self.send(
            connection,
            JsonRpcError::new(id, JsonRpcErrorObject::internal_error("the user went away")),
        )
        .await;
    }
}

/// A connection, what the server writes to it, and what a case read past while waiting.
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

    /// The first message `matches` accepts; the others are kept, in order, for later reads.
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

    /// The next `item/tool/requestUserInput` request.
    async fn user_input_request(&mut self) -> Value {
        self.until(|message| {
            message["method"] == "item/tool/requestUserInput" && message.get("id").is_some()
        })
        .await
    }

    /// The next `thread/status/changed` to `status`.
    async fn status(&mut self, status: &Value) {
        self.until(|message| {
            message["method"] == "thread/status/changed" && message["params"]["status"] == *status
        })
        .await;
    }

    /// Forgets what a case read past, so the next reads start after the last message waited for.
    fn forget_read_past(&mut self) {
        self.passed.clear();
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

/// Codex's `create_request_user_input_sse_response` arguments.
fn confirm_arguments() -> Value {
    json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    })
}

fn waiting_on_user_input() -> Value {
    json!({"type": "active", "activeFlags": ["waitingOnUserInput"]})
}

/// A thread whose turn has called `request_user_input` as `call_id` and is waiting on the
/// question it sent.
struct PendingQuestion {
    endpoint: Endpoint,
    server: Server,
    connection: Connection,
    thread_id: String,
    turn_id: String,
    request: Value,
}

async fn ask(call_id: &str) -> PendingQuestion {
    let endpoint = Endpoint::start().await;
    endpoint.call_next(call_id, "request_user_input", &confirm_arguments());
    let server = Server::start(&endpoint, agent("lead", vec![request_user_input(true)]));
    let mut connection = server.connect(1).await;
    let thread_id = server.start_thread(&mut connection).await;
    let turn_id = server
        .turn_start(&mut connection, &thread_id, "ask something")
        .await;
    let request = connection.user_input_request().await;
    PendingQuestion {
        endpoint,
        server,
        connection,
        thread_id,
        turn_id,
        request,
    }
}

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

/// Codex's `request_user_input_default_mode_forwards_non_blocking`: the request names the call and
/// its turn, carries the normalized questions and is not blocking outside Plan mode; the thread
/// waits on the user's input until it is answered; the request is reported resolved before the
/// turn completes; and the model is given the answers as Codex serializes them.
#[tokio::test]
async fn request_user_input_default_mode_forwards_non_blocking() {
    let mut pending = ask("call1").await;
    assert_eq!(
        pending.request["params"],
        json!({
            "threadId": pending.thread_id,
            "turnId": pending.turn_id,
            "itemId": "call1",
            "questions": [{
                "id": "confirm_path",
                "header": "Confirm",
                "question": "Proceed with the plan?",
                "isOther": true,
                "isSecret": false,
                "options": [{
                    "label": "Yes (Recommended)",
                    "description": "Continue the current plan."
                }, {
                    "label": "No",
                    "description": "Stop and revisit the approach."
                }]
            }],
            "isBlocking": false,
            "autoResolutionMs": null,
        })
    );
    pending.connection.status(&waiting_on_user_input()).await;
    pending.connection.forget_read_past();

    pending
        .server
        .answer(
            &pending.connection,
            &pending.request,
            json!({"answers": {"confirm_path": {"answers": ["yes"]}}}),
        )
        .await;
    let mut saw_resolved = false;
    let mut saw_stop_waiting = false;
    loop {
        let message = pending.connection.next().await;
        match message["method"].as_str() {
            Some("serverRequest/resolved") => {
                assert_eq!(message["params"]["threadId"], pending.thread_id);
                assert_eq!(message["params"]["requestId"], pending.request["id"]);
                saw_resolved = true;
            }
            Some("thread/status/changed") => {
                assert_ne!(message["params"]["status"], waiting_on_user_input());
                saw_stop_waiting |=
                    message["params"]["status"] == json!({"type": "active", "activeFlags": []});
            }
            Some("turn/completed") => {
                assert!(saw_resolved, "serverRequest/resolved should arrive first");
                assert!(saw_stop_waiting, "the thread stops waiting once answered");
                assert_eq!(message["params"]["turn"]["status"], "completed");
                break;
            }
            _ => {}
        }
    }

    let output = pending.endpoint.output_of("call1").await;
    assert_eq!(
        serde_json::from_str::<Value>(&output_text(&output)).unwrap(),
        json!({"answers": {"confirm_path": {"answers": ["yes"]}}})
    );
}

/// A question the model marks secret reaches the client secret, beside one it does not.
#[tokio::test]
async fn questions_reach_the_client_as_the_model_marked_them() {
    let endpoint = Endpoint::start().await;
    endpoint.call_next(
        "call-secret",
        "request_user_input",
        &json!({
            "questions": [{
                "id": "token",
                "header": "Token",
                "question": "Which token should I use?",
                "isSecret": true,
                "options": [{"label": "Staging", "description": "The staging token."}]
            }, {
                "id": "region",
                "header": "Region",
                "question": "Which region?",
                "options": [{"label": "EU", "description": "Deploy to the EU."}]
            }]
        }),
    );
    let server = Server::start(&endpoint, agent("lead", vec![request_user_input(true)]));
    let mut connection = server.connect(1).await;
    let thread_id = server.start_thread(&mut connection).await;
    server
        .turn_start(&mut connection, &thread_id, "deploy")
        .await;
    let request = connection.user_input_request().await;

    let questions = request["params"]["questions"].as_array().unwrap();
    assert_eq!(
        questions
            .iter()
            .map(|question| (
                question["id"].clone(),
                question["isSecret"].clone(),
                question["isOther"].clone()
            ))
            .collect::<Vec<_>>(),
        [
            (json!("token"), json!(true), json!(true)),
            (json!("region"), json!(false), json!(true)),
        ]
    );
}

/// The core suite's `request_user_input_rejected_in_default_mode_by_default`: a tool offered only
/// in Plan mode refuses a call in a turn's Default mode, and nobody is asked.
#[tokio::test]
async fn request_user_input_rejected_in_default_mode_by_default() {
    let endpoint = Endpoint::start().await;
    endpoint.call_next("call-default", "request_user_input", &confirm_arguments());
    let server = Server::start(&endpoint, agent("lead", vec![request_user_input(false)]));
    let mut connection = server.connect(1).await;
    let thread_id = server.start_thread(&mut connection).await;
    server
        .turn_start(&mut connection, &thread_id, "please confirm")
        .await;
    connection.notification("turn/completed").await;

    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| message["method"] != "item/tool/requestUserInput"),
        "nobody is asked"
    );
    assert_eq!(
        output_text(&endpoint.output_of("call-default").await),
        "request_user_input is unavailable in Default mode"
    );
}

/// A request the client answers with an error is answered with nothing, as Codex's
/// `on_request_user_input_response` answers it, and still reported resolved.
#[tokio::test]
async fn an_error_answer_is_answered_with_nothing() {
    let mut pending = ask("call-error").await;
    pending
        .server
        .answer_error(&pending.connection, &pending.request)
        .await;
    let resolved = pending
        .connection
        .notification("serverRequest/resolved")
        .await;
    assert_eq!(resolved["requestId"], pending.request["id"]);
    pending.connection.notification("turn/completed").await;

    assert_eq!(
        output_text(&pending.endpoint.output_of("call-error").await),
        r#"{"answers":{}}"#
    );
}

/// A turn interrupted while the question waits withdraws the request, which is reported resolved;
/// the thread stops waiting, a late answer changes nothing, and the model is not called again.
#[tokio::test]
async fn interrupting_the_turn_withdraws_the_question() {
    let mut pending = ask("call-interrupted").await;
    pending.connection.status(&waiting_on_user_input()).await;
    let interrupted = pending
        .server
        .request(
            &mut pending.connection,
            "turn/interrupt",
            json!({"threadId": pending.thread_id, "turnId": pending.turn_id}),
        )
        .await;
    pending.connection.response(interrupted).await;
    let resolved = pending
        .connection
        .notification("serverRequest/resolved")
        .await;
    assert_eq!(resolved["requestId"], pending.request["id"]);
    let turn = pending.connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted", "{turn}");

    pending
        .server
        .answer(
            &pending.connection,
            &pending.request,
            json!({"answers": {"confirm_path": {"answers": ["late"]}}}),
        )
        .await;
    let rest = pending.connection.drain().await;
    assert!(
        rest.iter()
            .all(|message| message["params"]["status"] != waiting_on_user_input()),
        "the thread no longer waits: {rest:?}"
    );
    assert_eq!(
        pending.endpoint.bodies().await.len(),
        1,
        "the model is not called again"
    );
}

/// A connection that attaches while the question waits is sent the request, and its answer reaches
/// the model.
#[tokio::test]
async fn a_connection_attaching_later_is_asked() {
    let pending = ask("call-attach").await;
    let mut late = pending.server.connect(2).await;
    pending
        .server
        .call(
            &mut late,
            "thread/resume",
            json!({"threadId": pending.thread_id}),
        )
        .await;
    let request = late.user_input_request().await;
    assert_eq!(request["id"], pending.request["id"]);
    assert_eq!(request["params"], pending.request["params"]);
    pending
        .server
        .answer(
            &late,
            &request,
            json!({"answers": {"confirm_path": {"answers": ["from the late client"]}}}),
        )
        .await;
    late.notification("turn/completed").await;

    assert_eq!(
        serde_json::from_str::<Value>(&output_text(
            &pending.endpoint.output_of("call-attach").await
        ))
        .unwrap(),
        json!({"answers": {"confirm_path": {"answers": ["from the late client"]}}})
    );
}

/// Codex's `multi_agent_v2_request_user_input_rejects_subagent_threads`, through the runtime: a
/// run an agent-tool call started is no root thread, so its call is refused and nobody is asked.
#[tokio::test]
async fn a_nested_run_may_not_ask_the_user() {
    let endpoint = Endpoint::start().await;
    endpoint.call_next("call-helper", "helper", &json!({"input": "ask the user"}));
    endpoint.call_next("call-nested", "request_user_input", &confirm_arguments());
    let helper = agent("helper", vec![request_user_input(true)]);
    let helper_tool: Arc<dyn Tool> = Arc::new(
        AgentTool::builder(helper)
            .tool_name("helper")
            .tool_description("Asks for help")
            .build()
            .unwrap(),
    );
    let server = Server::start(&endpoint, agent("lead", vec![helper_tool]));
    let mut connection = server.connect(1).await;
    let thread_id = server.start_thread(&mut connection).await;
    server
        .turn_start(&mut connection, &thread_id, "delegate")
        .await;
    connection.notification("turn/completed").await;

    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| message["method"] != "item/tool/requestUserInput"),
        "nobody is asked"
    );
    assert_eq!(
        output_text(&endpoint.output_of("call-nested").await),
        "request_user_input can only be used by the root thread"
    );
}

/// Holds the listener before it applies the turn's start while the tool can still run on another
/// worker. The bounded wait also releases the worker if the test fails before opening the gate.
#[derive(Default)]
struct ListenerGate {
    entered: Notify,
    released: Mutex<bool>,
    release: Condvar,
}

impl ListenerGate {
    fn open(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl ThreadSettingsAdapter for ListenerGate {
    fn prepare(
        &self,
        current: &RunConfig,
        _update: &ThreadPolicyOverrides,
    ) -> std::result::Result<RunConfig, String> {
        Ok(current.clone())
    }

    fn describe(&self, _config: &RunConfig) -> Value {
        json!({})
    }

    fn on_thread_event(&self, _config: &RunConfig, event: &ThreadEvent) {
        if matches!(event, ThreadEvent::TurnStarted { .. }) {
            self.entered.notify_one();
            let (_released, elapsed) = self
                .release
                .wait_timeout_while(self.released.lock().unwrap(), PATIENCE, |released| {
                    !*released
                })
                .unwrap();
            assert!(!elapsed.timed_out(), "the test releases the listener");
        }
    }
}

#[derive(Default)]
struct CallSignals {
    pending: Notify,
    finished: Notify,
}

struct CallFinished<'a>(&'a Notify);

impl Drop for CallFinished<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Signals only after the real tool has polled to its response wait, so opening the listener gate
/// cannot race ahead of the question being queued. Dropping the call also reports cancellation.
struct ObservedInputTool {
    inner: RequestUserInputTool,
    signals: Arc<CallSignals>,
}

#[async_trait]
impl Tool for ObservedInputTool {
    fn origin(&self) -> &ToolOrigin {
        self.inner.origin()
    }

    fn schema(&self) -> &ToolSchema {
        self.inner.schema()
    }

    fn options(&self) -> ToolOptions {
        self.inner.options()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let _finished = CallFinished(&self.signals.finished);
        let mut call = Box::pin(self.inner.call(context));
        let mut signalled = false;
        std::future::poll_fn(|cx| {
            let result = call.as_mut().poll(cx);
            if result.is_pending() && !signalled {
                signalled = true;
                self.signals.pending.notify_one();
            }
            result
        })
        .await
    }

    async fn handle_failure(
        &self,
        context: &ToolContext<'_>,
        error: &ra_core::error::Error,
    ) -> Result<Option<ToolOutput>> {
        self.inner.handle_failure(context, error).await
    }
}

struct QueuedQuestion {
    endpoint: Endpoint,
    server: Server,
    connection: Connection,
    thread_id: String,
    turn_id: String,
    gate: Arc<ListenerGate>,
    signals: Arc<CallSignals>,
}

impl Drop for QueuedQuestion {
    fn drop(&mut self) {
        self.gate.open();
    }
}

async fn queued_question() -> QueuedQuestion {
    let endpoint = Endpoint::start().await;
    endpoint.call_next("call-queued", "request_user_input", &confirm_arguments());
    let gate = Arc::new(ListenerGate::default());
    let signals = Arc::new(CallSignals::default());
    let tool = Arc::new(ObservedInputTool {
        inner: RequestUserInputTool::new(request_user_input_available_modes(true)).unwrap(),
        signals: Arc::clone(&signals),
    });
    let server = Server::start_with_adapter(
        &endpoint,
        agent("lead", vec![tool]),
        Some(Arc::clone(&gate) as Arc<dyn ThreadSettingsAdapter>),
    );
    let mut connection = server.connect(1).await;
    let thread_id = server.start_thread(&mut connection).await;
    let turn_id = server
        .turn_start(&mut connection, &thread_id, "ask something")
        .await;
    let queued = QueuedQuestion {
        endpoint,
        server,
        connection,
        thread_id,
        turn_id,
        gate,
        signals,
    };
    timeout(PATIENCE, queued.gate.entered.notified())
        .await
        .unwrap();
    timeout(PATIENCE, queued.signals.pending.notified())
        .await
        .unwrap();
    queued
}

/// A question queued while the listener lags must follow `turn/started`, survive its request
/// cleanup, and deliver the user's actual answer to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_waits_for_its_turn_started_notification() {
    let mut queued = queued_question().await;
    queued.gate.open();
    let mut saw_started = false;
    let request = loop {
        let message = queued.connection.next().await;
        if message["method"] == "turn/started" {
            saw_started = true;
        }
        if message["method"] == "item/tool/requestUserInput" {
            assert!(saw_started, "the question follows its turn's start");
            break message;
        }
    };
    assert_eq!(request["params"]["turnId"], queued.turn_id);
    queued
        .server
        .answer(
            &queued.connection,
            &request,
            json!({"answers": {"confirm_path": {"answers": ["yes"]}}}),
        )
        .await;
    queued.connection.notification("turn/completed").await;
    assert_eq!(
        output_text(&queued.endpoint.output_of("call-queued").await),
        r#"{"answers":{"confirm_path":{"answers":["yes"]}}}"#,
    );
}

/// Interrupting before a queued question is dispatched must not create a request or revive its
/// waiting flag after the turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_cancelled_while_queued_is_not_sent() {
    let mut queued = queued_question().await;
    let interrupted = queued
        .server
        .request(
            &mut queued.connection,
            "turn/interrupt",
            json!({"threadId": queued.thread_id, "turnId": queued.turn_id}),
        )
        .await;
    timeout(PATIENCE, queued.signals.finished.notified())
        .await
        .unwrap();
    queued.gate.open();
    queued.connection.response(interrupted).await;
    let turn = queued.connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted");
    let messages = queued.connection.drain().await;
    assert!(
        messages.iter().all(|message| {
            message["method"] != "item/tool/requestUserInput"
                && message["params"]["status"] != waiting_on_user_input()
        }),
        "no stale question or waiting state: {messages:?}"
    );
    assert_eq!(queued.endpoint.bodies().await.len(), 1);
}
