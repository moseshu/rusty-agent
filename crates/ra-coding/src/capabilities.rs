//! Composition of Shell / Filesystem / `ApplyPatch` / Search / Compaction.
//!
//! The built-in capability types live in [`ra_tools::capability`]; what this module decides is
//! which of them a coding agent installs for a given role, and in what order. That is the product
//! content: the capabilities themselves would serve a data-analysis agent unchanged, while "a
//! read-only specialist gets discovery and nothing that writes" is a statement about this product.
//!
//! # Why the role filter reads permission scopes instead of listing names
//!
//! A read-only role's own prompt says it has no editing tools and that an edit will fail. The set
//! that keeps that promise is exactly the capabilities whose every tool declares
//! [`PermissionScope::Read`], read from each tool's own declaration rather than from a second list
//! maintained here — a hand-written survivor list disagrees with the tools the first time one is
//! added, and the disagreement shows up as a prompt that denies an entry the request carries.
//!
//! The filter runs per capability rather than per tool, because a capability is atomic: keeping
//! half of one would produce a surface no installed capability describes, which is the arrangement
//! [`Capability`] exists to make unrepresentable. It happens that this product's split lands
//! cleanly on that boundary — `filesystem` and `search` only observe, `apply_patch` and `shell` do
//! not — which is a fact about how the families were drawn, not a coincidence to rely on.
//!
//! # Why the withheld half is kept rather than dropped
//!
//! A role does not select tools; it selects capabilities, and the tool surface is then assembled
//! from a [`CodingProfile`](crate::CodingProfile) tier that was written for an agent installing
//! every one of them. A tier states a floor as well as a ceiling, and the floor exists to catch a
//! surface that *lost* an entry — the failure a ceiling cannot see, where the run keeps going while
//! the prompt still describes a tool that is gone. A read-only agent trips that floor for a reason
//! that is not a mistake at all.
//!
//! Recording what the role withheld is what tells the two apart. The profile discounts exactly
//! those entries, so "three tools are missing because this role does not get them" narrows the
//! tier, while "three tools are missing" still fails it.
//!
//! # Where the resulting set goes
//!
//! It is read by the agent construction path, for **both** halves it carries: the tools, which go
//! through a registry and a profile to become one bounded surface, and the prompt fragments that
//! describe them, which join the product's own sections in the same stable prefix. One switch moves
//! both because one object produced both.
//!
//! It is not additionally installed on [`RunConfig`](ra_runtime::runner::RunConfig). A capability
//! installed there contributes its tools at run assembly and the agent already declares them, so
//! installing in both places declares one tool twice and fails the build.
//!
//! # Why the fragments arrive here and not at run assembly
//!
//! The obvious symmetry would be the other one: move the tools to `RunConfig` and let both halves
//! arrive when the run does. Two facts rule it out, and both are about the *inventory* section
//! rather than about the capability fragments.
//!
//! The inventory is product text rendered from the assembled surface — it names the entries this
//! profile advertises — and `ra-runtime` can neither write product text nor reach the assembler
//! that would place it. So the inventory cannot arrive at run assembly, and a fragment describing
//! `apply_patch` that arrived without the list saying the agent has it would be describing a
//! surface nothing in the prefix declares. Second, contributions collected at run assembly are
//! appended to the agent's instructions in assembly order, downstream of the canonical section
//! ranking and of the committed prompt dump — so a prefix half-assembled there is a prefix the
//! snapshot gate no longer covers.
//!
//! The direction that satisfies "the fragment cannot arrive without its tools" is therefore this
//! one: the single place that already turns capabilities into a tool surface also turns them into
//! prompt sections. [`CapabilityPlan::static_prompt_sections`] is what makes it possible before a run
//! exists, and it is the same rule the runtime applies to a host's own capabilities.
//!
//! `compaction` is installed on the run configuration by
//! [`CodingHost::build_run_config`](crate::host::CodingHost::build_run_config), and it is the case
//! that shows the split is about tools rather than about ownership: it contributes no tool and no
//! prefix text, so it has nothing to declare twice and nothing to place.
//!
//! # What the split costs: deferred fragments from a tool-bearing capability
//!
//! [`Capability::deferred_instructions`](ra_core::capability::Capability::deferred_instructions) is
//! delivered by the turn loop, from the capabilities installed on the run — so a capability that
//! arrives here instead has no deferred channel. The four below contribute none, and the split
//! costs nothing today; a capability installed on the run configuration, like `compaction`, has the
//! channel in full, which is the route for a heavy section that ships no tools of its own.
//!
//! It becomes a real limit the first time this product installs a capability that has both tools
//! and a heavy fragment — a browser, most likely. Resolving it then is the right time, because the
//! resolution is a choice between things that only that capability can weigh: installing it on the
//! run as well means teaching assembly to tell "the agent already declares this capability's tools"
//! apart from "the agent declares a colliding tool of its own", and that distinction is the one
//! thing the current collision check exists to make.

