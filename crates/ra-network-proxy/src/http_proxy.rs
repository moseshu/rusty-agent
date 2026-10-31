// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::ignored_unit_patterns,
    clippy::manual_let_else,
    clippy::map_unwrap_or,
    clippy::single_match_else,
    clippy::unused_async,
    reason = "Preserve the upstream implementation and callback contracts."
)]
use crate::attribution::BindConnectionAttribution;
use crate::config::NetworkMode;
use crate::connect_policy::TargetCheckedTcpConnector;
use crate::connection_lifecycle::CancelOnShutdown;
use crate::mitm;
use crate::mitm_hook::HookEvaluation;
use crate::network_policy::BlockDecisionAuditEventArgs;
use crate::network_policy::NetworkDecision;
use crate::network_policy::NetworkDecisionSource;
use crate::network_policy::NetworkPolicyDecider;
use crate::network_policy::NetworkPolicyDecision;
use crate::network_policy::NetworkPolicyRequest;
use crate::network_policy::NetworkPolicyRequestArgs;
use crate::network_policy::NetworkProtocol;
use crate::network_policy::emit_allow_decision_audit_event;
use crate::network_policy::emit_block_decision_audit_event;
use crate::network_policy::evaluate_host_policy;
use crate::policy::normalize_host;
use crate::reasons::REASON_METHOD_NOT_ALLOWED;
use crate::reasons::REASON_MITM_HOOK_DENIED;
use crate::reasons::REASON_MITM_REQUIRED;
use crate::reasons::REASON_NOT_ALLOWED;
use crate::reasons::REASON_PROXY_DISABLED;
use crate::reasons::REASON_UNIX_SOCKET_UNSUPPORTED;
use crate::request_disconnect::NetworkRequestDisconnect;
use crate::responses::PolicyDecisionDetails;
use crate::responses::blocked_header_value;
use crate::responses::blocked_message_with_policy;
use crate::responses::blocked_text_response_with_policy;
use crate::responses::json_response;
use crate::runtime::HostMitmRequirement;
use crate::runtime::unix_socket_permissions_supported;
use crate::state::BlockedRequest;
use crate::state::BlockedRequestArgs;
use crate::state::NetworkProxyState;
use crate::upstream::UpstreamClient;
use crate::upstream::proxy_for_connect;
use anyhow::Context as _;
use anyhow::Result;
use ra_tls::ensure_rustls_crypto_provider;
use rama_core::Layer;
use rama_core::Service;
use rama_core::error::ErrorExt as _;
use rama_core::error::OpaqueError;
use rama_core::extensions::ExtensionsMut;
use rama_core::extensions::ExtensionsRef;
use rama_core::graceful::ShutdownGuard;
use rama_core::service::BoxService;
use rama_core::service::service_fn;
use rama_core::stream::Stream;
use rama_http::Body;
use rama_http::HeaderMap;
use rama_http::HeaderName;
use rama_http::HeaderValue;
use rama_http::Request;
use rama_http::Response;
use rama_http::StatusCode;
use rama_http::header;
use rama_http::headers::HeaderMapExt;
use rama_http::headers::Host;
use rama_http::layer::remove_header::RemoveResponseHeaderLayer;
use rama_http::matcher::MethodMatcher;
use rama_http_backend::client::proxy::layer::HttpProxyConnector;
use rama_http_backend::server::HttpServer;
use rama_http_backend::server::layer::upgrade::UpgradeLayer;
use rama_http_backend::server::layer::upgrade::Upgraded;
use rama_net::Protocol;
use rama_net::client::ConnectorService;
use rama_net::client::EstablishedClientConnection;
use rama_net::http::RequestContext;
use rama_net::proxy::ProxyRequest;
use rama_net::proxy::ProxyTarget;
use rama_net::proxy::StreamForwardService;
use rama_net::stream::SocketInfo;
use rama_tcp::TcpStream;
use rama_tcp::client::Request as TcpRequest;
use rama_tcp::server::TcpListener;
use rama_tls_rustls::client::TlsConnectorDataBuilder;
use rama_tls_rustls::client::TlsConnectorLayer;
use serde::Serialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Instant;
use tracing::error;
use tracing::info;
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectMitmMode {
    Disabled,
    Enabled,
    DetectProtocol(crate::brokered_tunnel::BrokeredProtocols),
}

pub(crate) async fn run_http_proxy(
    state: Arc<NetworkProxyState>,
    addr: SocketAddr,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    guard: ShutdownGuard,
) -> Result<()> {
    let listener = TcpListener::build()
        .bind(addr)
        .await
        // Rama's `BoxError` is a `Box<dyn Error + Send + Sync>` without an explicit `'static`
        // lifetime bound, which means it doesn't satisfy `anyhow::Context`'s `StdError` constraint.
        // Wrap it in Rama's `OpaqueError` so we can preserve the original error as a source and
        // still use `anyhow` for chaining.
        .map_err(rama_core::error::OpaqueError::from)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("bind HTTP proxy: {addr}"))?;

    run_http_proxy_with_listener(state, listener, policy_decider, environment_id, guard).await
}

