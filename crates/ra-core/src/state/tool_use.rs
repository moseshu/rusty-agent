//! R3-6b: what each agent has already asked for, recorded on identities that cannot collide.
//!
//! Four later stages ask a question about the past rather than about this response:
//! `reset_tool_choice` asks whether the agent used anything at all this turn, the semantic loop
//! breaker asks whether the same call is being repeated, tool-use behavior asks which tools have
//! run, and an audit asks for the whole trail. Answering each from a different ad-hoc counter is how
//! four subsystems end up disagreeing about what happened, so all four read this one record.
//!
//! # Why neither key is a name
//!
//! The map is keyed by [`AgentId`], never by an agent's display name — `AgentSpec` deliberately lets
//! names collide. Inside an agent, calls are keyed by [`ToolUse`], which carries a
//! [`ToolLookupKey`] rather than a model-facing tool name: two MCP servers may both expose `search`,
//! and folding them into one counter would make the breaker fire on two unrelated tools while
//! missing a real repeat. This is the "不能只按可重名的 agent name 或 tool name 统计" constraint of
//! the milestone, expressed in the type rather than in a comment.
//!
//! # Why the arguments are hashed and the history is bounded
//!
//! Only an [`ArgumentFingerprint`] is retained, never the arguments themselves. Tool arguments carry
//! whatever the model decided to put in them — paths, queries, credentials a user pasted — and this
//! structure is written into every `RunState` checkpoint. A digest answers the only question the
//! consumers actually ask ("same call as last time?") and takes the payload out of the checkpoint
//! entirely.
//!
//! Per-identity history is capped at [`TOOL_USE_RECENT_LIMIT`]. An unbounded trail would grow with
//! every turn of a long run and be re-serialized on every checkpoint write. [`ToolUseEntry::run_calls`]
//! keeps the total that the window can no longer show, and [`ToolUseEntry::repeat_streak`] is stored
//! for the same reason: it is a fold over the complete sequence, which is deliberately not retained.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    compat::{SchemaVersion, Unknown},
    item::{AgentId, CallId},
    tool::{ToolLookupKey, ToolLookupKind},
};

/// Current tool-use tracking schema version.
pub const TOOL_USE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// How many recent calls are retained per agent and action identity.
///
/// This bounds the **second** tier of [`ToolUseTracker::record_turn`]'s idempotence, not the one
/// that matters for a resume: a call id re-recorded more than this many calls of the same identity
/// later is no longer recognized. Re-settling the turn that was just recorded is covered without a
/// length limit — see [`ToolUseTracker::record_turn`] on why one tier was not enough.
pub const TOOL_USE_RECENT_LIMIT: usize = 8;

/// One action identity the model invoked during a turn.
///
/// A plain name would not do: a namespaced tool and a bare tool can legitimately share a
/// model-facing name across agents, and counting them as one is exactly the mistake R3-6b names.
///
/// This type lives beside the tracker rather than beside the classification that produces it
/// because persistence is what pins its wire format. The settlement intermediates in
/// [`step`](crate::step) are graded `Internal` and may be refactored at any time; a value that a
/// saved `RunState` has to be readable with cannot be.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolUse {
    /// A local tool, identified by its collision-free routing key.
    Tool(ToolLookupKey),
    /// A control transfer to another agent.
    Handoff(AgentId),
    /// A hosted MCP tool on a named server.
    Mcp {
        /// Registered server name.
        server: String,
        /// Tool name on that server.
        tool_name: String,
    },
    /// A name the turn did not advertise. It still counts as an attempt, which is what
    /// `reset_tool_choice` (R3-6) reacts to.
    Unresolved(String),
}

/// Digest of one call's normalized arguments.
///
/// Normalization matters more than it looks: providers do not promise a key order, so the same
/// logical call can arrive as two different JSON strings across turns. Hashing the raw bytes would
/// make the repeat detector answer "different call" for a model that is doing the very thing the
/// detector exists to catch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArgumentFingerprint(String);

impl ArgumentFingerprint {
    /// Computes the digest of a call's arguments.
    #[must_use]
    pub fn compute(arguments: &Value) -> Self {
        let digest = Sha256::digest(canonicalize(arguments).to_string());
        Self(format!("{digest:x}"))
    }

