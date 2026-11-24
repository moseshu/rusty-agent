// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::{fmt::Write as _, sync::Arc};

use ra_core::{
    agent::{AgentSpec, control::SpawnAgentRequest},
    error::{Error, Result},
    model::Effort,
};
use ra_paths::AbsolutePathBuf;
use ra_runtime::runner::RunConfig;
use serde_json::json;

use super::{ModelInfo, ModelVisibility, ProductModelRuntime, ReasoningEffort, TurnCatalog};
use crate::config::roles_loader::read_resolved_agent_role_file;

const MAX_MODEL_OVERRIDES: usize = 5;

const BUILTIN_ROLES: [(&str, &str); 3] = [
    ("default", "Default agent."),
    (
        "explorer",
        r"Use `explorer` for specific codebase questions.
Explorers are fast and authoritative.
They must be used to ask specific, well-scoped questions on the codebase.
Rules:
- In order to avoid redundant work, you should avoid exploring the same problem that explorers have already covered. Typically, you should trust the explorer results without additional verification. You are still allowed to inspect the code yourself to gain the needed context!
- You are encouraged to spawn up multiple explorers in parallel when you have multiple distinct questions to ask about the codebase that can be answered independently. This allows you to get more information faster without waiting for one question to finish before asking the next. While waiting for the explorer results, you can continue working on other local tasks that do not depend on those results. This parallelism is a key advantage of delegation, so use it whenever you have multiple questions to ask.
- Reuse existing explorers for related questions.",
    ),
    (
        "worker",
        r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility). When the subtask involves code changes, you should clearly specify which files or modules the worker is responsible for. This helps avoid merge conflicts and ensures accountability. For example, you can say "Worker 1 is responsible for updating the authentication module, while Worker 2 will handle the database layer." By defining clear ownership, you can delegate more effectively and reduce coordination overhead.
- Always tell workers they are **not alone in the codebase**, and they should not revert the edits made by others, and they should adjust their implementation to accommodate the changes made by others. This is important because there may be multiple workers making changes in parallel, and they need to be aware of each other's work to avoid conflicts and ensure a cohesive final product."#,
    ),
];

impl ProductModelRuntime {
    fn spawn_models(&self) -> Vec<ModelInfo> {
        let mut models = if let Some(remote) = &self.remote {
            let catalog = match RunConfig::current_run_snapshot::<TurnCatalog>() {
                Some(snapshot) if Arc::ptr_eq(&snapshot.source, remote) => snapshot.models.clone(),
                _ => remote.snapshot(),
            };
            catalog.map_or_else(Vec::new, |catalog| catalog.models.clone())
        } else {
            self.static_catalog()
                .map_or_else(Vec::new, |catalog| catalog.models.clone())
        };
        // This product's catalog uses the API authentication surface.
        models.retain(|model| model.supported_in_api);
        models.sort_by_key(|model| model.priority);
        models
    }

    pub(crate) fn v1_spawn_models_description(&self) -> String {
        let models = self.spawn_models();
        let descriptions = visible_models(&models)
            .map(model_description)
            .collect::<Vec<_>>();
        if descriptions.is_empty() {
            "No picker-visible model overrides are currently loaded.".to_owned()
        } else {
            format!(
                "Available model overrides (optional; inherited parent model is preferred):\n{}",
                descriptions.join("\n")
            )
        }
    }

    pub(crate) fn v1_spawn_roles_description(&self) -> Option<String> {
        if self.config.agent_roles.is_empty() {
            return None;
        }
        let mut roles = self
            .config
            .agent_roles
            .iter()
            .map(|(name, role)| {
                if let Some(description) = &role.description {
                    let settings = role
                        .config_file
                        .as_ref()
                        .and_then(|file| std::fs::read_to_string(file).ok())
                        .and_then(|contents| toml::from_str::<toml::Value>(&contents).ok());
                    let note = role_settings_note(settings.as_ref());
                    format!("{name}: {{\n{description}{note}\n}}")
                } else {
                    format!("{name}: no description")
                }
            })
            .collect::<Vec<_>>();
        roles.extend(
            BUILTIN_ROLES
                .iter()
                .filter(|(name, _)| !self.config.agent_roles.contains_key(*name))
                .map(|(name, description)| format!("{name}: {{\n{description}\n}}")),
        );
        Some(format!("Available roles:\n{}", roles.join("\n")))
    }

    pub(crate) fn owns_v1_agent_type(&self, kind: &str) -> bool {
        self.config.agent_roles.contains_key(kind)
            || BUILTIN_ROLES.iter().any(|(name, _)| *name == kind)
    }

