//! `exec_command` and `write_stdin` against a sandbox session.
//!
//! A port of the reference's `capabilities/tools/shell_tool.py`: the argument models with their
//! defaults and descriptions, the working-directory and shell resolution, the two execution paths
//! — an interactive start where the session can offer one, a one-shot command where it cannot —
//! the fallback between them, and the plain-text response both tools return.
//!
//! # The response
//!
//! ```text
//! Chunk ID: 1a2b3c
//! Wall time: 0.2500 seconds
//! Process exited with code 7            (when it exited)
//! Process running with session ID 4242  (when it is still running)
//! Original token count: 900             (when the output was cut)
//! Output:
//! <output>
//! ```
//!
//! The chunk id is six hex digits of a random identifier, and the wall time is measured around the
//! whole call. Both change on every call, which is why they are the first two lines.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    sandbox::{
        ErrorCode, ExecRequest, PosixPath, PtyExecUpdate, PtyProcessId, PtyStartRequest,
        PtyWriteRequest, SandboxError, SandboxErrorDetails, SandboxSession, SandboxWorkspaceScope,
        ShellInvocation, User, shell::quote,
        token_truncation::formatted_truncate_text_with_token_count,
    },
    tool::{FuncSchema, Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{NeedsApproval, decode, sandbox_tool_options, session_failure};

/// The name `exec_command` is advertised under.
pub const EXEC_COMMAND_TOOL_NAME: &str = "exec_command";
/// The name `write_stdin` is advertised under.
pub const WRITE_STDIN_TOOL_NAME: &str = "write_stdin";

const DEFAULT_EXEC_YIELD_TIME_MS: u64 = 10_000;
const DEFAULT_WRITE_STDIN_YIELD_TIME_MS: u64 = 250;
const TOOL_OUTPUT_HEADER: &str = "Output:";
const FALLBACK_NOTICE: &str =
    "PTY transport failed before the interactive session opened; fell back to one-shot exec.";
const STDIN_UNAVAILABLE_OUTPUT: &str = "stdin is not available for this process. Start the command \
     with `tty=true` in `exec_command` before using `write_stdin`.";

const fn default_login() -> bool {
    true
}

const fn default_exec_yield_time_ms() -> u64 {
    DEFAULT_EXEC_YIELD_TIME_MS
}

const fn default_write_stdin_yield_time_ms() -> u64 {
    DEFAULT_WRITE_STDIN_YIELD_TIME_MS
}

/// The arguments `exec_command` takes.
///
/// Unknown fields are ignored, as the reference's model ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToolInput)]
#[tool_input(
    strict = false,
    description = "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
)]
pub struct ExecCommandArgs {
    /// Shell command to execute.
    #[schemars(length(min = 1))]
    cmd: String,
    /// Optional working directory to run the command in; defaults to the turn cwd.
    #[serde(default)]
    workdir: Option<String>,
    /// Shell binary to launch. Defaults to the user's default shell.
    #[serde(default)]
    shell: Option<String>,
    /// Whether to run the shell with -l/-i semantics. Defaults to true.
    #[serde(default = "default_login")]
    #[schemars(default = "default_login")]
    login: bool,
    /// Whether to allocate a TTY for the command. Defaults to false (plain pipes); set to true to
    /// open a PTY and access TTY process.
    #[serde(default)]
    tty: bool,
    /// How long to wait (in milliseconds) for output before yielding.
    #[serde(default = "default_exec_yield_time_ms")]
    #[schemars(default = "default_exec_yield_time_ms")]
    yield_time_ms: u64,
    /// Maximum number of tokens to return. Excess output will be truncated.
    #[serde(default)]
    #[schemars(range(min = 1))]
    max_output_tokens: Option<u64>,
}

impl ExecCommandArgs {
    /// Runs `cmd` with every other argument at its default.
    #[must_use]
    pub fn new(cmd: impl Into<String>) -> Self {
        Self {
            cmd: cmd.into(),
            workdir: None,
            shell: None,
            login: true,
            tty: false,
            yield_time_ms: DEFAULT_EXEC_YIELD_TIME_MS,
            max_output_tokens: None,
        }
    }

    /// Runs in `workdir`, measured from the turn's working directory when relative.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<String>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    /// Runs through `shell` instead of the default one.
    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    /// Whether the shell runs as a login shell.
    #[must_use]
    pub const fn with_login(mut self, login: bool) -> Self {
        self.login = login;
        self
    }

    /// Whether the command gets a terminal.
    #[must_use]
    pub const fn with_tty(mut self, tty: bool) -> Self {
        self.tty = tty;
        self
    }

