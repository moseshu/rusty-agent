//! Model-visible output of one tool invocation (R2-3).
//!
//! # Two audiences, one value
//!
//! A tool result is read by two consumers with opposite needs. The **host** — R5-1's budget
//! trimmer, the UI, the rollout log — needs to know *as data* that output was cut and how much of
//! it there was. The **model** needs to be told the same thing in a sentence it can act on, and
//! every byte it is told costs tokens on this turn and on every turn after it.
//!
//! So the structure holds facts and the projection produces prose: [`ObservationMetadata`] is
//! typed and stays typed, and [`ToolOutput::model_blocks`] renders it into a leading text block
//! at the moment the result is handed to a provider. Which facts are worth their tokens becomes a
//! rendering decision rather than a schema decision, and changing it does not migrate a wire
//! format.
//!
//! Two independent implementations arrived at the same shape: Codex's rollout puts
//! `Wall time 1.0 seconds` in a separate leading `input_text` block ahead of the real output, and
//! `openai-agents`' `ToolOutputTrimmer` writes `[Trimmed: … 12345 chars → 200 char preview]` into
//! the text it replaces. Neither carries a metadata field on the wire.
//!
//! # What is deliberately absent
//!
//! **A free-form extension map.** Cross-version growth is already covered — every struct here
//! carries `schema_version` and an [`Unknown`] that round-trips fields a newer build wrote. What a
//! map would add is a place for *this* build to put meaning without declaring it, and that has two
//! costs the project has already paid once: nothing records that the meaning changed, and an
//! untyped bag invites control flow that reads `meta["truncated"]` instead of a field. Adding a
//! typed field later is free — these structs are `#[non_exhaustive]` with private fields — so the
//! asymmetry that justified reserving a slot in R3-13 does not apply here.
//!
//! **Per-tool statistics.** `grep`'s scanned/skipped counts and `exec_command`'s wall time and
//! exit code are named in R2-3's brief, but they belong to one tool each. They arrive as typed
//! fields with their tools (R8-1, R8-6), which costs nothing and keeps a generic value from
//! carrying fields most tools leave empty.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use serde_json::Value;

use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
    item::{FileBlock, ImageBlock},
};

/// Current tool-output schema version.
pub const TOOL_OUTPUT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A provider-neutral tool result.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolOutput {
    schema_version: SchemaVersion,
    blocks: Vec<ToolOutputBlock>,
    metadata: ObservationMetadata,
    #[serde(flatten, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

#[derive(Deserialize)]
struct StructuredToolOutputWire {
    // Required, not defaulted: `stored_shape` already refused to claim a payload without it, so a
    // default here could never fire — and if the discriminator is ever loosened, a defaulted
    // version would read a version-less payload as "current" instead of saying it cannot tell.
    schema_version: SchemaVersion,
    blocks: Vec<ToolOutputBlock>,
    #[serde(default)]
    metadata: ObservationMetadata,
    #[serde(flatten, default)]
    unknown: Unknown,
}

/// R2-1 persisted a text result as `{"type":"text","text":"..."}` and nothing else — that enum
/// carried neither a schema version nor an unknown-field bag, so those two keys are the whole of
/// the shape and there is nothing else to carry forward.
#[derive(Deserialize)]
struct LegacyTextToolOutputWire {
    text: String,
}

/// Which stored shape a payload claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredShape {
    /// This build's shape: `schema_version` and a `blocks` array.
    Structured,
    /// R2-1's shape: `{"type":"text","text":"..."}`.
    LegacyText,
}

/// Decides what a stored payload claims to be, **without reading it**.
///
/// Claiming and being readable are different questions, and answering them in one step is what
/// makes "a record a newer build wrote" indistinguishable from "a value that was never a tool
/// result". Only the key layout is inspected here; whether the claim holds up is
/// [`ToolOutput::read_stored`]'s answer, and its error is the one worth showing.
fn stored_shape(payload: &Value) -> Option<StoredShape> {
    let map = payload.as_object()?;
    // **The pair, not `blocks` alone.** `blocks` is an ordinary English word and an ordinary JSON
    // key — Slack's Block Kit answers with `{"blocks": [{"type": "section", ...}]}` — so claiming
    // every payload that has one would take a host's own tool result, fail to read it as ours, and
    // (now that unreadable is an error rather than a fallback) make a session carrying it
    // unreplayable. `schema_version` is the framework's own marker, and every record this build
    // writes carries it, so the pair identifies us without stranding anybody else.
    if map.contains_key("schema_version") && map.contains_key("blocks") {
        return Some(StoredShape::Structured);
    }
    // `type` could only ever be `text`: R2-1's enum had exactly one variant.
    if map.get("type").and_then(Value::as_str) == Some("text") && map.contains_key("text") {
        return Some(StoredShape::LegacyText);
    }
    None
}

