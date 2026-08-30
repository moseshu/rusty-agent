//! Writes input to a running session and returns output produced afterward.
//!
//! This is deliberately the only interactive companion to `exec_command`. A background process
//! still owns the process manager that started it; this tool only supplies the session identifier,
//! writes verbatim input, waits briefly for a response, and reports the new bytes. Splitting every
//! command-line utility into an extra tool would spend schema budget without adding a capability.

use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    event::exec::{ExecSessionId, ExecStreamKind},
    tool::{
        DecodedToolInput, FuncSchema, Tool, ToolApprovalPolicy, ToolArgumentDecodeError,
        ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
    },
};
use ra_exec::{
    command::ExecCursor,
    session::{ExecError, ExecSessionState, ProcessManager},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;

const TOOL_NAME: &str = "write_stdin";
const DEFAULT_YIELD_TIME_MS: u64 = 1_000;

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Writes characters to a running command session and returns output produced afterward.
struct WriteStdinInput {
    /// Identifier returned when `exec_command` left a command running.
    session_id: String,
    /// Characters to send exactly as written. Send an empty string to wait for more output.
    chars: String,
    /// Milliseconds to wait for output after writing (default: 1000).
    yield_time_ms: Option<u64>,
}

/// The interactive input companion to `exec_command`.
pub struct WriteStdinTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    process_manager: Arc<ProcessManager>,
}

impl fmt::Debug for WriteStdinTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WriteStdinTool")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl WriteStdinTool {
    /// Creates a tool that addresses sessions owned by `process_manager`.
    pub fn new(process_manager: Arc<ProcessManager>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<WriteStdinInput>(TOOL_NAME)?,
            options: ToolOptions::new()
                .with_approval(ToolApprovalPolicy::Always)
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_concurrency(ToolConcurrency::Exclusive),
            process_manager,
        })
    }

    /// The manager shared with `exec_command`.
    #[must_use]
    pub fn process_manager(&self) -> &Arc<ProcessManager> {
        &self.process_manager
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

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema
            .decode_value_diagnostic(arguments.clone())
            .map(Some)
            .map_err(|error| WriteStdinFailure::BadArguments(error).into_error())
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = context
            .take_decoded_input::<WriteStdinInput>()?
            .map_or_else(
                || {
                    serde_json::from_value(context.arguments().clone()).map_err(|error| {
                        WriteStdinFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                            input_type: self.func_schema.input_type_name(),
                            message: error.to_string(),
                        })
                        .into_error()
                    })
                },
                Ok,
            )?;
        let session_id = ExecSessionId::new(input.session_id);
        let before = self
            .process_manager
            .get_output_summary(&session_id)
            .await
            .ok_or_else(|| WriteStdinFailure::UnknownSession(session_id.clone()).into_error())?;
        let stdout_cursor = before.stdout_bytes() as u64;
        let stderr_cursor = before.stderr_bytes() as u64;
        let emitter = context.event_emitter();
        if let Err(error) = self
            .process_manager
            .write_stdin(&session_id, Some(&input.chars), false, emitter.as_ref())
            .await
        {
            // An empty `chars` is a poll, and a poll has to survive the session's exit. A command
            // that yielded and then finished holds the output the model was waiting for, and the
            // manager still serves it; refusing here because the process is gone would drop that
            // output and make `exec_command`'s own guidance — collect the rest with this
            // identifier — false. Sending characters to a finished session is still a mistake, and
            // still reported as one.
            let failure = WriteStdinFailure::from_exec(&error);
            if !input.chars.is_empty() || !matches!(failure, WriteStdinFailure::SessionNotActive(_))
            {
                return Err(failure.into_error());
            }
        }
        let requested_yield =
            Duration::from_millis(input.yield_time_ms.unwrap_or(DEFAULT_YIELD_TIME_MS));
        let yield_timeout =
            requested_yield.min(self.process_manager.limits().initial_yield_timeout());
        self.process_manager
            .wait_for_output(&session_id, stdout_cursor, stderr_cursor, yield_timeout)
            .await
            .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?;

        let stdout = self
            .process_manager
            .read_output(
                &session_id,
                ExecCursor::new(ExecStreamKind::Stdout, stdout_cursor),
            )
            .await
            .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?
            .map(|(text, _)| text);
        let stderr = self
            .process_manager
            .read_output(
                &session_id,
                ExecCursor::new(ExecStreamKind::Stderr, stderr_cursor),
            )
            .await
            .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?
            .map(|(text, _)| text);
        let state = self
            .process_manager
            .get_session_state(&session_id)
            .await
            .ok_or_else(|| WriteStdinFailure::UnknownSession(session_id.clone()).into_error())?;

        Ok(render_output(
            &session_id,
            stdout.as_deref(),
            stderr.as_deref(),
            &state,
        ))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(WriteStdinFailure::of(error).map(|failure| ToolOutput::text(failure.to_string())))
    }
}

