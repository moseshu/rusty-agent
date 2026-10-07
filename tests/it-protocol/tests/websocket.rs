//! The WebSocket transport and the authentication of its upgrades, as Codex's app-server has them
//! (`app-server-transport/src/transport/websocket.rs`, `websocket-auth/src/lib.rs`).
//!
//! The authentication cases port Codex's `websocket-auth` unit tests through the public entry
//! points: flags are parsed as a command line would give them, and tokens are checked by loading
//! a policy from secret files and authorizing request headers. The transport cases port the
//! transport-level part of Codex's `connection_handling_websocket` suite against an echo server in
//! this process — Codex runs its whole app-server — plus pings, binary frames, closing from either
//! side and shutdown, which Codex's source handles and its suite does not reach.

use std::{
    collections::HashMap,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse},
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        websocket::{
            auth::{
                WebsocketAuthArgs, WebsocketAuthArgsError, WebsocketAuthConfig,
                WebsocketAuthPolicy, WebsocketAuthSettings, WebsocketCapabilityTokenSource,
                authorize_upgrade, is_unauthenticated_non_loopback_listener, policy_from_settings,
            },
            start_websocket_acceptor,
        },
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message, client::IntoClientRequest, http::header::ORIGIN},
};
use tokio_util::sync::CancellationToken;

type HmacSha256 = Hmac<Sha256>;
type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PATIENCE: Duration = Duration::from_secs(10);
const SHARED_SECRET: &str = "0123456789abcdef0123456789abcdef";

// ---------------------------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------------------------

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    auth: WebsocketAuthArgs,
}

fn settings_from(flags: &[&str]) -> Result<WebsocketAuthSettings, WebsocketAuthArgsError> {
    Cli::try_parse_from(std::iter::once("app-server").chain(flags.iter().copied()))
        .unwrap()
        .auth
        .try_into_settings()
}

/// Codex's `capability_token_args_require_token_file_or_hash`.
#[test]
fn capability_token_args_require_token_file_or_hash() {
    let error = settings_from(&["--ws-auth", "capability-token"]).unwrap_err();
    assert!(
        error.to_string().contains("--ws-token-file")
            && error.to_string().contains("--ws-token-sha256"),
        "unexpected error: {error}"
    );
}

/// Codex's `capability_token_args_accept_token_hash`.
#[test]
fn capability_token_args_accept_token_hash() {
    let settings = settings_from(&[
        "--ws-auth",
        "capability-token",
        "--ws-token-sha256",
        &"ab".repeat(32),
    ])
    .unwrap();
    assert_eq!(
        settings,
        WebsocketAuthSettings::new(WebsocketAuthConfig::CapabilityToken {
            source: WebsocketCapabilityTokenSource::TokenSha256 {
                token_sha256: [0xab; 32],
            },
        })
    );
}

