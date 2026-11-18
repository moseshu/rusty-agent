// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Gateway OAuth credentials composed onto a provider's primary authentication.
//!
//! A provider with `gateway_oauth` keeps its own credential in `Authorization` and also sends the
//! gateway's OAuth access token in the configured header or cookie. The token is resolved once per
//! request, for inference and for model discovery alike, from a [`GatewayAuthManager`] shared by
//! every provider instance with the same OAuth account, home directory, and HTTP routing.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::Weak;

use async_trait::async_trait;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use ra_config::GatewayOAuthConfig;
use ra_config::GatewayOAuthDelivery;
use ra_config::ModelProviderInfo;
use ra_core::error::Error;
use ra_core::error::ProviderErrorKind;
use ra_core::error::Result;
use ra_http::HttpClientFactory;
use ra_keyring_store::DefaultKeyringStore;
use ra_keyring_store::KeyringStore;
use ra_login::GatewayAuthConfig;
use ra_login::GatewayAuthError;
use ra_login::GatewayAuthManager;
use ra_model::openai::responses::ResponsesAuthHeaders;

/// The message for gateway failures whose details may echo issuer credentials.
const GATEWAY_FAILURE: &str =
    "Gateway OAuth authentication failed; check the gateway configuration and credential store.";

type Managers = Vec<(
    GatewayAuthConfig,
    PathBuf,
    HttpClientFactory,
    Weak<GatewayAuthManager>,
)>;

/// Gateway managers shared across independently configured threads, as Codex keeps them in its
/// process-wide provider state. Entries are weak, so a manager is released with its last user.
fn managers() -> &'static Mutex<Managers> {
    static MANAGERS: OnceLock<Mutex<Managers>> = OnceLock::new();
    MANAGERS.get_or_init(Mutex::default)
}

fn oauth_config(config: &GatewayOAuthConfig) -> GatewayAuthConfig {
    GatewayAuthConfig {
        authorization_url: config.authorization_url.clone(),
        token_url: config.token_url.clone(),
        client_id: config.client_id.clone(),
        resource: config.resource.clone(),
        scopes: config.scopes.clone(),
        redirect_port: config.redirect_port,
    }
}

/// The keyring that holds the encryption key of `home`'s gateway credentials.
fn keyring_for(home: &Path) -> Arc<dyn KeyringStore> {
    #[cfg(feature = "test-api")]
    if let Some(keyring) = test_api::keyring_override(home) {
        return keyring;
    }
    let _ = home;
    Arc::new(DefaultKeyringStore)
}

/// Returns the manager shared by every provider with this OAuth account, home, and routing,
/// creating it when none is alive.
fn shared_manager(
    config: &GatewayOAuthConfig,
    home: &Path,
    factory: &HttpClientFactory,
) -> io::Result<Arc<GatewayAuthManager>> {
    let oauth = oauth_config(config);
    let mut managers = managers().lock().unwrap_or_else(PoisonError::into_inner);
    managers.retain(|(_, _, _, manager)| manager.strong_count() != 0);
    if let Some(manager) = managers
        .iter()
        .find(|(cached, cached_home, cached_factory, _)| {
            cached == &oauth && cached_home == home && cached_factory == factory
        })
        .and_then(|(_, _, _, manager)| manager.upgrade())
    {
        return Ok(manager);
    }
    let manager = Arc::new(GatewayAuthManager::new(
        oauth.clone(),
        home.to_path_buf(),
        factory,
        keyring_for(home),
    )?);
    managers.push((
        oauth,
        home.to_path_buf(),
        factory.clone(),
        Arc::downgrade(&manager),
    ));
    Ok(manager)
}

/// The gateway credential for one provider, applied to every request it sends.
pub(crate) struct GatewayAuthHeaders {
    provider: ModelProviderInfo,
    config: GatewayOAuthConfig,
    // Construct eagerly; report setup failures when a request needs the credential, as Codex's
    // infallible provider factory does.
    manager: std::result::Result<Arc<GatewayAuthManager>, String>,
}

impl GatewayAuthHeaders {
    /// Returns the gateway credential for `provider`, or `None` when it has no `gateway_oauth`.
    pub(crate) fn for_provider(
        provider: &ModelProviderInfo,
        home: &Path,
        factory: &HttpClientFactory,
    ) -> Option<Arc<Self>> {
        let config = provider.gateway_oauth.clone()?;
        let manager = provider.validate().and_then(|()| {
            shared_manager(&config, home, factory)
                .map_err(|_| "failed to create provider OAuth HTTP client".to_string())
        });
        Some(Arc::new(Self {
            provider: provider.clone(),
            config,
            manager,
        }))
    }

    /// The shared manager, or why it could not be created: Codex's provider
    /// `gateway_auth_manager`.
    pub(crate) fn manager(&self) -> std::result::Result<Arc<GatewayAuthManager>, String> {
        self.manager.clone()
    }

