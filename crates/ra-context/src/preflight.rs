//! Oversized-input preprocessing for newly supplied user text.
//!
//! A compaction policy reduces retained history, but it cannot make one newly supplied user
//! message smaller without changing what the caller asked the model to consider. This module
//! detects that case before the primary request is assembled, preserves the message's beginning
//! and end verbatim, and replaces its middle with a map-reduce summary.
//!
//! `ra-context` does not make model requests. [`InputSummarizer`] is the host-owned port that
//! performs the map and reduce calls; the host can select a suitable model, account for its usage,
//! and apply its own retry policy. This is a material difference from a direct Codex UI limit: the
//! core stays provider-neutral while still ensuring an oversized input is preprocessed rather than
//! sent unchanged.

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt, stream};
use ra_core::{
    error::{Error, Result},
    item::{ContentBlock, Message, MessageRole, ModelInputItem},
    prompt::{CHARS_PER_TOKEN, estimate_tokens},
};

use crate::{
    compaction::{CompactionLimits, ContextUsage, summary::code_fence},
    window::ContextWindowConfig,
};

const PREAMBLE: &str =
    "The middle of this oversized user input was summarized before it was sent to the model.";
const BEGINNING_HEADING: &str = "--- original beginning ---";
const SUMMARY_HEADING: &str = "--- summary of omitted middle ---";
const END_HEADING: &str = "--- original end ---";

/// Smallest summary a configuration has to leave room for.
///
/// This is a feasibility floor, not a quality one: how long a useful summary of a given middle
/// section is depends on that section, and only the host's summarizer decides it. What the floor
/// does rule out is a configuration whose fixed structure and retained edges already fill the
/// allocation, which could never produce a fitting result no matter how terse the summary was.
const MIN_SUMMARY_TOKENS: usize = 1;

/// Map calls issued at once when the host does not choose a different width.
///
/// One oversized paste can split into dozens of chunks, and running them strictly one after
/// another makes preprocessing cost the sum of every round trip. The default overlaps a handful
/// while staying far below the point where a single paste becomes a burst against a provider rate
/// limit. It is deliberately narrower than the runtime's function-tool batch width, because each
/// unit of work here is a model round trip rather than a local call.
pub const DEFAULT_MAP_CONCURRENCY: usize = 4;

/// Validated limits for preprocessing one oversized user message.
///
/// `max_input_tokens` is the allocation the final, model-facing input must fit. The host derives
/// it after reserving space for its instructions, tools, retained history, and model output.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPreflightConfig {
    max_input_tokens: usize,
    max_chunk_tokens: usize,
    retained_edge_tokens: usize,
    map_concurrency: usize,
}

impl InputPreflightConfig {
    /// Creates limits for the final input, each map chunk, and each preserved source edge.
    ///
    /// The allocation is checked against what this policy actually emits, not merely against the
    /// retained edges: the section headings, the explanatory preamble, the fences around the
    /// verbatim edges, and one token of summary all have to fit as well. Rejecting that here is
    /// what keeps [`InputPreflight::preprocess`] from spending a full map-reduce pass before
    /// discovering that no output could have satisfied the limit.
    pub fn new(
        max_input_tokens: usize,
        max_chunk_tokens: usize,
        retained_edge_tokens: usize,
    ) -> Result<Self> {
        if max_input_tokens == 0 {
            return Err(Error::config(
                "an input-preflight token limit must be at least one",
            ));
        }
        if max_chunk_tokens == 0 {
            return Err(Error::config(
                "an input-preflight chunk token limit must be at least one",
            ));
        }
        if max_chunk_tokens > max_input_tokens {
            return Err(Error::config(format!(
                "an input-preflight chunk limit of {max_chunk_tokens} tokens exceeds the \
                 {max_input_tokens}-token final-input limit"
            )));
        }
        if retained_edge_tokens == 0 {
            return Err(Error::config(
                "an input-preflight retained-edge token limit must be at least one",
            ));
        }
        let structure = structural_overhead_tokens()?;
        let required = structure
            .saturating_add(retained_edge_tokens.saturating_mul(2))
            .saturating_add(MIN_SUMMARY_TOKENS);
        if required > max_input_tokens {
            return Err(Error::config(format!(
                "an input-preflight limit of {max_input_tokens} tokens cannot hold this policy's \
                 own output: {structure} tokens of fixed structure, two \
                 {retained_edge_tokens}-token retained edges and at least {MIN_SUMMARY_TOKENS} \
                 token of summary need {required} tokens"
            )));
        }
        Ok(Self {
            max_input_tokens,
            max_chunk_tokens,
            retained_edge_tokens,
            map_concurrency: DEFAULT_MAP_CONCURRENCY,
        })
    }

