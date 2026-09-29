//! The wrapper that emits audit events and trace spans around a session's operations.
//!
//! # Which operations are recorded
//!
//! As in the reference: start, stop, shutdown, exec, read, write, running, port resolution, and
//! persisting and hydrating the workspace. Listing, removing, making directories and the
//! interactive-process calls pass straight through, unrecorded. Each recorded operation gets a start
//! event, a finish event with the same span id, and a `tracing` span of kind `sandbox.<op>`.
//!
//! **Materializing and unpacking are recorded by their parts.** The reference's wrapper inherits
//! its base class's manifest application and archive extraction, which then write through the
//! wrapper, so each file they write is a recorded `write`. Here the backend keeps its own
//! materialization and is handed this wrapper to make its writes through
//! ([`SandboxSession::apply_manifest_through`]); archive extraction, which both built-in backends
//! do with the shared extractor, is run by the wrapper through itself. A session's own start does
//! not go through the wrapper, so what it materializes is not recorded write by write — the same
//! as in the reference, where start runs on the inner session.
//!
//! One deviation comes of keeping the backend's checks: the reference's wrapper materializes with
//! the base class's code, so a backend's own refusals — the local backend refusing manifest
//! accounts and host path grants, for one — do not run when a manifest is applied through the
//! wrapper. They run here. Skipping them is an accident of the reference's inheritance, and on the
//! local backend it would create accounts on the host machine.
//!
//! # Spans and audit ids
//!
//! The reference opens an SDK trace span per operation only when an SDK trace is active, and then
//! uses that span's ids in the events. This framework records spans with `tracing`, which has no
//! trace ids to share, and the port of the SDK's own trace objects is a later task. So every event
//! carries an audit id of its own — `sandbox_op_` and 32 hex digits, the reference's fallback —
//! with no trace or parent id, and the span records that id in `sandbox.audit_span_id` so a span
//! and its events can still be matched.
//!
//! # Failures are redacted before they are recorded
//!
//! Every recorded operation, and close, passes its failure through
//! [`SandboxSession::redact_mount_error`] first, as the reference's `@redact_mount_error_data` sits
//! inside its instrumentation decorator. A session whose manifest carries mount authority therefore
//! never puts a failure's context into an event.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, CompressionScheme, Dependencies, Entry, ErrorCode, ExecRequest, ExecResult,
    ExposedPortEndpoint, FileEntry, Manifest, MaterializationResult, MaterializedFile, OpName,
    PosixPath, PreStopHook, PtyExecUpdate, PtyStartRequest, PtyWriteRequest, SandboxArchiveLimits,
    SandboxConcurrencyLimits, SandboxError, SandboxResult, SandboxSession, SandboxSessionEvent,
    SandboxSessionEventBase, SandboxSessionFinishEvent, SandboxSessionStartEvent,
    SandboxSessionState, SessionPath, SessionResources, ShellInvocation, SnapshotFingerprint,
    WorkspacePathPolicy,
};
use ra_core::trace::{SpanKind, SpanOutcome, record_outcome};
use serde_json::{Map, Value, json};
use tracing::Instrument as _;
use tracing::field::Empty;

use super::Instrumentation;
use crate::archive::WorkspaceArchiveExtractor;

/// A session whose operations are recorded as audit events and trace spans.
///
/// Cheap to clone: clones share the wrapped session, the instrumentation and the event sequence.
#[derive(Clone)]
pub struct InstrumentedSession {
    inner: Arc<dyn SandboxSession>,
    instrumentation: Arc<Instrumentation>,
    seq: Arc<AtomicU64>,
}

impl std::fmt::Debug for InstrumentedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstrumentedSession")
            .field("backend", &self.inner.backend_id())
            .finish_non_exhaustive()
    }
}

/// What a successful operation adds to its finish event and span.
struct Recorded {
    /// Replaces the start data in the finish event; `None` keeps the start data.
    finish_data: Option<Map<String, Value>>,
    ok: bool,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
}

impl Recorded {
    const fn plain() -> Self {
        Self {
            finish_data: None,
            ok: true,
            stdout: None,
            stderr: None,
        }
    }
}