pub(crate) async fn run_http_proxy_with_std_listener(
    state: Arc<NetworkProxyState>,
    listener: StdTcpListener,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    guard: ShutdownGuard,
) -> Result<()> {
    let listener =
        TcpListener::try_from(listener).context("convert std listener to HTTP proxy listener")?;
    run_http_proxy_with_listener(state, listener, policy_decider, environment_id, guard).await
}

async fn run_http_proxy_with_listener(
    state: Arc<NetworkProxyState>,
    listener: TcpListener,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    guard: ShutdownGuard,
) -> Result<()> {
    let addr = listener
        .local_addr()
        .context("read HTTP proxy listener local addr")?;

    info!("HTTP proxy listening on {addr}");

    listener
        .serve_graceful(
            guard,
            CancelOnShutdown::new(http_proxy_service(state, policy_decider, environment_id)),
        )
        .await;
    Ok(())
}

pub(crate) fn http_proxy_service(
    state: Arc<NetworkProxyState>,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
) -> BoxService<TcpStream, (), rama_core::error::BoxError> {
    ensure_rustls_crypto_provider();

    // This proxy listener only needs HTTP/1 proxy semantics. Using Rama's auto builder
    // forces every accepted socket through the HTTP version sniffing pre-read path before proxy
    // request parsing, which can stall some local clients on macOS before CONNECT/absolute-form
    // handling runs at all.
    let http_service = HttpServer::http1().service(
        (
            UpgradeLayer::new(
                MethodMatcher::CONNECT,
                service_fn({
                    let policy_decider = policy_decider.clone();
                    let environment_id = environment_id.clone();
                    move |req| {
                        http_connect_accept(policy_decider.clone(), environment_id.clone(), req)
                    }
                }),
                CancelOnShutdown::new(service_fn(http_connect_proxy)),
            ),
            RemoveResponseHeaderLayer::hop_by_hop(),
        )
            .into_layer(service_fn({
                let policy_decider = policy_decider.clone();
                let environment_id = environment_id.clone();
                move |req| http_plain_proxy(policy_decider.clone(), environment_id.clone(), req)
            })),
    );

    BindConnectionAttribution::new(http_service, state, environment_id).boxed()
}

