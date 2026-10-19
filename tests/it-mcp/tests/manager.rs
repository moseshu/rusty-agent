//! Lifecycle contracts ported from Python's manager and cleanup-state suites.

#[path = "manager/support.rs"]
mod support;
use support::*;

use ra_mcp::{McpServer, McpServerManager, McpServerManagerOptions as Options};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::time::timeout;

#[test]
fn defaults_timeout_validation_and_initial_snapshots_match_python() {
    let probe = Probe::new("same-name");
    let other = Probe::new("same-name");
    let manager =
        McpServerManager::new([probe.clone() as Arc<dyn McpServer>, probe.clone(), other]);
    assert_eq!(manager.all_servers().len(), 2);
    assert_eq!(manager.active_servers().len(), 2);
    assert!(manager.failed_servers().is_empty());
    assert!(manager.errors().is_empty());
    assert_eq!(manager.connect_timeout(), Some(Duration::from_secs(10)));
    assert_eq!(manager.cleanup_timeout(), Some(Duration::from_secs(10)));
    assert!(manager.drop_failed_servers());
    assert!(!manager.strict());
    assert!(manager.suppress_cancelled_error());
    assert!(!manager.connect_in_parallel());
    for duration in [Duration::ZERO, Duration::MAX] {
        assert!(
            Options::default()
                .with_connect_timeout(Some(duration))
                .is_err()
        );
        assert!(
            Options::default()
                .with_cleanup_timeout(Some(duration))
                .is_err()
        );
        assert!(manager.set_connect_timeout(Some(duration)).is_err());
        assert!(manager.set_cleanup_timeout(Some(duration)).is_err());
    }
    manager.set_connect_timeout(None).unwrap();
    manager.set_cleanup_timeout(None).unwrap();
    assert_eq!(manager.connect_timeout(), None);
    assert_eq!(manager.cleanup_timeout(), None);
    let mut snapshot = manager.all_servers();
    snapshot.clear();
    assert_eq!(manager.all_servers().len(), 2);
}

#[tokio::test]
async fn connect_and_cleanup_keep_affinity_across_host_tasks_in_both_modes() {
    for parallel in [false, true] {
        let probe = Probe::new("bound");
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default().with_connect_in_parallel(parallel),
        );
        let connect = {
            let manager = manager.clone();
            tokio::spawn(async move {
                let caller = tokio::task::id();
                assert_eq!(manager.connect_all().await.unwrap().len(), 1);
                caller
            })
        };
        let caller = connect.await.unwrap();
        let cleanup = {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager.cleanup_all().await.unwrap();
            })
        };
        cleanup.await.unwrap();
        assert_ne!(*probe.owner.lock().unwrap(), Some(caller));
        assert_eq!(probe.connect_count(), 1);
        assert_eq!(probe.cleanup_count(), 1);
        assert!(manager.active_servers().is_empty());
        assert!(!probe.open.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn cleanup_is_reversed_and_distinct_servers_own_distinct_tasks() {
    let first = Probe::new("first");
    let mut second = Probe::new("second");
    Arc::get_mut(&mut second).unwrap().events = first.events.clone();
    let manager = manager(&[first.clone(), second.clone()], Options::default());
    assert_eq!(manager.connect_all().await.unwrap().len(), 2);
    manager.cleanup_all().await.unwrap();
    let events = first.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|(name, action, _)| (name.as_str(), *action))
            .collect::<Vec<_>>(),
        [
            ("first", "connect"),
            ("second", "connect"),
            ("second", "cleanup"),
            ("first", "cleanup")
        ]
    );
    assert_ne!(events[0].2, events[1].2);
}

