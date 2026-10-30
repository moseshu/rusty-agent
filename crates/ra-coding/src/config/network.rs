// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::struct_excessive_bools,
    clippy::needless_pass_by_value,
    clippy::implicit_hasher,
    clippy::too_many_lines,
    clippy::match_same_arms,
    reason = "Preserve the upstream configuration surface, branch order and diagnostic spelling."
)]
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
use ra_network_proxy::normalize_host;
pub use ra_network_proxy::{
    CredentialBrokerContext, NetworkDomainPermission, NetworkDomainPermissionEntry,
    NetworkDomainPermissions, NetworkMitmCaConfig, NetworkProxyConfig, NetworkProxyConstraintError,
    NetworkProxyConstraints, NetworkUnixSocketPermission, NetworkUnixSocketPermissions,
    host_and_port_from_network_addr, validate_policy_against_constraints,
};

use super::product::network_proxy_toml_config;

use ra_config::features::{
    Feature, FeatureToml, FeaturesToml, NetworkProxyConfigToml, NetworkProxyDomainPermissionToml,
    NetworkProxyModeToml, NetworkProxyUnixSocketPermissionToml,
};
use ra_config::network_values::NetworkMode;
use ra_config::permission_profiles::{
    is_builtin_permission_profile_name, resolve_permission_profile,
};
use ra_config::permissions_toml::{
    NetworkDomainPermissionToml, NetworkDomainPermissionsToml, NetworkUnixSocketPermissionsToml,
};
use ra_config::{ManagedFeatures, NetworkConstraints, NetworkToml, PermissionsToml, Sourced};
use ra_filesystem::models::PermissionProfile;

use std::{collections::HashMap, io};

pub fn network_proxy_config_from_profile_network(
    network: Option<&NetworkToml>,
) -> NetworkProxyConfig {
    let mut config = network.map_or_else(
        NetworkProxyConfig::default,
        NetworkTomlExt::to_network_proxy_config,
    );
    // Profile `network.enabled` controls sandbox network access. Profiles may
    // provide proxy settings for the feature gate to consume when that network
    // access is enabled, but they do not start the managed proxy on their own.
    config.enabled = false;
    config
}

pub(crate) fn apply_network_proxy_feature_config(
    config: &mut NetworkProxyConfig,
    feature_config: &NetworkProxyConfigToml,
) {
    NetworkToml {
        enabled: feature_config.enabled,
        proxy_url: feature_config.proxy_url.clone(),
        enable_socks5: feature_config.enable_socks5,
        socks_url: feature_config.socks_url.clone(),
        enable_socks5_udp: feature_config.enable_socks5_udp,
        allow_upstream_proxy: feature_config.allow_upstream_proxy,
        dangerously_allow_non_loopback_proxy: feature_config.dangerously_allow_non_loopback_proxy,
        dangerously_allow_all_unix_sockets: feature_config.dangerously_allow_all_unix_sockets,
        mode: feature_config.mode.map(|mode| match mode {
            NetworkProxyModeToml::Limited => NetworkMode::Limited,
            NetworkProxyModeToml::Full => NetworkMode::Full,
        }),
        domains: feature_config
            .domains
            .as_ref()
            .map(|domains| NetworkDomainPermissionsToml {
                entries: domains
                    .iter()
                    .map(|(pattern, permission)| {
                        let permission = match permission {
                            NetworkProxyDomainPermissionToml::Allow => {
                                NetworkDomainPermissionToml::Allow
                            }
                            NetworkProxyDomainPermissionToml::Deny => {
                                NetworkDomainPermissionToml::Deny
                            }
                        };
                        (pattern.clone(), permission)
                    })
                    .collect(),
            }),
        unix_sockets: feature_config.unix_sockets.as_ref().map(|unix_sockets| {
            NetworkUnixSocketPermissionsToml {
                entries: unix_sockets
                    .iter()
                    .map(|(path, permission)| {
                        let permission = match permission {
                            NetworkProxyUnixSocketPermissionToml::Allow => {
                                ra_config::permissions_toml::NetworkUnixSocketPermissionToml::Allow
                            }
                            NetworkProxyUnixSocketPermissionToml::Deny => {
                                ra_config::permissions_toml::NetworkUnixSocketPermissionToml::Deny
                            }
                        };
                        (path.clone(), permission)
                    })
                    .collect(),
            }
        }),
        allow_local_binding: feature_config.allow_local_binding,
        mitm: None,
    }
    .apply_to_network_proxy_config(config);
    if let Some(credential_broker) = feature_config.credential_broker {
        config.set_credential_broker_enabled(credential_broker);
    }
    if let Some(credentials) = feature_config.credentials.as_ref() {
        config.credential_providers.clone_from(credentials);
    }
}

