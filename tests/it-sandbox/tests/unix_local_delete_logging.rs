//! `ra-sandbox::unix_local`: what a failed unmount before a delete writes to the log.
//!
//! One test, in a binary of its own. A log assertion depends on `tracing`'s callsite cache, which
//! is process-wide: a test on another thread that reaches the same `warn!` with no subscriber
//! installed can leave it cached as uninteresting, and this test then sees nothing logged at all.
//! Alone in its process, nothing else can reach that callsite first.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    CreateRequest, Entry, Manifest, MaterializedFile, Mount, MountPattern, MountProvider,
    MountStrategy, PosixPath, RcloneOptions, S3Mount, SandboxClient, SandboxError, SandboxResult,
    SandboxSession,
};
use ra_sandbox::mounts::MountLifecycle;
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// Refuses every unmount with a message that must not reach the log.
struct FailingUnmount;

#[async_trait]
impl MountLifecycle for FailingUnmount {
    async fn activate(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        _dest: &PosixPath,
        _base_dir: &std::path::Path,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        Ok(Vec::new())
    }

    async fn deactivate(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        _dest: &PosixPath,
        _base_dir: &std::path::Path,
    ) -> SandboxResult<()> {
        Err(SandboxError::mount_config("SECRET_UNMOUNT_ERROR"))
    }

    async fn teardown_for_snapshot(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        _path: &PosixPath,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn restore_after_snapshot(
        &self,
        _mount: &Mount,
        _strategy: &MountStrategy,
        _session: &dyn SandboxSession,
        _path: &PosixPath,
    ) -> SandboxResult<()> {
        Ok(())
    }
}

/// Log output, collected in memory.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("captured").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_failed_unmount_is_logged_without_the_mount_path_or_the_error() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = UnixLocalSandboxClient::new().with_mount_lifecycle(Arc::new(FailingUnmount));
    let mount = Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            ..S3Mount::default()
        }),
        MountStrategy::in_container(MountPattern::Rclone(RcloneOptions::default())),
    )
    .expect("supported");
    let session =
        client
            .create(CreateRequest::new().with_manifest(
                Manifest::new().with_entry("SECRET_REMOTE_MOUNT", Entry::mount(mount)),
            ))
            .await
            .expect("create");
    let root = std::path::PathBuf::from(&session.state().manifest().root);

    client.delete(session.as_ref()).await.expect("best effort");

    // The reference's default for tool data in logs: the event is reported, what it was about is
    // not, since a mount's name and its failure can name buckets and credentials.
    let logged = String::from_utf8(captured.0.lock().expect("captured").clone()).expect("utf-8");
    assert!(
        logged.contains("failed to unmount a workspace mount before deleting the root"),
        "{logged}"
    );
    assert!(!logged.contains("SECRET_REMOTE_MOUNT"), "{logged}");
    assert!(!logged.contains("SECRET_UNMOUNT_ERROR"), "{logged}");
    assert!(root.exists(), "a root with a mount still attached is kept");
    std::fs::remove_dir_all(&root).expect("clean up");
}
