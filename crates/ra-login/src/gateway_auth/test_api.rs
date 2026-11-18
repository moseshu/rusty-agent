//! Manager internals the separate test workspace drives; not part of the supported surface.
//!
//! Codex's gateway tests sit inside the crate and reach these items directly. Each function here
//! forwards to the same private item so the ported tests exercise the production paths.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;
use tokio::sync::broadcast;
use url::Url;

use super::GatewayAuthCache;
use super::GatewayAuthManager;
use super::GatewayAuthStatus;
use super::GatewayAuthStatusChange;
use super::GatewayLoginControl;
use super::RefreshOutcome;
use super::RefreshPolicy;
use super::StoredToken;
use super::callback::CallbackListener;
use super::storage;

/// Whole seconds since the Unix epoch, as the manager reads the clock.
#[must_use]
pub fn now_timestamp() -> i64 {
    super::now_timestamp()
}

/// A persisted gateway credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The access token sent to the gateway.
    pub access_token: String,
    /// The refresh token, if the issuer granted one.
    pub refresh_token: Option<String>,
    /// Expiry as Unix seconds, if the issuer reported a lifetime.
    pub expires_at: Option<i64>,
}

impl Token {
    fn stored(&self) -> StoredToken {
        StoredToken {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: self.expires_at,
        }
    }
}

/// Persists `token` as the manager's credential, bypassing its cache.
pub fn save_token(manager: &GatewayAuthManager, token: &Token) -> io::Result<()> {
    manager.save_token(&token.stored())
}

/// The persisted credential in its stored JSON shape, or `null`.
pub fn load_token_json(manager: &GatewayAuthManager) -> io::Result<serde_json::Value> {
    let token = manager.load_token()?;
    serde_json::to_value(token).map_err(io::Error::other)
}

/// The keyring and secrets account under which the credential is stored.
#[must_use]
pub fn credential_id(manager: &GatewayAuthManager) -> String {
    manager.credential_id()
}

/// The home directory whose secrets store holds the credential.
#[must_use]
pub fn home(manager: &GatewayAuthManager) -> PathBuf {
    manager.state.home.clone()
}

/// The browser policy shared by managers with the same home and HTTP client factory.
#[must_use]
pub fn control(manager: &GatewayAuthManager) -> Arc<GatewayLoginControl> {
    Arc::clone(&manager.control)
}

/// Subscribes to the manager's status changes.
#[must_use]
pub fn subscribe(manager: &GatewayAuthManager) -> broadcast::Receiver<GatewayAuthStatusChange> {
    manager.state.status_tx.subscribe()
}

/// Publishes a status for `credential`, returning whether the status changed.
pub fn publish_status(
    manager: &GatewayAuthManager,
    status: GatewayAuthStatus,
    credential: Option<&Token>,
) -> bool {
    manager.publish_status(status, credential.map(Token::stored).as_ref())
}

/// Whether another task currently holds the credential cache.
#[must_use]
pub fn cache_is_locked(manager: &GatewayAuthManager) -> bool {
    manager.state.cached_token.try_lock().is_err()
}

/// The credential cache, held as background resolution and refresh hold it.
pub struct CacheGuard(OwnedMutexGuard<GatewayAuthCache>);

/// Takes the credential cache lock.
pub async fn lock_cache(manager: &GatewayAuthManager) -> CacheGuard {
    CacheGuard(Arc::clone(&manager.state.cached_token).lock_owned().await)
}

impl CacheGuard {
    /// The cached credential in its stored JSON shape, or `null`.
    pub fn token_json(&self) -> io::Result<serde_json::Value> {
        serde_json::to_value(self.0.token.as_ref()).map_err(io::Error::other)
    }

    /// Replaces the cached credential with the persisted one.
    pub fn reload_token(&mut self, manager: &GatewayAuthManager) -> io::Result<()> {
        self.0.token = manager.load_token()?;
        Ok(())
    }

    /// Changes the expiry of a rotation that is waiting to be persisted.
    pub fn set_pending_expires_at(&mut self, expires_at: i64) -> io::Result<()> {
        let pending = self
            .0
            .pending
            .as_mut()
            .ok_or_else(|| io::Error::other("no pending rotation"))?;
        pending.expires_at = Some(expires_at);
        Ok(())
    }
}

/// Runs browser authorization while the caller holds the cache.
pub async fn authorize_with_browser(
    manager: &GatewayAuthManager,
    cache: &mut CacheGuard,
    open_browser: impl FnOnce(&Url),
) -> io::Result<String> {
    manager
        .authorize_with_browser(&mut cache.0, open_browser)
        .await
}

/// The result of one refresh attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum RefreshResult {
    /// A usable access token.
    AccessToken(String),
    /// The issuer rejected the refresh; the user has to sign in again.
    LoginRequired,
}

/// Refreshes an expired credential while the caller holds the cache.
pub async fn refresh_when_expired(
    manager: &GatewayAuthManager,
    cache: &mut CacheGuard,
) -> io::Result<RefreshResult> {
    match manager
        .refresh(&mut cache.0, &RefreshPolicy::WhenExpired)
        .await?
    {
        RefreshOutcome::AccessToken(token) => Ok(RefreshResult::AccessToken(token)),
        RefreshOutcome::LoginRequired => Ok(RefreshResult::LoginRequired),
    }
}

/// Resolves a token for an expired credential, handing any browser authorization to
/// `open_browser` instead of the system browser.
pub async fn resolve_when_expired_with_browser(
    manager: &GatewayAuthManager,
    open_browser: impl FnOnce(&Url),
) -> io::Result<String> {
    manager
        .resolve_with_browser(RefreshPolicy::WhenExpired, open_browser)
        .await
}

/// Takes the cross-process lock that serializes gateway credential exchanges in `home`.
pub async fn lock_credentials(home: &Path) -> io::Result<std::fs::File> {
    storage::lock_credentials(home).await
}

/// Validates an OAuth endpoint as the manager does before any request.
pub fn validate_oauth_url(value: &str, description: &str) -> io::Result<()> {
    super::validate_oauth_url(value, description).map(|_| ())
}

/// The loopback listener that receives the browser's authorization callback.
pub struct Callback(CallbackListener);

impl Callback {
    /// Binds a listener that accepts only `expected_state`.
    pub fn new(redirect_port: Option<u16>, expected_state: String) -> io::Result<Self> {
        CallbackListener::new(redirect_port, expected_state).map(Self)
    }

    /// The redirect URI to register with the authorization request.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        self.0.redirect_uri()
    }

    /// Waits for an authorization code.
    pub async fn wait(&mut self) -> io::Result<String> {
        self.0.wait().await
    }
}
