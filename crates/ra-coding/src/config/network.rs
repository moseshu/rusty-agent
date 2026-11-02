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
use ra_execpolicy::Policy;
#[cfg(feature = "test-api")]
use ra_network_proxy::managed_proxy_ports;
use ra_network_proxy::normalize_host;
use ra_network_proxy::{
    BlockedRequestObserver, ConfigReloader, ConfigReloaderFuture, ConfigState,
    EnvironmentNetworkPolicy, LocalBindingPolicy, ManagedProxyRouting, NetworkDecision,
    NetworkPolicyDecider, NetworkProxy, NetworkProxyAuditMetadata, NetworkProxyHandle,
    NetworkProxyState, Platform, build_config_state,
};
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

use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid portable environment network policy")]
pub struct EnvironmentNetworkConfigError;

/// Drops listener addresses and rejects unsupported controller fields in an
/// environment's selected network configuration. Use the returned value when
/// deciding whether a policy is present and when preparing it.
///
/// Domain and socket values may be replaced by feature settings or requirements.
/// Validate the composed policy with [`validate_environment_network_policy`].
pub fn project_environment_profile_network(
    network: Option<NetworkToml>,
) -> Result<Option<NetworkToml>, EnvironmentNetworkConfigError> {
    let Some(mut network) = network else {
        return Ok(None);
    };
    network.proxy_url = None;
    network.socks_url = None;
    let supported = NetworkToml {
        enabled: network.enabled,
        allow_upstream_proxy: network.allow_upstream_proxy,
        dangerously_allow_all_unix_sockets: network.dangerously_allow_all_unix_sockets,
        domains: network.domains.clone(),
        unix_sockets: network.unix_sockets.clone(),
        allow_local_binding: network.allow_local_binding,
        ..Default::default()
    };
    if network != supported {
        return Err(EnvironmentNetworkConfigError);
    }
    Ok(Some(network))
}

/// Validates the owner policy for its final permission profile using the native
/// environment resolver. Execution also applies controller and saved exec rules.
/// This checks the supplied executor's path syntax; socket support is checked at execution.
pub fn validate_environment_network_policy(
    policy: &EnvironmentNetworkPolicy,
    permission_profile: &PermissionProfile,
    executor_os: Platform,
) -> Result<(), EnvironmentNetworkConfigError> {
    NetworkProxySpec::for_environment(
        /*controller*/ None,
        policy,
        permission_profile,
        &Policy::empty(),
        LocalBindingPolicy::DefaultFalse,
    )
    .and_then(|spec| spec.build_config_state_for_spec(executor_os).map(|_| ()))
    .map_err(|_| EnvironmentNetworkConfigError)
}

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

pub struct StartedNetworkProxy {
    proxy: NetworkProxy,
    network_policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    _handle: NetworkProxyHandle,
}

impl StartedNetworkProxy {
    fn new(
        proxy: NetworkProxy,
        handle: NetworkProxyHandle,
        network_policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    ) -> Self {
        Self {
            proxy,
            network_policy_decider,
            _handle: handle,
        }
    }

    #[must_use]
    pub fn proxy(&self) -> NetworkProxy {
        self.proxy.clone()
    }

    pub(crate) fn network_policy_decider(&self) -> Option<Arc<dyn NetworkPolicyDecider>> {
        self.network_policy_decider.as_ref().map(Arc::clone)
    }
}

#[derive(Clone)]
struct StaticNetworkProxyReloader {
    state: ConfigState,
}

impl StaticNetworkProxyReloader {
    fn new(state: ConfigState) -> Self {
        Self { state }
    }
}

impl ConfigReloader for StaticNetworkProxyReloader {
    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
        Box::pin(async { Ok(self.state.clone()) })
    }

    fn source_label(&self) -> String {
        "StaticNetworkProxyReloader".to_string()
    }
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

impl NetworkProxySpec {
    // Codex also compiles these two on Windows for `windows_sandbox_proxy_listeners`, which
    // arrives with the Windows sandbox backend.
    #[cfg(feature = "test-api")]
    pub(crate) fn configured_proxy_ports(&self) -> std::io::Result<Vec<u16>> {
        managed_proxy_ports(&self.config).map_err(std::io::Error::other)
    }