impl<'de> Deserialize<'de> for ToolOutput {
    /// Hand-written so a rejection can say what was wrong.
    ///
    /// The obvious spelling — an `#[serde(untagged)]` enum over the two shapes — compiles and
    /// works, and then reports every possible failure as `data did not match any variant of
    /// untagged enum ToolOutputWire`. Untagged tries each variant and, when all fail, has nothing
    /// left but that sentence: the real error ("unknown variant `bogus`, expected one of `text`,
    /// `image`, `file`") was discarded along with the attempt that produced it. These records come
    /// back during resume and rollout replay, where the question is *which* of thousands is
    /// unreadable and how — and that is exactly the question the sentence cannot answer.
    ///
    /// **This requires a self-describing format**, because discrimination reads the payload's keys
    /// before deciding how to read its values. Every one of these records is JSON, and the
    /// `#[serde(flatten)]` fields already buffer the same way.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let payload = Value::deserialize(deserializer)?;
        match stored_shape(&payload) {
            Some(shape) => Self::read_stored(&payload, shape).map_err(D::Error::custom),
            None => Err(D::Error::custom(
                "not a tool result: expected a `blocks` array, or R2-1's `{\"type\":\"text\"}` form",
            )),
        }
    }
}

impl ToolOutput {
    /// Creates a result from its blocks.
    ///
    /// **Empty is rejected.** A tool call with no output makes the history malformed — every
    /// provider requires the call to be answered — and the failure surfaces one turn later as a
    /// rejected request rather than here. `openai-agents` hit the same edge from the other side:
    /// `all([])` is `True`, so an empty structured list passed its conversion check and silently
    /// dropped the result, and it now carries an explicit guard for exactly this.
    pub fn new(blocks: Vec<ToolOutputBlock>) -> Result<Self> {
        if blocks.is_empty() {
            return Err(Error::caller(
                "a tool result must carry at least one block; a call answered with nothing leaves \
                 the history malformed, and every provider rejects it",
            ));
        }
        Ok(Self {
            schema_version: TOOL_OUTPUT_SCHEMA_VERSION,
            blocks,
            metadata: ObservationMetadata::new(),
            unknown: Unknown::new(),
        })
    }

