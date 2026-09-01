//! Byte and token ceilings for tool results.
//!
//! The complete [`ToolOutput`](ra_core::tool::ToolOutput) remains in the session record.  This
//! module only adds a bounded [`ModelExcerpt`](ra_core::tool::ModelExcerpt) when a result would
//! exceed the configured model-input allowance.  The runtime reaches it through the core
//! [`ToolOutputProjector`](ra_core::tool::ToolOutputProjector) port, so `ra-runtime` never learns
//! about a concrete context policy.
//!
//! # One measure, used twice
//!
//! The ceiling that admits a result and the ceiling that sizes its excerpt are the **same
//! measurement**, taken by [`measure`] over the final model-visible blocks. An earlier shape gated
//! on the serialized JSON of the block list and then trimmed the raw text against those numbers;
//! the two differ by the `{"type":"text","text":…}` framing and one escape per control character,
//! so the excerpt it produced could still exceed the ceiling that had just rejected the result. A
//! projector whose output fails its own admission check has no invariant left to test.
//!
//! Text is measured by its own bytes and by the shared prompt estimator; an opaque image or file is
//! measured by its serialized payload, because that is what it costs a provider and there is no
//! character count that means anything for it.

use ra_core::{
    error::{Error, Result},
    item::CallId,
    prompt::estimate_tokens,
    state::RunId,
    tool::{
        ArtifactRef, ModelExcerpt, ObservationMetadata, ToolOutput, ToolOutputBlock,
        ToolOutputProjection, ToolOutputProjector, Truncation, TruncationStage,
    },
};

/// Default maximum model-visible payload size admitted to one model excerpt.
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
/// Default maximum estimated token count admitted to one model excerpt.
pub const DEFAULT_MAX_TOOL_RESULT_TOKENS: usize = 8 * 1024;

/// Marker standing in for the omitted middle of a text body.
const MARKER: &str = "\n\n[... middle omitted by context budget ...]\n\n";

/// How many characters the shared estimator charges to one token.
///
/// Inverting [`estimate_tokens`] is what turns a token allowance into a character allowance, and
/// the two have to agree: a trimmer that assumed a different ratio would hand back an excerpt the
/// estimator then priced above the ceiling it was trimmed to fit.
const CHARS_PER_TOKEN: usize = 4;

/// The share of the byte ceiling opaque blocks may claim before the text body starts losing room.
///
/// An image is often the answer, so the excerpt keeps one that fits — but a single screenshot must
/// not be able to spend the whole allowance and leave the text with nothing, because the text is
/// what a model can act on when the image is gone.
const OPAQUE_BYTE_SHARE: usize = 2;

/// A temporary excerpt contains one empty text block. The real projection can contain a body and
/// an omission notice instead, which adds at most one more separator when text blocks are joined
/// for the shared measurement.
const EXTRA_EXCERPT_TEXT_SEPARATORS: usize = 1;

/// A dual byte-and-token ceiling for one completed tool result.
///
/// Both limits are required because they bind in opposite regimes.  With the shipped defaults the
/// token limit is the one ASCII trips: `estimate_tokens` charges one token per four characters, so
/// 8,192 tokens is reached at 32,769 ASCII bytes — half the byte ceiling.  The byte ceiling is
/// there for what a character count does not price: multi-byte scripts, where 64 KiB of CJK text is
/// only about 5,500 estimated tokens, and base64 image or file payloads, which cost a provider
/// bytes and have no characters at all.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultBudget {
    max_bytes: usize,
    max_tokens: usize,
}

impl Default for ToolResultBudget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
            max_tokens: DEFAULT_MAX_TOOL_RESULT_TOKENS,
        }
    }
}