#[tokio::test]
async fn connect_is_idempotent_and_cleanup_all_updates_the_connected_set() {
    let probe = Probe::new("bound");
    let manager = manager(&[probe.clone(), probe.clone()], Options::default());
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    assert_eq!(probe.connect_count(), 1);
    manager.cleanup_all().await.unwrap();
    assert!(manager.active_servers().is_empty());
    assert!(manager.reconnect(true).await.unwrap().is_empty());
    assert_eq!(probe.connect_count(), 1);
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    assert_eq!(probe.connect_count(), 2);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn parallel_connect_does_not_wait_for_a_blocked_peer() {
    let blocked = Probe::new("blocked");
    blocked.block_connect.store(true, Ordering::SeqCst);
    let fast = Probe::new("fast");
    let manager = manager(
        &[blocked.clone(), fast.clone()],
        Options::default().with_connect_in_parallel(true),
    );
    let caller = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.connect_all().await })
    };
    blocked.connect_started.notified().await;
    timeout(Duration::from_secs(1), fast.connect_started.notified())
        .await
        .unwrap();
    assert!(!caller.is_finished());
    blocked.connect_gate.add_permits(1);
    assert_eq!(caller.await.unwrap().unwrap().len(), 2);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn failed_only_reconnect_cleans_partial_resources_and_preserves_healthy_servers() {
    for parallel in [false, true] {
        let flaky = Probe::new("same-name");
        flaky.failures.store(1, Ordering::SeqCst);
        let healthy = Probe::new("same-name");
        let manager = manager(
            &[healthy.clone(), flaky.clone()],
            Options::default().with_connect_in_parallel(parallel),
        );
        assert_eq!(manager.connect_all().await.unwrap().len(), 1);
        assert_eq!(manager.failed_servers().len(), 1);
        assert_eq!(manager.errors().len(), 1);
        let mut failures = manager.failed_servers();
        failures.clear();
        let mut errors = manager.errors();
        errors.clear();
        assert_eq!(manager.reconnect(true).await.unwrap().len(), 2);
        assert_eq!(healthy.connect_count(), 1);
        assert_eq!(healthy.cleanup_count(), 0);
        assert_eq!(flaky.connect_count(), 2);
        assert_eq!(flaky.cleanup_count(), 1);
        assert!(manager.failed_servers().is_empty());
        assert!(manager.errors().is_empty());
        manager.cleanup_all().await.unwrap();
    }
}

#[tokio::test]
async fn cleanup_failure_is_sticky_and_blocks_every_reconnect_entry_point() {
    for failed_only in [false, true] {
        let probe = Probe::new("partial");
        probe.failures.store(1, Ordering::SeqCst);
        probe.cleanup_error.store(1, Ordering::SeqCst);
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default().with_connect_in_parallel(true),
        );
        assert!(manager.connect_all().await.unwrap().is_empty());
        assert!(manager.reconnect(failed_only).await.unwrap().is_empty());
        assert!(manager.connect_all().await.unwrap().is_empty());
        assert!(manager.reconnect(true).await.unwrap().is_empty());
        assert_eq!(probe.connect_count(), 1);
        assert_eq!(probe.cleanup_count(), 1);
        assert!(manager.errors()[0].1.to_string().contains("cleanup failed"));
        assert_eq!(manager.failed_servers().len(), 1);
        assert!(probe.open.load(Ordering::SeqCst));
    }
}