    /// Sets how many map calls may be in flight at once.
    pub fn with_map_concurrency(mut self, map_concurrency: usize) -> Result<Self> {
        if map_concurrency == 0 {
            return Err(Error::config(
                "an input-preflight map concurrency must be at least one",
            ));
        }
        self.map_concurrency = map_concurrency;
        Ok(self)
    }

    /// Derives the final-input allocation from a known model's compaction threshold.
    ///
    /// This shares the existing default of 60% of the model context window, which is the point the
    /// plan sets for preprocessing one oversized user message. Unknown models return `None`:
    /// capacity must come from a host override rather than a guess.
    ///
    /// **This ceiling is the whole compaction threshold, so it reserves nothing for retained
    /// history.** It answers "is this one message alone too large to send", which is the question
    /// a host without a compaction policy has. A host that runs one should call
    /// [`Self::for_compaction`] instead: an input admitted here can still be larger than that
    /// policy's single-item trigger, and a trigger fired by the newest item is one compaction
    /// cannot clear, because clearing it would mean discarding the message the caller just sent.
    pub fn for_model(
        context_windows: &ContextWindowConfig,
        model: &str,
        max_chunk_tokens: usize,
        retained_edge_tokens: usize,
    ) -> Result<Option<Self>> {
        let Some(max_input_tokens) = context_windows.compaction_threshold(model) else {
            return Ok(None);
        };
        // Reported against the ratio and the model that produced it, matching
        // `CompactionLimits::for_model`. Falling through to `Self::new`'s generic "must be at least
        // one" would name neither, and the host would be looking for a zero it never wrote.
        if max_input_tokens == 0 {
            return Err(Error::config(format!(
                "an input-preflight threshold of {} rounds model `{model}`'s context window down \
                 to zero tokens, which would leave no room for any input at all",
                context_windows.compaction_threshold_ratio()
            )));
        }
        let max_input_tokens = usize::try_from(max_input_tokens).map_err(|_| {
            Error::config(format!(
                "the input-preflight threshold for model `{model}` does not fit this platform's \
                 usize"
            ))
        })?;
        Self::new(max_input_tokens, max_chunk_tokens, retained_edge_tokens).map(Some)
    }

    /// Derives the final-input allocation from a compaction policy's single-item ceiling.
    ///
    /// [`CompactionLimits`] refuses a single-item ceiling at or above its total, so an input
    /// admitted below that ceiling cannot be the item that trips
    /// [`crate::compaction::CompactionReason::SingleItemTokens`] on arrival, and the room between
    /// the two limits stays available for retained history.
    ///
    /// A policy with no single-item ceiling returns `None`. Its total-token trigger describes the
    /// whole history and says nothing about how much of the window one new message may occupy;
    /// inventing a reserve would be the kind of guess this crate refuses elsewhere. Such a host
    /// configures a single-item ceiling, or builds the allocation itself with [`Self::new`].
    pub fn for_compaction(
        limits: &CompactionLimits,
        max_chunk_tokens: usize,
        retained_edge_tokens: usize,
    ) -> Result<Option<Self>> {
        let Some(max_input_tokens) = limits.max_single_item_tokens() else {
            return Ok(None);
        };
        // [`CompactionLimits`] refuses a zero ceiling, so the reservation cannot underflow; what it
        // can produce is an empty allocation, and only from a one-token ceiling. Naming that
        // ceiling here is the point of the branch — `Self::new`'s generic "must be at least one"
        // would send the host looking for a zero it never wrote.
        let Some(max_input_tokens) = max_input_tokens.checked_sub(1).filter(|tokens| *tokens > 0)
        else {
            return Err(Error::config(format!(
                "a compaction single-item ceiling of {max_input_tokens} leaves an input preflight \
                 no room: it has to admit strictly less than that ceiling"
            )));
        };
        Self::new(max_input_tokens, max_chunk_tokens, retained_edge_tokens).map(Some)
    }

