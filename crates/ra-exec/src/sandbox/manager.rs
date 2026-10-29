// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Select and lower canonical permissions at the native command execution boundary.
//!
//! Managed networking and the Linux helper are separate consumers. This manager never lowers a
//! canonical profile to a backend that would discard its restrictions.
#![allow(
    missing_docs,
    reason = "Preserve upstream sandbox manager records and selection semantics."
)]

use super::policy_transforms::{effective_permission_profile, should_require_platform_sandbox};
use super::{SandboxCommand, SandboxError};
use ra_filesystem::models::{AdditionalPermissionProfile, PermissionProfile};
use ra_filesystem::sandbox::SandboxType;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Retain upstream require/forbid selection for trusted callers and parity tests."
)]
#[cfg_attr(
    not(feature = "test-api"),
    allow(
        unreachable_pub,
        reason = "Reachable outside the crate only through the hidden test bridge."
    )
)]
#[non_exhaustive]
pub enum SandboxablePreference {
    Auto,
    Require,
    Forbid,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SandboxManager {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    file_system_helper: bool,
    #[cfg(target_os = "macos")]
    allowed_symlinked_codex_home: Option<ra_paths::AbsolutePathBuf>,
}

pub(crate) struct SandboxTransformRequest<'a> {
    pub(crate) command: SandboxCommand,
    pub(crate) permissions: &'a PermissionProfile,
    /// Grants merged onto `permissions` for this command, as Codex's `SandboxCommand` carries
    /// them; callers that merged their grants already pass `None`.
    pub(crate) additional_permissions: Option<&'a AdditionalPermissionProfile>,
    pub(crate) sandbox: SandboxType,
    pub(crate) sandbox_policy_cwd: &'a Path,
}

#[allow(
    dead_code,
    reason = "Retain the transformed permission snapshot for launch observations."
)]
pub(crate) struct SandboxExecRequest {
    pub(crate) command: SandboxCommand,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) sandbox: SandboxType,
}

pub(crate) fn get_platform_sandbox(windows_sandbox_enabled: bool) -> Option<SandboxType> {
    if cfg!(target_os = "macos") {
        Some(SandboxType::MacosSeatbelt)
    } else if cfg!(target_os = "linux") {
        Some(SandboxType::LinuxSeccomp)
    } else if cfg!(windows) && windows_sandbox_enabled {
        Some(SandboxType::WindowsRestrictedToken)
    } else {
        None
    }
}