#[tokio::test(start_paused = true)]
async fn connect_timeout_drops_the_future_but_keeps_partial_cleanup_in_the_owner_task() {
    let probe = Probe::new("timeout");
    probe.block_connect.store(true, Ordering::SeqCst);
    let options = Options::default().with_connect_timeout(None).unwrap();
    let manager = manager(std::slice::from_ref(&probe), options);
    manager
        .set_connect_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    assert!(manager.connect_all().await.unwrap().is_empty());
    assert_eq!(manager.errors()[0].1.code(), "tool.timeout");
    assert_eq!(probe.connect_dropped.load(Ordering::SeqCst), 1);
    manager.cleanup_all().await.unwrap();
    assert_eq!(probe.cleanup_count(), 1);
    assert!(!probe.open.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn cleanup_timeout_is_retained_and_never_replaced_by_a_fresh_worker() {
    let probe = Probe::new("timeout");
    let options = Options::default()
        .with_connect_in_parallel(true)
        .with_cleanup_timeout(None)
        .unwrap();
    let manager = manager(std::slice::from_ref(&probe), options);
    manager.connect_all().await.unwrap();
    manager
        .set_cleanup_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    probe.block_cleanup.store(true, Ordering::SeqCst);
    manager.cleanup_all().await.unwrap();
    settle().await;
    assert!(manager.active_servers().is_empty());
    assert_eq!(manager.errors()[0].1.code(), "tool.timeout");
    assert_eq!(probe.cleanup_dropped.load(Ordering::SeqCst), 1);
    assert!(manager.reconnect(false).await.unwrap().is_empty());
    assert!(manager.connect_all().await.unwrap().is_empty());
    assert_eq!(probe.connect_count(), 1);
    assert_eq!(probe.cleanup_count(), 1);
}

#[tokio::test]
async fn cleanup_waiter_drop_retains_the_worker_and_serializes_reconnect() {
    for failed_cleanup in [false, true] {
        let probe = Probe::new("blocking");
        probe.block_cleanup.store(true, Ordering::SeqCst);
        if failed_cleanup {
            probe.cleanup_error.store(1, Ordering::SeqCst);
        }
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default()
                .with_connect_in_parallel(true)
                .with_cleanup_timeout(None)
                .unwrap(),
        );
        manager.connect_all().await.unwrap();
        let cleanup = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.cleanup_all().await })
        };
        probe.cleanup_started.notified().await;
        cleanup.abort();
        assert!(cleanup.await.unwrap_err().is_cancelled());
        assert!(manager.errors()[0].1.is_cancelled());
        assert!(manager.active_servers().is_empty());
        let reconnect = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.connect_all().await })
        };
        settle().await;
        assert!(!reconnect.is_finished());
        assert_eq!(probe.connect_count(), 1);
        probe.cleanup_gate.add_permits(1);
        assert_eq!(
            reconnect.await.unwrap().unwrap().len(),
            usize::from(!failed_cleanup)
        );
        assert_eq!(probe.connect_count(), if failed_cleanup { 1 } else { 2 });
        if !failed_cleanup {
            probe.block_cleanup.store(false, Ordering::SeqCst);
            manager.cleanup_all().await.unwrap();
        } else {
            assert!(manager.errors()[0].1.to_string().contains("cleanup failed"));
        }
    }
}

#[tokio::test]
async fn overlapping_cleanup_and_full_reconnect_do_not_duplicate_cleanup() {
    let probe = Probe::new("blocking");
    probe.block_cleanup.store(true, Ordering::SeqCst);
    let manager = manager(
        std::slice::from_ref(&probe),
        Options::default().with_connect_in_parallel(true),
    );
    manager.connect_all().await.unwrap();
    let first = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.cleanup_all().await })
    };
    probe.cleanup_started.notified().await;
    let second = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.cleanup_all().await })
    };
    let reconnect = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.reconnect(false).await })
    };
    settle().await;
    assert_eq!(probe.cleanup_count(), 1);
    assert!(!second.is_finished());
    assert!(!reconnect.is_finished());
    probe.cleanup_gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(reconnect.await.unwrap().unwrap().len(), 1);
    assert_eq!(probe.connect_count(), 2);
    probe.block_cleanup.store(false, Ordering::SeqCst);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn server_cancelled_cleanup_is_recorded_with_either_suppression_policy() {
    for suppress in [false, true] {
        let probe = Probe::new("cancelled");
        probe.cleanup_error.store(2, Ordering::SeqCst);
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default()
                .with_connect_in_parallel(true)
                .with_suppress_cancelled_error(suppress),
        );
        manager.connect_all().await.unwrap();
        let result = manager.cleanup_all().await;
        assert_eq!(result.is_ok(), suppress);
        assert!(manager.errors()[0].1.is_cancelled());
        assert!(manager.active_servers().is_empty());
        let reconnect = manager.connect_all().await;
        if suppress {
            assert!(reconnect.unwrap().is_empty());
        } else {
            assert!(reconnect.err().unwrap().is_cancelled());
        }
        assert_eq!(probe.connect_count(), 1);
    }
}