    /// Maximum estimated token cost of the model-facing preprocessed input.
    #[must_use]
    pub const fn max_input_tokens(self) -> usize {
        self.max_input_tokens
    }

    /// Maximum estimated token cost of one source chunk passed to the map stage.
    #[must_use]
    pub const fn max_chunk_tokens(self) -> usize {
        self.max_chunk_tokens
    }

    /// Estimated tokens preserved verbatim from both ends of the original user text.
    #[must_use]
    pub const fn retained_edge_tokens(self) -> usize {
        self.retained_edge_tokens
    }

    /// Number of map calls this policy allows to be in flight at once.
    #[must_use]
    pub const fn map_concurrency(self) -> usize {
        self.map_concurrency
    }
}

/// Host port for the model calls used by oversized-input preprocessing.
///
/// The map stage receives source chunks in their original order and may run several of them at
/// once; the reduce stage receives one summary for every chunk in that same order and must return
/// a concise account of their combined content. Implementations should use a bounded output
/// setting for both calls.
#[async_trait]
pub trait InputSummarizer: Send + Sync + 'static {
    /// Summarizes one chunk of the original middle section.
    async fn summarize_chunk(&self, chunk: &str) -> Result<String>;

    /// Reduces ordered map summaries into one summary of the complete omitted middle section.
    async fn reduce_summaries(&self, summaries: &[String]) -> Result<String>;
}

/// Preprocesses oversized single-message user input before the primary request is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPreflight {
    config: InputPreflightConfig,
}

impl InputPreflight {
    /// Creates a preprocessor using the supplied host-owned limits.
    #[must_use]
    pub const fn new(config: InputPreflightConfig) -> Self {
        Self { config }
    }

    /// Limits used by this preprocessor.
    #[must_use]
    pub const fn config(self) -> InputPreflightConfig {
        self.config
    }

    /// Measures an input against the final model-input allocation.
    pub fn assess(&self, input: &[ModelInputItem]) -> Result<InputPreflightAssessment> {
        Ok(InputPreflightAssessment {
            usage: ContextUsage::estimate_model_input(input)?,
            max_input_tokens: self.config.max_input_tokens,
        })
    }

    /// Returns the original input when it fits, otherwise map-reduces its oversized user text.
    ///
    /// Only one user message containing exactly one text block is transformable. The narrower
    /// input shape is deliberate: silently dropping, reordering, or converting attachments would
    /// violate the caller's request. A host with multipart or multimodal input should project that
    /// content into a dedicated text representation before invoking this policy.
    pub async fn preprocess(
        &self,
        input: &[ModelInputItem],
        summarizer: &dyn InputSummarizer,
    ) -> Result<PreprocessedInput> {
        let original = self.assess(input)?;
        if original.is_accepted() {
            return Ok(PreprocessedInput {
                input: input.to_vec(),
                original_usage: original.usage,
                usage: original.usage,
                chunks_summarized: 0,
            });
        }

        let text = oversized_user_text(input)?;
        let (beginning, middle, end) = split_edges(text, self.config.retained_edge_tokens);
        if middle.is_empty() {
            // Unreachable through `InputPreflightConfig::new`, which reserves room for the fixed
            // structure on top of both edges, so an input large enough to reach this point is
            // longer than the edges can span. Reported rather than summarized because there is
            // nothing here to summarize, and paying for a map-reduce pass to learn that is worse
            // than saying so.
            return Err(Error::caller(format!(
                "oversized-input preprocessing has nothing to summarize: two \
                 {}-token retained edges already span this {}-token input",
                self.config.retained_edge_tokens,
                original.input_tokens(),
            )));
        }
        let chunks = split_chunks(middle, self.config.max_chunk_tokens);
        let summaries = self.map_chunks(&chunks, summarizer).await?;
        self.ensure_reduce_input_fits(&summaries)?;
        let summary = summarizer.reduce_summaries(&summaries).await?;
        require_summary("reduce", &summary)?;

        let model_input = ModelInputItem::Message(Message::user(render_preprocessed_input(
            beginning, &summary, end,
        )));
        let input = vec![model_input];
        let usage = ContextUsage::estimate_model_input(&input)?;
        if usage.total_tokens() > self.config.max_input_tokens {
            return Err(Error::caller(format!(
                "oversized-input preprocessing produced {} estimated tokens, above the configured \
                 {}-token final-input limit; the {}-token summary returned by the host summarizer \
                 has to be shorter, or the retained edges smaller",
                usage.total_tokens(),
                self.config.max_input_tokens,
                estimate_tokens(&summary),
            )));
        }

        Ok(PreprocessedInput {
            input,
            original_usage: original.usage,
            usage,
            chunks_summarized: chunks.len(),
        })
    }

