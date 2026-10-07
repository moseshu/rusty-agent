//! The transports, as Codex's app-server has them (`app-server-transport/src/transport/mod.rs` and
//! `transport/stdio.rs`).
//!
//! The stdio transport reads the process's own stdin and writes its own stdout, so its cases run
//! it in a child process: this test binary run again with only [`stdio_child`] selected, which
//! answers what arrives as [`CHILD_MODE`] says. The parent writes lines to the child's stdin and
//! reads the JSON lines it writes back; the test harness's own lines, which never start with `{`,
//! are skipped.
//!
//! How every transport hands a message to the server is ported from Codex's unit tests of
//! `enqueue_incoming_message`, through the crate's `test-api` door.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, error_code},
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        stdio::start_stdio_connection, test_api::enqueue_incoming_message,
    },
};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

/// Names what the child does: [`ECHO`], [`ECHO_WITH_SIGTERM`] or [`OVERLOAD`].
const CHILD_MODE: &str = "RA_STDIO_CHILD_MODE";
/// Answers every message, and reports the connection's end.
const ECHO: &str = "echo";
/// As [`ECHO`], with the `SIGTERM` handler installed.
const ECHO_WITH_SIGTERM: &str = "echo-sigterm";
/// Takes the connection, then reads nothing more from a server channel with room for one event.
const OVERLOAD: &str = "overload";

/// How long the parent waits for any one line or exit.
const PATIENCE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------------------------

