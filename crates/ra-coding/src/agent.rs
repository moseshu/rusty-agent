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
use ra_core::capability::{Capability, CapabilityFamily};
use ra_core::error::{Error, Result};
use ra_core::prompt::{PromptRole, PromptSection};
use ra_runtime::{
    agent::control::AgentControl,
    runner::{RunConfig, RunRequest},
    tool::{
        profile::{ToolSurface, ToolSurfaceBudget},
        registry::ToolRegistry,
    },
};

use crate::{
    capabilities::{COLLABORATION_FAMILY, RoleCapabilities},
    host::CodingHost,
    profile::CodingProfile,
    prompt::{assemble_stable_prefix, assemble_stable_prefix_for_surface},
};

/// The tier a host-backed agent assembles when the caller does not name one.
///
/// Deliberately not [`CodingProfile::default`]: the standard tier requires web, skills, user input
/// and collaboration services. A minimal host has only its workspace and process manager, so it
/// starts on Core. Selecting the standard tier without its services fails during assembly.
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
/// An agent offered the collaboration tools is refused here: those tools act on an agent tree the
/// caller has to own and bind every run to. Build it with [`build_agent_with_control`] instead,
/// which returns the tree along with the agent.
///
/// # Errors
///
/// Propagates tool construction, registry, and profile-assembly failures — including a tier naming
/// an entry no capability provides — plus fragment resolution, prompt assembly, and agent
/// validation failures, and refuses an agent offered the collaboration tools.
pub async fn build_agent_with_profile(
    id: AgentId,
    name: impl Into<String> + Send,
    role: &PromptRole,
    host: &CodingHost,
    profile: CodingProfile,
) -> Result<Arc<AgentSpec>> {
    if !host.capabilities().is_empty() {
        return Err(Error::config(
            "a coding host with additional capabilities must be built with \
             `build_agent_with_control` so its runtime contributions travel with the agent",
        ));
    }
    let assembled = assemble_agent(id, name, role, host, profile).await?;
    if assembled.collaborates {
        return Err(Error::config(
            "the assembled coding agent is offered the collaboration tools, which act on an agent \
             tree its runs must be bound to; build it with `build_agent_with_control`, which \
             returns that tree",
        ));
    }
    Ok(assembled.agent)
}

/// Builds the coding agent as [`build_agent_with_profile`] does, together with the agent tree its
/// collaboration tools act on when the role and tier offer them.
///
/// The tree prepares every agent it spawns from `host` again, with a process manager of its own,
/// as Codex starts each spawned agent as a session of its own. The caller owns the tree: it binds
/// each run to it with [`StandaloneCodingAgent::bind`], or hands it to an
/// [`AgentThreadBuilder`](ra_runtime::thread::AgentThreadBuilder), and shuts it down when done.
///
/// # Errors
///
/// Propagates the failures described on [`build_agent_with_profile`], other than its refusal of
/// the collaboration tools.
pub async fn build_agent_with_control(
    id: AgentId,
    name: impl Into<String> + Send,
    role: &PromptRole,
    host: &CodingHost,
    profile: CodingProfile,
) -> Result<StandaloneCodingAgent> {
    let name = name.into();
    let assembled = assemble_agent(id.clone(), name.clone(), role, host, profile).await?;
    let control = if assembled.collaborates {
        Some(crate::spawned::agent_control(
            host, id, name, role, profile,
        )?)
    } else {
        None
    };
    Ok(StandaloneCodingAgent {
        agent: assembled.agent,
        control,
        capabilities: assembled.capabilities,
        capability_tools: assembled.capability_tools,
    })
}

/// A coding agent built for standalone runs, with the agent tree its collaboration tools act on.
pub struct StandaloneCodingAgent {
    agent: Arc<AgentSpec>,
    control: Option<AgentControl>,
    capabilities: Vec<Arc<dyn Capability>>,
    capability_tools: Vec<Arc<dyn ra_core::tool::Tool>>,
}

