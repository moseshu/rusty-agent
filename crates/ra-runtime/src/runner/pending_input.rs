//! Input steered into a run while it runs: the pending input of Codex's session input queue
//! (`core/src/session/input_queue.rs`), for one run.
//!
//! A host pushes user messages while the run works; the run delivers them into its history at
//! its next model-call boundary, as Codex drains pending input before each sampling request but
//! the first of a turn that brought input of its own. A run that would finish while input is still
//! pending carries on and answers it, as Codex's turn follows up whenever input is pending.
//!
//! A run that finishes with nothing pending closes the queue in the same step, so input pushed
//! afterwards is refused rather than left for a run that will never read it. Codex instead keeps
//! input that arrives as its turn ends and starts another pass of the turn for it; a run here
//! cannot be restarted under its own identity once it has returned, so the decision is made
//! atomically with the push instead, and the refused input goes to the next run.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ra_core::item::Message;

/// Input waiting to be delivered into a running run.
///
/// Cloning shares the queue: the host keeps one handle to push through, and the run is given
/// another with [`RunRequest::with_pending_input`](super::RunRequest::with_pending_input).
#[derive(Debug, Clone, Default)]
pub struct PendingInput {
    state: Arc<Mutex<PendingInputState>>,
}

#[derive(Debug, Default)]
struct PendingInputState {
    messages: Vec<Message>,
    closed: bool,
}

impl PendingInput {
    /// An open queue with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `messages` for the run's next model-call boundary.
    ///
    /// # Errors
    ///
    /// Returns the messages unchanged once the queue is closed: the run has finished, or is about
    /// to, without reading them.
    pub fn push(&self, messages: Vec<Message>) -> Result<(), Vec<Message>> {
        let mut state = self.lock();
        if state.closed {
            return Err(messages);
        }
        state.messages.extend(messages);
        Ok(())
    }

    /// Whether nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().messages.is_empty()
    }

    /// Whether the queue refuses new input.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Closes the queue and returns what was still pending, as a run's end leaves it: Codex's
    /// `take_pending_input_for_turn_state` when its task finishes.
    #[must_use = "what was still pending is lost unless it is used"]
    pub fn close(&self) -> Vec<Message> {
        let mut state = self.lock();
        state.closed = true;
        std::mem::take(&mut state.messages)
    }

    /// Takes everything pending, leaving the queue open.
    pub(crate) fn take(&self) -> Vec<Message> {
        std::mem::take(&mut self.lock().messages)
    }

    /// Closes the queue if nothing is pending. Returns whether it did, which is whether the run
    /// may finish.
    pub(crate) fn close_if_empty(&self) -> bool {
        let mut state = self.lock();
        if state.messages.is_empty() {
            state.closed = true;
        }
        state.closed
    }

    fn lock(&self) -> MutexGuard<'_, PendingInputState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