    /// How long to wait for output before returning.
    #[must_use]
    pub const fn with_yield_time_ms(mut self, yield_time_ms: u64) -> Self {
        self.yield_time_ms = yield_time_ms;
        self
    }

    /// Caps the returned output.
    #[must_use]
    pub const fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// The command.
    #[must_use]
    pub fn cmd(&self) -> &str {
        &self.cmd
    }

    /// The working directory, when one was given.
    #[must_use]
    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }

    /// The shell binary, when one was given.
    #[must_use]
    pub fn shell(&self) -> Option<&str> {
        self.shell.as_deref()
    }

    /// Whether the shell runs as a login shell.
    #[must_use]
    pub const fn login(&self) -> bool {
        self.login
    }

    /// Whether the command gets a terminal.
    #[must_use]
    pub const fn tty(&self) -> bool {
        self.tty
    }

    /// How long to wait for output, in milliseconds.
    #[must_use]
    pub const fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms
    }

    /// The output cap, in tokens.
    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

    /// Enforces the two bounds the schema states and a decoder does not.
    fn validate(&self) -> Result<()> {
        if self.cmd.is_empty() {
            return Err(invalid_input(
                EXEC_COMMAND_TOOL_NAME,
                "`cmd` should have at least 1 character",
            ));
        }
        validate_max_output_tokens(EXEC_COMMAND_TOOL_NAME, self.max_output_tokens)
    }
}

/// The arguments `write_stdin` takes.
///
/// Unknown fields are ignored, as the reference's model ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToolInput)]
#[tool_input(
    strict = false,
    description = "Writes characters to an existing unified exec session and returns recent output."
)]
pub struct WriteStdinArgs {
    /// Identifier of the running unified exec session.
    session_id: i64,
    /// Bytes to write to stdin (may be empty to poll).
    #[serde(default)]
    chars: String,
    /// How long to wait (in milliseconds) for output before yielding.
    #[serde(default = "default_write_stdin_yield_time_ms")]
    #[schemars(default = "default_write_stdin_yield_time_ms")]
    yield_time_ms: u64,
    /// Maximum number of tokens to return. Excess output will be truncated.
    #[serde(default)]
    #[schemars(range(min = 1))]
    max_output_tokens: Option<u64>,
}

impl WriteStdinArgs {
    /// Polls `session_id` with every other argument at its default.
    #[must_use]
    pub const fn new(session_id: i64) -> Self {
        Self {
            session_id,
            chars: String::new(),
            yield_time_ms: DEFAULT_WRITE_STDIN_YIELD_TIME_MS,
            max_output_tokens: None,
        }
    }

    /// Sends `chars`.
    #[must_use]
    pub fn with_chars(mut self, chars: impl Into<String>) -> Self {
        self.chars = chars.into();
        self
    }

    /// How long to wait for output before returning.
    #[must_use]
    pub const fn with_yield_time_ms(mut self, yield_time_ms: u64) -> Self {
        self.yield_time_ms = yield_time_ms;
        self
    }

    /// Caps the returned output.
    #[must_use]
    pub const fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// The process addressed.
    #[must_use]
    pub const fn session_id(&self) -> i64 {
        self.session_id
    }

    /// The input.
    #[must_use]
    pub fn chars(&self) -> &str {
        &self.chars
    }

    /// How long to wait for output, in milliseconds.
    #[must_use]
    pub const fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms
    }

    /// The output cap, in tokens.
    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }
}

fn invalid_input(tool: &str, message: &str) -> Error {
    Error::tool(ToolErrorKind::InvalidInput, tool, message)
}

fn validate_max_output_tokens(tool: &str, max_output_tokens: Option<u64>) -> Result<()> {
    if max_output_tokens == Some(0) {
        return Err(invalid_input(
            tool,
            "`max_output_tokens` should be greater than or equal to 1",
        ));
    }
    Ok(())
}

/// Joins a one-shot command's two streams, as the reference's `_normalize_output` does.
///
/// Each stream is decoded with replacement. When both have text a newline separates them, unless
/// standard output already ended with one.
fn normalize_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => {
            let joiner = if stdout.ends_with('\n') { "" } else { "\n" };
            format!("{stdout}{joiner}{stderr}")
        }
        (false, true) => stdout.into_owned(),
        _ => stderr.into_owned(),
    }
}

/// What one call reports.
struct Response {
    output: String,
    exit_code: Option<i32>,
    process_id: Option<PtyProcessId>,
    original_token_count: Option<u64>,
}

