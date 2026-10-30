// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use crate::reasons::REASON_POLICY_DENIED;
use crate::request_cancellation::NetworkRequestCancellation;
use crate::request_disconnect::NetworkRequestDisconnect;
use crate::runtime::HostAuthorization;
use crate::runtime::HostBlockDecision;
use crate::runtime::HostBlockReason;
use crate::state::NetworkProxyState;
use anyhow::Result;
use chrono::SecondsFormat;
use chrono::Utc;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const AUDIT_TARGET: &str = "codex_otel.network_proxy";
const POLICY_DECISION_EVENT_NAME: &str = "codex.network_proxy.policy_decision";
const POLICY_SCOPE_DOMAIN: &str = "domain";
const POLICY_SCOPE_NON_DOMAIN: &str = "non_domain";
const POLICY_DECISION_ALLOW: &str = "allow";
const POLICY_DECISION_DENY: &str = "deny";
const POLICY_REASON_ALLOW: &str = "allow";
const DEFAULT_METHOD: &str = "none";
const DEFAULT_CLIENT_ADDRESS: &str = "unknown";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkProtocol {
    Http,
    HttpsConnect,
    Socks5Tcp,
    Socks5Udp,
}

/// A completed network-policy audit decision without tenant or session identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkPolicyAuditEvent {
    pub timestamp: String,
    pub scope: String,
    pub decision: String,
    pub source: String,
    pub reason: String,
    pub protocol: NetworkProtocol,
    pub host: String,
    pub port: u16,
    pub method: Option<String>,
    pub client: Option<String>,
    pub policy_override: bool,
}

/// Observes final network-policy decisions without delaying or altering enforcement.
///
/// Implementations must return immediately and treat notification delivery as best effort.
pub type NetworkPolicyAuditObserver = Arc<dyn Fn(NetworkPolicyAuditEvent) + Send + Sync + 'static>;

