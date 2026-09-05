//! The contract a long-term memory store answers, and the vocabulary its callers speak.
//!
//! A [`MemoryStore`] is where an agent's *durable* knowledge lives — what earlier runs concluded,
//! which conventions a workspace follows, what was decided and why. It is not conversation history:
//! that is [`Session`](crate::session::Session), which reads and appends the authoritative record of
//! one run. The two are unrelated contracts that a common naming accident puts side by side, and
//! keeping them apart is the first thing this module is for.
//!
//! # Nothing here describes a filesystem
//!
//! A record is named by an opaque [`MemoryRecordId`] the store issued, a position inside one is an
//! opaque [`MemoryAnchor`], and a position in a result set is an opaque [`MemoryCursor`]. There are
//! no paths, no directories, and no line numbers.
//!
//! That is a correction, not a preference. The first draft of this module took its shape from the
//! one production memory system this framework has read end to end, where memory is markdown on
//! disk — so requests carried paths and answers carried line numbers. Those are that system's
//! storage layout, and this crate is the framework constitution: a store backed by rows, documents,
//! or embeddings would have had to *invent* a directory tree and a line numbering to satisfy the
//! contract, and every one of those inventions is a fact a caller would then rely on.
//!
//! What survives is what every store genuinely has: things that can be read, things that can be
//! listed under, and a way to say where it stopped. A store that wants to show a caller something
//! more legible than an opaque handle writes it into [`MemoryRecord::label`] and
//! [`MemoryHit::location`] — `MEMORY.md:12`, `row 4821`, `chunk 7 of 19`. **Those are display
//! strings and nothing parses them**, which is exactly what lets each store choose the one that is
//! true of itself.
//!
//! # What a store is not asked to do
//!
//! There is **no `put` and no `forget`**, and their absence is the design rather than a gap.
//!
//! In the reference system the model may never write memory: the only write it can reach is a note
//! filed for a later consolidation pass to consider, and even that is gated on the user having
//! asked for it. What rewrites the durable artifacts is an offline agent, running outside the loop,
//! against the whole corpus rather than one run's impression of it. Deletion is the same shape from
//! the other side — records leave because a retention window and a usage ranking dropped them, not
//! because something in a run decided they were wrong.
//!
//! A `put` here would hand a running agent a write path that system deliberately withholds, and a
//! `forget` would let one run's mistaken confidence erase a conclusion every later run depends on.
//! Both belong to the offline pipeline, and the pipeline is not part of the agent loop.
//!
//! # Usage feedback
//!
//! Read operations have no retention side effects. Versioned results may supply evidence for a
//! final citation; the runtime validates citations against persisted tool-output exposures before
//! delivering usage to a separate sink. Unversioned records remain readable but do not affect
//! citation-based retention.
//!
//! # Search returns hits, not a ranking
//!
//! [`MemorySearchRequest`] has no relevance parameter and [`MemoryHits`] carries no scores. A `k`
//! and a score would describe a top-k ranked store, and a lexical one would have to fake both by
//! inventing an order. [`MemoryBudget`] bounds the answer without claiming the bound selected the
//! best of it, and a store that ranks internally is free to return its own best first — it simply
//! does not report a number for how good they were.
//!
//! # Bounds belong to the request, so the store is the one that stops
//!
//! Every request carries a byte budget as well as a count. A caller that only bounded the count
//! would have to throw away whatever did not fit after the fact, and everything it threw away would
//! be unreachable: continuation runs on the cursor the *store* issued, and a caller cannot invent
//! one. Handing the budget down means the store stops where it can still say where it stopped.
//!
//! # Where results are allowed to land
//!
//! Nothing a store returns may reach the cached prefix. A retrieval result is a function of the
//! query, so text derived from one moves the prefix on every turn that searches, and a prefix that
//! moves is a prefix that is never read from cache. Results reach a model as tool results, in the
//! request tail, where varying content costs what it costs and nothing else.
//!
//! That is a constraint on the *assembly* layer rather than on this trait, and the reason it is
//! stated here is that this trait is where the temptation starts: a capability holding a store can
//! trivially read it during assembly and paste a digest into its static fragment. A
//! query-independent digest with a hard ceiling — a summary the offline pipeline rewrites, not one
//! this trait produced — is the one form of memory text that belongs in the prefix, because it does
//! not vary with what the run is doing.
//!
//! # Growth
//!
//! The request and answer types are `#[non_exhaustive]` and reached through accessors, so a field
//! can be added without breaking a caller that constructed one. Optional feedback uses a separate
//! [`MemoryUsageSink`], so a read backend does not need a write method. The three read operations
//! have no default implementation, because a store
//! that answers none of them is not a store, and defaulting them would let one exist whose
//! advertised entries all refuse — which is worse than the store being absent, since a capability
//! built over it reports a working memory surface either way.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, ToolErrorKind};

