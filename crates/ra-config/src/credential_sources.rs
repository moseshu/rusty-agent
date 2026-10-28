// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Credential-family environment keys protected from project configuration.
#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
const PROVIDER_ENV_KEYS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
];
pub(crate) fn is_credential_broker_provider_env_key(key: &str) -> bool {
    PROVIDER_ENV_KEYS.iter().any(|candidate| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case(candidate)
        } else {
            key == *candidate
        }
    })
}
