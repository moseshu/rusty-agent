//! The contract a skill catalog answers, and the vocabulary progressive disclosure speaks.
//!
//! A skill is an instruction document a host has installed: how this organization writes a
//! changelog, which checklist a release follows, what the house style for a report is. It is
//! written for a model to read, and the whole point of the mechanism is that the model reads it
//! *only when it applies*.
//!
//! # Two sizes, because a skill is paid for twice
//!
//! [`SkillSummary`] is a name and one sentence; [`SkillDocument`] is the whole instruction. The
//! split is the mechanism rather than a convenience: the summaries are what a model needs in order
//! to know a skill exists, and they are small enough to carry for every task. The body is what it
//! needs in order to follow one, and it is large enough that carrying it for tasks that never invoke
//! it is the difference between a catalog a host can grow and one it cannot.
//!
//! Both halves are read through this trait, so the listing a prompt is built from and the body a
//! tool returns cannot come from two different sources and disagree about what is installed.
//!
//! # Where each half is allowed to land
//!
//! The summaries may reach the cached prefix and the body may not, and the rule is not the same one
//! [`MemoryStore`](crate::memory::MemoryStore) follows.
//!
//! A memory fragment must not vary with what the store holds, because memory changes as the agent
//! works and a prefix that moves with it is never read from cache. A skill catalog is host
//! *configuration*: it changes when someone installs a skill, not while a run is running. So a
//! listing rendered from [`Self::list`](SkillCatalog::list) is stable exactly as long as the
//! installation is, which is what
//! [`Capability::static_instructions`](crate::capability::Capability::static_instructions) asks of
//! text that lands in the prefix.
//!
//! The body has no such property — it is selected by what the model asked for — so it arrives as a
//! tool result, in the tail.
//!
//! # A skill is named by an identity the host issued
//!
//! [`SkillId`] is opaque for the reason [`MemoryRecordId`](crate::memory::MemoryRecordId) is: a
//! catalog backed by a directory, a package registry, or a database row would each have to invent
//! the others' addressing to satisfy a path-shaped contract. What a reader sees instead is
//! [`SkillSummary::name`], which is display text the host chooses, and
//! [`SkillDocument::location`], which is where the host says the material lives.
//!
//! # What a catalog is not asked to do
//!
//! There is no `install` and no `remove`. Which skills an agent may reach is a deployment decision,
//! and a run that could add one could add the instructions it would then follow. Discovery,
//! allow-listing, and the size hardening that goes with reading documents off a disk belong to the
//! implementation, not to this contract.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, ToolErrorKind};

/// A catalog's stable name for one skill.
///
/// Opaque because what a skill *is* differs per catalog — a directory, a package, a row — and a
/// caller that could read one would come to depend on whichever catalog it saw first. It is short
/// on purpose: a model sends it back to load a skill it saw in a listing, and every listing carries
/// one per entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(String);

impl SkillId {
    /// Creates a skill identity from a catalog's own encoding of it.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The encoded identity, meaningful to the catalog that issued it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Takes the encoded identity out.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a listing says about one skill: enough to decide, not enough to follow.
///
/// The description is the whole of the decision material, so it is the one field a catalog cannot
/// leave empty. A summary that said only "release-checklist" would put the burden of guessing what
/// the skill covers on the reader, and the guess is made once per task for every skill installed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    id: SkillId,
    name: String,
    description: String,
}

impl SkillSummary {
    /// Creates one listing entry.
    #[must_use]
    pub fn new(id: SkillId, name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            description: description.into(),
        }
    }

    /// Identity that loads this skill.
    #[must_use]
    pub const fn id(&self) -> &SkillId {
        &self.id
    }

    /// Display name a reader sees.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What this skill covers, in the words that decide whether to load it.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// One skill's instructions, loaded because a run asked for them.
///
/// [`Self::location`] is display text and nothing here parses it. A catalog backed by a directory
/// writes the directory, so a reader can go on to open the files the body refers to; one backed by
/// a registry writes whatever means the same thing there, or nothing at all.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDocument {
    id: SkillId,
    name: String,
    body: String,
    location: Option<String>,
}