/// Declares an opaque, store-issued handle: a newtype over a string nothing outside the store
/// interprets.
///
/// The three handles below are the same idea applied to three scopes, and they are separate types
/// rather than one because passing a result-set position where a record was wanted has to be a
/// compile error. A store that encodes them the same way is free to; a caller that confuses them
/// is not.
macro_rules! opaque_handle {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Creates ", $what, " from a store's own encoding of it.")]
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[doc = concat!("The encoded ", $what, ", meaningful to the store that issued it.")]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[doc = concat!("Takes the encoded ", $what, " out.")]
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

opaque_handle!(
    /// A store's stable name for one record.
    ///
    /// Stable is the load-bearing word: retention ranks a record by how often it was reached, so an
    /// identity that changed when the record's text was rewritten would reset that count on every
    /// consolidation pass and make the ranking a measure of recency.
    ///
    /// It is also what a model sends back to read a record it saw in a listing or a search, so a
    /// store should keep it short — every hit carries one, on a turn that may find nothing useful.
    /// A store whose records already have short natural names is free to use them.
    /// Hosts combining multiple stores in one run must namespace record identities across stores.
    MemoryRecordId,
    "a record identity"
);

opaque_handle!(
    /// A position inside one record, for continuing a read.
    ///
    /// Opaque because what a position *is* differs per store — a byte offset, a line, a chunk
    /// ordinal, a database key — and a caller that could read one would come to depend on whichever
    /// store it saw first.
    MemoryAnchor,
    "a position in a record"
);

opaque_handle!(
    /// A position in a result set, for continuing a listing or a search.
    MemoryCursor,
    "a position in a result set"
);

/// What a caller may do with a record.
///
/// The two variants are the caller's two verbs rather than a description of storage: read this, or
/// list what is under it. A file store maps them to files and directories, a database to rows and
/// tables, and neither mapping is something this contract needs to know.
///
/// `#[non_exhaustive]` because this is data a third-party store *produces* rather than a point
/// where the framework's own control flow converges. A caller's wildcard arm has a safe answer
/// available: treat an unrecognized kind as not listable, so a model that reaches for it is refused
/// by the store rather than sent somewhere by a guess.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryRecordKind {
    /// Something that can be read.
    Record,
    /// Something that can be listed under.
    Group,
}

/// Why a store could not answer.
///
/// It is part of the contract rather than each store's private business because the layer above has
/// to turn a refusal into a sentence a model can act on, and the only alternative to a value is
/// reading one back out of an error's prose. A caller attaches it as an [`Error`] source through
/// [`Self::into_error`] and reads it back with [`Self::of`], which is the same arrangement the
/// built-in tools use for their own failures.
///
/// The variants are the refusals that differ in *what the caller should do next*. There is no
/// "outside the store" variant: an identity is the store's own to issue, so one it does not
/// recognize is simply [`Self::NotFound`] — the confinement that a path-shaped contract had to
/// enforce is a property this one cannot express a violation of.
#[non_exhaustive]
#[derive(Debug)]
pub enum MemoryStoreError {
    /// No record with that identity.
    NotFound {
        /// Identity the caller named.
        record: MemoryRecordId,
    },
    /// The record cannot answer the operation that was asked of it.
    WrongKind {
        /// Identity the caller named.
        record: MemoryRecordId,
        /// What the caller needed it to be.
        expected: MemoryRecordKind,
    },
    /// The cursor was not one this store issued, or no longer names a position in it.
    InvalidCursor {
        /// Cursor the caller sent back.
        cursor: MemoryCursor,
    },
    /// The anchor was not one this store issued, or no longer names a position in that record.
    InvalidAnchor {
        /// Anchor the caller sent back.
        anchor: MemoryAnchor,
    },
    /// A search arrived with no queries, or with an empty one.
    EmptyQuery,
    /// A useful response cannot fit within the requested encoded byte budget.
    BudgetTooSmall,
    /// The store itself could not be reached or could not complete the operation.
    Unavailable {
        /// What went wrong, for the host's log rather than for a model.
        reason: String,
    },
}

