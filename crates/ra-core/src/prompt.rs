//! Prompt sections, stability, positions, roles, and cache plans.
//!
//! This module defines protocol-neutral types and contracts for prompt structuring, stable prefix
//! identification, dynamic prompt handling, and cache plan declarations. Concrete prompt assembly
//! logic lives in `ra-prompt`.

use std::borrow::Cow;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::RunContext;
use crate::error::{Error, Result};
use crate::item::{Message, ModelInputItem};

/// A 64-character lowercase hexadecimal representation of a SHA-256 hash.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    /// Computes the SHA-256 hash of the given byte slice.
    #[must_use]
    pub fn compute(data: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data.as_ref());
        let result = hasher.finalize();
        Self(format!("{result:x}"))
    }

    /// Creates a hash from an existing valid 64-character hexadecimal string.
    pub fn from_hex(hex: impl Into<String>) -> Result<Self> {
        let text = hex.into();
        if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::config(format!(
                "invalid content hash format: expected 64 hex characters, got `{text}`"
            )));
        }
        Ok(Self(text.to_ascii_lowercase()))
    }

    /// Returns the hash as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ContentHash").field(&self.0).finish()
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ContentHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The stability classification of a prompt section.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionStability {
    /// Stable across turns, eligible for prefix caching.
    Stable,
    /// Volatile across turns or runs, forced to tail positions.
    Volatile,
}

impl SectionStability {
    /// Returns whether this section is stable.
    #[must_use]
    pub const fn is_stable(&self) -> bool {
        matches!(self, Self::Stable)
    }

    /// Returns whether this section is volatile.
    #[must_use]
    pub const fn is_volatile(&self) -> bool {
        matches!(self, Self::Volatile)
    }
}

impl fmt::Display for SectionStability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable => formatter.write_str("stable"),
            Self::Volatile => formatter.write_str("volatile"),
        }
    }
}

/// The placement position of a prompt section in model requests.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionPosition {
    /// Placed in the stable system instruction prefix.
    Prefix,
    /// Placed as a tail message at the end of input items.
    TailMessage,
}

impl SectionPosition {
    /// Returns whether this section is in the prefix position.
    #[must_use]
    pub const fn is_prefix(&self) -> bool {
        matches!(self, Self::Prefix)
    }

    /// Returns whether this section is in the tail message position.
    #[must_use]
    pub const fn is_tail_message(&self) -> bool {
        matches!(self, Self::TailMessage)
    }
}

impl fmt::Display for SectionPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prefix => formatter.write_str("prefix"),
            Self::TailMessage => formatter.write_str("tail_message"),
        }
    }
}

/// A strongly typed identifier for a prompt section.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PromptSectionName(Cow<'static, str>);

impl PromptSectionName {
    /// Agent identity and collaboration contract section.
    pub const IDENTITY: Self = Self::from_static("identity");
    /// Core agent behavior section.
    pub const CORE_BEHAVIOR: Self = Self::from_static("core_behavior");
    /// Tool usage guidance section.
    pub const TOOL_USE: Self = Self::from_static("tool_use");
    /// Stable inventory of the tool schemas advertised with the prompt.
    pub const TOOL_SURFACE: Self = Self::from_static("tool_surface");
    /// Safety boundaries and operational safety section.
    pub const SAFETY: Self = Self::from_static("safety");
    /// Editing and post-change verification guidance section.
    pub const EDITING_VERIFICATION: Self = Self::from_static("editing_verification");
    /// Autonomous progress and stop-loss guidance section.
    pub const AUTONOMY: Self = Self::from_static("autonomy");
    /// Commentary and final response-channel guidance section.
    pub const CHANNELS: Self = Self::from_static("channels");
    /// Final answer and deliverable format section.
    pub const FINAL_ANSWER: Self = Self::from_static("final_answer");
    /// Context durability and memory guidance section.
    pub const CONTEXT_DURABILITY: Self = Self::from_static("context_durability");
    /// Tone and personality section.
    pub const PERSONALITY: Self = Self::from_static("personality");
    /// Role-specific guidance section.
    pub const ROLE: Self = Self::from_static("role");
    /// Per-turn instructions produced by a dynamic prompt generator.
    pub const DYNAMIC_INSTRUCTIONS: Self = Self::from_static("dynamic_instructions");

