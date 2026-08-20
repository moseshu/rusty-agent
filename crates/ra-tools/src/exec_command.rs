//! Runs a command in a shell; returns output or a `session_id` for background execution.
//!
//! # Three layers, and what each one is allowed to know
//!
//! - `ra-tools` — this file — owns the model-facing schema, argument decoding, and the sentence the
//!   model reads back. It knows what a tool call is and nothing about process groups.
//! - [`ra_exec`] owns process lifetime, signalling, output capture, and sessions. It knows nothing
//!   about tools, which is why its failures arrive as [`ExecError`] and are given a tool's name
//!   here rather than there.
//! - The product layer supplies the workspace root and, once approval lands, the policy that
//!   decides whether a command runs at all.
//!
//! # Every argument in the schema does something
//!
//! A field the model must fill and nothing reads is not free: it costs tokens on every call, and it
//! tells the model it has control it does not have. `tty` is the sharpest case — a request for a
//! terminal that quietly produced a pipe would leave the model waiting for a prompt that can never
//! arrive, so it is refused with a sentence instead. The approval-shaped arguments Codex carries
//! (`prefix_rule`, `sandbox_permissions`, `justification`) are absent for the same reason: they
//! belong to the milestone that reads them, and they arrive with their consumer.
//!
//! Output ceilings are host configuration rather than an argument, on the same grounds `read_file`
//! states: the model cannot know the context budget it is spending against, so an argument asking
//! for more output only adds a way to ask for more than the host can afford.

use std::{
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    tool::{
        ObservationMetadata, ResourceClaim, Tool, ToolConcurrency, ToolContext,
        ToolFailureHandling, ToolInput as _, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
        Truncation, TruncationStage,
    },
};
use ra_exec::{
    command::{ExecLimits, ExecRequest},
    fs::RootedFileSystem,
    output::ExecOutputSummary,
    session::{ExecError, ExecExecutionResult, ProcessManager},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "exec_command";

// The doc comment below is the model-facing description. R2-11's measurement is that the tools
// doing the work carry short descriptions and the dangerous ones carry contracts; this is both, so
// the description says what the tool returns and the argument descriptions carry the rest.
#[derive(Debug, Clone, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Runs a command in a shell. Returns its output, or a `session_id` when the command is still
/// running after the yield timeout.
struct ExecCommandInput {
    /// Command to execute in the shell.
    cmd: String,
    /// Working directory for the command. A relative path resolves against the workspace root.
    workdir: Option<String>,
    /// Shell to run the command with. Defaults to `/bin/sh`.
    shell: Option<String>,
    /// Request a terminal. Not available; leave unset or false.
    tty: Option<bool>,
    /// Run the shell as a login shell, so that profile files are read.
    login: Option<bool>,
    /// Milliseconds to wait before the command yields to the background (default: 10000). The host
    /// may cap this; asking for longer than it allows yields at the cap.
    yield_time_ms: Option<u64>,
    /// Milliseconds after which the command is stopped for good. Unset means the host's limit.
    timeout_ms: Option<u64>,
}

/// Ceilings and timing defaults applied by the `exec_command` tool.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecCommandLimits {
    max_output_bytes: usize,
    default_yield_time_ms: u64,
}

impl Default for ExecCommandLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecCommandLimits {
    /// Creates the default limits.
    ///
    /// The ceiling is per stream, so a command that writes to both can return twice it. That is
    /// deliberate: a failing build whose diagnostics all land on stderr must not lose them to a
    /// budget that stdout has already spent.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_output_bytes: 100 * 1024,
            default_yield_time_ms: 10_000,
        }
    }

    /// Sets the ceiling on captured bytes per stream before truncation.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Sets the default yield timeout in milliseconds.
    #[must_use]
    pub const fn with_default_yield_time_ms(mut self, ms: u64) -> Self {
        self.default_yield_time_ms = ms;
        self
    }

    /// Ceiling on captured bytes per stream before truncation.
    #[must_use]
    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Default yield timeout in milliseconds.
    #[must_use]
    pub const fn default_yield_time_ms(&self) -> u64 {
        self.default_yield_time_ms
    }
}

/// The `exec_command` tool.
pub struct ExecCommandTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    process_manager: Arc<ProcessManager>,
    limits: ExecCommandLimits,
    root: Option<PathBuf>,
    rooted_filesystem: Option<Arc<RootedFileSystem>>,
}

