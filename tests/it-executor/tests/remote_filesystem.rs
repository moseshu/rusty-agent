//! Loopback wire regression for remote filesystem ownership, coalescing and cleanup.
use futures::{SinkExt, StreamExt};
use ra_executor::{EnvironmentManager, GetMetadataOptions, ReadFileOptions, WriteFileOptions};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_paths::PathUri;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn foreign_paths_stay_remote_and_stream_drop_closes_the_handle() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel();
    let metadata_calls = Arc::new(AtomicUsize::new(0));
    let calls = metadata_calls.clone();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        loop {
            let message = tokio::select! {_=&mut stop_rx=>break,message=ws.next()=>message};
            let Some(Ok(message)) = message else { break };
            let Message::Text(text) = message else {
                continue;
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            let Some(id) = request.get("id") else {
                continue;
            };
            let method = request["method"].as_str().unwrap();
            let params = &request["params"];
            let result = match method {
                "initialize" => {
                    json!({"sessionId":"fs-session","environmentInfo":{"shell":{"name":"sh","path":"/bin/sh"},"capabilities":{"sandboxedFileStreaming":true}}})
                }
                "fs/readFile" => {
                    assert_eq!(params["path"], "file:///C:/executor/data.txt");
                    json!({"dataBase64":"cmVtb3Rl"})
                }
                "fs/getMetadata" => {
                    assert_eq!(params["path"], "file:///C:/executor/data.txt");
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    json!({"isDirectory":false,"isFile":true,"isSymlink":false,"size":6,"createdAtMs":0,"modifiedAtMs":0})
                }
                "fs/writeFile" => {
                    assert_eq!(params["path"], "file:///C:/executor/data.txt");
                    json!({})
                }
                "fs/open" => {
                    assert_eq!(params["path"], "file:///C:/executor/data.txt");
                    json!({"handleId":params["handleId"]})
                }
                "fs/readBlock" => json!({"chunk":"cmVtb3Rl","eof":false}),
                "fs/close" => {
                    closed_tx.send(params["handleId"].clone()).unwrap();
                    json!({})
                }
                other => panic!("unexpected method {other}"),
            };
            ws.send(Message::text(
                json!({"jsonrpc":"2.0","id":id,"result":result}).to_string(),
            ))
            .await
            .unwrap();
        }
    });
    let manager = EnvironmentManager::without_environments(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    ));
    manager
        .upsert_environment("executor".into(), url, None)
        .unwrap();
    let environment = manager.get_environment("executor").unwrap();
    tokio::time::timeout(Duration::from_secs(3), environment.wait_until_ready())
        .await
        .unwrap()
        .unwrap();
    let fs = environment.get_filesystem();
    let path = PathUri::parse("file:///C:/executor/data.txt").unwrap();
    assert_eq!(
        fs.read_file_text(&path, ReadFileOptions::default(), None)
            .await
            .unwrap(),
        "remote"
    );
    let (first, second) = tokio::join!(
        fs.get_metadata(&path, GetMetadataOptions::default(), None),
        fs.get_metadata(&path, GetMetadataOptions::default(), None)
    );
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
    fs.write_file(&path, b"new".to_vec(), WriteFileOptions::default(), None)
        .await
        .unwrap();
    fs.get_metadata(&path, GetMetadataOptions::default(), None)
        .await
        .unwrap();
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 2);
    let mut stream = fs.read_file_stream(&path, None).await.unwrap();
    assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"remote");
    drop(stream);
    let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(closed.as_str().is_some_and(|id| !id.is_empty()));
    stop_tx.send(()).unwrap();
    server.await.unwrap();
}
