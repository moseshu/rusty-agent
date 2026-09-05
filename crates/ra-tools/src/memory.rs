//! Dedicated, backend-neutral memory read tools.
//!
//! Backends and tools share compact JSON as the exact byte-budget representation. Pagination
//! handles are supplied by the backend and preserved without adding body framing. Tools attach
//! versioned exposure metadata; only final citations can produce usage feedback in the runtime.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    memory::{
        MemoryAnchor, MemoryBudget, MemoryCursor, MemoryExcerpt, MemoryExposure, MemoryHits,
        MemoryListRequest, MemoryListing, MemoryMatchMode, MemoryReadRequest, MemoryRecordId,
        MemoryRecordKind, MemorySearchRequest, MemoryStore, MemoryStoreError, memory_response_json,
    },
    permission::PermissionScope,
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, Tool, ToolArgumentDecodeError,
        ToolConcurrency, ToolContext, ToolFailureHandling, ToolInput, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The advertised name of the search entry.
const SEARCH_TOOL_NAME: &str = "memory_search";
/// The advertised name of the read entry.
const READ_TOOL_NAME: &str = "memory_read";
/// The advertised name of the listing entry.
const LIST_TOOL_NAME: &str = "memory_list";

/// Ceilings these entries apply to their own answers.
///
/// Configuration rather than schema, for the reason `read_file` keeps its own out of the schema:
/// the model cannot usefully choose them, because it does not know the context budget it is
/// spending against. What the model *can* choose is a smaller count, which is why the two counted
/// entries still take an optional limit and clamp it here.
///
/// The byte ceiling is in no schema at all. Bytes are not a unit a model can reason about against
/// text it has not seen, and it has a better lever anyway: read again from the anchor it was
/// handed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimits {
    hits: usize,
    records: usize,
    bytes: usize,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryLimits {
    /// Creates the defaults.
    ///
    /// They are deliberately smaller than the workspace tools': memory is read speculatively, at
    /// the start of a task, before the run knows whether any of it is relevant. A search that can
    /// return two hundred results turns a lookup that finds nothing useful into the most expensive
    /// turn of the run.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hits: 50,
            records: 200,
            bytes: 16 * 1024,
        }
    }

    /// Sets the ceiling on hits one search returns.
    #[must_use]
    pub const fn with_max_hits(mut self, hits: usize) -> Self {
        self.hits = if hits == 0 { 1 } else { hits };
        self
    }

    /// Sets the ceiling on records one listing returns.
    #[must_use]
    pub const fn with_max_records(mut self, records: usize) -> Self {
        self.records = if records == 0 { 1 } else { records };
        self
    }

    /// Sets the compact JSON body ceiling. Citation guidance has a separate 8 KiB ceiling.
    #[must_use]
    pub const fn with_max_bytes(mut self, bytes: usize) -> Self {
        self.bytes = if bytes == 0 { 1 } else { bytes };
        self
    }

    /// The budget one search is given, with its count optionally lowered by the model.
    ///
    /// Every ceiling reaches a store through [`MemoryBudget`], which normalizes a zero to one. That
    /// is what makes a zero configured here harmless rather than fatal: the clamp applied to a
    /// model-chosen count would panic against a ceiling of zero, and the way to be certain it
    /// cannot is for no zero ceiling to survive to the point where it runs.
    #[must_use]
    pub fn search_budget(&self, requested: Option<u32>) -> MemoryBudget {
        MemoryBudget::new(count(requested, self.hits), self.bytes)
    }

    /// The budget one listing is given, with its count optionally lowered by the model.
    #[must_use]
    pub fn list_budget(&self, requested: Option<u32>) -> MemoryBudget {
        MemoryBudget::new(count(requested, self.records), self.bytes)
    }

    /// The budget one read is given.
    ///
    /// One item, because a read answers about one record. The count is not the model's to lower:
    /// asking for less of a record it has not seen is not a choice it can make usefully, and the
    /// anchor is how it asks for the rest.
    #[must_use]
    pub const fn read_budget(&self) -> MemoryBudget {
        MemoryBudget::new(1, self.bytes)
    }

    /// Ceiling on hits one search returns.
    #[must_use]
    pub const fn max_hits(&self) -> usize {
        self.hits
    }

    /// Ceiling on records one listing returns.
    #[must_use]
    pub const fn max_records(&self) -> usize {
        self.records
    }

    /// Ceiling on the content bytes any one answer may carry.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.bytes
    }
}

