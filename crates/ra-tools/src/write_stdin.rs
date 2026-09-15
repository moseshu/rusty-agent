//! Writes input to a running session, waits on it, and returns output not yet delivered.
//!
//! This is deliberately the only interactive companion to `exec_command`. A background process
//! still owns the process manager that started it; this tool only supplies the session identifier,
//! writes verbatim input, waits for one of three conditions, and reports the bytes the model has
//! not been given yet. Splitting every way of waiting into its own tool would spend schema budget
//! without adding a capability — the reference implementation waits by calling its execution tool
//! again with a time limit, and so does this one.
//!
//! **Returned output is what has not been delivered, not what arrived during this call.** A command
//! that printed while nobody was asking has that output waiting here; a session carries the
//! delivery positions across calls, so nothing is repeated and nothing is skipped.
//!
//! # Waiting for a condition, within a limit
//!
//! `until` names the condition and `yield_time_ms` bounds it. They are separate on purpose: a
//! timeout is the ceiling on every wait rather than a third thing to wait for, and a call that
//! could hang forever waiting for a command to finish is a call a model cannot safely make.
//! Reaching the limit ends the wait and nothing else — the command keeps running.
//!
//! # Stopping a command is an argument, not a character
//!
//! Sessions currently use standard input pipes, and pipes accept ordinary byte writes. `"\u{3}"`
//! sent through `chars` is therefore one byte of input, and this tool writes it as such;
//! interrupting the process is `control: interrupt`, which sends a signal to its process group.
//! Quietly translating one into the other would break the verbatim-write contract for every program
//! that reads raw keystrokes. Terminal allocation still has no end-to-end contract and is not
//! declared here.

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
    job::{BackgroundJob, JobError, JobWaitResult, JobWaitUntil},
    session::{ExecError, ProcessManager},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::session_return::{ReturnReason, SessionReturn, Watched};

const TOOL_NAME: &str = "write_stdin";
const DEFAULT_YIELD_TIME_MS: u64 = 1_000;

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Writes characters to a running command session, then waits and returns output not yet delivered.
struct WriteStdinInput {
    /// Identifier returned when `exec_command` left a command running.
    session_id: String,
    /// Characters to send exactly as written. Send an empty string to wait without sending input.
    chars: String,
    /// What ends the wait (default: `output`). Every mode also ends when `yield_time_ms` elapses,
    /// which leaves the command running.
    until: Option<WaitCondition>,
    /// Literal text to wait for in this session's output. Required by `until: match`, and rejected
    /// otherwise. The search covers output this session has already produced, including output from
    /// before this call.
    match_text: Option<String>,
    /// Signal to send instead of input. Requires `chars` to be empty.
    control: Option<SessionControl>,
    /// Milliseconds to wait (default: 1000). The host may cap this.
    yield_time_ms: Option<u64>,
}

/// What ends a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WaitCondition {
    /// Return as soon as there is output the model has not been given.
    Output,
    /// Return when the command has finished and all of its output has been collected.
    Done,
    /// Return when `match_text` appears in the output.
    Match,
}

