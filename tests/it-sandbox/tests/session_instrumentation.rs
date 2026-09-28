//! `ra-sandbox::{instrumentation, sinks}`: audit events, their delivery, and the spans around them.
//!
//! Ported from the reference's `test_session_sinks.py` and the instrumentation half of
//! `test_session_manager.py`. Two sessions stand in for the reference's two fixtures: a real
//! unix-local session made by its client, and an in-memory one for the file-level tests that
//! need no process. Spans are read back through a `tracing` layer rather than an SDK trace
//! processor; see the wrapper's module documentation for why every event carries an audit span id
//! and no trace id.

#[path = "support/memory_session.rs"]
mod memory_session;
#[path = "support/span_log.rs"]
mod span_log;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use memory_session::MemorySession;
use ra_core::sandbox::{
    CreateRequest, DeliveryMode, Entry, ErrorCode, EventPayloadPolicy, EventPhase, EventSink,
    ExecRequest, ExecResult, Manifest, OnErrorPolicy, OpName, SandboxClient, SandboxSession,
    SandboxSessionEvent, SandboxSessionFinishEvent, SandboxSessionStartEvent, SinkError,
    SnapshotSpec,
};
use ra_sandbox::instrumentation::{Instrumentation, InstrumentedSession};
use ra_sandbox::sinks::{
    CallbackSink, ChainedSink, HttpProxySink, JsonlOutboxSink, WorkspaceJsonlSink,
};
use ra_sandbox::unix_local::{UnixLocalSandboxClient, UnixLocalSandboxClientOptions};
use serde_json::Value;
use span_log::SpanLog;
use uuid::Uuid;

type Events = Arc<Mutex<Vec<SandboxSessionEvent>>>;

/// A synchronous callback sink that keeps every event it is handed.
fn recording_sink() -> (Events, Arc<dyn EventSink>) {
    let events: Events = Arc::default();
    let seen = Arc::clone(&events);
    let sink = CallbackSink::new(move |event, _session| {
        seen.lock().expect("events").push(event);
        Ok(())
    });
    (events, Arc::new(sink))
}

fn recorded(events: &Events) -> Vec<SandboxSessionEvent> {
    events.lock().expect("events").clone()
}

fn of_op(
    events: &[SandboxSessionEvent],
    op: OpName,
    phase: EventPhase,
) -> Vec<SandboxSessionEvent> {
    events
        .iter()
        .filter(|event| event.op() == op && event.phase() == phase)
        .cloned()
        .collect()
}

fn command(text: &str) -> ExecRequest {
    ExecRequest::new([text.to_owned()])
}

/// A unix-local client over a workspace directory of its own, delivering to `instrumentation`.
async fn unix_local_session(
    workspace: &std::path::Path,
    snapshots: Option<&std::path::Path>,
    manifest: Manifest,
    instrumentation: Instrumentation,
    exposed_ports: &[u16],
) -> Box<dyn SandboxSession> {
    // Canonicalized: on macOS a temporary directory is reached through a symlinked parent, and a
    // root that names the link is a different path to every check that resolves one.
    let root = std::fs::canonicalize(workspace).expect("resolve the workspace");
    let manifest = manifest.with_root(root.to_string_lossy().into_owned());
    let mut request = CreateRequest::new().with_manifest(manifest);
    if let Some(base_path) = snapshots {
        request = request.with_snapshot_spec(SnapshotSpec::Local {
            base_path: base_path.to_path_buf(),
        });
    }
    if !exposed_ports.is_empty() {
        let options = UnixLocalSandboxClientOptions::new()
            .with_exposed_ports(exposed_ports.iter().copied())
            .expect("ports");
        request = request.with_options(options.to_payload());
    }
    UnixLocalSandboxClient::new()
        .with_instrumentation(Arc::new(instrumentation))
        .create(request)
        .await
        .expect("create")
}

/// The member names of a tar archive on disk.
fn tar_members(path: &std::path::Path) -> Vec<String> {
    let file = std::fs::File::open(path).expect("open the snapshot");
    let mut archive = tar::Archive::new(file);
    archive
        .entries()
        .expect("entries")
        .map(|entry| {
            entry
                .expect("entry")
                .path()
                .expect("path")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn outbox_ops(bytes: &[u8]) -> Vec<String> {
    String::from_utf8(bytes.to_vec())
        .expect("utf-8")
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("json")["op"]
                .as_str()
                .expect("op")
                .to_owned()
        })
        .collect()
}

fn start_event(session_id: Uuid, seq: u64, op: OpName) -> SandboxSessionEvent {
    SandboxSessionStartEvent::new(session_id, seq, op, format!("span_{seq}")).into()
}

fn finish_event(session_id: Uuid, seq: u64, op: OpName) -> SandboxSessionEvent {
    SandboxSessionFinishEvent::new(session_id, seq, op, format!("span_{seq}"), true, 0.0).into()
}

// --- test_session_sinks.py ------------------------------------------------------------------------

/// `test_sandbox_session_exec_emits_stdout_when_enabled`.
#[tokio::test]
async fn a_command_s_output_reaches_a_sink_whose_policy_includes_it() {
    let workspace = tempfile::tempdir().expect("temp");
    let (events, sink) = recording_sink();
    let instrumentation = Instrumentation::with_sinks([sink])
        .with_payload_policy(EventPayloadPolicy::new().with_include_exec_output(true));
    let session = unix_local_session(
        workspace.path(),
        None,
        Manifest::new(),
        instrumentation,
        &[],
    )
    .await;

    session.start().await.expect("start");
    let result = session.exec(command("echo hi")).await.expect("exec");
    assert!(result.ok());
    session.close().await.expect("close");

    let finish = of_op(&recorded(&events), OpName::Exec, EventPhase::Finish);
    let finish = finish[0].as_finish().expect("finish");
    assert!(finish.stdout().is_some_and(|out| out.contains("hi")));
    assert_eq!(finish.base().trace_id(), None);
    assert!(finish.base().span_id().starts_with("sandbox_op_"));
}

/// `test_sandbox_session_write_does_not_include_bytes_when_disabled`.
#[tokio::test]
async fn a_write_s_length_is_left_out_when_the_policy_says_so() {
    let (events, sink) = recording_sink();
    let instrumentation = Instrumentation::with_sinks([sink])
        .with_payload_policy(EventPayloadPolicy::new().with_include_write_len(false));
    let session = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(instrumentation)),
        None,
    )
    .expect("wrap");

    session.start().await.expect("start");
    session
        .write("x.txt".into(), b"hello".to_vec(), None)
        .await
        .expect("write");
    session.close().await.expect("close");

    let start = &of_op(&recorded(&events), OpName::Write, EventPhase::Start)[0];
    assert!(!start.data().contains_key("bytes"));
    assert_eq!(start.data()["path"], Value::from("x.txt"));
}

