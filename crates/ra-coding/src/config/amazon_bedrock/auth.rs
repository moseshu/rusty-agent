// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bedrock credential source selection and the Responses request signer.

use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::StatusCode;
use ra_aws_auth::{AwsAuthContext, AwsRequestToSign};
use ra_config::{ModelProviderAwsAuthInfo, ModelProviderInfo};
use ra_core::error::{Error, ProviderErrorKind, Result};
use ra_model::openai::responses::{
    ResponsesAuthFailure, ResponsesBearerToken, ResponsesRequestSigner, ResponsesSignableRequest,
    ResponsesSignedRequest,
};

use super::{
    BedrockEndpoint, auth_refresh::AwsAuthRecovery, credential_export::AwsCredentialExport, error,
    mantle::region_from_config,
};
use crate::config::model_auth::CommandBearerToken;

pub(super) const AWS_BEARER_TOKEN_BEDROCK_ENV_VAR: &str = "AWS_BEARER_TOKEN_BEDROCK";
const AWS_ACCESS_KEY_ID_ENV_VAR: &str = "AWS_ACCESS_KEY_ID";
const AWS_SECRET_ACCESS_KEY_ENV_VAR: &str = "AWS_SECRET_ACCESS_KEY";
const AWS_REGION_ENV_VAR: &str = "AWS_REGION";
const AWS_DEFAULT_REGION_ENV_VAR: &str = "AWS_DEFAULT_REGION";

/// Where Bedrock credentials come from, in Codex's precedence order.
///
/// Codex also selects credentials stored by its own sign-in flow between the configured profile
/// and the environment. This product has no sign-in credential store, so those two sources are
/// not represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BedrockAuthSource {
    /// `model_providers.<id>.auth` command bearer token.
    CommandBearerToken,
    /// `aws.credential_export` command credentials, signed with `SigV4`.
    CredentialExport,
    /// `aws.profile` credentials, signed with `SigV4`.
    ConfiguredAwsProfile,
    /// `AWS_BEARER_TOKEN_BEDROCK` bearer token.
    EnvBearerToken,
    /// Static AWS access keys from the environment, signed with `SigV4`.
    EnvAwsCredentials,
    /// The AWS SDK default credential chain, signed with `SigV4`.
    AwsSdk,
}