impl fmt::Debug for ExecCommandTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecCommandTool")
            .field("origin", &self.origin)
            .field("root", &self.root)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ExecCommandTool {
    /// Creates an ambient execution tool that runs anywhere the process can.
    pub fn new() -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            schema: ExecCommandInput::tool_schema(TOOL_NAME)?,
            options: base_options(),
            process_manager: Arc::new(ProcessManager::default()),
            limits: ExecCommandLimits::new(),
            root: None,
            rooted_filesystem: None,
        })
    }

    /// Creates a tool whose working directories are confined to `root`, which must exist.
    ///
    /// The confinement is on the directory a command starts in, not on what it then does: a shell
    /// can name any path it likes once it is running. Keeping a command from reaching outside the
    /// workspace is the sandbox's job, and this root is what tells it where the workspace is.
    pub fn rooted(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            Error::config(format!(
                "exec_command workspace root `{}` cannot be resolved",
                root.display()
            ))
            .with_source(error)
        })?;
        let rooted_filesystem = RootedFileSystem::open(&canonical).map_err(|error| {
            Error::config(format!(
                "exec_command workspace root `{}` cannot be opened",
                root.display()
            ))
            .with_source(error)
        })?;
        let resource_id =
            ra_exec::fs::workspace_resource_id(canonical.to_string_lossy().to_string()).map_err(
                |error| {
                    Error::config(format!(
                        "exec_command workspace root `{}` produces invalid resource identity",
                        canonical.display()
                    ))
                    .with_source(error)
                },
            )?;
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            schema: ExecCommandInput::tool_schema(TOOL_NAME)?,
            options: base_options().with_resource_claim(ResourceClaim::exclusive(resource_id)),
            process_manager: Arc::new(ProcessManager::default()),
            limits: ExecCommandLimits::new(),
            root: Some(canonical),
            rooted_filesystem: Some(Arc::new(rooted_filesystem)),
        })
    }

    /// Associates an existing [`ProcessManager`] runtime with this tool.
    ///
    /// Sharing one is what lets the interactive stdin tool address a session this tool started.
    #[must_use]
    pub fn with_manager(mut self, manager: Arc<ProcessManager>) -> Self {
        self.process_manager = manager;
        self
    }

    /// Replaces the output and execution ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExecCommandLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The ceilings in force.
    #[must_use]
    pub const fn limits(&self) -> &ExecCommandLimits {
        &self.limits
    }

    /// The workspace root, when this tool is confined to one.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The process manager backing this tool.
    #[must_use]
    pub fn process_manager(&self) -> &Arc<ProcessManager> {
        &self.process_manager
    }

    /// Turns decoded arguments into the request `ra-exec` runs.
    fn request(&self, input: ExecCommandInput) -> ExecResult<ExecRequest> {
        if input.tty == Some(true) {
            return Err(ExecCommandFailure::PtyUnavailable);
        }
        let cwd = self.resolve_cwd(input.workdir.as_deref())?;

        let mut limits = ExecLimits::new()
            .with_initial_yield_timeout(Duration::from_millis(
                input
                    .yield_time_ms
                    .unwrap_or(self.limits.default_yield_time_ms),
            ))
            .with_max_capture_bytes(self.limits.max_output_bytes);
        if let Some(timeout_ms) = input.timeout_ms {
            limits = limits.with_total_timeout(Some(Duration::from_millis(timeout_ms)));
        }

        let mut request = ExecRequest::new(input.cmd)
            .with_shell(input.shell)
            .with_login(input.login.unwrap_or(false))
            .with_limits(limits);
        if let Some(cwd) = cwd {
            request = request.with_cwd(cwd);
        }
        Ok(request)
    }

    /// Resolves a requested working directory against the workspace root.
    ///
    /// Two refusals rather than one, for the reason `read_file` separates them: a path that left the
    /// workspace and a path spelled with `..` are different news, and only the first is a boundary
    /// violation. An absolute path inside the root is the same request written another way and is
    /// accepted.
    fn resolve_cwd(&self, requested: Option<&str>) -> ExecResult<Option<PathBuf>> {
        let Some(requested) = requested else {
            return Ok(self.root.clone());
        };

        let requested_path = Path::new(requested);
        let Some(root) = &self.root else {
            return Ok(Some(requested_path.to_path_buf()));
        };

        let relative = if requested_path.is_absolute() {
            match requested_path.strip_prefix(root) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => return Err(ExecCommandFailure::OutsideRoot(requested.to_owned())),
            }
        } else {
            requested_path.to_path_buf()
        };

        for component in relative.components() {
            match component {
                Component::ParentDir => {
                    return Err(ExecCommandFailure::AmbiguousParent(requested.to_owned()));
                }
                // Reachable on Windows, where `C:dir` is relative and still carries a prefix.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ExecCommandFailure::OutsideRoot(requested.to_owned()));
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }

        // Resolved through the root's directory capability rather than by inspecting the pathname,
        // so a symbolic link inside the workspace that points out of it is refused instead of
        // followed. What remains is that the shell resolves the path a second time when it starts:
        // a link swapped in between these two moments would not be caught here, and closing that
        // gap is the sandbox's job rather than a check's.
        if let Some(filesystem) = &self.rooted_filesystem
            && !filesystem.exists(&relative)
        {
            return Err(ExecCommandFailure::MissingDirectory(requested.to_owned()));
        }

        let resolved = root.join(&relative);
        if !resolved.is_dir() {
            return Err(ExecCommandFailure::MissingDirectory(requested.to_owned()));
        }
        Ok(Some(resolved))
    }
}