/// Codex's `capability_token_args_reject_multiple_token_sources`.
#[test]
fn capability_token_args_reject_multiple_token_sources() {
    let error = settings_from(&[
        "--ws-auth",
        "capability-token",
        "--ws-token-file",
        "/tmp/token",
        "--ws-token-sha256",
        &"ab".repeat(32),
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("mutually exclusive"),
        "unexpected error: {error}"
    );
}

/// Codex's `capability_token_args_reject_malformed_token_hash`.
#[test]
fn capability_token_args_reject_malformed_token_hash() {
    let error = settings_from(&[
        "--ws-auth",
        "capability-token",
        "--ws-token-sha256",
        "not-a-sha256",
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("64-character hex"),
        "unexpected error: {error}"
    );
}

/// A secret path is made absolute as Codex's `AbsolutePathBuf::try_from` makes it: a leading `~` is
/// the home directory, a relative path is resolved against the current directory, and `.` and `..`
/// are normalized away lexically, stopping at the root.
#[test]
fn auth_args_make_secret_paths_absolute() {
    let cwd = std::env::current_dir().unwrap();
    let token_file = |path: &str| {
        let settings =
            settings_from(&["--ws-auth", "capability-token", "--ws-token-file", path]).unwrap();
        match settings.config() {
            Some(WebsocketAuthConfig::CapabilityToken {
                source: WebsocketCapabilityTokenSource::TokenFile { token_file },
            }) => token_file.clone(),
            other => panic!("unexpected config {other:?}"),
        }
    };

    assert_eq!(token_file("token"), cwd.join("token"));
    assert_eq!(token_file("./nested/../token"), cwd.join("token"));
    assert_eq!(token_file("../token"), cwd.parent().unwrap().join("token"));
    #[cfg(unix)]
    {
        assert_eq!(
            token_file("/tmp/codex/../codex-home/./token"),
            PathBuf::from("/tmp/codex-home/token")
        );
        assert_eq!(token_file("/../../token"), PathBuf::from("/token"));
    }
    if let Some(home) = std::env::home_dir() {
        assert_eq!(token_file("~"), home);
        assert_eq!(token_file("~/code"), home.join("code"));
        assert_eq!(token_file("~//code"), home.join("code"));
    }

    let settings = settings_from(&[
        "--ws-auth",
        "signed-bearer-token",
        "--ws-shared-secret-file",
        "secret",
    ])
    .unwrap();
    match settings.config() {
        Some(WebsocketAuthConfig::SignedBearerToken {
            shared_secret_file, ..
        }) => assert_eq!(*shared_secret_file, cwd.join("secret")),
        other => panic!("unexpected config {other:?}"),
    }
}

/// Codex's `signed_bearer_args_require_mode_when_mode_specific_flags_are_set`.
#[test]
fn signed_bearer_args_require_mode_when_mode_specific_flags_are_set() {
    let error = settings_from(&["--ws-shared-secret-file", "/tmp/secret"]).unwrap_err();
    assert!(
        error.to_string().contains("websocket auth flags require"),
        "unexpected error: {error}"
    );
}

/// Codex's `signed_bearer_args_default_clock_skew_and_trim_optional_claims`.
#[test]
fn signed_bearer_args_default_clock_skew_and_trim_optional_claims() {
    let settings = settings_from(&[
        "--ws-auth",
        "signed-bearer-token",
        "--ws-shared-secret-file",
        "/tmp/secret",
        "--ws-issuer",
        " issuer ",
        "--ws-audience",
        "   ",
    ])
    .unwrap();
    assert_eq!(
        settings,
        WebsocketAuthSettings::new(WebsocketAuthConfig::SignedBearerToken {
            shared_secret_file: "/tmp/secret".into(),
            issuer: Some("issuer".to_owned()),
            audience: None,
            max_clock_skew_seconds: 30,
        })
    );
}

/// No flags, no authentication.
#[test]
fn no_auth_flags_configure_no_auth() {
    assert_eq!(
        settings_from(&[]).unwrap(),
        WebsocketAuthSettings::default()
    );
}

// ---------------------------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------------------------

fn write_secret(dir: &TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn signed_bearer_policy(issuer: Option<&str>, audience: Option<&str>) -> WebsocketAuthPolicy {
    let dir = TempDir::new().unwrap();
    let secret = write_secret(&dir, "secret", &format!("{SHARED_SECRET}\n"));
    policy_from_settings(&WebsocketAuthSettings::new(
        WebsocketAuthConfig::SignedBearerToken {
            shared_secret_file: secret,
            issuer: issuer.map(str::to_owned),
            audience: audience.map(str::to_owned),
            max_clock_skew_seconds: 30,
        },
    ))
    .unwrap()
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

/// Codex's `signed_token` test helper: a JWT signed with HS256 by hand, independent of the library
/// that verifies it.
fn signed_token(shared_secret: &[u8], claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims_segment = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let payload = format!("{header}.{claims_segment}");
    let mut mac = HmacSha256::new_from_slice(shared_secret).unwrap();
    mac.update(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload}.{signature}")
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Codex's `detects_unauthenticated_non_loopback_listener`.
#[test]
fn detects_unauthenticated_non_loopback_listener() {
    let open = WebsocketAuthPolicy::default();
    assert!(is_unauthenticated_non_loopback_listener(
        "0.0.0.0:8765".parse().unwrap(),
        &open,
    ));
    assert!(!is_unauthenticated_non_loopback_listener(
        "127.0.0.1:8765".parse().unwrap(),
        &open,
    ));
    let token = policy_from_settings(&WebsocketAuthSettings::new(
        WebsocketAuthConfig::CapabilityToken {
            source: WebsocketCapabilityTokenSource::TokenSha256 {
                token_sha256: [0; 32],
            },
        },
    ))
    .unwrap();
    assert!(!is_unauthenticated_non_loopback_listener(
        "0.0.0.0:8765".parse().unwrap(),
        &token,
    ));
}

/// Codex's `capability_token_hash_policy_authorizes_matching_bearer_token`.
#[test]
fn capability_token_hash_policy_authorizes_matching_bearer_token() {
    let policy = policy_from_settings(&WebsocketAuthSettings::new(
        WebsocketAuthConfig::CapabilityToken {
            source: WebsocketCapabilityTokenSource::TokenSha256 {
                token_sha256: Sha256::digest(b"super-secret-token").into(),
            },
        },
    ))
    .unwrap();
    authorize_upgrade(&bearer("super-secret-token"), &policy).unwrap();

    let error = authorize_upgrade(&bearer("wrong-token"), &policy).unwrap_err();
    assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);
    assert_eq!(error.message(), "invalid websocket bearer token");
}

/// A token file is read once and compared without its surrounding whitespace; the header must be a
/// `Bearer` credential, its scheme in any case.
#[test]
fn capability_token_file_policy_reads_the_token_once() {
    let dir = TempDir::new().unwrap();
    let file = write_secret(&dir, "token", "  super-secret-token\n");
    let policy = policy_from_settings(&WebsocketAuthSettings::new(
        WebsocketAuthConfig::CapabilityToken {
            source: WebsocketCapabilityTokenSource::TokenFile {
                token_file: file.clone(),
            },
        },
    ))
    .unwrap();
    std::fs::write(&file, "another-token").unwrap();

    authorize_upgrade(&bearer("super-secret-token"), &policy).unwrap();
    let mut lower = HeaderMap::new();
    lower.insert(
        AUTHORIZATION,
        HeaderValue::from_static("bearer super-secret-token"),
    );
    authorize_upgrade(&lower, &policy).unwrap();

    for (headers, message) in [
        (HeaderMap::new(), "missing websocket bearer token"),
        (
            HeaderMap::from_iter([(AUTHORIZATION, HeaderValue::from_static("Basic abc"))]),
            "invalid authorization header",
        ),
        (
            HeaderMap::from_iter([(AUTHORIZATION, HeaderValue::from_static("Bearer"))]),
            "invalid authorization header",
        ),
        (
            HeaderMap::from_iter([(AUTHORIZATION, HeaderValue::from_static("Bearer   "))]),
            "invalid authorization header",
        ),
    ] {
        let error = authorize_upgrade(&headers, &policy).unwrap_err();
        assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);
        assert_eq!(error.message(), message);
    }
}

/// A policy that requires nothing admits a request without credentials.
#[test]
fn the_default_policy_admits_everyone() {
    authorize_upgrade(&HeaderMap::new(), &WebsocketAuthPolicy::default()).unwrap();
}

/// Codex's `signed_bearer_token_verification_rejects_tampering`.
#[test]
fn signed_bearer_token_verification_rejects_tampering() {
    let token = signed_token(SHARED_SECRET.as_bytes(), &json!({"exp": now() + 60}));
    let tampered = token.replace(".eyJleHAi", ".eyJleHBi");
    assert_ne!(tampered, token);
    let error =
        authorize_upgrade(&bearer(&tampered), &signed_bearer_policy(None, None)).unwrap_err();
    assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);
}

/// Codex's `signed_bearer_token_verification_accepts_valid_token`.
#[test]
fn signed_bearer_token_verification_accepts_valid_token() {
    let token = signed_token(
        SHARED_SECRET.as_bytes(),
        &json!({"exp": now() + 60, "iss": "issuer", "aud": "audience"}),
    );
    authorize_upgrade(
        &bearer(&token),
        &signed_bearer_policy(Some("issuer"), Some("audience")),
    )
    .unwrap();
}

/// Codex's `signed_bearer_token_verification_accepts_multiple_audiences`.
#[test]
fn signed_bearer_token_verification_accepts_multiple_audiences() {
    let token = signed_token(
        SHARED_SECRET.as_bytes(),
        &json!({"exp": now() + 60, "aud": ["other-audience", "audience"]}),
    );
    authorize_upgrade(
        &bearer(&token),
        &signed_bearer_policy(None, Some("audience")),
    )
    .unwrap();
}

/// Codex's `signed_bearer_token_verification_rejects_alg_none_tokens`.
#[test]
fn signed_bearer_token_verification_rejects_alg_none_tokens() {
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp": now() + 60})).unwrap());
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let token = format!("{header}.{claims}.");
    let error = authorize_upgrade(&bearer(&token), &signed_bearer_policy(None, None)).unwrap_err();
    assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);
}

/// Codex's `signed_bearer_token_verification_rejects_missing_exp`.
#[test]
fn signed_bearer_token_verification_rejects_missing_exp() {
    let token = signed_token(SHARED_SECRET.as_bytes(), &json!({"iss": "issuer"}));
    let error = authorize_upgrade(&bearer(&token), &signed_bearer_policy(None, None)).unwrap_err();
    assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);
}

