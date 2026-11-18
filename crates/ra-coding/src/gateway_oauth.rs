// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The coding product's gateway OAuth sign-in for the app server: what Codex's account processor
//! (`app-server/src/request_processors/account_processor/gateway_oauth.rs`) and its status
//! forwarder (`app-server/src/gateway_oauth_notifications.rs`) do with the product's provider.
//!
//! Every operation reloads the product configuration and uses the effective provider's gateway
//! manager, the same manager that inference and model discovery share, so a completed sign-in is
//! visible to existing requests at once. The login policy and status channel belong to the home
//! directory and network configuration the server started with, as Codex keys them on its
//! startup runtime configuration.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use ra_http::HttpClientFactory;
use ra_login::GatewayAuthManager;
use ra_login::GatewayAuthStatus;
use ra_login::GatewayLoginControl;
use ra_protocol::frame::JsonRpcErrorObject;
use ra_protocol::server::GatewayOAuth;
use ra_protocol::server::protocol::GatewayOAuthChangedNotification;
use ra_protocol::server::protocol::GatewayOAuthReadResponse;
use ra_protocol::server::protocol::GatewayOAuthStatus;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::ConfigBuilder;
use crate::config::gateway_auth::GatewayAuthHeaders;

/// Gateway OAuth sign-in for the app server over the product's configuration.
pub struct CodingGatewayOAuth {
    builder: ConfigBuilder,
    home: PathBuf,
    factory: HttpClientFactory,
    control: Arc<GatewayLoginControl>,
    /// Keeps the latest manager alive between requests, as Codex's `gateway_client` does.
    current: Mutex<Option<Arc<GatewayAuthManager>>>,
}

impl std::fmt::Debug for CodingGatewayOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingGatewayOAuth")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl CodingGatewayOAuth {
    /// Reloads configuration with `builder` for every operation. `startup` is the configuration
    /// the server started with; its home directory and network configuration select the login
    /// policy and status channel shared with the product's threads. Pass clones of the same
    /// builder to the thread factory so gateway authentication keeps that startup routing across
    /// configuration reloads, as Codex does through its account runtime.
    #[must_use]
    pub fn new(builder: ConfigBuilder, startup: &Config) -> Arc<Self> {
        let home = startup.codex_home.to_path_buf();
        let factory = builder.bind_gateway_auth_http_client_factory(startup.http_client_factory());
        Arc::new(Self {
            control: GatewayLoginControl::for_host(&home, &factory),
            builder,
            home,
            factory,
            current: Mutex::new(None),
        })
    }

    async fn gateway_client(
        &self,
    ) -> Result<(Config, Option<Arc<GatewayAuthManager>>), JsonRpcErrorObject> {
        let config = self.builder.clone().build().await.map_err(|err| {
            JsonRpcErrorObject::internal_error(format!(
                "failed to load gateway configuration: {err}"
            ))
        })?;
        let manager = gateway_auth_manager(&config).map_err(JsonRpcErrorObject::internal_error)?;
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone_from(&manager);
        Ok((config, manager))
    }
}

/// The effective provider's shared gateway manager; `None` when it does not use gateway OAuth.
fn gateway_auth_manager(config: &Config) -> Result<Option<Arc<GatewayAuthManager>>, String> {
    GatewayAuthHeaders::for_provider(
        &config.model_provider,
        config.codex_home.as_path(),
        &config.gateway_auth_http_client_factory(),
    )
    .map(|headers| headers.manager())
    .transpose()
}

fn status_fields(status: GatewayAuthStatus) -> (GatewayOAuthStatus, Option<String>) {
    match status {
        GatewayAuthStatus::NotReady => (GatewayOAuthStatus::NotReady, None),
        GatewayAuthStatus::Started => (GatewayOAuthStatus::Started, None),
        GatewayAuthStatus::Succeeded => (GatewayOAuthStatus::Succeeded, None),
        GatewayAuthStatus::Failed { message } => (GatewayOAuthStatus::Failed, Some(message)),
    }
}

#[async_trait]
impl GatewayOAuth for CodingGatewayOAuth {
    async fn read(&self) -> Result<GatewayOAuthReadResponse, JsonRpcErrorObject> {
        let (config, client) = self.gateway_client().await?;
        let response =
            GatewayOAuthReadResponse::new(config.model_provider_id, config.model_provider.name);
        let Some(client) = client else {
            return Ok(response);
        };
        let (status, error) = status_fields(
            client
                .status()
                .await
                .map_err(|err| JsonRpcErrorObject::internal_error(err.to_string()))?,
        );
        Ok(response.with_gateway_status(status, error))
    }

    async fn login(
        &self,
        cancel: CancellationToken,
        started: oneshot::Sender<GatewayOAuthChangedNotification>,
    ) -> Result<(), JsonRpcErrorObject> {
        let (config, client) = self.gateway_client().await?;
        let client = client.ok_or_else(|| {
            JsonRpcErrorObject::invalid_request("The current provider does not use gateway OAuth")
        })?;
        // Codex first requires a primary OpenAI account for providers that need one; that account
        // sign-in is not ported, so primary credentials are checked only when a request is sent.
        let provider_id = config.model_provider_id;
        client
            .login_with_browser(cancel.cancelled(), move |url| {
                let _ = started.send(
                    GatewayOAuthChangedNotification::new(
                        provider_id,
                        GatewayOAuthStatus::Started,
                        None,
                    )
                    .with_auth_url(url.as_str()),
                );
            })
            .await
            .map_err(|err| JsonRpcErrorObject::internal_error(err.to_string()))
    }

    fn set_explicit_login(&self, explicit: bool) {
        if explicit {
            self.control.require_explicit_login();
        } else {
            self.control.allow_automatic_login();
        }
    }

    fn subscribe(&self) -> BoxStream<'static, GatewayOAuthChangedNotification> {
        let events = ra_login::subscribe_gateway_auth_status(&self.home, &self.factory);
        futures::stream::unfold(
            (events, self.builder.clone()),
            |(mut events, builder)| async move {
                loop {
                    let change = match events.recv().await {
                        Ok(change) => change,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return None,
                    };
                    let Ok(config) = builder.clone().build().await else {
                        continue;
                    };
                    let Ok(Some(manager)) = gateway_auth_manager(&config) else {
                        continue;
                    };
                    if manager.config() != &change.config {
                        continue;
                    }
                    let (status, error) = status_fields(change.status);
                    let changed = GatewayOAuthChangedNotification::new(
                        config.model_provider_id,
                        status,
                        error,
                    );
                    return Some((changed, (events, builder)));
                }
            },
        )
        .boxed()
    }
}