impl NetworkProtocol {
    pub const fn as_policy_protocol(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::HttpsConnect => "https_connect",
            Self::Socks5Tcp => "socks5_tcp",
            Self::Socks5Udp => "socks5_udp",
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicyDecision {
    Deny,
    Ask,
}

impl NetworkPolicyDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDecisionSource {
    BaselinePolicy,
    ModeGuard,
    ProxyState,
    Decider,
}

impl NetworkDecisionSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BaselinePolicy => "baseline_policy",
            Self::ModeGuard => "mode_guard",
            Self::ProxyState => "proxy_state",
            Self::Decider => "decider",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkPolicyRequest {
    pub protocol: NetworkProtocol,
    pub host: String,
    pub port: u16,
    pub environment_id: Option<String>,
    pub client_addr: Option<String>,
    pub method: Option<String>,
    pub command: Option<String>,
    pub exec_policy_hint: Option<String>,
    pub execution_id: Option<String>,
    /// Present only when the local HTTP transport can identify an abandoned request.
    pub disconnect: Option<NetworkRequestDisconnect>,
    /// Controller-owned cause, published before an abandoned decision future is dropped.
    pub cancellation: Option<NetworkRequestCancellation>,
}

pub struct NetworkPolicyRequestArgs {
    pub protocol: NetworkProtocol,
    pub host: String,
    pub port: u16,
    pub environment_id: Option<String>,
    pub client_addr: Option<String>,
    pub method: Option<String>,
    pub command: Option<String>,
    pub exec_policy_hint: Option<String>,
}

impl NetworkPolicyRequest {
    pub fn new(args: NetworkPolicyRequestArgs) -> Self {
        let NetworkPolicyRequestArgs {
            protocol,
            host,
            port,
            environment_id,
            client_addr,
            method,
            command,
            exec_policy_hint,
        } = args;
        Self {
            protocol,
            host,
            port,
            environment_id,
            client_addr,
            method,
            command,
            exec_policy_hint,
            execution_id: None,
            disconnect: None,
            cancellation: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkDecision {
    Allow,
    Deny {
        reason: String,
        source: NetworkDecisionSource,
        decision: NetworkPolicyDecision,
    },
}

impl NetworkDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::deny_with_source(reason, NetworkDecisionSource::Decider)
    }

    pub fn ask(reason: impl Into<String>) -> Self {
        Self::ask_with_source(reason, NetworkDecisionSource::Decider)
    }

    pub fn deny_with_source(reason: impl Into<String>, source: NetworkDecisionSource) -> Self {
        let reason = reason.into();
        let reason = if reason.is_empty() {
            REASON_POLICY_DENIED.to_string()
        } else {
            reason
        };
        Self::Deny {
            reason,
            source,
            decision: NetworkPolicyDecision::Deny,
        }
    }

    pub fn ask_with_source(reason: impl Into<String>, source: NetworkDecisionSource) -> Self {
        let reason = reason.into();
        let reason = if reason.is_empty() {
            REASON_POLICY_DENIED.to_string()
        } else {
            reason
        };
        Self::Deny {
            reason,
            source,
            decision: NetworkPolicyDecision::Ask,
        }
    }
}

pub(crate) struct BlockDecisionAuditEventArgs<'a> {
    pub source: NetworkDecisionSource,
    pub reason: &'a str,
    pub protocol: NetworkProtocol,
    pub server_address: &'a str,
    pub server_port: u16,
    pub method: Option<&'a str>,
    pub client_addr: Option<&'a str>,
}

pub(crate) fn emit_block_decision_audit_event(
    state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
) {
    emit_non_domain_policy_decision_audit_event(state, args, POLICY_DECISION_DENY);
}

pub(crate) fn emit_allow_decision_audit_event(
    state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
) {
    emit_non_domain_policy_decision_audit_event(state, args, POLICY_DECISION_ALLOW);
}

fn emit_non_domain_policy_decision_audit_event(
    state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
    decision: &'static str,
) {
    let execution_id = state.execution_id();
    emit_policy_audit_event(
        state,
        PolicyAuditEventArgs {
            scope: POLICY_SCOPE_NON_DOMAIN,
            decision,
            source: args.source.as_str(),
            reason: args.reason,
            protocol: args.protocol,
            server_address: args.server_address,
            server_port: args.server_port,
            method: args.method,
            client_addr: args.client_addr,
            execution_id: execution_id.as_deref(),
            policy_override: false,
        },
    );
}

struct PolicyAuditEventArgs<'a> {
    scope: &'static str,
    decision: &'a str,
    source: &'a str,
    reason: &'a str,
    protocol: NetworkProtocol,
    server_address: &'a str,
    server_port: u16,
    method: Option<&'a str>,
    client_addr: Option<&'a str>,
    execution_id: Option<&'a str>,
    policy_override: bool,
}

fn emit_policy_audit_event(state: &NetworkProxyState, args: PolicyAuditEventArgs<'_>) {
    let audit_metadata = state.audit_metadata();
    let process_log_metadata = &state.process_log_metadata;
    let conversation_id = process_log_metadata
        .thread_id
        .as_deref()
        .or(audit_metadata.conversation_id.as_deref());
    let timestamp = audit_timestamp();
    let launch_trace_id = state
        .launch_span_context
        .as_ref()
        .map(|context| context.trace_id().to_string());
    let launch_span_id = state
        .launch_span_context
        .as_ref()
        .map(|context| context.span_id().to_string());
    let executor_identity = process_log_metadata.executor_identity.as_ref();
    tracing::event!(
        target: AUDIT_TARGET,
        tracing::Level::INFO,
        event.name = POLICY_DECISION_EVENT_NAME,
        event.timestamp = %timestamp,
        launch.trace_id = launch_trace_id.as_deref(),
        launch.span_id = launch_span_id.as_deref(),
        conversation.id = conversation_id,
        tool.call_id = process_log_metadata.tool_call_id.as_deref(),
        executor.environment_id = executor_identity.map(|identity| identity.environment_id.as_str()),
        executor.registration_id = executor_identity.map(|identity| identity.registration_id.as_str()),
        app.version = audit_metadata.app_version.as_deref(),
        auth_mode = audit_metadata.auth_mode.as_deref(),
        originator = audit_metadata.originator.as_deref(),
        user.account_id = audit_metadata.user_account_id.as_deref(),
        user.email = audit_metadata.user_email.as_deref(),
        terminal.type = audit_metadata.terminal_type.as_deref(),
        model = audit_metadata.model.as_deref(),
        slug = audit_metadata.slug.as_deref(),
        network.policy.scope = args.scope,
        network.policy.decision = args.decision,
        network.policy.source = args.source,
        network.policy.reason = args.reason,
        network.transport.protocol = args.protocol.as_policy_protocol(),
        server.address = args.server_address,
        server.port = args.server_port,
        http.request.method = args.method.unwrap_or(DEFAULT_METHOD),
        client.address = args.client_addr.unwrap_or(DEFAULT_CLIENT_ADDRESS),
        execution.id = args.execution_id,
        network.policy.override = args.policy_override,
    );
    if let Some(observer) = &state.policy_audit_observer {
        observer(NetworkPolicyAuditEvent {
            timestamp,
            scope: args.scope.to_string(),
            decision: args.decision.to_string(),
            source: args.source.to_string(),
            reason: args.reason.to_string(),
            protocol: args.protocol,
            host: args.server_address.to_string(),
            port: args.server_port,
            method: args.method.map(str::to_string),
            client: args.client_addr.map(str::to_string),
            policy_override: args.policy_override,
        });
    }
}

fn audit_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Decide whether a network request should be allowed.
///
/// If `command` or `exec_policy_hint` is provided, callers can map exec-policy
/// approvals to network access (e.g., allow all requests for commands matching
/// approved prefixes like `curl *`).
pub trait NetworkPolicyDecider: Send + Sync + 'static {
    fn decide(&self, req: NetworkPolicyRequest) -> NetworkPolicyDeciderFuture<'_>;
}

