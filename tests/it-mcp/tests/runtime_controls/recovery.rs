//! Explicit startup recovery, shared attempts and retry cooldowns.
use super::fixtures::settle;
use super::fixtures::{invoke, seed};
use super::*;

#[tokio::test]
async fn default_failed_startup_waits_for_explicit_recovery() {
    for background in [false, true] {
        let probe = Probe::new("docs");
        probe.failures.store(1, Ordering::SeqCst);
        let runtime = runtime(&[probe.clone()]);
        if background {
            runtime.start_background();
        } else {
            runtime.connect_all().await.unwrap();
        }
        settle().await;
        assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.get_mcp_status()[0].runtime_status(),
            Some(Status::Failed)
        );
        runtime.start_background();
        assert!(names(&runtime).await.is_empty());
        settle().await;
        assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
        runtime.reload_mcp_servers().await.unwrap();
        assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
        assert_eq!(names(&runtime).await, ["echo_2"]);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn opted_in_recovery_starts_on_discovery_after_explicit_operations() {
    let probe = Probe::new("docs");
    probe.failures.store(2, Ordering::SeqCst);
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(probe.clone()).with_automatic_retry(true)],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    runtime.reconnect_mcp_server(None, true).await.unwrap();
    runtime.start_background();
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::Failed)
    );
    assert_eq!(names(&runtime).await, ["echo_3"]);
    assert_eq!(probe.connects.load(Ordering::SeqCst), 3);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_cached_startup_advertises_tools_only_with_recovery_opt_in() {
    for automatic_retry in [false, true] {
        let probe = Probe::new("docs");
        probe.failures.store(1, Ordering::SeqCst);
        let runtime = CodingMcpRuntime::new(
            vec![
                McpServerConfig::new(probe.clone())
                    .with_lazy_startup(true)
                    .with_cached_catalog(seed("docs", McpClientOptions::default()))
                    .with_automatic_retry(automatic_retry),
            ],
            McpDiscoveryOptions::new(),
        )
        .unwrap();
        runtime.connect_all().await.unwrap();
        let tools = runtime
            .get_tools(&context(), &BTreeSet::new())
            .await
            .unwrap();
        assert!(invoke(tools[0].as_ref()).await.is_err());
        settle().await;
        assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.get_mcp_status()[0].runtime_status(),
            Some(Status::Failed)
        );
        let visible = names(&runtime).await;
        if automatic_retry {
            assert_eq!(visible, ["echo_1"]);
            settle().await;
            assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
            assert_eq!(names(&runtime).await, ["echo_2"]);
        } else {
            assert!(visible.is_empty());
            assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn failed_startup_recovery_is_single_flight_and_uses_exponential_backoff() {
    let probe = Probe::new("docs");
    probe.failures.store(3, Ordering::SeqCst);
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(probe.clone()).with_automatic_retry(true)],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert!(names(&runtime).await.is_empty());
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
    for _ in 0..3 {
        runtime.start_background();
    }
    let run = context();
    let reserved = BTreeSet::new();
    let (one, two) = tokio::join!(
        runtime.get_tools(&run, &reserved),
        runtime.get_tools(&run, &reserved)
    );
    one.unwrap();
    two.unwrap();
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(names(&runtime).await.is_empty());
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_secs(1)).await;
    let _ = names(&runtime).await;
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_secs(1)).await;
    let _ = names(&runtime).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runtime.get_mcp_status()[0].runtime_status() == Some(Status::Connected) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 4);
    assert_eq!(names(&runtime).await, ["echo_4"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn recovery_backoff_caps_at_thirty_seconds_and_disable_prevents_retries() {
    let probe = Probe::new("docs");
    probe.failures.store(20, Ordering::SeqCst);
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(probe.clone()).with_automatic_retry(true)],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert!(names(&runtime).await.is_empty());
    settle().await;
    let mut attempts = 2;
    assert_eq!(probe.connects.load(Ordering::SeqCst), attempts);
    for seconds in [1, 2, 4, 8, 16, 30, 30] {
        tokio::time::advance(Duration::from_secs(seconds) - Duration::from_millis(1)).await;
        let _ = names(&runtime).await;
        settle().await;
        assert_eq!(probe.connects.load(Ordering::SeqCst), attempts);
        tokio::time::advance(Duration::from_millis(1)).await;
        let _ = names(&runtime).await;
        settle().await;
        attempts += 1;
        assert_eq!(probe.connects.load(Ordering::SeqCst), attempts);
    }
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(names(&runtime).await.is_empty());
    settle().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), attempts);
    runtime.shutdown().await.unwrap();
}