impl ToolResultBudget {
    /// Creates a budget whose ceilings can carry a projection at all.
    ///
    /// **A floor rather than a non-zero check.** A projection is not only the excerpt body: it is
    /// also the rendered truncation line, the guidance sentence explaining the cut, the artifact
    /// note, and the omission marker, every one of which [`ToolOutput::model_blocks`] puts in front
    /// of a provider.  A ceiling below those fixed costs cannot produce a conforming excerpt for
    /// *any* result, so it is refused once here rather than failing on every call.
    ///
    /// **The floor is not the whole guarantee, and deliberately cannot be.** Two costs are unknown
    /// at configuration time: how long the run and call identifiers in an artifact reference turn
    /// out to be, and what metadata the tool itself attached to the result.  Pricing them requires
    /// the result, so [`Self::project_output`] measures the finished projection against the real
    /// ceiling and refuses it when they do not fit.
    pub fn new(max_bytes: usize, max_tokens: usize) -> Result<Self> {
        let floor = floor()?;
        if max_bytes < floor.bytes || max_tokens < floor.tokens {
            return Err(Error::caller(format!(
                "a tool-result budget must admit a projection's own framing and a marked \
                 head-and-tail excerpt: at least {} bytes and {} estimated tokens, not {max_bytes} \
                 and {max_tokens}",
                floor.bytes, floor.tokens
            )));
        }
        Ok(Self {
            max_bytes,
            max_tokens,
        })
    }

    /// Maximum model-visible payload bytes before this projector produces an excerpt.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Maximum estimated prompt tokens before this projector produces an excerpt.
    #[must_use]
    pub const fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Projects one result, preserving its head and tail whenever a text body is available.
    ///
    /// A result that fits is returned untouched, and the [`Truncation`] this records can never
    /// describe a cut that did not happen — telling a model it is missing output it actually
    /// received is the one report worse than silence.  That falls out of measuring the admission
    /// check over [`ToolOutput::model_blocks`]: the excerpt's allowance is the same ceiling less an
    /// overhead strictly larger than the metadata note the check already counted, so a body that
    /// survives whole is a body whose result could not have exceeded the ceiling to begin with.
    pub fn project_output(
        &self,
        run_id: &RunId,
        call_id: &CallId,
        output: ToolOutput,
    ) -> Result<ToolOutput> {
        let projection = self.project_projection(run_id, call_id, &output)?;
        Ok(output.with_model_projection(projection))
    }

    /// Produces the additive projection the runtime can safely apply to one complete result.
    fn project_projection(
        &self,
        run_id: &RunId,
        call_id: &CallId,
        output: &ToolOutput,
    ) -> Result<ToolOutputProjection> {
        let model_source = measure(&output.model_blocks())?;
        if model_source.bytes <= self.max_bytes && model_source.tokens <= self.max_tokens {
            return Ok(ToolOutputProjection::new());
        }

        // Measured after the early return, not before it: the overwhelming majority of results fit,
        // and that path must not pay for a text join and a serialization of every image it discards.
        let complete = measure(output.blocks())?;
        let causes = exceeded_causes(
            model_source.bytes > self.max_bytes,
            model_source.tokens > self.max_tokens,
        );
        let artifact_ref = artifact_ref(run_id, call_id)?;
        let guidance = guidance_sentence(causes, model_source.bytes, model_source.tokens);

        // `ToolOutput::model_blocks` renders metadata ahead of the excerpt and appends the
        // artifact note after it. Price those blocks before allocating output content.
        let overhead = overhead_of(output.metadata(), &artifact_ref, &guidance)?;

        // The excerpt is built from the measurement rather than re-deriving it: a second pass would
        // serialize every image again, and the two passes could only ever agree by coincidence.
        let excerpt = self.build_excerpt(output.blocks(), &complete, overhead);
        let projection = ToolOutputProjection::new()
            .with_truncation(Truncation::new(
                TruncationStage::ContextBudget,
                saturating_u64(complete.bytes),
                saturating_u64(excerpt.retained_bytes),
            ))
            .with_guidance(guidance)
            .with_model_excerpt(ModelExcerpt::new(excerpt.blocks, artifact_ref)?);
        self.ensure_fits(output.metadata(), &projection)?;
        Ok(projection)
    }

    /// Refuses a projection that would violate the ceiling after every framework-added block.
    ///
    /// **The metadata note is priced, never trimmed.** Its truncations and guidance are facts a
    /// tool and the framework recorded about the observation, and dropping half a sentence of an
    /// explanation is worse than not offering one; so a result whose metadata alone outgrows the
    /// ceiling is a budget that cannot serve this host, reported here rather than silently shipped.
    fn ensure_fits(
        &self,
        metadata: &ObservationMetadata,
        projection: &ToolOutputProjection,
    ) -> Result<()> {
        // A projection always supplies an excerpt, so this avoids cloning the complete result just
        // to measure the final model-visible wrapper.
        let preview = ToolOutput::text("")
            .with_metadata(metadata.clone())
            .with_model_projection(projection.clone());
        let cost = measure(&preview.model_blocks())?;
        if cost.bytes <= self.max_bytes && cost.tokens <= self.max_tokens {
            return Ok(());
        }
        Err(Error::caller(format!(
            "a tool-result budget of {} bytes and {} estimated tokens cannot carry this result's model-visible metadata and artifact reference (projected {} bytes and {} estimated tokens)",
            self.max_bytes, self.max_tokens, cost.bytes, cost.tokens
        )))
    }

