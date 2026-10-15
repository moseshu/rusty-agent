//! Control socket startup, guarded rendezvous paths, and WebSocket acceptance.
//!
//! Ported from Codex `44fe510`, `app-server-transport/src/transport/unix_socket.rs` and the Unix
//! half of `uds/src/lib.rs`. Paths use the existing absolute-path resolver rather than a new
//! wrapper type. Only Unix is supported here; Codex's Windows UDS and DACL adapter is not ported.

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use super::TransportEvent;
use crate::absolute_path;
use crate::transport::websocket::run_websocket_connection;
use futures::SinkExt;
use futures::StreamExt;
use ra_uds::{prepare_shared_daemon_socket_directory, shared_daemon_socket_directory};
use std::path::PathBuf;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::Response as HandshakeResponse;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::Response;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;

const CONTROL_SOCKET_MODE: u32 = 0o600;
// Advertise the effective incoming cap for single-frame messages so clients can
// reject oversized requests before the socket closes.
const MAX_UNFRAGMENTED_MESSAGE_BYTES_HEADER: &str =
    "x-codex-websocket-max-unfragmented-message-bytes";

/// Whether a local manager may request shutdown outside JSON-RPC.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonShutdownAccess {
    /// Refuse the shutdown endpoint with HTTP 403.
    Disabled,
    /// Accept the shutdown endpoint after the process-id handshake.
    Managed,
}