impl MemoryStoreError {
    /// Wraps this failure in a framework error that carries it as a typed source.
    ///
    /// `tool` is the entry the failure will be reported against, which is the caller's to name: one
    /// store serves several entries, and a store that guessed would attribute a refused read to
    /// whichever entry it happened to know about.
    #[must_use]
    pub fn into_error(self, tool: impl Into<String>) -> Error {
        Error::tool(self.kind(), tool, self.to_string()).with_source(self)
    }

    /// Recovers the typed failure from an error that carries one.
    #[must_use]
    pub fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// Which failure class a host records.
    ///
    /// Everything the caller could have avoided by asking differently is `InvalidInput`; only the
    /// store failing at its own job is not.
    #[must_use]
    pub const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::NotFound { .. }
            | Self::WrongKind { .. }
            | Self::InvalidCursor { .. }
            | Self::InvalidAnchor { .. }
            | Self::EmptyQuery
            | Self::BudgetTooSmall => ToolErrorKind::InvalidInput,
            Self::Unavailable { .. } => ToolErrorKind::ExecutionFailed,
        }
    }
}

impl fmt::Display for MemoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { record } => write!(formatter, "No memory record named `{record}`."),
            Self::WrongKind {
                record,
                expected: MemoryRecordKind::Record,
            } => write!(
                formatter,
                "`{record}` cannot be read; it groups other records."
            ),
            Self::WrongKind {
                record,
                expected: MemoryRecordKind::Group,
            } => write!(
                formatter,
                "`{record}` groups nothing; it is a record to read."
            ),
            Self::InvalidCursor { cursor } => {
                write!(
                    formatter,
                    "`{cursor}` is not a position in this result set."
                )
            }
            Self::InvalidAnchor { anchor } => {
                write!(formatter, "`{anchor}` is not a position in that record.")
            }
            Self::EmptyQuery => formatter.write_str("A memory search needs at least one query."),
            Self::BudgetTooSmall => {
                formatter.write_str("The memory response budget is too small for a useful page.")
            }
            // Deliberately without the reason: a model can do nothing with a connection string or
            // an errno, and the detail is already in the error's log-facing message.
            Self::Unavailable { .. } => {
                formatter.write_str("The memory store could not be reached.")
            }
        }
    }
}

impl std::error::Error for MemoryStoreError {}

/// How much of an answer a caller can afford.
///
/// Both halves are needed and neither implies the other: a thousand tiny records and one enormous
/// one are the same count and wildly different budgets. A store honours whichever binds first and
/// reports where it stopped.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    items: usize,
    bytes: usize,
}

impl MemoryBudget {
    /// Creates a budget, raising either half to one if it was given as zero.
    ///
    /// Zero is normalized rather than accepted because a budget of nothing describes an operation
    /// with no possible useful answer, and rather than refused because this is a host's
    /// configuration mistake rather than something a run can act on — a fallible constructor here
    /// would push a `Result` into every assembly site to guard against a value nobody sets.
    #[must_use]
    pub const fn new(items: usize, bytes: usize) -> Self {
        Self {
            items: if items == 0 { 1 } else { items },
            bytes: if bytes == 0 { 1 } else { bytes },
        }
    }

    /// Most records, hits, or entries one answer may carry. Never zero.
    #[must_use]
    pub const fn items(&self) -> usize {
        self.items
    }

    /// Maximum UTF-8 bytes of the response's compact JSON encoding. Never zero.
    ///
    /// Includes identifiers, labels, content, pagination handles, JSON escaping, and punctuation.
    /// Backends can measure candidate pages with [`memory_response_json`]. If no useful page fits,
    /// return [`MemoryStoreError::BudgetTooSmall`] instead of an empty page or a non-advancing cursor.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// One record's identity and how to show it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    id: MemoryRecordId,
    label: String,
    kind: MemoryRecordKind,
}

impl MemoryRecord {
    /// Creates a record entry.
    #[must_use]
    pub fn new(id: MemoryRecordId, label: impl Into<String>, kind: MemoryRecordKind) -> Self {
        Self {
            id,
            label: label.into(),
            kind,
        }
    }

