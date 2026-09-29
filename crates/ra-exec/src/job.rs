//! Background job lifecycle plus `wait{until: output|done|timeout|match}`.
//!
//! A background job is a view of one execution session, not a second process owner. The
//! [`ProcessManager`] that started the command remains solely
//! responsible for its child, process group, capture buffers, deadlines, and termination. This
//! module adds the consumer-side lifecycle contract: inspect a job, wait for new output, wait for
//! it to finish, wait for a bounded interval, or stop when retained output contains a literal
//! string.
//!
//! Every wait here declares itself to the session for as long as it runs, so that the two policies
//! that reclaim sessions nobody is watching — the idle sweep and the retention window — do not fire
//! against the one session a caller is waiting on. Neither the total deadline nor an explicit
//! cancellation is affected: those are about the command, not about who is still interested.

use std::{fmt, sync::Arc, time::Duration};

use ra_core::event::exec::{ExecSessionId, ExecStreamKind};

use crate::{
    output::{ExecOutputSummary, RetainedRead},
    session::{
        ExecExecutionResult, ExecSessionState, ProcessManager, SessionCloseWait, SessionSnapshot,
        SessionWatch,
    },
};

/// A handle for one execution session that was left running in the background.
///
/// Cloning a handle only clones its identity and the shared manager; it never duplicates the
/// process or changes which task supervises it.
#[derive(Clone)]
pub struct BackgroundJob {
    process_manager: Arc<ProcessManager>,
    session_id: ExecSessionId,
}

