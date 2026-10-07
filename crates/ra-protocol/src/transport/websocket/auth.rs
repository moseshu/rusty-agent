//! Authentication of incoming WebSocket upgrades: Codex's `websocket-auth` crate.
//!
//! A policy either admits every upgrade or asks for a bearer token in the `Authorization` header:
//! a capability token, compared by its SHA-256 digest, or a JWT signed with HS256 under a shared
//! secret, whose expiry, start, issuer and audience are checked. The listener decides whether a
//! policy that admits everyone is acceptable for its address: one that listens beyond loopback
//! refuses to start without authentication.
//!
//! Policies keep only token digests and verification secrets, read once when the policy is built;
//! a later change to a secret file does not reach a policy already built. The token is checked at
//! the upgrade only: expiry and revocation do not end a connection already established.
//!
//! Codex takes paths as its own absolute-path type; here they are [`PathBuf`]s that
//! [`WebsocketAuthArgs::try_into_settings`] makes absolute the way that type does: a leading `~`
//! is the home directory, a relative path is resolved against the current directory, and `.` and
//! `..` are normalized away without consulting the file system. Codex's flag and settings
//! structs have public fields; here the flags are only parsed and the settings built through
//! [`WebsocketAuthSettings::new`], so either can grow a field without breaking a caller. Codex's
//! `anyhow` errors for invalid flags are a [`WebsocketAuthArgsError`] with the same messages.

use std::{
    fmt, io,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use clap::{Args, ValueEnum};
use constant_time_eq::constant_time_eq_32;
use http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::absolute_path;

const DEFAULT_MAX_CLOCK_SKEW_SECONDS: u64 = 30;
const MIN_SIGNED_BEARER_SECRET_BYTES: usize = 32;
const INVALID_AUTHORIZATION_HEADER_MESSAGE: &str = "invalid authorization header";

/// The command-line flags that configure WebSocket authentication: Codex's `WebsocketAuthArgs`.
///
/// A command line takes them in by flattening this struct into its own arguments; the flags only
/// become a configuration through [`try_into_settings`](Self::try_into_settings).
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
#[allow(
    clippy::struct_field_names,
    reason = "Codex's field names, which mirror the `--ws-*` flags they hold"
)]
pub struct WebsocketAuthArgs {
    /// Authentication mode for incoming WebSocket connections.
    #[arg(long = "ws-auth", value_name = "MODE", value_enum)]
    ws_auth: Option<WebsocketAuthCliMode>,

    /// Absolute path to the capability-token file.
    #[arg(long = "ws-token-file", value_name = "PATH")]
    ws_token_file: Option<PathBuf>,

    /// Hex-encoded SHA-256 digest of the capability token.
    #[arg(long = "ws-token-sha256", value_name = "HEX")]
    ws_token_sha256: Option<String>,

    /// Absolute path to the shared secret file for signed JWT bearer tokens.
    #[arg(long = "ws-shared-secret-file", value_name = "PATH")]
    ws_shared_secret_file: Option<PathBuf>,

    /// Expected issuer for signed JWT bearer tokens.
    #[arg(long = "ws-issuer", value_name = "ISSUER")]
    ws_issuer: Option<String>,

    /// Expected audience for signed JWT bearer tokens.
    #[arg(long = "ws-audience", value_name = "AUDIENCE")]
    ws_audience: Option<String>,

    /// Maximum clock skew when validating signed JWT bearer tokens.
    #[arg(long = "ws-max-clock-skew-seconds", value_name = "SECONDS")]
    ws_max_clock_skew_seconds: Option<u64>,
}

/// The authentication modes the flags can select: Codex's `WebsocketAuthCliMode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WebsocketAuthCliMode {
    /// A capability token, given as a file or as its SHA-256 digest.
    CapabilityToken,
    /// A JWT signed with HS256 under a shared secret.
    SignedBearerToken,
}

