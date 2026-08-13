//! How each call turned out, kept as digests so a failing retry can be told from a stuck one.
//!
//! # Why this is not the call trail
//!
//! [`tool_use`](crate::state::tool_use) records what the model *asked for*, before any of it runs.
//! This records what came back. The two answer different questions and cannot be merged: the
//! attempt trail has to be complete before execution so the repeat breaker can see the turn it is
//! being asked about, and an outcome does not exist until after.
//!
//! # No progress is not the same as no change of arguments
//!
//! The repeat breaker asks "is this the same call again?" and reads the arguments. That question is
//! wrong for a retry: a model that fixes a path and fails on the next one has changed the call and
//! learned nothing, while a model that runs the same test command twice and gets a shorter failure
//! list has repeated the call and learned a great deal. So the counter here advances on the
//! **evidence**, not the input: a failure whose model-visible result is byte-for-byte what the
//! previous failure produced taught the run nothing, whatever the arguments were.
//!
//! Evidence is compared as the model sees it, which is what makes the degenerate case correct
//! rather than merely convenient. A tool that reports every failure as a bare code produces the
//! same evidence for two unrelated causes — and the model, which is the party being asked to change
//! its approach, genuinely cannot tell them apart either.
//!
//! # Success or refusal clears it
//!
//! Only failures advance the streak. A success on that identity resets it because the tool worked;
//! a refusal resets it because it must re-arm the breaker after telling the model to change course.
//! The counter is about a tool that cannot be made to work, not about a tool that is used a lot.
//! This differs from `GuardianRejectionCircuitBreaker`'s windowed second counter deliberately —
//! see [`ToolFailureEntry::no_progress_streak`] for why the blind spot that motivates a window does
//! not exist here.
//!
//! # What is retained
//!
//! Digests, counts, and a failure code. Never arguments, never a tool's output, never a prompt.
//! This value is written into every checkpoint, and the payload of a failing call is exactly where
//! a path, a query, or a credential a user pasted would show up.
//!
//! # The contract a result cache would have to meet
//!
//! Written here rather than left for whoever builds it, so the two cannot evolve apart. A cache
//! hit is normally a *success*, and successes clear this counter — so a run stuck asking for the
//! same cached answer would look like a run that keeps succeeding. Memoized results therefore have
//! to reach [`ToolFailureTracker::record_turn`] as an outcome that carries no new evidence, not as
//! a plain success. Everything needed to express that is already here: the hit repeats an
//! evidence fingerprint the entry has seen, which is the same condition a repeated failure meets.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    compat::{SchemaVersion, Unknown},
    item::{AgentId, CallId},
    state::tool_use::{
        ArgumentFingerprint, ToolUse, canonicalize, update_digest, update_identity_digest,
    },
};

/// Current tool-failure tracking schema version.
pub const TOOL_FAILURE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// How many recent outcomes are retained per agent and action identity.
///
/// Bounds the second tier of [`ToolFailureTracker::record_turn`]'s idempotence, exactly as
/// [`TOOL_USE_RECENT_LIMIT`](crate::state::TOOL_USE_RECENT_LIMIT) does for the call trail, and for
/// the same reason: an unbounded trail would grow with the run and be rewritten on every
/// checkpoint.
pub const TOOL_FAILURE_RECENT_LIMIT: usize = 8;

/// Digest of the model-visible result one call produced.
///
/// A separate type from [`ArgumentFingerprint`] even though both are hex SHA-256, because they
/// answer different questions — "same request?" versus "same answer?" — and one type for both would
/// let a comparison between them typecheck while meaning nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceFingerprint(String);

impl EvidenceFingerprint {
    /// Computes the digest of the result the model was shown.
    ///
    /// Normalized the same way arguments are: a provider does not promise key order, and hashing
    /// raw bytes would report "new evidence" for a re-serialized copy of the same answer — which is
    /// the one direction that must not happen, since it makes a stuck run look like a progressing
    /// one.
    #[must_use]
    pub fn compute(observation: &Value) -> Self {
        let digest = Sha256::digest(canonicalize(observation).to_string());
        Self(format!("{digest:x}"))
    }