    /// The identity a caller sends back to read or list under this record.
    #[must_use]
    pub const fn id(&self) -> &MemoryRecordId {
        &self.id
    }

    /// What to show a model. Display only; nothing parses it.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this can be read or listed under.
    #[must_use]
    pub const fn kind(&self) -> MemoryRecordKind {
        self.kind
    }
}

/// A request to enumerate what a store holds.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryListRequest {
    under: Option<MemoryRecordId>,
    cursor: Option<MemoryCursor>,
    budget: MemoryBudget,
}

impl MemoryListRequest {
    /// Creates a listing request over the whole store.
    #[must_use]
    pub const fn new(budget: MemoryBudget) -> Self {
        Self {
            under: None,
            cursor: None,
            budget,
        }
    }

    /// Restricts the listing to one group.
    #[must_use]
    pub fn under(mut self, record: MemoryRecordId) -> Self {
        self.under = Some(record);
        self
    }

    /// Continues a listing from where a previous answer stopped.
    #[must_use]
    pub fn from_cursor(mut self, cursor: MemoryCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Group to enumerate, or the whole store when absent.
    #[must_use]
    pub const fn scope(&self) -> Option<&MemoryRecordId> {
        self.under.as_ref()
    }

    /// Position to resume from.
    #[must_use]
    pub const fn cursor(&self) -> Option<&MemoryCursor> {
        self.cursor.as_ref()
    }

    /// How much this answer may cost.
    #[must_use]
    pub const fn budget(&self) -> MemoryBudget {
        self.budget
    }
}

/// What a store holds under the requested scope, within the budget it was given.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryListing {
    records: Vec<MemoryRecord>,
    next: Option<MemoryCursor>,
}

impl MemoryListing {
    /// Creates a complete listing: everything in scope fit in this answer.
    #[must_use]
    pub const fn new(records: Vec<MemoryRecord>) -> Self {
        Self {
            records,
            next: None,
        }
    }

    /// Marks the listing as stopping short, and names where it stopped.
    ///
    /// The cursor is the only signal that more exists. There is no separate `truncated` flag,
    /// because a flag and a cursor are two encodings of one fact and the pair can disagree — a
    /// truncated answer with no cursor is a dead end nothing can continue from.
    #[must_use]
    pub fn continuing_at(mut self, cursor: MemoryCursor) -> Self {
        self.next = Some(cursor);
        self
    }

    /// Records in this answer.
    #[must_use]
    pub fn records(&self) -> &[MemoryRecord] {
        &self.records
    }

    /// Where a caller resumes, when this answer did not exhaust the scope.
    #[must_use]
    pub const fn next_cursor(&self) -> Option<&MemoryCursor> {
        self.next.as_ref()
    }
}

/// A request for one record's content.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryReadRequest {
    record: MemoryRecordId,
    from: Option<MemoryAnchor>,
    budget: MemoryBudget,
}

impl MemoryReadRequest {
    /// Creates a read from the start of a record.
    #[must_use]
    pub const fn new(record: MemoryRecordId, budget: MemoryBudget) -> Self {
        Self {
            record,
            from: None,
            budget,
        }
    }

    /// Starts the read at a position the store issued.
    ///
    /// The anchor comes from a previous answer — the `next` of an earlier read, or the anchor on a
    /// search hit. There is no way to name a position the store has not handed out, which is what
    /// keeps "continue from here" from becoming arithmetic on a layout the caller cannot see.
    #[must_use]
    pub fn starting_at(mut self, anchor: MemoryAnchor) -> Self {
        self.from = Some(anchor);
        self
    }

    /// Record to read.
    #[must_use]
    pub const fn record(&self) -> &MemoryRecordId {
        &self.record
    }

    /// Where in the record to start, or its beginning when absent.
    #[must_use]
    pub const fn anchor(&self) -> Option<&MemoryAnchor> {
        self.from.as_ref()
    }

    /// How much this answer may cost.
    #[must_use]
    pub const fn budget(&self) -> MemoryBudget {
        self.budget
    }
}

/// Part or all of one record's content.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExcerpt {
    record: MemoryRecordId,
    label: String,
    location: Option<String>,
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<MemoryRevision>,
    next: Option<MemoryAnchor>,
}

