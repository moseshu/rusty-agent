//! The runs memory generation makes of its own: extraction and consolidation.
//!
//! The reference runs each phase as a `SandboxAgent` with `Runner.run`, under a run configuration
//! whose sandbox is the session being closed. That is [`MemoryRunConfig`]: the session the phase
//! runs borrow, the resolver their named models are resolved with, and the capabilities the phase
//! agents run with. A phase run counts only when it concludes on its own; see
//! [`require_completed`].

use std::fmt;
use std::sync::Arc;

use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    capability::Capability,
    error::{Error, Result},
    item::{Message, ModelInputItem},
    model::{
        ApiProtocol, Model, ModelResolver, ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
    },
    output::OutputSchema,
    sandbox::{MemoryModel, SandboxAgentConfig, SandboxSession},
    state::RunId,
};

use crate::agent::AgentBinding;
use crate::runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner};
use crate::sandbox::SandboxRunConfig;

/// The provider a phase model given as an instance is reported under.
///
/// An instance belongs to no provider registration, so provider-specific settings of the run's
/// providers do not apply to it; the key only names it in traces and lifecycle callbacks.
pub const MODEL_INSTANCE_PROVIDER: &str = "model_instance";

/// What memory generation's own runs are configured with.
///
/// The reference's `_memory_run_config`, a run configuration whose sandbox is the session memory
/// is generated for. A run here also needs a model resolver for phase models given by name, which
/// the reference takes from its global default provider; this is the resolver of the run that
/// first appended to the rollout. A phase model given as an instance is used directly and never
/// resolved.
#[derive(Clone)]
pub struct MemoryRunConfig {
    session: Arc<dyn SandboxSession>,
    resolver: Arc<dyn ModelResolver>,
    capabilities: Vec<Arc<dyn Capability>>,
}

impl MemoryRunConfig {
    /// Runs borrowing `session`, resolving named models with `resolver`, whose agents carry
    /// `capabilities`.
    #[must_use]
    pub fn new(
        session: Arc<dyn SandboxSession>,
        resolver: Arc<dyn ModelResolver>,
        capabilities: Vec<Arc<dyn Capability>>,
    ) -> Self {
        Self {
            session,
            resolver,
            capabilities,
        }
    }

    /// The session the runs borrow.
    #[must_use]
    pub fn session(&self) -> &Arc<dyn SandboxSession> {
        &self.session
    }

    /// Runs one phase agent on `prompt`, as the reference's `Runner.run(agent, prompt, ...)`, and
    /// answers the run once it has concluded.
    pub(super) async fn run(&self, phase: PhaseAgent<'_>, prompt: String) -> Result<RunResult> {
        let mut builder = AgentSpec::builder()
            .id(AgentId::new(phase.name))
            .name(phase.name)
            .sandbox(
                SandboxAgentConfig::new().with_capabilities(self.capabilities.iter().cloned()),
            );
        if let Some(instructions) = phase.instructions {
            builder = builder.instructions(instructions);
        }
        if let Some(settings) = phase.model_settings {
            builder = builder.model_settings(settings.clone());
        }
        if let Some(output_schema) = phase.output_schema {
            builder = builder.output_schema(output_schema);
        }
        let resolver: Arc<dyn ModelResolver> = if let Some(model) = phase.model.instance() {
            Arc::new(InstanceResolver {
                model: Arc::clone(model),
            })
        } else if let Some(name) = phase.model.name() {
            builder = builder.model(name);
            Arc::clone(&self.resolver)
        } else {
            return Err(Error::config(format!(
                "{} has a model that is neither a name nor an instance",
                phase.name
            )));
        };
        let agent = builder.build()?;

        let mut config = RunConfig::new()
            .with_sandbox(SandboxRunConfig::new().with_session(Arc::clone(&self.session)));
        if let Some(max_turns) = phase.max_turns {
            config = config.with_max_turns(max_turns);
        }
        let request = RunRequest::new(
            AgentBinding::direct(agent),
            resolver,
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(prompt))],
        )
        .with_config(config);
        require_completed(phase.name, Runner::run(request).await?)
    }
}

impl fmt::Debug for MemoryRunConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryRunConfig")
            .field("session", &self.session.backend_id())
            .field(
                "capabilities",
                &self
                    .capabilities
                    .iter()
                    .map(|capability| capability.kind())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The run, when it concluded on its own; an error saying how it stopped otherwise.
///
/// The reference's runner raises when a run hits its turn cap or trips a guardrail, and the
/// generation manager treats that as the phase failing. A run here reports those — and an
/// interruption, a cancellation, an exhausted budget or an error handler's closeout — as a result
/// that stopped without the agent's own answer, so a phase checks that its run concluded before its
/// output is used. A consolidation cut off at its turn cap has not consolidated, and recording its
/// selection as the last successful one would take those rollouts out of the next consolidation's
/// "added" list.
fn require_completed(agent_name: &str, result: RunResult) -> Result<RunResult> {
    match result.outcome() {
        RunOutcome::Completed { reason } if reason.is_complete() => Ok(result),
        RunOutcome::Completed { reason } => Err(Error::config(format!(
            "{agent_name} did not complete: it stopped with `{}`",
            reason.code()
        ))),
        _ => Err(Error::config(format!(
            "{agent_name} did not complete: it stopped for approval"
        ))),
    }
}

/// One phase agent, as the reference constructs its `SandboxAgent`.
pub(super) struct PhaseAgent<'a> {
    pub(super) name: &'static str,
    pub(super) instructions: Option<String>,
    pub(super) model: &'a MemoryModel,
    pub(super) model_settings: Option<&'a ModelSettings>,
    pub(super) output_schema: Option<OutputSchema>,
    /// `None` keeps the run's default turn cap.
    pub(super) max_turns: Option<u32>,
}

/// Answers every resolution with one model given as an instance.
///
/// The reference uses a `Model` instance on an agent directly, without asking its provider for
/// anything (`test_agent_model_object_is_used_when_present`). An agent here names its model and the
/// run resolves the name, so a phase configured with an instance resolves through this, which
/// never consults the run's resolver: a host with no default model can still generate memory with
/// instances. The selection names no model and the [`MODEL_INSTANCE_PROVIDER`] provider, with the
/// Responses protocol the reference's models speak by default; it identifies the call in traces and
/// callbacks only. The instance is called as it is, with no provider or model defaults.
struct InstanceResolver {
    model: Arc<dyn Model>,
}

impl ModelResolver for InstanceResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new(MODEL_INSTANCE_PROVIDER),
                None,
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}