    /// Creates a text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::block(ToolOutputBlock::text(text))
    }

    /// Creates a single-block result.
    ///
    /// Infallible where [`Self::new`] is not, because one block is never zero blocks. Without it,
    /// a tool that answers with one image has to handle an error that cannot occur, and the usual
    /// way that gets written is an `unwrap`.
    #[must_use]
    pub fn block(block: ToolOutputBlock) -> Self {
        Self {
            schema_version: TOOL_OUTPUT_SCHEMA_VERSION,
            blocks: vec![block],
            metadata: ObservationMetadata::new(),
            unknown: Unknown::new(),
        }
    }

    /// Attaches what the host should know about how this observation was produced.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ObservationMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Reads a stored tool-result payload, keeping "not one" apart from "cannot read one".
    ///
    /// Three outcomes, and the middle one is why this exists rather than a bare
    /// [`Deserialize`] call:
    ///
    /// - `Ok(Some(_))` — a tool result this build understands;
    /// - `Ok(None)` — **not a tool result at all**. Hosts stored bare JSON here before R2-3 gave
    ///   the payload a shape, and a resumed session replays it; a caller stringifies it and moves
    ///   on;
    /// - `Err(_)` — it *is* a tool result and this build cannot read it, because a newer build
    ///   wrote a block kind this one has no variant for.
    ///
    /// Collapsing the last two into "it did not parse" is the failure this signature prevents: the
    /// newer build's record would be quietly stringified into the model's context as raw JSON
    /// instead of failing where somebody can see it. Unknown *fields* still round-trip verbatim —
    /// only a kind this build cannot represent is an error, the same call
    /// [`FinishReason`](crate::finish::FinishReason) makes about an unknown wire value.
    pub fn from_stored(payload: &Value) -> Result<Option<Self>> {
        match stored_shape(payload) {
            Some(shape) => Self::read_stored(payload, shape).map(Some),
            None => Ok(None),
        }
    }

    fn read_stored(payload: &Value, shape: StoredShape) -> Result<Self> {
        match shape {
            StoredShape::Structured => {
                let wire = StructuredToolOutputWire::deserialize(payload).map_err(|error| {
                    Error::caller(format!("stored tool result is unreadable: {error}"))
                })?;
                let mut output = Self::new(wire.blocks)?;
                output.schema_version = wire.schema_version;
                output.metadata = wire.metadata;
                output.unknown = wire.unknown;
                Ok(output)
            }
            // Upgrading at the type boundary is what lets every session reader keep the
            // model-visible text of a result written before this milestone.
            StoredShape::LegacyText => LegacyTextToolOutputWire::deserialize(payload)
                .map(|wire| Self::text(wire.text))
                .map_err(|error| {
                    Error::caller(format!("stored R2-1 tool result is unreadable: {error}"))
                }),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// The result's blocks, in the order the tool produced them.
    #[must_use]
    pub fn blocks(&self) -> &[ToolOutputBlock] {
        &self.blocks
    }

    /// Facts about this observation, for the host rather than for the model.
    #[must_use]
    pub const fn metadata(&self) -> &ObservationMetadata {
        &self.metadata
    }

    /// Mutable metadata, for the stages that observe a result after the tool returned.
    ///
    /// R5-1's budget trimmer is the caller this exists for: it cuts a stored result long after
    /// dispatch and has to *append* its own truncation rather than replace the tool's.
    pub fn metadata_mut(&mut self) -> &mut ObservationMetadata {
        &mut self.metadata
    }

    /// Text projection when the result is a single text block.
    ///
    /// Deliberately narrow: it answers "is this one plain string?" and says `None` for everything
    /// else rather than concatenating, because a caller that wanted the whole result would then
    /// silently lose the image blocks.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self.blocks.as_slice() {
            [ToolOutputBlock::Text { text }] => Some(text),
            _ => None,
        }
    }

    /// The blocks a provider should be sent, metadata first.
    ///
    /// A projection, not a stored list: keeping the rendered form as a field would let it drift
    /// from the facts it was rendered from, and the rendering has to happen *after* every stage
    /// that may still add to the metadata.
    #[must_use]
    pub fn model_blocks(&self) -> Vec<ToolOutputBlock> {
        let mut blocks = Vec::with_capacity(self.blocks.len() + 1);
        if let Some(note) = self.metadata.render() {
            blocks.push(ToolOutputBlock::text(note));
        }
        blocks.extend(self.blocks.iter().cloned());
        blocks
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// One piece of a tool result.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutputBlock {
    /// Model-visible text.
    Text {
        /// Output text.
        text: String,
    },
    /// An image the model should see.
    Image(ImageBlock),
    /// A file the model should read.
    File(FileBlock),
}

impl ToolOutputBlock {
    /// Creates a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Text projection when this is a text block.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// What the host should know about how one observation was produced.
///
/// Everything here is a *fact about the result*, never an instruction to the framework. In
/// particular [`guidance`](Self::guidance) is prose written for the model to read; nothing in the
/// framework may branch on it, because control flow that reads free text is what R7-10 forbids and
/// what a vocabulary-matching agent fails at the moment its user switches language.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationMetadata {
    #[serde(default = "observation_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    truncations: Vec<Truncation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    guidance: Vec<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for ObservationMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl ObservationMetadata {
    /// Creates empty metadata.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: OBSERVATION_METADATA_SCHEMA_VERSION,
            truncations: Vec::new(),
            guidance: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// Records that something cut this result, keeping every earlier cut.
    ///
    /// **Appending is the contract.** A result can be cut twice by different stages — the tool
    /// stops at its own output ceiling, then R5-1 trims the stored record to fit the context
    /// budget — and a field that held one truncation would let the second silently erase the
    /// first, leaving the model told about a smaller loss than actually happened.
    #[must_use]
    pub fn with_truncation(mut self, truncation: Truncation) -> Self {
        self.truncations.push(truncation);
        self
    }

