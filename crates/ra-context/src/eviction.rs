//! Selective trimming of tool outputs from older model-input turns.
//!
//! [`ToolOutputTrimmer`] mirrors the `OpenAI` Agents SDK's input filter: it leaves the most recent
//! user turns intact, while replacing oversized outputs in earlier turns with a short preview. It
//! is a pure projection over [`ModelInputItem`] values, so callers retain the complete session
//! history and only the request sent to a model becomes smaller.
//!
//! # Provider-neutral boundary
//!
//! The upstream filter also recognizes `OpenAI`'s `tool_search_output` item. `ra-core` deliberately
//! has no provider-specific tool-search replay item, so this module trims only provider-neutral
//! [`ToolCallOutput`] records. Opaque image and file blocks are counted and named in the
//! replacement, never previewed.
//!
//! # Two deliberate deviations from upstream
//!
//! **The protected window has an item-count fallback.** Upstream measures it in user messages alone,
//! so a run with one request and fifty tool calls — the shape of every coding session, and the
//! case this milestone exists for — never trims anything at all. When the input holds fewer than
//! `recent_turns` user messages, the boundary falls back to `recent_items` trailing items instead
//! of protecting the whole history.
//!
//! **A trimmed structured result stays a [`ToolOutput`] when its rendered metadata fits.** The
//! [`ObservationMetadata`](ra_core::tool::ObservationMetadata) and
//! [`ArtifactRef`](ra_core::tool::ArtifactRef) survive alongside the summary whenever their
//! combined model view stays within the configured ceiling. If they alone exhaust that ceiling,
//! the request projection falls back to a bare summary; the authoritative session record remains
//! untouched in either case.

use std::collections::{BTreeMap, BTreeSet};

use ra_core::{
    error::{Error, Result},
    item::{CallId, MessageRole, ModelInputItem, ToolCallOutput},
    state::{RunId, ToolOutputReferenceTracker},
    tool::{ModelExcerpt, ModelInputProjector, ToolOutput, ToolOutputBlock},
};
use serde_json::Value;

use crate::budget::{artifact_ref, opaque_block_bytes, source_text};

/// Default number of recent user turns whose tool outputs remain complete.
pub const DEFAULT_RECENT_TURNS: usize = 2;
/// Default number of trailing items protected when the input has too few user turns to bound.
pub const DEFAULT_RECENT_ITEMS: usize = 20;
/// Default output size above which an older result becomes eligible for trimming.
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 500;
/// Default amount of text retained in a trimmed preview.
pub const DEFAULT_PREVIEW_CHARS: usize = 200;
/// Default number of completed turns a result may go unreferenced before it is eligible.
pub const DEFAULT_MAX_UNREFERENCED_TURNS: u64 = 8;
/// The shortest replacement that still tells a model its result was cut.
const MINIMAL_SUMMARY: &str = "[Trimmed]";

/// Stands in for a result whose call is not in this input.
const UNKNOWN_TOOL: &str = "unknown_tool";

/// The positional window [`ToolOutputReferenceTrimmer`] hands the shared renderer.
///
/// Reference-based selection never consults it. One is the smallest value the shared constructor
/// admits, and naming it here keeps `Default` and `new` building the same inner trimmer for a type
/// that derives `PartialEq`.
const UNUSED_POSITIONAL_WINDOW: usize = 1;

/// A sliding-window model-input filter for oversized outputs from older tool calls.
///
/// The latest `recent_turns` user messages and every item following them are untouched; an input
/// with fewer user messages than that protects its last `recent_items` items instead. An older
/// output is eligible only when it exceeds `max_output_chars`; when `trimmable_tools` is set, its
/// paired call must also name a member of that set. Results with no call in the input are trimmed
/// only when no allowlist is configured, matching the upstream filter's conservative behavior.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputTrimmer {
    recent_turns: usize,
    recent_items: usize,
    max_output_chars: usize,
    preview_chars: usize,
    trimmable_tools: Option<BTreeSet<String>>,
}

impl Default for ToolOutputTrimmer {
    fn default() -> Self {
        Self {
            recent_turns: DEFAULT_RECENT_TURNS,
            recent_items: DEFAULT_RECENT_ITEMS,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
            preview_chars: DEFAULT_PREVIEW_CHARS,
            trimmable_tools: None,
        }
    }
}

