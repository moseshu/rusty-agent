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

use crate::{host::CodingHost, prompt::assemble_stable_prefix};

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

/// Builds the coding agent with the product's currently implemented editing tool installed.
///
/// This deliberately installs only `apply_patch`: the complete profile remains unavailable until
/// the other declared core tools exist, and pretending the incomplete set were a profile would
/// make the prompt advertise capabilities the runtime cannot dispatch.
pub fn build_agent_with_host(
    id: AgentId,
    name: impl Into<String>,
    role: &PromptRole,
    host: &CodingHost,
) -> Result<Arc<AgentSpec>> {
    let prefix = assemble_stable_prefix(role)?;
    AgentSpec::builder()
        .id(id)
        .name(name)
        .instructions(prefix.system_instructions())
        .tools(vec![host.apply_patch_tool()?])
        .build()
}
