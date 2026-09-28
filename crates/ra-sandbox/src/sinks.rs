//! The sinks the reference ships, ported from `session/sinks.py`.
//!
//! | Sink | Default mode | Default `on_error` | Where events go |
//! | --- | --- | --- | --- |
//! | [`CallbackSink`] | `Sync` | `Raise` | a host callback, with the session |
//! | [`JsonlOutboxSink`] | `BestEffort` | `Log` | a JSONL file on the host |
//! | [`WorkspaceJsonlSink`] | `BestEffort` | `Log` | a JSONL file inside the workspace |
//! | `HttpProxySink` (feature `http-sink`) | `BestEffort` | `Log` | a JSON `POST` per event |
//! | [`ChainedSink`] | `Sync` | `Raise` | its members, one after another |
//!
//! The defaults are the reference's, and so is every line format: the two JSONL sinks write
//! [`event_to_json_line`], and the HTTP sink posts the event's JSON with the raw output bytes left
//! out.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::future::BoxFuture;
use ra_core::sandbox::{
    DeliveryMode, ErrorCode, EventPayloadPolicy, EventSink, OnErrorPolicy, OpName, PosixPath,
    SandboxResult, SandboxSession, SandboxSessionEvent, SessionPath, SinkError, event_to_json_line,
    undecorated_session,
};

#[cfg(feature = "http-sink")]
mod http_proxy;
#[cfg(feature = "http-sink")]
pub use http_proxy::{DEFAULT_HTTP_PROXY_TIMEOUT_S, HttpProxySink};

// --- callback -------------------------------------------------------------------------------------

/// What a callback sink calls: the event, and the session it was bound to.
type SinkCallback = Arc<
    dyn Fn(
            SandboxSessionEvent,
            Arc<dyn SandboxSession>,
        ) -> BoxFuture<'static, Result<(), SinkError>>
        + Send
        + Sync,
>;

/// The failure a callback sink reports when an event arrives before it was bound.
pub const CALLBACK_SINK_UNBOUND_MESSAGE: &str = "CallbackSink requires a bound session; use SandboxSession / a sandbox client with instrumentation (or call bind(session)).";

/// Delivers events to a host callback.
///
/// The callback is handed the session too — the undecorated one, so whatever it does there is not
/// itself recorded. An event that arrives before the sink was bound fails, as it does in the
/// reference.
pub struct CallbackSink {
    callback: SinkCallback,
    mode: DeliveryMode,
    on_error: OnErrorPolicy,
    payload_policy: Option<EventPayloadPolicy>,
    name: Option<String>,
    session: Mutex<Option<Arc<dyn SandboxSession>>>,
}

impl std::fmt::Debug for CallbackSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallbackSink")
            .field("mode", &self.mode)
            .field("on_error", &self.on_error)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl CallbackSink {
    /// Calls `callback` for every event, before the operation continues, and fails the operation
    /// when the callback fails.
    #[must_use]
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(SandboxSessionEvent, Arc<dyn SandboxSession>) -> Result<(), SinkError>
            + Send
            + Sync
            + 'static,
    {
        Self::from_callback(Arc::new(move |event, session| {
            let outcome = callback(event, session);
            Box::pin(async move { outcome })
        }))
    }

    /// As [`Self::new`], for a callback that has to await something.
    #[must_use]
    pub fn new_async<F, Fut>(callback: F) -> Self
    where
        F: Fn(SandboxSessionEvent, Arc<dyn SandboxSession>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), SinkError>> + Send + 'static,
    {
        Self::from_callback(Arc::new(move |event, session| {
            Box::pin(callback(event, session))
        }))
    }

    fn from_callback(callback: SinkCallback) -> Self {
        Self {
            callback,
            mode: DeliveryMode::Sync,
            on_error: OnErrorPolicy::Raise,
            payload_policy: None,
            name: None,
            session: Mutex::new(None),
        }
    }

    /// Delivers in `mode` instead.
    #[must_use]
    pub const fn with_mode(mut self, mode: DeliveryMode) -> Self {
        self.mode = mode;
        self
    }

    /// Handles a failure by `policy` instead.
    #[must_use]
    pub const fn with_on_error(mut self, policy: OnErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }

    /// Sees events through `policy`, over the instrumentation's.
    #[must_use]
    pub const fn with_payload_policy(mut self, policy: EventPayloadPolicy) -> Self {
        self.payload_policy = Some(policy);
        self
    }

    /// Names the sink.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

#[async_trait]
impl EventSink for CallbackSink {
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn type_name(&self) -> &'static str {
        "CallbackSink"
    }

    fn mode(&self) -> DeliveryMode {
        self.mode
    }

    fn on_error(&self) -> OnErrorPolicy {
        self.on_error
    }

    fn payload_policy(&self) -> Option<&EventPayloadPolicy> {
        self.payload_policy.as_ref()
    }

    fn bind(&self, session: Arc<dyn SandboxSession>) -> SandboxResult<()> {
        *lock(&self.session) = Some(undecorated_session(session));
        Ok(())
    }

    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError> {
        let session = lock(&self.session)
            .clone()
            .ok_or_else(|| SinkError::from(CALLBACK_SINK_UNBOUND_MESSAGE))?;
        (self.callback)(event, session).await
    }
}

