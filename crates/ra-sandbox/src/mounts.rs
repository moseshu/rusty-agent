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
//! # An in-container mount, end to end
//!
//! The provider's fields are turned into what its pattern runs with ([`config`]), and the pattern
//! runs its tool inside the sandbox ([`patterns`]). A Docker-volume mount is attached by the
//! container runtime instead; [`docker_volume_driver_config`] is what that runtime is handed.
//!
//! # What a failure may say
//!
//! A mount failure can carry what the mount was configured with: a command line, a tool's output, a
//! credential file's path. Every lifecycle method here, and each pattern, lets a failure out as it
//! is only when nothing about the call carries mount authority. Otherwise — the mount carries
//! credentials, the session's manifest does, or the failure was marked redacted where it was raised
//! — it is replaced as [`ra_core::sandbox::replace_protected_mount_error`] describes: the code, the
//! operation and the retryability survive, the context and the cause do not.

use std::path::Path;

use async_trait::async_trait;
use ra_core::sandbox::{
    ErrorCode, MaterializedFile, Mount, MountStrategy, OpName, PosixPath, SandboxError,
    SandboxResult, SandboxSession, manifest_has_configured_mount_authority,
    mount_has_configured_authority, replace_protected_mount_error,
    validate_mount_activation_credential_boundary,
};

pub mod config;
pub mod patterns;
mod transition;

pub use config::{
    DockerVolumeDriverConfig, MountPatternConfig, build_in_container_mount_config,
    docker_volume_driver_config,
};
pub use patterns::{apply_pattern, unapply_pattern};
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
        let result = match check_activation_boundary(mount, mount.strategy(), session, dest) {
            Ok(()) => {
                self.activate(mount, mount.strategy(), session, dest, base_dir)
                    .await
            }
            Err(error) => Err(error),
        };
        protect(result, Some(mount), session)
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
        let result = self
            .deactivate(mount, mount.strategy(), session, dest, base_dir)
            .await;
        protect(result, Some(mount), session)
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
        let result = match strategy {
            MountStrategy::InContainer { pattern } => {
                async {
                    check_activation_boundary(mount, strategy, session, dest)?;
                    let mount_path = in_container_mount_path(mount, session, dest)?;
                    let config =
                        build_in_container_mount_config(mount, pattern, session, true).await?;
                    apply_pattern(pattern, session, &mount_path, &config).await?;
                    Ok(Vec::new())
                }
                .await
            }
            MountStrategy::DockerVolume { .. } => {
                require_volume_mounts(mount, session).map(|()| Vec::new())
            }
            _ => Err(unsupported_strategy(mount, strategy)),
        };
        protect(result, Some(mount), session)
    }

    async fn deactivate(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        dest: &PosixPath,
        _base_dir: &Path,
    ) -> SandboxResult<()> {
        let result = match strategy {
            MountStrategy::InContainer { pattern } => {
                async {
                    let mount_path = in_container_mount_path(mount, session, dest)?;
                    let config =
                        build_in_container_mount_config(mount, pattern, session, false).await?;
                    unapply_pattern(pattern, session, &mount_path, &config).await
                }
                .await
            }
            MountStrategy::DockerVolume { .. } => require_volume_mounts(mount, session),
            _ => Err(unsupported_strategy(mount, strategy)),
        };
        protect(result, Some(mount), session)
    }

    async fn teardown_for_snapshot(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()> {
        let result = match strategy {
            MountStrategy::InContainer { pattern } => {
                async {
                    require_in_container_support(mount)?;
                    let config =
                        build_in_container_mount_config(mount, pattern, session, false).await?;
                    unapply_pattern(pattern, session, path, &config).await
                }
                .await
            }
            MountStrategy::DockerVolume { .. } => Ok(()),
            _ => Err(unsupported_strategy(mount, strategy)),
        };
        protect(result, Some(mount), session)
    }

    async fn restore_after_snapshot(
        &self,
        mount: &Mount,
        strategy: &MountStrategy,
        session: &dyn SandboxSession,
        path: &PosixPath,
    ) -> SandboxResult<()> {
        let result = match strategy {
            MountStrategy::InContainer { pattern } => {
                async {
                    let state = session.state();
                    validate_mount_activation_credential_boundary(
                        mount,
                        strategy,
                        Some(state.manifest()),
                        Some(path.as_str()),
                        Some(session.backend_id()),
                    )?;
                    require_in_container_support(mount)?;
                    let config =
                        build_in_container_mount_config(mount, pattern, session, true).await?;
                    apply_pattern(pattern, session, path, &config).await
                }
                .await
            }
            MountStrategy::DockerVolume { .. } => Ok(()),
            _ => Err(unsupported_strategy(mount, strategy)),
        };
        protect(result, Some(mount), session)
    }
}

/// Lets a failure out of a mount boundary, replacing it when the call carried mount authority.
///
/// Authority is carried when `mount` has or may hide some, when the session's manifest does, or when
/// the failure was marked redacted where it was raised.
pub(crate) fn protect<T>(
    result: SandboxResult<T>,
    mount: Option<&Mount>,
    session: &dyn SandboxSession,
) -> SandboxResult<T> {
    result.map_err(|error| {
        let authority = error.is_data_redacted()
            || mount.is_some_and(mount_has_configured_authority)
            || manifest_has_configured_mount_authority(session.state().manifest());
        if authority {
            replace_protected_mount_error(&error)
        } else {
            error
        }
    })
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
