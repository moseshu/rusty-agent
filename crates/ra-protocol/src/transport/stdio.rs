//! One JSON message per line over the process's stdin and stdout, with logs on stderr: Codex's
//! stdio transport (`app-server-transport/src/transport/stdio.rs`).
//!
//! Reading stdin and writing stdout block, so each runs on a thread of its own rather than on
//! Tokio's blocking pool: Tokio's stdin is an uncancellable blocking task that keeps the runtime
//! alive for as long as the client leaves stdin open, and a write blocked on a full pipe would do
//! the same. The threads are owned by the process and never joined, so a runtime can shut down
//! while one is still blocked.
//!
//! The connection ends when stdin reaches its end or, if asked, when the process receives
//! `SIGTERM` on Unix. When the signal is handled, either one also arms a process deadline: 45
//! seconds later the process exits with status 1, however far its own shutdown has got. A dedicated
//! thread receives the signal and arms the deadline, so neither depends on the Tokio runtime or on
//! logging, both of which may be blocked.
//!
//! Codex also passes the client name of the first `initialize` request out of the transport; its
//! only reader is remote control, which is not ported.

use std::{
    io::{self, BufRead, ErrorKind, Write},
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    CHANNEL_CAPACITY, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
    forward_incoming_message, next_connection_id, serialize_outgoing_message,
};

/// How long after stdin's end or `SIGTERM` the process exits however far its shutdown has got:
/// Codex's deadline, which leaves room for the server's 30-second drain of requests in flight and
/// then bounds the runtime's own teardown.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(45);

/// Opens the connection over the process's stdin and stdout and reports it on
/// `transport_event_tx`: Codex's `start_stdio_connection`.
///
/// The connection opens before this returns: [`TransportEvent::ConnectionOpened`] is sent first,
/// carrying the queue whose messages are written to stdout, one line each. Every line read from
/// stdin follows as [`TransportEvent::IncomingMessage`], and [`TransportEvent::ConnectionClosed`]
/// once stdin ends or cannot be read. Writing goes on after stdin's end until every sender of the
/// write queue has gone, so the server can still answer what it already received.
///
/// With `install_shutdown_signal_handler` on Unix, `SIGTERM` closes the connection at once instead
/// of ending the process, dropping what is still queued in either direction; a connection whose
/// stdin had already ended is then reported closed a second time, as in Codex. The signal and
/// stdin's end both arm a deadline: 45 seconds later the process exits with status 1, however far
/// its shutdown has got. The handler is installed before the connection opens. Elsewhere the flag
/// has no effect.
///
/// The returned task finishes when the connection has closed and nothing more will be written.
///
/// # Errors
///
/// Fails if the signal handler or a thread cannot be started, or if the server has already gone.
pub async fn start_stdio_connection(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    install_shutdown_signal_handler: bool,
) -> io::Result<JoinHandle<()>> {
    let shutdown_signal = CancellationToken::new();
    #[cfg(unix)]
    if install_shutdown_signal_handler {
        // Register before accepting requests.
        spawn_signal_thread(shutdown_signal.clone())?;
    }
    let connection_id = next_connection_id();
    let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);
    let writer_tx_for_reader = writer_tx.clone();
    transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            origin: ConnectionOrigin::Stdio,
            writer: writer_tx,
        })
        .await
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "processor unavailable"))?;

    let mut stdin_rx = spawn_stdin_thread()?;
    // The forwarding future owns `writer_rx`, so cancelling it also releases producers stuck on a
    // full queue.
    let stdout_tx = spawn_stdout_thread()?;

    let transport_event_tx_for_reader = transport_event_tx.clone();
    let read_messages = async move {
        while let Some(line) = stdin_rx.recv().await {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    tracing::error!("Failed reading stdin: {error}");
                    break;
                }
            };
            if !forward_incoming_message(
                &transport_event_tx_for_reader,
                &writer_tx_for_reader,
                connection_id,
                &line,
            )
            .await
            {
                break;
            }
        }

        // The end of stdin can finish the transport before the server or the runtime have cleaned
        // up. Arm the same process deadline even if no SIGTERM arrives.
        if cfg!(unix) && install_shutdown_signal_handler {
            start_shutdown_watchdog();
        }
        let _ = transport_event_tx_for_reader
            .send(TransportEvent::ConnectionClosed { connection_id })
            .await;
        tracing::debug!("stdin reader finished (EOF)");
    };

    let write_messages = async move {
        while let Some(queued_message) = writer_rx.recv().await {
            let (message, write_complete) = queued_message.into_parts();
            let Some(mut json) = serialize_outgoing_message(&message) else {
                continue;
            };
            json.push('\n');
            let (written_tx, written_rx) = oneshot::channel();
            if stdout_tx.send((json, written_tx)).await.is_err() || written_rx.await.is_err() {
                break;
            }
            if let Some(write_complete) = write_complete {
                let _ = write_complete.send(());
            }
        }
        tracing::info!("stdout writer exited (channel closed)");
    };

    Ok(tokio::spawn(async move {
        tokio::select! {
            () = shutdown_signal.cancelled() => {
                // Cancelling both forwarding futures drops their queues before the connection is
                // torn down, including when stdin's end has already begun draining them.
                tracing::info!("SIGTERM received; closing stdio connection (45s shutdown deadline)");
                let _ = transport_event_tx
                    .send(TransportEvent::ConnectionClosed { connection_id })
                    .await;
            }
            () = async move { tokio::join!(read_messages, write_messages); } => {}
        }
    }))
}