impl std::fmt::Debug for StandaloneCodingAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StandaloneCodingAgent")
            .field("agent", &self.agent)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

impl StandaloneCodingAgent {
    /// The agent's declaration.
    #[must_use]
    pub const fn agent(&self) -> &Arc<AgentSpec> {
        &self.agent
    }

    /// The agent's tree, when the agent is offered the collaboration tools.
    #[must_use]
    pub const fn agent_control(&self) -> Option<&AgentControl> {
        self.control.as_ref()
    }

    /// Installs the selected runtime contributions and binds `request` to the root of the agent's
    /// tree, when it has one.
    pub fn bind(&self, request: RunRequest) -> RunRequest {
        let request = request.with_prepared_capabilities(
            self.capabilities.iter().cloned(),
            self.capability_tools.iter().cloned(),
        );
        match &self.control {
            Some(control) => request.with_agent_control(control.root()),
            None => request,
        }
    }

    /// Appends the runtime contributions selected by this agent's role and tier to `config`.
    /// Use this for an `AgentThread`; standalone requests receive them through [`Self::bind`].
    pub fn configure_run(&self, config: RunConfig) -> RunConfig {
        config.with_prepared_capabilities(
            self.capabilities.iter().cloned(),
            self.capability_tools.iter().cloned(),
        )
    }

    /// The declaration, tree and configured run settings, retaining the selected runtime
    /// contributions for a caller that stores the pieces separately.
    pub fn into_parts(
        self,
        config: RunConfig,
    ) -> (Arc<AgentSpec>, Option<AgentControl>, RunConfig) {
        let config = self.configure_run(config);
        (self.agent, self.control, config)
    }
}

/// A coding agent as assembled, and whether it is offered the collaboration tools.
pub(crate) struct AssembledAgent {
    pub(crate) agent: Arc<AgentSpec>,
    pub(crate) collaborates: bool,
    pub(crate) capabilities: Vec<Arc<dyn Capability>>,
    pub(crate) capability_tools: Vec<Arc<dyn ra_core::tool::Tool>>,
}

/// Assembles the coding agent for one role and tier from `host`.
pub(crate) async fn assemble_agent(
    id: AgentId,
    name: impl Into<String> + Send,
    role: &PromptRole,
    host: &CodingHost,
    profile: CodingProfile,
) -> Result<AssembledAgent> {
    let assembled = host_backed_surface(role, host, profile).await?;
    let prefix = assemble_stable_prefix_for_surface(
        role,
        assembled.tool_surface(),
        assembled.capability_sections(),
    )?;
    // Read off the families installed on the surface rather than a tool's model-facing name,
    // which the host's namespace choice changes.
    let collaborates = assembled.installed().iter().any(|capability| {
        capability.family().as_str() == COLLABORATION_FAMILY && !capability.advertised().is_empty()
    });
    let capabilities = assembled.runtime_capabilities.clone();
    let capability_tools = assembled.tool_surface().tools().to_vec();
    let hosted = assembled.tool_surface().hosted_tools().to_vec();
    let mut builder = AgentSpec::builder()
        .id(id)
        .name(name)
        .instructions(prefix.system_instructions())
        .tools(assembled.into_tool_surface().into_tools());
    for tool in hosted {
        builder = if let Some(config) = host.web_search().filter(|_| tool.name() == "web_search") {
            builder.hosted_tool_resolver(tool, Arc::new(config.clone()))
        } else {
            builder.hosted_tool(tool)
        };
    }
    if let Some(runtime) = host.mcp_runtime() {
        runtime.start_background();
        builder = builder.tool_resolver(runtime.clone());
    }
    Ok(AssembledAgent {
        agent: builder.build()?,
        collaborates,
        capabilities,
        capability_tools,
    })
}