impl Response {
    fn from_update(update: &PtyExecUpdate) -> Self {
        Self {
            output: String::from_utf8_lossy(&update.output).into_owned(),
            exit_code: update.exit_code,
            process_id: update.process_id,
            original_token_count: update.original_token_count,
        }
    }

    fn failed(output: String) -> Self {
        Self {
            output,
            exit_code: Some(1),
            process_id: None,
            original_token_count: None,
        }
    }

    /// Renders the response in the reference's layout.
    fn render(self, wall_time: Duration) -> String {
        let chunk_id = uuid::Uuid::new_v4().simple().to_string();
        let mut sections = vec![
            format!("Chunk ID: {}", &chunk_id[..6]),
            format!("Wall time: {:.4} seconds", wall_time.as_secs_f64()),
        ];
        if let Some(exit_code) = self.exit_code {
            sections.push(format!("Process exited with code {exit_code}"));
        }
        if let Some(process_id) = self.process_id {
            sections.push(format!("Process running with session ID {process_id}"));
        }
        if let Some(count) = self.original_token_count {
            sections.push(format!("Original token count: {count}"));
        }
        sections.push(TOOL_OUTPUT_HEADER.to_owned());
        sections.push(self.output);
        sections.join("\n")
    }
}

/// The shell the reference's `_resolve_shell` chooses.
///
/// No shell named and a login shell asked for is the session's own default, which is a login
/// shell in the protocol and `sh -c` on the local backend. No shell named and no login is `sh -c`.
/// A named shell gets `-lc` or `-c`. The host's own login shell is never consulted, whatever the
/// argument's description says.
#[must_use]
pub fn resolve_shell(shell: Option<&str>, login: bool) -> ShellInvocation {
    match shell {
        None if login => ShellInvocation::Login,
        None => ShellInvocation::Prefix(vec!["sh".to_owned(), "-c".to_owned()]),
        Some(shell) => ShellInvocation::Prefix(vec![
            shell.to_owned(),
            if login { "-lc" } else { "-c" }.to_owned(),
        ]),
    }
}

/// Prefixes a `cd` into the working directory, as the reference's `_resolve_workdir_command` does.
///
/// A blank or missing directory means the turn's working directory, and no `cd` at all when the
/// turn has none. A relative directory is measured from the turn's working directory; backslashes
/// are read as separators first. The result is resolved by the session, which is what refuses a
/// directory outside the workspace and its grants.
///
/// # Errors
///
/// Returns the session's refusal of the directory.
pub async fn resolve_workdir_command(
    session: &dyn SandboxSession,
    workspace_scope: &SandboxWorkspaceScope,
    command: &str,
    workdir: Option<&str>,
) -> std::result::Result<String, SandboxError> {
    let workdir = match workdir.filter(|workdir| !workdir.trim().is_empty()) {
        Some(workdir) => workdir,
        None if workspace_scope.cwd().is_none() => return Ok(command.to_owned()),
        None => ".",
    };
    let anchored = workspace_scope.anchor(PosixPath::coerce(workdir).as_str());
    let resolved = session
        .validate_path_access(PosixPath::coerce(&anchored).as_str(), false)
        .await?;
    Ok(format!("cd {} && {command}", quote(&resolved)))
}

/// Runs `exec_command` against a sandbox session.
#[derive(Clone)]
pub struct ExecCommandTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    session: Arc<dyn SandboxSession>,
    user: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
    needs_approval: NeedsApproval,
}

impl fmt::Debug for ExecCommandTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecCommandTool")
            .field("backend", &self.session.backend_id())
            .field("user", &self.user)
            .field("workspace_scope", &self.workspace_scope)
            .field("needs_approval", &self.needs_approval)
            .finish_non_exhaustive()
    }
}

