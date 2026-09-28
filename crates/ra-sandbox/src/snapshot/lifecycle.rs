//! What a session does with its snapshot when it stops, and when it comes back.
//!
//! Stopping writes the workspace into storage. Starting decides between three answers: restore what
//! was stored, keep what is already there because it demonstrably matches, or build the workspace
//! from the manifest. This is the part of that decision that belongs to the snapshot.
//!
//! # The fingerprint is what makes "already there" checkable
//!
//! A backend that kept its workspace across a stop — a container that is still running — would lose
//! work if a resume extracted the stored archive over it: everything written since the snapshot was
//! taken would go. But skipping the restore is only safe if the workspace really is what was
//! stored, and the only way to know is to hash it. So persisting records what the workspace hashed
//! to, and a resume hashes it again and compares. A fingerprint that cannot be computed is not a
//! mismatch and not a match — it means the question was not answered, and the answer then falls
//! back to restoring, which is the branch that cannot silently lose the stored workspace.
//!
//! The manifest digest is mixed into the hash, so a workspace that is byte-identical but was
//! declared differently does not count as a match: what is on disk is only half of what a resumed
//! session is supposed to have.
//!
//! # Not reached on the local backend
//!
//! The skip branch needs a backend that reports workspace content surviving a stop, and the local
//! one never does — its `workspace_state_preserved_on_start` is the protocol's `false`, exactly as
//! the reference's local backend leaves it. So here a restorable snapshot is always restored, and
//! the comparison this module implements is for the container backend that will report otherwise.
//! It is implemented and tested now because persisting already records the fingerprint, and a
//! fingerprint nothing ever checks is a value drifting out of meaning.

use std::collections::BTreeSet;

use futures::future::BoxFuture;
use ra_core::sandbox::{
    ErrorCode, ExecRequest, Manifest, OpName, PosixPath, SandboxError, SandboxResult,
    SandboxSession, SandboxSessionState, SessionPath, ShellInvocation, SnapshotFingerprint,
};
use sha2::{Digest, Sha256};

use crate::runtime_helpers::{ensure_installed, workspace_fingerprint_helper};

use super::SnapshotStore;

/// Which scheme the fingerprints recorded here were produced by.
///
/// Read before a stored fingerprint is compared, and a stored one that names a different scheme is
/// not compared at all. That is what lets this change without a resume ever comparing two hashes
/// that were never meant to be equal.
pub const SNAPSHOT_FINGERPRINT_VERSION: &str = "workspace_tar_sha256_v1";

/// Where inside the sandbox a session caches what its workspace hashed to.
const FINGERPRINT_CACHE_ROOT: &str = "/tmp/rusty-agent/session-state";

/// The snapshot half of a session's lifecycle.
///
/// Holds the session it acts on and the storage its snapshot names, which is the pair every
/// operation here needs. The reference reaches both through the session, because there the snapshot
/// object carries its own storage; here the storage is supplied, so a host can hand in its own.
pub struct SnapshotLifecycle<'a> {
    session: &'a dyn SandboxSession,
    store: &'a dyn SnapshotStore,
}

impl<'a> SnapshotLifecycle<'a> {
    /// Acts on `session`, reading and writing its snapshot through `store`.
    #[must_use]
    pub const fn new(session: &'a dyn SandboxSession, store: &'a dyn SnapshotStore) -> Self {
        Self { session, store }
    }

    /// Whether this session's snapshot has something to restore.
    ///
    /// # Errors
    ///
    /// Returns the storage's failure to answer.
    pub async fn restorable(&self) -> SandboxResult<bool> {
        self.store
            .restorable(
                self.session.state().snapshot(),
                &self.session.dependencies(),
            )
            .await
    }