pub fn network_proxy_config_for_profile_selection(
    permissions: Option<&PermissionsToml>,
    profile_name: &str,
) -> io::Result<NetworkProxyConfig> {
    if is_builtin_permission_profile_name(profile_name) {
        return Ok(NetworkProxyConfig::default());
    }

    ra_config::permission_profiles::reject_unknown_builtin_permission_profile(profile_name)?;
    let permissions = permissions.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "default_permissions requires a `[permissions]` table",
        )
    })?;
    let profile = resolve_permission_profile(permissions, profile_name)?;
    Ok(network_proxy_config_from_profile_network(
        profile.network.as_ref(),
    ))
}
trait NetworkTomlExt {
    fn apply_to_network_proxy_config(&self, config: &mut NetworkProxyConfig);
    fn to_network_proxy_config(&self) -> NetworkProxyConfig;
}
impl NetworkTomlExt for NetworkToml {
    fn apply_to_network_proxy_config(&self, config: &mut NetworkProxyConfig) {
        if let Some(enabled) = self.enabled {
            config.enabled = enabled;
        }
        if let Some(proxy_url) = self.proxy_url.as_ref() {
            config.proxy_url.clone_from(proxy_url);
        }
        if let Some(enable_socks5) = self.enable_socks5 {
            config.enable_socks5 = enable_socks5;
        }
        if let Some(socks_url) = self.socks_url.as_ref() {
            config.socks_url.clone_from(socks_url);
        }
        if let Some(enable_socks5_udp) = self.enable_socks5_udp {
            config.enable_socks5_udp = enable_socks5_udp;
        }
        if let Some(allow_upstream_proxy) = self.allow_upstream_proxy {
            config.allow_upstream_proxy = allow_upstream_proxy;
        }
        if let Some(dangerously_allow_non_loopback_proxy) =
            self.dangerously_allow_non_loopback_proxy
        {
            config.dangerously_allow_non_loopback_proxy = dangerously_allow_non_loopback_proxy;
        }
        if let Some(dangerously_allow_all_unix_sockets) = self.dangerously_allow_all_unix_sockets {
            config.dangerously_allow_all_unix_sockets = Some(dangerously_allow_all_unix_sockets);
        }
        if let Some(mode) = self.mode {
            config.mode = mode;
        }
        if let Some(domains) = self.domains.as_ref() {
            overlay_network_domain_permissions(config, domains);
        }
        if let Some(unix_sockets) = self.unix_sockets.as_ref() {
            let mut proxy_unix_sockets = config.unix_sockets.take().unwrap_or_default();
            for (path, permission) in &unix_sockets.entries {
                let permission = match permission {
                    ra_config::permissions_toml::NetworkUnixSocketPermissionToml::Allow => {
                        NetworkUnixSocketPermission::Allow
                    }
                    ra_config::permissions_toml::NetworkUnixSocketPermissionToml::Deny => {
                        NetworkUnixSocketPermission::Deny
                    }
                };
                proxy_unix_sockets.entries.insert(path.clone(), permission);
            }
            config.unix_sockets = Some(proxy_unix_sockets);
        }
        if let Some(allow_local_binding) = self.allow_local_binding {
            config.allow_local_binding = Some(allow_local_binding);
        }
        if let Some(mitm) = self.mitm.as_ref() {
            config.mitm_hooks = mitm.to_runtime_hooks(mitm.actions.as_ref());
        }
        config.mitm = config.mode == NetworkMode::Limited || !config.mitm_hooks.is_empty();
    }

