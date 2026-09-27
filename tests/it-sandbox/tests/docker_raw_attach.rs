//! `ra-sandbox::docker::BollardDockerApi`: attaching to a command on a terminal.
//!
//! A terminal's output is not framed, and bollard's attachment reads a read that starts with byte
//! 0, 1 or 2 as a frame header. The reference reads a terminal's socket raw, so this client makes
//! terminal attachments over a connection of its own. These tests stand a fake daemon up in the
//! test process — on a Unix socket, over TCP, and over TLS with client certificates — send output
//! that starts with such a byte, and check it arrives exactly as sent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use ra_sandbox::docker::{BollardDockerApi, DockerApi, ExecStreamKind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Output that bollard's decoder would take for a frame header: stream 1, length 5.
const TERMINAL_OUTPUT: &[u8] = b"\x01\x00\x00\x00\x00\x00\x00\x05hello\x02\x00tail";

/// The daemon's answer to an exec start it upgrades.
const UPGRADED: &[u8] = b"HTTP/1.1 101 UPGRADED\r\n\
    Content-Type: application/vnd.docker.raw-stream\r\n\
    Connection: Upgrade\r\n\
    Upgrade: tcp\r\n\r\n";

/// Set in the environment of the child process that runs the TLS case.
const TLS_CHILD_ENV: &str = "RA_DOCKER_RAW_ATTACH_TLS_CHILD";