/// A ceiling the model may lower but not raise; absent means the host's own.
///
/// Asking for more than the host allows is not a mistake the model could have avoided — the ceiling
/// is configuration it cannot see — so the number is clamped rather than refused. The `max(1)` is
/// what keeps a host that configured a zero ceiling from turning every call into a panic, and
/// [`MemoryBudget`] normalizes the same way, so the two agree on which direction is safe.
fn count(requested: Option<u32>, ceiling: usize) -> usize {
    let ceiling = ceiling.max(1);
    requested
        .map_or(ceiling, |value| usize::try_from(value).unwrap_or(ceiling))
        .clamp(1, ceiling)
}

/// How several queries combine, as the model states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum MatchMode {
    /// A record matches when it contains any query.
    Any,
    /// A record matches when it contains every query, close enough to be shown together.
    All,
}

// The doc comment is the model-facing description. It says what the entry answers and what the
// answer is relative to, and stops: when to consult memory at all is a product's policy, and it is
// written once in the product's own prompt rather than a fourth time in each of these schemas.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Searches stored memory. Returns matching excerpts, each with the record identifier that reads
/// the rest of it.
struct MemorySearchInput {
    /// Text to look for. Substrings, not patterns.
    queries: Vec<String>,
    /// `any` matches a record containing any query; `all` requires every query. Defaults to `any`.
    mode: Option<MatchMode>,
    /// Record identifier to search within, from an earlier result. Defaults to the whole store.
    record: Option<String>,
    /// Match with regard to case. Defaults to false.
    case_sensitive: Option<bool>,
    /// Results to return. The host may enforce a lower ceiling.
    max_hits: Option<u32>,
    /// Continue a previous search from the position it reported.
    cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Reads one stored memory record, named by an identifier an earlier result reported.
struct MemoryReadInput {
    /// Record identifier, exactly as an earlier search or listing reported it.
    record: String,
    /// Continue from a position an earlier result reported, instead of the record's start.
    anchor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Lists what stored memory holds, with the identifier that reads or lists under each entry.
struct MemoryListInput {
    /// Record identifier to list under, from an earlier result. Defaults to the whole store.
    record: Option<String>,
    /// Entries to return. The host may enforce a lower ceiling.
    max_records: Option<u32>,
    /// Continue a previous listing from the position it reported.
    cursor: Option<String>,
}

/// Shared construction for the three entries: one identity, one schema, one set of options.
///
/// Read scope and parallel concurrency for all three, and that uniformity is the property the
/// capability above rests on rather than a coincidence — a memory family with one entry that wrote
/// could not be installed whole by a read-only role.
fn entry<I: ToolInput>(name: &'static str) -> Result<(ToolOrigin, FuncSchema, ToolOptions)> {
    Ok((
        ToolOrigin::new(name)?,
        FuncSchema::for_input::<I>(name)?,
        ToolOptions::new()
            .with_failure_handling(ToolFailureHandling::Custom)
            .with_permission_scope(PermissionScope::Read)
            .with_concurrency(ToolConcurrency::Parallel),
    ))
}

/// Decodes the schema-bound value the dispatcher put in the context, or the raw arguments.
///
/// The fallback keeps direct, isolated tool tests possible; provider calls never take it, because
/// the common entry validates and decodes before `call` is reached.
fn decode<I: ToolInput + for<'de> Deserialize<'de>>(
    context: &mut ToolContext<'_>,
    schema: &FuncSchema,
) -> Result<I> {
    context.take_decoded_input::<I>()?.map_or_else(
        || {
            serde_json::from_value(context.arguments().clone()).map_err(|error| {
                MemoryToolFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                    input_type: schema.input_type_name(),
                    message: error.to_string(),
                })
                .into_error(schema.tool_schema().name())
            })
        },
        Ok,
    )
}