    /// Lowercase hexadecimal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for EvidenceFingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One settled call to hand to [`ToolFailureTracker::record_turn`].
///
/// Both fingerprints are computed on construction rather than accepted from the caller, so two call
/// sites cannot digest the same value two different ways and produce a comparison that never
/// matches.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    identity: ToolUse,
    call_id: CallId,
    input: ArgumentFingerprint,
    evidence: EvidenceFingerprint,
    kind: OutcomeKind,
}

/// The three things that can become of one call, from this record's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutcomeKind {
    Succeeded,
    Failed(String),
    Refused,
}

impl ToolOutcome {
    /// Records a call that produced a usable result.
    #[must_use]
    pub fn succeeded(
        identity: ToolUse,
        call_id: CallId,
        arguments: &Value,
        observation: &Value,
    ) -> Self {
        Self {
            identity,
            call_id,
            input: ArgumentFingerprint::compute(arguments),
            evidence: EvidenceFingerprint::compute(observation),
            kind: OutcomeKind::Succeeded,
        }
    }

    /// Records a call that failed, under the stable code the model was shown.
    ///
    /// The code comes from the failure rather than from the rendered result because a tool that
    /// writes its own model-facing sentence still failed. Reading the rendered result instead would
    /// make the tools that explain themselves best the ones this record cannot see.
    #[must_use]
    pub fn failed(
        identity: ToolUse,
        call_id: CallId,
        arguments: &Value,
        observation: &Value,
        code: impl Into<String>,
    ) -> Self {
        Self {
            kind: OutcomeKind::Failed(code.into()),
            ..Self::succeeded(identity, call_id, arguments, observation)
        }
    }

    /// Records a call the runtime answered without running the tool.
    ///
    /// This is what keeps the breaker from becoming a death sentence. A refusal says nothing about
    /// the tool — nothing ran — so it cannot count as another failure; and leaving the streak
    /// standing would refuse every later call too, since only a result can lower it and no result
    /// can now be produced. So a refusal *clears* the streak: the model has been told it is going
    /// in circles, and the next attempt is judged on its own. A run that really is stuck fails its
    /// way back to the threshold and is told again, which costs it one call in every `limit + 1`
    /// instead of costing it the tool.
    #[must_use]
    pub fn refused(
        identity: ToolUse,
        call_id: CallId,
        arguments: &Value,
        observation: &Value,
    ) -> Self {
        Self {
            kind: OutcomeKind::Refused,
            ..Self::succeeded(identity, call_id, arguments, observation)
        }
    }

    /// The identity that was invoked.
    #[must_use]
    pub const fn identity(&self) -> &ToolUse {
        &self.identity
    }

    /// The pairing ID of this call.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Digest of the arguments it carried.
    #[must_use]
    pub const fn input(&self) -> &ArgumentFingerprint {
        &self.input
    }

    /// Digest of the result the model was shown.
    #[must_use]
    pub const fn evidence(&self) -> &EvidenceFingerprint {
        &self.evidence
    }

    /// The failure code, or `None` when the call did not run or produced a usable result.
    #[must_use]
    pub fn failure_code(&self) -> Option<&str> {
        match &self.kind {
            OutcomeKind::Failed(code) => Some(code),
            OutcomeKind::Succeeded | OutcomeKind::Refused => None,
        }
    }

    /// Whether the runtime answered this call without running the tool.
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        matches!(self.kind, OutcomeKind::Refused)
    }
}

