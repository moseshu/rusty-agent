// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product hooks behind the runtime's host hook dispatcher.
//!
//! The source core builds each hook request from its session and turn state (`hook_runtime.rs`)
//! and applies the outcome itself. Here the runtime owns dispatch, so each supported event is
//! registered as a [`UserHook`] that builds the same request and returns the control effect the
//! dispatcher can apply: a `PreToolUse` block becomes a denial with the source's message, a
//! `PermissionRequest` decision becomes a grant or denial, and a Stop or `SubagentStop` block becomes
//! a continuation prompt.
//!
//! Context is delivered through the run-owned history queue; native argument rewrites and
//! model-visible result replacements are applied at the runtime admission boundaries.
//! Session stop-processing is returned as a host-owned prompt rejection.
//!
//! Completed hook runs, including background handlers that finish later, are published on
//! [`ProductHookRuntime::subscribe_completed`]. Refreshing the configuration keeps running
//! background handlers, as the source's reconfiguration does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use ra_config::AskForApproval;
use ra_core::error::Result;
use ra_core::hook::HookContextQueue;
use ra_core::hook::{
    HookDecision, HookEvent, HookEventName, ToolHookData, UserHook, UserHookContext,
};
use ra_core::item::{Message, MessageRole, ModelInputItem};
use ra_core::session::SessionId;
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::protocol::HookCompletedEvent;
use ra_runtime::hook::{UserHookRegistration, UserHooks};
use serde_json::Value;
use tokio::sync::broadcast;

use super::events::permission_request::{PermissionRequestDecision, PermissionRequestRequest};
use super::events::post_tool_use::PostToolUseRequest;
use super::events::pre_tool_use::PreToolUseRequest;
use super::events::session_end::SessionEndRequest;
use super::events::session_start::{SessionStartRequest, StartHookTarget};
use super::events::stop::{StopHookTarget, StopRequest};
use super::events::user_prompt_submit::UserPromptSubmitRequest;
use super::{Hooks, HooksConfig};

/// Session-level values the source copies into every hook request.
#[derive(Debug, Clone)]
pub struct ProductHookSession {
    /// Thread identity serialized as `session_id`.
    pub session_id: SessionId,
    /// Host-side working directory; hooks always run on the host.
    pub cwd: AbsolutePathBuf,
    /// Model slug serialized as `model`.
    pub model: String,
    /// Approval policy, mapped to the source's `permission_mode` labels.
    pub approval_policy: AskForApproval,
    /// Transcript file shown to hooks, when the host keeps one.
    pub transcript_path: Option<PathBuf>,
}

/// A hook result the runtime dispatcher cannot apply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnappliedHookEffect {
    /// Text the source would add to the model context.
    AdditionalContext {
        /// Event that produced it.
        event: HookEventName,
        /// The context fragments, in handler order.
        contexts: Vec<String>,
    },
    /// Replacement arguments for a `PreToolUse` call.
    UpdatedInput {
        /// Call the hook rewrote.
        tool_use_id: String,
        /// The rewritten arguments.
        input: Value,
    },
    /// A request to stop processing the turn or session.
    StopProcessing {
        /// Event that produced it.
        event: HookEventName,
        /// The hook's reason, when given.
        reason: Option<String>,
    },
    /// Model-facing feedback or a block after a tool already ran.
    PostToolUseFeedback {
        /// Call the feedback concerns.
        tool_use_id: String,
        /// The feedback message, when given.
        message: Option<String>,
    },
}

/// One thread's product hooks, adapted to the runtime's host hook dispatcher.
pub struct ProductHookRuntime {
    hooks: RwLock<Arc<Hooks>>,
    session: ProductHookSession,
    completed: broadcast::Sender<HookCompletedEvent>,
    unapplied: Mutex<Vec<UnappliedHookEffect>>,
    forwarder: tokio::task::JoinHandle<()>,
    context_delivery: Arc<Mutex<ContextDelivery>>,
}