async fn http_connect_accept(
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    mut req: Request,
) -> Result<(Response, Request), Response> {
    if req.headers().get_all(header::HOST).iter().nth(1).is_some() {
        return Err(text_response(
            StatusCode::BAD_REQUEST,
            "multiple Host headers",
        ));
    }
    let started_at = Instant::now();
    let app_state = req
        .extensions()
        .get::<Arc<NetworkProxyState>>()
        .cloned()
        .ok_or_else(|| text_response(StatusCode::INTERNAL_SERVER_ERROR, "missing state"))?;

    let authority = match RequestContext::try_from(&req).map(|ctx| ctx.host_with_port()) {
        Ok(authority) => authority,
        Err(err) => {
            warn!("CONNECT missing authority: {err}");
            return Err(text_response(StatusCode::BAD_REQUEST, "missing authority"));
        }
    };

    let host = normalize_host(&authority.host.to_string());
    if host.is_empty() {
        return Err(text_response(StatusCode::BAD_REQUEST, "invalid host"));
    }

    let client = client_addr(&req);
    let enabled = app_state
        .enabled()
        .await
        .map_err(|err| internal_error("failed to read enabled state", err))?;
    if !enabled {
        let client = client.as_deref().unwrap_or_default();
        warn!("CONNECT blocked; proxy disabled (client={client}, host={host})");
        return Err(proxy_disabled_response(
            &app_state,
            host,
            authority.port,
            client_addr(&req),
            Some("CONNECT".to_string()),
            NetworkProtocol::HttpsConnect,
            /*audit_endpoint_override*/ None,
        )
        .await);
    }

    let disconnect = NetworkRequestDisconnect::default();
    let mut request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
        protocol: NetworkProtocol::HttpsConnect,
        host: host.clone(),
        port: authority.port,
        environment_id,
        client_addr: client.clone(),
        method: Some("CONNECT".to_string()),
        command: None,
        exec_policy_hint: None,
    });

    request.disconnect = Some(disconnect.clone());
    match disconnect
        .track_http_request(
            started_at,
            evaluate_host_policy(&app_state, policy_decider.as_ref(), &request),
        )
        .await
    {
        Ok(NetworkDecision::Deny {
            reason,
            source,
            decision,
        }) => {
            let details = PolicyDecisionDetails {
                decision,
                reason: &reason,
                source,
                protocol: NetworkProtocol::HttpsConnect,
                host: &host,
                port: authority.port,
            };
            let _ = app_state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: reason.clone(),
                    client: client.clone(),
                    method: Some("CONNECT".to_string()),
                    mode: None,
                    protocol: "http-connect".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(authority.port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!("CONNECT blocked (client={client}, host={host}, reason={reason})");
            return Err(blocked_text_with_details(&reason, &details));
        }
        Ok(NetworkDecision::Allow) => {
            let client = client.as_deref().unwrap_or_default();
            info!("CONNECT allowed (client={client}, host={host})");
        }
        Err(err) => {
            error!("failed to evaluate host for CONNECT {host}: {err}");
            return Err(text_response(StatusCode::INTERNAL_SERVER_ERROR, "error"));
        }
    }

    let mode = app_state
        .network_mode()
        .await
        .map_err(|err| internal_error("failed to read network mode", err))?;

    let mitm_state = match app_state.mitm_state().await {
        Ok(state) => state,
        Err(err) => {
            error!("failed to load MITM state: {err}");
            return Err(text_response(StatusCode::INTERNAL_SERVER_ERROR, "error"));
        }
    };
    let host_mitm_requirement = match app_state.host_mitm_requirement(&host, authority.port).await {
        Ok(requirement) => requirement,
        Err(err) => {
            error!("failed to inspect MITM requirements for {host}: {err}");
            return Err(text_response(StatusCode::INTERNAL_SERVER_ERROR, "error"));
        }
    };
    let brokered_http = matches!(host_mitm_requirement, HostMitmRequirement::Credential(protocols) if protocols.http);
    let connect_mitm_mode = if mode == NetworkMode::Limited && !brokered_http {
        ConnectMitmMode::Enabled
    } else {
        match host_mitm_requirement {
            HostMitmRequirement::None => ConnectMitmMode::Disabled,
            HostMitmRequirement::Credential(protocols) => {
                ConnectMitmMode::DetectProtocol(protocols)
            }
            HostMitmRequirement::Always => ConnectMitmMode::Enabled,
        }
    };

    if connect_mitm_mode == ConnectMitmMode::Enabled && mitm_state.is_none() {
        // Limited-mode enforcement and host-specific hooks require interception. Credential-only
        // interception is deferred until the upgraded stream presents a supported protocol.
        emit_http_block_decision_audit_event(
            &app_state,
            BlockDecisionAuditEventArgs {
                source: NetworkDecisionSource::ModeGuard,
                reason: REASON_MITM_REQUIRED,
                protocol: NetworkProtocol::HttpsConnect,
                server_address: host.as_str(),
                server_port: authority.port,
                method: Some("CONNECT"),
                client_addr: client.as_deref(),
            },
        );
        let details = PolicyDecisionDetails {
            decision: NetworkPolicyDecision::Deny,
            reason: REASON_MITM_REQUIRED,
            source: NetworkDecisionSource::ModeGuard,
            protocol: NetworkProtocol::HttpsConnect,
            host: &host,
            port: authority.port,
        };
        let _ = app_state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: host.clone(),
                reason: REASON_MITM_REQUIRED.to_string(),
                client: client.clone(),
                method: Some("CONNECT".to_string()),
                mode: Some(mode),
                protocol: "http-connect".to_string(),
                decision: Some(details.decision.as_str().to_string()),
                source: Some(details.source.as_str().to_string()),
                port: Some(authority.port),
            }))
            .await;
        let client = client.as_deref().unwrap_or_default();
        warn!(
            "CONNECT blocked; MITM required to enforce HTTPS policy (client={client}, host={host}, mode={mode:?}, host_mitm_requirement={host_mitm_requirement:?})"
        );
        return Err(blocked_text_with_details(REASON_MITM_REQUIRED, &details));
    }

    req.extensions_mut().insert(ProxyTarget(authority));
    req.extensions_mut().insert(connect_mitm_mode);
    req.extensions_mut().insert(mode);
    if connect_mitm_mode != ConnectMitmMode::Disabled
        && let Some(mitm_state) = mitm_state
    {
        req.extensions_mut().insert(mitm_state);
    }

    Ok((
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap_or_else(|_| Response::new(Body::empty())),
        req,
    ))
}

async fn http_connect_proxy(upgraded: Upgraded) -> Result<(), Infallible> {
    let mode = upgraded
        .extensions()
        .get::<NetworkMode>()
        .copied()
        .unwrap_or(NetworkMode::Full);
    let connect_mitm_mode = upgraded
        .extensions()
        .get::<ConnectMitmMode>()
        .copied()
        .unwrap_or(ConnectMitmMode::Disabled);
    let result: Result<(), OpaqueError> = match connect_mitm_mode {
        ConnectMitmMode::Disabled => forward_connect_tunnel(upgraded).await,
        ConnectMitmMode::Enabled => mitm_connect_tunnel(upgraded).await,
        ConnectMitmMode::DetectProtocol(protocols) => {
            match crate::brokered_tunnel::peek_protocol(upgraded, protocols).await {
                Ok((crate::brokered_tunnel::TunnelProtocol::Tls, stream)) => {
                    mitm_connect_tunnel(stream).await
                }
                Ok((crate::brokered_tunnel::TunnelProtocol::Http, stream)) => {
                    mitm::mitm_stream(stream, rama_http::uri::Scheme::HTTP)
                        .await
                        .map_err(|err| {
                            OpaqueError::from_display(format!("HTTP tunnel error: {err}"))
                        })
                }
                Ok((crate::brokered_tunnel::TunnelProtocol::Opaque, stream)) => {
                    if mode == NetworkMode::Limited {
                        Err(OpaqueError::from_display(
                            "opaque tunnels are not allowed in limited mode",
                        ))
                    } else {
                        forward_connect_tunnel(stream).await
                    }
                }
                Err(err) => Err(OpaqueError::from_display(format!(
                    "detect tunnel protocol: {err:#}"
                ))),
            }
        }
    };
    if let Err(err) = result {
        warn!("CONNECT tunnel error: {err}");
    }
    Ok(())
}