impl ToolOutputTrimmer {
    /// Creates a validated trimmer with the supplied sliding-window and preview limits.
    ///
    /// # Errors
    ///
    /// Returns an error when no recent user turn is retained, or when `max_output_chars` is below
    /// the marker a replacement cannot go without. A ceiling under that floor cannot produce a
    /// conforming replacement for *any* result, so it is refused once here rather than silently
    /// yielding a fragment of the marker on every call.
    pub fn new(recent_turns: usize, max_output_chars: usize, preview_chars: usize) -> Result<Self> {
        if recent_turns == 0 {
            return Err(Error::config(
                "tool-output recent_turns must be at least one",
            ));
        }
        let floor = MINIMAL_SUMMARY.chars().count();
        if max_output_chars < floor {
            return Err(Error::config(format!(
                "tool-output max_output_chars must leave room for `{MINIMAL_SUMMARY}`: at least \
                 {floor}, not {max_output_chars}"
            )));
        }
        Ok(Self {
            recent_turns,
            max_output_chars,
            preview_chars,
            ..Self::default()
        })
    }

    /// Sets how many trailing items stay protected when user turns cannot bound the window.
    ///
    /// # Errors
    ///
    /// Returns an error for zero, which would leave the newest tool result trimmable in exactly
    /// the runs this fallback exists to serve.
    pub fn with_recent_items(mut self, recent_items: usize) -> Result<Self> {
        if recent_items == 0 {
            return Err(Error::config(
                "tool-output recent_items must be at least one",
            ));
        }
        self.recent_items = recent_items;
        Ok(self)
    }

    /// Restricts trimming to these exact model-facing tool names.
    ///
    /// `None` permits every tool output. An empty set intentionally permits none, which lets a
    /// host disable a configured trimmer without replacing its other settings.
    #[must_use]
    pub fn with_trimmable_tools(mut self, trimmable_tools: Option<BTreeSet<String>>) -> Self {
        self.trimmable_tools = trimmable_tools;
        self
    }

    /// Number of latest user turns protected from trimming.
    #[must_use]
    pub const fn recent_turns(&self) -> usize {
        self.recent_turns
    }

    /// Number of trailing items protected when the input has too few user turns.
    #[must_use]
    pub const fn recent_items(&self) -> usize {
        self.recent_items
    }

    /// Older output size threshold, and the ceiling every replacement respects.
    #[must_use]
    pub const fn max_output_chars(&self) -> usize {
        self.max_output_chars
    }

    /// Maximum readable text retained in a trimmed preview.
    #[must_use]
    pub const fn preview_chars(&self) -> usize {
        self.preview_chars
    }

    /// Optional exact-name allowlist for trimmable tools.
    #[must_use]
    pub const fn trimmable_tools(&self) -> Option<&BTreeSet<String>> {
        self.trimmable_tools.as_ref()
    }

    /// Produces a model-input view with eligible older outputs replaced by compact previews.
    ///
    /// The supplied input is never mutated, and call and output records remain paired.
    ///
    /// # Errors
    ///
    /// Returns an error if an item claims to contain a structured [`ToolOutput`] that this build
    /// cannot read. Passing such a record onward would fail in the provider adapter as well.
    pub fn trim_model_input(&self, input: &[ModelInputItem]) -> Result<Vec<ModelInputItem>> {
        let boundary = self.recent_boundary(input);
        if boundary == 0 {
            return Ok(input.to_vec());
        }

        let tool_names = tool_names_by_call_id(input);
        let mut trimmed = Vec::with_capacity(input.len());
        for (index, item) in input.iter().enumerate() {
            let ModelInputItem::ToolCallOutput(output) = item else {
                trimmed.push(item.clone());
                continue;
            };

            if index >= boundary || !self.is_trimmable(output, &tool_names) {
                trimmed.push(item.clone());
                continue;
            }

            let tool_name = tool_names
                .get(output.call_id())
                .copied()
                .unwrap_or(UNKNOWN_TOOL);
            let Some(replacement) = self.trim_output(output.output(), tool_name)? else {
                trimmed.push(item.clone());
                continue;
            };
            trimmed.push(ModelInputItem::ToolCallOutput(
                output.clone().with_output(replacement),
            ));
        }
        Ok(trimmed)
    }