    fn to_network_proxy_config(&self) -> NetworkProxyConfig {
        let mut config = NetworkProxyConfig::default();
        self.apply_to_network_proxy_config(&mut config);
        config
    }
}
fn overlay_network_domain_permissions(
    config: &mut NetworkProxyConfig,
    domains: &NetworkDomainPermissionsToml,
) {
    let mut current = config.domains.take().unwrap_or_default();
    for (pattern, permission) in &domains.entries {
        let permission = match permission {
            NetworkDomainPermissionToml::Allow => NetworkDomainPermission::Allow,
            NetworkDomainPermissionToml::Deny => NetworkDomainPermission::Deny,
        };
        let normalized = normalize_host(pattern);
        current
            .entries
            .retain(|entry| normalize_host(&entry.pattern) != normalized);
        current.entries.push(NetworkDomainPermissionEntry {
            pattern: pattern.clone(),
            permission,
        });
    }
    config.domains = Some(current);
}
pub struct NetworkConfigInputs<'a> {
    pub configured_proxy: NetworkProxyConfig,
    pub feature_enabled: bool,
    pub features: Option<&'a FeaturesToml>,
    pub candidate_permission_profile: &'a PermissionProfile,
    pub credential_broker_base_url: Option<&'a str>,
}

/// The configured proxy before managed network requirements are applied.
pub struct PreparedNetworkConfig {
    pub(super) configured_proxy: NetworkProxyConfig,
}

impl PreparedNetworkConfig {
    #[must_use]
    pub fn from_inputs(inputs: NetworkConfigInputs<'_>) -> Self {
        let NetworkConfigInputs {
            mut configured_proxy,
            feature_enabled,
            features,
            candidate_permission_profile,
            credential_broker_base_url,
        } = inputs;
        if feature_enabled
            && candidate_permission_profile
                .network_sandbox_policy()
                .is_enabled()
        {
            if let Some(FeatureToml::Config(config)) =
                features.and_then(|features| features.network_proxy.as_ref())
            {
                apply_network_proxy_feature_config(&mut configured_proxy, config);
            }
            configured_proxy.enabled = true;
        }
        configured_proxy.set_credential_broker_openai_base_url(credential_broker_base_url);
        Self { configured_proxy }
    }

    /// Applies managed network requirements after permission fallback has run.
    pub fn build(
        self,
        requirements: Option<Sourced<NetworkConstraints>>,
        effective_permission_profile: &PermissionProfile,
    ) -> std::io::Result<Option<NetworkProxySpec>> {
        let has_requirements = requirements.is_some();
        let network = self.build_spec(requirements, effective_permission_profile)?;
        Ok(if has_requirements {
            Some(network)
        } else {
            network.enabled().then_some(network)
        })
    }
}
impl PreparedNetworkConfig {
    fn build_spec(
        self,
        requirements: Option<Sourced<NetworkConstraints>>,
        permission_profile: &PermissionProfile,
    ) -> io::Result<NetworkProxySpec> {
        let (requirements, source) = match requirements {
            Some(Sourced { value, source }) => (Some(value), Some(source)),
            None => (None, None),
        };
        NetworkProxySpec::from_config_and_constraints(
            self.configured_proxy,
            requirements,
            permission_profile,
        )
        .map_err(|error| {
            if let Some(source) = source {
                io::Error::new(
                    error.kind(),
                    format!("failed to build managed network proxy from {source}: {error}"),
                )
            } else {
                error
            }
        })
    }
}
#[must_use]
pub fn profile_allows_configured_network_proxy(permission_profile: &PermissionProfile) -> bool {
    match permission_profile {
        PermissionProfile::Managed { network, .. } | PermissionProfile::External { network } => {
            network.is_enabled()
        }
        PermissionProfile::Disabled => false,
    }
}

