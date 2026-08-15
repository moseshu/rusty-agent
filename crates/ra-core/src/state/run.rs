//! The run's own resumable state (the skeleton a future migration grows into the full checkpoint).
//!
//! A run has facts that are neither agent configuration nor session history: tool-use accounting,
//! budget counters, event sequences, and state owned by the loop itself. Keeping those values as
//! independent fields on the runner would make a new fact a signature change across every entry
//! point and would let a continuation accidentally carry one fact but not another. `RunState` is
//! the single carrier that crosses a run-segment boundary.
//!
//! It deliberately belongs to `ra-core`: a future migration grows this same value into the full
//! serializable run state (generated items, model responses, pending approvals, guardrail
//! results), and a persisted wire type cannot live in `ra-runtime` without reversing the
//! dependency direction.
//!
//! # Not to be confused with `WorkState`
//!
//! This is a **single run's** recoverable state. A future cross-run `WorkState`, reached through
//! [`WorkStateHandle`](crate::state::work::WorkStateHandle), is the **cross-run, cross-node task**
//! state — owned by whoever spans those runs, not by this value. Mixing them is forbidden, and the
//! reason is concrete: a checkpoint of this value is scoped to one run's resume, while task state
//! outlives every run that touches it. Folding the second into the first would make a resumed run
//! restore a stale copy of state another node has since advanced.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    budget::BudgetSnapshot,
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
    finish::FinishReason,
    item::CallId,
    state::{ToolFailureTracker, ToolUseTracker},
    usage::Usage,
};

/// Current [`RunState`] schema version.
pub const RUN_STATE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Stable identity of one run, across every segment it is resumed in.
///
/// It lives beside the recoverable state rather than beside the live
/// [`RunContext`](crate::context::RunContext) on purpose. Persisted events, stored items and replay
/// all attribute by this ID, so the run's identity has to be a value that survives a checkpoint;
/// the live context only ever shows a read view of it.
///
/// # Where an ID comes from
///
/// From whoever starts the run, as an explicit construction argument. [`Self::generate`] mints one,
/// but it has to be *called* — there is deliberately no `Default` and no minting inside another
/// constructor, because an ID a value type produces on its own reappears in two places that must
/// not have it: a `Default` that quietly acquires an identity, and a deserialization that hands a
/// restored run a brand new one and detaches it from everything already written under the old.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Creates an ID from a host-generated string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Mints a fresh random ID for a run that is starting now.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// String representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for RunId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Thread-safe sequence allocator for host events within a single run.
#[derive(Debug, Clone)]
pub struct EventSeqAllocator {
    run_id: RunId,
    next: Arc<AtomicU64>,
}

impl EventSeqAllocator {
    /// Creates an allocator for a run starting at the given sequence number.
    #[must_use]
    pub(crate) fn new(run_id: RunId, start: u64) -> Self {
        Self {
            run_id,
            next: Arc::new(AtomicU64::new(start)),
        }
    }

    /// Identity of the run this allocator belongs to.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Allocates the next candidate sequence number.
    ///
    /// This is the one place exhaustion is reported. A run that has issued every number a `u64` can
    /// hold must stop rather than wrap, because a wrapped number is indistinguishable from one this
    /// run already wrote, and every consumer of the sequence — replay, sorting, dedup — reads it as
    /// an identity.
    pub fn allocate(&self) -> Result<u64> {
        self.next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |val| {
                val.checked_add(1)
            })
            .map_err(|_| Error::caller("host event sequence numbers exhausted"))
    }

    /// Returns the exclusive upper bound (next candidate sequence number) as of now.
    #[must_use]
    pub fn current_next(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// Initializes a restored allocator from a checkpointed next sequence and an optional persisted
    /// maximum sequence.
    ///
    /// The restored next sequence is `max(checkpoint_next, persisted_run_max_seq + 1)`. The second
    /// argument is `None` only when there is no event log to reconcile against; passing it is how a
    /// caller states which of the two situations it is in, so that a resume cannot silently keep
    /// the checkpoint's bound while the log has already gone further.
    ///
    /// Saturating at [`u64::MAX`] rather than failing here is deliberate: a run that far along is
    /// exhausted whichever door it arrives through, and [`Self::allocate`] is the door that has to
    /// say so. Reporting it twice would put a fallible constructor on the resume path for a
    /// condition it cannot act on differently.
    #[must_use]
    pub(crate) fn restore(
        run_id: RunId,
        checkpoint_next: u64,
        persisted_run_max_seq: Option<u64>,
    ) -> Self {
        let candidate = persisted_run_max_seq.map_or(checkpoint_next, |max_seq| {
            checkpoint_next.max(max_seq.saturating_add(1))
        });
        Self::new(run_id, candidate)
    }
}