impl ProductHookRuntime {
    /// Builds hooks from `config` for one thread, as the source does when a session starts.
    ///
    /// # Errors
    ///
    /// When a required managed hook fails to load.
    pub fn new(
        config: HooksConfig,
        session: ProductHookSession,
        mcp_executor: Arc<dyn super::HookMcpExecutor>,
    ) -> anyhow::Result<Arc<Self>> {
        let (hooks, background) = Hooks::new(config, session.session_id.clone(), mcp_executor)?;
        let (completed, _) = broadcast::channel(256);
        let forward = completed.clone();
        let context_delivery = Arc::new(Mutex::new(ContextDelivery::default()));
        let delivery = Arc::clone(&context_delivery);
        let forwarder = tokio::spawn(async move {
            while let Ok(event) = background.recv().await {
                let messages = event
                    .run
                    .entries
                    .iter()
                    .filter(|entry| {
                        matches!(
                            entry.kind,
                            ra_protocol::server::protocol::HookOutputEntryKind::Context
                        )
                    })
                    .map(|entry| Message::developer(entry.text.clone()))
                    .collect();
                delivery
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .deliver_to(event.turn_id.as_deref(), messages, true);
                let _ = forward.send(event);
            }
        });
        Ok(Arc::new(Self {
            hooks: RwLock::new(Arc::new(hooks)),
            session,
            completed,
            unapplied: Mutex::new(Vec::new()),
            forwarder,
            context_delivery,
        }))
    }

    /// Applies a refreshed configuration while background handlers keep running.
    pub fn refresh(&self, config: HooksConfig) {
        let next = Arc::new(self.current().reconfigured(config));
        *self.hooks.write().unwrap_or_else(PoisonError::into_inner) = next;
    }

