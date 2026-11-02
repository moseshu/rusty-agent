// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's network approval service (`core/src/tools/network_approval.rs`): the calls whose
//! network traffic may be reviewed, the host reviews in flight and their deduplication, the hosts
//! approved or denied for the session, and the outcome a reviewed denial leaves on its call.
//!
//! # Differences from Codex
//!
//! - Codex's handler reaches its session for the active turn, the client's decision and the
//!   amendment it saves; here that is [`NetworkApprovalSession`], which the thread's proxy
//!   implements. Guardian review, permission-request hooks, the model-visible "rule saved"
//!   message, warning events and decision telemetry are not part of it.
//! - Per-call registration is Codex's `begin_network_approval` for the thread's own proxy: a
//!   local thread has no per-environment owner policy, so no execution-only carrier is built.
//! - `GuardianNetworkAccessTrigger` is Codex's Guardian review payload; Guardian is not ported, so
//!   the call keeps the same fields as [`NetworkAccessTrigger`].
//! - A call does not keep Codex's `TurnEnvironmentSnapshot`: a local thread has one environment,
//!   and the call already carries its permission profile and the trigger its working directory.
//! - Outcomes are returned as the rejection text rather than Codex's `ToolError::Rejected`; the
//!   command tool reports it as its rejection.
//! - The blocked-request observer does not record Codex's sandbox-violation telemetry.
//! - Everything the original tests reach is public, and the module itself is public only with
//!   `test-api`.
#![cfg_attr(
    not(feature = "test-api"),
    allow(
        unreachable_pub,
        reason = "Reachable outside the crate only through the hidden test bridge."
    )
)]
#![cfg_attr(
    not(feature = "test-api"),
    expect(
        dead_code,
        reason = "The inline approval handler and per-call registration consume the service."
    )
)]
#![allow(
    missing_docs,
    clippy::missing_const_for_fn,
    clippy::new_without_default,
    clippy::unused_async,
    clippy::must_use_candidate,
    reason = "Preserve the upstream network approval service records and async contracts."
)]

use crate::exec_policy::SandboxPermissions;
use crate::network_policy_decision::denied_network_policy_message;
use crate::permissions::AdditionalPermissionProfile;
use async_trait::async_trait;
use indexmap::IndexMap;
use ra_core::sandbox::token_truncation::{TruncationPolicy, truncate_with_token_budget};
use ra_core::tool::ToolLookupKey;
use ra_filesystem::models::PermissionProfile;
use ra_network_proxy::{
    BlockedRequest, BlockedRequestObserver, NetworkDecision, NetworkPolicyDecider,
    NetworkPolicyRequest, NetworkProtocol, NetworkProxy, NetworkRequestCancellation,
    NetworkRequestCancellationReason, NetworkRequestDisconnect,
};
use ra_paths::PathUri;
use ra_protocol::server::ReviewDecision;
use ra_protocol::server::protocol::{
    AskForApproval, NetworkApprovalContext, NetworkApprovalProtocol, NetworkPolicyAmendment,
    NetworkPolicyRuleAction,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as SyncMutex, OnceLock as SyncOnceLock};
use tokio::sync::{Mutex, Notify, OnceCell};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

const ABANDONED_NETWORK_APPROVAL_MESSAGE: &str =
    "network approval was cancelled before a decision was returned";
const REJECTION_MESSAGE_MAX_TOKENS: i64 = 900;

/// Codex's `truncate_rejection_message`: a rejection the model reads stays within its budget.
fn truncate_rejection_message(message: &str) -> String {
    truncate_with_token_budget(
        message,
        TruncationPolicy::tokens(REJECTION_MESSAGE_MAX_TOKENS),
    )
    .0
}

/// The command whose network access is under review: the fields of Codex's
/// `GuardianNetworkAccessTrigger`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkAccessTrigger {
    pub call_id: String,
    pub tool_name: String,
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub justification: Option<String>,
    pub tty: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct DeferredNetworkApproval {
    registration_id: String,
    cancellation_token: CancellationToken,
    finish_outcome: Arc<OnceCell<Option<String>>>,
    _execution_proxy: Option<NetworkProxy>,
}

impl DeferredNetworkApproval {
    pub fn registration_id(&self) -> &str {
        &self.registration_id
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    pub async fn finish(&self, service: &NetworkApprovalService) -> Result<(), String> {
        let outcome = self
            .finish_outcome
            .get_or_init(|| async { service.finish_call_outcome(&self.registration_id).await })
            .await
            .clone();
        let outcome =
            outcome.or_else(|| abandoned_network_approval_outcome(&self.cancellation_token));
        network_approval_outcome_to_result(outcome)
    }
}

#[derive(Debug)]
pub struct ActiveNetworkApproval {
    registration_id: Option<String>,
    cancellation_token: CancellationToken,
    execution_proxy: NetworkProxy,
}

impl ActiveNetworkApproval {
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    pub fn execution_proxy(&self) -> &NetworkProxy {
        &self.execution_proxy
    }

