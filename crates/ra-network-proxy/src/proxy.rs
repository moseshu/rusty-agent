// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::implicit_hasher,
    reason = "Preserve the upstream implementation and callback contracts."
)]
use crate::attribution::PROXY_ATTRIBUTION_TOKEN_ENV_KEY;
use crate::credential_broker::{BROKERED_CREDENTIALS_ENV_KEY, CREDENTIAL_BROKER_ACTIVE_ENV_KEY};
pub const PROXY_URL_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "ALL_PROXY",
    "FTP_PROXY",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "NPM_CONFIG_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
    "PIP_PROXY",
    "DOCKER_HTTP_PROXY",
    "DOCKER_HTTPS_PROXY",
];

pub const ALL_PROXY_ENV_KEYS: &[&str] = &["ALL_PROXY", "all_proxy"];
pub const PROXY_ACTIVE_ENV_KEY: &str = "CODEX_NETWORK_PROXY_ACTIVE";
pub const ALLOW_LOCAL_BINDING_ENV_KEY: &str = "CODEX_NETWORK_ALLOW_LOCAL_BINDING";
// Internal wire format shared with windows-sandbox-rs/src/setup.rs. The value is a
// comma-separated, sorted list of non-zero loopback proxy ports used only when computing the
// Windows offline sandbox setup marker.
#[cfg(target_os = "windows")]
const WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY: &str = "CODEX_WINDOWS_SANDBOX_PROXY_PORTS";
const ELECTRON_GET_USE_PROXY_ENV_KEY: &str = "ELECTRON_GET_USE_PROXY";
const NODE_USE_ENV_PROXY_ENV_KEY: &str = "NODE_USE_ENV_PROXY";
#[cfg(target_os = "macos")]
const GIT_SSH_COMMAND_ENV_KEY: &str = "GIT_SSH_COMMAND";
pub const PROXY_ENV_KEYS: &[&str] = &[
    PROXY_ACTIVE_ENV_KEY,
    CREDENTIAL_BROKER_ACTIVE_ENV_KEY,
    BROKERED_CREDENTIALS_ENV_KEY,
    ALLOW_LOCAL_BINDING_ENV_KEY,
    #[cfg(target_os = "windows")]
    WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY,
    PROXY_ATTRIBUTION_TOKEN_ENV_KEY,
    ELECTRON_GET_USE_PROXY_ENV_KEY,
    NODE_USE_ENV_PROXY_ENV_KEY,
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "npm_config_http_proxy",
    "npm_config_https_proxy",
    "npm_config_proxy",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "NPM_CONFIG_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
    "PIP_PROXY",
    "DOCKER_HTTP_PROXY",
    "DOCKER_HTTPS_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "ws_proxy",
    "wss_proxy",
    "NO_PROXY",
    "no_proxy",
    "npm_config_noproxy",
    "NPM_CONFIG_NOPROXY",
    "YARN_NO_PROXY",
    "BUNDLE_NO_PROXY",
    "ALL_PROXY",
    "all_proxy",
    "FTP_PROXY",
    "ftp_proxy",
];

pub fn is_managed_proxy_env_var(key: &str, value: &str) -> bool {
    if PROXY_ENV_KEYS.contains(&key) {
        return true;
    }
    if crate::certs::CUSTOM_CA_ENV_KEYS.contains(&key) {
        return crate::certs::is_managed_mitm_ca_trust_bundle_path(value);
    }
    #[cfg(target_os = "macos")]
    {
        key == PROXY_GIT_SSH_COMMAND_ENV_KEY
            && value.starts_with(CODEX_PROXY_GIT_SSH_COMMAND_MARKER)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
const PROXY_GIT_SSH_COMMAND_ENV_KEY: &str = GIT_SSH_COMMAND_ENV_KEY;
#[cfg(target_os = "macos")]
const CODEX_PROXY_GIT_SSH_COMMAND_MARKER: &str = "CODEX_PROXY_GIT_SSH_COMMAND=1 ";

use std::collections::HashMap;
pub fn strip_managed_proxy_env(env: &mut HashMap<String, String>) {
    let brokered_credential_dummy_env_keys =
        crate::credential_broker::environment::marked_credential_dummy_env_keys(env);
    env.retain(|key, value| {
        !brokered_credential_dummy_env_keys.contains(key) && !is_managed_proxy_env_var(key, value)
    });
}
