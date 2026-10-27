// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    unreachable_pub,
    reason = "Private implementation items are exposed only by the feature-gated test facade."
)]
//! Filesystem-facing executor client with owned RPC transports and session recovery.
#![allow(
    clippy::default_trait_access,
    reason = "Retain inferred defaults at source protocol call sites."
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "Preserve the source ownership and public argument contract."
)]
#![allow(
    clippy::unused_async,
    reason = "Preserve the source asynchronous operation contract."
)]
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
#[cfg(feature = "test-api")]
use crate::client_api::StdioExecServerConnectArgs;
use crate::client_api::{
    ExecServerClientConnectOptions, ExecServerTransportParams, RemoteExecServerConnectArgs,
};
use crate::connection::JsonRpcConnection;
use crate::rpc::SESSION_ALREADY_ATTACHED_ERROR_CODE;
use crate::rpc::{RpcCallError, RpcClient, RpcClientEvent};
use crate::{
    ConnectionAttemptOutcome, EnvironmentConnectionState, ExecServerError,
    protocol::{
        CAPABILITY_ROOTS_DISCOVER_METHOD, CapabilityRootsDiscoverParams,
        CapabilityRootsDiscoverResponse, ENVIRONMENT_INFO_METHOD, ENVIRONMENT_STATUS_METHOD,
        EnvironmentInfo, EnvironmentStatus, FS_CANONICALIZE_METHOD, FS_CLOSE_METHOD,
        FS_COPY_METHOD, FS_CREATE_DIRECTORY_METHOD, FS_GET_METADATA_METHOD, FS_OPEN_METHOD,
        FS_READ_BLOCK_METHOD, FS_READ_DIRECTORY_METHOD, FS_READ_FILE_METHOD, FS_REMOVE_METHOD,
        FS_WALK_METHOD, FS_WRITE_BLOCK_METHOD, FS_WRITE_FILE_METHOD, FsCanonicalizeParams,
        FsCanonicalizeResponse, FsCloseParams, FsCloseResponse, FsCopyParams, FsCopyResponse,
        FsCreateDirectoryParams, FsCreateDirectoryResponse, FsGetMetadataParams,
        FsGetMetadataResponse, FsOpenMode, FsOpenParams, FsOpenResponse, FsReadBlockParams,
        FsReadBlockResponse, FsReadDirectoryParams, FsReadDirectoryResponse, FsReadFileParams,
        FsReadFileResponse, FsRemoveParams, FsRemoveResponse, FsWalkParams, FsWalkResponse,
        FsWriteBlockParams, FsWriteBlockResponse, FsWriteFileParams, FsWriteFileResponse,
        INITIALIZE_METHOD, INITIALIZED_METHOD, InitializeParams, InitializeResponse,
        WireFsCanonicalizeParams, WireFsCopyParams, WireFsCreateDirectoryParams,
        WireFsGetMetadataParams, WireFsOpenParams, WireFsReadDirectoryParams, WireFsReadFileParams,
        WireFsRemoveParams, WireFsWalkParams, WireFsWriteFileParams,
    },
};
use futures::future::BoxFuture;
use ra_http::HttpClientFactory;
use std::result::Result;
use std::{
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};
use tokio::sync::{OnceCell, mpsc, watch};
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::{Instrument, debug, instrument::WithSubscriber};
mod connection_failure;
use connection_failure::ConnectionFailure;
pub(crate) use connection_failure::can_retry_connection_attempt;
const ENVIRONMENT_INFO_TIMEOUT: Duration = Duration::from_secs(10);
const ENVIRONMENT_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_RECOVERY_TIMEOUT: Duration = Duration::from_secs(25);
const SESSION_RECOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(100);
struct Inner {
    connection: StdMutex<ConnectionState>,
    connection_changed: watch::Sender<()>,
    session_id: OnceLock<String>,
    retired: CancellationToken,
    environment_info: OnceCell<Arc<EnvironmentInfo>>,
    reconnect_strategy: Option<(ExecServerTransportParams, HttpClientFactory)>,
    recovery_timeout: Duration,
}
struct ConnectionState {
    status: ConnectionStatus,
    // Publish registration renewal together with the recovered transport.
    executor_registration_id: Option<String>,
    environment_connection_state_tx: watch::Sender<EnvironmentConnectionState>,
}

enum ConnectionStatus {
    Connected(Arc<RpcClient>),
    Recovering,
    Failed(ConnectionFailure),
}

impl ConnectionState {
    fn set_status(&mut self, status: ConnectionStatus) {
        self.status = status;
        self.publish_environment_connection_state();
    }

    fn publish_environment_connection_state(&self) {
        let state = match &self.status {
            ConnectionStatus::Connected(rpc_client) if !rpc_client.is_disconnected() => {
                EnvironmentConnectionState::Connected
            }
            ConnectionStatus::Connected(_)
            | ConnectionStatus::Recovering
            | ConnectionStatus::Failed(_) => EnvironmentConnectionState::Disconnected,
        };
        let _ = self
            .environment_connection_state_tx
            .send_if_modified(|current| {
                // A terminal failure must wake callers waiting on recovery.
                if *current == state && !matches!(self.status, ConnectionStatus::Failed(_)) {
                    false
                } else {
                    *current = state;
                    true
                }
            });
    }
}

