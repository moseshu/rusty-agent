// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Managed network proxy policy contracts ported from Codex.
#![allow(missing_docs)]
#![allow(
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::match_same_arms,
    clippy::must_use_candidate,
    reason = "Preserve the upstream configuration and policy surface and branch order."
)]
#[expect(
    dead_code,
    reason = "Policy helpers await their proxy runtime consumers."
)]
mod authorization_path;
mod config;
mod credential_broker;
#[expect(
    dead_code,
    reason = "Policy helpers await their proxy runtime consumers."
)]
mod domain_matcher;
mod environment_policy;
mod mitm_validation;
mod network_policy;
#[expect(
    dead_code,
    reason = "Policy helpers await their proxy runtime consumers."
)]
mod policy;
#[expect(
    dead_code,
    reason = "Policy helpers await their proxy runtime consumers."
)]
mod reasons;
mod request_cancellation;
#[expect(
    dead_code,
    reason = "Policy helpers await their proxy runtime consumers."
)]
mod request_disconnect;
mod socket_path;
mod state;
pub use config::{
    CredentialProviderConfig, MitmHookConfig, NetworkDomainPermission,
    NetworkDomainPermissionEntry, NetworkDomainPermissions, NetworkMitmCaConfig, NetworkMode,
    NetworkProxyConfig, NetworkUnixSocketPermission, NetworkUnixSocketPermissions,
    host_and_port_from_network_addr, managed_proxy_ports,
};
pub use credential_broker::CredentialBrokerContext;
pub use environment_policy::EnvironmentNetworkPolicy;
pub use network_policy::{
    NetworkDecision, NetworkDecisionSource, NetworkPolicyAuditEvent, NetworkPolicyAuditObserver,
    NetworkPolicyDecider, NetworkPolicyDeciderFuture, NetworkPolicyDecision, NetworkPolicyRequest,
    NetworkPolicyRequestArgs, NetworkProtocol,
};
pub use policy::{is_non_public_ip, normalize_host};
pub use request_cancellation::{NetworkRequestCancellation, NetworkRequestCancellationReason};
pub use request_disconnect::NetworkRequestDisconnect;
pub use state::{
    NetworkProxyConstraintError, NetworkProxyConstraints, validate_policy_against_constraints,
};

pub use ra_paths::Platform;
