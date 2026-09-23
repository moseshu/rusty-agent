//! Reading and writing what a snapshot names.
//!
//! `ra_core::sandbox::Snapshot` says which storage holds a workspace and under which id. This puts
//! bytes in that storage and takes them back out.
//!
//! # A store rather than a method on the snapshot
//!
//! The reference hangs `persist`, `restore` and `restorable` on the snapshot object itself, so a
//! host adds storage by subclassing it. Here a snapshot is data that a session state serializes,
//! and data cannot carry behaviour across a round trip: a snapshot read back from disk would have
//! to find its implementation again anyway. So the three operations live on a [`SnapshotStore`]
//! that dispatches on the snapshot's type, and a host with storage of its own supplies one instead
//! of subclassing. [`BuiltinSnapshotStore`] covers the kinds the reference builds in.
//!
//! Every operation is handed the session's dependency container, as the reference hands it to the
//! snapshot's methods: a remote snapshot names its storage client only by a dependency key, and the
//! client itself is resolved from there each time.
//!
//! # Whole archives, not streams
//!
//! The reference passes file objects, and the local kind copies them a chunk at a time. Here a
//! workspace archive is a `Vec<u8>`, which is the shape `SandboxSession::persist_workspace` and
//! `hydrate_workspace` already have, so a snapshot is bounded by memory in the same way the rest of
//! the archive path is. It changes with them.

pub mod defaults;
pub mod lifecycle;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::sandbox::{
    CloseDependency, Dependencies, DependencyValue, ErrorCode, LOCAL_SNAPSHOT_TYPE,
    NOOP_SNAPSHOT_TYPE, OpName, REMOTE_SNAPSHOT_TYPE, SandboxError, SandboxResult, Snapshot,
};
use uuid::Uuid;

/// What a local snapshot's file is called, after the id.
const LOCAL_SNAPSHOT_SUFFIX: &str = ".tar";

/// The path a snapshot that stores nothing reports when asked to restore.
const NOOP_SNAPSHOT_PATH: &str = "<noop>";