/// `test_sandbox_session_apply_manifest_preserves_write_instrumentation`.
#[tokio::test]
async fn applying_a_manifest_through_the_wrapper_records_its_writes() {
    let workspace = tempfile::tempdir().expect("temp");
    let (events, sink) = recording_sink();
    let manifest = Manifest::new().with_entry("materialized.txt", Entry::file("hello"));
    let session = unix_local_session(
        workspace.path(),
        None,
        manifest,
        Instrumentation::with_sinks([sink]),
        &[],
    )
    .await;

    session.apply_manifest(true).await.expect("apply");

    let phases: Vec<EventPhase> = recorded(&events)
        .iter()
        .filter(|event| event.op() == OpName::Write)
        .map(SandboxSessionEvent::phase)
        .collect();
    assert_eq!(phases, [EventPhase::Start, EventPhase::Finish]);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("materialized.txt")).expect("read"),
        "hello"
    );
}

/// The deviation beside the previous test: the backend's own refusal still runs.
#[tokio::test]
async fn applying_a_manifest_through_the_wrapper_keeps_the_backend_s_refusals() {
    let workspace = tempfile::tempdir().expect("temp");
    let manifest = Manifest::new()
        .with_entry("x.txt", Entry::file("x"))
        .with_user(ra_core::sandbox::User::new("someone"));
    let session = unix_local_session(
        workspace.path(),
        None,
        manifest,
        Instrumentation::new(),
        &[],
    )
    .await;

    let error = session.apply_manifest(true).await.expect_err("accounts");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(!workspace.path().join("x.txt").exists());
}

/// `test_jsonl_outbox_sink_appends_one_line_per_event`.
#[tokio::test]
async fn the_host_outbox_gets_one_line_per_event() {
    let directory = tempfile::tempdir().expect("temp");
    let outbox = directory.path().join("nested").join("events.jsonl");
    let sink = JsonlOutboxSink::new(&outbox)
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    let session_id = Uuid::new_v4();

    sink.handle(start_event(session_id, 1, OpName::Write))
        .await
        .expect("start");
    sink.handle(finish_event(session_id, 2, OpName::Write))
        .await
        .expect("finish");

    let text = std::fs::read_to_string(&outbox).expect("read");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["phase"], "start");
    assert_eq!(lines[1]["phase"], "finish");
}

/// `test_chained_sink_runs_in_order`.
#[tokio::test]
async fn a_group_runs_its_members_in_order() {
    let directory = tempfile::tempdir().expect("temp");
    let outbox = directory.path().join("events.jsonl");
    let seen: Arc<Mutex<Vec<usize>>> = Arc::default();
    let counted = Arc::clone(&seen);
    let reader = outbox.clone();
    let callback = CallbackSink::new(move |_event, _session| {
        let lines = std::fs::read_to_string(&reader)
            .unwrap_or_default()
            .lines()
            .count();
        counted.lock().expect("seen").push(lines);
        Ok(())
    });
    callback.bind(MemorySession::empty()).expect("bind");
    let instrumentation = Instrumentation::with_sinks([Arc::new(ChainedSink::new([
        Arc::new(
            JsonlOutboxSink::new(&outbox)
                .with_mode(DeliveryMode::Sync)
                .with_on_error(OnErrorPolicy::Raise),
        ) as Arc<dyn EventSink>,
        Arc::new(callback),
    ])) as Arc<dyn EventSink>]);
    let session_id = Uuid::new_v4();

    instrumentation
        .emit(&start_event(session_id, 1, OpName::Write))
        .await
        .expect("start");
    instrumentation
        .emit(&finish_event(session_id, 2, OpName::Write))
        .await
        .expect("finish");

    assert_eq!(*seen.lock().expect("seen"), [1, 2]);
}

/// `test_workspace_jsonl_sink_writes_into_workspace_and_persists`.
#[tokio::test]
async fn the_workspace_outbox_is_written_into_the_workspace_and_persisted_with_it() {
    let workspace = tempfile::tempdir().expect("temp");
    let snapshots = tempfile::tempdir().expect("temp");
    let sink = WorkspaceJsonlSink::new()
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    let session = unix_local_session(
        workspace.path(),
        Some(snapshots.path()),
        Manifest::new(),
        Instrumentation::with_sinks([Arc::new(sink) as Arc<dyn EventSink>]),
        &[],
    )
    .await;
    let session_id = session.state().session_id();
    let relpath = format!("logs/events-{session_id}.jsonl");

    session.start().await.expect("start");
    session.exec(command("echo hi")).await.expect("exec");
    session.close().await.expect("close");

    let inner = session.inner_session().expect("wrapped");
    let outbox = inner
        .read((&relpath).into(), None)
        .await
        .expect("read the outbox");
    assert!(outbox_ops(&outbox).iter().any(|op| op == "exec"));

    let members = tar_members(&snapshots.path().join(format!("{session_id}.tar")));
    assert!(
        members.iter().any(|name| name.ends_with(&relpath)),
        "{members:?}"
    );
}

/// `test_workspace_jsonl_sink_supports_session_id_template`.
#[tokio::test]
async fn the_workspace_outbox_path_expands_the_session_id_templates() {
    let workspace = tempfile::tempdir().expect("temp");
    let sink = WorkspaceJsonlSink::new()
        .with_workspace_relpath("logs/{session_id_hex}/events-{session_id}.jsonl")
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    let session = unix_local_session(
        workspace.path(),
        None,
        Manifest::new(),
        Instrumentation::with_sinks([Arc::new(sink) as Arc<dyn EventSink>]),
        &[],
    )
    .await;
    let session_id = session.state().session_id();

    session.start().await.expect("start");
    session.exec(command("echo hi")).await.expect("exec");
    session.close().await.expect("close");

    let expected = format!("logs/{}/events-{session_id}.jsonl", session_id.simple());
    let outbox = session
        .inner_session()
        .expect("wrapped")
        .read((&expected).into(), None)
        .await
        .expect("read the outbox");
    assert!(outbox_ops(&outbox).iter().any(|op| op == "exec"));
}

