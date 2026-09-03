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
//! # Where the resulting set goes
//!
//! Today it is read for its tools, by the agent construction path that also assembles the prompt
//! naming them. It is not additionally installed on [`RunConfig`](ra_runtime::runner::RunConfig),
//! because a capability contributes its tools at run assembly and the agent already declares them —
//! installing both would declare one tool twice and fail the build. Moving installation to the run
//! configuration is a later step, and it has to move together with the prompt fragments: an agent
//! whose tools arrive at assembly needs its inventory section to arrive the same way, or the prefix
//! describes a surface the request does not carry.
//!
//! `compaction` is the exception and is installed on the run configuration by
//! [`CodingHost::build_run_config`](crate::host::CodingHost::build_run_config). It contributes no
//! tool, so it has nothing to declare twice, and it is role-independent.

use std::sync::Arc;

use ra_core::{
    capability::Capability, error::Result, permission::PermissionScope, prompt::PromptRole,
    tool::Tool,
};
use ra_runtime::capability::CapabilityPlan;

use crate::host::CodingHost;

/// Resolves the tool-bearing capabilities a coding agent installs for one role.
///
/// The order the plan is built in is the order the tools reach the agent: discovery first, then the
/// editing entry, then execution. Nothing here declares a dependency, so resolution keeps it.
///
/// **A one-off role installs nothing, whatever host is handed in.** Its role text says it answers
/// without tool execution, and installing a capability anyway would put dispatchable entries behind
/// a prompt that denies they exist. The host is still accepted rather than refused: which
/// capabilities a role gets is this function's answer to give, not the caller's to pre-compute.
///
/// # Errors
///
/// Propagates tool construction failures, and any incoherence the plan finds in the installed set —
/// two capabilities claiming one family, a dependency nothing installs, or a dependency cycle.
pub(crate) fn tool_capabilities(role: &PromptRole, host: &CodingHost) -> Result<CapabilityPlan> {
    if role.is_one_off() {
        return CapabilityPlan::resolve(Vec::new());
    }

    let installed: Vec<Arc<dyn Capability>> = vec![
        Arc::new(host.filesystem_capability()?),
        Arc::new(host.search_capability()?),
        Arc::new(host.apply_patch_capability()?),
        Arc::new(host.shell_capability()?),
    ];
    if role.is_read_only() {
        return CapabilityPlan::resolve(installed.into_iter().filter(only_observes));
    }
    CapabilityPlan::resolve(installed)
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

/// The tools an installed set contributes, in assembly order.
pub(crate) fn contributed_tools(plan: &CapabilityPlan) -> Vec<Arc<dyn Tool>> {
    plan.capabilities()
        .iter()
        .flat_map(|capability| capability.tools())
        .collect()
}