/// The claims checks of Codex's `websocket_transport_verifies_signed_short_lived_bearer_tokens`,
/// each refused with its own message.
#[test]
fn signed_bearer_token_verification_checks_each_claim() {
    let policy = signed_bearer_policy(Some("codex-enroller"), Some("codex-app-server"));
    let claims = |overrides: Value| {
        let mut claims =
            json!({"exp": now() + 60, "iss": "codex-enroller", "aud": "codex-app-server"});
        claims
            .as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        claims
    };
    for (token, message) in [
        (
            signed_token(
                SHARED_SECRET.as_bytes(),
                &claims(json!({"exp": now() - 3600})),
            ),
            "expired websocket jwt",
        ),
        ("not-a-jwt".to_owned(), "invalid websocket jwt"),
        (
            signed_token(
                SHARED_SECRET.as_bytes(),
                &claims(json!({"nbf": now() + 3600})),
            ),
            "websocket jwt is not valid yet",
        ),
        (
            signed_token(
                SHARED_SECRET.as_bytes(),
                &claims(json!({"iss": "someone-else"})),
            ),
            "websocket jwt issuer mismatch",
        ),
        (
            signed_token(
                SHARED_SECRET.as_bytes(),
                &claims(json!({"aud": "wrong-audience"})),
            ),
            "websocket jwt audience mismatch",
        ),
        (
            signed_token(b"fedcba9876543210fedcba9876543210", &claims(json!({}))),
            "invalid websocket jwt",
        ),
    ] {
        let error = authorize_upgrade(&bearer(&token), &policy).unwrap_err();
        assert_eq!(
            (error.status_code(), error.message()),
            (StatusCode::UNAUTHORIZED, message)
        );
    }

    // Within the clock skew, an expired token and one not yet valid are still accepted. Every
    // timestamp here sits at least 20 seconds from the 30-second skew boundary, so a slow run
    // cannot cross it between signing and checking.
    for overrides in [json!({"exp": now() - 10}), json!({"nbf": now() + 10})] {
        let token = signed_token(SHARED_SECRET.as_bytes(), &claims(overrides));
        authorize_upgrade(&bearer(&token), &policy).unwrap();
    }
}

