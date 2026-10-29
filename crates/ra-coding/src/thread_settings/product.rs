// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Named permission profiles resolved through the product configuration, as Codex resolves them.
//!
//! Codex selects a named profile by loading its configuration again with the profile as
//! `default_permissions` (`ConfigManager::load_permission_config_for_thread`), rejecting a
//! selection its requirements disallow, and restores a recorded profile as
//! `persisted_permission_profile_id`, which falls back to the configured default when it no longer
//! resolves. A thread here does the same with the builder its configuration was loaded from.

use std::path::Path;

use ra_protocol::server::protocol::ThreadPolicyOverrides;

use super::{CodingThreadSettings, CodingThreadSettingsAdapter, profile};
use crate::config::{Config, ConfigBuilder};

/// Codex's warning when requirements refuse a configured permission profile.
const DISALLOWED_PROFILE_WARNING: &str = "Configured value for `permission_profile` is disallowed";

/// The configuration a thread's named permission profiles are resolved through.
#[derive(Clone)]
pub struct ProductPermissionSource {
    builder: ConfigBuilder,
}

impl std::fmt::Debug for ProductPermissionSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProductPermissionSource")
            .finish_non_exhaustive()
    }
}

impl ProductPermissionSource {
    /// Resolves profiles with `builder`, the builder the thread's configuration came from.
    #[must_use]
    pub const fn new(builder: ConfigBuilder) -> Self {
        Self { builder }
    }

    /// Merge request selections and persisted fallback state before the first configuration
    /// load, matching Codex's thread config overrides. Explicit selections replace harness
    /// permission overrides; saved state remains subordinate to those overrides.
    pub(crate) fn apply_initial_policy(
        builder: &mut ConfigBuilder,
        policy: &ThreadPolicyOverrides,
    ) -> Result<(), String> {
        policy.validate()?;
        let overrides = builder.harness_overrides_mut();
        if let Some(approval) = policy.approval_policy() {
            overrides.approval_policy = Some(approval);
        }
        if let Some(reviewer) = policy.approvals_reviewer() {
            overrides.approvals_reviewer = Some(reviewer);
        }
        if policy.permissions().is_some() || policy.sandbox_policy().is_some() {
            overrides.default_permissions = None;
            overrides.sandbox_mode = None;
            overrides.permission_profile = None;
            overrides.persisted_permission_profile_id = None;
            overrides.persisted_permission_profile = None;
        }
        if let Some(id) = policy.permissions() {
            overrides.default_permissions = Some(id.to_owned());
        } else if let Some(sandbox) = policy.sandbox_policy() {
            // Thread loading accepts a mode, not a complete live sandboxPolicy. Let Config
            // derive roots, network access and temporary-directory settings from its table.
            overrides.sandbox_mode = Some(match sandbox {
                ra_config::SandboxPolicy::ReadOnly { .. } => ra_config::SandboxMode::ReadOnly,
                ra_config::SandboxPolicy::WorkspaceWrite { .. } => {
                    ra_config::SandboxMode::WorkspaceWrite
                }
                ra_config::SandboxPolicy::DangerFullAccess => {
                    ra_config::SandboxMode::DangerFullAccess
                }
                _ => return Err("thread loading requires a supported sandbox mode".to_owned()),
            });
        } else if let Some(id) = policy.persisted_permissions() {
            overrides.persisted_permission_profile_id = Some(id.to_owned());
        } else if let Some(saved) = policy.persisted_permission_profile() {
            overrides.persisted_permission_profile = serde_json::from_value(saved.clone()).ok();
        }
        Ok(())
    }

    /// Loads the configuration for `cwd`, with `select` adjusting the harness overrides.
    async fn load(
        &self,
        cwd: &Path,
        select: impl FnOnce(&mut crate::config::ConfigOverrides),
    ) -> std::io::Result<Config> {
        let mut builder = self.builder.clone();
        let overrides = builder.harness_overrides_mut();
        overrides.cwd = Some(cwd.to_path_buf());
        overrides.approval_policy = None;
        overrides.approvals_reviewer = None;
        overrides.default_permissions = None;
        overrides.persisted_permission_profile_id = None;
        overrides.sandbox_mode = None;
        overrides.permission_profile = None;
        overrides.persisted_permission_profile = None;
        select(overrides);
        builder.build().await
    }