#[test]
fn a_template_that_cannot_be_expanded_is_used_as_written() {
    let session = MemorySession::empty();
    for template in [
        "logs/{unknown}.jsonl",
        "logs/{session_id.jsonl",
        "logs/}{.jsonl",
    ] {
        let sink = WorkspaceJsonlSink::new().with_workspace_relpath(template);
        sink.bind(session.clone()).expect("bind");
        assert_eq!(sink.resolved_workspace_relpath().as_deref(), Some(template));
    }
    let sink = WorkspaceJsonlSink::new().with_workspace_relpath("logs/{{literal}}.jsonl");
    sink.bind(session).expect("bind");
    assert_eq!(
        sink.resolved_workspace_relpath().as_deref(),
        Some("logs/{literal}.jsonl")
    );
}

/// `test_workspace_jsonl_sink_preserves_preexisting_outbox_contents`.
#[tokio::test]
async fn the_workspace_outbox_appends_to_what_is_already_there() {
    let inner = MemorySession::empty();
    inner.start().await.expect("start");
    let relpath = format!("logs/events-{}.jsonl", inner.state().session_id());
    inner
        .write((&relpath).into(), b"{\"old\":true}\n".to_vec(), None)
        .await
        .expect("write");
    let sink = WorkspaceJsonlSink::new()
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    sink.bind(inner.clone()).expect("bind");
    let session_id = inner.state().session_id();

    sink.handle(start_event(session_id, 1, OpName::Write))
        .await
        .expect("start");
    sink.handle(finish_event(session_id, 2, OpName::Write))
        .await
        .expect("finish");

    let text = String::from_utf8(inner.file(&relpath).expect("outbox")).expect("utf-8");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], serde_json::json!({"old": true}));
    assert_eq!(lines[1]["seq"], 1);
    assert_eq!(lines[2]["seq"], 2);
}

/// `test_workspace_jsonl_sink_does_not_duplicate_lines_across_flushes` and
/// `test_workspace_jsonl_sink_clears_flushed_buffer`.
#[tokio::test]
async fn each_flush_writes_only_what_was_not_written_before() {
    let inner = MemorySession::empty();
    inner.start().await.expect("start");
    let session_id = inner.state().session_id();
    let relpath = format!("logs/events-{session_id}.jsonl");
    let sink = WorkspaceJsonlSink::new()
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise)
        .with_flush_every(1);
    sink.bind(inner.clone()).expect("bind");

    for seq in 1..=3 {
        sink.handle(start_event(session_id, seq, OpName::Write))
            .await
            .expect("handle");
        assert!(
            !sink.has_buffered_lines().await,
            "flushed lines are cleared"
        );
    }

    let text = String::from_utf8(inner.file(&relpath).expect("outbox")).expect("utf-8");
    let seqs: Vec<u64> = text
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("json")["seq"]
                .as_u64()
                .expect("seq")
        })
        .collect();
    assert_eq!(seqs, [1, 2, 3]);
}

#[tokio::test]
async fn lines_wait_while_the_session_is_not_running() {
    let inner = MemorySession::empty();
    let session_id = inner.state().session_id();
    let relpath = format!("logs/events-{session_id}.jsonl");
    let sink = WorkspaceJsonlSink::new()
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    sink.bind(inner.clone()).expect("bind");

    sink.handle(start_event(session_id, 1, OpName::Start))
        .await
        .expect("handle");
    assert!(sink.has_buffered_lines().await);
    assert_eq!(inner.file(&relpath), None);

    inner.start().await.expect("start");
    sink.handle(finish_event(session_id, 2, OpName::Start))
        .await
        .expect("handle");
    let text = String::from_utf8(inner.file(&relpath).expect("outbox")).expect("utf-8");
    assert_eq!(
        text.lines().count(),
        2,
        "the waiting line went out with the next"
    );
}

#[tokio::test]
async fn an_unbound_workspace_outbox_ignores_events() {
    let sink = WorkspaceJsonlSink::new();
    sink.handle(start_event(Uuid::new_v4(), 1, OpName::Write))
        .await
        .expect("a no-op");
    assert!(!sink.has_buffered_lines().await);
}

/// `test_workspace_jsonl_sink_ephemeral_excludes_runtime_outbox_with_existing_parent`.
#[tokio::test]
async fn an_ephemeral_workspace_outbox_is_left_out_of_the_snapshot() {
    let workspace = tempfile::tempdir().expect("temp");
    let snapshots = tempfile::tempdir().expect("temp");
    let sink = WorkspaceJsonlSink::new()
        .with_ephemeral(true)
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    let manifest = Manifest::new().with_entry(
        "logs",
        Entry::dir().with_child("keep.txt", Entry::file("keep")),
    );
    let session = unix_local_session(
        workspace.path(),
        Some(snapshots.path()),
        manifest,
        Instrumentation::with_sinks([Arc::new(sink) as Arc<dyn EventSink>]),
        &[],
    )
    .await;
    let session_id = session.state().session_id();
    let relpath = format!("logs/events-{session_id}.jsonl");

    session.start().await.expect("start");
    session.exec(command("echo hi")).await.expect("exec");
    let inner = session.inner_session().expect("wrapped");
    assert!(
        !inner
            .read((&relpath).into(), None)
            .await
            .expect("outbox")
            .is_empty()
    );
    // The manifest is not changed to exclude it: the exclusion is the session's.
    let state = session.state();
    let logs = &state.manifest().entries["logs"];
    assert_eq!(
        logs.children()
            .expect("a directory")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["keep.txt"]
    );
    session.close().await.expect("close");

    let members = tar_members(&snapshots.path().join(format!("{session_id}.tar")));
    assert!(
        members.iter().any(|name| name.ends_with("logs/keep.txt")),
        "{members:?}"
    );
    assert!(
        !members.iter().any(|name| name.ends_with(&relpath)),
        "{members:?}"
    );
}