/// A signal to deliver to a session's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SessionControl {
    /// Interrupt the command, as Ctrl-C does, letting it stop on its own terms.
    Interrupt,
    /// Stop the command for good.
    Cancel,
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
                .with_concurrency(ToolConcurrency::Parallel),
            process_manager,
        })
    }

    /// The manager shared with `exec_command`.
    #[must_use]
    pub fn process_manager(&self) -> &Arc<ProcessManager> {
        &self.process_manager
    }

    /// Decodes arguments, whether or not the runtime decoded them first.
    fn input(&self, context: &mut ToolContext<'_>) -> Result<WriteStdinInput> {
        context
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
            )
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

    // Session serialization belongs to the manager's interaction guard inside the deadline.
    // A resource claim here would queue before that deadline starts. Control signals are sent
    // before taking the guard, while all output delivery still takes the same guard.
    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = self.input(&mut context)?;
        let plan = WaitPlan::of(&input).map_err(WriteStdinFailure::into_error)?;
        let session_id = ExecSessionId::new(input.session_id.clone());
        let job = BackgroundJob::new(Arc::clone(&self.process_manager), session_id.clone());
        // Held for the whole call rather than only for the wait. The steps that matter most come
        // after the wait returns — reading the output it waited for, then recording how much of it
        // was delivered — and a session reclaimed in between would turn an answer this call had
        // already earned into "no such session".
        let _watch = job.watch();

        // One budget for the call, not one per phase. Taking the delivery lock, writing input, and
        // waiting are three things that can each block, and giving each of them the full
        // `yield_time_ms` would let a call that asked to wait one second take three — while the
        // argument's own description promises the one. Every phase below spends what is left of it.
        let waited = Duration::from_millis(plan.yield_time_ms)
            .min(self.process_manager.limits().initial_yield_timeout());
        let deadline = tokio::time::Instant::now() + waited;
        let emitter = context.event_emitter();
        match input.control {
            Some(SessionControl::Cancel) => {
                job.cancel()
                    .await
                    .map_err(|error| WriteStdinFailure::from_job(&error).into_error())?;
            }
            Some(SessionControl::Interrupt) => {
                self.process_manager
                    .write_stdin(&session_id, None, true, emitter.as_ref())
                    .await
                    .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?;
            }
            None => {}
        }

        let _interaction = if let Ok(result) =
            tokio::time::timeout_at(deadline, self.process_manager.lock_interaction(&session_id))
                .await
        {
            result.map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?
        } else {
            // Said in the same vocabulary as every other result: this one returned without looking,
            // which is a different fact from a wait that looked and saw nothing, and the snapshot
            // still has to answer where the session stands.
            let snapshot = job
                .snapshot()
                .await
                .map_err(|error| WriteStdinFailure::from_job(&error).into_error())?;
            return Ok(SessionReturn {
                session_id: &session_id,
                watched: plan.watched(),
                waited,
                reason: ReturnReason::Contended {
                    control_sent: input.control.is_some(),
                },
                state: snapshot.state().clone(),
                closed: snapshot.is_closed(),
                exit_code: snapshot.output().exit_code(),
                stdout: String::new(),
                stderr: String::new(),
                matched: None,
                delivered_bytes: 0,
                omitted_bytes: 0,
            }
            .render());
        };
        let (stdout_cursor, stderr_cursor) = self
            .process_manager
            .interactive_output_cursors(&session_id)
            .await
            .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?;
        if input.control.is_none() {
            self.send_input(&session_id, &input.chars, deadline, emitter.as_ref())
                .await?;
        }

        let outcome = job
            .wait(plan.until(stdout_cursor, stderr_cursor, remaining(deadline)))
            .await
            .map_err(|error| WriteStdinFailure::from_job(&error).into_error())?;

        let stdout = self
            .deliver(&session_id, ExecStreamKind::Stdout, stdout_cursor)
            .await?;
        let stderr = self
            .deliver(&session_id, ExecStreamKind::Stderr, stderr_cursor)
            .await?;
        self.process_manager
            .mark_interactive_output_delivered(&session_id, stdout.next, stderr.next)
            .await
            .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?;

        let snapshot = outcome.snapshot();
        Ok(SessionReturn {
            session_id: &session_id,
            watched: plan.watched(),
            waited,
            reason: reason_of(&outcome),
            state: snapshot.state().clone(),
            closed: snapshot.is_closed(),
            exit_code: snapshot.output().exit_code(),
            stdout: stdout.text,
            stderr: stderr.text,
            matched: matched_of(&outcome),
            delivered_bytes: stdout.delivered.saturating_add(stderr.delivered),
            omitted_bytes: stdout.omitted.saturating_add(stderr.omitted),
        }
        .render())
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

impl WriteStdinTool {
    /// Bounds writes without pretending a timed-out write was atomic.
    async fn send_input(
        &self,
        session_id: &ExecSessionId,
        chars: &str,
        deadline: tokio::time::Instant,
        emitter: Option<&ra_core::event::HostEventEmitter>,
    ) -> Result<()> {
        let result = tokio::time::timeout_at(
            deadline,
            self.process_manager
                .write_stdin(session_id, Some(chars), false, emitter),
        )
        .await
        .map_err(|_| WriteStdinFailure::InputTimedOut.into_error())?;
        if let Err(error) = result {
            // Empty input is a poll: it must still collect output after the process exits.
            let failure = WriteStdinFailure::from_exec(&error);
            if !chars.is_empty() || !matches!(failure, WriteStdinFailure::SessionNotActive(_)) {
                return Err(failure.into_error());
            }
        }
        Ok(())
    }

    /// Reads one stream from the position this call is delivering from.
    ///
    /// The delivery position advances to what this read covers and no further, so bytes that arrive
    /// between the read and the next call stay undelivered rather than being marked as sent.
    async fn deliver(
        &self,
        session_id: &ExecSessionId,
        stream: ExecStreamKind,
        from: u64,
    ) -> Result<Delivery> {
        let read = self
            .process_manager
            .read_output(session_id, ExecCursor::new(stream, from))
            .await
            .map_err(|error| WriteStdinFailure::from_exec(&error).into_error())?;
        Ok(read.map_or_else(
            || Delivery {
                text: String::new(),
                next: from,
                delivered: 0,
                omitted: 0,
            },
            |read| Delivery {
                next: read.next().offset(),
                delivered: read.next().offset().saturating_sub(from),
                omitted: as_u64(read.omitted_bytes()),
                text: read.into_text(),
            },
        ))
    }
}

/// One stream's contribution to a result.
struct Delivery {
    text: String,
    next: u64,
    delivered: u64,
    omitted: u64,
}

/// The validated wait this call performs.
struct WaitPlan {
    condition: WaitCondition,
    text: Option<String>,
    yield_time_ms: u64,
}

