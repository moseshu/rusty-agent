//! The app server's handshake and the routing of what it sends, as Codex's app-server has them
//! (`app-server/src/message_processor.rs`, `request_processors/initialize_processor.rs`,
//! `app-server/src/transport.rs`, `app-server/src/lib.rs`).
//!
//! Most cases run the server in this process and hand it connections whose write queues the test
//! holds, through the crate's `test-api` door, so a case controls their capacity and whether the
//! server can drop them. The routing cases port Codex's unit tests of `route_outgoing_envelope`
//! through the whole server: a connection's flags are set by its own `initialize`, not by hand.
//! The handshake cases port Codex's `initialize` and `experimental_api` suites, the part of its
//! WebSocket suite that checks connections are initialized separately runs over the real WebSocket
//! transport, and one case serves the real stdio transport in a child process.
//!
//! A notification the server must withhold is checked by sending another after it: the router
//! writes a connection's messages in the order they are sent, so if the next one to arrive is the
//! later notification, the withheld one was not written.

use std::{
    io::{BufRead, BufReader, Write},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use futures::{SinkExt, StreamExt};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId},
    server::{
        AppServer,
        protocol::{
            ClientInfo, ClientNotification, ClientRequest, InitializeCapabilities,
            InitializeParams, InitializeResponse, MockExperimentalMethodParams,
            MockExperimentalMethodResponse, experimental_required_message,
        },
        test_api::ServerNotifier,
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        stdio::start_stdio_connection,
        test_api::connection_opened,
        websocket::{auth::WebsocketAuthPolicy, start_websocket_acceptor},
    },
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// How long a case waits for any one message or exit.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

/// A running server and the channel its transports would report on.
struct Server {
    events: mpsc::Sender<TransportEvent>,
    notifier: ServerNotifier,
    task: JoinHandle<()>,
}

impl Server {
    fn start() -> Self {
        Self::start_with_home(server_home())
    }

    fn start_with_home(codex_home: impl AsRef<Path>) -> Self {
        let server = AppServer::new(codex_home).unwrap();
        let notifier = ServerNotifier::new(&server);
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            notifier,
            task,
        }
    }

    /// Opens a WebSocket connection with a write queue of `CHANNEL_CAPACITY`.
    async fn open(&self, id: u64) -> Connection {
        self.open_with(id, ConnectionOrigin::WebSocket, CHANNEL_CAPACITY, true)
            .await
    }

    /// Opens a connection from `origin` whose write queue holds `capacity` messages, which the
    /// server can drop if `can_disconnect`.
    async fn open_with(
        &self,
        id: u64,
        origin: ConnectionOrigin,
        capacity: usize,
        can_disconnect: bool,
    ) -> Connection {
        let id = ConnectionId::new(id);
        let (writer, written) = mpsc::channel(capacity);
        let disconnect = can_disconnect.then(CancellationToken::new);
        self.events
            .send(connection_opened(
                id,
                origin,
                writer.clone(),
                disconnect.clone(),
            ))
            .await
            .unwrap();
        Connection {
            id,
            writer,
            written,
            disconnect,
        }
    }

    async fn close(&self, connection: &Connection) {
        self.events
            .send(TransportEvent::ConnectionClosed {
                connection_id: connection.id,
            })
            .await
            .unwrap();
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

    /// Sends `id` calling `method` with `params` and returns what the connection is sent next.
    async fn call(
        &self,
        connection: &mut Connection,
        id: i64,
        method: &str,
        params: Option<Value>,
    ) -> Value {
        self.send(connection, request(id, method, params)).await;
        connection.next().await
    }

    /// Initializes `connection` as `name`, with `capabilities` if there are any, and checks it
    /// succeeded.
    ///
    /// The server takes the handshake into account for what it routes once it has handled the
    /// request, which may be after its answer has been written; so this then asks again, and the
    /// `Already initialized` answer arrives once it has.
    async fn initialize(
        &self,
        connection: &mut Connection,
        name: &str,
        capabilities: Option<InitializeCapabilities>,
    ) {
        let answer = self
            .call(
                connection,
                0,
                "initialize",
                Some(initialize_params(name, capabilities)),
            )
            .await;
        assert!(answer.get("result").is_some(), "{answer}");
        let again = self
            .call(
                connection,
                0,
                "initialize",
                Some(initialize_params(name, None)),
            )
            .await;
        assert_eq!(again, error(0, "Already initialized"));
    }

    /// Sends a notification of `method` to `targets`, or to every connection when there are none.
    async fn notify(
        &self,
        targets: &[&Connection],
        method: &str,
        experimental_reason: Option<&'static str>,
    ) {
        let targets: Vec<_> = targets.iter().map(|connection| connection.id).collect();
        self.notifier
            .send(
                &targets,
                method,
                json!({"note": method}),
                experimental_reason,
            )
            .await;
    }
}

