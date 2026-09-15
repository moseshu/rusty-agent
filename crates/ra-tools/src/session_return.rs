//! The result shape shared by the two tools that speak for an execution session.
//!
//! `exec_command` and `write_stdin` hand back the same kind of thing — some output, and a session
//! that may or may not be finished with it — and for a while they said it in two vocabularies. The
//! model reading them cannot tell that apart from a difference in the facts, so both now build this
//! one value and let it do the talking.
//!
//! Two questions are answered every time, because either one alone leaves a caller guessing:
//!
//! - **Why did this return?** New output, a match, closeout, or the wait running out. A result that
//!   shows no output without saying why is indistinguishable from a command that printed nothing.
//! - **Is the output complete?** Only closeout says so. A cancelled session records its death
//!   immediately and keeps draining for up to the grace period afterwards, so "it was cancelled" and
//!   "everything it printed is here" are separate claims, and reporting the first as if it implied
//!   the second loses whatever the command said on its way out.

use std::{fmt::Write as _, time::Duration};

use ra_core::{
    event::exec::{ExecSessionId, ExecStreamKind},
    tool::{ObservationMetadata, ToolOutput, Truncation, TruncationStage},
};
use ra_exec::session::ExecSessionState;

/// What a call was waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Watched<'a> {
    /// Output the caller has not been given yet.
    NewOutput,
    /// The command finishing and its output being collected.
    Closeout,
    /// Literal text appearing in the output.
    Text(&'a str),
}

/// Why the call returned when it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReturnReason {
    /// Output the caller had not seen arrived.
    Output,
    /// The text being waited for appeared.
    Matched,
    /// The command finished and its output was collected.
    Done,
    /// The wait reached its limit first.
    Timeout,
    /// Another call owned this session's delivery, so the wait never started.
    ///
    /// This call did not inspect or consume output. Another call may have consumed it, and
    /// capture limits may have discarded bytes; contention makes no promise about what remains.
    Contended {
        /// Whether this call's control signal was delivered before it gave up on the lock.
        control_sent: bool,
    },
}

/// Everything a session-facing tool result reports, in one place.
pub(crate) struct SessionReturn<'a> {
    /// The session this result is about.
    pub(crate) session_id: &'a ExecSessionId,
    /// What the call was waiting for.
    pub(crate) watched: Watched<'a>,
    /// How long it was prepared to wait.
    pub(crate) waited: Duration,
    /// Why it stopped waiting.
    pub(crate) reason: ReturnReason,
    /// The session's lifecycle state when the wait returned.
    pub(crate) state: ExecSessionState,
    /// Whether the child was reaped and its output readers finished.
    pub(crate) closed: bool,
    /// The process exit status, when one was recorded.
    ///
    /// Carried separately from [`Self::state`] because the two can disagree about which fact
    /// matters: a cancelled session ends as `Cancelled` and still exits with a status, and a reader
    /// that only had the state would never learn the command's own answer.
    pub(crate) exit_code: Option<i32>,
    /// Output being delivered by this result, from the standard output stream.
    pub(crate) stdout: String,
    /// Output being delivered by this result, from the standard error stream.
    pub(crate) stderr: String,
    /// The stream and window of a match, when one ended the wait.
    pub(crate) matched: Option<(ExecStreamKind, String)>,
    /// Source bytes of output this result covers.
    pub(crate) delivered_bytes: u64,
    /// Source bytes the capture ceiling dropped from within that span.
    pub(crate) omitted_bytes: u64,
}

