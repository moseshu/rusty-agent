// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use crate::Platform;
use crate::credential_broker::{
    CredentialBrokerContext, environment::is_credential_broker_provider_env_key,
};
use crate::domain_matcher::normalize_host;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
pub use ra_config::network_values::{CredentialProviderConfig, MitmHookConfig, NetworkMode};
use ra_paths::AbsolutePathBuf;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::{BTreeMap, HashMap},
    net::IpAddr,
};
use std::{net::SocketAddr, path::Path};
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
    /// Trusted local destination context, never sent to remote executors or child environments.
    pub credential_broker_context: CredentialBrokerContext,
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
pub(crate) fn trusted_credential_broker_host(base_url: &str) -> Option<String> {
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
fn clamp_non_loopback(
    addr: SocketAddr,
    allow_non_loopback: bool,
    name: &str,
    override_setting_name: &str,
) -> SocketAddr {
    if addr.ip().is_loopback() {
        return addr;
    }

    if allow_non_loopback {
        warn!("DANGEROUS: {name} listening on non-loopback address {addr}");
        return addr;
    }

    warn!(
        "{name} requested non-loopback bind ({addr}); clamping to 127.0.0.1:{port} (set {override_setting_name} to override)",
        port = addr.port()
    );
    SocketAddr::from(([127, 0, 0, 1], addr.port()))
}

pub(crate) fn clamp_bind_addrs(
    http_addr: SocketAddr,
    socks_addr: SocketAddr,
    cfg: &NetworkProxyConfig,
) -> (SocketAddr, SocketAddr) {
    let http_addr = clamp_non_loopback(
        http_addr,
        cfg.dangerously_allow_non_loopback_proxy,
        "HTTP proxy",
        "dangerously_allow_non_loopback_proxy",
    );
    let socks_addr = clamp_non_loopback(
        socks_addr,
        cfg.dangerously_allow_non_loopback_proxy,
        "SOCKS5 proxy",
        "dangerously_allow_non_loopback_proxy",
    );
    if cfg.allow_unix_sockets().is_empty()
        && !cfg.dangerously_allow_all_unix_sockets.unwrap_or(false)
    {
        return (http_addr, socks_addr);
    }

    // `x-unix-socket` is intentionally a local escape hatch. If the proxy is reachable from
    // outside the machine, it can become a remote bridge into local daemons
    // (e.g. docker.sock). To avoid footguns, enforce loopback binding whenever unix sockets
    // are enabled.
    if cfg.dangerously_allow_non_loopback_proxy && !http_addr.ip().is_loopback() {
        warn!(
            "unix socket proxying is enabled; ignoring dangerously_allow_non_loopback_proxy and clamping HTTP proxy to loopback"
        );
    }
    if cfg.dangerously_allow_non_loopback_proxy && !socks_addr.ip().is_loopback() {
        warn!(
            "unix socket proxying is enabled; ignoring dangerously_allow_non_loopback_proxy and clamping SOCKS5 proxy to loopback"
        );
    }
    (
        SocketAddr::from(([127, 0, 0, 1], http_addr.port())),
        SocketAddr::from(([127, 0, 0, 1], socks_addr.port())),
    )
}

pub(crate) struct RuntimeConfig {
    pub http_addr: SocketAddr,
    pub socks_addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "Socket normalization is consumed by the upcoming proxy runtime."
)]
pub(crate) struct UnixStyleAbsolutePath(String);

#[expect(
    dead_code,
    reason = "Socket normalization is consumed by the upcoming proxy runtime."
)]
impl UnixStyleAbsolutePath {
    fn parse(value: &str) -> Option<Self> {
        value.starts_with('/').then(|| Self(value.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "Socket normalization is consumed by the upcoming proxy runtime."
)]
pub(crate) enum ValidatedUnixSocketPath {
    Native(AbsolutePathBuf),
    UnixStyleAbsolute(UnixStyleAbsolutePath),
}

#[expect(
    dead_code,
    reason = "Socket normalization is consumed by the upcoming proxy runtime."
)]
impl ValidatedUnixSocketPath {
    pub(crate) fn parse(socket_path: &str) -> Result<Self> {
        let path = Path::new(socket_path);
        if path.is_absolute() {
            let path = AbsolutePathBuf::from_absolute_path(path)
                .with_context(|| format!("failed to normalize unix socket path {socket_path:?}"))?;
            return Ok(Self::Native(path));
        }

        if let Some(path) = UnixStyleAbsolutePath::parse(socket_path) {
            return Ok(Self::UnixStyleAbsolute(path));
        }
        bail!("expected an absolute path, got {socket_path:?}");
    }
}

pub(crate) fn validate_unix_socket_allowlist_paths(
    cfg: &NetworkProxyConfig,
    executor_os: Platform,
) -> Result<()> {
    for (index, socket_path) in cfg.allow_unix_sockets().iter().enumerate() {
        anyhow::ensure!(
            crate::socket_path::socket_path_is_absolute(executor_os, socket_path)
                && !socket_path.contains('\0'),
            "invalid network.allow_unix_sockets[{index}]: expected a NUL-free absolute path for {executor_os:?}, got {socket_path:?}"
        );
    }
    Ok(())
}

pub(crate) fn resolve_runtime(
    cfg: &NetworkProxyConfig,
    executor_os: Platform,
) -> Result<RuntimeConfig> {
    validate_unix_socket_allowlist_paths(cfg, executor_os)?;

    let http_addr = resolve_addr(&cfg.proxy_url, /*default_port*/ 3128)
        .with_context(|| format!("invalid network.proxy_url: {}", cfg.proxy_url))?;
    let socks_addr = resolve_addr(&cfg.socks_url, /*default_port*/ 8081)
        .with_context(|| format!("invalid network.socks_url: {}", cfg.socks_url))?;
    let (http_addr, socks_addr) = clamp_bind_addrs(http_addr, socks_addr, cfg);

    Ok(RuntimeConfig {
        http_addr,
        socks_addr,
    })
}

/// Returns the sorted loopback ports used by the configured managed proxy listeners.
pub fn managed_proxy_ports(cfg: &NetworkProxyConfig) -> Result<Vec<u16>> {
    let runtime = resolve_runtime(cfg, Platform::native())?;
    if runtime.http_addr.port() == 0 {
        bail!("network.proxy_url must use a fixed non-zero port for managed proxy provisioning");
    }
    let mut ports = vec![runtime.http_addr.port()];
    if cfg.enable_socks5 {
        if runtime.socks_addr.port() == 0 {
            bail!(
                "network.socks_url must use a fixed non-zero port for managed proxy provisioning"
            );
        }
        ports.push(runtime.socks_addr.port());
    }
    ports.sort_unstable();
    ports.dedup();
    Ok(ports)
}

fn resolve_addr(url: &str, default_port: u16) -> Result<SocketAddr> {
    let addr_parts = parse_host_port(url, default_port)?;
    let host = if addr_parts.host.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".to_string()
    } else {
        addr_parts.host
    };
    match host.parse::<IpAddr>() {
        Ok(ip) => Ok(SocketAddr::new(ip, addr_parts.port)),
        Err(_) => Ok(SocketAddr::from(([127, 0, 0, 1], addr_parts.port))),
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

fn parse_host_port(url: &str, default_port: u16) -> Result<SocketAddressParts> {
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

fn parse_host_port_fallback(input: &str, default_port: u16) -> Result<SocketAddressParts> {
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