/// Where a session's workspace archive is kept between a stop and the next start.
#[async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Writes the workspace archive to the storage `snapshot` names.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SnapshotPersistError`] when the storage refused it.
    async fn persist(
        &self,
        snapshot: &Snapshot,
        data: Vec<u8>,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<()>;

    /// Reads back the workspace archive the storage holds.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SnapshotRestoreError`] when the archive could not be read, or
    /// [`ErrorCode::SnapshotNotRestorable`] when the storage holds nothing by design.
    async fn restore(
        &self,
        snapshot: &Snapshot,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<Vec<u8>>;

    /// Whether there is something to restore.
    ///
    /// Asked before a start decides between restoring and materializing the manifest, so a `false`
    /// here is an ordinary answer rather than a failure.
    ///
    /// # Errors
    ///
    /// Returns the storage's failure to answer.
    async fn restorable(
        &self,
        snapshot: &Snapshot,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<bool>;
}

/// The storage the reference builds in: a tar file on this machine, a client resolved from the
/// session's dependencies, and nowhere at all.
///
/// A snapshot type this does not know is refused rather than ignored. Quietly treating unknown
/// storage as empty would turn a resume into a fresh workspace and report success.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinSnapshotStore;

#[async_trait]
impl SnapshotStore for BuiltinSnapshotStore {
    async fn persist(
        &self,
        snapshot: &Snapshot,
        data: Vec<u8>,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<()> {
        match snapshot.snapshot_type() {
            NOOP_SNAPSHOT_TYPE => Ok(()),
            LOCAL_SNAPSHOT_TYPE => persist_local(snapshot, data),
            REMOTE_SNAPSHOT_TYPE => persist_remote(snapshot, data, dependencies).await,
            _ => Err(unsupported(snapshot, OpName::SnapshotPersist)),
        }
    }

    async fn restore(
        &self,
        snapshot: &Snapshot,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<Vec<u8>> {
        match snapshot.snapshot_type() {
            NOOP_SNAPSHOT_TYPE => Err(SandboxError::snapshot_not_restorable(
                snapshot.id(),
                NOOP_SNAPSHOT_PATH,
            )),
            LOCAL_SNAPSHOT_TYPE => restore_local(snapshot),
            REMOTE_SNAPSHOT_TYPE => restore_remote(snapshot, dependencies).await,
            _ => Err(unsupported(snapshot, OpName::SnapshotRestore)),
        }
    }

    async fn restorable(
        &self,
        snapshot: &Snapshot,
        dependencies: &Arc<Dependencies>,
    ) -> SandboxResult<bool> {
        match snapshot.snapshot_type() {
            NOOP_SNAPSHOT_TYPE => Ok(false),
            LOCAL_SNAPSHOT_TYPE => Ok(local_path(snapshot, OpName::SnapshotRestore)?.is_file()),
            REMOTE_SNAPSHOT_TYPE => remote_restorable(snapshot, dependencies).await,
            _ => Err(unsupported(snapshot, OpName::SnapshotRestore)),
        }
    }
}

/// The storage client a remote snapshot's bytes go through.
///
/// A host binds one in the session's dependencies, under the key the snapshot names. The reference
/// finds the three methods on whatever object is bound there; this is that shape as a trait.
///
/// `exists` has a default that refuses, because on the reference a client without it still uploads
/// and downloads: only asking whether there is something to restore fails.
#[async_trait]
pub trait RemoteSnapshotClient: Send + Sync {
    /// Stores the workspace archive under `snapshot_id`.
    ///
    /// # Errors
    ///
    /// Returns the storage's failure.
    async fn upload(&self, snapshot_id: &str, data: Vec<u8>) -> Result<(), RemoteSnapshotError>;

    /// Reads back the archive stored under `snapshot_id`.
    ///
    /// # Errors
    ///
    /// Returns the storage's failure.
    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>, RemoteSnapshotError>;

    /// Whether anything is stored under `snapshot_id`.
    ///
    /// # Errors
    ///
    /// Returns the storage's failure; by default, that the client cannot answer.
    async fn exists(&self, snapshot_id: &str) -> Result<bool, RemoteSnapshotError> {
        let _ = snapshot_id;
        Err(RemoteSnapshotError::new(
            "Remote snapshot client must implement `exists(snapshot_id, ...)`",
        ))
    }
}

/// Packs a remote snapshot client as a dependency value, for binding under a snapshot's key.
///
/// The container never closes a client packed this way; see
/// [`closable_remote_snapshot_client_dependency`] for one it should.
#[must_use]
pub fn remote_snapshot_client_dependency(client: Arc<dyn RemoteSnapshotClient>) -> DependencyValue {
    DependencyValue::new(Arc::new(client))
}

/// Packs a remote snapshot client that holds a resource, so that a factory binding that owns its
/// result has the client closed with the session's dependencies.
///
/// The snapshot reads the client through its trait; the container closes the concrete client
/// behind it, once however many times it was packed.
#[must_use]
pub fn closable_remote_snapshot_client_dependency<T>(client: Arc<T>) -> DependencyValue
where
    T: RemoteSnapshotClient + CloseDependency + 'static,
{
    let readable: Arc<dyn RemoteSnapshotClient> = client.clone();
    DependencyValue::with_closer(Arc::new(readable), client)
}

/// A remote storage client's failure.
pub struct RemoteSnapshotError(Box<dyn std::error::Error + Send + Sync>);

impl RemoteSnapshotError {
    /// A failure described by `message` alone.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into().into())
    }

    /// A failure caused by `error`.
    #[must_use]
    pub fn from_error(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
}

impl fmt::Debug for RemoteSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, formatter)
    }
}

impl fmt::Display for RemoteSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for RemoteSnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// The path a remote snapshot reports in its errors: the dependency its client came from.
fn remote_path(snapshot: &Snapshot) -> String {
    format!(
        "<remote:{}>",
        snapshot.remote_client_dependency_key().unwrap_or_default()
    )
}

/// Resolves the storage client a remote snapshot names.
///
/// A missing or mistyped dependency is a configuration problem rather than a storage failure. The
/// reference lets the lookup's own exception escape; a sandbox error has to carry a code, and this
/// is the one that says the host wired something up wrong.
async fn remote_client(
    snapshot: &Snapshot,
    dependencies: &Arc<Dependencies>,
    op: OpName,
) -> SandboxResult<Arc<dyn RemoteSnapshotClient>> {
    let key = snapshot.remote_client_dependency_key().unwrap_or_default();
    let client = dependencies
        .require_as::<Arc<dyn RemoteSnapshotClient>>(key, Some("RemoteSnapshot"))
        .await
        .map_err(|error| {
            SandboxError::new(ErrorCode::SandboxConfigInvalid, op, error.to_string())
                .with_context("snapshot_id", snapshot.id())
                .with_context("client_dependency_key", key)
                .with_cause(error)
        })?;
    Ok(Arc::clone(&client))
}

/// Uploads the archive; every failure, the client's lookup included, is a persist failure, as it is
/// on the reference.
async fn persist_remote(
    snapshot: &Snapshot,
    data: Vec<u8>,
    dependencies: &Arc<Dependencies>,
) -> SandboxResult<()> {
    let failed = || SandboxError::snapshot_persist(snapshot.id(), &remote_path(snapshot));
    let client = remote_client(snapshot, dependencies, OpName::SnapshotPersist)
        .await
        .map_err(|error| failed().with_sandbox_cause(error))?;
    client
        .upload(snapshot.id(), data)
        .await
        .map_err(|error| failed().with_cause(error))
}

/// Downloads the archive; every failure, the client's lookup included, is a restore failure.
async fn restore_remote(
    snapshot: &Snapshot,
    dependencies: &Arc<Dependencies>,
) -> SandboxResult<Vec<u8>> {
    let failed = || SandboxError::snapshot_restore(snapshot.id(), &remote_path(snapshot));
    let client = remote_client(snapshot, dependencies, OpName::SnapshotRestore)
        .await
        .map_err(|error| failed().with_sandbox_cause(error))?;
    client
        .download(snapshot.id())
        .await
        .map_err(|error| failed().with_cause(error))
}

/// Asks the client whether anything is stored.
///
/// The reference does not wrap this one: a lookup failure escapes as itself, and so does the
/// client's. Here the lookup reports as a configuration problem, and the client's failure as a
/// restore failure, because that is the question being asked on the way to a restore.
async fn remote_restorable(
    snapshot: &Snapshot,
    dependencies: &Arc<Dependencies>,
) -> SandboxResult<bool> {
    let client = remote_client(snapshot, dependencies, OpName::SnapshotRestore).await?;
    client.exists(snapshot.id()).await.map_err(|error| {
        SandboxError::snapshot_restore(snapshot.id(), &remote_path(snapshot))
            .with_context("reason", error.to_string())
            .with_cause(error)
    })
}

/// Writes the archive, leaving whatever was already stored untouched if anything goes wrong.
///
/// Through a temporary file and a rename, as the reference does: a snapshot half-written over the
/// previous one is worse than no new snapshot, because the session it belonged to is gone by the
/// time anybody finds out.
fn persist_local(snapshot: &Snapshot, data: Vec<u8>) -> SandboxResult<()> {
    let path = local_path(snapshot, OpName::SnapshotPersist)?;
    let temp_path = temp_path(&path);
    let failed = |error: std::io::Error| {
        // Best effort, as the reference is: the archive was not stored, and failing to tidy up
        // after that failure is not what the caller needs to hear about.
        let _ = std::fs::remove_file(&temp_path);
        SandboxError::snapshot_persist(snapshot.id(), &path.to_string_lossy()).with_cause(error)
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    std::fs::write(&temp_path, data).map_err(failed)?;
    std::fs::rename(&temp_path, &path).map_err(failed)
}

/// Reads the stored archive back.
fn restore_local(snapshot: &Snapshot) -> SandboxResult<Vec<u8>> {
    let path = local_path(snapshot, OpName::SnapshotRestore)?;
    std::fs::read(&path).map_err(|error| {
        SandboxError::snapshot_restore(snapshot.id(), &path.to_string_lossy()).with_cause(error)
    })
}

/// Where a local snapshot's archive belongs.
fn local_path(snapshot: &Snapshot, op: OpName) -> SandboxResult<PathBuf> {
    let base_path = snapshot.local_base_path().ok_or_else(|| {
        SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            op,
            "a local snapshot must carry the directory its archive lives in",
        )
        .with_context("snapshot_id", snapshot.id())
    })?;
    let file_name = local_snapshot_file_name(snapshot.id()).ok_or_else(|| {
        SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            op,
            "local snapshot id must be a single path segment",
        )
        .with_context("snapshot_id", snapshot.id())
    })?;
    Ok(base_path.join(file_name))
}