/// A connection as the server sees it, and what the server writes to it.
struct Connection {
    id: ConnectionId,
    writer: mpsc::Sender<QueuedOutgoingMessage>,
    written: mpsc::Receiver<QueuedOutgoingMessage>,
    disconnect: Option<CancellationToken>,
}

impl Connection {
    /// The next message the server writes, as JSON.
    async fn next(&mut self) -> Value {
        let queued = timeout(PATIENCE, self.written.recv())
            .await
            .expect("the server writes a message")
            .expect("the server keeps the write queue");
        serde_json::to_value(queued.message()).unwrap()
    }

    /// The method of the next message the server writes, which must be a notification.
    async fn next_notification(&mut self) -> String {
        let message = self.next().await;
        message["method"]
            .as_str()
            .unwrap_or_else(|| panic!("not a notification: {message}"))
            .to_owned()
    }
}

fn server_home() -> PathBuf {
    std::env::temp_dir().join("rusty-agent-app-server-test")
}

fn request(id: i64, method: &str, params: Option<Value>) -> JsonRpcRequest {
    let request = JsonRpcRequest::new(id, method);
    match params {
        Some(params) => request.with_params(params),
        None => request,
    }
}

fn initialize_params(name: &str, capabilities: Option<InitializeCapabilities>) -> Value {
    let mut params =
        InitializeParams::new(ClientInfo::new(name, "0.1.0").with_title(format!("{name} title")));
    if let Some(capabilities) = capabilities {
        params = params.with_capabilities(capabilities);
    }
    serde_json::to_value(params).unwrap()
}

fn experimental() -> InitializeCapabilities {
    InitializeCapabilities::new().with_experimental_api(true)
}

fn error(id: i64, message: &str) -> Value {
    json!({"id": id, "error": {"code": -32600, "message": message}})
}

// ---------------------------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------------------------

/// Adapted from Codex's `initialize_uses_client_info_name_as_originator` and
/// `initialize_probe_does_not_override_originator`: the answer names the agent, home and platform.
/// Codex makes the first client's name its own `User-Agent` prefix; this agent's `User-Agent` is
/// the one its model adapters send, whoever connects.
#[tokio::test]
async fn initialize_answers_with_the_agents_user_agent_home_and_platform() {
    let server = Server::start();
    let mut vscode = server.open(1).await;
    let mut other = server.open(2).await;

    let answer = server
        .call(
            &mut vscode,
            1,
            "initialize",
            Some(initialize_params("codex_vscode", None)),
        )
        .await;
    let result = answer["result"].clone();
    assert_eq!(answer, json!({"id": 1, "result": result}));
    let response: InitializeResponse = serde_json::from_value(result.clone()).unwrap();
    assert_eq!(
        result,
        json!({
            "userAgent": response.user_agent(),
            "codexHome": server_home(),
            "platformFamily": std::env::consts::FAMILY,
            "platformOs": std::env::consts::OS,
        })
    );
    let version = response
        .user_agent()
        .strip_prefix("rusty-agent/")
        .unwrap_or_else(|| panic!("the agent's own user agent: {}", response.user_agent()));
    assert!(!version.is_empty() && !version.contains("codex_vscode"));
    assert_eq!(response.codex_home(), server_home());
    assert_eq!(response.platform_family(), std::env::consts::FAMILY);
    assert_eq!(response.platform_os(), std::env::consts::OS);

    let answer = server
        .call(
            &mut other,
            1,
            "initialize",
            Some(initialize_params("codex-backend", None)),
        )
        .await;
    assert_eq!(answer["result"], result, "the same whoever connects");
}