    /// Creates a section name from a static string.
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// Creates a section name from any string.
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }

    /// Returns the section name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PromptSectionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PromptSectionName")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for PromptSectionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&'static str> for PromptSectionName {
    fn from(value: &'static str) -> Self {
        Self::from_static(value)
    }
}

impl From<String> for PromptSectionName {
    fn from(value: String) -> Self {
        Self(Cow::Owned(value))
    }
}

/// Provenance and origin of a prompt section.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum PromptSource {
    /// Built-in framework defaults.
    Builtin,
    /// Declared on the agent specification.
    Agent,
    /// Contributed by an enabled capability.
    Capability(String),
    /// Generated dynamically at runtime.
    Dynamic(String),
    /// Third-party or custom origin.
    Custom(Cow<'static, str>),
}

impl fmt::Display for PromptSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin => formatter.write_str("builtin"),
            Self::Agent => formatter.write_str("agent"),
            Self::Capability(cap) => write!(formatter, "capability({cap})"),
            Self::Dynamic(source) => write!(formatter, "dynamic({source})"),
            Self::Custom(name) => write!(formatter, "custom({name})"),
        }
    }
}

/// A structured prompt section with stability, position, and hash guarantees.
///
/// Deserialization routes through [`PromptSection::new`] rather than filling the fields directly.
/// A derived `Deserialize` would be a second constructor that skips validation, and both of this
/// type's guarantees are only worth as much as the path that cannot be bypassed: a volatile
/// section could re-enter the prefix, and `content_hash` could disagree with `content` — after
/// which every cache decision keyed on that hash is reasoning about text the section does not
/// hold.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PromptSectionWire")]
pub struct PromptSection {
    name: PromptSectionName,
    purpose: String,
    source: PromptSource,
    stability: SectionStability,
    position: SectionPosition,
    content_hash: ContentHash,
    token_estimate: usize,
    token_budget: Option<usize>,
    content: String,
}

/// Wire shape of [`PromptSection`], validated on the way in.
#[derive(Deserialize)]
struct PromptSectionWire {
    name: PromptSectionName,
    purpose: String,
    source: PromptSource,
    stability: SectionStability,
    position: SectionPosition,
    content_hash: ContentHash,
    token_estimate: usize,
    // Defaulted, unlike every other field here: a section recorded before budgets existed declared
    // no allowance, and that is exactly what `None` says. Refusing it would reject old dumps over a
    // fact they could not have carried.
    #[serde(default)]
    token_budget: Option<usize>,
    content: String,
}

impl TryFrom<PromptSectionWire> for PromptSection {
    type Error = Error;

    fn try_from(wire: PromptSectionWire) -> Result<Self> {
        let section = Self::new(
            wire.name,
            wire.purpose,
            wire.source,
            wire.stability,
            wire.position,
            wire.content,
        )?;

        // The hash is derived from the content, so a stored value that disagrees is not a stale
        // field to refresh — it means the two travelled separately and one of them is not what it
        // claims. Recomputing silently would paper over exactly the corruption worth catching.
        if section.content_hash != wire.content_hash {
            return Err(Error::config(format!(
                "prompt section `{}` carries content hash `{}`, but its content hashes to `{}`",
                section.name, wire.content_hash, section.content_hash
            )));
        }

        // The estimate is restored verbatim: an external tokenizer's exact count is data, and
        // recomputing it would discard the reason `with_token_estimate` exists.
        let section = section.with_token_estimate(wire.token_estimate);
        Ok(match wire.token_budget {
            Some(budget) => section.with_token_budget(budget),
            None => section,
        })
    }
}

impl PromptSection {
    /// Constructs a validated prompt section.
    ///
    /// # Errors
    ///
    /// Returns an error if a volatile section is assigned to the prefix position.
    pub fn new(
        name: impl Into<PromptSectionName>,
        purpose: impl Into<String>,
        source: PromptSource,
        stability: SectionStability,
        position: SectionPosition,
        content: impl Into<String>,
    ) -> Result<Self> {
        let name = name.into();
        let content = content.into();
        let purpose = purpose.into();

        if stability.is_volatile() && position.is_prefix() {
            return Err(Error::config(format!(
                "prompt section `{name}` violates cache invariants: volatile sections cannot be \
                 placed in the prefix position"
            )));
        }

        let content_hash = ContentHash::compute(&content);
        let token_estimate = estimate_tokens(&content);

        Ok(Self {
            name,
            purpose,
            source,
            stability,
            position,
            content_hash,
            token_estimate,
            token_budget: None,
            content,
        })
    }

