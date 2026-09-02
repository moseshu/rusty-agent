//! Model-visible output of one tool invocation.
//!
//! # Two audiences, one value
//!
//! A tool result is read by two consumers with opposite needs. The **host** — a future context
//! budget trimmer, the UI, the rollout log — needs to know *as data* that output was cut and how
//! much of it there was. The **model** needs to be told the same thing in a sentence it can act
//! on, and every byte it is told costs tokens on this turn and on every turn after it.
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
//! asymmetry that justified reserving a slot for cross-run task state does not apply here.
//!
//! **Per-tool statistics.** `grep`'s scanned/skipped counts and `exec_command`'s wall time and
//! exit code belong to one tool each. They arrive as typed fields with their tools, which costs
//! nothing and keeps a generic value from carrying fields most tools leave empty.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use serde_json::Value;

use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
    item::{CallId, FileBlock, ImageBlock, ModelInputItem, ModelResponse},
    state::{RunId, ToolOutputReferenceTracker},
};

/// Current tool-output schema version.
pub const TOOL_OUTPUT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(2);

/// A provider-neutral tool result.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolOutput {
    schema_version: SchemaVersion,
    blocks: Vec<ToolOutputBlock>,
    metadata: ObservationMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_excerpt: Option<ModelExcerpt>,
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
    #[serde(default)]
    model_excerpt: Option<ModelExcerpt>,
    #[serde(flatten, default)]
    unknown: Unknown,
}

/// The previous schema version persisted a text result as `{"type":"text","text":"..."}` and
/// nothing else — that enum carried neither a schema version nor an unknown-field bag, so those
/// two keys are the whole of the shape and there is nothing else to carry forward.
#[derive(Deserialize)]
struct LegacyTextToolOutputWire {
    text: String,
}