/// Receives `SIGTERM` on a thread of its own, then arms the shutdown deadline and cancels
/// `shutdown_signal`. Receiving the signal and arming the deadline must not depend on Tokio or on
/// logging.
#[cfg(unix)]
fn spawn_signal_thread(shutdown_signal: CancellationToken) -> io::Result<()> {
    use signal_hook::{consts::SIGTERM, iterator::Signals};

    let mut signals = Signals::new([SIGTERM])?;
    std::thread::Builder::new()
        .name("app-server-signal".to_owned())
        .spawn(move || {
            if signals.forever().next().is_some() {
                start_shutdown_watchdog();
                shutdown_signal.cancel();
            }
        })?;
    Ok(())
}

/// Reads stdin line by line on a thread of its own.
///
/// Tokio's stdin uses an uncancellable blocking task, which keeps its runtime alive while the
/// client leaves stdin open. This process-owned thread may stay blocked until the process exits,
/// but the runtime never joins it.
fn spawn_stdin_thread() -> io::Result<mpsc::Receiver<io::Result<String>>> {
    let (stdin_tx, stdin_rx) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("app-server-stdin".to_owned())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                if stdin_tx.blocking_send(line).is_err() {
                    break;
                }
            }
        })?;
    Ok(stdin_rx)
}

/// Writes to stdout on a thread of its own, keeping its blocking writes off Tokio's pool too. Each
/// text sent is written whole, then its sender is told; the thread stops at the first failed
/// write.
fn spawn_stdout_thread() -> io::Result<mpsc::Sender<(String, oneshot::Sender<()>)>> {
    let (stdout_tx, mut stdout_rx) = mpsc::channel::<(String, oneshot::Sender<()>)>(1);
    std::thread::Builder::new()
        .name("app-server-stdout".to_owned())
        .spawn(move || {
            let mut stdout = io::stdout().lock();
            while let Some((json, written_tx)) = stdout_rx.blocking_recv() {
                if let Err(error) = stdout.write_all(json.as_bytes()) {
                    tracing::error!("Failed to write to stdout: {error}");
                    break;
                }
                let _ = written_tx.send(());
            }
        })?;
    Ok(stdout_tx)
}

/// Exits the process with status 1 once [`SHUTDOWN_DEADLINE`] has passed: Codex's
/// `start_shutdown_watchdog`. Stdin's end and `SIGTERM` can both arm it; the first deadline holds.
fn start_shutdown_watchdog() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("app-server-shutdown".to_owned())
            .spawn(|| {
                // Leave room for the server's drain of requests in flight, then bound even the
                // runtime's teardown. Do not log here: stderr may be blocked too.
                std::thread::sleep(SHUTDOWN_DEADLINE);
                std::process::exit(1);
            })
            .unwrap_or_else(|_| std::process::exit(1));
    });
}
