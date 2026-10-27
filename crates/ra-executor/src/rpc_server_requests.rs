// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    unreachable_pub,
    reason = "Private implementation items are exposed only by the feature-gated test facade."
)]
#![cfg_attr(
    feature = "test-api",
    allow(missing_docs, reason = "Hidden source-test facade access.")
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "Preserve the source ownership and public argument contract."
)]
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ra_executor_protocol::JSONRPCRequest;
use ra_executor_protocol::RequestId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::rpc::RpcCallError;
use crate::rpc::RpcServerOutboundMessage;

#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
pub const MAX_IN_FLIGHT_SERVER_CALLS: usize = 256;
#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
const MAX_SERVER_REQUEST_TRACESTATE_LEN: usize = 512;

#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
type PendingRequest = oneshot::Sender<Result<Value, RpcCallError>>;

#[derive(Clone)]
#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
pub struct RpcServerRequestSender {
    inner: Arc<RpcServerRequestSenderInner>,
}

#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
struct RpcServerRequestSenderInner {
    outgoing_tx: mpsc::Sender<RpcServerOutboundMessage>,
    pending: Mutex<HashMap<RequestId, PendingRequest>>,
    call_slots: Semaphore,
    next_request_id: AtomicI64,
    closed: CancellationToken,
}

#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
struct PendingServerRequestGuard {
    inner: Arc<RpcServerRequestSenderInner>,
    request_id: RequestId,
}

impl Drop for PendingServerRequestGuard {
    fn drop(&mut self) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.request_id);
    }
}

#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
impl RpcServerRequestSender {
    #[must_use]
    pub fn new(outgoing_tx: mpsc::Sender<RpcServerOutboundMessage>) -> Self {
        Self {
            inner: Arc::new(RpcServerRequestSenderInner {
                outgoing_tx,
                pending: Mutex::new(HashMap::new()),
                call_slots: Semaphore::new(MAX_IN_FLIGHT_SERVER_CALLS),
                next_request_id: AtomicI64::new(1),
                closed: CancellationToken::new(),
            }),
        }
    }

    pub async fn call_with_timeout<P, T>(
        &self,
        method: &str,
        params: &P,
        call_timeout: Duration,
    ) -> Result<T, RpcCallError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let _call_slot = self.inner.call_slots.try_acquire().map_err(|_| {
            RpcCallError::PendingRequestLimitExceeded {
                limit: MAX_IN_FLIGHT_SERVER_CALLS,
            }
        })?;
        let params = serde_json::to_value(params).map_err(RpcCallError::Json)?;
        let request_id =
            RequestId::Integer(self.inner.next_request_id.fetch_add(1, Ordering::SeqCst));
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.inner.closed.is_cancelled() {
                return Err(RpcCallError::Closed);
            }
            pending.insert(request_id.clone(), response_tx);
        }
        let _pending = PendingServerRequestGuard {
            inner: Arc::clone(&self.inner),
            request_id: request_id.clone(),
        };
        let trace = Option::<ra_executor_protocol::W3cTraceContext>::None.map(|mut trace| {
            if trace
                .tracestate
                .as_ref()
                .is_some_and(|tracestate| tracestate.len() > MAX_SERVER_REQUEST_TRACESTATE_LEN)
            {
                trace.tracestate = None;
            }
            trace
        });
        let request = RpcServerOutboundMessage::Request(JSONRPCRequest {
            id: request_id,
            method: method.to_string(),
            params: Some(params),
            trace,
        });

        let response = timeout(call_timeout, async {
            tokio::select! {
                biased;
                () = self.inner.closed.cancelled() => return Err(RpcCallError::Closed),
                result = self.inner.outgoing_tx.send(request) => {
                    result.map_err(|_| RpcCallError::Closed)?;
                }
            }
            response_rx.await.map_err(|_| RpcCallError::Closed)?
        })
        .await
        .map_err(|_| RpcCallError::TimedOut {
            method: method.to_string(),
            timeout: call_timeout,
        })??;
        serde_json::from_value(response).map_err(RpcCallError::Json)
    }

    pub fn complete(&self, request_id: RequestId, result: Result<Value, RpcCallError>) -> bool {
        if let Some(pending) = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&request_id)
        {
            let _ = pending.send(result);
            true
        } else {
            matches!(
                request_id,
                RequestId::Integer(id)
                    if id > 0 && id < self.inner.next_request_id.load(Ordering::Acquire)
            )
        }
    }

    pub fn close(&self) {
        self.inner.closed.cancel();
        let pending = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending
                .drain()
                .map(|(_, pending)| pending)
                .collect::<Vec<_>>()
        };
        for pending in pending {
            let _ = pending.send(Err(RpcCallError::Closed));
        }
    }
}

#[cfg(feature = "test-api")]
#[allow(
    dead_code,
    reason = "Retain this source server request item for the pending executor server; source tests use it through the facade."
)]
impl RpcServerRequestSender {
    pub fn pending_request_count(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}
