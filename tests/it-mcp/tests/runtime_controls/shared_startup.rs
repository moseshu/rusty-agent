//! Explicit lifecycle operations share startup with discovery and cached tool invocations.

use super::fixtures::{invoke, settle};
use super::*;

#[derive(Clone, Copy, Debug)]
enum Control {
    Connect,
    Reload,
    Reconnect,
    Enable,
}

async fn apply(runtime: Arc<CodingMcpRuntime>, control: Control) {
    match control {
        Control::Connect => runtime.connect_all().await,
        Control::Reload => runtime.reload_mcp_servers().await,
        Control::Reconnect => runtime.reconnect_mcp_server(None, false).await,
        Control::Enable => runtime.toggle_mcp_server("docs", true).await,
    }
    .unwrap();
}

#[tokio::test]
async fn uncached_discovery_waits_for_explicit_startup_and_lists_the_new_session() {
    for control in [
        Control::Connect,
        Control::Reload,
        Control::Reconnect,
        Control::Enable,
    ] {
        let probe = Probe::new("docs");
        let runtime = runtime(std::slice::from_ref(&probe));
        runtime.connect_all().await.unwrap();
        probe.started.notified().await;
        probe.current_client().unwrap().cleanup().await.unwrap();
        probe.block_connect.store(true, Ordering::SeqCst);
        let operation = tokio::spawn(apply(runtime.clone(), control));
        tokio::time::timeout(Duration::from_secs(1), probe.started.notified())
            .await
            .unwrap();
        assert_eq!(
            runtime.get_mcp_status()[0].runtime_status(),
            Some(Status::Starting)
        );
        let discovery = tokio::spawn({
            let runtime = runtime.clone();
            async move { names(&runtime).await }
        });
        settle().await;
        assert!(
            !discovery.is_finished(),
            "{control:?} discovery must join startup"
        );
        assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
        probe.gate.add_permits(1);
        operation.await.unwrap();
        assert_eq!(discovery.await.unwrap(), ["echo_2"]);
        assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cached_calls_join_explicit_failure_without_starting_an_implicit_retry() {
    for control in [
        Control::Connect,
        Control::Reload,
        Control::Reconnect,
        Control::Enable,
    ] {
        let probe = Probe::new("docs");
        let runtime = runtime(std::slice::from_ref(&probe));
        runtime.connect_all().await.unwrap();
        assert_eq!(names(&runtime).await, ["echo_1"]);
        probe.started.notified().await;
        probe.current_client().unwrap().cleanup().await.unwrap();
        probe.failures.store(1, Ordering::SeqCst);
        probe.block_connect.store(true, Ordering::SeqCst);
        let operation = tokio::spawn(apply(runtime.clone(), control));
        tokio::time::timeout(Duration::from_secs(1), probe.started.notified())
            .await
            .unwrap();
        let tools = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.get_tools(&context(), &BTreeSet::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(tools[0].schema().name(), "echo_1");
        let call = tokio::spawn({
            let tool = tools[0].clone();
            async move { invoke(tool.as_ref()).await }
        });
        settle().await;
        assert!(
            !call.is_finished(),
            "{control:?} invocation must join startup"
        );
        assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
        probe.block_connect.store(false, Ordering::SeqCst);
        probe.gate.add_permits(1);
        operation.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), call)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(invoke(tools[0].as_ref()).await.is_err());
        assert!(names(&runtime).await.is_empty());
        settle().await;
        assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
        assert_eq!(
            runtime.get_mcp_status()[0].runtime_status(),
            Some(Status::Failed)
        );
        runtime.shutdown().await.unwrap();
    }
}
