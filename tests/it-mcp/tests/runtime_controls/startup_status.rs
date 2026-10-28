//! Actionable startup notifications and lifecycle diagnostic transitions.
use super::*;

#[tokio::test]
async fn startup_notifications_preserve_failure_details_and_timeout_recovery_hint() {
    for timeout in [None, Some(Duration::from_millis(40))] {
        let probe = Probe::new("docs");
        if timeout.is_some() {
            probe.block_connect.store(true, Ordering::SeqCst);
        } else {
            probe.failures.store(1, Ordering::SeqCst);
        }
        let mut config = McpServerConfig::new(probe.clone());
        if let Some(timeout) = timeout {
            config = config.with_lifecycle_options(
                McpServerManagerOptions::default()
                    .with_connect_timeout(Some(timeout))
                    .unwrap(),
            );
        }
        let runtime = CodingMcpRuntime::new(vec![config], McpDiscoveryOptions::new()).unwrap();
        let server = AppServer::new("/tmp/mcp-error-display")
            .unwrap()
            .with_mcp_runtime(runtime.clone());
        let (events, receiver) = mpsc::channel(128);
        let task = tokio::spawn(server.run(receiver));
        let mut client = open(&events, 1).await;
        call(&events, &mut client, 1, 1, "initialize", init_params(false)).await;
        let (_, mut updates) = call(
            &events,
            &mut client,
            1,
            2,
            "mcpServer/reconnect",
            json!({"failedOnly":false}),
        )
        .await;
        let failure = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(update) = updates.iter().find(|value| {
                    value["method"] == "mcpServer/startupStatus/updated"
                        && value["params"]["status"] == "failed"
                }) {
                    break update.clone();
                }
                let queued = client.recv().await.unwrap();
                updates.push(serde_json::to_value(queued.message()).unwrap());
            }
        })
        .await
        .unwrap();
        let error = failure["params"]["error"].as_str().unwrap();
        if timeout.is_some() {
            assert!(error.contains("timed out after 0.04 seconds"), "{error}");
            assert!(error.contains("with_connect_timeout"), "{error}");
        } else {
            assert!(
                error.contains("MCP client for `docs` failed to start"),
                "{error}"
            );
            assert!(error.contains("unsupported protocol"), "{error}");
            assert!(!error.contains("secret-token"));
            assert!(
                std::error::Error::source(runtime.errors()[0].2.as_ref())
                    .unwrap()
                    .to_string()
                    .contains("secret-token")
            );
        }
        drop(events);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn starting_clears_old_failure_and_disabled_cleanup_failure_stays_disabled() {
    let probe = Probe::new("docs");
    probe.failures.store(1, Ordering::SeqCst);
    let runtime = runtime(std::slice::from_ref(&probe));
    runtime.connect_all().await.unwrap();
    let mut changes = McpRuntimeControl::subscribe(runtime.as_ref());
    runtime.reconnect_mcp_server(None, true).await.unwrap();
    let starting = changes.recv().await.unwrap();
    assert_eq!(starting.runtime_status(), Some(Status::Starting));
    assert!(starting.startup_error().is_none());
    let wire = serde_json::to_value(starting).unwrap();
    assert!(wire.get("lifecyclePhase").is_none());
    assert!(wire.get("lifecycleErrorCode").is_none());
    while changes.try_recv().is_ok() {}
    probe.fail_cleanup.store(true, Ordering::SeqCst);
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    let mut updates = Vec::new();
    while let Ok(status) = changes.try_recv() {
        updates.push(status);
    }
    assert!(!updates.is_empty());
    assert!(
        updates
            .iter()
            .all(|status| status.runtime_status() == Some(Status::Disabled))
    );
    let disabled = serde_json::to_value(runtime.get_mcp_status()).unwrap();
    assert_eq!(disabled[0]["runtimeStatus"], "disabled");
    assert_eq!(disabled[0]["lifecyclePhase"], "cleanup");
    assert!(names(&runtime).await.is_empty());
    runtime.toggle_mcp_server("docs", true).await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.errors()[0].1, "cleanup");
    probe.fail_cleanup.store(false, Ordering::SeqCst);
    runtime.shutdown().await.unwrap();
}