    /// Index of the first item the trimmer must leave alone.
    ///
    /// The item count is a fallback, not a floor: an input with enough user messages is bounded
    /// the way upstream bounds it, and only one without them falls back. Making it a floor
    /// instead would silently protect the whole history of a short session, because the trailing
    /// item count can easily reach further back than the user-turn window does.
    fn recent_boundary(&self, input: &[ModelInputItem]) -> usize {
        let mut user_messages = 0;
        for (index, item) in input.iter().enumerate().rev() {
            if matches!(item, ModelInputItem::Message(message) if message.role() == MessageRole::User)
            {
                user_messages += 1;
                if user_messages >= self.recent_turns {
                    return index;
                }
            }
        }
        input.len().saturating_sub(self.recent_items)
    }

    fn is_trimmable(&self, output: &ToolCallOutput, tool_names: &BTreeMap<&CallId, &str>) -> bool {
        self.trimmable_tools.as_ref().is_none_or(|allowlist| {
            tool_names
                .get(output.call_id())
                .is_some_and(|name| allowlist.contains(*name))
        })
    }

    /// The payload that replaces one eligible result, or `None` when it is not oversized.
    fn trim_output(&self, output: &Value, tool_name: &str) -> Result<Option<Value>> {
        self.trim_output_with_artifact(output, tool_name, None)
    }

    /// The payload that replaces one eligible result, optionally naming its retained original.
    ///
    /// A reference-aware projection passes an artifact identity here even when the result was never
    /// individually budgeted. The artifact note is priced before preview text; if metadata plus
    /// that note cannot fit, this takes the same bare-summary fallback as the positional trimmer.
    ///
    /// **A reference turns a legacy result into a structured one.** An unrecognized payload has
    /// nowhere to carry a [`ModelExcerpt`], so replacing one while naming its artifact means
    /// emitting a [`ToolOutput`] where a bare string stood. Called without a reference — every call
    /// from the positional trimmer — a legacy result stays a string, exactly as before. The
    /// projection is the only thing reshaped; the authoritative session record still holds the
    /// original payload in its original shape.
    fn trim_output_with_artifact(
        &self,
        output: &Value,
        tool_name: &str,
        artifact_ref: Option<&ra_core::tool::ArtifactRef>,
    ) -> Result<Option<Value>> {
        let Some(structured) = ToolOutput::from_stored(output)? else {
            let text = legacy_output_text(output)?;
            let source = TrimSource::from_text(&text);
            if source.output_chars <= self.max_output_chars {
                return Ok(None);
            }

            let Some(artifact_ref) = artifact_ref else {
                return Ok(Some(Value::String(self.summary_text(
                    tool_name,
                    &source,
                    self.max_output_chars,
                ))));
            };
            let placeholder = ToolOutput::text("");
            let Some(summary_budget) =
                structured_summary_budget(&placeholder, Some(artifact_ref), self.max_output_chars)?
            else {
                return Ok(Some(Value::String(self.summary_text(
                    tool_name,
                    &source,
                    self.max_output_chars,
                ))));
            };
            let summary = self.summary_text(tool_name, &source, summary_budget);
            let trimmed = trimmed_structured_output(&placeholder, summary, Some(artifact_ref))?;
            return serde_json::to_value(trimmed).map(Some).map_err(|error| {
                Error::caller(format!(
                    "a trimmed tool result could not be re-serialized: {error}"
                ))
            });
        };

        let source = TrimSource::from_structured(&structured)?;
        if source.output_chars <= self.max_output_chars {
            return Ok(None);
        }
        // The supplied reference is passed through unresolved: `trimmed_structured_output` owns the
        // rule that a reference the result already carries wins over a replacement, and both the
        // budget below and the projection after it route through that one function. Resolving it
        // here as well would put the same precedence in two places, free to drift — and a drift
        // would price one reference while installing another, breaking the ceiling this budget
        // exists to enforce.
        let Some(summary_budget) =
            structured_summary_budget(&structured, artifact_ref, self.max_output_chars)?
        else {
            return Ok(Some(Value::String(self.summary_text(
                tool_name,
                &source,
                self.max_output_chars,
            ))));
        };
        let summary = self.summary_text(tool_name, &source, summary_budget);
        let trimmed = trimmed_structured_output(&structured, summary, artifact_ref)?;
        serde_json::to_value(trimmed).map(Some).map_err(|error| {
            Error::caller(format!(
                "a trimmed tool result could not be re-serialized: {error}"
            ))
        })
    }