#[tokio::test]
async fn server_cancelled_connect_is_recorded_and_strict_failure_cleans_partial_connections() {
    for parallel in [false, true] {
        for suppress in [false, true] {
            let probe = Probe::new("cancelled");
            probe.connect_cancelled.store(true, Ordering::SeqCst);
            let manager = manager(
                std::slice::from_ref(&probe),
                Options::default()
                    .with_strict(true)
                    .with_suppress_cancelled_error(suppress)
                    .with_connect_in_parallel(parallel),
            );
            let result = manager.connect_all().await;
            assert_eq!(result.is_ok(), suppress);
            assert!(manager.errors()[0].1.is_cancelled());
            assert_eq!(manager.failed_servers().len(), 1);
            if !suppress {
                assert_eq!(probe.cleanup_count(), 1);
            }
            manager.cleanup_all().await.unwrap();
            assert!(!probe.open.load(Ordering::SeqCst));
        }
    }
}

#[tokio::test]
async fn strict_connect_cleans_new_successes_and_partial_failures() {
    for parallel in [false, true] {
        let healthy = Probe::new("healthy");
        let flaky = Probe::new("flaky");
        flaky.failures.store(2, Ordering::SeqCst);
        let manager = manager(
            &[healthy.clone(), flaky.clone()],
            Options::default()
                .with_strict(true)
                .with_connect_in_parallel(parallel),
        );
        assert!(manager.connect_all().await.is_err());
        assert!(manager.active_servers().is_empty());
        assert_eq!(healthy.cleanup_count(), 1);
        assert_eq!(flaky.cleanup_count(), 1);
        flaky.failures.store(0, Ordering::SeqCst);
        assert_eq!(manager.reconnect(false).await.unwrap().len(), 2);
        assert_eq!(manager.connect_all().await.unwrap().len(), 2);
        manager.cleanup_all().await.unwrap();
    }
}

