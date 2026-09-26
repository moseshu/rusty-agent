//! Gathering what an interactive process printed while a caller waited for it.
//!
//! A port of the reference's `session/pty_output.py`. Readers push chunks as they arrive and wake
//! whoever is waiting; a caller waits up to its deadline, or until the process is done and every
//! chunk it produced has been taken, and gets back what accumulated — cut to its token budget.
//! Backends share it, because what "wait for output" means has to be the same whichever one a
//! model is talking to.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ra_core::sandbox::truncate_text_by_tokens;
use tokio::sync::Notify;

/// Output from one interactive process that has not been handed to a caller yet.
///
/// The reference keeps a deque, a lock and an event side by side; here they are one value, and the
/// event is a [`Notify`] whose stored permit plays the part of an event left set: a chunk pushed
/// while nobody waits still wakes the next wait at once.
#[derive(Debug, Default)]
pub struct PtyOutputBuffer {
    chunks: Mutex<VecDeque<Vec<u8>>>,
    notify: Notify,
}

impl PtyOutputBuffer {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a chunk and wakes a waiting caller.
    pub fn push(&self, chunk: Vec<u8>) {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(chunk);
        self.notify.notify_one();
    }

    /// Wakes a waiting caller without adding anything, as the process ending does.
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Moves every pending chunk onto the end of `output`.
    fn drain_into(&self, output: &mut Vec<u8>) {
        let mut chunks = self.chunks.lock().unwrap_or_else(PoisonError::into_inner);
        for chunk in chunks.drain(..) {
            output.extend_from_slice(&chunk);
        }
    }
}

/// Waits up to `yield_time_ms` for output, and returns what accumulated, cut to the budget.
///
/// Returns early once `is_done` answers `true` — after taking whatever arrived up to that moment,
/// including anything pushed while `is_done` was being asked. The output is decoded as UTF-8 with
/// replacement before it is cut and encoded again, as the reference's is, so bytes that are not
/// valid UTF-8 come back replaced even when nothing was cut. The second value is the token count
/// before cutting, present only when something was cut.
pub async fn collect_pty_output(
    buffer: &PtyOutputBuffer,
    is_done: impl Fn() -> bool,
    yield_time_ms: u64,
    max_output_tokens: Option<u64>,
) -> (Vec<u8>, Option<u64>) {
    let deadline = Instant::now() + Duration::from_millis(yield_time_ms);
    let mut output = Vec::new();

    loop {
        buffer.drain_into(&mut output);

        if Instant::now() >= deadline {
            break;
        }

        if is_done() {
            buffer.drain_into(&mut output);
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        if tokio::time::timeout(remaining, buffer.notify.notified())
            .await
            .is_err()
        {
            break;
        }
    }

    let text = String::from_utf8_lossy(&output);
    let (truncated, original_token_count) = truncate_text_by_tokens(&text, max_output_tokens);
    (truncated.into_bytes(), original_token_count)
}
