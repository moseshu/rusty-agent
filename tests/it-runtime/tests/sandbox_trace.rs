//! What sandbox execution puts into its spans and its log, and what it keeps out of them.
//!
//! Ported from the reference's `test_sandbox_runtime_emits_high_level_sdk_spans` and the
//! `test_runner_owned_cleanup_redacts_*` cases of `tests/sandbox/test_runtime.py`. A cleanup
//! failure never reaches the caller — the runner logs it and keeps the run's own outcome — so the
//! log is where its redaction is observable.
//!
//! # Why this is its own test binary
//!
//! For the reason `runner_trace.rs` gives: span and event callsites decide whether anyone listens
//! the first time they are reached, for the whole process. Every case here installs its subscriber
//! before the runtime runs, and **a case that runs the runtime without one reopens that race**.

use std::{
    collections::HashMap,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::Result,
    item::{ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    sandbox::{
        AsUser, CreateRequest, Entry, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest,
        Mount, MountProvider, MountStrategy, OpName, S3Mount, SandboxAgentConfig, SandboxClient,
        SandboxError, SandboxResult, SandboxSession, SandboxSessionState, SessionPath,
        SessionResources, Snapshot, pre_stop_hook,
    },
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunRequest, Runner},
    sandbox::SandboxRunConfig,
};
use tracing::{
    Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
    subscriber::DefaultGuard,
};
use tracing_subscriber::{
    Layer,
    fmt::MakeWriter,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

// -- capture ------------------------------------------------------------------------------------

/// One closed span: its name, its parent's name, and its fields.
#[derive(Clone, Debug)]
struct SpanRecord {
    name: String,
    parent: Option<String>,
    fields: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct SpanCollector(Arc<Mutex<Vec<SpanRecord>>>);

impl SpanCollector {
    fn named(&self, name: &str) -> Vec<SpanRecord> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

struct SpanFields(HashMap<String, String>);

impl<S> Layer<S> for SpanCollector
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = HashMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(SpanFields(fields)) = span.extensions_mut().get_mut::<SpanFields>()
        {
            values.record(&mut FieldVisitor(fields));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let fields = span
            .extensions()
            .get::<SpanFields>()
            .map(|stored| stored.0.clone())
            .unwrap_or_default();
        self.0.lock().unwrap().push(SpanRecord {
            name: span.name().to_owned(),
            parent: span.parent().map(|parent| parent.name().to_owned()),
            fields,
        });
    }
}

/// Formatted output, for asserting on what the log says and what it never says.
#[derive(Clone, Default)]
struct TextCapture(Arc<Mutex<Vec<u8>>>);

impl TextCapture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for TextCapture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for TextCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs both captures for the current thread, which on a current-thread runtime is also where
/// the cleanup task the runtime spawns runs.
fn capture() -> (SpanCollector, TextCapture, DefaultGuard) {
    let spans = SpanCollector::default();
    let text = TextCapture::default();
    let subscriber = tracing_subscriber::registry()
        .with(spans.clone())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(text.clone())
                .with_ansi(false),
        )
        .with(tracing_subscriber::filter::LevelFilter::TRACE);
    let guard = tracing::subscriber::set_default(subscriber);
    (spans, text, guard)
}

// -- a backend ----------------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<String>>>;

fn note(log: &Log, entry: &str) {
    log.lock().unwrap().push(entry.to_owned());
}

/// Where cleanup fails, and with what message.
#[derive(Clone, Copy)]
enum CleanupFault {
    None,
    PreStop(&'static str),
    Delete(&'static str),
}

struct FakeSession {
    state: Mutex<SandboxSessionState>,
    resources: SessionResources,
    running: AtomicBool,
    log: Log,
}

#[async_trait]
impl SandboxSession for FakeSession {
    fn backend_id(&self) -> &str {
        "docker"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.lock().unwrap().clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }

    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Ok(Vec::new())
    }

    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn mkdir(
        &self,
        _path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn read(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn write(
        &self,
        _path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }

    fn replace_manifest(&self, manifest: Manifest) -> SandboxResult<()> {
        let mut state = self.state.lock().unwrap();
        *state = state.clone().with_manifest(manifest);
        Ok(())
    }

    async fn start(&self) -> SandboxResult<bool> {
        note(&self.log, "start");
        self.running.store(true, Ordering::SeqCst);
        Ok(false)
    }

    async fn stop(&self) -> SandboxResult<()> {
        note(&self.log, "stop");
        Ok(())
    }

    async fn shutdown(&self) -> SandboxResult<()> {
        note(&self.log, "shutdown");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn close_dependencies(&self) -> SandboxResult<()> {
        note(&self.log, "close_dependencies");
        Ok(())
    }
}

struct FakeClient {
    fault: CleanupFault,
    log: Log,
}

fn cleanup_failure(message: &str) -> SandboxError {
    SandboxError::new(ErrorCode::WorkspaceStopError, OpName::Stop, message)
}

#[async_trait]
impl SandboxClient for FakeClient {
    fn backend_id(&self) -> &str {
        "docker"
    }

    fn supports_default_options(&self) -> bool {
        true
    }

    async fn create(&self, request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>> {
        note(&self.log, "create");
        let session = FakeSession {
            state: Mutex::new(SandboxSessionState::new(
                "docker",
                Snapshot::noop(),
                request.manifest().cloned().unwrap_or_default(),
            )),
            resources: SessionResources::new(),
            running: AtomicBool::new(false),
            log: Arc::clone(&self.log),
        };
        if let CleanupFault::PreStop(message) = self.fault {
            session.register_pre_stop_hook(pre_stop_hook(move || async move {
                Err(cleanup_failure(message))
            }));
        }
        Ok(Box::new(session))
    }

    async fn resume(&self, _state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>> {
        Err(cleanup_failure("no resume in these cases"))
    }

    async fn delete(&self, _session: &dyn SandboxSession) -> SandboxResult<()> {
        note(&self.log, "delete");
        match self.fault {
            CleanupFault::Delete(message) => Err(cleanup_failure(message)),
            _ => Ok(()),
        }
    }
}

// -- a model ------------------------------------------------------------------------------------

struct Answering;

#[async_trait]
impl Model for Answering {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new("msg-1"),
            RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
        )]))
    }
}

struct FixedResolver;

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(Answering) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

// -- fixtures -----------------------------------------------------------------------------------

/// An S3 mount with its keys, mounted through a Docker volume.
fn docker_s3(secret: &str) -> Entry {
    Entry::mount(
        Mount::new(
            MountProvider::S3(S3Mount {
                bucket: "example-bucket".to_owned(),
                access_key_id: Some("example-access-key".to_owned()),
                secret_access_key: Some(secret.to_owned()),
                ..S3Mount::default()
            }),
            MountStrategy::docker_volume("rclone"),
        )
        .unwrap(),
    )
}

/// Runs a sandbox agent over `manifest` against a backend failing cleanup as `fault` says, and
/// returns what the backend was asked to do.
async fn run(manifest: Manifest, fault: CleanupFault) -> Vec<String> {
    let log = Log::default();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .instructions("do the task")
        .sandbox(SandboxAgentConfig::empty().with_default_manifest(manifest))
        .build()
        .unwrap();
    let client = Arc::new(FakeClient {
        fault,
        log: Arc::clone(&log),
    });
    let result =
        Runner::run(
            RunRequest::new(
                AgentBinding::direct(agent),
                Arc::new(FixedResolver),
                RunId::new("run-sandbox"),
                CancelScope::root(),
                vec![ModelInputItem::Message(Message::user("go"))],
            )
            .with_config(RunConfig::new().with_sandbox(
                SandboxRunConfig::new().with_client(client as Arc<dyn SandboxClient>),
            )),
        )
        .await
        .unwrap();
    assert_eq!(result.final_text(), "done");
    if !matches!(fault, CleanupFault::None) {
        assert_eq!(result.state().sandbox_resume_state(), None);
    }
    log.lock().unwrap().clone()
}

// -- cases --------------------------------------------------------------------------------------

/// `test_sandbox_runtime_emits_high_level_sdk_spans`, for the spans the runtime itself opens:
/// preparing the agent, creating its session, and cleaning up. The reference's `sandbox.start`,
/// `sandbox.stop` and `sandbox.shutdown` come from its wrapper around every session, which is not
/// here.
#[tokio::test]
async fn the_runtime_marks_preparation_creation_and_cleanup() {
    let (spans, _text, _guard) = capture();
    run(Manifest::new(), CleanupFault::None).await;

    let prepare = spans.named("sandbox.prepare_agent");
    assert_eq!(prepare.len(), 1, "{prepare:?}");
    assert_eq!(
        prepare[0].fields.get("agent.name").map(String::as_str),
        Some("Coder")
    );
    let create = spans.named("sandbox.create_session");
    assert_eq!(create.len(), 1, "{create:?}");
    assert_eq!(create[0].parent.as_deref(), Some("sandbox.prepare_agent"));
    assert_eq!(
        create[0].fields.get("backend_id").map(String::as_str),
        Some("docker")
    );
    assert_eq!(spans.named("sandbox.cleanup").len(), 1);
    let sessions = spans.named("sandbox.cleanup_sessions");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0].parent.as_deref(), Some("sandbox.cleanup"));
    assert_eq!(
        sessions[0].fields.get("session_count").map(String::as_str),
        Some("1")
    );
}

