// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's network policy decisions: the proxy's decision payload, the approval context an "ask"
//! becomes, the message a denial shows, and the exec policy rule an approved amendment saves.
//!
//! # Differences from Codex
//!
//! - `NetworkPolicyDecisionPayload` is Codex's `protocol::network_policy` record. Its only other
//!   consumer there is the denied-sandbox error, which is not ported, so it lives with its
//!   consumer here rather than adding a proxy dependency to the protocol crate.
//! - Codex's core `NetworkApprovalProtocol` is `snake_case` and accepts the proxy's `https_connect`
//!   and `http-connect` spellings; the local protocol type is the app server's camelCase one, so
//!   the payload reads the proxy's spellings with its own deserializer into that type.
//! - The protocol enums are `#[non_exhaustive]` wire types of this workspace; a variant added
//!   there has to be given its rule here, so the matches end in `unreachable!` rather than a
//!   default that would save a wrong rule.
#![allow(
    missing_docs,
    reason = "Preserve upstream network policy decision records reachable only through the hidden test bridge."
)]

use ra_execpolicy::{
    Decision as ExecPolicyDecision, NetworkRuleProtocol as ExecPolicyNetworkRuleProtocol,
};
use ra_network_proxy::{BlockedRequest, NetworkDecisionSource, NetworkPolicyDecision};
use ra_protocol::server::protocol::{
    NetworkApprovalContext, NetworkApprovalProtocol, NetworkPolicyAmendment,
    NetworkPolicyRuleAction,
};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    not(feature = "test-api"),
    allow(
        unreachable_pub,
        reason = "Reachable outside the crate only through the hidden test bridge."
    )
)]
pub struct NetworkPolicyDecisionPayload {
    pub decision: NetworkPolicyDecision,
    pub source: NetworkDecisionSource,
    #[serde(default, deserialize_with = "deserialize_proxy_protocol")]
    pub protocol: Option<NetworkApprovalProtocol>,
    pub host: Option<String>,
    pub reason: Option<String>,
    pub port: Option<u16>,
}

impl NetworkPolicyDecisionPayload {
    #[must_use]
    #[cfg_attr(
        not(feature = "test-api"),
        allow(
            unreachable_pub,
            reason = "Reachable outside the crate only through the hidden test bridge."
        )
    )]
    pub fn is_ask_from_decider(&self) -> bool {
        self.decision == NetworkPolicyDecision::Ask && self.source == NetworkDecisionSource::Decider
    }
}

/// Codex's core `NetworkApprovalProtocol` spellings, including the proxy's aliases for HTTPS.
fn deserialize_proxy_protocol<'de, D>(
    deserializer: D,
) -> Result<Option<NetworkApprovalProtocol>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let protocol = match value.as_str() {
        "http" => NetworkApprovalProtocol::Http,
        "https" | "https_connect" | "http-connect" => NetworkApprovalProtocol::Https,
        "socks5_tcp" => NetworkApprovalProtocol::Socks5Tcp,
        "socks5_udp" => NetworkApprovalProtocol::Socks5Udp,
        other => {
            return Err(serde::de::Error::unknown_variant(
                other,
                &[
                    "http",
                    "https",
                    "https_connect",
                    "http-connect",
                    "socks5_tcp",
                    "socks5_udp",
                ],
            ));
        }
    };
    Ok(Some(protocol))
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "test-api"),
    allow(
        unreachable_pub,
        reason = "Reachable outside the crate only through the hidden test bridge."
    )
)]
pub struct ExecPolicyNetworkRuleAmendment {
    pub protocol: ExecPolicyNetworkRuleProtocol,
    pub decision: ExecPolicyDecision,
    pub justification: String,
}

fn parse_network_policy_decision(value: &str) -> Option<NetworkPolicyDecision> {
    match value {
        "deny" => Some(NetworkPolicyDecision::Deny),
        "ask" => Some(NetworkPolicyDecision::Ask),
        _ => None,
    }
}

#[cfg_attr(
    not(feature = "test-api"),
    expect(
        dead_code,
        reason = "Codex itself calls it only from its tests; the payload is otherwise read by the denied-sandbox error, which is not ported."
    )
)]
pub(crate) fn network_approval_context_from_payload(
    payload: &NetworkPolicyDecisionPayload,
) -> Option<NetworkApprovalContext> {
    if !payload.is_ask_from_decider() {
        return None;
    }

    let protocol = payload.protocol?;

    let host = payload.host.as_deref()?.trim();
    if host.is_empty() {
        return None;
    }

    Some(NetworkApprovalContext::new(host, protocol))
}

