//! Registry replacement, as the source's connection set reconciles on refresh: unchanged
//! identities keep their connection, changed and removed servers are released, new servers start
//! once the host has started the runtime, and configuration and runtime toggles stay distinct.

use super::*;

fn identified(probe: &Arc<Probe>, identity: &str) -> McpServerConfig {
    McpServerConfig::new(probe.clone()).with_connection_identity(identity)
}

async fn wait_connected(runtime: &CodingMcpRuntime, name: &str) {
    for _ in 0..200 {
        if runtime.get_mcp_status().iter().any(|status| {
            status.name() == name && status.runtime_status() == Some(Status::Connected)
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{name} did not connect: {:?}", runtime.get_mcp_status());
}

#[tokio::test]
async fn unchanged_identity_keeps_the_live_connection_and_takes_the_new_filter() {
    let docs = Probe::new("docs");
    let runtime =
        CodingMcpRuntime::new(vec![identified(&docs, "a")], McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let client = docs.current_client().unwrap();

    let statuses = runtime
        .replace_mcp_servers(vec![identified(&docs, "a").with_filter(
            ra_mcp::filter::ToolFilter::new(None, ["echo_1".to_owned()].into_iter().collect()),
        )])
        .await
        .unwrap();

    assert_eq!(statuses[0].runtime_status(), Some(Status::Connected));
    assert_eq!(docs.connects.load(Ordering::SeqCst), 1);
    assert_eq!(docs.cleanups.load(Ordering::SeqCst), 0);
    assert!(client.is_connected().await);
    assert!(
        names(&runtime).await.is_empty(),
        "the replacement filter applies to the retained session"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_identity_and_removal_release_the_previous_entries() {
    let docs = Probe::new("docs");
    let gone = Probe::new("gone");
    let runtime = CodingMcpRuntime::new(
        vec![identified(&docs, "a"), identified(&gone, "a")],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let old = docs.current_client().unwrap();
    let removed = gone.current_client().unwrap();

    let replacement = Probe::new("docs");
    let statuses = runtime
        .replace_mcp_servers(vec![identified(&replacement, "b")])
        .await
        .unwrap();

    assert_eq!(statuses.len(), 1);
    assert!(!old.is_connected().await);
    assert!(!removed.is_connected().await);
    assert_eq!(docs.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(gone.cleanups.load(Ordering::SeqCst), 1);
    wait_connected(&runtime, "docs").await;
    assert_eq!(replacement.connects.load(Ordering::SeqCst), 1);
    assert_eq!(names(&runtime).await, ["echo_1"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_servers_wait_for_host_startup() {
    let docs = Probe::new("docs");
    let runtime = CodingMcpRuntime::new(Vec::new(), McpDiscoveryOptions::new()).unwrap();

    let statuses = runtime
        .replace_mcp_servers(vec![identified(&docs, "a")])
        .await
        .unwrap();

    assert_eq!(statuses[0].runtime_status(), Some(Status::NotStarted));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(docs.connects.load(Ordering::SeqCst), 0);
    runtime.start_background();
    wait_connected(&runtime, "docs").await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn configuration_disable_releases_and_reenable_starts_a_fresh_entry() {
    let docs = Probe::new("docs");
    let runtime =
        CodingMcpRuntime::new(vec![identified(&docs, "a")], McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let old = docs.current_client().unwrap();

    let statuses = runtime
        .replace_mcp_servers(vec![identified(&docs, "a").with_enabled(false)])
        .await
        .unwrap();
    assert_eq!(statuses[0].runtime_status(), Some(Status::Disabled));
    assert!(!old.is_connected().await);
    assert!(names(&runtime).await.is_empty());

    runtime
        .replace_mcp_servers(vec![identified(&docs, "a")])
        .await
        .unwrap();
    wait_connected(&runtime, "docs").await;
    assert_eq!(docs.connects.load(Ordering::SeqCst), 2);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_toggle_survives_an_unchanged_refresh() {
    let docs = Probe::new("docs");
    let runtime =
        CodingMcpRuntime::new(vec![identified(&docs, "a")], McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    runtime.toggle_mcp_server("docs", false).await.unwrap();

    let statuses = runtime
        .replace_mcp_servers(vec![identified(&docs, "a")])
        .await
        .unwrap();

    assert_eq!(statuses[0].runtime_status(), Some(Status::Disabled));
    assert_eq!(docs.connects.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn registrations_without_identity_are_always_replaced() {
    let docs = Probe::new("docs");
    let runtime = runtime(&[docs.clone()]);
    runtime.connect_all().await.unwrap();

    runtime
        .replace_mcp_servers(vec![McpServerConfig::new(docs.clone())])
        .await
        .unwrap();

    assert_eq!(docs.cleanups.load(Ordering::SeqCst), 1);
    wait_connected(&runtime, "docs").await;
    assert_eq!(docs.connects.load(Ordering::SeqCst), 2);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_failing_replacement_does_not_affect_retained_servers() {
    let good = Probe::new("good");
    let runtime =
        CodingMcpRuntime::new(vec![identified(&good, "a")], McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let bad = Probe::new("bad");
    bad.failures.store(1, Ordering::SeqCst);

    runtime
        .replace_mcp_servers(vec![identified(&good, "a"), identified(&bad, "a")])
        .await
        .unwrap();
    for _ in 0..200 {
        if runtime.get_mcp_status()[1].runtime_status() == Some(Status::Failed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let status = runtime.get_mcp_status();
    assert_eq!(status[0].runtime_status(), Some(Status::Connected));
    assert_eq!(status[1].runtime_status(), Some(Status::Failed));
    assert_eq!(names(&runtime).await, ["echo_1"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn replacement_rejects_duplicate_names_before_changing_the_registry() {
    let docs = Probe::new("docs");
    let runtime =
        CodingMcpRuntime::new(vec![identified(&docs, "a")], McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();

    let error = runtime
        .replace_mcp_servers(vec![
            identified(&Probe::new("x"), "a"),
            identified(&Probe::new("x"), "b"),
        ])
        .await
        .unwrap_err();

    assert!(error.to_string().contains("unique"), "{error}");
    assert_eq!(runtime.get_mcp_status()[0].name(), "docs");
    assert_eq!(docs.cleanups.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_replacements_leave_one_set_and_release_the_rest() {
    let original = Probe::new("docs");
    let runtime =
        CodingMcpRuntime::new(vec![identified(&original, "a")], McpDiscoveryOptions::new())
            .unwrap();
    runtime.connect_all().await.unwrap();
    let first = Probe::new("docs");
    let second = Probe::new("docs");

    let (left, right) = tokio::join!(
        runtime.replace_mcp_servers(vec![identified(&first, "b")]),
        runtime.replace_mcp_servers(vec![identified(&second, "c")]),
    );
    left.unwrap();
    right.unwrap();
    wait_connected(&runtime, "docs").await;

    assert_eq!(runtime.get_mcp_status().len(), 1);
    let live = [&first, &second]
        .into_iter()
        .filter(|probe| probe.current_client().is_ok())
        .count();
    assert_eq!(live, 1, "exactly one replacement keeps a live session");
    assert!(original.current_client().is_err());
    runtime.shutdown().await.unwrap();
    assert!(first.current_client().is_err() && second.current_client().is_err());
}