/// The failure this crate produces on its own, before a store is ever reached.
///
/// Only one variant: everything else that can go wrong is the store's, and it arrives already typed
/// as [`MemoryStoreError`]. Restating those here would be a second copy of a taxonomy the contract
/// already owns, and the copies would drift the first time the contract grew a variant.
#[derive(Debug)]
enum MemoryToolFailure {
    /// The argument object does not match the schema.
    BadArguments(ToolArgumentDecodeError),
}

impl MemoryToolFailure {
    fn into_error(self, tool: &str) -> Error {
        Error::tool(ToolErrorKind::InvalidInput, tool, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    const fn next_step(&self) -> &'static str {
        match self {
            Self::BadArguments(_) => "Send the arguments the schema names, and nothing else.",
        }
    }
}

impl fmt::Display for MemoryToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field, which is the whole of
            // what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(formatter, "Invalid arguments: {reason}."),
        }
    }
}

impl std::error::Error for MemoryToolFailure {}

/// The next step that follows a store's refusal.
///
/// Kept beside the sentence rather than inside [`MemoryStoreError`]'s `Display` for the reason
/// `read_file` keeps its own out: a log line wants the fact without an instruction addressed to a
/// model.
const fn store_next_step(failure: &MemoryStoreError) -> &'static str {
    match failure {
        MemoryStoreError::NotFound { .. } => {
            "Use an identifier exactly as an earlier search or listing reported it."
        }
        MemoryStoreError::WrongKind {
            expected: MemoryRecordKind::Record,
            ..
        } => "List under it instead of reading it.",
        MemoryStoreError::WrongKind {
            expected: MemoryRecordKind::Group,
            ..
        } => "Read it instead of listing under it.",
        MemoryStoreError::InvalidCursor { .. } => {
            "Start the search or listing again without a cursor."
        }
        MemoryStoreError::InvalidAnchor { .. } => "Read the record again without an anchor.",
        MemoryStoreError::EmptyQuery => "Send at least one non-empty query.",
        MemoryStoreError::BudgetTooSmall => {
            "Continue without this memory page; the host must increase the response budget."
        }
        MemoryStoreError::Unavailable { .. } => {
            "Continue without memory, and say the answer is unverified against it."
        }
        // The contract's failures are `#[non_exhaustive]`, so a refusal this crate cannot name yet
        // is still a refusal, and the model still gets something to do about it.
        _ => "Try a different memory request, or continue without memory.",
    }
}

/// Renders a store refusal into the sentence a model reads.
///
/// Both halves come from the typed value: the sentence from the failure's own `Display`, the next
/// step from [`store_next_step`]. Neither is scraped back out of the framework error's message.
fn failure_output(error: &Error) -> Option<ToolOutput> {
    // Control signals must reach the runner, even when they carry a store-specific cause.
    // They stop or close out work rather than describe an optional lookup that failed.
    if error.is_cancelled() || matches!(error, Error::Budget { .. } | Error::Guardrail { .. }) {
        return None;
    }
    if let Some(failure) = MemoryToolFailure::of(error) {
        return Some(
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(failure.next_step())),
        );
    }
    if let Some(failure) = MemoryStoreError::of(error) {
        return Some(
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(store_next_step(failure))),
        );
    }
    // Everything else is a store that reported a failure in its own vocabulary rather than as a
    // `MemoryStoreError`. That is not a framework bug to surface as one: `MemoryStore` is a
    // third-party extension point, so an error this crate cannot name is a routine event.
    //
    // **It still has to become a sentence.** These entries take `ToolFailureHandling::Custom`,
    // where returning `None` propagates the error and ends the turn — so an unrecognized store
    // failure would take down a whole run over a memory lookup the run can finish without. What
    // the model is told is generic on purpose: the error's own message is a developer-facing
    // string this crate did not write and cannot vouch for, and it may name a host, a table, or a
    // connection.
    Some(
        ToolOutput::text("The memory store could not answer.").with_metadata(
            ObservationMetadata::new().with_guidance(
                "Continue without memory, and say the answer is unverified against it.",
            ),
        ),
    )
}

