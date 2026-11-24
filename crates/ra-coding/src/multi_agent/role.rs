// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Developer context for root and spawned V2 sessions.

use std::sync::Arc;

use ra_core::{
    error::Result,
    filter::{ContextFilter, ContextFilterRequest, ModelInputData},
    tool::ToolNamespace,
};
use ra_tools::agent_ns::CollaborationKind;

use super::{MultiAgentVersion, ThreadCollaboration};

#[derive(Clone, Copy)]
pub(super) enum Role {
    Root,
    Subagent,
}

pub(super) struct ThreadModelContext {
    pub(super) thread: Arc<ThreadCollaboration>,
    pub(super) namespace: Option<ToolNamespace>,
    pub(super) collaborates: bool,
}

impl ContextFilter for ThreadModelContext {
    fn name(&self) -> &'static str {
        "product_model_context"
    }

    fn filter_model_input(
        &self,
        request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        let thread = &self.thread;
        // Spawned runs inherit the filter and install child hooks with their own session ID.
        // The factory sets the initial role for threads loaded independently of their tree.
        let spawned_in_tree = request
            .session_id()
            .is_some_and(|id| id != &thread.session_id);
        let role = if spawned_in_tree {
            Some(Role::Subagent)
        } else {
            thread.role
        };
        let model = role
            .and_then(|_| request.model())
            .and_then(|model| thread.runtime.model_info(model));
        let model_version = model.as_ref().and_then(|info| info.multi_agent_version);
        let role =
            role.filter(|_| thread.version_for_model(model_version) == MultiAgentVersion::V2);
        let catalog_role = model
            .as_ref()
            .and_then(|info| info.model_messages.as_ref())
            .and_then(|messages| messages.multi_agent.as_ref())
            .and_then(|messages| messages.role.as_ref());
        let config = thread.runtime.multi_agent_v2();
        let hint = role.and_then(|role| {
            let (configured, catalog, builtin) = match role {
                Role::Root => (
                    config.root_agent_usage_hint_text.as_deref(),
                    catalog_role.and_then(|role| role.root.as_deref()),
                    ROOT,
                ),
                Role::Subagent => (
                    config.subagent_usage_hint_text.as_deref(),
                    catalog_role.and_then(|role| role.subagent.as_deref()),
                    SUBAGENT,
                ),
            };
            if let Some(configured) = configured {
                return (!configured.is_empty()).then(|| configured.to_owned());
            }
            let base = catalog.unwrap_or(builtin);
            if base.is_empty() {
                return None;
            }
            let hint = compose(
                base,
                &CollaborationTools {
                    thread,
                    namespace: self.namespace.as_ref().map(ToolNamespace::as_str),
                    enabled: self.collaborates
                        && thread.tools_enabled_for_model(model_version, spawned_in_tree),
                },
                thread.runtime.omit_role_update_plan_instructions(),
                config.max_concurrent_threads_per_session,
            );
            Some(if catalog.is_some() {
                format!("<multi_agent_role>{hint}</multi_agent_role>")
            } else {
                hint
            })
        });
        thread.runtime.project_model_input(
            request,
            data,
            hint,
            matches!(role, Some(Role::Subagent)),
        )
    }
}

/// The configured collaboration tools after the coding product's delegation gate.
struct CollaborationTools<'a> {
    thread: &'a ThreadCollaboration,
    namespace: Option<&'a str>,
    enabled: bool,
}

impl CollaborationTools<'_> {
    fn offers(&self, kind: CollaborationKind) -> bool {
        self.enabled && self.thread.offers(kind)
    }

    /// The direct-call guidance of Codex's shared hint, naming only the tools offered and the
    /// recipient in the configured namespace. Codex's sentences about `functions.exec` are left
    /// out: this product has no code mode.
    fn direct_call_guidance(&self) -> Option<String> {
        let names = SHARED_TOOL_ORDER
            .into_iter()
            .filter(|kind| self.offers(*kind))
            .map(|kind| format!("`{}`", kind.name()))
            .collect::<Vec<_>>();
        let listed = match names.as_slice() {
            [] => return None,
            [only] => only.clone(),
            [first, second] => format!("{first} and {second}"),
            [init @ .., last] => format!("{}, and {last}", init.join(", ")),
        };
        let example = SHARED_TOOL_ORDER
            .into_iter()
            .find(|kind| self.offers(*kind))
            .map(CollaborationKind::name)
            .unwrap_or_default();
        let recipient = match self.namespace {
            Some(namespace) => format!("functions.{namespace}.{example}"),
            None => format!("functions.{example}"),
        };
        Some(format!(
            "Call {listed} only as direct tool calls using the recipient shown in their tool definitions, such as `to={recipient}`.\n\n"
        ))
    }
}