#[tokio::test]
async fn drop_failed_false_retains_all_servers_in_the_active_view() {
    let probe = Probe::new("flaky");
    probe.failures.store(1, Ordering::SeqCst);
    let manager = manager(
        std::slice::from_ref(&probe),
        Options::default().with_drop_failed_servers(false),
    );
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    assert_eq!(manager.failed_servers().len(), 1);
    manager.cleanup_all().await.unwrap();
    assert_eq!(manager.active_servers().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn manager_drop_cancels_pending_connect_and_uses_the_current_cleanup_deadline() {
    let probe = Probe::new("emergency");
    probe.block_connect.store(true, Ordering::SeqCst);
    probe.block_cleanup.store(true, Ordering::SeqCst);
    let options = Options::default()
        .with_connect_timeout(None)
        .unwrap()
        .with_cleanup_timeout(None)
        .unwrap();
    let manager = manager(std::slice::from_ref(&probe), options);
    let caller = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.connect_all().await })
    };
    probe.connect_started.notified().await;
    caller.abort();
    assert!(caller.await.err().unwrap().is_cancelled());
    assert!(manager.errors()[0].1.is_cancelled());
    manager
        .set_cleanup_timeout(Some(Duration::from_millis(30)))
        .unwrap();
    drop(manager);
    probe.cleanup_started.notified().await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(probe.connect_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(probe.cleanup_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(probe.cleanup_count(), 1);
}

#[tokio::test]
async fn strict_connect_preserves_preexisting_active_servers_and_strict_retry_refreshes_views() {
    for parallel in [false, true] {
        let healthy = Probe::new("healthy");
        let flaky = Probe::new("flaky");
        flaky.failures.store(2, Ordering::SeqCst);
        let manager = manager(
            &[healthy.clone(), flaky.clone()],
            Options::default().with_connect_in_parallel(parallel),
        );
        assert_eq!(manager.connect_all().await.unwrap().len(), 1);
        // The upstream flaky server does not leave a partial resource; model that variant here.
        flaky.open.store(false, Ordering::SeqCst);
        manager.set_strict(true);
        assert!(manager.strict());
        assert!(manager.connect_all().await.is_err());
        assert_eq!(manager.active_servers().len(), 1);
        assert_eq!(healthy.connect_count(), 1);
        assert_eq!(healthy.cleanup_count(), 0);
        assert_eq!(manager.reconnect(true).await.unwrap().len(), 2);
        manager.cleanup_all().await.unwrap();
    }

    let first = Probe::new("first");
    let second = Probe::new("second");
    first.failures.store(1, Ordering::SeqCst);
    second.failures.store(2, Ordering::SeqCst);
    let manager = manager(&[first.clone(), second.clone()], Options::default());
    assert!(manager.connect_all().await.unwrap().is_empty());
    manager.set_strict(true);
    assert!(manager.reconnect(true).await.is_err());
    assert_eq!(manager.active_servers().len(), 1);
    assert_eq!(manager.active_servers()[0].name(), "first");
    assert_eq!(manager.failed_servers()[0].name(), "second");
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn self_cancelled_connect_does_not_terminate_the_worker() {
    let probe = Probe::new("self-cancelling");
    probe.connect_cancelled.store(true, Ordering::SeqCst);
    let manager = manager(std::slice::from_ref(&probe), Options::default());
    assert!(manager.connect_all().await.unwrap().is_empty());
    let owner = *probe.owner.lock().unwrap();
    probe.connect_cancelled.store(false, Ordering::SeqCst);
    // This probe variant releases its partial resource itself before retrying, as upstream's
    // SelfCancellingThenOkServer does. No cleanup command should be needed to revive the worker.
    probe.open.store(false, Ordering::SeqCst);
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    assert_eq!(*probe.owner.lock().unwrap(), owner);
    assert_eq!(probe.cleanup_count(), 0);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cleanup_wait_is_bounded_while_a_connect_without_a_deadline_is_still_running() {
    let probe = Probe::new("blocked");
    probe.block_connect.store(true, Ordering::SeqCst);
    let options = Options::default()
        .with_connect_in_parallel(true)
        .with_connect_timeout(None)
        .unwrap()
        .with_cleanup_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let manager = manager(std::slice::from_ref(&probe), options);
    let connecting = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.connect_all().await })
    };
    probe.connect_started.notified().await;
    connecting.abort();
    assert!(connecting.await.err().unwrap().is_cancelled());
    manager.cleanup_all().await.unwrap();
    assert_eq!(manager.errors()[0].1.code(), "tool.timeout");
    assert!(manager.connect_all().await.unwrap().is_empty());
    assert_eq!(probe.connect_count(), 1);
    probe.connect_gate.add_permits(1);
    probe.cleanup_finished.notified().await;
    probe.block_connect.store(false, Ordering::SeqCst);
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn dropping_a_waiter_for_the_lifecycle_lock_does_not_change_the_owner_operation() {
    let probe = Probe::new("blocked");
    probe.block_cleanup.store(true, Ordering::SeqCst);
    let manager = manager(std::slice::from_ref(&probe), Options::default());
    manager.connect_all().await.unwrap();
    let owner = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.cleanup_all().await })
    };
    probe.cleanup_started.notified().await;
    let waiter = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.connect_all().await })
    };
    settle().await;
    waiter.abort();
    assert!(waiter.await.err().unwrap().is_cancelled());
    assert!(manager.errors().is_empty());
    probe.cleanup_gate.add_permits(1);
    owner.await.unwrap().unwrap();
    assert!(manager.active_servers().is_empty());
    assert_eq!(probe.cleanup_count(), 1);
}