/// The host's relative home is made absolute and normalized without requiring it to exist, as
/// Codex's `AbsolutePathBuf::from_absolute_path` does when loading configuration.
#[tokio::test]
async fn initialize_returns_the_hosts_normalized_absolute_home() {
    let home = Path::new("app-server-home").join("..").join("server home");
    let expected = std::env::current_dir().unwrap().join("server home");
    let server = Server::start_with_home(home);
    let mut connection = server.open(1).await;
    let answer = server
        .call(
            &mut connection,
            1,
            "initialize",
            Some(initialize_params("client", None)),
        )
        .await;
    assert_eq!(answer["result"]["codexHome"], json!(expected));
    let response: InitializeResponse = serde_json::from_value(answer["result"].clone()).unwrap();
    assert_eq!(response.codex_home(), expected);
    assert!(response.codex_home().is_absolute());
}

/// Codex's response requires `codexHome` and reads it as an absolute, normalized path.
#[test]
fn initialize_response_requires_an_absolute_home() {
    let mut wire = json!({
        "userAgent": "rusty-agent/0.2.2",
        "platformFamily": std::env::consts::FAMILY,
        "platformOs": std::env::consts::OS,
    });
    let missing = serde_json::from_value::<InitializeResponse>(wire.clone()).unwrap_err();
    assert!(missing.to_string().contains("missing field `codexHome`"));

    for home in [Value::Null, json!("relative/home")] {
        wire["codexHome"] = home;
        assert!(serde_json::from_value::<InitializeResponse>(wire.clone()).is_err());
    }

    wire["codexHome"] = json!(server_home().join("discarded").join("..").join("."));
    let response: InitializeResponse = serde_json::from_value(wire).unwrap();
    assert_eq!(response.codex_home(), server_home());
    assert_eq!(
        serde_json::to_value(&response).unwrap()["codexHome"],
        json!(server_home())
    );
}

/// Codex's `initialize_rejects_invalid_client_name`, and a client may try again.
#[tokio::test]
async fn initialize_rejects_invalid_client_name() {
    let server = Server::start();
    let mut connection = server.open(1).await;

    let answer = server
        .call(
            &mut connection,
            1,
            "initialize",
            Some(initialize_params("bad\rname", None)),
        )
        .await;
    assert_eq!(
        answer,
        error(
            1,
            "Invalid clientInfo.name: 'bad\rname'. Must be a valid HTTP header value."
        )
    );

    let answer = server
        .call(
            &mut connection,
            2,
            "mock/experimentalMethod",
            Some(json!({})),
        )
        .await;
    assert_eq!(
        answer,
        error(2, "Not initialized"),
        "the refusal opens nothing"
    );

    let answer = server
        .call(
            &mut connection,
            3,
            "initialize",
            Some(initialize_params("good name", None)),
        )
        .await;
    assert!(answer.get("result").is_some(), "{answer}");
}

/// Codex's `Already initialized`: a connection is initialized once, and a second `initialize`
/// changes nothing it declared.
#[tokio::test]
async fn initialize_succeeds_once_per_connection() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    server.initialize(&mut connection, "first", None).await;

    let answer = server
        .call(
            &mut connection,
            2,
            "initialize",
            Some(initialize_params("second", Some(experimental()))),
        )
        .await;
    assert_eq!(answer, error(2, "Already initialized"));
    let answer = server
        .call(
            &mut connection,
            3,
            "initialize",
            Some(initialize_params("bad\rname", None)),
        )
        .await;
    assert_eq!(
        answer,
        error(3, "Already initialized"),
        "checked before anything else"
    );

    let answer = server
        .call(
            &mut connection,
            4,
            "mock/experimentalMethod",
            Some(json!({})),
        )
        .await;
    assert_eq!(
        answer,
        error(
            4,
            "mock/experimentalMethod requires experimentalApi capability"
        )
    );
}