    pub fn into_deferred(self) -> Option<DeferredNetworkApproval> {
        let ActiveNetworkApproval {
            registration_id,
            cancellation_token,
            execution_proxy,
        } = self;
        registration_id.map(|registration_id| DeferredNetworkApproval {
            registration_id,
            cancellation_token,
            finish_outcome: Arc::new(OnceCell::new()),
            _execution_proxy: Some(execution_proxy),
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HostApprovalKey {
    pub environment_id: String,
    pub host: String,
    pub protocol: &'static str,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PendingHostApprovalKey {
    pub host: HostApprovalKey,
    pub turn_id: String,
    pub execution_id: Option<String>,
}

impl HostApprovalKey {
    pub fn from_request(
        request: &NetworkPolicyRequest,
        protocol: NetworkApprovalProtocol,
        environment_id: String,
    ) -> Self {
        Self {
            environment_id,
            host: request.host.to_ascii_lowercase(),
            protocol: protocol_key_label(protocol),
            port: request.port,
        }
    }
}

pub fn protocol_key_label(protocol: NetworkApprovalProtocol) -> &'static str {
    match protocol {
        NetworkApprovalProtocol::Http => "http",
        NetworkApprovalProtocol::Https => "https",
        NetworkApprovalProtocol::Socks5Tcp => "socks5-tcp",
        NetworkApprovalProtocol::Socks5Udp => "socks5-udp",
        protocol => unreachable!("network approval protocol {protocol:?} has no approval key"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingApprovalDecision {
    AllowOnce,
    AllowForSession,
    Deny,
}

pub fn network_approval_outcome_to_result(outcome: Option<String>) -> Result<(), String> {
    match outcome {
        Some(rejection) => Err(truncate_rejection_message(&rejection)),
        None => Ok(()),
    }
}

pub fn abandoned_network_approval_outcome(
    cancellation_token: &CancellationToken,
) -> Option<String> {
    cancellation_token
        .is_cancelled()
        .then(|| ABANDONED_NETWORK_APPROVAL_MESSAGE.to_string())
}

/// Whether an allowlist miss may be reviewed instead of hard-denied.
pub fn allows_network_approval_flow(policy: AskForApproval) -> bool {
    !matches!(policy, AskForApproval::Never)
}

pub fn permission_profile_allows_network_approval_flow(
    permission_profile: &PermissionProfile,
) -> bool {
    matches!(permission_profile, PermissionProfile::Managed { .. })
}

impl PendingApprovalDecision {
    pub fn to_network_decision(self) -> NetworkDecision {
        match self {
            Self::AllowOnce | Self::AllowForSession => NetworkDecision::Allow,
            Self::Deny => NetworkDecision::deny("not_allowed"),
        }
    }
}

pub struct PendingHostApproval {
    pub decision: SyncOnceLock<PendingApprovalDecision>,
    pub notify: Notify,
}

impl PendingHostApproval {
    pub fn new() -> Self {
        Self {
            decision: SyncOnceLock::new(),
            notify: Notify::new(),
        }
    }

    pub async fn wait_for_decision(&self) -> PendingApprovalDecision {
        loop {
            let notified = self.notify.notified();
            if let Some(decision) = self.decision.get() {
                return *decision;
            }
            notified.await;
        }
    }

    pub fn set_decision(&self, decision: PendingApprovalDecision) {
        if self.decision.set(decision).is_ok() {
            self.notify.notify_waiters();
        }
    }
}

pub struct ActiveNetworkApprovalCall {
    pub registration_id: String,
    pub turn_id: String,
    pub trigger: NetworkAccessTrigger,
    pub tool_name: ToolLookupKey,
    pub command: String,
    pub environment_id: String,
    pub permission_profile: PermissionProfile,
    pub cancellation_token: CancellationToken,
}

pub enum ActiveNetworkApprovalAttribution {
    None,
    Single(Arc<ActiveNetworkApprovalCall>),
    Ambiguous,
}

pub struct NetworkRequestAttribution {
    pub owner_call: Option<Arc<ActiveNetworkApprovalCall>>,
    pub environment_id: Option<String>,
}

#[derive(Default)]
pub struct NetworkApprovalCallState {
    pub active_calls: IndexMap<String, Arc<ActiveNetworkApprovalCall>>,
    pub call_outcomes: HashMap<String, String>,
}

pub struct NetworkApprovalService {
    // Pending-request cleanup must publish a tool outcome synchronously before cancelling it.
    pub calls: SyncMutex<NetworkApprovalCallState>,
    // Owner cleanup runs from Drop, so this lock cannot require an async task.
    pub pending_host_approvals:
        SyncMutex<HashMap<PendingHostApprovalKey, Arc<PendingHostApproval>>>,
    // Keep persisted session policy and the in-memory approval caches in the same order.
    pub session_policy_commit_lock: Mutex<()>,
    pub session_approved_hosts: Mutex<HashSet<HostApprovalKey>>,
    pub session_denied_hosts: Mutex<HashSet<HostApprovalKey>>,
}

/// Removes and resolves the exact pending generation created by one request.
///
/// Dropping an unfinished owner fails its existing waiters closed without
/// deleting a newer same-host request.
pub struct PendingHostApprovalOwner<'a> {
    pub service: &'a NetworkApprovalService,
    pub key: PendingHostApprovalKey,
    pub pending: Arc<PendingHostApproval>,
    pub execution_cancellation: Option<CancellationToken>,
    pub disconnect: Option<NetworkRequestDisconnect>,
    pub cancellation: Option<NetworkRequestCancellation>,
    pub decision_on_drop: PendingApprovalDecision,
    pub completed: bool,
}

impl<'a> PendingHostApprovalOwner<'a> {
    pub fn new(
        service: &'a NetworkApprovalService,
        key: PendingHostApprovalKey,
        pending: Arc<PendingHostApproval>,
        execution_cancellation: Option<CancellationToken>,
    ) -> Self {
        Self {
            service,
            key,
            pending,
            execution_cancellation,
            disconnect: None,
            cancellation: None,
            decision_on_drop: PendingApprovalDecision::Deny,
            completed: false,
        }
    }

    pub fn set_decision_on_drop(&mut self, decision: PendingApprovalDecision) {
        self.decision_on_drop = decision;
    }

    pub fn complete(mut self, decision: PendingApprovalDecision) {
        self.cancel_execution_if_denied(decision);
        self.publish_and_remove(decision);
        self.completed = true;
    }

    pub fn cancel_execution_if_denied(&self, decision: PendingApprovalDecision) {
        if matches!(decision, PendingApprovalDecision::Deny)
            && let Some(execution_cancellation) = &self.execution_cancellation
        {
            execution_cancellation.cancel();
        }
    }

    pub fn publish_and_remove(&self, decision: PendingApprovalDecision) {
        // Remove this generation before waking its waiters so a concurrent retry
        // cannot attach to an already-completed approval.
        let mut approvals = self
            .service
            .pending_host_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if approvals
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.pending))
        {
            approvals.remove(&self.key);
        }
        drop(approvals);
        self.pending.set_decision(decision);
    }
}

impl Drop for PendingHostApprovalOwner<'_> {
    fn drop(&mut self) {
        if !self.completed {
            if self
                .cancellation
                .as_ref()
                .and_then(NetworkRequestCancellation::reason)
                == Some(NetworkRequestCancellationReason::ProcessFinished)
            {
                // Normal process cleanup is not a new review failure. Any earlier
                // denial is already recorded in the call outcome.
                self.publish_and_remove(self.decision_on_drop);
                return;
            }
            if matches!(self.decision_on_drop, PendingApprovalDecision::Deny)
                && let Some(registration_id) = self.key.execution_id.as_deref()
                && let Some(elapsed) = self
                    .disconnect
                    .as_ref()
                    .and_then(NetworkRequestDisconnect::elapsed)
            {
                let elapsed_ms = elapsed.as_millis();
                self.service.record_call_outcome_if_absent(registration_id, format!(
                    "Network request disconnected after {elapsed_ms} ms, before approval could complete"
                ));
            }
            self.cancel_execution_if_denied(self.decision_on_drop);
            self.publish_and_remove(self.decision_on_drop);
        }
    }
}

impl Default for NetworkApprovalService {
    fn default() -> Self {
        Self {
            calls: SyncMutex::new(NetworkApprovalCallState::default()),
            pending_host_approvals: SyncMutex::new(HashMap::new()),
            session_policy_commit_lock: Mutex::new(()),
            session_approved_hosts: Mutex::new(HashSet::new()),
            session_denied_hosts: Mutex::new(HashSet::new()),
        }
    }
}

impl NetworkApprovalService {
    /// Replace the target session's approval cache with the source session's
    /// currently approved hosts.
    // Codex expects this lint under its clippy configuration, which this workspace does not set.
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "the approval snapshot must not interleave with a session policy commit"
    )]
    pub async fn sync_session_approved_hosts_to(&self, other: &Self) {
        let _commit_guard = self.session_policy_commit_lock.lock().await;
        let approved_hosts = self.session_approved_hosts.lock().await.clone();
        let mut other_approved_hosts = other.session_approved_hosts.lock().await;
        other_approved_hosts.clear();
        other_approved_hosts.extend(approved_hosts.iter().cloned());
    }

