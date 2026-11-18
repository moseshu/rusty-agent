// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Gateway sign-in RPCs: Codex's `account_processor/gateway_oauth.rs`,
//! `gateway_oauth_notifications.rs` and the gateway policy of its `initialize`.
//!
//! The product owns provider configuration and gateway credentials and plugs them in through
//! [`GatewayOAuth`]. The server keeps what Codex's account processor keeps: one active login per
//! server, bound to the connection that started it. Cancellation is acknowledged only after the
//! active login releases its slot, and closing the owning connection cancels its login.
//!
//! Installing the port selects explicit login at once, so credential reads before `initialize`
//! never open a browser. The first originating client to initialize without
//! `explicitGatewayOauth` restores automatic browser authorization; any explicit opt-in is sticky
//! for later connections.

use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    connection_rpc_gate::ConnectionRpcGate,
    outgoing::{OutgoingMessageSender, ServerNotification},
    protocol::{
        GATEWAY_OAUTH_CHANGED_METHOD, GatewayOAuthCancelResponse, GatewayOAuthChangedNotification,
        GatewayOAuthLoginResponse, GatewayOAuthReadResponse,
    },
};
use crate::{frame::JsonRpcErrorObject, transport::ConnectionId};

/// Clients that do not originate requests on their own behalf and so cannot select the gateway's
/// automatic login: Codex's `NON_ORIGINATING_CLIENT_NAMES`.
const NON_ORIGINATING_CLIENT_NAMES: &[&str] = &["codex_app_server_daemon", "codex-backend"];

/// A product's gateway OAuth sign-in for its effective model provider.
///
/// Each operation reloads the product's current configuration, as Codex's do, and reports a
/// configuration that cannot be loaded as an error.
#[async_trait]
pub trait GatewayOAuth: Send + Sync + 'static {
    /// Reads the effective provider's gateway readiness without refreshing tokens, opening a
    /// browser, or waiting for a login.
    async fn read(&self) -> Result<GatewayOAuthReadResponse, JsonRpcErrorObject>;

    /// Runs browser authorization for the effective provider until the credential has been saved.
    ///
    /// Once authorization starts, the `started` change carrying the authorization URL is sent to
    /// `started`. The login fails, releasing its callback listener, once `cancel` fires.
    async fn login(
        &self,
        cancel: CancellationToken,
        started: oneshot::Sender<GatewayOAuthChangedNotification>,
    ) -> Result<(), JsonRpcErrorObject>;

    /// Requires sign-in through `account/gatewayOAuth/login` when `explicit`, and otherwise
    /// restores automatic browser authorization for requests that need a credential.
    fn set_explicit_login(&self, explicit: bool);

    /// Readiness changes of the then-effective provider's gateway credential, without
    /// authorization URLs. Changes of a gateway the configuration no longer selects are skipped.
    fn subscribe(&self) -> BoxStream<'static, GatewayOAuthChangedNotification>;
}

/// The login occupying the server's single sign-in slot.
struct ActiveGatewayLogin {
    owner: ConnectionId,
    cancel: CancellationToken,
    finished: CancellationToken,
}

type ActiveLogin = Arc<std::sync::Mutex<Option<ActiveGatewayLogin>>>;

/// Releases the slot when the login request ends, however it ends.
struct LoginGuard(ActiveLogin);

impl Drop for LoginGuard {
    fn drop(&mut self) {
        if let Some(active) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
            active.finished.cancel();
        }
    }
}

/// The installed gateway port and the server's sign-in state.
#[derive(Default)]
pub(crate) struct GatewayOAuthSlot {
    control: RwLock<Option<Arc<dyn GatewayOAuth>>>,
    active: ActiveLogin,
    login_policy_initialized: OnceLock<()>,
}

impl std::fmt::Debug for GatewayOAuthSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayOAuthSlot").finish_non_exhaustive()
    }
}

impl GatewayOAuthSlot {
    /// Installs the port and, as Codex's server does at startup, requires explicit login until a
    /// client selects the policy.
    pub(crate) fn set(&self, control: Arc<dyn GatewayOAuth>) {
        control.set_explicit_login(true);
        *self.control.write().unwrap_or_else(PoisonError::into_inner) = Some(control);
    }

