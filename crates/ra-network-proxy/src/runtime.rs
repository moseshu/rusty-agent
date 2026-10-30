// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::if_not_else,
    clippy::manual_let_else,
    reason = "Preserve the upstream implementation and callback contracts."
)]
use crate::config::NetworkDomainPermission;
use crate::config::NetworkMode;
use crate::config::NetworkProxyConfig;
use crate::config::ValidatedUnixSocketPath;
use crate::credential_broker::CredentialBroker;
use crate::domain_matcher::DomainPatternSet;
use crate::mitm::MitmState;
use crate::mitm_hook::HookEvaluation;
use crate::mitm_hook::MitmHooksByHost;
use crate::mitm_hook::evaluate_mitm_hooks;
use crate::network_policy::NetworkPolicyAuditObserver;
use crate::policy::Host;
use crate::policy::is_loopback_host;
use crate::policy::is_non_public_ip;
use crate::policy::normalize_host;
use crate::policy::unscoped_ip_literal;
use crate::reasons::REASON_DENIED;
use crate::reasons::REASON_NOT_ALLOWED;
use crate::reasons::REASON_NOT_ALLOWED_LOCAL;
use crate::state::NetworkProxyConstraintError;
use crate::state::NetworkProxyConstraints;
use crate::state::build_config_state;
use crate::state::validate_policy_against_constraints;
use anyhow::Context;
use anyhow::Result;
use opentelemetry::trace::SpanContext;
use ra_paths::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::lookup_host;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::debug;
use tracing::info;
use tracing::warn;

const MAX_BLOCKED_EVENTS: usize = 200;
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const NETWORK_POLICY_VIOLATION_PREFIX: &str = "CODEX_NETWORK_POLICY_VIOLATION";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProxyAuditMetadata {
    pub conversation_id: Option<String>,
    pub app_version: Option<String>,
    pub user_account_id: Option<String>,
    pub auth_mode: Option<String>,
    pub originator: Option<String>,
    pub user_email: Option<String>,
    pub terminal_type: Option<String>,
    pub model: Option<String>,
    pub slug: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostBlockReason {
    Denied,
    NotAllowed,
    NotAllowedLocal,
}

impl HostBlockReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => REASON_DENIED,
            Self::NotAllowed => REASON_NOT_ALLOWED,
            Self::NotAllowedLocal => REASON_NOT_ALLOWED_LOCAL,
        }
    }
}

impl std::fmt::Display for HostBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostAuthorization {
    RequireAllowlist,
    Approved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostBlockDecision {
    Allowed,
    Blocked(HostBlockReason),
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockedRequest {
    pub host: String,
    pub reason: String,
    pub client: Option<String>,
    pub method: Option<String>,
    pub mode: Option<NetworkMode>,
    pub protocol: String,
    #[serde(skip)]
    pub execution_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub timestamp: i64,
}

pub struct BlockedRequestArgs {
    pub host: String,
    pub reason: String,
    pub client: Option<String>,
    pub method: Option<String>,
    pub mode: Option<NetworkMode>,
    pub protocol: String,
    pub decision: Option<String>,
    pub source: Option<String>,
    pub port: Option<u16>,
}

impl BlockedRequest {
    pub fn new(args: BlockedRequestArgs) -> Self {
        let BlockedRequestArgs {
            host,
            reason,
            client,
            method,
            mode,
            protocol,
            decision,
            source,
            port,
        } = args;
        Self {
            host,
            reason,
            client,
            method,
            mode,
            protocol,
            execution_id: None,
            decision,
            source,
            port,
            timestamp: unix_timestamp(),
        }
    }
}

fn blocked_request_violation_log_line(entry: &BlockedRequest) -> String {
    match serde_json::to_string(entry) {
        Ok(json) => format!("{NETWORK_POLICY_VIOLATION_PREFIX} {json}"),
        Err(err) => {
            debug!("failed to serialize blocked request for violation log: {err}");
            format!(
                "{NETWORK_POLICY_VIOLATION_PREFIX} host={} reason={}",
                entry.host, entry.reason
            )
        }
    }
}

#[derive(Clone)]
pub struct ConfigState {
    /// Preserve the target when policy edits rebuild state on a controller.
    pub(crate) executor_os: crate::Platform,
    pub config: NetworkProxyConfig,
    pub(crate) brokerage_created_default_allowlist: bool,
    pub allow_set: DomainPatternSet,
    pub deny_set: DomainPatternSet,
    pub mitm: Option<Arc<MitmState>>,
    pub mitm_hooks: MitmHooksByHost,
    pub constraints: NetworkProxyConstraints,
    pub blocked: VecDeque<BlockedRequest>,
    pub blocked_total: u64,
}

pub trait ConfigReloader: Send + Sync {
    /// Human-readable description of where config is loaded from, for logs.
    fn source_label(&self) -> String;

    /// Return a freshly loaded state if a reload is needed; otherwise, return `None`.
    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>>;

    /// Force a reload, regardless of whether a change was detected.
    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState>;
}

pub type ConfigReloaderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

struct StaticConfigReloader;

impl ConfigReloader for StaticConfigReloader {
    fn source_label(&self) -> String {
        "static config state".to_string()
    }

    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
        Box::pin(async { anyhow::bail!("static config state cannot be reloaded") })
    }
}

pub trait BlockedRequestObserver: Send + Sync + 'static {
    fn on_blocked_request(&self, request: BlockedRequest) -> BlockedRequestObserverFuture<'_>;
}