    pub(crate) async fn configure_v1_spawn(
        &self,
        request: &SpawnAgentRequest,
        agent: &AgentSpec,
        parent: &RunConfig,
    ) -> Result<RunConfig> {
        let invoking_model = request
            .invoking_model()
            .or(parent.model())
            .or(agent.model())
            .or(self.default_model.as_deref());
        let requested_model = request
            .model()
            .or(self.config.agent_default_subagent_model.as_deref());
        let mut model = requested_model.or(invoking_model).map(str::to_owned);
        let requested_effort = if let Some(label) = request.reasoning_effort_label() {
            Some(label.parse::<ReasoningEffort>().map_err(Error::caller)?)
        } else if let Some(effort) = request.reasoning_effort() {
            Some(
                effort
                    .label()
                    .parse::<ReasoningEffort>()
                    .map_err(Error::caller)?,
            )
        } else {
            self.config.agent_default_subagent_reasoning_effort.clone()
        };
        let mut effort = None;
        if let Some(requested_model) = requested_model {
            let models = self.spawn_models();
            let selected = models.iter().find(|candidate| candidate.slug == requested_model)
                .ok_or_else(|| {
                    let available = visible_models(&models).map(|candidate| candidate.slug.as_str())
                        .collect::<Vec<_>>().join(", ");
                    Error::caller(format!("Unknown model `{requested_model}` for spawn_agent. Available models: {available}"))
                })?;
            effort = Some(
                requested_effort
                    .clone()
                    .or_else(|| selected.default_reasoning_level.clone()),
            );
        }
        if let Some(requested) = requested_effort {
            self.validate_spawn_effort(model.as_deref(), &requested)?;
            effort = Some(Some(requested));
        }

        if request.fork_mode().is_none() {
            let role = request.agent_type().map_or("default", |id| id.as_str());
            let (role_model, role_effort) = self.v1_role_model_settings(role, agent).await?;
            if role_model.is_some() || role_effort.is_some() {
                // Without an override the child carries the parent's effort, which is what a
                // role setting is compared against, as Codex compares against the inherited
                // config.
                let previous_effort = effort
                    .clone()
                    .unwrap_or_else(|| self.inherited_spawn_effort(invoking_model, parent));
                let changed = role_model
                    .as_ref()
                    .is_some_and(|role_model| Some(role_model) != model.as_ref())
                    || role_effort
                        .as_ref()
                        .is_some_and(|role_effort| previous_effort.as_ref() != Some(role_effort));
                if let Some(role_model) = role_model {
                    if changed && effort.is_none() && role_effort.is_none() {
                        effort = Some(previous_effort);
                    }
                    model = Some(role_model);
                }
                if let Some(role_effort) = role_effort {
                    effort = Some(Some(role_effort));
                }
                // Codex skips this check when the role's model has only fallback metadata, so a
                // role may name a model the catalog does not list.
                if changed
                    && let Some(candidate) = effort.as_ref().and_then(Option::as_ref)
                    && model
                        .as_deref()
                        .is_some_and(|model| self.model_info(model).is_some())
                {
                    self.validate_spawn_effort(model.as_deref(), candidate)?;
                }
            }
        }
        let config = match &model {
            Some(model) => parent.clone().with_model(model),
            None => parent.clone(),
        };
        let Some(effort) = effort else {
            return Ok(config);
        };
        let settings = self.spawn_effort_settings(model.as_deref(), effort.as_ref(), &config)?;
        Ok(config.with_model_settings(settings))
    }

    async fn v1_role_model_settings(
        &self,
        role: &str,
        agent: &AgentSpec,
    ) -> Result<(Option<String>, Option<ReasoningEffort>)> {
        if !self.config.agent_roles.contains_key(role) && agent.id().as_str() == role {
            let effort = agent
                .model_settings()
                .effort()
                .map(|effort| effort.label().parse::<ReasoningEffort>())
                .transpose()
                .map_err(Error::caller)?;
            return Ok((agent.model().map(str::to_owned), effort));
        }
        let Some(file) = self
            .config
            .agent_roles
            .get(role)
            .and_then(|role| role.config_file.as_ref())
        else {
            return Ok((None, None));
        };
        let file = AbsolutePathBuf::from_absolute_path(file)
            .map_err(|_| Error::caller("agent type is currently not available"))?;
        let settings = read_resolved_agent_role_file(
            ra_exec::filesystem::LOCAL_FS.as_ref(),
            &file,
            Some(role),
        )
        .await
        .and_then(|settings| {
            settings
                .config
                .try_into::<ra_config::config_toml::ConfigToml>()
                .map_err(std::io::Error::other)
        })
        .map_err(|error| {
            tracing::warn!(%error, %role, "failed to apply role to config");
            Error::caller("agent type is currently not available")
        })?;
        Ok((settings.model, settings.model_reasoning_effort))
    }