async fn mitm_connect_tunnel<S>(stream: S) -> Result<(), OpaqueError>
where
    S: Stream + Unpin + ExtensionsMut,
{
    let target = stream
        .extensions()
        .get::<ProxyTarget>()
        .map(|target| target.0.clone())
        .ok_or_else(|| OpaqueError::from_display("missing MITM authority"))?;
    let host = normalize_host(&target.host.to_string());
    let port = target.port;
    let mode = stream
        .extensions()
        .get::<NetworkMode>()
        .copied()
        .unwrap_or(NetworkMode::Full);
    if stream.extensions().get::<Arc<mitm::MitmState>>().is_none() {
        return Err(OpaqueError::from_display(format!(
            "cannot enable MITM without state (host={host}, port={port})"
        )));
    }

    info!("CONNECT MITM enabled (host={host}, port={port}, mode={mode:?})");
    mitm::mitm_stream(stream, rama_http::uri::Scheme::HTTPS)
        .await
        .map_err(|err| OpaqueError::from_display(format!("MITM tunnel error: {err}")))
}

async fn forward_connect_tunnel<S>(upgraded: S) -> Result<(), OpaqueError>
where
    S: Stream + Unpin + ExtensionsMut,
{
    let authority = upgraded
        .extensions()
        .get::<ProxyTarget>()
        .map(|target| target.0.clone())
        .ok_or_else(|| OpaqueError::from_display("missing forward authority"))?;
    let app_state = upgraded
        .extensions()
        .get::<Arc<NetworkProxyState>>()
        .cloned()
        .ok_or_else(|| OpaqueError::from_display("missing app state"))?;
    let allow_upstream_proxy = match app_state.allow_upstream_proxy().await {
        Ok(allowed) => allowed,
        Err(err) => {
            error!("failed to read upstream proxy setting: {err}");
            false
        }
    };
    let proxy = if allow_upstream_proxy {
        proxy_for_connect(&authority, &app_state)
    } else {
        None
    };
    match proxy.as_ref() {
        Some(proxy) => info!(
            "CONNECT route selected (host={}, port={}, route=upstream_proxy, proxy={})",
            authority.host, authority.port, proxy.address
        ),
        None => info!(
            "CONNECT route selected (host={}, port={}, route=direct)",
            authority.host, authority.port
        ),
    }

    let mut extensions = upgraded.extensions().clone();
    if let Some(proxy) = proxy {
        extensions.insert(proxy);
    }

    let req = TcpRequest::new_with_extensions(authority.clone(), extensions)
        .with_protocol(Protocol::HTTPS);
    let proxy_connector = HttpProxyConnector::optional(TargetCheckedTcpConnector::new(app_state));
    let tls_config = TlsConnectorDataBuilder::new()
        .with_alpn_protocols_http_auto()
        .build();
    let connector = TlsConnectorLayer::tunnel(None)
        .with_connector_data(tls_config)
        .into_layer(proxy_connector);
    info!("CONNECT upstream dial started (target={authority})");
    let connect_started_at = Instant::now();
    let EstablishedClientConnection { conn: target, .. } = match connector.connect(req).await {
        Ok(connection) => {
            info!(
                "CONNECT upstream dial established (target={authority}, elapsed_ms={})",
                connect_started_at.elapsed().as_millis()
            );
            connection
        }
        Err(err) => {
            warn!(
                "CONNECT upstream dial failed (target={authority}, elapsed_ms={})",
                connect_started_at.elapsed().as_millis()
            );
            return Err(OpaqueError::from_boxed(err)
                .with_context(|| format!("establish CONNECT tunnel to {authority}")));
        }
    };

    let proxy_req = ProxyRequest {
        source: upgraded,
        target,
    };
    info!("CONNECT tunnel forwarding started (target={authority})");
    let forward_started_at = Instant::now();
    StreamForwardService::default()
        .serve(proxy_req)
        .await
        .map(|_| {
            info!(
                "CONNECT tunnel forwarding completed (target={authority}, elapsed_ms={})",
                forward_started_at.elapsed().as_millis()
            );
        })
        .map_err(|err| {
            warn!(
                "CONNECT tunnel forwarding failed (target={authority}, elapsed_ms={})",
                forward_started_at.elapsed().as_millis()
            );
            OpaqueError::from_boxed(err.into())
                .with_context(|| format!("forward CONNECT tunnel to {authority}"))
        })
}