/// Codex's `Not initialized`, which `connection_handling_websocket` checks: nothing but
/// `initialize` is answered before it succeeds, and the `initialized` notification does not stand
/// in for it.
#[tokio::test]
async fn methods_before_initialize_are_not_initialized() {
    let server = Server::start();
    let mut connection = server.open(1).await;

    let answer = server
        .call(
            &mut connection,
            1,
            "mock/experimentalMethod",
            Some(json!({"value": "x"})),
        )
        .await;
    assert_eq!(answer, error(1, "Not initialized"));

    let initialized = serde_json::to_value(ClientNotification::Initialized).unwrap();
    assert_eq!(initialized, json!({"method": "initialized"}));
    server
        .send(&connection, JsonRpcNotification::new("initialized"))
        .await;
    let answer = server
        .call(
            &mut connection,
            2,
            "mock/experimentalMethod",
            Some(json!({})),
        )
        .await;
    assert_eq!(answer, error(2, "Not initialized"));
}

/// Codex's `deserialize_client_request`: a request is read before anything else is checked, so an
/// unknown method or parameters that do not fit are an invalid request, initialized or not.
#[tokio::test]
async fn unknown_methods_and_unfit_parameters_are_invalid_requests() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    let unknown = "Invalid request: unknown variant `config/read`, expected one of \
                   `initialize`, `thread/settings/update`, `turn/settings/update`, `turn/start`, `turn/steer`, \
                   `turn/interrupt`, `mock/experimentalMethod`";

    let answer = server.call(&mut connection, 1, "config/read", None).await;
    assert_eq!(answer, error(1, unknown));
    let answer = server.call(&mut connection, 2, "initialize", None).await;
    assert_eq!(answer, error(2, "Invalid request: missing field `params`"));
    let answer = server
        .call(&mut connection, 3, "initialize", Some(json!({})))
        .await;
    assert_eq!(
        answer,
        error(3, "Invalid request: missing field `clientInfo`")
    );
    let answer = server
        .call(
            &mut connection,
            4,
            "initialize",
            Some(json!({"clientInfo": {"name": "no version"}})),
        )
        .await;
    assert_eq!(answer, error(4, "Invalid request: missing field `version`"));

    server.initialize(&mut connection, "client", None).await;
    let answer = server.call(&mut connection, 5, "config/read", None).await;
    assert_eq!(answer, error(5, unknown));
}

/// Codex's `mock_experimental_method_requires_experimental_api_capability`, whether the client
/// declares no capabilities or declines the experimental API.
#[tokio::test]
async fn mock_experimental_method_requires_experimental_api_capability() {
    let server = Server::start();
    for (id, capabilities) in [
        (1, None),
        (
            2,
            Some(InitializeCapabilities::new().with_experimental_api(false)),
        ),
    ] {
        let mut connection = server.open(id).await;
        server
            .initialize(&mut connection, "client", capabilities)
            .await;
        let answer = server
            .call(
                &mut connection,
                1,
                "mock/experimentalMethod",
                Some(json!({"value": "x"})),
            )
            .await;
        assert_eq!(
            answer,
            error(1, &experimental_required_message("mock/experimentalMethod"))
        );
    }
}

/// With the experimental API, Codex's mock method echoes its value.
#[tokio::test]
async fn mock_experimental_method_echoes_with_capability() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    server
        .initialize(&mut connection, "client", Some(experimental()))
        .await;

    let answer = server
        .call(
            &mut connection,
            1,
            "mock/experimentalMethod",
            Some(json!({"value": "hello"})),
        )
        .await;
    assert_eq!(answer, json!({"id": 1, "result": {"echoed": "hello"}}));
    let response: MockExperimentalMethodResponse =
        serde_json::from_value(answer["result"].clone()).unwrap();
    assert_eq!(response.echoed(), Some("hello"));

    let answer = server
        .call(
            &mut connection,
            2,
            "mock/experimentalMethod",
            Some(json!({})),
        )
        .await;
    assert_eq!(answer, json!({"id": 2, "result": {"echoed": null}}));
}

/// As in Codex, the server reads no notification from a client: `initialized` is accepted and
/// gets nothing back, and neither does an answer to a request the server is not waiting on.
#[tokio::test]
async fn client_notifications_and_stray_answers_get_nothing_back() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    server
        .initialize(&mut connection, "client", Some(experimental()))
        .await;

    server
        .send(&connection, JsonRpcNotification::new("initialized"))
        .await;
    server
        .send(&connection, JsonRpcResponse::new(99, json!({"ok": true})))
        .await;
    let stray_error: JsonRpcMessage =
        serde_json::from_value(json!({"id": 100, "error": {"code": 1, "message": "no"}})).unwrap();
    server.send(&connection, stray_error).await;

    let answer = server
        .call(
            &mut connection,
            1,
            "mock/experimentalMethod",
            Some(json!({})),
        )
        .await;
    assert_eq!(
        answer["id"], 1,
        "the next thing written answers the next request"
    );
}