/// `test_workspace_jsonl_sink_flushes_on_stop_when_flush_every_gt_one`.
#[tokio::test]
async fn a_batching_workspace_outbox_still_writes_before_the_snapshot() {
    let workspace = tempfile::tempdir().expect("temp");
    let snapshots = tempfile::tempdir().expect("temp");
    let sink = WorkspaceJsonlSink::new()
        .with_flush_every(10)
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);
    let session = unix_local_session(
        workspace.path(),
        Some(snapshots.path()),
        Manifest::new(),
        Instrumentation::with_sinks([Arc::new(sink) as Arc<dyn EventSink>]),
        &[],
    )
    .await;
    let session_id = session.state().session_id();
    let relpath = format!("logs/events-{session_id}.jsonl");

    session.start().await.expect("start");
    session.exec(command("echo hi")).await.expect("exec");
    session.close().await.expect("close");

    let inner = session.inner_session().expect("wrapped");
    assert!(
        !inner
            .read((&relpath).into(), None)
            .await
            .expect("outbox")
            .is_empty()
    );
    let members = tar_members(&snapshots.path().join(format!("{session_id}.tar")));
    assert!(
        members.iter().any(|name| name.ends_with(&relpath)),
        "{members:?}"
    );
}

/// `test_callback_sink_receives_bound_inner_session`.
#[tokio::test]
async fn a_callback_is_handed_the_session_underneath_the_wrapper() {
    let workspace = tempfile::tempdir().expect("temp");
    let seen: Arc<Mutex<Vec<Arc<dyn SandboxSession>>>> = Arc::default();
    let kept = Arc::clone(&seen);
    let sink = CallbackSink::new(move |_event, session| {
        kept.lock().expect("seen").push(session);
        Ok(())
    });
    let session = unix_local_session(
        workspace.path(),
        None,
        Manifest::new(),
        Instrumentation::with_sinks([Arc::new(sink) as Arc<dyn EventSink>]),
        &[],
    )
    .await;

    session.start().await.expect("start");
    session.exec(command("echo hi")).await.expect("exec");
    session.close().await.expect("close");

    let inner = session.inner_session().expect("wrapped");
    let seen = seen.lock().expect("seen");
    assert!(!seen.is_empty());
    assert!(
        seen.iter()
            .all(|handed| std::ptr::addr_eq(Arc::as_ptr(handed), Arc::as_ptr(&inner)))
    );
}

#[tokio::test]
async fn a_sink_bound_to_the_wrapper_keeps_the_session_underneath_it() {
    let inner = MemorySession::empty();
    let wrapper: Arc<dyn SandboxSession> =
        Arc::new(InstrumentedSession::new(inner.clone(), None, None).expect("wrap"));
    let seen: Arc<Mutex<Option<Arc<dyn SandboxSession>>>> = Arc::default();
    let kept = Arc::clone(&seen);
    let sink = CallbackSink::new(move |_event, session| {
        *kept.lock().expect("seen") = Some(session);
        Ok(())
    });

    sink.bind(wrapper).expect("bind");
    sink.handle(start_event(Uuid::new_v4(), 1, OpName::Write))
        .await
        .expect("handle");

    let handed = seen.lock().expect("seen").clone().expect("handed");
    let inner: Arc<dyn SandboxSession> = inner;
    assert!(std::ptr::addr_eq(Arc::as_ptr(&handed), Arc::as_ptr(&inner)));
}

#[tokio::test]
async fn an_unbound_callback_fails_as_the_reference_s_does() {
    let sink = CallbackSink::new(|_event, _session| Ok(()));
    let error = sink
        .handle(start_event(Uuid::new_v4(), 1, OpName::Write))
        .await
        .expect_err("unbound");
    assert!(
        error
            .to_string()
            .contains("CallbackSink requires a bound session")
    );
}

/// A server that accepts one request, answers with `status`, and hands back what it received.
fn one_shot_server(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut received = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).expect("read");
            received.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&received);
            if let Some(split) = text.find("\r\n\r\n") {
                let length = text[..split]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if received.len() >= split + 4 + length {
                    break;
                }
            }
            if read == 0 {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )
        .expect("respond");
        String::from_utf8_lossy(&received).into_owned()
    });
    (format!("http://{address}/events"), handle)
}

/// `test_http_proxy_sink_snapshots_headers`, and what a successful post carries.
#[tokio::test]
async fn a_post_carries_the_event_as_json_with_the_headers_given_at_construction() {
    let (endpoint, server) = one_shot_server("200 OK");
    let mut headers = vec![("authorization".to_owned(), "Bearer original".to_owned())];
    let sink = HttpProxySink::new(endpoint).with_headers(headers.clone());
    headers[0].1 = "Bearer changed".to_owned();
    let session_id = Uuid::new_v4();

    sink.handle(start_event(session_id, 7, OpName::Exec))
        .await
        .expect("post");

    let request = server.join().expect("server").to_lowercase();
    assert!(request.starts_with("post /events"), "{request}");
    assert!(
        request.contains("authorization: bearer original"),
        "{request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "{request}"
    );
    let body = &request[request.find("\r\n\r\n").expect("body") + 4..];
    let body: Value = serde_json::from_str(body).expect("json body");
    assert_eq!(body["seq"], 7);
    assert_eq!(body["phase"], "start");
}

/// `test_http_proxy_sink_spools_direct_timeout`.
#[tokio::test]
async fn a_post_that_times_out_is_spooled_and_reported() {
    // Accepts and never answers, so the post runs into its timeout.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/events", listener.local_addr().expect("address"));
    let directory = tempfile::tempdir().expect("temp");
    let spool = directory.path().join("spool").join("events.jsonl");
    let sink = HttpProxySink::new(endpoint)
        .with_timeout_s(0.2)
        .with_spool_path(&spool)
        .with_mode(DeliveryMode::Sync)
        .with_on_error(OnErrorPolicy::Raise);

    let error = sink
        .handle(start_event(Uuid::new_v4(), 1, OpName::Write))
        .await
        .expect_err("timed out");
    drop(listener);

    assert!(
        error.to_string().starts_with("http proxy sink POST failed"),
        "{error}"
    );
    let spooled = std::fs::read_to_string(&spool).expect("spool");
    let lines: Vec<&str> = spooled.lines().collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(lines[0]).expect("json")["seq"],
        1
    );
}

