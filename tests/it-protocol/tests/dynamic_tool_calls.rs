//! Calls of the tools a client provides, as Codex's app-server carries them
//! (`app-server/tests/suite/v2/dynamic_tools.rs`, `core/src/tools/handlers/dynamic.rs`): the tools
//! `thread/start.dynamicTools` gives the thread's agent, the `item/tool/call` request a call
//! becomes, the call's `DynamicToolCall` item live and read back, what the model is given of the
//! answer, and the tools a resumed or forked thread takes from its history.
//!
//! The server runs in this process, and the thread's agent runs on the Responses adapter against a
//! mock endpoint, so the cases read the request bodies the model is sent, as Codex's do. Codex's
//! deferred tools wait for tool search, so its cases drop them.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec},
    capability::{Capability, CapabilityFamily},
    error::Result,
    model::{
        ApiProtocol, Model, ModelResolver, ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
    },
    session::SessionId,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_protocol::{
    frame::{
        JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
        RequestId,
    },
    server::{
        AppServer, PreparedThread, ThreadFactory, ThreadFactoryRequest,
        protocol::{
            ClientInfo, InitializeCapabilities, InitializeParams, TurnStartParams, UserInput,
        },
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::RunConfig,
    thread::AgentThread,
    turn::prepare::ToolNameCollisionPolicy,
};
use ra_session::{
    RolloutSessionMeta,
    rollout::RolloutPayload,
    store::{CreateThreadParams, InMemoryThreadStore, LoadThreadHistoryParams, ThreadStore},
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

/// How long a case waits for any one message.
const PATIENCE: Duration = Duration::from_secs(10);

const TINY_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const INLINE_AUDIO_DATA_URL: &str = "data:audio/wav;base64,YXVkaW8=";
const INVALID_AUDIO_URL_ERROR: &str = "audio URLs must use an inline data URL";
const REMOTE_IMAGE_URL_ERROR: &str =
    "remote image URLs are not supported; use an inline data URL instead";

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
            .unwrap_or_else(|| message_stream("Done"));
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

    /// Answers the next request with a call `call_id` of `tool` within `namespace`.
    fn call_next_in(&self, call_id: &str, namespace: &str, tool: &str, arguments: &Value) {
        let item = json!({
            "id": format!("fc-{call_id}"),
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "namespace": namespace,
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
// The threads: an agent with tools of its own, on the endpoint
// ---------------------------------------------------------------------------------------------

/// A tool of the agent's own, which answers with its name.
struct OwnTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl OwnTool {
    fn shared(name: &str) -> Arc<dyn Tool> {
        Arc::new(Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::loose(name, json!({"type": "object", "properties": {}}))
                .unwrap()
                .with_description("the agent's own"),
        })
    }
}

#[async_trait]
impl Tool for OwnTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text(format!("{} ran", self.origin.name())))
    }
}

fn agent(id: &str, tools: Vec<Arc<dyn Tool>>, handoffs: Vec<HandoffSpec>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .instructions(id)
        .tools(tools)
        .handoffs(handoffs)
        .build()
        .unwrap()
}

struct Factory {
    base_url: String,
    agent: Arc<AgentSpec>,
    config: RunConfig,
}

#[async_trait]
impl ThreadFactory for Factory {
    async fn prepare_thread(&self, request: &ThreadFactoryRequest) -> Result<PreparedThread> {
        Ok(PreparedThread::new(
            AgentThread::builder(
                request.thread_id().clone(),
                AgentBinding::direct(Arc::clone(&self.agent)),
                Arc::new(ResponsesResolver(self.base_url.clone())) as Arc<dyn ModelResolver>,
            )
            .with_config(self.config.clone()),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

struct Server {
    events: mpsc::Sender<TransportEvent>,
    store: Arc<dyn ThreadStore>,
    _task: JoinHandle<()>,
}

impl Server {
    /// A server on `store`, running threads of `agent` with `config` on `endpoint`.
    fn start(
        endpoint: &Endpoint,
        store: Arc<dyn ThreadStore>,
        agent: Arc<AgentSpec>,
        config: RunConfig,
    ) -> Self {
        let server = AppServer::new(std::env::temp_dir().join("rusty-agent-dynamic-tools-test"))
            .unwrap()
            .with_thread_store(Arc::clone(&store))
            .with_thread_factory(Arc::new(Factory {
                base_url: endpoint.server.uri(),
                agent,
                config,
            }));
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            store,
            _task: task,
        }
    }

    /// A server on a store of its own, running a plain agent on `endpoint`.
    fn plain(endpoint: &Endpoint) -> Self {
        Self::start(
            endpoint,
            Arc::new(InMemoryThreadStore::new()),
            agent("lead", Vec::new(), Vec::new()),
            RunConfig::new(),
        )
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

    async fn call_error(&self, connection: &mut Connection, method: &str, params: Value) -> Value {
        let id = self.request(connection, method, params).await;
        connection.error(id).await
    }

    /// Starts a thread with `dynamic_tools` and returns its id.
    async fn start_thread(&self, connection: &mut Connection, dynamic_tools: Value) -> String {
        let started = self
            .call(
                connection,
                "thread/start",
                json!({"dynamicTools": dynamic_tools}),
            )
            .await;
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

    /// The turns `thread/read` rebuilds from the thread's rollout.
    async fn read_turns(&self, connection: &mut Connection, thread_id: &str) -> Vec<Value> {
        let read = self
            .call(
                connection,
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": true}),
            )
            .await;
        read["thread"]["turns"].as_array().unwrap().clone()
    }

    /// The session metadata the thread's rollout opens with.
    async fn session_meta(&self, thread_id: &str) -> RolloutSessionMeta {
        let history = self
            .store
            .load_history(&LoadThreadHistoryParams::new(SessionId::new(thread_id)))
            .await
            .unwrap();
        history
            .records()
            .iter()
            .find_map(|record| match record.payload() {
                Ok(RolloutPayload::SessionMeta(meta)) => Some(meta),
                _ => None,
            })
            .expect("the rollout opens with session metadata")
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

    async fn error(&mut self, id: i64) -> Value {
        let message = self
            .until(|message| message["id"] == id && message.get("method").is_none())
            .await;
        assert!(message.get("error").is_some(), "{message}");
        message["error"].clone()
    }

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }

    /// The `DynamicToolCall` item of `call_id` that `method` reports.
    async fn dynamic_item(&mut self, method: &str, call_id: &str) -> Value {
        self.until(|message| {
            message["method"] == method
                && message["params"]["item"]["type"] == "dynamicToolCall"
                && message["params"]["item"]["id"] == call_id
        })
        .await["params"]
            .clone()
    }

    /// The next `item/tool/call` request.
    async fn tool_call_request(&mut self) -> Value {
        self.until(|message| message["method"] == "item/tool/call" && message.get("id").is_some())
            .await
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

fn city_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "city": { "type": "string" }
        },
        "required": ["city"],
        "additionalProperties": false,
    })
}

fn demo_tool() -> Value {
    json!({
        "type": "function",
        "name": "demo_tool",
        "description": "Demo dynamic tool",
        "inputSchema": city_schema(),
    })
}

fn find_tool<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["tools"]
        .as_array()?
        .iter()
        .find(|tool| tool["name"] == name)
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn function_call_output(body: &Value, call_id: &str) -> Option<Value> {
    body["input"]
        .as_array()?
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .map(|item| item["output"].clone())
}

fn invalid_request(message: &str) -> Value {
    json!({"code": -32600, "message": message})
}

/// The `DynamicToolCall` item of `call_id` among `turns`.
fn read_back_item(turns: &[Value], call_id: &str) -> Value {
    turns
        .iter()
        .flat_map(|turn| turn["items"].as_array().cloned().unwrap_or_default())
        .find(|item| item["type"] == "dynamicToolCall" && item["id"] == call_id)
        .expect("the call's item reads back")
}

/// A thread with `demo_tool` whose turn has called it with `{"city": "Paris"}` as `call_id`:
/// the started item and the request it was sent, both checked against Codex's.
struct PendingCall {
    endpoint: Endpoint,
    server: Server,
    connection: Connection,
    thread_id: String,
    turn_id: String,
    request: Value,
}

/// Codex's `start_function_dynamic_tool_call`.
async fn start_function_dynamic_tool_call(call_id: &str) -> PendingCall {
    let endpoint = Endpoint::start().await;
    endpoint.call_next(call_id, "demo_tool", &json!({"city": "Paris"}));
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    let thread_id = server
        .start_thread(&mut connection, json!([demo_tool()]))
        .await;
    let turn_id = server
        .turn_start(&mut connection, &thread_id, "Run the tool")
        .await;

    let started = connection.dynamic_item("item/started", call_id).await;
    assert_eq!(started["threadId"], thread_id);
    assert_eq!(started["turnId"], turn_id);
    assert_eq!(
        started["item"],
        json!({
            "type": "dynamicToolCall",
            "id": call_id,
            "namespace": null,
            "tool": "demo_tool",
            "arguments": {"city": "Paris"},
            "status": "inProgress",
            "contentItems": null,
            "success": null,
            "durationMs": null,
        })
    );
    let request = connection.tool_call_request().await;
    assert_eq!(
        request["params"],
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "callId": call_id,
            "namespace": null,
            "tool": "demo_tool",
            "arguments": {"city": "Paris"},
        })
    );
    PendingCall {
        endpoint,
        server,
        connection,
        thread_id,
        turn_id,
        request,
    }
}

impl PendingCall {
    /// Answers the call with `response`, and returns its completed item once the turn completes.
    async fn answer(&mut self, call_id: &str, response: Value) -> Value {
        self.server
            .answer(&self.connection, &self.request, response)
            .await;
        let completed = self
            .connection
            .dynamic_item("item/completed", call_id)
            .await;
        assert_eq!(completed["threadId"], self.thread_id);
        assert_eq!(completed["turnId"], self.turn_id);
        let turn = self.connection.notification("turn/completed").await;
        assert_eq!(turn["turn"]["status"], "completed", "{turn}");
        completed["item"].clone()
    }

    /// What the model was given for `call_id`.
    async fn model_output(&self, call_id: &str) -> Value {
        self.endpoint
            .bodies()
            .await
            .iter()
            .find_map(|body| function_call_output(body, call_id))
            .expect("the model is given the call's output")
    }
}

// ---------------------------------------------------------------------------------------------
// What the model is given
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_start_normalizes_legacy_dynamic_tools_into_model_request`, for a
/// function outside a namespace: a function in either shape reaches the model as Codex describes
/// it, its schema read into the subset and not strict.
#[tokio::test]
async fn thread_start_puts_dynamic_tools_into_model_request() {
    let visible_schema = json!({
        "type": "object",
        "properties": {
            "ticket_id": { "type": "string" }
        },
        "required": ["ticket_id"],
        "additionalProperties": false,
    });
    for dynamic_tools in [
        json!([{
            "name": "lookup_ticket",
            "description": "Look up a ticket",
            "inputSchema": visible_schema,
        }]),
        json!([{
            "type": "function",
            "name": "lookup_ticket",
            "description": "Look up a ticket",
            "inputSchema": visible_schema,
        }]),
    ] {
        let endpoint = Endpoint::start().await;
        let server = Server::plain(&endpoint);
        let mut connection = server.connect(1).await;
        let thread_id = server.start_thread(&mut connection, dynamic_tools).await;
        server
            .turn_start(&mut connection, &thread_id, "Look up the ticket")
            .await;
        connection.notification("turn/completed").await;

        let bodies = endpoint.bodies().await;
        assert_eq!(
            find_tool(&bodies[0], "lookup_ticket"),
            Some(&json!({
                "type": "function",
                "name": "lookup_ticket",
                "description": "Look up a ticket",
                "strict": false,
                "parameters": visible_schema,
            }))
        );
    }
}

/// Codex's `parse_tool_input_schema` is what the model is given, as its handler reads the schema:
/// keywords outside the subset are dropped and a missing type is filled in.
#[tokio::test]
async fn dynamic_tool_schema_is_read_into_the_subset() {
    let endpoint = Endpoint::start().await;
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    let thread_id = server
        .start_thread(
            &mut connection,
            json!([{
                "type": "function",
                "name": "lookup",
                "description": "",
                "inputSchema": {"properties": {"id": {"type": "string", "format": "uuid"}}},
            }]),
        )
        .await;
    server.turn_start(&mut connection, &thread_id, "Go").await;
    connection.notification("turn/completed").await;

    let bodies = endpoint.bodies().await;
    let tool = find_tool(&bodies[0], "lookup").expect("the tool is described");
    assert_eq!(tool["description"], "");
    assert_eq!(tool["strict"], false);
    assert_eq!(
        tool["parameters"],
        json!({"type": "object", "properties": {"id": {"type": "string"}}})
    );
}

/// Codex's registry keeps a tool it already holds and never takes an external tool named
/// `exec_command` or `shell_command`: a dynamic function named like one of the agent's tools, its
/// handoffs or a reserved name is skipped, and the rest are added after the agent's own.
#[tokio::test]
async fn dynamic_tools_named_like_existing_or_reserved_tools_are_skipped() {
    let endpoint = Endpoint::start().await;
    let delegate = agent("delegate", Vec::new(), Vec::new());
    let handoff = HandoffSpec::new(
        AgentId::new("delegate"),
        ToolSchema::new(
            "transfer_to_delegate",
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        )
        .unwrap(),
    );
    let lead = agent("lead", vec![OwnTool::shared("own_tool")], vec![handoff]);
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&lead))
        .register(delegate)
        .build()
        .unwrap();
    endpoint.call_next("own-1", "own_tool", &json!({}));
    let server = Server::start(
        &endpoint,
        Arc::new(InMemoryThreadStore::new()),
        lead,
        RunConfig::new().with_agent_registry(registry),
    );
    let mut connection = server.connect(1).await;
    let function = |name: &str| json!({"type": "function", "name": name, "description": "dynamic", "inputSchema": {"type": "object", "properties": {}}});
    let thread_id = server
        .start_thread(
            &mut connection,
            json!([
                function("own_tool"),
                function("transfer_to_delegate"),
                function("exec_command"),
                function("shell_command"),
                function("client_tool"),
            ]),
        )
        .await;
    server.turn_start(&mut connection, &thread_id, "Go").await;
    connection.notification("turn/completed").await;

    let bodies = endpoint.bodies().await;
    assert_eq!(
        tool_names(&bodies[0]),
        ["own_tool", "client_tool", "transfer_to_delegate"]
    );
    assert_eq!(
        find_tool(&bodies[0], "own_tool").unwrap()["description"],
        "the agent's own"
    );
    // The agent's own tool still answers its calls, and nothing is asked of the client.
    assert_eq!(
        function_call_output(&bodies[1], "own-1"),
        Some(json!([{"type": "input_text", "text": "own_tool ran"}]))
    );
    assert!(
        connection
            .drain()
            .await
            .iter()
            .all(|message| message["method"] != "item/tool/call")
    );
}

/// Supplies a different tool on each run; its unbound declaration must never be inspected.
struct RunBoundTools {
    binds: Arc<Mutex<usize>>,
}

struct BoundTool(Arc<dyn Tool>);

#[async_trait]
impl Capability for BoundTool {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("dynamic_tool_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::clone(&self.0)]
    }
}

#[async_trait]
impl Capability for RunBoundTools {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("dynamic_tool_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        panic!("only the run-bound capability supplies tools")
    }

    fn bind(&self, _context: &ra_core::context::RunContext) -> Result<Option<Arc<dyn Capability>>> {
        let mut binds = self.binds.lock().unwrap();
        let name = if *binds == 0 {
            "demo_tool"
        } else {
            "bound_tool"
        };
        *binds += 1;
        Ok(Some(Arc::new(BoundTool(OwnTool::shared(name)))))
    }
}

/// Codex registers external tools after the host's tools. A run-bound capability wins a
/// collision without failing the turn, and the next run reconsiders its own bound tool set.
#[tokio::test]
async fn run_bound_tools_take_precedence_over_dynamic_tools() {
    for policy in [
        ToolNameCollisionPolicy::Warn,
        ToolNameCollisionPolicy::Error,
    ] {
        let endpoint = Endpoint::start().await;
        let binds = Arc::new(Mutex::new(0));
        let server = Server::start(
            &endpoint,
            Arc::new(InMemoryThreadStore::new()),
            agent("lead", Vec::new(), Vec::new()),
            RunConfig::new()
                .with_tool_name_collision_policy(policy)
                .with_capability(Arc::new(RunBoundTools {
                    binds: Arc::clone(&binds),
                })),
        );
        let mut connection = server.connect(1).await;
        let thread_id = server
            .start_thread(&mut connection, json!([demo_tool()]))
            .await;

        endpoint.call_next("bound-1", "demo_tool", &json!({}));
        server
            .turn_start(&mut connection, &thread_id, "First")
            .await;
        let completed = connection.notification("turn/completed").await;
        assert_eq!(completed["turn"]["status"], "completed", "{completed}");
        let bodies = endpoint.bodies().await;
        assert_eq!(tool_names(&bodies[0]), ["demo_tool"]);
        assert_eq!(
            find_tool(&bodies[0], "demo_tool").unwrap()["description"],
            "the agent's own"
        );
        assert_eq!(
            function_call_output(&bodies[1], "bound-1"),
            Some(json!([
                {"type": "input_text", "text": "demo_tool ran"}
            ]))
        );
        assert!(
            connection
                .drain()
                .await
                .iter()
                .all(|message| message["method"] != "item/tool/call")
        );

        endpoint.call_next("dynamic-2", "demo_tool", &json!({"city": "Paris"}));
        server
            .turn_start(&mut connection, &thread_id, "Second")
            .await;
        let request = connection.tool_call_request().await;
        assert_eq!(request["params"]["callId"], "dynamic-2");
        server.answer(&connection, &request, json!({
            "contentItems": [{"type": "inputText", "text": "from the client"}], "success": true
        })).await;
        let completed = connection.notification("turn/completed").await;
        assert_eq!(completed["turn"]["status"], "completed", "{completed}");
        let bodies = endpoint.bodies().await;
        assert_eq!(tool_names(&bodies[2]), ["bound_tool", "demo_tool"]);
        assert_eq!(
            function_call_output(&bodies[3], "dynamic-2"),
            Some(json!([
                {"type": "input_text", "text": "from the client"}
            ]))
        );
        assert_eq!(*binds.lock().unwrap(), 2);
    }
}

// ---------------------------------------------------------------------------------------------
// A call and its answer
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `dynamic_tool_call_round_trip_sends_text_content_items_to_model`, for a
/// function outside a namespace: the call's item starts, the client is asked, its answer completes
/// the item and reaches the model. The Responses adapter gives the model a content array where
/// Codex gives a lone text as a string.
#[tokio::test]
async fn dynamic_tool_call_round_trip_sends_text_content_items_to_model() {
    let call_id = "dyn-call-1";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending
        .answer(
            call_id,
            json!({"contentItems": [{"type": "inputText", "text": "dynamic-ok"}], "success": true}),
        )
        .await;
    assert!(completed["durationMs"].is_i64(), "{completed}");
    assert_eq!(
        completed,
        json!({
            "type": "dynamicToolCall",
            "id": call_id,
            "namespace": null,
            "tool": "demo_tool",
            "arguments": {"city": "Paris"},
            "status": "completed",
            "contentItems": [{"type": "inputText", "text": "dynamic-ok"}],
            "success": true,
            "durationMs": completed["durationMs"],
        })
    );
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": "dynamic-ok"}])
    );

    // Codex does not report a dynamic tool call's request resolved.
    assert!(
        pending
            .connection
            .drain()
            .await
            .iter()
            .all(|message| message["method"] != "serverRequest/resolved")
    );
    // The item reads back as it was sent.
    let turns = pending
        .server
        .read_turns(&mut pending.connection, &pending.thread_id)
        .await;
    assert_eq!(read_back_item(&turns, call_id), completed);
}

/// Ported from Codex's `dynamic_tool_call_round_trip_handles_content_items`, without audio: text
/// and images reach the model, an image at Codex's default detail.
#[tokio::test]
async fn dynamic_tool_call_round_trip_handles_content_items() {
    let call_id = "dyn-call-items-1";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let content_items = json!([
        {"type": "inputText", "text": "dynamic-ok"},
        {"type": "inputImage", "imageUrl": TINY_PNG_DATA_URL},
    ]);
    let completed = pending
        .answer(
            call_id,
            json!({"contentItems": content_items, "success": true}),
        )
        .await;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["contentItems"], content_items);
    assert_eq!(completed["success"], true);
    assert_eq!(
        pending.model_output(call_id).await,
        json!([
            {"type": "input_text", "text": "dynamic-ok"},
            {"type": "input_image", "image_url": TINY_PNG_DATA_URL, "detail": "high"},
        ])
    );
}

/// A client that says its tool failed fails the call's item; the model is given what it returned.
#[tokio::test]
async fn dynamic_tool_failure_fails_the_item() {
    let call_id = "dyn-call-failed";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending
        .answer(
            call_id,
            json!({"contentItems": [{"type": "inputText", "text": "no such city"}], "success": false}),
        )
        .await;
    assert_eq!(completed["status"], "failed");
    assert_eq!(
        completed["contentItems"],
        json!([{"type": "inputText", "text": "no such city"}])
    );
    assert_eq!(completed["success"], false);
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": "no such city"}])
    );
}

/// Ported from Codex's `dynamic_tool_remote_image_response_becomes_model_visible_error`.
#[tokio::test]
async fn dynamic_tool_remote_image_response_becomes_model_visible_error() {
    let call_id = "dyn-call-remote-image";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending
        .answer(
            call_id,
            json!({
                "contentItems": [{"type": "inputImage", "imageUrl": "https://example.com/tool.png"}],
                "success": true
            }),
        )
        .await;
    assert_eq!(completed["status"], "failed");
    assert_eq!(
        completed["contentItems"],
        json!([{"type": "inputText", "text": REMOTE_IMAGE_URL_ERROR}])
    );
    assert_eq!(completed["success"], false);
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": REMOTE_IMAGE_URL_ERROR}])
    );
}

/// Ported from Codex's `dynamic_tool_remote_audio_response_becomes_model_visible_error`.
#[tokio::test]
async fn dynamic_tool_remote_audio_response_becomes_model_visible_error() {
    let call_id = "dyn-call-remote-audio";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending
        .answer(
            call_id,
            json!({
                "contentItems": [{"type": "inputAudio", "audioUrl": "https://example.com/tool.wav"}],
                "success": true
            }),
        )
        .await;
    assert_eq!(completed["status"], "failed");
    assert_eq!(
        completed["contentItems"],
        json!([{"type": "inputText", "text": INVALID_AUDIO_URL_ERROR}])
    );
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": INVALID_AUDIO_URL_ERROR}])
    );
}

/// Inline audio, which Codex passes to the model, is refused until tool results can carry it.
#[tokio::test]
async fn dynamic_tool_inline_audio_is_refused() {
    let call_id = "dyn-call-audio";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending
        .answer(
            call_id,
            json!({
                "contentItems": [
                    {"type": "inputText", "text": "dynamic-ok"},
                    {"type": "inputAudio", "audioUrl": INLINE_AUDIO_DATA_URL},
                ],
                "success": true
            }),
        )
        .await;
    let refusal = "dynamic tool audio output is not supported";
    assert_eq!(completed["status"], "failed");
    assert_eq!(
        completed["contentItems"],
        json!([{"type": "inputText", "text": refusal}])
    );
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": refusal}])
    );
}

/// Codex's `on_call_response`: an error answer, or one that does not read as a response, fails the
/// call with Codex's texts; an answer with no content gives the model empty text.
#[tokio::test]
async fn unusable_answers_fail_the_call() {
    let call_id = "dyn-call-error";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let id: RequestId = serde_json::from_value(pending.request["id"].clone()).unwrap();
    pending
        .server
        .send(
            &pending.connection,
            JsonRpcError::new(id, JsonRpcErrorObject::new(-32000, "client failed")),
        )
        .await;
    let completed = pending
        .connection
        .dynamic_item("item/completed", call_id)
        .await;
    assert_eq!(completed["item"]["status"], "failed");
    assert_eq!(
        completed["item"]["contentItems"],
        json!([{"type": "inputText", "text": "dynamic tool request failed"}])
    );
    pending.connection.notification("turn/completed").await;

    let call_id = "dyn-call-invalid";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending.answer(call_id, json!({"content": "nope"})).await;
    assert_eq!(completed["status"], "failed");
    assert_eq!(
        completed["contentItems"],
        json!([{"type": "inputText", "text": "dynamic tool response was invalid"}])
    );

    let call_id = "dyn-call-empty";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let completed = pending
        .answer(call_id, json!({"contentItems": [], "success": true}))
        .await;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["contentItems"], json!([]));
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": ""}])
    );
}

/// A turn interrupted while the client runs the tool ends the call as Codex's handler does when
/// its answer will not come: failed, with nothing returned. A late answer changes nothing, and the
/// item reads back as it was sent.
#[tokio::test]
async fn interrupting_the_turn_fails_the_waiting_call() {
    let call_id = "dyn-call-interrupted";
    let mut pending = start_function_dynamic_tool_call(call_id).await;
    let interrupted = pending
        .server
        .request(
            &mut pending.connection,
            "turn/interrupt",
            json!({"threadId": pending.thread_id, "turnId": pending.turn_id}),
        )
        .await;
    let completed = pending
        .connection
        .dynamic_item("item/completed", call_id)
        .await;
    assert_eq!(completed["turnId"], pending.turn_id);
    assert!(completed["item"]["durationMs"].is_i64());
    assert_eq!(
        completed["item"],
        json!({
            "type": "dynamicToolCall",
            "id": call_id,
            "namespace": null,
            "tool": "demo_tool",
            "arguments": {"city": "Paris"},
            "status": "failed",
            "contentItems": [],
            "success": false,
            "durationMs": completed["item"]["durationMs"],
        })
    );
    pending.connection.response(interrupted).await;
    let turn = pending.connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted", "{turn}");

    pending
        .server
        .answer(
            &pending.connection,
            &pending.request,
            json!({"contentItems": [{"type": "inputText", "text": "late"}], "success": true}),
        )
        .await;
    assert!(
        pending
            .connection
            .drain()
            .await
            .iter()
            .all(|message| message["params"]["item"]["id"] != call_id),
        "a late answer completes nothing"
    );
    let turns = pending
        .server
        .read_turns(&mut pending.connection, &pending.thread_id)
        .await;
    assert_eq!(read_back_item(&turns, call_id), completed["item"]);
    assert_eq!(
        pending.endpoint.bodies().await.len(),
        1,
        "the model is not called again"
    );
}

/// A connection that attaches while the call waits is sent its request, and its answer completes
/// the call.
#[tokio::test]
async fn a_connection_attaching_later_is_asked() {
    let call_id = "dyn-call-attach";
    let pending = start_function_dynamic_tool_call(call_id).await;
    let mut late = pending.server.connect(2).await;
    pending
        .server
        .call(
            &mut late,
            "thread/resume",
            json!({"threadId": pending.thread_id}),
        )
        .await;
    let request = late.tool_call_request().await;
    assert_eq!(request["id"], pending.request["id"]);
    assert_eq!(request["params"], pending.request["params"]);
    pending
        .server
        .answer(
            &late,
            &request,
            json!({"contentItems": [{"type": "inputText", "text": "from the late client"}], "success": true}),
        )
        .await;
    let completed = late.dynamic_item("item/completed", call_id).await;
    assert_eq!(completed["item"]["status"], "completed");
    late.notification("turn/completed").await;
    assert_eq!(
        pending.model_output(call_id).await,
        json!([{"type": "input_text", "text": "from the late client"}])
    );
}

// ---------------------------------------------------------------------------------------------
// Session metadata, resume and fork
// ---------------------------------------------------------------------------------------------

/// Codex records a thread's dynamic tools in its session metadata, and an empty list as none.
#[tokio::test]
async fn thread_start_records_dynamic_tools_in_session_metadata() {
    let endpoint = Endpoint::start().await;
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    let legacy = json!([{"name": "demo_tool", "description": "Demo dynamic tool", "inputSchema": city_schema()}]);
    let thread_id = server.start_thread(&mut connection, legacy).await;
    assert_eq!(
        server.session_meta(&thread_id).await.dynamic_tools(),
        Some(&json!([demo_tool()])),
        "recorded in the canonical shape"
    );

    let thread_id = server.start_thread(&mut connection, json!([])).await;
    assert_eq!(server.session_meta(&thread_id).await.dynamic_tools(), None);
}

/// A resumed thread takes its dynamic tools from the session metadata its rollout opens with —
/// either shape — as Codex's session does when given none; its calls are carried as a new
/// thread's are, namespaces and all.
#[tokio::test]
async fn thread_resume_takes_dynamic_tools_from_history() {
    let endpoint = Endpoint::start().await;
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::new());
    let thread_id = "019a0000-0000-7000-8000-000000000001";
    let recorder = store
        .create_thread_with(&CreateThreadParams::new(
            RolloutSessionMeta::new(SessionId::new(thread_id)).with_dynamic_tools(json!([
                {"name": "demo_tool", "description": "Demo dynamic tool", "inputSchema": city_schema()},
                {"namespace": "app", "name": "in_namespace", "description": "", "inputSchema": city_schema()},
            ])),
        ))
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    let server = Server::start(
        &endpoint,
        Arc::clone(&store),
        agent("lead", Vec::new(), Vec::new()),
        RunConfig::new(),
    );
    let mut connection = server.connect(1).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread_id}),
        )
        .await;
    endpoint.call_next("resumed-1", "demo_tool", &json!({"city": "Oslo"}));
    server.turn_start(&mut connection, thread_id, "Go").await;
    let request = connection.tool_call_request().await;
    assert_eq!(request["params"]["callId"], "resumed-1");
    server
        .answer(
            &connection,
            &request,
            json!({"contentItems": [{"type": "inputText", "text": "sunny"}], "success": true}),
        )
        .await;
    connection.notification("turn/completed").await;

    let bodies = endpoint.bodies().await;
    assert_eq!(tool_names(&bodies[0]), ["demo_tool", "app"]);
    assert_eq!(
        find_tool(&bodies[0], "app").unwrap()["description"],
        "Tools in the app namespace."
    );
    assert_eq!(
        function_call_output(&bodies[1], "resumed-1"),
        Some(json!([{"type": "input_text", "text": "sunny"}]))
    );
}

/// A fork takes its dynamic tools from the source's history and records them in its own session
/// metadata, as Codex's forked session does.
#[tokio::test]
async fn thread_fork_takes_dynamic_tools_from_the_source() {
    let endpoint = Endpoint::start().await;
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    let source = server
        .start_thread(&mut connection, json!([demo_tool()]))
        .await;
    server.turn_start(&mut connection, &source, "First").await;
    connection.notification("turn/completed").await;

    let forked = server
        .call(&mut connection, "thread/fork", json!({"threadId": source}))
        .await;
    let fork_id = forked["thread"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        server.session_meta(&fork_id).await.dynamic_tools(),
        Some(&json!([demo_tool()]))
    );
    server.turn_start(&mut connection, &fork_id, "Second").await;
    connection.notification("turn/completed").await;
    let bodies = endpoint.bodies().await;
    assert_eq!(tool_names(&bodies[1]), ["demo_tool"]);

    let ephemeral = server
        .call(
            &mut connection,
            "thread/fork",
            json!({"threadId": source, "ephemeral": true}),
        )
        .await;
    let ephemeral_id = ephemeral["thread"]["id"].as_str().unwrap().to_owned();
    server
        .turn_start(&mut connection, &ephemeral_id, "Third")
        .await;
    connection.notification("turn/completed").await;
    let bodies = endpoint.bodies().await;
    assert_eq!(tool_names(&bodies[2]), ["demo_tool"]);
}

/// Deferred functions are refused until tool search carries them, before anything is built.
#[tokio::test]
async fn thread_start_refuses_deferred_tools() {
    let endpoint = Endpoint::start().await;
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    let schema = json!({"type": "object", "properties": {}});
    let function =
        json!({"type": "function", "name": "lookup", "description": "", "inputSchema": schema});
    let deferred = json!({"type": "function", "name": "hidden", "description": "", "inputSchema": schema, "deferLoading": true});
    for dynamic_tools in [
        json!([{"type": "namespace", "name": "app", "description": "", "tools": [function, deferred]}]),
        json!([{"namespace": "app", "name": "hidden", "description": "", "inputSchema": schema,
            "exposeToContext": false}]),
    ] {
        assert_eq!(
            server
                .call_error(
                    &mut connection,
                    "thread/start",
                    json!({"dynamicTools": dynamic_tools}),
                )
                .await,
            invalid_request("thread/start.dynamicTools.deferLoading is not supported")
        );
    }
    let listed = server
        .call(&mut connection, "thread/loaded/list", json!({}))
        .await;
    assert_eq!(listed["data"], json!([]));
}

/// The framework reserves the same-name namespace shape for deferred top-level calls and refuses
/// it explicitly, before a thread or its metadata is created, in both input formats.
#[tokio::test]
async fn thread_start_rejects_same_name_namespace_tools() {
    let endpoint = Endpoint::start().await;
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    for tools in [
        json!([{"type": "namespace", "name": "app", "description": "App tools", "tools": [
            {"type": "function", "name": "app", "description": "App function", "inputSchema": city_schema()}
        ]}]),
        json!([{"namespace": "app", "name": "app", "description": "App function", "inputSchema": city_schema()}]),
    ] {
        let error = server
            .call_error(
                &mut connection,
                "thread/start",
                json!({"dynamicTools": tools}),
            )
            .await;
        assert_eq!(
            error,
            invalid_request(
                "Responses tool-search reserves the synthetic namespace `app.app` for deferred top-level function tools. Rename the namespace or tool name to avoid ambiguous dispatch."
            )
        );
    }
    let listed = server
        .call(&mut connection, "thread/loaded/list", json!({}))
        .await;
    assert_eq!(listed["data"], json!([]));
    assert!(endpoint.bodies().await.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Namespaces
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_start_normalizes_legacy_dynamic_tools_into_model_request`, without
/// its deferred tool: legacy functions naming a namespace are grouped into it, described as Codex
/// describes a namespace the client left undescribed.
#[tokio::test]
async fn thread_start_normalizes_legacy_dynamic_tools_into_model_request() {
    let endpoint = Endpoint::start().await;
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;
    let visible_schema = json!({
        "type": "object",
        "properties": {
            "ticket_id": { "type": "string" }
        },
        "required": ["ticket_id"],
        "additionalProperties": false,
    });
    let thread_id = server
        .start_thread(
            &mut connection,
            json!([
                {
                    "name": "lookup_ticket",
                    "description": "Look up a ticket",
                    "inputSchema": visible_schema,
                },
                {
                    "namespace": "legacy_app",
                    "name": "lookup_status",
                    "description": "Look up a ticket status",
                    "inputSchema": visible_schema,
                    "exposeToContext": true
                }
            ]),
        )
        .await;
    server
        .turn_start(&mut connection, &thread_id, "Look up the ticket")
        .await;
    connection.notification("turn/completed").await;

    let bodies = endpoint.bodies().await;
    assert_eq!(
        find_tool(&bodies[0], "lookup_ticket"),
        Some(&json!({
            "type": "function",
            "name": "lookup_ticket",
            "description": "Look up a ticket",
            "strict": false,
            "parameters": visible_schema,
        }))
    );
    assert_eq!(
        find_tool(&bodies[0], "legacy_app"),
        Some(&json!({
            "type": "namespace",
            "name": "legacy_app",
            "description": "Tools in the legacy_app namespace.",
            "tools": [{
                "type": "function",
                "name": "lookup_status",
                "description": "Look up a ticket status",
                "strict": false,
                "parameters": visible_schema,
            }],
        }))
    );
}

/// Ported from Codex's `dynamic_tool_call_round_trip_sends_text_content_items_to_model`, namespace
/// and all: the namespace reaches the model as the client describes it, the model's call names it,
/// the item and the request carry it, and the call's answer reaches the model.
#[tokio::test]
async fn dynamic_tool_call_round_trip_in_a_namespace() {
    let call_id = "dyn-call-1";
    let tool_namespace = "codex_app";
    let tool_name = "demo_tool";
    let tool_args = json!({ "city": "Paris" });
    let endpoint = Endpoint::start().await;
    endpoint.call_next_in(call_id, tool_namespace, tool_name, &tool_args);
    let server = Server::plain(&endpoint);
    let mut connection = server.connect(1).await;

    let status_schema = json!({
        "type": "object",
        "properties": {
            "ticket_id": { "type": "string" }
        },
        "required": ["ticket_id"],
        "additionalProperties": false,
    });
    let namespace_description = "Demo namespace tools";
    let thread_id = server
        .start_thread(
            &mut connection,
            json!([{
                "type": "namespace",
                "name": tool_namespace,
                "description": namespace_description,
                "tools": [
                    {
                        "type": "function",
                        "name": tool_name,
                        "description": "Demo dynamic tool",
                        "inputSchema": city_schema(),
                    },
                    {
                        "type": "function",
                        "name": "lookup_status",
                        "description": "Look up ticket status",
                        "inputSchema": status_schema,
                    },
                ],
            }]),
        )
        .await;
    let turn_id = server
        .turn_start(&mut connection, &thread_id, "Run the tool")
        .await;

    let started = connection.dynamic_item("item/started", call_id).await;
    assert_eq!(started["threadId"], thread_id);
    assert_eq!(started["turnId"], turn_id);
    assert_eq!(
        started["item"],
        json!({
            "type": "dynamicToolCall",
            "id": call_id,
            "namespace": tool_namespace,
            "tool": tool_name,
            "arguments": tool_args,
            "status": "inProgress",
            "contentItems": null,
            "success": null,
            "durationMs": null,
        })
    );
    let request = connection.tool_call_request().await;
    assert_eq!(
        request["params"],
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "callId": call_id,
            "namespace": tool_namespace,
            "tool": tool_name,
            "arguments": tool_args,
        })
    );
    server
        .answer(
            &connection,
            &request,
            json!({"contentItems": [{"type": "inputText", "text": "dynamic-ok"}], "success": true}),
        )
        .await;
    let completed = connection.dynamic_item("item/completed", call_id).await;
    assert_eq!(completed["item"]["namespace"], tool_namespace);
    assert_eq!(completed["item"]["status"], "completed");
    assert_eq!(
        completed["item"]["contentItems"],
        json!([{"type": "inputText", "text": "dynamic-ok"}])
    );
    let turn = connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "completed", "{turn}");

    let bodies = endpoint.bodies().await;
    assert_eq!(
        find_tool(&bodies[0], tool_namespace),
        Some(&json!({
            "type": "namespace",
            "name": tool_namespace,
            "description": namespace_description,
            "tools": [
                {
                    "type": "function",
                    "name": tool_name,
                    "description": "Demo dynamic tool",
                    "strict": false,
                    "parameters": city_schema(),
                },
                {
                    "type": "function",
                    "name": "lookup_status",
                    "description": "Look up ticket status",
                    "strict": false,
                    "parameters": status_schema,
                },
            ],
        }))
    );
    // The call is handed back to the model as it made it, with its answer.
    let echoed = bodies[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap()
        .clone();
    assert_eq!(echoed["namespace"], tool_namespace);
    assert_eq!(echoed["name"], tool_name);
    assert_eq!(
        function_call_output(&bodies[1], call_id),
        Some(json!([{"type": "input_text", "text": "dynamic-ok"}]))
    );

    let turns = server.read_turns(&mut connection, &thread_id).await;
    assert_eq!(read_back_item(&turns, call_id), completed["item"]);
}