/// The handshake's types read and write Codex's wire shape, and ignore what Codex's server reads
/// and this one does not.
#[test]
fn handshake_types_use_codex_wire_shape() {
    let params = InitializeParams::new(ClientInfo::new("client", "1.0")).with_capabilities(
        InitializeCapabilities::new()
            .with_experimental_api(true)
            .with_opt_out_notification_methods(["thread/started"]),
    );
    assert_eq!(
        serde_json::to_value(&params).unwrap(),
        json!({
            "clientInfo": {"name": "client", "title": null, "version": "1.0"},
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": ["thread/started"],
            },
        })
    );
    assert_eq!(
        serde_json::to_value(InitializeParams::new(ClientInfo::new("c", "1"))).unwrap(),
        json!({"clientInfo": {"name": "c", "title": null, "version": "1"}})
    );

    let from_codex: InitializeParams = serde_json::from_value(json!({
        "clientInfo": {"name": "codex_vscode", "title": "Codex", "version": "0.1.0"},
        "capabilities": {
            "explicitGatewayOauth": true,
            "experimentalApi": true,
            "requestAttestation": true,
            "mcpServerOpenaiFormElicitation": true,
            "optOutNotificationMethods": null,
            "extensions": {"openai/form": {}},
        },
    }))
    .unwrap();
    assert_eq!(from_codex.client_info().name(), "codex_vscode");
    assert_eq!(from_codex.client_info().title(), Some("Codex"));
    assert_eq!(from_codex.client_info().version(), "0.1.0");
    let capabilities = from_codex.capabilities().unwrap();
    assert!(capabilities.experimental_api());
    assert_eq!(capabilities.opt_out_notification_methods(), None);
    let defaults: InitializeCapabilities = serde_json::from_value(json!({})).unwrap();
    assert_eq!(defaults, InitializeCapabilities::new());

    let request = ClientRequest::try_from(
        JsonRpcRequest::new("abc", "mock/experimentalMethod").with_params(
            serde_json::to_value(MockExperimentalMethodParams::new().with_value("v")).unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(request.id(), &RequestId::from("abc"));
    assert_eq!(request.method_name(), "mock/experimentalMethod");
    assert_eq!(
        request.experimental_reason(),
        Some("mock/experimentalMethod")
    );
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({"id": "abc", "method": "mock/experimentalMethod", "params": {"value": "v"}})
    );
    let request = ClientRequest::try_from(
        JsonRpcRequest::new(1, "initialize").with_params(serde_json::to_value(&params).unwrap()),
    )
    .unwrap();
    assert_eq!(request.method_name(), "initialize");
    assert_eq!(request.experimental_reason(), None);
}

// ---------------------------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------------------------

/// Codex's `to_connection_notification_respects_opt_out_filters` and
/// `to_connection_notifications_are_dropped_for_opted_out_clients`.
#[tokio::test]
async fn to_connection_notifications_are_dropped_for_opted_out_clients() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    let capabilities = experimental().with_opt_out_notification_methods(["configWarning"]);
    server
        .initialize(&mut connection, "client", Some(capabilities))
        .await;

    server.notify(&[&connection], "configWarning", None).await;
    server.notify(&[&connection], "thread/started", None).await;
    assert_eq!(connection.next_notification().await, "thread/started");
}

/// Codex's `to_connection_notifications_are_preserved_for_non_opted_out_clients`.
#[tokio::test]
async fn to_connection_notifications_are_preserved_for_non_opted_out_clients() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    server
        .initialize(&mut connection, "client", Some(experimental()))
        .await;

    server.notify(&[&connection], "configWarning", None).await;
    assert_eq!(
        connection.next().await,
        json!({"method": "configWarning", "params": {"note": "configWarning"}})
    );
}

