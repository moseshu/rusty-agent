//! The run's own resumable state (the skeleton a future migration grows into the full checkpoint).
//!
//! A run has facts that are neither agent configuration nor session history: tool-use accounting,
//! turn and token spend, event sequences, and state owned by the loop itself. Keeping those values as
//! independent fields on the runner would make a new fact a signature change across every entry
//! point and would let a continuation accidentally carry one fact but not another. `RunState` is
//! the single carrier that crosses a run-segment boundary.
//!
//! It deliberately belongs to `ra-core`: it already carries the run's own history — generated
//! items, model responses, pending approvals, and the verdicts its guardrails reached — and a
//! persisted wire type cannot live in `ra-runtime` without reversing the dependency direction.
//!
//! # Not to be confused with task state
//!
//! This is a **single run's** recoverable state. State that spans several runs belongs to whatever
//! spans them, not to this value: a checkpoint of this value is scoped to one run's resume, while
//! such state outlives every run that touches it. Folding the second into the first would make a
//! resumed run restore a stale copy of state another run has since advanced.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    budget::{BudgetLimit, BudgetSnapshot},
    cancel::Deadline,
    compat::{SchemaVersion, Unknown},
    error::{BudgetKind, Error, Result},
    finish::FinishReason,
    guardrail::{
        InputGuardrailResult, OutputGuardrailResult, ToolInputGuardrailResult,
        ToolOutputGuardrailResult,
    },
    item::{
        AgentId, CallId, InputItemDigest, ItemId, ModelInputItem, ModelResponse, RunItem,
        RunItemKind, ToolApproval,
    },
    permission::{PermissionDecision, PermissionRule},
    sandbox::{builtin_entry_registry, sanitize_run_state_sandbox_mount_authority},
    session::{SessionCompactionContext, SessionId},
    state::{ToolFailureTracker, ToolOutputReferenceTracker, ToolUseTracker},
    tool::{ToolLookupKey, ToolOrigin},
    usage::Usage,
};

/// A Session append retained until its acknowledgement or exact tail reconciliation.
///
/// Ported from the reference's `pending_session_write`. The application supplies the original
/// backend and serializes access; Session has no distributed compare-and-swap contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingSessionWrite {
    session_id: SessionId,
    items: Vec<RunItem>,
    before: Option<Vec<InputItemDigest>>,
    persisted_count: usize,
    #[serde(default)]
    append_acknowledged: bool,
}

impl PendingSessionWrite {
    /// Captures a batch before any fallible Session operation.
    #[must_use]
    pub fn new(session_id: SessionId, items: Vec<RunItem>, persisted_count: usize) -> Self {
        Self {
            session_id,
            items,
            before: None,
            persisted_count,
            append_acknowledged: false,
        }
    }
    /// The original Session's identity.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    /// The detached batch owed to the Session.
    #[must_use]
    pub fn items(&self) -> &[RunItem] {
        &self.items
    }
    /// Tail digests captured before starting the append; None means no append started.
    #[must_use]
    pub fn before(&self) -> Option<&[InputItemDigest]> {
        self.before.as_deref()
    }
    /// The generated-history cursor after the batch settles.
    #[must_use]
    pub const fn persisted_count(&self) -> usize {
        self.persisted_count
    }
    /// Whether the append settled and only post-write compaction remains.
    #[must_use]
    pub const fn append_acknowledged(&self) -> bool {
        self.append_acknowledged
    }

    /// Retains append settlement across a failed post-write compaction.
    #[doc(hidden)]
    pub fn acknowledge_append(&mut self) {
        self.append_acknowledged = true;
    }

    /// Records the tail immediately before starting the append.
    #[doc(hidden)]
    pub fn set_before(&mut self, before: Vec<InputItemDigest>) {
        self.before = Some(before);
    }
}

