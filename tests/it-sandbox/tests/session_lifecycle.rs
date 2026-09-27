//! Closing an instrumented session directly, and what a close lets out when the session's manifest
//! carries mount authority.
//!
//! Ported from the four `test_sandbox_session_aclose_*` tests in the reference's `test_runtime.py`.
//! They drive the reference's `SandboxSession(inner).aclose()`; here that is
//! [`InstrumentedSession`]'s `close`, over an in-memory session that counts the steps.

#[path = "support/memory_session.rs"]
mod memory_session;

use std::sync::Arc;

use memory_session::MemorySession;
use ra_core::sandbox::{
    Entry, ErrorCode, Manifest, Mount, MountPattern, MountProvider, MountStrategy,
    MountpointOptions, OpName, S3Mount, SandboxError, SandboxSession, pre_stop_hook,
};
use ra_sandbox::instrumentation::InstrumentedSession;

/// A manifest whose mount carries `secret` as its key, the reference's `_external_mount_manifest`.
fn external_mount_manifest(secret: &str) -> Manifest {
    let mount = Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            access_key_id: Some("access-key".to_owned()),
            secret_access_key: Some(secret.to_owned()),
            ..S3Mount::default()
        }),
        MountStrategy::InContainer {
            pattern: MountPattern::Mountpoint(MountpointOptions::default()),
        },
    )
    .expect("a supported mount");
    Manifest::new()
        .with_root("/workspace")
        .with_entry("remote", Entry::mount(mount))
}

/// The reference's `_assert_mount_error_redacted`: the replacement says only that a protected
/// configuration was involved, and nothing of what failed underneath is reachable from it.
fn assert_mount_error_redacted(error: &SandboxError, secret: &str) {
    assert!(
        error.to_string().contains("protected mount configuration"),
        "{error}"
    );
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
    assert!(error.context().is_empty());
    assert!(std::error::Error::source(error).is_none());
    assert!(error.is_data_redacted());
}

fn wrap(inner: &Arc<MemorySession>) -> InstrumentedSession {
    InstrumentedSession::new(inner.clone(), None, None).expect("wrap")
}

/// `test_sandbox_session_aclose_runs_public_cleanup_lifecycle`.
#[tokio::test]
async fn closing_stops_shuts_down_and_releases_the_dependencies() {
    let inner = MemorySession::new(Manifest::new());
    let session = wrap(&inner);

    session.close().await.expect("close");

    assert_eq!(inner.stops(), 1);
    assert_eq!(inner.shutdowns(), 1);
    assert_eq!(inner.dependency_closes(), 1);
}

/// `test_sandbox_session_aclose_closes_dependencies_when_stop_fails`.
#[tokio::test]
async fn a_failed_stop_skips_the_shutdown_and_still_releases_the_dependencies() {
    let inner = MemorySession::new(Manifest::new());
    inner.fail_stop("stop failed");
    let session = wrap(&inner);

    let error = session.close().await.expect_err("the stop failed");

    assert_eq!(error.to_string(), "stop failed");
    assert_eq!(inner.stops(), 1);
    assert_eq!(inner.shutdowns(), 0);
    assert_eq!(inner.dependency_closes(), 1);
}

/// `test_sandbox_session_aclose_redacts_pre_stop_hook_failure`.
#[tokio::test]
async fn a_failed_pre_stop_callback_over_mount_authority_is_redacted() {
    let secret = "pre-stop-hook-secret";
    let inner = MemorySession::new(external_mount_manifest(secret));
    let session = wrap(&inner);
    let message = format!("pre-stop hook failed with {secret}");
    session.register_pre_stop_hook(pre_stop_hook(move || {
        let message = message.clone();
        async move {
            Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Stop,
                message,
            ))
        }
    }));

    let error = session.close().await.expect_err("the callback failed");

    assert_mount_error_redacted(&error, secret);
    assert_eq!(inner.stops(), 0, "the workspace is not persisted");
    assert_eq!(inner.shutdowns(), 1);
    assert_eq!(inner.dependency_closes(), 1);
}

/// `test_sandbox_session_aclose_redacts_dependency_close_failure`.
#[tokio::test]
async fn a_failed_dependency_release_over_mount_authority_is_redacted() {
    let secret = "dependency-close-secret";
    let inner = MemorySession::new(external_mount_manifest(secret));
    inner.fail_close_dependencies(&format!("dependency close failed with {secret}"));
    let session = wrap(&inner);

    let error = session.close().await.expect_err("the release failed");

    assert_mount_error_redacted(&error, secret);
    assert_eq!(inner.stops(), 1);
    assert_eq!(inner.shutdowns(), 1);
    assert_eq!(inner.dependency_closes(), 1);
}

/// The same boundary on an operation the wrapper records: a command failure that quotes the
/// configuration is replaced before it is returned.
#[tokio::test]
async fn a_recorded_operation_over_mount_authority_is_redacted() {
    let secret = "exec-secret";
    let inner = MemorySession::new(external_mount_manifest(secret));
    let session = wrap(&inner);

    let error = session
        .resolve_exposed_port(8080)
        .await
        .expect_err("no port was published");

    assert_eq!(error.error_code(), ErrorCode::ExposedPortUnavailable);
    assert!(error.is_data_redacted());
    assert!(error.context().is_empty());
}

/// Without authority in the manifest, nothing is replaced.
#[tokio::test]
async fn a_failure_without_mount_authority_passes_through_unchanged() {
    let inner = MemorySession::new(Manifest::new().with_root("/workspace"));
    let session = wrap(&inner);

    let error = session
        .resolve_exposed_port(8080)
        .await
        .expect_err("no port was published");

    assert_eq!(error.error_code(), ErrorCode::ExposedPortUnavailable);
    assert!(!error.is_data_redacted());
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("not_configured")
    );
}