#[tokio::test]
async fn an_error_status_is_a_failed_post() {
    let (endpoint, server) = one_shot_server("503 Service Unavailable");
    let sink = HttpProxySink::new(endpoint);
    let error = sink
        .handle(start_event(Uuid::new_v4(), 1, OpName::Write))
        .await
        .expect_err("503");
    server.join().expect("server");
    assert!(
        error.to_string().contains("http proxy sink POST failed"),
        "{error}"
    );
}

/// `test_sandbox_session_error_events_and_traces_include_retryability`.
#[tokio::test]
async fn a_failure_s_retryability_reaches_the_event_and_the_span() {
    let (log, _guard) = SpanLog::install();
    let (events, sink) = recording_sink();
    let session = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(Instrumentation::with_sinks([sink]))),
        None,
    )
    .expect("wrap");

    session.start().await.expect("start");
    let error = session
        .read("missing.txt".into(), None)
        .await
        .expect_err("missing");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
    session.close().await.expect("close");

    let finish = &of_op(&recorded(&events), OpName::Read, EventPhase::Finish)[0];
    assert_eq!(
        finish.as_finish().expect("finish").error_retryable(),
        Some(false)
    );

    let span = log
        .sandbox_spans()
        .into_iter()
        .find(|span| span.kind() == "sandbox.read")
        .expect("read span");
    assert_eq!(span.field("error.retryable"), Some("false"));
    assert_eq!(span.field("outcome"), Some("error"));
    assert_eq!(span.field("error.type"), Some("WorkspaceReadNotFoundError"));
    assert_eq!(span.field("error.code"), Some("workspace_read_not_found"));
}

/// `test_expected_read_span_error_is_call_scoped_and_preserves_audit_failures`.
#[tokio::test]
async fn an_expected_failure_is_not_a_span_error_and_is_still_an_audited_failure() {
    let (log, _guard) = SpanLog::install();
    let (events, sink) = recording_sink();
    let session = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(Instrumentation::with_sinks([sink]))),
        None,
    )
    .expect("wrap");

    let (expected, ordinary) = tokio::join!(
        session.read_expecting(
            "expected-missing.txt".into(),
            None,
            &[ErrorCode::WorkspaceReadNotFound]
        ),
        session.read("ordinary-missing.txt".into(), None),
    );
    assert_eq!(
        expected.expect_err("missing").error_code(),
        ErrorCode::WorkspaceReadNotFound
    );
    assert_eq!(
        ordinary.expect_err("missing").error_code(),
        ErrorCode::WorkspaceReadNotFound
    );

    let events = recorded(&events);
    let path_by_span: std::collections::BTreeMap<String, String> =
        of_op(&events, OpName::Read, EventPhase::Start)
            .iter()
            .map(|event| {
                (
                    event.base().span_id().to_owned(),
                    event.data()["path"].as_str().expect("path").to_owned(),
                )
            })
            .collect();
    let outcome_by_path: std::collections::BTreeMap<String, String> = log
        .sandbox_spans()
        .into_iter()
        .filter(|span| span.kind() == "sandbox.read")
        .map(|span| {
            (
                path_by_span[span.field("sandbox.audit_span_id").expect("audit id")].clone(),
                span.field("outcome").expect("outcome").to_owned(),
            )
        })
        .collect();
    assert_eq!(outcome_by_path["expected-missing.txt"], "ok");
    assert_eq!(outcome_by_path["ordinary-missing.txt"], "error");

    let finishes = of_op(&events, OpName::Read, EventPhase::Finish);
    assert_eq!(finishes.len(), 2);
    for event in finishes {
        let finish = event.as_finish().expect("finish");
        assert!(!finish.ok());
        assert_eq!(finish.error_type(), Some("WorkspaceReadNotFoundError"));
        assert_eq!(finish.error_code(), Some(ErrorCode::WorkspaceReadNotFound));
        assert_eq!(finish.error_retryable(), Some(false));
    }
}

/// `test_expected_read_span_records_finish_sink_failure`.
#[tokio::test]
async fn a_sink_failing_on_an_expected_failure_still_marks_the_span() {
    let (log, _guard) = SpanLog::install();
    let sink = CallbackSink::new(|event, _session| {
        if event.op() == OpName::Read && event.phase() == EventPhase::Finish {
            return Err("simulated sink failure".into());
        }
        Ok(())
    });
    let session = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(Instrumentation::with_sinks([
            Arc::new(sink) as Arc<dyn EventSink>
        ]))),
        None,
    )
    .expect("wrap");

    let error = session
        .read_expecting(
            "expected-missing.txt".into(),
            None,
            &[ErrorCode::WorkspaceReadNotFound],
        )
        .await
        .expect_err("the sink failed");
    assert_eq!(error.error_code(), ErrorCode::EventSinkFailed);
    assert!(
        error.to_string().starts_with("sandbox event sink failed"),
        "{error}"
    );
    assert!(error.to_string().contains("CallbackSink"), "{error}");

    let span = log
        .sandbox_spans()
        .into_iter()
        .find(|span| span.kind() == "sandbox.read")
        .expect("read span");
    assert_eq!(span.field("outcome"), Some("error"));
    assert_eq!(span.field("error.type"), Some("RuntimeError"));
    assert_eq!(
        span.field("error.code"),
        None,
        "not a code the reference publishes"
    );
}