    #[cfg(feature = "test-api")]
    pub(crate) fn allow_local_binding(&self) -> bool {
        self.config.allow_local_binding()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_proxy(
        &self,
        permission_profile: &PermissionProfile,
        managed_proxy_routing: ManagedProxyRouting,
        local_binding_policy: LocalBindingPolicy,
        policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
        enable_network_approval_flow: bool,
        audit_metadata: NetworkProxyAuditMetadata,
    ) -> std::io::Result<StartedNetworkProxy> {
        let state = self.build_state_with_audit_metadata(
            audit_metadata,
            Platform::native(),
            local_binding_policy,
        )?;
        let mut builder = NetworkProxy::builder()
            .state(Arc::new(state))
            .managed_proxy_routing(managed_proxy_routing);
        let retained_policy_decider = policy_decider.as_ref().map(Arc::clone);
        if enable_network_approval_flow && !self.hard_deny_allowlist_misses {
            if let Some(policy_decider) = policy_decider {
                builder = builder.policy_decider_arc(policy_decider);
            } else if Self::managed_sandbox_active(permission_profile) {
                builder = builder
                    .policy_decider(|_request| async { NetworkDecision::ask("not_allowed") });
            }
        }
        if let Some(blocked_request_observer) = blocked_request_observer {
            builder = builder.blocked_request_observer_arc(blocked_request_observer);
        }
        let proxy = builder.build().await.map_err(|err| {
            std::io::Error::other(format!("failed to build network proxy: {err}"))
        })?;
        let handle = proxy
            .run()
            .await
            .map_err(|err| std::io::Error::other(format!("failed to run network proxy: {err}")))?;
        Ok(StartedNetworkProxy::new(
            proxy,
            handle,
            retained_policy_decider,
        ))
    }

    pub(crate) fn recompute_for_permission_profile(
        &self,
        permission_profile: &PermissionProfile,
    ) -> std::io::Result<Self> {
        Self::from_config_and_constraints(
            self.base_config.clone(),
            self.requirements.clone(),
            permission_profile,
        )
    }

    /// Returns proxy activation and traffic policy without controller-owned runtime settings.
    #[must_use]
    pub fn environment_policy(&self) -> EnvironmentNetworkPolicy {
        EnvironmentNetworkPolicy::from_config(&self.config, self.hard_deny_allowlist_misses)
    }

    pub(crate) fn for_environment(
        controller: Option<&Self>,
        policy: &EnvironmentNetworkPolicy,
        permission_profile: &PermissionProfile,
        exec_policy: &Policy,
        local_binding_policy: LocalBindingPolicy,
    ) -> std::io::Result<Self> {
        if matches!(permission_profile, PermissionProfile::Disabled) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "environment network policy requires managed network enforcement",
            ));
        }
        if controller.is_some_and(|controller| !controller.enabled()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "environment network policy cannot override a disabled controller proxy",
            ));
        }
        let mut spec = match controller {
            Some(controller) => controller.recompute_for_permission_profile(permission_profile)?,
            None => Self::from_config_and_constraints(
                NetworkProxyConfig {
                    enabled: true,
                    // Without a controller, the owner supplies the entire permission ceiling.
                    dangerously_allow_all_unix_sockets: Some(true),
                    allow_local_binding: Some(policy.allow_local_binding.unwrap_or(false)),
                    ..NetworkProxyConfig::default()
                },
                /*requirements*/ None,
                permission_profile,
            )?,
        };
        policy.apply_to(&mut spec.config);
        spec.config.allow_local_binding = Some(local_binding_policy.resolve(&spec.config));
        let protected_denials = spec.config.denied_domains().unwrap_or_default();

        // A fixed controller allowlist remains a ceiling; an expandable one is only a baseline.
        // Profiles without managed approvals must not reuse approvals cached by another profile.
        let fixed_allowlist = spec.hard_deny_allowlist_misses
            || spec.constraints.allowlist_expansion_enabled == Some(false);
        spec.hard_deny_allowlist_misses |= policy.managed_allowed_domains_only
            || !Self::managed_sandbox_active(permission_profile);
        let allow_owner_grants = !spec.hard_deny_allowlist_misses && !fixed_allowlist;
        if fixed_allowlist {
            spec.constraints.allowlist_expansion_enabled = None;
        } else {
            spec.constraints.allowed_domains =
                Some(spec.config.allowed_domains().unwrap_or_default());
            spec.constraints.allowlist_expansion_enabled = Some(allow_owner_grants);
        }
        spec.constraints.denylist_expansion_enabled = Some(true);

        // Saved grants can extend a reviewable owner policy; owner denials are restored last.
        let (allowed_domains, denied_domains) = exec_policy.compiled_network_domains();
        if allow_owner_grants {
            upsert_network_domains(&mut spec.config, allowed_domains, /*allow*/ true);
        }
        upsert_network_domains(&mut spec.config, denied_domains, /*allow*/ false);
        upsert_network_domains(&mut spec.config, protected_denials, /*allow*/ false);
        spec.constraints.denied_domains = spec.config.denied_domains();

        validate_policy_against_constraints(&spec.config, &spec.constraints).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("environment network policy violates managed requirements: {error}"),
            )
        })?;
        tracing::debug!(
            controller_policy = ?controller.map(Self::environment_policy),
            controller_allow_all_unix_sockets = ?controller.and_then(|spec| spec.config.dangerously_allow_all_unix_sockets),
            attachment_policy = ?policy,
            effective_policy = ?spec.environment_policy(),
            "resolved environment network policy"
        );
        Ok(spec)
    }

    pub(crate) fn with_exec_policy_network_rules(
        &self,
        exec_policy: &Policy,
    ) -> std::io::Result<Self> {
        let mut spec = self.clone();
        apply_exec_policy_network_rules(&mut spec.config, exec_policy);
        validate_policy_against_constraints(&spec.config, &spec.constraints).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("network proxy constraints are invalid: {err}"),
            )
        })?;
        Ok(spec)
    }

    pub(crate) async fn apply_to_started_proxy(
        &self,
        started_proxy: &StartedNetworkProxy,
    ) -> std::io::Result<()> {
        let state = self
            .build_config_state_for_spec(Platform::from_platform_os(Some(std::env::consts::OS)))?;
        started_proxy
            .proxy()
            .replace_config_state(state)
            .await
            .map_err(|err| {
                std::io::Error::other(format!("failed to update network proxy state: {err}"))
            })
    }

    pub(crate) fn build_state_with_audit_metadata(
        &self,
        audit_metadata: NetworkProxyAuditMetadata,
        executor_os: Platform,
        local_binding_policy: LocalBindingPolicy,
    ) -> std::io::Result<NetworkProxyState> {
        let state = self.build_config_state_for_spec(executor_os)?;
        let reloader = Arc::new(StaticNetworkProxyReloader::new(state.clone()));
        Ok(NetworkProxyState::with_reloader_and_audit_metadata(
            state,
            reloader,
            audit_metadata,
            local_binding_policy,
        ))
    }

    pub(super) fn build_config_state_for_spec(
        &self,
        executor_os: Platform,
    ) -> std::io::Result<ConfigState> {
        let mut config = self.config.clone();
        if !self.enabled() {
            // Retained managed constraints must not let brokerage activate a disabled proxy.
            config.set_credential_broker_enabled(/*enabled*/ false);
        }
        build_config_state(config, self.constraints.clone(), executor_os).map_err(|err| {
            std::io::Error::other(format!("failed to build network proxy state: {err}"))
        })
    }
}

