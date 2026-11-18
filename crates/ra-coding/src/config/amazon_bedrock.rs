// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Amazon Bedrock's OpenAI-compatible Responses endpoints, Mantle and Runtime.
//!
//! The endpoint, region, and credentials are resolved when a thread's model runtime is built;
//! each request is then authenticated by [`auth::BedrockSigner`].

mod auth;
mod auth_refresh;
mod catalog;
mod credential_export;
mod error;
mod mantle;
mod runtime;
mod shared_state;

use std::sync::Arc;

use ra_aws_auth::AwsAuthContext;
use ra_config::{ModelProviderAwsAuthInfo, ModelProviderInfo};
use ra_core::error::{Error, Result};

use self::auth::{AwsRecovery, BedrockAuthSource, BedrockSigner, SigningMethod};
use self::shared_state::process_shared_state;
use super::{Config, ModelsResponse, model_auth::CommandBearerToken};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BedrockEndpoint {
    Mantle,
    Runtime,
}

impl BedrockEndpoint {
    fn of(provider: &ModelProviderInfo) -> Self {
        if provider.is_amazon_bedrock_runtime() {
            Self::Runtime
        } else {
            Self::Mantle
        }
    }

    fn aws_auth_config(self, aws: &ModelProviderAwsAuthInfo) -> ra_aws_auth::AwsAuthConfig {
        match self {
            Self::Mantle => mantle::aws_auth_config(aws),
            Self::Runtime => runtime::aws_auth_config(aws),
        }
    }

    fn base_url(self, region: &str) -> Result<String> {
        match self {
            Self::Mantle => mantle::base_url(region),
            Self::Runtime => Ok(runtime::base_url(region)),
        }
    }
}

/// Resolved Bedrock transport for one thread.
pub(super) struct PreparedBedrock {
    pub(super) base_url: String,
    pub(super) signer: Arc<BedrockSigner>,
}

/// Resolves the endpoint and credentials with Codex's source precedence.
///
/// `command` is the provider's shared command bearer, when `auth` is configured.
pub(super) async fn prepare(
    config: &Config,
    command: Option<Arc<CommandBearerToken>>,
) -> Result<PreparedBedrock> {
    let info = &config.model_provider;
    let endpoint = BedrockEndpoint::of(info);
    let aws = info.aws.clone().unwrap_or(ModelProviderAwsAuthInfo {
        profile: None,
        region: None,
        credential_export: None,
        auth_refresh: None,
    });
    let source = auth::auth_source(info, std::env::var);
    let base_factory = config.http_client_factory();
    let factory = base_factory
        .clone()
        .with_network_policy(base_factory.network_policy().clone().for_current_account());
    let (method, region) = resolve_method(source, info, &aws, endpoint, command, &factory).await?;
    let base_url = match (&info.base_url, region) {
        (Some(base_url), _) => base_url.clone(),
        (None, Some(region)) => endpoint.base_url(&region)?,
        (None, None) => return Err(Error::config("Amazon Bedrock region is unavailable")),
    };
    // Matches Codex: only SDK-signed sources use `aws login` and credential-export recovery.
    let export = (source == BedrockAuthSource::CredentialExport)
        .then(|| process_shared_state().aws_credential_export(&aws))
        .flatten();
    let refresh = matches!(
        source,
        BedrockAuthSource::CredentialExport
            | BedrockAuthSource::ConfiguredAwsProfile
            | BedrockAuthSource::AwsSdk
    )
    .then(|| process_shared_state().aws_auth_recovery(&aws))
    .flatten();
    let recovery = (export.is_some() || refresh.is_some()).then(|| AwsRecovery {
        export,
        refresh,
        network_policy: factory.network_policy().clone(),
    });
    Ok(PreparedBedrock {
        base_url,
        signer: Arc::new(BedrockSigner {
            method,
            endpoint,
            recovery,
        }),
    })
}