    /// Renders a replacement that fits the given model-facing character allowance.
    fn summary_text(
        &self,
        tool_name: &str,
        source: &TrimSource,
        max_output_chars: usize,
    ) -> String {
        // The header names the preview's length, so its own width depends on that number. Two
        // renders converge and a third could not change anything: the fitted preview is never
        // longer than the requested one, and a shorter preview only ever shortens the header,
        // which cannot take budget back.
        let requested = source.text.chars().count().min(self.preview_chars);
        let requested_header = header(tool_name, source, requested);
        let preview_chars = requested.min(preview_budget(max_output_chars, &requested_header));
        let fitted_header = header(tool_name, source, preview_chars);

        if fitted_header.chars().count() > max_output_chars {
            // The caller guarantees room for the marker itself, but a descriptive header can
            // still exceed what remains after structured metadata has taken its share.
            return MINIMAL_SUMMARY.to_owned();
        }
        if preview_chars == 0 {
            return fitted_header;
        }
        format!(
            "{fitted_header}\n{}",
            take_chars(&source.text, preview_chars)
        )
    }
}

/// A selective tool-output trimmer whose eligibility follows explicit reference recency.
///
/// This is intentionally separate from [`ToolOutputTrimmer`]. The latter protects a positional
/// user-turn window; this type preserves an output however far back it appears while a later turn
/// still references it. Both use the same bounded replacement renderer, retain output metadata,
/// and leave the authoritative input unchanged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputReferenceTrimmer {
    trimmer: ToolOutputTrimmer,
    max_unreferenced_turns: u64,
}

impl Default for ToolOutputReferenceTrimmer {
    /// Exactly what [`ToolOutputReferenceTrimmer::new`] builds from the module defaults.
    ///
    /// The inner positional window is pinned to the value `new` supplies rather than left at
    /// [`ToolOutputTrimmer`]'s own default. This type derives `PartialEq`, and a field it neither
    /// reads nor exposes must not be the thing that makes two identically configured trimmers
    /// compare unequal.
    fn default() -> Self {
        Self {
            trimmer: ToolOutputTrimmer {
                recent_turns: UNUSED_POSITIONAL_WINDOW,
                ..ToolOutputTrimmer::default()
            },
            max_unreferenced_turns: DEFAULT_MAX_UNREFERENCED_TURNS,
        }
    }
}

impl ToolOutputReferenceTrimmer {
    /// Creates a reference-aware trimmer with the supplied completed-turn and output limits.
    ///
    /// # Budgeting `max_output_chars` against a real artifact reference
    ///
    /// Unlike the positional trimmer, this one mints an [`ArtifactRef`](ra_core::tool::ArtifactRef)
    /// for every result it replaces, and the locator sentence
    /// [`ToolOutput::model_blocks`] renders around it is charged against `max_output_chars` before
    /// any preview text. That sentence is 69 characters plus the reference, and the reference is
    /// `tool-output/` followed by both identifiers hex-encoded — which doubles their length. A
    /// generated [`RunId`] is a 36-character UUID and provider call IDs run near 30, so a realistic
    /// pair costs 143 characters of reference and 213 of fixed overhead once the block separator is
    /// paid. A ceiling under roughly 222 therefore cannot carry the marker *and* the locator, and
    /// every replacement silently falls back to a bare summary with no artifact reference at all.
    ///
    /// That floor is deliberately not validated here. It depends on identifiers this constructor
    /// has not been given, and inventing a bound for them would either reject workable budgets or
    /// promise a guarantee this function cannot give — the same reasoning
    /// [`budget`](crate::budget) applies to its own floor. Budget for the identifiers the host
    /// actually issues; [`DEFAULT_MAX_OUTPUT_CHARS`] leaves room for them.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero reference window or for a replacement ceiling that cannot carry
    /// the shared `[Trimmed]` marker.
    pub fn new(
        max_unreferenced_turns: u64,
        max_output_chars: usize,
        preview_chars: usize,
    ) -> Result<Self> {
        if max_unreferenced_turns == 0 {
            return Err(Error::config(
                "tool-output max_unreferenced_turns must be at least one",
            ));
        }
        Ok(Self {
            // The positional window is irrelevant to reference-aware selection; the shared renderer
            // is constructed here anyway because it validates the output ceiling.
            trimmer: ToolOutputTrimmer::new(
                UNUSED_POSITIONAL_WINDOW,
                max_output_chars,
                preview_chars,
            )?,
            max_unreferenced_turns,
        })
    }