impl ExecCommandTool {
    /// A tool running commands on `session` as its own user, from its workspace root, without
    /// approval.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the schema cannot be built, which is a defect here rather
    /// than a condition a caller can cause.
    pub fn new(session: Arc<dyn SandboxSession>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(EXEC_COMMAND_TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ExecCommandArgs>(EXEC_COMMAND_TOOL_NAME)?,
            session,
            user: None,
            workspace_scope: SandboxWorkspaceScope::root(),
            needs_approval: NeedsApproval::Never,
        })
    }

    /// Runs commands as `user`.
    #[must_use]
    pub fn with_user(mut self, user: Option<User>) -> Self {
        self.user = user;
        self
    }

    /// Measures relative working directories from `workspace_scope`.
    #[must_use]
    pub fn with_workspace_scope(mut self, workspace_scope: SandboxWorkspaceScope) -> Self {
        self.workspace_scope = workspace_scope;
        self
    }

    /// Makes calls wait for approval as `needs_approval` says.
    #[must_use]
    pub fn with_needs_approval(mut self, needs_approval: impl Into<NeedsApproval>) -> Self {
        self.needs_approval = needs_approval.into();
        self
    }

    /// Changes whether calls wait for approval, as a tool-set configurator does.
    pub fn set_needs_approval(&mut self, needs_approval: impl Into<NeedsApproval>) {
        self.needs_approval = needs_approval.into();
    }

    /// The session commands run on.
    #[must_use]
    pub fn session(&self) -> &Arc<dyn SandboxSession> {
        &self.session
    }

    /// The user commands run as, or `None` for the session's own.
    #[must_use]
    pub const fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }

    /// Where relative working directories are measured from.
    #[must_use]
    pub const fn workspace_scope(&self) -> &SandboxWorkspaceScope {
        &self.workspace_scope
    }

    /// Whether calls wait for approval.
    #[must_use]
    pub const fn needs_approval_policy(&self) -> &NeedsApproval {
        &self.needs_approval
    }

    /// Runs one call and renders its response.
    ///
    /// Where the session offers terminals, the command is started interactively and waited on for
    /// `yield_time_ms`, and a command still running comes back with a session id. Where it does not,
    /// the command runs to completion with `yield_time_ms` as its timeout. A failure to open the
    /// interactive transport that the backend marks as safe to retry falls back to the one-shot
    /// path, unless a terminal was asked for. A timeout on either path is a response, not an error.
    ///
    /// # Errors
    ///
    /// Returns invalid-input for arguments outside their bounds, and the session's failure for
    /// anything else it refused.
    pub async fn run(&self, args: &ExecCommandArgs) -> Result<String> {
        args.validate()?;
        let start = Instant::now();
        let timeout = Duration::from_millis(args.yield_time_ms);
        let wrapped = resolve_workdir_command(
            self.session.as_ref(),
            &self.workspace_scope,
            &args.cmd,
            args.workdir.as_deref(),
        )
        .await
        .map_err(|error| session_failure(EXEC_COMMAND_TOOL_NAME, error))?;
        let shell = resolve_shell(args.shell.as_deref(), args.login);

        let outcome = if self.session.supports_pty() {
            match self
                .start_interactive(args, &wrapped, &shell, timeout)
                .await
            {
                Err(error) if !args.tty && supports_transport_fallback(&error) => self
                    .one_shot(&wrapped, &shell, timeout, args.max_output_tokens)
                    .await
                    .map(|mut response| {
                        response.output = prepend_notice(&response.output, FALLBACK_NOTICE);
                        response
                    }),
                other => other,
            }
        } else {
            self.one_shot(&wrapped, &shell, timeout, args.max_output_tokens)
                .await
        };

        let response = match outcome {
            Ok(response) => response,
            Err(error) if error.error_code() == ErrorCode::ExecTimeout => Response {
                output: format!(
                    "Command timed out after {:.3} seconds.",
                    timeout.as_secs_f64()
                ),
                exit_code: None,
                process_id: None,
                original_token_count: None,
            },
            Err(error) => return Err(session_failure(EXEC_COMMAND_TOOL_NAME, error)),
        };
        Ok(response.render(start.elapsed()))
    }

    async fn start_interactive(
        &self,
        args: &ExecCommandArgs,
        wrapped: &str,
        shell: &ShellInvocation,
        yield_time: Duration,
    ) -> std::result::Result<Response, SandboxError> {
        let mut request = PtyStartRequest::new([wrapped.to_owned()])
            .with_shell(shell.clone())
            .with_tty(args.tty)
            .with_yield_time_s(yield_time.as_secs_f64());
        request.user.clone_from(&self.user);
        if let Some(max_output_tokens) = args.max_output_tokens {
            request = request.with_max_output_tokens(max_output_tokens);
        }
        self.session
            .pty_start(request)
            .await
            .map(|update| Response::from_update(&update))
    }

    async fn one_shot(
        &self,
        wrapped: &str,
        shell: &ShellInvocation,
        timeout: Duration,
        max_output_tokens: Option<u64>,
    ) -> std::result::Result<Response, SandboxError> {
        let mut request = ExecRequest::new([wrapped.to_owned()])
            .with_timeout_s(timeout.as_secs_f64())
            .with_shell(shell.clone());
        request.user.clone_from(&self.user);
        let result = self.session.exec(request).await?;
        let output = normalize_output(&result.stdout, &result.stderr);
        let (output, original_token_count) =
            formatted_truncate_text_with_token_count(&output, max_output_tokens);
        Ok(Response {
            output,
            exit_code: Some(result.exit_code),
            process_id: None,
            original_token_count,
        })
    }
}