#[async_trait]
impl Tool for ExecCommandTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let input: ExecCommandInput = serde_json::from_value(context.arguments().clone())
            .map_err(|error| ExecCommandFailure::BadArguments(error.to_string()).into_error())?;
        let request = self.request(input).map_err(ExecCommandFailure::into_error)?;

        let emitter = context.event_emitter();
        let outcome = self
            .process_manager
            .execute(request, emitter.as_ref())
            .await
            .map_err(|error| ExecCommandFailure::from(&error).into_error())?;

        Ok(match outcome {
            ExecExecutionResult::Completed(summary) => completed_output(&summary),
            ExecExecutionResult::Yielded {
                session_id,
                summary,
            } => yielded_output(&session_id.to_string(), &summary),
            // `ExecExecutionResult` is `#[non_exhaustive]`, so the wildcard is mandatory. A result
            // this build cannot name is still a result, and answering the call is what keeps the
            // history well formed.
            other => ToolOutput::text(format!(
                "The command ran, but this build cannot describe how it ended ({:?} bytes of output captured).",
                other.summary().total_bytes()
            )),
        })
    }

    fn options(&self) -> ToolOptions {
        // `Exclusive` is the conservative default a command deserves: it can write anything, so
        // nothing else may run beside it until resource-level admission can say otherwise.
        //
        // `Custom` failure handling is what buys the model a sentence instead of a bare error code,
        // and every failure this tool produces carries one — a gap would turn a mistyped argument
        // into a stopped run.
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Matched on the typed cause carried in the error's source, never on its message. Reading
        // the sentence back out of prose is the de-lexicalization rule this project bans, and it
        // also produced the doubled text a message-sniffing version emitted.
        Ok(ExecCommandFailure::of(error).map(|failure| {
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(failure.next_step()))
        }))
    }
}

fn base_options() -> ToolOptions {
    ToolOptions::new()
        .with_failure_handling(ToolFailureHandling::Custom)
        .with_concurrency(ToolConcurrency::Exclusive)
}

/// Renders a command that finished within the yield window.
fn completed_output(summary: &ExecOutputSummary) -> ToolOutput {
    let mut body = render_streams(summary);
    if body.is_empty() {
        body = match summary.exit_code() {
            Some(code) => format!("Command exited with status {code} and produced no output."),
            None => "Command finished and produced no output.".to_owned(),
        };
    }

    let mut metadata = ObservationMetadata::new();
    if let Some(code) = summary.exit_code().filter(|code| *code != 0) {
        metadata = metadata.with_guidance(format!("Command exited with status {code}."));
    }
    metadata = with_truncation(metadata, summary);
    ToolOutput::text(body).with_metadata(metadata)
}

/// Renders a command that was still running when the yield timeout elapsed.
fn yielded_output(session_id: &str, summary: &ExecOutputSummary) -> ToolOutput {
    let captured = render_streams(summary);
    let body = if captured.is_empty() {
        format!("Command is still running and has produced no output yet (session {session_id}).")
    } else {
        format!("Command is still running (session {session_id}). Output so far:\n\n{captured}")
    };

    let metadata = with_truncation(
        ObservationMetadata::new().with_guidance(format!(
            "The command is running in the background as session `{session_id}`; send input to it or collect the rest of its output with that identifier."
        )),
        summary,
    );
    ToolOutput::text(body).with_metadata(metadata)
}

/// Joins the two streams, labelling stderr only when there is something on both.
fn render_streams(summary: &ExecOutputSummary) -> String {
    let stdout = summary.stdout().trim();
    let stderr = summary.stderr().trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("{stdout}\n\n--- stderr ---\n{stderr}"),
    }
}