/// Publishes a private Unix socket and accepts WebSocket connections on it, as Codex does.
///
/// The absolute advertised path is a symlink to a short, protected physical path. Existing
/// parents keep their permissions; unsafe parents and occupied paths are refused. Cancelling
/// `shutdown_token` stops acceptance and removes the owned socket and alias. Established clients
/// keep running until disconnected by the server, as with the TCP WebSocket acceptor.
///
/// `DaemonShutdownAccess::Managed` enables `/daemon/shutdown`: the client sends this process's
/// decimal pid, receives that pid, and closes (or waits two seconds) before a shutdown event.
/// When paired with `AppServer`, pass the same token to `AppServer::with_graceful_restart` so
/// a shutdown request stops the acceptor after draining. This does not install a daemon manager
/// or restore its loaded threads.
pub async fn start_control_socket_acceptor(
    socket_path: impl AsRef<Path>,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    daemon_shutdown_access: DaemonShutdownAccess,
) -> IoResult<JoinHandle<()>> {
    let socket_path = absolute_path::from_absolute_path_checked(socket_path.as_ref())?;
    let (socket_path, rendezvous_path, _startup_lock) = {
        use std::os::unix::fs::MetadataExt;

        if let Some(parent) = socket_path.as_path().parent() {
            // The advertised path is only an alias; preserve existing parents.
            match tokio::fs::DirBuilder::new()
                .mode(0o700)
                .create(parent)
                .await
            {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let metadata = std::fs::metadata(parent)?;
            if !metadata.is_dir()
                || (metadata.uid() != 0 && metadata.uid() != rustix::process::geteuid().as_raw())
                || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
            {
                return Err(std::io::Error::new(
                    ErrorKind::PermissionDenied,
                    "socket parent must be owned by the user or root and prevent other users from replacing socket entries",
                ));
            }
        }
        prepare_shared_daemon_socket_directory()?;
        let physical_path = protected_socket_path(socket_path.as_path())?;
        let lock = acquire_app_server_startup_lock(physical_path.with_extension("lock")).await?;
        prepare_control_socket_path(socket_path.as_path()).await?;
        (physical_path, socket_path, lock)
    };
    prepare_control_socket_path(socket_path.as_path()).await?;
    let listener = UnixListener::bind(socket_path.as_path())?;
    let socket_guard = ControlSocketFileGuard {
        socket_path,
        rendezvous_path,
    };
    set_control_socket_permissions(socket_guard.socket_path.as_path()).await?;
    std::os::unix::fs::symlink(
        socket_guard.socket_path.as_path(),
        socket_guard.rendezvous_path.as_path(),
    )?;
    info!(
        socket_path = %socket_guard.socket_path.display(),
        "app-server control socket listening"
    );

    Ok(tokio::spawn(run_control_socket_acceptor(
        listener,
        transport_event_tx,
        shutdown_token,
        socket_guard,
        daemon_shutdown_access,
    )))
}

#[allow(clippy::result_large_err)] // Tungstenite requires an HTTP rejection response by value.
async fn run_control_socket_acceptor(
    listener: UnixListener,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    socket_guard: ControlSocketFileGuard,
    daemon_shutdown_access: DaemonShutdownAccess,
) {
    let _socket_guard = socket_guard;
    loop {
        let stream = tokio::select! {
            () = shutdown_token.cancelled() => {
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _address)) => stream,
                    Err(err) => {
                        if matches!(
                            err.kind(),
                            ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
                        ) {
                            warn!("recoverable control socket accept error: {err}");
                            continue;
                        }
                        error!("control socket accept error: {err}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
        };

        let transport_event_tx = transport_event_tx.clone();
        tokio::spawn(async move {
            let mut shutdown_request = false;
            let websocket_config = WebSocketConfig::default();
            let max_unfragmented_message_bytes = [
                websocket_config.max_frame_size,
                websocket_config.max_message_size,
            ]
            .into_iter()
            .flatten()
            .min();
            let websocket_stream = match accept_hdr_async_with_config(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: HandshakeResponse| {
                    if request.uri().path() == "/daemon/shutdown" {
                        if !matches!(daemon_shutdown_access, DaemonShutdownAccess::Managed) {
                            let mut rejection = Response::new(Some("unmanaged server".to_string()));
                            *rejection.status_mut() = StatusCode::FORBIDDEN;
                            return Err(rejection);
                        }
                        shutdown_request = true;
                    }
                    if let Some(max_bytes) = max_unfragmented_message_bytes {
                        response.headers_mut().insert(
                            MAX_UNFRAGMENTED_MESSAGE_BYTES_HEADER,
                            HeaderValue::from(max_bytes),
                        );
                    }
                    Ok(response)
                },
                Some(websocket_config),
            )
            .await
            {
                Ok(websocket_stream) => websocket_stream,
                Err(err) => {
                    warn!("failed to upgrade control socket websocket connection: {err}");
                    return;
                }
            };
            if shutdown_request {
                run_daemon_shutdown(websocket_stream, transport_event_tx).await;
                return;
            }
            let (websocket_writer, websocket_reader) = websocket_stream.split();
            run_websocket_connection(websocket_writer, websocket_reader, transport_event_tx).await;
        });
    }
    info!("control socket acceptor shutting down");
}

async fn run_daemon_shutdown(
    mut websocket: tokio_tungstenite::WebSocketStream<UnixStream>,
    transport_event_tx: mpsc::Sender<TransportEvent>,
) {
    let pid = std::process::id().to_string();
    if !matches!(websocket.next().await, Some(Ok(Message::Text(request))) if request == pid) {
        return;
    }
    if websocket.send(Message::Text(pid.into())).await.is_err() {
        return;
    }
    // Let the manager receive the acknowledgment before the main loop closes connections.
    let _ = tokio::time::timeout(Duration::from_secs(2), websocket.next()).await;
    let _ = transport_event_tx
        .send(TransportEvent::DaemonShutdown)
        .await;
}

// Unix callers hold the physical socket's startup lock through bind and publication.
async fn prepare_control_socket_path(socket_path: &Path) -> IoResult<()> {
    match connect_control_socket(socket_path).await {
        Ok(_stream) => {
            return Err(std::io::Error::new(
                ErrorKind::AddrInUse,
                format!(
                    "app-server control socket is already in use at {}",
                    socket_path.display()
                ),
            ));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => {}
        Err(err) => {
            if !socket_path.exists() {
                return Ok(());
            }
            return Err(err);
        }
    }

    // A crashed daemon can leave a dangling rendezvous symlink. Only recognize
    // our own deterministic alias; never remove an arbitrary symlink target.
    if std::fs::read_link(socket_path).ok().as_deref()
        == Some(protected_socket_path(socket_path)?.as_path())
    {
        return tokio::fs::remove_file(socket_path).await;
    }

    if !socket_path.try_exists()? {
        return Ok(());
    }

    if !tokio::fs::symlink_metadata(socket_path)
        .await?
        .file_type()
        .is_socket()
    {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "app-server control socket path exists and is not a socket: {}",
                socket_path.display()
            ),
        ));
    }
    tokio::fs::remove_file(socket_path).await
}