/// The authentication configured, not yet loaded: Codex's `WebsocketAuthSettings`. No
/// configuration admits every upgrade.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebsocketAuthSettings {
    config: Option<WebsocketAuthConfig>,
}

impl WebsocketAuthSettings {
    /// Settings that require `config`.
    #[must_use]
    pub fn new(config: WebsocketAuthConfig) -> Self {
        Self {
            config: Some(config),
        }
    }

    /// The configuration, if authentication is required.
    #[must_use]
    pub const fn config(&self) -> Option<&WebsocketAuthConfig> {
        self.config.as_ref()
    }
}

/// How upgrades authenticate: Codex's `WebsocketAuthConfig`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebsocketAuthConfig {
    /// A capability token.
    CapabilityToken {
        /// Where the expected token comes from.
        source: WebsocketCapabilityTokenSource,
    },
    /// A JWT signed with HS256.
    SignedBearerToken {
        /// The absolute path of the file holding the shared secret.
        shared_secret_file: PathBuf,
        /// The issuer the token must name, if any.
        issuer: Option<String>,
        /// The audience the token must include, if any.
        audience: Option<String>,
        /// How far the clock may be off when checking expiry and start.
        max_clock_skew_seconds: u64,
    },
}

/// Where the expected capability token comes from: Codex's `WebsocketCapabilityTokenSource`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebsocketCapabilityTokenSource {
    /// A file holding the token; surrounding whitespace is ignored.
    TokenFile {
        /// The absolute path of the file.
        token_file: PathBuf,
    },
    /// The SHA-256 digest of the token.
    TokenSha256 {
        /// The digest.
        token_sha256: [u8; 32],
    },
}

/// Flags that do not describe a valid configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WebsocketAuthArgsError(String);

/// A loaded upgrade policy: Codex's `WebsocketAuthPolicy`.
///
/// The default admits every upgrade; each listener decides whether that is acceptable for the
/// address it binds.
#[derive(Clone, Debug, Default)]
pub struct WebsocketAuthPolicy {
    mode: Option<WebsocketAuthMode>,
}

#[derive(Clone)]
enum WebsocketAuthMode {
    CapabilityToken {
        token_sha256: [u8; 32],
    },
    SignedBearerToken {
        shared_secret: Vec<u8>,
        issuer: Option<String>,
        audience: Option<String>,
        max_clock_skew_seconds: i64,
    },
}

impl fmt::Debug for WebsocketAuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityToken { .. } => formatter
                .debug_struct("CapabilityToken")
                .finish_non_exhaustive(),
            Self::SignedBearerToken { .. } => formatter
                .debug_struct("SignedBearerToken")
                .finish_non_exhaustive(),
        }
    }
}

/// Why an upgrade was refused: Codex's `WebsocketAuthError`.
#[derive(Debug)]
pub struct WebsocketAuthError {
    status_code: StatusCode,
    message: &'static str,
}

impl WebsocketAuthError {
    /// The HTTP status the upgrade is answered with.
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        self.status_code
    }

    /// What the response body says.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