// --- host JSONL file ------------------------------------------------------------------------------

/// Appends each event as one line of a JSONL file on the host.
///
/// Each line is appended under an exclusive advisory lock where the platform has one, so several
/// processes can share an outbox; the parent directory is created when it is missing.
#[derive(Debug)]
pub struct JsonlOutboxSink {
    path: PathBuf,
    mode: DeliveryMode,
    on_error: OnErrorPolicy,
    payload_policy: Option<EventPayloadPolicy>,
}

impl JsonlOutboxSink {
    /// Appends to `path`, in the background, logging failures.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: DeliveryMode::BestEffort,
            on_error: OnErrorPolicy::Log,
            payload_policy: None,
        }
    }

    /// Delivers in `mode` instead.
    #[must_use]
    pub const fn with_mode(mut self, mode: DeliveryMode) -> Self {
        self.mode = mode;
        self
    }

    /// Handles a failure by `policy` instead.
    #[must_use]
    pub const fn with_on_error(mut self, policy: OnErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }

    /// Sees events through `policy`, over the instrumentation's.
    #[must_use]
    pub const fn with_payload_policy(mut self, policy: EventPayloadPolicy) -> Self {
        self.payload_policy = Some(policy);
        self
    }

    /// The file appended to.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Appends `line` to `path`, creating the parent directory and holding an exclusive lock where the
/// platform offers one. A lock that cannot be taken is not a failure, as it is not in the reference.
pub(crate) fn append_locked(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    #[cfg(unix)]
    let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive);
    file.write_all(line.as_bytes())?;
    file.flush()?;
    #[cfg(unix)]
    let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock);
    Ok(())
}

#[async_trait]
impl EventSink for JsonlOutboxSink {
    fn type_name(&self) -> &'static str {
        "JsonlOutboxSink"
    }

    fn mode(&self) -> DeliveryMode {
        self.mode
    }

    fn on_error(&self) -> OnErrorPolicy {
        self.on_error
    }

    fn payload_policy(&self) -> Option<&EventPayloadPolicy> {
        self.payload_policy.as_ref()
    }

    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError> {
        let line = event_to_json_line(&event);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || append_locked(&path, &line)).await??;
        Ok(())
    }
}

// --- workspace JSONL file -------------------------------------------------------------------------

/// The path a workspace sink writes to unless told otherwise.
pub const DEFAULT_WORKSPACE_EVENTS_PATH: &str = "logs/events-{session_id}.jsonl";