    /// Appends a truncation in place, for stages that receive a finished result.
    pub fn push_truncation(&mut self, truncation: Truncation) {
        self.truncations.push(truncation);
    }

    /// Adds a sentence suggesting what the model might do next.
    ///
    /// This is the mechanism behind "observations that carry truncation, noise, and a suggestion
    /// raise the self-correction rate" — a narrowing hint on a truncated search costs a line and
    /// saves a wasted turn. It is generation guidance, not a gate.
    #[must_use]
    pub fn with_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.guidance.push(guidance.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Every cut made to this result, in the order the stages made them.
    #[must_use]
    pub fn truncations(&self) -> &[Truncation] {
        &self.truncations
    }

    /// Whether anything cut this result.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        !self.truncations.is_empty()
    }

    /// Suggestions written for the model.
    #[must_use]
    pub fn guidance(&self) -> &[String] {
        &self.guidance
    }

    /// The sentence the model is told, or `None` when there is nothing worth its tokens.
    ///
    /// **This is the only place metadata becomes model-visible**, which is what makes "how much of
    /// this is worth paying for" a decision that can be changed without migrating stored records.
    /// The wording is not settled: a rendering that reads well is a measurable property, and
    /// R3-11's snapshots plus an eval decide it rather than one reading of one log.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        if self.truncations.is_empty() && self.guidance.is_empty() {
            return None;
        }
        let mut note = String::new();
        for truncation in &self.truncations {
            note.push_str(&truncation.render());
            note.push('\n');
        }
        for guidance in &self.guidance {
            note.push_str(guidance);
            note.push('\n');
        }
        Some(note)
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Current observation-metadata schema version.
pub const OBSERVATION_METADATA_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn observation_schema_version() -> SchemaVersion {
    OBSERVATION_METADATA_SCHEMA_VERSION
}

/// One cut made to a result, and who made it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    #[serde(default = "truncation_schema_version")]
    schema_version: SchemaVersion,
    stage: TruncationStage,
    original_bytes: u64,
    retained_bytes: u64,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Truncation {
    /// Records a cut from `original_bytes` down to `retained_bytes`.
    ///
    /// Bytes, not characters or tokens: bytes are the one unit every stage can measure without
    /// agreeing on an encoding or owning a tokenizer, and R5-1's token ceiling converts into it.
    #[must_use]
    pub fn new(stage: TruncationStage, original_bytes: u64, retained_bytes: u64) -> Self {
        Self {
            schema_version: TRUNCATION_SCHEMA_VERSION,
            stage,
            original_bytes,
            retained_bytes,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Which stage cut the result.
    #[must_use]
    pub const fn stage(&self) -> TruncationStage {
        self.stage
    }

    /// How large the result was before this cut.
    #[must_use]
    pub const fn original_bytes(&self) -> u64 {
        self.original_bytes
    }

    /// How large it was after.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    fn render(&self) -> String {
        format!(
            "[truncated by {}: {} of {} bytes kept]",
            self.stage.label(),
            self.retained_bytes,
            self.original_bytes
        )
    }
}

/// Current truncation schema version.
pub const TRUNCATION_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn truncation_schema_version() -> SchemaVersion {
    TRUNCATION_SCHEMA_VERSION
}

/// Which stage cut a result.
///
/// Recorded because the two mean different things to the model: its own tool stopping early is a
/// reason to narrow the request, while the framework trimming an old record to fit the budget is
/// not something a better query would have avoided.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationStage {
    /// The tool stopped at its own output ceiling.
    Tool,
    /// The context budget trimmed a stored result (R5-1).
    ContextBudget,
}

impl TruncationStage {
    /// Stable machine-readable label, identical to the serde wire value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::ContextBudget => "context_budget",
        }
    }
}

impl core::fmt::Display for TruncationStage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}
