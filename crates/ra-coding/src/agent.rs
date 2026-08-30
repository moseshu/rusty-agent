//! Construction of the coding agent's declaration.
//!
//! This is where the assembled prompt meets the agent, and it is the only place the two can meet.
//! `AgentSpec` lives in the kernel and takes instructions as text; the assembler lives in a service
//! crate the kernel cannot see. Whoever builds the agent is therefore the one component able to
//! ensure the text in `system_instructions` is an assembled prefix rather than an unstructured
//! string — with a known section order, a prefix hash, and a dump the review process can diff.

use std::sync::Arc;

use ra_core::agent::{AgentId, AgentSpec};
use ra_core::error::Result;
use ra_core::prompt::PromptRole;

use crate::{
    host::CodingHost,
    prompt::{assemble_stable_prefix, assemble_stable_prefix_for_tools},
};

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

/// Builds the coding agent with the host-backed tools its role permits.
///
/// For a role that may edit, this deliberately installs only `apply_patch`: the complete profile
/// remains unavailable until the other declared core tools exist, and pretending the incomplete set
/// were a profile would make the prompt advertise capabilities the runtime cannot dispatch.
pub fn build_agent_with_host(
    id: AgentId,
    name: impl Into<String>,
    role: &PromptRole,
    host: &CodingHost,
) -> Result<Arc<AgentSpec>> {
    let tools = host_backed_tools(role, host)?;
    let prefix = assemble_stable_prefix_for_tools(role, &tools)?;
    AgentSpec::builder()
        .id(id)
        .name(name)
        .instructions(prefix.system_instructions())
        .tools(tools)
        .build()
}

/// The tool entries a host-backed coding agent installs for one role.
///
/// Extracted so the prompt dump reports the surface [`build_agent_with_host`] actually installs.
/// A dump that assembled its own list would be a report about a different agent the moment the two
/// lists diverged — and the report exists to be trusted about exactly this.
///
/// **A read-only or one-off role gets nothing, whatever host is handed in.** Its role text says it
/// has no editing tools and that an edit will fail; installing one anyway would put a dispatchable
/// entry behind a prompt that denies it exists, and the `editing` section — which is assembled from
/// the advertised surface — would then name an entry the same prefix goes on to disown. The host is
/// still accepted for these roles rather than refused: which capabilities a role gets is this
/// function's answer to give, not the caller's to pre-compute.
pub(crate) fn host_backed_tools(
    role: &PromptRole,
    host: &CodingHost,
) -> Result<Vec<Arc<dyn ra_core::tool::Tool>>> {
    if role.is_read_only() || role.is_one_off() {
        return Ok(Vec::new());
    }
    Ok(vec![host.apply_patch_tool()?])
}
