//! Attaching and detaching mounts inside a live session.
//!
//! A mount is declared in `ra_core::sandbox`; this is what happens to one once a session exists. It
//! is attached while the manifest is applied, detached before a workspace is snapshotted and
//! reattached afterwards, and detached again before a workspace is deleted — deleting through a
//! live mount would delete what is on the other side of it.
//!
//! # Behaviour comes from a lifecycle, not from the declaration
//!
//! The reference hangs these methods on its strategy classes, so a host that registers a strategy
//! of its own brings its behaviour with it. Here the strategy is data — a host strategy parses as
//! [`MountStrategy::Extension`] — so the behaviour lives in a [`MountLifecycle`] instead.
//! [`BuiltinMountLifecycle`] covers the two strategies the reference ships; a host with a strategy
//! of its own implements the trait and delegates the rest to the builtin one.
//!
//! # What an in-container mount does not do yet
//!
//! Running an in-container mount means building the pattern's runtime configuration from the
//! provider's fields and then running the pattern's commands — `mount-s3`, `rclone`, `blobfuse2`,
//! `mount.s3files`. Neither half is carried over yet. Until they are, an in-container mount is
//! refused at the point it would run a command, after the credential boundary has been checked, so
//! a workspace never comes up with a mount point that is an ordinary empty directory.

use std::path::Path;

use async_trait::async_trait;
use ra_core::sandbox::{
    ErrorCode, MaterializedFile, Mount, MountPattern, MountStrategy, OpName, PosixPath,
    SandboxError, SandboxResult, SandboxSession, validate_mount_activation_credential_boundary,
};

mod transition;

pub use transition::{
    ArchiveErrorKind, EphemeralMountRemoval, restore_detached_mounts,
    with_ephemeral_mounts_removed, workspace_archive_error_summary,
};

/// What each mount strategy does to a live session.
///
/// Every method is given the strategy to run separately from the mount: a backend may attach a
/// mount with a strategy other than the declared one, and what runs is what matters. `base_dir` is
/// where the manifest's relative host sources are measured from; neither builtin strategy reads
/// it, and it is passed for one that does.
#[async_trait]
pub trait MountLifecycle: Send + Sync {
    /// Attaches the mount while a manifest is being applied.
    ///
    /// # Errors
    ///
    /// Returns the strategy's failure to attach.
    async fn activate(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        base_dir: &Path,
    ) -> SandboxResult<Vec<MaterializedFile>>;

    /// Detaches the mount before a workspace is torn down.
    ///
    /// # Errors
    ///
    /// Returns the strategy's failure to detach.
    async fn deactivate(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        base_dir: &Path,
    ) -> SandboxResult<()>;