    /// A settings or turn update must not silently accept an explicit disallowed profile, as
    /// Codex's `turn_processor` refuses it; loading a thread falls back instead.
    fn validate_named_selection(config: &Config) -> Result<(), String> {
        if let Some(warning) = config
            .startup_warnings
            .iter()
            .find(|warning| warning.contains(DISALLOWED_PROFILE_WARNING))
        {
            return Err(warning.clone());
        }
        Ok(())
    }

    /// Applies the named profile `update` selects, explicitly or as recorded.
    pub(super) async fn select_named_profile(
        &self,
        settings: &mut CodingThreadSettings,
        update: &ThreadPolicyOverrides,
    ) -> Result<(), String> {
        if let Some(profile_id) = update.permissions() {
            let config = self
                .load(&settings.cwd, |overrides| {
                    overrides.default_permissions = Some(profile_id.to_owned());
                })
                .await
                .map_err(|error| error.to_string())?;
            Self::validate_named_selection(&config)?;
            return apply_profile(settings, &config);
        }
        if let Some(profile_id) = update.persisted_permissions()
            && update.sandbox_policy().is_none()
        {
            let config = self
                .load(&settings.cwd, |overrides| {
                    overrides.persisted_permission_profile_id = Some(profile_id.to_owned());
                })
                .await
                .map_err(|error| error.to_string())?;
            return apply_profile(settings, &config);
        }
        if let Some(saved) = update.persisted_permission_profile()
            && update.sandbox_policy().is_none()
        {
            // A profile this product no longer reads leaves the configured permissions in place.
            let Ok(saved) = serde_json::from_value(saved.clone()) else {
                return Ok(());
            };
            let config = self
                .load(&settings.cwd, |overrides| {
                    overrides.persisted_permission_profile = Some(saved);
                })
                .await
                .map_err(|error| error.to_string())?;
            return apply_profile(settings, &config);
        }
        Ok(())
    }
}

/// Moves the profile `config` resolved onto the thread, within the thread's own constraints.
fn apply_profile(settings: &mut CodingThreadSettings, config: &Config) -> Result<(), String> {
    let snapshot = profile::preserve_session_denies(
        &settings.profile_permissions,
        &config.permissions.permission_profile_state().snapshot(),
    );
    let mut permissions = settings.profile_permissions.clone();
    permissions
        .set_permission_profile_from_session_snapshot(snapshot)
        .map_err(|error| error.to_string())?;
    settings.sandbox_policy =
        profile::wire_policy(&permissions.legacy_sandbox_policy(&settings.cwd));
    settings.active_permission_profile = permissions.active_permission_profile();
    settings.file_system_read_policy = None;
    settings.profile_permissions = permissions;
    Ok(())
}

impl CodingThreadSettingsAdapter {
    /// Takes the thread's permissions from `config`, Codex's effective configuration for the
    /// thread, and resolves later named selections through `source`, the builder `config` was
    /// loaded with. Approval policy, reviewer, working directory, the canonical permission profile
    /// and its requirement constraints all come from `config`; command rules, rule home and the
    /// approval cache stay as this adapter was set up.
    #[must_use]
    pub fn with_product_config(mut self, config: &Config, source: ProductPermissionSource) -> Self {
        let cwd = config.cwd.to_path_buf();
        let permissions = config.permissions.clone();
        self.initial.sandbox_policy =
            profile::wire_policy(&permissions.legacy_sandbox_policy(&cwd));
        self.initial.active_permission_profile = permissions.active_permission_profile();
        self.initial.approval_policy = permissions.approval_policy.value();
        self.initial.approvals_reviewer = config.approvals_reviewer;
        self.initial.file_system_read_policy = None;
        self.initial.profile_permissions = permissions;
        self.initial.cwd = cwd;
        self.product = Some(std::sync::Arc::new(source));
        self
    }
}