pub type BlockedRequestObserverFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

impl<O: BlockedRequestObserver + ?Sized> BlockedRequestObserver for Arc<O> {
    fn on_blocked_request(&self, request: BlockedRequest) -> BlockedRequestObserverFuture<'_> {
        Box::pin(async move { (**self).on_blocked_request(request).await })
    }
}

impl<F, Fut> BlockedRequestObserver for F
where
    F: Fn(BlockedRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn on_blocked_request(&self, request: BlockedRequest) -> BlockedRequestObserverFuture<'_> {
        Box::pin((self)(request))
    }
}

/// How the executor resolves and validates local binding when managed networking is enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LocalBindingPolicy {
    /// Omission defaults to false; either explicit value is accepted.
    #[default]
    DefaultFalse,
    /// Omission defaults to true; explicit false is rejected.
    RequireTrue,
}

impl LocalBindingPolicy {
    /// Resolves omission without changing the configured value.
    pub fn resolve(self, config: &NetworkProxyConfig) -> bool {
        config
            .allow_local_binding
            .unwrap_or(config.enabled && self == Self::RequireTrue)
    }

    pub(crate) fn resolved_config(self, config: &NetworkProxyConfig) -> NetworkProxyConfig {
        NetworkProxyConfig {
            allow_local_binding: Some(self.resolve(config)),
            ..config.clone()
        }
    }
}

pub struct NetworkProxyState {
    state: Arc<RwLock<ConfigState>>,
    /// Belongs to this proxy; config retains omission for launches on other executors.
    pub(crate) local_binding_policy: LocalBindingPolicy,
    pub(crate) proxy_private_ips_via_upstream: bool,
    reloader: Arc<dyn ConfigReloader>,
    blocked_request_observer: Arc<RwLock<Option<Arc<dyn BlockedRequestObserver>>>>,
    pub(crate) policy_audit_observer: Option<NetworkPolicyAuditObserver>,
    pub(crate) launch_span_context: Option<SpanContext>,
    pub(crate) process_log_metadata: crate::NetworkProxyProcessLogMetadata,
    credential_broker: CredentialBroker,
    audit_metadata: NetworkProxyAuditMetadata,
    execution_attributions: Arc<Mutex<HashMap<String, ExecutionAttribution>>>,
    environment_id: Option<Arc<str>>,
    execution_id: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostMitmRequirement {
    None,
    Credential(crate::brokered_tunnel::BrokeredProtocols),
    Always,
}

#[derive(Clone)]
struct ExecutionAttribution {
    environment_id: String,
    execution_id: String,
}

impl std::fmt::Debug for NetworkProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid logging internal state (config contents, compiled domain patterns, etc.) which can be noisy
        // and may contain sensitive paths.
        f.debug_struct("NetworkProxyState").finish_non_exhaustive()
    }
}

impl Clone for NetworkProxyState {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            local_binding_policy: self.local_binding_policy,
            proxy_private_ips_via_upstream: self.proxy_private_ips_via_upstream,
            reloader: self.reloader.clone(),
            blocked_request_observer: self.blocked_request_observer.clone(),
            policy_audit_observer: self.policy_audit_observer.clone(),
            launch_span_context: self.launch_span_context.clone(),
            process_log_metadata: self.process_log_metadata.clone(),
            credential_broker: self.credential_broker.clone(),
            audit_metadata: self.audit_metadata.clone(),
            execution_attributions: self.execution_attributions.clone(),
            environment_id: self.environment_id.clone(),
            execution_id: self.execution_id.clone(),
        }
    }
}

impl NetworkProxyState {
    /// Selects routing for permitted private IPs without changing destination policy.
    /// Set by the execution host, independently of reloadable or remote policy.
    pub fn set_proxy_private_ips_via_upstream(&mut self, enabled: bool) {
        self.proxy_private_ips_via_upstream = enabled;
    }