    fn get(&self) -> Result<Arc<dyn GatewayOAuth>, JsonRpcErrorObject> {
        self.control
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| JsonRpcErrorObject::invalid_request("Gateway OAuth is not configured"))
    }

    /// Applies a newly initialized connection's gateway login policy.
    pub(crate) fn initialize(&self, client_name: &str, explicit: bool) {
        let Ok(control) = self.get() else {
            return;
        };
        let originating = !NON_ORIGINATING_CLIENT_NAMES.contains(&client_name);
        if explicit || originating {
            // Only the first originating client may restore legacy automatic login.
            // Any explicit opt-in is sticky across subsequent connections.
            self.login_policy_initialized.get_or_init(|| {
                if !explicit {
                    control.set_explicit_login(false);
                }
            });
            if explicit {
                control.set_explicit_login(true);
            }
        }
    }

    pub(crate) async fn read(&self) -> Result<GatewayOAuthReadResponse, JsonRpcErrorObject> {
        self.get()?.read().await
    }

    pub(crate) async fn login(
        &self,
        owner: ConnectionId,
        connection_gate: &ConnectionRpcGate,
        outgoing: &OutgoingMessageSender,
    ) -> Result<GatewayOAuthLoginResponse, JsonRpcErrorObject> {
        let control = self.get()?;
        let cancel = CancellationToken::new();
        let guard = {
            let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
            if connection_gate.is_closed() {
                return Err(JsonRpcErrorObject::invalid_request(
                    "The connection is closed",
                ));
            }
            if active.is_some() {
                return Err(JsonRpcErrorObject::invalid_request(
                    "Gateway sign-in is already in progress",
                ));
            }
            *active = Some(ActiveGatewayLogin {
                owner,
                cancel: cancel.clone(),
                finished: CancellationToken::new(),
            });
            LoginGuard(Arc::clone(&self.active))
        };
        let (started_tx, started_rx) = oneshot::channel();
        let login = control.login(cancel, started_tx);
        tokio::pin!(login);
        let result = tokio::select! {
            result = &mut login => result,
            started = started_rx => {
                if let Ok(started) = started {
                    outgoing
                        .send_server_notification_to_connections(&[owner], changed(&started))
                        .await;
                }
                login.await
            }
        };
        result?;
        drop(guard);
        Ok(GatewayOAuthLoginResponse::new())
    }

    pub(crate) async fn cancel(
        &self,
        owner: ConnectionId,
    ) -> Result<GatewayOAuthCancelResponse, JsonRpcErrorObject> {
        let finished = {
            let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(active) = active.as_ref() else {
                return Ok(GatewayOAuthCancelResponse::new());
            };
            if active.owner != owner {
                return Err(JsonRpcErrorObject::invalid_request(
                    "Gateway sign-in belongs to another connection",
                ));
            }
            active.cancel.cancel();
            active.finished.clone()
        };
        finished.cancelled().await;
        Ok(GatewayOAuthCancelResponse::new())
    }

    /// Cancels the login a closed connection started, releasing its callback listener.
    pub(crate) fn connection_closed(&self, owner: ConnectionId) {
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(active) = active.as_ref()
            && active.owner == owner
        {
            active.cancel.cancel();
        }
    }

    /// Broadcasts the product's readiness changes to every initialized connection.
    pub(crate) fn forward(&self, outgoing: Arc<OutgoingMessageSender>) -> Option<GatewayForwarder> {
        let mut changes = self.get().ok()?.subscribe();
        Some(GatewayForwarder(tokio::spawn(async move {
            while let Some(change) = changes.next().await {
                outgoing.send_server_notification(changed(&change)).await;
            }
        })))
    }
}

fn changed(change: &GatewayOAuthChangedNotification) -> ServerNotification {
    ServerNotification::new(
        GATEWAY_OAUTH_CHANGED_METHOD,
        serde_json::to_value(change).unwrap_or_default(),
    )
}

/// Stops forwarding readiness changes when the server stops.
pub(crate) struct GatewayForwarder(tokio::task::JoinHandle<()>);

impl Drop for GatewayForwarder {
    fn drop(&mut self) {
        self.0.abort();
    }
}