pub(crate) fn auth_source(
    provider_info: &ModelProviderInfo,
    env_var: impl Fn(&'static str) -> std::result::Result<String, std::env::VarError> + Copy,
) -> BedrockAuthSource {
    if provider_info.auth.is_some() {
        BedrockAuthSource::CommandBearerToken
    } else if provider_info
        .aws
        .as_ref()
        .is_some_and(|aws| aws.credential_export.is_some())
    {
        BedrockAuthSource::CredentialExport
    } else if provider_info
        .aws
        .as_ref()
        .is_some_and(|aws| aws.profile.is_some())
    {
        BedrockAuthSource::ConfiguredAwsProfile
    } else if non_empty_env_var_from(AWS_BEARER_TOKEN_BEDROCK_ENV_VAR, env_var).is_some() {
        BedrockAuthSource::EnvBearerToken
    } else if non_empty_env_var_from(AWS_ACCESS_KEY_ID_ENV_VAR, env_var).is_some()
        && non_empty_env_var_from(AWS_SECRET_ACCESS_KEY_ENV_VAR, env_var).is_some()
    {
        BedrockAuthSource::EnvAwsCredentials
    } else {
        BedrockAuthSource::AwsSdk
    }
}

pub(super) fn non_empty_env_var_from(
    name: &'static str,
    env_var: impl Fn(&'static str) -> std::result::Result<String, std::env::VarError>,
) -> Option<String> {
    env_var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn bearer_token_region(
    aws: &ModelProviderAwsAuthInfo,
    env_var: impl Fn(&'static str) -> std::result::Result<String, std::env::VarError> + Copy,
) -> Result<String> {
    region_from_config(aws)
        .or_else(|| non_empty_env_var_from(AWS_REGION_ENV_VAR, env_var))
        .or_else(|| non_empty_env_var_from(AWS_DEFAULT_REGION_ENV_VAR, env_var))
        .ok_or_else(|| {
            Error::config(
                "Amazon Bedrock bearer token auth requires \
`model_providers.amazon-bedrock.aws.region`, `AWS_REGION`, or `AWS_DEFAULT_REGION`",
            )
        })
}

pub(crate) fn remove_headers_not_preserved_by_bedrock_mantle(headers: &mut HeaderMap) {
    // The Bedrock Mantle front door does not preserve legacy OpenAI
    // compatibility headers that use snake_case, such as `session_id` and
    // `thread_id`, before SigV4 verification. Signing that header class makes
    // richer agent requests fail even though raw Responses requests work.
    let headers_to_remove = headers
        .keys()
        .filter(|name| name.as_str().contains('_'))
        .cloned()
        .collect::<Vec<_>>();
    for name in headers_to_remove {
        headers.remove(name);
    }
}

pub(super) enum SigningMethod {
    Bearer(String),
    Command(Arc<CommandBearerToken>),
    SigV4(ReloadableContext),
}

/// Builds a fresh `SigV4` context, as Codex does each time it resolves request auth.
pub(super) type ContextLoader =
    Arc<dyn Fn() -> BoxFuture<'static, Result<AwsAuthContext>> + Send + Sync>;

/// The current `SigV4` context and how to rebuild it after credential recovery.
///
/// The SDK caches credentials inside the profile and login providers of a context. After `aws
/// login` or a credential export succeeds, the next attempt signs with a newly loaded context so
/// those caches cannot replay a token the endpoint has already rejected.
pub(super) struct ReloadableContext {
    current: RwLock<AwsAuthContext>,
    loader: ContextLoader,
}

impl ReloadableContext {
    pub(super) fn new(context: AwsAuthContext, loader: ContextLoader) -> Self {
        Self {
            current: RwLock::new(context),
            loader,
        }
    }

    fn current(&self) -> AwsAuthContext {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn reload(&self) -> Result<()> {
        let context = (self.loader)().await?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = context;
        Ok(())
    }
}

/// Codex's `aws login` and credential-export recovery for SDK-signed requests.
pub(super) struct AwsRecovery {
    pub(super) export: Option<Arc<AwsCredentialExport>>,
    pub(super) refresh: Option<Arc<AwsAuthRecovery>>,
    pub(super) network_policy: ra_http::NetworkPolicy,
}

/// Authenticates Bedrock Responses requests and recovers from refreshable auth failures.
pub(crate) struct BedrockSigner {
    pub(super) method: SigningMethod,
    pub(super) endpoint: BedrockEndpoint,
    pub(super) recovery: Option<AwsRecovery>,
}

impl BedrockSigner {
    async fn recover_aws(&self, recovery: &AwsRecovery) -> Result<()> {
        // Hold the cache guard across both steps so concurrent callers share recovery.
        let export_refresh = if let Some(exporter) = &recovery.export {
            let refresh = exporter.begin_refresh().await;
            if refresh.is_none() {
                // Another caller completed recovery while this one was waiting.
                return Ok(());
            }
            refresh
        } else {
            None
        };
        let permit = recovery
            .network_policy
            .acquire_for_unsupported_sdk()
            .map_err(|error| Error::provider(ProviderErrorKind::Auth, error.to_string()))?;
        let result: std::io::Result<()> = permit
            .run(async {
                if let Some(refresh) = &recovery.refresh {
                    refresh.refresh().await?;
                }
                if let Some(exporter) = export_refresh {
                    exporter.refresh().await?;
                }
                Ok(())
            })
            .await
            .map_err(|error| Error::provider(ProviderErrorKind::Auth, error.to_string()))?;
        result.map_err(|error| {
            let kind = if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData
                    | std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
            ) {
                ProviderErrorKind::BadRequest
            } else {
                ProviderErrorKind::Auth
            };
            Error::provider(kind, error.to_string())
        })
    }
}

#[async_trait]
impl ResponsesRequestSigner for BedrockSigner {
    async fn sign(&self, request: ResponsesSignableRequest) -> Result<ResponsesSignedRequest> {
        let (method, url, mut headers, body) = request.into_parts();
        let token = match &self.method {
            SigningMethod::Bearer(token) => token.clone(),
            SigningMethod::Command(command) => command.token(None).await?,
            SigningMethod::SigV4(context) => {
                if self.endpoint == BedrockEndpoint::Mantle {
                    remove_headers_not_preserved_by_bedrock_mantle(&mut headers);
                }
                let signed = context
                    .current()
                    .sign(AwsRequestToSign {
                        method,
                        url,
                        headers,
                        body: body.into(),
                    })
                    .await
                    .map_err(|error| error::signing_error(&error))?;
                return Ok(ResponsesSignedRequest::new(signed.url, signed.headers));
            }
        };
        let value = http::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            Error::provider(
                ProviderErrorKind::Auth,
                "Amazon Bedrock bearer token is not a valid header value",
            )
        })?;
        headers.insert(http::header::AUTHORIZATION, value);
        Ok(ResponsesSignedRequest::new(url, headers))
    }

    async fn recover(&self, failure: ResponsesAuthFailure<'_>) -> Result<bool> {
        if let SigningMethod::Command(command) = &self.method {
            // Refresh the token this request carried. A concurrent request may already have
            // replaced it, in which case the newer cached token is reused without running the
            // command again.
            if let ResponsesAuthFailure::Response {
                request,
                status: StatusCode::UNAUTHORIZED,
                ..
            } = failure
            {
                let rejected = bearer_token(request.headers());
                command.token(rejected).await?;
                return Ok(true);
            }
            return Ok(false);
        }
        let Some(recovery) = &self.recovery else {
            return Ok(false);
        };
        let refreshable = match failure {
            ResponsesAuthFailure::Signing(error) => error::is_refreshable_signing_error(error),
            ResponsesAuthFailure::Response { status, body, .. } => {
                error::is_refreshable_response(status, body)
            }
            _ => false,
        };
        if !refreshable {
            return Ok(false);
        }
        self.recover_aws(recovery).await?;
        if let SigningMethod::SigV4(context) = &self.method {
            context.reload().await?;
        }
        Ok(true)
    }

    fn failure_message(&self, status: StatusCode, body: &str) -> Option<String> {
        error::failure_message(status, body)
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
