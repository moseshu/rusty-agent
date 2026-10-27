// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_executor_protocol::JSONRPCMessage;
use ra_executor_protocol::JSONRPCNotification;
use ra_executor_protocol::JSONRPCResponse;
use ra_executor_protocol::RequestId;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

use ra_executor::test_api::connection::JsonRpcConnection;
use ra_executor::test_api::connection::JsonRpcConnectionEvent;
use ra_executor::test_api::connection::JsonRpcTransport;
use ra_executor::test_api::rpc::MAX_IN_FLIGHT_REGULAR_CALLS;
use ra_executor::test_api::rpc::RESERVED_OUTBOUND_CONTROL_MESSAGES;
use ra_executor::test_api::rpc::RpcCallError;
use ra_executor::test_api::rpc::RpcClient;
use ra_executor::test_api::rpc::RpcNotificationSender;
#[tokio::test]
async fn best_effort_notifications_preserve_outbound_control_capacity() {
    let (outgoing_tx, _outgoing_rx) = mpsc::channel(RESERVED_OUTBOUND_CONTROL_MESSAGES + 2);
    let notifications = RpcNotificationSender::new(outgoing_tx);
    assert!(notifications.try_notify("network/policyDecision", &serde_json::json!({"n": 1})));
    assert!(notifications.try_notify("network/policyDecision", &serde_json::json!({"n": 2})));
    assert!(!notifications.try_notify("network/policyDecision", &serde_json::json!({"n": 3})));
    notifications
        .response(RequestId::Integer(7), serde_json::json!({"ok": true}))
        .await
        .expect("reserved capacity must remain available for controller responses");
}
async fn read_jsonrpc_line<R>(lines: &mut tokio::io::Lines<BufReader<R>>) -> JSONRPCMessage
where
    R: tokio::io::AsyncRead + Unpin,
{
    let next_line = timeout(Duration::from_secs(1), lines.next_line()).await;
    let line_result = match next_line {
        Ok(line_result) => line_result,
        Err(err) => panic!("timed out waiting for JSON-RPC line: {err}"),
    };
    let maybe_line = match line_result {
        Ok(maybe_line) => maybe_line,
        Err(err) => panic!("failed to read JSON-RPC line: {err}"),
    };
    let line = match maybe_line {
        Some(line) => line,
        None => panic!("server connection closed before JSON-RPC line arrived"),
    };
    match serde_json::from_str::<JSONRPCMessage>(&line) {
        Ok(message) => message,
        Err(err) => panic!("failed to parse JSON-RPC line: {err}"),
    }
}
async fn write_jsonrpc_line<W>(writer: &mut W, message: JSONRPCMessage)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let encoded = match serde_json::to_string(&message) {
        Ok(encoded) => encoded,
        Err(err) => panic!("failed to encode JSON-RPC message: {err}"),
    };
    if let Err(err) = writer.write_all(format!("{encoded}\n").as_bytes()).await {
        panic!("failed to write JSON-RPC line: {err}");
    }
}
#[tokio::test]
async fn rpc_client_matches_out_of_order_responses_by_request_id() {
    let (client_stdin, server_reader) = tokio::io::duplex(4096);
    let (mut server_writer, client_stdout) = tokio::io::duplex(4096);
    let connection =
        JsonRpcConnection::from_stdio(client_stdout, client_stdin, "test-rpc".to_string());
    let (client, _events_rx) = RpcClient::new(connection);
    let server = tokio::spawn(async move {
        let mut lines = BufReader::new(server_reader).lines();
        let first = read_jsonrpc_line(&mut lines).await;
        let second = read_jsonrpc_line(&mut lines).await;
        let (slow_request, fast_request) = match (first, second) {
            (JSONRPCMessage::Request(first_request), JSONRPCMessage::Request(second_request))
                if first_request.method == "slow" && second_request.method == "fast" =>
            {
                (first_request, second_request)
            }
            (JSONRPCMessage::Request(first_request), JSONRPCMessage::Request(second_request))
                if first_request.method == "fast" && second_request.method == "slow" =>
            {
                (second_request, first_request)
            }
            _ => panic!("expected slow and fast requests"),
        };
        write_jsonrpc_line(
            &mut server_writer,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: fast_request.id,
                result: serde_json::json!({ "value": "fast" }),
            }),
        )
        .await;
        write_jsonrpc_line(
            &mut server_writer,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: slow_request.id,
                result: serde_json::json!({ "value": "slow" }),
            }),
        )
        .await;
    });
    let slow_params = serde_json::json!({ "n": 1 });
    let fast_params = serde_json::json!({ "n": 2 });
    let (slow, fast) = tokio::join!(
        client.call::<_, serde_json::Value>("slow", &slow_params),
        client.call::<_, serde_json::Value>("fast", &fast_params),
    );
    let slow = slow.unwrap_or_else(|err| panic!("slow request failed: {err:?}"));
    let fast = fast.unwrap_or_else(|err| panic!("fast request failed: {err:?}"));
    assert_eq!(slow, serde_json::json!({ "value": "slow" }));
    assert_eq!(fast, serde_json::json!({ "value": "fast" }));
    assert_eq!(client.pending_request_count().await, 0);
    if let Err(err) = server.await {
        panic!("server task failed: {err}");
    }
}
#[tokio::test(start_paused = true)]
async fn rpc_client_call_has_no_implicit_deadline() {
    let (client_stdin, server_reader) = tokio::io::duplex(4096);
    let (mut server_writer, client_stdout) = tokio::io::duplex(4096);
    let connection =
        JsonRpcConnection::from_stdio(client_stdout, client_stdin, "test-rpc".to_string());
    let (client, _events_rx) = RpcClient::new(connection);
    let mut lines = BufReader::new(server_reader).lines();
    let params = serde_json::json!({});
    let call = client.call::<_, serde_json::Value>("slow", &params);
    tokio::pin!(call);
    assert!(futures::poll!(call.as_mut()).is_pending());
    let request = match read_jsonrpc_line(&mut lines).await {
        JSONRPCMessage::Request(request) => request,
        other => panic!("expected JSON-RPC request, got {other:?}"),
    };
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(futures::poll!(call.as_mut()).is_pending());
    let expected = serde_json::json!({ "value": "done" });
    write_jsonrpc_line(
        &mut server_writer,
        JSONRPCMessage::Response(JSONRPCResponse {
            id: request.id,
            result: expected.clone(),
        }),
    )
    .await;
    assert_eq!(call.await.expect("RPC response"), expected);
}
#[tokio::test]
async fn rpc_client_timeout_removes_pending_request() {
    let (client_stdin, server_reader) = tokio::io::duplex(4096);
    let (server_writer, client_stdout) = tokio::io::duplex(4096);
    let (release_server_tx, release_server_rx) = tokio::sync::oneshot::channel();
    let connection =
        JsonRpcConnection::from_stdio(client_stdout, client_stdin, "test-rpc".to_string());
    let (client, _events_rx) = RpcClient::new(connection);
    let server = tokio::spawn(async move {
        let mut lines = BufReader::new(server_reader).lines();
        let request = read_jsonrpc_line(&mut lines).await;
        assert!(matches!(request, JSONRPCMessage::Request(_)));
        let _server_writer = server_writer;
        let _ = release_server_rx.await;
    });
    let call_timeout = Duration::from_millis(10);
    let result = client
        .call_with_timeout::<_, serde_json::Value>("slow", &serde_json::json!({}), call_timeout)
        .await;
    assert!(matches!(
        result,
        Err(ra_executor::test_api::rpc::RpcCallError::TimedOut { method, timeout })
            if method == "slow" && timeout == call_timeout
    ));
    assert_eq!(client.pending_request_count().await, 0);
    let _ = release_server_tx.send(());
    if let Err(err) = server.await {
        panic!("server task failed: {err}");
    }
}
#[tokio::test]
async fn rpc_client_cancellation_removes_pending_request_without_further_traffic() {
    let (client_stdin, server_reader) = tokio::io::duplex(4096);
    let (_server_writer, client_stdout) = tokio::io::duplex(4096);
    let connection =
        JsonRpcConnection::from_stdio(client_stdout, client_stdin, "test-rpc".to_string());
    let (client, _events_rx) = RpcClient::new(connection);
    let params = serde_json::json!({});
    let mut call = Box::pin(client.call::<_, serde_json::Value>("waiting", &params));
    let mut lines = BufReader::new(server_reader).lines();
    tokio::select! {
        message = read_jsonrpc_line(&mut lines) => {
            assert!(matches!(message, JSONRPCMessage::Request(_)));
        }
        result = &mut call => panic!("call completed without a response: {result:?}"),
    }
    assert_eq!(client.pending_request_count().await, 1);
    drop(call);
    assert_eq!(client.pending_request_count().await, 0);
}
#[tokio::test]
async fn rpc_client_bounds_in_flight_calls_and_preserves_cleanup() {
    let (outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(1);
    outgoing_tx
        .send(JSONRPCMessage::Notification(JSONRPCNotification {
            method: "blocker".to_string(),
            params: None,
        }))
        .await
        .expect("outbound queue should accept the blocker");
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel(MAX_IN_FLIGHT_REGULAR_CALLS);
    let (_disconnected_tx, disconnected_rx) = tokio::sync::watch::channel(false);
    let connection = JsonRpcConnection {
        outgoing_tx,
        incoming_rx,
        disconnected_rx,
        task_handles: Vec::new(),
        transport: JsonRpcTransport::Plain,
    };
    let (client, _events_rx) = RpcClient::new(connection);
    let client = Arc::new(client);
    let mut calls = JoinSet::new();
    for index in 0..MAX_IN_FLIGHT_REGULAR_CALLS {
        let client = Arc::clone(&client);
        calls.spawn(async move {
            client
                .call::<_, serde_json::Value>("pending", &serde_json::json!({ "index": index }))
                .await
        });
    }
    timeout(Duration::from_secs(1), async {
        while client.pending_request_count().await < MAX_IN_FLIGHT_REGULAR_CALLS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pending requests should reach the regular limit");
    for request_id in 1..=MAX_IN_FLIGHT_REGULAR_CALLS {
        incoming_tx
            .send(JsonRpcConnectionEvent::Message(JSONRPCMessage::Response(
                JSONRPCResponse {
                    id: RequestId::Integer(
                        i64::try_from(request_id).expect("request id should fit in i64"),
                    ),
                    result: serde_json::json!({}),
                },
            )))
            .await
            .expect("reader should accept the spoofed response");
    }
    timeout(Duration::from_secs(1), async {
        while client.pending_request_count().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("spoofed responses should drain response routing");
    let params = serde_json::json!({});
    let overflow = client.call::<_, serde_json::Value>("overflow", &params);
    tokio::pin!(overflow);
    assert!(matches!(
        futures::poll!(overflow.as_mut()),
        std::task::Poll::Ready(Err(RpcCallError::PendingRequestLimitExceeded { limit }))
            if limit == MAX_IN_FLIGHT_REGULAR_CALLS
    ));
    let cleanup_client = Arc::clone(&client);
    calls.spawn(async move {
        let params = serde_json::json!({});
        cleanup_client
            .call_for_cleanup::<_, serde_json::Value>("cleanup", &params)
            .await
    });
    timeout(Duration::from_secs(1), async {
        while client.pending_request_count().await != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup request should use the reserved capacity");
    let cleanup_params = serde_json::json!({});
    let cleanup_overflow = timeout(
        Duration::from_secs(1),
        client.call_for_cleanup::<_, serde_json::Value>("cleanup-overflow", &cleanup_params),
    )
    .await
    .expect("cleanup circuit breaker should not block");
    assert!(matches!(cleanup_overflow, Err(RpcCallError::Closed)));
    assert!(client.is_disconnected());
    assert_eq!(client.pending_request_count().await, 0);
    drop(outgoing_rx);
    while let Some(call) = calls.join_next().await {
        assert!(matches!(
            call.expect("pending call task should join"),
            Err(RpcCallError::Closed)
        ));
    }
}
