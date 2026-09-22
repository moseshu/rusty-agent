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
//! # Whole archives, not streams
//!
//! The reference passes file objects, and the local kind copies them a chunk at a time. Here a
//! workspace archive is a `Vec<u8>`, which is the shape `SandboxSession::persist_workspace` and
//! `hydrate_workspace` already have, so a snapshot is bounded by memory in the same way the rest of
//! the archive path is. It changes with them.

pub mod defaults;
pub mod lifecycle;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ra_core::sandbox::{
    ErrorCode, LOCAL_SNAPSHOT_TYPE, NOOP_SNAPSHOT_TYPE, OpName, REMOTE_SNAPSHOT_TYPE, SandboxError,
    SandboxResult, Snapshot,
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
    async fn persist(&self, snapshot: &Snapshot, data: Vec<u8>) -> SandboxResult<()>;

    /// Reads back the workspace archive the storage holds.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SnapshotRestoreError`] when the archive could not be read, or
    /// [`ErrorCode::SnapshotNotRestorable`] when the storage holds nothing by design.
    async fn restore(&self, snapshot: &Snapshot) -> SandboxResult<Vec<u8>>;

    /// Whether there is something to restore.
    ///
    /// Asked before a start decides between restoring and materializing the manifest, so a `false`
    /// here is an ordinary answer rather than a failure.
    ///
    /// # Errors
    ///
    /// Returns the storage's failure to answer.
    async fn restorable(&self, snapshot: &Snapshot) -> SandboxResult<bool>;
}

/// The storage the reference builds in: a tar file on this machine, and nowhere at all.
///
/// A snapshot type this does not know is refused rather than ignored. Quietly treating unknown
/// storage as empty would turn a resume into a fresh workspace and report success.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinSnapshotStore;

#[async_trait]
impl SnapshotStore for BuiltinSnapshotStore {
    async fn persist(&self, snapshot: &Snapshot, data: Vec<u8>) -> SandboxResult<()> {
        match snapshot.snapshot_type() {
            NOOP_SNAPSHOT_TYPE => Ok(()),
            LOCAL_SNAPSHOT_TYPE => persist_local(snapshot, data),
            _ => Err(unsupported(snapshot, OpName::SnapshotPersist)),
        }
    }

    async fn restore(&self, snapshot: &Snapshot) -> SandboxResult<Vec<u8>> {
        match snapshot.snapshot_type() {
            NOOP_SNAPSHOT_TYPE => Err(SandboxError::snapshot_not_restorable(
                snapshot.id(),
                NOOP_SNAPSHOT_PATH,
            )),
            LOCAL_SNAPSHOT_TYPE => restore_local(snapshot),
            _ => Err(unsupported(snapshot, OpName::SnapshotRestore)),
        }
    }

    async fn restorable(&self, snapshot: &Snapshot) -> SandboxResult<bool> {
        match snapshot.snapshot_type() {
            NOOP_SNAPSHOT_TYPE => Ok(false),
            LOCAL_SNAPSHOT_TYPE => Ok(local_path(snapshot, OpName::SnapshotRestore)?.is_file()),
            _ => Err(unsupported(snapshot, OpName::SnapshotRestore)),
        }
    }
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
    let detail = if snapshot_type == REMOTE_SNAPSHOT_TYPE {
        "its storage client is resolved from the session's dependencies, which are not carried \
         over yet"
    } else {
        "no storage is registered for it"
    };
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        op,
        format!("cannot use a `{snapshot_type}` snapshot: {detail}"),
    )
    .with_context("snapshot_id", snapshot.id())
    .with_context("snapshot_type", snapshot_type)
}