async fn http_plain_proxy(
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    mut req: Request,
) -> Result<Response, Infallible> {
    // Policy and upstream parsers must not select different Host field lines.
    if req.headers().get_all(header::HOST).iter().nth(1).is_some() {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "multiple Host headers",
        ));
    }
    let started_at = Instant::now();
    let app_state = match req.extensions().get::<Arc<NetworkProxyState>>().cloned() {
        Some(state) => state,
        None => {
            error!("missing app state");
            return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "error"));
        }
    };
    let client = client_addr(&req);
    let method_allowed = match app_state
        .method_allowed(req.method().as_str())
        .await
        .map_err(|err| internal_error("failed to evaluate method policy", err))
    {
        Ok(allowed) => allowed,
        Err(resp) => return Ok(resp),
    };

    // `x-unix-socket` is an escape hatch for talking to local daemons. We keep it tightly scoped:
    // macOS-only + explicit allowlist by default, to avoid turning the proxy into a general local
    // capability escalation mechanism.
    if let Some(unix_socket_header) = req.headers().get("x-unix-socket") {
        let socket_path = match unix_socket_header.to_str() {
            Ok(value) => value.to_string(),
            Err(_) => {
                warn!("invalid x-unix-socket header value (non-UTF8)");
                return Ok(text_response(
                    StatusCode::BAD_REQUEST,
                    "invalid x-unix-socket header",
                ));
            }
        };
        let enabled = match app_state
            .enabled()
            .await
            .map_err(|err| internal_error("failed to read enabled state", err))
        {
            Ok(enabled) => enabled,
            Err(resp) => return Ok(resp),
        };
        if !enabled {
            let client = client.as_deref().unwrap_or_default();
            warn!("unix socket blocked; proxy disabled (client={client}, path={socket_path})");
            return Ok(proxy_disabled_response(
                &app_state,
                socket_path,
                /*port*/ 0,
                client_addr(&req),
                Some(req.method().as_str().to_string()),
                NetworkProtocol::Http,
                Some(("unix-socket", 0)),
            )
            .await);
        }
        if !method_allowed {
            emit_http_block_decision_audit_event(
                &app_state,
                BlockDecisionAuditEventArgs {
                    source: NetworkDecisionSource::ModeGuard,
                    reason: REASON_METHOD_NOT_ALLOWED,
                    protocol: NetworkProtocol::Http,
                    server_address: "unix-socket",
                    server_port: 0,
                    method: Some(req.method().as_str()),
                    client_addr: client.as_deref(),
                },
            );
            let client = client.as_deref().unwrap_or_default();
            let method = req.method();
            warn!(
                "unix socket blocked by method policy (client={client}, method={method}, mode=limited, allowed_methods=GET, HEAD, OPTIONS)"
            );
            return Ok(json_blocked(
                "unix-socket",
                REASON_METHOD_NOT_ALLOWED,
                /*details*/ None,
            ));
        }

        if !unix_socket_permissions_supported() {
            emit_http_block_decision_audit_event(
                &app_state,
                BlockDecisionAuditEventArgs {
                    source: NetworkDecisionSource::ProxyState,
                    reason: REASON_UNIX_SOCKET_UNSUPPORTED,
                    protocol: NetworkProtocol::Http,
                    server_address: "unix-socket",
                    server_port: 0,
                    method: Some(req.method().as_str()),
                    client_addr: client.as_deref(),
                },
            );
            warn!("unix socket proxy unsupported on this platform (path={socket_path})");
            return Ok(text_response(
                StatusCode::NOT_IMPLEMENTED,
                "unix sockets unsupported",
            ));
        }

        return match app_state.is_unix_socket_allowed(&socket_path).await {
            Ok(true) => {
                emit_http_allow_decision_audit_event(
                    &app_state,
                    BlockDecisionAuditEventArgs {
                        source: NetworkDecisionSource::ProxyState,
                        reason: "allow",
                        protocol: NetworkProtocol::Http,
                        server_address: "unix-socket",
                        server_port: 0,
                        method: Some(req.method().as_str()),
                        client_addr: client.as_deref(),
                    },
                );
                let client = client.as_deref().unwrap_or_default();
                info!("unix socket allowed (client={client}, path={socket_path})");
                match proxy_via_unix_socket(req, &socket_path).await {
                    Ok(resp) => Ok(resp),
                    Err(err) => {
                        warn!("unix socket proxy failed: {err}");
                        Ok(text_response(
                            StatusCode::BAD_GATEWAY,
                            "unix socket proxy failed",
                        ))
                    }
                }
            }
            Ok(false) => {
                emit_http_block_decision_audit_event(
                    &app_state,
                    BlockDecisionAuditEventArgs {
                        source: NetworkDecisionSource::ProxyState,
                        reason: REASON_NOT_ALLOWED,
                        protocol: NetworkProtocol::Http,
                        server_address: "unix-socket",
                        server_port: 0,
                        method: Some(req.method().as_str()),
                        client_addr: client.as_deref(),
                    },
                );
                let client = client.as_deref().unwrap_or_default();
                warn!("unix socket blocked (client={client}, path={socket_path})");
                Ok(json_blocked(
                    "unix-socket",
                    REASON_NOT_ALLOWED,
                    /*details*/ None,
                ))
            }
            Err(err) => {
                warn!("unix socket check failed: {err}");
                Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "error"))
            }
        };
    }

    let request_ctx = match RequestContext::try_from(&req) {
        Ok(request_ctx) => request_ctx,
        Err(err) => {
            warn!("missing host: {err}");
            return Ok(text_response(StatusCode::BAD_REQUEST, "missing host"));
        }
    };
    let authority = request_ctx.host_with_port();
    let host = normalize_host(&authority.host.to_string());
    let port = authority.port;
    if let Err(reason) = validate_absolute_form_host_header(&req, &request_ctx) {
        let client = client.as_deref().unwrap_or_default();
        let host_header = req
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<missing>");
        warn!(
            "request rejected due to mismatched Host header (client={client}, target={host}:{port}, host_header={host_header}, reason={reason})"
        );
        return Ok(text_response(StatusCode::BAD_REQUEST, reason));
    }
    let enabled = match app_state
        .enabled()
        .await
        .map_err(|err| internal_error("failed to read enabled state", err))
    {
        Ok(enabled) => enabled,
        Err(resp) => return Ok(resp),
    };
    if !enabled {
        let client = client.as_deref().unwrap_or_default();
        let method = req.method();
        warn!("request blocked; proxy disabled (client={client}, host={host}, method={method})");
        return Ok(proxy_disabled_response(
            &app_state,
            host,
            port,
            client_addr(&req),
            Some(req.method().as_str().to_string()),
            NetworkProtocol::Http,
            /*audit_endpoint_override*/ None,
        )
        .await);
    }

    let disconnect = NetworkRequestDisconnect::default();
    let mut request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
        protocol: NetworkProtocol::Http,
        host: host.clone(),
        port,
        environment_id,
        client_addr: client.clone(),
        method: Some(req.method().as_str().to_string()),
        command: None,
        exec_policy_hint: None,
    });

    request.disconnect = Some(disconnect.clone());
    match disconnect
        .track_http_request(
            started_at,
            evaluate_host_policy(&app_state, policy_decider.as_ref(), &request),
        )
        .await
    {
        Ok(NetworkDecision::Deny {
            reason,
            source,
            decision,
        }) => {
            let details = PolicyDecisionDetails {
                decision,
                reason: &reason,
                source,
                protocol: NetworkProtocol::Http,
                host: &host,
                port,
            };
            let _ = app_state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: reason.clone(),
                    client: client.clone(),
                    method: Some(req.method().as_str().to_string()),
                    mode: None,
                    protocol: "http".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!("request blocked (client={client}, host={host}, reason={reason})");
            return Ok(json_blocked(&host, &reason, Some(&details)));
        }
        Ok(NetworkDecision::Allow) => {}
        Err(err) => {
            error!("failed to evaluate host for {host}: {err}");
            return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "error"));
        }
    }

    let host_mitm_requirement = match app_state.host_mitm_requirement(&host, port).await {
        Ok(requirement) => requirement,
        Err(err) => {
            return Ok(internal_error("failed to inspect MITM requirements", err));
        }
    };
    if host_mitm_requirement == HostMitmRequirement::Always {
        emit_http_block_decision_audit_event(
            &app_state,
            BlockDecisionAuditEventArgs {
                source: NetworkDecisionSource::ModeGuard,
                reason: REASON_MITM_REQUIRED,
                protocol: NetworkProtocol::Http,
                server_address: host.as_str(),
                server_port: port,
                method: Some(req.method().as_str()),
                client_addr: client.as_deref(),
            },
        );
        let details = PolicyDecisionDetails {
            decision: NetworkPolicyDecision::Deny,
            reason: REASON_MITM_REQUIRED,
            source: NetworkDecisionSource::ModeGuard,
            protocol: NetworkProtocol::Http,
            host: &host,
            port,
        };
        let _ = app_state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: host.clone(),
                reason: REASON_MITM_REQUIRED.to_string(),
                client: client.clone(),
                method: Some(req.method().as_str().to_string()),
                mode: None,
                protocol: "http".to_string(),
                decision: Some(details.decision.as_str().to_string()),
                source: Some(details.source.as_str().to_string()),
                port: Some(port),
            }))
            .await;
        let client = client.as_deref().unwrap_or_default();
        warn!(
            "request blocked; MITM required to enforce host policy (client={client}, host={host}, method={})",
            req.method()
        );
        return Ok(json_blocked(&host, REASON_MITM_REQUIRED, Some(&details)));
    }

    if !method_allowed {
        emit_http_block_decision_audit_event(
            &app_state,
            BlockDecisionAuditEventArgs {
                source: NetworkDecisionSource::ModeGuard,
                reason: REASON_METHOD_NOT_ALLOWED,
                protocol: NetworkProtocol::Http,
                server_address: host.as_str(),
                server_port: port,
                method: Some(req.method().as_str()),
                client_addr: client.as_deref(),
            },
        );
        let details = PolicyDecisionDetails {
            decision: NetworkPolicyDecision::Deny,
            reason: REASON_METHOD_NOT_ALLOWED,
            source: NetworkDecisionSource::ModeGuard,
            protocol: NetworkProtocol::Http,
            host: &host,
            port,
        };
        let _ = app_state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: host.clone(),
                reason: REASON_METHOD_NOT_ALLOWED.to_string(),
                client: client.clone(),
                method: Some(req.method().as_str().to_string()),
                mode: Some(NetworkMode::Limited),
                protocol: "http".to_string(),
                decision: Some(details.decision.as_str().to_string()),
                source: Some(details.source.as_str().to_string()),
                port: Some(port),
            }))
            .await;
        let client = client.as_deref().unwrap_or_default();
        let method = req.method();
        warn!(
            "request blocked by method policy (client={client}, host={host}, method={method}, mode=limited, allowed_methods=GET, HEAD, OPTIONS)"
        );
        return Ok(json_blocked(
            &host,
            REASON_METHOD_NOT_ALLOWED,
            Some(&details),
        ));
    }

    let credential_destinations =
        match forward_request_credential_destinations(app_state.as_ref(), &request_ctx, &req).await
        {
            Ok(destinations) => destinations,
            Err(err) => {
                return Ok(internal_error(
                    "failed to read credential injection config",
                    err,
                ));
            }
        };
    // Evaluate the original Authorization header before replacing brokered dummy credentials.
    // Otherwise an unhooked provider alias can receive the real credential without hook policy.
    match app_state
        .evaluate_mitm_hook_request(&host, &req, &credential_destinations)
        .await
    {
        Ok(HookEvaluation::HookedHostNoMatch) => {
            emit_http_block_decision_audit_event(
                &app_state,
                BlockDecisionAuditEventArgs {
                    source: NetworkDecisionSource::ModeGuard,
                    reason: REASON_MITM_HOOK_DENIED,
                    protocol: NetworkProtocol::Http,
                    server_address: host.as_str(),
                    server_port: port,
                    method: Some(req.method().as_str()),
                    client_addr: client.as_deref(),
                },
            );
            let details = PolicyDecisionDetails {
                decision: NetworkPolicyDecision::Deny,
                reason: REASON_MITM_HOOK_DENIED,
                source: NetworkDecisionSource::ModeGuard,
                protocol: NetworkProtocol::Http,
                host: &host,
                port,
            };
            let _ = app_state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: REASON_MITM_HOOK_DENIED.to_string(),
                    client: client.clone(),
                    method: Some(req.method().as_str().to_string()),
                    mode: None,
                    protocol: "http".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!(
                "request blocked by MITM hook policy (client={client}, host={host}, method={})",
                req.method()
            );
            return Ok(json_blocked(&host, REASON_MITM_HOOK_DENIED, Some(&details)));
        }
        Ok(HookEvaluation::Matched { .. } | HookEvaluation::NoHooksForHost) => {}
        Err(err) => return Ok(internal_error("failed to evaluate MITM hook policy", err)),
    }

    for destination in credential_destinations {
        app_state.inject_request_credentials(&destination, req.headers_mut());
    }

    let client = client.as_deref().unwrap_or_default();
    let method = req.method();
    info!("request allowed (client={client}, host={host}, method={method})");

    let allow_upstream_proxy = match app_state
        .allow_upstream_proxy()
        .await
        .map_err(|err| internal_error("failed to read upstream proxy config", err))
    {
        Ok(allow) => allow,
        Err(resp) => return Ok(resp),
    };
    let client = if allow_upstream_proxy {
        UpstreamClient::from_env_proxy(app_state.clone())
    } else {
        UpstreamClient::direct(app_state.clone())
    };

    // Strip hop-by-hop headers only after extracting metadata used for policy correlation.
    remove_hop_by_hop_request_headers(req.headers_mut());
    match client.serve(req).await {
        Ok(resp) => Ok(resp),
        Err(err) => {
            warn!("upstream request failed: {err}");
            Ok(text_response(StatusCode::BAD_GATEWAY, "upstream failure"))
        }
    }
}