/// Serializes exactly the representation measured by the backend. An invalid oversized response
/// is refused as a whole: no partial handle or evidence is presented as usable.
fn render_response(
    response: &impl Serialize,
    ceiling: usize,
    exposures: Vec<MemoryExposure>,
) -> Result<ToolOutput> {
    let body = memory_response_json(response)?;
    if body.len() > ceiling {
        return Ok(over_budget_output());
    }
    // Citation guidance is a separate, bounded control envelope. Only evidence whose citation
    // token fits in that envelope is eligible for subsequent feedback.
    let mut guidance = String::new();
    let mut delivered = Vec::new();
    for evidence in exposures {
        let line = format!(
            "For record {}, revision {}, anchor {}, cite [[memory:{}]] only if used.\n",
            memory_response_json(evidence.record())?,
            memory_response_json(evidence.revision())?,
            memory_response_json(&evidence.anchor())?,
            evidence.token()
        );
        if guidance.len().saturating_add(line.len()) > 8192 {
            break;
        }
        guidance.push_str(&line);
        delivered.push(evidence);
    }
    let mut metadata = ObservationMetadata::new().with_memory_exposures(delivered);
    if !guidance.is_empty() {
        metadata = metadata.with_guidance(guidance);
    }
    Ok(ToolOutput::text(body).with_metadata(metadata))
}

/// The refusal that replaces a page no caller can afford.
///
/// **The sentence is the body, not a note beside an empty one.** A result must carry at least one
/// block, and an empty text block is a block a provider may drop — Anthropic refuses one outright,
/// so a run whose backend overran its budget would fail on its *next* request rather than on the
/// call that overran. `read_file` states the same rule for a read that selected no lines, and
/// answers it the same way: a single sentence takes the body's place.
///
/// It is deliberately not measured against the page ceiling. That ceiling bounds the compact JSON a
/// backend produced, which is what [`MemoryLimits::with_max_bytes`] says it bounds; this text is the
/// tool's own control envelope, alongside the citation guidance the same ceiling already excludes.
/// Bounding it would mean cutting a refusal down to a ceiling of one byte, and the only shapes that
/// fit are the empty block this exists to avoid.
fn over_budget_output() -> ToolOutput {
    ToolOutput::text("The memory backend exceeded its response budget. No page was delivered.")
        .with_metadata(ObservationMetadata::new().with_guidance(
            "Continue without this memory page; the host must raise the memory response budget.",
        ))
}

/// The `memory_search` tool.
pub struct MemorySearchTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    store: Arc<dyn MemoryStore>,
    limits: MemoryLimits,
}

impl fmt::Debug for MemorySearchTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemorySearchTool")
            .field("origin", &self.origin)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl MemorySearchTool {
    /// Creates the search entry over one store.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(store: Arc<dyn MemoryStore>) -> Result<Self> {
        let (origin, func_schema, options) = entry::<MemorySearchInput>(SEARCH_TOOL_NAME)?;
        Ok(Self {
            origin,
            func_schema,
            options,
            store,
            limits: MemoryLimits::new(),
        })
    }

    /// Replaces the output ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: MemoryLimits) -> Self {
        self.limits = limits;
        self
    }

    fn request(&self, input: MemorySearchInput) -> MemorySearchRequest {
        let mut request =
            MemorySearchRequest::new(input.queries, self.limits.search_budget(input.max_hits));
        if matches!(input.mode, Some(MatchMode::All)) {
            request = request.matching(MemoryMatchMode::All);
        }
        if let Some(record) = input.record {
            request = request.under(MemoryRecordId::new(record));
        }
        if let Some(cursor) = input.cursor {
            request = request.from_cursor(MemoryCursor::new(cursor));
        }
        if input.case_sensitive == Some(true) {
            request = request.case_sensitive();
        }
        request
    }

    fn render(&self, found: &MemoryHits) -> Result<ToolOutput> {
        let exposures = found
            .hits()
            .iter()
            .filter_map(|hit| {
                Some(MemoryExposure::new(
                    hit.record().clone(),
                    hit.revision()?.clone(),
                    hit.anchor().cloned(),
                    hit.excerpt(),
                ))
            })
            .collect();
        render_response(found, self.limits.max_bytes(), exposures)
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
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
            .map_err(|error| MemoryToolFailure::BadArguments(error).into_error(SEARCH_TOOL_NAME))
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = decode::<MemorySearchInput>(&mut context, &self.func_schema)?;
        let request = self.request(input);
        let max_items = request.budget().items();
        let found = self.store.search(request).await?;
        if found.hits().len() > max_items {
            return Ok(over_budget_output());
        }
        self.render(&found)
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

/// The `memory_read` tool.
pub struct MemoryReadTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    store: Arc<dyn MemoryStore>,
    limits: MemoryLimits,
}