    /// Overrides the computed token estimate if an external tokenizer provides an exact count.
    #[must_use]
    pub fn with_token_estimate(mut self, estimate: usize) -> Self {
        self.token_estimate = estimate;
        self
    }

    /// Declares the largest share of the cached prefix this section may spend.
    ///
    /// A prefix section is paid for on every turn of every run, so a topic that doubles in length
    /// is a recurring cost rather than a one-off edit. The allowance is declared next to the text
    /// it governs, travels with the section into the dump, and is enforced where sections become
    /// the cached artifact — see `PromptAssembler::assemble`.
    ///
    /// Declaring one is optional. A section without an allowance is not exempt by intent; it is a
    /// section whose author has not stated a limit, and the dump shows the difference rather than
    /// inventing a number nobody chose.
    #[must_use]
    pub fn with_token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }

    /// Section name.
    #[must_use]
    pub const fn name(&self) -> &PromptSectionName {
        &self.name
    }

    /// Brief description of the section's purpose.
    #[must_use]
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    /// Provenance of this section.
    #[must_use]
    pub const fn source(&self) -> &PromptSource {
        &self.source
    }

    /// Stability classification.
    #[must_use]
    pub const fn stability(&self) -> SectionStability {
        self.stability
    }

    /// Placement position.
    #[must_use]
    pub const fn position(&self) -> SectionPosition {
        self.position
    }

    /// Content SHA-256 hash.
    #[must_use]
    pub const fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    /// Estimated token count.
    #[must_use]
    pub const fn token_estimate(&self) -> usize {
        self.token_estimate
    }

    /// Declared cached-prefix allowance, if the section states one.
    #[must_use]
    pub const fn token_budget(&self) -> Option<usize> {
        self.token_budget
    }

    /// Section text content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
}

impl fmt::Debug for PromptSection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptSection")
            .field("name", &self.name)
            .field("purpose", &self.purpose)
            .field("source", &self.source)
            .field("stability", &self.stability)
            .field("position", &self.position)
            .field("content_hash", &self.content_hash)
            .field("token_estimate", &self.token_estimate)
            .field("token_budget", &self.token_budget)
            .field("bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// Characters [`estimate_tokens`] charges to one token.
///
/// Standard rule-of-thumb for English prose and code. It is public because inverting the estimator
/// is what turns a token allowance into a character allowance, and what lets a caller price a
/// character count it accumulated itself. A private copy of this number in each of those callers
/// would let them and the estimator disagree about the same text.
pub const CHARS_PER_TOKEN: usize = 4;

/// Estimates a token count from character length.
///
/// This is the single estimator the whole prompt path shares. A second copy elsewhere would drift
/// from this one, and the two would then disagree about how large the same prefix is — which is
/// the number budgeting and the dump report are read off.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Role profiles for specialized agent modes.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "role", content = "name")]
pub enum PromptRole {
    /// Main autonomous execution agent.
    Main,
    /// Read-only specialist agent without editing tools.
    ReadOnlySpecialist,
    /// Read-only planning and architecture agent.
    Planner,
    /// One-off answer agent for background queries without tool access.
    OneOffAnswer,
    /// Multi-agent coordinator for delegation and dispatch.
    Coordinator,
    /// Custom role identifier.
    Custom(Cow<'static, str>),
}

impl PromptRole {
    /// Returns whether this role is strictly read-only.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnlySpecialist | Self::Planner)
    }

    /// Returns whether this role is a one-off query responder.
    #[must_use]
    pub const fn is_one_off(&self) -> bool {
        matches!(self, Self::OneOffAnswer)
    }

    /// Returns the canonical role name.
    #[must_use]
    pub fn role_name(&self) -> &str {
        match self {
            Self::Main => "main",
            Self::ReadOnlySpecialist => "read_only_specialist",
            Self::Planner => "planner",
            Self::OneOffAnswer => "one_off_answer",
            Self::Coordinator => "coordinator",
            Self::Custom(name) => name.as_ref(),
        }
    }
}

impl fmt::Display for PromptRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.role_name())
    }
}