/// The child: serves the stdio connection as [`CHILD_MODE`] says.
///
/// Once the connection opens it says so with a `child/opened` notification, so the parent knows the
/// signal handler is in place. In the echo modes a request is answered with its method and
/// parameters — or, for `ack`, answered and then followed by `child/written` once the transport
/// reports the answer written — a notification with `child/notified`, a response or error from the
/// host with `child/answered`, and the connection's end with `child/closed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs only as the child process of the stdio transport tests"]
async fn stdio_child() {
    let mode = std::env::var(CHILD_MODE).expect("set by the parent test");
    let capacity = if mode == OVERLOAD {
        1
    } else {
        CHANNEL_CAPACITY
    };
    let (transport_event_tx, mut events) = mpsc::channel(capacity);
    let connection = start_stdio_connection(transport_event_tx, mode == ECHO_WITH_SIGTERM)
        .await
        .unwrap();
    let Some(TransportEvent::ConnectionOpened {
        connection_id,
        origin,
        writer,
        ..
    }) = events.recv().await
    else {
        panic!("the connection opens first");
    };
    assert_eq!(origin, ConnectionOrigin::Stdio);
    let send = |message: JsonRpcMessage| {
        let writer = writer.clone();
        async move {
            writer
                .send(QueuedOutgoingMessage::new(message))
                .await
                .is_ok()
        }
    };
    send(notification(
        "child/opened",
        json!({"connection": connection_id.get()}),
    ))
    .await;
    if mode == OVERLOAD {
        std::future::pending::<()>().await;
    }

    let mut closed = false;
    while let Some(event) = events.recv().await {
        match event {
            TransportEvent::IncomingMessage {
                connection_id: from,
                message,
            } => {
                assert_eq!(from, connection_id);
                match message {
                    JsonRpcMessage::Request(request) if request.method() == "ack" => {
                        let (written_tx, written_rx) = oneshot::channel();
                        writer
                            .send(
                                QueuedOutgoingMessage::new(JsonRpcResponse::new(
                                    request.id().clone(),
                                    json!("acked"),
                                ))
                                .with_write_complete(written_tx),
                            )
                            .await
                            .unwrap();
                        written_rx.await.expect("the transport reports the write");
                        send(notification("child/written", json!({}))).await;
                    }
                    JsonRpcMessage::Request(request) => {
                        let result = json!({
                            "method": request.method(),
                            "params": request.params(),
                        });
                        send(JsonRpcResponse::new(request.id().clone(), result).into()).await;
                    }
                    JsonRpcMessage::Notification(notified) => {
                        let params =
                            json!({"method": notified.method(), "params": notified.params()});
                        send(notification("child/notified", params)).await;
                    }
                    JsonRpcMessage::Response(response) => {
                        let params = json!({"id": response.id(), "result": response.result()});
                        send(notification("child/answered", params)).await;
                    }
                    JsonRpcMessage::Error(error) => {
                        let params = json!({"id": error.id(), "code": error.error().code()});
                        send(notification("child/answered", params)).await;
                    }
                    other => panic!("unexpected message {other:?}"),
                }
            }
            TransportEvent::ConnectionClosed {
                connection_id: from,
            } => {
                assert_eq!(from, connection_id);
                // After SIGTERM the write queue is gone and this is dropped.
                send(notification("child/closed", json!({}))).await;
                closed = true;
                break;
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(closed, "the transport reports the connection closed");
    drop(writer);
    timeout(PATIENCE, connection)
        .await
        .expect("the transport finishes once nothing more can be written")
        .unwrap();
}

fn notification(method: &str, params: Value) -> JsonRpcMessage {
    JsonRpcNotification::new(method).with_params(params).into()
}

// ---------------------------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------------------------

/// A running child and the JSON lines it has written.
struct StdioChild {
    process: Child,
    stdin: Option<ChildStdin>,
    lines: std_mpsc::Receiver<Value>,
}

impl StdioChild {
    /// Starts the child in `mode` and waits until its connection has opened.
    fn start(mode: &str) -> Self {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "stdio_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env(CHILD_MODE, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = process.stdin.take();
        let stdout = process.stdout.take().unwrap();
        let (line_tx, lines) = std_mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.starts_with('{') {
                    let value = serde_json::from_str(&line)
                        .unwrap_or_else(|error| panic!("not JSON: {line:?}: {error}"));
                    if line_tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        let child = Self {
            process,
            stdin,
            lines,
        };
        let opened = child.next_line();
        assert_eq!(opened["method"], "child/opened", "{opened}");
        child
    }

    /// Writes `line` and its end to the child's stdin.
    fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn write(&mut self, message: impl Into<JsonRpcMessage>) {
        self.write_line(&serde_json::to_string(&message.into()).unwrap());
    }

    fn next_line(&self) -> Value {
        self.lines
            .recv_timeout(PATIENCE)
            .expect("the child writes a line")
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every line the child wrote after the last one read, once it has exited.
    fn remaining_lines(&self) -> Vec<Value> {
        self.lines.try_iter().collect()
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Each line of stdin is a message and each answer one line of stdout, in order. The answers are
/// written without the `jsonrpc` member, a client may send it, and parameters and ids reach the
/// server as sent.
#[test]
fn stdio_answers_each_line_with_a_line() {
    let mut child = StdioChild::start(ECHO);
    child.write(JsonRpcRequest::new(1, "thread/start").with_params(json!({"cwd": "/w", "x": [1]})));
    child.write_line(r#"{"jsonrpc":"2.0","id":"req-2","method":"turn/start"}"#);
    child.write(JsonRpcRequest::new(3, "thread/read"));

    assert_eq!(
        child.next_line(),
        json!({"id": 1, "result": {"method": "thread/start", "params": {"cwd": "/w", "x": [1]}}})
    );
    assert_eq!(
        child.next_line(),
        json!({"id": "req-2", "result": {"method": "turn/start", "params": null}})
    );
    assert_eq!(
        child.next_line(),
        json!({"id": 3, "result": {"method": "thread/read", "params": null}})
    );
}

/// Notifications and the host's answers to the agent's own requests reach the server too.
#[test]
fn stdio_forwards_notifications_and_answers() {
    let mut child = StdioChild::start(ECHO);
    child.write(JsonRpcNotification::new("initialized"));
    child.write_line(r#"{"id":0,"result":{"decision":"accept"}}"#);
    child.write_line(r#"{"id":1,"error":{"code":-32603,"message":"boom"}}"#);

    assert_eq!(
        child.next_line(),
        json!({"method": "child/notified", "params": {"method": "initialized", "params": null}})
    );
    assert_eq!(
        child.next_line(),
        json!({"method": "child/answered", "params": {"id": 0, "result": {"decision": "accept"}}})
    );
    assert_eq!(
        child.next_line(),
        json!({"method": "child/answered", "params": {"id": 1, "code": -32603}})
    );
}

/// A line that is not a message — malformed, empty, or JSON of another shape — is skipped, and the
/// connection keeps reading, as Codex's `forward_incoming_message` does.
#[test]
fn stdio_skips_a_line_that_is_not_a_message() {
    let mut child = StdioChild::start(ECHO);
    child.write_line("not json");
    child.write_line("");
    child.write_line(r#""a string""#);
    child.write_line(r#"{"id":4}"#);
    child.write(JsonRpcRequest::new(5, "thread/list"));

    assert_eq!(
        child.next_line(),
        json!({"id": 5, "result": {"method": "thread/list", "params": null}})
    );
}

/// A message queued with a write-complete signal gets it once the line has been written.
#[test]
fn stdio_signals_a_completed_write() {
    let mut child = StdioChild::start(ECHO);
    child.write(JsonRpcRequest::new(6, "ack"));

    assert_eq!(child.next_line(), json!({"id": 6, "result": "acked"}));
    assert_eq!(
        child.next_line(),
        json!({"method": "child/written", "params": {}})
    );
}

/// The end of stdin closes the connection, and what the server writes afterwards still goes out;
/// the transport finishes once the server lets go of the write queue.
#[test]
fn stdio_closes_at_the_end_of_stdin_and_keeps_writing() {
    let mut child = StdioChild::start(ECHO);
    child.write(JsonRpcRequest::new(7, "thread/read"));
    assert_eq!(child.next_line()["id"], 7);
    child.close_stdin();

    assert_eq!(
        child.next_line(),
        json!({"method": "child/closed", "params": {}})
    );
    assert!(child.wait().success());
    assert_eq!(child.remaining_lines(), Vec::<Value>::new());
}

/// With the handler installed, `SIGTERM` closes the connection while the client still holds stdin
/// open, and the process ends on its own instead of being killed by the signal: the blocked stdin
/// thread does not keep the runtime alive.
#[cfg(unix)]
#[test]
fn stdio_closes_on_sigterm_while_stdin_stays_open() {
    let mut child = StdioChild::start(ECHO_WITH_SIGTERM);
    child.write(JsonRpcRequest::new(8, "thread/read"));
    assert_eq!(child.next_line()["id"], 8);

    let killed = Command::new("kill")
        .args(["-TERM", &child.process.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());

    let status = child.wait();
    assert!(status.success(), "{status}");
    assert!(child.stdin.is_some(), "stdin is still open");
    assert_eq!(child.remaining_lines(), Vec::<Value>::new());
}

/// When the server's channel is full, a request read from stdin is answered at once with the
/// overloaded error, carrying its id.
#[test]
fn stdio_answers_overloaded_when_the_server_is_full() {
    let mut child = StdioChild::start(OVERLOAD);
    child.write(JsonRpcNotification::new("initialized"));
    child.write(JsonRpcRequest::new(9, "thread/list"));

    assert_eq!(
        child.next_line(),
        json!({
            "id": 9,
            "error": {"code": error_code::OVERLOADED, "message": "Server overloaded; retry later."},
        })
    );
}

// ---------------------------------------------------------------------------------------------
// Handing messages to the server
// ---------------------------------------------------------------------------------------------

fn initialized() -> JsonRpcMessage {
    JsonRpcNotification::new("initialized").into()
}

fn incoming(event: Option<TransportEvent>) -> (ConnectionId, JsonRpcMessage) {
    match event {
        Some(TransportEvent::IncomingMessage {
            connection_id,
            message,
        }) => (connection_id, message),
        other => panic!("expected an incoming message, got {other:?}"),
    }
}

/// Codex's `enqueue_incoming_request_returns_overload_error_when_queue_is_full`.
#[tokio::test]
async fn enqueue_incoming_request_returns_overload_error_when_queue_is_full() {
    let connection_id = ConnectionId::new(42);
    let (transport_event_tx, mut transport_event_rx) = mpsc::channel(1);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    assert!(
        enqueue_incoming_message(
            &transport_event_tx,
            &writer_tx,
            connection_id,
            initialized()
        )
        .await
    );

    let request =
        JsonRpcRequest::new(7, "config/read").with_params(json!({"includeLayers": false}));
    let enqueued = timeout(
        Duration::from_millis(100),
        enqueue_incoming_message(
            &transport_event_tx,
            &writer_tx,
            connection_id,
            request.into(),
        ),
    )
    .await
    .expect("a request does not wait for room");
    assert!(enqueued);

    assert_eq!(
        incoming(transport_event_rx.recv().await),
        (connection_id, initialized())
    );
    let overload = writer_rx.recv().await.unwrap();
    assert_eq!(
        serde_json::to_value(overload.message()).unwrap(),
        json!({
            "id": 7,
            "error": {"code": error_code::OVERLOADED, "message": "Server overloaded; retry later."},
        })
    );
}

/// Codex's `enqueue_incoming_response_waits_instead_of_dropping_when_queue_is_full`.
#[tokio::test]
async fn enqueue_incoming_response_waits_instead_of_dropping_when_queue_is_full() {
    let connection_id = ConnectionId::new(42);
    let (transport_event_tx, mut transport_event_rx) = mpsc::channel(1);
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    assert!(
        enqueue_incoming_message(
            &transport_event_tx,
            &writer_tx,
            connection_id,
            initialized()
        )
        .await
    );

    let response: JsonRpcMessage = JsonRpcResponse::new(7, json!({"ok": true})).into();
    let enqueue = tokio::spawn({
        let transport_event_tx = transport_event_tx.clone();
        let writer_tx = writer_tx.clone();
        let response = response.clone();
        async move {
            enqueue_incoming_message(&transport_event_tx, &writer_tx, connection_id, response).await
        }
    });

    assert_eq!(
        incoming(transport_event_rx.recv().await),
        (connection_id, initialized())
    );
    assert!(enqueue.await.unwrap());
    assert_eq!(
        incoming(transport_event_rx.recv().await),
        (connection_id, response)
    );
}

/// Codex's `enqueue_incoming_request_does_not_block_when_writer_queue_is_full`: the overloaded
/// answer is dropped rather than block reading.
#[tokio::test]
async fn enqueue_incoming_request_does_not_block_when_writer_queue_is_full() {
    let connection_id = ConnectionId::new(42);
    let (transport_event_tx, _transport_event_rx) = mpsc::channel(1);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    assert!(
        enqueue_incoming_message(
            &transport_event_tx,
            &writer_tx,
            connection_id,
            initialized()
        )
        .await
    );
    let queued = notification("configWarning", json!({"summary": "queued"}));
    writer_tx
        .send(QueuedOutgoingMessage::new(queued.clone()))
        .await
        .unwrap();

    let request = JsonRpcRequest::new(7, "config/read").into();
    let enqueued = timeout(
        Duration::from_millis(100),
        enqueue_incoming_message(&transport_event_tx, &writer_tx, connection_id, request),
    )
    .await
    .expect("enqueue should not block while writer queue is full");
    assert!(enqueued);

    assert_eq!(writer_rx.recv().await.unwrap().into_message(), queued);
    assert!(
        writer_rx.try_recv().is_err(),
        "the overloaded answer was dropped"
    );
}

/// Once the server has gone, the connection stops reading.
#[tokio::test]
async fn enqueue_stops_reading_once_the_server_has_gone() {
    let (transport_event_tx, transport_event_rx) = mpsc::channel(1);
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    drop(transport_event_rx);

    assert!(
        !enqueue_incoming_message(
            &transport_event_tx,
            &writer_tx,
            ConnectionId::new(1),
            initialized()
        )
        .await
    );
}

/// A connection's id prints as its number.
#[test]
fn connection_ids_print_as_their_number() {
    assert_eq!(ConnectionId::new(42).to_string(), "42");
    assert_eq!(ConnectionId::new(42).get(), 42);
}