/// Codex's `experimental_notifications_are_dropped_without_capability`: stable notifications still
/// arrive.
#[tokio::test]
async fn experimental_notifications_are_dropped_without_capability() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    server.initialize(&mut connection, "client", None).await;

    server
        .notify(
            &[&connection],
            "thread/realtime/started",
            Some("thread/realtime/started"),
        )
        .await;
    for method in [
        "modelProvider/authRecoveryStarted",
        "modelProvider/authRecoveryCompleted",
    ] {
        server.notify(&[&connection], method, None).await;
        assert_eq!(connection.next_notification().await, method);
    }
}

/// Codex's `experimental_notifications_are_preserved_with_capability`.
#[tokio::test]
async fn experimental_notifications_are_preserved_with_capability() {
    let server = Server::start();
    let mut connection = server.open(1).await;
    server
        .initialize(&mut connection, "client", Some(experimental()))
        .await;

    server
        .notify(
            &[&connection],
            "thread/realtime/started",
            Some("thread/realtime/started"),
        )
        .await;
    assert_eq!(
        connection.next_notification().await,
        "thread/realtime/started"
    );
}

/// Codex's `route_outgoing_envelope`: what is sent to every connection reaches only those that
/// have been initialized.
#[tokio::test]
async fn broadcasts_reach_only_initialized_connections() {
    let server = Server::start();
    let mut early = server.open(1).await;
    let mut late = server.open(2).await;
    server.initialize(&mut early, "early", None).await;

    server.notify(&[], "first", None).await;
    assert_eq!(early.next_notification().await, "first");

    server.initialize(&mut late, "late", None).await;
    server.notify(&[], "second", None).await;
    assert_eq!(early.next_notification().await, "second");
    assert_eq!(late.next_notification().await, "second");
}

/// Codex's `initialize_opt_out_notification_methods_filters_notifications`: the methods a
/// connection opted out of are withheld from what is sent to every connection too, as are
/// experimental ones without the capability.
#[tokio::test]
async fn broadcasts_respect_each_connections_handshake() {
    let server = Server::start();
    let mut opted_out = server.open(1).await;
    let mut stable_only = server.open(2).await;
    let capabilities = experimental().with_opt_out_notification_methods(["thread/started"]);
    server
        .initialize(&mut opted_out, "opted-out", Some(capabilities))
        .await;
    server.initialize(&mut stable_only, "stable", None).await;

    server.notify(&[], "thread/started", None).await;
    server
        .notify(
            &[],
            "thread/realtime/started",
            Some("thread/realtime/started"),
        )
        .await;
    server.notify(&[], "turn/started", None).await;

    assert_eq!(
        opted_out.next_notification().await,
        "thread/realtime/started"
    );
    assert_eq!(opted_out.next_notification().await, "turn/started");
    assert_eq!(stable_only.next_notification().await, "thread/started");
    assert_eq!(stable_only.next_notification().await, "turn/started");
}

/// Codex's `broadcast_does_not_block_on_slow_connection`: a connection the server can drop is
/// dropped when its write queue is full, keeping what it had queued, and the others still receive
/// the message.
#[tokio::test]
async fn broadcast_does_not_block_on_slow_connection() {
    let server = Server::start();
    let mut fast = server
        .open_with(1, ConnectionOrigin::WebSocket, 1, true)
        .await;
    let mut slow = server
        .open_with(2, ConnectionOrigin::WebSocket, 1, true)
        .await;
    server.initialize(&mut fast, "fast", None).await;
    server.initialize(&mut slow, "slow", None).await;
    slow.writer
        .try_send(QueuedOutgoingMessage::new(JsonRpcNotification::new(
            "already-buffered",
        )))
        .expect("the queue has room");

    server.notify(&[], "test", None).await;
    assert_eq!(fast.next_notification().await, "test");
    let slow_disconnect = slow.disconnect.clone().unwrap();
    timeout(PATIENCE, slow_disconnect.cancelled())
        .await
        .expect("the slow connection is dropped");
    assert!(!fast.disconnect.as_ref().unwrap().is_cancelled());
    assert_eq!(slow.next_notification().await, "already-buffered");

    server.notify(&[], "after", None).await;
    assert_eq!(fast.next_notification().await, "after");
    assert!(
        slow.written.try_recv().is_err(),
        "a dropped connection is sent nothing more"
    );
}

