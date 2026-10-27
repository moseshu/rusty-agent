// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Split readers keep their revocation wakeup and report denial even when writers fail first.

use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use ra_http::DestinationPolicy;
use ra_http::NetworkPolicyController;
use ra_http::OutboundProxyPolicy::ReqwestDefault;
use ra_http::{
    HttpClientFactory,
    websocket::{WebSocketConnector, network_policy_denial},
};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::{pin::Pin, sync::Arc, task::Context};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

struct WakeFlag(AtomicBool);

impl futures::task::ArcWake for WakeFlag {
    fn wake_by_ref(waker: &Arc<Self>) {
        waker.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn revocation_wakes_split_reader_and_survives_writer_observing_it_first() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = websocket.next().await;
    });
    let controller = NetworkPolicyController::default();
    let policy = controller.policy();
    assert!(controller.publish(policy.revision(), DestinationPolicy::Unrestricted));
    let factory = HttpClientFactory::new(ReqwestDefault).with_network_policy(policy.clone());
    let connector = WebSocketConnector::new(&factory).unwrap();
    let request = url.into_client_request().unwrap();
    let (connection, _) = connector
        .connect(request, WebSocketConfig::default())
        .await
        .unwrap();
    let (mut writer, mut reader) = connection.split();
    let wake = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = futures::task::waker(Arc::clone(&wake));
    let mut context = Context::from_waker(&waker);
    assert!(Pin::new(&mut reader).poll_next(&mut context).is_pending());
    writer.flush().await.unwrap();
    policy.invalidate();
    assert!(wake.0.load(Ordering::SeqCst));
    let error = writer.flush().await.unwrap_err();
    assert!(network_policy_denial(&error).is_some());
    let error = reader.next().await.unwrap().unwrap_err();
    assert!(network_policy_denial(&error).is_some());
    assert!(reader.next().await.is_none());
    server.abort();
}
