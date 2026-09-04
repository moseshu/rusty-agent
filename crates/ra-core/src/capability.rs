//! The assembly unit that packages tools, prompt text, sampling settings, and context transforms.
//!
//! A [`Capability`] is one installable thing an agent can be given. Carrying all four
//! contributions on one trait is the point: an implementation that adds a tool also adds the
//! paragraph telling the model the tool exists, and neither can be switched on without the other.
//! Splitting them across a tool registry, a prompt-fragment registry, and a settings table is what
//! produces a surface whose prompt describes tools it no longer advertises.
//!
//! [`ContextProcessor`] remains a contract of its own rather than a method on the trait. It is the
//! richest of the four — it is asynchronous, it may ask the runtime for a model-produced summary,
//! and it emits authoritative records — and it is installed on the run configuration directly by
//! hosts that want one without an enclosing capability. A capability that transforms context
//! implements both traits and returns itself from [`Capability::context_processor`], so there is
//! one context-processing contract rather than a weaker second copy on this trait.
//!
//! Implementations live in service or product crates; the kernel only carries the contracts.

use std::{borrow::Cow, collections::BTreeSet, fmt, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    context::RunContext,
    error::{Error, Result},
    item::{ItemId, ModelInputItem, ModelResponse, RunItem},
    model::{ModelOutputSchema, ModelSettings},
    prompt::{PromptSection, PromptSectionName, PromptSource},
    state::RunId,
    tool::Tool,
    usage::Usage,
};

/// The family a capability belongs to, and the name a dependency on it is declared against.
///
/// This is an open newtype rather than a closed enum, for the same reason
/// [`ToolNamespace`](crate::tool::ToolNamespace) is one: a plugin or an integration has to be able
/// to introduce a family without changing `ra-core`. The built-in families are associated
/// constants so the set has one spelling rather than one per implementor.
///
/// # Why not an enum with a `Custom` variant
///
/// Because equality is the entire operation this type exists for:
/// [`Capability::required_capabilities`] is checked by comparing declared families against
/// installed ones. An enum carrying `Custom(Cow<'static, str>)` beside a `Shell` variant makes
/// `Custom("shell")` and `Shell` two unequal values naming the same capability — so a third-party
/// capability that declares its dependency the obvious way fails validation against a capability
/// that is installed and present. A newtype has one representation per name, and the constructor is
/// the only way to reach it.
///
/// The same reasoning drives the spelling rules in [`CapabilityFamily::new`]: `Shell` and `shell`
/// would be two names for one thing, so only one of them is a name at all.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CapabilityFamily(Cow<'static, str>);

impl CapabilityFamily {
    /// Command execution.
    pub const SHELL: Self = Self::from_static("shell");
    /// File reading and workspace traversal.
    pub const FILESYSTEM: Self = Self::from_static("filesystem");
    /// Patch application, the editing entry point.
    pub const APPLY_PATCH: Self = Self::from_static("apply_patch");
    /// Text and file-pattern search.
    pub const SEARCH: Self = Self::from_static("search");
    /// Plan and task tracking.
    pub const TODO: Self = Self::from_static("todo");
    /// Context compaction.
    pub const COMPACTION: Self = Self::from_static("compaction");
    /// Long-term memory retrieval and retention.
    pub const MEMORY: Self = Self::from_static("memory");
    /// Image viewing.
    pub const VIEW_IMAGE: Self = Self::from_static("view_image");
    /// Web fetching and search.
    pub const WEB: Self = Self::from_static("web");
    /// Skill discovery and loading.
    pub const SKILLS: Self = Self::from_static("skills");

    /// Creates a family from a name that is already known to be canonical.
    #[must_use]
    const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// Creates a family from a name.
    ///
    /// A name is lowercase ASCII: it starts with a letter and continues with letters, digits, `_`,
    /// or `.` — the last for namespacing a third party's own families, as in `plugin.browser`.
    ///
    /// # Errors
    ///
    /// Returns a caller error when the name is empty or holds anything outside that set.
    pub fn new(name: impl Into<Cow<'static, str>>) -> Result<Self> {
        let name = name.into();
        let mut characters = name.chars();
        let starts_with_letter = characters.next().is_some_and(|c| c.is_ascii_lowercase());
        let rest_is_canonical = characters
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
        if !starts_with_letter || !rest_is_canonical {
            return Err(Error::caller(format!(
                "capability family `{name}` is not a canonical name: it must start with a \
                 lowercase ASCII letter and continue with lowercase letters, digits, `_`, or `.`"
            )));
        }
        Ok(Self(name))
    }