    /// Detaches the mount so a snapshot does not record what is on the other side of it.
    ///
    /// `path` is where the mount is attached, already resolved.
    ///
    /// # Errors
    ///
    /// Returns the strategy's failure to detach.
    async fn teardown_for_snapshot(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()>;

    /// Reattaches a mount [`Self::teardown_for_snapshot`] detached.
    ///
    /// # Errors
    ///
    /// Returns the strategy's failure to reattach.
    async fn restore_after_snapshot(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()>;

    /// Attaches a mount with its declared strategy, for a manifest application pass.
    ///
    /// The credential boundary is checked here, against the declared strategy and the session's
    /// manifest, before the strategy runs anything; an in-container strategy checks it again for
    /// itself. Two checks because the second one is the strategy's own: a lifecycle that swaps in
    /// a different strategy still has that one checked.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::MountConfigInvalid`] when the boundary refuses the mount, or the
    /// strategy's failure to attach.
    async fn apply(
        &self,
        mount: &Mount,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        base_dir: &Path,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        check_activation_boundary(mount, mount.strategy(), session, dest)?;
        self.activate(mount, mount.strategy(), session, dest, base_dir)
            .await
    }

    /// Detaches a mount with its declared strategy, for manifest teardown.
    ///
    /// # Errors
    ///
    /// Returns the strategy's failure to detach.
    async fn unmount(
        &self,
        mount: &Mount,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        base_dir: &Path,
    ) -> SandboxResult<()> {
        self.deactivate(mount, mount.strategy(), session, dest, base_dir)
            .await
    }
}

/// The lifecycle of the reference's two strategies.
///
/// - **In-container** runs the pattern's commands inside the sandbox. It checks the credential
///   boundary for itself on the way in, both when attaching and when reattaching after a snapshot.
/// - **Docker volume** is attached by the container runtime before the session starts, so
///   attaching and detaching are no-ops — on a backend that attaches volumes. Anywhere else they
///   are refused rather than silently skipped, because a mount nobody attached is an empty
///   directory. Detaching for a snapshot is a no-op everywhere: the volume stays where it is and
///   the snapshot leaves its path out.
/// - A strategy a host registered has no lifecycle here and is refused.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinMountLifecycle;

#[async_trait]
impl MountLifecycle for BuiltinMountLifecycle {
    async fn activate(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        _base_dir: &Path,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        match strategy {
            MountStrategy::InContainer { pattern } => {
                check_activation_boundary(mount, strategy, session, dest)?;
                let mount_path = in_container_mount_path(mount, session, dest)?;
                apply_pattern(mount, pattern, session, &mount_path)?;
                Ok(Vec::new())
            }
            MountStrategy::DockerVolume { .. } => {
                require_volume_mounts(mount, session)?;
                Ok(Vec::new())
            }
            _ => Err(unsupported_strategy(mount, strategy)),
        }
    }

    async fn deactivate(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        _base_dir: &Path,
    ) -> SandboxResult<()> {
        match strategy {
            MountStrategy::InContainer { pattern } => {
                let mount_path = in_container_mount_path(mount, session, dest)?;
                unapply_pattern(mount, pattern, session, &mount_path)
            }
            MountStrategy::DockerVolume { .. } => require_volume_mounts(mount, session),
            _ => Err(unsupported_strategy(mount, strategy)),
        }
    }

    async fn teardown_for_snapshot(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()> {
        match strategy {
            MountStrategy::InContainer { pattern } => {
                require_in_container_support(mount)?;
                unapply_pattern(mount, pattern, session, path)
            }
            MountStrategy::DockerVolume { .. } => Ok(()),
            _ => Err(unsupported_strategy(mount, strategy)),
        }
    }

    async fn restore_after_snapshot(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()> {
        match strategy {
            MountStrategy::InContainer { pattern } => {
                let state = session.state();
                validate_mount_activation_credential_boundary(
                    mount,
                    strategy,
                    Some(state.manifest()),
                    Some(path.as_str()),
                    Some(session.backend_id()),
                )?;
                require_in_container_support(mount)?;
                apply_pattern(mount, pattern, session, path)
            }
            MountStrategy::DockerVolume { .. } => Ok(()),
            _ => Err(unsupported_strategy(mount, strategy)),
        }
    }
}

/// Checks the credential boundary for a mount about to be attached at `dest`.
fn check_activation_boundary(
    mount: &Mount,
    strategy: &MountStrategy,
    session: &dyn SandboxSession,
    dest: &PosixPath,
) -> SandboxResult<()> {
    let state = session.state();
    let manifest = state.manifest();
    let mount_path = mount.resolve_path_for_root(&PosixPath::coerce(&manifest.root), dest)?;
    validate_mount_activation_credential_boundary(
        mount,
        strategy,
        Some(manifest),
        Some(mount_path.as_str()),
        Some(session.backend_id()),
    )
}

/// Where an in-container mount attaches in this session's workspace.
fn in_container_mount_path(
    mount: &Mount,
    session: &dyn SandboxSession,
    dest: &PosixPath,
) -> SandboxResult<PosixPath> {
    require_in_container_support(mount)?;
    let state = session.state();
    mount.resolve_path_for_root(&PosixPath::coerce(&state.manifest().root), dest)
}

/// Refuses a mount type that cannot be attached from inside the sandbox.
///
/// Every provider this crate models can be; a host-registered one has no in-container adapter
/// unless its host supplies a lifecycle of its own.
fn require_in_container_support(mount: &Mount) -> SandboxResult<()> {
    if mount.provider().is_modelled() {
        return Ok(());
    }
    Err(
        SandboxError::mount_config("in-container mounts are not supported for this mount type")
            .with_context("mount_type", mount.type_name()),
    )
}

/// Refuses a Docker-volume mount on a backend that does not attach volumes.
fn require_volume_mounts(mount: &Mount, session: &dyn SandboxSession) -> SandboxResult<()> {
    if session.supports_volume_mounts() {
        return Ok(());
    }
    Err(SandboxError::mount_config(
        "docker-volume mounts are not supported by this sandbox backend",
    )
    .with_context("mount_type", mount.type_name())
    .with_context("session_type", session.backend_id()))
}

/// Runs an in-container pattern's attach commands.
///
/// Not carried over yet: see the module documentation.
fn apply_pattern(
    mount: &Mount,
    pattern: &MountPattern,
    _session: &dyn SandboxSession,
    _path: &PosixPath,
) -> SandboxResult<()> {
    Err(pattern_not_carried_over(mount, pattern))
}

/// Runs an in-container pattern's detach commands.
///
/// Not carried over yet: see the module documentation.
fn unapply_pattern(
    mount: &Mount,
    pattern: &MountPattern,
    _session: &dyn SandboxSession,
    _path: &PosixPath,
) -> SandboxResult<()> {
    Err(pattern_not_carried_over(mount, pattern))
}

/// The refusal for an in-container pattern whose commands are not carried over yet.
fn pattern_not_carried_over(mount: &Mount, pattern: &MountPattern) -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Materialize,
        format!(
            "cannot run a `{}` mount with the `{}` pattern: in-container mount patterns are not \
             implemented yet",
            mount.type_name(),
            pattern.as_str()
        ),
    )
    .with_context("mount_type", mount.type_name())
    .with_context("pattern", pattern.as_str())
}

/// The refusal for a strategy this crate has no lifecycle for.
fn unsupported_strategy(mount: &Mount, strategy: &MountStrategy) -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Materialize,
        format!(
            "cannot run a `{}` mount with the `{}` strategy: this strategy is registered but has no \
             lifecycle here",
            mount.type_name(),
            strategy.type_name()
        ),
    )
    .with_context("mount_type", mount.type_name())
    .with_context("strategy_type", strategy.type_name())
}