impl WaitPlan {
    /// Checks that the arguments describe one wait, and that the wait is one this tool can perform.
    fn of(input: &WriteStdinInput) -> WaitResult<Self> {
        let condition = input.until.unwrap_or(WaitCondition::Output);
        let text = input.match_text.clone().filter(|text| !text.is_empty());
        match (condition, &text) {
            (WaitCondition::Match, None) => return Err(WriteStdinFailure::MatchTextMissing),
            (WaitCondition::Output | WaitCondition::Done, Some(_)) => {
                return Err(WriteStdinFailure::MatchTextUnused);
            }
            _ => {}
        }
        if input.control.is_some() && !input.chars.is_empty() {
            return Err(WriteStdinFailure::ControlWithInput);
        }
        Ok(Self {
            condition,
            text,
            yield_time_ms: input.yield_time_ms.unwrap_or(DEFAULT_YIELD_TIME_MS),
        })
    }

    /// The condition, as the job facade names it.
    fn until(&self, stdout_cursor: u64, stderr_cursor: u64, timeout: Duration) -> JobWaitUntil {
        match (self.condition, &self.text) {
            (WaitCondition::Match, Some(text)) => JobWaitUntil::Match {
                text: text.clone(),
                timeout,
            },
            // A bounded wait for closeout is exactly what the job facade's `Timeout` already is:
            // it returns early when the command finishes and its output is collected.
            (WaitCondition::Done, _) | (WaitCondition::Match, None) => {
                JobWaitUntil::Timeout(timeout)
            }
            (WaitCondition::Output, _) => JobWaitUntil::Output {
                delivered_stdout: stdout_cursor,
                delivered_stderr: stderr_cursor,
                timeout,
            },
        }
    }

    /// The condition, as the result describes it.
    fn watched(&self) -> Watched<'_> {
        match (self.condition, &self.text) {
            (WaitCondition::Match, Some(text)) => Watched::Text(text),
            (WaitCondition::Done, _) | (WaitCondition::Match, None) => Watched::Closeout,
            (WaitCondition::Output, _) => Watched::NewOutput,
        }
    }
}

/// Why the wait returned.
///
/// The wildcard is not a default: a result this build cannot name is classified from the snapshot it
/// carries, so an unnamed variant still reports whether the output is complete.
fn reason_of(outcome: &JobWaitResult) -> ReturnReason {
    match outcome {
        JobWaitResult::Done(_) => ReturnReason::Done,
        JobWaitResult::Matched { .. } => ReturnReason::Matched,
        JobWaitResult::OutputReady(_) => ReturnReason::Output,
        JobWaitResult::TimedOut(_) => ReturnReason::Timeout,
        other => {
            if other.snapshot().is_closed() {
                ReturnReason::Done
            } else {
                ReturnReason::Timeout
            }
        }
    }
}

fn matched_of(outcome: &JobWaitResult) -> Option<(ExecStreamKind, String)> {
    match outcome {
        JobWaitResult::Matched {
            stream, excerpt, ..
        } => Some((*stream, excerpt.clone())),
        _ => None,
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// What is left of the call's budget.
///
/// Zero once it is spent, which is a wait that looks once and returns rather than one that refuses:
/// the caller asked for a bounded look, and a phase reached with nothing left still owes it an
/// answer about what is there now.
fn remaining(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

type WaitResult<T> = std::result::Result<T, WriteStdinFailure>;

#[derive(Debug)]
enum WriteStdinFailure {
    BadArguments(ToolArgumentDecodeError),
    MatchTextMissing,
    MatchTextUnused,
    ControlWithInput,
    InputTimedOut,
    UnknownSession(ExecSessionId),
    SessionNotActive(ExecSessionId),
    StdinClosed(ExecSessionId),
    StdinWrite(ExecSessionId),
    Execution(String),
}

impl WriteStdinFailure {
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::MatchTextMissing
            | Self::MatchTextUnused
            | Self::ControlWithInput
            | Self::UnknownSession(_)
            | Self::SessionNotActive(_) => ToolErrorKind::InvalidInput,
            Self::InputTimedOut
            | Self::StdinClosed(_)
            | Self::StdinWrite(_)
            | Self::Execution(_) => ToolErrorKind::ExecutionFailed,
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

    fn from_job(error: &JobError) -> Self {
        match error {
            JobError::UnknownJob { session_id } => Self::UnknownSession(session_id.clone()),
            other => Self::Execution(other.to_string()),
        }
    }
}

impl fmt::Display for WriteStdinFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadArguments(reason) => write!(formatter, "Invalid arguments: {reason}."),
            Self::MatchTextMissing => write!(
                formatter,
                "`until: match` needs `match_text` to say what to wait for."
            ),
            Self::MatchTextUnused => write!(
                formatter,
                "`match_text` only applies to `until: match`; send `until: match` or drop it."
            ),
            Self::InputTimedOut => write!(
                formatter,
                "Standard input timed out. Some bytes may already have been written; do not blindly resend the input. Poll the session or send control: cancel."
            ),
            Self::ControlWithInput => write!(
                formatter,
                "`control` sends a signal rather than input, so `chars` must be empty."
            ),
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