/// Resolves the signing method and, when the endpoint is derived from it, the region.
async fn resolve_method(
    source: BedrockAuthSource,
    info: &ModelProviderInfo,
    aws: &ModelProviderAwsAuthInfo,
    endpoint: BedrockEndpoint,
    command: Option<Arc<CommandBearerToken>>,
    factory: &ra_http::HttpClientFactory,
) -> Result<(SigningMethod, Option<String>)> {
    let aws_config = endpoint.aws_auth_config(aws);
    let load_error = |error: ra_aws_auth::AwsAuthError| {
        Error::config(format!("failed to resolve Amazon Bedrock auth: {error}"))
    };
    let result = match source {
        BedrockAuthSource::CommandBearerToken => {
            let command = command.ok_or_else(|| {
                Error::config("Amazon Bedrock command authentication is not configured")
            })?;
            let region = if info.base_url.is_none() {
                let context = AwsAuthContext::load(aws_config, factory.clone())
                    .await
                    .map_err(load_error)?;
                Some(context.region().to_owned())
            } else {
                None
            };
            (SigningMethod::Command(command), region)
        }
        BedrockAuthSource::EnvBearerToken => {
            let token =
                auth::non_empty_env_var_from(auth::AWS_BEARER_TOKEN_BEDROCK_ENV_VAR, std::env::var)
                    .ok_or_else(|| {
                        Error::config(
                            "selected `AWS_BEARER_TOKEN_BEDROCK` credential is no longer available",
                        )
                    })?;
            let region = auth::bearer_token_region(aws, std::env::var)?;
            (SigningMethod::Bearer(token), Some(region))
        }
        BedrockAuthSource::CredentialExport
        | BedrockAuthSource::ConfiguredAwsProfile
        | BedrockAuthSource::EnvAwsCredentials
        | BedrockAuthSource::AwsSdk => {
            let exporter = if source == BedrockAuthSource::CredentialExport {
                Some(
                    process_shared_state()
                        .aws_credential_export(aws)
                        .ok_or_else(|| {
                            Error::config(
                                "selected Amazon Bedrock credential exporter is no longer configured",
                            )
                        })?,
                )
            } else {
                None
            };
            let context = load_sigv4_context(
                source,
                aws_config.clone(),
                exporter.clone(),
                factory.clone(),
            )
            .await?;
            let region = context.region().to_owned();
            let factory = factory.clone();
            let loader: auth::ContextLoader = Arc::new(move || {
                Box::pin(load_sigv4_context(
                    source,
                    aws_config.clone(),
                    exporter.clone(),
                    factory.clone(),
                ))
            });
            (
                SigningMethod::SigV4(auth::ReloadableContext::new(context, loader)),
                Some(region),
            )
        }
    };
    Ok(result)
}

/// Loads the `SigV4` context for an SDK-signed source, as Codex's `resolve_auth_method` does.
async fn load_sigv4_context(
    source: BedrockAuthSource,
    config: ra_aws_auth::AwsAuthConfig,
    exporter: Option<Arc<credential_export::AwsCredentialExport>>,
    factory: ra_http::HttpClientFactory,
) -> Result<AwsAuthContext> {
    let loaded = match (source, exporter) {
        (BedrockAuthSource::CredentialExport, Some(exporter)) => {
            AwsAuthContext::load_with_credentials_provider(config, exporter, factory).await
        }
        (BedrockAuthSource::ConfiguredAwsProfile, _) => {
            AwsAuthContext::load_profile(config, factory).await
        }
        _ => AwsAuthContext::load(config, factory).await,
    };
    loaded.map_err(|error| Error::config(format!("failed to resolve Amazon Bedrock auth: {error}")))
}

/// Bedrock's default catalog, or the configured catalog normalized for Bedrock's wire format.
///
/// `base_url` is the endpoint the transport resolved, so a region-derived `GovCloud` endpoint
/// selects the `GovCloud` catalog even when no `base_url` is configured.
pub(super) fn model_catalog(
    info: &ModelProviderInfo,
    base_url: Option<&str>,
    configured: Option<&ModelsResponse>,
    bundled: &ModelsResponse,
) -> ModelsResponse {
    if let Some(configured) = configured {
        return catalog::normalize_bedrock_catalog(configured.clone());
    }
    match BedrockEndpoint::of(info) {
        BedrockEndpoint::Mantle => {
            let is_govcloud = base_url
                .or(info.base_url.as_deref())
                .and_then(|base_url| url::Url::parse(base_url).ok())
                .as_ref()
                .and_then(url::Url::host_str)
                .is_some_and(|host| {
                    host.starts_with("bedrock-mantle.us-gov-") && host.ends_with(".api.aws")
                });
            if is_govcloud {
                catalog::static_gov_model_catalog(bundled)
            } else {
                catalog::static_model_catalog(bundled)
            }
        }
        BedrockEndpoint::Runtime => catalog::static_runtime_model_catalog(bundled),
    }
}