    pub async fn register_call(&self, call: ActiveNetworkApprovalCall) {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls
            .active_calls
            .insert(call.registration_id.clone(), Arc::new(call));
    }

    pub async fn unregister_call(&self, registration_id: &str) {
        self.remove_call(registration_id).await;
    }

    pub async fn resolve_single_active_call(&self) -> Option<Arc<ActiveNetworkApprovalCall>> {
        let calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Shared proxy requests can still arrive without an execution ID. Only pick an owner when
        // there is exactly one candidate; with concurrent calls, canceling one would be a guess.
        if calls.active_calls.len() == 1 {
            return calls.active_calls.values().next().cloned();
        }

        None
    }

    pub async fn resolve_active_call_by_execution_id(
        &self,
        execution_id: &str,
    ) -> Option<Arc<ActiveNetworkApprovalCall>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_calls
            .get(execution_id)
            .cloned()
    }

    pub async fn resolve_active_call_attribution(&self) -> ActiveNetworkApprovalAttribution {
        let calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match calls.active_calls.len() {
            0 => ActiveNetworkApprovalAttribution::None,
            1 => calls.active_calls.values().next().cloned().map_or(
                ActiveNetworkApprovalAttribution::None,
                ActiveNetworkApprovalAttribution::Single,
            ),
            _ => ActiveNetworkApprovalAttribution::Ambiguous,
        }
    }