impl SandboxManager {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(target_os = "macos")]
    #[must_use]
    pub(crate) const fn for_file_system_helpers() -> Self {
        Self {
            file_system_helper: true,
            #[cfg(target_os = "macos")]
            allowed_symlinked_codex_home: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn with_allowed_symlinked_codex_home(
        mut self,
        home: Option<ra_paths::AbsolutePathBuf>,
    ) -> Self {
        self.allowed_symlinked_codex_home = home;
        self
    }

    #[must_use]
    #[allow(
        clippy::unused_self,
        reason = "Preserve the upstream manager selection method."
    )]
    pub(crate) fn should_sandbox(
        &self,
        permissions: &PermissionProfile,
        pref: SandboxablePreference,
        has_managed_network_requirements: bool,
    ) -> bool {
        match pref {
            SandboxablePreference::Forbid => false,
            SandboxablePreference::Require => true,
            SandboxablePreference::Auto => {
                let (file_system, network) = permissions.to_runtime_permissions();
                should_require_platform_sandbox(
                    &file_system,
                    network,
                    has_managed_network_requirements,
                )
            }
        }
    }

    #[must_use]
    pub(crate) fn select_initial(
        &self,
        permissions: &PermissionProfile,
        pref: SandboxablePreference,
        windows_sandbox_type: SandboxType,
        has_managed_network_requirements: bool,
    ) -> SandboxType {
        if !self.should_sandbox(permissions, pref, has_managed_network_requirements) {
            return SandboxType::None;
        }
        if cfg!(windows) && windows_sandbox_type == SandboxType::WindowsMxc {
            return SandboxType::WindowsMxc;
        }
        get_platform_sandbox(windows_sandbox_type != SandboxType::None).unwrap_or(SandboxType::None)
    }

    #[cfg_attr(not(target_os = "macos"), allow(clippy::unused_self))]
    pub(crate) fn transform(
        &self,
        request: SandboxTransformRequest<'_>,
    ) -> Result<SandboxExecRequest, SandboxError> {
        if request.sandbox != SandboxType::None && !request.sandbox_policy_cwd.is_absolute() {
            return Err(SandboxError::UnusableRoot {
                root: request.sandbox_policy_cwd.to_path_buf(),
                detail: "sandbox policy cwd must be absolute on the executing host".to_owned(),
            });
        }
        let permissions =
            effective_permission_profile(request.permissions, request.additional_permissions);
        let command = match request.sandbox {
            SandboxType::None => request.command,
            SandboxType::MacosSeatbelt => {
                #[cfg(target_os = "macos")]
                {
                    use super::seatbelt_profile::{self, MacosSeatbeltProfile};
                    let mut command =
                        vec![request.command.program().to_string_lossy().into_owned()];
                    command.extend_from_slice(request.command.args());
                    let args = seatbelt_profile::create_seatbelt_command_args_with_profile(
                        command,
                        &permissions.file_system_sandbox_policy(),
                        request.sandbox_policy_cwd,
                        permissions.network_sandbox_policy(),
                        if self.file_system_helper {
                            MacosSeatbeltProfile::FileSystemHelper
                        } else {
                            MacosSeatbeltProfile::Process
                        },
                        self.allowed_symlinked_codex_home.as_ref(),
                    )
                    .map_err(|error| SandboxError::Preparation {
                        backend: "Seatbelt",
                        detail: error.to_string(),
                    })?;
                    SandboxCommand::new(seatbelt_profile::MACOS_PATH_TO_SEATBELT_EXECUTABLE, args)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    return Err(unsupported("Seatbelt is available only on macOS"));
                }
            }
            _ => {
                return Err(unsupported(
                    "canonical command sandbox profiles require an enforcing backend; Linux helper and Windows command lowering are not ported",
                ));
            }
        };
        Ok(SandboxExecRequest {
            command,
            permission_profile: permissions,
            sandbox: request.sandbox,
        })
    }
}

fn unsupported(reason: &str) -> SandboxError {
    SandboxError::Preparation {
        backend: "permission-profile",
        detail: reason.to_owned(),
    }
}

/// Hidden bridges for the source manager tests in the external test workspace.
#[cfg(feature = "test-api")]
pub mod test_api {
    #![allow(missing_docs, reason = "Hidden bridges for source tests.")]
    pub use super::SandboxablePreference;
    use super::{
        AdditionalPermissionProfile, Path, PermissionProfile, SandboxCommand, SandboxError,
        SandboxManager, SandboxTransformRequest, SandboxType,
    };

    #[must_use]
    pub fn platform_sandbox(windows_sandbox_enabled: bool) -> Option<SandboxType> {
        super::get_platform_sandbox(windows_sandbox_enabled)
    }

    #[must_use]
    pub fn select_initial(
        permissions: &PermissionProfile,
        pref: SandboxablePreference,
        windows_sandbox_type: SandboxType,
        has_managed_network_requirements: bool,
    ) -> SandboxType {
        SandboxManager::new().select_initial(
            permissions,
            pref,
            windows_sandbox_type,
            has_managed_network_requirements,
        )
    }

    /// The transformed argv and effective permission profile of a command manager launch.
    pub fn transform(
        program: &str,
        args: Vec<String>,
        permissions: &PermissionProfile,
        additional_permissions: Option<&AdditionalPermissionProfile>,
        sandbox: SandboxType,
        sandbox_policy_cwd: &Path,
    ) -> Result<(Vec<String>, PermissionProfile), SandboxError> {
        let request = SandboxManager::new().transform(SandboxTransformRequest {
            command: SandboxCommand::new(program, args),
            permissions,
            additional_permissions,
            sandbox,
            sandbox_policy_cwd,
        })?;
        let mut transformed = vec![request.command.program().to_string_lossy().into_owned()];
        transformed.extend_from_slice(request.command.args());
        Ok((transformed, request.permission_profile))
    }
}