use std::{collections::BTreeSet, sync::Arc};

use ra_core::{
    capability::{Capability, CapabilityFamily},
    error::{Error, Result},
    permission::PermissionScope,
    prompt::{PromptRole, PromptSection},
    tool::{Tool, ToolLookupKey},
};
use ra_runtime::{capability::CapabilityPlan, tool::profile::ToolSurface};

use crate::{agent::InstalledCapability, host::CodingHost};

/// What one role installs, and what being that role costs it.
///
/// Both halves are answers this module gives, not inputs a caller supplies: which capabilities a
/// role gets is decided here, and so is which ones it gives up.
pub(crate) struct RoleCapabilities {
    installed: CapabilityPlan,
    withheld: Vec<Arc<dyn Capability>>,
}

impl RoleCapabilities {
    /// Resolves the tool-bearing capabilities a coding agent installs for one role.
    ///
    /// The order the plan is built in is the order the tools reach the registry: discovery first,
    /// then the editing entry, then execution. Nothing here declares a dependency, so resolution
    /// keeps it.
    ///
    /// **A one-off role installs nothing, whatever host is handed in.** Its role text says it
    /// answers without tool execution, and installing a capability anyway would put dispatchable
    /// entries behind a prompt that denies they exist. It withholds all four rather than never
    /// naming them: the profile has to be told that an empty surface is this role's shape and not a
    /// tier that lost every entry it declared.
    ///
    /// # Errors
    ///
    /// Propagates tool construction failures, and any incoherence the plan finds in the installed
    /// set — two capabilities claiming one family, a dependency nothing installs, or a dependency
    /// cycle.
    pub(crate) fn resolve(role: &PromptRole, host: &CodingHost) -> Result<Self> {
        let declared: Vec<Arc<dyn Capability>> = vec![
            Arc::new(host.filesystem_capability()?),
            Arc::new(host.search_capability()?),
            Arc::new(host.apply_patch_capability()?),
            Arc::new(host.shell_capability()?),
        ];

        let (installed, withheld) = if role.is_one_off() {
            (Vec::new(), declared)
        } else if role.is_read_only() {
            declared.into_iter().partition(only_observes)
        } else {
            (declared, Vec::new())
        };

        Ok(Self {
            installed: CapabilityPlan::resolve(installed)?,
            withheld,
        })
    }