    /// Builds the bounded block list, keeping the opaque blocks the byte ceiling can still afford.
    fn build_excerpt(
        &self,
        blocks: &[ToolOutputBlock],
        source: &Cost,
        overhead: Overhead,
    ) -> Excerpt {
        let opaque = Self::admit_opaque(
            blocks,
            &source.opaque_bytes,
            self.max_bytes
                .saturating_sub(overhead.bytes)
                .min(self.max_bytes / OPAQUE_BYTE_SHARE),
        );
        let notice = omission_notice(opaque.dropped);

        // Everything except the text body has a size known before the body is trimmed, including
        // the metadata and artifact note that `model_blocks` adds around this excerpt.
        let byte_allowance = self
            .max_bytes
            .saturating_sub(overhead.bytes)
            .saturating_sub(opaque.kept_bytes)
            .saturating_sub(notice.len());
        let char_allowance = self
            .max_tokens
            .saturating_sub(overhead.tokens)
            .saturating_mul(CHARS_PER_TOKEN)
            .saturating_sub(notice.chars().count());

        let mut excerpt_blocks = Vec::with_capacity(opaque.kept.len().saturating_add(2));
        let mut retained_bytes = opaque.kept_bytes;
        if !source.text.is_empty() {
            let body = head_tail(&source.text, byte_allowance, char_allowance);
            retained_bytes = retained_bytes.saturating_add(body.retained_bytes);
            excerpt_blocks.push(ToolOutputBlock::text(body.text));
        } else if opaque.kept.is_empty() && notice.is_empty() {
            // A model excerpt is itself a call answer and therefore cannot be empty. This only
            // occurs when oversized metadata, rather than a tool body, triggered projection; the
            // final admission check below reports the unsatisfiable budget if the empty answer and
            // framework prose cannot fit.
            excerpt_blocks.push(ToolOutputBlock::text(""));
        }
        excerpt_blocks.extend(opaque.kept);
        if !notice.is_empty() {
            excerpt_blocks.push(ToolOutputBlock::text(notice));
        }

        Excerpt {
            blocks: excerpt_blocks,
            retained_bytes,
        }
    }

    /// Splits the opaque blocks into the ones the excerpt keeps and a count of the ones it drops.
    fn admit_opaque(
        blocks: &[ToolOutputBlock],
        sizes: &[usize],
        allowance: usize,
    ) -> AdmittedOpaque {
        let mut admitted = AdmittedOpaque::default();
        let opaque = blocks.iter().filter(|block| block.as_text().is_none());
        for (block, bytes) in opaque.zip(sizes.iter().copied()) {
            // Order matters, not size: keeping a later small block after skipping an earlier large
            // one would hand the model a subset it cannot tell apart from the whole sequence.
            if admitted.dropped == 0 && admitted.kept_bytes.saturating_add(bytes) <= allowance {
                admitted.kept_bytes = admitted.kept_bytes.saturating_add(bytes);
                admitted.kept.push(block.clone());
            } else {
                admitted.dropped = admitted.dropped.saturating_add(1);
            }
        }
        admitted
    }
}

impl ToolOutputProjector for ToolResultBudget {
    fn project(
        &self,
        run_id: &RunId,
        call_id: &CallId,
        output: &ToolOutput,
    ) -> Result<ToolOutputProjection> {
        self.project_projection(run_id, call_id, output)
    }
}

/// The bounded block list and how much of the source survived into it.
struct Excerpt {
    blocks: Vec<ToolOutputBlock>,
    /// Source bytes preserved, counting neither the marker nor the omission notice — the framework's
    /// own words are not output the tool produced.
    retained_bytes: usize,
}