/// The recorded identity of one prompt lowered into the model request.
///
/// This is what a turn keeps after the prompt text itself has been lowered into the request: which
/// generator produced it, the canonical tail text it lowered to, and the version and provenance
/// tag it declared. A prompt that reaches the model without this record leaves no way to answer
/// "which generator wrote the text in turn 7" after the fact.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptProvenance {
    source: PromptSource,
    content_hash: ContentHash,
    version: Option<String>,
    provenance: Option<String>,
}

impl PromptProvenance {
    /// Which generator or declaration produced the prompt.
    #[must_use]
    pub const fn source(&self) -> &PromptSource {
        &self.source
    }

    /// Hash of the canonical tail text actually lowered into the model request.
    #[must_use]
    pub const fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    /// Optional version tag declared by the generator.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Optional free-form provenance detail declared by the generator.
    #[must_use]
    pub fn provenance(&self) -> Option<&str> {
        self.provenance.as_deref()
    }
}

/// Resolved prompt payload with sections and provenance.
///
/// Deserialization routes through the validating constructor for the same reason
/// [`PromptSection`] does: a derived `Deserialize` would let `content_hash` disagree with `text`.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ResolvedPromptWire")]
pub struct ResolvedPrompt {
    text: String,
    sections: Vec<PromptSection>,
    source: PromptSource,
    version: Option<String>,
    content_hash: ContentHash,
    provenance: Option<String>,
}

/// Wire shape of [`ResolvedPrompt`], validated on the way in.
#[derive(Deserialize)]
struct ResolvedPromptWire {
    text: String,
    sections: Vec<PromptSection>,
    source: PromptSource,
    version: Option<String>,
    content_hash: ContentHash,
    provenance: Option<String>,
}

impl TryFrom<ResolvedPromptWire> for ResolvedPrompt {
    type Error = Error;

    fn try_from(wire: ResolvedPromptWire) -> Result<Self> {
        let mut resolved = Self::new(wire.text, wire.source).with_sections(wire.sections);
        if resolved.content_hash != wire.content_hash {
            return Err(Error::config(format!(
                "resolved prompt carries content hash `{}`, but its text hashes to `{}`",
                wire.content_hash, resolved.content_hash
            )));
        }
        resolved.version = wire.version;
        resolved.provenance = wire.provenance;
        Ok(resolved)
    }
}

impl ResolvedPrompt {
    /// Constructs a resolved prompt.
    pub fn new(text: impl Into<String>, source: PromptSource) -> Self {
        let text = text.into();
        let content_hash = ContentHash::compute(&text);
        Self {
            text,
            sections: Vec::new(),
            source,
            version: None,
            content_hash,
            provenance: None,
        }
    }

    /// Attaches the prompt sections that contributed to this resolved prompt.
    #[must_use]
    pub fn with_sections(mut self, sections: Vec<PromptSection>) -> Self {
        self.sections = sections;
        self
    }

    /// Attaches an optional version tag.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Attaches an optional provenance metadata string.
    #[must_use]
    pub fn with_provenance(mut self, provenance: impl Into<String>) -> Self {
        self.provenance = Some(provenance.into());
        self
    }

