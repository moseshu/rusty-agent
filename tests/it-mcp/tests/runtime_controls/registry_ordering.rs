//! Registry reconciliation, request ordering and inventory snapshots.
use super::fixtures::{invoke, settle};
use super::*;

#[tokio::test]
async fn blocked_inventory_does_not_hold_up_toggle_from_another_connection() {
    for params in [json!({}), json!({"detail":"toolsAndAuthOnly"})] {
        let probe = Probe::new("docs");
        let runtime = runtime(&[probe.clone()]);
        runtime.connect_all().await.unwrap();
        probe.inventory_gate.blocked.store(true, Ordering::SeqCst);
        let server = AppServer::new("/tmp/mcp-inventory-order")
            .unwrap()
            .with_mcp_runtime(runtime.clone());
        let (events, receiver) = mpsc::channel(128);
        let task = tokio::spawn(server.run(receiver));
        let mut one = open(&events, 1).await;
        let mut two = open(&events, 2).await;
        call(&events, &mut one, 1, 1, "initialize", init_params(true)).await;
        call(&events, &mut two, 2, 2, "initialize", init_params(true)).await;
        let listed = tokio::spawn({
            let events = events.clone();
            async move { call(&events, &mut one, 1, 3, "mcpServerStatus/list", params).await }
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            probe.inventory_gate.started.notified(),
        )
        .await
        .unwrap();
        let (toggled, _) = tokio::time::timeout(
            Duration::from_secs(1),
            call(
                &events,
                &mut two,
                2,
                4,
                "mcpServer/toggle",
                json!({"name":"docs","enabled":false}),
            ),
        )
        .await
        .expect("toggle must not wait for inventory's five-second request timeout");
        assert_eq!(toggled["result"], json!({}));
        assert_eq!(
            runtime.get_mcp_status()[0].runtime_status(),
            Some(Status::Disabled)
        );
        probe.inventory_gate.permits.add_permits(1);
        let listed = listed.await.unwrap().0;
        assert_eq!(listed["result"]["data"][0]["runtimeStatus"], "disabled");
        assert_eq!(listed["result"]["data"][0]["tools"], json!({}));
        drop(events);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn inventory_captures_its_generation_before_the_returned_future_is_polled() {
    let probe = Probe::new("docs");
    let runtime = runtime(&[probe.clone()]);
    runtime.connect_all().await.unwrap();
    let pending = McpRuntimeControl::inventory(runtime.as_ref(), "docs", false);
    runtime.reconnect_mcp_server(None, false).await.unwrap();
    let status = serde_json::to_value(pending.await).unwrap();
    assert_eq!(status["runtimeStatus"], "connected");
    assert_eq!(status["tools"], json!({}));
    assert_eq!(names(&runtime).await, ["echo_2"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_keeps_healthy_session_and_frozen_tool_binding() {
    let good = Probe::new("good");
    let bad = Probe::new("bad");
    bad.failures.store(1, Ordering::SeqCst);
    let runtime = runtime(&[good.clone(), bad.clone()]);
    runtime.connect_all().await.unwrap();
    bad.fail_listing.store(true, Ordering::SeqCst);
    let tools = runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap();
    let original = good.current_client().unwrap();
    runtime.reload_mcp_servers().await.unwrap();
    assert!(original.is_connected().await);
    invoke(tools[0].as_ref()).await.unwrap();
    assert_eq!(good.connects.load(Ordering::SeqCst), 1);
    assert_eq!(good.cleanups.load(Ordering::SeqCst), 0);
    assert_eq!(bad.connects.load(Ordering::SeqCst), 2);
    assert_eq!(bad.cleanups.load(Ordering::SeqCst), 1);
    bad.current_client().unwrap().cleanup().await.unwrap();
    runtime.reload_mcp_servers().await.unwrap();
    assert_eq!(bad.connects.load(Ordering::SeqCst), 3);
    assert_eq!(good.connects.load(Ordering::SeqCst), 1);
    assert_eq!(good.cleanups.load(Ordering::SeqCst), 0);
    invoke(tools[0].as_ref()).await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn registry_scope_orders_requests_from_different_connections() {
    let probe = Probe::new("docs");
    let runtime = runtime(&[probe.clone()]);
    runtime.connect_all().await.unwrap();
    let server = AppServer::new("/tmp/mcp-registry-order")
        .unwrap()
        .with_mcp_runtime(runtime.clone());
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let mut one = open(&events, 1).await;
    let mut two = open(&events, 2).await;
    call(&events, &mut one, 1, 1, "initialize", init_params(true)).await;
    call(&events, &mut two, 2, 2, "initialize", init_params(true)).await;
    probe.block_cleanup.store(true, Ordering::SeqCst);
    let disabled = tokio::spawn({
        let events = events.clone();
        async move {
            call(
                &events,
                &mut one,
                1,
                3,
                "mcpServer/toggle",
                json!({"name":"docs","enabled":false}),
            )
            .await
        }
    });
    probe.cleanup_started.notified().await;
    let listed = tokio::spawn({
        let events = events.clone();
        async move { call(&events, &mut two, 2, 4, "mcpServerStatus/list", json!({})).await }
    });
    settle().await;
    assert!(!listed.is_finished());
    probe.block_cleanup.store(false, Ordering::SeqCst);
    probe.cleanup_gate.add_permits(1);
    disabled.await.unwrap();
    assert_eq!(
        listed.await.unwrap().0["result"]["data"][0]["runtimeStatus"],
        "disabled"
    );
    drop(events);
    task.await.unwrap();
}

#[test]
fn reload_wire_params_follow_codex_optional_unit_contract() {
    use ra_protocol::server::protocol::ClientRequest;
    for value in [
        json!({"id":1,"method":"config/mcpServer/reload"}),
        json!({"id":1,"method":"config/mcpServer/reload","params":null}),
    ] {
        let request: ClientRequest = serde_json::from_value(value).unwrap();
        assert!(
            !serde_json::to_value(request)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("params")
        );
    }
    let empty: ClientRequest =
        serde_json::from_value(json!({"id":1,"method":"config/mcpServer/reload","params":{}}))
            .unwrap();
    assert_eq!(serde_json::to_value(empty).unwrap()["params"], Value::Null);
    assert!(
        serde_json::from_value::<ClientRequest>(
            json!({"id":1,"method":"config/mcpServer/reload","params":{"unexpected":true}})
        )
        .is_err()
    );
}

#[tokio::test]
async fn reload_then_list_without_waiting_observes_completed_reload() {
    let probe = Probe::new("docs");
    probe.failures.store(1, Ordering::SeqCst);
    let runtime = runtime(&[probe.clone()]);
    runtime.connect_all().await.unwrap();
    probe.started.notified().await;
    probe.block_connect.store(true, Ordering::SeqCst);
    let server = AppServer::new("/tmp/mcp-reload-order")
        .unwrap()
        .with_mcp_runtime(runtime.clone());
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let mut client = open(&events, 1).await;
    call(&events, &mut client, 1, 1, "initialize", init_params(true)).await;
    for (id, method) in [(2, "config/mcpServer/reload"), (3, "mcpServerStatus/list")] {
        events
            .send(TransportEvent::IncomingMessage {
                connection_id: ConnectionId::new(1),
                message: JsonRpcMessage::Request(
                    JsonRpcRequest::new(RequestId::Integer(id), method).with_params(json!({})),
                ),
            })
            .await
            .unwrap();
    }
    probe.started.notified().await;
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert!(client.try_recv().is_err());
    probe.gate.add_permits(1);
    let reloaded = client.recv().await.unwrap();
    let reloaded = serde_json::to_value(reloaded.message()).unwrap();
    assert_eq!(reloaded["id"], 2);
    assert_eq!(reloaded["result"], json!({}));
    let listed = client.recv().await.unwrap();
    let listed = serde_json::to_value(listed.message()).unwrap();
    assert_eq!(listed["id"], 3);
    assert_eq!(listed["result"]["data"][0]["runtimeStatus"], "connected");
    assert!(listed["result"]["data"][0]["tools"]["echo_2"].is_object());
    drop(events);
    task.await.unwrap();
}