#[derive(Deserialize)]
struct JwtClaims {
    exp: i64,
    nbf: Option<i64>,
    iss: Option<String>,
    aud: Option<JwtAudienceClaim>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JwtAudienceClaim {
    Single(String),
    Multiple(Vec<String>),
}

impl WebsocketAuthArgs {
    /// Checks that the flags describe one configuration and builds it: Codex's
    /// `try_into_settings`.
    ///
    /// Each mode takes only its own flags, a capability token needs exactly one of a file or a
    /// digest, paths are made absolute against the home and current directories, an issuer or
    /// audience of only whitespace counts as none, and the clock skew defaults to 30 seconds.
    pub fn try_into_settings(self) -> Result<WebsocketAuthSettings, WebsocketAuthArgsError> {
        let normalize = |value: Option<String>| {
            value.and_then(|value| {
                let trimmed = value.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            })
        };

        let config = match self.ws_auth {
            Some(WebsocketAuthCliMode::CapabilityToken) => {
                if self.ws_shared_secret_file.is_some()
                    || self.ws_issuer.is_some()
                    || self.ws_audience.is_some()
                    || self.ws_max_clock_skew_seconds.is_some()
                {
                    return Err(args_error(
                        "`--ws-shared-secret-file`, `--ws-issuer`, `--ws-audience`, and `--ws-max-clock-skew-seconds` require `--ws-auth signed-bearer-token`",
                    ));
                }
                let source = match (self.ws_token_file, self.ws_token_sha256) {
                    (Some(_), Some(_)) => {
                        return Err(args_error(
                            "`--ws-token-file` and `--ws-token-sha256` are mutually exclusive",
                        ));
                    }
                    (Some(token_file), None) => WebsocketCapabilityTokenSource::TokenFile {
                        token_file: absolute_path_arg("--ws-token-file", &token_file)?,
                    },
                    (None, Some(token_sha256)) => WebsocketCapabilityTokenSource::TokenSha256 {
                        token_sha256: sha256_digest_arg("--ws-token-sha256", &token_sha256)?,
                    },
                    (None, None) => {
                        return Err(args_error(
                            "`--ws-token-file` or `--ws-token-sha256` is required when `--ws-auth capability-token` is set",
                        ));
                    }
                };
                Some(WebsocketAuthConfig::CapabilityToken { source })
            }
            Some(WebsocketAuthCliMode::SignedBearerToken) => {
                if self.ws_token_file.is_some() || self.ws_token_sha256.is_some() {
                    return Err(args_error(
                        "`--ws-token-file` and `--ws-token-sha256` require `--ws-auth capability-token`, not `signed-bearer-token`",
                    ));
                }
                let shared_secret_file = self.ws_shared_secret_file.ok_or_else(|| {
                    args_error(
                        "`--ws-shared-secret-file` is required when `--ws-auth signed-bearer-token` is set",
                    )
                })?;
                Some(WebsocketAuthConfig::SignedBearerToken {
                    shared_secret_file: absolute_path_arg(
                        "--ws-shared-secret-file",
                        &shared_secret_file,
                    )?,
                    issuer: normalize(self.ws_issuer),
                    audience: normalize(self.ws_audience),
                    max_clock_skew_seconds: self
                        .ws_max_clock_skew_seconds
                        .unwrap_or(DEFAULT_MAX_CLOCK_SKEW_SECONDS),
                })
            }
            None => {
                if self.ws_token_file.is_some()
                    || self.ws_token_sha256.is_some()
                    || self.ws_shared_secret_file.is_some()
                    || self.ws_issuer.is_some()
                    || self.ws_audience.is_some()
                    || self.ws_max_clock_skew_seconds.is_some()
                {
                    return Err(args_error(
                        "websocket auth flags require `--ws-auth capability-token` or `--ws-auth signed-bearer-token`",
                    ));
                }
                None
            }
        };

        Ok(WebsocketAuthSettings { config })
    }
}

/// Loads the credentials `settings` name, once: Codex's `policy_from_settings`. A later change to
/// a secret file does not update the policy.
///
/// # Errors
///
/// Fails if a secret file cannot be read or holds only whitespace, if a signing secret is shorter
/// than 32 bytes, or if the clock skew does not fit a signed 64-bit integer.
pub fn policy_from_settings(settings: &WebsocketAuthSettings) -> io::Result<WebsocketAuthPolicy> {
    let mode = match settings.config.as_ref() {
        Some(WebsocketAuthConfig::CapabilityToken { source }) => match source {
            WebsocketCapabilityTokenSource::TokenFile { token_file } => {
                let token = read_trimmed_secret(token_file)?;
                Some(WebsocketAuthMode::CapabilityToken {
                    token_sha256: sha256_digest(token.as_bytes()),
                })
            }
            WebsocketCapabilityTokenSource::TokenSha256 { token_sha256 } => {
                Some(WebsocketAuthMode::CapabilityToken {
                    token_sha256: *token_sha256,
                })
            }
        },
        Some(WebsocketAuthConfig::SignedBearerToken {
            shared_secret_file,
            issuer,
            audience,
            max_clock_skew_seconds,
        }) => {
            let shared_secret = read_trimmed_secret(shared_secret_file)?.into_bytes();
            validate_signed_bearer_secret(shared_secret_file, &shared_secret)?;
            let max_clock_skew_seconds = i64::try_from(*max_clock_skew_seconds).map_err(|_| {
                io::Error::new(
                    ErrorKind::InvalidInput,
                    "websocket auth clock skew must fit in a signed 64-bit integer",
                )
            })?;
            Some(WebsocketAuthMode::SignedBearerToken {
                shared_secret,
                issuer: issuer.clone(),
                audience: audience.clone(),
                max_clock_skew_seconds,
            })
        }
        None => None,
    };

    Ok(WebsocketAuthPolicy { mode })
}

/// Whether a listener on `bind_address` would admit unauthenticated clients from beyond loopback:
/// Codex's `is_unauthenticated_non_loopback_listener`.
#[must_use]
pub fn is_unauthenticated_non_loopback_listener(
    bind_address: SocketAddr,
    policy: &WebsocketAuthPolicy,
) -> bool {
    !bind_address.ip().is_loopback() && policy.mode.is_none()
}

/// Checks the bearer credential in `headers` before an upgrade: Codex's `authorize_upgrade`. It does
/// not enforce expiry or revocation once a WebSocket has been established.
///
/// # Errors
///
/// Fails with `401 Unauthorized` if the policy asks for a token and the header is missing, is not
/// a `Bearer` credential, or carries a token the policy does not accept.
pub fn authorize_upgrade(
    headers: &HeaderMap,
    policy: &WebsocketAuthPolicy,
) -> Result<(), WebsocketAuthError> {
    let Some(mode) = policy.mode.as_ref() else {
        return Ok(());
    };

    let token = bearer_token_from_headers(headers)?;
    match mode {
        WebsocketAuthMode::CapabilityToken { token_sha256 } => {
            let actual_sha256 = sha256_digest(token.as_bytes());
            if constant_time_eq_32(token_sha256, &actual_sha256) {
                Ok(())
            } else {
                Err(unauthorized("invalid websocket bearer token"))
            }
        }
        WebsocketAuthMode::SignedBearerToken {
            shared_secret,
            issuer,
            audience,
            max_clock_skew_seconds,
        } => verify_signed_bearer_token(
            token,
            shared_secret,
            issuer.as_deref(),
            audience.as_deref(),
            *max_clock_skew_seconds,
        ),
    }
}

fn verify_signed_bearer_token(
    token: &str,
    shared_secret: &[u8],
    issuer: Option<&str>,
    audience: Option<&str>,
    max_clock_skew_seconds: i64,
) -> Result<(), WebsocketAuthError> {
    let claims = decode_jwt_claims(token, shared_secret)?;
    validate_jwt_claims(&claims, issuer, audience, max_clock_skew_seconds)
}

fn decode_jwt_claims(token: &str, shared_secret: &[u8]) -> Result<JwtClaims, WebsocketAuthError> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;

