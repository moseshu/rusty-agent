// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use crate::reasons::REASON_POLICY_DENIED;
use crate::{NetworkRequestCancellation, NetworkRequestDisconnect};
use std::{future::Future, pin::Pin, sync::Arc};
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