    /// Builds runtime state for one executor-local proxy launch.
    /// The launching executor supplies its own OS; the wire policy cannot override it.
    pub fn from_remote_launch_config(
        launch: crate::RemoteNetworkProxyLaunchConfig,
        executor_os: crate::Platform,
    ) -> Result<Self> {
        let crate::RemoteNetworkProxyLaunchConfig {
            proxy,
            audit_metadata,
            environment_id,
            execution_id,
            policy_decision_timeout_ms: _,
        } = launch;
        anyhow::ensure!(
            proxy.enabled,
            "executor-local network proxy launch requires an enabled proxy"
        );
        let config = proxy.into_network_proxy_config();
        let state = build_config_state(config, NetworkProxyConstraints::default(), executor_os)?;
        Ok(Self {
            environment_id: environment_id.map(Into::into),
            execution_id: execution_id.map(Into::into),
            ..Self::with_reloader_and_audit_metadata(
                state,
                Arc::new(StaticConfigReloader),
                audit_metadata,
                LocalBindingPolicy::DefaultFalse,
            )
        })
    }

    pub fn with_reloader(state: ConfigState, reloader: Arc<dyn ConfigReloader>) -> Self {
        Self::with_reloader_and_audit_metadata(
            state,
            reloader,
            NetworkProxyAuditMetadata::default(),
            LocalBindingPolicy::DefaultFalse,
        )
    }

    pub fn with_reloader_and_blocked_observer(
        state: ConfigState,
        reloader: Arc<dyn ConfigReloader>,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    ) -> Self {
        Self::with_reloader_and_audit_metadata_and_blocked_observer(
            state,
            reloader,
            NetworkProxyAuditMetadata::default(),
            LocalBindingPolicy::DefaultFalse,
            blocked_request_observer,
        )
    }

    pub fn with_reloader_and_audit_metadata(
        state: ConfigState,
        reloader: Arc<dyn ConfigReloader>,
        audit_metadata: NetworkProxyAuditMetadata,
        local_binding_policy: LocalBindingPolicy,
    ) -> Self {
        Self::with_reloader_and_audit_metadata_and_blocked_observer(
            state,
            reloader,
            audit_metadata,
            local_binding_policy,
            /*blocked_request_observer*/ None,
        )
    }

    pub fn with_reloader_and_audit_metadata_and_blocked_observer(
        state: ConfigState,
        reloader: Arc<dyn ConfigReloader>,
        audit_metadata: NetworkProxyAuditMetadata,
        local_binding_policy: LocalBindingPolicy,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    ) -> Self {
        let credential_broker = CredentialBroker::new(state.config.credential_broker);
        credential_broker.configure(&local_binding_policy.resolved_config(&state.config));
        Self {
            credential_broker,
            state: Arc::new(RwLock::new(state)),
            local_binding_policy,
            proxy_private_ips_via_upstream: false,
            reloader,
            blocked_request_observer: Arc::new(RwLock::new(blocked_request_observer)),
            policy_audit_observer: None,
            launch_span_context: None,
            process_log_metadata: crate::NetworkProxyProcessLogMetadata::default(),
            audit_metadata,
            execution_attributions: Arc::new(Mutex::new(HashMap::new())),
            environment_id: None,
            execution_id: None,
        }
    }