async fn forward_request_credential_destinations(
    app_state: &NetworkProxyState,
    context: &RequestContext,
    req: &Request,
) -> Result<Vec<String>> {
    let authority = context.host_with_port();
    let scheme = req.uri().scheme_str().unwrap_or("http");
    // Server-wide OPTIONS may use root-scoped credentials without changing its wire target.
    let request_path = req
        .uri()
        .path_and_query()
        .map(rama_http::uri::PathAndQuery::as_str)
        .filter(|path| *path != "*")
        .unwrap_or("/");
    let mut destinations = vec![format!("{scheme}://{authority}{request_path}")];
    if app_state.plaintext_credential_injection_enabled().await? {
        destinations.push(normalize_host(&authority.host.to_string()));
    }
    Ok(destinations)
}

async fn proxy_via_unix_socket(req: Request, socket_path: &str) -> Result<Response> {
    #[cfg(target_os = "macos")]
    {
        let client = UpstreamClient::unix_socket(socket_path);

        let (mut parts, body) = req.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map(rama_http::uri::PathAndQuery::as_str)
            .unwrap_or("/");
        parts.uri = path
            .parse()
            .with_context(|| format!("invalid unix socket request path: {path}"))?;
        parts.headers.remove("x-unix-socket");
        remove_hop_by_hop_request_headers(&mut parts.headers);

        let req = Request::from_parts(parts, body);
        client.serve(req).await.map_err(anyhow::Error::from)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = req;
        let _ = socket_path;
        Err(anyhow::anyhow!("unix sockets not supported"))
    }
}