    /// Complete prompt text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Sections contributing to this prompt.
    #[must_use]
    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
    }

    /// Source of this resolved prompt.
    #[must_use]
    pub const fn source(&self) -> &PromptSource {
        &self.source
    }

    /// Optional version string.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Overall content SHA-256 hash.
    #[must_use]
    pub const fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    /// Optional provenance details.
    #[must_use]
    pub fn provenance(&self) -> Option<&str> {
        self.provenance.as_deref()
    }

    /// Captures the source, lowered-content hash, version, and provenance for the run record.
    ///
    /// The hash covers the text that lowering sends, not [`Self::text`] — a generator that supplies
    /// explicit sections may leave `text` as a summary that never reaches the model, and recording
    /// a hash of bytes nobody sent defeats the point of recording one.
    ///
    /// It is a hash of the *content*, over sections joined by a blank line. It deliberately does
    /// not identify the split: one section holding two paragraphs and two sections holding one
    /// each hash alike, though they lower to a different number of messages. That is enough to
    /// answer "what did the generator say on this turn", which is what the record is for.
    ///
    /// # Errors
    ///
    /// Propagates the placement failures described on [`Self::volatile_tail_sections`].
    pub fn provenance_record(&self) -> Result<PromptProvenance> {
        Ok(self.lower()?.1)
    }

    /// Lowers into tail items and the matching record in one pass.
    ///
    /// This is the entry a turn uses. Calling [`Self::lower_to_tail_items`] and
    /// [`Self::provenance_record`] separately would validate and clone the sections twice, and
    /// would leave "the hash covers the text that was sent" resting on the two calls happening to
    /// see the same value — true here, but true by inspection rather than by construction. One
    /// pass makes the record and the items the same derivation.
    ///
    /// # Errors
    ///
    /// Propagates the placement failures described on [`Self::volatile_tail_sections`].
    pub fn lower(&self) -> Result<(Vec<ModelInputItem>, PromptProvenance)> {
        let sections = self.volatile_tail_sections()?;
        let texts: Vec<&str> = sections
            .iter()
            .map(|section| section.content().trim())
            .filter(|content| !content.is_empty())
            .collect();

        // The transport receives separate messages, but all have the same role, and this
        // newline-separated form is the stable textual identity used for provenance. Both halves
        // read the same `texts`, so a hash can never name the unused `text` field when structured
        // sections are present.
        let record = PromptProvenance {
            source: self.source.clone(),
            content_hash: ContentHash::compute(texts.join("\n\n")),
            version: self.version.clone(),
            provenance: self.provenance.clone(),
        };
        let items = texts
            .into_iter()
            .map(|content| ModelInputItem::Message(Message::user(content)))
            .collect();
        Ok((items, record))
    }

    /// Projects this prompt onto the only placement a dynamically generated prompt may occupy.
    ///
    /// A generated prompt reads the live run, so its text is a function of the turn. The stable
    /// prefix is the cached span: text that varies per turn cannot be placed there without
    /// invalidating the cache on every single call, which is why the placement is a validated
    /// contract rather than a convention the generator is trusted to follow.
    ///
    /// A prompt carrying no sections is treated as one volatile tail section holding its whole
    /// text; empty text yields no sections at all.
    ///
    /// # Errors
    ///
    /// Returns an error when a section asks for the prefix position or for stable stability.
    /// Dropping such a section instead would be the silent fallback the contract forbids: the
    /// generator would believe it had contributed text that never reached the model.
    pub fn volatile_tail_sections(&self) -> Result<Vec<PromptSection>> {
        if self.sections.is_empty() {
            let text = self.text.trim();
            if text.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(vec![PromptSection::new(
                PromptSectionName::DYNAMIC_INSTRUCTIONS,
                "Volatile dynamic prompt instructions evaluated per turn",
                self.source.clone(),
                SectionStability::Volatile,
                SectionPosition::TailMessage,
                text,
            )?]);
        }

        for section in &self.sections {
            if !section.position().is_tail_message() {
                return Err(Error::config(format!(
                    "dynamic prompt section `{}` requests position `{}`, but a generated prompt \
                     may only occupy `{}`; placing per-turn text in the stable prefix invalidates \
                     the prompt cache on every turn",
                    section.name(),
                    section.position(),
                    SectionPosition::TailMessage,
                )));
            }
            if !section.stability().is_volatile() {
                return Err(Error::config(format!(
                    "dynamic prompt section `{}` declares stability `{}`, but a generated prompt \
                     may only contribute `{}` sections",
                    section.name(),
                    section.stability(),
                    SectionStability::Volatile,
                )));
            }
        }

        Ok(self.sections.clone())
    }

    /// Lowers this prompt into tail input items, rejecting any attempt to reach the stable prefix.
    ///
    /// # Errors
    ///
    /// Propagates the placement failures described on [`Self::volatile_tail_sections`].
    pub fn lower_to_tail_items(&self) -> Result<Vec<ModelInputItem>> {
        Ok(self.lower()?.0)
    }
}

impl fmt::Debug for ResolvedPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedPrompt")
            .field("bytes", &self.text.len())
            .field("sections_count", &self.sections.len())
            .field("source", &self.source)
            .field("version", &self.version)
            .field("content_hash", &self.content_hash)
            .field("provenance", &self.provenance)
            .finish_non_exhaustive()
    }
}