/// `test_exec_span_records_cancellation_during_finish_sink_delivery`.
#[tokio::test]
async fn a_command_cancelled_while_its_finish_is_delivered_records_its_exit_status() {
    let (log, _guard) = SpanLog::install();
    let delivering = Arc::new(tokio::sync::Notify::new());
    let started = Arc::clone(&delivering);
    let exit_codes: Arc<Mutex<Vec<i64>>> = Arc::default();
    let codes = Arc::clone(&exit_codes);
    let sink = CallbackSink::new_async(move |event, _session| {
        let started = Arc::clone(&started);
        let codes = Arc::clone(&codes);
        async move {
            if event.op() == OpName::Exec && event.phase() == EventPhase::Finish {
                codes
                    .lock()
                    .expect("codes")
                    .push(event.data()["exit_code"].as_i64().expect("exit code"));
                started.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    });
    let inner = MemorySession::empty();
    inner.answer_exec_with(ExecResult::new(Vec::new(), Vec::new(), 7));
    let session = InstrumentedSession::new(
        inner,
        Some(Arc::new(Instrumentation::with_sinks([
            Arc::new(sink) as Arc<dyn EventSink>
        ]))),
        None,
    )
    .expect("wrap");

    let exec = session.exec(command("exit 7"));
    tokio::select! {
        _ = exec => panic!("the sink never finishes"),
        () = delivering.notified() => {}
    }

    let span = log
        .sandbox_spans()
        .into_iter()
        .find(|span| span.kind() == "sandbox.exec")
        .expect("exec span");
    assert_eq!(span.field("outcome"), Some("cancelled"));
    assert_eq!(span.field("error.type"), None);
    assert_eq!(*exit_codes.lock().expect("codes"), [7]);
    assert_eq!(span.field("process.exit_code"), Some("7"));
}

/// `test_sandbox_session_ops_nest_under_sdk_trace_and_events_carry_trace_ids`, with the audit ids
/// this port uses in place of the SDK trace's.
#[tokio::test]
async fn every_recorded_operation_gets_a_span_in_the_order_it_ran() {
    let (log, _guard) = SpanLog::install();
    let workspace = tempfile::tempdir().expect("temp");
    let snapshots = tempfile::tempdir().expect("temp");
    let (events, sink) = recording_sink();
    let session = unix_local_session(
        workspace.path(),
        Some(snapshots.path()),
        Manifest::new(),
        Instrumentation::with_sinks([sink])
            .with_payload_policy(EventPayloadPolicy::new().with_include_exec_output(true)),
        &[8765],
    )
    .await;
    let written = b"hello from sandbox tracing test\n".to_vec();

    session.start().await.expect("start");
    assert!(session.running().await.expect("running"));
    session
        .write("notes.txt".into(), written.clone(), None)
        .await
        .expect("write");
    assert_eq!(
        session.read("notes.txt".into(), None).await.expect("read"),
        written
    );
    let endpoint = session.resolve_exposed_port(8765).await.expect("port");
    assert_eq!(
        (endpoint.host.as_str(), endpoint.port, endpoint.tls),
        ("127.0.0.1", 8765, false)
    );
    let archive = session.persist_workspace().await.expect("persist");
    assert!(!archive.is_empty());
    session.hydrate_workspace(archive).await.expect("hydrate");
    assert!(
        session
            .exec(command("sleep 1 && echo slow span"))
            .await
            .expect("slow")
            .ok()
    );
    assert!(session.exec(command("echo hi")).await.expect("fast").ok());
    let failing = session
        .exec(command("echo failing >&2; exit 7"))
        .await
        .expect("failing");
    assert_eq!(failing.exit_code, 7);
    session.close().await.expect("close");

    let spans = log.sandbox_spans();
    let kinds: Vec<&str> = spans.iter().map(span_log::ClosedSpan::kind).collect();
    assert_eq!(
        kinds,
        [
            "sandbox.start",
            "sandbox.running",
            "sandbox.write",
            "sandbox.read",
            "sandbox.resolve_exposed_port",
            "sandbox.persist_workspace",
            "sandbox.hydrate_workspace",
            "sandbox.exec",
            "sandbox.exec",
            "sandbox.exec",
            "sandbox.stop",
            "sandbox.shutdown",
        ]
    );
    for span in &spans {
        assert_eq!(span.name, "custom");
        assert_eq!(span.field("sandbox.backend"), Some("unix_local"));
        assert_eq!(
            span.field("sandbox.operation"),
            span.kind().strip_prefix("sandbox.")
        );
    }
    assert_eq!(spans[1].field("sandbox.alive"), Some("true"));
    assert_eq!(spans[4].field("server.address"), Some("127.0.0.1"));
    assert_eq!(spans[4].field("server.port"), Some("8765"));
    assert_eq!(spans[7].field("process.exit_code"), Some("0"));
    assert_eq!(spans[8].field("outcome"), Some("ok"));
    assert_eq!(spans[9].field("process.exit_code"), Some("7"));
    assert_eq!(spans[9].field("error.type"), Some("ExecNonZeroError"));
    assert_eq!(spans[9].field("outcome"), Some("error"));

    let session_ids: std::collections::BTreeSet<&str> = spans
        .iter()
        .map(|span| span.field("sandbox.session_id").expect("session id"))
        .collect();
    assert_eq!(session_ids.len(), 1);
    let session_id = *session_ids.iter().next().expect("one");
    assert_eq!(
        Uuid::parse_str(session_id).expect("uuid").to_string(),
        session_id
    );

    let events = recorded(&events);
    let exec_finishes = of_op(&events, OpName::Exec, EventPhase::Finish);
    assert_eq!(exec_finishes.len(), 3);
    let exec_finish = exec_finishes[0].as_finish().expect("finish");
    assert_eq!(exec_finish.base().trace_id(), None);
    assert_eq!(exec_finish.base().parent_span_id(), None);
    assert_eq!(
        Some(exec_finish.base().span_id()),
        spans[7].field("sandbox.audit_span_id")
    );
    // The two events of every operation share its audit id and number themselves in order.
    let seqs: Vec<u64> = events.iter().map(SandboxSessionEvent::seq).collect();
    assert_eq!(
        seqs,
        (1..=u64::try_from(events.len()).expect("len")).collect::<Vec<_>>()
    );
}

/// `test_sandbox_session_events_fallback_to_audit_ids_under_disabled_parent_span`.
#[tokio::test]
async fn an_operation_s_two_events_share_one_audit_id_and_no_trace() {
    let (events, sink) = recording_sink();
    let session = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(Instrumentation::with_sinks([sink]))),
        None,
    )
    .expect("wrap");

    assert!(session.exec(command("echo hi")).await.expect("exec").ok());

    let exec: Vec<SandboxSessionEvent> = recorded(&events)
        .into_iter()
        .filter(|event| event.op() == OpName::Exec)
        .collect();
    assert_eq!(exec.len(), 2);
    let (start, finish) = (exec[0].base(), exec[1].base());
    assert_eq!(exec[0].phase(), EventPhase::Start);
    assert_eq!(exec[1].phase(), EventPhase::Finish);
    assert_eq!(start.trace_id(), None);
    assert_eq!(finish.trace_id(), None);
    assert_eq!(start.parent_span_id(), None);
    assert_eq!(start.span_id(), finish.span_id());
    assert!(start.span_id().starts_with("sandbox_op_"));
    assert_eq!(start.span_id().len(), "sandbox_op_".len() + 32);
}