    /// Summarizes every chunk, keeping source order while overlapping the configured number.
    async fn map_chunks(
        &self,
        chunks: &[&str],
        summarizer: &dyn InputSummarizer,
    ) -> Result<Vec<String>> {
        let summaries: Vec<String> = stream::iter(chunks)
            .map(|chunk| summarizer.summarize_chunk(chunk))
            .buffered(self.config.map_concurrency)
            .try_collect()
            .await?;
        for summary in &summaries {
            require_summary("map", summary)?;
        }
        Ok(summaries)
    }

    /// Refuses map output that would make the reduce request oversized in its own right.
    fn ensure_reduce_input_fits(&self, summaries: &[String]) -> Result<()> {
        let tokens = summary_input_tokens(summaries)?;
        if tokens > self.config.max_input_tokens {
            return Err(Error::caller(format!(
                "oversized-input map summaries total {tokens} estimated tokens, above the \
                 configured {}-token reduce-input limit; refuse the reduce request and require \
                 shorter map summaries",
                self.config.max_input_tokens,
            )));
        }
        Ok(())
    }
}

/// The provider-neutral size assessment of one newly supplied input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPreflightAssessment {
    usage: ContextUsage,
    max_input_tokens: usize,
}

impl InputPreflightAssessment {
    /// Complete provider-neutral measurement of the supplied input.
    #[must_use]
    pub const fn usage(self) -> ContextUsage {
        self.usage
    }

    /// Aggregate estimated input cost in tokens.
    #[must_use]
    pub const fn input_tokens(self) -> usize {
        self.usage.total_tokens()
    }

    /// The final model-input allocation used for this assessment.
    #[must_use]
    pub const fn max_input_tokens(self) -> usize {
        self.max_input_tokens
    }

    /// Whether the complete newly supplied input fits without preprocessing.
    #[must_use]
    pub const fn is_accepted(self) -> bool {
        self.input_tokens() <= self.max_input_tokens
    }
}

/// The input that the host should send after the preflight stage.
#[derive(Debug, Clone, PartialEq)]
pub struct PreprocessedInput {
    input: Vec<ModelInputItem>,
    original_usage: ContextUsage,
    usage: ContextUsage,
    chunks_summarized: usize,
}

impl PreprocessedInput {
    /// Input to send to the primary model request.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Consumes the result and returns its model-facing input.
    #[must_use]
    pub fn into_input(self) -> Vec<ModelInputItem> {
        self.input
    }

    /// Measurement before preprocessing.
    #[must_use]
    pub const fn original_usage(&self) -> ContextUsage {
        self.original_usage
    }

    /// Measurement of the input that will be sent to the primary model.
    #[must_use]
    pub const fn usage(&self) -> ContextUsage {
        self.usage
    }

    /// Number of map chunks summarized before the reduce stage.
    #[must_use]
    pub const fn chunks_summarized(&self) -> usize {
        self.chunks_summarized
    }

    /// Whether this result replaced an oversized original input.
    ///
    /// A replacement always summarizes at least one chunk: preprocessing rejects an input whose
    /// middle section is empty rather than rendering one with nothing between its edges.
    #[must_use]
    pub const fn was_preprocessed(&self) -> bool {
        self.chunks_summarized > 0
    }
}

