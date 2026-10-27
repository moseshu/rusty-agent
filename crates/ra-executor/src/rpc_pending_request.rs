// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Removes client RPC registrations when their call completes or is canceled.
//! The pending map is locked only for synchronous bookkeeping, including in Drop.

use std::collections::HashMap;
use std::sync::Mutex;

use ra_executor_protocol::RequestId;

use super::PendingRequest;

pub(super) struct PendingRequestGuard<'a> {
    pub(super) pending: &'a Mutex<HashMap<RequestId, PendingRequest>>,
    pub(super) request_id: RequestId,
}

impl Drop for PendingRequestGuard<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.request_id);
    }
}
