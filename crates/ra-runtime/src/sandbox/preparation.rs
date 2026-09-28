//! Turning a sandbox agent and its session into the agent instance that executes.
//!
//! Ported from the reference's `sandbox/runtime_agent_preparation.py`: capabilities are checked
//! for their dependencies, contribute their tools after the agent's own and fold over its model
//! settings in installation order, and the prompt is assembled as
//!
//! ```text
//! base prompt                          (the built-in one unless the agent replaces it)
//! # Agent instructions                 (the agent's own, when it has any)
//! # Sandbox capability instructions    (each capability's fragment, in order)
//! # Sandbox remote mount policy        (when the manifest mounts remote storage)
//! # Filesystem                         (the workspace tree, and the run's working directory)
//! ```
//!
//! # Where the assembled prompt lands
//!
//! The reference hands the model this text as its instructions whichever way it was produced. Here
//! static text and generated text land in different places — the cached prefix and the volatile
//! tail — so the assembly keeps the distinction: when the base prompt and the agent's own
//! instructions are both static, the whole text is resolved once and is static; when either is
//! generated per turn, the whole text is generated per turn, in the same order, and lands where
//! generated instructions land. Capability fragments are resolved once either way, which is the
//! contract [`Capability::instructions`] states.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentInstructions, AgentSpec, ResolvedInstructions},
    capability::{Capability, SamplingContext},
    context::RunContext,
    error::{Error, Result},
    prompt::{DynamicPromptHandler, PromptSource, ResolvedPrompt},
    sandbox::{
        ExecRequest, Manifest, PosixPath, SandboxAgentConfig, SandboxSession,
        SandboxWorkspaceScope, SessionPath, ShellInvocation, User,
        build_remote_mount_policy_instructions,
    },
};

use super::sandbox_error;

/// The base sandbox prompt an agent gets unless it replaces it.
///
/// The reference's `sandbox/instructions/prompt.md` (MIT), carried verbatim: it is what the model
/// reads, and a paraphrase would be a different instruction.
pub const DEFAULT_SANDBOX_INSTRUCTIONS: &str = include_str!("prompt.md");

/// How deep the workspace tree in the filesystem section goes.
const FILESYSTEM_TREE_DEPTH: usize = 3;

/// The filesystem section: the workspace tree, and where relative paths are measured from.
///
/// # Errors
///
/// Returns the manifest's failure to describe itself.
pub fn filesystem_instructions(
    manifest: &Manifest,
    workspace_scope: &SandboxWorkspaceScope,
) -> Result<String> {
    let mut header = "# Filesystem\nYou have access to a container with a filesystem. The \
                      filesystem layout is:"
        .to_owned();
    if let Some(cwd) = workspace_scope.cwd() {
        let workspace_root = PosixPath::coerce(&manifest.root);
        let working_directory = workspace_root.join(cwd.as_str());
        for line in [
            format!(
                "For this run, the working directory is `{}`.",
                working_directory.as_str()
            ),
            "Relative paths passed to the built-in `exec_command`, `view_image`, and \
             `apply_patch` tools resolve from this directory."
                .to_owned(),
            "Other sandbox tools follow their own path contract.".to_owned(),
            format!(
                "The session workspace root remains `{}`.",
                workspace_root.as_str()
            ),
            "The working directory changes path resolution; it does not isolate this run from the \
             rest of the session workspace."
                .to_owned(),
            "Files outside the working directory may be visible to or shared with other runs."
                .to_owned(),
        ] {
            header.push('\n');
            header.push_str(&line);
        }
    }
    let tree = manifest
        .describe(Some(FILESYSTEM_TREE_DEPTH))
        .map_err(sandbox_error)?;
    Ok(format!("{header}\n\n{}", tree.trim()))
}

/// The prompt text that follows the base prompt and the agent's own instructions.
///
/// Resolved once per preparation: capability fragments, the remote mount policy and the
/// filesystem section do not depend on the turn.
async fn sandbox_sections(
    capabilities: &[Arc<dyn Capability>],
    manifest: &Manifest,
    workspace_scope: &SandboxWorkspaceScope,
) -> Result<Vec<String>> {
    let mut sections = Vec::new();
    let mut fragments = Vec::new();
    for capability in capabilities {
        if let Some(section) = capability.instructions().await? {
            let text = section.content();
            if !text.is_empty() {
                fragments.push(text.to_owned());
            }
        }
    }
    if !fragments.is_empty() {
        sections.push(instruction_section(
            "Sandbox capability instructions",
            &fragments.join("\n\n"),
        ));
    }
    if let Some(policy) = build_remote_mount_policy_instructions(manifest).map_err(sandbox_error)? {
        sections.push(instruction_section("Sandbox remote mount policy", &policy));
    }
    sections.push(filesystem_instructions(manifest, workspace_scope)?);
    Ok(sections)
}

fn instruction_section(title: &str, body: &str) -> String {
    format!("# {title}\n\n{body}")
}

/// Puts the parts in order, dropping empty ones as the reference does.
fn assemble(base: Option<&str>, agent: Option<&str>, sections: &[String]) -> String {
    let mut parts = Vec::new();
    if let Some(base) = base.filter(|base| !base.is_empty()) {
        parts.push(base.to_owned());
    }
    if let Some(agent) = agent.filter(|agent| !agent.is_empty()) {
        parts.push(instruction_section("Agent instructions", agent));
    }
    parts.extend(sections.iter().cloned());
    parts.join("\n\n")
}

/// The base prompt text, resolved against the run when it is generated.
async fn resolve_text(instructions: &AgentInstructions, context: &RunContext) -> Result<String> {
    Ok(match instructions.resolve(context).await? {
        ResolvedInstructions::Prefix(text) => text,
        ResolvedInstructions::Generated(prompt) => prompt.text().to_owned(),
    })
}