#[tokio::test]
async fn real_stdio_servers_keep_healthy_sessions_on_failed_only_retry_and_replace_them_on_full_retry()
 {
    use ra_mcp::client::stdio::{McpServerStdio, McpStdioParams};
    let healthy = Arc::new(
        McpServerStdio::new(McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")))
            .with_name("healthy"),
    );
    let unavailable = Arc::new(
        McpServerStdio::new(McpStdioParams::new("/rusty-agent-missing-mcp-fixture"))
            .with_name("missing"),
    );
    let manager = McpServerManager::with_options(
        [healthy.clone() as Arc<dyn McpServer>, unavailable],
        Options::default().with_connect_in_parallel(true),
    );
    assert_eq!(manager.connect_all().await.unwrap().len(), 1);
    assert_eq!(manager.failed_servers()[0].name(), "missing");
    let original = healthy.client().unwrap();
    assert!(!healthy.list_tools().await.unwrap().is_empty());
    assert_eq!(manager.reconnect(true).await.unwrap().len(), 1);
    assert!(original.is_connected().await);
    assert_eq!(manager.reconnect(false).await.unwrap().len(), 1);
    assert!(!original.is_connected().await);
    assert!(!healthy.list_tools().await.unwrap().is_empty());
    manager.cleanup_all().await.unwrap();
    assert!(!healthy.is_connected().await);
}

#[tokio::test]
async fn sequential_cleanup_can_be_retried_on_the_original_task_after_failure() {
    let probe = Probe::new("retry-cleanup");
    probe.failures.store(1, Ordering::SeqCst);
    probe.cleanup_error.store(1, Ordering::SeqCst);
    let manager = manager(std::slice::from_ref(&probe), Options::default());
    assert!(manager.connect_all().await.unwrap().is_empty());
    assert!(manager.reconnect(true).await.unwrap().is_empty());
    assert_eq!(probe.connect_count(), 1);
    assert_eq!(probe.cleanup_count(), 1);
    let owner = *probe.owner.lock().unwrap();
    probe.cleanup_error.store(0, Ordering::SeqCst);
    assert_eq!(manager.reconnect(true).await.unwrap().len(), 1);
    assert_eq!(probe.connect_count(), 2);
    assert_eq!(probe.cleanup_count(), 2);
    assert_eq!(*probe.owner.lock().unwrap(), owner);
    manager.cleanup_all().await.unwrap();
    manager.cleanup_all().await.unwrap();
    assert_eq!(probe.cleanup_count(), 4);
}

#[tokio::test]
async fn unsuppressed_cleanup_cancellation_propagates_during_strict_connect_recovery() {
    for parallel in [false, true] {
        let probe = Probe::new("failed-connect");
        probe.failures.store(1, Ordering::SeqCst);
        probe.cleanup_error.store(2, Ordering::SeqCst);
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default()
                .with_connect_in_parallel(parallel)
                .with_strict(true)
                .with_suppress_cancelled_error(false),
        );
        let error = manager.connect_all().await.err().unwrap();
        assert!(error.is_cancelled());
        assert!(manager.errors()[0].1.is_cancelled());
        // Cleanup interrupted the failure handler before it could restore the prior active view.
        assert_eq!(manager.active_servers().len(), 1);
        assert_eq!(probe.cleanup_count(), 1);
    }
}

