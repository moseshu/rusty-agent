// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Network configuration declarations; proxy execution belongs to its runtime adapter.
#![allow(
    missing_docs,
    reason = "Preserve source network configuration records."
)]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MitmHookConfig {
    pub host: String,
    #[serde(rename = "match", default)]
    pub matcher: MitmHookMatchConfig,
    #[serde(default)]
    pub actions: MitmHookActionsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MitmHookMatchConfig {
    pub methods: Vec<String>,
    pub path_prefixes: Vec<String>,
    pub query: BTreeMap<String, Vec<String>>,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Option<MitmHookBodyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MitmHookActionsConfig {
    pub strip_request_headers: Vec<String>,
    pub inject_request_headers: Vec<InjectedHeaderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct InjectedHeaderConfig {
    pub name: String,
    pub secret_env_var: Option<String>,
    pub secret_file: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct MitmHookBodyConfig(pub serde_json::Value);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Limited (read-only) access: only GET/HEAD/OPTIONS are allowed for HTTP. HTTPS CONNECT is
    /// blocked unless MITM is enabled so the proxy can enforce method policy on inner requests.
    /// SOCKS5 UDP and non-HTTPS SOCKS5 TCP remain blocked in limited mode.
    Limited,
    /// Full network access: all HTTP methods are allowed. HTTPS CONNECTs are tunneled directly.
    /// MITM hooks do not currently make full mode enter MITM.
    #[default]
    Full,
}

impl NetworkMode {
    pub fn allows_method(self, method: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Limited => matches!(method, "GET" | "HEAD" | "OPTIONS"),
        }
    }
}

/// Declarative description of an environment-backed credential family.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialProviderConfig {
    pub env: Vec<String>,
    pub patterns: Vec<String>,
    /// URL prefixes authorized for injection. Bare loopback hosts imply HTTP; other bare hosts
    /// imply HTTPS.
    pub url_prefixes: Vec<String>,
    /// Environment variable containing an additional URL prefix or hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_prefix_from_env: Option<String>,
    pub auth: Vec<CredentialAuthMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

/// Authentication formats supported by declarative credential providers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAuthMethod {
    Bearer,
    Token,
    Basic,
    Header,
}