/// Which stored shape a payload claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredShape {
    /// This build's shape: `schema_version` and a `blocks` array.
    Structured,
    /// The previous schema version's shape: `{"type":"text","text":"..."}`.
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
    // `type` could only ever be `text`: the previous schema version's enum had exactly one variant.
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
                "not a tool result: expected a `blocks` array, or the legacy `{\"type\":\"text\"}` form",
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
            model_excerpt: None,
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
            model_excerpt: None,
            unknown: Unknown::new(),
        }
    }

    /// Attaches what the host should know about how this observation was produced.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ObservationMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Replaces the provider-facing projection while retaining the complete blocks for storage.
    ///
    /// A context stage owns this field rather than a tool: the tool reports what it observed, and
    /// the host decides how much of that observation can enter the next model request.  Keeping
    /// the complete blocks beside the excerpt makes the session record authoritative without
    /// duplicating untrimmed output; this field is absent until a projection actually differs.
    #[must_use]
    pub fn with_model_excerpt(mut self, model_excerpt: ModelExcerpt) -> Self {
        self.model_excerpt = Some(model_excerpt);
        self
    }

    /// Applies an additive model-facing projection without changing the complete observation.
    ///
    /// The runtime, rather than a context-policy implementation, owns this transition so a
    /// projector cannot replace the result's blocks or overwrite metadata a tool already recorded.
    #[must_use]
    pub fn with_model_projection(mut self, projection: ToolOutputProjection) -> Self {
        if let Some(model_excerpt) = projection.model_excerpt {
            self.model_excerpt = Some(model_excerpt);
        }
        self.metadata.truncations.extend(projection.truncations);
        self.metadata.guidance.extend(projection.guidance);
        self
    }

    /// Reads a stored tool-result payload, keeping "not one" apart from "cannot read one".
    ///
    /// Three outcomes, and the middle one is why this exists rather than a bare
    /// [`Deserialize`] call:
    ///
    /// - `Ok(Some(_))` — a tool result this build understands;
    /// - `Ok(None)` — **not a tool result at all**. Hosts stored bare JSON here before this type
    ///   gave the payload a shape, and a resumed session replays it; a caller stringifies it and
    ///   moves on;
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
                // No re-validation here: `ModelExcerpt` enforces its own invariants while being
                // read, so an excerpt cannot reach this point in a shape its constructor rejects.
                output.model_excerpt = wire.model_excerpt;
                output.unknown = wire.unknown;
                Ok(output)
            }
            // Upgrading at the type boundary is what lets every session reader keep the
            // model-visible text of a result written before this milestone.
            StoredShape::LegacyText => LegacyTextToolOutputWire::deserialize(payload)
                .map(|wire| Self::text(wire.text))
                .map_err(|error| {
                    Error::caller(format!("stored legacy tool result is unreadable: {error}"))
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

    /// The bounded model-facing projection, when a context stage installed one.
    #[must_use]
    pub const fn model_excerpt(&self) -> Option<&ModelExcerpt> {
        self.model_excerpt.as_ref()
    }

    /// Mutable metadata, for the stages that observe a result after the tool returned.
    ///
    /// A future context-budget trimmer is the caller this exists for: it cuts a stored result long
    /// after dispatch and has to *append* its own truncation rather than replace the tool's.
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
        let source = self
            .model_excerpt
            .as_ref()
            .map_or_else(|| self.blocks.as_slice(), ModelExcerpt::blocks);
        let mut blocks = Vec::with_capacity(source.len() + 2);
        if let Some(note) = self.metadata.render() {
            blocks.push(ToolOutputBlock::text(note));
        }
        blocks.extend(source.iter().cloned());
        if let Some(excerpt) = &self.model_excerpt {
            // A locator, not an offer. Naming the record lets a person reading the session find the
            // complete result; promising the *model* it can fetch one would invite a call that no
            // tool answers, and the retrieval port does not exist yet.
            blocks.push(ToolOutputBlock::text(format!(
                "The complete result is retained in the session record as artifact `{}`.",
                excerpt.artifact_ref()
            )));
        }
        blocks
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Stable name for the complete result retained outside the model excerpt.
///
/// This is an identity, not a path or a promise that this process can fetch the artifact. This
/// milestone only establishes the link between a bounded prompt projection and the authoritative
/// session record; a later one gives session and archive implementations the retrieval port.
///
/// **A reference must identify one result across the whole store**, so whoever mints one scopes it
/// by the run as well as the call: a provider only promises a call identifier is unique within its
/// own conversation, and two sessions reusing `call-1` would otherwise name the same artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ArtifactRef(String);

impl ArtifactRef {
    /// Creates a stable artifact reference.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    /// String representation of the opaque reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(value: &str) -> Result<()> {
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            return Err(Error::caller(
                "an artifact reference must be non-empty, trimmed, and contain no control characters",
            ));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ArtifactRef {
    /// Reads a stored reference through the same rule the constructor applies.
    ///
    /// Validation written only in the constructor is validation not written: checkpoint and rollout
    /// reach these values by deserializing them, so a derived impl would let an empty or
    /// control-character reference into a record that every later reader trusts.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

impl core::fmt::Display for ArtifactRef {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Bounded blocks sent to a model in place of a complete tool result.
///
/// The companion [`ArtifactRef`] names the full result retained by the session.  The excerpt is
/// intentionally a block list rather than a text field: a context policy may retain structured
/// facts while dropping an opaque image or file without pretending that either was text.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelExcerpt {
    schema_version: SchemaVersion,
    blocks: Vec<ToolOutputBlock>,
    artifact_ref: ArtifactRef,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

#[derive(Deserialize)]
struct ModelExcerptWire {
    #[serde(default = "model_excerpt_schema_version")]
    schema_version: SchemaVersion,
    blocks: Vec<ToolOutputBlock>,
    artifact_ref: ArtifactRef,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for ModelExcerpt {
    /// Reads a stored excerpt through the same guard [`ModelExcerpt::new`] applies.
    ///
    /// A derived impl would admit a zero-block excerpt, and that value answers its tool call with
    /// nothing the moment [`ToolOutput::model_blocks`] projects it — one turn later, as a provider
    /// rejection, rather than here where the record can be named.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelExcerptWire::deserialize(deserializer)?;
        let excerpt = Self {
            schema_version: wire.schema_version,
            blocks: wire.blocks,
            artifact_ref: wire.artifact_ref,
            unknown: wire.unknown,
        };
        excerpt.validate().map_err(D::Error::custom)?;
        Ok(excerpt)
    }
}

/// Current model-excerpt schema version.
pub const MODEL_EXCERPT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn model_excerpt_schema_version() -> SchemaVersion {
    MODEL_EXCERPT_SCHEMA_VERSION
}

impl ModelExcerpt {
    /// Creates a non-empty provider-facing excerpt for one complete artifact.
    pub fn new(blocks: Vec<ToolOutputBlock>, artifact_ref: ArtifactRef) -> Result<Self> {
        let excerpt = Self {
            schema_version: MODEL_EXCERPT_SCHEMA_VERSION,
            blocks,
            artifact_ref,
            unknown: Unknown::new(),
        };
        excerpt.validate()?;
        Ok(excerpt)
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Blocks the provider receives before the artifact-reference note.
    #[must_use]
    pub fn blocks(&self) -> &[ToolOutputBlock] {
        &self.blocks
    }

    /// Stable reference to the complete result retained by the session/archive layer.
    #[must_use]
    pub const fn artifact_ref(&self) -> &ArtifactRef {
        &self.artifact_ref
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// The artifact reference is not re-checked here: [`ArtifactRef`] admits no invalid value,
    /// whether it was constructed or deserialized.
    fn validate(&self) -> Result<()> {
        if self.blocks.is_empty() {
            return Err(Error::caller(
                "a model excerpt must carry at least one block; otherwise its tool call would be unanswered",
            ));
        }
        Ok(())
    }
}

/// Additive changes a context policy may make to one complete tool result.
///
/// This deliberately does not carry the complete blocks or a replacement metadata value. The
/// runtime applies it to the original [`ToolOutput`], preserving the record while allowing a
/// policy to install a bounded model excerpt and append facts about that projection.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutputProjection {
    model_excerpt: Option<ModelExcerpt>,
    truncations: Vec<Truncation>,
    guidance: Vec<String>,
}

impl ToolOutputProjection {
    /// Creates a projection that leaves the model-facing result unchanged.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model_excerpt: None,
            truncations: Vec::new(),
            guidance: Vec::new(),
        }
    }

    /// Replaces the model-facing excerpt while retaining complete blocks for storage.
    #[must_use]
    pub fn with_model_excerpt(mut self, model_excerpt: ModelExcerpt) -> Self {
        self.model_excerpt = Some(model_excerpt);
        self
    }

    /// Appends a fact about one stage that trimmed the result.
    #[must_use]
    pub fn with_truncation(mut self, truncation: Truncation) -> Self {
        self.truncations.push(truncation);
        self
    }

    /// Appends model-facing guidance produced by the context policy.
    #[must_use]
    pub fn with_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.guidance.push(guidance.into());
        self
    }
}

impl Default for ToolOutputProjection {
    fn default() -> Self {
        Self::new()
    }
}

/// Projects one complete tool result into the bounded form a model may receive.
///
/// The runtime owns the call boundary but not context policy, so it reaches a projector through
/// [`crate::tool::ToolServices`]. Implementations can provide an excerpt and metadata additions,
/// but cannot replace complete blocks or existing metadata; the runtime applies the returned
/// [`ToolOutputProjection`] to the original [`ToolOutput`].
///
/// Both identifiers are passed because an [`ArtifactRef`] has to name one result across the whole
/// store: a call identifier is unique only within the conversation the provider issued it for.
pub trait ToolOutputProjector: Send + Sync + 'static {
    /// Returns the additive model-facing projection for one completed call.
    fn project(
        &self,
        run_id: &RunId,
        call_id: &CallId,
        output: &ToolOutput,
    ) -> Result<ToolOutputProjection>;
}

/// Projects the authoritative input into the bounded view sent to a model.
///
/// The runtime owns when a request is made but intentionally does not own context retention
/// policy. Implementations receive the persisted output-reference ledger so they can retain
/// results with a recent typed reference without parsing model narration themselves.
pub trait ModelInputProjector: Send + Sync + 'static {
    /// Returns the input view for the current model request.
    fn project_model_input(
        &self,
        run_id: &RunId,
        current_turn: u64,
        references: &ToolOutputReferenceTracker,
        input: &[ModelInputItem],
    ) -> Result<Vec<ModelInputItem>>;
}

/// Extracts typed references to earlier tool outputs from one model response.
///
/// A response's prose is not a retention signal. Products that have a structured reference
/// contract install this port; without one, the runtime records produced outputs but treats no
/// response as an explicit reference.
pub trait ToolOutputReferenceExtractor: Send + Sync + 'static {
    /// Returns the tool-output call IDs explicitly referenced by this response.
    fn referenced_tool_outputs(&self, response: &ModelResponse) -> Result<Vec<CallId>>;
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
/// framework may branch on it, because control flow that reads free text is what this framework
/// forbids and what a vocabulary-matching agent fails at the moment its user switches language.
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
    /// stops at its own output ceiling, then a future context-budget trimmer cuts the stored
    /// record to fit the context budget — and a field that held one truncation would let the
    /// second silently erase the first, leaving the model told about a smaller loss than actually
    /// happened.
    #[must_use]
    pub fn with_truncation(mut self, truncation: Truncation) -> Self {
        self.truncations.push(truncation);
        self
    }

    /// Appends a truncation in place, for stages that receive a finished result.
    pub fn push_truncation(&mut self, truncation: Truncation) {
        self.truncations.push(truncation);
    }

    /// Appends model-facing guidance in place for a stage that receives a finished result.
    pub fn push_guidance(&mut self, guidance: impl Into<String>) {
        self.guidance.push(guidance.into());
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
    /// future eval snapshots decide it rather than one reading of one log.
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
    /// agreeing on an encoding or owning a tokenizer, and a future token ceiling converts into it.
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
    /// The context budget trimmed a stored result.
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