    /// Stable string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Prompt provenance for a section this family contributes.
    ///
    /// Assembly attributes a capability's prompt text to the capability, and the family is that
    /// attribution. Deriving it here rather than at each call site is what keeps the tag in a
    /// prompt dump equal to the value [`Capability::kind`] returns.
    #[must_use]
    pub fn prompt_source(&self) -> PromptSource {
        PromptSource::Capability(self.as_str().to_owned())
    }

    /// The prompt section name a fragment from this family claims.
    ///
    /// The family *is* the name, so one family cannot hold two slots in the prefix and two
    /// families cannot contend for one. Assembly already refuses two capabilities that claim the
    /// same section name; deriving the name here is what keeps that refusal from being something a
    /// capability can walk into by accident — a capability free to name its own section is free to
    /// land on an unrelated topic's slot, and the collision message would say nothing about the two
    /// having been meant as different things.
    ///
    /// It is also what gives the canonical prefix order something stable to rank. A section's
    /// position in the cached span is keyed on its name, and a name chosen per implementation is a
    /// name the order table cannot know in advance.
    #[must_use]
    pub fn prompt_section_name(&self) -> PromptSectionName {
        PromptSectionName::new(self.0.clone())
    }
}

impl fmt::Display for CapabilityFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CapabilityFamily {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// One installable unit of agent capability.
///
/// Everything a capability contributes is optional except its identity, so a new contribution
/// point can be added to this trait without breaking implementations that predate it.
///
/// # The order assembly calls these in
///
/// 1. The host constructs the capabilities it wants to install. Anything that can fail — opening a
///    workspace, connecting a store — belongs to that constructor, which is why nothing below is
///    a place to do work that can fail for configuration reasons.
/// 2. [`Self::required_capabilities`] is checked against the installed set, before anything else
///    runs. A missing dependency is a configuration error, and reporting it after a model has been
///    paid to read a half-assembled surface is reporting it too late.
/// 3. [`Self::bind`] gives each capability the run it is about to serve.
/// 4. [`Self::tools`] contributes to the agent's tool set, which a tool profile then selects from.
/// 5. [`Self::instructions`] contributes prompt sections.
/// 6. [`Self::sampling_params`] folds over the agent's model-settings layer, in installation order.
/// 7. [`Self::context_processor`] is installed on the run configuration and runs before each
///    ordinary model call.
///
/// Steps 4 through 7 happen after binding, so anything a capability needs from the run it captured
/// in step 3 rather than receiving as a parameter here.
///
/// # Deviations from the reference contract
///
/// - **`clone_for_run` and `bind` are one operation.** The reference implementation deep-copies a
///   capability per run and then binds a live session into the copy, because binding mutates the
///   object and one capability instance serves many runs. In Rust a capability is shared as an
///   `Arc` and never mutated, so [`Self::bind`] returns the bound value instead of writing into
///   `self` — and the deep copy exists only to make mutation safe, so it has nothing left to do.
/// - **There is no `process_manifest`.** A manifest there is a sandbox session's declared mount
///   set; this framework has no such value yet, and inventing one to fill a method signature would
///   freeze a shape before the sandbox that owns it exists.
#[async_trait]
pub trait Capability: Send + Sync + 'static {
    /// Which family this capability belongs to.
    ///
    /// Returned by value rather than borrowed: the built-in families are borrowed constants, and a
    /// borrowing accessor would force every implementation to store a field purely to have
    /// something to lend out.
    fn kind(&self) -> CapabilityFamily;

    /// Families that must be installed alongside this one.
    ///
    /// This is a declaration, not a check. Validating it — and reporting which capability is
    /// missing which dependency — belongs to assembly, which is the only party that knows the
    /// complete installed set.
    fn required_capabilities(&self) -> BTreeSet<CapabilityFamily> {
        BTreeSet::new()
    }