impl InstrumentedSession {
    /// Wraps `inner`, delivering its events through `instrumentation` — a fresh one without sinks
    /// when `None` — and giving it `dependencies` when there are any.
    ///
    /// Every sink is bound to `inner` here, a group's members one by one, so a sink that uses the
    /// session reaches it without going through this wrapper.
    ///
    /// # Errors
    ///
    /// Returns the first sink's failure to bind.
    pub fn new(
        inner: Arc<dyn SandboxSession>,
        instrumentation: Option<Arc<Instrumentation>>,
        dependencies: Option<Arc<Dependencies>>,
    ) -> SandboxResult<Self> {
        inner.set_dependencies(dependencies);
        let session = Self {
            inner,
            instrumentation: instrumentation.unwrap_or_default(),
            seq: Arc::new(AtomicU64::new(0)),
        };
        session.bind_session_to_sinks()?;
        Ok(session)
    }

    /// The session this one records.
    #[must_use]
    pub const fn inner(&self) -> &Arc<dyn SandboxSession> {
        &self.inner
    }

    /// Where this session's events go.
    #[must_use]
    pub const fn instrumentation(&self) -> &Arc<Instrumentation> {
        &self.instrumentation
    }

    fn bind_session_to_sinks(&self) -> SandboxResult<()> {
        for sink in self.instrumentation.sinks() {
            match sink.grouped_sinks() {
                Some(members) => {
                    for member in members {
                        member.bind(Arc::clone(&self.inner))?;
                    }
                }
                None => sink.bind(Arc::clone(&self.inner))?,
            }
        }
        Ok(())
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn as_session(&self) -> Arc<dyn SandboxSession> {
        Arc::new(self.clone())
    }

    /// Runs `run` between a start and a finish event, inside a span.
    ///
    /// `expected` lists failure codes the caller handles itself: the events record them as the
    /// failures they are, but the span does not count them as errors.
    #[allow(clippy::too_many_lines)] // The reference's `_annotate`: one sequence, both outcomes.
    async fn annotate<T, F>(
        &self,
        op: OpName,
        start_data: Map<String, Value>,
        run: F,
        describe: impl FnOnce(&T, &Map<String, Value>) -> Recorded + Send,
        expected: &[ErrorCode],
    ) -> SandboxResult<T>
    where
        F: Future<Output = SandboxResult<T>> + Send,
        T: Send,
    {
        let session_id = self.inner.state().session_id();
        let span_id = format!("sandbox_op_{}", uuid::Uuid::new_v4().simple());
        let kind = SpanKind::custom(format!("sandbox.{}", op.as_str()));
        let span = tracing::info_span!(
            "custom",
            span.kind = kind.label(),
            span.label = kind.label(),
            sandbox.backend = self.inner.backend_id(),
            sandbox.operation = op.as_str(),
            sandbox.session_id = %session_id,
            sandbox.audit_span_id = %span_id,
            outcome = Empty,
            duration.ms = Empty,
            error.code = Empty,
            "error.type" = Empty,
            error.retryable = Empty,
            sandbox.alive = Empty,
            process.exit_code = Empty,
            server.port = Empty,
            server.address = Empty,
        );

        let span_handle = span.clone();
        async move {
            // Until the operation finishes, dropping this future is a cancellation, and the guard
            // says so on the span with whatever the operation had produced by then.
            let mut unfinished = Unfinished::new(span_handle.clone(), start_data.clone());
            let start = SandboxSessionStartEvent::from_base(
                SandboxSessionEventBase::new(session_id, self.next_seq(), op, &span_id)
                    .with_data(start_data.clone()),
            );
            if let Err(sink_error) = self
                .instrumentation
                .emit(&SandboxSessionEvent::from(start))
                .await
            {
                unfinished.disarm();
                record_span_finish(
                    &span_handle,
                    &start_data,
                    Some(&sink_error),
                    false,
                    op,
                    None,
                );
                return Err(sink_error);
            }

            let started = Instant::now();
            unfinished.started = Some(started);
            match run.await {
                Err(error) => {
                    let finish = SandboxSessionFinishEvent::from_base(
                        SandboxSessionEventBase::new(session_id, self.next_seq(), op, &span_id)
                            .with_data(start_data.clone()),
                        false,
                        elapsed_ms(started),
                    )
                    .with_failure(&error);
                    let delivered = self
                        .instrumentation
                        .emit(&SandboxSessionEvent::from(finish))
                        .await;
                    unfinished.disarm();
                    if let Err(sink_error) = delivered {
                        record_span_finish(
                            &span_handle,
                            &start_data,
                            Some(&sink_error),
                            false,
                            op,
                            Some(started),
                        );
                        return Err(sink_error);
                    }
                    let is_expected = expected.contains(&error.error_code());
                    record_span_finish(
                        &span_handle,
                        &start_data,
                        (!is_expected).then_some(&error),
                        is_expected,
                        op,
                        Some(started),
                    );
                    Err(error)
                }
                Ok(value) => {
                    let recorded = describe(&value, &start_data);
                    let finish_data = recorded.finish_data.unwrap_or(start_data);
                    unfinished.data = finish_data.clone();
                    let finish = SandboxSessionFinishEvent::from_base(
                        SandboxSessionEventBase::new(session_id, self.next_seq(), op, &span_id)
                            .with_data(finish_data.clone()),
                        recorded.ok,
                        elapsed_ms(started),
                    )
                    .with_output(recorded.stdout, recorded.stderr);
                    let delivered = self
                        .instrumentation
                        .emit(&SandboxSessionEvent::from(finish))
                        .await;
                    unfinished.disarm();
                    if let Err(sink_error) = delivered {
                        record_span_finish(
                            &span_handle,
                            &finish_data,
                            Some(&sink_error),
                            false,
                            op,
                            Some(started),
                        );
                        return Err(sink_error);
                    }
                    record_span_finish(
                        &span_handle,
                        &finish_data,
                        None,
                        recorded.ok,
                        op,
                        Some(started),
                    );
                    Ok(value)
                }
            }
        }
        .instrument(span)
        .await
    }

    /// The reference's `persist_workspace` / `hydrate_workspace` preamble: the mount credential
    /// boundary, checked against the wrapped session's current manifest.
    fn validate_inner_mount_boundaries(&self) -> SandboxResult<()> {
        self.inner.validate_mount_credential_boundaries()
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

/// Records a cancellation on an operation's span if the operation is dropped before it finished.
///
/// The reference marks the span with the `CancelledError` that interrupted it. A cancellation is not
/// a failure in this framework's span vocabulary, so the outcome is `cancelled` and no error type is
/// recorded; what the operation had produced by then — an exit status whose finish event was still
/// being delivered, say — is recorded as it would have been.
struct Unfinished {
    span: tracing::Span,
    data: Map<String, Value>,
    started: Option<Instant>,
    armed: bool,
}

impl Unfinished {
    const fn new(span: tracing::Span, data: Map<String, Value>) -> Self {
        Self {
            span,
            data,
            started: None,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if self.armed {
            record_span_data(&self.span, &self.data, self.started);
            record_outcome(&self.span, SpanOutcome::Cancelled);
        }
    }
}

/// Copies what the operation data says about liveness, exit status and ports onto the span, with
/// the elapsed time.
fn record_span_data(span: &tracing::Span, data: &Map<String, Value>, started: Option<Instant>) {
    if let Some(started) = started {
        span.record("duration.ms", elapsed_ms(started));
    }
    if let Some(alive) = data.get("alive").and_then(Value::as_bool) {
        span.record("sandbox.alive", alive);
    }
    if let Some(exit_code) = data.get("exit_code").and_then(Value::as_i64) {
        span.record("process.exit_code", exit_code);
    }
    if let Some(port) = data.get("server.port").and_then(Value::as_u64) {
        span.record("server.port", port);
    }
    if let Some(address) = data.get("server.address").and_then(Value::as_str) {
        span.record("server.address", address);
    }
}

/// Records how an operation ended on its span.
///
/// `failure` is the error to record, when the operation failed and that failure is not one the
/// caller expected; `ok` is the operation's own verdict otherwise. What the data says about
/// liveness, exit status and ports is copied onto the span either way.
fn record_span_finish(
    span: &tracing::Span,
    data: &Map<String, Value>,
    failure: Option<&SandboxError>,
    ok: bool,
    op: OpName,
    started: Option<Instant>,
) {
    record_span_data(span, data, started);
    if let Some(error) = failure {
        span.record("error.type", error.error_code().reference_type_name());
        if let Some(code) = error.error_code().reference_code() {
            span.record("error.code", code.as_str());
        }
        if let Some(retryable) = error.retryable() {
            span.record("error.retryable", retryable);
        }
        record_outcome(span, SpanOutcome::Error);
        return;
    }
    if !ok {
        if op == OpName::Exec {
            span.record("error.type", "ExecNonZeroError");
        }
        record_outcome(span, SpanOutcome::Error);
        return;
    }
    record_outcome(span, SpanOutcome::Ok);
}

fn user_value(user: Option<&ra_core::sandbox::User>) -> Value {
    user.map_or(Value::Null, |user| Value::String(user.name.clone()))
}

fn exec_start_data(request: &ExecRequest) -> Map<String, Value> {
    let shell = match request.shell() {
        ShellInvocation::Login => Value::Bool(true),
        ShellInvocation::None => Value::Bool(false),
        ShellInvocation::Prefix(prefix) => json!(prefix),
    };
    let mut data = Map::new();
    data.insert("command".to_owned(), json!(request.command()));
    data.insert(
        "timeout_s".to_owned(),
        request
            .timeout_s()
            .map_or(Value::Null, |timeout| json!(timeout)),
    );
    data.insert("shell".to_owned(), shell);
    data.insert("user".to_owned(), user_value(request.user()));
    data
}

fn path_start_data(path: &str, user: Option<&ra_core::sandbox::User>) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("path".to_owned(), Value::String(path.to_owned()));
    data.insert("user".to_owned(), user_value(user));
    data
}

/// Whether a resolved host is one a trace may name: `localhost`, or a loopback address.
fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim().to_lowercase();
    if normalized == "localhost" || normalized == "::1" {
        return true;
    }
    normalized
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn exposed_port_finish_data(endpoint: &ExposedPortEndpoint) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("server.port".to_owned(), json!(endpoint.port));
    if is_loopback_host(&endpoint.host) {
        data.insert(
            "server.address".to_owned(),
            Value::String(endpoint.host.clone()),
        );
    }
    data
}

/// Where a local snapshot writes its archive on the host, best effort.
fn snapshot_tar_path(state: &SandboxSessionState) -> Option<String> {
    let snapshot = state.snapshot();
    let base = snapshot.local_base_path()?;
    if snapshot.id().is_empty() {
        return None;
    }
    let mut path = base.join(snapshot.id()).into_os_string();
    path.push(".tar");
    Some(path.to_string_lossy().into_owned())
}

#[async_trait]
impl SandboxSession for InstrumentedSession {
    fn backend_id(&self) -> &str {
        self.inner.backend_id()
    }

    fn state(&self) -> SandboxSessionState {
        self.inner.state()
    }

    fn resources(&self) -> &SessionResources {
        self.inner.resources()
    }

    fn inner_session(&self) -> Option<Arc<dyn SandboxSession>> {
        Some(Arc::clone(&self.inner))
    }

    fn dependencies(&self) -> Arc<Dependencies> {
        self.inner.dependencies()
    }

    fn set_dependencies(&self, dependencies: Option<Arc<Dependencies>>) {
        self.inner.set_dependencies(dependencies);
    }

    fn register_pre_stop_hook(&self, hook: PreStopHook) {
        self.inner.register_pre_stop_hook(hook);
    }

    fn register_persist_workspace_skip_path(
        &self,
        path: SessionPath<'_>,
    ) -> SandboxResult<PosixPath> {
        self.inner.register_persist_workspace_skip_path(path)
    }

    fn persist_workspace_skip_relpaths(&self) -> SandboxResult<BTreeSet<PosixPath>> {
        self.inner.persist_workspace_skip_relpaths()
    }

    fn set_concurrency_limits(&self, limits: SandboxConcurrencyLimits) {
        self.inner.set_concurrency_limits(limits);
    }

    fn concurrency_limits(&self) -> SandboxConcurrencyLimits {
        self.inner.concurrency_limits()
    }

    fn set_archive_limits(&self, limits: Option<SandboxArchiveLimits>) {
        self.inner.set_archive_limits(limits);
    }

    fn archive_limits(&self) -> Option<SandboxArchiveLimits> {
        self.inner.archive_limits()
    }

    fn replace_manifest(&self, manifest: Manifest) -> SandboxResult<()> {
        self.inner.replace_manifest(manifest)
    }

    async fn apply_manifest_entries(
        &self,
        entries: Vec<(PosixPath, Entry)>,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        self.inner
            .apply_manifest_entries_through(self.as_session(), entries)
            .await
    }

    async fn apply_manifest_entries_through(
        &self,
        through: Arc<dyn SandboxSession>,
        entries: Vec<(PosixPath, Entry)>,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        self.inner
            .apply_manifest_entries_through(through, entries)
            .await
    }

    async fn validate_manifest_application(
        &self,
        manifest: &Manifest,
        session_running: bool,
    ) -> SandboxResult<()> {
        self.inner
            .validate_manifest_application(manifest, session_running)
            .await
    }

    fn supports_pty(&self) -> bool {
        self.inner.supports_pty()
    }

    fn supports_volume_mounts(&self) -> bool {
        self.inner.supports_volume_mounts()
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        let start_data = exec_start_data(&request);
        self.annotate(
            OpName::Exec,
            start_data,
            async {
                self.inner
                    .exec(request)
                    .await
                    .map_err(|error| self.redact_mount_error(error))
            },
            |result: &ExecResult, start_data| {
                let mut data = start_data.clone();
                data.insert("exit_code".to_owned(), json!(result.exit_code));
                data.insert("process.exit.code".to_owned(), json!(result.exit_code));
                Recorded {
                    finish_data: Some(data),
                    ok: result.ok(),
                    stdout: Some(result.stdout.clone()),
                    stderr: Some(result.stderr.clone()),
                }
            },
            &[],
        )
        .await
    }

    async fn running(&self) -> SandboxResult<bool> {
        self.annotate(
            OpName::Running,
            Map::new(),
            self.inner.running(),
            |alive: &bool, _| {
                let mut data = Map::new();
                data.insert("alive".to_owned(), Value::Bool(*alive));
                Recorded {
                    finish_data: Some(data),
                    ..Recorded::plain()
                }
            },
            &[],
        )
        .await
    }

    async fn pty_start(&self, request: PtyStartRequest) -> SandboxResult<PtyExecUpdate> {
        self.inner.pty_start(request).await
    }

    async fn pty_write(&self, request: PtyWriteRequest) -> SandboxResult<PtyExecUpdate> {
        self.inner.pty_write(request).await
    }

    async fn pty_terminate_all(&self) -> SandboxResult<()> {
        self.inner.pty_terminate_all().await
    }

    fn pty_unsupported(&self) -> SandboxError {
        self.inner.pty_unsupported()
    }

    fn workspace_path_policy(&self) -> SandboxResult<WorkspacePathPolicy> {
        self.inner.workspace_path_policy()
    }

    async fn validate_path_access(
        &self,
        path: SessionPath<'_>,
        for_write: bool,
    ) -> SandboxResult<PosixPath> {
        self.inner.validate_path_access(path, for_write).await
    }

    async fn ls(&self, path: SessionPath<'_>, user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        self.inner.ls(path, user).await
    }

    async fn rm(&self, path: SessionPath<'_>, recursive: bool, user: AsUser) -> SandboxResult<()> {
        self.inner.rm(path, recursive, user).await
    }

    async fn mkdir(&self, path: SessionPath<'_>, parents: bool, user: AsUser) -> SandboxResult<()> {
        self.inner.mkdir(path, parents, user).await
    }

    async fn read(&self, path: SessionPath<'_>, user: AsUser) -> SandboxResult<Vec<u8>> {
        self.read_expecting(path, user, &[]).await
    }

    async fn read_expecting(
        &self,
        path: SessionPath<'_>,
        user: AsUser,
        expected: &[ErrorCode],
    ) -> SandboxResult<Vec<u8>> {
        let start_data = path_start_data(path.as_str(), user.as_ref());
        self.annotate(
            OpName::Read,
            start_data,
            self.inner.read(path, user.clone()),
            |_: &Vec<u8>, _| Recorded::plain(),
            expected,
        )
        .await
    }

    async fn read_up_to(
        &self,
        path: SessionPath<'_>,
        user: AsUser,
        max_bytes: u64,
    ) -> SandboxResult<Vec<u8>> {
        let start_data = path_start_data(path.as_str(), user.as_ref());
        self.annotate(
            OpName::Read,
            start_data,
            self.inner.read_up_to(path, user.clone(), max_bytes),
            |_: &Vec<u8>, _| Recorded::plain(),
            &[],
        )
        .await
    }

    async fn write(&self, path: SessionPath<'_>, data: Vec<u8>, user: AsUser) -> SandboxResult<()> {
        let mut start_data = path_start_data(path.as_str(), user.as_ref());
        start_data.insert("bytes".to_owned(), json!(data.len()));
        self.annotate(
            OpName::Write,
            start_data,
            self.inner.write(path, data, user.clone()),
            |(): &(), _| Recorded::plain(),
            &[],
        )
        .await
    }

    async fn extract(
        &self,
        path: SessionPath<'_>,
        data: Vec<u8>,
        scheme: Option<CompressionScheme>,
        limits: Option<SandboxArchiveLimits>,
    ) -> SandboxResult<()> {
        // Through this wrapper, as the reference's inherited extraction is: each member written is
        // a recorded write.
        WorkspaceArchiveExtractor::new(self)
            .extract(path, data, scheme, limits.or_else(|| self.archive_limits()))
            .await
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        let state = self.inner.state();
        let mut start_data = Map::new();
        start_data.insert(
            "workspace_root".to_owned(),
            Value::String(state.manifest().root.clone()),
        );
        if let Some(tar_path) = snapshot_tar_path(&state) {
            start_data.insert("tar_path".to_owned(), Value::String(tar_path));
        }
        self.annotate(
            OpName::PersistWorkspace,
            start_data,
            async {
                let outcome = async {
                    self.validate_inner_mount_boundaries()?;
                    self.inner.persist_workspace().await
                }
                .await;
                outcome.map_err(|error| self.redact_mount_error(error))
            },
            |archive: &Vec<u8>, start_data| {
                let mut data = start_data.clone();
                data.insert("bytes".to_owned(), json!(archive.len()));
                Recorded {
                    finish_data: Some(data),
                    ..Recorded::plain()
                }
            },
            &[],
        )
        .await
    }

    async fn hydrate_workspace(&self, data: Vec<u8>) -> SandboxResult<()> {
        let mut start_data = Map::new();
        start_data.insert(
            "untar_dir".to_owned(),
            Value::String(self.inner.state().manifest().root.clone()),
        );
        start_data.insert("bytes".to_owned(), json!(data.len()));
        self.annotate(
            OpName::HydrateWorkspace,
            start_data,
            async {
                let outcome = async {
                    self.validate_inner_mount_boundaries()?;
                    self.inner.hydrate_workspace(data).await
                }
                .await;
                outcome.map_err(|error| self.redact_mount_error(error))
            },
            |(): &(), _| Recorded::plain(),
            &[],
        )
        .await
    }

    async fn resolve_exposed_port(&self, port: u16) -> SandboxResult<ExposedPortEndpoint> {
        let mut start_data = Map::new();
        start_data.insert("port".to_owned(), json!(port));
        self.annotate(
            OpName::ResolveExposedPort,
            start_data,
            async {
                self.inner
                    .resolve_exposed_port(port)
                    .await
                    .map_err(|error| self.redact_mount_error(error))
            },
            |endpoint: &ExposedPortEndpoint, _| Recorded {
                finish_data: Some(exposed_port_finish_data(endpoint)),
                ..Recorded::plain()
            },
            &[],
        )
        .await
    }

    // --- lifecycle hooks: the wrapped session's own ------------------------------------------

    async fn ensure_backend_started(&self) -> SandboxResult<()> {
        self.inner.ensure_backend_started().await
    }

    async fn probe_workspace_root(&self) -> SandboxResult<bool> {
        self.inner.probe_workspace_root().await
    }

    async fn prepare_backend_workspace(&self) -> SandboxResult<()> {
        self.inner.prepare_backend_workspace().await
    }

    async fn ensure_runtime_helpers(&self) -> SandboxResult<()> {
        self.inner.ensure_runtime_helpers().await
    }

    async fn snapshot_restorable(&self) -> SandboxResult<bool> {
        self.inner.snapshot_restorable().await
    }

    fn workspace_state_preserved_on_start(&self) -> bool {
        self.inner.workspace_state_preserved_on_start()
    }

    async fn can_skip_snapshot_restore(&self, is_running: bool) -> SandboxResult<bool> {
        self.inner.can_skip_snapshot_restore(is_running).await
    }

    fn system_state_preserved_on_start(&self) -> bool {
        self.inner.system_state_preserved_on_start()
    }

    fn should_provision_accounts(&self) -> bool {
        self.inner.should_provision_accounts()
    }

    async fn provision_accounts(&self) -> SandboxResult<()> {
        self.inner.provision_accounts().await
    }

    async fn restore_snapshot(&self) -> SandboxResult<()> {
        self.inner.restore_snapshot().await
    }

    async fn remove_workspace_entry_on_resume(&self, path: SessionPath<'_>) -> SandboxResult<()> {
        self.inner.remove_workspace_entry_on_resume(path).await
    }

    async fn reapply_ephemeral_manifest(&self) -> SandboxResult<()> {
        self.inner.reapply_ephemeral_manifest().await
    }

    async fn apply_manifest(
        &self,
        provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        self.inner
            .apply_manifest_through(self.as_session(), provision_accounts)
            .await
            .map_err(|error| self.redact_mount_error(error))
    }

    async fn apply_manifest_through(
        &self,
        through: Arc<dyn SandboxSession>,
        provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        self.inner
            .apply_manifest_through(through, provision_accounts)
            .await
    }

    async fn after_start(&self) -> SandboxResult<()> {
        self.inner.after_start().await
    }

    async fn record_workspace_root_ready(&self) -> SandboxResult<()> {
        self.inner.record_workspace_root_ready().await
    }

    async fn after_start_failed(&self) {
        self.inner.after_start_failed().await;
    }

    fn wrap_start_error(&self, error: SandboxError) -> SandboxError {
        self.inner.wrap_start_error(error)
    }

    async fn before_stop(&self) -> SandboxResult<()> {
        self.inner.before_stop().await
    }

    async fn persist_snapshot(&self) -> SandboxResult<()> {
        self.inner.persist_snapshot().await
    }

    async fn record_snapshot_fingerprint(
        &self,
        fingerprint: Option<SnapshotFingerprint>,
    ) -> SandboxResult<()> {
        self.inner.record_snapshot_fingerprint(fingerprint).await
    }

    async fn after_stop(&self) {
        self.inner.after_stop().await;
    }

    fn wrap_stop_error(&self, error: SandboxError) -> SandboxError {
        self.inner.wrap_stop_error(error)
    }

    async fn before_shutdown(&self) -> SandboxResult<()> {
        self.inner.before_shutdown().await
    }

    async fn shutdown_backend(&self) -> SandboxResult<()> {
        self.inner.shutdown_backend().await
    }

    async fn after_shutdown(&self) -> SandboxResult<()> {
        self.inner.after_shutdown().await
    }

    async fn run_pre_stop_hooks(&self) -> SandboxResult<()> {
        self.inner.run_pre_stop_hooks().await
    }

    fn pre_stop_hooks_failed(&self) -> bool {
        self.inner.pre_stop_hooks_failed()
    }

    async fn close_dependencies(&self) -> SandboxResult<()> {
        self.inner.close_dependencies().await
    }

    fn validate_mount_credential_boundaries(&self) -> SandboxResult<()> {
        self.inner.validate_mount_credential_boundaries()
    }

    // --- lifecycle: recorded around the wrapped session's own --------------------------------

    async fn start(&self) -> SandboxResult<bool> {
        self.annotate(
            OpName::Start,
            Map::new(),
            async {
                self.inner
                    .start()
                    .await
                    .map_err(|error| self.redact_mount_error(error))
            },
            |_: &bool, _| Recorded::plain(),
            &[],
        )
        .await
    }

    async fn start_guarded(&self) -> SandboxResult<bool> {
        self.inner.start_guarded().await
    }

    async fn start_workspace(&self, root_ready_at_start: bool) -> SandboxResult<()> {
        self.inner.start_workspace(root_ready_at_start).await
    }

    async fn stop(&self) -> SandboxResult<()> {
        self.annotate(
            OpName::Stop,
            Map::new(),
            async {
                self.inner
                    .stop()
                    .await
                    .map_err(|error| self.redact_mount_error(error))
            },
            |(): &(), _| Recorded::plain(),
            &[],
        )
        .await
    }

    async fn shutdown(&self) -> SandboxResult<()> {
        self.annotate(
            OpName::Shutdown,
            Map::new(),
            async {
                self.inner
                    .shutdown()
                    .await
                    .map_err(|error| self.redact_mount_error(error))
            },
            |(): &(), _| Recorded::plain(),
            &[],
        )
        .await
    }

    async fn terminate_ambiguous_mount_transition(&self) -> SandboxResult<()> {
        self.inner.terminate_ambiguous_mount_transition().await
    }

    /// Closes as the protocol's default does — pre-stop callbacks, then this wrapper's recorded
    /// stop and shutdown, then dependencies — and then waits for every event delivery still in
    /// flight, whether or not the close succeeded.
    async fn close(&self) -> SandboxResult<()> {
        let outcome = async {
            let _closing = self.resources().lock_close().await;
            self.close_guarded().await
        }
        .await
        .map_err(|error| self.redact_mount_error(error));
        self.instrumentation.flush().await;
        outcome
    }
}
