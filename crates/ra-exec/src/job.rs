//! Background job lifecycle plus `wait{until: done|timeout|match}`.
//!
//! A background job is a view of one execution session, not a second process owner. The
//! [`ProcessManager`](crate::session::ProcessManager) that started the command remains solely
//! responsible for its child, process group, capture buffers, deadlines, and termination. This
//! module adds the consumer-side lifecycle contract: inspect a job, wait for it to finish, wait
//! for a bounded interval, or stop when retained output contains a literal string.

use std::{fmt, sync::Arc, time::Duration};

use ra_core::event::exec::{ExecSessionId, ExecStreamKind};

use crate::{
    output::{ExecOutputSummary, RetainedRead},
    session::{
        ExecExecutionResult, ExecSessionState, ProcessManager, SessionCloseWait, SessionSnapshot,
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
        }
    }

    async fn wait_for_done(&self, timeout: Option<Duration>) -> Result<JobWaitResult, JobError> {
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
                let snapshot = self.job_snapshot(snapshot);
                return Ok(JobWaitResult::Matched { snapshot, stream });
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
}

impl<'a> StreamMatcher<'a> {
    fn new(needle: &'a str) -> Self {
        Self {
            needle,
            suffix: String::new(),
        }
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
        let matched = candidate.contains(self.needle);
        let keep = self.needle.len().saturating_sub(1);
        let mut start = candidate.len().saturating_sub(keep);
        while start < candidate.len() && !candidate.is_char_boundary(start) {
            start = start.saturating_add(1);
        }
        candidate[start..].clone_into(&mut self.suffix);
        matched
    }
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
    },
}

impl JobWaitResult {
    /// The state and output observed when this wait returned.
    #[must_use]
    pub const fn snapshot(&self) -> &JobSnapshot {
        match self {
            Self::Done(snapshot) | Self::TimedOut(snapshot) | Self::Matched { snapshot, .. } => {
                snapshot
            }
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