/// Appends events to a JSONL file inside the session's workspace.
///
/// It runs in the host process but writes through the session, so it works wherever the workspace
/// is, with no host volume. Lines are buffered and written every `flush_every` events, and also
/// when the workspace is about to be persisted or stopped, so a snapshot holds what was recorded
/// before it. A write only happens while the session reports itself running; before start finishes
/// and after shutdown the lines stay buffered.
///
/// The path is relative to the workspace root and may name `{session_id}` (the UUID with hyphens)
/// or `{session_id_hex}` (without); a template that cannot be expanded is used as it is written. An
/// ephemeral sink's file is excluded from every snapshot of the session it is bound to.
pub struct WorkspaceJsonlSink {
    workspace_relpath: String,
    ephemeral: bool,
    mode: DeliveryMode,
    on_error: OnErrorPolicy,
    payload_policy: Option<EventPayloadPolicy>,
    flush_every: u64,
    state: tokio::sync::Mutex<WorkspaceOutbox>,
    bound: Mutex<Option<(Arc<dyn SandboxSession>, String)>>,
}

impl std::fmt::Debug for WorkspaceJsonlSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceJsonlSink")
            .field("workspace_relpath", &self.workspace_relpath)
            .field("ephemeral", &self.ephemeral)
            .field("flush_every", &self.flush_every)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct WorkspaceOutbox {
    buffer: Vec<u8>,
    seen: u64,
}

impl WorkspaceJsonlSink {
    /// Writes to [`DEFAULT_WORKSPACE_EVENTS_PATH`], persisted with the workspace, after every event,
    /// in the background, logging failures.
    #[must_use]
    pub fn new() -> Self {
        Self {
            workspace_relpath: DEFAULT_WORKSPACE_EVENTS_PATH.to_owned(),
            ephemeral: false,
            mode: DeliveryMode::BestEffort,
            on_error: OnErrorPolicy::Log,
            payload_policy: None,
            flush_every: 1,
            state: tokio::sync::Mutex::new(WorkspaceOutbox::default()),
            bound: Mutex::new(None),
        }
    }

    /// Writes to `relpath` instead; see the type's documentation for the templates it may use.
    #[must_use]
    pub fn with_workspace_relpath(mut self, relpath: impl Into<String>) -> Self {
        self.workspace_relpath = relpath.into();
        self
    }

    /// Keeps the file out of snapshots.
    #[must_use]
    pub const fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    /// Writes after every `flush_every` events; zero is taken as one.
    #[must_use]
    pub fn with_flush_every(mut self, flush_every: u64) -> Self {
        self.flush_every = flush_every.max(1);
        self
    }

    /// Delivers in `mode` instead.
    #[must_use]
    pub const fn with_mode(mut self, mode: DeliveryMode) -> Self {
        self.mode = mode;
        self
    }

    /// Handles a failure by `policy` instead.
    #[must_use]
    pub const fn with_on_error(mut self, policy: OnErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }

    /// Sees events through `policy`, over the instrumentation's.
    #[must_use]
    pub const fn with_payload_policy(mut self, policy: EventPayloadPolicy) -> Self {
        self.payload_policy = Some(policy);
        self
    }

    /// The path as configured, before expansion.
    #[must_use]
    pub fn workspace_relpath(&self) -> &str {
        &self.workspace_relpath
    }

    /// The path the sink writes to, once bound.
    #[must_use]
    pub fn resolved_workspace_relpath(&self) -> Option<String> {
        lock(&self.bound).as_ref().map(|(_, path)| path.clone())
    }

    /// Whether lines are waiting to be written.
    pub async fn has_buffered_lines(&self) -> bool {
        !self.state.lock().await.buffer.is_empty()
    }

    /// Buffers `event`'s line and says whether it is time to write.
    fn buffer_event(&self, outbox: &mut WorkspaceOutbox, event: &SandboxSessionEvent) -> bool {
        outbox
            .buffer
            .extend_from_slice(event_to_json_line(event).as_bytes());
        outbox.seen += 1;

        let start = event.as_finish().is_none();
        if outbox.seen.is_multiple_of(self.flush_every) {
            return true;
        }
        match event.op() {
            // Before the workspace is persisted, so the snapshot holds the lines; and before
            // shutdown tears the workspace down, since after it there is nothing to write to.
            OpName::PersistWorkspace | OpName::Shutdown => start,
            OpName::Stop => true,
            _ => false,
        }
    }
}