#[tokio::test]
async fn sequential_strict_failure_does_not_clean_an_idle_worker_not_attempted_this_round() {
    let first = Probe::new("first");
    let failing = Probe::new("failing");
    let untouched = Probe::new("untouched");
    let manager = manager(
        &[first.clone(), failing.clone(), untouched.clone()],
        Options::default(),
    );
    manager.connect_all().await.unwrap();
    manager.cleanup_all().await.unwrap();
    failing.failures.store(1, Ordering::SeqCst);
    manager.set_strict(true);

    assert!(manager.connect_all().await.is_err());
    assert_eq!(first.connect_count(), 2);
    assert_eq!(failing.connect_count(), 2);
    assert_eq!(untouched.connect_count(), 1);
    assert_eq!(untouched.cleanup_count(), 1);
    assert_eq!(first.cleanup_count(), 2);
    assert_eq!(failing.cleanup_count(), 2);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn active_snapshot_is_published_at_lifecycle_boundaries() {
    for parallel in [false, true] {
        let first = Probe::new("first");
        let blocked = Probe::new("blocked");
        blocked.block_connect.store(true, Ordering::SeqCst);
        let manager = manager(
            &[first.clone(), blocked.clone()],
            Options::default().with_connect_in_parallel(parallel),
        );
        let connecting = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.connect_all().await })
        };
        blocked.connect_started.notified().await;
        assert_eq!(manager.active_servers().len(), 2);
        blocked.connect_gate.add_permits(1);
        assert_eq!(connecting.await.unwrap().unwrap().len(), 2);

        blocked.block_cleanup.store(true, Ordering::SeqCst);
        let cleaning = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.cleanup_all().await })
        };
        blocked.cleanup_started.notified().await;
        assert_eq!(manager.active_servers().len(), 2);
        blocked.cleanup_gate.add_permits(1);
        cleaning.await.unwrap().unwrap();
        assert!(manager.active_servers().is_empty());

        manager.set_drop_failed_servers(false);
        assert!(manager.active_servers().is_empty());
        blocked.block_connect.store(false, Ordering::SeqCst);
        blocked.block_cleanup.store(false, Ordering::SeqCst);
        assert_eq!(manager.connect_all().await.unwrap().len(), 2);
        manager.cleanup_all().await.unwrap();
        assert_eq!(manager.active_servers().len(), 2);
        manager.set_drop_failed_servers(true);
        assert_eq!(manager.active_servers().len(), 2);
        manager.cleanup_all().await.unwrap();
        assert!(manager.active_servers().is_empty());

        blocked.block_connect.store(true, Ordering::SeqCst);
        let reconnecting = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.reconnect(false).await })
        };
        blocked.connect_started.notified().await;
        // Full reconnect publishes the cleaned view before starting its new connection phase.
        assert!(manager.active_servers().is_empty());
        blocked.connect_gate.add_permits(1);
        assert_eq!(reconnecting.await.unwrap().unwrap().len(), 2);
        manager.cleanup_all().await.unwrap();
    }
}

