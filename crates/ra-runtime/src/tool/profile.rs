//! Which registered tools one agent advertises, and what that surface is allowed to cost.
//!
//! A profile is a selection plus a budget. It names no tools of its own and knows nothing about
//! what any of them do: the host registers tools in a [`ToolRegistry`](super::registry::ToolRegistry)
//! and a profile says which of them this agent gets and how large the result may be. Assembly then
//! happens in one direction only — registry plus profile produces a [`ToolSurface`] — so the
//! declared surface can be inspected, snapshotted, and rejected before a model has been paid to
//! read it.
//!
//! **The named tiers are not here.** `core` / `codex_like` / `full`, and the entry counts that go
//! with them, are one product's policy about its own tool surface; a graph-orchestration product
//! or a read-only assistant sets different numbers for good reasons. What the kernel owns is the
//! mechanism and the fact that every profile has to state a number.
//!
//! # Why the budget is mandatory
//!
//! A tool surface only ever grows. Each addition is individually reasonable, nothing fails, and
//! the cost shows up as a slightly larger bill on every single turn — the measured end state of
//! that process is a 43-entry surface spending 11.9k tokens per turn on schemas alone. A ceiling
//! turns the next addition into a decision someone has to make on purpose, which is the only
//! moment at which the trade is visible. [`ToolProfileBuilder::build`] therefore refuses a profile
//! that has not declared one; "no ceiling" is expressible, but only by writing it down.
//!
//! A profile budgets tool selection, not the final provider request. Handoffs belong to an agent
//! declaration and are resolved only after dynamic availability, so the combined tool-and-handoff
//! ceiling is [`ActionSurfaceBudget`](crate::runner::ActionSurfaceBudget), applied during turn
//! preparation.

use std::{collections::BTreeSet, fmt, sync::Arc};

use ra_core::{
    error::{Error, Result},
    tool::{Tool, ToolLookupKey},
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

/// Stable name of a tool-surface profile.
///
/// It is an identity, not display text: it appears in configuration, in assembly reports, and in
/// the snapshots that prove a surface has not silently grown.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolProfileId(String);

impl ToolProfileId {
    /// Creates a non-empty profile identity.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            return Err(Error::caller(
                "tool profile ID must be non-empty, trimmed, and contain no control characters",
            ));
        }
        Ok(Self(value))
    }

    /// Stable string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// The ceiling — and floor — one profile holds its advertised surface to.
///
/// Both bounds count only entries the model will actually be shown: a tool that is registered but
/// withheld from the tool list costs nothing per turn and is not measured. The count is the
/// declared worst case rather than what any single turn ends up sending, because a tool with
/// dynamic availability may be on, and a budget that assumed it off would be a budget that only
/// holds on the turns nobody worried about.
///
/// # Why there is a floor
///
/// A surface that lost a tool fails in a way a ceiling cannot catch: the run keeps going, the
/// prompt still describes the missing entry, and the model spends turns asking for something that
/// is no longer there. The floor is how a profile says which of its entries were the point.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "DeclaredBudget")]
pub struct ToolSurfaceBudget {
    min_advertised: usize,
    max_advertised: usize,
    max_advertised_bytes: Option<usize>,
    max_advertised_name_chars: Option<usize>,
}

impl ToolSurfaceBudget {
    /// Declares how many advertised entries the surface must have.
    ///
    /// Pass `usize::MAX` as the ceiling to opt out of one. That is deliberately something a
    /// profile has to write rather than something it gets by saying nothing.
    pub fn new(min_advertised: usize, max_advertised: usize) -> Result<Self> {
        if min_advertised > max_advertised {
            return Err(Error::caller(format!(
                "tool surface budget floor {min_advertised} cannot exceed its ceiling \
                 {max_advertised}"
            )));
        }
        Ok(Self {
            min_advertised,
            max_advertised,
            max_advertised_bytes: None,
            max_advertised_name_chars: None,
        })
    }

    /// Adds a ceiling on the total bytes the advertised entries contribute.
    ///
    /// Entry count and byte count are separate limits because they fail separately: a surface can
    /// stay inside its entry count while one tool's description triples, and the per-turn bill
    /// follows the bytes.
    #[must_use]
    pub const fn with_max_advertised_bytes(mut self, bytes: usize) -> Self {
        self.max_advertised_bytes = Some(bytes);
        self
    }

    /// Adds a ceiling on the total characters in advertised model-facing names.
    ///
    /// This is separate from the schema-byte ceiling because a prompt inventory repeats names
    /// but not schemas. A profile that renders those names into a cached prefix can therefore
    /// state the cost of that projection without coupling its allowance to provider wire format.
    #[must_use]
    pub const fn with_max_advertised_name_chars(mut self, chars: usize) -> Self {
        self.max_advertised_name_chars = Some(chars);
        self
    }

