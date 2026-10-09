//! Which requests may still start or change turns: Codex's `TurnAdmission`
//! (`app-server/src/turn_admission.rs`).
//!
//! A request that starts or steers a turn, or changes the settings turns run with, is admitted
//! when it arrives and holds its permit until it has been handled. Once the server begins to drain
//! for a restart, admission closes: new such requests are refused with
//! [`server_draining_error`], and those already admitted finish. The server stops once no permit
//! is held and no turn runs.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;

use crate::frame::JsonRpcErrorObject;

/// What a request refused while the server drains is answered with: Codex's
/// `server_draining_error`.
pub(crate) fn server_draining_error() -> JsonRpcErrorObject {
    JsonRpcErrorObject::invalid_request("Server is draining; retry after reconnecting")
}

#[derive(Debug, Default)]
struct AdmissionState {
    closed: bool,
    active: usize,
}

/// Admits turn work until the server drains, counting the permits held.
#[derive(Clone, Debug)]
pub(crate) struct TurnAdmission {
    state: Arc<Mutex<AdmissionState>>,
    active_tx: watch::Sender<usize>,
}

impl Default for TurnAdmission {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(AdmissionState::default())),
            active_tx: watch::channel(0).0,
        }
    }
}

/// Held by an admitted request until it has been handled.
#[derive(Debug)]
pub(crate) struct TurnPermit(TurnAdmission);

impl TurnAdmission {
    /// Refuses all turn work from now on.
    pub(crate) fn begin_drain(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
    }

    /// The number of permits held, as it changes.
    pub(crate) fn subscribe_active(&self) -> watch::Receiver<usize> {
        self.active_tx.subscribe()
    }

    // Admitting and closing take the same short lock. The permit keeps the drain from finishing
    // while an earlier request is still preparing or submitting its work.
    pub(crate) fn admit(&self) -> Result<TurnPermit, JsonRpcErrorObject> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(server_draining_error());
        }
        state.active += 1;
        self.active_tx.send_replace(state.active);
        Ok(TurnPermit(self.clone()))
    }
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.active -= 1;
        self.0.active_tx.send_replace(state.active);
    }
}