pub type NetworkPolicyDeciderFuture<'a> =
    Pin<Box<dyn Future<Output = NetworkDecision> + Send + 'a>>;

impl<D: NetworkPolicyDecider + ?Sized> NetworkPolicyDecider for Arc<D> {
    fn decide(&self, req: NetworkPolicyRequest) -> NetworkPolicyDeciderFuture<'_> {
        Box::pin(async move { (**self).decide(req).await })
    }
}

impl<F, Fut> NetworkPolicyDecider for F
where
    F: Fn(NetworkPolicyRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = NetworkDecision> + Send + 'static,
{
    fn decide(&self, req: NetworkPolicyRequest) -> NetworkPolicyDeciderFuture<'_> {
        Box::pin((self)(req))
    }
}

pub(crate) async fn evaluate_host_policy(
    state: &NetworkProxyState,
    decider: Option<&Arc<dyn NetworkPolicyDecider>>,
    request: &NetworkPolicyRequest,
) -> Result<NetworkDecision> {
    let execution_id = state.execution_id();
    let host_decision = state.host_blocked(&request.host, request.port).await?;
    let (decision, policy_override) = match host_decision {
        HostBlockDecision::Allowed => (NetworkDecision::Allow, false),
        HostBlockDecision::Blocked(HostBlockReason::NotAllowed) => {
            if let Some(decider) = decider {
                let mut attributed_request = request.clone();
                if attributed_request.environment_id.is_none()
                    && let Some(environment_id) = state.environment_id()
                {
                    attributed_request.environment_id = Some(environment_id.to_string());
                }
                attributed_request.execution_id = execution_id.clone();
                let mut decider_decision =
                    map_decider_decision(decider.decide(attributed_request).await);
                // Approval permits DNS, but cannot bypass private-address or explicit deny policy.
                if matches!(decider_decision, NetworkDecision::Allow)
                    && let HostBlockDecision::Blocked(reason) = state
                        .host_blocked_with_local_binding(
                            &request.host,
                            request.port,
                            /*allow_local_binding*/ None,
                            HostAuthorization::Approved,
                        )
                        .await?
                {
                    decider_decision = NetworkDecision::deny_with_source(
                        reason.as_str(),
                        NetworkDecisionSource::BaselinePolicy,
                    );
                }
                let policy_override = matches!(decider_decision, NetworkDecision::Allow);
                (decider_decision, policy_override)
            } else {
                (
                    NetworkDecision::deny_with_source(
                        HostBlockReason::NotAllowed.as_str(),
                        NetworkDecisionSource::BaselinePolicy,
                    ),
                    false,
                )
            }
        }
        HostBlockDecision::Blocked(reason) => (
            NetworkDecision::deny_with_source(
                reason.as_str(),
                NetworkDecisionSource::BaselinePolicy,
            ),
            false,
        ),
    };

    let (policy_decision, source, reason) = match &decision {
        NetworkDecision::Allow => (
            POLICY_DECISION_ALLOW,
            if policy_override {
                NetworkDecisionSource::Decider
            } else {
                NetworkDecisionSource::BaselinePolicy
            },
            if policy_override {
                HostBlockReason::NotAllowed.as_str()
            } else {
                POLICY_REASON_ALLOW
            },
        ),
        NetworkDecision::Deny {
            reason,
            source,
            decision,
        } => (decision.as_str(), *source, reason.as_str()),
    };

    emit_policy_audit_event(
        state,
        PolicyAuditEventArgs {
            scope: POLICY_SCOPE_DOMAIN,
            decision: policy_decision,
            source: source.as_str(),
            reason,
            protocol: request.protocol,
            server_address: request.host.as_str(),
            server_port: request.port,
            method: request.method.as_deref(),
            client_addr: request.client_addr.as_deref(),
            execution_id: execution_id.as_deref(),
            policy_override,
        },
    );

    Ok(decision)
}

fn map_decider_decision(decision: NetworkDecision) -> NetworkDecision {
    match decision {
        NetworkDecision::Allow => NetworkDecision::Allow,
        NetworkDecision::Deny {
            reason, decision, ..
        } => NetworkDecision::Deny {
            reason,
            source: NetworkDecisionSource::Decider,
            decision,
        },
    }
}