    decode::<JwtClaims>(token, &DecodingKey::from_secret(shared_secret), &validation)
        .map(|token_data| token_data.claims)
        .map_err(|_| unauthorized("invalid websocket jwt"))
}

fn validate_jwt_claims(
    claims: &JwtClaims,
    issuer: Option<&str>,
    audience: Option<&str>,
    max_clock_skew_seconds: i64,
) -> Result<(), WebsocketAuthError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if now > claims.exp.saturating_add(max_clock_skew_seconds) {
        return Err(unauthorized("expired websocket jwt"));
    }
    if let Some(nbf) = claims.nbf
        && now < nbf.saturating_sub(max_clock_skew_seconds)
    {
        return Err(unauthorized("websocket jwt is not valid yet"));
    }
    if let Some(expected_issuer) = issuer
        && claims.iss.as_deref() != Some(expected_issuer)
    {
        return Err(unauthorized("websocket jwt issuer mismatch"));
    }
    if let Some(expected_audience) = audience
        && !audience_matches(claims.aud.as_ref(), expected_audience)
    {
        return Err(unauthorized("websocket jwt audience mismatch"));
    }

    Ok(())
}

fn audience_matches(audience: Option<&JwtAudienceClaim>, expected_audience: &str) -> bool {
    match audience {
        Some(JwtAudienceClaim::Single(actual)) => actual == expected_audience,
        Some(JwtAudienceClaim::Multiple(actual)) => {
            actual.iter().any(|audience| audience == expected_audience)
        }
        None => false,
    }
}