    /// The tools the installed set contributes, in assembly order.
    pub(crate) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.installed
            .capabilities()
            .iter()
            .flat_map(|capability| capability.tools())
            .collect()
    }

    /// The routing identities this role gave up, for a profile to discount.
    ///
    /// Keys rather than names: a profile selects by routing identity, and the model-facing name a
    /// tool projects to is allowed to differ from it.
    pub(crate) fn withheld_tool_keys(&self) -> BTreeSet<ToolLookupKey> {
        self.withheld
            .iter()
            .flat_map(|capability| capability.tools())
            .map(|tool| tool.origin().lookup_key().clone())
            .collect()
    }

    /// The withheld routing identities that would have occupied an advertised-budget slot.
    ///
    /// Profile selection removes every withheld tool, including host-only ones. Its entry-count
    /// budget measures only tools exposed to a model, so only this subset may lower its bounds.
    pub(crate) fn withheld_advertised_tool_keys(&self) -> BTreeSet<ToolLookupKey> {
        self.withheld
            .iter()
            .flat_map(|capability| capability.tools())
            .filter(|tool| tool.options().is_advertised_to_model())
            .map(|tool| tool.origin().lookup_key().clone())
            .collect()
    }

    /// The families this role gave up, in the order they were declared.
    ///
    /// Families rather than tool names, because that is the unit the role decided on: a read-only
    /// agent did not decline three entries, it declined the capabilities those entries belong to.
    pub(crate) fn withheld_families(&self) -> Vec<CapabilityFamily> {
        self.withheld
            .iter()
            .map(|capability| capability.kind())
            .collect()
    }

    /// What the installed capabilities contribute to one assembled surface: fragments and entries.
    ///
    /// A role decides which capabilities are installed; a tier then decides which of their entries
    /// the request actually carries. This is where the second decision reaches the prompt: a
    /// fragment is kept only when every entry it can speak for survived into the advertised
    /// surface. The alternative is the failure the whole path exists to prevent — a paragraph
    /// telling the model how `grep` returns its matches, in a request that carries no `grep`.
    ///
    /// A family the tier dropped entirely is dropped silently, because that is the tier saying so:
    /// the surface's own floor is what catches a tier that lost entries it meant to keep, and
    /// re-reporting it here would name the prompt for a decision the profile made.
    ///
    /// A family the tier dropped *in part* is refused. A capability is atomic — that is the whole
    /// claim of the type — and its fragment describes the family as one thing, so half a family
    /// leaves text naming an entry the request does not carry with no way to tell which half the
    /// text meant.
    ///
    /// A capability with nothing advertised at all keeps its fragment. Not every capability speaks
    /// for tools; one that contributes none has nothing for a tier to drop, and gating it on an
    /// empty intersection would silence exactly the fragments that are pure policy.
    ///
    /// **The entry attribution falls out of the same walk rather than being asked for separately.**
    /// Which family produced `write_stdin` is knowable only where the installed set meets the
    /// assembled surface, and a second walk asking the tools to project their names again could
    /// answer differently from the one the fragments were filtered against — an integration is
    /// allowed to be mutable, and a report is exactly where the two readings would be presented as
    /// one fact.
    ///
    /// # Errors
    ///
    /// Propagates fragment resolution and the section checks
    /// [`CapabilityPlan::static_prompt_sections`] performs, and refuses a tier that advertises part of a
    /// capability's entries.
    pub(crate) async fn contributions_for(
        &self,
        surface: &ToolSurface,
    ) -> Result<(Vec<PromptSection>, Vec<InstalledCapability>)> {
        let charged: BTreeSet<&str> = surface.advertised_names().collect();
        let mut fragments = self.installed.static_prompt_sections().await?;
        let mut kept = Vec::new();
        let mut installed = Vec::new();

        for capability in self.installed.capabilities() {
            let (present, absent) = split_advertised(capability, &charged);
            // Located by provenance rather than by position: a capability that contributes no
            // fragment leaves no row, so pairing by index would attribute every later fragment to
            // the wrong family. Resolution already refused a fragment whose source is not its own
            // family's, so the match is exact.
            let source = capability.kind().prompt_source();
            if let Some(index) = fragments
                .iter()
                .position(|fragment| fragment.source() == &source)
            {
                let fragment = fragments.remove(index);
                if absent.is_empty() {
                    kept.push(fragment);
                } else if !present.is_empty() {
                    return Err(Error::config(format!(
                        "the tool surface `{}` advertises {} of capability `{}` but not {}, and \
                         the capability's prompt fragment `{}` describes them as one mechanism; a \
                         prefix built from it would name an entry the request does not carry. \
                         Select the family's entries together, or leave all of them out",
                        surface.profile(),
                        render_names(&present),
                        capability.kind(),
                        render_names(&absent),
                        fragment.name()
                    )));
                }
            }
            installed.push(InstalledCapability::new(capability.kind(), present));
        }

        if let Some(orphan) = fragments.first() {
            return Err(Error::caller(format!(
                "prompt fragment `{}` is attributed to `{}`, which is not among the installed \
                 capabilities",
                orphan.name(),
                orphan.source()
            )));
        }
        Ok((kept, installed))
    }
}

/// One capability's advertised entries, split by whether the assembled surface carries them.
///
/// Read once and used for both answers a caller needs — which fragments survive, and which entries
/// each family put on the surface — because the projection it reads is allowed to change between
/// calls.
///
/// Sorted by model-facing name, the order the surface and the prompt inventory both use. A
/// capability is free to construct its tools in whatever order suits it, and a report or an error
/// that echoed that order would present a host's internal arrangement as a fact about the surface.
fn split_advertised(
    capability: &Arc<dyn Capability>,
    charged: &BTreeSet<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut names: Vec<String> = capability
        .tools()
        .iter()
        .filter(|tool| tool.options().is_advertised_to_model())
        .map(|tool| tool.model_definition().name().to_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .partition(|name| charged.contains(name.as_str()))
}

/// Renders one side of a partial selection, for an error that has to name both.
fn render_names(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether installing this capability adds nothing that can change the workspace.
///
/// A capability that contributes no tool passes: there is nothing in it to withhold from a
/// read-only agent.
fn only_observes(capability: &Arc<dyn Capability>) -> bool {
    capability
        .tools()
        .iter()
        .all(|tool| tool.options().permission_scope() == PermissionScope::Read)
}
