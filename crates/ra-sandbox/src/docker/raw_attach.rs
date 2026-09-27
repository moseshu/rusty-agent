//! Attaching to a command on a terminal, over a connection of its own.
//!
//! The daemon sends a terminal's output as it comes, without the eight-byte headers it puts in
//! front of each frame of a command that has no terminal. bollard's exec attachment does not use
//! the terminal flag when it reads: it takes a read that starts with byte 0, 1 or 2 as a frame
//! header, drops those eight bytes, and waits for as many bytes as the header's last four claim.
//! Terminal output that happens to start that way is cut, or held back until the command exits. The
//! reference reads the terminal's socket as it comes (`docker-py`'s `frames_iter` with `tty=True`).
//! bollard 0.21 has no public call that hands the upgraded connection back unread. So a terminal
//! attachment opens its own connection to the daemon, sends the same upgrade request bollard would,
//! and reads the upgraded stream raw. Attachments without a terminal stay with bollard, whose frame
//! decoding is right for them.
//!
//! The daemon's address is read the way bollard's `connect_with_host` reads `DOCKER_HOST`: a Unix
//! socket, plain TCP, or TLS for `https://` and for `tcp://` when `DOCKER_TLS_VERIFY` is set, with the
//! certificates from `DOCKER_CERT_PATH` (or `DOCKER_CONFIG`, or `~/.docker`). A named pipe is not
//! supported here.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use hyper::header::{CONNECTION, CONTENT_TYPE, HOST, UPGRADE};
use hyper_util::rt::TokioIo;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use super::api::{DockerApiError, ExecAttachment, ExecFrame, ExecStreamKind};

/// The address a client connects to when `DOCKER_HOST` is not set: bollard's `DEFAULT_DOCKER_HOST`.
#[cfg(unix)]
pub(crate) const DEFAULT_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
/// The address a client connects to when `DOCKER_HOST` is not set: bollard's `DEFAULT_DOCKER_HOST`.
#[cfg(windows)]
pub(crate) const DEFAULT_DOCKER_HOST: &str = "npipe:////./pipe/docker_engine";

/// How much one read of a terminal's output takes.
const READ_CHUNK_BYTES: usize = 16_384;

/// Where the daemon listens, as far as a terminal attachment needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DaemonEndpoint {
    /// A Unix socket at this path.
    Unix(PathBuf),
    /// Plain HTTP to `host:port`.
    Tcp(String),
    /// HTTPS to `host:port`, verified against the certificates in `cert_dir`.
    Tls {
        authority: String,
        cert_dir: PathBuf,
    },
    /// An address a terminal attachment cannot reach, named for the error it gives.
    Unsupported(String),
}

impl DaemonEndpoint {
    /// Reads a `DOCKER_HOST` value as bollard's `connect_with_host` does.
    ///
    /// TLS is chosen by the scheme and `DOCKER_TLS_VERIFY`, and its certificate directory is found,
    /// from the environment as it is when the client is built — when bollard reads it too.
    pub(crate) fn from_host(host: &str) -> Self {
        if let Some(path) = host.strip_prefix("unix://") {
            return Self::Unix(PathBuf::from(path));
        }
        let tls = |authority: &str| match cert_dir() {
            Some(cert_dir) => Self::Tls {
                authority: authority.to_owned(),
                cert_dir,
            },
            None => Self::Unsupported(host.to_owned()),
        };
        if let Some(authority) = host
            .strip_prefix("tcp://")
            .or_else(|| host.strip_prefix("http://"))
        {
            return if std::env::var_os("DOCKER_TLS_VERIFY").is_some() {
                tls(authority)
            } else {
                Self::Tcp(authority.to_owned())
            };
        }
        if let Some(authority) = host.strip_prefix("https://") {
            return tls(authority);
        }
        Self::Unsupported(host.to_owned())
    }
}

/// The certificate directory, as bollard's `default_cert_path` finds it.
fn cert_dir() -> Option<PathBuf> {
    std::env::var_os("DOCKER_CERT_PATH")
        .or_else(|| std::env::var_os("DOCKER_CONFIG"))
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".docker")))
}

/// Something a request can be sent over.
trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

/// Starts a command set up with a terminal and attaches to it raw.
///
/// The request goes to the unversioned path, as bollard's do: bollard formats a `/v<major>.<minor>`
/// prefix and then joins the absolute endpoint path onto it, which replaces the prefix, so every
/// request it sends is served at the daemon's own API version. A versioned path here would be
/// refused by a daemon older than the client's default version while bollard's calls to the same
/// daemon succeed.
///
/// # Errors
///
/// Returns a not-found error when the daemon does not know the exec, the daemon's status for any
/// other refusal, and a transport error when it cannot be reached or the upgrade fails.
pub(crate) async fn start_exec_raw(
    endpoint: &DaemonEndpoint,
    exec_id: &str,
) -> Result<ExecAttachment, DockerApiError> {
    let (connection, host) = connect(endpoint).await?;
    let (mut sender, driver) = hyper::client::conn::http1::handshake(TokioIo::new(connection))
        .await
        .map_err(|error| transport(&error))?;
    tokio::spawn(async move {
        let _ = driver.with_upgrades().await;
    });

    let body = serde_json::to_vec(&serde_json::json!({ "Detach": false, "Tty": true }))
        .map_err(|error| DockerApiError::transport(error.to_string()))?;
    let request = hyper::Request::post(format!("/exec/{exec_id}/start"))
        .header(HOST, host)
        .header(CONTENT_TYPE, "application/json")
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "tcp")
        .body(Full::new(Bytes::from(body)))
        .map_err(|error| DockerApiError::transport(error.to_string()))?;
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|error| transport(&error))?;

    let status = response.status();
    if status != StatusCode::SWITCHING_PROTOCOLS {
        return Err(refusal(status, response.into_body().collect().await));
    }
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .map_err(|error| transport(&error))?;
    let (read, write) = tokio::io::split(TokioIo::new(upgraded));
    let output = futures::stream::unfold(Some(read), |reader| async move {
        let mut reader = reader?;
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        match reader.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((
                    Ok(ExecFrame::new(ExecStreamKind::Console, buffer)),
                    Some(reader),
                ))
            }
            Err(error) => Some((Err(DockerApiError::transport(error.to_string())), None)),
        }
    })
    .boxed();
    Ok(ExecAttachment::new(output, Box::pin(write)))
}