fn render_output(
    session_id: &ExecSessionId,
    stdout: Option<&str>,
    stderr: Option<&str>,
    state: &ExecSessionState,
) -> ToolOutput {
    let body = match (
        stdout.filter(|text| !text.is_empty()),
        stderr.filter(|text| !text.is_empty()),
    ) {
        (Some(stdout), Some(stderr)) => format!("{stdout}\n\n--- stderr ---\n{stderr}"),
        (Some(stdout), None) => stdout.to_owned(),
        (None, Some(stderr)) => stderr.to_owned(),
        (None, None) if state.is_active() => format!(
            "Session {session_id} is still running and has produced no output since the last interaction."
        ),
        (None, None) => match state {
            ExecSessionState::Exited {
                exit_code: Some(code),
            } => {
                format!(
                    "Session {session_id} exited with status {code} and produced no new output."
                )
            }
            ExecSessionState::Exited { exit_code: None } => {
                format!("Session {session_id} finished and produced no new output.")
            }
            other => format!("Session {session_id} ended as {other:?} and produced no new output."),
        },
    };
    ToolOutput::text(body)
}

#[derive(Debug)]
enum WriteStdinFailure {
    BadArguments(ToolArgumentDecodeError),
    UnknownSession(ExecSessionId),
    SessionNotActive(ExecSessionId),
    StdinClosed(ExecSessionId),
    StdinWrite(ExecSessionId),
    Execution(String),
}

impl WriteStdinFailure {
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_) | Self::UnknownSession(_) | Self::SessionNotActive(_) => {
                ToolErrorKind::InvalidInput
            }
            Self::StdinClosed(_) | Self::StdinWrite(_) | Self::Execution(_) => {
                ToolErrorKind::ExecutionFailed
            }
        }
    }

    fn into_error(self) -> Error {
        Error::tool(self.kind(), TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    fn from_exec(error: &ExecError) -> Self {
        match error {
            ExecError::UnknownSession { session_id } => Self::UnknownSession(session_id.clone()),
            ExecError::SessionNotActive { session_id, .. } => {
                Self::SessionNotActive(session_id.clone())
            }
            ExecError::StdinClosed { session_id } => Self::StdinClosed(session_id.clone()),
            ExecError::StdinWrite { session_id, .. } => Self::StdinWrite(session_id.clone()),
            other => Self::Execution(other.to_string()),
        }
    }
}

impl fmt::Display for WriteStdinFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadArguments(reason) => write!(formatter, "Invalid arguments: {reason}."),
            Self::UnknownSession(session_id) => {
                write!(formatter, "No running session is known as `{session_id}`.")
            }
            Self::SessionNotActive(session_id) => {
                write!(formatter, "Session `{session_id}` is no longer running.")
            }
            Self::StdinClosed(session_id) => {
                write!(
                    formatter,
                    "Session `{session_id}` no longer accepts standard input."
                )
            }
            Self::StdinWrite(session_id) => {
                write!(formatter, "Could not write to session `{session_id}`.")
            }
            Self::Execution(reason) => {
                write!(formatter, "The execution session is unavailable: {reason}.")
            }
        }
    }
}

impl std::error::Error for WriteStdinFailure {}