/// Codex's `validate_signed_bearer_secret_rejects_short_secret`, through loading the policy, as
/// Codex's `websocket_transport_rejects_short_signed_bearer_secret_configuration` reaches it.
#[test]
fn validate_signed_bearer_secret_rejects_short_secret() {
    let dir = TempDir::new().unwrap();
    let secret = write_secret(&dir, "secret", "too-short\n");
    let error = policy_from_settings(&WebsocketAuthSettings::new(
        WebsocketAuthConfig::SignedBearerToken {
            shared_secret_file: secret,
            issuer: None,
            audience: None,
            max_clock_skew_seconds: 30,
        },
    ))
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(
        error.to_string().contains("must be at least 32 bytes"),
        "unexpected error: {error}"
    );
}

/// A secret file that is missing or holds only whitespace fails to load.
#[test]
fn secret_files_must_exist_and_not_be_empty() {
    let dir = TempDir::new().unwrap();
    let empty = write_secret(&dir, "empty", " \n");
    let load = |token_file: &Path| {
        policy_from_settings(&WebsocketAuthSettings::new(
            WebsocketAuthConfig::CapabilityToken {
                source: WebsocketCapabilityTokenSource::TokenFile {
                    token_file: token_file.to_owned(),
                },
            },
        ))
        .unwrap_err()
    };

    let error = load(&empty);
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.to_string().contains("must not be empty"), "{error}");
    let error = load(&dir.path().join("missing"));
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(
        error
            .to_string()
            .contains("failed to read websocket auth secret"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------------------------

/// What the echo server saw of a connection's life.
#[derive(Debug, PartialEq)]
enum Seen {
    Opened {
        connection: ConnectionId,
        origin: ConnectionOrigin,
        can_disconnect: bool,
    },
    Closed(ConnectionId),
}

/// A server that answers each request with its connection and method. `ack` is answered and then
/// followed by a `written` notification once the transport reports the answer written;
/// `disconnect` drops the connection; a notification is echoed back as `notified`.
fn spawn_echo_server() -> (mpsc::Sender<TransportEvent>, mpsc::UnboundedReceiver<Seen>) {
    let (transport_event_tx, mut events) = mpsc::channel(CHANNEL_CAPACITY);
    let (seen_tx, seen) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut connections = HashMap::new();
        while let Some(event) = events.recv().await {
            match event {
                TransportEvent::ConnectionOpened {
                    connection_id,
                    origin,
                    writer,
                    disconnect_sender,
                    ..
                } => {
                    let _ = seen_tx.send(Seen::Opened {
                        connection: connection_id,
                        origin,
                        can_disconnect: disconnect_sender.is_some(),
                    });
                    connections.insert(connection_id, (writer, disconnect_sender));
                }
                TransportEvent::IncomingMessage {
                    connection_id,
                    message,
                } => {
                    let (writer, disconnect): &(
                        mpsc::Sender<QueuedOutgoingMessage>,
                        Option<CancellationToken>,
                    ) = &connections[&connection_id];
                    match message {
                        JsonRpcMessage::Request(request) if request.method() == "disconnect" => {
                            disconnect.as_ref().unwrap().cancel();
                        }
                        JsonRpcMessage::Request(request) if request.method() == "ack" => {
                            let (written_tx, written_rx) = oneshot::channel();
                            let answer = JsonRpcResponse::new(request.id().clone(), json!("acked"));
                            writer
                                .send(
                                    QueuedOutgoingMessage::new(answer)
                                        .with_write_complete(written_tx),
                                )
                                .await
                                .unwrap();
                            written_rx.await.unwrap();
                            let written = JsonRpcNotification::new("written");
                            writer
                                .send(QueuedOutgoingMessage::new(written))
                                .await
                                .unwrap();
                        }
                        JsonRpcMessage::Request(request) => {
                            let result = json!({
                                "connection": connection_id.get(),
                                "method": request.method(),
                            });
                            let answer = JsonRpcResponse::new(request.id().clone(), result);
                            writer
                                .send(QueuedOutgoingMessage::new(answer))
                                .await
                                .unwrap();
                        }
                        JsonRpcMessage::Notification(notified) => {
                            let echo = JsonRpcNotification::new("notified")
                                .with_params(json!({"method": notified.method()}));
                            writer.send(QueuedOutgoingMessage::new(echo)).await.unwrap();
                        }
                        other => panic!("unexpected message {other:?}"),
                    }
                }
                TransportEvent::ConnectionClosed { connection_id } => {
                    connections.remove(&connection_id);
                    let _ = seen_tx.send(Seen::Closed(connection_id));
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
    });
    (transport_event_tx, seen)
}

/// A listener on loopback with `policy`, serving the echo server.
struct Listener {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
    seen: mpsc::UnboundedReceiver<Seen>,
}

impl Listener {
    async fn start(policy: WebsocketAuthPolicy) -> Self {
        let (transport_event_tx, seen) = spawn_echo_server();
        let shutdown = CancellationToken::new();
        let (addr, task) = start_websocket_acceptor(
            (Ipv4Addr::LOCALHOST, 0).into(),
            transport_event_tx,
            shutdown.clone(),
            policy,
        )
        .await
        .unwrap();
        Self {
            addr,
            shutdown,
            task,
            seen,
        }
    }

    async fn connect(&mut self) -> (WsClient, ConnectionId) {
        let (ws, response) = connect_async(self.request(None, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let Seen::Opened {
            connection,
            origin,
            can_disconnect,
        } = self.next_seen().await
        else {
            panic!("the connection opens first");
        };
        assert_eq!(origin, ConnectionOrigin::WebSocket);
        assert!(can_disconnect);
        (ws, connection)
    }

    fn request(
        &self,
        bearer_token: Option<&str>,
        origin: Option<&str>,
    ) -> tungstenite::handshake::client::Request {
        let mut request = format!("ws://{}", self.addr).into_client_request().unwrap();
        if let Some(bearer_token) = bearer_token {
            request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {bearer_token}")).unwrap(),
            );
        }
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        request
    }

    /// Codex's `assert_websocket_connect_rejected_with_headers`.
    async fn assert_rejected(
        &self,
        bearer_token: Option<&str>,
        origin: Option<&str>,
        expected_status: StatusCode,
    ) {
        match connect_async(self.request(bearer_token, origin)).await {
            Ok((_, response)) => panic!(
                "expected websocket handshake rejection, got {}",
                response.status()
            ),
            Err(tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), expected_status);
            }
            Err(error) => panic!("expected http rejection during websocket handshake: {error}"),
        }
    }

    async fn next_seen(&mut self) -> Seen {
        timeout(PATIENCE, self.seen.recv()).await.unwrap().unwrap()
    }

    /// The status of `GET path` on the listener, with an `Origin` header if given.
    async fn http_get(&self, path: &str, origin: Option<&str>) -> u16 {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let origin = origin.map_or_else(String::new, |origin| format!("Origin: {origin}\r\n"));
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{origin}Connection: close\r\n\r\n",
            self.addr
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        timeout(PATIENCE, stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or_else(|| panic!("not an HTTP response: {response:?}"))
    }
}

async fn send(ws: &mut WsClient, message: impl Into<JsonRpcMessage>) {
    let text = serde_json::to_string(&message.into()).unwrap();
    ws.send(Message::text(text)).await.unwrap();
}

/// The next frame from the server.
async fn next_frame(ws: &mut WsClient) -> Option<Message> {
    timeout(PATIENCE, ws.next())
        .await
        .expect("the server answers in time")
        .map(Result::unwrap)
}

/// The next JSON message from the server, skipping control frames.
async fn next_json(ws: &mut WsClient) -> Value {
    loop {
        match next_frame(ws).await {
            Some(Message::Text(text)) => return serde_json::from_str(&text).unwrap(),
            Some(Message::Ping(_) | Message::Pong(_)) => {}
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

async fn assert_no_message(ws: &mut WsClient, wait_for: Duration) {
    if let Ok(frame) = timeout(wait_for, ws.next()).await {
        panic!("unexpected frame {frame:?}");
    }
}

/// The transport part of Codex's `websocket_transport_routes_per_connection_handshake_and_responses`:
/// each connection's answers reach only that connection, and the same request id on two
/// connections is routed independently.
#[tokio::test]
async fn websocket_routes_answers_per_connection() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (mut ws1, first) = listener.connect().await;
    let (mut ws2, second) = listener.connect().await;
    assert_ne!(first, second);

    send(&mut ws1, JsonRpcRequest::new(1, "initialize")).await;
    assert_eq!(
        next_json(&mut ws1).await,
        json!({"id": 1, "result": {"connection": first.get(), "method": "initialize"}})
    );
    assert_no_message(&mut ws2, Duration::from_millis(250)).await;

    send(&mut ws1, JsonRpcRequest::new(77, "config/read")).await;
    send(&mut ws2, JsonRpcRequest::new(77, "config/read")).await;
    assert_eq!(
        next_json(&mut ws1).await,
        json!({"id": 77, "result": {"connection": first.get(), "method": "config/read"}})
    );
    assert_eq!(
        next_json(&mut ws2).await,
        json!({"id": 77, "result": {"connection": second.get(), "method": "config/read"}})
    );
}

/// Codex's `websocket_transport_serves_health_endpoints_on_same_listener`.
#[tokio::test]
async fn websocket_serves_health_endpoints_on_the_same_listener() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    assert_eq!(listener.http_get("/readyz", None).await, 200);
    assert_eq!(listener.http_get("/healthz", None).await, 200);

    let (mut ws, _) = listener.connect().await;
    send(&mut ws, JsonRpcRequest::new(1, "initialize")).await;
    assert_eq!(next_json(&mut ws).await["id"], 1);
}

/// Codex's `websocket_transport_rejects_browser_origin_without_auth`: a request with an `Origin`
/// header is refused, on every path.
#[tokio::test]
async fn websocket_rejects_a_browser_origin() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (ws, _) = listener.connect().await;
    drop(ws);

    listener
        .assert_rejected(None, Some("https://evil.example"), StatusCode::FORBIDDEN)
        .await;
    assert_eq!(
        listener
            .http_get("/healthz", Some("https://evil.example"))
            .await,
        403
    );
}

/// Codex's `websocket_transport_rejects_missing_and_invalid_capability_tokens`, on loopback: the
/// policy is the same whatever the address.
#[tokio::test]
async fn websocket_rejects_missing_and_invalid_capability_tokens() {
    let dir = TempDir::new().unwrap();
    let token_file = write_secret(&dir, "app-server-token", "super-secret-token\n");
    let policy = policy_from_settings(&WebsocketAuthSettings::new(
        WebsocketAuthConfig::CapabilityToken {
            source: WebsocketCapabilityTokenSource::TokenFile { token_file },
        },
    ))
    .unwrap();
    let mut listener = Listener::start(policy).await;

    listener
        .assert_rejected(None, None, StatusCode::UNAUTHORIZED)
        .await;
    listener
        .assert_rejected(Some("wrong-token"), None, StatusCode::UNAUTHORIZED)
        .await;

    let (mut ws, _) = connect_async(listener.request(Some("super-secret-token"), None))
        .await
        .unwrap();
    // The refused handshakes never opened a connection.
    assert!(matches!(listener.next_seen().await, Seen::Opened { .. }));
    assert!(listener.seen.try_recv().is_err());
    send(&mut ws, JsonRpcRequest::new(1, "initialize")).await;
    assert_eq!(next_json(&mut ws).await["id"], 1);
}

/// The transport part of Codex's `websocket_transport_verifies_signed_short_lived_bearer_tokens`.
#[tokio::test]
async fn websocket_verifies_signed_bearer_tokens() {
    let mut listener = Listener::start(signed_bearer_policy(
        Some("codex-enroller"),
        Some("codex-app-server"),
    ))
    .await;
    let expired = signed_token(
        SHARED_SECRET.as_bytes(),
        &json!({"exp": now() - 3600, "iss": "codex-enroller", "aud": "codex-app-server"}),
    );
    listener
        .assert_rejected(Some(&expired), None, StatusCode::UNAUTHORIZED)
        .await;

    let valid = signed_token(
        SHARED_SECRET.as_bytes(),
        &json!({"exp": now() + 60, "iss": "codex-enroller", "aud": "codex-app-server"}),
    );
    let (mut ws, _) = connect_async(listener.request(Some(&valid), None))
        .await
        .unwrap();
    assert!(matches!(listener.next_seen().await, Seen::Opened { .. }));
    assert!(listener.seen.try_recv().is_err());
    send(&mut ws, JsonRpcRequest::new(1, "initialize")).await;
    assert_eq!(next_json(&mut ws).await["id"], 1);
}

/// Codex's `websocket_transport_rejects_unauthenticated_non_loopback_startup`.
#[tokio::test]
async fn websocket_refuses_an_unauthenticated_non_loopback_listener() {
    let (transport_event_tx, _seen) = spawn_echo_server();
    let error = start_websocket_acceptor(
        (Ipv4Addr::UNSPECIFIED, 0).into(),
        transport_event_tx,
        CancellationToken::new(),
        WebsocketAuthPolicy::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("refusing to start non-loopback websocket listener"),
        "unexpected error: {error}"
    );
}

/// Notifications reach the server, and a message queued with a write-complete signal gets it once
/// the frame has been sent.
#[tokio::test]
async fn websocket_forwards_notifications_and_signals_completed_writes() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (mut ws, _) = listener.connect().await;

    send(&mut ws, JsonRpcNotification::new("initialized")).await;
    assert_eq!(
        next_json(&mut ws).await,
        json!({"method": "notified", "params": {"method": "initialized"}})
    );
    send(&mut ws, JsonRpcRequest::new(2, "ack")).await;
    assert_eq!(
        next_json(&mut ws).await,
        json!({"id": 2, "result": "acked"})
    );
    assert_eq!(next_json(&mut ws).await, json!({"method": "written"}));
}

/// A ping is answered with a pong carrying its payload; a binary frame and a text frame that is not
/// a message are dropped and the connection stays open.
#[tokio::test]
async fn websocket_answers_pings_and_drops_what_it_cannot_read() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (mut ws, _) = listener.connect().await;

    ws.send(Message::Ping(b"are you there".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        next_frame(&mut ws).await,
        Some(Message::Pong(b"are you there".to_vec().into()))
    );

    ws.send(Message::Binary(b"{}".to_vec().into()))
        .await
        .unwrap();
    ws.send(Message::text("not json")).await.unwrap();
    send(&mut ws, JsonRpcRequest::new(3, "thread/list")).await;
    assert_eq!(next_json(&mut ws).await["id"], 3);
}

/// A close frame from the client ends the connection, and the server is told.
#[tokio::test]
async fn websocket_reports_a_connection_the_client_closes() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (mut ws, connection) = listener.connect().await;

    ws.close(None).await.unwrap();
    assert_eq!(listener.next_seen().await, Seen::Closed(connection));
}

/// A client that goes away without a close frame is reported closed too.
#[tokio::test]
async fn websocket_reports_a_client_that_vanishes() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (ws, connection) = listener.connect().await;

    drop(ws);
    assert_eq!(listener.next_seen().await, Seen::Closed(connection));
}