impl fmt::Debug for MemoryReadTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryReadTool")
            .field("origin", &self.origin)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl MemoryReadTool {
    /// Creates the read entry over one store.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(store: Arc<dyn MemoryStore>) -> Result<Self> {
        let (origin, func_schema, options) = entry::<MemoryReadInput>(READ_TOOL_NAME)?;
        Ok(Self {
            origin,
            func_schema,
            options,
            store,
            limits: MemoryLimits::new(),
        })
    }

    /// Replaces the output ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: MemoryLimits) -> Self {
        self.limits = limits;
        self
    }

    fn render(&self, excerpt: &MemoryExcerpt, anchor: Option<MemoryAnchor>) -> Result<ToolOutput> {
        let exposures = excerpt
            .revision()
            .map(|revision| {
                MemoryExposure::new(
                    excerpt.record().clone(),
                    revision.clone(),
                    anchor,
                    excerpt.text(),
                )
            })
            .into_iter()
            .collect();
        render_response(excerpt, self.limits.max_bytes(), exposures)
    }
}

#[async_trait]
impl Tool for MemoryReadTool {
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
            .map_err(|error| MemoryToolFailure::BadArguments(error).into_error(READ_TOOL_NAME))
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = decode::<MemoryReadInput>(&mut context, &self.func_schema)?;
        let mut request =
            MemoryReadRequest::new(MemoryRecordId::new(input.record), self.limits.read_budget());
        if let Some(anchor) = input.anchor {
            request = request.starting_at(MemoryAnchor::new(anchor));
        }
        let anchor = request.anchor().cloned();
        let excerpt = self.store.read(request).await?;
        self.render(&excerpt, anchor)
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

/// The `memory_list` tool.
pub struct MemoryListTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    store: Arc<dyn MemoryStore>,
    limits: MemoryLimits,
}

impl fmt::Debug for MemoryListTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryListTool")
            .field("origin", &self.origin)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl MemoryListTool {
    /// Creates the listing entry over one store.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new(store: Arc<dyn MemoryStore>) -> Result<Self> {
        let (origin, func_schema, options) = entry::<MemoryListInput>(LIST_TOOL_NAME)?;
        Ok(Self {
            origin,
            func_schema,
            options,
            store,
            limits: MemoryLimits::new(),
        })
    }

    /// Replaces the output ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: MemoryLimits) -> Self {
        self.limits = limits;
        self
    }

    fn render(&self, listing: &MemoryListing) -> Result<ToolOutput> {
        render_response(listing, self.limits.max_bytes(), Vec::new())
    }
}

#[async_trait]
impl Tool for MemoryListTool {
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
            .map_err(|error| MemoryToolFailure::BadArguments(error).into_error(LIST_TOOL_NAME))
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = decode::<MemoryListInput>(&mut context, &self.func_schema)?;
        let mut request = MemoryListRequest::new(self.limits.list_budget(input.max_records));
        if let Some(record) = input.record {
            request = request.under(MemoryRecordId::new(record));
        }
        if let Some(cursor) = input.cursor {
            request = request.from_cursor(MemoryCursor::new(cursor));
        }
        let max_items = request.budget().items();
        let listing = self.store.list(request).await?;
        if listing.records().len() > max_items {
            return Ok(over_budget_output());
        }
        self.render(&listing)
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
