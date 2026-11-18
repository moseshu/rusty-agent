// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bedrock auth failure classification and the user-facing expired-signature message.

use http::StatusCode;
use ra_aws_auth::AwsAuthError;
use ra_core::error::{Error, ProviderErrorKind};

/// Prefix shared by every credential-loading failure that recovery can repair.
pub(super) const CREDENTIAL_LOAD_ERROR_PREFIX: &str = "failed to load AWS credentials:";

pub(super) const CREDENTIAL_EXPORT_CONFIG_ERROR_PREFIX: &str =
    "AWS credential export configuration error:";

pub(super) const BEDROCK_EXPIRED_SIGNATURE_MESSAGE: &str = concat!(
    "Amazon Bedrock rejected the request because its AWS signature has expired. ",
    "Refresh your AWS credentials and retry. If `AWS_BEARER_TOKEN_BEDROCK` is set, ",
    "update or unset it, then restart the agent",
);

/// Maps a signing failure so retryable credential errors stay transient and exporter
/// configuration errors stop immediately.
pub(super) fn signing_error(error: &AwsAuthError) -> Error {
    if let Some(source) = error.credentials_provider_error() {
        // Command failures need provider recovery, not repeated HTTP transport attempts.
        return if error.is_retryable() {
            Error::provider(
                ProviderErrorKind::Network,
                format!("{CREDENTIAL_LOAD_ERROR_PREFIX} {source}"),
            )
        } else {
            Error::provider(
                ProviderErrorKind::BadRequest,
                format!("{CREDENTIAL_EXPORT_CONFIG_ERROR_PREFIX} {source}"),
            )
        };
    }
    if error.is_retryable() {
        Error::provider(ProviderErrorKind::Network, error.to_string())
    } else {
        Error::provider(ProviderErrorKind::Auth, error.to_string())
    }
}

pub(super) fn is_refreshable_signing_error(error: &Error) -> bool {
    error.to_string().contains(CREDENTIAL_LOAD_ERROR_PREFIX)
}

pub(super) fn is_refreshable_response(status: StatusCode, body: &str) -> bool {
    if status == StatusCode::UNAUTHORIZED {
        return true;
    }
    if status != StatusCode::FORBIDDEN {
        return false;
    }
    let body = body.to_ascii_lowercase();
    [
        "expiredtoken",
        "unrecognizedclientexception",
        "invalidclienttokenid",
    ]
    .iter()
    .any(|error_code| body.contains(error_code))
}

pub(super) fn failure_message(status: StatusCode, body: &str) -> Option<String> {
    (status == StatusCode::UNAUTHORIZED && body.contains("Signature expired:"))
        .then(|| BEDROCK_EXPIRED_SIGNATURE_MESSAGE.to_owned())
}
