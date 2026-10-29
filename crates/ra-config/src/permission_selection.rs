// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Managed permission catalog composition and selection.
#![allow(
    missing_docs,
    reason = "Preserve upstream managed permission selection functions."
)]
use crate::permission_profiles::{
    BUILT_IN_READ_ONLY_PROFILE, BUILT_IN_WORKSPACE_PROFILE, is_builtin_permission_profile_name,
};
use crate::permission_profiles::{
    compile_permission_profile, validate_user_permission_profile_names,
};
use crate::{ConfigLayerSource, ConfigLayerStack, ConfigPathContext, ConfigToml, SandboxMode};
use crate::{ConfigRequirementsToml, PermissionsToml};
use serde::Deserialize;
use std::{collections::BTreeMap, io::ErrorKind};

pub fn merge_managed_permission_profiles(
    configured_permissions: Option<&PermissionsToml>,
    requirements_toml: &ConfigRequirementsToml,
) -> std::io::Result<Option<PermissionsToml>> {
    let managed_profiles = requirements_toml
        .permissions
        .as_ref()
        .map(|permissions| &permissions.profiles)
        .filter(|profiles| !profiles.is_empty());
    let Some(managed_profiles) = managed_profiles else {
        return Ok(configured_permissions.cloned());
    };

    let mut merged_permissions = configured_permissions.cloned().unwrap_or_default();
    for (profile_id, managed_profile) in managed_profiles {
        if merged_permissions.entries.contains_key(profile_id) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "requirements.toml permissions profile `{profile_id}` conflicts with a config-defined profile of the same name"
                ),
            ));
        }
        merged_permissions
            .entries
            .insert(profile_id.clone(), managed_profile.clone());
    }

    Ok(Some(merged_permissions))
}

pub fn resolve_default_permissions<'a>(
    default_permissions_override: Option<&'a str>,
    configured_default_permissions: Option<&'a str>,
    requirements_toml: &'a ConfigRequirementsToml,
    startup_warnings: &mut Vec<String>,
) -> std::io::Result<Option<&'a str>> {
    let selected_permissions = default_permissions_override.or(configured_default_permissions);
    let Some(allowed_permission_profiles) = requirements_toml.allowed_permission_profiles.as_ref()
    else {
        return Ok(selected_permissions);
    };
    let Some(fallback_permissions) = requirements_toml
        .default_permissions
        .as_deref()
        .or_else(|| implicit_default_permissions(allowed_permission_profiles))
    else {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "requirements.toml default_permissions must be set unless allowed_permission_profiles allows both `:workspace` and `:read-only`",
        ));
    };

    match selected_permissions {
        None => Ok(Some(fallback_permissions)),
        Some(selected_permissions)
            if is_permission_allowed(allowed_permission_profiles, selected_permissions) =>
        {
            Ok(Some(selected_permissions))
        }
        Some(selected_permissions) => {
            startup_warnings.push(format!(
                "Configured value for `permission_profile` is disallowed by requirements; falling back from `{selected_permissions}` to required value `{fallback_permissions}`."
            ));
            Ok(Some(fallback_permissions))
        }
    }
}

pub fn validate_required_permission_profile_catalog(
    requirements_toml: &ConfigRequirementsToml,
    available_permissions: Option<&PermissionsToml>,
) -> std::io::Result<()> {
    let is_known_profile = |profile_id: &str| {
        is_builtin_permission_profile_name(profile_id)
            || available_permissions
                .as_ref()
                .is_some_and(|permissions| permissions.entries.contains_key(profile_id))
    };

    let Some(allowed_permission_profiles) = requirements_toml.allowed_permission_profiles.as_ref()
    else {
        if requirements_toml.default_permissions.is_some() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "requirements.toml default_permissions requires allowed_permission_profiles",
            ));
        }
        return Ok(());
    };
    for profile_id in allowed_permission_profiles.keys() {
        if !is_known_profile(profile_id) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "requirements.toml allowed_permission_profiles refers to undefined profile `{profile_id}`"
                ),
            ));
        }
    }

    let Some(default_permissions) = requirements_toml
        .default_permissions
        .as_deref()
        .or_else(|| implicit_default_permissions(allowed_permission_profiles))
    else {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "requirements.toml default_permissions must be set unless allowed_permission_profiles allows both `:workspace` and `:read-only`",
        ));
    };
    if !is_permission_allowed(allowed_permission_profiles, default_permissions) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "requirements.toml default_permissions `{default_permissions}` must be allowed by allowed_permission_profiles"
            ),
        ));
    }

    Ok(())
}

fn implicit_default_permissions(
    allowed_permission_profiles: &BTreeMap<String, bool>,
) -> Option<&'static str> {
    (is_permission_allowed(allowed_permission_profiles, BUILT_IN_WORKSPACE_PROFILE)
        && is_permission_allowed(allowed_permission_profiles, BUILT_IN_READ_ONLY_PROFILE))
    .then_some(BUILT_IN_WORKSPACE_PROFILE)
}

pub(crate) fn is_permission_allowed(
    allowed_permission_profiles: &BTreeMap<String, bool>,
    profile_id: &str,
) -> bool {
    allowed_permission_profiles
        .get(profile_id)
        .copied()
        .unwrap_or(false)
}

/// A selected permission profile and its uncompiled configured and managed catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPermissionProfileSelection<'a> {
    /// The effective profile selected by configuration and managed requirements.
    pub profile_id: Option<&'a str>,
    /// Configured and managed named profiles, without platform-specific compilation.
    pub profiles: Option<PermissionsToml>,
}