    /// The hooks used by the next dispatch.
    #[must_use]
    pub fn current(&self) -> Arc<Hooks> {
        self.hooks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Receives completed hook runs, including background handlers.
    #[must_use]
    pub fn subscribe_completed(&self) -> broadcast::Receiver<HookCompletedEvent> {
        self.completed.subscribe()
    }

    /// Legacy diagnostic drain retained for source compatibility. Implemented effects are applied
    /// directly and no longer appear here.
    pub fn take_unapplied_effects(&self) -> Vec<UnappliedHookEffect> {
        std::mem::take(
            &mut *self
                .unapplied
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Registrations for every event the product hooks handle.
    #[must_use]
    pub fn user_hooks(self: &Arc<Self>) -> UserHooks {
        [
            HookEventName::SessionStart,
            HookEventName::SessionEnd,
            HookEventName::UserPromptSubmit,
            HookEventName::PreToolUse,
            HookEventName::PermissionRequest,
            HookEventName::PostToolUse,
            HookEventName::Stop,
            HookEventName::SubagentStop,
        ]
        .into_iter()
        .fold(
            UserHooks::default().with_session_id(self.session.session_id.clone()),
            |hooks, event| {
                hooks.with_hook(UserHookRegistration::new(
                    event,
                    Arc::new(ProductHook {
                        runtime: Arc::clone(self),
                    }),
                ))
            },
        )
    }

    fn publish(&self, events: Vec<HookCompletedEvent>) {
        for event in events {
            let _ = self.completed.send(event);
        }
    }

    fn record_contexts(&self, context: &UserHookContext<'_>, contexts: Vec<String>) {
        self.context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .deliver_to(
                Some(context.run().run_id().as_str()),
                contexts.into_iter().map(Message::developer).collect(),
                false,
            );
    }

    fn permission_mode(&self) -> String {
        hook_permission_mode(self.session.approval_policy)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One arm per event, as the source hook runtime has one function per event."
    )]
    async fn dispatch(&self, context: &UserHookContext<'_>, event: &HookEvent<'_>) -> HookDecision {
        let hooks = self.current();
        let session = &self.session;
        let turn_id = context.run().run_id().to_string();
        match *event {
            HookEvent::SessionStart { source, .. } => {
                let request = SessionStartRequest {
                    session_id: session.session_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path: session.transcript_path.clone(),
                    model: session.model.clone(),
                    permission_mode: self.permission_mode(),
                    target: StartHookTarget::SessionStart { source },
                };
                let outcome = hooks.run_session_start(request, None).await;
                self.publish(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: outcome
                            .stop_reason
                            .unwrap_or_else(|| "Hook stopped prompt processing".to_owned()),
                    };
                }
                HookDecision::Continue
            }
            HookEvent::SessionEnd { .. } => {
                let outcome = hooks
                    .run_session_end(SessionEndRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                    })
                    .await;
                self.publish(outcome.hook_events);
                hooks.shutdown().await;
                HookDecision::Continue
            }
            HookEvent::UserPromptSubmit { input } => {
                let outcome = hooks
                    .run_user_prompt_submit(UserPromptSubmitRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        prompt: user_prompt(input),
                    })
                    .await;
                self.publish(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: outcome
                            .stop_reason
                            .unwrap_or_else(|| "Hook stopped prompt processing".to_owned()),
                    };
                }
                HookDecision::Continue
            }
            HookEvent::PreToolUse(call) => {
                let tool = HookToolName::for_call(&call);
                let tool_use_id = call.call_id().to_string();
                let outcome = hooks
                    .run_pre_tool_use(PreToolUseRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        tool_name: tool.name.clone(),
                        matcher_aliases: tool.matcher_aliases.clone(),
                        tool_use_id: tool_use_id.clone(),
                        tool_input: hook_input(&call),
                    })
                    .await;
                self.publish(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if !outcome.should_block {
                    return outcome
                        .updated_input
                        .map_or(HookDecision::Continue, |input| {
                            match native_input(&call, input) {
                                Ok(input) => HookDecision::UpdateInput { input },
                                Err(message) => HookDecision::Deny { message },
                            }
                        });
                }
                let Some(reason) = outcome.block_reason else {
                    return HookDecision::Continue;
                };
                HookDecision::Deny {
                    message: blocked_tool_message(&tool, &hook_input(&call), &reason),
                }
            }
            HookEvent::PermissionRequest(call) => {
                let tool = HookToolName::for_call(&call);
                let outcome = hooks
                    .run_permission_request(PermissionRequestRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.to_path_buf(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        tool_name: tool.name,
                        matcher_aliases: tool.matcher_aliases,
                        run_id_suffix: call.call_id().to_string(),
                        tool_input: hook_input(&call),
                    })
                    .await;
                self.publish(outcome.hook_events);
                match outcome.decision {
                    Some(PermissionRequestDecision::Allow) => HookDecision::Allow,
                    Some(PermissionRequestDecision::Deny { message }) => {
                        HookDecision::Deny { message }
                    }
                    None => HookDecision::Continue,
                }
            }
            HookEvent::PostToolUse { call, output } => {
                let tool = HookToolName::for_call(&call);
                let tool_response = output.as_text().map_or_else(
                    || serde_json::to_value(output).unwrap_or(Value::Null),
                    |text| Value::String(text.to_owned()),
                );
                let outcome = hooks
                    .run_post_tool_use(PostToolUseRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        subagent: None,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        tool_name: tool.name,
                        matcher_aliases: tool.matcher_aliases,
                        tool_use_id: call.call_id().to_string(),
                        tool_input: hook_input(&call),
                        tool_response,
                    })
                    .await;
                self.publish(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if outcome.should_block {
                    return HookDecision::Deny {
                        message: outcome.feedback_message.unwrap_or_else(|| {
                            "PostToolUse hook blocked the tool result".to_owned()
                        }),
                    };
                }
                outcome
                    .feedback_message
                    .map_or(HookDecision::Continue, |message| {
                        HookDecision::ReplaceOutput { message }
                    })
            }
            HookEvent::Stop(delivery) | HookEvent::SubagentStop { delivery, .. } => {
                let event_name = event.name();
                let target = if event_name == HookEventName::SubagentStop {
                    StopHookTarget::SubagentStop {
                        agent_id: context.run().run_id().to_string(),
                        agent_type: context.run().agent().name().to_owned(),
                        agent_transcript_path: session.transcript_path.clone(),
                    }
                } else {
                    StopHookTarget::Stop
                };
                let mut outcome = hooks
                    .run_stop(StopRequest {
                        session_id: session.session_id.clone(),
                        turn_id,
                        cwd: session.cwd.clone(),
                        transcript_path: session.transcript_path.clone(),
                        model: session.model.clone(),
                        permission_mode: self.permission_mode(),
                        request_metadata: None,
                        stop_hook_active: delivery.stop_hook_active(),
                        last_assistant_message: delivery
                            .message()
                            .map(ra_core::item::Message::text_content),
                        target,
                    })
                    .await;
                self.publish(std::mem::take(&mut outcome.hook_events));
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: outcome
                            .stop_reason
                            .unwrap_or_else(|| "Stop hook stopped processing".to_owned()),
                    };
                }
                if !outcome.should_block {
                    return HookDecision::Continue;
                }
                let prompt = outcome
                    .continuation_fragments
                    .iter()
                    .filter(|fragment| !fragment.hook_run_id.trim().is_empty())
                    .map(|fragment| {
                        format!(
                            "<hook_prompt hook_run_id=\"{}\">{}</hook_prompt>",
                            escape_xml(&fragment.hook_run_id),
                            escape_xml(&fragment.text)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                HookDecision::Block { prompt }
            }
            _ => HookDecision::Continue,
        }
    }
}

impl Drop for ProductHookRuntime {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

struct ProductHook {
    runtime: Arc<ProductHookRuntime>,
}

#[async_trait]
impl UserHook for ProductHook {
    fn name(&self) -> &'static str {
        "product hooks"
    }