/// Codex's `to_connection_stdio_waits_instead_of_disconnecting_when_writer_queue_is_full`.
///
/// Codex's case times the router directly. Here the router's wait shows in what it routes next: a
/// message for another connection, sent after, is held up until the full queue has room — so
/// waiting holds up every connection, as in Codex — and a router that dropped the message would
/// deliver it at once.
#[tokio::test]
async fn to_connection_stdio_waits_instead_of_disconnecting_when_writer_queue_is_full() {
    let server = Server::start();
    let mut connection = server.open_with(1, ConnectionOrigin::Stdio, 1, false).await;
    let mut other = server.open(2).await;
    server.initialize(&mut connection, "client", None).await;
    server.initialize(&mut other, "other", None).await;
    connection
        .writer
        .send(QueuedOutgoingMessage::new(JsonRpcNotification::new(
            "queued",
        )))
        .await
        .unwrap();

    server.notify(&[&connection], "second", None).await;
    server.notify(&[&other], "behind", None).await;
    assert!(
        timeout(Duration::from_millis(200), other.written.recv())
            .await
            .is_err(),
        "the router waits for room"
    );
    assert_eq!(connection.next_notification().await, "queued");
    assert_eq!(connection.next_notification().await, "second");
    assert_eq!(other.next_notification().await, "behind");
}

/// Codex's processor loop: a connection closing is forgotten, and messages for it are dropped,
/// while the others go on; a transport may report the same close twice. Closing the stdio
/// connection ends the server, as Codex's does when it serves stdio.
#[tokio::test]
async fn closing_a_connection_forgets_it_and_closing_stdio_ends_the_server() {
    let server = Server::start();
    let mut gone = server.open(1).await;
    let mut kept = server.open(2).await;
    let stdio = server
        .open_with(3, ConnectionOrigin::Stdio, CHANNEL_CAPACITY, false)
        .await;
    server.initialize(&mut gone, "gone", None).await;
    server.initialize(&mut kept, "kept", None).await;

    server.close(&gone).await;
    server.close(&gone).await;
    server.send(&gone, request(9, "initialize", None)).await;
    // The server handles events in order, so once this is answered the close has reached the
    // router, which handles it before anything sent after it.
    let answer = server
        .call(&mut kept, 1, "mock/experimentalMethod", Some(json!({})))
        .await;
    assert_eq!(answer["id"], 1, "the server still serves the others");
    assert!(!server.task.is_finished());

    server.notify(&[&gone], "for the gone", None).await;
    server.notify(&[], "for everyone", None).await;
    assert_eq!(kept.next_notification().await, "for everyone");
    assert!(
        gone.written.try_recv().is_err(),
        "a closed connection is sent nothing"
    );

    server.close(&stdio).await;
    timeout(PATIENCE, server.task)
        .await
        .expect("closing stdio ends the server")
        .unwrap();
}

/// The server ends once every transport has gone.
#[tokio::test]
async fn the_server_ends_when_every_transport_has_gone() {
    let Server {
        events,
        notifier,
        task,
    } = Server::start();
    drop(events);
    timeout(PATIENCE, task)
        .await
        .expect("the server ends")
        .unwrap();
    drop(notifier);
}

// ---------------------------------------------------------------------------------------------
// Over the real transports
// ---------------------------------------------------------------------------------------------