/// `test_sandbox_session_aclose_flushes_best_effort_sink_tasks`.
#[tokio::test]
async fn closing_waits_for_deliveries_still_in_flight() {
    let seen: Arc<Mutex<Vec<(OpName, EventPhase)>>> = Arc::default();
    let kept = Arc::clone(&seen);
    let sink = CallbackSink::new_async(move |event, _session| {
        let kept = Arc::clone(&kept);
        async move {
            tokio::task::yield_now().await;
            kept.lock().expect("seen").push((event.op(), event.phase()));
            Ok(())
        }
    })
    .with_mode(DeliveryMode::BestEffort)
    .with_on_error(OnErrorPolicy::Log);
    let session = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(Instrumentation::with_sinks([
            Arc::new(sink) as Arc<dyn EventSink>
        ]))),
        None,
    )
    .expect("wrap");

    session.start().await.expect("start");
    session.close().await.expect("close");

    let seen = seen.lock().expect("seen");
    assert!(
        seen.contains(&(OpName::Stop, EventPhase::Finish)),
        "{seen:?}"
    );
    assert!(
        seen.contains(&(OpName::Shutdown, EventPhase::Finish)),
        "{seen:?}"
    );
}

#[test]
fn an_ephemeral_outbox_that_cannot_be_excluded_refuses_the_wrap() {
    let sink = WorkspaceJsonlSink::new()
        .with_workspace_relpath("../outside.jsonl")
        .with_ephemeral(true);
    let error = InstrumentedSession::new(
        MemorySession::empty(),
        Some(Arc::new(Instrumentation::with_sinks([
            Arc::new(sink) as Arc<dyn EventSink>
        ]))),
        None,
    )
    .expect_err("an invalid skip path");
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
}

// --- test_session_manager.py ----------------------------------------------------------------------

fn finish_with_output(stdout: &[u8]) -> SandboxSessionEvent {
    SandboxSessionFinishEvent::new(Uuid::new_v4(), 1, OpName::Exec, "span_exec", true, 0.0)
        .with_output(Some(stdout.to_vec()), Some(Vec::new()))
        .into()
}

fn bound_recording_sink(policy: Option<EventPayloadPolicy>) -> (Events, Arc<dyn EventSink>) {
    let events: Events = Arc::default();
    let seen = Arc::clone(&events);
    let mut sink = CallbackSink::new(move |event, _session| {
        seen.lock().expect("events").push(event);
        Ok(())
    });
    if let Some(policy) = policy {
        sink = sink.with_payload_policy(policy);
    }
    sink.bind(MemorySession::empty()).expect("bind");
    (events, Arc::new(sink))
}

/// `test_instrumentation_per_op_policy_overrides_default`.
#[tokio::test]
async fn a_policy_for_the_operation_overrides_the_default() {
    let (events, sink) = bound_recording_sink(None);
    let instrumentation = Instrumentation::with_sinks([sink])
        .with_payload_policy(EventPayloadPolicy::new().with_include_exec_output(false))
        .with_payload_policy_for(
            OpName::Exec,
            EventPayloadPolicy::new().with_include_exec_output(true),
        );

    instrumentation
        .emit(&finish_with_output(b"hello"))
        .await
        .expect("emit");

    let event = &recorded(&events)[0];
    assert_eq!(event.as_finish().expect("finish").stdout(), Some("hello"));
}

/// `test_instrumentation_snapshots_per_op_policy_mapping`: the mapping is owned once given, so the
/// part that remains is that an operation's policy can turn output off again.
#[tokio::test]
async fn a_policy_for_the_operation_can_turn_output_off() {
    let (events, sink) = bound_recording_sink(None);
    let instrumentation = Instrumentation::with_sinks([sink])
        .with_payload_policy(EventPayloadPolicy::new().with_include_exec_output(true))
        .with_payload_policy_for(
            OpName::Exec,
            EventPayloadPolicy::new().with_include_exec_output(false),
        );

    instrumentation
        .emit(&finish_with_output(b"secret"))
        .await
        .expect("emit");

    let finish = recorded(&events)[0].as_finish().cloned().expect("finish");
    assert_eq!(finish.stdout(), None);
    assert_eq!(finish.stdout_bytes(), None);
}

/// `test_instrumentation_per_sink_policy_overrides_per_op`.
#[tokio::test]
async fn a_sink_s_own_policy_overrides_the_operation_s() {
    let (first, sink_a) = bound_recording_sink(None);
    let (second, sink_b) = bound_recording_sink(Some(
        EventPayloadPolicy::new().with_include_exec_output(true),
    ));
    let instrumentation = Instrumentation::with_sinks([sink_a, sink_b])
        .with_payload_policy(EventPayloadPolicy::new().with_include_exec_output(false))
        .with_payload_policy_for(
            OpName::Exec,
            EventPayloadPolicy::new().with_include_exec_output(false),
        );

    instrumentation
        .emit(&finish_with_output(b"hello"))
        .await
        .expect("emit");

    assert_eq!(
        recorded(&first)[0].as_finish().expect("finish").stdout(),
        None
    );
    assert_eq!(
        recorded(&second)[0].as_finish().expect("finish").stdout(),
        Some("hello")
    );
}

/// `test_instrumentation_redacts_raw_exec_bytes_when_output_disabled`.
#[tokio::test]
async fn raw_output_never_reaches_a_sink_that_may_not_see_it() {
    let (events, sink) = bound_recording_sink(None);
    let instrumentation = Instrumentation::with_sinks([sink])
        .with_payload_policy(EventPayloadPolicy::new().with_include_exec_output(false));

    instrumentation
        .emit(&finish_with_output(b"secret"))
        .await
        .expect("emit");

    let finish = recorded(&events)[0].as_finish().cloned().expect("finish");
    assert_eq!(finish.stdout_bytes(), None);
    assert_eq!(finish.stderr_bytes(), None);
}