pub(crate) fn denied_network_policy_message(blocked: &BlockedRequest) -> Option<String> {
    let decision = blocked
        .decision
        .as_deref()
        .and_then(parse_network_policy_decision);
    if decision != Some(NetworkPolicyDecision::Deny) {
        return None;
    }

    let host = blocked.host.trim();
    if host.is_empty() {
        return Some("Network access was blocked by policy.".to_string());
    }

    let detail = match blocked.reason.as_str() {
        "denied" => "domain is explicitly denied by policy and cannot be approved from this prompt",
        "not_allowed" => "domain is not on the allowlist for the current sandbox mode",
        "not_allowed_local" => "local/private network addresses are blocked by the sandbox policy",
        "method_not_allowed" => "request method is blocked by the current network mode",
        "proxy_disabled" => "network proxy is disabled",
        _ => "request is blocked by network policy",
    };

    Some(format!(
        "Network access to \"{host}\" was blocked: {detail}."
    ))
}

pub(crate) fn execpolicy_network_rule_amendment(
    amendment: &NetworkPolicyAmendment,
    network_approval_context: &NetworkApprovalContext,
    host: &str,
) -> ExecPolicyNetworkRuleAmendment {
    let protocol = match network_approval_context.protocol() {
        NetworkApprovalProtocol::Http => ExecPolicyNetworkRuleProtocol::Http,
        NetworkApprovalProtocol::Https => ExecPolicyNetworkRuleProtocol::Https,
        NetworkApprovalProtocol::Socks5Tcp => ExecPolicyNetworkRuleProtocol::Socks5Tcp,
        NetworkApprovalProtocol::Socks5Udp => ExecPolicyNetworkRuleProtocol::Socks5Udp,
        protocol => unreachable!("network approval protocol {protocol:?} has no exec policy rule"),
    };
    let (decision, action_verb) = match amendment.action() {
        NetworkPolicyRuleAction::Allow => (ExecPolicyDecision::Allow, "Allow"),
        NetworkPolicyRuleAction::Deny => (ExecPolicyDecision::Forbidden, "Deny"),
        action => unreachable!("network policy rule action {action:?} has no exec policy decision"),
    };
    let protocol_label = match network_approval_context.protocol() {
        NetworkApprovalProtocol::Http => "http",
        NetworkApprovalProtocol::Https => "https_connect",
        NetworkApprovalProtocol::Socks5Tcp => "socks5_tcp",
        NetworkApprovalProtocol::Socks5Udp => "socks5_udp",
        protocol => unreachable!("network approval protocol {protocol:?} has no rule label"),
    };
    let justification = format!("{action_verb} {protocol_label} access to {host}");

    ExecPolicyNetworkRuleAmendment {
        protocol,
        decision,
        justification,
    }
}

/// Codex's crate-private network policy decisions for the external integration-test workspace.
#[cfg(feature = "test-api")]
pub mod test_api {
    #![allow(missing_docs, reason = "Hidden bridges for source tests.")]
    pub use super::{ExecPolicyNetworkRuleAmendment, NetworkPolicyDecisionPayload};
    use ra_network_proxy::BlockedRequest;
    use ra_protocol::server::protocol::{NetworkApprovalContext, NetworkPolicyAmendment};

    #[must_use]
    pub fn network_approval_context_from_payload(
        payload: &NetworkPolicyDecisionPayload,
    ) -> Option<NetworkApprovalContext> {
        super::network_approval_context_from_payload(payload)
    }

    #[must_use]
    pub fn denied_network_policy_message(blocked: &BlockedRequest) -> Option<String> {
        super::denied_network_policy_message(blocked)
    }

    #[must_use]
    pub fn execpolicy_network_rule_amendment(
        amendment: &NetworkPolicyAmendment,
        network_approval_context: &NetworkApprovalContext,
        host: &str,
    ) -> ExecPolicyNetworkRuleAmendment {
        super::execpolicy_network_rule_amendment(amendment, network_approval_context, host)
    }
}