/// How the fake daemon answers the exec start.
#[derive(Clone, Copy)]
enum Reply {
    /// Upgrades, sends `output`, echoes one line of input, and closes.
    Upgrade(&'static [u8]),
    /// Upgrades, sends `output`, and closes without reading input.
    UpgradeAndClose(&'static [u8]),
    /// Refuses with this status and a JSON message.
    Refuse(u16, &'static str),
}

/// What the fake daemon received.
#[derive(Debug)]
struct Received {
    head: String,
    body: Vec<u8>,
    input: Vec<u8>,
}

impl Received {
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

/// Serves one exec start on `stream`.
async fn serve(mut stream: impl AsyncRead + AsyncWrite + Unpin, reply: Reply) -> Received {
    let mut buffered = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.expect("read the request");
        assert!(read > 0, "the client closed before sending a request");
        buffered.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffered.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&buffered[..head_end]).into_owned();
    let mut body = buffered[head_end + 4..].to_vec();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while body.len() < length {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.expect("read the body");
        assert!(read > 0, "the client closed inside the body");
        body.extend_from_slice(&chunk[..read]);
    }

    let mut input = Vec::new();
    match reply {
        Reply::UpgradeAndClose(output) => {
            stream.write_all(UPGRADED).await.expect("upgrade");
            stream.write_all(output).await.expect("output");
            // The client may have hung up already, having read what it wanted.
            let _ = stream.shutdown().await;
        }
        Reply::Upgrade(output) => {
            stream.write_all(UPGRADED).await.expect("upgrade");
            stream.write_all(output).await.expect("output");
            stream.flush().await.expect("flush");
            while !input.ends_with(b"\n") {
                let mut chunk = [0_u8; 64];
                let read = stream.read(&mut chunk).await.expect("read input");
                if read == 0 {
                    break;
                }
                input.extend_from_slice(&chunk[..read]);
            }
            stream.write_all(&input).await.expect("echo");
            stream.shutdown().await.expect("close");
        }
        Reply::Refuse(status, message) => {
            let body = format!(r#"{{"message":"{message}"}}"#);
            let response = format!(
                "HTTP/1.1 {status} Refused\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.expect("refuse");
            stream.shutdown().await.expect("close");
        }
    }
    Received { head, body, input }
}

/// Starts `exec-1` on a terminal, reads the output, writes a line, and reads to the end.
async fn attach_and_converse(api: &BollardDockerApi) -> Vec<u8> {
    let (mut output, mut input) = api
        .exec_start("exec-1", true)
        .await
        .expect("attach")
        .into_parts();
    let mut received = Vec::new();
    while received.len() < TERMINAL_OUTPUT.len() {
        let frame = output.next().await.expect("more output").expect("a frame");
        assert_eq!(frame.stream(), ExecStreamKind::Console);
        received.extend_from_slice(frame.data());
    }
    input.write_all(b"ls\n").await.expect("write");
    input.flush().await.expect("flush");
    while let Some(frame) = output.next().await {
        received.extend_from_slice(frame.expect("a frame").data());
    }
    received
}

/// The request is the one bollard would send, on the same path — unversioned, as bollard's
/// requests turn out to be (see `output_without_a_terminal_is_still_demultiplexed`).
fn assert_terminal_start_request(received: &Received) {
    assert_eq!(received.request_line(), "POST /exec/exec-1/start HTTP/1.1");
    assert_eq!(
        received.header("upgrade").as_deref(),
        Some("tcp"),
        "{}",
        received.head
    );
    assert_eq!(
        received
            .header("connection")
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("upgrade")
    );
    assert_eq!(
        received.header("content-type").as_deref(),
        Some("application/json")
    );
    let body: serde_json::Value = serde_json::from_slice(&received.body).expect("a JSON body");
    assert_eq!(body, serde_json::json!({"Detach": false, "Tty": true}));
}

/// A Unix socket the fake daemon listens on, and the directory holding it.
fn unix_listener() -> (tempfile::TempDir, PathBuf, tokio::net::UnixListener) {
    let directory = tempfile::tempdir().expect("temp");
    let socket = directory.path().join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    (directory, socket, listener)
}

fn unix_host(socket: &Path) -> String {
    format!("unix://{}", socket.display())
}

#[tokio::test]
async fn terminal_output_over_a_unix_socket_arrives_exactly_as_sent() {
    let (_directory, socket, listener) = unix_listener();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        serve(stream, Reply::Upgrade(TERMINAL_OUTPUT)).await
    });
    let api = BollardDockerApi::connect_with_host(&unix_host(&socket)).expect("a client");

    let output = attach_and_converse(&api).await;

    assert_eq!(output, [TERMINAL_OUTPUT, b"ls\n"].concat());
    let received = server.await.expect("the server");
    assert_terminal_start_request(&received);
    assert_eq!(received.input, b"ls\n");
}

#[tokio::test]
async fn terminal_output_over_tcp_arrives_exactly_as_sent() {
    if std::env::var_os("DOCKER_TLS_VERIFY").is_some() {
        eprintln!("DOCKER_TLS_VERIFY is set, so tcp:// means TLS here; see the TLS case");
        return;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("an address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        serve(stream, Reply::Upgrade(TERMINAL_OUTPUT)).await
    });
    let api =
        BollardDockerApi::connect_with_host(&format!("tcp://127.0.0.1:{port}")).expect("a client");

    let output = attach_and_converse(&api).await;

    assert_eq!(output, [TERMINAL_OUTPUT, b"ls\n"].concat());
    let received = server.await.expect("the server");
    assert_terminal_start_request(&received);
    assert_eq!(received.header("host"), Some(format!("127.0.0.1:{port}")));
}

/// Without a terminal the attachment is still bollard's, whose frame decoding is right for it — and
/// whose request goes to the unversioned path, which the terminal attachment copies.
#[tokio::test]
async fn output_without_a_terminal_is_still_demultiplexed() {
    let (_directory, socket, listener) = unix_listener();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        serve(
            stream,
            Reply::UpgradeAndClose(
                b"\x01\x00\x00\x00\x00\x00\x00\x05hello\x02\x00\x00\x00\x00\x00\x00\x04oops",
            ),
        )
        .await
    });
    let api = BollardDockerApi::connect_with_host(&unix_host(&socket)).expect("a client");

    let (output, input) = api
        .exec_start("exec-1", false)
        .await
        .expect("attach")
        .into_parts();
    let frames: Vec<_> = output
        .take(2)
        .map(|frame| {
            let frame = frame.expect("a frame");
            (frame.stream(), frame.data().to_vec())
        })
        .collect()
        .await;
    drop(input);

    assert_eq!(
        frames,
        [
            (ExecStreamKind::Stdout, b"hello".to_vec()),
            (ExecStreamKind::Stderr, b"oops".to_vec()),
        ]
    );
    let received = server.await.expect("the server");
    assert_eq!(received.request_line(), "POST /exec/exec-1/start HTTP/1.1");
}

#[tokio::test]
async fn a_daemon_that_does_not_know_the_exec_says_so() {
    let (_directory, socket, listener) = unix_listener();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        serve(stream, Reply::Refuse(404, "No such exec instance: exec-1")).await
    });
    let api = BollardDockerApi::connect_with_host(&unix_host(&socket)).expect("a client");

    let error = api.exec_start("exec-1", true).await.expect_err("refused");

    assert!(error.is_not_found(), "{error}");
    assert!(
        error.to_string().contains("No such exec instance: exec-1"),
        "{error}"
    );
    server.await.expect("the server");
}