impl fmt::Debug for BackgroundJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackgroundJob")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl BackgroundJob {
    /// Creates a handle for a retained execution session.
    ///
    /// The identifier is checked when the handle is used so callers can retain a job identifier
    /// independently of the process manager that owns the session.
    #[must_use]
    pub fn new(process_manager: Arc<ProcessManager>, session_id: impl Into<ExecSessionId>) -> Self {
        Self {
            process_manager,
            session_id: session_id.into(),
        }
    }

    /// Creates a handle when an execution attempt actually yielded into the background.
    ///
    /// A completed execution has no background job and returns `None`.
    #[must_use]
    pub fn from_execution_result(
        process_manager: Arc<ProcessManager>,
        result: &ExecExecutionResult,
    ) -> Option<Self> {
        result
            .session_id()
            .cloned()
            .map(|session_id| Self::new(process_manager, session_id))
    }

    /// The execution-session identity of this job.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Declares that this job is being worked with until the returned guard is dropped.
    ///
    /// Each wait below takes one of these for its own duration. A caller doing more than waiting —
    /// reading the output it just waited for, then recording how much of it was delivered — wants
    /// one across the whole sequence, because the session has to still be there for the steps that
    /// follow the wait, not only for the wait itself.
    #[must_use]
    pub fn watch(&self) -> SessionWatch {
        self.process_manager.watch_session(&self.session_id)
    }

    /// Returns the current state and output captured for this job from one instant.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] after the manager has discarded the retained session.
    pub async fn snapshot(&self) -> Result<JobSnapshot, JobError> {
        let snapshot = self
            .process_manager
            .session_snapshot(&self.session_id)
            .await
            .ok_or_else(|| self.unknown_job())?;
        Ok(self.job_snapshot(snapshot))
    }

    /// Requests cancellation of this job's process group.
    ///
    /// Cancellation records the terminal reason promptly but the process can remain in the active
    /// set through its drain grace period. Use [`Self::wait`] with [`JobWaitUntil::Done`] when a
    /// caller needs to observe that it was reaped.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] after the manager has discarded the retained session.
    pub async fn cancel(&self) -> Result<(), JobError> {
        self.process_manager
            .cancel_retained(&self.session_id)
            .await
            .then_some(())
            .ok_or_else(|| self.unknown_job())
    }

    /// Waits for the condition named by `until`.
    ///
    /// A closed job wins over a timeout, and a matching retained output wins over closeout.
    /// `Match` is a case-sensitive literal substring search of each retained stream independently;
    /// it intentionally does not claim to reconstruct stdout/stderr interleaving. Text that was
    /// removed by output truncation cannot match, and neither can the marker standing in for it:
    /// a search sees a gap as a break, never as the words describing it. A `Match` condition
    /// always names a timeout, so a silent job cannot leave its caller waiting without a deadline.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] after the manager has discarded the retained session, or
    /// [`JobError::EmptyMatch`] when `Match` names an empty string.
    pub async fn wait(&self, until: JobWaitUntil) -> Result<JobWaitResult, JobError> {
        match until {
            JobWaitUntil::Done => self.wait_for_done(None).await,
            JobWaitUntil::Timeout(timeout) => self.wait_for_done(Some(timeout)).await,
            JobWaitUntil::Match { text, timeout } => self.wait_for_match(text, timeout).await,
            JobWaitUntil::Output {
                delivered_stdout,
                delivered_stderr,
                timeout,
            } => {
                self.wait_for_output(delivered_stdout, delivered_stderr, timeout)
                    .await
            }
        }
    }

    /// Waits for either stream to produce bytes past what the caller has already taken.
    ///
    /// Closeout outranks new output. Both can be true when the wait returns, and of the two only
    /// closeout says the output is complete; reporting the other would leave a caller asking again
    /// for a session that has nothing left to give.
    async fn wait_for_output(
        &self,
        delivered_stdout: u64,
        delivered_stderr: u64,
        timeout: Duration,
    ) -> Result<JobWaitResult, JobError> {
        let _watch = self.process_manager.watch_session(&self.session_id);
        self.process_manager
            .wait_for_output_or_close(
                &self.session_id,
                as_usize(delivered_stdout),
                as_usize(delivered_stderr),
                timeout,
            )
            .await
            .ok_or_else(|| self.unknown_job())?;
        let snapshot = self.snapshot().await?;
        if snapshot.is_closed() {
            return Ok(JobWaitResult::Done(snapshot));
        }
        let advanced = snapshot.output().stdout_bytes() > as_usize(delivered_stdout)
            || snapshot.output().stderr_bytes() > as_usize(delivered_stderr);
        Ok(if advanced {
            JobWaitResult::OutputReady(snapshot)
        } else {
            JobWaitResult::TimedOut(snapshot)
        })
    }

    async fn wait_for_done(&self, timeout: Option<Duration>) -> Result<JobWaitResult, JobError> {
        let _watch = self.process_manager.watch_session(&self.session_id);
        let outcome = self
            .process_manager
            .wait_for_session_close(&self.session_id, timeout)
            .await
            .ok_or_else(|| self.unknown_job())?;
        match outcome {
            SessionCloseWait::Closed(snapshot) => {
                Ok(JobWaitResult::Done(self.job_snapshot(snapshot)))
            }
            SessionCloseWait::TimedOut(snapshot) => {
                Ok(JobWaitResult::TimedOut(self.job_snapshot(snapshot)))
            }
        }
    }

    async fn wait_for_match(
        &self,
        text: String,
        timeout: Duration,
    ) -> Result<JobWaitResult, JobError> {
        if text.is_empty() {
            return Err(JobError::EmptyMatch);
        }

        let _watch = self.process_manager.watch_session(&self.session_id);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stdout_cursor = 0_usize;
        let mut stderr_cursor = 0_usize;
        let mut stdout_matcher = StreamMatcher::new(&text);
        let mut stderr_matcher = StreamMatcher::new(&text);
        loop {
            let final_pass = deadline <= tokio::time::Instant::now();
            let window = self
                .process_manager
                .scan_match_window(
                    &self.session_id,
                    stdout_cursor,
                    stderr_cursor,
                    final_pass,
                    |stdout, stderr| {
                        if stdout.is_some_and(|read| stdout_matcher.scan(read)) {
                            Some(ExecStreamKind::Stdout)
                        } else if stderr.is_some_and(|read| stderr_matcher.scan(read)) {
                            Some(ExecStreamKind::Stderr)
                        } else {
                            None
                        }
                    },
                )
                .await
                .ok_or_else(|| self.unknown_job())?;
            stdout_cursor = window.stdout_cursor;
            stderr_cursor = window.stderr_cursor;
            if let Some((stream, snapshot)) = window.matched {
                let excerpt = match stream {
                    ExecStreamKind::Stdout => stdout_matcher.take_excerpt(),
                    _ => stderr_matcher.take_excerpt(),
                };
                let snapshot = self.job_snapshot(snapshot);
                return Ok(JobWaitResult::Matched {
                    snapshot,
                    stream,
                    excerpt,
                });
            }
            if let Some(snapshot) = window.snapshot {
                let snapshot = self.job_snapshot(snapshot);
                if snapshot.is_closed() {
                    return Ok(JobWaitResult::Done(snapshot));
                }
                // A final pass has scanned every byte available at the deadline. No matching
                // output and no closeout means the deadline, rather than an observation, won.
                return Ok(JobWaitResult::TimedOut(snapshot));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            self.process_manager
                .wait_for_output_or_close(&self.session_id, stdout_cursor, stderr_cursor, remaining)
                .await
                .ok_or_else(|| self.unknown_job())?;
        }
    }

    fn job_snapshot(&self, snapshot: SessionSnapshot) -> JobSnapshot {
        JobSnapshot {
            session_id: self.session_id.clone(),
            state: snapshot.state,
            output: snapshot.output,
            closed: snapshot.closed,
        }
    }

    fn unknown_job(&self) -> JobError {
        JobError::UnknownJob {
            session_id: self.session_id.clone(),
        }
    }
}