/// Cancelling the disconnect token drops the connection: the client's stream ends and the server is
/// told the connection closed.
#[tokio::test]
async fn websocket_drops_a_connection_the_server_disconnects() {
    let mut listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let (mut ws, connection) = listener.connect().await;
    let (mut other, _) = listener.connect().await;

    send(&mut ws, JsonRpcRequest::new(4, "disconnect")).await;
    assert_eq!(listener.next_seen().await, Seen::Closed(connection));
    // Codex drops the socket without a closing handshake.
    let end = timeout(PATIENCE, ws.next()).await.unwrap();
    assert!(
        matches!(
            end,
            None | Some(Ok(Message::Close(_)))
                | Some(Err(tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                )))
        ),
        "unexpected frame {end:?}"
    );

    send(&mut other, JsonRpcRequest::new(5, "thread/list")).await;
    assert_eq!(next_json(&mut other).await["id"], 5);
}

/// Once the shutdown token is cancelled the listener stops accepting and its task finishes.
#[tokio::test]
async fn websocket_stops_accepting_on_shutdown() {
    let listener = Listener::start(WebsocketAuthPolicy::default()).await;
    let addr = listener.addr;

    listener.shutdown.cancel();
    timeout(PATIENCE, listener.task).await.unwrap().unwrap();
    assert!(TcpStream::connect(addr).await.is_err());
}