    /// Fewest advertised entries the surface may have.
    #[must_use]
    pub const fn min_advertised(&self) -> usize {
        self.min_advertised
    }

    /// Most advertised entries the surface may have.
    #[must_use]
    pub const fn max_advertised(&self) -> usize {
        self.max_advertised
    }

    /// Byte ceiling on the advertised entries, if the profile declared one.
    #[must_use]
    pub const fn max_advertised_bytes(&self) -> Option<usize> {
        self.max_advertised_bytes
    }

    /// Character ceiling on advertised model-facing names, if the profile declared one.
    #[must_use]
    pub const fn max_advertised_name_chars(&self) -> Option<usize> {
        self.max_advertised_name_chars
    }
}

/// The wire shape of a budget, before anything has checked it.
///
/// A budget that arrives from configuration goes through [`ToolSurfaceBudget::new`] like every
/// other one. Deriving `Deserialize` straight onto the fields would let an inverted budget exist,
/// and it would then fail every assembly with an error pointing at the tool surface rather than at
/// the two numbers that are the actual mistake.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredBudget {
    min_advertised: usize,
    max_advertised: usize,
    #[serde(default)]
    max_advertised_bytes: Option<usize>,
    #[serde(default)]
    max_advertised_name_chars: Option<usize>,
}

impl TryFrom<DeclaredBudget> for ToolSurfaceBudget {
    type Error = Error;

    fn try_from(declared: DeclaredBudget) -> Result<Self> {
        let budget = Self::new(declared.min_advertised, declared.max_advertised)?;
        let budget = match declared.max_advertised_bytes {
            Some(bytes) => budget.with_max_advertised_bytes(bytes),
            None => budget,
        };
        Ok(match declared.max_advertised_name_chars {
            Some(chars) => budget.with_max_advertised_name_chars(chars),
            None => budget,
        })
    }
}

/// Which registered tools a profile takes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSelection {
    /// Exactly these lookup keys. A key the registry does not hold fails assembly.
    ///
    /// Naming the keys is what makes a surface reviewable: the list is the specification, and a
    /// tool that has not been written yet fails loudly against it instead of being quietly absent.
    Explicit(BTreeSet<ToolLookupKey>),
    /// Every tool the registry holds, whatever it is.
    ///
    /// This is not a shorthand for listing them. A registry is also where tools that appear at run
    /// time land — an MCP server's exports are not knowable when the profile is written — and a
    /// profile that could only enumerate keys could never include them. The budget is what keeps
    /// this honest: "everything installed" stays a bounded statement.
    AllRegistered,
}

/// A named selection of registered tools, bounded by a budget.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ToolProfile {
    id: ToolProfileId,
    selection: ToolSelection,
    budget: ToolSurfaceBudget,
}

impl ToolProfile {
    /// Starts a builder for a profile with this identity.
    pub fn builder(id: ToolProfileId) -> ToolProfileBuilder {
        ToolProfileBuilder {
            id,
            keys: Vec::new(),
            all_registered: false,
            budget: None,
        }
    }

    /// Profile identity.
    #[must_use]
    pub const fn id(&self) -> &ToolProfileId {
        &self.id
    }

    /// Which registered tools this profile takes.
    #[must_use]
    pub const fn selection(&self) -> &ToolSelection {
        &self.selection
    }

    /// The bounds an assembled surface is checked against.
    #[must_use]
    pub const fn budget(&self) -> &ToolSurfaceBudget {
        &self.budget
    }
}

/// Builder for an immutable [`ToolProfile`].
#[must_use]
pub struct ToolProfileBuilder {
    id: ToolProfileId,
    keys: Vec<ToolLookupKey>,
    all_registered: bool,
    budget: Option<ToolSurfaceBudget>,
}

impl ToolProfileBuilder {
    /// Adds one lookup key to an explicit selection.
    pub fn include(mut self, key: ToolLookupKey) -> Self {
        self.keys.push(key);
        self
    }

    /// Adds lookup keys to an explicit selection.
    pub fn include_all(mut self, keys: impl IntoIterator<Item = ToolLookupKey>) -> Self {
        self.keys.extend(keys);
        self
    }

    /// Takes every tool the registry holds instead of an explicit list.
    pub fn all_registered(mut self) -> Self {
        self.all_registered = true;
        self
    }