/// Incrementally matches one decoded stream, retaining just enough text to bridge a read boundary.
struct StreamMatcher<'a> {
    needle: &'a str,
    suffix: String,
    excerpt: Option<String>,
}

impl<'a> StreamMatcher<'a> {
    fn new(needle: &'a str) -> Self {
        Self {
            needle,
            suffix: String::new(),
            excerpt: None,
        }
    }

    /// Takes the window around the match this matcher found.
    fn take_excerpt(&mut self) -> String {
        self.excerpt.take().unwrap_or_default()
    }

    /// Scans one read, treating a gap inside it as a break no carried prefix can cross.
    fn scan(&mut self, read: &RetainedRead) -> bool {
        if self.push(&read.continued) {
            return true;
        }
        match &read.resumed {
            Some(resumed) => {
                // The bytes between the two runs are gone for good, so anything held back from the
                // first cannot be completed by the second.
                self.suffix.clear();
                self.push(resumed)
            }
            None => false,
        }
    }

    fn push(&mut self, text: &str) -> bool {
        let mut candidate = std::mem::take(&mut self.suffix);
        candidate.push_str(text);
        let matched = candidate.find(self.needle);
        if let Some(at) = matched {
            self.excerpt = Some(excerpt_around(&candidate, at, self.needle.len()));
        }
        let keep = self.needle.len().saturating_sub(1);
        let mut start = candidate.len().saturating_sub(keep);
        while start < candidate.len() && !candidate.is_char_boundary(start) {
            start = start.saturating_add(1);
        }
        candidate[start..].clone_into(&mut self.suffix);
        matched.is_some()
    }
}

/// How much text on each side of a match the excerpt carries.
const EXCERPT_CONTEXT_BYTES: usize = 30;