/// One retained failure.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailureRecord {
    #[serde(default = "tool_failure_schema_version")]
    schema_version: SchemaVersion,
    call_id: CallId,
    input: ArgumentFingerprint,
    evidence: EvidenceFingerprint,
    code: String,
    #[serde(default)]
    new_evidence: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolFailureRecord {
    /// Pairing ID of the failed call.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Digest of the arguments it carried.
    #[must_use]
    pub const fn input(&self) -> &ArgumentFingerprint {
        &self.input
    }

    /// Digest of the result the model was shown.
    #[must_use]
    pub const fn evidence(&self) -> &EvidenceFingerprint {
        &self.evidence
    }

    /// Stable code of the failure.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Whether this failure showed the model something the previous one had not.
    ///
    /// Retained rather than re-derived from the neighbouring records: the comparison is against the
    /// previous failure of the same identity, which the bounded window may no longer hold.
    #[must_use]
    pub const fn new_evidence(&self) -> bool {
        self.new_evidence
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// How one agent has fared with one action identity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailureEntry {
    #[serde(default = "tool_failure_schema_version")]
    schema_version: SchemaVersion,
    identity: ToolUse,
    #[serde(default)]
    run_failures: u32,
    #[serde(default)]
    no_progress_streak: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_evidence: Option<EvidenceFingerprint>,
    #[serde(default)]
    recent: Vec<ToolFailureRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_outcome_call_ids: Vec<CallId>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolFailureEntry {
    fn new(identity: ToolUse) -> Self {
        Self {
            schema_version: TOOL_FAILURE_SCHEMA_VERSION,
            identity,
            run_failures: 0,
            no_progress_streak: 0,
            last_evidence: None,
            recent: Vec::new(),
            recent_outcome_call_ids: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// The action identity these counts belong to.
    #[must_use]
    pub const fn identity(&self) -> &ToolUse {
        &self.identity
    }

    /// How many times it failed during the run.
    #[must_use]
    pub const fn run_failures(&self) -> u32 {
        self.run_failures
    }

    /// Consecutive failures that told the run the same story, counting the latest one.
    ///
    /// A first failure reads `1`, exactly as a first call reads `1` in the call trail: the number
    /// is how long the current run of failures is, so a threshold of three means three failures in
    /// a row whose result the model could not tell apart. Zero means it has never failed.
    ///
    /// Three things reset it and nothing else does: a success, a breaker refusal that re-arms the
    /// tool, and a failure carrying evidence different from the previous failure's. A call to a
    /// *different* tool in between does not, because a run alternating between two hopeless calls
    /// is stuck on both.
    ///
    /// # Why there is no second, windowed counter
    ///
    /// `GuardianRejectionCircuitBreaker` pairs its consecutive count with a fixed window, and the
    /// blind spot that justifies the pair is real — a strict consecutive reading misses a loop that
    /// something interrupts. It does not exist here. The interruption that would reset this counter
    /// is a *success on this same identity*, and a tool that intermittently works is not a tool the
    /// run is stuck on. A refusal is a control-flow re-arm rather than evidence about the tool.
    /// Adding a window would mean firing on an identity that has been working for the last five
    /// calls because of three older failures still inside it.
    #[must_use]
    pub const fn no_progress_streak(&self) -> u32 {
        self.no_progress_streak
    }

    /// Digest of the last failure's result, or `None` before the first failure.
    #[must_use]
    pub const fn last_evidence(&self) -> Option<&EvidenceFingerprint> {
        self.last_evidence.as_ref()
    }

    /// The retained failures, oldest first.
    #[must_use]
    pub fn recent(&self) -> &[ToolFailureRecord] {
        &self.recent
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Whether this settled call was already recorded, within the retained window.
    fn already_recorded(&self, call_id: &CallId) -> bool {
        self.recent_outcome_call_ids
            .iter()
            .any(|recorded| recorded == call_id)
            // Check the older failure-only representation too, so restoring a checkpoint written
            // before successful and refused calls were retained stays idempotent.
            || self.recent.iter().any(|record| record.call_id() == call_id)
    }

    /// Ends whatever streak was running, without leaving a record.
    ///
    /// A success ends it because the tool worked. A refusal ends it because nothing ran, and a
    /// streak that survived its own refusal could never be lowered again.
    fn clear_streak(&mut self) {
        self.no_progress_streak = 0;
        self.last_evidence = None;
    }

    fn record_failure(
        &mut self,
        call_id: &CallId,
        input: ArgumentFingerprint,
        evidence: EvidenceFingerprint,
        code: String,
    ) {
        let new_evidence = self.last_evidence.as_ref() != Some(&evidence);
        self.no_progress_streak = if new_evidence {
            1
        } else {
            self.no_progress_streak.saturating_add(1)
        };
        self.run_failures = self.run_failures.saturating_add(1);
        self.last_evidence = Some(evidence.clone());
        self.recent.push(ToolFailureRecord {
            schema_version: TOOL_FAILURE_SCHEMA_VERSION,
            call_id: call_id.clone(),
            input,
            evidence,
            code,
            new_evidence,
            unknown: Unknown::new(),
        });
        // A record written by a build with a larger window is read back whole rather than truncated
        // on arrival; it is trimmed here, when this build appends to it.
        while self.recent.len() > TOOL_FAILURE_RECENT_LIMIT {
            self.recent.remove(0);
        }
    }

    fn record_outcome_call_id(&mut self, call_id: CallId) {
        self.recent_outcome_call_ids.push(call_id);
        // A record written by a build with a larger window is read back whole rather than truncated
        // on arrival; it is trimmed here, when this build appends to it.
        while self.recent_outcome_call_ids.len() > TOOL_FAILURE_RECENT_LIMIT {
            self.recent_outcome_call_ids.remove(0);
        }
    }
}

/// Everything one agent's calls produced, across every identity it invoked.
///
/// Entries stay in first-failure order, which is deterministic — checkpoint bytes require that —
/// and reads as the run's actual history rather than as an alphabetical list.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentToolFailures {
    schema_version: SchemaVersion,
    #[serde(default)]
    entries: Vec<ToolFailureEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_turn_fingerprint: Option<OutcomeTurnFingerprint>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentToolFailures {
    fn new() -> Self {
        Self {
            schema_version: TOOL_FAILURE_SCHEMA_VERSION,
            entries: Vec::new(),
            last_turn_fingerprint: None,
            unknown: Unknown::new(),
        }
    }

    /// Every identity that has produced an outcome, in first-outcome order.
    #[must_use]
    pub fn entries(&self) -> &[ToolFailureEntry] {
        &self.entries
    }

    /// The record for one identity.
    #[must_use]
    pub fn entry(&self, identity: &ToolUse) -> Option<&ToolFailureEntry> {
        self.entries
            .iter()
            .find(|entry| entry.identity() == identity)
    }

    /// Total failures this agent produced during the run.
    #[must_use]
    pub fn run_failures(&self) -> u32 {
        self.entries
            .iter()
            .fold(0, |total, entry| total.saturating_add(entry.run_failures()))
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    fn entry_mut(&mut self, identity: &ToolUse) -> &mut ToolFailureEntry {
        let existing = self
            .entries
            .iter()
            .position(|entry| entry.identity() == identity);
        let position = if let Some(position) = existing {
            position
        } else {
            self.entries.push(ToolFailureEntry::new(identity.clone()));
            self.entries.len() - 1
        };
        &mut self.entries[position]
    }
}

#[derive(Deserialize)]
struct AgentToolFailuresWire {
    #[serde(default = "tool_failure_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    entries: Vec<ToolFailureEntry>,
    #[serde(default)]
    last_turn_fingerprint: Option<OutcomeTurnFingerprint>,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for AgentToolFailures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = AgentToolFailuresWire::deserialize(deserializer)?;
        // Two entries for one identity split its counts, and every consumer reads whichever one it
        // finds first. The breaker would then need twice the failures to fire, on a record that
        // looks perfectly well formed.
        for (position, entry) in wire.entries.iter().enumerate() {
            if wire.entries[..position]
                .iter()
                .any(|earlier| earlier.identity() == entry.identity())
            {
                return Err(D::Error::custom(format!(
                    "tool-failure identity {:?} appears in more than one entry; its counts would \
                     be split across them",
                    entry.identity()
                )));
            }
        }
        Ok(Self {
            schema_version: wire.schema_version,
            entries: wire.entries,
            last_turn_fingerprint: wire.last_turn_fingerprint,
            unknown: wire.unknown,
        })
    }
}

/// Per-agent failure history for one run.
///
/// Serializable in full, so a run restored from a checkpoint keeps the streaks it had when it was
/// paused. Restoring an empty tracker would make "pause and continue" the way to defeat the
/// breaker, and the loop it was about to stop would run again from zero.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailureTracker {
    #[serde(default = "tool_failure_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    agents: BTreeMap<AgentId, AgentToolFailures>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for ToolFailureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolFailureTracker {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: TOOL_FAILURE_SCHEMA_VERSION,
            agents: BTreeMap::new(),
            unknown: Unknown::new(),
        }
    }

    /// Records how one agent's turn turned out.
    ///
    /// One call per settled response, after execution — the mirror of the call trail's recording
    /// point, which is before it. Outcomes arrive in the response's order. A call still awaiting
    /// approval is absent because it has not turned out any way yet. A breaker refusal is present
    /// as [`ToolOutcome::refused`]: it is not a failure, but it clears the streak so the refusal
    /// does not permanently retire the tool.
    ///
    /// # Recording the same turn twice changes nothing
    ///
    /// Resuming an interruption settles the same response again, and a double count would let the
    /// breaker fire at half its configured threshold — on a record that looks correct. The two
    /// tiers are the ones the call trail uses, for the reasons documented there: a digest of the
    /// whole ordered turn catches the immediate re-settle at any length, and a `call_id` still
    /// inside an identity's retained outcome window catches an older one. The latter covers
    /// successes and refusals too: replaying an old reset after a newer failure must not erase
    /// that newer evidence.
    pub fn record_turn(
        &mut self,
        agent: &AgentId,
        outcomes: impl IntoIterator<Item = ToolOutcome>,
    ) {
        let outcomes = outcomes.into_iter().collect::<Vec<_>>();
        let turn_fingerprint = fingerprint_outcomes(&outcomes);
        let agent_failures = self
            .agents
            .entry(agent.clone())
            .or_insert_with(AgentToolFailures::new);
        let replay = agent_failures.last_turn_fingerprint.as_ref() == Some(&turn_fingerprint);

        for outcome in outcomes {
            let ToolOutcome {
                identity,
                call_id,
                input,
                evidence,
                kind,
            } = outcome;
            let entry = agent_failures.entry_mut(&identity);
            if replay || entry.already_recorded(&call_id) {
                continue;
            }
            match kind {
                OutcomeKind::Succeeded | OutcomeKind::Refused => entry.clear_streak(),
                OutcomeKind::Failed(code) => {
                    entry.record_failure(&call_id, input, evidence, code);
                }
            }
            entry.record_outcome_call_id(call_id);
        }
        agent_failures.last_turn_fingerprint = Some(turn_fingerprint);
    }

    /// Every agent that has settled a turn, in agent-ID order.
    pub fn agents(&self) -> impl Iterator<Item = (&AgentId, &AgentToolFailures)> {
        self.agents.iter()
    }

    /// One agent's history.
    #[must_use]
    pub fn agent(&self, agent: &AgentId) -> Option<&AgentToolFailures> {
        self.agents.get(agent)
    }

    /// How long this identity's current run of same-answer failures is.
    ///
    /// Zero when the identity has never failed, and the value the no-progress breaker compares
    /// against its threshold. Reading it here rather than re-deriving it per call site is what
    /// keeps "what counts as progress" a single definition.
    #[must_use]
    pub fn no_progress_streak(&self, agent: &AgentId, identity: &ToolUse) -> u32 {
        self.agent(agent)
            .and_then(|failures| failures.entry(identity))
            .map_or(0, ToolFailureEntry::no_progress_streak)
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Digest of one whole ordered turn of outcomes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct OutcomeTurnFingerprint(String);

/// Hashes an ordered turn without retaining a second copy of its digests.
fn fingerprint_outcomes(outcomes: &[ToolOutcome]) -> OutcomeTurnFingerprint {
    let mut digest = Sha256::new();
    for outcome in outcomes {
        update_identity_digest(&mut digest, outcome.identity());
        update_digest(&mut digest, b"call_id", outcome.call_id().as_str());
        update_digest(&mut digest, b"input", outcome.input().as_str());
        update_digest(&mut digest, b"evidence", outcome.evidence().as_str());
        let kind: &[u8] = match &outcome.kind {
            OutcomeKind::Succeeded => b"succeeded",
            OutcomeKind::Failed(_) => b"failed",
            OutcomeKind::Refused => b"refused",
        };
        update_digest(&mut digest, b"kind", kind);
        update_digest(&mut digest, b"code", outcome.failure_code().unwrap_or(""));
    }
    OutcomeTurnFingerprint(format!("{:x}", digest.finalize()))
}

const fn tool_failure_schema_version() -> SchemaVersion {
    TOOL_FAILURE_SCHEMA_VERSION
}