    /// Tools this capability contributes to the agent.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    /// The prompt section this installed capability contributes to an agent's static prefix.
    ///
    /// This method runs before any [`RunContext`] exists, so its answer must depend only on the
    /// installed capability and host configuration. It is for an agent builder or prompt dump that
    /// needs to construct one cached prefix shared by many runs. A capability bound to a run must
    /// not use this method for text it derives from that run; that text belongs to
    /// [`Self::instructions`].
    ///
    /// The section must use both [`CapabilityFamily::prompt_section_name`] and
    /// [`CapabilityFamily::prompt_source`] for this capability's family. The runtime checks those
    /// claims so a capability cannot take another topic's prefix slot or make a prompt dump blame
    /// another contributor.
    ///
    /// # Errors
    ///
    /// Returns whatever reading the static fragment's source material produced.
    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        Ok(None)
    }

    /// The prompt section this capability contributes, resolved once per run.
    ///
    /// Resolved during assembly and not again, which is what lets the section reach the cached
    /// prefix: text that varies per turn invalidates the prompt cache on every call. A capability
    /// whose contribution genuinely does vary per turn belongs in
    /// [`ContextProcessor::process_context`], which runs against the live request and writes into
    /// the tail rather than the prefix.
    ///
    /// The section must use both [`CapabilityFamily::prompt_section_name`] and
    /// [`CapabilityFamily::prompt_source`] for this capability's family.
    ///
    /// # Errors
    ///
    /// Returns whatever reading the fragment's source material produced.
    async fn instructions(&self) -> Result<Option<PromptSection>> {
        Ok(None)
    }

    /// Model settings this capability needs, folded onto the agent's layer.
    ///
    /// Each capability receives the settings the ones before it produced and returns the settings
    /// the ones after it will see, so the fold is ordered and a later capability can override an
    /// earlier one on purpose. The result becomes the agent layer of the settings resolve; it does
    /// not reach the provider or model layers, which are not a capability's to speak for.
    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        settings
    }

    /// The context transformation this capability performs, if it performs one.
    ///
    /// An implementation that also implements [`ContextProcessor`] returns `Some(self)`.
    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        None
    }

    /// Binds this capability to the run it is about to serve.
    ///
    /// `None` — the default — means the capability holds no per-run state and the installed value
    /// is used as it is. `Some` supplies the bound form, which assembly uses in place of the
    /// installed one for this run only. Returning a new value rather than mutating `self` is what
    /// lets one installed capability serve concurrent runs without a per-run deep copy.
    ///
    /// The parameter is the run context because that is this framework's one live per-run value:
    /// tools, guards, hooks, and dynamic instructions already read it, and a second per-run context
    /// object built for capabilities alone would be a second answer to "who is running".
    ///
    /// # Errors
    ///
    /// Returns an error when this capability cannot serve the run at all — a dependency it needs is
    /// absent from the host context, for instance. Assembly propagates it rather than continuing
    /// with an unbound capability.
    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        let _ = context;
        Ok(None)
    }
}

/// One context transformation installed by a host.
///
/// A processor receives authoritative history separately from the model-facing prefix and tail.
/// It may replace only the history projection while emitting new authoritative records such as a
/// compaction summary. The runner appends those records; a processor never mutates `RunState`
/// itself.
///
/// A [`Capability`] that transforms context implements this trait too and returns itself from
/// [`Capability::context_processor`]. A host may also install a processor on its own, without an
/// enclosing capability, which is why this is a separate contract rather than a method there.
#[async_trait]
pub trait ContextProcessor: Send + Sync {
    /// Transforms the context before the ordinary model request is sent.
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult>;
}

/// Facts a context processor needs for one pending model request.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ContextProcessorRequest {
    run_id: RunId,
    current_turn: u64,
    record_id: ItemId,
    model_name: Option<String>,
    prefix: Vec<ModelInputItem>,
    history: Vec<RunItem>,
    suffix: Vec<ModelInputItem>,
    input: Vec<ModelInputItem>,
}