/// Codex's `MultiAgentRoleInstructions::Composed` body: the selected base, the shared hint, the
/// `wait_agent` advice when that tool is offered, and the concurrency slots.
fn compose(
    base: &str,
    tools: &CollaborationTools<'_>,
    omit_update_plan_instructions: bool,
    max_concurrency: usize,
) -> String {
    let base = if omit_update_plan_instructions {
        without_update_plan_instructions(base)
    } else {
        base.to_owned()
    };
    let direct_calls = tools.direct_call_guidance().unwrap_or_default();
    let wait_agent_guidance = if tools.offers(CollaborationKind::WaitAgent) {
        format!("{WAIT_AGENT_USAGE_HINT}\n\n")
    } else {
        String::new()
    };
    format!(
        "{base}\n{direct_calls}{SHARED_DIRECTORY_HINT}\n{wait_agent_guidance}There are {max_concurrency} available concurrency slots, meaning that up to {max_concurrency} agents can be active at once, including you."
    )
}

/// The order Codex's shared hint names the tools in.
const SHARED_TOOL_ORDER: [CollaborationKind; 6] = [
    CollaborationKind::SpawnAgent,
    CollaborationKind::SendMessage,
    CollaborationKind::FollowupTask,
    CollaborationKind::WaitAgent,
    CollaborationKind::InterruptAgent,
    CollaborationKind::ListAgents,
];

const WAIT_AGENT_USAGE_HINT: &str =
    "When calling `wait_agent`, prefer longer waits (minutes) to avoid busy polling.";

const SHARED_DIRECTORY_HINT: &str = r"All agents share the same directory. In detail:
- All agents have access to the same container and filesystem as you.
- All agents use the same current working directory.
- As a result, edits made by one agent are immediately visible to all other agents.
";

const ROOT: &str = r"You are `/root`, the primary agent in a team of agents collaborating to fulfill the user's goals.

At the start of your turn, you are the active agent.
You can spawn sub-agents to handle subtasks, and those sub-agents can spawn their own sub-agents.
All agents in the team, including the agents that you can assign tasks to, are equally intelligent and capable, and have access to the same set of tools.

You can use `spawn_agent` to create a new agent, `followup_task` to give an existing agent a new task and trigger a turn, and `send_message` to pass a message to a running agent without triggering a turn.
Child agents can also spawn their own sub-agents.
You can decide how much context you want to propagate to your sub-agents with the `fork_turns` parameter.

You will receive messages in the analysis channel in the form:
```
Message Type: MESSAGE | FINAL_ANSWER
Task name: <recipient>
Sender: <author>
Payload:
<payload text>
```
They may be addressed as to=/root
";

const SUBAGENT: &str = r"You are an agent in a team of agents collaborating to complete a task.

You can spawn sub-agents to handle subtasks, and those sub-agents can spawn their own sub-agents. All agents in the team, including the agents that you can assign tasks to, are equally intelligent and capable, and have access to the same set of tools.

You can use `spawn_agent` to create a new agent, `followup_task` to give an existing agent a new task and trigger a turn, and `send_message` to pass a message to a running agent.
Child agents can also spawn their own sub-agents.

When you provide a response in the final channel, that content is immediately delivered back to your parent agent.

You will receive messages in the analysis channel in the form:
```
Message Type: NEW_TASK | MESSAGE | FINAL_ANSWER
Task name: <recipient>
Sender: <author>
Payload:
<payload text>
```
You may also see them addressed as to=/root/..., which indicates your identity is /root/...
";

/// Omits checklist-tool guidance from the base of a composed role hint.
fn without_update_plan_instructions(instructions: &str) -> String {
    let lines = instructions.split_inclusive('\n').collect::<Vec<_>>();
    let mut rendered = String::with_capacity(instructions.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim_end();
        if matches!(
            line,
            "## Planning"
                | "## `update_plan`"
                | "## Plan tool"
                | "## Plan Mode vs update_plan tool"
        ) {
            let end = (index + 1..lines.len())
                .find(|&next| lines[next].starts_with("# ") || lines[next].starts_with("## "))
                .unwrap_or(lines.len());
            let is_checklist_section = line != "## Planning"
                || lines[index..end].iter().any(|line| {
                    line.starts_with("You have access to an `update_plan` tool")
                        || line.starts_with("When `update_plan` is available, follow this section")
                });
            if is_checklist_section {
                index = end;
                continue;
            }
        }

        if line == "Progress visibility:"
            && lines
                .get(index + 1)
                .is_some_and(|line| line.starts_with("If update_plan is available"))
        {
            index += 2;
            if lines.get(index).is_some_and(|line| line.trim().is_empty()) {
                index += 1;
            }
            continue;
        }

        if line.starts_with("- Use the plan tool ")
            || line.starts_with("- If you create a checklist or task list,")
        {
            index += 1;
            while index < lines.len()
                && (lines[index].starts_with(' ') || lines[index].starts_with('\t'))
            {
                index += 1;
            }
            continue;
        }

        rendered.push_str(lines[index]);
        index += 1;
    }
    rendered
}
