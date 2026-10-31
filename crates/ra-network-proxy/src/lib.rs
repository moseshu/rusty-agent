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
mod authorization_path;
mod config;
mod credential_broker;
mod domain_matcher;
mod environment_policy;
mod mitm_hook;
mod network_policy;
mod policy;
mod reasons;
mod request_cancellation;
mod request_disconnect;
mod socket_path;
mod state;
pub use config::{
    NetworkDomainPermission, NetworkDomainPermissionEntry, NetworkDomainPermissions,
    NetworkMitmCaConfig, NetworkMode, NetworkProxyConfig, NetworkUnixSocketPermission,
    NetworkUnixSocketPermissions, host_and_port_from_network_addr, managed_proxy_ports,
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

mod attribution;
mod brokered_tunnel;
mod certs;
mod connect_policy;
mod connection_lifecycle;
mod http_proxy;
mod mitm;
mod native_certs;
mod process_log_metadata;
mod proxy;
mod remote_config;
mod responses;
mod runtime;
mod socks5;
mod upstream;

#[cfg(target_os = "macos")]
mod system_dns;
#[cfg(target_os = "windows")]
mod windows_proxy_ingress;
#[cfg(target_os = "windows")]
#[allow(
    unsafe_code,
    reason = "Windows IP Helper and token FFI ported from Codex."
)]
mod windows_tcp_attribution;
pub use attribution::{PROXY_ATTRIBUTION_TOKEN_ENV_KEY, write_attribution_frame};
pub use certs::{CUSTOM_CA_ENV_KEYS, is_managed_mitm_ca_trust_bundle_path};
pub use credential_broker::{
    CREDENTIAL_BROKER_ACTIVE_ENV_KEY, brokered_credential_binding_env_keys,
    brokered_credential_dummy_env_keys, brokered_credential_env_keys,
    brokered_credential_marker_env_keys, brokered_credential_value_env_keys,
    credential_broker_provider_context_env_keys, credential_broker_provider_sources_allowed,
    is_credential_broker_provider_env_key,
};
pub use credential_broker::{
    CredentialAuthMethod, CredentialBrokerEnvironment, CredentialProviderConfig,
};
pub use mitm_hook::{
    InjectedHeaderConfig, MitmHookActionsConfig, MitmHookBodyConfig, MitmHookConfig,
    MitmHookMatchConfig,
};
pub use process_log_metadata::{ExecutorLogIdentity, NetworkProxyProcessLogMetadata};
#[cfg(target_os = "macos")]
pub use proxy::CODEX_PROXY_GIT_SSH_COMMAND_MARKER;
#[cfg(target_os = "macos")]
pub use proxy::PROXY_GIT_SSH_COMMAND_ENV_KEY;
pub use proxy::{
    ALL_PROXY_ENV_KEYS, ALLOW_LOCAL_BINDING_ENV_KEY, DEFAULT_NO_PROXY_VALUE,
    ManagedNetworkSandboxContext, ManagedProxyRouting, NO_PROXY_ENV_KEYS, NetworkProxy,
    NetworkProxyBuilder, NetworkProxyHandle, PROXY_ACTIVE_ENV_KEY, PROXY_ENV_KEYS,
    PROXY_URL_ENV_KEYS, PreparedManagedNetwork, has_proxy_url_env_vars, is_managed_proxy_env_var,
    proxy_url_env_value, strip_managed_proxy_env,
};
pub use remote_config::{RemoteNetworkProxyConfig, RemoteNetworkProxyLaunchConfig};
pub use runtime::{
    BlockedRequest, BlockedRequestArgs, BlockedRequestObserver, BlockedRequestObserverFuture,
    ConfigReloader, ConfigReloaderFuture, ConfigState, LocalBindingPolicy,
    NetworkProxyAuditMetadata, NetworkProxyState,
};
pub use state::{PartialNetworkProxyConfig, build_config_state};

pub use credential_broker::CredentialProviderConfigExt;