fn bearer_token_from_headers(headers: &HeaderMap) -> Result<&str, WebsocketAuthError> {
    let raw_header = headers
        .get(AUTHORIZATION)
        .ok_or_else(|| unauthorized("missing websocket bearer token"))?;
    let header = raw_header
        .to_str()
        .map_err(|_| unauthorized(INVALID_AUTHORIZATION_HEADER_MESSAGE))?;
    let Some((scheme, token)) = header.split_once(' ') else {
        return Err(unauthorized(INVALID_AUTHORIZATION_HEADER_MESSAGE));
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(unauthorized(INVALID_AUTHORIZATION_HEADER_MESSAGE));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(unauthorized(INVALID_AUTHORIZATION_HEADER_MESSAGE));
    }
    Ok(token)
}

fn validate_signed_bearer_secret(path: &Path, shared_secret: &[u8]) -> io::Result<()> {
    if shared_secret.len() < MIN_SIGNED_BEARER_SECRET_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "signed websocket bearer secret {} must be at least {MIN_SIGNED_BEARER_SECRET_BYTES} bytes",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn read_trimmed_secret(path: &Path) -> io::Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to read websocket auth secret {}: {error}",
                path.display()
            ),
        )
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("websocket auth secret {} must not be empty", path.display()),
        ));
    }
    Ok(trimmed.to_owned())
}

/// `path` made absolute as Codex's `AbsolutePathBuf::try_from` makes it. That fails only when a
/// relative path meets a current directory that cannot be read.
fn absolute_path_arg(flag_name: &str, path: &Path) -> Result<PathBuf, WebsocketAuthArgsError> {
    absolute_path::from_absolute_path(path)
        .map_err(|_| args_error(format!("{flag_name} must be an absolute path")))
}

fn sha256_digest_arg(flag_name: &str, value: &str) -> Result<[u8; 32], WebsocketAuthArgsError> {
    let trimmed = value.trim();
    if trimmed.len() != 64 {
        return Err(args_error(format!(
            "{flag_name} must be a 64-character hex SHA-256 digest"
        )));
    }

    let mut digest = [0u8; 32];
    for (index, pair) in trimmed.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(flag_name, pair[0])?;
        let low = hex_nibble(flag_name, pair[1])?;
        digest[index] = (high << 4) | low;
    }
    Ok(digest)
}

fn hex_nibble(flag_name: &str, byte: u8) -> Result<u8, WebsocketAuthArgsError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(args_error(format!(
            "{flag_name} must be a 64-character hex SHA-256 digest"
        ))),
    }
}

fn sha256_digest(input: &[u8]) -> [u8; 32] {
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&Sha256::digest(input));
    digest
}

fn unauthorized(message: &'static str) -> WebsocketAuthError {
    WebsocketAuthError {
        status_code: StatusCode::UNAUTHORIZED,
        message,
    }
}

fn args_error(message: impl Into<String>) -> WebsocketAuthArgsError {
    WebsocketAuthArgsError(message.into())
}
