//! Construction of the coding agent's declaration.
//!
//! This is where the assembled prompt meets the agent, and it is the only place the two can meet.
//! `AgentSpec` lives in the kernel and takes instructions as text; the assembler lives in a service
//! crate the kernel cannot see. Whoever builds the agent is therefore the one component able to
//! ensure the text in `system_instructions` is an assembled prefix rather than an unstructured
//! string — with a known section order, a prefix hash, and a dump the review process can diff.
//!
//! It is also where the tool profile becomes a single switch. The tools an agent declares, the
//! inventory its prompt names, and the fragments each installed capability contributes are all read
//! off one [`HostBackedSurface`], so there is no arrangement of this path in which a tier is
//! changed on one of them and not the others.

use std::sync::Arc;

use ra_core::agent::{AgentId, AgentSpec};
use ra_core::error::Result;
use ra_core::prompt::{PromptRole, PromptSection};
use ra_runtime::tool::{profile::ToolSurface, registry::ToolRegistry};

use crate::{
    capabilities::RoleCapabilities,
    host::CodingHost,
    profile::CodingProfile,
    prompt::{assemble_stable_prefix, assemble_stable_prefix_for_surface},
};

/// The tier a host-backed agent assembles when the caller does not name one.
///
/// Deliberately not [`CodingProfile::default`]. The default tier is `codex_like`, and nine of its
/// fifteen entries have no implementation yet, so assembling it fails — which is the behaviour that
/// tier is there for: the list is the specification, and a surface quietly smaller than the prompt
/// describing it is the outcome the failure prevents. `core` is the widest tier that assembles from
/// what exists today, and this constant is what moves when that stops being true.
pub const HOST_BACKED_PROFILE: CodingProfile = CodingProfile::Core;

/// Builds the coding agent's declaration with an assembled stable prefix.
///
/// # Errors
///
/// Propagates prompt assembly failures and agent validation failures.
pub fn build_agent(
    id: AgentId,
    name: impl Into<String>,
    role: &PromptRole,
) -> Result<Arc<AgentSpec>> {
    let prefix = assemble_stable_prefix(role)?;
    AgentSpec::builder()
        .id(id)
        .name(name)
        .instructions(prefix.system_instructions())
        .build()
}

/// Builds the coding agent on the [`HOST_BACKED_PROFILE`] tier, narrowed by its role.
///
/// # Errors
///
/// Propagates the failures described on [`build_agent_with_profile`].
pub async fn build_agent_with_host(
    id: AgentId,
    name: impl Into<String> + Send,
    role: &PromptRole,
    host: &CodingHost,
) -> Result<Arc<AgentSpec>> {
    build_agent_with_profile(id, name, role, host, HOST_BACKED_PROFILE).await
}

/// Builds the coding agent with the surface one tier gives this role, and the prompt that names it.
///
/// The tools, the inventory, and the capability fragments come from the same
/// [`HostBackedSurface`]: what [`AgentSpec::builder`]`.tools()` receives is what the prefix lists,
/// entry for entry, and the paragraphs explaining those entries came from the capabilities that
/// contributed them. A tier the registry cannot satisfy fails here rather than shipping an agent
/// whose prompt describes tools the provider was never sent.
///
/// # Errors
///
/// Propagates tool construction, registry, and profile-assembly failures — including a tier naming
/// an entry no capability provides — plus fragment resolution, prompt assembly, and agent
/// validation failures.
pub async fn build_agent_with_profile(
    id: AgentId,
    name: impl Into<String> + Send,
    role: &PromptRole,
    host: &CodingHost,
    profile: CodingProfile,
) -> Result<Arc<AgentSpec>> {
    let assembled = host_backed_surface(role, host, profile).await?;
    let prefix = assemble_stable_prefix_for_surface(
        role,
        assembled.tool_surface(),
        assembled.capability_sections(),
    )?;
    AgentSpec::builder()
        .id(id)
        .name(name)
        .instructions(prefix.system_instructions())
        .tools(assembled.into_tool_surface().into_tools())
        .build()
}

/// What one role installs at one tier: the entries, and the prompt text that describes them.
///
/// The two halves travel together because they were produced together. Handing back only the
/// surface would let a caller assemble a prefix beside it from some other reading of the same
/// capabilities, which is the arrangement this whole path exists to remove.
#[must_use]
#[non_exhaustive]
#[derive(Debug)]
pub struct HostBackedSurface {
    tool_surface: ToolSurface,
    capability_sections: Vec<PromptSection>,
}

impl HostBackedSurface {
    /// The bounded, ordered entries this role and tier advertise.
    #[must_use]
    pub const fn tool_surface(&self) -> &ToolSurface {
        &self.tool_surface
    }

    /// The fragments the installed capabilities contribute, already reconciled with the surface.
    #[must_use]
    pub fn capability_sections(&self) -> &[PromptSection] {
        &self.capability_sections
    }

    /// Takes the surface, for the caller that installs its tools on an agent.
    #[must_use]
    pub fn into_tool_surface(self) -> ToolSurface {
        self.tool_surface
    }
}

/// Assembles what a host-backed coding agent gets for one role and one tier.
///
/// Public so the prompt dump and a host inspecting its own configuration report what
/// [`build_agent_with_profile`] installs rather than composing a second answer beside it — the
/// report exists to be trusted about exactly this.
///
/// Three decisions meet here and each belongs to a different owner. Which capabilities the role
/// installs is the capability composition layer's answer; which of the registered entries this
/// product advertises and what they may cost is the [`CodingProfile`] tier's; and turning the two
/// into one bounded, ordered surface is the framework's. The tools land in lookup-key order
/// rather than the order the capabilities contributed them, which is what keeps two hosts that
/// install the same capabilities in different orders on the same cached prefix.
///
/// The fragments are collected after the surface rather than beside it, because whether a fragment
/// belongs in the prefix is a question about the assembled surface: a tier that dropped a family
/// dropped the paragraph describing it too.
///
/// # Errors
///
/// Propagates tool construction failures, capability incoherence, duplicate registrations, any way
/// the assembled surface falls outside the tier's declared bounds, and a tier that advertises part
/// of a capability's entries.
pub async fn host_backed_surface(
    role: &PromptRole,
    host: &CodingHost,
    profile: CodingProfile,
) -> Result<HostBackedSurface> {
    let capabilities = RoleCapabilities::resolve(role, host)?;
    let registry = ToolRegistry::builder()
        .register_all(capabilities.tools())
        .build()?;
    let tool_surface = registry.assemble(&profile.to_tool_profile_for_role(
        role,
        &capabilities.withheld_tool_keys(),
        &capabilities.withheld_advertised_tool_keys(),
    )?)?;
    let capability_sections = capabilities.prompt_sections_for(&tool_surface).await?;
    Ok(HostBackedSurface {
        tool_surface,
        capability_sections,
    })
}