    /// Writes the workspace into the session's storage.
    ///
    /// The fingerprint is computed **before** the archive is made, so it describes the same
    /// workspace the archive does rather than one that was still being written. A fingerprint that
    /// could not be computed is not an error: the workspace is still worth storing, it just cannot
    /// be skipped on the way back. What is an error is recording one that does not match what was
    /// stored, so a persist that fails removes the cached value it had just written.
    ///
    /// # Errors
    ///
    /// Returns the failure to read the workspace out or to write it into storage.
    pub async fn persist(&self) -> SandboxResult<()> {
        let state = self.session.state();
        // Nothing to store, and nothing to fingerprint for: the workspace is not coming back.
        if state.snapshot().is_noop() {
            return Ok(());
        }

        let recorded = match self.compute_and_cache_fingerprint().await {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) => {
                // Swallowed, as the reference swallows it: the workspace is still worth storing.
                // Said out loud all the same, because the consequence — every later resume restores
                // instead of reusing a workspace that may not have changed — is otherwise a silence
                // nobody can trace back to here.
                tracing::debug!(
                    backend = self.session.backend_id(),
                    error = %error,
                    "storing a workspace whose fingerprint could not be computed"
                );
                None
            }
        };
        let archive = self.session.persist_workspace().await?;
        if let Err(error) = self
            .store
            .persist(state.snapshot(), archive, &self.session.dependencies())
            .await
        {
            // The cached value describes a workspace that was never stored. Leaving it behind would
            // let the next start compare against it and skip a restore that has to happen.
            if recorded.is_some() {
                self.delete_cached_fingerprint().await;
            }
            return Err(error);
        }