/// A client handed in without its daemon's address cannot reach a terminal until it is told one.
#[tokio::test]
async fn a_client_handed_in_needs_the_daemon_address_for_a_terminal() {
    let (_directory, socket, listener) = unix_listener();
    let docker =
        bollard::Docker::connect_with_unix(&unix_host(&socket), 120, bollard::API_DEFAULT_VERSION)
            .expect("a bollard client");

    let without = BollardDockerApi::from_client(docker.clone());
    let error = without
        .exec_start("exec-1", true)
        .await
        .expect_err("no address");
    assert!(error.to_string().contains("daemon's address"), "{error}");

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        serve(stream, Reply::Upgrade(TERMINAL_OUTPUT)).await
    });
    let with = BollardDockerApi::from_client(docker).with_host(&unix_host(&socket));
    let output = attach_and_converse(&with).await;
    assert_eq!(output, [TERMINAL_OUTPUT, b"ls\n"].concat());
    server.await.expect("the server");
}

/// The test certificates: a CA, a server certificate for `127.0.0.1`, and a client certificate
/// laid out as Docker's certificate directory expects (`ca.pem`, `cert.pem`, `key.pem`).
fn tls_fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/docker_tls")
}

// Runs the TLS case in a child process: the certificate directory and `DOCKER_TLS_VERIFY` come
// from the environment, which a test cannot change for itself without `unsafe`.
#[test]
fn terminal_output_over_tls_arrives_exactly_as_sent() {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tls_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(TLS_CHILD_ENV, "1")
        .env("DOCKER_TLS_VERIFY", "1")
        .env("DOCKER_CERT_PATH", tls_fixtures())
        .output()
        .expect("run the TLS case");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    // A filter that matched nothing is reported as success.
    assert!(stdout.contains("1 passed"), "{stdout}");
}

#[tokio::test]
#[ignore = "run by `terminal_output_over_tls_arrives_exactly_as_sent`"]
async fn tls_child() {
    if std::env::var_os(TLS_CHILD_ENV).is_none() {
        return;
    }
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

    let fixtures = tls_fixtures();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut client_roots = rustls::RootCertStore::empty();
    client_roots.add_parsable_certificates(
        CertificateDer::pem_file_iter(fixtures.join("ca.pem"))
            .expect("ca.pem")
            .map(|certificate| certificate.expect("a certificate")),
    );
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(client_roots),
        Arc::clone(&provider),
    )
    .build()
    .expect("a client verifier");
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            CertificateDer::pem_file_iter(fixtures.join("server.pem"))
                .expect("server.pem")
                .map(|certificate| certificate.expect("a certificate"))
                .collect(),
            PrivateKeyDer::from_pem_file(fixtures.join("server-key.pem")).expect("a key"),
        )
        .expect("a server configuration");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("an address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let stream = acceptor.accept(stream).await.expect("a TLS handshake");
        let presented = stream
            .get_ref()
            .1
            .peer_certificates()
            .is_some_and(|certificates| !certificates.is_empty());
        (
            presented,
            serve(stream, Reply::Upgrade(TERMINAL_OUTPUT)).await,
        )
    });
    let api =
        BollardDockerApi::connect_with_host(&format!("tcp://127.0.0.1:{port}")).expect("a client");

    let output = attach_and_converse(&api).await;

    assert_eq!(output, [TERMINAL_OUTPUT, b"ls\n"].concat());
    let (presented, received) = server.await.expect("the server");
    assert!(presented, "the client presented no certificate");
    assert_terminal_start_request(&received);
}