#[tokio::test]
async fn worker_panic_propagates_and_a_subsequent_connect_can_replace_the_finished_worker() {
    for parallel in [false, true] {
        for strict in [false, true] {
            let probe = Probe::new("panicking");
            probe.connect_panics.store(1, Ordering::SeqCst);
            probe
                .allow_unconnected_cleanup
                .store(true, Ordering::SeqCst);
            let manager = manager(
                std::slice::from_ref(&probe),
                Options::default()
                    .with_connect_in_parallel(parallel)
                    .with_strict(strict),
            );
            let error = manager
                .connect_all()
                .await
                .err()
                .expect("worker panic must propagate");
            assert!(!error.is_cancelled());
            assert!(panic_join_source(error.as_ref()).is_panic());
            assert_eq!(manager.failed_servers().len(), 1);
            assert!(manager.active_servers().is_empty());
            // The sequential contract calls cleanup; parallel skips a discarded unconnected worker.
            assert_eq!(probe.cleanup_count(), usize::from(!parallel));
            assert_eq!(manager.connect_all().await.unwrap().len(), 1);
            assert_eq!(probe.connect_count(), 2);
            let connect_tasks = probe
                .events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, action, task)| (*action == "connect").then_some(*task))
                .collect::<Vec<_>>();
            assert_ne!(connect_tasks[0], connect_tasks[1]);
            assert!(manager.failed_servers().is_empty());
            assert!(manager.errors().is_empty());
            manager.cleanup_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn sequential_strict_cleanup_reverses_successes_then_failures_as_upstream_does() {
    let cancelled = Probe::new("cancelled");
    cancelled.connect_cancelled.store(true, Ordering::SeqCst);
    let mut healthy = Probe::new("healthy");
    Arc::get_mut(&mut healthy).unwrap().events = cancelled.events.clone();
    let mut failing = Probe::new("failing");
    Arc::get_mut(&mut failing).unwrap().events = cancelled.events.clone();
    failing.failures.store(1, Ordering::SeqCst);
    let manager = manager(
        &[cancelled.clone(), healthy, failing],
        Options::default().with_strict(true),
    );
    assert!(manager.connect_all().await.is_err());
    let cleaned = cancelled
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(name, action, _)| (*action == "cleanup").then_some(name.clone()))
        .collect::<Vec<_>>();
    assert_eq!(cleaned, ["failing", "cancelled", "healthy"]);
    manager.cleanup_all().await.unwrap();
}

#[tokio::test]
async fn cleanup_worker_panic_is_recorded_and_prevents_reconnection() {
    for parallel in [false, true] {
        let probe = Probe::new("panic-cleanup");
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default().with_connect_in_parallel(parallel),
        );
        manager.connect_all().await.unwrap();
        probe.cleanup_error.store(3, Ordering::SeqCst);
        let error = manager
            .cleanup_all()
            .await
            .expect_err("cleanup panic must propagate");
        assert!(!error.is_cancelled());
        assert!(panic_join_source(error.as_ref()).is_panic());
        assert!(manager.active_servers().is_empty());
        assert!(!manager.errors()[0].1.is_cancelled());
        probe.cleanup_error.store(0, Ordering::SeqCst);
        assert!(manager.connect_all().await.is_err());
        assert!(manager.reconnect(true).await.is_err());
        assert_eq!(probe.connect_count(), 1);
        assert_eq!(probe.cleanup_count(), 1);
    }
}

#[tokio::test]
async fn server_connect_error_with_a_panic_join_source_remains_an_ordinary_failure() {
    for parallel in [false, true] {
        let probe = Probe::new("child-task-error");
        probe
            .connect_error_with_join_source
            .store(true, Ordering::SeqCst);
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default().with_connect_in_parallel(parallel),
        );
        assert!(manager.connect_all().await.unwrap().is_empty());
        let error = manager.errors()[0].1.clone();
        assert_eq!(error.code(), "config");
        assert!(
            std::error::Error::source(error.as_ref())
                .unwrap()
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic()
        );
        probe
            .connect_error_with_join_source
            .store(false, Ordering::SeqCst);
        assert_eq!(manager.reconnect(true).await.unwrap().len(), 1);
        assert!(manager.errors().is_empty());
        manager.cleanup_all().await.unwrap();
    }
}

#[tokio::test]
async fn server_cleanup_error_with_a_panic_join_source_remains_an_ordinary_failure() {
    for parallel in [false, true] {
        let probe = Probe::new("child-task-error");
        let manager = manager(
            std::slice::from_ref(&probe),
            Options::default().with_connect_in_parallel(parallel),
        );
        manager.connect_all().await.unwrap();
        probe.cleanup_error.store(4, Ordering::SeqCst);
        manager.cleanup_all().await.unwrap();
        let error = manager.errors()[0].1.clone();
        assert_eq!(error.code(), "config");
        assert!(
            std::error::Error::source(error.as_ref())
                .unwrap()
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic()
        );
        probe.cleanup_error.store(0, Ordering::SeqCst);
        manager.cleanup_all().await.unwrap();
    }
}