    pub async fn resolve_request_attribution(
        &self,
        request: &NetworkPolicyRequest,
    ) -> Option<NetworkRequestAttribution> {
        if let Some(execution_id) = request.execution_id.as_deref() {
            let call = self
                .resolve_active_call_by_execution_id(execution_id)
                .await?;
            let environment_id = request
                .environment_id
                .clone()
                .unwrap_or_else(|| call.environment_id.clone());
            return (call.environment_id == environment_id).then_some(NetworkRequestAttribution {
                owner_call: Some(call),
                environment_id: Some(environment_id),
            });
        }

        if let Some(environment_id) = request.environment_id.clone() {
            let owner_call = match self.resolve_active_call_attribution().await {
                ActiveNetworkApprovalAttribution::Single(call) => {
                    (call.environment_id == environment_id).then_some(call)
                }
                ActiveNetworkApprovalAttribution::None
                | ActiveNetworkApprovalAttribution::Ambiguous => None,
            };
            return Some(NetworkRequestAttribution {
                owner_call,
                environment_id: Some(environment_id),
            });
        }

        match self.resolve_active_call_attribution().await {
            ActiveNetworkApprovalAttribution::None => Some(NetworkRequestAttribution {
                owner_call: None,
                environment_id: None,
            }),
            ActiveNetworkApprovalAttribution::Single(call) => {
                let environment_id = call.environment_id.clone();
                Some(NetworkRequestAttribution {
                    owner_call: Some(call),
                    environment_id: Some(environment_id),
                })
            }
            ActiveNetworkApprovalAttribution::Ambiguous => None,
        }
    }

    pub fn get_or_create_pending_approval(
        &self,
        key: PendingHostApprovalKey,
    ) -> (Arc<PendingHostApproval>, bool) {
        let mut pending = self
            .pending_host_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = pending.get(&key).cloned() {
            return (existing, false);
        }

        let created = Arc::new(PendingHostApproval::new());
        pending.insert(key, Arc::clone(&created));
        (created, true)
    }

    #[cfg(feature = "test-api")]
    pub async fn take_call_outcome(&self, registration_id: &str) -> Option<String> {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.call_outcomes.remove(registration_id)
    }

    pub fn record_call_outcome(&self, registration_id: &str, outcome: String) {
        if let Some(cancellation_token) = self.store_call_outcome(registration_id, outcome) {
            cancellation_token.cancel();
        }
    }

    pub fn store_call_outcome(
        &self,
        registration_id: &str,
        outcome: String,
    ) -> Option<CancellationToken> {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let call = calls.active_calls.get(registration_id)?.clone();
        // Explicit network-review outcomes replace generic blocked-request fallbacks.
        calls
            .call_outcomes
            .insert(registration_id.to_string(), outcome);

        Some(call.cancellation_token.clone())
    }

    pub fn record_call_outcome_if_absent(&self, registration_id: &str, outcome: String) {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(call) = calls.active_calls.get(registration_id).cloned() else {
            return;
        };
        // Cancelling an execution can disconnect its other pending requests.
        // Those fallbacks must not replace the reason it was cancelled.
        calls
            .call_outcomes
            .entry(registration_id.to_string())
            .or_insert(outcome);

        drop(calls);
        call.cancellation_token.cancel();
    }

    pub async fn remove_call(&self, registration_id: &str) -> Option<String> {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.active_calls.shift_remove(registration_id);
        calls.call_outcomes.remove(registration_id)
    }

    pub async fn finish_call_outcome(&self, registration_id: &str) -> Option<String> {
        self.remove_call(registration_id).await
    }

    pub async fn record_blocked_request(&self, blocked: BlockedRequest) {
        let Some(message) = denied_network_policy_message(&blocked) else {
            return;
        };

        let owner_call = if let Some(execution_id) = blocked.execution_id.as_deref() {
            self.resolve_active_call_by_execution_id(execution_id).await
        } else {
            self.resolve_single_active_call().await
        };
        let Some(owner_call) = owner_call else {
            return;
        };

        self.record_call_outcome_if_absent(&owner_call.registration_id, message);
    }

    pub fn format_network_target(protocol: &str, host: &str, port: u16) -> String {
        format!("{protocol}://{host}:{port}")
    }

    pub fn approval_id_for_key(key: &HostApprovalKey) -> String {
        format!(
            "network#{}#{}#{}#{}",
            key.environment_id, key.protocol, key.host, key.port
        )
    }
}