impl Default for WorkspaceJsonlSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Expands `{session_id}` and `{session_id_hex}` in `template`, as `str.format` would with those
/// two names. Any other field, a format specification, or an unmatched brace makes the expansion
/// fail, and the template is then used as written.
fn expand_workspace_relpath(template: &str, session_id: uuid::Uuid) -> String {
    let fields = BTreeMap::from([
        ("session_id", session_id.hyphenated().to_string()),
        ("session_id_hex", session_id.simple().to_string()),
    ]);
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    name.push(next);
                }
                match fields.get(name.as_str()) {
                    Some(value) if closed => out.push_str(value),
                    _ => return template.to_owned(),
                }
            }
            '}' => return template.to_owned(),
            other => out.push(other),
        }
    }
    out
}

#[async_trait]
impl EventSink for WorkspaceJsonlSink {
    fn type_name(&self) -> &'static str {
        "WorkspaceJsonlSink"
    }

    fn mode(&self) -> DeliveryMode {
        self.mode
    }

    fn on_error(&self) -> OnErrorPolicy {
        self.on_error
    }

    fn payload_policy(&self) -> Option<&EventPayloadPolicy> {
        self.payload_policy.as_ref()
    }

    fn bind(&self, session: Arc<dyn SandboxSession>) -> SandboxResult<()> {
        let session = undecorated_session(session);
        let relpath =
            expand_workspace_relpath(&self.workspace_relpath, session.state().session_id());
        // A path, as the reference renders its template into one: a backslash stays in the name.
        if self.ephemeral {
            session.register_persist_workspace_skip_path(SessionPath::Posix(&PosixPath::new(
                relpath.as_str(),
            )))?;
        }
        *lock(&self.bound) = Some((session, relpath));
        Ok(())
    }

    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError> {
        // Unbound — events emitted without a session wrapper — is a no-op, as in the reference.
        let Some((session, relpath)) = lock(&self.bound).clone() else {
            return Ok(());
        };

        let mut outbox = self.state.lock().await;
        if !self.buffer_event(&mut outbox, &event) {
            return Ok(());
        }
        // Writes can still fail early in start and late in teardown, when the session does not
        // report itself running; the lines wait for the next write.
        if !matches!(session.running().await, Ok(true)) {
            return Ok(());
        }
        if outbox.buffer.is_empty() {
            return Ok(());
        }

        let path = PosixPath::new(relpath.as_str());
        let mut contents = match session.read(SessionPath::Posix(&path), None).await {
            Ok(existing) => existing,
            Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => Vec::new(),
            Err(error) => return Err(Box::new(error)),
        };
        contents.extend_from_slice(&outbox.buffer);
        session
            .write(SessionPath::Posix(&path), contents, None)
            .await?;
        outbox.buffer.clear();
        Ok(())
    }
}

// --- ordered group --------------------------------------------------------------------------------

/// Sinks that run one after another.
///
/// The instrumentation delivers to each member with that member's own policy and waits for it
/// before the next, whatever the member's mode, so a later sink never sees the event before an
/// earlier one has finished with it. Used directly as a sink, it hands each member the same event
/// in order.
pub struct ChainedSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl std::fmt::Debug for ChainedSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChainedSink")
            .field("len", &self.sinks.len())
            .finish()
    }
}

impl ChainedSink {
    /// Groups `sinks`, in order.
    #[must_use]
    pub fn new(sinks: impl IntoIterator<Item = Arc<dyn EventSink>>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }

    /// The members, in order.
    #[must_use]
    pub fn sinks(&self) -> &[Arc<dyn EventSink>] {
        &self.sinks
    }
}

#[async_trait]
impl EventSink for ChainedSink {
    fn type_name(&self) -> &'static str {
        "ChainedSink"
    }

    fn mode(&self) -> DeliveryMode {
        DeliveryMode::Sync
    }

    fn on_error(&self) -> OnErrorPolicy {
        OnErrorPolicy::Raise
    }

    fn grouped_sinks(&self) -> Option<&[Arc<dyn EventSink>]> {
        Some(&self.sinks)
    }

    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError> {
        for sink in &self.sinks {
            sink.handle(event.clone()).await?;
        }
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
