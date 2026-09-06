//! Loading one installed skill's instructions.
//!
//! # The entry is the second half of a mechanism whose first half is prompt text
//!
//! A skill is only reachable if the model knows it exists, and it is only affordable if the
//! knowing costs a line rather than a document. So the catalog's summaries are rendered into the
//! instructions once — [`render_catalog_listing`] is what does it — and this entry returns the body
//! of whichever one the run turns out to need.
//!
//! Both halves read the same [`SkillCatalog`], which is what keeps them from disagreeing: a listing
//! naming a skill this entry cannot load, or an entry serving one the listing never mentioned, are
//! both arrangements that require two sources.
//!
//! # A skill body is host material, not model input to be doubted
//!
//! Unlike a fetched page, what a catalog returns was installed by whoever runs this agent. It is
//! instructions in the ordinary sense, and the entry renders it without a warning attached. What it
//! does bound is the size: an unbounded body is one that can spend a context window on a document
//! nobody re-read before installing it.

use std::{
    fmt::{self, Write as _},
    sync::Arc,
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    permission::PermissionScope,
    prompt::estimate_tokens,
    skill::{SkillCatalog, SkillCatalogError, SkillId, SkillSummary},
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, Tool, ToolArgumentDecodeError,
        ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema, Truncation, TruncationStage,
    },
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "skill";

// The doc comment below is the model-facing description. It names where the identifiers come from,
// because a model that invents one gets a refusal it could not have predicted from the schema.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Loads a skill by identifier, or lists installed skills when skill is null.
struct SkillInput {
    /// Exact skill identifier to load, or null to list installed skills.
    skill: Option<String>,
    /// Zero-based listing offset; use the next offset returned by a listing.
    offset: Option<usize>,
}

/// Ceilings this entry applies to a loaded skill.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillLimits {
    body_bytes: usize,
}

impl Default for SkillLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillLimits {
    /// Creates the default ceiling.
    ///
    /// 32 KiB is roughly eight thousand tokens: enough for a long procedure with examples, and
    /// small enough that loading one is a decision the run can afford to have made wrongly.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            body_bytes: 32 * 1024,
        }
    }

    /// Sets the ceiling on one skill body, in bytes.
    #[must_use]
    pub const fn with_max_body_bytes(mut self, bytes: usize) -> Self {
        self.body_bytes = if bytes == 0 { 1 } else { bytes };
        self
    }

    /// Ceiling on one skill body, in bytes.
    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.body_bytes
    }
}

/// How much of a catalog one listing may describe.
///
/// The listing lands in the cached prefix, where it is paid for on every turn of every run — so it
/// is bounded by the same declared token share every other prefix fragment answers to, rather than
/// by how many skills a host happened to install.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillListingLimits {
    tokens: usize,
    description_chars: usize,
}

impl Default for SkillListingLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillListingLimits {
    /// Creates the defaults.
    ///
    /// 256 tokens is about a dozen entries — the size at which a catalog is a menu rather than a
    /// manual. A host with more skills than that raises the share deliberately, which is the point:
    /// the resident cost of a catalog is a decision somebody makes, not a number that grows with an
    /// install.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tokens: 256,
            description_chars: 160,
        }
    }

    /// Sets the prefix share the listing may spend, in estimated tokens.
    #[must_use]
    pub const fn with_max_tokens(mut self, tokens: usize) -> Self {
        self.tokens = if tokens == 0 { 1 } else { tokens };
        self
    }

    /// Sets the ceiling on one entry's description, in characters.
    #[must_use]
    pub const fn with_max_description_chars(mut self, chars: usize) -> Self {
        self.description_chars = if chars == 0 { 1 } else { chars };
        self
    }

    /// Prefix share the listing may spend, in estimated tokens.
    #[must_use]
    pub const fn max_tokens(&self) -> usize {
        self.tokens
    }

    /// Ceiling on one entry's description, in characters.
    #[must_use]
    pub const fn max_description_chars(&self) -> usize {
        self.description_chars
    }
}

/// Renders the installed skills into the lines a prefix fragment carries.
///
/// One line per skill: the identifier the entry takes, then what the skill is for. The identifier
/// comes first because it is the part that has to be sent back exactly.
///
/// **It stops at the declared share rather than rendering everything and hoping.** A listing that
/// overran would fail assembly — the section's budget is checked — which is a worse outcome than a
/// listing that describes what it could and says how many it left out: the first makes installing a
/// skill break the agent, the second makes it visible.
#[must_use]
pub fn render_catalog_listing(skills: &[SkillSummary], limits: SkillListingLimits) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut listing = String::new();
    let mut described = 0_usize;
    for skill in skills {
        let line = format!(
            "- `{}`: {}\n",
            skill.id(),
            clamp_chars(skill.description(), limits.description_chars)
        );
        // The first entry is rendered whatever it costs. A share too small for even one line is a
        // misconfiguration, and answering it with an empty listing would hide the catalog rather
        // than report the share.
        if described > 0 && estimate_tokens(&listing) + estimate_tokens(&line) > limits.tokens {
            break;
        }
        listing.push_str(&line);
        described = described.saturating_add(1);
    }
    let omitted = skills.len().saturating_sub(described);
    if omitted > 0 {
        let _ = writeln!(
            listing,
            "- ({omitted} more are installed; call `skill` with skill=null and offset={described} to list them.)"
        );
    }
    listing
}

/// Cuts text to a character count, marking the cut so a reader does not take a clause for a
/// sentence.
fn clamp_chars(text: &str, ceiling: usize) -> String {
    if text.chars().count() <= ceiling {
        return text.to_owned();
    }
    let kept: String = text.chars().take(ceiling).collect();
    format!("{}…", kept.trim_end())
}