/// Codex's `build_blocked_request_observer`, without its sandbox-violation telemetry.
pub fn build_blocked_request_observer(
    network_approval: Arc<NetworkApprovalService>,
) -> Arc<dyn BlockedRequestObserver> {
    Arc::new(move |blocked: BlockedRequest| {
        let network_approval = Arc::clone(&network_approval);
        async move {
            network_approval.record_blocked_request(blocked).await;
        }
    })
}

/// The turn a network access is reviewed under: what Codex's session reads from its active turn
/// context and its environments.
#[derive(Debug, Clone)]
pub struct NetworkApprovalTurn {
    pub turn_id: String,
    pub environment_id: String,
    pub approval_policy: AskForApproval,
    pub permission_profile: PermissionProfile,
    pub cwd: PathUri,
}

/// What the client is asked about a network access: the fields of Codex's network
/// `ExecApprovalRequest` that its app server sends.
#[derive(Debug, Clone)]
pub struct NetworkApprovalRequest {
    pub item_id: String,
    pub turn_id: String,
    pub environment_id: String,
    pub reason: String,
    pub network_approval_context: NetworkApprovalContext,
    /// The `host:port` target, as the permission-request hooks describe the access.
    pub target: String,
    /// The command the hooks are told asks: the owning call's, or `network-access <target>`.
    pub hook_command: String,
    /// Distinguishes this request's hook runs: the approval id, with the owning call's
    /// registration when there is one.
    pub hook_run_id: String,
}

/// Why no decision came back: Codex's `ToolError` from `Session::request_approval`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkApprovalError {
    /// The review refused the access with this explanation.
    Rejected(String),
    /// The turn was aborted before an answer came.
    TurnAborted,
    /// Asking failed.
    Failed(String),
}

/// What Codex's network approval handler uses its session for.
#[async_trait]
pub trait NetworkApprovalSession: Send + Sync {
    /// The active turn, if any: Codex's `active_turn_context_and_strict_auto_review`.
    fn active_turn(&self) -> Option<NetworkApprovalTurn>;

    /// Asks for a decision about `request`: Codex's `Session::request_approval` with a
    /// `NetworkAccess` action.
    async fn request_network_approval(
        &self,
        request: NetworkApprovalRequest,
    ) -> Result<ReviewDecision, NetworkApprovalError>;

    /// Codex's `Session::persist_network_policy_amendment`.
    async fn persist_network_policy_amendment<'a>(
        &'a self,
        amendment: &'a NetworkPolicyAmendment,
        network_approval_context: &'a NetworkApprovalContext,
        on_policy_applied: Box<dyn FnOnce() + Send + 'a>,
    ) -> anyhow::Result<()>;

    /// Codex's `Session::record_network_policy_amendment_message`: tells the model, without
    /// starting a turn, that the rule was saved for `turn_id`. A session with no way to reach the
    /// conversation records nothing.
    async fn record_network_policy_amendment_message(
        &self,
        turn_id: &str,
        amendment: &NetworkPolicyAmendment,
    ) {
        let _ = (turn_id, amendment);
    }

    /// Codex's `Session::send_event_raw` with a `Warning` event for `turn_id`. A session with no
    /// client to tell sends nothing.
    async fn send_warning(&self, turn_id: &str, message: String) {
        let _ = (turn_id, message);
    }
}

impl NetworkApprovalService {
    /// Registers a call without waiting: Codex's `register_call` holds only a synchronous lock.
    pub fn register_call_now(&self, call: ActiveNetworkApprovalCall) {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls
            .active_calls
            .insert(call.registration_id.clone(), Arc::new(call));
    }

    /// Removes a call without waiting: Codex's `remove_call` holds only a synchronous lock.
    pub fn remove_call_now(&self, registration_id: &str) -> Option<String> {
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.active_calls.shift_remove(registration_id);
        calls.call_outcomes.remove(registration_id)
    }