    fn bind_context(&self, run: &ra_core::context::RunContext, output: &HookContextQueue) {
        let mut delivery = self
            .runtime
            .context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        delivery.queues.retain(|_, queue| !queue.is_closed());
        delivery
            .queues
            .insert(run.run_id().to_string(), output.clone());
        if run.parent_run_id().is_none() {
            delivery.root = Some(output.clone());
            let pending = std::mem::take(&mut delivery.pending);
            delivery.deliver_to(Some(run.run_id().as_str()), pending, true);
        }
    }

    async fn call(
        &self,
        context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        Ok(self.runtime.dispatch(context, event).await)
    }
}

/// The source's hook-facing tool identity: the serialized name and extra matcher inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HookToolName {
    name: String,
    matcher_aliases: Vec<String>,
}

impl HookToolName {
    fn for_call(call: &ToolHookData<'_>) -> Self {
        let origin = call.origin();
        match origin.name() {
            // Shell-like tools keep the historical hook name.
            "exec_command" | "shell" | "shell_command" | "local_shell"
                if origin.namespace().is_none() =>
            {
                Self {
                    name: "Bash".to_owned(),
                    matcher_aliases: Vec::new(),
                }
            }
            // `Write` and `Edit` select the same handlers for edit-oriented hook configurations.
            "apply_patch" if origin.namespace().is_none() => Self {
                name: "apply_patch".to_owned(),
                matcher_aliases: vec!["Write".to_owned(), "Edit".to_owned()],
            },
            "spawn_agent" if origin.namespace().is_none() => Self {
                name: "spawn_agent".to_owned(),
                matcher_aliases: vec!["Agent".to_owned()],
            },
            _ => Self {
                name: origin.qualified_name().to_owned(),
                matcher_aliases: Vec::new(),
            },
        }
    }
}

fn blocked_tool_message(tool: &HookToolName, input: &Value, reason: &str) -> String {
    if (tool.name == "Bash" || tool.name == "apply_patch")
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        format!("Command blocked by PreToolUse hook: {reason}. Command: {command}")
    } else {
        format!(
            "Tool call blocked by PreToolUse hook: {reason}. Tool: {}",
            tool.name
        )
    }
}

fn hook_permission_mode(approval_policy: AskForApproval) -> String {
    // Every policy that may still ask the user maps to the source's `default` label.
    if matches!(approval_policy, AskForApproval::Never) {
        "bypassPermissions"
    } else {
        "default"
    }
    .to_string()
}

fn user_prompt(input: &[ModelInputItem]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) if message.role() == MessageRole::User => {
                Some(message.text_content())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default)]
struct ContextDelivery {
    queues: std::collections::HashMap<String, HookContextQueue>,
    root: Option<HookContextQueue>,
    pending: Vec<Message>,
}
impl ContextDelivery {
    fn deliver_to(&mut self, turn_id: Option<&str>, messages: Vec<Message>, background: bool) {
        if messages.is_empty() {
            return;
        }
        let queue = turn_id
            .and_then(|id| self.queues.get(id))
            .or(self.root.as_ref());
        let remaining = match queue {
            Some(queue) => {
                let result = if background {
                    queue.push_background(messages)
                } else {
                    queue.push(messages)
                };
                result.err().unwrap_or_default()
            }
            None => messages,
        };
        self.pending.extend(remaining);
    }
}

fn hook_input(call: &ToolHookData<'_>) -> Value {
    if call.origin().namespace().is_none() {
        match call.origin().name() {
            "exec_command" => {
                return serde_json::json!({"command": call.arguments().get("cmd").cloned().unwrap_or(Value::Null)});
            }
            "apply_patch" => {
                return serde_json::json!({"command": call.arguments().as_str().map(str::to_owned).or_else(|| call.arguments().get("patch").and_then(Value::as_str).map(str::to_owned))});
            }
            _ => {}
        }
    }
    call.arguments().clone()
}
fn native_input(call: &ToolHookData<'_>, input: Value) -> std::result::Result<Value, String> {
    if call.origin().namespace().is_none()
        && matches!(call.origin().name(), "exec_command" | "apply_patch")
    {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "hook input rewrite requires a string command".to_owned())?;
        if call.origin().name() == "apply_patch" {
            return match call.arguments() {
                Value::String(_) => Ok(Value::String(command.to_owned())),
                Value::Object(_) => {
                    let mut native = call.arguments().clone();
                    native["patch"] = Value::String(command.to_owned());
                    Ok(native)
                }
                _ => Err(
                    "hook input rewrite requires string or object apply_patch arguments".to_owned(),
                ),
            };
        }
        let mut native = call.arguments().clone();
        native
            .as_object_mut()
            .ok_or_else(|| "hook input rewrite requires exec_command arguments".to_owned())?
            .insert("cmd".to_owned(), Value::String(command.to_owned()));
        return Ok(native);
    }
    Ok(input)
}
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