    /// Declares the bounds the assembled surface is held to.
    pub fn budget(mut self, budget: ToolSurfaceBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Validates the profile and freezes it.
    pub fn build(self) -> Result<ToolProfile> {
        let id = self.id;
        let Some(budget) = self.budget else {
            return Err(Error::config(format!(
                "tool profile `{id}` must declare a surface budget"
            )));
        };

        let selection = if self.all_registered {
            if !self.keys.is_empty() {
                // The two say different things and there is no reading under which one implies
                // the other, so picking either would be picking for the author.
                return Err(Error::config(format!(
                    "tool profile `{id}` both takes every registered tool and names {} of them; \
                     it can do one or the other",
                    self.keys.len()
                )));
            }
            ToolSelection::AllRegistered
        } else {
            let mut keys = BTreeSet::new();
            for key in self.keys {
                if !keys.insert(key.clone()) {
                    // Silent de-duplication would hide the copy/paste it comes from, and the
                    // author would be looking at a surface one entry smaller than the list reads.
                    return Err(Error::config(format!(
                        "tool profile `{id}` names lookup key `{key:?}` more than once"
                    )));
                }
            }
            ToolSelection::Explicit(keys)
        };

        Ok(ToolProfile {
            id,
            selection,
            budget,
        })
    }
}

/// One agent's assembled tool surface: the executable tools plus what they cost.
///
/// The tools are in lookup-key order rather than the order they were registered or listed in. Two
/// hosts that install the same tools in different orders therefore advertise byte-identical tool
/// tables, and a cached prefix built over that table survives a reordering in host startup code.
#[non_exhaustive]
pub struct ToolSurface {
    profile: ToolProfileId,
    tools: Vec<Arc<dyn Tool>>,
    advertised: Vec<String>,
    advertised_bytes: usize,
}

impl ToolSurface {
    pub(super) fn new(
        profile: ToolProfileId,
        tools: Vec<Arc<dyn Tool>>,
        advertised: Vec<String>,
        advertised_bytes: usize,
    ) -> Self {
        Self {
            profile,
            tools,
            advertised,
            advertised_bytes,
        }
    }

    /// Which profile produced this surface.
    #[must_use]
    pub const fn profile(&self) -> &ToolProfileId {
        &self.profile
    }

    /// Every selected tool, advertised or not, in lookup-key order.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Takes the tools, for handing to an agent declaration.
    #[must_use]
    pub fn into_tools(self) -> Vec<Arc<dyn Tool>> {
        self.tools
    }

    /// Model-facing names of the entries that count against the budget, in lookup-key order.
    ///
    /// This is the list a prompt's tool section has to agree with: a profile that stops
    /// advertising a tool while the prompt still tells the model to use it produces turns spent
    /// asking for something that is not there.
    ///
    /// Assembly recorded these rather than the surface deriving them again, so they are the same
    /// names the budget was charged for and the same ones the uniqueness check ruled on.
    pub fn advertised_names(&self) -> impl Iterator<Item = &str> {
        self.advertised.iter().map(String::as_str)
    }

    /// How many entries the model is shown.
    #[must_use]
    pub const fn advertised_count(&self) -> usize {
        self.advertised.len()
    }

    /// What the advertised tool entries cost, by [`ToolSchema::advertised_bytes`].
    ///
    /// This excludes handoffs because a [`ToolSurface`] is a tool-selection result. Use the final
    /// turn's [`ActionSurfaceBudget`](crate::runner::ActionSurfaceBudget) to bound the complete
    /// provider action table.
    ///
    /// [`ToolSchema::advertised_bytes`]: ra_core::tool::ToolSchema::advertised_bytes
    #[must_use]
    pub const fn advertised_bytes(&self) -> usize {
        self.advertised_bytes
    }

    /// How many tools the surface holds in total, advertised or not.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the surface holds no tools at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl fmt::Debug for ToolSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolSurface")
            .field("profile", &self.profile)
            .field("tools", &self.len())
            .field("advertised_count", &self.advertised_count())
            .field("advertised_bytes", &self.advertised_bytes)
            .field("advertised", &self.advertised)
            .finish_non_exhaustive()
    }
}

/// Whether this tool occupies a slot in the turn's tool table **today**.
///
/// A tool that is registered for host dispatch, or switched off entirely, is still a tool the
/// registry routes — it just is not one the surface budget is paying for. The rule that decides
/// this lives on
/// [`ToolOptions::is_advertised_to_model`](ra_core::tool::ToolOptions::is_advertised_to_model)
/// rather than being spelled out again here, so the budget and the prompt's advertised inventory
/// cannot come to disagree about what "switched off" means. What this adds is only the `&dyn Tool`
/// shape the budget iterates over.
pub(super) fn is_advertised(tool: &dyn Tool) -> bool {
    tool.options().is_advertised_to_model()
}