    // Codex expects this lint under its clippy configuration, which this workspace does not set.
    #[allow(
        clippy::await_holding_invalid_type,
        clippy::too_many_lines,
        reason = "persisted session policy and its in-memory caches must commit atomically"
    )]
    pub async fn handle_inline_policy_request(
        &self,
        session: Arc<dyn NetworkApprovalSession>,
        request: NetworkPolicyRequest,
    ) -> NetworkDecision {
        const REASON_NOT_ALLOWED: &str = "not_allowed";

        let protocol = match request.protocol {
            NetworkProtocol::Http => NetworkApprovalProtocol::Http,
            NetworkProtocol::HttpsConnect => NetworkApprovalProtocol::Https,
            NetworkProtocol::Socks5Tcp => NetworkApprovalProtocol::Socks5Tcp,
            NetworkProtocol::Socks5Udp => NetworkApprovalProtocol::Socks5Udp,
        };
        let Some(NetworkRequestAttribution {
            owner_call,
            environment_id: active_environment_id,
        }) = self.resolve_request_attribution(&request).await
        else {
            return NetworkDecision::deny(REASON_NOT_ALLOWED);
        };
        if owner_call
            .as_ref()
            .is_some_and(|call| call.cancellation_token.is_cancelled())
        {
            return NetworkDecision::deny(REASON_NOT_ALLOWED);
        }
        let active_turn = session.active_turn();
        let Some(environment_id) = active_environment_id
            .or_else(|| active_turn.as_ref().map(|turn| turn.environment_id.clone()))
        else {
            return NetworkDecision::deny(REASON_NOT_ALLOWED);
        };
        let key = HostApprovalKey::from_request(&request, protocol, environment_id.clone());

        {
            let _commit_guard = self.session_policy_commit_lock.lock().await;
            {
                let denied_hosts = self.session_denied_hosts.lock().await;
                if denied_hosts.contains(&key) {
                    return NetworkDecision::deny(REASON_NOT_ALLOWED);
                }
            }

            let approved_hosts = self.session_approved_hosts.lock().await;
            if approved_hosts.contains(&key) {
                return NetworkDecision::Allow;
            }
        }

        let target = Self::format_network_target(key.protocol, request.host.as_str(), key.port);
        let policy_denial_message =
            format!("Network access to \"{target}\" was blocked by policy.");
        let prompt_reason = format!("{} is not in the allowed_domains", request.host);

        let Some(turn) = active_turn else {
            if let Some(owner_call) = owner_call.as_ref() {
                self.record_call_outcome(&owner_call.registration_id, policy_denial_message);
            }
            return NetworkDecision::deny(REASON_NOT_ALLOWED);
        };
        let pending_key = PendingHostApprovalKey {
            host: key.clone(),
            turn_id: owner_call
                .as_ref()
                .map_or_else(|| turn.turn_id.clone(), |call| call.turn_id.clone()),
            execution_id: owner_call
                .as_ref()
                .map(|owner_call| owner_call.registration_id.clone()),
        };
        let (pending, is_owner) = self.get_or_create_pending_approval(pending_key.clone());
        if !is_owner {
            return pending.wait_for_decision().await.to_network_decision();
        }
        let mut pending_owner = PendingHostApprovalOwner::new(
            self,
            pending_key,
            Arc::clone(&pending),
            owner_call
                .as_ref()
                .map(|call| call.cancellation_token.clone()),
        );
        pending_owner.disconnect = request.disconnect.clone();
        pending_owner.cancellation = request.cancellation.clone();

        // Review under current settings, but an owned process keeps the profile it started with.
        let permission_profile = owner_call
            .as_ref()
            .map_or(&turn.permission_profile, |call| &call.permission_profile);
        if !permission_profile_allows_network_approval_flow(permission_profile) {
            if let Some(owner_call) = owner_call.as_ref() {
                self.record_call_outcome(&owner_call.registration_id, policy_denial_message);
            }
            pending_owner.complete(PendingApprovalDecision::Deny);
            return NetworkDecision::deny(REASON_NOT_ALLOWED);
        }
        if !allows_network_approval_flow(turn.approval_policy) {
            if let Some(owner_call) = owner_call.as_ref() {
                self.record_call_outcome(&owner_call.registration_id, policy_denial_message);
            }
            pending_owner.complete(PendingApprovalDecision::Deny);
            return NetworkDecision::deny(REASON_NOT_ALLOWED);
        }

        let network_approval_context = NetworkApprovalContext::new(request.host.clone(), protocol);
        let approval_id = Self::approval_id_for_key(&key);
        let approval_call_id = format!("{approval_id}#{}", uuid::Uuid::new_v4());
        let hook_run_id = owner_call.as_ref().map_or_else(
            || approval_id.clone(),
            |call| format!("{approval_id}#{}", call.registration_id),
        );
        let hook_command = owner_call.as_ref().map_or_else(
            || format!("network-access {target}"),
            |call| call.command.clone(),
        );
        let approval = session.request_network_approval(NetworkApprovalRequest {
            item_id: approval_call_id,
            target: target.clone(),
            hook_command,
            hook_run_id,
            turn_id: turn.turn_id.clone(),
            environment_id,
            reason: prompt_reason,
            network_approval_context: network_approval_context.clone(),
        });
        let approval_result = if let Some(owner_call) = &owner_call {
            // A ready review result takes precedence over concurrent cancellation.
            let Some(result) = owner_call
                .cancellation_token
                .run_until_cancelled(approval)
                .await
            else {
                pending_owner.complete(PendingApprovalDecision::Deny);
                return NetworkDecision::deny(REASON_NOT_ALLOWED);
            };
            result
        } else {
            approval.await
        };
        let approval_decision = match approval_result {
            Ok(decision) => decision,
            Err(NetworkApprovalError::Rejected(rejection)) => {
                if let Some(owner_call) = owner_call.as_ref() {
                    self.record_call_outcome(&owner_call.registration_id, rejection);
                }
                pending_owner.complete(PendingApprovalDecision::Deny);
                return NetworkDecision::deny(REASON_NOT_ALLOWED);
            }
            Err(error) => {
                if let Some(owner_call) = owner_call.as_ref() {
                    let rejection = match error {
                        NetworkApprovalError::TurnAborted => "rejected by user".to_string(),
                        NetworkApprovalError::Failed(err) => {
                            format!("Error while requesting approval: {err}")
                        }
                        NetworkApprovalError::Rejected(rejection) => rejection,
                    };
                    self.record_call_outcome(&owner_call.registration_id, rejection);
                }
                pending_owner.complete(PendingApprovalDecision::Deny);
                return NetworkDecision::deny(REASON_NOT_ALLOWED);
            }
        };

        if matches!(
            &approval_decision,
            ReviewDecision::NetworkPolicyAmendment { network_policy_amendment }
                if network_policy_amendment.action() == NetworkPolicyRuleAction::Deny
        ) && let Some(owner_call) = owner_call.as_ref()
        {
            // Preserve the denial before waiting for the policy commit. Defer
            // cancellation until after saving so it cannot interrupt persistence.
            let _ = self
                .store_call_outcome(&owner_call.registration_id, "rejected by user".to_string());
        }

        let _session_policy_commit_guard = if matches!(
            &approval_decision,
            ReviewDecision::Approved
                | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                | ReviewDecision::ApprovedForSession
                | ReviewDecision::NetworkPolicyAmendment { .. }
        ) {
            Some(self.session_policy_commit_lock.lock().await)
        } else {
            None
        };
        let resolved = match approval_decision {
            ReviewDecision::Approved | ReviewDecision::ApprovedExecpolicyAmendment { .. } => {
                if self.session_denied_hosts.lock().await.contains(&key) {
                    if let Some(owner_call) = owner_call.as_ref() {
                        self.record_call_outcome(
                            &owner_call.registration_id,
                            policy_denial_message.clone(),
                        );
                    }
                    PendingApprovalDecision::Deny
                } else {
                    PendingApprovalDecision::AllowOnce
                }
            }
            ReviewDecision::ApprovedForSession => {
                if self.session_denied_hosts.lock().await.contains(&key) {
                    if let Some(owner_call) = owner_call.as_ref() {
                        self.record_call_outcome(
                            &owner_call.registration_id,
                            policy_denial_message.clone(),
                        );
                    }
                    PendingApprovalDecision::Deny
                } else {
                    self.session_approved_hosts.lock().await.insert(key.clone());
                    PendingApprovalDecision::AllowForSession
                }
            }
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } => {
                if network_policy_amendment.action() == NetworkPolicyRuleAction::Allow {
                    match session
                        .persist_network_policy_amendment(
                            &network_policy_amendment,
                            &network_approval_context,
                            Box::new(|| {
                                pending_owner
                                    .set_decision_on_drop(PendingApprovalDecision::AllowForSession);
                            }),
                        )
                        .await
                    {
                        Ok(()) => {
                            session
                                .record_network_policy_amendment_message(
                                    &turn.turn_id,
                                    &network_policy_amendment,
                                )
                                .await;
                        }
                        Err(err) => {
                            let message =
                                format!("Failed to apply network policy amendment: {err}");
                            warn!("{message}");
                            session.send_warning(&turn.turn_id, message).await;
                        }
                    }
                    if pending_owner.decision_on_drop == PendingApprovalDecision::AllowForSession {
                        {
                            let mut denied_hosts = self.session_denied_hosts.lock().await;
                            denied_hosts.remove(&key);
                        }
                        self.session_approved_hosts.lock().await.insert(key.clone());
                        PendingApprovalDecision::AllowForSession
                    } else {
                        if let Some(owner_call) = owner_call.as_ref() {
                            self.record_call_outcome(
                                &owner_call.registration_id,
                                policy_denial_message.clone(),
                            );
                        }
                        PendingApprovalDecision::Deny
                    }
                } else {
                    match session
                        .persist_network_policy_amendment(
                            &network_policy_amendment,
                            &network_approval_context,
                            Box::new(|| {
                                pending_owner.set_decision_on_drop(PendingApprovalDecision::Deny);
                            }),
                        )
                        .await
                    {
                        Ok(()) => {
                            session
                                .record_network_policy_amendment_message(
                                    &turn.turn_id,
                                    &network_policy_amendment,
                                )
                                .await;
                        }
                        Err(err) => {
                            let message =
                                format!("Failed to apply network policy amendment: {err}");
                            warn!("{message}");
                            session.send_warning(&turn.turn_id, message).await;
                        }
                    }
                    if let Some(owner_call) = owner_call.as_ref() {
                        self.record_call_outcome(
                            &owner_call.registration_id,
                            "rejected by user".to_string(),
                        );
                    }
                    {
                        let mut approved_hosts = self.session_approved_hosts.lock().await;
                        approved_hosts.remove(&key);
                    }
                    self.session_denied_hosts.lock().await.insert(key.clone());
                    PendingApprovalDecision::Deny
                }
            }
            _ => {
                error!("centralized network approval returned an invalid decision");
                if let Some(owner_call) = owner_call.as_ref() {
                    self.record_call_outcome(
                        &owner_call.registration_id,
                        "Error while requesting approval".to_string(),
                    );
                }
                PendingApprovalDecision::Deny
            }
        };
        pending_owner.set_decision_on_drop(resolved);
        pending_owner.complete(resolved);

        resolved.to_network_decision()
    }
}