#[derive(Clone, Copy)]
enum RecoveryPolicy {
    Wait,
    FailFast,
}

#[derive(Clone)]
pub struct ExecServerClient {
    inner: Arc<Inner>,
    recovery_policy: RecoveryPolicy,
}

type ConnectionResult = Result<ExecServerClient, Arc<ExecServerError>>;
pub(super) type ConnectionObserver = Arc<dyn Fn(Duration, ConnectionAttemptOutcome) + Send + Sync>;
#[derive(Default)]
struct ConnectionAttempt {
    result: OnceCell<ConnectionResult>,
    cancelled: CancellationToken,
    transport: Option<ExecServerTransportParams>,
}
struct ConnectionObservation<'a> {
    started: Option<Instant>,
    outcome: ConnectionAttemptOutcome,
    observer: Option<&'a ConnectionObserver>,
}
impl Drop for ConnectionObservation<'_> {
    fn drop(&mut self) {
        if let (Some(started), Some(observer)) = (self.started, self.observer) {
            observer(started.elapsed(), self.outcome);
        }
    }
}
#[derive(Clone)]
pub struct LazyRemoteExecServerClient {
    transport_params: Option<ExecServerTransportParams>,
    recovery_timeout: Duration,
    http_client_factory: HttpClientFactory,
    recovery_policy: RecoveryPolicy,
    // Saves the first startup result so callers share it; retryable failures use reconnect.
    startup: Arc<ConnectionAttempt>,
    // The latest successful client, replaced whenever reconnecting succeeds.
    current_client: Arc<StdMutex<Option<ExecServerClient>>>,
    reconnect: Arc<StdMutex<Option<Arc<ConnectionAttempt>>>>,
    pub connection_observer: Arc<OnceLock<ConnectionObserver>>,
    environment_connection_state_tx: watch::Sender<EnvironmentConnectionState>,
}

