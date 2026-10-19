//! Thread preparation schedules shared startup without waiting for transport.
use super::*;

#[tokio::test]
async fn agent_preparation_returns_while_uncached_startup_is_shared_with_discovery() {
    use ra_coding::{agent::build_agent_with_host, host::CodingHost};
    let probe = Probe::new("docs");
    probe.block_connect.store(true, Ordering::SeqCst);
    let runtime = runtime(&[probe.clone()]);
    let host = CodingHost::open(std::env::temp_dir())
        .unwrap()
        .with_mcp_runtime(runtime.clone());
    for _ in 0..3 {
        tokio::time::timeout(
            Duration::from_secs(1),
            build_agent_with_host(
                AgentId::new("coding"),
                "Coding",
                &ra_core::prompt::PromptRole::Main,
                &host,
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }
    probe.started.notified().await;
    let discovery = tokio::spawn({
        let runtime = runtime.clone();
        async move { names(&runtime).await }
    });
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert!(!discovery.is_finished());
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    probe.gate.add_permits(1);
    assert_eq!(discovery.await.unwrap(), ["echo_1"]);
    runtime.shutdown().await.unwrap();
}