impl MemoryExcerpt {
    /// Binds this content to an immutable store revision. Unversioned content is readable but cannot be cited for usage feedback.
    #[must_use]
    pub fn with_revision(mut self, revision: MemoryRevision) -> Self {
        self.revision = Some(revision);
        self
    }

    /// Immutable revision of the returned content, when supplied by the store.
    #[must_use]
    pub const fn revision(&self) -> Option<&MemoryRevision> {
        self.revision.as_ref()
    }

    /// Creates an excerpt that reached the end of the record.
    #[must_use]
    pub fn new(record: MemoryRecordId, label: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            record,
            label: label.into(),
            location: None,
            text: text.into(),
            revision: None,
            next: None,
        }
    }

    /// Adds the store's own description of where this text sits in the record.
    ///
    /// Display only. `lines 12-40`, `rows 400-420`, `chunk 3 of 19` — whatever is true of this
    /// store. Nothing parses it, which is why each store may say something different.
    #[must_use]
    pub fn at(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Marks the excerpt as stopping short, and names where a caller resumes.
    #[must_use]
    pub fn continuing_at(mut self, anchor: MemoryAnchor) -> Self {
        self.next = Some(anchor);
        self
    }

    /// Record this text came from.
    #[must_use]
    pub const fn record(&self) -> &MemoryRecordId {
        &self.record
    }

    /// What to show a model for the record.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Where this text sits, in the store's own words.
    #[must_use]
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// The content itself, without numbering or any other framing.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where a caller resumes, when the record continues past this excerpt.
    #[must_use]
    pub const fn next_anchor(&self) -> Option<&MemoryAnchor> {
        self.next.as_ref()
    }
}

/// How several queries combine into one hit.
///
/// A single query makes both equivalent; the distinction exists for the search that is only
/// meaningful as a conjunction — a symbol together with the decision that mentions it. Without it a
/// caller has to run one search per term and intersect the answers itself, which costs a round trip
/// per term and loses the proximity that made the conjunction worth asking for.
///
/// The conjunction has no window parameter. A window is a count of *lines*, and lines are one
/// store's layout; the region a conjunction is evaluated over is the region that store would return
/// as an excerpt, which is the same region the caller is about to be shown.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryMatchMode {
    /// A hit needs any one query.
    Any,
    /// A hit needs every query, within the excerpt the store would return for it.
    All,
}

/// A request to find records matching some text.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySearchRequest {
    queries: Vec<String>,
    mode: MemoryMatchMode,
    under: Option<MemoryRecordId>,
    cursor: Option<MemoryCursor>,
    case_sensitive: bool,
    budget: MemoryBudget,
}

impl MemorySearchRequest {
    /// Creates a search for any of `queries`.
    #[must_use]
    pub const fn new(queries: Vec<String>, budget: MemoryBudget) -> Self {
        Self {
            queries,
            mode: MemoryMatchMode::Any,
            under: None,
            cursor: None,
            case_sensitive: false,
            budget,
        }
    }

    /// Sets how the queries combine.
    #[must_use]
    pub const fn matching(mut self, mode: MemoryMatchMode) -> Self {
        self.mode = mode;
        self
    }

    /// Restricts the search to one record or group.
    #[must_use]
    pub fn under(mut self, record: MemoryRecordId) -> Self {
        self.under = Some(record);
        self
    }

    /// Continues a search from where a previous answer stopped.
    #[must_use]
    pub fn from_cursor(mut self, cursor: MemoryCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Requires the queries to match with their original casing.
    #[must_use]
    pub const fn case_sensitive(mut self) -> Self {
        self.case_sensitive = true;
        self
    }

    /// Queries to look for.
    #[must_use]
    pub fn queries(&self) -> &[String] {
        &self.queries
    }

    /// How the queries combine.
    #[must_use]
    pub const fn mode(&self) -> MemoryMatchMode {
        self.mode
    }

    /// Record or group the search is confined to, or the whole store when absent.
    #[must_use]
    pub const fn scope(&self) -> Option<&MemoryRecordId> {
        self.under.as_ref()
    }

    /// Position to resume from.
    #[must_use]
    pub const fn cursor(&self) -> Option<&MemoryCursor> {
        self.cursor.as_ref()
    }

    /// Whether casing is part of the match.
    ///
    /// Insensitive by default, because the caller is a model writing a query from a paraphrase of
    /// something it has not read yet.
    #[must_use]
    pub const fn is_case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// How much this answer may cost.
    #[must_use]
    pub const fn budget(&self) -> MemoryBudget {
        self.budget
    }
}

/// One record a search matched, with the text that matched in it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHit {
    record: MemoryRecordId,
    label: String,
    location: Option<String>,
    excerpt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<MemoryRevision>,
    anchor: Option<MemoryAnchor>,
}