    /// Lowercase hexadecimal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ArgumentFingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rebuilds a value with every object's keys in lexical order.
///
/// The sort is not redundant with `serde_json`'s default `BTreeMap` backing. `preserve_order` is an
/// additive feature: any crate anywhere in the dependency graph can switch `Map` to an `IndexMap`,
/// and the fingerprints of an entire deployment would then change with the linked feature set
/// rather than with the data. Sorting here makes the digest a property of the value.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by_key(|(key, _)| *key);
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, nested)| (key.clone(), canonicalize(nested)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// One call to hand to [`ToolUseTracker::record_turn`].
///
/// The fingerprint is computed on construction rather than accepted from the caller, so two call
/// sites cannot hash the same arguments two different ways and produce a repeat that never matches.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseAttempt {
    identity: ToolUse,
    call_id: CallId,
    fingerprint: ArgumentFingerprint,
}

impl ToolUseAttempt {
    /// Records one invoked identity and the arguments it carried.
    #[must_use]
    pub fn new(identity: ToolUse, call_id: CallId, arguments: &Value) -> Self {
        Self {
            identity,
            call_id,
            fingerprint: ArgumentFingerprint::compute(arguments),
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
    pub const fn fingerprint(&self) -> &ArgumentFingerprint {
        &self.fingerprint
    }
}

/// One retained call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUseRecord {
    #[serde(default = "tool_use_schema_version")]
    schema_version: SchemaVersion,
    call_id: CallId,
    fingerprint: ArgumentFingerprint,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolUseRecord {
    /// Pairing ID of the call.
    ///
    /// For a [`ToolUse::Mcp`] identity this is the approval `request_id`: the hosted path pairs on
    /// that string, and [`CallId`] is the framework's pairing ID for a tool, handoff, or MCP call
    /// alike. Nothing reads it as a local call id, because the identity beside it says it is hosted.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Digest of the arguments the call carried.
    #[must_use]
    pub const fn fingerprint(&self) -> &ArgumentFingerprint {
        &self.fingerprint
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

/// Everything one agent did with one action identity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUseEntry {
    #[serde(default = "tool_use_schema_version")]
    schema_version: SchemaVersion,
    identity: ToolUse,
    #[serde(default)]
    run_calls: u32,
    #[serde(default)]
    turn_calls: u32,
    #[serde(default)]
    repeat_streak: u32,
    #[serde(default)]
    recent: Vec<ToolUseRecord>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolUseEntry {
    fn new(identity: ToolUse) -> Self {
        Self {
            schema_version: TOOL_USE_SCHEMA_VERSION,
            identity,
            run_calls: 0,
            turn_calls: 0,
            repeat_streak: 0,
            recent: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// The action identity these counts belong to.
    #[must_use]
    pub const fn identity(&self) -> &ToolUse {
        &self.identity
    }

    /// How many times this run invoked it.
    #[must_use]
    pub const fn run_calls(&self) -> u32 {
        self.run_calls
    }

    /// How many times the most recently recorded turn invoked it.
    #[must_use]
    pub const fn turn_calls(&self) -> u32 {
        self.turn_calls
    }

    /// How many consecutive calls carried the same arguments, counting the latest one.
    ///
    /// The sequence being counted is this identity's own call sequence, not the agent's. A model
    /// stuck alternating `read(f)` and `grep(x)` repeats both of them, and a streak that reset
    /// whenever a *different* tool intervened would report `1` for each and see no loop at all.
    ///
    /// Stored rather than derived because it folds over the whole run, while [`Self::recent`] is
    /// capped at [`TOOL_USE_RECENT_LIMIT`]. This is the one place in the type where a second source
    /// of truth is accepted, and the alternative was retaining an unbounded trail in every
    /// checkpoint.
    #[must_use]
    pub const fn repeat_streak(&self) -> u32 {
        self.repeat_streak
    }

    /// The retained calls, oldest first.
    #[must_use]
    pub fn recent(&self) -> &[ToolUseRecord] {
        &self.recent
    }

    /// Arguments of the latest call, or `None` before the first one.
    #[must_use]
    pub fn last_fingerprint(&self) -> Option<&ArgumentFingerprint> {
        self.recent.last().map(ToolUseRecord::fingerprint)
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

    /// Whether this call was already recorded, within the retained window.
    fn already_recorded(&self, call_id: &CallId) -> bool {
        self.recent.iter().any(|record| record.call_id() == call_id)
    }

    fn record(&mut self, call_id: CallId, fingerprint: ArgumentFingerprint) {
        self.repeat_streak = if self.last_fingerprint() == Some(&fingerprint) {
            self.repeat_streak.saturating_add(1)
        } else {
            1
        };
        self.run_calls = self.run_calls.saturating_add(1);
        self.recent.push(ToolUseRecord {
            schema_version: TOOL_USE_SCHEMA_VERSION,
            call_id,
            fingerprint,
            unknown: Unknown::new(),
        });
        // A record written by a build with a larger window is read back whole rather than truncated
        // on arrival; it is trimmed here, when this build appends to it.
        while self.recent.len() > TOOL_USE_RECENT_LIMIT {
            self.recent.remove(0);
        }
    }
}

/// Everything one agent did, across every identity it invoked.
///
/// Entries stay in first-use order. That is deterministic, which checkpoint bytes require, and it
/// also reads as the agent's actual history rather than as an alphabetical list.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentToolUse {
    schema_version: SchemaVersion,
    #[serde(default)]
    entries: Vec<ToolUseEntry>,
    // `recent` is deliberately small, but a response itself is not constrained to eight calls. A
    // digest of the immediately preceding whole turn makes re-settling that response idempotent
    // even when its calls have already displaced one another from per-entry `recent` windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_turn_fingerprint: Option<TurnFingerprint>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentToolUse {
    fn new() -> Self {
        Self {
            schema_version: TOOL_USE_SCHEMA_VERSION,
            entries: Vec::new(),
            last_turn_fingerprint: None,
            unknown: Unknown::new(),
        }
    }

    /// Every identity this agent invoked, in first-use order.
    #[must_use]
    pub fn entries(&self) -> &[ToolUseEntry] {
        &self.entries
    }

    /// The record for one identity.
    #[must_use]
    pub fn entry(&self, identity: &ToolUse) -> Option<&ToolUseEntry> {
        self.entries
            .iter()
            .find(|entry| entry.identity() == identity)
    }

    /// Total calls this agent made during the run.
    #[must_use]
    pub fn run_calls(&self) -> u32 {
        self.entries
            .iter()
            .fold(0, |total, entry| total.saturating_add(entry.run_calls()))
    }

    /// Total calls the most recently recorded turn made.
    #[must_use]
    pub fn turn_calls(&self) -> u32 {
        self.entries
            .iter()
            .fold(0, |total, entry| total.saturating_add(entry.turn_calls()))
    }

    /// Whether the agent asked for anything at all this turn.
    ///
    /// This is the question `reset_tool_choice` (R3-6) answers: a forced `tool_choice` that stays
    /// forced after the model complied is how a run spends every remaining turn calling the same
    /// tool. Unresolved names count — the model tried.
    #[must_use]
    pub fn used_any_this_turn(&self) -> bool {
        self.entries.iter().any(|entry| entry.turn_calls() > 0)
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

    fn entry_mut(&mut self, identity: &ToolUse) -> &mut ToolUseEntry {
        let existing = self
            .entries
            .iter()
            .position(|entry| entry.identity() == identity);
        let position = if let Some(position) = existing {
            position
        } else {
            self.entries.push(ToolUseEntry::new(identity.clone()));
            self.entries.len() - 1
        };
        &mut self.entries[position]
    }
}

#[derive(Deserialize)]
struct AgentToolUseWire {
    #[serde(default = "tool_use_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    entries: Vec<ToolUseEntry>,
    #[serde(default)]
    last_turn_fingerprint: Option<TurnFingerprint>,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for AgentToolUse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = AgentToolUseWire::deserialize(deserializer)?;
        // Two entries for one identity split its counts in half, and every consumer reads whichever
        // one it happens to find first. The breaker would then need twice the repeats to fire, on a
        // record that looks perfectly well formed.
        for (position, entry) in wire.entries.iter().enumerate() {
            if wire.entries[..position]
                .iter()
                .any(|earlier| earlier.identity() == entry.identity())
            {
                return Err(D::Error::custom(format!(
                    "tool-use identity {:?} appears in more than one entry; its counts would be \
                     split across them",
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

/// Per-agent tool-use history for one run.
///
/// Serializable in full, so a run restored from a checkpoint keeps answering the same four questions
/// it answered before it was paused. Restoring an empty tracker instead would reset every repeat
/// streak on resume, which turns "pause and continue" into a way to defeat the loop breaker.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUseTracker {
    #[serde(default = "tool_use_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    agents: BTreeMap<AgentId, AgentToolUse>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for ToolUseTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolUseTracker {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: TOOL_USE_SCHEMA_VERSION,
            agents: BTreeMap::new(),
            unknown: Unknown::new(),
        }
    }

    /// Records everything one agent asked for in one turn.
    ///
    /// One call per settled response, which is what makes the per-turn counts correct without a
    /// separate `begin_turn` that a caller can forget: this method *is* the turn boundary. Attempts
    /// arrive in response order, and the agent's other identities have their per-turn counts cleared
    /// — an identity the model stopped calling has to read as zero this turn, not as its last value.
    ///
    /// # Recording the same turn twice changes nothing
    ///
    /// Settling one response twice is the ordinary shape of resuming an interruption, and a double
    /// count there would make the model look exactly twice as repetitive as it was — on the number
    /// R3-6's breaker acts on. Two tiers guarantee it does not:
    ///
    /// 1. **The turn just recorded, at any length.** A digest of the whole ordered turn is kept per
    ///    agent, so re-recording it is recognized before any entry is touched.
    /// 2. **An older call, within [`TOOL_USE_RECENT_LIMIT`] calls of its identity.** A `call_id`
    ///    still in that identity's window is recognized on its own.
    ///
    /// The first tier is not redundant. A single response may make more calls on one identity than
    /// the window retains, and then checking one call at a time *cascades*: each re-recorded call
    /// evicts the next one about to be checked, so every call in the turn counts twice.
    ///
    /// Either way the per-turn count is still recomputed from the attempts, so it reads the same on
    /// a replay as it did the first time; only the run total, the streak, and the window hold still.
    ///
    /// Both tiers assume a `call_id` identifies one call. An adapter that reused call ids across
    /// turns would have a genuine turn read as a replay — which is the same assumption every
    /// `call_id`-keyed pairing in the framework already makes.
    pub fn record_turn(
        &mut self,
        agent: &AgentId,
        attempts: impl IntoIterator<Item = ToolUseAttempt>,
    ) {
        let attempts = attempts.into_iter().collect::<Vec<_>>();
        let turn_fingerprint = fingerprint_turn(&attempts);
        // Created even when the agent asked for nothing: "ran a turn and wanted no tools" is a fact
        // the audit needs, and an absent key cannot be told apart from an agent that never ran.
        let agent_use = self
            .agents
            .entry(agent.clone())
            .or_insert_with(AgentToolUse::new);
        for entry in &mut agent_use.entries {
            entry.turn_calls = 0;
        }

        // A single response may contain more calls for one identity than `recent` retains. Looking
        // at the per-entry windows one call at a time then makes a replay evict the next call it is
        // about to check, so all of them count twice. The whole-turn fingerprint is a bounded,
        // persisted replay key for that immediate resume path.
        let replay = agent_use.last_turn_fingerprint.as_ref() == Some(&turn_fingerprint);

        for attempt in attempts {
            let ToolUseAttempt {
                identity,
                call_id,
                fingerprint,
            } = attempt;
            let entry = agent_use.entry_mut(&identity);
            entry.turn_calls = entry.turn_calls.saturating_add(1);
            if replay || entry.already_recorded(&call_id) {
                continue;
            }
            entry.record(call_id, fingerprint);
        }
        agent_use.last_turn_fingerprint = Some(turn_fingerprint);
    }

    /// Every agent that has run a turn, in agent-ID order.
    pub fn agents(&self) -> impl Iterator<Item = (&AgentId, &AgentToolUse)> {
        self.agents.iter()
    }

    /// One agent's history.
    #[must_use]
    pub fn agent(&self, agent: &AgentId) -> Option<&AgentToolUse> {
        self.agents.get(agent)
    }

    /// Whether the agent asked for anything at all this turn.
    #[must_use]
    pub fn used_any_this_turn(&self, agent: &AgentId) -> bool {
        self.agent(agent)
            .is_some_and(AgentToolUse::used_any_this_turn)
    }

    /// How many consecutive calls of one identity carried the same arguments.
    ///
    /// Zero when the agent has never invoked it. This is the value R3-6's loop breaker compares
    /// against its threshold, and reading it here rather than re-deriving it per call site is what
    /// keeps "what counts as a repeat" a single definition.
    ///
    /// **It counts the whole turn already recorded, not the calls settled so far.** Settlement
    /// records a turn before it executes any of it, so `N` identical parallel calls in one response
    /// all read `N`, including the first one to be dispatched. A threshold of `N` therefore refuses
    /// all of them rather than letting the first through — deliberate, since `N` identical calls in
    /// one response is itself the pathology, but it is a policy R3-6 inherits rather than chooses.
    #[must_use]
    pub fn repeat_streak(&self, agent: &AgentId, identity: &ToolUse) -> u32 {
        self.agent(agent)
            .and_then(|agent_use| agent_use.entry(identity))
            .map_or(0, ToolUseEntry::repeat_streak)
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

/// Digest of one whole ordered turn: its identities, call IDs, and argument digests.
///
/// A separate type from [`ArgumentFingerprint`] on purpose, even though both are hex SHA-256 and
/// neither is public. They answer different questions — "same arguments?" versus "same turn?" — and
/// one type for both would let a comparison between them typecheck while meaning nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct TurnFingerprint(String);

/// Hashes an ordered turn without retaining its arguments or call IDs in a second history.
fn fingerprint_turn(attempts: &[ToolUseAttempt]) -> TurnFingerprint {
    let mut digest = Sha256::new();
    for attempt in attempts {
        update_identity_digest(&mut digest, attempt.identity());
        update_digest(&mut digest, b"call_id", attempt.call_id().as_str());
        update_digest(&mut digest, b"arguments", attempt.fingerprint().as_str());
    }
    TurnFingerprint(format!("{:x}", digest.finalize()))
}

/// Adds the equality-defining portion of one action identity to a turn digest.
fn update_identity_digest(digest: &mut Sha256, identity: &ToolUse) {
    match identity {
        ToolUse::Tool(key) => {
            update_digest(digest, b"type", b"tool");
            let kind = match key.kind() {
                ToolLookupKind::Bare => &b"bare"[..],
                ToolLookupKind::Namespaced => &b"namespaced"[..],
                ToolLookupKind::DeferredTopLevel => &b"deferred_top_level"[..],
            };
            update_digest(digest, b"kind", kind);
            update_digest(digest, b"name", key.name());
            update_digest(
                digest,
                b"namespace",
                key.namespace().map_or("", |namespace| namespace.as_str()),
            );
        }
        ToolUse::Handoff(agent) => {
            update_digest(digest, b"type", b"handoff");
            update_digest(digest, b"agent", agent.as_str());
        }
        ToolUse::Mcp { server, tool_name } => {
            update_digest(digest, b"type", b"mcp");
            update_digest(digest, b"server", server);
            update_digest(digest, b"tool_name", tool_name);
        }
        ToolUse::Unresolved(name) => {
            update_digest(digest, b"type", b"unresolved");
            update_digest(digest, b"name", name);
        }
    }
}

/// Length-prefixes every segment so adjacent attempts cannot produce an ambiguous byte stream.
fn update_digest(digest: &mut Sha256, label: &[u8], value: impl AsRef<[u8]>) {
    let value = value.as_ref();
    digest.update((label.len() as u64).to_be_bytes());
    digest.update(label);
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

const fn tool_use_schema_version() -> SchemaVersion {
    TOOL_USE_SCHEMA_VERSION
}