    /// Restricts trimming to these exact model-facing tool names.
    #[must_use]
    pub fn with_trimmable_tools(mut self, trimmable_tools: Option<BTreeSet<String>>) -> Self {
        self.trimmer = self.trimmer.with_trimmable_tools(trimmable_tools);
        self
    }

    /// Number of completed turns an output may go unreferenced before it is eligible.
    #[must_use]
    pub const fn max_unreferenced_turns(&self) -> u64 {
        self.max_unreferenced_turns
    }

    /// Older output size threshold, and the ceiling every replacement respects.
    #[must_use]
    pub const fn max_output_chars(&self) -> usize {
        self.trimmer.max_output_chars()
    }

    /// Maximum readable text retained in a trimmed preview.
    #[must_use]
    pub const fn preview_chars(&self) -> usize {
        self.trimmer.preview_chars()
    }

    /// Optional exact-name allowlist for trimmable tools.
    #[must_use]
    pub const fn trimmable_tools(&self) -> Option<&BTreeSet<String>> {
        self.trimmer.trimmable_tools()
    }

    /// Produces a model-input view with only stale, unreferenced oversized outputs replaced.
    ///
    /// Every output must be registered through [`ToolOutputReferenceTracker::record_turn`] before
    /// it can become eligible. The tracker and `run_id` must agree so a call ID from another run
    /// cannot accidentally select this run's result.
    ///
    /// # Errors
    ///
    /// Returns an error for a tracker from another run or for an unreadable structured result.
    pub fn trim_model_input(
        &self,
        run_id: &RunId,
        current_turn: u64,
        references: &ToolOutputReferenceTracker,
        input: &[ModelInputItem],
    ) -> Result<Vec<ModelInputItem>> {
        if references.run_id() != run_id {
            return Err(Error::caller(format!(
                "tool-output reference tracker belongs to run `{}`, not `{run_id}`",
                references.run_id()
            )));
        }

        let tool_names = tool_names_by_call_id(input);
        let mut trimmed = Vec::with_capacity(input.len());
        for item in input {
            let ModelInputItem::ToolCallOutput(output) = item else {
                trimmed.push(item.clone());
                continue;
            };
            if !references.is_unreferenced_for(
                output.call_id(),
                current_turn,
                self.max_unreferenced_turns,
            ) || !self.trimmer.is_trimmable(output, &tool_names)
            {
                trimmed.push(item.clone());
                continue;
            }

            let tool_name = tool_names
                .get(output.call_id())
                .copied()
                .unwrap_or(UNKNOWN_TOOL);
            let artifact_ref = artifact_ref(run_id, output.call_id())?;
            let Some(replacement) = self.trimmer.trim_output_with_artifact(
                output.output(),
                tool_name,
                Some(&artifact_ref),
            )?
            else {
                trimmed.push(item.clone());
                continue;
            };
            trimmed.push(ModelInputItem::ToolCallOutput(
                output.clone().with_output(replacement),
            ));
        }
        Ok(trimmed)
    }
}

impl ModelInputProjector for ToolOutputReferenceTrimmer {
    fn project_model_input(
        &self,
        run_id: &RunId,
        current_turn: u64,
        references: &ToolOutputReferenceTracker,
        input: &[ModelInputItem],
    ) -> Result<Vec<ModelInputItem>> {
        self.trim_model_input(run_id, current_turn, references, input)
    }
}

/// Characters left for a preview once its header and separating newline are paid for.
fn preview_budget(max_output_chars: usize, header: &str) -> usize {
    max_output_chars
        .saturating_sub(header.chars().count())
        .saturating_sub(1)
}

/// Names the tool, what its result measured, and what the replacement kept of it.
fn header(tool_name: &str, source: &TrimSource, preview_chars: usize) -> String {
    let opaque_note = if source.opaque_blocks == 0 {
        String::new()
    } else {
        format!("; dropped {} opaque block(s)", source.opaque_blocks)
    };
    format!(
        "[Trimmed: {tool_name} output — {} chars → {preview_chars} char preview{opaque_note}]",
        source.output_chars
    )
}