/// The `skill` tool.
pub struct SkillTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    catalog: Arc<dyn SkillCatalog>,
    limits: SkillLimits,
}

impl fmt::Debug for SkillTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillTool")
            .field("origin", &self.origin)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl SkillTool {
    /// Creates the entry over one catalog.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(catalog: Arc<dyn SkillCatalog>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<SkillInput>(TOOL_NAME)?,
            options: ToolOptions::new()
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_permission_scope(PermissionScope::Read)
                .with_concurrency(ToolConcurrency::Parallel),
            catalog,
            limits: SkillLimits::new(),
        })
    }

    /// Replaces the size ceiling.
    #[must_use]
    pub const fn with_limits(mut self, limits: SkillLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The ceiling in force.
    #[must_use]
    pub const fn limits(&self) -> &SkillLimits {
        &self.limits
    }
}

/// The failure this crate produces on its own, before a catalog is ever reached.
#[derive(Debug)]
enum SkillToolFailure {
    /// The argument object does not match the schema.
    BadArguments(ToolArgumentDecodeError),
}

impl SkillToolFailure {
    fn into_error(self) -> Error {
        Error::tool(ToolErrorKind::InvalidInput, TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    const fn next_step(&self) -> &'static str {
        match self {
            Self::BadArguments(_) => {
                "Send `skill` to load an identifier, or null with `offset` to list skills."
            }
        }
    }
}

impl fmt::Display for SkillToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field, which is the whole of
            // what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(formatter, "Invalid arguments: {reason}."),
        }
    }
}

impl std::error::Error for SkillToolFailure {}

/// The next step that follows a catalog's refusal.
const fn catalog_next_step(failure: &SkillCatalogError) -> &'static str {
    match failure {
        SkillCatalogError::NotFound { .. } => {
            "Use an identifier exactly as the skill list gives it."
        }
        SkillCatalogError::NotAllowed { .. } | SkillCatalogError::TooLarge { .. } => {
            "Continue without it; this is not something a different request reaches."
        }
        SkillCatalogError::Unavailable { .. } => {
            "Continue without it, and say the instructions were not available."
        }
        // The contract's failures are `#[non_exhaustive]`, so a refusal this crate cannot name yet
        // is still a refusal, and the model still gets something to do about it.
        _ => "Continue without the skill.",
    }
}

/// Renders a failure into the observation a model reads, or lets a control signal through.
///
/// An unrecognized catalog failure still becomes a sentence. This entry takes
/// [`ToolFailureHandling::Custom`], where returning `None` propagates the error and ends the turn —
/// so a catalog reporting in its own vocabulary would take down a whole run over instructions the
/// run can proceed without.
fn failure_output(error: &Error) -> Option<ToolOutput> {
    if error.is_cancelled() || matches!(error, Error::Budget { .. } | Error::Guardrail { .. }) {
        return None;
    }
    if let Some(failure) = SkillToolFailure::of(error) {
        return Some(
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(failure.next_step())),
        );
    }
    if let Some(failure) = SkillCatalogError::of(error) {
        return Some(
            ToolOutput::text(failure.to_string()).with_metadata(
                ObservationMetadata::new().with_guidance(catalog_next_step(failure)),
            ),
        );
    }
    Some(
        ToolOutput::text("The skill could not be loaded.")
            .with_metadata(ObservationMetadata::new().with_guidance(
                "Continue without it, and say the instructions were not available.",
            )),
    )
}

#[async_trait]
impl Tool for SkillTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema
            .decode_value_diagnostic(arguments.clone())
            .map(Some)
            .map_err(|error| SkillToolFailure::BadArguments(error).into_error())
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = match context.take_decoded_input::<SkillInput>()? {
            Some(input) => input,
            None => serde_json::from_value(context.arguments().clone()).map_err(|error| {
                SkillToolFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                    input_type: self.func_schema.input_type_name(),
                    message: error.to_string(),
                })
                .into_error()
            })?,
        };

        let Some(skill) = input.skill else {
            let skills = self.catalog.list().await?;
            let offset = input.offset.unwrap_or(0).min(skills.len());
            let end = offset.saturating_add(16).min(skills.len());
            let mut text = String::new();
            for summary in &skills[offset..end] {
                let _ = writeln!(text, "- `{}`: {}", summary.id(), summary.description());
            }
            if text.is_empty() {
                text.push_str("No more installed skills.");
            }
            let mut metadata = ObservationMetadata::new();
            if end < skills.len() {
                metadata = metadata.with_guidance(format!(
                    "To list more skills, call `skill` with skill=null and offset={end}."
                ));
            }
            return Ok(ToolOutput::text(text).with_metadata(metadata));
        };
        let document = self.catalog.load(&SkillId::new(skill)).await?;
        let body = document.body();
        let ceiling = self.limits.body_bytes;
        let mut metadata = ObservationMetadata::new();
        let text = if body.len() > ceiling {
            let mut end = ceiling;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            metadata = metadata
                .with_truncation(Truncation::new(
                    TruncationStage::Tool,
                    as_u64(body.len()),
                    as_u64(end),
                ))
                .with_guidance("These instructions are longer than this entry returns.");
            &body[..end]
        } else {
            body
        };

        // The header is one line and it earns it: the display name is what a person reading the
        // transcript recognizes, and the location is how the model opens whatever the body refers
        // to. Both come from the catalog rather than from the identifier the model happened to send.
        let mut rendered = format!("Skill `{}` — {}\n", document.id(), document.name());
        if let Some(location) = document.location() {
            let _ = writeln!(rendered, "Its files are at {location}");
        }
        rendered.push('\n');
        rendered.push_str(text);
        Ok(ToolOutput::text(rendered).with_metadata(metadata))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(failure_output(error))
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
