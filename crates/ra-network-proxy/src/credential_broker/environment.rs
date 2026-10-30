// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use super::env_key_matches;
use super::env_value;
use crate::NetworkProxyConfig;
use std::collections::HashMap;

/// Trusted destination hints retained separately from a child's visible environment.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CredentialBrokerContext(HashMap<String, String>);
impl std::fmt::Debug for CredentialBrokerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialBrokerContext(<redacted>)")
    }
}
impl CredentialBrokerContext {
    pub(crate) fn capture(
        config: &NetworkProxyConfig,
        overrides: &HashMap<String, String>,
    ) -> Self {
        Self(
            ["GH_HOST", "OPENAI_BASE_URL"]
                .into_iter()
                .map(str::to_string)
                .chain(
                    config
                        .credential_providers
                        .values()
                        .filter_map(|provider| provider.url_prefix_from_env.clone()),
                )
                .filter_map(|key| {
                    env_value(overrides, &key)
                        .map(str::to_string)
                        .or_else(|| std::env::var(&key).ok())
                        .map(|value| (key, value))
                })
                .collect(),
        )
    }
}

/// Returns whether an environment key belongs to a supported credential provider.
pub(crate) fn is_credential_broker_provider_env_key(key: &str) -> bool {
    [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_HOST",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
    ]
    .iter()
    .any(|candidate| env_key_matches(key, candidate))
}