/// `test_runner_owned_cleanup_redacts_pre_stop_hook_failure` and
/// `test_runner_owned_cleanup_redacts_client_delete_failure`: over a manifest carrying mount
/// credentials, a cleanup failure is logged without what its message quoted, and every step after
/// it still runs. Over one without, the same failure keeps its words.
#[tokio::test]
async fn a_cleanup_failure_over_mount_credentials_is_logged_without_them() {
    let (_spans, text, _guard) = capture();

    let secret = "runner-pre-stop-hook-secret";
    let log = run(
        Manifest::new().with_entry("data", docker_s3(secret)),
        CleanupFault::PreStop("pre-stop hook failed with runner-pre-stop-hook-secret"),
    )
    .await;
    // A failed pre-stop callback skips the stop, and the rest still runs.
    assert_eq!(
        log,
        [
            "create",
            "start",
            "shutdown",
            "delete",
            "close_dependencies"
        ]
    );

    let secret_on_delete = "client-delete-secret";
    let log = run(
        Manifest::new().with_entry("data", docker_s3(secret_on_delete)),
        CleanupFault::Delete("delete failed with client-delete-secret"),
    )
    .await;
    assert_eq!(
        log,
        [
            "create",
            "start",
            "stop",
            "shutdown",
            "delete",
            "close_dependencies"
        ]
    );

    let logged = text.text();
    assert!(!logged.contains(secret), "{logged}");
    assert!(!logged.contains(secret_on_delete), "{logged}");
    assert!(!logged.contains("example-access-key"), "{logged}");
    let warnings: Vec<&str> = logged
        .lines()
        .filter(|line| line.contains("failed to clean up sandbox resources after run"))
        .collect();
    assert_eq!(warnings.len(), 2, "{logged}");
    for warning in warnings {
        assert!(
            warning
                .contains("sandbox operation failed while using a protected mount configuration"),
            "{warning}"
        );
    }

    // Nothing to protect: the failure is logged as it was raised.
    let log = run(
        Manifest::new(),
        CleanupFault::Delete("delete failed for an ordinary reason"),
    )
    .await;
    assert!(log.contains(&"close_dependencies".to_owned()));
    assert!(
        text.text().contains("delete failed for an ordinary reason"),
        "{}",
        text.text()
    );
}