fn apply_exec_policy_network_rules(config: &mut NetworkProxyConfig, exec_policy: &Policy) {
    let (allowed_domains, denied_domains) = exec_policy.compiled_network_domains();
    upsert_network_domains(config, allowed_domains, /*allow*/ true);
    upsert_network_domains(config, denied_domains, /*allow*/ false);
}

fn upsert_network_domains(config: &mut NetworkProxyConfig, hosts: Vec<String>, allow: bool) {
    let mut incoming = HashSet::new();
    for host in hosts {
        if incoming.insert(host.clone()) {
            config.upsert_domain_permission(
                host,
                if allow {
                    NetworkDomainPermission::Allow
                } else {
                    NetworkDomainPermission::Deny
                },
                normalize_host,
            );
        }
    }
}

/// Codex's crate-private spec internals for the external integration-test workspace, which ports
/// `network_proxy_spec_tests.rs`.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::{
        ConfigState, EnvironmentNetworkPolicy, LocalBindingPolicy, NetworkConstraints,
        NetworkProxyAuditMetadata, NetworkProxyConfig, NetworkProxyConstraints, NetworkProxySpec,
        NetworkProxyState, NetworkToml, NetworkTomlExt, PermissionProfile, Platform, Policy,
    };

    #[must_use]
    pub fn constraints(spec: &NetworkProxySpec) -> &NetworkProxyConstraints {
        &spec.constraints
    }

    #[must_use]
    pub fn hard_deny_allowlist_misses(spec: &NetworkProxySpec) -> bool {
        spec.hard_deny_allowlist_misses
    }

    /// Codex's tests build a spec from its fields directly.
    #[must_use]
    pub fn from_parts(
        base_config: NetworkProxyConfig,
        requirements: Option<NetworkConstraints>,
        config: NetworkProxyConfig,
        constraints: NetworkProxyConstraints,
        hard_deny_allowlist_misses: bool,
    ) -> NetworkProxySpec {
        NetworkProxySpec {
            base_config,
            requirements,
            config,
            constraints,
            hard_deny_allowlist_misses,
        }
    }

    pub fn build_config_state_for_spec(
        spec: &NetworkProxySpec,
        executor_os: Platform,
    ) -> std::io::Result<ConfigState> {
        spec.build_config_state_for_spec(executor_os)
    }

    pub fn for_environment(
        controller: Option<&NetworkProxySpec>,
        policy: &EnvironmentNetworkPolicy,
        permission_profile: &PermissionProfile,
        exec_policy: &Policy,
        local_binding_policy: LocalBindingPolicy,
    ) -> std::io::Result<NetworkProxySpec> {
        NetworkProxySpec::for_environment(
            controller,
            policy,
            permission_profile,
            exec_policy,
            local_binding_policy,
        )
    }

    pub fn build_state_with_audit_metadata(
        spec: &NetworkProxySpec,
        audit_metadata: NetworkProxyAuditMetadata,
        executor_os: Platform,
        local_binding_policy: LocalBindingPolicy,
    ) -> std::io::Result<NetworkProxyState> {
        spec.build_state_with_audit_metadata(audit_metadata, executor_os, local_binding_policy)
    }

    pub fn configured_proxy_ports(spec: &NetworkProxySpec) -> std::io::Result<Vec<u16>> {
        spec.configured_proxy_ports()
    }

    #[must_use]
    pub fn allow_local_binding(spec: &NetworkProxySpec) -> bool {
        spec.allow_local_binding()
    }

    #[must_use]
    pub fn to_network_proxy_config(network: &NetworkToml) -> NetworkProxyConfig {
        network.to_network_proxy_config()
    }
}