/// Resolves the effective permission profile without interpreting profile paths.
///
/// Managed profiles and allowlists are applied using Core's normal selection rules,
/// while the merged profile catalog remains uncompiled for the executor to interpret.
pub fn resolve_permission_profile_selection<'a>(
    configured_profiles: Option<&PermissionsToml>,
    configured_default_profile_id: Option<&'a str>,
    requirements: &'a ConfigRequirementsToml,
) -> std::io::Result<ResolvedPermissionProfileSelection<'a>> {
    let selection = resolve_effective_permission_selection(
        configured_profiles,
        /*default_permissions_override*/ None,
        /*persisted_profile_id*/ None,
        configured_default_profile_id,
        requirements,
        &mut Vec::new(),
    )?;

    Ok(ResolvedPermissionProfileSelection {
        profile_id: selection.selected_profile_id,
        profiles: selection.profiles,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionConfigSyntax {
    Legacy,
    Profiles,
}

#[derive(Debug, Deserialize, Default)]
struct PermissionSelectionToml {
    default_permissions: Option<String>,
    sandbox_mode: Option<SandboxMode>,
}

// Resolve the named-profile catalog and selected profile id together. Runtime
// profile constraints are applied later after this selection compiles into a
// concrete `PermissionProfile`.
#[derive(Debug)]
pub struct EffectivePermissionSelection<'a> {
    pub profiles: Option<PermissionsToml>,
    pub selected_profile_id: Option<&'a str>,
    pub persisted_profile_id_was_provided: bool,
    pub requirements_force_profile_selection: bool,
}

impl EffectivePermissionSelection<'_> {
    #[must_use]
    pub fn has_profiles(&self) -> bool {
        self.profiles
            .as_ref()
            .is_some_and(|profiles| !profiles.is_empty())
    }

    #[must_use]
    pub fn profiles_are_active(
        &self,
        default_permissions_override: Option<&str>,
        permission_config_syntax: Option<PermissionConfigSyntax>,
    ) -> bool {
        self.persisted_profile_id_was_provided
            || self.requirements_force_profile_selection
            || default_permissions_override.is_some()
            || matches!(
                permission_config_syntax,
                Some(PermissionConfigSyntax::Profiles)
            )
            || permission_config_syntax.is_none()
    }
}

#[must_use]
pub fn resolve_permission_config_syntax(
    config_layer_stack: &ConfigLayerStack,
    cfg: &ConfigToml,
    sandbox_mode_override: Option<SandboxMode>,
) -> Option<PermissionConfigSyntax> {
    if sandbox_mode_override.is_some() {
        return Some(PermissionConfigSyntax::Legacy);
    }

    let session_flags_select_profiles = config_layer_stack
        .layers_high_to_low()
        .find(|layer| matches!(layer.name, ConfigLayerSource::SessionFlags))
        .and_then(|layer| {
            layer
                .config
                .clone()
                .try_into::<PermissionSelectionToml>()
                .ok()
        })
        .is_some_and(|selection| selection.default_permissions.is_some());
    if session_flags_select_profiles {
        return Some(PermissionConfigSyntax::Profiles);
    }

    let mut selection = None;
    for layer in config_layer_stack.layers_low_to_high() {
        let Ok(layer_selection) = layer.config.clone().try_into::<PermissionSelectionToml>() else {
            continue;
        };

        if layer_selection.sandbox_mode.is_some() {
            selection = Some(PermissionConfigSyntax::Legacy);
        }
        if layer_selection.default_permissions.is_some() {
            selection = Some(PermissionConfigSyntax::Profiles);
        }
    }

    selection.or_else(|| {
        if cfg.default_permissions.is_some() {
            Some(PermissionConfigSyntax::Profiles)
        } else if cfg.sandbox_mode.is_some() {
            Some(PermissionConfigSyntax::Legacy)
        } else {
            None
        }
    })
}

pub fn resolve_effective_permission_selection<'a>(
    configured_profiles: Option<&PermissionsToml>,
    default_permissions_override: Option<&'a str>,
    persisted_profile_id: Option<(&'a str, &ConfigPathContext)>,
    configured_default_profile_id: Option<&'a str>,
    requirements_toml: &'a ConfigRequirementsToml,
    startup_warnings: &mut Vec<String>,
) -> std::io::Result<EffectivePermissionSelection<'a>> {
    let profiles = merge_managed_permission_profiles(configured_profiles, requirements_toml)?;
    validate_user_permission_profile_names(profiles.as_ref())?;
    validate_required_permission_profile_catalog(requirements_toml, profiles.as_ref())?;
    let valid_persisted_profile_id = persisted_profile_id.and_then(|(profile_id, context)| {
        compile_permission_profile(
            profiles.as_ref(),
            profile_id,
            context,
            /*workspace_write*/ None,
            &mut Vec::new(),
        )
        .is_ok()
        .then_some(profile_id)
    });
    let selected_profile_id = resolve_default_permissions(
        default_permissions_override.or(valid_persisted_profile_id),
        configured_default_profile_id,
        requirements_toml,
        startup_warnings,
    )?;

    Ok(EffectivePermissionSelection {
        profiles,
        selected_profile_id,
        persisted_profile_id_was_provided: default_permissions_override.is_none()
            && valid_persisted_profile_id.is_some(),
        requirements_force_profile_selection: requirements_toml
            .allowed_permission_profiles
            .is_some(),
    })
}