impl SkillDocument {
    /// Creates one loaded skill.
    #[must_use]
    pub fn new(id: SkillId, name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            body: body.into(),
            location: None,
        }
    }

    /// Records where the material lives, for a reader that has to open what the body refers to.
    #[must_use]
    pub fn with_location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Identity this document was loaded by.
    #[must_use]
    pub const fn id(&self) -> &SkillId {
        &self.id
    }

    /// Display name a reader sees.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The instructions themselves.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Where the catalog says this skill's material lives, when it says.
    #[must_use]
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }
}

/// Why a catalog could not answer.
///
/// Part of the contract rather than each catalog's private business, because the layer above has to
/// turn a refusal into a sentence a model can act on, and the only alternative to a value is reading
/// one back out of an error's prose. A caller attaches it as an [`Error`] source through
/// [`Self::into_error`] and reads it back with [`Self::of`].
///
/// The variants are the refusals that differ in *what the caller should do next*. A skill that is
/// installed but not permitted here is [`Self::NotAllowed`] rather than [`Self::NotFound`], because
/// the two lead somewhere different: one is a name to correct, the other is a deployment decision no
/// rewording will get past.
#[non_exhaustive]
#[derive(Debug)]
pub enum SkillCatalogError {
    /// No skill with that identity.
    NotFound {
        /// Identity the caller named.
        skill: SkillId,
    },
    /// The skill exists and this agent may not load it.
    NotAllowed {
        /// Identity the caller named.
        skill: SkillId,
    },
    /// The document is larger than this catalog will serve.
    TooLarge {
        /// Identity the caller named.
        skill: SkillId,
    },
    /// The catalog itself could not be reached or could not complete the operation.
    Unavailable {
        /// What went wrong, for the host's log rather than for a model.
        reason: String,
    },
}

impl SkillCatalogError {
    /// Wraps this failure in a framework error that carries it as a typed source.
    ///
    /// `tool` is the entry the failure will be reported against, which is the caller's to name: one
    /// catalog can serve several entries, and a catalog that guessed would attribute a refused load
    /// to whichever entry it happened to know about.
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
    /// Only the name is the caller's to get wrong, so only [`Self::NotFound`] is `InvalidInput`. The
    /// other three are the catalog declining or failing at its own job — policy the model cannot
    /// see, material it cannot resize, a backend it cannot reach — and none of them is a mistake in
    /// the request.
    ///
    /// [`ToolErrorKind::PermissionDenied`] is deliberately not used for [`Self::NotAllowed`]. That
    /// class means the run's permission evaluator refused the call before it reached anything; this
    /// refusal comes back from a catalog the call did reach, and recording it as a permission
    /// decision would put a backend's allow-list into the same count as the host's own approvals.
    #[must_use]
    pub const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::NotFound { .. } => ToolErrorKind::InvalidInput,
            Self::NotAllowed { .. } | Self::TooLarge { .. } | Self::Unavailable { .. } => {
                ToolErrorKind::ExecutionFailed
            }
        }
    }
}

impl fmt::Display for SkillCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { skill } => write!(formatter, "No skill named `{skill}`."),
            Self::NotAllowed { skill } => {
                write!(formatter, "`{skill}` is not available to this agent.")
            }
            Self::TooLarge { skill } => {
                write!(formatter, "`{skill}` is too large to load.")
            }
            // Deliberately without the reason: a model can do nothing with a path or an errno, and
            // the detail is already in the error's log-facing message.
            Self::Unavailable { .. } => {
                formatter.write_str("The skill catalog could not be reached.")
            }
        }
    }
}

impl std::error::Error for SkillCatalogError {}

/// The skills a host has installed, listed cheaply and loaded on demand.
///
/// Both operations are required. A catalog that could list without loading would advertise skills
/// no run can follow, which is worse than the catalog being absent — the surface reports a working
/// skill mechanism either way, and only one of them has one.
#[async_trait]
pub trait SkillCatalog: Send + Sync + 'static {
    /// Every skill this agent may load, in the order the catalog considers meaningful.
    ///
    /// Called during assembly rather than per turn, because what it returns is rendered into
    /// instructions that a run reads from cache. A catalog that consults a network on every call
    /// pays for it once per assembly, not once per model request.
    ///
    /// # Errors
    ///
    /// Returns a [`SkillCatalogError::Unavailable`] when the catalog cannot be read at all.
    async fn list(&self) -> Result<Vec<SkillSummary>>;

    /// One skill's instructions.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal for an identity this catalog does not serve, will not serve to
    /// this agent, or cannot read.
    async fn load(&self, skill: &SkillId) -> Result<SkillDocument>;
}