impl LazyRemoteExecServerClient {
    #[must_use]
    pub fn new(
        transport_params: ExecServerTransportParams,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            transport_params: Some(transport_params),
            recovery_timeout: SESSION_RECOVERY_TIMEOUT,
            http_client_factory,
            recovery_policy: RecoveryPolicy::Wait,
            startup: Arc::new(ConnectionAttempt::default()),
            current_client: Arc::new(StdMutex::new(None)),
            reconnect: Arc::new(StdMutex::new(None)),
            connection_observer: Default::default(),
            environment_connection_state_tx: watch::channel(
                EnvironmentConnectionState::Disconnected,
            )
            .0,
        }
    }
    #[cfg(feature = "test-api")]
    #[must_use]
    pub fn with_recovery_timeout_for_test(
        transport_params: ExecServerTransportParams,
        http_client_factory: HttpClientFactory,
        recovery_timeout: Duration,
    ) -> Self {
        let mut client = Self::new(transport_params, http_client_factory);
        client.recovery_timeout = recovery_timeout;
        client
    }
    #[must_use]
    pub fn subscribe_connection_state(&self) -> watch::Receiver<EnvironmentConnectionState> {
        self.environment_connection_state_tx.subscribe()
    }
    #[must_use]
    pub fn start_connecting(&self) -> Option<AbortOnDropHandle<()>> {
        // Stdio starts a process, so keep it lazy until the environment is used.
        if matches!(
            self.transport_params,
            Some(ExecServerTransportParams::StdioCommand { .. })
        ) {
            return None;
        }
        let client = self.clone();
        Some(AbortOnDropHandle::new(tokio::spawn(
            async move {
                if let Err(error) = client.wait_until_ready().await {
                    debug!(%error, "exec-server environment startup failed");
                }
            }
            .in_current_span()
            .with_current_subscriber(),
        )))
    }
    #[must_use]
    pub fn startup_finished(&self) -> bool {
        // Explicit refresh can install the first client without polling startup.
        self.cached_client().is_some() || self.startup.result.get().is_some()
    }
    #[must_use]
    pub fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        if let Some(client) = self.cached_client() {
            return client.readiness_result();
        }
        self.startup.result.get().and_then(|result| match result {
            Ok(client) => client.readiness_result(),
            Err(error) => Some(Err(ExecServerError::ConnectionAttempt(Arc::clone(error)))),
        })
    }
    pub async fn status(&self) -> crate::EnvironmentObservedStatus {
        if let Some(ExecServerTransportParams::Deferred(deferred)) = &self.transport_params
            && let Some(Err(error)) = deferred.readiness.borrow().as_ref()
        {
            return crate::EnvironmentObservedStatus::Disconnected {
                error: ExecServerError::ProvisioningFailed(error.clone()).to_string(),
            };
        }
        // Fail-fast lookup preserves the non-mutating contract: never start or recover a client.
        let client = match self.fail_fast().get().await {
            Ok(client) => client,
            Err(error) => {
                // Without a completed startup attempt, there is no exec-server connection to probe.
                if self.cached_client().is_none() && self.startup.result.get().is_none() {
                    return crate::EnvironmentObservedStatus::Pending;
                }
                // A known connection failure is reported without retrying it as part of status.
                return crate::EnvironmentObservedStatus::Disconnected {
                    error: error.to_string(),
                };
            }
        };
        // Every callable client is probed so callers never receive a cached health result.
        match client.environment_status().await {
            Ok(_) => crate::EnvironmentObservedStatus::Ready,
            Err(error) => crate::EnvironmentObservedStatus::Disconnected {
                error: error.to_string(),
            },
        }
    }
    #[must_use]
    pub fn fail_fast(&self) -> Self {
        Self {
            recovery_policy: RecoveryPolicy::FailFast,
            ..self.clone()
        }
    }
    pub async fn wait_until_ready(&self) -> Result<(), ExecServerError> {
        self.initial_client().await.map(drop)
    }
    pub async fn get(&self) -> Result<ExecServerClient, ExecServerError> {
        if matches!(self.recovery_policy, RecoveryPolicy::FailFast) {
            let client = match self.cached_client() {
                Some(client) => client,
                None => match self.startup.result.get() {
                    Some(Ok(client)) => client.clone(),
                    Some(Err(error)) => {
                        return Err(ExecServerError::ConnectionAttempt(Arc::clone(error)));
                    }
                    None => {
                        return Err(ExecServerError::Disconnected(
                            "exec-server environment is not ready".to_string(),
                        ));
                    }
                },
            };
            return client.fail_fast();
        }
        if let Some(client) = self.connected_client() {
            return Ok(client);
        }

        let Some(cached_client) = self.cached_client() else {
            let client = self.initial_client().await?;
            if !client.is_disconnected() || !self.can_reconnect() {
                return Ok(client);
            }
            return self.reconnect().await;
        };

        if !self.can_reconnect() {
            return Ok(cached_client);
        }

        self.reconnect().await
    }
    async fn initial_client(&self) -> Result<ExecServerClient, ExecServerError> {
        let result = if self.can_reconnect()
            && (self.startup.cancelled.is_cancelled()
                || self.startup.result.get().is_some_and(|result| {
                    result
                        .as_ref()
                        .is_err_and(|error| can_retry_connection_attempt(error))
                })) {
            Box::pin(self.reconnect()).await
        } else {
            self.startup
                .result
                .get_or_init(|| self.connect_once(&self.startup))
                .await
                .clone()
                .map_err(ExecServerError::ConnectionAttempt)
        };
        // Ready may arrive before an older attempt publishes its provisioning failure.
        if let Err(ExecServerError::ConnectionAttempt(error)) = &result
            && matches!(error.as_ref(), ExecServerError::ProvisioningFailed(_))
            && matches!(
                &self.transport_params,
                Some(ExecServerTransportParams::Deferred(deferred))
                    if matches!(*deferred.readiness.borrow(), Some(Ok(())))
            )
        {
            return Box::pin(self.reconnect()).await;
        }
        result
    }
    async fn reconnect(&self) -> Result<ExecServerClient, ExecServerError> {
        // Callers handling the same outage share one reconnect attempt.
        let attempt = {
            let mut reconnect = self
                .reconnect
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(client) = self.connected_client() {
                return Ok(client);
            }
            reconnect
                .get_or_insert_with(|| Arc::new(ConnectionAttempt::default()))
                .clone()
        };
        let result = attempt
            .result
            .get_or_init(|| self.connect_once(&attempt))
            .await;
        let mut reconnect = self
            .reconnect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Forget only this completed attempt so a later operation can retry after failure.
        if reconnect
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &attempt))
        {
            *reconnect = None;
        }
        result.clone().map_err(ExecServerError::ConnectionAttempt)
    }
    fn connected_client(&self) -> Option<ExecServerClient> {
        self.cached_client()
            .filter(|client| !client.is_disconnected())
    }
    #[must_use]
    pub fn cached_executor_registration_id(&self) -> Option<String> {
        self.cached_client()
            .and_then(|client| client.executor_registration_id())
    }
    fn cached_client(&self) -> Option<ExecServerClient> {
        self.current_client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn can_reconnect(&self) -> bool {
        matches!(
            self.transport_params,
            Some(
                ExecServerTransportParams::Deferred(_)
                    | ExecServerTransportParams::WebSocketUrl { .. }
            )
        )
    }
    pub async fn environment_info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        Ok(self.get().await?.environment_info().await?.as_ref().clone())
    }
    fn connect_once<'a>(
        &'a self,
        attempt: &'a ConnectionAttempt,
    ) -> BoxFuture<'a, ConnectionResult> {
        // Keep the transport future out of every caller's async layout, including
        // the CLI entry point, which otherwise exceeds rustc's query-depth limit.
        Box::pin(async move {
            let transport = attempt
                .transport
                .as_ref()
                .or(self.transport_params.as_ref())
                .ok_or_else(|| {
                    Arc::new(ExecServerError::Protocol(
                        "missing transport params for lazy exec-server connection".to_string(),
                    ))
                })?;
            let mut observation = ConnectionObservation {
                started: None,
                outcome: ConnectionAttemptOutcome::Cancelled,
                observer: (self.cached_client().is_none() && attempt.transport.is_none())
                    .then(|| self.connection_observer.get())
                    .flatten(),
            };
            let result = tokio::select! {
                biased;
                () = attempt.cancelled.cancelled() => return Err(Arc::new(ExecServerError::Disconnected("connection attempt was superseded".to_string()))),
                result = ExecServerClient::connect_for_transport(transport.clone(), self.http_client_factory.clone(), &mut observation.started, self.recovery_timeout) => result,
            };
            let client = result.map_err(|error| {
                observation.outcome = ConnectionAttemptOutcome::Failure;
                Arc::new(error)
            })?;
            // Cancellation can race with a completed handshake. Recheck before attaching
            // state or installing the client, under the same lock used by refresh.
            {
                let mut current = self
                    .current_client
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !attempt.cancelled.is_cancelled() {
                    client.attach_environment_connection_state(
                        self.environment_connection_state_tx.clone(),
                    );
                    *current = Some(client.clone());
                    observation.outcome = ConnectionAttemptOutcome::Success;
                    return Ok(client);
                }
            }
            client.inner.retire().await;
            Err(Arc::new(ExecServerError::Disconnected(
                "connection attempt was superseded".to_string(),
            )))
        })
    }

    pub async fn refresh_connection(&self) -> Result<(), ExecServerError> {
        Err(ExecServerError::Protocol(
            "connection refresh requires a Noise registry".to_string(),
        ))
    }
}