fn client_addr<T: ExtensionsRef>(input: &T) -> Option<String> {
    input
        .extensions()
        .get::<SocketInfo>()
        .map(|info| info.peer_addr().to_string())
}

fn validate_absolute_form_host_header(
    req: &Request,
    request_ctx: &RequestContext,
) -> Result<(), &'static str> {
    if req.uri().scheme_str().is_none() {
        return Ok(());
    }

    let Some(host_header) = req
        .headers()
        .typed_try_get::<Host>()
        .map_err(|_| "invalid Host header")?
    else {
        return Ok(());
    };

    if host_header.0.host != request_ctx.authority.host {
        return Err("Host header does not match request target");
    }

    if let Some(host_port) = host_header.0.port {
        if Some(host_port) != request_ctx.authority.port {
            return Err("Host header does not match request target");
        }
        return Ok(());
    }

    if !request_ctx.authority_has_default_port() {
        return Err("Host header does not match request target");
    }

    Ok(())
}
pub(crate) fn remove_hop_by_hop_request_headers(headers: &mut HeaderMap) {
    while let Some(raw_connection) = headers.get(header::CONNECTION).cloned() {
        headers.remove(header::CONNECTION);
        if let Ok(raw_connection) = raw_connection.to_str() {
            let connection_headers: Vec<String> = raw_connection
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            for token in connection_headers {
                if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
                    headers.remove(name);
                }
            }
        }
    }
    for name in [
        &header::KEEP_ALIVE,
        &header::PROXY_CONNECTION,
        &header::PROXY_AUTHORIZATION,
        &header::TRAILER,
        &header::TRANSFER_ENCODING,
        &header::UPGRADE,
    ] {
        headers.remove(name);
    }

    // codespell:ignore te,TE
    // 0x74,0x65 is ASCII "te" (the HTTP TE hop-by-hop header).
    if let Ok(short_hop_header_name) = HeaderName::from_bytes(&[0x74, 0x65]) {
        headers.remove(short_hop_header_name);
    }
}