/// Current [`RunState`] schema version.
pub const RUN_STATE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(14);

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
        SchemaVersion::new(3),
        "Persisted tool-output reference retention facts for context projections across resumes.",
    ),
    (
        SchemaVersion::new(4),
        "Persisted the model-input projection a transfer of control installs. An older runtime \
         reads this checkpoint without it and rebuilds the request from the whole history, which \
         hands the receiving agent the transcript the transfer withheld.",
    ),
    (
        SchemaVersion::new(5),
        "Persisted what resumes each sandbox agent's session. An older runtime carries it as an \
         unknown field and starts every sandbox agent on a fresh workspace, losing the one the \
         paused run was working in.",
    ),
    (
        SchemaVersion::new(6),
        "Persisted the agent-tool runs paused on an approval, each with its own checkpoint, and \
         routed answers to them. An older runtime neither asks for those approvals nor resumes \
         the calls waiting on them, and continues with a tool call that has no output.",
    ),
    (
        SchemaVersion::new(7),
        "Persisted the Session append cursor, pending batches and terminal write failures. An \
         older runtime cannot reconcile an uncertain append or prevent replaying terminal work.",
    ),
    (
        SchemaVersion::new(8),
        "Persisted automatic session compaction exchange evidence and append acknowledgement.",
    ),
    (
        SchemaVersion::new(9),
        "Persisted the text a host rejection returns to the model, on the answer and on the deny \
         rule an `always` rejection leaves. An older runtime drops the answer's text and returns \
         its own refusal, and cannot read a session deny rule that carries one.",
    ),
    (
        SchemaVersion::new(10),
        "Persisted the state a tool keeps with an approval, and approvals a tool asks for from \
         inside its invocation. An older runtime resumes such a call without the tool's state, \
         and dispatches a call that already started as if it had not. Started continuations are \
         retained after failures so a restored checkpoint cannot repeat their side effects.",
    ),
    (
        SchemaVersion::new(11),
        "Persisted handoffs deferred by same-turn tool approvals, their selected targets, the \
         declaration each call was bound to, their unpublished outputs and model-input \
         decomposition, and handoff \
         projections installed before the run stored any record. An older runtime forgets the \
         pending control transfer and continues the source agent with an unanswered handoff call, \
         and refuses a projection whose boundary record a filter removed.",
    ),
    (
        SchemaVersion::new(12),
        "Persisted pending settlement of committed tool outputs after a post-output callback \
         fails. An older runtime may skip the tool stop policy once the last nested approval \
         has been removed and call the model instead of concluding from the saved output.",
    ),
    (
        SchemaVersion::new(13),
        "Persisted the trace settings a resumed run falls back to, currently its trace group. An \
         older runtime keeps them as an unknown field but resumes without a trace group unless \
         the host supplies the group again.",
    ),
    (
        RUN_STATE_SCHEMA_VERSION,
        "Persisted the run's turn limit, including an explicitly unlimited run, so a resumed run \
         keeps the limit it started with. An older runtime keeps it as an unknown field and \
         resumes under whatever limit the host supplies.",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejection_message: Option<String>,
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

    /// The text a rejection returns to the model in place of the runtime's own refusal, exactly as
    /// the host supplied it through [`RunState::reject_with_message`]. An empty string is a message
    /// too. Never set on an approval.
    #[must_use]
    pub fn rejection_message(&self) -> Option<&str> {
        self.rejection_message.as_deref()
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

    /// Raises this allocator's bound to at least `other`'s, atomically.
    ///
    /// For two allocators of one run that must become one: a run continued from its checkpoint
    /// while an earlier segment's allocator is still in use elsewhere. The earlier one is kept and
    /// raised past everything the restored one promises not to reissue, and the continuation then
    /// draws from it, so the two can never hand out the same number. The bound only moves forward.
    ///
    /// # Panics
    ///
    /// In debug builds, if the allocators belong to different runs; release builds leave this
    /// allocator unchanged, since one run's numbers say nothing about another's.
    pub fn advance_past(&self, other: &Self) {
        debug_assert_eq!(
            self.run_id, other.run_id,
            "event sequence allocators of different runs"
        );
        if self.run_id == other.run_id {
            self.next.fetch_max(other.current_next(), Ordering::Relaxed);
        }
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
///
/// A parent records one for each agent-tool call whose nested run stopped to ask the host
/// something, and drops it once that call has an output. It is the reference's registry entry for
/// an interrupted `Agent.as_tool()` run, keyed the way the reference keys it — by the tool call
/// within the parent's scope — and serialized with the parent the way the reference serializes the
/// nested run's state onto the parent's pending function call.
///
/// # Why it carries the child's whole checkpoint
///
/// A host persists one value: the parent's [`RunState`]. The nested run's approvals are answered
/// through that value and its resume starts from it, so the child's state has to be inside it —
/// a pointer to some other store would make a parent checkpoint that cannot resume on its own.
///
/// The routing identity and the arguments are what the parent re-dispatches when it resumes. The
/// call's record is in the parent's history too, but a call ID is only unique within one
/// response, and finding the call by it would be a guess the day a model reuses one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedRunRef {
    scope_id: String,
    call_id: CallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lookup_key: Option<Box<ToolLookupKey>>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    arguments: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<Box<RunState>>,
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
            lookup_key: None,
            arguments: serde_json::Value::Null,
            state: None,
            unknown: Unknown::new(),
        }
    }

    /// Records the paused nested run behind one agent-tool call.
    ///
    /// `scope_id` is the parent run's ID: the reference scopes its registry per run state so two
    /// independently restored copies never read each other's entries, and here the entry travels
    /// inside the parent's state, which makes the parent's identity that scope.
    #[must_use]
    pub fn interrupted(
        scope_id: impl Into<String>,
        tool: &ToolOrigin,
        call_id: CallId,
        arguments: serde_json::Value,
        state: RunState,
    ) -> Self {
        Self {
            lookup_key: Some(Box::new(tool.lookup_key().clone())),
            arguments,
            state: Some(Box::new(state)),
            ..Self::new(scope_id, call_id)
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

    /// Exact routing key of the agent tool the parent re-dispatches on resume.
    #[must_use]
    pub fn lookup_key(&self) -> Option<&ToolLookupKey> {
        self.lookup_key.as_deref()
    }

    /// Arguments of the parent's call, which the resumed call is dispatched with again.
    #[must_use]
    pub const fn arguments(&self) -> &serde_json::Value {
        &self.arguments
    }

    /// Checkpoint of the paused nested run, when this reference records one.
    #[must_use]
    pub fn state(&self) -> Option<&RunState> {
        self.state.as_deref()
    }

    /// Takes the paused nested run's checkpoint out.
    #[must_use]
    pub fn into_state(self) -> Option<RunState> {
        self.state.map(|state| *state)
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

/// The model input a settled transfer of control installed, and where history resumes behind it.
///
/// A handoff may narrow what the receiving agent sees — through the declared
/// [`HistoryProjection`](crate::agent::HistoryProjection), through a
/// [`HandoffInputFilter`](crate::agent::HandoffInputFilter), or both — and the stored history is a
/// different list: a projection never changes it, and a filter changes only the records it was
/// handed that the session has not received yet. The two can only stay apart if the narrowed view
/// is *state*: rebuilding the next request from the opening input and the stored history would hand
/// the receiving agent a transcript the transfer did not choose, silently, on the first turn after a
/// resume.
///
/// `resume_after` names the last record folded into `input` rather than counting how many there
/// were. A position is only meaningful against the list it was taken from; an identity still says
/// what it means in a checkpoint read by another build, and a run whose history no longer contains
/// it refuses rather than guessing at a boundary.
///
/// A filter can leave a run with no stored record at all, as when it removes every record of the
/// run's first turn. [`Self::from_start`] covers that case: no stored record is folded into
/// `input`, and every record the run stores afterwards follows it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffProjection {
    input: Vec<ModelInputItem>,
    resume_after: ItemId,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    from_start: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

/// A transfer deferred until the interrupted turn's function tools have finished.
///
/// Call records retain model order and target identity. Live callbacks stay in the agent registry;
/// a checkpoint stores only the input decomposition and the boundary of this turn's records.
///
/// Each call also records the position of the declaration the turn bound it to. Two declarations
/// may share a name and a target, and the turn's collision policy and dynamic enablement chose one
/// of them; resuming by name and target alone could run the other one's callback and filter.
/// Outputs checkpointed before a callback or filter fails retain their publication boundary, so a
/// resumed attempt can publish what its filter keeps without repeating earlier staged outputs.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingHandoff {
    calls: Vec<RunItem>,
    #[serde(default)]
    declarations: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unpublished_items: Vec<ItemId>,
    input_history: Vec<ModelInputItem>,
    pre_handoff_items: Vec<RunItem>,
    new_items_start: ItemId,
    #[serde(default)]
    server_manages_conversation: bool,
}

impl PendingHandoff {
    /// Captures typed handoff calls and the conversation decomposition of an interrupted turn.
    #[must_use]
    pub fn new(
        calls: Vec<RunItem>,
        input_history: Vec<ModelInputItem>,
        pre_handoff_items: Vec<RunItem>,
        new_items_start: ItemId,
    ) -> Self {
        Self {
            calls,
            declarations: Vec::new(),
            unpublished_items: Vec::new(),
            input_history,
            pre_handoff_items,
            new_items_start,
            server_manages_conversation: false,
        }
    }

    /// Typed handoff calls in original model order.
    #[must_use]
    pub fn calls(&self) -> &[RunItem] {
        &self.calls
    }

    /// Records, for each call in order, the position of the declaration the turn bound it to.
    #[must_use]
    pub fn with_declarations(mut self, declarations: Vec<usize>) -> Self {
        self.declarations = declarations;
        self
    }

    /// Positions in the source agent's handoff declarations, one per call in order.
    #[must_use]
    pub fn declarations(&self) -> &[usize] {
        &self.declarations
    }

    /// Records retained for recovery but not yet published to the stream or rollout.
    #[must_use]
    pub fn unpublished_items(&self) -> &[ItemId] {
        &self.unpublished_items
    }

    /// Retains publication boundaries across callback and filter failures.
    #[must_use]
    pub fn with_unpublished_items(mut self, items: Vec<ItemId>) -> Self {
        self.unpublished_items = items;
        self
    }

    /// Position of the first stored record of the interrupted turn.
    pub fn start(&self, generated: &[RunItem]) -> Result<usize> {
        self.validate(generated)
    }

    /// Retains the provider's conversation-ownership constraint across approval recovery.
    #[must_use]
    pub const fn with_server_managed_conversation(mut self, enabled: bool) -> Self {
        self.server_manages_conversation = enabled;
        self
    }

    /// Whether the interrupted provider call used server-managed conversation history.
    #[must_use]
    pub const fn server_manages_conversation(&self) -> bool {
        self.server_manages_conversation
    }

    /// Reconstructs the handoff input including outputs settled during approval recovery.
    pub fn input_data(&self, generated: &[RunItem]) -> Result<crate::agent::HandoffInputData> {
        let start = self.validate(generated)?;
        Ok(crate::agent::HandoffInputData::new(
            self.input_history.clone(),
            self.pre_handoff_items.clone(),
            generated[start..].to_vec(),
        ))
    }

    fn validate(&self, generated: &[RunItem]) -> Result<usize> {
        let start = generated
            .iter()
            .position(|item| item.id() == &self.new_items_start)
            .ok_or_else(|| Error::caller("pending handoff has no stored turn boundary"))?;
        if self.calls.is_empty() {
            return Err(Error::caller("pending handoff has no calls"));
        }
        if self.declarations.len() != self.calls.len() {
            return Err(Error::caller(
                "pending handoff must name the declaration each call was bound to",
            ));
        }
        let mut unpublished = std::collections::BTreeSet::new();
        for id in &self.unpublished_items {
            if !unpublished.insert(id)
                || !generated[start..]
                    .iter()
                    .any(|item| item.id() == id && item.is_model_input())
            {
                return Err(Error::caller(
                    "unpublished handoff records must name distinct stored model-input records",
                ));
            }
        }
        for call in &self.calls {
            let RunItemKind::HandoffCall(handoff) = call.kind() else {
                return Err(Error::caller("pending handoff contains a non-handoff call"));
            };
            if !generated[start..].iter().any(|item| {
                item.id() == call.id()
                    && match item.kind() {
                        RunItemKind::ToolCall(stored) => {
                            stored.call_id() == handoff.call_id()
                                && Some(stored.name()) == handoff.tool_name()
                                && stored.arguments() == handoff.arguments()
                        }
                        RunItemKind::HandoffCall(stored) => {
                            stored.call_id() == handoff.call_id()
                                && stored.target_agent() == handoff.target_agent()
                                && stored.arguments() == handoff.arguments()
                        }
                        _ => false,
                    }
            }) {
                return Err(Error::caller("pending handoff call has no stored record"));
            }
        }
        Ok(start)
    }
}

impl HandoffProjection {
    /// Creates the projection a settled transfer installs.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>, resume_after: ItemId) -> Self {
        Self {
            input,
            resume_after,
            from_start: false,
            unknown: Unknown::new(),
        }
    }

    /// Creates a projection installed before the run stored any record.
    ///
    /// `transfer` names the record that moved control, which the filter removed from history; it
    /// is kept for diagnostics and is not looked up.
    #[must_use]
    pub fn from_start(input: Vec<ModelInputItem>, transfer: ItemId) -> Self {
        Self {
            input,
            resume_after: transfer,
            from_start: true,
            unknown: Unknown::new(),
        }
    }

    /// Whether no stored record precedes the projection, so all stored history follows it.
    #[must_use]
    pub const fn is_from_start(&self) -> bool {
        self.from_start
    }

    /// Model input the receiving agent continues from.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// The last record folded into [`Self::input`]; history carries on after it.
    ///
    /// For a projection [`from_start`](Self::from_start) this names the removed transfer record
    /// instead, and history carries on from the beginning.
    #[must_use]
    pub const fn resume_after(&self) -> &ItemId {
        &self.resume_after
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

/// Trace settings captured from the trace a run segment executed in, under the reference's
/// `trace` key.
///
/// The reference stores its whole `TraceState` here and falls back to each value a resumed run's
/// configuration leaves unset. This runtime has no configurable trace ID, workflow name or trace
/// metadata yet, so only the group is captured; fields written by a build that ports the others
/// are retained until the next segment captures its current trace.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct RunTraceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group_id: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

/// Matches the reference's tolerant parsing of diagnostic trace settings without weakening the
/// validation of resumable execution state.
fn deserialize_run_trace_state<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<RunTraceState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let serde_json::Value::Object(mut fields) = serde_json::Value::deserialize(deserializer)?
    else {
        return Ok(None);
    };
    let group_id = match fields.remove("group_id") {
        Some(serde_json::Value::String(group)) => Some(group),
        _ => None,
    };
    let unknown = Unknown::deserialize(serde_json::Value::Object(fields))
        .map_err(serde::de::Error::custom)?;
    Ok((group_id.is_some() || !unknown.is_empty()).then_some(RunTraceState { group_id, unknown }))
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
/// Deserialization goes through the private `RunStateRecord`, which exists so that spend recorded
/// under an earlier layout still counts. That is the whole of the migration, and it is on the way
/// in rather than at a call site, because a resumed run that has to remember to migrate is a
/// resumed run that silently gets its allowance back the day someone forgets.
///
/// # Why the latch fields stay separate bools
///
/// Four of them are one-shot markers of things that already happened — input checks were sent, a
/// stop hook already asked for more work, a child announced itself, the input history is whole.
/// They are independent, not the states of one machine, and each is a serialized field with a
/// documented shape. Grouping any two of them to satisfy a field-count heuristic would change the
/// checkpoint's JSON for a reason that has nothing to do with what a checkpoint means.
#[allow(clippy::struct_excessive_bools)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    memory_exposures: Vec<crate::memory::MemoryExposure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    input_guardrail_results: Vec<InputGuardrailResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    output_guardrail_results: Vec<OutputGuardrailResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_input_guardrail_results: Vec<ToolInputGuardrailResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_output_guardrail_results: Vec<ToolOutputGuardrailResult>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    input_guardrails_started: bool,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    stop_hook_active: bool,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    subagent_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<RunId>,
    #[serde(default)]
    budget: BudgetSnapshot,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_turn_limit"
    )]
    #[allow(
        clippy::option_option,
        reason = "missing legacy field differs from a saved unlimited limit"
    )]
    max_turns: Option<Option<u32>>,
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
    started_tool_continuations: Vec<ItemId>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    tool_output_settlement_pending: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permission_rules: Vec<PermissionRule>,
    #[serde(default = "default_input_history_is_complete")]
    input_history_is_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff_projection: Option<HandoffProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_handoff: Option<PendingHandoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_persisted_item_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_session_write: Option<PendingSessionWrite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_compaction: Option<SessionCompactionContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trace: Option<RunTraceState>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    terminal_unrecoverable: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_sandbox_envelope"
    )]
    sandbox: Option<serde_json::Value>,
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
// Mirrors `RunState` field for field, including its latch bools; see that type for why they stay
// separate. Regrouping them here alone would break the mirror the round-trip test relies on.
#[allow(clippy::struct_excessive_bools)]
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
    memory_exposures: Vec<crate::memory::MemoryExposure>,
    #[serde(default)]
    input_guardrail_results: Vec<InputGuardrailResult>,
    #[serde(default)]
    output_guardrail_results: Vec<OutputGuardrailResult>,
    #[serde(default)]
    tool_input_guardrail_results: Vec<ToolInputGuardrailResult>,
    #[serde(default)]
    tool_output_guardrail_results: Vec<ToolOutputGuardrailResult>,
    #[serde(default)]
    input_guardrails_started: bool,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    stop_hook_active: bool,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    subagent_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<RunId>,
    #[serde(default)]
    budget: BudgetSnapshot,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_turn_limit"
    )]
    #[allow(
        clippy::option_option,
        reason = "missing legacy field differs from a saved unlimited limit"
    )]
    max_turns: Option<Option<u32>>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    started_tool_continuations: Vec<ItemId>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    tool_output_settlement_pending: bool,
    #[serde(default)]
    permission_rules: Vec<PermissionRule>,
    #[serde(default = "default_input_history_is_complete")]
    input_history_is_complete: bool,
    #[serde(default)]
    handoff_projection: Option<HandoffProjection>,
    #[serde(default)]
    pending_handoff: Option<PendingHandoff>,
    #[serde(default)]
    session_persisted_item_count: Option<usize>,
    #[serde(default)]
    pending_session_write: Option<PendingSessionWrite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_compaction: Option<SessionCompactionContext>,
    #[serde(default, deserialize_with = "deserialize_run_trace_state")]
    trace: Option<RunTraceState>,
    #[serde(default)]
    terminal_unrecoverable: bool,
    #[serde(default)]
    sandbox: Option<serde_json::Value>,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl TryFrom<RunStateRecord> for RunState {
    type Error = Error;

    // Its length is the struct's field count, twice: once taken apart and once put back together.
    // Splitting it would not shorten anything, it would only move half the mirror somewhere the
    // "field for field" claim above can no longer be checked by reading one function.
    #[allow(clippy::too_many_lines)]
    fn try_from(record: RunStateRecord) -> std::result::Result<Self, Self::Error> {
        let RunStateRecord {
            schema_version,
            run_id,
            next_host_event_seq,
            tool_use,
            tool_failure,
            tool_output_references,
            mut budget,
            max_turns,
            memory_exposures,
            input_guardrail_results,
            output_guardrail_results,
            tool_input_guardrail_results,
            tool_output_guardrail_results,
            input_guardrails_started,
            stop_hook_active,
            subagent_started,
            parent_run_id,
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
            started_tool_continuations,
            tool_output_settlement_pending,
            permission_rules,
            input_history_is_complete,
            handoff_projection,
            pending_handoff,
            session_persisted_item_count,
            pending_session_write,
            session_compaction,
            trace,
            terminal_unrecoverable,
            sandbox,
            unknown,
        } = record;
        // Before anything else reads it: a checkpoint is the one input that arrives from outside
        // the process, and one written by hand or by an older build may carry mount authority.
        let sandbox = sandbox
            .as_ref()
            .map(sanitize_sandbox_envelope)
            .transpose()?;

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
        for id in &started_tool_continuations {
            if !generated_items
                .iter()
                .any(|item| item.id() == id && matches!(item.kind(), RunItemKind::ToolApproval(_)))
            {
                return Err(Error::caller(
                    "a started tool continuation names no stored approval",
                ));
            }
        }
        validate_nested_runs(&nested_runs, &run_id)?;
        if let Some(pending) = &pending_handoff {
            pending.validate(&generated_items)?;
        }
        // A count past the end would make the next append skip records the session never received,
        // and nothing afterwards could tell which ones.
        if let Some(count) = session_persisted_item_count
            && count > generated_items.len()
        {
            return Err(Error::caller(format!(
                "run state says its session was given {count} records, but it holds only {}",
                generated_items.len()
            )));
        }

        if let Some(context) = &session_compaction
            && context.model_exchange().iter().any(|digest| {
                digest.as_str().len() != 64
                    || !digest.as_str().bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(Error::caller(
                "invalid session compaction exchange checkpoint",
            ));
        }
        if let Some(pending) = &pending_session_write {
            validate_pending_session_write(
                pending,
                generated_items.len(),
                session_persisted_item_count,
            )?;
        }

        Ok(Self {
            schema_version,
            run_id,
            next_host_event_seq,
            tool_use,
            tool_failure,
            tool_output_references,
            memory_exposures,
            input_guardrail_results,
            output_guardrail_results,
            tool_input_guardrail_results,
            tool_output_guardrail_results,
            input_guardrails_started,
            stop_hook_active,
            subagent_started,
            parent_run_id,
            budget,
            max_turns,
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
            started_tool_continuations,
            tool_output_settlement_pending,
            permission_rules,
            input_history_is_complete,
            handoff_projection,
            pending_handoff,
            session_persisted_item_count,
            pending_session_write,
            session_compaction,
            trace,
            terminal_unrecoverable,
            sandbox,
            unknown,
        })
    }
}

impl RunState {
    /// Saved turn limit: absent for an older checkpoint, `Some(None)` for unlimited.
    #[doc(hidden)]
    #[must_use]
    pub const fn max_turns(&self) -> Option<Option<u32>> {
        self.max_turns
    }

    /// Saves the configured turn limit before the run starts sampling.
    #[doc(hidden)]
    pub fn set_max_turns(&mut self, max_turns: Option<u32>) {
        self.max_turns = Some(max_turns);
    }

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
            memory_exposures: Vec::new(),
            input_guardrail_results: Vec::new(),
            output_guardrail_results: Vec::new(),
            tool_input_guardrail_results: Vec::new(),
            tool_output_guardrail_results: Vec::new(),
            input_guardrails_started: false,
            stop_hook_active: false,
            subagent_started: false,
            parent_run_id: None,
            budget: BudgetSnapshot::new(),
            max_turns: None,
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
            started_tool_continuations: Vec::new(),
            tool_output_settlement_pending: false,
            permission_rules: Vec::new(),
            input_history_is_complete: true,
            handoff_projection: None,
            pending_handoff: None,
            session_persisted_item_count: None,
            pending_session_write: None,
            session_compaction: None,
            trace: None,
            terminal_unrecoverable: false,
            sandbox: None,
            unknown: Unknown::new(),
        }
    }

    /// Schema version of this value.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Whether committed tool outputs still need the interrupted turn's stop-policy settlement.
    ///
    /// This is the persisted subset of the reference's pending step needed after a post-output
    /// callback fails. It remains meaningful when no approval or nested run is left to resume.
    #[doc(hidden)]
    #[must_use]
    pub const fn tool_output_settlement_pending(&self) -> bool {
        self.tool_output_settlement_pending
    }

    /// Records or settles the pending stop-policy phase without repeating committed callables.
    #[doc(hidden)]
    pub const fn set_tool_output_settlement_pending(&mut self, pending: bool) {
        self.tool_output_settlement_pending = pending;
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

    /// Versioned memory evidence included in successful model requests in this run.
    #[must_use]
    pub fn memory_exposures(&self) -> &[crate::memory::MemoryExposure] {
        &self.memory_exposures
    }

    /// Records newly exposed memory evidence without duplicating a previously exposed token.
    pub fn record_memory_exposures(&mut self, exposures: Vec<crate::memory::MemoryExposure>) {
        for evidence in exposures {
            if !self
                .memory_exposures
                .iter()
                .any(|existing| existing.token() == evidence.token())
            {
                self.memory_exposures.push(evidence);
            }
        }
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

    /// Agent-tool runs paused on an approval, each waiting to be continued when this run resumes.
    ///
    /// Only paused runs are recorded, as in the reference: a nested run that finished has already
    /// become its call's output, and nothing about it is left to resume.
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

    /// What resumes the sandbox sessions this run's sandbox agents used, if any.
    ///
    /// Written by the runtime when a run ends, from each session's state as its client serialized
    /// it, and read back when the run is continued. Kept as the document the runtime wrote rather
    /// than a typed value: only the client that wrote a session's state can read it, and this
    /// checkpoint is read long before any client is known.
    ///
    /// **Never carries mount authority.** Every way in — this setter, deserialization — strips it
    /// (see [`sanitize_run_state_sandbox_mount_authority`]), and serialization strips it again, so
    /// a checkpoint on disk holds no bucket key whatever a host put here.
    #[must_use]
    pub const fn sandbox_resume_state(&self) -> Option<&serde_json::Value> {
        self.sandbox.as_ref()
    }

    /// Replaces what resumes this run's sandbox sessions, with mount authority stripped.
    ///
    /// `None` forgets it, which is what a run that could not settle its sandboxes records: a state
    /// describing sessions whose cleanup failed would resume a workspace nobody can vouch for.
    ///
    /// # Errors
    ///
    /// Returns a caller error, quoting nothing, for an envelope without the documented shape or a
    /// session state whose manifest cannot be sanitized. The checkpoint is left with no sandbox
    /// state rather than the one it had, as the reference clears it.
    pub fn set_sandbox_resume_state(&mut self, sandbox: Option<serde_json::Value>) -> Result<()> {
        self.sandbox = None;
        self.sandbox = sandbox
            .map(|payload| sanitize_sandbox_envelope(&payload))
            .transpose()?;
        Ok(())
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

    /// Whether the model input is reconstructible from this state alone.
    ///
    /// False once a segment resumed with caller-supplied input: that projection is the caller's
    /// and this state never recorded it, so rebuilding the request from
    /// [`Self::original_input`] plus [`Self::generated_items`] would silently send something the
    /// caller did not ask for. Anything that reprojects the history — context processing above
    /// all — has to consult this rather than re-deriving it from its own view of the segment.
    #[must_use]
    pub const fn input_history_is_complete(&self) -> bool {
        self.input_history_is_complete
    }

    /// Authoritative records generated across every completed segment of this run.
    ///
    /// These are the run's in-memory resume projection. The session log may persist the same
    /// records as the durable source of history; retaining this projection makes a checkpoint
    /// self-sufficient between a pause and the session store's next materialization.
    ///
    /// **This is the session's view, not the model's.** After a transfer of control narrowed what
    /// the receiving agent may see, the two differ; [`Self::model_input_base`] is the one to build a
    /// request from.
    #[must_use]
    pub fn generated_items(&self) -> &[RunItem] {
        &self.generated_items
    }

    /// The model input this run continues from, and the records generated since.
    ///
    /// Without a transfer of control these are the run's opening input and its whole history, which
    /// is the projection [`Self::original_input`] and [`Self::generated_items`] describe on their
    /// own. Once a handoff has installed a [`HandoffProjection`], they are that projection and only
    /// the records appended after it — which is the entire point of installing one.
    ///
    /// # Errors
    ///
    /// Returns a caller error when an installed projection names a record this history no longer
    /// contains. Continuing would mean choosing a boundary, and both choices are wrong: taking all
    /// the records re-sends the history the projection withheld, and taking none discards the work
    /// done since the transfer.
    pub fn model_input_base(&self) -> Result<(&[ModelInputItem], &[RunItem])> {
        let Some(projection) = &self.handoff_projection else {
            return Ok((&self.original_input, &self.generated_items));
        };
        if projection.is_from_start() {
            return Ok((projection.input(), &self.generated_items));
        }
        let boundary = self
            .generated_items
            .iter()
            .position(|item| item.id() == projection.resume_after())
            .ok_or_else(|| {
                Error::caller(format!(
                    "the model input projection resumes after record `{}`, which this run's history \
                     does not contain",
                    projection.resume_after()
                ))
            })?;
        Ok((projection.input(), &self.generated_items[boundary + 1..]))
    }

    /// The model-input projection a transfer of control installed, if one is in force.
    #[must_use]
    pub const fn handoff_projection(&self) -> Option<&HandoffProjection> {
        self.handoff_projection.as_ref()
    }

    /// Transfer waiting for this turn's function tools to finish.
    #[must_use]
    pub const fn pending_handoff(&self) -> Option<&PendingHandoff> {
        self.pending_handoff.as_ref()
    }

    /// Saves or clears a deferred transfer without serializing live callbacks.
    pub fn set_pending_handoff(&mut self, pending: Option<PendingHandoff>) -> Result<()> {
        if let Some(pending) = &pending {
            pending.validate(&self.generated_items)?;
        }
        self.pending_handoff = pending;
        Ok(())
    }

    /// How many leading [`Self::generated_items`] have been settled with the run's session, or
    /// `None` when no segment of this run was bound to one.
    ///
    /// The reference's `current_turn_persisted_item_count`, kept for the whole run rather than for
    /// one turn: the records are append-only, so a single index says what a continuation still
    /// owes the session, and a resumed or streamed segment appends from it instead of appending
    /// again what an earlier segment already wrote. Records before it are not necessarily all in
    /// the session — approval records never go there, and a run bound to a session only when it
    /// resumed owes nothing generated before that.
    #[must_use]
    pub const fn session_persisted_item_count(&self) -> Option<usize> {
        self.session_persisted_item_count
    }

    /// Binds this run's history to a session: records generated from here on are owed to it.
    ///
    /// A run already bound keeps its count. A run binding now — a fresh run, or one first resumed
    /// with a session — owes nothing it generated before.
    #[doc(hidden)]
    pub fn bind_session_persistence(&mut self) {
        if self.session_persisted_item_count.is_none() {
            self.session_persisted_item_count = Some(self.generated_items.len());
        }
    }

    /// Records that the session has been given every record before `count`.
    ///
    /// # Errors
    ///
    /// Returns a caller error for a count that moves backwards or past the end of the history;
    /// either would make a later append repeat or skip records.
    #[doc(hidden)]
    pub fn mark_session_persisted(&mut self, count: usize) -> Result<()> {
        let current = self.session_persisted_item_count.unwrap_or(0);
        if count < current || count > self.generated_items.len() {
            return Err(Error::caller(format!(
                "cannot move the session persistence count from {current} to {count} over a \
                 history of {} records",
                self.generated_items.len()
            )));
        }
        self.session_persisted_item_count = Some(count);
        Ok(())
    }

    /// The latest successful model exchange and session mutation ownership.
    #[must_use]
    pub const fn session_compaction(&self) -> Option<&SessionCompactionContext> {
        self.session_compaction.as_ref()
    }

    /// Updates compaction evidence without retaining plaintext model requests.
    #[doc(hidden)]
    pub fn set_session_compaction(&mut self, context: SessionCompactionContext) {
        self.session_compaction = Some(context);
    }

    /// The trace group captured from the trace this run last executed in, exactly as configured.
    ///
    /// A resumed run whose configuration sets no group uses this one, as the reference resolves
    /// trace settings from its checkpointed `TraceState`.
    #[must_use]
    pub fn trace_group_id(&self) -> Option<&str> {
        self.trace.as_ref()?.group_id.as_deref()
    }

    /// Captures the trace a run segment executes in, replacing any settings an earlier segment
    /// recorded, as the reference's `RunState.set_trace` does.
    #[doc(hidden)]
    pub fn set_trace_group_id(&mut self, group_id: Option<String>) {
        self.trace = group_id.map(|group_id| RunTraceState {
            group_id: Some(group_id),
            unknown: Unknown::new(),
        });
    }

    /// The append that must settle before further model or tool work.
    #[must_use]
    pub const fn pending_session_write(&self) -> Option<&PendingSessionWrite> {
        self.pending_session_write.as_ref()
    }

    /// Mutable access for the runtime to capture the pre-append tail.
    #[doc(hidden)]
    pub fn pending_session_write_mut(&mut self) -> Option<&mut PendingSessionWrite> {
        self.pending_session_write.as_mut()
    }

    /// Starts an append without replacing an unresolved batch.
    #[doc(hidden)]
    pub fn begin_session_write(&mut self, pending: PendingSessionWrite) -> Result<()> {
        if self.pending_session_write.is_some() {
            return Err(Error::caller(
                "resolve the pending Session write before saving another batch",
            ));
        }
        validate_pending_session_write(
            &pending,
            self.generated_items.len(),
            self.session_persisted_item_count,
        )?;
        self.pending_session_write = Some(pending);
        Ok(())
    }

    /// Acknowledges the batch and moves its cursor together.
    #[doc(hidden)]
    pub fn finish_session_write(&mut self) -> Result<()> {
        if let Some(pending) = &self.pending_session_write {
            self.mark_session_persisted(pending.persisted_count)?;
        }
        self.pending_session_write = None;
        Ok(())
    }

    /// Whether this run or a nested agent-tool run retains an uncertain or terminal Session write.
    #[must_use]
    pub fn has_session_write_checkpoint(&self) -> bool {
        self.pending_session_write.is_some()
            || self.terminal_unrecoverable
            || self
                .nested_runs
                .iter()
                .filter_map(NestedRunRef::state)
                .any(Self::has_session_write_checkpoint)
    }

    /// Whether a terminal output's failed append made this run unsafe to resume.
    #[must_use]
    pub const fn terminal_unrecoverable(&self) -> bool {
        self.terminal_unrecoverable
    }

    /// Prevents replaying completed terminal work after its Session append failed.
    #[doc(hidden)]
    pub fn mark_terminal_unrecoverable(&mut self) {
        self.terminal_unrecoverable = true;
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

    /// Every record the host still has to answer: the ones [`Self::pending_interruptions`] names,
    /// then those of each paused agent-tool run in [`Self::nested_runs`], depth first.
    ///
    /// The nested ones are the reference's `get_interruptions()` for a run whose agent tool
    /// stopped on an approval: the question is the nested run's, but the host is asked through
    /// the parent, and answers it through [`Self::approve`] or [`Self::reject`] on the parent.
    pub fn pending_interruption_items(&self) -> impl Iterator<Item = &RunItem> {
        let mut items = Vec::new();
        self.collect_pending_interruption_items(&mut items);
        items.into_iter()
    }

    fn collect_pending_interruption_items<'a>(&'a self, items: &mut Vec<&'a RunItem>) {
        items.extend(self.pending_interruptions.iter().filter_map(|id| {
            self.generated_items
                .iter()
                .find(|generated| generated.id() == id)
        }));
        for state in self.nested_runs.iter().filter_map(NestedRunRef::state) {
            state.collect_pending_interruption_items(items);
        }
    }

    /// Answers a pending tool approval and retains that answer for resume.
    ///
    /// An approval raised inside a paused agent-tool run is answered on that run's own state, as
    /// the reference routes it: an `always` answer there becomes a rule of the nested run, and
    /// never one of this run.
    pub fn approve(&mut self, item: &RunItem, always: bool) -> Result<()> {
        let owner = self.interruption_owner(item)?;
        self.owner_mut(&owner).approve_own(item, always)
    }

    /// Rejects a pending tool approval and retains that answer for resume.
    ///
    /// Routed to the run that raised the approval, as [`Self::approve`] is.
    pub fn reject(&mut self, item: &RunItem, always: bool) -> Result<()> {
        let owner = self.interruption_owner(item)?;
        self.owner_mut(&owner).answer_interruption(
            item,
            InterruptionResolution::Reject { always },
            None,
        )
    }

    /// Rejects a pending tool approval, sending `message` back to the model in place of the
    /// runtime's own refusal: the reference's `reject(..., rejection_message=...)`.
    ///
    /// The text is returned verbatim, an empty string included. With `always`, the deny rule the
    /// answer leaves behind keeps it, so a later call that rule refuses returns the same text, and
    /// so does a call of the same action rejected without text of its own (see
    /// [`Self::rejection_message_for`]). A later answer to the same record replaces this one
    /// together with its text, and an approval carries none. Routed to the run that raised the
    /// approval, as [`Self::approve`] is.
    ///
    /// # Exact answers and sticky defaults
    ///
    /// As in the reference, an answer without `always` is an exception for its own call and leaves
    /// the action's sticky default — the session rule an earlier `always` answer left — in place
    /// for every other call. An `always` answer replaces that default and settles the other calls
    /// of the same action still waiting in this run the same way, each keeping its own rejection
    /// text when it stays rejected.
    pub fn reject_with_message(
        &mut self,
        item: &RunItem,
        always: bool,
        message: impl Into<String>,
    ) -> Result<()> {
        let owner = self.interruption_owner(item)?;
        self.owner_mut(&owner).answer_interruption(
            item,
            InterruptionResolution::Reject { always },
            Some(message.into()),
        )
    }

    fn approve_own(&mut self, item: &RunItem, always: bool) -> Result<()> {
        let approval = self.authoritative_tool_approval(item)?;
        if approval.lookup_key().is_none() {
            return Err(Error::caller(format!(
                "approval `{}` cannot resume because it lacks a serialized tool routing identity",
                item.id()
            )));
        }
        self.answer_interruption(item, InterruptionResolution::Approve { always }, None)
    }

    /// Finds the run — this one, or a paused agent-tool run inside it — that is waiting on `item`.
    ///
    /// Returned as a path of indexes into [`Self::nested_runs`], empty for this run. An item no run
    /// is waiting on resolves to this run, whose own answer then reports it as not pending.
    ///
    /// Record IDs derive from call IDs, and a nested run's calls are numbered by another model
    /// call than this run's, so the same ID can be pending in two places. The record the host was
    /// shown settles that when it can — the two copies name different producers — and when it
    /// cannot, the answer is refused rather than applied to whichever came first, as the reference
    /// fails closed on an ambiguous approval identity.
    fn interruption_owner(&self, item: &RunItem) -> Result<Vec<usize>> {
        let mut owners = Vec::new();
        self.collect_interruption_owners(item.id(), &mut Vec::new(), &mut owners);
        if owners.len() > 1 {
            owners.retain(|path| {
                self.owner(path)
                    .generated_items
                    .iter()
                    .any(|stored| stored == item)
            });
            if owners.len() != 1 {
                return Err(Error::caller(format!(
                    "cannot apply an answer to `{}`: more than one pending approval in this run and                      its nested agent-tool runs has that identity; use unique call IDs",
                    item.id()
                )));
            }
        }
        Ok(owners.pop().unwrap_or_default())
    }

    fn collect_interruption_owners(
        &self,
        id: &ItemId,
        path: &mut Vec<usize>,
        owners: &mut Vec<Vec<usize>>,
    ) {
        if self
            .pending_interruptions
            .iter()
            .any(|pending| pending == id)
        {
            owners.push(path.clone());
        }
        for (index, nested) in self.nested_runs.iter().enumerate() {
            if let Some(state) = nested.state() {
                path.push(index);
                state.collect_interruption_owners(id, path, owners);
                path.pop();
            }
        }
    }

    fn owner(&self, path: &[usize]) -> &Self {
        path.iter().fold(self, |state, index| {
            state.nested_runs[*index]
                .state
                .as_deref()
                .unwrap_or_else(|| unreachable!("owner paths only pass through recorded states"))
        })
    }

    fn owner_mut(&mut self, path: &[usize]) -> &mut Self {
        path.iter().fold(self, |state, index| {
            state.nested_runs[*index]
                .state
                .as_deref_mut()
                .unwrap_or_else(|| unreachable!("owner paths only pass through recorded states"))
        })
    }

    /// The records in [`Self::pending_interruption_items`] that have no answer yet.
    ///
    /// What a resumed run asks again: the reference re-asks only the approvals still unanswered,
    /// and an answer already given stays with the run that owns it until that run continues.
    pub fn unanswered_interruption_items(&self) -> impl Iterator<Item = &RunItem> {
        let mut items = Vec::new();
        self.collect_unanswered_interruption_items(&mut items);
        items.into_iter()
    }

    fn collect_unanswered_interruption_items<'a>(&'a self, items: &mut Vec<&'a RunItem>) {
        items.extend(
            self.pending_interruptions
                .iter()
                .filter(|id| {
                    !self
                        .pending_interruption_resolutions
                        .iter()
                        .any(|entry| entry.item_id == **id)
                })
                .filter_map(|id| {
                    self.generated_items
                        .iter()
                        .find(|generated| generated.id() == id)
                }),
        );
        for state in self.nested_runs.iter().filter_map(NestedRunRef::state) {
            state.collect_unanswered_interruption_items(items);
        }
    }

    /// Whether any record this run or a paused agent-tool run inside it waits on has no answer.
    ///
    /// Answered records stay pending until their outputs are settled. This reports only records
    /// that still need a host decision, including those in deeper nested runs.
    #[must_use]
    pub fn has_unanswered_interruptions(&self) -> bool {
        self.pending_interruptions.iter().any(|id| {
            !self
                .pending_interruption_resolutions
                .iter()
                .any(|entry| entry.item_id == *id)
        }) || self
            .nested_runs
            .iter()
            .filter_map(NestedRunRef::state)
            .any(Self::has_unanswered_interruptions)
    }

    /// Records the agent-tool runs the settled turn left paused on an approval.
    ///
    /// Replaces whatever was recorded before: a turn settles on one interruption, and the nested
    /// runs of an earlier one were continued — and so dropped — before it started.
    ///
    /// # Errors
    ///
    /// Returns a caller error for a reference that could not be resumed from this state: one
    /// without a checkpoint, a routing key or anything pending, one whose checkpoint names a
    /// different parent, or two references for the same call.
    #[doc(hidden)]
    pub fn set_nested_runs(&mut self, nested_runs: Vec<NestedRunRef>) -> Result<()> {
        validate_nested_runs(&nested_runs, &self.run_id)?;
        self.nested_runs = nested_runs;
        Ok(())
    }

    /// Takes the paused agent-tool runs out so each can be continued.
    #[doc(hidden)]
    pub fn take_nested_runs(&mut self) -> Vec<NestedRunRef> {
        std::mem::take(&mut self.nested_runs)
    }

    /// Answers the awaiting records without removing them before their result is recorded.
    ///
    /// Removing an approval at click time would lose the call on a crash between the click and its
    /// execution. The runtime removes it only when it has appended either the output or refusal.
    fn answer_interruption(
        &mut self,
        item: &RunItem,
        resolution: InterruptionResolution,
        rejection_message: Option<String>,
    ) -> Result<()> {
        let authoritative = self.authoritative_tool_approval(item)?.clone();
        self.set_interruption_answer(item.id(), resolution, rejection_message.clone());
        if !resolution.always() {
            // An exact answer is an exception for this call alone. The reference keeps the sticky
            // default of the same tool beside it, for every other call, so the rule stays.
            return Ok(());
        }
        // A sticky answer replaces the previous default for the action, as the reference resets
        // its approval record, and settles every other call of that action still waiting here:
        // the reference's sticky value supersedes their exact entries. A rejection keeps their
        // own rejection text, an approval clears it.
        let rule =
            session_rule(&authoritative, resolution).with_rejection_message(rejection_message);
        self.permission_rules
            .retain(|existing| !targets_same_action(existing, &rule));
        let derived = match resolution {
            InterruptionResolution::Approve { .. } => {
                InterruptionResolution::Approve { always: false }
            }
            InterruptionResolution::Reject { .. } => {
                InterruptionResolution::Reject { always: false }
            }
        };
        let same_action: Vec<ItemId> = self
            .pending_interruptions
            .iter()
            .filter(|id| *id != item.id())
            .filter(|id| {
                self.generated_items
                    .iter()
                    .find(|stored| stored.id() == *id)
                    .and_then(|stored| match stored.kind() {
                        RunItemKind::ToolApproval(approval) => Some(approval),
                        _ => None,
                    })
                    .is_some_and(|approval| {
                        targets_same_action(&session_rule(approval, resolution), &rule)
                    })
            })
            .cloned()
            .collect();
        for id in same_action {
            let kept = match derived {
                InterruptionResolution::Reject { .. } => self
                    .pending_interruption_resolutions
                    .iter()
                    .find(|entry| entry.item_id == id)
                    .filter(|entry| {
                        matches!(entry.resolution, InterruptionResolution::Reject { .. })
                    })
                    .and_then(|entry| entry.rejection_message.clone()),
                InterruptionResolution::Approve { .. } => None,
            };
            self.set_interruption_answer(&id, derived, kept);
        }
        self.permission_rules.push(rule);
        Ok(())
    }

    fn set_interruption_answer(
        &mut self,
        item_id: &ItemId,
        resolution: InterruptionResolution,
        rejection_message: Option<String>,
    ) {
        if let Some(existing) = self
            .pending_interruption_resolutions
            .iter_mut()
            .find(|entry| entry.item_id == *item_id)
        {
            existing.resolution = resolution;
            existing.rejection_message = rejection_message;
        } else {
            self.pending_interruption_resolutions
                .push(PendingInterruptionResolution {
                    item_id: item_id.clone(),
                    resolution,
                    rejection_message,
                });
        }
    }

    /// The text a rejected interruption returns to the model: its own rejection text, or else the
    /// text of the sticky rejection of the same action, as the reference falls back from a call's
    /// exact message to the sticky one. `None` for an approval, or when neither carries text.
    #[must_use]
    pub fn rejection_message_for(&self, item_id: &ItemId) -> Option<&str> {
        let answer = self
            .pending_interruption_resolutions
            .iter()
            .find(|entry| entry.item_id == *item_id)?;
        if !matches!(answer.resolution, InterruptionResolution::Reject { .. }) {
            return None;
        }
        if let Some(message) = answer.rejection_message.as_deref() {
            return Some(message);
        }
        let RunItemKind::ToolApproval(approval) = self
            .generated_items
            .iter()
            .find(|stored| stored.id() == item_id)?
            .kind()
        else {
            return None;
        };
        let identity = session_rule(approval, answer.resolution);
        self.permission_rules
            .iter()
            .rev()
            .find(|rule| targets_same_action(rule, &identity))
            .filter(|rule| matches!(rule.decision(), PermissionDecision::Deny))
            .and_then(PermissionRule::rejection_message)
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

    /// Consumes an in-call answer before dispatch, as the reference records execution before
    /// invoking an approved tool. Kept after failures so restoring their checkpoint cannot replay
    /// side effects. A subsequent question has its own item identity and remains executable.
    #[doc(hidden)]
    pub fn begin_tool_continuation(&mut self, item_id: &ItemId) -> Result<()> {
        if self.started_tool_continuations.contains(item_id) {
            return Err(Error::caller(
                "the approved tool continuation has already executed",
            ));
        }
        if !self
            .pending_interruption_resolutions
            .iter()
            .any(|answer| &answer.item_id == item_id)
        {
            return Err(Error::caller("the tool continuation has no host answer"));
        }
        self.started_tool_continuations.push(item_id.clone());
        Ok(())
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

    /// What this run's input guardrails concluded, in the order their verdicts arrived.
    ///
    /// **Arrival order, not registration order.** Checks are collected as each completes, and the
    /// blocking half of the stage finishes before the raced half starts, so nothing about this
    /// list says which declaration produced which verdict. A reader that needs that has to be
    /// given it explicitly; a name repeated across two registrations appears twice here.
    #[must_use]
    pub fn input_guardrail_results(&self) -> &[InputGuardrailResult] {
        &self.input_guardrail_results
    }

    /// What this run's output guardrails concluded, in the order their verdicts arrived.
    #[must_use]
    pub fn output_guardrail_results(&self) -> &[OutputGuardrailResult] {
        &self.output_guardrail_results
    }

    /// Whether a stop hook has already requested continuation in this logical run.
    #[must_use]
    pub const fn stop_hook_active(&self) -> bool {
        self.stop_hook_active
    }

    /// Records an accepted stop-hook continuation before the next model call.
    #[doc(hidden)]
    pub fn mark_stop_hook_active(&mut self) {
        self.stop_hook_active = true;
    }

    /// Explicit parent identity, present only for a host-created child run.
    #[must_use]
    pub const fn parent_run_id(&self) -> Option<&RunId> {
        self.parent_run_id.as_ref()
    }

    /// Assigns a parent before a run begins. Restored runs keep their recorded identity.
    #[doc(hidden)]
    pub fn assign_parent_run_id(&mut self, parent: RunId) -> Result<()> {
        if parent == self.run_id || self.current_agent.is_some() || self.parent_run_id.is_some() {
            return Err(Error::caller(
                "a parent run must be distinct and assigned once before the child starts",
            ));
        }
        self.parent_run_id = Some(parent);
        Ok(())
    }

    /// Whether the child-start notification was completed in an earlier segment.
    #[must_use]
    pub const fn subagent_started(&self) -> bool {
        self.subagent_started
    }

    /// Prevents an approval resume from announcing the same child again.
    #[doc(hidden)]
    pub fn mark_subagent_started(&mut self) {
        self.subagent_started = true;
    }

    /// What this run's tool input guardrails concluded, in settlement order.
    ///
    /// One entry per completed check per call, so a tool checked on ten turns files ten sets. Each
    /// carries the tool and the call it examined, which is what tells two verdicts under the same
    /// identity apart.
    #[must_use]
    pub fn tool_input_guardrail_results(&self) -> &[ToolInputGuardrailResult] {
        &self.tool_input_guardrail_results
    }

    /// What this run's tool output guardrails concluded, in settlement order.
    #[must_use]
    pub fn tool_output_guardrail_results(&self) -> &[ToolOutputGuardrailResult] {
        &self.tool_output_guardrail_results
    }

    /// Records what one turn's tool input guardrails concluded.
    #[doc(hidden)]
    pub fn record_tool_input_guardrail_results(
        &mut self,
        results: impl IntoIterator<Item = ToolInputGuardrailResult>,
    ) {
        self.tool_input_guardrail_results.extend(results);
    }

    /// Records what one turn's tool output guardrails concluded.
    #[doc(hidden)]
    pub fn record_tool_output_guardrail_results(
        &mut self,
        results: impl IntoIterator<Item = ToolOutputGuardrailResult>,
    ) {
        self.tool_output_guardrail_results.extend(results);
    }

    /// Whether blocking input checks passed and the raced stage was admitted.
    ///
    /// The mark is independent of verdict names and counts because names need not be unique.
    /// It stays false when blocking checks time out, so a continuation must retry them before
    /// calling the model. Once true, pending raced checks are not retried on continuation.
    /// This separates the pre-model blocking requirement from first-turn racing semantics;
    /// resumable budget stops can occur before this runtime has made its first model call.
    #[must_use]
    pub const fn input_guardrails_started(&self) -> bool {
        self.input_guardrails_started
    }

    /// Marks blocking checks as passed and admits the raced stage.
    ///
    /// Call only after all blocking checks pass, even when there are no raced checks.
    #[doc(hidden)]
    pub const fn mark_input_guardrails_started(&mut self) {
        self.input_guardrails_started = true;
    }

    /// Records what one input-guardrail stage concluded.
    ///
    /// Appends rather than replaces: the blocking and raced halves report separately, and a
    /// continuation adds to what earlier segments recorded.
    #[doc(hidden)]
    pub fn record_input_guardrail_results(
        &mut self,
        results: impl IntoIterator<Item = InputGuardrailResult>,
    ) {
        self.input_guardrail_results.extend(results);
    }

    /// Records what one output-guardrail stage concluded.
    #[doc(hidden)]
    pub fn record_output_guardrail_results(
        &mut self,
        results: impl IntoIterator<Item = OutputGuardrailResult>,
    ) {
        self.output_guardrail_results.extend(results);
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
    ///
    /// Supplying input also **discards a projection a handoff installed**, and that is the honest
    /// reading of what the caller did: the two are answers to the same question, and keeping the
    /// framework's answer would silently overrule the one the caller just gave — while keeping both
    /// would send the caller's input on top of a history it had already replaced.
    #[doc(hidden)]
    pub fn begin_segment(&mut self, agent: AgentId, input: Vec<ModelInputItem>) -> Result<()> {
        // Resume may settle only some approvals. The runner returns the remaining questions
        // before another model call; entering a segment must not prevent that settlement.
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
                    self.handoff_projection = None;
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

    /// Installs the model input a settled transfer of control hands the receiving agent.
    ///
    /// # Errors
    ///
    /// Returns a caller error when the projection resumes after a record this run never generated.
    /// Accepting it would produce a state whose next request cannot be built at all, and the
    /// failure would surface one turn later with nothing left to say which transfer caused it.
    #[doc(hidden)]
    pub fn install_handoff_projection(&mut self, projection: HandoffProjection) -> Result<()> {
        if projection.is_from_start() {
            if !self.generated_items.is_empty() {
                return Err(Error::caller(
                    "a transfer of control projected history from the start of a run that has \
                     already stored records",
                ));
            }
            self.handoff_projection = Some(projection);
            return Ok(());
        }
        if !self
            .generated_items
            .iter()
            .any(|item| item.id() == projection.resume_after())
        {
            return Err(Error::caller(format!(
                "a transfer of control projected history up to record `{}`, which this run never \
                 generated",
                projection.resume_after()
            )));
        }
        self.handoff_projection = Some(projection);
        Ok(())
    }

    /// Replaces the stored records from `start` on with the records a handoff filter kept.
    ///
    /// Only records the session has not received yet can be replaced, as the reference's filter
    /// cannot unpersist history an earlier write already delivered. The replacement must still hold
    /// every record a pending question, answer or continuation names, and the record an installed
    /// projection resumes after.
    #[doc(hidden)]
    pub fn replace_unpersisted_records(
        &mut self,
        start: usize,
        records: Vec<RunItem>,
    ) -> Result<()> {
        if start > self.generated_items.len()
            || start < self.session_persisted_item_count.unwrap_or(0)
            || self.pending_session_write.is_some()
        {
            return Err(Error::caller(
                "only records the session has not received yet can be replaced",
            ));
        }
        let mut replaced = self.generated_items[..start].to_vec();
        replaced.extend(records);
        let mut seen = std::collections::BTreeSet::new();
        if let Some(duplicate) = replaced.iter().find(|item| !seen.insert(item.id().clone())) {
            return Err(Error::caller(format!(
                "replacement history stores record `{}` more than once",
                duplicate.id()
            )));
        }
        let referenced = self
            .pending_interruptions
            .iter()
            .chain(&self.started_tool_continuations)
            .chain(
                self.pending_interruption_resolutions
                    .iter()
                    .map(PendingInterruptionResolution::item_id),
            )
            .chain(
                self.handoff_projection
                    .iter()
                    .filter(|projection| !projection.is_from_start())
                    .map(HandoffProjection::resume_after),
            );
        for id in referenced {
            if !seen.contains(id) {
                return Err(Error::caller(format!(
                    "replacement history drops record `{id}`, which the run still refers to"
                )));
            }
        }
        if let Some(pending) = &self.pending_handoff {
            pending.validate(&replaced)?;
        }
        self.generated_items = replaced;
        Ok(())
    }

    /// Appends settled records to the checkpoint's resume projection.
    #[doc(hidden)]
    pub fn record_generated_items(&mut self, items: impl IntoIterator<Item = RunItem>) {
        self.generated_items.extend(items);
    }

    /// Adds a subsequent tool question without discarding answers to other pending questions.
    #[doc(hidden)]
    pub fn push_pending_interruption(&mut self, item: RunItem) -> Result<()> {
        if !matches!(item.kind(), RunItemKind::ToolApproval(_))
            || self
                .generated_items
                .iter()
                .any(|stored| stored.id() == item.id())
        {
            return Err(Error::caller(
                "a subsequent interruption must be a new tool approval",
            ));
        }
        self.pending_interruptions.push(item.id().clone());
        self.generated_items.push(item);
        Ok(())
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

/// Strips mount authority from a checkpoint's sandbox envelope.
///
/// Judged against the built-in entry types. A checkpoint is read before any host registry is
/// known, so a custom entry that looks like a mount is refused here rather than trusted; one that
/// does not is carried as it is, for the client that reads it to route.
fn sanitize_sandbox_envelope(payload: &serde_json::Value) -> Result<serde_json::Value> {
    sanitize_run_state_sandbox_mount_authority(payload, &builtin_entry_registry())
        .map(|(sanitized, _)| sanitized)
        .map_err(|error| Error::caller(error.to_string()))
}

/// Writes the sandbox envelope with mount authority stripped once more.
///
/// The field is sanitized on every way in, so this only repeats that; it is here so the one place
/// a checkpoint leaves the process does not depend on every way in having been found.
// The signature is serde's: `serialize_with` hands the field by reference.
#[allow(clippy::ref_option)]
fn serialize_sandbox_envelope<S: serde::Serializer>(
    sandbox: &Option<serde_json::Value>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let sanitized = sandbox
        .as_ref()
        .map(sanitize_sandbox_envelope)
        .transpose()
        .map_err(|error| <S::Error as serde::ser::Error>::custom(error.user_message()))?;
    sanitized.serialize(serializer)
}

/// Checks that every named interruption resolves to an interruption record the run generated.
///
/// The same check runs on the way in from a checkpoint and on the way in from settlement. They
/// produce the same value, and a rule enforced on only one of them is a rule a restart walks
/// around: an ID naming nothing leaves the run permanently unresumable, and one naming an ordinary
/// message leaves it waiting for an answer to a question nobody was asked.
/// A paused agent-tool run this state can resume: its own checkpoint under this run, the key
/// that routes the call again, something still pending, and one reference per call.
///
/// A reference without a checkpoint is the persistent identity alone, which is all this slot held
/// before paused runs were recorded; it has nothing to resume and is let through as it always was.
fn validate_nested_runs(nested_runs: &[NestedRunRef], run_id: &RunId) -> Result<()> {
    let mut calls = std::collections::BTreeSet::new();
    for nested in nested_runs {
        let call_id = nested.call_id();
        let Some(state) = nested.state() else {
            continue;
        };
        if !calls.insert(call_id) {
            return Err(Error::caller(format!(
                "run state records two paused agent-tool runs for call `{call_id}`"
            )));
        }
        if nested.lookup_key().is_none() {
            return Err(Error::caller(format!(
                "paused agent-tool run for call `{call_id}` has no tool routing identity to resume \
                 through"
            )));
        }
        if state.parent_run_id() != Some(run_id) {
            return Err(Error::caller(format!(
                "paused agent-tool run for call `{call_id}` belongs to another parent run than \
                 `{run_id}`"
            )));
        }
        if state.pending_interruption_items().next().is_none()
            && !state.has_session_write_checkpoint()
        {
            return Err(Error::caller(format!(
                "paused agent-tool run for call `{call_id}` is not waiting on anything"
            )));
        }
    }
    Ok(())
}

/// Checks a retained Session batch against the history it was cut from.
///
/// A batch is owed model-input records, settles to a count inside this history that does not move
/// backwards, and carries only well-formed tail digests: a checkpoint violating any of these would
/// make the next reconciliation append, skip, or compare the wrong records.
fn validate_pending_session_write(
    pending: &PendingSessionWrite,
    history_len: usize,
    persisted_count: Option<usize>,
) -> Result<()> {
    if (pending.append_acknowledged && pending.before.is_none())
        || pending.items.is_empty()
        || pending.items.iter().any(|item| !item.is_model_input())
        || pending.persisted_count > history_len
        || persisted_count.is_none_or(|count| pending.persisted_count < count)
        || pending.before.as_ref().is_some_and(|before| {
            before.iter().any(|digest| {
                digest.as_str().len() != 64
                    || !digest.as_str().bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
    {
        return Err(Error::caller("invalid pending Session write checkpoint"));
    }
    Ok(())
}

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

// A present null means explicitly unlimited, whereas a missing field is a legacy checkpoint.
#[allow(
    clippy::option_option,
    reason = "serde distinguishes a missing legacy field from explicit null"
)]
fn deserialize_turn_limit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<u32>>, D::Error> {
    Option::<u32>::deserialize(deserializer).map(Some)
}