pub fn build_network_proxy_spec(
    mut configured_network_proxy_config: NetworkProxyConfig,
    network_requirements: Option<Sourced<ra_config::NetworkConstraints>>,
    permission_profile: &PermissionProfile,
    environment_overrides: &HashMap<String, String>,
    features: &ManagedFeatures,
    feature_settings: Option<&FeaturesToml>,
) -> std::io::Result<Option<NetworkProxySpec>> {
    if features.enabled(Feature::CredentialMasking) {
        configured_network_proxy_config.set_credential_broker_enabled(/*enabled*/ true);
        if let Some(credentials) = network_proxy_toml_config(feature_settings)
            .and_then(|network_proxy| network_proxy.credentials.as_ref())
        {
            configured_network_proxy_config
                .credential_providers
                .clone_from(credentials);
        }
    }
    configured_network_proxy_config.configure_credential_broker_environment(environment_overrides);
    PreparedNetworkConfig {
        configured_proxy: configured_network_proxy_config,
    }
    .build(network_requirements, permission_profile)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkProxySpec {
    base_config: NetworkProxyConfig,
    requirements: Option<NetworkConstraints>,
    config: NetworkProxyConfig,
    constraints: NetworkProxyConstraints,
    hard_deny_allowlist_misses: bool,
}

impl NetworkProxySpec {
    pub fn from_config_and_constraints(
        config: NetworkProxyConfig,
        requirements: Option<NetworkConstraints>,
        permission_profile: &PermissionProfile,
    ) -> std::io::Result<Self> {
        let base_config = config.clone();
        let hard_deny_allowlist_misses = requirements
            .as_ref()
            .is_some_and(Self::managed_allowed_domains_only);
        let (config, constraints) = if let Some(requirements) = requirements.as_ref() {
            Self::apply_requirements(
                config,
                requirements,
                permission_profile,
                hard_deny_allowlist_misses,
            )
        } else {
            (config, NetworkProxyConstraints::default())
        };
        validate_policy_against_constraints(&config, &constraints).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("network proxy constraints are invalid: {err}"),
            )
        })?;
        Ok(Self {
            base_config,
            requirements,
            config,
            constraints,
            hard_deny_allowlist_misses,
        })
    }

    fn apply_requirements(
        mut config: NetworkProxyConfig,
        requirements: &NetworkConstraints,
        permission_profile: &PermissionProfile,
        hard_deny_allowlist_misses: bool,
    ) -> (NetworkProxyConfig, NetworkProxyConstraints) {
        let mut constraints = NetworkProxyConstraints::default();
        let allowlist_expansion_enabled =
            Self::allowlist_expansion_enabled(permission_profile, hard_deny_allowlist_misses);
        let denylist_expansion_enabled = Self::denylist_expansion_enabled(permission_profile);

        if let Some(enabled) = requirements.enabled {
            config.enabled = enabled;
            constraints.enabled = Some(enabled);
        }
        if let Some(http_port) = requirements.http_port {
            config.proxy_url = format!("http://127.0.0.1:{http_port}");
        }
        if let Some(socks_port) = requirements.socks_port {
            config.socks_url = format!("http://127.0.0.1:{socks_port}");
        }
        if let Some(allow_upstream_proxy) = requirements.allow_upstream_proxy {
            config.allow_upstream_proxy = allow_upstream_proxy;
            constraints.allow_upstream_proxy = Some(allow_upstream_proxy);
        }
        if let Some(dangerously_allow_non_loopback_proxy) =
            requirements.dangerously_allow_non_loopback_proxy
        {
            config.dangerously_allow_non_loopback_proxy = dangerously_allow_non_loopback_proxy;
            constraints.dangerously_allow_non_loopback_proxy =
                Some(dangerously_allow_non_loopback_proxy);
        }
        if let Some(dangerously_allow_all_unix_sockets) =
            requirements.dangerously_allow_all_unix_sockets
        {
            config.dangerously_allow_all_unix_sockets = Some(dangerously_allow_all_unix_sockets);
            constraints.dangerously_allow_all_unix_sockets =
                Some(dangerously_allow_all_unix_sockets);
        }
        let managed_allowed_domains = if hard_deny_allowlist_misses {
            Some(
                requirements
                    .domains
                    .as_ref()
                    .and_then(ra_config::NetworkDomainPermissionsToml::allowed_domains)
                    .unwrap_or_default(),
            )
        } else {
            requirements
                .domains
                .as_ref()
                .and_then(ra_config::NetworkDomainPermissionsToml::allowed_domains)
        };
        if let Some(managed_allowed_domains) = managed_allowed_domains {
            // Managed requirements seed the baseline allowlist. User additions
            // can extend that baseline unless managed-only mode pins the
            // effective allowlist to the managed set.
            let effective_allowed_domains = if allowlist_expansion_enabled {
                Self::merge_domain_lists(
                    managed_allowed_domains.clone(),
                    config.allowed_domains().as_deref().unwrap_or(&[]),
                )
            } else {
                managed_allowed_domains.clone()
            };
            config.set_allowed_domains(effective_allowed_domains);
            constraints.allowed_domains = Some(managed_allowed_domains);
            constraints.allowlist_expansion_enabled = Some(allowlist_expansion_enabled);
        }
        let managed_denied_domains = requirements
            .domains
            .as_ref()
            .and_then(ra_config::NetworkDomainPermissionsToml::denied_domains);
        if let Some(managed_denied_domains) = managed_denied_domains {
            let effective_denied_domains = if denylist_expansion_enabled {
                Self::merge_domain_lists(
                    managed_denied_domains.clone(),
                    config.denied_domains().as_deref().unwrap_or(&[]),
                )
            } else {
                managed_denied_domains.clone()
            };
            config.set_denied_domains(effective_denied_domains);
            constraints.denied_domains = Some(managed_denied_domains);
            constraints.denylist_expansion_enabled = Some(denylist_expansion_enabled);
        }
        if requirements.unix_sockets.is_some() {
            let allow_unix_sockets = requirements
                .unix_sockets
                .as_ref()
                .map(ra_config::NetworkUnixSocketPermissionsToml::allow_unix_sockets)
                .unwrap_or_default();
            config.set_allow_unix_sockets(allow_unix_sockets.clone());
            constraints.allow_unix_sockets = Some(allow_unix_sockets);
        }
        if let Some(allow_local_binding) = requirements.allow_local_binding {
            config.allow_local_binding = Some(allow_local_binding);
            constraints.allow_local_binding = Some(allow_local_binding);
        }

        (config, constraints)
    }

    fn allowlist_expansion_enabled(
        permission_profile: &PermissionProfile,
        hard_deny_allowlist_misses: bool,
    ) -> bool {
        Self::managed_sandbox_active(permission_profile) && !hard_deny_allowlist_misses
    }

    fn managed_allowed_domains_only(requirements: &NetworkConstraints) -> bool {
        requirements.managed_allowed_domains_only.unwrap_or(false)
    }

    fn denylist_expansion_enabled(permission_profile: &PermissionProfile) -> bool {
        Self::managed_sandbox_active(permission_profile)
    }

    fn managed_sandbox_active(permission_profile: &PermissionProfile) -> bool {
        matches!(permission_profile, PermissionProfile::Managed { .. })
    }

    fn merge_domain_lists(mut managed: Vec<String>, user_entries: &[String]) -> Vec<String> {
        for entry in user_entries {
            if !managed
                .iter()
                .any(|managed_entry| managed_entry.eq_ignore_ascii_case(entry))
            {
                managed.push(entry.clone());
            }
        }
        managed
    }
}
impl NetworkProxySpec {
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }
    #[must_use]
    pub fn credential_broker_enabled(&self) -> bool {
        self.enabled() && self.config.credential_broker && self.constraints.enabled != Some(false)
    }
    #[must_use]
    pub fn config(&self) -> &NetworkProxyConfig {
        &self.config
    }
}

impl NetworkProxySpec {
    #[must_use]
    pub fn proxy_host_and_port(&self) -> String {
        host_and_port_from_network_addr(&self.config.proxy_url, 3128)
    }
    #[must_use]
    pub fn socks_enabled(&self) -> bool {
        self.config.enable_socks5
    }
}
