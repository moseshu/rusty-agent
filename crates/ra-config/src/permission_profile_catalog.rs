// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Permission catalog availability and managed runtime validation.
#![allow(
    missing_docs,
    reason = "Preserve the source permission catalog records."
)]
use crate::ConfigLayerStack;
use crate::ConfigPathContext;
use crate::PermissionsToml;
use crate::RequirementSource;
use crate::SandboxModeRequirement;
use crate::Sourced;
use crate::sandbox_mode_requirement_for_permission_profile;
use ra_filesystem::models::PermissionProfile;

use super::ConstraintError;
use super::ConstraintResult;
use crate::permission_profiles::BUILT_IN_DANGER_FULL_ACCESS_PROFILE;
use crate::permission_profiles::BUILT_IN_READ_ONLY_PROFILE;
use crate::permission_profiles::BUILT_IN_WORKSPACE_PROFILE;
use crate::permission_profiles::compile_permission_profile;
use crate::permission_profiles::validate_user_permission_profile_names;
use crate::permission_selection::is_permission_allowed;
use crate::permission_selection::merge_managed_permission_profiles;
use crate::permission_selection::validate_required_permission_profile_catalog;

/// A permission profile exposed to clients together with its effective availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionProfileCatalogEntry {
    pub id: String,
    pub description: Option<String>,
    pub allowed: bool,
}

/// Builds the effective catalog using supplied execution-host path facts.
pub fn permission_profile_catalog(
    config_layer_stack: &ConfigLayerStack,
    context: &ConfigPathContext,
) -> std::io::Result<Vec<PermissionProfileCatalogEntry>> {
    let permissions = config_layer_stack
        .effective_config()
        .get("permissions")
        .cloned()
        .map(toml::Value::try_into::<PermissionsToml>)
        .transpose()
        .map_err(std::io::Error::other)?;
    let requirements_toml = config_layer_stack.requirements_toml();
    let permissions = merge_managed_permission_profiles(permissions.as_ref(), requirements_toml)?;

    permission_profile_catalog_from_permissions(config_layer_stack, permissions.as_ref(), context)
}

pub fn permission_profile_catalog_from_permissions(
    config_layer_stack: &ConfigLayerStack,
    permissions: Option<&PermissionsToml>,
    context: &ConfigPathContext,
) -> std::io::Result<Vec<PermissionProfileCatalogEntry>> {
    let requirements_toml = config_layer_stack.requirements_toml();
    validate_user_permission_profile_names(permissions)?;
    validate_required_permission_profile_catalog(requirements_toml, permissions)?;

    let entries = [
        BUILT_IN_READ_ONLY_PROFILE,
        BUILT_IN_WORKSPACE_PROFILE,
        BUILT_IN_DANGER_FULL_ACCESS_PROFILE,
    ]
    .into_iter()
    .map(|id| (id, None))
    .chain(
        permissions
            .into_iter()
            .flat_map(|permissions| &permissions.entries)
            .map(|(id, profile)| (id.as_str(), profile.description.clone())),
    );
    let catalog = entries
        .map(|(id, description)| {
            let allowed = compile_permission_profile(
                permissions,
                id,
                context,
                /*workspace_write*/ None,
                &mut Vec::new(),
            )
            .is_ok_and(|compiled| {
                permission_profile_is_allowed(config_layer_stack, id, &compiled.permission_profile)
            });
            PermissionProfileCatalogEntry {
                id: id.to_string(),
                description,
                allowed,
            }
        })
        .collect();

    Ok(catalog)
}

#[must_use]
pub fn permission_profile_is_allowed(
    config_layer_stack: &ConfigLayerStack,
    profile_id: &str,
    permission_profile: &PermissionProfile,
) -> bool {
    let allowed_by_id = config_layer_stack
        .requirements_toml()
        .allowed_permission_profiles
        .as_ref()
        .is_none_or(|allowed| is_permission_allowed(allowed, profile_id));
    allowed_by_id
        && permission_profile_satisfies_requirements(config_layer_stack, permission_profile)
}

#[must_use]
pub fn permission_profile_satisfies_requirements(
    config_layer_stack: &ConfigLayerStack,
    permission_profile: &PermissionProfile,
) -> bool {
    let allowed_by_sandbox_mode = config_layer_stack
        .requirements()
        .permission_profile
        .can_set(permission_profile)
        .is_ok();
    let allowed_by_filesystem = config_layer_stack
        .requirements()
        .filesystem
        .as_ref()
        .is_none_or(|Sourced { value, source }| {
            value.deny_read.is_empty()
                || validate_permission_profile_for_deny_read(permission_profile, source).is_ok()
        });
    allowed_by_sandbox_mode && allowed_by_filesystem
}

/// Validates that a permission candidate can enforce managed filesystem denials.
/// Add this validator before applying profile requirements so the native
/// constrained fallback can replace full-access or external candidates.
pub fn validate_permission_profile_for_deny_read(
    permission_profile: &PermissionProfile,
    requirement_source: &RequirementSource,
) -> ConstraintResult<()> {
    let mode = sandbox_mode_requirement_for_permission_profile(permission_profile);
    match mode {
        SandboxModeRequirement::ReadOnly | SandboxModeRequirement::WorkspaceWrite => Ok(()),
        SandboxModeRequirement::DangerFullAccess | SandboxModeRequirement::ExternalSandbox => {
            Err(ConstraintError::InvalidValue {
                field_name: "sandbox_mode",
                candidate: format!("{mode:?}"),
                allowed: "[read-only, workspace-write]".to_string(),
                requirement_source: requirement_source.clone(),
            })
        }
    }
}