/// Reference to a nested child run spawned as a tool execution.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedRunRef {
    scope_id: String,
    call_id: CallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl NestedRunRef {
    /// Creates a reference to a nested run.
    #[must_use]
    pub fn new(scope_id: impl Into<String>, call_id: CallId) -> Self {
        Self {
            scope_id: scope_id.into(),
            call_id,
            signature: None,
            unknown: Unknown::new(),
        }
    }

    /// Attaches an optional execution signature.
    #[must_use]
    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    /// Scope identifier of the parent-child delegation.
    #[must_use]
    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }

    /// Tool call ID that initiated this child run.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Signature of the child run execution when present.
    #[must_use]
    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Persistent reference to a workspace lease held by the run.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLeaseRef {
    lease_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl WorkspaceLeaseRef {
    /// Creates a workspace lease reference.
    #[must_use]
    pub fn new(lease_id: impl Into<String>) -> Self {
        Self {
            lease_id: lease_id.into(),
            path: None,
            unknown: Unknown::new(),
        }
    }

    /// Attaches an optional target workspace path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Identifier of the lease.
    #[must_use]
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    /// Target path of the workspace lease when specified.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Persistent reference to a cross-run task state.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStateRef {
    task_id: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl WorkStateRef {
    /// Creates a cross-run work state reference.
    #[must_use]
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            unknown: Unknown::new(),
        }
    }

    /// Identifier of the cross-run task.
    #[must_use]
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Persistent cursor within an execution graph topology.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCursor {
    node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_id: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl GraphCursor {
    /// Creates a graph cursor at the specified node.
    #[must_use]
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            edge_id: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the active edge identifier.
    #[must_use]
    pub fn with_edge_id(mut self, edge_id: impl Into<String>) -> Self {
        self.edge_id = Some(edge_id.into());
        self
    }

    /// Active node identifier.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Active edge identifier when traversing an edge.
    #[must_use]
    pub fn edge_id(&self) -> Option<&str> {
        self.edge_id.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Persistent representation of a pending approval or control request.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingControlRequest {
    request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call_id: Option<CallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl PendingControlRequest {
    /// Creates a pending control request.
    #[must_use]
    pub fn new(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            call_id: None,
            description: None,
            unknown: Unknown::new(),
        }
    }

    /// Attaches the associated tool call ID.
    #[must_use]
    pub fn with_call_id(mut self, call_id: CallId) -> Self {
        self.call_id = Some(call_id);
        self
    }

    /// Attaches a human-readable description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Request identifier.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Tool call ID associated with this control request, if any.
    #[must_use]
    pub const fn call_id(&self) -> Option<&CallId> {
        self.call_id.as_ref()
    }

    /// Description of the pending request when available.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// All mutable, framework-owned facts one run carries between its segments.
///
/// This is not host application context. Host context is an arbitrary live object read through
/// [`RunContext::app_context`](crate::context::RunContext::app_context), while this value is safe
/// to checkpoint and restore. Run identity and event sequence upper bound are required explicit
/// fields, while optional framework-owned extensions are populated with serde defaults; callers
/// pass the complete value through the runner's request API so a resumed run cannot accidentally
/// reset part of its accounting.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    #[serde(default = "run_state_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    next_host_event_seq: u64,
    #[serde(default)]
    tool_use: ToolUseTracker,
    #[serde(default)]
    tool_failure: ToolFailureTracker,
    #[serde(default)]
    budget: BudgetSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nested_runs: Vec<NestedRunRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_lease: Option<WorkspaceLeaseRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work_state_ref: Option<WorkStateRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graph_cursor: Option<GraphCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage_totals: Option<Usage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_control_requests: Vec<PendingControlRequest>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RunState {
    /// Creates empty state for a new run with the specified run identity.
    #[must_use]
    pub fn start(run_id: RunId) -> Self {
        Self {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            run_id,
            next_host_event_seq: 0,
            tool_use: ToolUseTracker::new(),
            tool_failure: ToolFailureTracker::new(),
            budget: BudgetSnapshot::new(),
            finish_reason: None,
            nested_runs: Vec::new(),
            workspace_lease: None,
            work_state_ref: None,
            graph_cursor: None,
            usage_totals: None,
            pending_control_requests: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version of this value.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Stable identity of the run this state belongs to.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Next candidate host event sequence number (exclusive upper bound).
    #[must_use]
    pub const fn next_host_event_seq(&self) -> u64 {
        self.next_host_event_seq
    }

    /// Raises the next host event sequence number without allowing it to regress.
    ///
    /// A checkpointed sequence bound is a promise that lower values will never be issued again.
    /// Taking the maximum preserves that promise when a caller combines state from different
    /// persistence points.
    #[must_use]
    pub fn with_next_host_event_seq(mut self, seq: u64) -> Self {
        self.next_host_event_seq = self.next_host_event_seq.max(seq);
        self
    }

    /// Builds this run's event sequence allocator, reconciled against an optional persisted maximum.
    ///
    /// This is the only way to obtain an allocator bound to this state, which is what keeps the two
    /// carriers of the run's identity from drifting apart. `persisted_run_max_seq` is the sidecar
    /// value a rollout writer maintains; `None` states that there is no log to reconcile against,
    /// rather than leaving the choice to a default that would quietly keep a bound the log has
    /// already passed.
    #[must_use]
    pub fn restore_event_seq_allocator(
        &self,
        persisted_run_max_seq: Option<u64>,
    ) -> EventSeqAllocator {
        EventSeqAllocator::restore(
            self.run_id.clone(),
            self.next_host_event_seq,
            persisted_run_max_seq,
        )
    }

    /// Snapshots the allocator's current bound into the checkpoint.
    ///
    /// Takes the maximum rather than assigning: the bound is what resume promises never to hand out
    /// again, so moving it backwards — from a stale allocator, or from two snapshots landing out of
    /// order — would re-issue numbers this run has already written under.
    ///
    /// # Panics
    ///
    /// In debug builds, if the allocator belongs to a different run. The allocator can only be
    /// built by [`Self::restore_event_seq_allocator`], so a mismatch is a wiring bug rather than a
    /// condition a caller can recover from — and the alternative, skipping the snapshot, leaves a
    /// checkpoint whose bound never advances, which is the duplicate-sequence failure this method
    /// exists to prevent.
    pub fn snapshot_event_seq(&mut self, allocator: &EventSeqAllocator) {
        debug_assert_eq!(
            &self.run_id,
            allocator.run_id(),
            "event sequence allocator belongs to a different run"
        );
        if self.run_id == *allocator.run_id() {
            self.next_host_event_seq = self.next_host_event_seq.max(allocator.current_next());
        }
    }

    /// Tool-use history as of the most recently settled turn.
    #[must_use]
    pub const fn tool_use(&self) -> &ToolUseTracker {
        &self.tool_use
    }

    /// Replaces the carried tool-use history, leaving every other field alone.
    #[must_use]
    pub fn with_tool_use(mut self, tool_use: ToolUseTracker) -> Self {
        self.tool_use = tool_use;
        self
    }

    /// Mutable tool-use history for the loop's settlement path.
    #[doc(hidden)]
    pub fn tool_use_mut(&mut self) -> &mut ToolUseTracker {
        &mut self.tool_use
    }

    /// Failure history as of the most recently settled turn.
    #[must_use]
    pub const fn tool_failure(&self) -> &ToolFailureTracker {
        &self.tool_failure
    }

    /// Replaces the carried failure history, leaving every other field alone.
    #[must_use]
    pub fn with_tool_failure(mut self, tool_failure: ToolFailureTracker) -> Self {
        self.tool_failure = tool_failure;
        self
    }

    /// Both trackers at once, for the settlement path that records into each.
    #[doc(hidden)]
    pub fn trackers_mut(&mut self) -> (&mut ToolUseTracker, &mut ToolFailureTracker) {
        (&mut self.tool_use, &mut self.tool_failure)
    }

    /// Recoverable budget accounting as of the most recently completed operation.
    #[must_use]
    pub const fn budget(&self) -> &BudgetSnapshot {
        &self.budget
    }

    /// Replaces budget accounting while preserving all other run state.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetSnapshot) -> Self {
        self.budget = budget;
        self
    }

    /// Mutable budget accounting for the runner.
    #[doc(hidden)]
    pub fn budget_mut(&mut self) -> &mut BudgetSnapshot {
        &mut self.budget
    }

    /// Structured finish reason when the run settled with one.
    #[must_use]
    pub const fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }

    /// Replaces the recorded finish reason.
    #[must_use]
    pub fn with_finish_reason(mut self, finish_reason: impl Into<Option<FinishReason>>) -> Self {
        self.finish_reason = finish_reason.into();
        self
    }

    /// Nested child runs spawned during this run.
    #[must_use]
    pub fn nested_runs(&self) -> &[NestedRunRef] {
        &self.nested_runs
    }

    /// Replaces the list of nested child runs.
    #[must_use]
    pub fn with_nested_runs(mut self, nested_runs: Vec<NestedRunRef>) -> Self {
        self.nested_runs = nested_runs;
        self
    }

    /// Workspace lease reference held by this run, if any.
    #[must_use]
    pub const fn workspace_lease(&self) -> Option<&WorkspaceLeaseRef> {
        self.workspace_lease.as_ref()
    }

    /// Replaces the workspace lease reference.
    #[must_use]
    pub fn with_workspace_lease(
        mut self,
        workspace_lease: impl Into<Option<WorkspaceLeaseRef>>,
    ) -> Self {
        self.workspace_lease = workspace_lease.into();
        self
    }

    /// Task state reference spanning across runs, if any.
    #[must_use]
    pub const fn work_state_ref(&self) -> Option<&WorkStateRef> {
        self.work_state_ref.as_ref()
    }

    /// Replaces the task state reference.
    #[must_use]
    pub fn with_work_state_ref(mut self, work_state_ref: impl Into<Option<WorkStateRef>>) -> Self {
        self.work_state_ref = work_state_ref.into();
        self
    }

    /// Execution graph cursor, if executing within a graph.
    #[must_use]
    pub const fn graph_cursor(&self) -> Option<&GraphCursor> {
        self.graph_cursor.as_ref()
    }

    /// Replaces the execution graph cursor.
    #[must_use]
    pub fn with_graph_cursor(mut self, graph_cursor: impl Into<Option<GraphCursor>>) -> Self {
        self.graph_cursor = graph_cursor.into();
        self
    }

    /// Accumulated usage totals, projected from settled turn responses.
    #[must_use]
    pub const fn usage_totals(&self) -> Option<&Usage> {
        self.usage_totals.as_ref()
    }

    /// Replaces the accumulated usage totals.
    #[must_use]
    pub fn with_usage_totals(mut self, usage_totals: impl Into<Option<Usage>>) -> Self {
        self.usage_totals = usage_totals.into();
        self
    }

    /// Pending control or approval requests.
    #[must_use]
    pub fn pending_control_requests(&self) -> &[PendingControlRequest] {
        &self.pending_control_requests
    }

    /// Replaces the list of pending control requests.
    #[must_use]
    pub fn with_pending_control_requests(
        mut self,
        pending_control_requests: Vec<PendingControlRequest>,
    ) -> Self {
        self.pending_control_requests = pending_control_requests;
        self
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

const fn run_state_schema_version() -> SchemaVersion {
    RUN_STATE_SCHEMA_VERSION
}