    pub(crate) fn register_execution(
        &self,
        attribution_token: &str,
        environment_id: &str,
        execution_id: &str,
    ) {
        self.execution_attributions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                attribution_token.to_string(),
                ExecutionAttribution {
                    environment_id: environment_id.to_string(),
                    execution_id: execution_id.to_string(),
                },
            );
    }

    pub(crate) fn unregister_execution(&self, attribution_token: &str) {
        self.execution_attributions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(attribution_token);
    }

    pub(crate) fn for_execution_token(&self, token: &str) -> Option<Self> {
        let attribution = self
            .execution_attributions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(token)?
            .clone();
        Some(Self {
            environment_id: Some(attribution.environment_id.into()),
            execution_id: Some(attribution.execution_id.into()),
            ..self.clone()
        })
    }

    pub(crate) fn for_environment_id(&self, environment_id: Option<&str>) -> Self {
        Self {
            environment_id: environment_id
                .map(Arc::from)
                .or_else(|| self.environment_id.clone()),
            ..self.clone()
        }
    }

    pub(crate) fn environment_id(&self) -> Option<&str> {
        self.environment_id.as_deref()
    }

    pub(crate) fn execution_id(&self) -> Option<String> {
        self.execution_id.as_deref().map(str::to_string)
    }

    pub async fn set_blocked_request_observer(
        &self,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    ) {
        let mut observer = self.blocked_request_observer.write().await;
        *observer = blocked_request_observer;
    }

    /// Installs a best-effort observer for every final domain and non-domain policy decision.
    pub fn set_policy_audit_observer(&mut self, observer: NetworkPolicyAuditObserver) {
        self.policy_audit_observer = Some(observer);
    }

    /// Retains the initiating process request's trace reference for network decision logs.
    pub fn set_launch_span_context(&mut self, span_context: SpanContext) {
        self.launch_span_context = span_context.is_valid().then_some(span_context);
    }

    /// Adds launch-local log fields. Registration IDs must come from the executor, not the peer.
    pub fn set_process_log_metadata(&mut self, metadata: crate::NetworkProxyProcessLogMetadata) {
        self.process_log_metadata = metadata;
    }

    pub fn audit_metadata(&self) -> &NetworkProxyAuditMetadata {
        &self.audit_metadata
    }

    pub fn virtualize_child_credentials(&self, env: &mut HashMap<String, String>) {
        self.virtualize_child_credentials_for_environment(env, self.environment_id());
    }

    pub(crate) fn virtualize_child_credentials_for_environment(
        &self,
        env: &mut HashMap<String, String>,
        environment_id: Option<&str>,
    ) {
        let parent_env = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        self.credential_broker
            .discover_parent_credentials_for_environment(&parent_env, env, environment_id);
        self.credential_broker
            .virtualize_child_env_for_environment(env, environment_id);
    }

    pub(crate) fn restore_child_credentials(
        &self,
        env: &mut HashMap<String, String>,
        command: &mut [String],
    ) {
        self.credential_broker.restore_child_env(env, command);
    }

    pub(crate) fn virtualize_snapshot_credentials(
        &self,
        env: &mut HashMap<String, String>,
        environment_id: Option<&str>,
    ) {
        self.credential_broker
            .virtualize_snapshot_env(env, environment_id);
    }

    pub(crate) fn child_credential_alias_matches(
        &self,
        key: &str,
        value: &str,
        snapshot_value: &str,
        environment_id: Option<&str>,
    ) -> bool {
        self.credential_broker
            .child_alias_matches(key, value, snapshot_value, environment_id)
    }

    pub(crate) fn restore_and_disable_child_credentials(
        &self,
        env: &mut HashMap<String, String>,
        command: &mut [String],
    ) {
        self.credential_broker
            .restore_and_disable_child_env(env, command);
    }

    pub(crate) fn virtualize_brokered_text(
        &self,
        text: &mut String,
        env: &HashMap<String, String>,
    ) -> bool {
        self.credential_broker.virtualize_text(text, env)
    }

    pub(crate) fn credential_broker_environment(
        &self,
        env: &HashMap<String, String>,
    ) -> crate::CredentialBrokerEnvironment {
        self.credential_broker.environment(env)
    }

    pub(crate) fn credential_broker_environment_for_text(
        &self,
        text: &str,
        env: &HashMap<String, String>,
    ) -> crate::CredentialBrokerEnvironment {
        self.credential_broker.environment_for_text(text, env)
    }

    pub(crate) fn credential_broker_source_matches_text(
        &self,
        source: &str,
        source_value: &str,
        text: &str,
    ) -> bool {
        self.credential_broker
            .source_matches_text(source, source_value, text)
    }

    pub(crate) fn credential_broker_sources_allowed(
        &self,
        value: &str,
        virtualized: &str,
        source_env: &HashMap<String, String>,
        is_allowed: impl Fn(&str) -> bool,
    ) -> bool {
        self.credential_broker
            .provider_sources_allowed(value, virtualized, source_env, is_allowed)
    }

    pub(crate) fn restore_brokered_text(&self, text: &mut String) -> bool {
        self.credential_broker.restore_text(text)
    }

    pub fn inject_request_credentials(&self, host: &str, headers: &mut rama_http::HeaderMap) {
        self.credential_broker
            .inject_request_headers_for_environment(host, headers, self.environment_id());
    }

    pub async fn plaintext_credential_injection_enabled(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard
            .config
            .dangerously_allow_plaintext_credential_injection)
    }

    pub async fn current_cfg(&self) -> Result<NetworkProxyConfig> {
        self.current_cfg_with_brokerage_provenance()
            .await
            .map(|(config, _, _)| config)
    }

    pub(crate) fn credential_broker_config_revision(&self) -> u64 {
        self.credential_broker.config_revision()
    }

    pub(crate) async fn current_cfg_with_brokerage_provenance(
        &self,
    ) -> Result<(NetworkProxyConfig, bool, crate::Platform)> {
        // Callers treat `NetworkProxyState` as a live view of policy. We reload-on-demand so edits to
        // `config.toml` (including Codex-managed writes) take effect without a restart.
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok((
            guard.config.clone(),
            guard.brokerage_created_default_allowlist,
            guard.executor_os,
        ))
    }

    pub async fn current_patterns(&self) -> Result<(Vec<String>, Vec<String>)> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok((
            guard.config.allowed_domains().unwrap_or_default(),
            guard.config.denied_domains().unwrap_or_default(),
        ))
    }

    pub async fn enabled(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.enabled)
    }

    pub async fn force_reload(&self) -> Result<()> {
        match self.reloader.reload_now().await {
            Ok(mut new_state) => {
                {
                    let mut guard = self.state.write().await;
                    self.credential_broker
                        .configure(&self.local_binding_policy.resolved_config(&new_state.config));
                    // Log policy diffs without dumping potentially sensitive config values.
                    log_policy_changes(&guard.config, &new_state.config);
                    new_state.blocked = guard.blocked.clone();
                    *guard = new_state;
                }
                let source = self.reloader.source_label();
                info!("reloaded config from {source}");
                Ok(())
            }
            Err(err) => {
                let source = self.reloader.source_label();
                warn!("failed to reload config from {source}: {err}; keeping previous config");
                Err(err)
            }
        }
    }

    pub async fn replace_config_state(&self, mut new_state: ConfigState) -> Result<()> {
        self.reload_if_needed().await?;
        let mut guard = self.state.write().await;
        self.credential_broker
            .configure(&self.local_binding_policy.resolved_config(&new_state.config));
        log_policy_changes(&guard.config, &new_state.config);
        new_state.blocked = guard.blocked.clone();
        new_state.blocked_total = guard.blocked_total;
        *guard = new_state;
        info!("updated network proxy config state");
        Ok(())
    }

    pub async fn host_blocked(&self, host: &str, port: u16) -> Result<HostBlockDecision> {
        self.host_blocked_with_local_binding(
            host,
            port,
            /*allow_local_binding*/ None,
            HostAuthorization::RequireAllowlist,
        )
        .await
    }

    pub(crate) async fn host_blocked_with_local_binding(
        &self,
        host: &str,
        port: u16,
        allow_local_binding: Option<bool>,
        authorization: HostAuthorization,
    ) -> Result<HostBlockDecision> {
        self.host_blocked_with_lookup(
            host,
            port,
            allow_local_binding,
            authorization,
            |host, port| async move {
                lookup_host((host.as_str(), port))
                    .await
                    .map(Iterator::collect)
            },
        )
        .await
    }

    async fn host_blocked_with_lookup<F, Fut>(
        &self,
        host: &str,
        port: u16,
        allow_local_binding: Option<bool>,
        authorization: HostAuthorization,
        lookup: F,
    ) -> Result<HostBlockDecision>
    where
        F: FnOnce(String, u16) -> Fut,
        Fut: Future<Output = std::io::Result<Vec<SocketAddr>>>,
    {
        self.reload_if_needed().await?;
        let host = match Host::parse(host) {
            Ok(host) => host,
            Err(_) => return Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowed)),
        };
        let (deny_set, allow_set, allow_local_binding, allowed_domains) = {
            let guard = self.state.read().await;
            let allowed_domains = guard.config.allowed_domains();
            (
                guard.deny_set.clone(),
                guard.allow_set.clone(),
                // Keep explicit controller restrictions even if the executor launched with true.
                allow_local_binding
                    .unwrap_or_else(|| self.local_binding_policy.resolve(&guard.config))
                    && guard.config.allow_local_binding != Some(false),
                allowed_domains,
            )
        };
        let allowed_domains = allowed_domains.unwrap_or_default();

        let host_str = host.as_str();

        // Decision order matters:
        //  1) explicit deny always wins
        //  2) local/private networking is opt-in (defense-in-depth)
        //  3) DNS requires an allowlist match or an approval
        if domains_match_host_or_unscoped(&deny_set, host_str) {
            return Ok(HostBlockDecision::Blocked(HostBlockReason::Denied));
        }

        let is_authorized = authorization == HostAuthorization::Approved
            || domains_match_host_or_unscoped(&allow_set, host_str);
        if !allow_local_binding {
            // If the intent is "prevent access to local/internal networks", we must not rely solely
            // on string checks like `localhost` / `127.0.0.1`. Attackers can use DNS rebinding or
            // public suffix services that map hostnames onto private IPs.
            //
            // We therefore do a best-effort DNS + IP classification check before allowing the
            // request. Explicit local/loopback literals are allowed only when explicitly
            // allowlisted; hostnames that resolve to local/private IPs are blocked even if
            // allowlisted.
            let local_literal = {
                let host_no_scope = unscoped_ip_literal(host_str).unwrap_or(host_str);
                if is_loopback_host(&host) {
                    true
                } else if let Ok(ip) = host_no_scope.parse::<IpAddr>() {
                    is_non_public_ip(ip)
                } else {
                    false
                }
            };

            if local_literal {
                if !is_explicit_local_allowlisted(&allowed_domains, &host) {
                    return Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal));
                }
            } else if is_authorized
                && host_resolves_to_non_public_ip(host_str, port, DNS_LOOKUP_TIMEOUT, lookup).await
            {
                return Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal));
            }
        }

        if !is_authorized {
            Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowed))
        } else {
            Ok(HostBlockDecision::Allowed)
        }
    }

    pub async fn record_blocked(&self, mut entry: BlockedRequest) -> Result<()> {
        self.reload_if_needed().await?;
        entry.execution_id = self.execution_id();
        let blocked_for_observer = entry.clone();
        let blocked_request_observer = self.blocked_request_observer.read().await.clone();
        let violation_line = blocked_request_violation_log_line(&entry);
        let host = entry.host.clone();
        let reason = entry.reason.clone();
        let decision = entry.decision.clone();
        let source = entry.source.clone();
        let protocol = entry.protocol.clone();
        let port = entry.port;
        let (total, buffered) = {
            let mut guard = self.state.write().await;
            guard.blocked.push_back(entry);
            guard.blocked_total = guard.blocked_total.saturating_add(1);
            let total = guard.blocked_total;
            while guard.blocked.len() > MAX_BLOCKED_EVENTS {
                guard.blocked.pop_front();
            }
            (total, guard.blocked.len())
        };
        debug!(
            "recorded blocked request telemetry (\
             total={total}, host={host}, reason={reason}, \
             decision={decision:?}, source={source:?}, \
             protocol={protocol}, port={port:?}, buffered={buffered})"
        );
        debug!("{violation_line}");

        if let Some(observer) = blocked_request_observer {
            observer.on_blocked_request(blocked_for_observer).await;
        }
        Ok(())
    }

    /// Returns a snapshot of buffered blocked-request entries without consuming
    /// them.
    pub async fn blocked_snapshot(&self) -> Result<Vec<BlockedRequest>> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.blocked.iter().cloned().collect())
    }

    /// Drain and return the buffered blocked-request entries in FIFO order.
    pub async fn drain_blocked(&self) -> Result<Vec<BlockedRequest>> {
        self.reload_if_needed().await?;
        let blocked = {
            let mut guard = self.state.write().await;
            std::mem::take(&mut guard.blocked)
        };
        Ok(blocked.into_iter().collect())
    }

    pub async fn is_unix_socket_allowed(&self, path: &str) -> Result<bool> {
        self.reload_if_needed().await?;
        if !unix_socket_permissions_supported() {
            return Ok(false);
        }

        // We only support absolute unix socket paths (a relative path would be ambiguous with
        // respect to the proxy process's CWD and can lead to confusing allowlist behavior).
        let requested_path = Path::new(path);
        if !requested_path.is_absolute() {
            return Ok(false);
        }

        let guard = self.state.read().await;
        if guard
            .config
            .dangerously_allow_all_unix_sockets
            .unwrap_or(false)
        {
            return Ok(true);
        }

        // Normalize the path while keeping the absolute-path requirement explicit.
        let requested_abs = match AbsolutePathBuf::from_absolute_path(requested_path) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        let requested_canonical = std::fs::canonicalize(requested_abs.as_path()).ok();
        for allowed in &guard.config.allow_unix_sockets() {
            let allowed_path = match ValidatedUnixSocketPath::parse(allowed) {
                Ok(ValidatedUnixSocketPath::Native(path)) => path,
                Ok(ValidatedUnixSocketPath::UnixStyleAbsolute(_)) => continue,
                Err(err) => {
                    warn!("ignoring invalid network.allow_unix_sockets entry at runtime: {err:#}");
                    continue;
                }
            };

            if allowed_path.as_path() == requested_abs.as_path() {
                return Ok(true);
            }

            // Best-effort canonicalization to reduce surprises with symlinks.
            // If canonicalization fails (e.g., socket not created yet), fall back to raw comparison.
            let Some(requested_canonical) = &requested_canonical else {
                continue;
            };
            if let Ok(allowed_canonical) = std::fs::canonicalize(allowed_path.as_path())
                && &allowed_canonical == requested_canonical
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn method_allowed(&self, method: &str) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.mode.allows_method(method))
    }

    pub async fn allow_upstream_proxy(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.allow_upstream_proxy)
    }

    pub async fn allow_local_binding(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(self.local_binding_policy.resolve(&guard.config))
    }

    pub async fn network_mode(&self) -> Result<NetworkMode> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.mode)
    }

    pub async fn set_network_mode(&self, mode: NetworkMode) -> Result<()> {
        loop {
            self.reload_if_needed().await?;
            let (candidate, constraints) = {
                let guard = self.state.read().await;
                let mut candidate = guard.config.clone();
                candidate.mode = mode;
                (candidate, guard.constraints.clone())
            };

            let resolved = self.local_binding_policy.resolved_config(&candidate);
            validate_policy_against_constraints(&resolved, &constraints)
                .map_err(NetworkProxyConstraintError::into_anyhow)
                .context("network.mode constrained by managed config")?;

            let mut guard = self.state.write().await;
            if guard.constraints != constraints {
                drop(guard);
                continue;
            }
            guard.config.mode = mode;
            info!("updated network mode to {mode:?}");
            return Ok(());
        }
    }

    pub async fn mitm_state(&self) -> Result<Option<Arc<MitmState>>> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.mitm.clone())
    }

    pub(crate) async fn evaluate_mitm_hook_request(
        &self,
        host: &str,
        req: &rama_http::Request,
        credential_destinations: &[String],
    ) -> Result<HookEvaluation> {
        self.reload_if_needed().await?;
        let hooked_hosts = {
            let guard = self.state.read().await;
            match evaluate_mitm_hooks(&guard.mitm_hooks, host, req) {
                HookEvaluation::NoHooksForHost if !guard.mitm_hooks.is_empty() => {
                    guard.mitm_hooks.keys().cloned().collect::<Vec<_>>()
                }
                evaluation => return Ok(evaluation),
            }
        };

        if credential_destinations.iter().any(|destination| {
            self.credential_broker.request_matches_hooked_host_alias(
                destination,
                req.headers(),
                &hooked_hosts,
                self.environment_id(),
            )
        }) {
            Ok(HookEvaluation::HookedHostNoMatch)
        } else {
            Ok(HookEvaluation::NoHooksForHost)
        }
    }

    pub(crate) async fn host_mitm_requirement(
        &self,
        host: &str,
        port: u16,
    ) -> Result<HostMitmRequirement> {
        self.reload_if_needed().await?;
        let normalized_host = normalize_host(host);
        let host_has_mitm_hooks = {
            let guard = self.state.read().await;
            guard.mitm_hooks.contains_key(&normalized_host)
        };
        let protocols = self.credential_broker.host_protocols_for_environment(
            &normalized_host,
            port,
            self.environment_id(),
        );
        Ok(if host_has_mitm_hooks {
            HostMitmRequirement::Always
        } else if protocols.tls || protocols.http {
            HostMitmRequirement::Credential(protocols)
        } else {
            HostMitmRequirement::None
        })
    }

    pub async fn add_allowed_domain(&self, host: &str) -> Result<()> {
        self.update_domain_list(host, DomainListKind::Allow).await
    }

    pub async fn add_denied_domain(&self, host: &str) -> Result<()> {
        self.update_domain_list(host, DomainListKind::Deny).await
    }

    async fn update_domain_list(&self, host: &str, target: DomainListKind) -> Result<()> {
        let host = Host::parse(host).context("invalid network host")?;
        let normalized_host = host.as_str().to_string();
        let list_name = target.list_name();
        let constraint_field = target.constraint_field();

        loop {
            self.reload_if_needed().await?;
            let (previous_cfg, constraints, blocked, blocked_total, executor_os) = {
                let guard = self.state.read().await;
                (
                    guard.config.clone(),
                    guard.constraints.clone(),
                    guard.blocked.clone(),
                    guard.blocked_total,
                    guard.executor_os,
                )
            };

            let mut candidate = previous_cfg.clone();
            let target_entries = target.entries(&candidate);
            let opposite_entries = target.opposite_entries(&candidate);
            let target_contains = target_entries
                .iter()
                .any(|entry| normalize_host(entry) == normalized_host);
            let opposite_contains = opposite_entries
                .iter()
                .any(|entry| normalize_host(entry) == normalized_host);
            if target_contains && !opposite_contains {
                return Ok(());
            }

            candidate.upsert_domain_permission(
                normalized_host.clone(),
                target.permission(),
                normalize_host,
            );

            let resolved = self.local_binding_policy.resolved_config(&candidate);
            validate_policy_against_constraints(&resolved, &constraints)
                .map_err(NetworkProxyConstraintError::into_anyhow)
                .with_context(|| format!("{constraint_field} constrained by managed config"))?;

            let mut new_state =
                build_config_state(candidate.clone(), constraints.clone(), executor_os)
                    .with_context(|| format!("failed to compile updated network {list_name}"))?;
            new_state.blocked = blocked;
            new_state.blocked_total = blocked_total;

            let mut guard = self.state.write().await;
            if guard.constraints != constraints
                || guard.config != previous_cfg
                || guard.executor_os != executor_os
            {
                drop(guard);
                continue;
            }

            log_policy_changes(&guard.config, &candidate);
            *guard = new_state;
            info!("updated network {list_name} with {normalized_host}");
            return Ok(());
        }
    }

    async fn reload_if_needed(&self) -> Result<()> {
        match self.reloader.maybe_reload().await? {
            None => Ok(()),
            Some(mut new_state) => {
                {
                    let mut guard = self.state.write().await;
                    self.credential_broker
                        .configure(&self.local_binding_policy.resolved_config(&new_state.config));
                    log_policy_changes(&guard.config, &new_state.config);
                    new_state.blocked = guard.blocked.clone();
                    new_state.blocked_total = guard.blocked_total;
                    *guard = new_state;
                }
                let source = self.reloader.source_label();
                info!("reloaded config from {source}");
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum DomainListKind {
    Allow,
    Deny,
}

impl DomainListKind {
    fn list_name(self) -> &'static str {
        match self {
            Self::Allow => "allowlist",
            Self::Deny => "denylist",
        }
    }

    fn constraint_field(self) -> &'static str {
        match self {
            Self::Allow => "network.allowed_domains",
            Self::Deny => "network.denied_domains",
        }
    }

    fn permission(self) -> NetworkDomainPermission {
        match self {
            Self::Allow => NetworkDomainPermission::Allow,
            Self::Deny => NetworkDomainPermission::Deny,
        }
    }

    fn entries(self, network: &crate::config::NetworkProxyConfig) -> Vec<String> {
        match self {
            Self::Allow => network.allowed_domains().unwrap_or_default(),
            Self::Deny => network.denied_domains().unwrap_or_default(),
        }
    }

    fn opposite_entries(self, network: &crate::config::NetworkProxyConfig) -> Vec<String> {
        match self {
            Self::Allow => network.denied_domains().unwrap_or_default(),
            Self::Deny => network.allowed_domains().unwrap_or_default(),
        }
    }
}

pub(crate) fn unix_socket_permissions_supported() -> bool {
    cfg!(target_os = "macos")
}

async fn host_resolves_to_non_public_ip<F, Fut>(
    host: &str,
    port: u16,
    lookup_timeout: Duration,
    lookup: F,
) -> bool
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_non_public_ip(ip);
    }

    // Block the request if this DNS lookup fails. We resolve the hostname again when we connect,
    // so a failed check here does not prove the destination is public.
    let addrs = match timeout(lookup_timeout, lookup(host.to_string(), port)).await {
        Ok(Ok(addrs)) => addrs,
        Ok(Err(err)) => {
            debug!(
                "blocking host because DNS lookup failed during local/private IP check (host={host}, port={port}): {err}"
            );
            return true;
        }
        Err(_) => {
            debug!(
                "blocking host because DNS lookup timed out during local/private IP check (host={host}, port={port})"
            );
            return true;
        }
    };

    for addr in addrs {
        if is_non_public_ip(addr.ip()) {
            return true;
        }
    }

    false
}

