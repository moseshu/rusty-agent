// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use futures::channel::mpsc as futures_mpsc;
use futures::task::AtomicWaker;
use futures::{Sink, SinkExt, Stream, StreamExt};
use ra_executor_protocol::JSONRPCMessage;
use ra_executor_protocol::JSONRPCRequest;
use ra_executor_protocol::RequestId;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use ra_executor::test_api::connection::*;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
#[tokio::test]
async fn stdio_connection_accepts_message_at_size_limit() -> anyhow::Result<()> {
    let message = test_jsonrpc_message();
    let encoded = serde_json::to_string(&message)?;
    let max_message_len = encoded.len();
    for line_ending in [b"\n".as_slice(), b"\r\n".as_slice()] {
        let (reader, mut peer) =
            tokio::io::duplex(max_message_len.saturating_add(line_ending.len()));
        let mut connection = JsonRpcConnection::from_stdio_with_max_message_len(
            reader,
            tokio::io::sink(),
            "test stdio peer".to_string(),
            max_message_len,
            None,
        );
        peer.write_all(encoded.as_bytes()).await?;
        peer.write_all(line_ending).await?;
        let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv())
            .await?
            .expect("stdio connection should report the message");
        match event {
            JsonRpcConnectionEvent::QueuedRequest { request, .. } => {
                assert_eq!(JSONRPCMessage::Request(request), message)
            }
            event => anyhow::bail!("expected JSON-RPC message, got {event:?}"),
        }
        drop(peer);
        drop(connection);
    }
    Ok(())
}
#[tokio::test]
async fn stdio_connection_rejects_overlong_unterminated_message() -> anyhow::Result<()> {
    let max_message_len: usize = 32;
    let (reader, mut peer) = tokio::io::duplex(max_message_len.saturating_add(1));
    let mut connection = JsonRpcConnection::from_stdio_with_max_message_len(
        reader,
        tokio::io::sink(),
        "hostile stdio peer".to_string(),
        max_message_len,
        None,
    );
    let overlong_message = vec![b'x'; max_message_len + 1];
    peer.write_all(&overlong_message).await?;
    let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv())
        .await?
        .expect("stdio connection should report the framing violation");
    match event {
        JsonRpcConnectionEvent::Disconnected { reason } => assert_eq!(
            reason,
            Some(
                "JSON-RPC message from hostile stdio peer exceeds maximum length of 32 bytes"
                    .to_string()
            )
        ),
        event => anyhow::bail!("expected stdio disconnect, got {event:?}"),
    }
    drop(peer);
    drop(connection);
    Ok(())
}
#[tokio::test]
async fn client_stdio_connection_rejects_overlong_request() -> anyhow::Result<()> {
    let request = serde_json::json!({
        "params": { "padding": "x".repeat(MAX_CLIENT_INBOUND_REQUEST_LEN) },
        "method": "network/policyRequest",
        "id": null,
    });
    let encoded = serde_json::to_vec(&request)?;
    let (reader, mut peer) = tokio::io::duplex(encoded.len().saturating_add(1));
    let mut connection = JsonRpcConnection::client_from_stdio(
        reader,
        tokio::io::sink(),
        "hostile stdio peer".to_string(),
    );
    peer.write_all(&encoded).await?;
    peer.write_all(b"\n").await?;
    assert!(matches!(
        timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
        Some(JsonRpcConnectionEvent::Disconnected { reason: Some(reason) })
            if reason == format!(
                "JSON-RPC message from hostile stdio peer exceeds maximum length of {MAX_CLIENT_INBOUND_REQUEST_LEN} bytes"
            )
    ));
    drop(peer);
    drop(connection);
    Ok(())
}
#[tokio::test]
async fn client_websocket_connection_rejects_overlong_request_and_accepts_large_response()
-> anyhow::Result<()> {
    let (client_websocket, mut server_websocket) = websocket_pair().await?;
    let mut connection = JsonRpcConnection::from_websocket(client_websocket, "test".into());
    let response = serde_json::json!({
        "id": 1,
        "result": { "padding": "x".repeat(MAX_CLIENT_INBOUND_REQUEST_LEN) },
    });
    server_websocket
        .send(Message::Text(response.to_string().into()))
        .await?;
    assert!(matches!(
        timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
        Some(JsonRpcConnectionEvent::Message(JSONRPCMessage::Response(_)))
    ));
    let request = serde_json::json!({
        "params": { "padding": "x".repeat(MAX_CLIENT_INBOUND_REQUEST_LEN) },
        "method": "network/policyRequest",
        "id": 2,
    });
    server_websocket
        .send(Message::Binary(request.to_string().into_bytes().into()))
        .await?;
    assert!(matches!(
        timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
        Some(JsonRpcConnectionEvent::Disconnected { reason: Some(reason) })
            if reason == format!(
                "JSON-RPC message from test exceeds maximum length of {MAX_CLIENT_INBOUND_REQUEST_LEN} bytes"
            )
    ));
    drop(connection);
    Ok(())
}
#[test]
fn client_request_limit_fails_closed_for_malformed_oversized_frames() {
    let malformed = format!(
        "{{\"params\":{{\"padding\":\"{}\"}},\"method\":\"network/policyRequest\",\"id\":",
        "x".repeat(MAX_CLIENT_INBOUND_REQUEST_LEN)
    );
    let message = serde_json::from_str::<JSONRPCMessage>(&malformed);
    assert_eq!(
        client_inbound_message_exceeded_limit(
            message.as_ref(),
            malformed.len(),
            MAX_CLIENT_INBOUND_REQUEST_LEN,
        ),
        Some(MAX_CLIENT_INBOUND_REQUEST_LEN)
    );
}
#[tokio::test]
async fn websocket_connection_sends_configured_ping() -> anyhow::Result<()> {
    let (client_websocket, mut server_websocket) = websocket_pair().await?;
    let connection = JsonRpcConnection::from_websocket_stream(
        client_websocket,
        "test".into(),
        Some(Duration::from_millis(25)),
        None,
    );
    let message = timeout(Duration::from_secs(1), server_websocket.next())
        .await?
        .expect("websocket should stay open")?;
    assert!(matches!(message, Message::Ping(_)));
    drop(connection);
    Ok(())
}
#[tokio::test]
async fn websocket_connection_ignores_server_pong() -> anyhow::Result<()> {
    let (client_websocket, mut server_websocket) = websocket_pair().await?;
    let mut connection = JsonRpcConnection::from_websocket(client_websocket, "test".into());
    server_websocket
        .send(Message::Pong(b"check".to_vec().into()))
        .await?;
    assert!(
        timeout(Duration::from_millis(50), connection.incoming_rx.recv())
            .await
            .is_err()
    );
    drop(connection);
    Ok(())
}
#[tokio::test]
async fn websocket_connection_reports_server_close() -> anyhow::Result<()> {
    let (client_websocket, mut server_websocket) = websocket_pair().await?;
    let mut connection = JsonRpcConnection::from_websocket(client_websocket, "test".into());
    server_websocket.close(None).await?;
    assert!(matches!(
        timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
        Some(JsonRpcConnectionEvent::Disconnected { reason: None })
    ));
    drop(connection);
    Ok(())
}
#[tokio::test]
async fn websocket_connection_accepts_binary_jsonrpc_message() -> anyhow::Result<()> {
    let (client_websocket, mut server_websocket) = websocket_pair().await?;
    let mut connection = JsonRpcConnection::from_websocket(client_websocket, "test".into());
    let message = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: "test".to_string(),
        params: None,
        trace: None,
    });
    server_websocket
        .send(Message::Binary(serde_json::to_vec(&message)?.into()))
        .await?;
    let Some(JsonRpcConnectionEvent::QueuedRequest { request, .. }) =
        timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?
    else {
        anyhow::bail!("expected a queued JSON-RPC request");
    };
    assert_eq!(JSONRPCMessage::Request(request), message);
    drop(connection);
    Ok(())
}
#[tokio::test]
async fn websocket_connection_keeps_outbound_message_while_send_is_backpressured()
-> anyhow::Result<()> {
    let (websocket, control, mut outbound_rx) = ControlledWebSocket::new(false);
    let mut connection =
        JsonRpcConnection::from_websocket_stream(websocket, "test".into(), None, None);
    let message = test_jsonrpc_message();
    connection.outgoing_tx.send(message.clone()).await?;
    control.wait_for_blocked_write().await?;
    control.send_inbound(Message::Pong(b"check".to_vec().into()))?;
    assert!(
        timeout(Duration::from_millis(50), connection.incoming_rx.recv())
            .await
            .is_err()
    );
    control.set_write_ready();
    assert!(matches!(
        timeout(Duration::from_secs(1), outbound_rx.next()).await?,
        Some(Message::Text(text)) if serde_json::from_str::<JSONRPCMessage>(&text)? == message
    ));
    drop(connection);
    Ok(())
}
async fn websocket_pair() -> anyhow::Result<(
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WebSocketStream<tokio::net::TcpStream>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        accept_async(stream).await.map_err(anyhow::Error::from)
    });
    let (client_websocket, _) = connect_async(websocket_url).await?;
    let server_websocket = server_task.await??;
    Ok((client_websocket, server_websocket))
}
fn test_jsonrpc_message() -> JSONRPCMessage {
    JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: "test".to_string(),
        params: None,
        trace: None,
    })
}
struct ControlledWebSocket {
    inbound_rx: futures_mpsc::UnboundedReceiver<Result<Message, std::convert::Infallible>>,
    outbound_tx: futures_mpsc::UnboundedSender<Message>,
    write_ready: Arc<AtomicBool>,
    write_blocked: Arc<AtomicBool>,
    write_blocked_waker: Arc<AtomicWaker>,
    write_waker: Arc<AtomicWaker>,
}
struct ControlledWebSocketHandle {
    inbound_tx: futures_mpsc::UnboundedSender<Result<Message, std::convert::Infallible>>,
    write_ready: Arc<AtomicBool>,
    write_blocked: Arc<AtomicBool>,
    write_blocked_waker: Arc<AtomicWaker>,
    write_waker: Arc<AtomicWaker>,
}
impl ControlledWebSocket {
    fn new(
        write_ready: bool,
    ) -> (
        Self,
        ControlledWebSocketHandle,
        futures_mpsc::UnboundedReceiver<Message>,
    ) {
        let (inbound_tx, inbound_rx) = futures_mpsc::unbounded();
        let (outbound_tx, outbound_rx) = futures_mpsc::unbounded();
        let write_ready = Arc::new(AtomicBool::new(write_ready));
        let write_blocked = Arc::new(AtomicBool::new(false));
        let write_blocked_waker = Arc::new(AtomicWaker::new());
        let write_waker = Arc::new(AtomicWaker::new());
        (
            Self {
                inbound_rx,
                outbound_tx,
                write_ready: Arc::clone(&write_ready),
                write_blocked: Arc::clone(&write_blocked),
                write_blocked_waker: Arc::clone(&write_blocked_waker),
                write_waker: Arc::clone(&write_waker),
            },
            ControlledWebSocketHandle {
                inbound_tx,
                write_ready,
                write_blocked,
                write_blocked_waker,
                write_waker,
            },
            outbound_rx,
        )
    }
}
impl ControlledWebSocketHandle {
    fn send_inbound(&self, message: Message) -> anyhow::Result<()> {
        self.inbound_tx
            .unbounded_send(Ok(message))
            .map_err(anyhow::Error::from)
    }
    fn set_write_ready(&self) {
        self.write_ready.store(true, Ordering::Release);
        self.write_waker.wake();
    }
    async fn wait_for_blocked_write(&self) -> anyhow::Result<()> {
        timeout(
            Duration::from_secs(1),
            futures::future::poll_fn(|cx| {
                if self.write_blocked.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    self.write_blocked_waker.register(cx.waker());
                    Poll::Pending
                }
            }),
        )
        .await?;
        Ok(())
    }
}
impl Sink<Message> for ControlledWebSocket {
    type Error = std::convert::Infallible;
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.write_ready.load(Ordering::Acquire) {
            Poll::Ready(Ok(()))
        } else {
            self.write_blocked.store(true, Ordering::Release);
            self.write_blocked_waker.wake();
            self.write_waker.register(cx.waker());
            Poll::Pending
        }
    }
    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.outbound_tx
            .unbounded_send(item)
            .expect("test outbound receiver should stay open");
        Ok(())
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}
impl Stream for ControlledWebSocket {
    type Item = Result<Message, std::convert::Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inbound_rx).poll_next(cx)
    }
}