fn protected_socket_path(rendezvous_path: &Path) -> IoResult<std::path::PathBuf> {
    use sha2::Digest;
    use sha2::Sha256;
    use std::os::unix::ffi::OsStrExt;

    let parent = rendezvous_path.parent().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidInput, "socket path must have a parent")
    })?;
    let name = rendezvous_path.file_name().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidInput, "socket path must have a filename")
    })?;
    let path = std::fs::canonicalize(parent)?.join(name);
    let hash = Sha256::digest(path.as_os_str().as_bytes());
    Ok(shared_daemon_socket_directory()?.join(format!("{hash:x}")))
}

/// An exclusive startup lock held until dropped. The lock file remains for later waiters.
///
/// Uses `rustix::fs::flock`, matching Codex's `File::lock` on Unix while supporting Rust 1.88.
pub struct AppServerStartupLock {
    _file: std::fs::File,
}

/// Serializes startup across processes without blocking a Tokio worker.
///
/// The absolute lock path's parent is created with mode 0700 or repaired to that mode, as in
/// Codex's private socket directory helper. A symlink in place of that directory is refused.
pub async fn acquire_app_server_startup_lock(
    startup_lock_path: impl AsRef<Path>,
) -> IoResult<AppServerStartupLock> {
    let startup_lock_path = absolute_path::from_absolute_path_checked(startup_lock_path.as_ref())?;
    if let Some(parent) = startup_lock_path.as_path().parent() {
        prepare_private_socket_directory(parent).await?;
    }
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(startup_lock_path.as_path())?;
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(AppServerStartupLock { _file: file })
    })
    .await
    .map_err(|err| std::io::Error::other(format!("startup lock task failed: {err}")))?
}

async fn set_control_socket_permissions(socket_path: &Path) -> IoResult<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(
        socket_path,
        std::fs::Permissions::from_mode(CONTROL_SOCKET_MODE),
    )
    .await
}

struct ControlSocketFileGuard {
    socket_path: PathBuf,
    rendezvous_path: PathBuf,
}

impl Drop for ControlSocketFileGuard {
    fn drop(&mut self) {
        if std::fs::read_link(self.rendezvous_path.as_path())
            .ok()
            .as_deref()
            == Some(self.socket_path.as_path())
        {
            let _ = std::fs::remove_file(self.rendezvous_path.as_path());
        }
        if let Err(err) = std::fs::remove_file(self.socket_path.as_path())
            && err.kind() != ErrorKind::NotFound
        {
            warn!(
                socket_path = %self.socket_path.display(),
                %err,
                "failed to remove app-server control socket file"
            );
        }
    }
}

/// Connects to a control socket, resolving a long rendezvous alias when it exceeds `sun_path`.
/// This is the Unix behavior of Codex's `codex_uds::UnixStream::connect`.
pub async fn connect_control_socket(socket_path: impl AsRef<Path>) -> IoResult<UnixStream> {
    let socket_path = socket_path.as_ref();
    match UnixStream::connect(socket_path).await {
        Err(error) if error.kind() == ErrorKind::InvalidInput => {
            UnixStream::connect(tokio::fs::canonicalize(socket_path).await?).await
        }
        result => result,
    }
}

async fn prepare_private_socket_directory(directory: &Path) -> IoResult<()> {
    use std::os::unix::fs::PermissionsExt;
    match tokio::fs::DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .await
    {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = tokio::fs::symlink_metadata(directory).await?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "socket directory path exists and is not a directory: {}",
                directory.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

/// The advertised control socket beneath the host-supplied state directory.
pub fn app_server_control_socket_path(state_directory: impl AsRef<Path>) -> IoResult<PathBuf> {
    absolute_path::from_absolute_path(
        &state_directory
            .as_ref()
            .join("app-server-control/app-server-control.sock"),
    )
}

/// The daemon manager's startup lock beneath the host-supplied state directory.
pub fn app_server_startup_lock_path(state_directory: impl AsRef<Path>) -> IoResult<PathBuf> {
    absolute_path::from_absolute_path(
        &state_directory
            .as_ref()
            .join("app-server-control/app-server-startup.lock"),
    )
}