/// A sink that runs a closure over each event, with the mode and policy given.
struct ScriptedSink<F> {
    mode: DeliveryMode,
    on_error: OnErrorPolicy,
    handle: F,
}

#[async_trait::async_trait]
impl<F, Fut> EventSink for ScriptedSink<F>
where
    F: Fn(SandboxSessionEvent) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<(), SinkError>> + Send,
{
    fn mode(&self) -> DeliveryMode {
        self.mode
    }

    fn on_error(&self) -> OnErrorPolicy {
        self.on_error
    }

    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError> {
        (self.handle)(event).await
    }
}

/// `test_chained_sink_preserves_completion_order_across_modes`.
#[tokio::test]
async fn a_group_waits_for_a_background_member_before_the_next() {
    let completed = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&completed);
    let slow = ScriptedSink {
        mode: DeliveryMode::BestEffort,
        on_error: OnErrorPolicy::Raise,
        handle: move |_event| {
            let done = Arc::clone(&done);
            async move {
                tokio::task::yield_now().await;
                done.store(true, Ordering::SeqCst);
                Ok(())
            }
        },
    };
    let checked = Arc::clone(&completed);
    let after = ScriptedSink {
        mode: DeliveryMode::Sync,
        on_error: OnErrorPolicy::Raise,
        handle: move |_event| {
            let ran_after = checked.load(Ordering::SeqCst);
            async move {
                if ran_after {
                    Ok(())
                } else {
                    Err("later sink ran before earlier sink completed".into())
                }
            }
        },
    };
    let instrumentation = Instrumentation::with_sinks([Arc::new(ChainedSink::new([
        Arc::new(slow) as Arc<dyn EventSink>,
        Arc::new(after),
    ])) as Arc<dyn EventSink>]);

    instrumentation
        .emit(&finish_event(Uuid::new_v4(), 1, OpName::Running))
        .await
        .expect("in order");
}

/// `test_async_sink_raise_propagates_to_emit`.
#[tokio::test]
async fn a_background_sink_that_raises_is_awaited_and_its_own_failure_let_out() {
    let failing = ScriptedSink {
        mode: DeliveryMode::Async,
        on_error: OnErrorPolicy::Raise,
        handle: |_event| async {
            tokio::task::yield_now().await;
            Err::<(), SinkError>("boom".into())
        },
    };
    let instrumentation = Instrumentation::with_sinks([Arc::new(failing) as Arc<dyn EventSink>]);

    let error = instrumentation
        .emit(&finish_event(Uuid::new_v4(), 1, OpName::Running))
        .await
        .expect_err("raised");
    assert_eq!(error.error_code(), ErrorCode::EventSinkFailed);
    assert_eq!(error.to_string(), "boom");
}

#[tokio::test]
async fn a_synchronous_sink_that_raises_is_reported_by_type_and_event() {
    let failing = ScriptedSink {
        mode: DeliveryMode::Sync,
        on_error: OnErrorPolicy::Raise,
        handle: |_event| async { Err::<(), SinkError>("boom".into()) },
    };
    let instrumentation = Instrumentation::with_sinks([Arc::new(failing) as Arc<dyn EventSink>]);
    let event = finish_event(Uuid::new_v4(), 1, OpName::Running);

    let error = instrumentation.emit(&event).await.expect_err("raised");
    let message = error.to_string();
    assert!(
        message.starts_with("sandbox event sink failed: "),
        "{message}"
    );
    assert!(
        message.ends_with(&format!(" while handling event {}", event.event_id())),
        "{message}"
    );
    assert!(std::error::Error::source(&error).is_some_and(|cause| cause.to_string() == "boom"));
}

#[tokio::test]
async fn a_best_effort_sink_never_fails_the_operation_whatever_its_policy() {
    let failing = ScriptedSink {
        mode: DeliveryMode::BestEffort,
        on_error: OnErrorPolicy::Raise,
        handle: |_event| async { Err::<(), SinkError>("boom".into()) },
    };
    let instrumentation = Instrumentation::with_sinks([Arc::new(failing) as Arc<dyn EventSink>]);

    instrumentation
        .emit(&finish_event(Uuid::new_v4(), 1, OpName::Running))
        .await
        .expect("never raised");
    instrumentation.flush().await;
}

/// A layer that keeps the message of every event logged at ERROR.
#[derive(Clone, Default)]
struct ErrorLog(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ErrorLog {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() != tracing::Level::ERROR {
            return;
        }
        struct Message<'a>(&'a mut Vec<String>);
        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push(format!("{}={value:?}", field.name()));
            }
        }
        let mut fields = Vec::new();
        event.record(&mut Message(&mut fields));
        self.0.lock().expect("log").push(fields.join(" "));
    }
}

/// `test_logged_sink_failure_conditionally_includes_sink_type`, the redacted case — the reference's
/// default. The diagnostic case depends on the framework-wide tool-data logging switch, which has
/// not been carried over.
#[tokio::test]
async fn a_logged_sink_failure_says_nothing_about_the_sink() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let log = ErrorLog::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
    let failing = ScriptedSink {
        mode: DeliveryMode::Sync,
        on_error: OnErrorPolicy::Log,
        handle: |_event| async { Err::<(), SinkError>("SECRET_SINK_ERROR".into()) },
    };
    let instrumentation = Instrumentation::with_sinks([Arc::new(failing) as Arc<dyn EventSink>]);

    instrumentation
        .emit(&finish_event(Uuid::new_v4(), 1, OpName::Running))
        .await
        .expect("logged, not raised");

    let lines = log.0.lock().expect("log").clone();
    assert_eq!(lines, ["message=Sandbox event sink failed (ignored)"]);
}

#[tokio::test]
async fn an_ignored_sink_failure_is_not_logged() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let log = ErrorLog::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
    let failing = ScriptedSink {
        mode: DeliveryMode::Sync,
        on_error: OnErrorPolicy::Ignore,
        handle: |_event| async { Err::<(), SinkError>("boom".into()) },
    };
    let instrumentation = Instrumentation::with_sinks([Arc::new(failing) as Arc<dyn EventSink>]);

    instrumentation
        .emit(&finish_event(Uuid::new_v4(), 1, OpName::Running))
        .await
        .expect("ignored");
    assert!(log.0.lock().expect("log").is_empty());
}