/// Asynchronous generator for dynamic prompt content.
#[async_trait]
pub trait DynamicPromptHandler: Send + Sync {
    /// Evaluates the dynamic prompt against the live run context.
    async fn resolve(&self, context: &RunContext) -> Result<ResolvedPrompt>;
}

/// Shortest cached span worth asking a provider to cache, in estimated tokens.
///
/// Every provider family that caches at all ignores a shorter span, so asking buys nothing and may
/// cost a breakpoint on providers that ration them.
///
/// **The span is the whole cached prefix, not the instructions.** The tool table sits in the same
/// prefix and is frequently the larger half of it, and hosted tools appear only once an adapter has
/// merged them — which is why this constant is applied by the adapter, the one place that sees the
/// final wire form, rather than by whoever assembled the prompt.
///
/// The exact floor is per model rather than universal (Anthropic's Haiku models want twice this).
/// This is the common lower bound; refining it belongs with the model capability matrix.
pub const MIN_CACHEABLE_PREFIX_TOKENS: usize = 1024;

/// A protocol-neutral statement of **what** should be cached, carrying no opinion on **how**.
///
/// It holds two facts and deliberately nothing else: which bytes form the stable prefix — as a
/// hash, so a request can be checked against the plan it claims — and the span of calls that
/// should land on the same cache entry.
///
/// # Why the strategy is not here
///
/// An earlier shape carried a `ProviderCacheStrategy` naming `AnthropicEphemeral` /
/// `OpenAiInstructions` / `OpenAiChatMessages`, plus breakpoints whose `is_ephemeral` and per-mark
/// `ttl_seconds` are Anthropic's `cache_control` shape and nobody else's. That put three vendors'
/// wire vocabulary inside the provider-neutral kernel, and derived it from
/// [`ApiProtocol`](crate::model::ApiProtocol) — which the protocol matrix explicitly forbids:
/// whether an endpoint honours `prompt_cache_key` follows from *being first-party `OpenAI`*, not
/// from speaking the Responses wire format, so a compatible gateway behind a custom `base_url`
/// would have been sent a field it may reject. Lowering this plan is the adapter's job, and
/// whether a given endpoint supports a mechanism is a provider fact, declared on its registration.
///
/// `cache_scope` identifies the span of calls that should share a cache entry — a session, or
/// failing that a run. A value that changes every turn is worse than `None`, because it partitions
/// the cache instead of sharing it.
///
/// # Why there is no "is this worth caching" verdict here
///
/// Because nothing at this layer can reach one. The floor applies to the whole cached prefix, and
/// the prefix is the instructions *plus the tool table* — including provider-hosted tools that have
/// no protocol-neutral representation and exist only after an adapter has merged them into the wire
/// request. Judging here would mean judging on the instructions alone, which denies caching to
/// exactly the requests that would benefit most: a short instruction block in front of a dozen tool
/// schemas is well past the floor while looking, on its own, far below it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePlan {
    prefix_hash: ContentHash,
    cache_scope: Option<String>,
}

impl CachePlan {
    /// Creates a plan naming the given prefix hash.
    #[must_use]
    pub const fn new(prefix_hash: ContentHash) -> Self {
        Self {
            prefix_hash,
            cache_scope: None,
        }
    }

    /// Builds the plan naming one stable prefix.
    ///
    /// Always returns a plan. Whether it is worth acting on is the adapter's call — see the type
    /// documentation.
    #[must_use]
    pub fn for_prefix(prefix: &str, cache_scope: Option<&str>) -> Self {
        let plan = Self::new(ContentHash::compute(prefix));
        match cache_scope {
            Some(scope) => plan.with_cache_scope(scope),
            None => plan,
        }
    }

    /// Attaches the span of calls that should share one cache entry.
    #[must_use]
    pub fn with_cache_scope(mut self, scope: impl Into<String>) -> Self {
        self.cache_scope = Some(scope.into());
        self
    }

    /// Stable prefix hash targeted by this cache plan.
    #[must_use]
    pub const fn prefix_hash(&self) -> &ContentHash {
        &self.prefix_hash
    }

    /// Span of calls that should land on the same cache entry, if one was named.
    #[must_use]
    pub fn cache_scope(&self) -> Option<&str> {
        self.cache_scope.as_deref()
    }
}