fn json_blocked(host: &str, reason: &str, details: Option<&PolicyDecisionDetails<'_>>) -> Response {
    let (message, decision, source, protocol, port) = details
        .map(|details| {
            (
                Some(blocked_message_with_policy(reason, details)),
                Some(details.decision.as_str()),
                Some(details.source.as_str()),
                Some(details.protocol.as_policy_protocol()),
                Some(details.port),
            )
        })
        .unwrap_or((None, None, None, None, None));
    let response = BlockedResponse {
        status: "blocked",
        host,
        reason,
        decision,
        source,
        protocol,
        port,
        message,
    };
    let mut resp = json_response(&response);
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp.headers_mut().insert(
        "x-proxy-error",
        HeaderValue::from_static(blocked_header_value(reason)),
    );
    resp
}

fn blocked_text_with_details(reason: &str, details: &PolicyDecisionDetails<'_>) -> Response {
    blocked_text_response_with_policy(reason, details)
}

async fn proxy_disabled_response(
    app_state: &NetworkProxyState,
    host: String,
    port: u16,
    client: Option<String>,
    method: Option<String>,
    protocol: NetworkProtocol,
    audit_endpoint_override: Option<(&str, u16)>,
) -> Response {
    let (audit_server_address, audit_server_port) =
        audit_endpoint_override.unwrap_or((host.as_str(), port));
    emit_http_block_decision_audit_event(
        app_state,
        BlockDecisionAuditEventArgs {
            source: NetworkDecisionSource::ProxyState,
            reason: REASON_PROXY_DISABLED,
            protocol,
            server_address: audit_server_address,
            server_port: audit_server_port,
            method: method.as_deref(),
            client_addr: client.as_deref(),
        },
    );

    let blocked_host = host.clone();
    let _ = app_state
        .record_blocked(BlockedRequest::new(BlockedRequestArgs {
            host: blocked_host,
            reason: REASON_PROXY_DISABLED.to_string(),
            client,
            method,
            mode: None,
            protocol: protocol.as_policy_protocol().to_string(),
            decision: Some("deny".to_string()),
            source: Some("proxy_state".to_string()),
            port: Some(port),
        }))
        .await;

    let details = PolicyDecisionDetails {
        decision: NetworkPolicyDecision::Deny,
        reason: REASON_PROXY_DISABLED,
        source: NetworkDecisionSource::ProxyState,
        protocol,
        host: &host,
        port,
    };
    text_response(
        StatusCode::SERVICE_UNAVAILABLE,
        &blocked_message_with_policy(REASON_PROXY_DISABLED, &details),
    )
}

fn internal_error(context: &str, err: impl std::fmt::Display) -> Response {
    error!("{context}: {err}");
    text_response(StatusCode::INTERNAL_SERVER_ERROR, "error")
}

fn text_response(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(body.to_string())))
}

fn emit_http_block_decision_audit_event(
    app_state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
) {
    emit_block_decision_audit_event(app_state, args);
}

fn emit_http_allow_decision_audit_event(
    app_state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
) {
    emit_allow_decision_audit_event(app_state, args);
}

#[derive(Serialize)]
struct BlockedResponse<'a> {
    status: &'static str,
    host: &'a str,
    reason: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}