/// The known model-visible cost surrounding an excerpt body.
#[derive(Debug, Clone, Copy)]
struct Overhead {
    bytes: usize,
    tokens: usize,
}

/// Which opaque blocks an excerpt can afford.
#[derive(Default)]
struct AdmittedOpaque {
    kept: Vec<ToolOutputBlock>,
    kept_bytes: usize,
    dropped: usize,
}

/// What one block list costs against a budget, and the pieces the excerpt is built from.
struct Cost {
    /// Model-visible payload bytes across every block.
    bytes: usize,
    /// Estimated tokens of the textual part; an opaque payload costs bytes, not characters.
    tokens: usize,
    /// The model-visible text, joined once and reused by whoever trims it.
    text: String,
    /// Serialized size of each block carrying no text, in the order those blocks appear.
    opaque_bytes: Vec<usize>,
}

/// Measures the model-visible cost of a block list.
fn measure(blocks: &[ToolOutputBlock]) -> Result<Cost> {
    let text = source_text(blocks);
    let mut bytes = text.len();
    let mut opaque_bytes = Vec::new();
    for block in blocks.iter().filter(|block| block.as_text().is_none()) {
        let block_bytes = opaque_block_bytes(block)?;
        bytes = bytes.saturating_add(block_bytes);
        opaque_bytes.push(block_bytes);
    }
    Ok(Cost {
        bytes,
        tokens: estimate_tokens(&text),
        text,
        opaque_bytes,
    })
}

