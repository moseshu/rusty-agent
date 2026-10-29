// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::implicit_hasher,
    reason = "Preserve the upstream configuration surface, branch order and diagnostic spelling."
)]
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
use ra_config::{ModelProviderAwsAuthInfo, ModelProviderInfo, WireApi};
use std::collections::HashMap;
const OPENAI_PROVIDER_NAME: &str = "OpenAI";
pub const OPENAI_PROVIDER_ID: &str = "openai";
const AMAZON_BEDROCK_PROVIDER_NAME: &str = "Amazon Bedrock";
pub const AMAZON_BEDROCK_PROVIDER_ID: &str = "amazon-bedrock";
const AMAZON_BEDROCK_RUNTIME_PROVIDER_NAME: &str = "Amazon Bedrock Runtime";
pub const AMAZON_BEDROCK_RUNTIME_PROVIDER_ID: &str = "amazon-bedrock-runtime";
const AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_HEADER: &str = "x-amzn-mantle-client-agent";
const AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_VALUE: &str = "codex";
pub const LEGACY_OLLAMA_CHAT_PROVIDER_ID: &str = "ollama-chat";
pub const OLLAMA_CHAT_PROVIDER_REMOVED_ERROR: &str = "`ollama-chat` is no longer supported.\nHow to fix: replace `ollama-chat` with `ollama` in `model_provider`, `oss_provider`, or `--local-provider`.\nMore info: https://github.com/openai/codex/discussions/7782";
trait ProviderDefaults {
    fn create_openai_provider(base_url: Option<String>) -> ModelProviderInfo;
    fn create_amazon_bedrock_provider(aws: Option<ModelProviderAwsAuthInfo>) -> ModelProviderInfo;
    fn create_amazon_bedrock_runtime_provider(
        aws: Option<ModelProviderAwsAuthInfo>,
    ) -> ModelProviderInfo;
}
impl ProviderDefaults for ModelProviderInfo {
    fn create_openai_provider(base_url: Option<String>) -> ModelProviderInfo {
        ModelProviderInfo {
            name: OPENAI_PROVIDER_NAME.into(),
            base_url,
            model_catalog_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            gateway_oauth: None,
            aws: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(
                [("version".to_string(), env!("CARGO_PKG_VERSION").into())]
                    .into_iter()
                    .collect(),
            ),
            env_http_headers: Some(
                [
                    (
                        "OpenAI-Organization".to_string(),
                        "OPENAI_ORGANIZATION".to_string(),
                    ),
                    ("OpenAI-Project".to_string(), "OPENAI_PROJECT".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            // Use global defaults for retry/timeout unless overridden in config.toml.
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            supports_websockets: true,
            supports_standalone_web_search: true,
            capabilities: None,
        }
    }

    fn create_amazon_bedrock_provider(aws: Option<ModelProviderAwsAuthInfo>) -> ModelProviderInfo {
        ModelProviderInfo {
            name: AMAZON_BEDROCK_PROVIDER_NAME.into(),
            // The runtime provider derives the regional Mantle endpoint when
            // this is unset. A configured value is therefore unambiguously an
            // endpoint override.
            base_url: None,
            model_catalog_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            gateway_oauth: None,
            aws: Some(aws.unwrap_or(ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
                credential_export: None,
                auth_refresh: None,
            })),
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(HashMap::from([(
                AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_HEADER.to_string(),
                AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_VALUE.into(),
            )])),
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            supports_websockets: false,
            supports_standalone_web_search: false,
            capabilities: None,
        }
    }

    fn create_amazon_bedrock_runtime_provider(
        aws: Option<ModelProviderAwsAuthInfo>,
    ) -> ModelProviderInfo {
        let mut provider = Self::create_amazon_bedrock_provider(aws);
        provider.name = AMAZON_BEDROCK_RUNTIME_PROVIDER_NAME.into();
        provider
    }
}
pub const DEFAULT_LMSTUDIO_PORT: u16 = 1234;
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

pub const LMSTUDIO_OSS_PROVIDER_ID: &str = "lmstudio";
pub const OLLAMA_OSS_PROVIDER_ID: &str = "ollama";

/// Built-in default provider list.
#[must_use]
pub fn built_in_model_providers(
    openai_base_url: Option<String>,
) -> HashMap<String, ModelProviderInfo> {
    use ModelProviderInfo as P;
    let openai_provider = P::create_openai_provider(openai_base_url);
    let amazon_bedrock_provider = P::create_amazon_bedrock_provider(/*aws*/ None);
    let amazon_bedrock_runtime_provider =
        P::create_amazon_bedrock_runtime_provider(/*aws*/ None);

    // We do not want to be in the business of adjucating which third-party
    // providers are bundled with Codex CLI, so we only include the OpenAI and
    // open source ("oss") providers by default. Users are encouraged to add to
    // `model_providers` in config.toml to add their own providers.
    [
        (OPENAI_PROVIDER_ID, openai_provider),
        (AMAZON_BEDROCK_PROVIDER_ID, amazon_bedrock_provider),
        (
            AMAZON_BEDROCK_RUNTIME_PROVIDER_ID,
            amazon_bedrock_runtime_provider,
        ),
        (
            OLLAMA_OSS_PROVIDER_ID,
            create_oss_provider(DEFAULT_OLLAMA_PORT, WireApi::Responses),
        ),
        (
            LMSTUDIO_OSS_PROVIDER_ID,
            create_oss_provider(DEFAULT_LMSTUDIO_PORT, WireApi::Responses),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Merge configured providers into the built-in provider catalog.
///
/// Configured providers extend the built-in set. Built-in providers are not
/// generally overridable, but built-in Amazon Bedrock providers allow the user
/// to customize their endpoint, authentication, headers, and AWS settings.
pub fn merge_configured_model_providers(
    mut model_providers: HashMap<String, ModelProviderInfo>,
    configured_model_providers: HashMap<String, ModelProviderInfo>,
) -> Result<HashMap<String, ModelProviderInfo>, String> {
    for (key, mut provider) in configured_model_providers {
        if matches!(
            key.as_str(),
            AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
        ) {
            provider
                .validate_bedrock_override()
                .map_err(|message| format!("model_providers.{key} {message}"))?;
            let base_url_override = provider.base_url.take();
            let auth_override = provider.auth.take();
            let aws_override = provider.aws.take();
            let http_headers_override = provider.http_headers.take();
            if let Some(built_in_provider) = model_providers.get_mut(&key) {
                built_in_provider.base_url = base_url_override;
                built_in_provider.auth = auth_override;
                if let Some(aws_override) = aws_override {
                    built_in_provider.aws = Some(aws_override);
                }
                if let Some(http_headers_override) = http_headers_override {
                    built_in_provider
                        .http_headers
                        .get_or_insert_default()
                        .extend(http_headers_override);
                }
            }
        } else {
            model_providers.entry(key).or_insert(provider);
        }
    }

    Ok(model_providers)
}

#[must_use]
pub fn create_oss_provider(default_provider_port: u16, wire_api: WireApi) -> ModelProviderInfo {
    // These RA_OSS_ environment variables are experimental: we may
    // switch to reading values from config.toml instead.
    let default_codex_oss_base_url = format!(
        "http://localhost:{codex_oss_port}/v1",
        codex_oss_port = std::env::var("RA_OSS_PORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default_provider_port)
    );

    let codex_oss_base_url = std::env::var("RA_OSS_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(default_codex_oss_base_url);
    create_oss_provider_with_base_url(&codex_oss_base_url, wire_api)
}

#[must_use]
pub fn create_oss_provider_with_base_url(base_url: &str, wire_api: WireApi) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "gpt-oss".into(),
        base_url: Some(base_url.into()),
        model_catalog_url: None,
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        gateway_oauth: None,
        aws: None,
        wire_api,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        supports_websockets: false,
        supports_standalone_web_search: false,
        capabilities: None,
    }
}