impl ContextProcessorRequest {
    /// Creates the input to one context processor.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        run_id: RunId,
        current_turn: u64,
        record_id: ItemId,
        model_name: Option<String>,
        prefix: Vec<ModelInputItem>,
        history: Vec<RunItem>,
        suffix: Vec<ModelInputItem>,
        input: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            run_id,
            current_turn,
            record_id,
            model_name,
            prefix,
            history,
            suffix,
            input,
        }
    }

    /// Identity of the run whose context is being processed.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Whole-run ordinal of the pending turn.
    #[must_use]
    pub const fn current_turn(&self) -> u64 {
        self.current_turn
    }

    /// Reserved ID for an authoritative record this processing pass emits.
    #[must_use]
    pub const fn record_id(&self) -> &ItemId {
        &self.record_id
    }

    /// Resolved model name, when the resolver selected one explicitly.
    #[must_use]
    pub fn model_name(&self) -> Option<&str> {
        self.model_name.as_deref()
    }

    /// Caller-owned input that precedes the authoritative generated history.
    #[must_use]
    pub fn prefix(&self) -> &[ModelInputItem] {
        &self.prefix
    }

    /// Complete authoritative records the processor may project.
    #[must_use]
    pub fn history(&self) -> &[RunItem] {
        &self.history
    }

    /// Ephemeral model-input items that follow history, such as a budget reminder.
    #[must_use]
    pub fn suffix(&self) -> &[ModelInputItem] {
        &self.suffix
    }

    /// Complete request input before this processor runs.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }
}

/// Model request a context processor asks the runtime to perform.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ContextSummaryRequest {
    input: Vec<ModelInputItem>,
    instructions: String,
    output_schema: Option<ModelOutputSchema>,
}

impl ContextSummaryRequest {
    /// Creates a summary request over the supplied model-visible input.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>, instructions: impl Into<String>) -> Self {
        Self {
            input,
            instructions: instructions.into(),
            output_schema: None,
        }
    }

    /// Requires a provider-neutral structured output schema for the summary.
    #[must_use]
    pub fn with_output_schema(mut self, output_schema: ModelOutputSchema) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    /// Input to summarize, before the runtime appends the task instruction.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Task instruction appended by the runtime as a volatile user message.
    #[must_use]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// Optional structured-output contract for the summary response.
    #[must_use]
    pub const fn output_schema(&self) -> Option<&ModelOutputSchema> {
        self.output_schema.as_ref()
    }
}

/// Result of a context-summary request.
#[derive(Debug, Clone)]
pub struct ContextSummaryResponse {
    text: String,
    response: ModelResponse,
}

impl ContextSummaryResponse {
    /// Creates a summary response with the usage its request consumed.
    #[must_use]
    pub fn new(text: impl Into<String>, response: ModelResponse) -> Self {
        Self {
            text: text.into(),
            response,
        }
    }

    /// Returned summary text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Usage charged by the summary request.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        self.response.usage()
    }

    /// Complete provider response that produced this summary.
    #[must_use]
    pub const fn response(&self) -> &ModelResponse {
        &self.response
    }
}

/// Runtime callback a processor uses when it needs a model-produced summary.
#[async_trait]
pub trait ContextSummarizer: Send + Sync {
    /// Produces one bounded summary without exposing a capability's implementation to the runner.
    async fn summarize(&self, request: ContextSummaryRequest) -> Result<ContextSummaryResponse>;
}

/// One processor's replacement model input and authoritative records.
///
/// Spend is deliberately absent as a field. It is derived from [`Self::model_responses`], because
/// a separately supplied total is a second ledger: a processor that returns two responses and the
/// usage of one would under-report the run's spend with nothing able to notice.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ContextProcessorResult {
    input: Vec<ModelInputItem>,
    generated_items: Vec<RunItem>,
    model_responses: Vec<ModelResponse>,
}

impl ContextProcessorResult {
    /// Creates a result that only changes the model-facing input.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>) -> Self {
        Self {
            input,
            generated_items: Vec::new(),
            model_responses: Vec::new(),
        }
    }

    /// Adds authoritative records emitted by this processor.
    #[must_use]
    pub fn with_generated_items(mut self, generated_items: Vec<RunItem>) -> Self {
        self.generated_items = generated_items;
        self
    }

    /// Adds model responses produced while creating this projection.
    #[must_use]
    pub fn with_model_responses(mut self, model_responses: Vec<ModelResponse>) -> Self {
        self.model_responses = model_responses;
        self
    }

    /// Replacement input for the ordinary model call.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Records the runner must append to authoritative history.
    #[must_use]
    pub fn generated_items(&self) -> &[RunItem] {
        &self.generated_items
    }

    /// Model responses produced while creating this projection.
    #[must_use]
    pub fn model_responses(&self) -> &[ModelResponse] {
        &self.model_responses
    }

    /// Spend this projection cost, summed from the responses that paid it.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.model_responses
            .iter()
            .fold(Usage::default(), |total, response| {
                total.accumulate(response.usage())
            })
    }
}