/// Generates the assembled prompt per turn, for an agent whose base or own instructions are
/// generated.
struct GeneratedSandboxInstructions {
    base: Option<AgentInstructions>,
    agent: Option<AgentInstructions>,
    sections: Vec<String>,
}

#[async_trait]
impl DynamicPromptHandler for GeneratedSandboxInstructions {
    async fn resolve(&self, context: &RunContext) -> Result<ResolvedPrompt> {
        let base = match &self.base {
            None => DEFAULT_SANDBOX_INSTRUCTIONS.trim().to_owned(),
            Some(base) => resolve_text(base, context).await?,
        };
        let agent = match &self.agent {
            None => None,
            Some(agent) => Some(resolve_text(agent, context).await?),
        };
        Ok(ResolvedPrompt::new(
            assemble(Some(&base), agent.as_deref(), &self.sections),
            PromptSource::Dynamic("sandbox".to_owned()),
        ))
    }
}

/// The instructions a prepared sandbox agent runs with.
async fn build_sandbox_instructions(
    base: Option<&AgentInstructions>,
    agent: Option<&AgentInstructions>,
    capabilities: &[Arc<dyn Capability>],
    manifest: &Manifest,
    workspace_scope: &SandboxWorkspaceScope,
) -> Result<AgentInstructions> {
    let sections = sandbox_sections(capabilities, manifest, workspace_scope).await?;
    let base_text = match base {
        None => Some(DEFAULT_SANDBOX_INSTRUCTIONS.trim()),
        Some(base) => base.as_static(),
    };
    let agent_text = agent.map(AgentInstructions::as_static);
    match (base_text, agent_text) {
        (Some(base), None) => Ok(AgentInstructions::static_text(assemble(
            Some(base),
            None,
            &sections,
        ))),
        (Some(base), Some(Some(agent))) => Ok(AgentInstructions::static_text(assemble(
            Some(base),
            Some(agent),
            &sections,
        ))),
        _ => Ok(AgentInstructions::dynamic(Arc::new(
            GeneratedSandboxInstructions {
                base: base.cloned(),
                agent: agent.cloned(),
                sections,
            },
        ))),
    }
}

/// Binds each capability to the session, keeping the installed value where it needs no binding.
pub(super) fn bind_capabilities(
    capabilities: &[Arc<dyn Capability>],
    binding: &ra_core::capability::SandboxBinding,
) -> Result<Vec<Arc<dyn Capability>>> {
    capabilities
        .iter()
        .map(|capability| {
            Ok(capability
                .bind_sandbox(binding)?
                .unwrap_or_else(|| Arc::clone(capability)))
        })
        .collect()
}

/// The instance a sandbox agent executes as.
///
/// Built from `base`, the instance the run would otherwise execute — the public agent, or what
/// run-level capability assembly made of it — so neither preparation undoes the other.
pub(super) async fn prepare_sandbox_agent(
    base: &AgentSpec,
    sandbox: &SandboxAgentConfig,
    capabilities: &[Arc<dyn Capability>],
    manifest: &Manifest,
    workspace_scope: &SandboxWorkspaceScope,
    sampling: &SamplingContext,
) -> Result<Arc<AgentSpec>> {
    let available: BTreeSet<_> = capabilities
        .iter()
        .map(|capability| capability.kind())
        .collect();
    for capability in capabilities {
        let missing: Vec<String> = capability
            .required_capabilities()
            .difference(&available)
            .map(ToString::to_string)
            .collect();
        if !missing.is_empty() {
            return Err(Error::config(format!(
                "capability `{}` requires missing capabilities: {}",
                capability.kind(),
                missing.join(", ")
            )));
        }
    }

    let tools: Vec<_> = capabilities
        .iter()
        .flat_map(|capability| capability.tools())
        .collect();
    let settings = capabilities
        .iter()
        .fold(base.model_settings().clone(), |settings, capability| {
            capability.sampling_params_for(settings, sampling)
        });
    let instructions = build_sandbox_instructions(
        sandbox.base_instructions(),
        base.instructions(),
        capabilities,
        manifest,
        workspace_scope,
    )
    .await?;

    base.to_builder()
        .with_instructions(instructions)
        .model_settings(settings)
        .tools(tools)
        .build()
        .map_err(|error| error.with_context(format!("preparing sandbox agent `{}`", base.id())))
}

/// Checks that the run's working directory exists and is reachable by the agent's user.
///
/// Asked of the session itself, as the agent's user, so the answer is the one the agent's commands
/// will get.
///
/// # Errors
///
/// Returns a configuration error naming the directory when it is missing or unreachable.
pub(super) async fn validate_workspace_scope(
    session: &dyn SandboxSession,
    scope: &SandboxWorkspaceScope,
    run_as: Option<&User>,
) -> Result<()> {
    let Some(cwd) = scope.cwd() else {
        return Ok(());
    };
    let anchored = scope.anchor(".");
    let resolved = session
        .validate_path_access(SessionPath::Text(&anchored), false)
        .await
        .map_err(sandbox_error)?;
    for flag in ["-d", "-x"] {
        let mut request = ExecRequest::new([
            "test".to_owned(),
            flag.to_owned(),
            resolved.as_str().to_owned(),
        ])
        .with_shell(ShellInvocation::None);
        if let Some(user) = run_as {
            request = request.as_user(user.clone());
        }
        let result = session.exec(request).await.map_err(sandbox_error)?;
        if !result.ok() {
            return Err(Error::config(format!(
                "Sandbox working directory `{}` does not exist or is not accessible for the \
                 configured sandbox user",
                cwd.as_str()
            )));
        }
    }
    Ok(())
}