/// Codex's registry keys a tool by namespace and name: a namespace's function is not skipped for
/// sharing its name with one of the agent's own tools, a namespace's function of a reserved name
/// is no reserved tool, and each call reaches the tool it names.
#[tokio::test]
async fn namespaced_dynamic_tools_are_told_apart_from_top_level_tools() {
    let endpoint = Endpoint::start().await;
    endpoint.call_next_in("dyn-1", "app", "own_tool", &json!({}));
    let server = Server::start(
        &endpoint,
        Arc::new(InMemoryThreadStore::new()),
        agent("lead", vec![OwnTool::shared("own_tool")], Vec::new()),
        RunConfig::new(),
    );
    let mut connection = server.connect(1).await;
    let function = |name: &str| json!({"type": "function", "name": name, "description": "dynamic", "inputSchema": {"type": "object", "properties": {}}});
    let thread_id = server
        .start_thread(
            &mut connection,
            json!([{
                "type": "namespace",
                "name": "app",
                "description": "App tools",
                "tools": [function("own_tool"), function("exec_command")],
            }]),
        )
        .await;
    server.turn_start(&mut connection, &thread_id, "Go").await;
    let request = connection.tool_call_request().await;
    assert_eq!(request["params"]["namespace"], "app");
    assert_eq!(request["params"]["tool"], "own_tool");
    server
        .answer(
            &connection,
            &request,
            json!({"contentItems": [{"type": "inputText", "text": "from the client"}], "success": true}),
        )
        .await;
    connection.notification("turn/completed").await;

    let bodies = endpoint.bodies().await;
    assert_eq!(tool_names(&bodies[0]), ["own_tool", "app"]);
    let namespaced: Vec<_> = find_tool(&bodies[0], "app").unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(namespaced, ["own_tool", "exec_command"]);
    assert_eq!(
        function_call_output(&bodies[1], "dyn-1"),
        Some(json!([{"type": "input_text", "text": "from the client"}]))
    );
}