/// What one role installs at one tier: the entries, and the prompt text that describes them.
///
/// The two halves travel together because they were produced together. Handing back only the
/// surface would let a caller assemble a prefix beside it from some other reading of the same
/// capabilities, which is the arrangement this whole path exists to remove.
///
/// It also carries what the assembly *decided*, which nothing else records: which family each
/// advertised entry came from, which families the role gave up, and the budget the result was held
/// to. A surface can be inspected for its entries and a prefix for its sections, but "this entry is
/// here because the shell capability is installed" exists only where the installed set met the
/// assembled surface.
#[must_use]
#[non_exhaustive]
pub struct HostBackedSurface {
    tool_surface: ToolSurface,
    capability_sections: Vec<PromptSection>,
    installed: Vec<InstalledCapability>,
    withheld: Vec<CapabilityFamily>,
    budget: ToolSurfaceBudget,
    runtime_capabilities: Vec<Arc<dyn Capability>>,
}

impl std::fmt::Debug for HostBackedSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostBackedSurface")
            .field("tool_surface", &self.tool_surface)
            .field("capability_sections", &self.capability_sections)
            .field("installed", &self.installed)
            .field("withheld", &self.withheld)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
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

    /// The installed capabilities and the entries each of them put on this surface, in install
    /// order.
    #[must_use]
    pub fn installed(&self) -> &[InstalledCapability] {
        &self.installed
    }

    /// The families this role declined, and with them every entry and fragment they carry.
    #[must_use]
    pub fn withheld_families(&self) -> &[CapabilityFamily] {
        &self.withheld
    }

    /// The bounds this surface was assembled against, already narrowed by the role.
    #[must_use]
    pub const fn budget(&self) -> &ToolSurfaceBudget {
        &self.budget
    }

    /// Appends the runtime half of the capabilities selected by this same surface. Their tools
    /// and static fragments have already been placed; binding, per-run and deferred text,
    /// sampling settings and context processing remain on the run lifecycle.
    pub fn configure_run(&self, config: RunConfig) -> RunConfig {
        config.with_prepared_capabilities(
            self.runtime_capabilities.iter().cloned(),
            self.tool_surface.tools().iter().cloned(),
        )
    }

    /// Takes the surface, for the caller that installs its tools on an agent.
    #[must_use]
    pub fn into_tool_surface(self) -> ToolSurface {
        self.tool_surface
    }
}

/// One installed capability's share of an assembled surface.
///
/// The entries are this family's own, and only the ones the tier kept. A capability whose entries a
/// tier dropped entirely is still installed and still owns its prefix paragraph, so it is present
/// here with nothing advertised rather than absent.
///
/// Tools and static text are placed during construction. The enclosing surface retains the same
/// selection's runtime contributions for [`HostBackedSurface::configure_run`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct InstalledCapability {
    family: CapabilityFamily,
    advertised: Vec<String>,
}

impl InstalledCapability {
    pub(crate) const fn new(family: CapabilityFamily, advertised: Vec<String>) -> Self {
        Self { family, advertised }
    }

    /// Which family this is.
    #[must_use]
    pub const fn family(&self) -> &CapabilityFamily {
        &self.family
    }

    /// The model-facing names this family contributed to the surface, sorted as the inventory
    /// lists them.
    #[must_use]
    pub fn advertised(&self) -> &[String] {
        &self.advertised
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
    let profile = profile.to_tool_profile_for_role(
        role,
        &capabilities.withheld_tool_keys(),
        &capabilities.withheld_advertised_tool_keys(),
        host.collaboration_namespace()?.as_ref(),
    )?;
    let budget = *profile.budget();
    let tool_surface =
        registry.assemble_with_hosted_tools(&profile, &capabilities.hosted_tools()?)?;
    let contributions = capabilities.contributions_for(&tool_surface).await?;
    Ok(HostBackedSurface {
        tool_surface,
        capability_sections: contributions.sections,
        installed: contributions.installed,
        withheld: capabilities.withheld_families(),
        budget,
        runtime_capabilities: contributions.runtime,
    })
}
