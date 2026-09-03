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
//! Today it is read for its tools, by the agent construction path that also assembles the prompt
//! naming them — through a registry and a profile, so one switch moves both. It is not additionally
//! installed on [`RunConfig`](ra_runtime::runner::RunConfig), because a capability contributes its
//! tools at run assembly and the agent already declares them — installing both would declare one
//! tool twice and fail the build. Moving installation to the run configuration is a later step, and
//! it has to move together with the prompt fragments: an agent whose tools arrive at assembly needs
//! its inventory section to arrive the same way, or the prefix describes a surface the request does
//! not carry.
//!
//! `compaction` is the exception and is installed on the run configuration by
//! [`CodingHost::build_run_config`](crate::host::CodingHost::build_run_config). It contributes no
//! tool, so it has nothing to declare twice, and it is role-independent.

use std::{collections::BTreeSet, sync::Arc};

use ra_core::{
    capability::Capability,
    error::Result,
    permission::PermissionScope,
    prompt::PromptRole,
    tool::{Tool, ToolLookupKey},
};
use ra_runtime::capability::CapabilityPlan;

use crate::host::CodingHost;

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