/// Codex's `build_network_policy_decider`: answers the proxy's allowlist misses through the
/// session while it lives, and asks again once it is gone.
pub fn build_network_policy_decider(
    network_approval: Arc<NetworkApprovalService>,
    session: std::sync::Weak<dyn NetworkApprovalSession>,
) -> Arc<dyn NetworkPolicyDecider> {
    Arc::new(move |request: NetworkPolicyRequest| {
        let network_approval = Arc::clone(&network_approval);
        let session = session.clone();
        async move {
            let Some(session) = session.upgrade() else {
                return NetworkDecision::ask("not_allowed");
            };
            network_approval
                .handle_inline_policy_request(session, request)
                .await
        }
    })
}

/// The call a command runs as: what Codex's `NetworkApprovalSpec` carries into
/// `begin_network_approval` for a local command.
#[derive(Debug, Clone)]
pub struct NetworkApprovalCallSpec {
    pub trigger: NetworkAccessTrigger,
    pub tool_name: ToolLookupKey,
    pub command: String,
    pub turn_id: String,
    pub environment_id: String,
    pub permission_profile: PermissionProfile,
}

/// Codex's `begin_network_approval` for a command on the thread's own proxy: scopes the proxy to
/// this execution so its requests are attributed to the call, and registers the call so a review
/// can stop it.
///
/// # Errors
///
/// Fails when the proxy cannot be scoped to the execution.
pub fn begin_network_approval(
    service: &NetworkApprovalService,
    network: &NetworkProxy,
    spec: NetworkApprovalCallSpec,
) -> anyhow::Result<ActiveNetworkApproval> {
    let NetworkApprovalCallSpec {
        trigger,
        tool_name,
        command,
        turn_id,
        environment_id,
        permission_profile,
    } = spec;
    let registration_id = uuid::Uuid::new_v4().to_string();
    let attribution_token = uuid::Uuid::new_v4().to_string();
    let execution_proxy = network
        .for_execution(
            &environment_id,
            &registration_id,
            attribution_token,
            /*environment_policy*/ None,
            /*fallback_policy_decider*/ None,
        )
        .map_err(|err| anyhow::anyhow!("failed to create execution-scoped network proxy: {err}"))?;
    let cancellation_token = CancellationToken::new();
    service.register_call_now(ActiveNetworkApprovalCall {
        registration_id: registration_id.clone(),
        turn_id,
        trigger,
        tool_name,
        command,
        environment_id,
        permission_profile,
        cancellation_token: cancellation_token.clone(),
    });

    Ok(ActiveNetworkApproval {
        registration_id: Some(registration_id),
        cancellation_token,
        execution_proxy,
    })
}

