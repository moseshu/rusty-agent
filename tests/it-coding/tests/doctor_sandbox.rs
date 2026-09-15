//! Regression tests for inconclusive probes and scratch directory ownership.

use std::path::Path;

use ra_coding::doctor::{Check, CheckResult, Verdict, test_api};
use ra_exec::{
    sandbox::{ExecEnvironment, NetworkAccess, SandboxLevel, SandboxPolicy, platform_backend},
    session::ProcessManager,
};

/// A probe that never started says nothing about the sandbox — but it must never read as a pass.
///
/// This is the host whose image has no `bash`: the check is skipped, the report refuses to claim
/// confinement, and the verdict says the claim is unproven rather than accusing the sandbox.
#[tokio::test]
async fn missing_probe_shell_is_skipped_and_leaves_confinement_unproven() {
    let root = tempfile::tempdir().expect("root");
    let manager = ProcessManager::default();
    let check = test_api::network_check(
        &manager,
        &manager,
        root.path(),
        &root.path().join("missing-shell"),
    )
    .await;
    assert_eq!(check.result, CheckResult::Skipped, "{}", check.detail);
    assert!(
        check.detail.contains("could not be started"),
        "{}",
        check.detail
    );
    assert_eq!(test_api::verdict_with(check.clone()), Verdict::Unverified);
    assert!(
        !test_api::healthy_with(check),
        "unproven must never read as confined"
    );
}

#[tokio::test]
async fn successful_connection_is_not_reported_as_denied() {
    let root = tempfile::tempdir().expect("root");
    let manager = ProcessManager::default();
    let check =
        test_api::network_check(&manager, &manager, root.path(), Path::new("/bin/bash")).await;
    assert_eq!(check.result, CheckResult::Failed, "{}", check.detail);
    assert!(check.detail.contains("exit Some(0)"), "{}", check.detail);
}

#[test]
fn skipped_required_check_is_not_healthy() {
    let check = Check {
        what: "reach the network".to_owned(),
        result: CheckResult::Skipped,
        detail: "listener unavailable".to_owned(),
    };
    assert!(!test_api::healthy_with(check.clone()));
    assert_eq!(test_api::verdict_with(check), Verdict::Unverified);
}

/// A check that ran and came out wrong is the one verdict entitled to accuse the machine.
#[test]
fn a_failed_check_is_the_only_thing_that_reports_the_machine_unconfined() {
    assert_eq!(
        test_api::verdict_with(Check {
            what: "write outside every writable root".to_owned(),
            result: CheckResult::Failed,
            detail: "the file was created".to_owned(),
        }),
        Verdict::NotConfined
    );
    assert_eq!(
        test_api::verdict_with(Check {
            what: "write inside the workspace root".to_owned(),
            result: CheckResult::Ok,
            detail: "allowed".to_owned(),
        }),
        Verdict::Confined
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_broken_control_probe_cannot_establish_denial() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().expect("root");
    let shell = root.path().join("broken-shell");
    std::fs::write(&shell, "#!/bin/sh\necho RA_PROBE_READY\nexit 42\n").expect("shell");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let manager = ProcessManager::default();
    let check = test_api::network_check(&manager, &manager, root.path(), &shell).await;
    assert_eq!(check.result, CheckResult::Failed);
    assert!(
        check.detail.contains("control probe did not connect"),
        "{}",
        check.detail
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_unfinished_probe_is_cancelled_and_fails_the_check() {
    use std::{os::unix::fs::PermissionsExt as _, time::Duration};
    let root = tempfile::tempdir().expect("root");
    let shell = root.path().join("stalled-shell");
    std::fs::write(&shell, "#!/bin/sh\nwhile :; do :; done\n").expect("shell");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let manager = ProcessManager::default();
    let check = test_api::network_check(&manager, &manager, root.path(), &shell).await;
    assert_eq!(check.result, CheckResult::Failed);
    assert!(check.detail.contains("deadline"), "{}", check.detail);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !manager.active_sessions().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled probe reaped");
}

#[cfg(unix)]
#[test]
fn scratch_cleanup_does_not_adopt_existing_paths_or_follow_links() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let parent = tempfile::tempdir().expect("parent");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("sentinel"), "keep").expect("sentinel");
    let existing = parent.path().join("ra-doctor-existing");
    std::fs::create_dir(&existing).expect("existing directory");
    let link = parent.path().join("ra-doctor-link");
    symlink(outside.path(), &link).expect("symlink");

    let (first, first_path) = test_api::scratch_dir_in(parent.path()).expect("first");
    let (second, second_path) = test_api::scratch_dir_in(parent.path()).expect("second");
    assert_ne!(first_path, second_path);
    assert_eq!(
        std::fs::metadata(&first_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    drop(first);
    drop(second);
    assert!(!first_path.exists());
    assert!(!second_path.exists());
    assert!(existing.is_dir());
    assert!(link.is_symlink());
    assert_eq!(
        std::fs::read_to_string(outside.path().join("sentinel")).expect("sentinel"),
        "keep"
    );
}

/// The same missing shell, but with a real backend in force — which is the production path.
///
/// **This is the case the unconfined test above cannot reach.** With a sandbox, the program actually
/// launched is the wrapper (`sandbox-exec`, or bubblewrap), and the wrapper exists: it starts, fails
/// to exec the shell inside it, and exits non-zero. There is no spawn error to classify, so a report
/// keyed on "did the launch fail" called this a sandbox that does not confine — measured as exit 71
/// under Seatbelt. The marker each probe prints first is what tells the two apart.
#[tokio::test]
async fn a_missing_shell_under_a_real_backend_is_still_only_unverified() {
    let Some(level) = deliverable_level() else {
        // No backend on this machine, so there is no production path to exercise. The unconfined
        // shape of this case is covered above.
        return;
    };
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace = workspace.path().canonicalize().expect("canonical root");
    let confined = confined_manager(&workspace, level);

    let check = test_api::network_check(
        &confined,
        &confined,
        &workspace,
        &workspace.join("no-such-shell"),
    )
    .await;

    assert_eq!(
        check.result,
        CheckResult::Skipped,
        "a wrapper that started and found no shell says nothing about confinement: {}",
        check.detail
    );
    assert_eq!(test_api::verdict_with(check.clone()), Verdict::Unverified);
    assert!(
        !test_api::healthy_with(check),
        "unproven must never read as confined"
    );
}

/// A real, confined manager whose writable and readable root is `root`.
fn confined_manager(root: &std::path::Path, level: SandboxLevel) -> ProcessManager {
    ProcessManager::default().with_environment(
        ExecEnvironment::new().with_sandbox(
            SandboxPolicy::new(level)
                .with_network(NetworkAccess::Denied)
                .with_writable_root(root)
                .with_readable_root(root),
        ),
    )
}

/// The strongest level this machine can actually deliver, if it can deliver one.
fn deliverable_level() -> Option<SandboxLevel> {
    platform_backend()?.available_level().ok()
}
