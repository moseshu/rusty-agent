//! # `ra-coding`
//!
//! The coding agent (product layer): prompt content, editing discipline, host assembly, dangerous
//! -action facts, and profile configuration.
//!
//! **It owns no tool.** Every entry it advertises, `apply_patch` included, comes from [`ra_tools`];
//! what this crate contributes is which of them are installed, what capability each is handed, and
//! what the prompt says about using them.
//!
//! **Boundary**: this is a consumer of the framework and assembles itself purely through the
//! public APIs of the other crates. It defines no reusable framework contract; downstream only
//! consumes product assembly entries such as `CodingHost`, `CodingProfile`, and `build_agent`.
//!
//! **Stability**: `Internal`. **It is a reference product, not a framework contract** — prompts,
//! tool set, and discipline criteria all change with the business, and no crate may depend on it
//! (enforced by the layering gate).

pub(crate) mod additional_permissions;
pub mod agent;
pub(crate) mod approvals;
pub mod assembly;
pub mod background_terminals;
pub(crate) mod capabilities;
pub(crate) mod closeout;
pub mod command_canonicalization;
pub mod dangerous_action;
pub mod doctor;
pub mod exec_policy;
pub(crate) mod final_answer;
pub mod host;
pub mod mcp;
pub mod permissions;
pub mod profile;
pub mod prompt;
pub(crate) mod request_permissions;
pub mod shell_command;
pub mod thread_factory;
pub mod thread_items;

pub use agent::{
    HOST_BACKED_PROFILE, HostBackedSurface, InstalledCapability, build_agent,
    build_agent_with_host, build_agent_with_profile, host_backed_surface,
};
pub use assembly::render_capability_assembly;
pub use host::CodingHost;
pub use profile::CodingProfile;

pub mod thread_settings;

/// Controlled executor-path context for the external integration-test workspace.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod request_permissions_test_api {
    use crate::permissions::FileSystemSandboxPolicyContext;
    use serde_json::Value;

    /// Resolves a permission path with an explicitly supplied executor home, including no home.
    pub fn resolve_path(
        value: &mut Value,
        context: &FileSystemSandboxPolicyContext<'_>,
    ) -> Result<(), String> {
        crate::request_permissions::resolve_permission_path_string(value, context)
    }
}

/// Codex's permission helpers for the external integration-test workspace, which ports their unit
/// tests.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod additional_permissions_test_api {
    use crate::{
        additional_permissions::EffectiveAdditionalPermissions,
        exec_policy::SandboxPermissions,
        permissions::{AdditionalPermissionProfile, FileSystemSandboxPolicyContext},
    };
    use ra_protocol::server::protocol::AskForApproval;

    /// Codex's `preapproved_permission_profile`.
    #[must_use]
    pub fn preapproved_permission_profile(
        effective_permissions: &AdditionalPermissionProfile,
        granted_permissions: &AdditionalPermissionProfile,
        context: &FileSystemSandboxPolicyContext<'_>,
    ) -> Option<AdditionalPermissionProfile> {
        crate::additional_permissions::preapproved_permission_profile(
            effective_permissions,
            granted_permissions,
            context,
        )
    }

    /// Codex's `normalize_and_validate_additional_permissions`.
    ///
    /// # Errors
    ///
    /// Returns Codex's refusal.
    pub fn normalize_and_validate_additional_permissions(
        additional_permissions_allowed: bool,
        approval_policy: AskForApproval,
        sandbox_permissions: SandboxPermissions,
        additional_permissions: Option<AdditionalPermissionProfile>,
        permissions_preapproved: bool,
        context: &FileSystemSandboxPolicyContext<'_>,
    ) -> Result<Option<AdditionalPermissionProfile>, String> {
        crate::additional_permissions::normalize_and_validate_additional_permissions(
            additional_permissions_allowed,
            approval_policy,
            sandbox_permissions,
            additional_permissions,
            permissions_preapproved,
            context,
        )
    }

    /// Codex's `implicit_granted_permissions`, given the effective permissions' parts.
    #[must_use]
    pub fn implicit_granted_permissions(
        sandbox_permissions: SandboxPermissions,
        additional_permissions: Option<&AdditionalPermissionProfile>,
        effective_sandbox_permissions: SandboxPermissions,
        effective_additional_permissions: Option<AdditionalPermissionProfile>,
        permissions_preapproved: bool,
    ) -> Option<AdditionalPermissionProfile> {
        crate::additional_permissions::implicit_granted_permissions(
            sandbox_permissions,
            additional_permissions,
            &EffectiveAdditionalPermissions {
                sandbox_permissions: effective_sandbox_permissions,
                additional_permissions: effective_additional_permissions,
                permissions_preapproved,
            },
        )
    }
}