    /// The configuration that distinguishes this gateway's credentials in cache identities.
    pub(crate) const fn config(&self) -> &GatewayOAuthConfig {
        &self.config
    }

    async fn header(&self) -> Result<(HeaderName, HeaderValue)> {
        self.provider.validate().map_err(Error::config)?;
        let manager = self
            .manager
            .as_ref()
            .map_err(|error| Error::config(error.clone()))?;
        let token = manager.resolve_access_token().await.map_err(|error| {
            if let Some(login) = error
                .get_ref()
                .and_then(|source| source.downcast_ref::<GatewayAuthError>())
            {
                // Sign-in is required or in progress: retrying the request cannot help.
                return Error::provider(ProviderErrorKind::Auth, login.to_string());
            }
            // Issuer errors may echo arbitrary credentials from configured URLs. Keep the
            // diagnostic safe and bounded, preserving retries for transient failures.
            Error::provider(ProviderErrorKind::Network, GATEWAY_FAILURE)
        })?;
        gateway_header(&self.config, &token)
    }
}

#[async_trait]
impl ResponsesAuthHeaders for GatewayAuthHeaders {
    async fn apply_auth_headers(&self, headers: &mut HeaderMap) -> Result<()> {
        let (name, value) = self.header().await?;
        if headers.contains_key(&name) {
            return Err(Error::config(
                "gateway OAuth conflicts with request headers",
            ));
        }
        headers.insert(name, value);
        Ok(())
    }
}

fn gateway_header(config: &GatewayOAuthConfig, token: &str) -> Result<(HeaderName, HeaderValue)> {
    if token.is_empty() {
        return Err(Error::config("gateway OAuth returned an empty token"));
    }
    let (name, value) = match &config.delivery {
        GatewayOAuthDelivery::Header { name, scheme } => (
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::config("invalid gateway header"))?,
            format!("{scheme} {token}"),
        ),
        GatewayOAuthDelivery::Cookie { name } => {
            // RFC 6265 cookie-octet excludes separators, quotes, whitespace and non-ASCII.
            if !token.bytes().all(
                |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
            ) {
                return Err(Error::config(
                    "gateway OAuth token is not a valid cookie value",
                ));
            }
            (http::header::COOKIE, format!("{name}={token}"))
        }
    };
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| Error::config("invalid gateway OAuth token header"))?;
    value.set_sensitive(true);
    Ok((name, value))
}

/// Keeps product gateway credentials off the user's keyring in the separate test workspace.
#[cfg(feature = "test-api")]
pub mod test_api {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::PoisonError;

    use ra_config::GatewayOAuthConfig;
    use ra_http::HttpClientFactory;
    use ra_http::OutboundProxyPolicy;
    use ra_keyring_store::KeyringStore;
    use ra_login::GatewayAuthManager;

    type Overrides = Vec<(PathBuf, Arc<dyn KeyringStore>)>;

    static OVERRIDES: Mutex<Overrides> = Mutex::new(Vec::new());

    pub(super) fn keyring_override(home: &Path) -> Option<Arc<dyn KeyringStore>> {
        OVERRIDES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(path, _)| path == home)
            .map(|(_, keyring)| Arc::clone(keyring))
    }

    /// Routes the gateway credentials of `home` through `keyring` until dropped.
    pub struct KeyringOverride(PathBuf);

    impl Drop for KeyringOverride {
        fn drop(&mut self) {
            let mut overrides = OVERRIDES.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(index) = overrides.iter().rposition(|(path, _)| path == &self.0) {
                overrides.remove(index);
            }
        }
    }

    /// Makes every gateway manager the product creates for `home` use `keyring`.
    ///
    /// `home` must be the exact product home, as configured, because stored credentials are keyed
    /// on it.
    #[must_use]
    pub fn override_keyring(home: &Path, keyring: Arc<dyn KeyringStore>) -> KeyringOverride {
        OVERRIDES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((home.to_path_buf(), keyring));
        KeyringOverride(home.to_path_buf())
    }

    /// Stores `token` as the gateway credential for `config` in `home`, as an earlier sign-in
    /// would have.
    pub fn seed_gateway_auth(
        config: &GatewayOAuthConfig,
        home: &Path,
        keyring: Arc<dyn KeyringStore>,
        token: &ra_login::test_api::Token,
    ) -> std::io::Result<()> {
        let manager = GatewayAuthManager::new(
            super::oauth_config(config),
            home.to_path_buf(),
            &HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            keyring,
        )?;
        ra_login::test_api::save_token(&manager, token)
    }

    /// The live manager the product shares for `config` in `home`, whatever its routing.
    #[must_use]
    pub fn shared_manager(
        config: &GatewayOAuthConfig,
        home: &Path,
    ) -> Option<Arc<GatewayAuthManager>> {
        let oauth = super::oauth_config(config);
        super::managers()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(cached, cached_home, _, _)| cached == &oauth && cached_home == home)
            .find_map(|(_, _, _, manager)| manager.upgrade())
    }
}