const LATE_NETWORK_DENIAL_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

/// A command's deferred network approval, finished once the command has ended: Codex's unified
/// exec `finish_deferred_network_approval_after_process_exit`. Finishing again reports the
/// outcome the first finish took, as [`DeferredNetworkApproval::finish`] caches it; the exec tools
/// supply Codex's default denial message.
pub struct CommandNetworkApproval {
    service: Arc<NetworkApprovalService>,
    deferred: DeferredNetworkApproval,
    finished: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for CommandNetworkApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandNetworkApproval")
            .field("registration_id", &self.deferred.registration_id())
            .finish_non_exhaustive()
    }
}

impl CommandNetworkApproval {
    #[must_use]
    pub fn new(service: Arc<NetworkApprovalService>, deferred: DeferredNetworkApproval) -> Self {
        Self {
            service,
            deferred,
            finished: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Codex's `wait_for_late_network_denial`: a denial racing the command's exit still counts.
    async fn wait_for_late_network_denial(&self) {
        let network_cancelled = self.deferred.cancellation_token();
        if network_cancelled.is_cancelled() {
            return;
        }
        tokio::select! {
            () = network_cancelled.cancelled() => {}
            () = tokio::time::sleep(LATE_NETWORK_DENIAL_GRACE_PERIOD) => {}
        }
    }
}

#[async_trait]
impl ra_tools::exec_command::ExecNetworkApproval for CommandNetworkApproval {
    async fn finish_after_exit(&self) -> Result<(), String> {
        self.wait_for_late_network_denial().await;
        self.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.deferred.finish(&self.service).await
    }

    fn is_denied(&self) -> bool {
        self.deferred.is_cancelled()
    }
}

impl Drop for CommandNetworkApproval {
    fn drop(&mut self) {
        // A command that never ran, or whose result was refused before it ended, still releases
        // its registration: Codex finishes the approval of a failed attempt the same way.
        if !self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            self.service
                .remove_call_now(self.deferred.registration_id());
        }
    }
}

/// A deferred approval's constructor for the tests: Codex builds it through `into_deferred`.
#[cfg(feature = "test-api")]
#[must_use]
pub fn deferred_for_test(
    registration_id: impl Into<String>,
    cancellation_token: CancellationToken,
) -> DeferredNetworkApproval {
    DeferredNetworkApproval {
        registration_id: registration_id.into(),
        cancellation_token,
        finish_outcome: Arc::new(OnceCell::new()),
        _execution_proxy: None,
    }
}
