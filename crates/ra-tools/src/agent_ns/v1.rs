// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::path::PathBuf;

use ra_core::{
    item::{ContentBlock, ImageSource, Message, MessageRole},
    model::Effort,
    session::SessionId,
};

use std::time::Duration;

use ra_core::{
    agent::{
        AgentId,
        control::{AgentControlPort, SpawnAgentForkMode, SpawnAgentRequest},
    },
    error::Result,
    event::SubAgentActivityKind,
    tool::{FuncSchema, ToolContext, ToolOutput, ToolSchema},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use super::{
    CollaborationKind, CollaborationTool, DEFAULT_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS,
    MIN_WAIT_TIMEOUT_MS, json_output, record_activity, record_activity_for_session,
};

/// Codex's namespace for the V1 collaboration functions.
pub const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputItem {
    Text { text: String },
    Image { image_url: String },
    LocalImage,
    Audio,
    LocalAudio,
    Skill { name: String, path: PathBuf },
    Mention { name: String, path: String },
}

impl InputItem {
    fn into_content(self) -> std::result::Result<ContentBlock, &'static str> {
        Ok(match self {
            Self::Text { text } => ContentBlock::text(text),
            Self::Image { image_url } => ContentBlock::image(ImageSource::url(image_url)),
            Self::Skill { name, path } => ContentBlock::Skill { name, path },
            Self::Mention { name, path } => ContentBlock::Mention { name, path },
            Self::LocalImage => {
                return Err(
                    "local_image input is unavailable for collaboration tools; use a URL image or fork a conversation containing the image",
                );
            }
            Self::Audio | Self::LocalAudio => {
                return Err("audio input is unavailable for collaboration tools");
            }
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
struct SpawnInput {
    /// Initial plain-text task for the new agent. Use either message or items.
    message: Option<String>,
    /// Structured input items, including explicit skill and resource mentions.
    items: Option<Vec<InputItem>>,
    /// Registered agent type to use for the task.
    agent_type: Option<String>,
    /// True forks the current thread history into the new agent; false or omitted starts with only the initial prompt.
    fork_context: Option<bool>,
    /// Model override for the new agent. Omit to inherit the parent model.
    model: Option<String>,
    /// Reasoning effort override for the new agent. Omit to inherit the parent effort.
    reasoning_effort: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
struct SendInput {
    /// Agent ID to message, as returned by `spawn_agent`.
    target: String,
    /// Legacy plain-text message to send to the agent. Use either message or items.
    message: Option<String>,
    /// Structured input items, including explicit skill and resource mentions.
    items: Option<Vec<InputItem>>,
    /// True interrupts the current task and handles this message immediately; false or omitted queues it.
    interrupt: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
struct WaitInput {
    /// Agent IDs to wait on. Pass multiple IDs to wait for whichever finishes first.
    targets: Vec<String>,
    /// Timeout in milliseconds. Defaults to 30000, min 10000, max 3600000. Prefer longer waits (minutes) to avoid busy polling.
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
struct ResumeInput {
    /// Agent ID to resume.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false)]
#[serde(deny_unknown_fields)]
struct CloseInput {
    /// Agent ID to close, as returned by `spawn_agent`.
    target: String,
}

pub(super) fn schema(kind: CollaborationKind) -> Result<(FuncSchema, String)> {
    let name = kind.name();
    let (schema, description) = match kind {
        CollaborationKind::SpawnAgentV1 => (FuncSchema::for_input::<SpawnInput>(name)?, ""),
        CollaborationKind::SendInput => (
            FuncSchema::for_input::<SendInput>(name)?,
            "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task.",
        ),
        CollaborationKind::WaitAgentV1 => (
            FuncSchema::for_input::<WaitInput>(name)?,
            "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status.",
        ),
        CollaborationKind::ResumeAgent => (
            FuncSchema::for_input::<ResumeInput>(name)?,
            "Resume a previously closed agent by id so it can receive send_input and wait_agent calls.",
        ),
        CollaborationKind::CloseAgent => (
            FuncSchema::for_input::<CloseInput>(name)?,
            "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Completed agents remain open and count toward the concurrency limit until closed. Don't keep agents open for too long if they are not needed anymore.",
        ),
        _ => unreachable!("V1 schemas are used only for V1 tools"),
    };
    Ok((schema, description.to_owned()))
}

pub(super) fn parameters(kind: CollaborationKind, roles: Option<&str>) -> serde_json::Value {
    let field = |kind: &str, description: &str| json!({"type": kind, "description": description});
    let items = json!({
        "type": "array",
        "description": "Structured input items. Use this to pass explicit mentions (for example app:// connector paths).",
        "items": {"type": "object", "additionalProperties": false, "properties": {
            "type": field("string", "Input item type: text, image, local_image, audio, local_audio, skill, or mention."),
            "text": field("string", "Text content when type is text."),
            "image_url": field("string", "Image URL when type is image."),
            "audio_url": field("string", "Audio data URL when type is audio."),
            "path": field("string", "Path when type is local_image/local_audio/skill, or structured mention target such as app://<connector-id> or plugin://<plugin-name>@<marketplace-name> when type is mention."),
            "name": field("string", "Display name when type is skill or mention.")
        }}
    });
    let (mut properties, required) = match kind {
        CollaborationKind::SpawnAgentV1 => (
            json!({
                "message": field("string", "Initial plain-text task for the new agent. Use either message or items."),
                "items": items,
                "fork_context": field("boolean", "True forks the current thread history into the new agent; false or omitted starts with only the initial prompt."),
                "model": field("string", "Model override for the new agent. Omit unless an explicit override is needed."),
                "reasoning_effort": field("string", "Reasoning effort override for the new agent. Omit to inherit the parent effort.")
            }),
            None,
        ),
        CollaborationKind::SendInput => (
            json!({
                "target": field("string", "Agent id to message (from spawn_agent)."),
                "message": field("string", "Legacy plain-text message to send to the agent. Use either message or items."),
                "items": items,
                "interrupt": field("boolean", "True interrupts the current task and handles this message immediately; false or omitted queues it.")
            }),
            Some("target"),
        ),
        CollaborationKind::WaitAgentV1 => (
            json!({
                "targets": {"type":"array", "items":{"type":"string"}, "description":"Agent ids to wait on. Pass multiple ids to wait for whichever finishes first."},
                "timeout_ms": field("number", "Timeout in milliseconds. Defaults to 30000, min 10000, max 3600000. Prefer longer waits (minutes) to avoid busy polling.")
            }),
            Some("targets"),
        ),
        CollaborationKind::ResumeAgent => (
            json!({"id": field("string", "Agent id to resume.")}),
            Some("id"),
        ),
        CollaborationKind::CloseAgent => (
            json!({"target": field("string", "Agent id to close (from spawn_agent).")}),
            Some("target"),
        ),
        _ => unreachable!("V1 parameters are used only for V1 tools"),
    };
    if let Some(roles) = roles.filter(|_| kind == CollaborationKind::SpawnAgentV1) {
        properties["agent_type"] = field(
            "string",
            &format!(
                "Agent type override for the new agent. Omit to inherit the parent agent type with a full-history fork; otherwise, `default` is used.\n{roles}"
            ),
        );
    }
    let mut schema =
        json!({"type": "object", "properties": properties, "additionalProperties": false});
    if let Some(required) = required {
        schema["required"] = json!([required]);
    }
    schema
}

const SPAWN_AGENT_INHERITED_MODEL_GUIDANCE: &str = "Spawned agents inherit your current model by default. Omit `model` to use that preferred default; set `model` only when an explicit override is needed.";
const SPAWN_AGENT_MODEL_CATALOG_GUIDANCE: &str =
    "Pick model overrides from the latest <model_catalog> listing.";

fn spawn_agent_tool_description(
    available_models_description: Option<&str>,
    model_catalog_in_context: bool,
    inherited_model_guidance: Option<&str>,
    return_value_description: &str,
    usage_hint_text: Option<String>,
) -> String {
    let model_catalog_guidance = inherited_model_guidance
        .map(|_| SPAWN_AGENT_MODEL_CATALOG_GUIDANCE)
        .unwrap_or_default();
    let inherited_model_guidance = inherited_model_guidance.unwrap_or_default();

    let tool_description = if model_catalog_in_context {
        format!(
            r"
        Spawn a sub-agent for a well-scoped task. {return_value_description} {inherited_model_guidance}
{model_catalog_guidance}"
        )
    } else {
        let available_models_description = available_models_description.unwrap_or_default();
        format!(
            r"
        {available_models_description}
        Spawn a sub-agent for a well-scoped task. {return_value_description} {inherited_model_guidance}"
        )
    };

    if let Some(usage_hint_text) = usage_hint_text {
        return format!(
            r"
        {tool_description}
{usage_hint_text}"
        );
    }
    let agent_role_usage_hint = if model_catalog_in_context {
        ""
    } else if available_models_description.is_some() {
        "\nAgent-role guidance below only helps choose which agent to use after spawning is already authorized; it never authorizes spawning by itself."
    } else {
        "\n"
    };
    format!(
        r"
        {tool_description}
This spawn_agent tool provides you access to sub-agents that inherit your current model by default. Do not set the `model` field unless the user explicitly asks for a different model. You should follow the rules and guidelines below to use this tool.

Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.
Requests for depth, thoroughness, research, investigation, or detailed codebase analysis do not count as permission to spawn.{agent_role_usage_hint}

### When to delegate vs. do the subtask yourself
- First, quickly analyze the overall user task and form a succinct high-level plan. Identify which tasks are immediate blockers on the critical path, and which tasks are sidecar tasks that are needed but can run in parallel without blocking the next local step. As part of that plan, explicitly decide what immediate task you should do locally right now. Do this planning step before delegating to agents so you do not hand off the immediate blocking task to a submodel and then waste time waiting on it.
- Use a subagent when a subtask is easy enough for it to handle and can run in parallel with your local work. Prefer delegating concrete, bounded sidecar tasks that materially advance the main task without blocking your immediate next local step.
- Do not delegate urgent blocking work when your immediate next step depends on that result. If the very next action is blocked on that task, the main rollout should usually do it locally to keep the critical path moving.
- Keep work local when the subtask is too difficult to delegate well and when it is tightly coupled, urgent, or likely to block your immediate next step.

### Designing delegated subtasks
- Subtasks must be concrete, well-defined, and self-contained.
- Delegated subtasks must materially advance the main task.
- Do not duplicate work between the main rollout and delegated subtasks.
- Avoid issuing multiple delegate calls on the same unresolved thread unless the new delegated task is genuinely different and necessary.
- Narrow the delegated ask to the concrete output you need next.
- For coding tasks, prefer delegating concrete code-change worker subtasks over read-only explorer analysis when the subagent can make a bounded patch in a clear write scope.
- When delegating coding work, instruct the submodel to edit files directly in its forked workspace and list the file paths it changed in the final answer.
- For code-edit subtasks, decompose work so each delegated task has a disjoint write set.

### After you delegate
- Call wait_agent very sparingly. Only call wait_agent when you need the result immediately for the next critical-path step and you are blocked until it returns.
- Do not redo delegated subagent tasks yourself; focus on integrating results or tackling non-overlapping work.
- While the subagent is running in the background, do meaningful non-overlapping work immediately.
- Do not repeatedly wait by reflex.
- When a delegated coding task returns, quickly review the uploaded changes, then integrate or refine them.

### Parallel delegation patterns
- Run multiple independent information-seeking subtasks in parallel when you have distinct questions that can be answered independently.
- Split implementation into disjoint codebase slices and spawn multiple agents for them in parallel when the write scopes do not overlap.
- Delegate verification only when it can run in parallel with ongoing implementation and is likely to catch a concrete risk before final integration.
- The key is to find opportunities to spawn multiple independent subtasks in parallel within the same round, while ensuring each subtask is well-defined, self-contained, and materially advances the main task."
    )
}

impl CollaborationTool {
    /// Composes the V1 spawn schema from the product's model and role catalogs.
    ///
    /// # Errors
    ///
    /// Returns an error if the function schema cannot be built.
    pub fn with_v1_spawn_description(
        mut self,
        models: &str,
        roles: Option<&str>,
        usage_hint: Option<String>,
    ) -> Result<Self> {
        if self.kind == CollaborationKind::SpawnAgentV1 {
            let description = spawn_agent_tool_description(
                Some(models),
                false,
                Some(SPAWN_AGENT_INHERITED_MODEL_GUIDANCE),
                "Returns the spawned agent id plus the user-facing nickname when available.",
                usage_hint,
            );
            let schema = ToolSchema::loose(self.kind.name(), parameters(self.kind, roles))?
                .with_description(description);
            self.schema = match self.schema.namespace_description() {
                Some(description) => schema.with_namespace_description(description),
                None => schema,
            };
        }
        Ok(self)
    }

    pub(super) async fn run_v1(
        &self,
        context: &mut ToolContext<'_>,
        port: &dyn AgentControlPort,
    ) -> Result<ToolOutput> {
        match self.kind {
            CollaborationKind::SpawnAgentV1 => self.spawn_v1(context, port).await,
            CollaborationKind::SendInput => {
                let args: SendInput = self.input(context)?;
                let target = self.agent_id(&args.target)?;
                let input = self.user_input(args.message, args.items)?;
                let (submission_id, path) = port
                    .send_input(&target, input, args.interrupt.unwrap_or(false))
                    .await
                    .map_err(|error| self.agent_error(&error))?;
                record_activity(context, port, &path, SubAgentActivityKind::Interacted);
                json_output(&json!({"submission_id": submission_id}))
            }
            CollaborationKind::WaitAgentV1 => {
                let args: WaitInput = self.input(context)?;
                if args.targets.is_empty() {
                    return Err(self.refuse("agent ids must be non-empty"));
                }
                let targets = args
                    .targets
                    .iter()
                    .map(|id| self.agent_id(id))
                    .collect::<Result<Vec<_>>>()?;
                let timeout = args.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
                if timeout <= 0 {
                    return Err(self.refuse("timeout_ms must be greater than zero"));
                }
                let timeout = timeout.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS);
                let status = port
                    .wait_for_agents(
                        &targets,
                        Duration::from_millis(u64::try_from(timeout).unwrap_or_default()),
                    )
                    .await
                    .map_err(|error| self.agent_error(&error))?;
                json_output(&json!({"status": status, "timed_out": status.is_empty()}))
            }
            CollaborationKind::ResumeAgent => {
                let args: ResumeInput = self.input(context)?;
                let agent = port
                    .resume_agent(&self.agent_id(&args.id)?)
                    .await
                    .map_err(|error| self.agent_error(&error))?;
                record_activity(
                    context,
                    port,
                    agent.agent_path(),
                    SubAgentActivityKind::custom("resumed"),
                );
                json_output(&json!({"status": agent.status()}))
            }
            CollaborationKind::CloseAgent => {
                let args: CloseInput = self.input(context)?;
                let agent = port
                    .close_agent(&self.agent_id(&args.target)?)
                    .await
                    .map_err(|error| self.agent_error(&error))?;
                record_activity_for_session(
                    context,
                    agent.agent_path(),
                    agent.session_id().cloned(),
                    SubAgentActivityKind::custom("closed"),
                );
                json_output(&json!({"previous_status": agent.status()}))
            }
            _ => unreachable!("V1 dispatch is used only for V1 tools"),
        }
    }

    async fn spawn_v1(
        &self,
        context: &mut ToolContext<'_>,
        port: &dyn AgentControlPort,
    ) -> Result<ToolOutput> {
        let args: SpawnInput = self.input(context)?;
        let input = self.user_input(args.message, args.items)?;
        let mut request =
            SpawnAgentRequest::new(uuid::Uuid::now_v7().simple().to_string(), "").with_input(input);
        let agent_type = args
            .agent_type
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if args.fork_context.unwrap_or(false) {
            if agent_type.is_some() {
                return Err(self.refuse("Full-history forked agents inherit the parent agent type; omit agent_type, or spawn without a full-history fork."));
            }
            request = request.with_fork_mode(SpawnAgentForkMode::FullHistory);
        } else if let Some(agent_type) = agent_type {
            request = request.with_agent_type(AgentId::new(agent_type));
        }
        if let Some(model) = context.run().resolved_model_name() {
            request = request.with_invoking_model(model.to_owned());
        }
        if let Some(model) = args.model.or_else(|| self.default_spawn_model.clone()) {
            request = request.with_model(model);
        }
        if let Some(effort) = args.reasoning_effort {
            if let Ok(typed) = serde_json::from_value::<Effort>(json!(&effort)) {
                request = request.with_reasoning_effort(typed);
            }
            request = request.with_reasoning_effort_label(effort);
        } else if let Some(effort) = self.default_spawn_effort {
            request = request.with_reasoning_effort(effort);
        }
        let agent = port.spawn(request).await.map_err(|error| match error {
            ra_core::agent::control::AgentControlError::InvalidRequest(message) => {
                self.refuse(message)
            }
            other => self.spawn_error(&other),
        })?;
        let id = agent
            .session_id()
            .ok_or_else(|| self.refuse("Spawned agent has no session ID"))?;
        record_activity(
            context,
            port,
            agent.agent_path(),
            SubAgentActivityKind::Started,
        );
        json_output(&json!({"agent_id": id, "nickname": null}))
    }

    fn agent_id(&self, id: &str) -> Result<SessionId> {
        uuid::Uuid::parse_str(id.strip_prefix("sess-").unwrap_or(id))
            .map_err(|error| self.refuse(format!("invalid agent id {id}: {error}")))?;
        Ok(SessionId::new(id))
    }

    fn user_input(
        &self,
        message: Option<String>,
        items: Option<Vec<InputItem>>,
    ) -> Result<Message> {
        match (message, items) {
            (Some(_), Some(_)) => Err(self.refuse("Provide either message or items, but not both")),
            (None, None) => Err(self.refuse("Provide one of: message or items")),
            (Some(message), None) => self.message_content(message).map(Message::user),
            (None, Some(items)) if items.is_empty() => Err(self.refuse("Items can't be empty")),
            (None, Some(items)) => {
                let content = items
                    .into_iter()
                    .map(InputItem::into_content)
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|message| self.refuse(message))?;
                Ok(Message::new(MessageRole::User, content))
            }
        }
    }
}