impl SessionReturn<'_> {
    /// Renders the model-facing result.
    pub(crate) fn render(self) -> ToolOutput {
        let mut body = self.headline();
        let streams = render_streams(&self.stdout, &self.stderr);
        if streams.is_empty() {
            // Not said twice: a headline that has already reported an empty-handed return — a
            // timeout that saw no output, or a call that never got to look — does not need a second
            // sentence saying the same thing in other words.
            if !self.headline_already_reported_no_output() {
                body.push_str(" No new output.");
            }
        } else {
            body.push_str("\n\n");
            body.push_str(&streams);
        }

        let mut metadata = ObservationMetadata::new();
        if let Some(next_step) = self.next_step() {
            metadata = metadata.with_guidance(next_step);
        }
        if self.omitted_bytes > 0 {
            metadata = metadata
                .with_truncation(Truncation::new(
                    TruncationStage::Tool,
                    self.delivered_bytes,
                    self.delivered_bytes.saturating_sub(self.omitted_bytes),
                ))
                .with_guidance(
                    "Output was cut to fit; the middle of this delivery is missing. Narrow the command or filter its output to see the rest.",
                );
        }
        ToolOutput::text(body).with_metadata(metadata)
    }

    /// The first line: why this returned, then where the session stands.
    fn headline(&self) -> String {
        let mut line = String::new();
        match self.reason {
            // The output below, and the standing sentence, already say it.
            ReturnReason::Output | ReturnReason::Done => {}
            ReturnReason::Matched => {
                if let Some((stream, excerpt)) = &self.matched {
                    let _ = write!(
                        line,
                        "Matched on {stream}: {excerpt}. ",
                        stream = stream_label(*stream)
                    );
                }
            }
            ReturnReason::Timeout => {
                let millis = self.waited.as_millis();
                match self.watched {
                    Watched::NewOutput => {
                        let _ = write!(line, "Waited {millis} ms and no new output arrived. ");
                    }
                    Watched::Closeout => {
                        let _ = write!(
                            line,
                            "Waited {millis} ms and the command has not finished. "
                        );
                    }
                    Watched::Text(text) => {
                        let _ = write!(line, "Waited {millis} ms without `{text}` appearing. ");
                    }
                }
            }
            ReturnReason::Contended { control_sent } => {
                let millis = self.waited.as_millis();
                let _ = write!(
                    line,
                    "Another call still owned this session's interaction after {millis} ms; this call did not inspect or collect output. "
                );
                line.push_str(if control_sent {
                    "The control signal was sent before giving up on the wait. "
                } else {
                    "No input was sent. "
                });
            }
        }
        line.push_str(&self.standing());
        line
    }

    /// Whether the headline has already accounted for a result that carries no output.
    const fn headline_already_reported_no_output(&self) -> bool {
        match self.reason {
            ReturnReason::Timeout => matches!(self.watched, Watched::NewOutput),
            ReturnReason::Contended { .. } => true,
            _ => false,
        }
    }

    /// Where the session stands, and whether its output is all here.
    fn standing(&self) -> String {
        let session_id = self.session_id;
        if self.state.is_active() {
            return format!("Session `{session_id}` is still running.");
        }
        let ended = ended_phrase(&self.state, self.exit_code);
        if self.closed && matches!(self.reason, ReturnReason::Contended { .. }) {
            format!(
                "Session `{session_id}` {ended}; output capture has closed, but this call did not check for undelivered output."
            )
        } else if self.closed {
            format!("Session `{session_id}` {ended}; its output is complete.")
        } else {
            format!(
                "Session `{session_id}` {ended} and is still shutting down, so its output is not complete yet."
            )
        }
    }

    /// What the caller can still do with this session, when there is anything left to do.
    fn next_step(&self) -> Option<String> {
        if matches!(self.reason, ReturnReason::Contended { .. }) {
            return Some(format!(
                "Call `write_stdin` for session `{}` with empty `chars` to check for remaining output after the other interaction releases it. Another call may already have collected the output; capture limits may also have discarded bytes.",
                self.session_id
            ));
        }
        if self.closed {
            return None;
        }
        let session_id = self.session_id;
        Some(format!(
            "Session `{session_id}` is still there: call `write_stdin` with that identifier to send input or collect more output, `until: done` to wait for it to finish, or `control: cancel` to stop it."
        ))
    }
}

/// How a finished session finished, as a verb phrase.
fn ended_phrase(state: &ExecSessionState, exit_code: Option<i32>) -> String {
    match state {
        ExecSessionState::Exited {
            exit_code: Some(code),
        } => format!("exited with status {code}"),
        ExecSessionState::Exited { exit_code: None } => match exit_code {
            Some(code) => format!("exited with status {code}"),
            None => "exited without a status".to_owned(),
        },
        ExecSessionState::Cancelled => with_status("was cancelled", exit_code),
        ExecSessionState::Expired { reason } => {
            with_status(&format!("was stopped by the host ({reason})"), exit_code)
        }
        ExecSessionState::Failed { error } => format!("failed ({error})"),
        // Not reachable from the branch that calls this, and `ExecSessionState` is open: a state
        // this build cannot name is still a state that has ended, and saying only that is better
        // than naming it wrongly.
        _ => "has ended".to_owned(),
    }
}

/// Appends the process's own answer to a phrase that describes why it was stopped.
fn with_status(phrase: &str, exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("{phrase} and exited with status {code}"),
        None => phrase.to_owned(),
    }
}

/// Joins the two streams, labelling stderr only when there is something on both.
pub(crate) fn render_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("{stdout}\n\n--- stderr ---\n{stderr}"),
    }
}

const fn stream_label(stream: ExecStreamKind) -> &'static str {
    match stream {
        ExecStreamKind::Stdout => "stdout",
        ExecStreamKind::Stderr => "stderr",
        _ => "the output",
    }
}
