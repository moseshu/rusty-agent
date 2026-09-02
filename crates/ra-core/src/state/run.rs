//! The run's own resumable state (the skeleton a future migration grows into the full checkpoint).
//!
//! A run has facts that are neither agent configuration nor session history: tool-use accounting,
//! turn and token spend, event sequences, and state owned by the loop itself. Keeping those values as
//! independent fields on the runner would make a new fact a signature change across every entry
//! point and would let a continuation accidentally carry one fact but not another. `RunState` is
//! the single carrier that crosses a run-segment boundary.
//!
//! It deliberately belongs to `ra-core`: it already carries the run's own history — generated
//! items, model responses, pending approvals — and guardrail results join them when they land, and
//! a persisted wire type cannot live in `ra-runtime` without reversing the dependency direction.
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
    budget::{BudgetLimit, BudgetSnapshot},
    cancel::Deadline,
    compat::{SchemaVersion, Unknown},
    error::{BudgetKind, Error, Result},
    finish::FinishReason,
    item::{
        AgentId, CallId, ItemId, ModelInputItem, ModelResponse, RunItem, RunItemKind, ToolApproval,
    },
    permission::{PermissionDecision, PermissionRule},
    state::{ToolFailureTracker, ToolOutputReferenceTracker, ToolUseTracker},
    usage::Usage,
};

/// Current [`RunState`] schema version.
pub const RUN_STATE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(3);

/// Human-readable summaries of every run-state wire version this build understands.
///
/// The list stays beside the version number. A checkpoint is a long-lived wire contract, so a
/// version bump without a short statement of what changed leaves a future reader unable to tell
/// whether an older runtime may safely resume it.
pub const RUN_STATE_SCHEMA_VERSION_SUMMARIES: &[(SchemaVersion, &str)] = &[
    (
        SchemaVersion::new(1),
        "Initial resumable run identity, accounting, history, and interruption records.",
    ),
    (
        SchemaVersion::new(2),
        "Persisted host approval answers, exact routing identities, and session permission rules.",
    ),
    (
        RUN_STATE_SCHEMA_VERSION,
        "Persisted tool-output reference retention facts for context projections across resumes.",
    ),
];

const fn default_input_history_is_complete() -> bool {
    true
}

/// A host answer retained until the runtime has turned the interrupted action into history.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionResolution {
    /// Execute the action the host approved.
    Approve {
        /// Whether to retain an exact allow rule for later calls.
        always: bool,
    },
    /// Do not execute the action and return a refusal to the model.
    Reject {
        /// Whether to retain an exact deny rule for later calls.
        always: bool,
    },
}

impl InterruptionResolution {
    /// Whether this answer should be retained as an exact permission rule.
    #[must_use]
    pub const fn always(self) -> bool {
        match self {
            Self::Approve { always } | Self::Reject { always } => always,
        }
    }
}

/// One pending interruption together with the host answer it received.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInterruptionResolution {
    item_id: ItemId,
    resolution: InterruptionResolution,
}