impl Inner {
    pub(super) async fn rpc_client(self: &Arc<Self>) -> Result<Arc<RpcClient>, ExecServerError> {
        let mut connection_changed = self.connection_changed.subscribe();
        loop {
            if let Some(message) = self.connection_failure() {
                return Err(message.into());
            }

            let rpc_client = {
                let connection = self
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &connection.status {
                    ConnectionStatus::Connected(rpc_client) => Some(Arc::clone(rpc_client)),
                    ConnectionStatus::Recovering | ConnectionStatus::Failed(_) => None,
                }
            };
            let Some(rpc_client) = rpc_client else {
                let _ = connection_changed.changed().await;
                continue;
            };
            if !rpc_client.is_disconnected() {
                return Ok(rpc_client);
            }

            let _ = connection_changed.changed().await;
        }
    }
    pub(super) fn is_failed(&self) -> bool {
        self.connection_failure().is_some()
    }
    pub(super) fn connection_failure(&self) -> Option<ConnectionFailure> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &connection.status {
            ConnectionStatus::Failed(message) => Some(message.clone()),
            ConnectionStatus::Connected(_) | ConnectionStatus::Recovering => None,
        }
    }
    pub(super) fn request_recovery(
        self: &Arc<Self>,
        failed_rpc_client: Arc<RpcClient>,
        disconnect_message: String,
    ) {
        let should_recover = {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &connection.status {
                ConnectionStatus::Connected(current)
                    if Arc::ptr_eq(current, &failed_rpc_client) =>
                {
                    connection.set_status(ConnectionStatus::Recovering);
                    true
                }
                ConnectionStatus::Connected(_)
                | ConnectionStatus::Recovering
                | ConnectionStatus::Failed(_) => false,
            }
        };
        if !should_recover {
            return;
        }

        self.notify_connection_changed();
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = inner.retired.cancelled() => {},
                () = inner.recover(disconnect_message) => {},
            }
        });
    }
    async fn recover(self: &Arc<Self>, disconnect_message: String) {
        let recovery_timeout = self.recovery_timeout;
        let deadline = Instant::now() + recovery_timeout;
        if self.reconnect_strategy.is_none() {
            self.fail(ConnectionFailure::Disconnected(disconnect_message))
                .await;
            return;
        }

        let Some(session_id) = self.session_id.get().cloned() else {
            let message = format!(
                "{disconnect_message}; failed to resume exec-server session: missing session id"
            );
            self.fail(ConnectionFailure::Disconnected(message)).await;
            return;
        };
        let last_error = loop {
            match timeout_at(deadline, self.resume_once(&session_id)).await {
                Ok(Ok((rpc_client, _attempt, executor_registration_id))) => {
                    if self.install_recovered_client(rpc_client, executor_registration_id) {
                        return;
                    }
                }
                Ok(Err(error)) => {
                    if let Some(denial) = error.application_network_policy_denial() {
                        self.fail(ConnectionFailure::Policy(denial)).await;
                        return;
                    }
                    if !is_retryable_recovery_error(&error) {
                        break error.to_string();
                    }
                }
                Err(_) => {
                    break format!("recovery timed out after {recovery_timeout:?}");
                }
            }

            let retry_delay = SESSION_RECOVERY_RETRY_INTERVAL;

            let now = Instant::now();
            if now >= deadline {
                break format!("recovery timed out after {recovery_timeout:?}");
            }
            sleep(retry_delay.min(deadline - now)).await;
        };

        let message =
            format!("{disconnect_message}; failed to resume exec-server session: {last_error}");
        self.fail(ConnectionFailure::Disconnected(message)).await;
    }
    fn install_recovered_client(
        &self,
        rpc_client: Arc<RpcClient>,
        executor_registration_id: Option<String>,
    ) -> bool {
        let installed = {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.retired.is_cancelled()
                || !matches!(connection.status, ConnectionStatus::Recovering)
                || rpc_client.is_disconnected()
            {
                false
            } else {
                connection.executor_registration_id = executor_registration_id;
                connection.set_status(ConnectionStatus::Connected(rpc_client));
                true
            }
        };
        if installed {
            self.notify_connection_changed();
        }
        installed
    }
    fn notify_connection_changed(&self) {
        self.connection_changed.send_replace(());
    }
    async fn fail(self: &Arc<Self>, message: ConnectionFailure) {
        let (_message, newly_failed) = {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &connection.status {
                ConnectionStatus::Failed(existing) => (existing.clone(), false),
                ConnectionStatus::Connected(_) | ConnectionStatus::Recovering => {
                    connection.set_status(ConnectionStatus::Failed(message.clone()));
                    (message, true)
                }
            }
        };
        if newly_failed {
            self.notify_connection_changed();
        }
    }

    async fn resume_once(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<
        (
            Arc<RpcClient>,
            Option<tokio::sync::OwnedSemaphorePermit>,
            Option<String>,
        ),
        ExecServerError,
    > {
        let (transport, factory) = self
            .reconnect_strategy
            .as_ref()
            .ok_or_else(|| ExecServerError::Protocol("missing reconnect strategy".to_string()))?;
        let (connection, options) = crate::client_transport::open_transport(
            transport.clone(),
            factory.clone(),
            Some(session_id.to_string()),
        )
        .await?;
        let (rpc_client, events_rx) = RpcClient::new(connection);
        let rpc_client = Arc::new(rpc_client);
        let client = ExecServerClient {
            inner: Arc::clone(self),
            recovery_policy: RecoveryPolicy::Wait,
        };
        client.spawn_rpc_reader(&rpc_client, events_rx);
        client.initialize_rpc(&rpc_client, options).await?;
        Ok((rpc_client, None, None))
    }
    async fn retire(self: &Arc<Self>) {
        let rpc = {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            connection.environment_connection_state_tx =
                watch::channel(EnvironmentConnectionState::Disconnected).0;
            let rpc = match &connection.status {
                ConnectionStatus::Connected(rpc) => Some(Arc::clone(rpc)),
                _ => None,
            };
            self.retired.cancel();
            connection.set_status(ConnectionStatus::Failed(ConnectionFailure::Disconnected(
                "exec-server executor was replaced".to_string(),
            )));
            rpc
        };
        self.notify_connection_changed();
        if let Some(rpc) = rpc {
            rpc.close_transport().await;
        }
    }
}
impl ExecServerClient {
    pub(crate) async fn connect_for_transport(
        transport: ExecServerTransportParams,
        factory: HttpClientFactory,
        started: &mut Option<Instant>,
        recovery_timeout: Duration,
    ) -> Result<Self, ExecServerError> {
        *started = Some(Instant::now());
        let reconnectable = matches!(
            transport,
            ExecServerTransportParams::WebSocketUrl { .. } | ExecServerTransportParams::Deferred(_)
        );
        let (connection, options) =
            crate::client_transport::open_transport(transport.clone(), factory.clone(), None)
                .await?;
        Self::connect_with_recovery(
            connection,
            options,
            reconnectable.then_some((transport, factory)),
            recovery_timeout,
        )
        .await
    }
    #[cfg(feature = "test-api")]
    #[must_use]
    pub fn recovery_timeout_for_test(&self) -> Duration {
        self.inner.recovery_timeout
    }
    #[cfg(feature = "test-api")]
    #[must_use]
    pub fn shares_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
    #[cfg(feature = "test-api")]
    pub async fn connect_stdio_command(
        args: StdioExecServerConnectArgs,
    ) -> Result<Self, ExecServerError> {
        let factory = HttpClientFactory::new(ra_http::OutboundProxyPolicy::ReqwestDefault);
        let options: ExecServerClientConnectOptions = args.clone().into();
        let transport = ExecServerTransportParams::StdioCommand {
            command: args.command,
            initialize_timeout: args.initialize_timeout,
        };
        let (connection, _) =
            crate::client_transport::open_transport(transport, factory, args.resume_session_id)
                .await?;
        Self::connect_with_recovery(connection, options, None, SESSION_RECOVERY_TIMEOUT).await
    }