/// The model-visible text of a block list, in the order the tool produced it.
fn source_text(blocks: &[ToolOutputBlock]) -> String {
    blocks
        .iter()
        .filter_map(ToolOutputBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serialized payload size of a block that carries no text.
///
/// Serialization rather than a match on the variants: [`ToolOutputBlock`] is `#[non_exhaustive]`, so
/// a match here would need a wildcard that silently prices a future block kind at zero.
fn opaque_block_bytes(block: &ToolOutputBlock) -> Result<usize> {
    serde_json::to_string(block)
        .map(|serialized| serialized.len())
        .map_err(|error| {
            Error::caller("a non-text tool-output block could not be measured for budgeting")
                .with_source(error)
        })
}

/// Names the opaque blocks an excerpt left out, without pretending they were text.
fn omission_notice(dropped: usize) -> String {
    if dropped == 0 {
        return String::new();
    }
    format!("[{dropped} non-text tool-output block(s) omitted from this excerpt.]")
}

/// A trimmed body and how many source bytes it preserved.
struct Body {
    text: String,
    retained_bytes: usize,
}

/// Preserves a bounded prefix and suffix, separated by an explicit omission marker.
fn head_tail(text: &str, max_bytes: usize, max_chars: usize) -> Body {
    if text.len() <= max_bytes && text.chars().count() <= max_chars {
        return Body {
            retained_bytes: text.len(),
            text: text.to_owned(),
        };
    }

    // The marker earns its bytes only while it leaves content on both sides of it. Spending an
    // allowance the marker alone would fill produces a body that is nothing but the framework's own
    // ellipsis — and one that overruns the ceiling this function exists to respect, while saying
    // less than the truncation the metadata already records.
    let marker = if max_bytes > MARKER.len() && max_chars > MARKER.chars().count() {
        MARKER
    } else {
        ""
    };
    let content_bytes = max_bytes.saturating_sub(marker.len());
    let content_chars = max_chars.saturating_sub(marker.chars().count());
    let head = prefix_within(text, content_bytes / 2, content_chars / 2);
    let tail = suffix_within(
        &text[head.len()..],
        content_bytes.saturating_sub(head.len()),
        content_chars.saturating_sub(head.chars().count()),
    );
    Body {
        text: format!("{head}{marker}{tail}"),
        retained_bytes: head.len().saturating_add(tail.len()),
    }
}

/// Returns the longest valid UTF-8 prefix satisfying both source-content allowances.
fn prefix_within(text: &str, max_bytes: usize, max_chars: usize) -> &str {
    let mut end = 0_usize;
    let mut chars = 0_usize;
    for (index, character) in text.char_indices() {
        let next = index.saturating_add(character.len_utf8());
        if next > max_bytes || chars == max_chars {
            break;
        }
        end = next;
        chars = chars.saturating_add(1);
    }
    &text[..end]
}

/// Returns the longest valid UTF-8 suffix satisfying both source-content allowances.
fn suffix_within(text: &str, max_bytes: usize, max_chars: usize) -> &str {
    let mut start = text.len();
    let mut chars = 0_usize;
    for (index, _) in text.char_indices().rev() {
        if text.len().saturating_sub(index) > max_bytes || chars == max_chars {
            break;
        }
        start = index;
        chars = chars.saturating_add(1);
    }
    &text[start..]
}

/// Prices every block the model receives around an excerpt body.
///
/// The truncation is priced at `u64::MAX` on both counts because the real figures are not known
/// until the excerpt has been built, and no smaller pair of numbers renders wider.  The placeholder
/// excerpt is what makes the body itself cost nothing here: [`ToolOutput::model_blocks`] reads the
/// excerpt rather than the complete blocks whenever one is installed, so the preview needs the
/// result's metadata and nothing else of it.
fn overhead_of(
    metadata: &ObservationMetadata,
    artifact_ref: &ArtifactRef,
    guidance: &str,
) -> Result<Overhead> {
    let mut preview = ToolOutput::text("").with_metadata(metadata.clone());
    preview.metadata_mut().push_truncation(Truncation::new(
        TruncationStage::ContextBudget,
        u64::MAX,
        u64::MAX,
    ));
    preview.metadata_mut().push_guidance(guidance);
    let placeholder = ModelExcerpt::new(vec![ToolOutputBlock::text("")], artifact_ref.clone())?;
    let cost = measure(&preview.with_model_excerpt(placeholder).model_blocks())?;
    Ok(Overhead {
        bytes: cost.bytes.saturating_add(EXTRA_EXCERPT_TEXT_SEPARATORS),
        tokens: cost.tokens.saturating_add(EXTRA_EXCERPT_TEXT_SEPARATORS),
    })
}

/// The sentence the model is told about why its result was cut.
///
/// One spelling, used both to write the real guidance and to price the widest one a floor has to
/// admit; two copies of this format string would drift and the floor would then be computed for a
/// sentence the projector no longer produces.
fn guidance_sentence(causes: &str, bytes: usize, tokens: usize) -> String {
    format!(
        "Context-budget excerpt limited by {causes}: the complete model-visible result was {bytes} \
         bytes and {tokens} estimated tokens."
    )
}

/// The smallest ceilings that can carry a projection's own framing plus a marked excerpt.
///
/// The artifact reference is priced at one character rather than at some assumed identifier length.
/// A reference carries whatever the provider issued as a call identifier and the host chose as a
/// run identifier, and inventing a bound for those here would either reject workable budgets or
/// pretend to a guarantee this function cannot give;
/// [`ToolResultBudget::ensure_fits`] prices the real one against the real ceiling.
fn floor() -> Result<Overhead> {
    let overhead = overhead_of(
        &ObservationMetadata::new(),
        &ArtifactRef::new("x")?,
        &guidance_sentence(exceeded_causes(true, true), usize::MAX, usize::MAX),
    )?;
    // The marker on both sides, the widest omission notice, and content worth reading.
    let content = MARKER
        .len()
        .saturating_mul(2)
        .saturating_add(omission_notice(1).len());
    Ok(Overhead {
        bytes: overhead.bytes.saturating_add(content),
        tokens: overhead
            .tokens
            .saturating_add(content.div_ceil(CHARS_PER_TOKEN)),
    })
}

fn exceeded_causes(over_bytes: bool, over_tokens: bool) -> &'static str {
    match (over_bytes, over_tokens) {
        (true, true) => "the byte and token limits",
        (true, false) => "the byte limit",
        // Reached only after one ceiling was exceeded, so "neither" is not a case to answer.
        (false, _) => "the token limit",
    }
}

/// Names one result by the run that produced it as well as the call that asked for it.
fn artifact_ref(run_id: &RunId, call_id: &CallId) -> Result<ArtifactRef> {
    ArtifactRef::new(format!(
        "tool-output/{}/{}",
        hex(run_id.as_str()),
        hex(call_id.as_str())
    ))
}

/// Hex-encodes an identifier so a reference stays path-safe whatever the provider sent.
fn hex(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len().saturating_mul(2));
    for byte in value.as_bytes() {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