impl PendingInterruptionResolution {
    /// ID of the authoritative interruption record being answered.
    #[must_use]
    pub const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// Host answer awaiting runtime settlement.
    #[must_use]
    pub const fn resolution(&self) -> InterruptionResolution {
        self.resolution
    }
}

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
///
/// # Reading one written by an older build
///
/// Deserialization goes through [`RunStateRecord`], which exists so that spend recorded under an
/// earlier layout still counts. That is the whole of the migration, and it is on the way in rather
/// than at a call site, because a resumed run that has to remember to migrate is a resumed run that
/// silently gets its allowance back the day someone forgets.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RunStateRecord")]
pub struct RunState {
    #[serde(default = "run_state_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    next_host_event_seq: u64,
    #[serde(default)]
    tool_use: ToolUseTracker,
    #[serde(default)]
    tool_failure: ToolFailureTracker,
    tool_output_references: ToolOutputReferenceTracker,
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
    #[serde(default)]
    usage_totals: Usage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_control_requests: Vec<PendingControlRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    starting_agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    original_input: Vec<ModelInputItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    generated_items: Vec<RunItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_responses: Vec<ModelResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_interruptions: Vec<ItemId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_interruption_resolutions: Vec<PendingInterruptionResolution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permission_rules: Vec<PermissionRule>,
    #[serde(default = "default_input_history_is_complete")]
    input_history_is_complete: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

// `RunState` was publicly `Eq` before it gained the serializable response projection. The values
// accepted by this state are JSON-shaped and therefore have reflexive equality; keeping the
// implementation preserves the public trait contract without exposing a second state type.
impl Eq for RunState {}

/// What a checkpoint literally contains, before this build's invariants are applied to it.
///
/// **It mirrors [`RunState`] field for field** and exists only to give deserialization a place to
/// run afterwards. A field added to one and not the other is caught by the round-trip test that
/// populates every slot: the missing field comes back defaulted and the comparison fails.
///
/// It does two things. It **migrates**: token spend an older layout kept on the budget snapshot
/// moves into the usage ledger, which is where every ceiling is now measured. Without that, a
/// continuation from such a checkpoint starts its token accounting at zero — spend already paid
/// for, invisible, and a budget that stops the run at twice what it was given.
///
/// And it **refuses**. A checkpoint is the one input to this framework that arrives from outside
/// the process that wrote it: a disk, an older build, an editor. Every invariant the run relies on
/// afterwards is therefore checked here rather than assumed, because the alternative is not a
/// crash — it is a run that resumes as something quietly different from what was paused.
#[derive(Deserialize)]
struct RunStateRecord {
    #[serde(default = "run_state_schema_version")]
    schema_version: SchemaVersion,
    run_id: RunId,
    next_host_event_seq: u64,
    #[serde(default)]
    tool_use: ToolUseTracker,
    #[serde(default)]
    tool_failure: ToolFailureTracker,
    #[serde(default)]
    tool_output_references: Option<ToolOutputReferenceTracker>,
    #[serde(default)]
    budget: BudgetSnapshot,
    #[serde(default)]
    finish_reason: Option<FinishReason>,
    #[serde(default)]
    nested_runs: Vec<NestedRunRef>,
    #[serde(default)]
    workspace_lease: Option<WorkspaceLeaseRef>,
    #[serde(default)]
    work_state_ref: Option<WorkStateRef>,
    #[serde(default)]
    graph_cursor: Option<GraphCursor>,
    #[serde(default)]
    usage_totals: Usage,
    #[serde(default)]
    pending_control_requests: Vec<PendingControlRequest>,
    #[serde(default)]
    starting_agent: Option<AgentId>,
    #[serde(default)]
    current_agent: Option<AgentId>,
    #[serde(default)]
    original_input: Vec<ModelInputItem>,
    #[serde(default)]
    generated_items: Vec<RunItem>,
    #[serde(default)]
    model_responses: Vec<ModelResponse>,
    #[serde(default)]
    pending_interruptions: Vec<ItemId>,
    #[serde(default)]
    pending_interruption_resolutions: Vec<PendingInterruptionResolution>,
    #[serde(default)]
    permission_rules: Vec<PermissionRule>,
    #[serde(default = "default_input_history_is_complete")]
    input_history_is_complete: bool,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl TryFrom<RunStateRecord> for RunState {
    type Error = Error;

    fn try_from(record: RunStateRecord) -> std::result::Result<Self, Self::Error> {
        let RunStateRecord {
            schema_version,
            run_id,
            next_host_event_seq,
            tool_use,
            tool_failure,
            tool_output_references,
            mut budget,
            finish_reason,
            nested_runs,
            workspace_lease,
            work_state_ref,
            graph_cursor,
            mut usage_totals,
            pending_control_requests,
            starting_agent,
            current_agent,
            original_input,
            generated_items,
            model_responses,
            pending_interruptions,
            pending_interruption_resolutions,
            permission_rules,
            input_history_is_complete,
            unknown,
        } = record;

        let schema_version = if schema_version <= RUN_STATE_SCHEMA_VERSION {
            RUN_STATE_SCHEMA_VERSION
        } else {
            schema_version
        };
        let tool_output_references = tool_output_references
            .unwrap_or_else(|| ToolOutputReferenceTracker::new(run_id.clone()));
        if tool_output_references.run_id() != &run_id {
            return Err(Error::caller(format!(
                "run state for `{run_id}` carries tool-output references for `{}`",
                tool_output_references.run_id()
            )));
        }

        // Carried in as a total with no split, because that is all the older record said. It counts
        // against the token ceiling and stays out of the input and output counters that cache
        // metrics divide by.
        let carried = budget.take_legacy_tokens_used();
        if carried > 0 {
            usage_totals = usage_totals.accumulate(&Usage::from_carried_total(carried));
        }

        if current_agent.is_some() && starting_agent.is_none() {
            return Err(Error::caller(
                "run state has a current agent but no starting agent",
            ));
        }
        // The dangerous direction is this one, not the one above. A checkpoint that has history but
        // names nobody looks exactly like a run that never started, so [`RunState::begin_segment`]
        // would take it for a first segment and overwrite `original_input` with whatever the
        // resumed request happened to carry — while keeping the records generated against the input
        // it just replaced. The transcript that comes out of that is spliced, and nothing after it
        // can tell.
        if current_agent.is_none()
            && !(original_input.is_empty()
                && generated_items.is_empty()
                && model_responses.is_empty())
        {
            return Err(Error::caller(
                "run state carries history but names no current agent, so nothing can say which \
                 declaration is entitled to resume it",
            ));
        }
        validate_pending_interruptions(&pending_interruptions, &generated_items)?;
        validate_interruption_resolutions(
            &pending_interruption_resolutions,
            &pending_interruptions,
        )?;

        Ok(Self {
            schema_version,
            run_id,
            next_host_event_seq,
            tool_use,
            tool_failure,
            tool_output_references,
            budget,
            finish_reason,
            nested_runs,
            workspace_lease,
            work_state_ref,
            graph_cursor,
            usage_totals,
            pending_control_requests,
            starting_agent,
            current_agent,
            original_input,
            generated_items,
            model_responses,
            pending_interruptions,
            pending_interruption_resolutions,
            permission_rules,
            input_history_is_complete,
            unknown,
        })
    }
}

impl RunState {
    /// Creates empty state for a new run with the specified run identity.
    #[must_use]
    pub fn start(run_id: RunId) -> Self {
        let tool_output_references = ToolOutputReferenceTracker::new(run_id.clone());
        Self {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            run_id,
            next_host_event_seq: 0,
            tool_use: ToolUseTracker::new(),
            tool_failure: ToolFailureTracker::new(),
            tool_output_references,
            budget: BudgetSnapshot::new(),
            finish_reason: None,
            nested_runs: Vec::new(),
            workspace_lease: None,
            work_state_ref: None,
            graph_cursor: None,
            usage_totals: Usage::default(),
            pending_control_requests: Vec::new(),
            starting_agent: None,
            current_agent: None,
            original_input: Vec::new(),
            generated_items: Vec::new(),
            model_responses: Vec::new(),
            pending_interruptions: Vec::new(),
            pending_interruption_resolutions: Vec::new(),
            permission_rules: Vec::new(),
            input_history_is_complete: true,
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

    /// Tool-output reference retention facts as of the most recently settled turn.
    #[must_use]
    pub const fn tool_output_references(&self) -> &ToolOutputReferenceTracker {
        &self.tool_output_references
    }

    /// Mutable retention ledger for the runner's completed-turn recording path.
    #[doc(hidden)]
    pub fn tool_output_references_mut(&mut self) -> &mut ToolOutputReferenceTracker {
        &mut self.tool_output_references
    }

    /// Recoverable budget accounting as of the most recently completed operation.
    #[must_use]
    pub const fn budget(&self) -> &BudgetSnapshot {
        &self.budget
    }

    /// Replaces turn accounting while preserving all other run state.
    ///
    /// A snapshot restored on its own from a pre-ledger checkpoint carries token spend that belongs
    /// in the ledger, so it is moved here too. Otherwise the migration would depend on which of the
    /// two doors the value came through — deserializing the whole state, or deserializing a
    /// snapshot and attaching it — and only one of them would charge the run for what it spent.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetSnapshot) -> Self {
        let mut budget = budget;
        let carried = budget.take_legacy_tokens_used();
        if carried > 0 {
            self.usage_totals = self
                .usage_totals
                .accumulate(&Usage::from_carried_total(carried));
        }
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

    /// The run's usage ledger: every model call it has paid for, across every segment.
    ///
    /// An empty ledger reports zero requests, which is the same statement `None` used to make with
    /// one more state to handle. It is cumulative, so a resumed run's ledger covers the earlier
    /// segments too — unlike the total a single run result reports, which describes only the
    /// segment that produced it.
    #[must_use]
    pub const fn usage_totals(&self) -> &Usage {
        &self.usage_totals
    }

    /// Adds one settled model call's usage to the ledger.
    ///
    /// The single place a run's spend grows. The budget's token ceiling is measured against this
    /// ledger rather than against a counter of its own, so there is nothing here that a second
    /// caller could advance halfway.
    pub fn record_usage(&mut self, usage: &Usage) {
        self.usage_totals = self.usage_totals.accumulate(usage);
    }

    /// Replaces the ledger wholesale, for restoring one rather than extending it.
    ///
    /// Use [`Self::record_usage`] to record a call. This setter exists for the caller that
    /// reconstructs state from a persisted record, and it replaces rather than adds precisely so
    /// that reconstructing twice cannot double the spend.
    #[must_use]
    pub fn with_usage_totals(mut self, usage_totals: Usage) -> Self {
        self.usage_totals = usage_totals;
        self
    }

    /// Total model tokens this run has spent, across every segment.
    #[must_use]
    pub const fn tokens_used(&self) -> u64 {
        self.usage_totals.total_tokens()
    }

    /// Remaining token allowance, if tokens are limited.
    #[must_use]
    pub fn remaining_tokens(&self, limit: &BudgetLimit) -> Option<u64> {
        limit
            .max_tokens()
            .map(|maximum| maximum.saturating_sub(self.tokens_used()))
    }

    /// The first exhausted budget dimension in a deterministic priority order.
    ///
    /// It lives here because answering it needs both carriers of spend: the turn counter on
    /// [`BudgetSnapshot`] and the usage ledger this state owns. A version that read only one of
    /// them would have to take the other as an argument, and the argument a caller passes by
    /// mistake — one turn's usage instead of the run's — is a run that never stops.
    #[must_use]
    pub fn exhausted_budget_kind(&self, limit: &BudgetLimit) -> Option<BudgetKind> {
        if self.budget.turns_exhausted(limit) {
            return Some(BudgetKind::MaxTurns);
        }
        if limit
            .max_tokens()
            .is_some_and(|maximum| self.tokens_used() >= maximum)
        {
            return Some(BudgetKind::Tokens);
        }
        if limit.deadline().is_some_and(Deadline::is_expired) {
            return Some(BudgetKind::WallClock);
        }
        None
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

    /// Public declaration that started the run, once its first segment has begun.
    #[must_use]
    pub const fn starting_agent(&self) -> Option<&AgentId> {
        self.starting_agent.as_ref()
    }

    /// Public declaration that must execute the next segment.
    #[must_use]
    pub const fn current_agent(&self) -> Option<&AgentId> {
        self.current_agent.as_ref()
    }

    /// Original model input from the first segment of this run.
    #[must_use]
    pub fn original_input(&self) -> &[ModelInputItem] {
        &self.original_input
    }

    /// Authoritative records generated across every completed segment of this run.
    ///
    /// These are the run's in-memory resume projection. The session log may persist the same
    /// records as the durable source of history; retaining this projection makes a checkpoint
    /// self-sufficient between a pause and the session store's next materialization.
    #[must_use]
    pub fn generated_items(&self) -> &[RunItem] {
        &self.generated_items
    }

    /// Completed model responses across every segment of this run.
    #[must_use]
    pub fn model_responses(&self) -> &[ModelResponse] {
        &self.model_responses
    }

    /// Last completed model response, if the run has made one.
    #[must_use]
    pub fn last_model_response(&self) -> Option<&ModelResponse> {
        self.model_responses.last()
    }

    /// IDs of the records the host still has to answer before this run may continue.
    ///
    /// IDs rather than second copies of the records: every one of them is already in
    /// [`Self::generated_items`], and a checkpoint holding both would carry two versions of the
    /// same question to keep in step. The day something annotates the authoritative record — a
    /// session key, provenance — a resume that compared the two copies would refuse to continue
    /// over a difference that changes nothing about the question being asked.
    #[must_use]
    pub fn pending_interruptions(&self) -> &[ItemId] {
        &self.pending_interruptions
    }

    /// The authoritative records [`Self::pending_interruptions`] names.
    pub fn pending_interruption_items(&self) -> impl Iterator<Item = &RunItem> {
        self.pending_interruptions.iter().filter_map(|id| {
            self.generated_items
                .iter()
                .find(|generated| generated.id() == id)
        })
    }

    /// Answers a pending tool approval and retains that answer for resume.
    pub fn approve(&mut self, item: &RunItem, always: bool) -> Result<()> {
        let approval = self.authoritative_tool_approval(item)?;
        if approval.lookup_key().is_none() {
            return Err(Error::caller(format!(
                "approval `{}` cannot resume because it lacks a serialized tool routing identity",
                item.id()
            )));
        }
        self.answer_interruption(item, InterruptionResolution::Approve { always })
    }

    /// Rejects a pending tool approval and retains that answer for resume.
    pub fn reject(&mut self, item: &RunItem, always: bool) -> Result<()> {
        self.answer_interruption(item, InterruptionResolution::Reject { always })
    }

    /// Answers the awaiting records without removing them before their result is recorded.
    ///
    /// Removing an approval at click time would lose the call on a crash between the click and its
    /// execution. The runtime removes it only when it has appended either the output or refusal.
    fn answer_interruption(
        &mut self,
        item: &RunItem,
        resolution: InterruptionResolution,
    ) -> Result<()> {
        let authoritative = self.authoritative_tool_approval(item)?.clone();
        if let Some(existing) = self
            .pending_interruption_resolutions
            .iter_mut()
            .find(|entry| entry.item_id == *item.id())
        {
            existing.resolution = resolution;
        } else {
            self.pending_interruption_resolutions
                .push(PendingInterruptionResolution {
                    item_id: item.id().clone(),
                    resolution,
                });
        }
        // A re-answer supersedes the one before it, and the rule that answer minted has to go with
        // it. Appending alone would leave "always allow", later corrected to a plain rejection,
        // still allowing every subsequent call — a grant the host withdrew and has no way to reach.
        let rule = session_rule(&authoritative, resolution);
        self.permission_rules
            .retain(|existing| !targets_same_action(existing, &rule));
        if resolution.always() {
            self.permission_rules.push(rule);
        }
        Ok(())
    }

    fn authoritative_tool_approval(&self, item: &RunItem) -> Result<&ToolApproval> {
        if !self.pending_interruptions.iter().any(|id| id == item.id()) {
            return Err(Error::caller(format!(
                "interruption `{}` is not pending",
                item.id()
            )));
        }
        let Some(authoritative) = self
            .generated_items
            .iter()
            .find(|stored| stored.id() == item.id())
        else {
            return Err(Error::caller(format!(
                "pending interruption `{}` is absent from generated items",
                item.id()
            )));
        };
        let RunItemKind::ToolApproval(approval) = authoritative.kind() else {
            return Err(Error::caller(format!(
                "interruption `{}` is not a local tool approval",
                item.id()
            )));
        };
        Ok(approval)
    }

    /// Answers that have not yet been settled into model-visible history.
    #[must_use]
    pub fn pending_interruption_resolutions(&self) -> &[PendingInterruptionResolution] {
        &self.pending_interruption_resolutions
    }

    /// Session-scoped rules created by an `always` approval or rejection.
    #[must_use]
    pub fn permission_rules(&self) -> &[PermissionRule] {
        &self.permission_rules
    }

    /// Marks an answered interruption as represented in history.
    #[doc(hidden)]
    pub fn settle_interruption_resolution(&mut self, item_id: &ItemId) -> Result<()> {
        if !self
            .pending_interruption_resolutions
            .iter()
            .any(|entry| entry.item_id == *item_id)
        {
            return Err(Error::caller(format!(
                "interruption `{item_id}` has no host answer"
            )));
        }
        self.pending_interruption_resolutions
            .retain(|entry| entry.item_id != *item_id);
        self.pending_interruptions.retain(|id| id != item_id);
        Ok(())
    }

    /// Starts or resumes a segment under a stable public agent identity.
    ///
    /// A restored checkpoint must bind back to the same declaration. Replacing it with an agent
    /// that merely has a similar display name could select different instructions, tools, or
    /// handoffs after a restart, so the mismatch is rejected before any provider call is made.
    ///
    /// The state records the opening input only once. A caller may still supply a model-input
    /// projection when it resumes, but that makes its input history incomplete: later segments
    /// must also provide input instead of asking the runner to project this checkpoint.
    #[doc(hidden)]
    pub fn begin_segment(&mut self, agent: AgentId, input: Vec<ModelInputItem>) -> Result<()> {
        if self.pending_interruptions.iter().any(|id| {
            !self
                .pending_interruption_resolutions
                .iter()
                .any(|entry| entry.item_id == *id)
        }) {
            return Err(Error::caller(
                "run state has unanswered interruptions; resolve them before resuming",
            ));
        }
        match &self.current_agent {
            Some(current) if current != &agent => Err(Error::caller(format!(
                "run state expects current agent `{current}`, not `{agent}`"
            ))),
            Some(_) if input.is_empty() && !self.input_history_is_complete => Err(Error::caller(
                "run state cannot project input after a caller-managed continuation; resume with \
                 explicit input",
            )),
            Some(_) => {
                if !input.is_empty() {
                    self.input_history_is_complete = false;
                }
                Ok(())
            }
            None => {
                self.starting_agent = Some(agent.clone());
                self.current_agent = Some(agent);
                self.original_input = input;
                Ok(())
            }
        }
    }

    /// Changes the public agent after a validated handoff.
    #[doc(hidden)]
    pub fn set_current_agent(&mut self, agent: AgentId) {
        debug_assert!(self.starting_agent.is_some());
        self.current_agent = Some(agent);
    }

    /// Appends settled records to the checkpoint's resume projection.
    #[doc(hidden)]
    pub fn record_generated_items(&mut self, items: impl IntoIterator<Item = RunItem>) {
        self.generated_items.extend(items);
    }

    /// Appends a completed model response to the checkpoint's resume projection.
    #[doc(hidden)]
    pub fn record_model_response(&mut self, response: ModelResponse) {
        self.model_responses.push(response);
    }

    /// Names the records the host must answer, after checking the run generated every one of them.
    ///
    /// Takes the records and keeps their IDs: the caller has them in hand, and resolving each one
    /// against [`Self::generated_items`] here is what makes the stored names refer to something.
    #[doc(hidden)]
    pub fn set_pending_interruptions(&mut self, items: &[RunItem]) -> Result<()> {
        let ids: Vec<ItemId> = items.iter().map(|item| item.id().clone()).collect();
        validate_pending_interruptions(&ids, &self.generated_items)?;
        self.pending_interruptions = ids;
        self.pending_interruption_resolutions.clear();
        Ok(())
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// The session rule one `always` answer records.
///
/// Pinned to the exact executable the host was shown, not to its model-facing name: a click on
/// "always allow `write_file`" is an answer about the tool in front of the user, and a name rule
/// would extend it to every namespace that happens to advertise the same name. The unpinned
/// fallback is reachable only for a rejection of a record written before routing identities were
/// stored — [`RunState::approve`] refuses that case outright — and it errs toward denying more.
fn session_rule(approval: &ToolApproval, resolution: InterruptionResolution) -> PermissionRule {
    let decision = match resolution {
        InterruptionResolution::Approve { .. } => PermissionDecision::Allow,
        InterruptionResolution::Reject { .. } => PermissionDecision::Deny,
    };
    let mut rule = PermissionRule::new(decision).with_tool_name(approval.tool_name());
    if let Some(namespace) = approval.namespace() {
        rule = rule.with_namespace(namespace);
    }
    if let Some(lookup_key) = approval.lookup_key() {
        rule = rule.with_lookup_key(lookup_key.clone());
    }
    rule
}

/// Whether two session rules speak about the same action, whatever they decide about it.
///
/// The decision is deliberately excluded: replacing an answer has to retract the previous rule
/// precisely when the two disagree.
fn targets_same_action(rule: &PermissionRule, other: &PermissionRule) -> bool {
    rule.tool_name() == other.tool_name()
        && rule.namespace() == other.namespace()
        && rule.lookup_key() == other.lookup_key()
}

fn validate_interruption_resolutions(
    resolutions: &[PendingInterruptionResolution],
    pending: &[ItemId],
) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for resolution in resolutions {
        if !seen.insert(resolution.item_id()) {
            return Err(Error::caller(format!(
                "interruption `{}` has more than one host answer",
                resolution.item_id()
            )));
        }
        if !pending.iter().any(|id| id == resolution.item_id()) {
            return Err(Error::caller(format!(
                "interruption resolution `{}` names no pending interruption",
                resolution.item_id()
            )));
        }
    }
    Ok(())
}

/// Checks that every named interruption resolves to an interruption record the run generated.
///
/// The same check runs on the way in from a checkpoint and on the way in from settlement. They
/// produce the same value, and a rule enforced on only one of them is a rule a restart walks
/// around: an ID naming nothing leaves the run permanently unresumable, and one naming an ordinary
/// message leaves it waiting for an answer to a question nobody was asked.
fn validate_pending_interruptions(ids: &[ItemId], generated: &[RunItem]) -> Result<()> {
    for id in ids {
        let Some(item) = generated.iter().find(|generated| generated.id() == id) else {
            return Err(Error::caller(format!(
                "pending interruption `{id}` is absent from generated items"
            )));
        };
        if !item.kind().is_interruption() {
            return Err(Error::caller(format!(
                "pending interruption `{id}` names a record that is not an interruption"
            )));
        }
    }
    Ok(())
}

const fn run_state_schema_version() -> SchemaVersion {
    RUN_STATE_SCHEMA_VERSION
}
