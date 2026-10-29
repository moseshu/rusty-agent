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
use super::product::network_proxy_toml_config;
use anyhow::bail;
use ra_config::features::{
    Feature, FeatureToml, FeaturesToml, NetworkProxyConfigToml, NetworkProxyDomainPermissionToml,
    NetworkProxyModeToml, NetworkProxyUnixSocketPermissionToml,
};
use ra_config::network_values::{CredentialProviderConfig, MitmHookConfig, NetworkMode};
use ra_config::permission_profiles::{
    is_builtin_permission_profile_name, resolve_permission_profile,
};
use ra_config::permissions_toml::{
    NetworkDomainPermissionToml, NetworkDomainPermissionsToml, NetworkUnixSocketPermissionsToml,
};
use ra_config::{ManagedFeatures, NetworkConstraints, NetworkToml, PermissionsToml, Sourced};
use ra_filesystem::models::PermissionProfile;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::net::IpAddr;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
};
use tracing::warn;
use url::Url;
/// Variant order encodes effective precedence for duplicate patterns:
/// `None < Allow < Deny`, so deny wins over allow when entries conflict.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDomainPermission {
    None,
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkDomainPermissionEntry {
    pub pattern: String,
    pub permission: NetworkDomainPermission,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkDomainPermissions {
    pub entries: Vec<NetworkDomainPermissionEntry>,
}

impl Serialize for NetworkDomainPermissions {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.effective_entries()
            .into_iter()
            .map(|entry| (entry.pattern, entry.permission))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NetworkDomainPermissions {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = BTreeMap::<String, NetworkDomainPermission>::deserialize(deserializer)?
            .into_iter()
            .map(|(pattern, permission)| NetworkDomainPermissionEntry {
                pattern,
                permission,
            })
            .collect();
        Ok(Self { entries })
    }
}

impl NetworkDomainPermissions {
    fn effective_entries(&self) -> Vec<NetworkDomainPermissionEntry> {
        let mut order = Vec::new();
        let mut effective_permissions = BTreeMap::new();

        for entry in &self.entries {
            if !effective_permissions.contains_key(&entry.pattern) {
                order.push(entry.pattern.clone());
            }

            let permission = effective_permissions
                .entry(entry.pattern.clone())
                .or_insert(entry.permission);
            if entry.permission > *permission {
                *permission = entry.permission;
            }
        }

        order
            .into_iter()
            .filter_map(|pattern| {
                effective_permissions.remove(&pattern).map(|permission| {
                    NetworkDomainPermissionEntry {
                        pattern,
                        permission,
                    }
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkUnixSocketPermission {
    Allow,
    Deny,
}

/// Socket policy keys are preserved across controller and executor hosts.
/// Allow entries must be NUL-free and absolute according to the executor OS;
/// Deny entries are retained without path validation or expansion.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NetworkUnixSocketPermissions {
    #[serde(flatten)]
    pub entries: BTreeMap<String, NetworkUnixSocketPermission>,
}

/// A caller-provided MITM CA for trusted proxy configuration, not sandbox `config.toml`.
/// The private key file is readable only by the proxy process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkMitmCaConfig {
    pub certificate_file: String,
    pub private_key_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkProxyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_proxy_url")]
    pub proxy_url: String,
    pub enable_socks5: bool,
    #[serde(default = "default_socks_url")]
    pub socks_url: String,
    pub enable_socks5_udp: bool,
    pub allow_upstream_proxy: bool,
    #[serde(default)]
    pub dangerously_allow_non_loopback_proxy: bool,
    /// When no socket map is set, omission defers to attachment policy; execution defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    #[serde(default)]
    pub mode: NetworkMode,
    #[serde(default)]
    pub domains: Option<NetworkDomainPermissions>,
    #[serde(default)]
    pub unix_sockets: Option<NetworkUnixSocketPermissions>,
    /// Omission is resolved for the selected sandbox; ordinary proxy execution defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    #[serde(default)]
    pub mitm: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitm_ca: Option<NetworkMitmCaConfig>,
    #[serde(default)]
    pub credential_broker: bool,
    /// Whether brokerage enabled MITM rather than inheriting an explicit setting.
    #[serde(skip)]
    pub credential_broker_enabled_mitm: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_providers: BTreeMap<String, CredentialProviderConfig>,
    /// Trusted `OpenAI` endpoint derived from local configuration, never sent to remote executors.
    #[serde(skip)]
    pub credential_broker_openai_host: Option<String>,
    #[serde(skip)]
    pub credential_broker_context: CredentialBrokerContext,
    /// Trusted local destination context, never sent to remote executors or child environments.
    #[serde(skip)]
    #[serde(default)]
    pub dangerously_allow_plaintext_credential_injection: bool,
    #[serde(default)]
    pub mitm_hooks: Vec<MitmHookConfig>,
}

impl Default for NetworkProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            proxy_url: default_proxy_url(),
            enable_socks5: true,
            socks_url: default_socks_url(),
            enable_socks5_udp: true,
            allow_upstream_proxy: true,
            dangerously_allow_non_loopback_proxy: false,
            dangerously_allow_all_unix_sockets: None,
            mode: NetworkMode::default(),
            domains: None,
            unix_sockets: None,
            allow_local_binding: None,
            mitm: false,
            mitm_ca: None,
            credential_broker: false,
            credential_broker_enabled_mitm: false,
            credential_providers: BTreeMap::new(),
            credential_broker_openai_host: None,
            credential_broker_context: CredentialBrokerContext::default(),
            dangerously_allow_plaintext_credential_injection: false,
            mitm_hooks: Vec::new(),
        }
    }
}

impl NetworkProxyConfig {
    #[must_use]
    pub fn allow_local_binding(&self) -> bool {
        self.allow_local_binding.unwrap_or(false)
    }

    pub fn set_credential_broker_enabled(&mut self, enabled: bool) {
        self.credential_broker = enabled;
        if enabled {
            self.credential_broker_enabled_mitm |= !self.mitm;
            self.mitm = true;
        } else if self.credential_broker_enabled_mitm {
            self.mitm = !self.mitm_hooks.is_empty() || self.mitm_ca.is_some();
            self.credential_broker_enabled_mitm = false;
        }
    }

    pub fn set_credential_broker_openai_base_url(&mut self, base_url: Option<&str>) {
        self.credential_broker_openai_host = base_url.and_then(trusted_credential_broker_host);
    }

    /// Retains trusted destination context without changing child environment policy. Conflicting
    /// case-insensitive provider overrides disable brokerage on Windows.
    pub fn configure_credential_broker_environment(
        &mut self,
        environment: &HashMap<String, String>,
    ) {
        if cfg!(windows)
            && self.credential_broker
            && self.has_ambiguous_windows_credential_environment(environment)
        {
            warn!(
                "credential brokerage disabled because shell environment overrides contain \
                 conflicting case-insensitive provider keys"
            );
            self.set_credential_broker_enabled(/*enabled*/ false);
        }

        self.credential_broker_context = if self.credential_broker {
            CredentialBrokerContext::capture(self, environment)
        } else {
            CredentialBrokerContext::default()
        };
    }

    fn has_ambiguous_windows_credential_environment(
        &self,
        environment: &HashMap<String, String>,
    ) -> bool {
        environment.iter().any(|(key, value)| {
            let is_provider_key = is_credential_broker_provider_env_key(key)
                || self.credential_providers.values().any(|provider| {
                    provider
                        .env
                        .iter()
                        .chain(provider.url_prefix_from_env.iter())
                        .any(|candidate| key.eq_ignore_ascii_case(candidate))
                });
            is_provider_key
                && environment.iter().any(|(candidate, candidate_value)| {
                    key != candidate
                        && key.eq_ignore_ascii_case(candidate)
                        && value != candidate_value
                })
        })
    }

    #[must_use]
    pub fn allowed_domains(&self) -> Option<Vec<String>> {
        self.domain_entries(NetworkDomainPermission::Allow)
    }

    #[must_use]
    pub fn denied_domains(&self) -> Option<Vec<String>> {
        self.domain_entries(NetworkDomainPermission::Deny)
    }

    fn domain_entries(&self, permission: NetworkDomainPermission) -> Option<Vec<String>> {
        self.domains
            .as_ref()
            .map(|domains| {
                domains
                    .effective_entries()
                    .iter()
                    .filter(|entry| entry.permission == permission)
                    .map(|entry| entry.pattern.clone())
                    .collect()
            })
            .filter(|entries: &Vec<String>| !entries.is_empty())
    }

    #[must_use]
    pub fn allow_unix_sockets(&self) -> Vec<String> {
        self.unix_sockets
            .as_ref()
            .map(|unix_sockets| {
                unix_sockets
                    .entries
                    .iter()
                    .filter(|(_, permission)| {
                        matches!(permission, NetworkUnixSocketPermission::Allow)
                    })
                    .map(|(path, _)| path.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_allowed_domains(&mut self, allowed_domains: Vec<String>) {
        self.set_domain_entries(allowed_domains, NetworkDomainPermission::Allow);
    }

    pub fn set_denied_domains(&mut self, denied_domains: Vec<String>) {
        self.set_domain_entries(denied_domains, NetworkDomainPermission::Deny);
    }

    pub fn upsert_domain_permission(
        &mut self,
        host: String,
        permission: NetworkDomainPermission,
        normalize: impl Fn(&str) -> String,
    ) {
        let mut domains = self.domains.take().unwrap_or_default();
        let normalized_host = normalize(&host);
        domains
            .entries
            .retain(|entry| normalize(&entry.pattern) != normalized_host);
        domains.entries.push(NetworkDomainPermissionEntry {
            pattern: host,
            permission,
        });
        self.domains = (!domains.entries.is_empty()).then_some(domains);
    }

    pub fn set_allow_unix_sockets(&mut self, allow_unix_sockets: Vec<String>) {
        self.set_unix_socket_entries(allow_unix_sockets, NetworkUnixSocketPermission::Allow);
    }

    fn set_domain_entries(&mut self, entries: Vec<String>, permission: NetworkDomainPermission) {
        let mut domains = self.domains.take().unwrap_or_default();
        domains
            .entries
            .retain(|entry| entry.permission != permission);
        for entry in entries {
            if !domains
                .entries
                .iter()
                .any(|existing| existing.pattern == entry && existing.permission == permission)
            {
                domains.entries.push(NetworkDomainPermissionEntry {
                    pattern: entry,
                    permission,
                });
            }
        }
        self.domains = (!domains.entries.is_empty()).then_some(domains);
    }

    fn set_unix_socket_entries(
        &mut self,
        entries: Vec<String>,
        permission: NetworkUnixSocketPermission,
    ) {
        let mut unix_sockets = self.unix_sockets.take().unwrap_or_default();
        unix_sockets
            .entries
            .retain(|_, existing| *existing != permission);
        for entry in entries {
            unix_sockets.entries.insert(entry, permission);
        }
        self.unix_sockets = Some(unix_sockets);
    }
}

#[must_use]
pub fn trusted_credential_broker_host(base_url: &str) -> Option<String> {
    Url::parse(base_url)
        .ok()
        .filter(|url| {
            url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
        })
        .and_then(|url| url.host_str().map(normalize_host))
}

fn default_proxy_url() -> String {
    "http://127.0.0.1:3128".to_string()
}

fn default_socks_url() -> String {
    "http://127.0.0.1:8081".to_string()
}

/// Clamp non-loopback bind addresses to loopback unless explicitly allowed.
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetworkProxyConstraints {
    pub enabled: Option<bool>,
    pub mode: Option<NetworkMode>,
    pub allow_upstream_proxy: Option<bool>,
    pub dangerously_allow_non_loopback_proxy: Option<bool>,
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    pub allowed_domains: Option<Vec<String>>,
    pub allowlist_expansion_enabled: Option<bool>,
    pub denied_domains: Option<Vec<String>>,
    pub denylist_expansion_enabled: Option<bool>,
    pub allow_unix_sockets: Option<Vec<String>>,
    pub allow_local_binding: Option<bool>,
}

pub fn validate_policy_against_constraints(
    config: &NetworkProxyConfig,
    constraints: &NetworkProxyConstraints,
) -> Result<(), NetworkProxyConstraintError> {
    fn invalid_value(
        field_name: &'static str,
        candidate: impl Into<String>,
        allowed: impl Into<String>,
    ) -> NetworkProxyConstraintError {
        NetworkProxyConstraintError::InvalidValue {
            field_name,
            candidate: candidate.into(),
            allowed: allowed.into(),
        }
    }

    fn validate<T>(
        candidate: T,
        validator: impl FnOnce(&T) -> Result<(), NetworkProxyConstraintError>,
    ) -> Result<(), NetworkProxyConstraintError> {
        validator(&candidate)
    }

    super::mitm_validation::validate_mitm_hook_config(config).map_err(|error| {
        NetworkProxyConstraintError::InvalidValue {
            field_name: "network.mitm_hooks",
            candidate: error.to_string(),
            allowed: "valid MITM hook configuration".to_string(),
        }
    })?;
    let enabled = config.enabled;
    let config_allowed_domains = config.allowed_domains().unwrap_or_default();
    let config_denied_domains = config.denied_domains().unwrap_or_default();
    let denied_domain_overrides: HashSet<String> = config_denied_domains
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect();
    let config_allow_unix_sockets = config.allow_unix_sockets();
    validate_non_global_wildcard_domain_patterns("network.denied_domains", &config_denied_domains)?;
    if let Some(max_enabled) = constraints.enabled {
        validate(enabled, move |candidate| {
            if *candidate && !max_enabled {
                Err(invalid_value(
                    "network.enabled",
                    "true",
                    "false (disabled by managed config)",
                ))
            } else {
                Ok(())
            }
        })?;
    }

    if let Some(max_mode) = constraints.mode {
        validate(config.mode, move |candidate| {
            if network_mode_rank(*candidate) > network_mode_rank(max_mode) {
                Err(invalid_value(
                    "network.mode",
                    format!("{candidate:?}"),
                    format!("{max_mode:?} or more restrictive"),
                ))
            } else {
                Ok(())
            }
        })?;
    }

    let allow_upstream_proxy = constraints.allow_upstream_proxy;
    validate(
        config.allow_upstream_proxy,
        move |candidate| match allow_upstream_proxy {
            Some(true) | None => Ok(()),
            Some(false) => {
                if *candidate {
                    Err(invalid_value(
                        "network.allow_upstream_proxy",
                        "true",
                        "false (disabled by managed config)",
                    ))
                } else {
                    Ok(())
                }
            }
        },
    )?;

    let allow_non_loopback_proxy = constraints.dangerously_allow_non_loopback_proxy;
    validate(
        config.dangerously_allow_non_loopback_proxy,
        move |candidate| match allow_non_loopback_proxy {
            Some(true) | None => Ok(()),
            Some(false) => {
                if *candidate {
                    Err(invalid_value(
                        "network.dangerously_allow_non_loopback_proxy",
                        "true",
                        "false (disabled by managed config)",
                    ))
                } else {
                    Ok(())
                }
            }
        },
    )?;

    let allow_all_unix_sockets = constraints
        .dangerously_allow_all_unix_sockets
        .unwrap_or(constraints.allow_unix_sockets.is_none());
    validate(
        config.dangerously_allow_all_unix_sockets.unwrap_or(false),
        move |candidate| {
            if *candidate && !allow_all_unix_sockets {
                Err(invalid_value(
                    "network.dangerously_allow_all_unix_sockets",
                    "true",
                    "false (disabled by managed config)",
                ))
            } else {
                Ok(())
            }
        },
    )?;

    if let Some(allow_local_binding) = constraints.allow_local_binding {
        validate(config.allow_local_binding(), move |candidate| {
            if *candidate && !allow_local_binding {
                Err(invalid_value(
                    "network.allow_local_binding",
                    "true",
                    "false (disabled by managed config)",
                ))
            } else {
                Ok(())
            }
        })?;
    }

    if let Some(allowed_domains) = &constraints.allowed_domains {
        validate_non_global_wildcard_domain_patterns("network.allowed_domains", allowed_domains)?;
        match constraints.allowlist_expansion_enabled {
            Some(true) => {
                let required_set: HashSet<String> = allowed_domains
                    .iter()
                    .map(|entry| entry.to_ascii_lowercase())
                    .collect();
                validate(config_allowed_domains, |candidate| {
                    let candidate_set: HashSet<String> = candidate
                        .iter()
                        .map(|entry| entry.to_ascii_lowercase())
                        .collect();
                    let missing: Vec<String> = required_set
                        .iter()
                        .filter(|entry| {
                            !candidate_set.contains(*entry)
                                && !denied_domain_overrides.contains(*entry)
                        })
                        .cloned()
                        .collect();
                    if missing.is_empty() {
                        Ok(())
                    } else {
                        Err(invalid_value(
                            "network.allowed_domains",
                            "missing managed allowed_domains entries",
                            format!("{missing:?}"),
                        ))
                    }
                })?;
            }
            Some(false) => {
                let required_set: HashSet<String> = allowed_domains
                    .iter()
                    .map(|entry| entry.to_ascii_lowercase())
                    .collect();
                validate(config_allowed_domains, |candidate| {
                    let candidate_set: HashSet<String> = candidate
                        .iter()
                        .map(|entry| entry.to_ascii_lowercase())
                        .collect();
                    let expected_set: HashSet<String> = required_set
                        .difference(&denied_domain_overrides)
                        .cloned()
                        .collect();
                    if candidate_set == expected_set {
                        Ok(())
                    } else {
                        Err(invalid_value(
                            "network.allowed_domains",
                            format!("{candidate:?}"),
                            "must match managed allowed_domains",
                        ))
                    }
                })?;
            }
            None => {
                let managed_patterns: Vec<DomainPattern> = allowed_domains
                    .iter()
                    .map(|entry| DomainPattern::parse_for_constraints(entry))
                    .collect();
                validate(config_allowed_domains, move |candidate| {
                    let mut invalid = Vec::new();
                    for entry in candidate {
                        let candidate_pattern = DomainPattern::parse_for_constraints(entry);
                        if !managed_patterns
                            .iter()
                            .any(|managed| managed.allows(&candidate_pattern))
                        {
                            invalid.push(entry.clone());
                        }
                    }
                    if invalid.is_empty() {
                        Ok(())
                    } else {
                        Err(invalid_value(
                            "network.allowed_domains",
                            format!("{invalid:?}"),
                            "subset of managed allowed_domains",
                        ))
                    }
                })?;
            }
        }
    }

    if let Some(denied_domains) = &constraints.denied_domains {
        validate_non_global_wildcard_domain_patterns("network.denied_domains", denied_domains)?;
        let required_set: HashSet<String> = denied_domains
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        match constraints.denylist_expansion_enabled {
            Some(false) => {
                validate(config_denied_domains, move |candidate| {
                    let candidate_set: HashSet<String> = candidate
                        .iter()
                        .map(|entry| entry.to_ascii_lowercase())
                        .collect();
                    if candidate_set == required_set {
                        Ok(())
                    } else {
                        Err(invalid_value(
                            "network.denied_domains",
                            format!("{candidate:?}"),
                            "must match managed denied_domains",
                        ))
                    }
                })?;
            }
            Some(true) | None => {
                validate(config_denied_domains, move |candidate| {
                    let candidate_set: HashSet<String> =
                        candidate.iter().map(|s| s.to_ascii_lowercase()).collect();
                    let missing: Vec<String> = required_set
                        .iter()
                        .filter(|entry| !candidate_set.contains(*entry))
                        .cloned()
                        .collect();
                    if missing.is_empty() {
                        Ok(())
                    } else {
                        Err(invalid_value(
                            "network.denied_domains",
                            "missing managed denied_domains entries",
                            format!("{missing:?}"),
                        ))
                    }
                })?;
            }
        }
    }

    if let Some(allow_unix_sockets) = &constraints.allow_unix_sockets {
        let allowed_set: HashSet<String> = allow_unix_sockets
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        validate(config_allow_unix_sockets, move |candidate| {
            let mut invalid = Vec::new();
            for entry in candidate {
                if !allowed_set.contains(&entry.to_ascii_lowercase()) {
                    invalid.push(entry.clone());
                }
            }
            if invalid.is_empty() {
                Ok(())
            } else {
                Err(invalid_value(
                    "network.allow_unix_sockets",
                    format!("{invalid:?}"),
                    "subset of managed allow_unix_sockets",
                ))
            }
        })?;
    }

    Ok(())
}

fn validate_non_global_wildcard_domain_patterns(
    field_name: &'static str,
    patterns: &[String],
) -> Result<(), NetworkProxyConstraintError> {
    if let Some(pattern) = patterns
        .iter()
        .find(|pattern| is_global_wildcard_domain_pattern(pattern))
    {
        return Err(NetworkProxyConstraintError::InvalidValue {
            field_name,
            candidate: pattern.trim().to_string(),
            allowed: "exact hosts or scoped wildcards like *.example.com or **.example.com"
                .to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetworkProxyConstraintError {
    #[error("invalid value for {field_name}: {candidate} (allowed {allowed})")]
    InvalidValue {
        field_name: &'static str,
        candidate: String,
        allowed: String,
    },
}

impl NetworkProxyConstraintError {
    #[must_use]
    pub fn into_anyhow(self) -> anyhow::Error {
        anyhow::anyhow!(self)
    }
}

fn network_mode_rank(mode: NetworkMode) -> u8 {
    match mode {
        NetworkMode::Limited => 0,
        NetworkMode::Full => 1,
    }
}

#[derive(Debug, Clone)]
pub(crate) enum DomainPattern {
    ApexAndSubdomains(String),
    SubdomainsOnly(String),
    Exact(String),
}

impl DomainPattern {
    /// Parse a policy pattern for constraint comparisons, validating domain parts with `url`.
    pub(crate) fn parse_for_constraints(input: &str) -> Self {
        let input = input.trim();
        if input.is_empty() {
            return Self::Exact(String::new());
        }
        if let Some(domain) = input.strip_prefix("**.") {
            return Self::ApexAndSubdomains(parse_domain_for_constraints(domain));
        }
        if let Some(domain) = input.strip_prefix("*.") {
            return Self::SubdomainsOnly(parse_domain_for_constraints(domain));
        }
        Self::Exact(parse_domain_for_constraints(input))
    }

    pub(crate) fn allows(&self, candidate: &DomainPattern) -> bool {
        match self {
            DomainPattern::Exact(domain) => match candidate {
                DomainPattern::Exact(candidate) => domain_eq(candidate, domain),
                _ => false,
            },
            DomainPattern::SubdomainsOnly(domain) => match candidate {
                DomainPattern::Exact(candidate) => is_strict_subdomain(candidate, domain),
                DomainPattern::SubdomainsOnly(candidate) => {
                    is_subdomain_or_equal(candidate, domain)
                }
                DomainPattern::ApexAndSubdomains(candidate) => {
                    is_strict_subdomain(candidate, domain)
                }
            },
            DomainPattern::ApexAndSubdomains(domain) => match candidate {
                DomainPattern::Exact(candidate) => is_subdomain_or_equal(candidate, domain),
                DomainPattern::SubdomainsOnly(candidate) => {
                    is_subdomain_or_equal(candidate, domain)
                }
                DomainPattern::ApexAndSubdomains(candidate) => {
                    is_subdomain_or_equal(candidate, domain)
                }
            },
        }
    }
}

fn parse_domain_for_constraints(domain: &str) -> String {
    let domain = domain.trim().trim_end_matches('.');
    if domain.is_empty() {
        return String::new();
    }
    let host = if domain.starts_with('[') && domain.ends_with(']') {
        &domain[1..domain.len().saturating_sub(1)]
    } else {
        domain
    };
    if host.contains('*') || host.contains('?') || host.contains('%') {
        return domain.to_string();
    }
    match UrlHost::parse(host) {
        Ok(host) => host.to_string(),
        Err(_) => String::new(),
    }
}

fn normalize_domain(domain: &str) -> String {
    domain.trim_end_matches('.').to_ascii_lowercase()
}

fn domain_eq(left: &str, right: &str) -> bool {
    normalize_domain(left) == normalize_domain(right)
}

fn is_subdomain_or_equal(child: &str, parent: &str) -> bool {
    let child = normalize_domain(child);
    let parent = normalize_domain(parent);
    if child == parent {
        return true;
    }
    child.ends_with(&format!(".{parent}"))
}

fn is_strict_subdomain(child: &str, parent: &str) -> bool {
    let child = normalize_domain(child);
    let parent = normalize_domain(parent);
    child != parent && child.ends_with(&format!(".{parent}"))
}

use super::domain_matcher::{is_global_wildcard_domain_pattern, normalize_host};
use url::Host as UrlHost;
fn is_credential_broker_provider_env_key(key: &str) -> bool {
    [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_HOST",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
    ]
    .iter()
    .any(|candidate| key.eq_ignore_ascii_case(candidate))
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

/// Trusted destination hints retained separately from a child's visible environment.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CredentialBrokerContext(HashMap<String, String>);
impl std::fmt::Debug for CredentialBrokerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialBrokerContext(<redacted>)")
    }
}
impl CredentialBrokerContext {
    fn capture(config: &NetworkProxyConfig, overrides: &HashMap<String, String>) -> Self {
        Self(
            ["GH_HOST", "OPENAI_BASE_URL"]
                .into_iter()
                .map(str::to_string)
                .chain(
                    config
                        .credential_providers
                        .values()
                        .filter_map(|provider| provider.url_prefix_from_env.clone()),
                )
                .filter_map(|key| {
                    overrides
                        .iter()
                        .find(|(candidate, _)| {
                            if cfg!(windows) {
                                candidate.eq_ignore_ascii_case(&key)
                            } else {
                                *candidate == &key
                            }
                        })
                        .map(|(_, value)| value.clone())
                        .or_else(|| std::env::var(&key).ok())
                        .map(|value| (key, value))
                })
                .collect(),
        )
    }
}

#[must_use]
pub fn host_and_port_from_network_addr(value: &str, default_port: u16) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<missing>".to_string();
    }

    let Ok(parts) = parse_host_port(trimmed, default_port) else {
        return format_host_and_port(trimmed, default_port);
    };

    format_host_and_port(&parts.host, parts.port)
}

fn format_host_and_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SocketAddressParts {
    host: String,
    port: u16,
}

fn parse_host_port(url: &str, default_port: u16) -> anyhow::Result<SocketAddressParts> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("missing host in network proxy address: {url}");
    }

    // Avoid treating unbracketed IPv6 literals like "2001:db8::1" as scheme-prefixed URLs.
    if matches!(trimmed.parse::<IpAddr>(), Ok(IpAddr::V6(_))) && !trimmed.starts_with('[') {
        return Ok(SocketAddressParts {
            host: trimmed.to_string(),
            port: default_port,
        });
    }

    // Prefer the standard URL parser when the input is URL-like. Prefix a scheme when absent so
    // we still accept loose host:port inputs.
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    if let Ok(parsed) = Url::parse(&candidate)
        && let Some(host) = parsed.host_str()
    {
        let host = host.trim_matches(|c| c == '[' || c == ']');
        if host.is_empty() {
            bail!("missing host in network proxy address: {url}");
        }
        return Ok(SocketAddressParts {
            host: host.to_string(),
            port: parsed.port().unwrap_or(default_port),
        });
    }

    parse_host_port_fallback(trimmed, default_port)
}

fn parse_host_port_fallback(input: &str, default_port: u16) -> anyhow::Result<SocketAddressParts> {
    let without_scheme = input.split_once("://").map_or(input, |(_, rest)| rest);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    let host_port = host_port
        .rsplit_once('@')
        .map_or(host_port, |(_, rest)| rest);

    if host_port.starts_with('[')
        && let Some(end) = host_port.find(']')
    {
        let host = &host_port[1..end];
        let port = host_port[end + 1..]
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(default_port);
        if host.is_empty() {
            bail!("missing host in network proxy address: {input}");
        }
        return Ok(SocketAddressParts {
            host: host.to_string(),
            port,
        });
    }

    // Only treat `host:port` as such when there's a single `:`. This avoids
    // accidentally interpreting unbracketed IPv6 addresses as `host:port`.
    if host_port.bytes().filter(|b| *b == b':').count() == 1
        && let Some((host, port)) = host_port.rsplit_once(':')
    {
        if host.is_empty() {
            bail!("missing host in network proxy address: {input}");
        }
        return Ok(SocketAddressParts {
            host: host.to_string(),
            port: port.parse::<u16>().ok().unwrap_or(default_port),
        });
    }

    if host_port.is_empty() {
        bail!("missing host in network proxy address: {input}");
    }
    Ok(SocketAddressParts {
        host: host_port.to_string(),
        port: default_port,
    })
}