impl MemoryHit {
    /// Binds this content to an immutable store revision. Unversioned content is readable but cannot be cited for usage feedback.
    #[must_use]
    pub fn with_revision(mut self, revision: MemoryRevision) -> Self {
        self.revision = Some(revision);
        self
    }

    /// Immutable revision of the returned content, when supplied by the store.
    #[must_use]
    pub const fn revision(&self) -> Option<&MemoryRevision> {
        self.revision.as_ref()
    }

    /// Creates a hit in one record.
    #[must_use]
    pub fn new(
        record: MemoryRecordId,
        label: impl Into<String>,
        excerpt: impl Into<String>,
    ) -> Self {
        Self {
            record,
            label: label.into(),
            location: None,
            excerpt: excerpt.into(),
            revision: None,
            anchor: None,
        }
    }

    /// Adds the store's own description of where the match sits.
    #[must_use]
    pub fn at(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Adds the position a read continues this hit from.
    ///
    /// This is what turns a hit into somewhere to go: a match is a fragment, and the paragraph
    /// around it is what a caller usually wants next. Without it a caller can only re-read the
    /// record from its start.
    #[must_use]
    pub fn continuing_at(mut self, anchor: MemoryAnchor) -> Self {
        self.anchor = Some(anchor);
        self
    }

    /// Record the match is in.
    #[must_use]
    pub const fn record(&self) -> &MemoryRecordId {
        &self.record
    }

    /// What to show a model for the record.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Where the match sits, in the store's own words.
    #[must_use]
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// The matched text and whatever the store chose to show around it.
    #[must_use]
    pub fn excerpt(&self) -> &str {
        &self.excerpt
    }

    /// Where a read of this record picks up from the match.
    #[must_use]
    pub const fn anchor(&self) -> Option<&MemoryAnchor> {
        self.anchor.as_ref()
    }
}

/// What a search found, within the budget it was given.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHits {
    hits: Vec<MemoryHit>,
    next: Option<MemoryCursor>,
}

impl MemoryHits {
    /// Creates a complete answer: the search found these and nothing more.
    #[must_use]
    pub const fn new(hits: Vec<MemoryHit>) -> Self {
        Self { hits, next: None }
    }

    /// Marks the answer as stopping at the budget, and names where it stopped.
    #[must_use]
    pub fn continuing_at(mut self, cursor: MemoryCursor) -> Self {
        self.next = Some(cursor);
        self
    }

    /// Hits in this answer.
    #[must_use]
    pub fn hits(&self) -> &[MemoryHit] {
        &self.hits
    }

    /// Where a caller resumes, when the budget stopped the answer short.
    #[must_use]
    pub const fn next_cursor(&self) -> Option<&MemoryCursor> {
        self.next.as_ref()
    }
}

/// The asynchronous contract for reading an agent's durable memory.
///
/// Implementations may keep memory as markdown on disk, rows in a database, or documents behind a
/// service. What they may not do is decide the vocabulary: the operations below are what the
/// assembly layer builds a tool surface out of, and a store that answered them differently would
/// produce a surface that reads differently from one host to the next.
///
/// A store issues every identity, anchor, and cursor a caller can name, so it is also the only
/// party that has to validate them. There is no path to confine and no traversal to refuse: an
/// identity a store did not issue is one it does not recognize.
#[async_trait]
pub trait MemoryStore: Send + Sync + 'static {
    /// Enumerates records in scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the scope is unknown or is not a group, or when the cursor was not one
    /// this store issued.
    async fn list(&self, request: MemoryListRequest) -> Result<MemoryListing>;

    /// Returns part or all of one record's content.
    ///
    /// # Errors
    ///
    /// Returns an error when the record is unknown or is not readable, or when the anchor was not
    /// one this store issued for it.
    async fn read(&self, request: MemoryReadRequest) -> Result<MemoryExcerpt>;

    /// Finds records matching the request's queries.
    ///
    /// # Errors
    ///
    /// Returns an error when the queries are empty, the scope is unknown, or the cursor was not one
    /// this store issued.
    async fn search(&self, request: MemorySearchRequest) -> Result<MemoryHits>;
}

