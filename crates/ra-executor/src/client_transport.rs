// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Opens the environment owner's selected RPC transport.
#![allow(
    clippy::too_many_lines,
    reason = "Keep the source lifecycle and policy lowering order visible in one operation."
)]
use crate::{
    ExecServerError,
    client_api::{ExecServerClientConnectOptions, ExecServerTransportParams},
    connection::JsonRpcConnection,
};
use futures::future::BoxFuture;
use ra_http::{
    HttpClientFactory,
    websocket::{WebSocketConnector, WebSocketTlsMode},
};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    time::timeout,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
const MAX_STDIO_STDERR_LOG_LINE_LEN: u64 = 8 * 1024;
const ENVIRONMENT_CLIENT_NAME: &str = "codex-environment";
pub(crate) fn open_transport(
    transport: ExecServerTransportParams,
    factory: HttpClientFactory,
    resume_session_id: Option<String>,
) -> BoxFuture<'static, Result<(JsonRpcConnection, ExecServerClientConnectOptions), ExecServerError>>
{
    Box::pin(async move {
        let transport = match transport {
            ExecServerTransportParams::Deferred(mut deferred) => {
                let result = deferred
                    .readiness
                    .wait_for(Option::is_some)
                    .await
                    .map_err(|_| {
                        ExecServerError::Disconnected(
                            "environment unavailable: environment provisioning ended before completion"
                                .to_string(),
                        )
                    })?
                    .clone()
                    .ok_or_else(|| {
                        ExecServerError::Disconnected(
                            "environment unavailable: provisioning remained pending after completion"
                                .to_string(),
                        )
                    })?;
                result.map_err(ExecServerError::ProvisioningFailed)?;
                deferred.transport
            }
            transport => transport,
        };
        match transport {
            ExecServerTransportParams::Deferred(_) => Err(ExecServerError::Protocol(
                "nested deferred exec-server transports are unsupported".to_string(),
            )),
            ExecServerTransportParams::WebSocketUrl {
                websocket_url,
                connect_timeout,
                initialize_timeout,
                http_headers,
            } => {
                reject_rendezvous_harness_url(&websocket_url)?;
                ra_http::ensure_rustls_crypto_provider();
                let mut request =
                    websocket_url
                        .as_str()
                        .into_client_request()
                        .map_err(|source| ExecServerError::WebSocketConnect {
                            url: websocket_url.clone(),
                            source,
                        })?;
                request.headers_mut().extend(http_headers.clone());
                let connector = WebSocketConnector::new_with_tls_mode(
                    &factory,
                    WebSocketTlsMode::TungsteniteDefault,
                )
                .map_err(|error| ExecServerError::WebSocketConfiguration(error.to_string()))?;
                let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
                let (stream, _response) = timeout(connect_timeout, async {
                    if !http_headers.is_empty() && websocket_url.starts_with("ws://") {
                        connector.connect_loopback_direct(request, config).await
                    } else {
                        connector.connect(request, config).await
                    }
                })
                .await
                .map_err(|_| ExecServerError::WebSocketConnectTimeout {
                    url: websocket_url.clone(),
                    timeout: connect_timeout,
                })?
                .map_err(|source| ExecServerError::WebSocketConnect {
                    url: websocket_url.clone(),
                    source,
                })?;
                Ok((
                    JsonRpcConnection::from_websocket(
                        stream,
                        format!("exec-server websocket {websocket_url}"),
                    ),
                    ExecServerClientConnectOptions {
                        client_name: ENVIRONMENT_CLIENT_NAME.to_string(),
                        initialize_timeout,
                        resume_session_id,
                    },
                ))
            }
            ExecServerTransportParams::StdioCommand {
                command,
                initialize_timeout,
            } => {
                let mut process = crate::background_command(&command.program);
                process.args(command.args).envs(command.env);
                crate::shell_environment::scrub_non_inheritable_env_vars(process.as_std_mut());
                if let Some(cwd) = command.cwd {
                    process.current_dir(cwd);
                }
                #[cfg(unix)]
                process.process_group(0);
                let mut child = process
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(ExecServerError::Spawn)?;
                let stdin = child.stdin.take().ok_or_else(|| {
                    ExecServerError::Protocol(
                        "spawned exec-server command has no stdin".to_string(),
                    )
                })?;
                let stdout = child.stdout.take().ok_or_else(|| {
                    ExecServerError::Protocol(
                        "spawned exec-server command has no stdout".to_string(),
                    )
                })?;
                if let Some(stderr) = child.stderr.take() {
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(stderr);
                        let mut line = Vec::new();
                        loop {
                            line.clear();
                            match (&mut reader)
                                .take(MAX_STDIO_STDERR_LOG_LINE_LEN)
                                .read_until(b'\n', &mut line)
                                .await
                            {
                                Ok(0) => break,
                                Ok(_) => {
                                    let line = line.strip_suffix(b"\n").unwrap_or(&line);
                                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                                    let line = String::from_utf8_lossy(line);
                                    tracing::debug!("exec-server stdio stderr: {line}");
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        "failed to read exec-server stdio stderr: {err}"
                                    );
                                    break;
                                }
                            }
                        }
                    });
                }
                Ok((
                    JsonRpcConnection::client_from_stdio(
                        stdout,
                        stdin,
                        "exec-server stdio command".to_string(),
                    )
                    .with_child_process(child),
                    ExecServerClientConnectOptions {
                        client_name: ENVIRONMENT_CLIENT_NAME.to_string(),
                        initialize_timeout,
                        resume_session_id,
                    },
                ))
            }
        }
    })
}

fn is_rendezvous_harness_url(websocket_url: &str) -> bool {
    let Some((_path, query)) = websocket_url.split_once('?') else {
        return false;
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| key == "role" && value == "harness")
}
/// Rejects relay framing endpoints until that transport is implemented.
pub(crate) fn reject_rendezvous_harness_url(url: &str) -> Result<(), ExecServerError> {
    if is_rendezvous_harness_url(url) {
        return Err(ExecServerError::Protocol(
            "rendezvous harness URLs require relay framing, which is not supported".to_string(),
        ));
    }
    Ok(())
}