    fn inherited_spawn_effort(
        &self,
        model: Option<&str>,
        parent: &RunConfig,
    ) -> Option<ReasoningEffort> {
        let settings = parent.model_settings();
        let label = settings.effort().map(Effort::label).or_else(|| {
            let provider = self.registry.as_ref()?.default_provider();
            settings
                .extra_body()
                .get(provider)?
                .get("reasoning")?
                .get("effort")?
                .as_str()
        });
        if let Some(label) = label {
            return Some(
                label
                    .parse()
                    .unwrap_or_else(|_| ReasoningEffort::Custom(label.to_owned())),
            );
        }
        let info = model.and_then(|model| self.model_info(model));
        self.configured_reasoning_effort(info.as_ref()).cloned()
    }

    fn validate_spawn_effort(&self, model: Option<&str>, effort: &ReasoningEffort) -> Result<()> {
        let name = model.unwrap_or_default();
        let info = self.model_info(name);
        let supported = info
            .as_ref()
            .map_or(&[][..], |info| &info.supported_reasoning_levels);
        if supported.iter().any(|preset| &preset.effort == effort) {
            return Ok(());
        }
        let supported = supported
            .iter()
            .map(|preset| preset.effort.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::caller(format!(
            "Reasoning effort `{effort}` is not supported for model `{name}`. Supported reasoning efforts: {supported}"
        )))
    }

    fn spawn_effort_settings(
        &self,
        model: Option<&str>,
        effort: Option<&ReasoningEffort>,
        config: &RunConfig,
    ) -> Result<ra_core::model::ModelSettings> {
        let mut settings = config.model_settings().clone().without_effort();
        if let Some(registry) = &self.registry {
            let provider = registry.default_provider();
            let info = model.and_then(|model| self.model_info(model));
            let defaults = self.product_settings_with_effort(info.as_ref(), provider, effort);
            let mut reasoning = defaults.extra_body()[provider]["reasoning"].clone();
            if let Some(previous) = settings
                .extra_body()
                .get(provider)
                .and_then(|body| body.get("reasoning"))
                .and_then(serde_json::Value::as_object)
                && let Some(values) = reasoning.as_object_mut()
            {
                values.extend(
                    previous
                        .iter()
                        .filter(|(key, _)| key.as_str() != "effort")
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
            }
            settings = settings.with_extra_body_value(provider.clone(), "reasoning", reasoning);
        } else if let Some(effort) = effort {
            let typed = serde_json::from_value::<Effort>(json!(effort.as_str())).map_err(|_| {
                Error::caller(format!(
                    "Reasoning effort `{effort}` is unavailable through this model resolver"
                ))
            })?;
            settings = settings.with_effort(typed);
        }
        Ok(settings)
    }
}

fn role_settings_note(settings: Option<&toml::Value>) -> String {
    let model = settings
        .and_then(|settings| settings.get("model"))
        .and_then(toml::Value::as_str);
    let effort = settings
        .and_then(|settings| settings.get("model_reasoning_effort"))
        .and_then(toml::Value::as_str);
    match (model, effort) {
        (Some(model), Some(effort)) => format!(
            "\n- This role's model is set to `{model}` and its reasoning effort is set to `{effort}`. These settings cannot be changed."
        ),
        (Some(model), None) => {
            format!("\n- This role's model is set to `{model}` and cannot be changed.")
        }
        (None, Some(effort)) => {
            format!("\n- This role's reasoning effort is set to `{effort}` and cannot be changed.")
        }
        (None, None) => String::new(),
    }
}

fn visible_models(models: &[ModelInfo]) -> impl Iterator<Item = &ModelInfo> {
    models
        .iter()
        .filter(|model| model.visibility == ModelVisibility::List)
        .take(MAX_MODEL_OVERRIDES)
}

fn model_description(model: &ModelInfo) -> String {
    let description = model.description.as_deref().unwrap_or_default();
    let mut line = format!("- `{}`: {description}", model.slug);
    let efforts = model
        .supported_reasoning_levels
        .iter()
        .map(|preset| {
            let effort = preset.effort.as_str().chars().take(64).collect::<String>();
            if model.default_reasoning_level.as_ref() == Some(&preset.effort) {
                format!("{effort} (default)")
            } else {
                effort
            }
        })
        .collect::<Vec<_>>();
    if !efforts.is_empty() {
        let _ = write!(line, " Reasoning efforts: {}.", efforts.join(", "));
    }
    let tiers = model
        .service_tiers
        .iter()
        .map(|tier| tier.id.as_str())
        .collect::<Vec<_>>();
    if !tiers.is_empty() {
        let _ = write!(line, " Service tiers: {}.", tiers.join(", "));
    }
    line
}