/// Whether a transport failure is one the backend says can be retried as a one-shot command.
fn supports_transport_fallback(error: &SandboxError) -> bool {
    error.error_code() == ErrorCode::ExecTransportError
        && error.context().get("retry_safe") == Some(&serde_json::Value::Bool(true))
}

fn prepend_notice(output: &str, notice: &str) -> String {
    if output.is_empty() {
        notice.to_owned()
    } else {
        format!("{notice}\n{output}")
    }
}

#[async_trait]
impl Tool for ExecCommandTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn options(&self) -> ToolOptions {
        sandbox_tool_options(&self.needs_approval)
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.needs_approval.evaluate(context).await
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let args: ExecCommandArgs = decode(&mut context, &self.func_schema)?;
        self.run(&args).await.map(ToolOutput::text)
    }
}

/// Runs `write_stdin` against a sandbox session.
#[derive(Clone)]
pub struct WriteStdinTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    session: Arc<dyn SandboxSession>,
    needs_approval: NeedsApproval,
}

impl fmt::Debug for WriteStdinTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WriteStdinTool")
            .field("backend", &self.session.backend_id())
            .field("needs_approval", &self.needs_approval)
            .finish_non_exhaustive()
    }
}

impl WriteStdinTool {
    /// A tool writing to processes `session` started, without approval.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the schema cannot be built, which is a defect here rather
    /// than a condition a caller can cause.
    pub fn new(session: Arc<dyn SandboxSession>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(WRITE_STDIN_TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<WriteStdinArgs>(WRITE_STDIN_TOOL_NAME)?,
            session,
            needs_approval: NeedsApproval::Never,
        })
    }

    /// Makes calls wait for approval as `needs_approval` says.
    #[must_use]
    pub fn with_needs_approval(mut self, needs_approval: impl Into<NeedsApproval>) -> Self {
        self.needs_approval = needs_approval.into();
        self
    }

    /// Changes whether calls wait for approval, as a tool-set configurator does.
    pub fn set_needs_approval(&mut self, needs_approval: impl Into<NeedsApproval>) {
        self.needs_approval = needs_approval.into();
    }

    /// The session whose processes this writes to.
    #[must_use]
    pub fn session(&self) -> &Arc<dyn SandboxSession> {
        &self.session
    }

    /// Whether calls wait for approval.
    #[must_use]
    pub const fn needs_approval_policy(&self) -> &NeedsApproval {
        &self.needs_approval
    }

    /// Runs one call and renders its response.
    ///
    /// A process the session no longer knows, and input sent to a process without a terminal, are
    /// both responses with exit code 1 rather than errors, so the model reads what went wrong.
    ///
    /// # Errors
    ///
    /// Returns a failure when the session offers no terminals — the tool should not have been
    /// installed — invalid-input for arguments outside their bounds, and the session's failure for
    /// anything else.
    pub async fn run(&self, args: &WriteStdinArgs) -> Result<String> {
        if !self.session.supports_pty() {
            return Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                WRITE_STDIN_TOOL_NAME,
                "write_stdin is not available for non-PTY sandboxes",
            ));
        }
        validate_max_output_tokens(WRITE_STDIN_TOOL_NAME, args.max_output_tokens)?;

        let start = Instant::now();
        let mut request = PtyWriteRequest::new(PtyProcessId(args.session_id), args.chars.clone())
            .with_yield_time_s(Duration::from_millis(args.yield_time_ms).as_secs_f64());
        if let Some(max_output_tokens) = args.max_output_tokens {
            request = request.with_max_output_tokens(max_output_tokens);
        }
        let response = match self.session.pty_write(request).await {
            Ok(update) => Response::from_update(&update),
            Err(error) if error.error_code() == ErrorCode::PtySessionNotFound => {
                Response::failed(format!("write_stdin failed: {}", error.message()))
            }
            Err(error)
                if matches!(
                    error.details(),
                    Some(SandboxErrorDetails::PtyStdinUnavailable { .. })
                ) =>
            {
                Response::failed(STDIN_UNAVAILABLE_OUTPUT.to_owned())
            }
            Err(error) => return Err(session_failure(WRITE_STDIN_TOOL_NAME, error)),
        };
        Ok(response.render(start.elapsed()))
    }
}

#[async_trait]
impl Tool for WriteStdinTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn options(&self) -> ToolOptions {
        sandbox_tool_options(&self.needs_approval)
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.needs_approval.evaluate(context).await
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let args: WriteStdinArgs = decode(&mut context, &self.func_schema)?;
        self.run(&args).await.map(ToolOutput::text)
    }
}