/// Opens a connection to the daemon, and the `Host` header requests over it carry.
async fn connect(
    endpoint: &DaemonEndpoint,
) -> Result<(Pin<Box<dyn Connection>>, String), DockerApiError> {
    match endpoint {
        #[cfg(unix)]
        DaemonEndpoint::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|error| io_failure(&path.display().to_string(), &error))?;
            // The daemon ignores the host of a request over its socket; this is what the Docker
            // CLI sends.
            Ok((Box::pin(stream), "docker".to_owned()))
        }
        #[cfg(not(unix))]
        DaemonEndpoint::Unix(path) => Err(DockerApiError::transport(format!(
            "a Unix socket is not available on this platform: {}",
            path.display()
        ))),
        DaemonEndpoint::Tcp(authority) => {
            let stream = tokio::net::TcpStream::connect(authority.as_str())
                .await
                .map_err(|error| io_failure(authority, &error))?;
            Ok((Box::pin(stream), authority.clone()))
        }
        DaemonEndpoint::Tls {
            authority,
            cert_dir,
        } => {
            let config = tls_config(cert_dir)?;
            let server_name = ServerName::try_from(host_of(authority).to_owned())
                .map_err(|error| DockerApiError::transport(error.to_string()))?;
            let stream = tokio::net::TcpStream::connect(authority.as_str())
                .await
                .map_err(|error| io_failure(authority, &error))?;
            let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(server_name, stream)
                .await
                .map_err(|error| io_failure(authority, &error))?;
            Ok((Box::pin(stream), authority.clone()))
        }
        DaemonEndpoint::Unsupported(host) => Err(DockerApiError::transport(format!(
            "attaching to a terminal is not supported for this daemon address: {host}"
        ))),
    }
}

/// The TLS configuration bollard's `connect_with_ssl` builds for a certificate directory.
///
/// The platform's roots plus `ca.pem` are trusted. `cert.pem` and `key.pem` are presented as the
/// client's certificate when both read; bollard's resolver likewise presents none when they do not.
fn tls_config(cert_dir: &Path) -> Result<rustls::ClientConfig, DockerApiError> {
    let failure =
        |what: &str, path: &Path| DockerApiError::transport(format!("{what}: {}", path.display()));

    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if let Some(error) = native.errors.first() {
        return Err(DockerApiError::transport(format!(
            "could not load the platform's certificates: {error}"
        )));
    }
    for certificate in native.certs {
        roots
            .add(certificate)
            .map_err(|error| DockerApiError::transport(error.to_string()))?;
    }
    let ca_path = cert_dir.join("ca.pem");
    let ca = std::fs::read(&ca_path).map_err(|_| failure("could not read", &ca_path))?;
    let ca: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&ca)
        .collect::<Result<_, _>>()
        .map_err(|_| failure("could not parse", &ca_path))?;
    roots.add_parsable_certificates(ca);

    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| DockerApiError::transport(error.to_string()))?
    .with_root_certificates(roots);

    let certificates = CertificateDer::pem_file_iter(cert_dir.join("cert.pem"))
        .and_then(Iterator::collect::<Result<Vec<_>, _>>);
    let key = PrivateKeyDer::from_pem_file(cert_dir.join("key.pem"));
    match (certificates, key) {
        (Ok(certificates), Ok(key)) if !certificates.is_empty() => builder
            .with_client_auth_cert(certificates, key)
            .map_err(|error| DockerApiError::transport(error.to_string())),
        _ => Ok(builder.with_no_client_auth()),
    }
}

/// The host part of `host:port`, without the brackets of an IPv6 literal.
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split_once(']').map_or(rest, |(host, _)| host);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

/// A request that never got an answer, or an upgrade that failed.
fn transport(error: &hyper::Error) -> DockerApiError {
    DockerApiError::transport(error.to_string())
}

/// A connection that could not be made.
fn io_failure(address: &str, error: &std::io::Error) -> DockerApiError {
    DockerApiError::transport(format!("could not connect to {address}: {error}"))
}

/// The daemon's refusal to start the exec, with its message when the body carries one.
fn refusal(
    status: StatusCode,
    body: Result<http_body_util::Collected<Bytes>, hyper::Error>,
) -> DockerApiError {
    let body = body
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
    if status == StatusCode::NOT_FOUND {
        DockerApiError::not_found(message)
    } else {
        DockerApiError::api(status.as_u16(), message)
    }
}