/// Prices the sections this policy always emits, in the form the model finally receives them.
///
/// Rendering placeholders through the real template is what keeps this honest: a hand-counted
/// constant would stop matching the moment a heading changed, and it would be a construction-time
/// check for a shape the request path no longer produces.
fn structural_overhead_tokens() -> Result<usize> {
    let placeholder = [ModelInputItem::Message(Message::user(
        render_preprocessed_input("", "", ""),
    ))];
    Ok(ContextUsage::estimate_model_input(&placeholder)?.total_tokens())
}

fn oversized_user_text(input: &[ModelInputItem]) -> Result<&str> {
    let [ModelInputItem::Message(message)] = input else {
        return Err(Error::caller(
            "oversized-input preprocessing requires exactly one user text message",
        ));
    };
    if message.role() != MessageRole::User {
        return Err(Error::caller(
            "oversized-input preprocessing requires a user message",
        ));
    }
    let [ContentBlock::Text(text)] = message.content() else {
        return Err(Error::caller(
            "oversized-input preprocessing requires a user message with exactly one text block",
        ));
    };
    Ok(text.text())
}

/// Splits the source into the two verbatim edges and the middle section between them.
///
/// The two boundaries are computed from opposite ends, so a source shorter than both edges
/// together would cross them. `InputPreflightConfig::new` makes that unreachable for an input
/// large enough to be preprocessed, and the clamp keeps the arithmetic total rather than leaving a
/// panic as the only thing standing behind that reasoning.
fn split_edges(text: &str, retained_edge_tokens: usize) -> (&str, &str, &str) {
    let edge_chars = retained_edge_tokens.saturating_mul(CHARS_PER_TOKEN);
    let beginning_end = byte_index_after_chars(text, edge_chars);
    let end_start = byte_index_before_chars(text, edge_chars).max(beginning_end);
    (
        &text[..beginning_end],
        &text[beginning_end..end_start],
        &text[end_start..],
    )
}

fn split_chunks(text: &str, max_chunk_tokens: usize) -> Vec<&str> {
    let max_chars = max_chunk_tokens.saturating_mul(CHARS_PER_TOKEN).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (index, _) in text.char_indices() {
        if chars == max_chars {
            chunks.push(&text[start..index]);
            start = index;
            chars = 0;
        }
        chars = chars.saturating_add(1);
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

fn byte_index_after_chars(text: &str, count: usize) -> usize {
    text.char_indices()
        .nth(count)
        .map_or(text.len(), |(index, _)| index)
}

fn byte_index_before_chars(text: &str, count: usize) -> usize {
    text.char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map_or(0, |(index, _)| index)
}

fn require_summary(stage: &str, summary: &str) -> Result<()> {
    if summary.trim().is_empty() {
        return Err(Error::caller(format!(
            "oversized-input {stage} summary must not be blank"
        )));
    }
    Ok(())
}

/// Estimates the text passed to the reduce stage without building a second joined copy of it.
///
/// The reducer receives the summaries in source order with one newline between neighbours. This
/// counts exactly that content basis, matching [`ContextUsage::estimate_model_input`] while
/// avoiding an additional allocation proportional to already oversized summaries.
fn summary_input_tokens(summaries: &[String]) -> Result<usize> {
    let chars = summaries
        .iter()
        .enumerate()
        .try_fold(0_usize, |total, (index, summary)| {
            total
                .checked_add(usize::from(index > 0))
                .and_then(|total| total.checked_add(summary.chars().count()))
        })
        .ok_or_else(|| Error::caller("the map-summary character total exceeds usize"))?;
    Ok(chars.div_ceil(CHARS_PER_TOKEN))
}

/// Renders the preserved edges and the summary into one replacement user message.
///
/// The retained edges are enclosed in individually sized code fences, as
/// [`crate::compaction::summary`] does for preserved user messages and for the same reason: the
/// edges are the caller's own text, and text that can close its own section could present part of
/// the original input as the summary of what was dropped.
fn render_preprocessed_input(beginning: &str, summary: &str, end: &str) -> String {
    let beginning_fence = code_fence(beginning);
    let end_fence = code_fence(end);
    format!(
        "{PREAMBLE}\n\n\
         {BEGINNING_HEADING}\n{beginning_fence}\n{beginning}\n{beginning_fence}\n\n\
         {SUMMARY_HEADING}\n{summary}\n\n\
         {END_HEADING}\n{end_fence}\n{end}\n{end_fence}"
    )
}