/// Renders a short single-line window around a match.
///
/// A caller that matched on output produced before it ever asked has a result to explain: the wait
/// returned, and the bytes it returns may contain nothing resembling the text that ended it. Runs of
/// whitespace collapse so that one line of a result can hold the window whatever the stream did with
/// newlines; it is an excerpt, not a transcript.
fn excerpt_around(text: &str, at: usize, needle_len: usize) -> String {
    let start = floor_boundary(text, at.saturating_sub(EXCERPT_CONTEXT_BYTES));
    let end = ceil_boundary(
        text,
        at.saturating_add(needle_len)
            .saturating_add(EXCERPT_CONTEXT_BYTES)
            .min(text.len()),
    );
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    let window = text[start..end].split_whitespace().collect::<Vec<_>>();
    excerpt.push_str(&window.join(" "));
    if end < text.len() {
        excerpt.push('…');
    }
    excerpt
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index = index.saturating_sub(1);
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index = index.saturating_add(1);
    }
    index
}

fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// A coherent lifecycle and output snapshot of one background job.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    session_id: ExecSessionId,
    state: ExecSessionState,
    output: ExecOutputSummary,
    closed: bool,
}

impl JobSnapshot {
    /// The execution-session identity of this job.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// The lifecycle state observed with this output snapshot.
    #[must_use]
    pub const fn state(&self) -> &ExecSessionState {
        &self.state
    }

    /// The output summary observed with this lifecycle snapshot.
    #[must_use]
    pub const fn output(&self) -> &ExecOutputSummary {
        &self.output
    }

    /// Whether the job had reached a terminal state in this snapshot.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Whether the child was reaped and the output readers completed their closeout.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
}

/// A condition that a background-job wait observes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobWaitUntil {
    /// Wait until the child is reaped and its output readers finish closeout.
    Done,
    /// Wait for at most this long, returning early if closeout finishes.
    Timeout(Duration),
    /// Wait until one stream contains literal text, the job closes, or `timeout` elapses.
    Match {
        /// Case-sensitive literal text to find in one output stream.
        text: String,
        /// Maximum time spent waiting for the literal text.
        timeout: Duration,
    },
    /// Wait until either stream passes the offsets the caller already took, the job closes, or
    /// `timeout` elapses.
    ///
    /// The offsets are what makes this about output the caller has not seen rather than about a
    /// session that may have been printing for minutes before anyone asked.
    Output {
        /// Stdout offset the caller has already delivered onward.
        delivered_stdout: u64,
        /// Stderr offset the caller has already delivered onward.
        delivered_stderr: u64,
        /// Maximum time spent waiting.
        timeout: Duration,
    },
}

/// The observed reason a background-job wait returned.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum JobWaitResult {
    /// The child was reaped and its output readers finished closeout.
    Done(JobSnapshot),
    /// A bounded wait elapsed before the job closed out.
    ///
    /// The snapshot's state may already be terminal: a cancellation or an expiry records its
    /// reason immediately, and the process group can still be draining when the wait expires.
    TimedOut(JobSnapshot),
    /// A retained output stream contained the requested literal text.
    Matched {
        /// State and output observed when the text matched.
        snapshot: JobSnapshot,
        /// The stream that contained the requested text.
        stream: ExecStreamKind,
        /// A short window around the match, whitespace collapsed onto one line.
        excerpt: String,
    },
    /// A stream produced output past the offsets the caller had already taken.
    OutputReady(JobSnapshot),
}

impl JobWaitResult {
    /// The state and output observed when this wait returned.
    #[must_use]
    pub const fn snapshot(&self) -> &JobSnapshot {
        match self {
            Self::Done(snapshot)
            | Self::TimedOut(snapshot)
            | Self::OutputReady(snapshot)
            | Self::Matched { snapshot, .. } => snapshot,
        }
    }
}

/// A background-job operation could not be completed.
#[non_exhaustive]
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum JobError {
    /// The process manager no longer retains the job's session.
    #[error("background job `{session_id}` is no longer available")]
    UnknownJob {
        /// Identifier that no longer resolves to a retained execution session.
        session_id: ExecSessionId,
    },
    /// A match condition did not name any text.
    #[error("a background-job match must not be empty")]
    EmptyMatch,
}