opaque_handle!(
    /// An immutable version of one record. A store must change it whenever that record's content changes.
    MemoryRevision,
    "a record revision"
);

/// Compact JSON used both by backends to measure pages and by tools to deliver them.
/// No additional framing is inserted into this body.
pub fn memory_response_json(response: &impl Serialize) -> Result<String> {
    serde_json::to_string(response).map_err(|error| Error::caller(error.to_string()))
}

/// Versioned evidence actually rendered by a memory tool. The token identifies this exact excerpt.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExposure {
    record: MemoryRecordId,
    revision: MemoryRevision,
    anchor: Option<MemoryAnchor>,
    token: String,
}

impl MemoryExposure {
    /// Binds a citation token to an immutable excerpt and its read position.
    #[must_use]
    pub fn new(
        record: MemoryRecordId,
        revision: MemoryRevision,
        anchor: Option<MemoryAnchor>,
        text: &str,
    ) -> Self {
        use sha2::{Digest, Sha256};
        // Length framing prevents different field boundaries from producing the same input.
        let mut hash = Sha256::new();
        hash.update(b"memory-exposure-v1");
        hash.update([u8::from(anchor.is_some())]);
        for field in [
            record.as_str(),
            revision.as_str(),
            anchor.as_ref().map_or("", MemoryAnchor::as_str),
            text,
        ] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field.as_bytes());
        }
        Self {
            record,
            revision,
            anchor,
            token: format!("mem-{:x}", hash.finalize()),
        }
    }

    /// Token accepted by the final citation protocol.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }
    /// Record named by this evidence.
    #[must_use]
    pub const fn record(&self) -> &MemoryRecordId {
        &self.record
    }
    /// Immutable revision that was exposed.
    #[must_use]
    pub const fn revision(&self) -> &MemoryRevision {
        &self.revision
    }
    /// Position of the exposed excerpt, if available.
    #[must_use]
    pub const fn anchor(&self) -> Option<&MemoryAnchor> {
        self.anchor.as_ref()
    }
}

/// Citations accepted for one final delivery. Sinks must deduplicate by run, final item and token.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUsage {
    /// Run that produced the final citation.
    run_id: crate::state::RunId,
    /// Agent responsible for the final delivery.
    agent_id: crate::item::AgentId,
    /// Persisted final item used as part of the sink's idempotency key.
    final_item_id: crate::item::ItemId,
    /// Validated and deduplicated excerpt references.
    citations: Vec<MemoryExposure>,
}

impl MemoryUsage {
    /// Run that produced the final citation.
    #[must_use]
    pub const fn run_id(&self) -> &crate::state::RunId {
        &self.run_id
    }

    /// Agent responsible for the final delivery.
    #[must_use]
    pub const fn agent_id(&self) -> &crate::item::AgentId {
        &self.agent_id
    }

    /// Persisted final item used in the idempotency key.
    #[must_use]
    pub const fn final_item_id(&self) -> &crate::item::ItemId {
        &self.final_item_id
    }

    /// Validated, deduplicated excerpt references.
    #[must_use]
    pub fn citations(&self) -> &[MemoryExposure] {
        &self.citations
    }

    /// Creates one deduplicable final-delivery feedback event.
    #[must_use]
    pub const fn new(
        run_id: crate::state::RunId,
        agent_id: crate::item::AgentId,
        final_item_id: crate::item::ItemId,
        citations: Vec<MemoryExposure>,
    ) -> Self {
        Self {
            run_id,
            agent_id,
            final_item_id,
            citations,
        }
    }
}

/// Optional host-owned delivery port for validated citations.
/// Implementations may enqueue events for a background retention worker. Reads never call this port.
#[async_trait]
pub trait MemoryUsageSink: Send + Sync + 'static {
    /// Records validated citations. The runtime bounds this optional operation and preserves the
    /// final answer if delivery fails. Durable delivery and retry belong to the host implementation.
    async fn record_usage(&self, usage: MemoryUsage) -> Result<()>;
}