/// Hooks for the separate test workspace; not part of the supported API.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use std::time::SystemTime;

    use http::HeaderMap;
    use http::StatusCode;
    use ra_config::{ModelProviderAwsAuthInfo, ModelProviderInfo};

    use crate::config::ModelsResponse;

    /// Selects the credential source with an injected environment, named as its variant.
    pub fn auth_source(
        provider: &ModelProviderInfo,
        env_var: impl Fn(&'static str) -> Result<String, std::env::VarError> + Copy,
    ) -> String {
        format!("{:?}", super::auth::auth_source(provider, env_var))
    }

    /// Resolves the bearer-token region with an injected environment.
    pub fn bearer_token_region(
        aws: &ModelProviderAwsAuthInfo,
        env_var: impl Fn(&'static str) -> Result<String, std::env::VarError> + Copy,
    ) -> Result<String, String> {
        super::auth::bearer_token_region(aws, env_var).map_err(|error| error.to_string())
    }

    /// Removes headers Mantle does not preserve before `SigV4` verification.
    pub fn strip_mantle_headers(headers: &mut HeaderMap) {
        super::auth::remove_headers_not_preserved_by_bedrock_mantle(headers);
    }

    /// Mantle base URL for `region`.
    pub fn mantle_base_url(region: &str) -> Result<String, String> {
        super::mantle::base_url(region).map_err(|error| error.to_string())
    }

    /// Runtime base URL for `region`.
    #[must_use]
    pub fn runtime_base_url(region: &str) -> String {
        super::runtime::base_url(region)
    }

    /// `SigV4` service and region of the Mantle or Runtime auth config.
    #[must_use]
    pub fn aws_auth_config(
        runtime: bool,
        aws: &ModelProviderAwsAuthInfo,
    ) -> (Option<String>, Option<String>, String) {
        let endpoint = if runtime {
            super::BedrockEndpoint::Runtime
        } else {
            super::BedrockEndpoint::Mantle
        };
        let config = endpoint.aws_auth_config(aws);
        (config.profile, config.region, config.service)
    }

    /// Whether a response would trigger AWS credential recovery.
    #[must_use]
    pub fn is_refreshable_response(status: StatusCode, body: &str) -> bool {
        super::error::is_refreshable_response(status, body)
    }

    /// User-facing replacement for an auth failure.
    #[must_use]
    pub fn failure_message(status: StatusCode, body: &str) -> Option<String> {
        super::error::failure_message(status, body)
    }

    /// Mantle, `GovCloud`, and Runtime catalogs built from the bundled models.
    #[must_use]
    pub fn static_catalogs() -> Option<[ModelsResponse; 3]> {
        let bundled = crate::config::model_runtime::bundled_models()?;
        Some([
            super::catalog::static_model_catalog(bundled),
            super::catalog::static_gov_model_catalog(bundled),
            super::catalog::static_runtime_model_catalog(bundled),
        ])
    }

    /// The bundled `OpenAI` catalog.
    #[must_use]
    pub fn bundled_catalog() -> Option<ModelsResponse> {
        crate::config::model_runtime::bundled_models().cloned()
    }

    /// The catalog a Bedrock provider uses, from its configured catalog or the bundled one.
    #[must_use]
    pub fn provider_catalog(
        provider: &ModelProviderInfo,
        configured: Option<&ModelsResponse>,
    ) -> Option<ModelsResponse> {
        let bundled = crate::config::model_runtime::bundled_models()?;
        Some(super::model_catalog(provider, None, configured, bundled))
    }

    /// Normalizes a configured catalog for Bedrock.
    #[must_use]
    pub fn normalize_catalog(catalog: ModelsResponse) -> ModelsResponse {
        super::catalog::normalize_bedrock_catalog(catalog)
    }

    /// Parses credential exporter output at `now`.
    pub fn parse_credential_export(
        output: &[u8],
        now: SystemTime,
    ) -> std::io::Result<(String, String, Option<String>, SystemTime)> {
        super::credential_export::parse_output_for_test(output, now).map(|(keys, refresh_at)| {
            (
                keys.access_key_id,
                keys.secret_access_key,
                keys.session_token,
                refresh_at,
            )
        })
    }

    /// Wraps the shared credential exporter for cache and recovery tests.
    pub struct CredentialExporter(std::sync::Arc<super::credential_export::AwsCredentialExport>);

    impl CredentialExporter {
        /// Creates an exporter for `command` with `args`.
        #[must_use]
        pub fn new(command: String, args: Vec<String>, timeout_ms: u64) -> Self {
            Self(std::sync::Arc::new(
                super::credential_export::AwsCredentialExport::new(
                    ra_config::AwsCredentialExportConfig {
                        command,
                        args: args.into_iter().map(Into::into).collect(),
                        timeout_ms: std::num::NonZeroU64::new(timeout_ms)
                            .unwrap_or(std::num::NonZeroU64::MIN),
                    },
                ),
            ))
        }

        /// Returns cached or freshly exported keys.
        pub async fn credentials(&self) -> std::io::Result<(String, String, Option<String>)> {
            use ra_aws_auth::AwsCredentialsProvider;
            let keys = self.0.credentials().await?;
            Ok((
                keys.access_key_id,
                keys.secret_access_key,
                keys.session_token,
            ))
        }

        /// Marks cached keys as due for refresh.
        pub async fn expire(&self) {
            self.0.expire_for_test().await;
        }

        /// Whether keys are cached.
        pub async fn is_cached(&self) -> bool {
            self.0.is_cached_for_test().await
        }

        /// Debug rendering, which must not contain secrets.
        #[must_use]
        pub fn debug(&self) -> String {
            format!("{:?}", self.0)
        }

        /// Runs the signer recovery for a 401 with this exporter and an optional refresh command.
        pub async fn recover(
            &self,
            refresh_command: Option<&str>,
            network_policy: ra_http::NetworkPolicy,
        ) -> Result<bool, String> {
            use ra_model::openai::responses::{ResponsesAuthFailure, ResponsesRequestSigner};
            let refresh = refresh_command.map(|command| {
                std::sync::Arc::new(super::auth_refresh::AwsAuthRecovery::new(
                    ra_config::AwsAuthRefreshConfig {
                        command: command.to_owned(),
                        args: Vec::new(),
                        timeout_ms: std::num::NonZeroU64::MIN.saturating_add(4_999),
                    },
                ))
            });
            let signer = super::auth::BedrockSigner {
                method: super::auth::SigningMethod::Bearer("unused".to_owned()),
                endpoint: super::BedrockEndpoint::Mantle,
                recovery: Some(super::auth::AwsRecovery {
                    export: Some(std::sync::Arc::clone(&self.0)),
                    refresh,
                    network_policy,
                }),
            };
            let rejected = ra_model::openai::responses::ResponsesSignedRequest::new(
                String::new(),
                HeaderMap::new(),
            );
            signer
                .recover(ResponsesAuthFailure::Response {
                    request: &rejected,
                    status: StatusCode::UNAUTHORIZED,
                    body: "",
                })
                .await
                .map_err(|error| error.to_string())
        }

        /// A Mantle `SigV4` signer recovering through this exporter whose context loader hands
        /// out `access_keys` in order, one per load. The first key is loaded immediately.
        pub async fn reloading_signer(
            &self,
            access_keys: Vec<String>,
        ) -> std::sync::Arc<dyn ra_model::openai::responses::ResponsesRequestSigner> {
            let keys = std::sync::Arc::new(std::sync::Mutex::new(
                access_keys
                    .into_iter()
                    .collect::<std::collections::VecDeque<_>>(),
            ));
            let loader: super::auth::ContextLoader = std::sync::Arc::new(move || {
                let next = keys
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front();
                Box::pin(async move {
                    let key = next.ok_or_else(|| {
                        ra_core::error::Error::config("no further test credentials")
                    })?;
                    ra_aws_auth::AwsAuthContext::load_with_access_keys(
                        ra_aws_auth::AwsAuthConfig {
                            profile: None,
                            region: Some("us-west-2".to_owned()),
                            service: "bedrock-mantle".to_owned(),
                        },
                        ra_aws_auth::AwsAccessKeys {
                            access_key_id: key,
                            secret_access_key: "test-secret".to_owned(),
                            session_token: None,
                        },
                        ra_http::HttpClientFactory::new(
                            ra_http::OutboundProxyPolicy::ReqwestDefault,
                        ),
                    )
                    .await
                    .map_err(|error| ra_core::error::Error::config(error.to_string()))
                })
            });
            let first = loader().await.unwrap_or_else(|error| panic!("{error}"));
            std::sync::Arc::new(super::auth::BedrockSigner {
                method: super::auth::SigningMethod::SigV4(super::auth::ReloadableContext::new(
                    first, loader,
                )),
                endpoint: super::BedrockEndpoint::Mantle,
                recovery: Some(super::auth::AwsRecovery {
                    export: Some(std::sync::Arc::clone(&self.0)),
                    refresh: None,
                    network_policy: ra_http::NetworkPolicy::default(),
                }),
            })
        }
    }

    /// A signer that authenticates with the provider command bearer token.
    #[must_use]
    pub fn command_signer(
        auth: ra_config::ModelProviderAuthInfo,
    ) -> std::sync::Arc<dyn ra_model::openai::responses::ResponsesRequestSigner> {
        std::sync::Arc::new(super::auth::BedrockSigner {
            method: super::auth::SigningMethod::Command(std::sync::Arc::new(
                crate::config::model_auth::CommandBearerToken::new(auth),
            )),
            endpoint: super::BedrockEndpoint::Mantle,
            recovery: None,
        })
    }
}