        self.session.record_snapshot_fingerprint(recorded).await
    }

    /// Replaces the workspace with what storage holds.
    ///
    /// The workspace is emptied first. Extracting over what is there would leave behind every file
    /// the stored archive does not mention, and a resumed session would then see a mixture of two
    /// workspaces rather than the one it asked for. Paths an ephemeral mount owns are left alone:
    /// they belong to whatever is mounted there, not to this workspace.
    ///
    /// # Errors
    ///
    /// Returns the failure to clear the workspace, to read the archive, or to extract it.
    pub async fn restore_on_resume(&self) -> SandboxResult<()> {
        self.clear_workspace_root().await?;
        let archive = self
            .store
            .restore(
                self.session.state().snapshot(),
                &self.session.dependencies(),
            )
            .await?;
        self.session.hydrate_workspace(archive).await
    }

    /// Whether a resume can keep the live workspace instead of restoring over it.
    ///
    /// Only when the session is running: a workspace belonging to a backend that is not up cannot
    /// be the one the fingerprint describes, whatever it hashes to.
    ///
    /// # Errors
    ///
    /// Returns the failure to read the session's state.
    pub async fn can_skip_restore(&self, is_running: bool) -> SandboxResult<bool> {
        if !is_running {
            return Ok(false);
        }
        self.live_workspace_matches().await
    }

    /// Whether the workspace on disk still hashes to what the snapshot was taken at.
    async fn live_workspace_matches(&self) -> SandboxResult<bool> {
        let state = self.session.state();
        let Some((stored_fingerprint, stored_version)) = state.snapshot_fingerprint() else {
            return Ok(false);
        };

        // A fingerprint that cannot be computed answers nothing, and "nothing" has to read as "do
        // not skip": the alternative is keeping a workspace nobody could confirm.
        let Ok(computed) = self.compute_and_cache_fingerprint().await else {
            return Ok(false);
        };
        Ok(computed.fingerprint() == stored_fingerprint && computed.version() == stored_version)
    }

    /// Hashes the workspace from inside the sandbox, and leaves the answer cached there.
    ///
    /// # Errors
    ///
    /// Returns the failure to install the helper, to run it, or to read what it printed.
    pub async fn compute_and_cache_fingerprint(&self) -> SandboxResult<SnapshotFingerprint> {
        let helper = workspace_fingerprint_helper();
        ensure_installed(self.session, &helper).await?;

        let state = self.session.state();
        let mut command = vec![
            helper.install_path().to_owned(),
            state.manifest().root.clone(),
            SNAPSHOT_FINGERPRINT_VERSION.to_owned(),
            Self::fingerprint_cache_path(&state),
            resume_manifest_digest(state.manifest())?,
        ];
        command.extend(
            fingerprint_skip_relpaths(self.session)?
                .iter()
                .map(|path| path.as_str().to_owned()),
        );

        let result = self
            .session
            .exec(ExecRequest::new(command.clone()).with_shell(ShellInvocation::None))
            .await?;
        if !result.ok() {
            // Reported under what was being asked rather than under the helper's installed path,
            // which is scratch detail the caller has no use for.
            let mut reported = vec!["compute_workspace_fingerprint".to_owned()];
            reported.extend(command.into_iter().skip(1));
            return Err(SandboxError::exec_nonzero(result, reported));
        }
        parse_fingerprint_record(&result.stdout)
    }

    /// Removes the cached fingerprint, and says nothing if it cannot.
    ///
    /// Best effort by design: this runs on the failure path of a persist, and a cleanup failure
    /// must not replace the failure the caller needs to see. A cached value left behind is read
    /// again only if it also matches a fingerprint recorded in the state, and this path is reached
    /// precisely when no such value was recorded.
    async fn delete_cached_fingerprint(&self) {
        let state = self.session.state();
        let command = vec![
            "rm".to_owned(),
            "-f".to_owned(),
            "--".to_owned(),
            Self::fingerprint_cache_path(&state),
        ];
        let _ = self
            .session
            .exec(ExecRequest::new(command).with_shell(ShellInvocation::None))
            .await;
    }

    /// Where this session's cached fingerprint lives inside the sandbox.
    fn fingerprint_cache_path(state: &SandboxSessionState) -> String {
        format!(
            "{FINGERPRINT_CACHE_ROOT}/{}/fingerprint.json",
            state.session_id().simple()
        )
    }

    /// Empties the workspace, keeping whatever an ephemeral mount owns.
    async fn clear_workspace_root(&self) -> SandboxResult<()> {
        let state = self.session.state();
        let root = PosixPath::new(state.manifest().root.clone());
        let skip = mount_skip_relpaths(state.manifest(), &root)?;

        // A mount at the workspace root itself leaves nothing to clear: everything under it belongs
        // to the mount, and removing it would reach through to the other side.
        if skip
            .iter()
            .any(|path| path.as_str().is_empty() || path.as_str() == ".")
        {
            return Ok(());
        }

        self.clear_dir_pruned(root.clone(), root, skip).await
    }

    /// Removes everything in one directory except what a mount owns, recursing where it must.
    fn clear_dir_pruned(
        &self,
        directory: PosixPath,
        root: PosixPath,
        skip: BTreeSet<PosixPath>,
    ) -> BoxFuture<'_, SandboxResult<()>> {
        Box::pin(async move {
            let entries = match self.session.ls(SessionPath::Posix(&directory), None).await {
                Ok(entries) => entries,
                // A directory that is not there, or cannot be listed, is treated as empty: the
                // restore that follows creates what it needs, and refusing here would fail a resume
                // over a workspace that simply has not been made yet.
                Err(error) if is_missing_or_unlistable(&error) => return Ok(()),
                Err(error) => return Err(error),
            };

            for entry in entries {
                let child = PosixPath::new(entry.path.clone());
                // Something the listing reported from outside the workspace has no relative path to
                // reason about, and is removed rather than kept.
                let Some(relative) = child.relative_to(&root) else {
                    self.session
                        .remove_workspace_entry_on_resume(SessionPath::Posix(&child))
                        .await?;
                    continue;
                };

                if skip.contains(&relative) {
                    continue;
                }

                if skip
                    .iter()
                    .any(|path| path != &relative && path.is_under(&relative))
                {
                    // On the way to a mount: clear around it rather than through it.
                    if entry.is_dir() {
                        self.clear_dir_pruned(child, root.clone(), skip.clone())
                            .await?;
                    } else {
                        self.session
                            .remove_workspace_entry_on_resume(SessionPath::Posix(&child))
                            .await?;
                    }
                    continue;
                }

                self.session
                    .remove_workspace_entry_on_resume(SessionPath::Posix(&child))
                    .await?;
            }
            Ok(())
        })
    }
}