    pub async fn connect_websocket(
        args: RemoteExecServerConnectArgs,
    ) -> Result<Self, ExecServerError> {
        let transport = ExecServerTransportParams::WebSocketUrl {
            websocket_url: args.websocket_url,
            connect_timeout: args.connect_timeout,
            initialize_timeout: args.initialize_timeout,
            http_headers: Default::default(),
        };
        let (connection, options) = crate::client_transport::open_transport(
            transport.clone(),
            args.http_client_factory.clone(),
            args.resume_session_id,
        )
        .await?;
        Self::connect_with_recovery(
            connection,
            ExecServerClientConnectOptions {
                client_name: args.client_name,
                ..options
            },
            Some((transport, args.http_client_factory)),
            SESSION_RECOVERY_TIMEOUT,
        )
        .await
    }
    pub(crate) async fn connect_with_recovery(
        connection: JsonRpcConnection,
        options: ExecServerClientConnectOptions,
        reconnect_strategy: Option<(ExecServerTransportParams, HttpClientFactory)>,
        recovery_timeout: Duration,
    ) -> Result<Self, ExecServerError> {
        let (rpc, events) = RpcClient::new(connection);
        let rpc = Arc::new(rpc);
        let client = Self {
            inner: Arc::new(Inner {
                connection: StdMutex::new(ConnectionState {
                    status: ConnectionStatus::Connected(Arc::clone(&rpc)),
                    executor_registration_id: None,
                    environment_connection_state_tx: watch::channel(
                        EnvironmentConnectionState::Connected,
                    )
                    .0,
                }),
                connection_changed: watch::channel(()).0,
                session_id: OnceLock::new(),
                retired: CancellationToken::new(),
                environment_info: OnceCell::new(),
                reconnect_strategy,
                recovery_timeout,
            }),
            recovery_policy: RecoveryPolicy::Wait,
        };
        client.spawn_rpc_reader(&rpc, events);
        let response = client.initialize_rpc(&rpc, options).await?;
        if let Some(info) = response.environment_info {
            let _ = client.inner.environment_info.set(Arc::new(info));
        }
        Ok(client)
    }
    async fn initialize_rpc(
        &self,
        rpc_client: &RpcClient,
        options: ExecServerClientConnectOptions,
    ) -> Result<InitializeResponse, ExecServerError> {
        let ExecServerClientConnectOptions {
            client_name,
            initialize_timeout,
            resume_session_id,
        } = options;
        timeout(initialize_timeout, async {
            let response: InitializeResponse = rpc_client
                .call(
                    INITIALIZE_METHOD,
                    &InitializeParams {
                        client_name,
                        resume_session_id,
                    },
                )
                .await?;
            let session_id = self
                .inner
                .session_id
                .get_or_init(|| response.session_id.clone());
            if session_id != &response.session_id {
                return Err(ExecServerError::Protocol(format!(
                    "exec-server initialized an unexpected session {}",
                    response.session_id
                )));
            }
            rpc_client
                .notify(INITIALIZED_METHOD, &serde_json::json!({}))
                .await?;
            Ok(response)
        })
        .await
        .map_err(|_| ExecServerError::InitializeTimedOut {
            timeout: initialize_timeout,
        })?
    }
    fn spawn_rpc_reader(
        &self,
        rpc_client: &Arc<RpcClient>,
        mut events: mpsc::Receiver<RpcClientEvent>,
    ) {
        let inner = Arc::downgrade(&self.inner);
        let rpc = Arc::downgrade(rpc_client);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let (Some(inner), Some(rpc)) = (inner.upgrade(), rpc.upgrade()) else {
                    return;
                };
                match event {
                    RpcClientEvent::Disconnected { reason } => {
                        inner.request_recovery(rpc, disconnected_message(reason.as_deref()));
                        return;
                    }
                    RpcClientEvent::Request { request, .. } => {
                        if rpc
                            .respond_error(
                                request.id,
                                crate::rpc::method_not_found(format!(
                                    "exec-server client does not implement `{}` yet",
                                    request.method
                                )),
                            )
                            .await
                            .is_err()
                        {
                            inner.request_recovery(rpc, disconnected_message(None));
                            return;
                        }
                    }
                    RpcClientEvent::Notification(_) => {}
                }
            }
        });
    }
    fn executor_registration_id(&self) -> Option<String> {
        self.inner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .executor_registration_id
            .clone()
    }
    fn attach_environment_connection_state(
        &self,
        state_tx: watch::Sender<EnvironmentConnectionState>,
    ) {
        let mut connection = self
            .inner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        connection.environment_connection_state_tx = state_tx;
        connection.publish_environment_connection_state();
    }
    fn fail_fast(&self) -> Result<Self, ExecServerError> {
        self.rpc_client_without_recovery()?;
        Ok(Self {
            inner: Arc::clone(&self.inner),
            recovery_policy: RecoveryPolicy::FailFast,
        })
    }
    fn rpc_client_without_recovery(&self) -> Result<Arc<RpcClient>, ExecServerError> {
        let connection = self
            .inner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &connection.status {
            ConnectionStatus::Connected(rpc_client) if !rpc_client.is_disconnected() => {
                Ok(Arc::clone(rpc_client))
            }
            ConnectionStatus::Connected(_) | ConnectionStatus::Recovering => Err(
                ExecServerError::Disconnected("exec-server environment is recovering".to_string()),
            ),
            ConnectionStatus::Failed(message) => Err(message.clone().into()),
        }
    }
    async fn rpc_client(&self) -> Result<Arc<RpcClient>, ExecServerError> {
        match self.recovery_policy {
            RecoveryPolicy::Wait => self.inner.rpc_client().await,
            RecoveryPolicy::FailFast => self.rpc_client_without_recovery(),
        }
    }
    pub async fn environment_info(&self) -> Result<Arc<EnvironmentInfo>, ExecServerError> {
        self.inner
            .environment_info
            .get_or_try_init(|| async { self.force_environment_info().await.map(Arc::new) })
            .await
            .cloned()
    }
    pub async fn force_environment_info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        self.call_with_timeout_recovery(ENVIRONMENT_INFO_METHOD, &(), ENVIRONMENT_INFO_TIMEOUT)
            .await
    }
    pub async fn environment_status(&self) -> Result<EnvironmentStatus, ExecServerError> {
        // Health checks only reuse an existing RPC connection and never initiate recovery.
        let rpc_client = self.rpc_client_without_recovery()?;
        self.map_rpc_call_result(
            rpc_client
                .call_with_timeout(ENVIRONMENT_STATUS_METHOD, &(), ENVIRONMENT_STATUS_TIMEOUT)
                .await,
        )
    }
    pub async fn discover_capability_roots(
        &self,
        params: CapabilityRootsDiscoverParams,
    ) -> Result<CapabilityRootsDiscoverResponse, ExecServerError> {
        self.call(CAPABILITY_ROOTS_DISCOVER_METHOD, &params).await
    }
    pub async fn fs_read_file(
        &self,
        params: FsReadFileParams,
    ) -> Result<FsReadFileResponse, ExecServerError> {
        self.call(FS_READ_FILE_METHOD, &WireFsReadFileParams::from(params))
            .await
    }
    pub async fn fs_open(&self, params: FsOpenParams) -> Result<FsOpenResponse, ExecServerError> {
        // Older executors ignore the mode field and could silently return a read-only handle.
        if params.mode == FsOpenMode::Replace
            && !self
                .environment_info()
                .await?
                .capabilities
                .file_write_streaming
        {
            return Err(ExecServerError::Protocol(
                "exec-server does not support writable file streams".to_string(),
            ));
        }
        self.call(FS_OPEN_METHOD, &WireFsOpenParams::from(params))
            .await
    }
    pub async fn fs_read_block(
        &self,
        params: FsReadBlockParams,
    ) -> Result<FsReadBlockResponse, ExecServerError> {
        self.call(FS_READ_BLOCK_METHOD, &params).await
    }
    pub async fn fs_write_block(
        &self,
        params: FsWriteBlockParams,
    ) -> Result<FsWriteBlockResponse, ExecServerError> {
        if !self
            .environment_info()
            .await?
            .capabilities
            .file_write_streaming
        {
            return Err(ExecServerError::Protocol(
                "exec-server does not support writable file streams".to_string(),
            ));
        }
        self.call(FS_WRITE_BLOCK_METHOD, &params).await
    }
    pub async fn fs_close(
        &self,
        params: FsCloseParams,
    ) -> Result<FsCloseResponse, ExecServerError> {
        self.call_for_cleanup(FS_CLOSE_METHOD, &params).await
    }
    pub async fn fs_write_file(
        &self,
        params: FsWriteFileParams,
    ) -> Result<FsWriteFileResponse, ExecServerError> {
        self.call(FS_WRITE_FILE_METHOD, &WireFsWriteFileParams::from(params))
            .await
    }
    pub async fn fs_create_directory(
        &self,
        params: FsCreateDirectoryParams,
    ) -> Result<FsCreateDirectoryResponse, ExecServerError> {
        self.call(
            FS_CREATE_DIRECTORY_METHOD,
            &WireFsCreateDirectoryParams::from(params),
        )
        .await
    }
    pub async fn fs_get_metadata(
        &self,
        params: FsGetMetadataParams,
    ) -> Result<FsGetMetadataResponse, ExecServerError> {
        self.call(
            FS_GET_METADATA_METHOD,
            &WireFsGetMetadataParams::from(params),
        )
        .await
    }
    pub async fn fs_canonicalize(
        &self,
        params: FsCanonicalizeParams,
    ) -> Result<FsCanonicalizeResponse, ExecServerError> {
        self.call(
            FS_CANONICALIZE_METHOD,
            &WireFsCanonicalizeParams::from(params),
        )
        .await
    }
    pub async fn fs_read_directory(
        &self,
        params: FsReadDirectoryParams,
    ) -> Result<FsReadDirectoryResponse, ExecServerError> {
        self.call(
            FS_READ_DIRECTORY_METHOD,
            &WireFsReadDirectoryParams::from(params),
        )
        .await
    }
    pub async fn fs_walk(&self, params: FsWalkParams) -> Result<FsWalkResponse, ExecServerError> {
        self.call(FS_WALK_METHOD, &WireFsWalkParams::from(params))
            .await
    }
    pub async fn fs_remove(
        &self,
        params: FsRemoveParams,
    ) -> Result<FsRemoveResponse, ExecServerError> {
        self.call(FS_REMOVE_METHOD, &WireFsRemoveParams::from(params))
            .await
    }
    pub async fn fs_copy(&self, params: FsCopyParams) -> Result<FsCopyResponse, ExecServerError> {
        self.call(FS_COPY_METHOD, &WireFsCopyParams::from(params))
            .await
    }
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.inner.session_id.get().cloned()
    }
    fn is_disconnected(&self) -> bool {
        self.inner.is_failed()
    }
    fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        let connection = self
            .inner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &connection.status {
            ConnectionStatus::Connected(rpc_client) if !rpc_client.is_disconnected() => {
                Some(Ok(()))
            }
            ConnectionStatus::Connected(_) | ConnectionStatus::Recovering => None,
            ConnectionStatus::Failed(message) => Some(Err(message.clone().into())),
        }
    }
    async fn call<P, T>(&self, method: &str, params: &P) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rpc_client = self.rpc_client().await?;
        self.call_rpc(&rpc_client, method, params).await
    }
    async fn call_with_timeout_recovery<P, T>(
        &self,
        method: &str,
        params: &P,
        call_timeout: Duration,
    ) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rpc_client = self.rpc_client().await?;
        // Bound sending as well as receiving: a stuck transport can fill the outbound queue.
        if let Ok(result) = timeout(call_timeout, rpc_client.call(method, params)).await {
            self.map_rpc_call_result(result)
        } else {
            let error = ExecServerError::RpcTimedOut {
                method: method.to_string(),
                timeout: call_timeout,
            };
            if self.inner.reconnect_strategy.is_some() {
                // Retire only the connection we probed; recovery ignores a stale client.
                rpc_client.close_transport().await;
                self.inner.request_recovery(rpc_client, error.to_string());
            }
            Err(error)
        }
    }
    async fn call_rpc<P, T>(
        &self,
        rpc_client: &Arc<RpcClient>,
        method: &str,
        params: &P,
    ) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        self.map_rpc_call_result(rpc_client.call(method, params).await)
    }
    async fn call_for_cleanup<P, T>(&self, method: &str, params: &P) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rpc_client = self.inner.rpc_client().await?;
        self.map_rpc_call_result(rpc_client.call_for_cleanup(method, params).await)
    }
    fn map_rpc_call_result<T>(
        &self,
        result: Result<T, RpcCallError>,
    ) -> Result<T, ExecServerError> {
        // Explicit retirement rejects late responses. Ordinary EOF still preserves
        // responses received before disconnect, as ordered by the RPC reader.
        if self.inner.retired.is_cancelled() {
            return Err(ExecServerError::Disconnected(
                "exec-server executor was replaced".to_string(),
            ));
        }
        result.map_err(|error| {
            let error = ExecServerError::from(error);
            if is_transport_closed_error(&error) {
                ExecServerError::Disconnected(disconnected_message(/*reason*/ None))
            } else {
                error
            }
        })
    }
}
fn disconnected_message(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("exec-server transport disconnected: {reason}"),
        None => "exec-server transport disconnected".to_string(),
    }
}
fn is_transport_closed_error(error: &ExecServerError) -> bool {
    matches!(
        error,
        ExecServerError::Closed | ExecServerError::Disconnected(_)
    ) || matches!(
        error,
        ExecServerError::Server {
            code: -32000,
            message,
        } if message == "JSON-RPC transport closed"
    )
}
impl From<RpcCallError> for ExecServerError {
    fn from(value: RpcCallError) -> Self {
        match value {
            RpcCallError::Closed => Self::Closed,
            RpcCallError::Json(err) => Self::Json(err),
            RpcCallError::Server(error) => Self::Server {
                code: error.code,
                message: error.message,
            },
            RpcCallError::TimedOut { method, timeout } => Self::RpcTimedOut { method, timeout },
            RpcCallError::PendingRequestLimitExceeded { limit } => Self::Protocol(format!(
                "exec-server has reached its limit of {limit} pending requests"
            )),
        }
    }
}
#[must_use]
pub fn is_retryable_recovery_error(error: &ExecServerError) -> bool {
    if error.application_network_policy_denial().is_some() {
        return false;
    }
    if let ExecServerError::ConnectionAttempt(error) = error {
        return is_retryable_recovery_error(error.as_ref());
    }
    is_transport_closed_error(error)
        || matches!(
            error,
            ExecServerError::ProvisioningFailed(_)
                | ExecServerError::WebSocketConnectTimeout { .. }
                | ExecServerError::WebSocketConnect { .. }
                | ExecServerError::InitializeTimedOut { .. }
        )
        || is_retryable_registry_error(error)
        || matches!(
            error,
            ExecServerError::Server { code, .. }
                if *code == SESSION_ALREADY_ATTACHED_ERROR_CODE
        )
}
#[must_use]
pub fn is_retryable_registry_error(error: &ExecServerError) -> bool {
    matches!(
        error,
        ExecServerError::EnvironmentRegistryRequest(error)
            if error.is_connect()
                || error.is_timeout()
                || error.is_body()
                || matches!(
                    error,
                    ra_http::RouteAwareRequestError::Request(error)
                        if error.is_decode()
                )
    ) || matches!(
        error,
        ExecServerError::EnvironmentRegistryHttp { status, .. }
            if status.is_server_error()
                || *status == http::StatusCode::REQUEST_TIMEOUT
                || *status == http::StatusCode::TOO_MANY_REQUESTS
    ) || is_environment_offline_error(error)
}
pub fn is_environment_offline_error(error: &ExecServerError) -> bool {
    matches!(
        error,
        ExecServerError::EnvironmentRegistryHttp { status, code, .. }
            if *status == http::StatusCode::CONFLICT
                && code.as_deref() == Some("environment_offline")
    )
}

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub async fn connect_for_transport_for_test(
    transport: ExecServerTransportParams,
    factory: HttpClientFactory,
    started: &mut Option<Instant>,
) -> Result<ExecServerClient, ExecServerError> {
    ExecServerClient::connect_for_transport(transport, factory, started, SESSION_RECOVERY_TIMEOUT)
        .await
}
#[cfg(feature = "test-api")]
#[doc(hidden)]
#[must_use]
pub fn is_disconnected_for_test(client: &ExecServerClient) -> bool {
    client.is_disconnected()
}