/// Records what the capture ceiling cut, in source bytes on both sides of the pair.
///
/// The retained count comes from the buffers rather than from the rendered body, which also carries
/// the marker standing in for the omitted middle; counting that marker would report more output
/// surviving than the command produced.
fn with_truncation(
    metadata: ObservationMetadata,
    summary: &ExecOutputSummary,
) -> ObservationMetadata {
    if !summary.is_truncated() {
        return metadata;
    }
    metadata
        .with_truncation(Truncation::new(
            TruncationStage::Tool,
            as_u64(summary.total_bytes()),
            as_u64(summary.retained_bytes()),
        ))
        .with_guidance(
            "Output was cut to fit; the middle is missing. Narrow the command or filter its output to see the rest.",
        )
}

type ExecResult<T> = std::result::Result<T, ExecCommandFailure>;

/// Why a command could not run, or could not be asked for.
///
/// It travels in an [`Error`]'s source rather than in its message, so the model-facing sentence is
/// produced once, in [`Tool::handle_failure`], from a value rather than from prose.
#[derive(Debug)]
enum ExecCommandFailure {
    /// The argument object does not match the schema.
    BadArguments(String),
    /// The working directory is outside the workspace root.
    OutsideRoot(String),
    /// The working directory is spelled with `..`, which this tool refuses to resolve.
    AmbiguousParent(String),
    /// The working directory does not exist below the root.
    MissingDirectory(String),
    /// A terminal was requested, and this build has none to give.
    PtyUnavailable,
    /// The process could not be started.
    Spawn(String),
    /// Every execution slot is held by something that will not stop.
    Busy(String),
    /// An execution session could not be addressed.
    Session(String),
}

impl ExecCommandFailure {
    /// Which failure class the host records. Everything the model could have avoided by sending
    /// different arguments is `InvalidInput`; the rest is the tool failing to do its job.
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::OutsideRoot(_)
            | Self::AmbiguousParent(_)
            | Self::MissingDirectory(_)
            | Self::PtyUnavailable => ToolErrorKind::InvalidInput,
            Self::Spawn(_) | Self::Busy(_) | Self::Session(_) => ToolErrorKind::ExecutionFailed,
        }
    }

    fn into_error(self) -> Error {
        Error::tool(self.kind(), TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// The next step that follows the diagnosis, kept out of [`fmt::Display`] so a log line gets the
    /// fact without an instruction addressed to a model.
    const fn next_step(&self) -> &'static str {
        match self {
            Self::BadArguments(_) => "Send `cmd`, and leave the rest unset unless you need it.",
            Self::OutsideRoot(_) => "Run the command inside the workspace root.",
            Self::AmbiguousParent(_) => "Send `workdir` without `..`.",
            Self::MissingDirectory(_) => "Check the directory exists, or run from the root.",
            Self::PtyUnavailable => "Run the command without `tty`, in a form that needs no terminal.",
            Self::Spawn(_) => "Check the command and the shell, then try again.",
            // Not "stop one": by the time this failure exists every one of them has already been
            // told to stop and outlived the deadline for doing so, so a stop request against any
            // of them is a call that returns having done nothing. Waiting is the only move the
            // model has, and the stuck processes are something the user needs to hear about.
            Self::Busy(_) => "Wait for them to be reclaimed and try again; tell the user if it persists.",
            Self::Session(_) => "Start the command again rather than addressing that session.",
        }
    }
}

impl From<&ExecError> for ExecCommandFailure {
    fn from(error: &ExecError) -> Self {
        match error {
            ExecError::Spawn { command, source } => {
                Self::Spawn(format!("`{command}` could not be started: {source}"))
            }
            // Says that they were already asked, because that is what rules out the obvious reply:
            // there is no command here left to stop.
            ExecError::AtCapacity { max_sessions } => Self::Busy(format!(
                "all {max_sessions} slots are held by commands that did not stop when they were told to"
            )),
            // Reachable through a shared process manager rather than through `call`, and
            // `ExecError` is `#[non_exhaustive]`, so both are folded into one class: the model's
            // move is the same for all of them, which is to run the command again.
            other => Self::Session(other.to_string()),
        }
    }
}

impl fmt::Display for ExecCommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field — which is the whole
            // of what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(f, "Invalid arguments: {reason}."),
            Self::OutsideRoot(path) => write!(f, "`{path}` is outside the workspace."),
            Self::AmbiguousParent(path) => {
                write!(f, "`{path}` uses `..`, which this tool does not resolve.")
            }
            Self::MissingDirectory(path) => write!(f, "No such directory: `{path}`."),
            Self::PtyUnavailable => {
                write!(f, "This build runs commands on pipes and cannot allocate a terminal.")
            }
            Self::Spawn(reason) => write!(f, "The command did not start: {reason}."),
            Self::Busy(reason) => write!(f, "No room to run another command: {reason}."),
            Self::Session(reason) => write!(f, "The execution session is unavailable: {reason}."),
        }
    }
}

impl std::error::Error for ExecCommandFailure {}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