/// Reads the record the fingerprint helper printed.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] when the output is not the object this expects. A
/// half-read record is refused rather than defaulted, because every field of it is part of deciding
/// whether a restore can be skipped.
pub fn parse_fingerprint_record(payload: &[u8]) -> SandboxResult<SnapshotFingerprint> {
    let invalid = |reason: &str| {
        SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::SnapshotPersist,
            format!("workspace fingerprint record is unusable: {reason}"),
        )
    };

    let text = std::str::from_utf8(payload).map_err(|_| invalid("it is not UTF-8"))?;
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|error| invalid(&error.to_string()))?;
    let field = |key: &str| match value.get(key) {
        Some(serde_json::Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(invalid(&format!("`{key}` is missing or empty"))),
    };
    Ok(SnapshotFingerprint::new(
        field("fingerprint")?,
        field("version")?,
    ))
}

/// A digest of what the manifest declared, mixed into every fingerprint.
///
/// Two workspaces with identical bytes are still different if they were declared differently — one
/// of them is missing an entry the other's manifest asks for — so the declaration is part of what a
/// fingerprint answers about.
///
/// **This is not comparable with the reference's digest for the same manifest.** Both hash the
/// manifest's canonical JSON with sorted keys and no spaces, but the two renderings need not agree
/// field for field. That only matters to a state carried between the two implementations, and it
/// fails in the safe direction: a digest that does not match produces a fingerprint that does not
/// match, and the resume restores.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] when the manifest cannot be rendered.
pub fn resume_manifest_digest(manifest: &Manifest) -> SandboxResult<String> {
    let rendered = serde_json::to_string(manifest).map_err(|error| {
        SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::SnapshotPersist,
            format!("failed to render the manifest a fingerprint is measured against: {error}"),
        )
        .with_cause(error)
    })?;
    Ok(format!("{:x}", Sha256::digest(rendered.as_bytes())))
}

/// The workspace-relative paths a fingerprint of `session` leaves out.
///
/// Everything a snapshot of the session leaves out — what the manifest declared as not worth
/// persisting and what the session registered at runtime — plus everything an ephemeral mount owns.
/// All of it is content this workspace does not keep, so hashing it would make the fingerprint
/// answer a question about somebody else's storage.
///
/// # Errors
///
/// Returns the manifest's failure to resolve its own declared paths.
pub fn fingerprint_skip_relpaths(
    session: &dyn SandboxSession,
) -> SandboxResult<BTreeSet<PosixPath>> {
    let state = session.state();
    let manifest = state.manifest();
    let mut skip = session.persist_workspace_skip_relpaths()?;
    skip.extend(mount_skip_relpaths(
        manifest,
        &PosixPath::new(manifest.root.clone()),
    )?);
    Ok(skip)
}

/// The workspace-relative paths ephemeral mounts occupy.
///
/// A mount whose target is outside the workspace is not one of them: there is nothing inside to
/// skip, and measuring it from a root it does not sit under would give a path that means something
/// else.
fn mount_skip_relpaths(
    manifest: &Manifest,
    root: &PosixPath,
) -> SandboxResult<BTreeSet<PosixPath>> {
    Ok(manifest
        .ephemeral_mount_targets()?
        .into_iter()
        .filter_map(|(_mount, target)| target.relative_to(root))
        .collect())
}

/// Whether a listing failure means "there is nothing here" rather than "this went wrong".
fn is_missing_or_unlistable(error: &SandboxError) -> bool {
    matches!(
        error.error_code(),
        ErrorCode::WorkspaceReadNotFound
            | ErrorCode::ExecNonzero
            | ErrorCode::WorkspaceRootNotFound
    )
}