/// The file name an id maps to, or `None` when the id is not one path segment.
///
/// Checked against POSIX **and** Windows spelling, whichever host this is running on, because the
/// id travels in a session state: one written on Windows is read on Linux, and an id that names a
/// directory there must not name a file here. `.` and `..` are refused outright, and so is anything
/// carrying a separator, a trailing separator, or a drive prefix.
fn local_snapshot_file_name(id: &str) -> Option<String> {
    if matches!(id, "" | "." | "..") {
        return None;
    }
    if id.contains('/') || id.contains('\\') {
        return None;
    }
    // A leading `X:` is a drive, which makes the rest of the id a path relative to it. Only a
    // single ASCII letter starts one; `:a` and `ab:c` are ordinary names.
    let mut characters = id.chars();
    if characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.next() == Some(':')
    {
        return None;
    }
    Some(format!("{id}{LOCAL_SNAPSHOT_SUFFIX}"))
}

/// The name the archive is written under before it replaces the stored one.
///
/// Hidden, suffixed and carrying a random component, so that two sessions persisting the same
/// snapshot at once cannot write over each other's half-finished file, and so that a leftover from
/// a killed process is not mistaken for a snapshot.
fn temp_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()))
}

/// Storage this implementation cannot reach.
fn unsupported(snapshot: &Snapshot, op: OpName) -> SandboxError {
    let snapshot_type = snapshot.snapshot_type();
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        op,
        format!("cannot use a `{snapshot_type}` snapshot: no storage is registered for it"),
    )
    .with_context("snapshot_id", snapshot.id())
    .with_context("snapshot_type", snapshot_type)
}