/// Codex's `websocket_transport_routes_per_connection_handshake_and_responses`, with Codex's mock
/// method standing in for `config/read`: each connection is initialized on its own, an answer
/// reaches only the connection that asked, and the same id on two connections is answered on each.
#[tokio::test]
async fn websocket_transport_routes_per_connection_handshake_and_responses() {
    let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    let (addr, acceptor) = start_websocket_acceptor(
        (Ipv4Addr::LOCALHOST, 0).into(),
        events,
        shutdown.clone(),
        WebsocketAuthPolicy::default(),
    )
    .await
    .unwrap();
    let server = tokio::spawn(
        AppServer::new(server_home())
            .unwrap()
            .run(transport_event_rx),
    );

    let (mut ws1, _) = connect_async(format!("ws://{addr}")).await.unwrap();
    let (mut ws2, _) = connect_async(format!("ws://{addr}")).await.unwrap();
    let initialize = |id: i64, name: &str| {
        request(
            id,
            "initialize",
            Some(initialize_params(name, Some(experimental()))),
        )
    };
    let mock = |id: i64| {
        request(
            id,
            "mock/experimentalMethod",
            Some(json!({"value": id.to_string()})),
        )
    };

    send_ws(&mut ws1, initialize(1, "ws_client_one")).await;
    let answer = read_ws(&mut ws1).await;
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["codexHome"], json!(server_home()));

    // The first thing the second connection is sent answers its own request, so the first
    // connection's answer did not reach it.
    send_ws(&mut ws2, mock(2)).await;
    assert_eq!(read_ws(&mut ws2).await, error(2, "Not initialized"));

    send_ws(&mut ws2, initialize(3, "ws_client_two")).await;
    assert_eq!(read_ws(&mut ws2).await["id"], 3);

    send_ws(&mut ws1, mock(77)).await;
    send_ws(&mut ws2, mock(77)).await;
    assert_eq!(
        read_ws(&mut ws1).await,
        json!({"id": 77, "result": {"echoed": "77"}})
    );
    assert_eq!(
        read_ws(&mut ws2).await,
        json!({"id": 77, "result": {"echoed": "77"}})
    );

    shutdown.cancel();
    drop((ws1, ws2));
    timeout(PATIENCE, acceptor).await.unwrap().unwrap();
    timeout(PATIENCE, server)
        .await
        .expect("the server ends once the transport has gone")
        .unwrap();
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send_ws(ws: &mut WsClient, message: impl Into<JsonRpcMessage>) {
    let text = serde_json::to_string(&message.into()).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

async fn read_ws(ws: &mut WsClient) -> Value {
    loop {
        let frame = timeout(PATIENCE, ws.next())
            .await
            .expect("the server writes a frame")
            .expect("the connection stays open")
            .unwrap();
        if let Message::Text(text) = frame {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

/// Marks the run of this binary that serves stdio for [`stdio_handshake_and_exit`].
const STDIO_CHILD: &str = "RA_APP_SERVER_STDIO_CHILD";

/// The child of [`stdio_handshake_and_exit`]: the server over this process's stdin and stdout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs only as the child process of the stdio app server test"]
async fn stdio_app_server_child() {
    assert!(
        std::env::var_os(STDIO_CHILD).is_some(),
        "set by the parent test"
    );
    let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let connection = start_stdio_connection(events, false).await.unwrap();
    AppServer::new(server_home())
        .unwrap()
        .run(transport_event_rx)
        .await;
    timeout(PATIENCE, connection)
        .await
        .expect("stdout is written out once the server has gone")
        .unwrap();
}

/// Over the real stdio transport, a client initializes and calls a method, and the server ends
/// when stdin does, so the process exits on its own.
#[test]
fn stdio_handshake_and_exit() {
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args([
            "stdio_app_server_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--quiet",
        ])
        .env(STDIO_CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    let stdout = process.stdout.take().unwrap();
    let (line_tx, lines) = std_mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.starts_with('{') && line_tx.send(serde_json::from_str(&line).unwrap()).is_err()
            {
                break;
            }
        }
    });
    let mut write = |message: JsonRpcMessage| {
        writeln!(stdin, "{}", serde_json::to_string(&message).unwrap()).unwrap();
        stdin.flush().unwrap();
    };
    let next = || {
        lines
            .recv_timeout(PATIENCE)
            .expect("the child writes a line")
    };

    write(request(1, "mock/experimentalMethod", Some(json!({}))).into());
    assert_eq!(next(), error(1, "Not initialized"));
    write(
        request(
            2,
            "initialize",
            Some(initialize_params("stdio", Some(experimental()))),
        )
        .into(),
    );
    let answer = next();
    assert_eq!(answer["id"], 2);
    assert_eq!(answer["result"]["codexHome"], json!(server_home()));
    write(JsonRpcNotification::new("initialized").into());
    write(request(3, "mock/experimentalMethod", Some(json!({"value": "v"}))).into());
    assert_eq!(next(), json!({"id": 3, "result": {"echoed": "v"}}));

    drop(stdin);
    let deadline = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = process.kill();
            panic!("the child did not exit when stdin ended");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status}");
}