/// The model-facing content of one result, measured once and previewed from the same values.
struct TrimSource {
    /// Readable text, in block order.
    text: String,
    /// What the model would have been sent, readable and opaque alike.
    output_chars: usize,
    /// Blocks with no character count of their own: counted, named, never previewed.
    opaque_blocks: usize,
}

impl TrimSource {
    fn from_text(text: &str) -> Self {
        Self {
            output_chars: text.chars().count(),
            text: text.to_owned(),
            opaque_blocks: 0,
        }
    }

    /// Reads preview content separately from the complete provider-facing cost.
    ///
    /// **Previewing does not use [`ToolOutput::model_blocks`].** That renders the metadata note ahead of the content
    /// and the artifact locator after it, so previewing it spends the whole preview on the
    /// framework's prose — a result the context budget already excerpted leads with
    /// `[truncated by context budget: …]`, and the model would be handed that sentence and none
    /// of the output it asked for. The metadata is not lost by leaving it out here: it is carried
    /// over whole by [`trimmed_structured_output`] and rendered again on the way to the provider.
    fn from_structured(output: &ToolOutput) -> Result<Self> {
        let blocks = output
            .model_excerpt()
            .map_or_else(|| output.blocks(), ModelExcerpt::blocks);
        let text = source_text(blocks);
        let model_blocks = output.model_blocks();
        let mut output_chars = source_text(&model_blocks).chars().count();
        let mut opaque_blocks = 0;
        for block in model_blocks
            .iter()
            .filter(|block| block.as_text().is_none())
        {
            opaque_blocks += 1;
            // Priced in serialized bytes, the measure `budget` settled on: an image has no
            // character count that means anything, and a base64 payload costs the same either way.
            output_chars = output_chars.saturating_add(opaque_block_bytes(block)?);
        }
        Ok(Self {
            text,
            output_chars,
            opaque_blocks,
        })
    }
}

/// Rebuilds a trimmed result as a tool result rather than as a bare string.
///
/// The metadata travels with the summary, so a cut an earlier stage recorded and the sentence it
/// wrote for the model are still rendered by [`ToolOutput::model_blocks`]. When the result already
/// carried a [`ModelExcerpt`], its artifact reference is re-attached so the locator for the
/// complete record survives this second projection. The positional trimmer passes no replacement
/// reference because it has no run identity; the reference-aware caller supplies one from its run
/// and call coordinates when this stage creates the first bounded projection.
fn trimmed_structured_output(
    output: &ToolOutput,
    summary: String,
    artifact_ref: Option<&ra_core::tool::ArtifactRef>,
) -> Result<ToolOutput> {
    let trimmed = ToolOutput::text(summary.clone()).with_metadata(output.metadata().clone());
    match output
        .model_excerpt()
        .map(ModelExcerpt::artifact_ref)
        .or(artifact_ref)
    {
        Some(artifact_ref) => Ok(trimmed.with_model_excerpt(ModelExcerpt::new(
            vec![ToolOutputBlock::text(summary)],
            artifact_ref.clone(),
        )?)),
        None => Ok(trimmed),
    }
}

/// Space left for a structured summary after every retained model-facing field is rendered.
///
/// The empty text block stands in for the future summary. Joining the rendered text blocks puts
/// exactly the same separators around that block as it will around the non-empty replacement.
fn structured_summary_budget(
    output: &ToolOutput,
    artifact_ref: Option<&ra_core::tool::ArtifactRef>,
    max_output_chars: usize,
) -> Result<Option<usize>> {
    let fixed = trimmed_structured_output(output, String::new(), artifact_ref)?;
    let fixed_chars = source_text(&fixed.model_blocks()).chars().count();
    let marker_chars = MINIMAL_SUMMARY.chars().count();
    Ok(
        (fixed_chars.saturating_add(marker_chars) <= max_output_chars)
            .then_some(max_output_chars.saturating_sub(fixed_chars)),
    )
}

fn tool_names_by_call_id(input: &[ModelInputItem]) -> BTreeMap<&CallId, &str> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCall(call) => Some((call.call_id(), call.name())),
            _ => None,
        })
        .collect()
}

fn legacy_output_text(output: &Value) -> Result<String> {
    match output.as_str() {
        Some(text) => Ok(text.to_owned()),
        None => serde_json::to_string(output).map_err(|error| {
            Error::caller(format!(
                "tool output could not be serialized for trimming: {error}"
            ))
        }),
    }
}

fn take_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}