fn log_policy_changes(previous: &NetworkProxyConfig, next: &NetworkProxyConfig) {
    let previous_allowed_domains = previous.allowed_domains().unwrap_or_default();
    let next_allowed_domains = next.allowed_domains().unwrap_or_default();
    log_domain_list_changes(
        "allowlist",
        &previous_allowed_domains,
        &next_allowed_domains,
    );
    let previous_denied_domains = previous.denied_domains().unwrap_or_default();
    let next_denied_domains = next.denied_domains().unwrap_or_default();
    log_domain_list_changes("denylist", &previous_denied_domains, &next_denied_domains);
}

fn log_domain_list_changes(list_name: &str, previous: &[String], next: &[String]) {
    let previous_set: HashSet<String> = previous
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect();
    let next_set: HashSet<String> = next
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect();

    let added = next_set
        .difference(&previous_set)
        .cloned()
        .collect::<HashSet<_>>();
    let removed = previous_set
        .difference(&next_set)
        .cloned()
        .collect::<HashSet<_>>();

    let mut seen_next = HashSet::new();
    for entry in next {
        let key = entry.to_ascii_lowercase();
        if seen_next.insert(key.clone()) && added.contains(&key) {
            info!("config entry added to {list_name}: {entry}");
        }
    }

    let mut seen_previous = HashSet::new();
    for entry in previous {
        let key = entry.to_ascii_lowercase();
        if seen_previous.insert(key.clone()) && removed.contains(&key) {
            info!("config entry removed from {list_name}: {entry}");
        }
    }
}

fn domains_match_host_or_unscoped(set: &DomainPatternSet, host: &str) -> bool {
    set.is_match(host) || unscoped_ip_literal(host).is_some_and(|ip| set.is_match(ip))
}

fn is_explicit_local_allowlisted(allowed_domains: &[String], host: &Host) -> bool {
    let normalized_host = host.as_str();
    let unscoped_host = unscoped_ip_literal(normalized_host);
    allowed_domains.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern == "*" || pattern.starts_with("*.") || pattern.starts_with("**.") {
            return false;
        }
        if pattern.contains('*') || pattern.contains('?') {
            return false;
        }
        let normalized_pattern = normalize_host(pattern);
        normalized_pattern == normalized_host
            || unscoped_host.is_some_and(|ip| normalized_pattern == ip)
    })
}

fn unix_timestamp() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}
