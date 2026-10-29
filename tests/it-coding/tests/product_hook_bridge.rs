//! Product command hooks reaching the runtime's host hook dispatcher, from real config layers.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ra_coding::hooks::{HooksConfig, ProductHookRuntime, ProductHookSession};
use ra_coding::plugins::{PluginConfigManager, PluginServices};
use ra_coding::skills::HostSkillsService;
use ra_config::{AskForApproval, ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_core::agent::{AgentId, AgentSpec};
use ra_core::cancel::CancelScope;
use ra_core::context::RunContext;
use ra_core::finish::FinishReason;
use ra_core::hook::{HookDecision, HookEvent, HookEventName, StopHookData, ToolHookData};
use ra_core::item::{CallId, Message, ModelInputItem, OutputPhase};
use ra_core::session::SessionId;
use ra_core::state::RunId;
use ra_core::tool::{ToolOrigin, ToolServices};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_paths::AbsolutePathBuf;
use tempfile::TempDir;

struct NoMcp;

impl ra_coding::hooks::HookMcpExecutor for NoMcp {
    fn execute(
        &self,
        _: ra_coding::hooks::HookMcpCall,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async { anyhow::bail!("no MCP hooks are configured in these fixtures") })
    }
}

fn mcp_executor() -> Arc<dyn ra_coding::hooks::HookMcpExecutor> {
    Arc::new(NoMcp)
}

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}

async fn hooks_config(home: &Path, hooks: &str) -> HooksConfig {
    std::fs::write(
        home.join("config.toml"),
        format!("[features]\nhooks = true\n[skills.bundled]\nenabled = false\n{hooks}"),
    )
    .unwrap();
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home),
        absolute(home),
        ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let services = PluginServices::new(
        manager,
        Arc::new(HostSkillsService::new(absolute(home), false)),
    );
    let mut config = services
        .snapshot(absolute(home))
        .await
        .unwrap()
        .hooks_config();
    // These fixtures are not trusted interactively; trust itself is covered by the engine suite.
    config.bypass_hook_trust = true;
    config
}

fn session(home: &Path) -> ProductHookSession {
    ProductHookSession {
        session_id: SessionId::new("thread-1"),
        cwd: absolute(home),
        model: "test-model".to_owned(),
        approval_policy: AskForApproval::OnRequest,
        transcript_path: None,
    }
}

fn bound_with_queue(
    runtime: &Arc<ProductHookRuntime>,
    queue: &ra_core::hook::HookContextQueue,
) -> ra_runtime::hook::BoundUserHooks {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    runtime.user_hooks().bind(
        Arc::new(RunContext::new(RunId::new("turn-1"), &agent)),
        CancelScope::root(),
        ToolServices::new().with_hook_context(queue.clone()),
    )
}

#[tokio::test]
async fn pre_tool_use_block_and_native_rewrite_reach_dispatcher() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(
        home.path(),
        r#"
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo 'no deletes' >&2; exit 2"

[[hooks.PreToolUse]]
matcher = "^apply_patch$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"updatedInput\":{\"command\":\"patched\"},\"additionalContext\":\"patch noted\"}}'"
"#,
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let mut completed = runtime.subscribe_completed();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let call_id = CallId::new("call-1");
    let arguments = serde_json::json!({"cmd": "rm -rf build"});
    let shell = ToolOrigin::new("exec_command").unwrap();

    let decision = bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &shell, &call_id, &arguments,
        )))
        .await
        .unwrap();

    assert_eq!(
        decision,
        HookDecision::Deny {
            message: "Command blocked by PreToolUse hook: no deletes. Command: rm -rf build"
                .to_owned()
        }
    );
    let event = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.run.event_name, HookEventName::PreToolUse);

    let patch = ToolOrigin::new("apply_patch").unwrap();
    let arguments = serde_json::json!({"patch":"original patch"});
    let decision = bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &patch, &call_id, &arguments,
        )))
        .await
        .unwrap();
    assert_eq!(
        decision,
        HookDecision::UpdateInput {
            input: serde_json::json!({"patch":"patched"})
        }
    );
    assert!(runtime.take_unapplied_effects().is_empty());
    assert_eq!(queue.take(), vec![Message::developer("patch noted")]);
}

#[tokio::test]
async fn stop_block_becomes_a_continuation_prompt_and_refresh_takes_new_handlers() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(
        home.path(),
        r#"
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "echo 'run the tests first' >&2; exit 2"
"#,
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let message = Message::assistant("done", OutputPhase::Final);

    let decision = bound
        .dispatch(HookEvent::Stop(StopHookData::new(
            FinishReason::Final,
            Some(&message),
            &[],
            false,
        )))
        .await
        .unwrap();
    let HookDecision::Block { prompt } = decision else {
        panic!("stop continuation expected")
    };
    assert!(prompt.starts_with("<hook_prompt hook_run_id=\""));
    assert!(prompt.ends_with(">run the tests first</hook_prompt>"));

    runtime.refresh(hooks_config(home.path(), "").await);
    let decision = bound
        .dispatch(HookEvent::Stop(StopHookData::new(
            FinishReason::Final,
            Some(&message),
            &[],
            true,
        )))
        .await
        .unwrap();
    assert_eq!(decision, HookDecision::Continue);
}

#[tokio::test]
async fn prompt_rejection_and_context_are_applied_at_the_host_boundary() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(
        home.path(),
        r#"
[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "echo 'prompt rejected' >&2; exit 2"

[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "echo 'repository uses pnpm'"
"#,
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let session_id = SessionId::new("thread-1");
    let input = [ModelInputItem::Message(Message::user("delete everything"))];

    assert_eq!(
        bound
            .dispatch(HookEvent::SessionStart {
                session_id: &session_id,
                resumed: false,
                source: ra_core::hook::SessionStartSource::Startup,
            })
            .await
            .unwrap(),
        HookDecision::Continue
    );
    assert_eq!(
        bound
            .dispatch(HookEvent::UserPromptSubmit { input: &input })
            .await
            .unwrap(),
        HookDecision::Deny {
            message: "prompt rejected".to_owned()
        }
    );
    assert!(runtime.take_unapplied_effects().is_empty());
    assert_eq!(
        queue.take(),
        vec![Message::developer("repository uses pnpm")]
    );
}

#[tokio::test]
async fn bound_hook_runtime_is_rebuilt_after_each_invalidation() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(home.path(), "").await;
    let manager = PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home.path()),
        absolute(home.path()),
        ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
        Arc::new(NoopThreadConfigLoader),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let services = PluginServices::new(
        manager,
        Arc::new(HostSkillsService::new(absolute(home.path()), false)),
    );
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let binding = services.bind_hook_runtime(&runtime, absolute(home.path()));
    assert_eq!(
        binding
            .wait_for(services.generation())
            .await
            .unwrap()
            .error(),
        None
    );
    let before = runtime.current();

    services.invalidate();
    binding.wait_for(services.generation()).await.unwrap();

    assert!(!Arc::ptr_eq(&before, &runtime.current()));
    drop(binding);
    let after = runtime.current();
    services.invalidate();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        Arc::ptr_eq(&after, &runtime.current()),
        "a dropped binding stops refreshing"
    );
}

#[tokio::test]
async fn command_rewrite_preserves_exec_options_and_post_feedback_preserves_success() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(home.path(), r#"
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"rewritten"}}}' '''
[[hooks.PostToolUse]]
matcher = "Bash"
[[hooks.PostToolUse.hooks]]
type = "command"
command = '''printf '%s' '{"decision":"block","reason":"review the output"}' '''
"#).await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let origin = ToolOrigin::new("exec_command").unwrap();
    let id = CallId::new("command");
    let input = serde_json::json!({"cmd":"original", "yield_time_ms":1000, "workdir":"/tmp"});
    let decision = bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &origin, &id, &input,
        )))
        .await
        .unwrap();
    assert_eq!(
        decision,
        HookDecision::UpdateInput {
            input: serde_json::json!({"cmd":"rewritten", "yield_time_ms":1000, "workdir":"/tmp"})
        }
    );
    let output = ra_core::tool::ToolOutput::text("original output");
    let decision = bound
        .dispatch(HookEvent::PostToolUse {
            call: ToolHookData::new(&origin, &id, &input),
            output: &output,
        })
        .await
        .unwrap();
    assert_eq!(
        decision,
        HookDecision::Deny {
            message: "review the output".to_owned()
        }
    );
    assert!(runtime.take_unapplied_effects().is_empty());
}

#[tokio::test]
async fn background_context_delivers_during_a_run_or_before_the_next_prompt() {
    for close_first in [false, true] {
        let home = TempDir::new().unwrap();
        let gate = home.path().join("release");
        let config = hooks_config(home.path(), &format!(r#"
[[hooks.PreToolUse]]
[[hooks.PreToolUse.hooks]]
type = "command"
async = true
command = '''while [ ! -f '{}' ]; do sleep 0.01; done; printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","additionalContext":"background $unselected"}}}}' '''
"#, gate.display())).await;
        let runtime =
            ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
        let mut completed = runtime.subscribe_completed();
        let queue = ra_core::hook::HookContextQueue::default();
        let bound = bound_with_queue(&runtime, &queue);
        let origin = ToolOrigin::new("echo").unwrap();
        let id = CallId::new("background");
        let args = serde_json::json!({});
        assert_eq!(
            bound
                .dispatch(HookEvent::PreToolUse(ToolHookData::new(
                    &origin, &id, &args
                )))
                .await
                .unwrap(),
            HookDecision::Continue
        );
        if close_first {
            assert!(queue.close().is_empty());
        }
        std::fs::write(&gate, "release").unwrap();
        tokio::time::timeout(Duration::from_secs(5), completed.recv())
            .await
            .unwrap()
            .unwrap();
        if close_first {
            assert!(queue.is_empty());
            let next = ra_core::hook::HookContextQueue::default();
            let _next_bound = bound_with_queue(&runtime, &next);
            assert_eq!(
                next.take(),
                vec![Message::developer("background $unselected")]
            );
        } else {
            assert_eq!(
                queue.take(),
                vec![Message::developer("background $unselected")]
            );
        }
        runtime.current().shutdown().await;
    }
}

struct InjectionWireTool {
    origin: ToolOrigin,
    schema: ra_core::tool::ToolSchema,
}
#[async_trait::async_trait]
impl ra_core::tool::Tool for InjectionWireTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ra_core::tool::ToolSchema {
        &self.schema
    }
    async fn call(
        &self,
        context: ra_core::tool::ToolContext<'_>,
    ) -> ra_core::error::Result<ra_core::tool::ToolOutput> {
        assert_eq!(
            context.arguments(),
            &serde_json::json!({"message":"rewritten"})
        );
        Ok(ra_core::tool::ToolOutput::text("original output"))
    }
}
struct InjectionWireModel(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl ra_core::model::Model for InjectionWireModel {
    async fn get_response(
        &self,
        request: ra_core::model::ModelRequest,
    ) -> ra_core::error::Result<ra_core::item::ModelResponse> {
        use ra_core::item::{ItemId, ModelResponse, RunItem, RunItemKind, ToolCall};
        let step = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let contexts: Vec<_> = request
            .input()
            .iter()
            .filter_map(|i| match i {
                ModelInputItem::Message(m) if m.role() == ra_core::item::MessageRole::Developer => {
                    Some(m.text_content())
                }
                _ => None,
            })
            .collect();
        let kind = match step {
            0 => {
                assert_eq!(contexts, ["session context", "prompt context"]);
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("wire-call"),
                    "echo",
                    serde_json::json!({"message":"original"}),
                ))
            }
            1 => {
                assert_eq!(
                    contexts,
                    [
                        "session context",
                        "prompt context",
                        "pre context",
                        "post context"
                    ]
                );
                let index = request
                    .input()
                    .iter()
                    .position(|i| matches!(i, ModelInputItem::ToolCallOutput(_)))
                    .unwrap();
                assert!(
                    matches!(&request.input()[index + 1], ModelInputItem::Message(m) if m == &Message::developer("pre context"))
                );
                let ModelInputItem::ToolCallOutput(output) = &request.input()[index] else {
                    unreachable!()
                };
                assert!(!output.is_error());
                let stored = ra_core::tool::ToolOutput::from_stored(output.output())?.unwrap();
                assert_eq!(stored.as_text(), Some("original output"));
                assert_eq!(
                    stored.model_blocks(),
                    vec![ra_core::tool::ToolOutputBlock::text("replacement feedback")]
                );
                RunItemKind::Message(Message::assistant("candidate", OutputPhase::Final))
            }
            2 => {
                assert!(request.input().iter().any(|i| matches!(i, ModelInputItem::Message(m) if m.text_content().contains("<hook_prompt hook_run_id="))));
                RunItemKind::Message(Message::assistant("final", OutputPhase::Final))
            }
            _ => panic!("unexpected extra sampling"),
        };
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("wire-{step}")),
            kind,
        )]))
    }
}
struct InjectionWireResolver(Arc<InjectionWireModel>);
impl ra_core::model::ModelResolver for InjectionWireResolver {
    fn resolve_model(
        &self,
        _: Option<&str>,
    ) -> ra_core::error::Result<ra_core::model::ResolvedModel> {
        use ra_core::model::{
            ApiProtocol, ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
        };
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("wire"),
                Some("wire".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

#[tokio::test]
async fn app_server_runs_command_hooks_and_projects_hook_prompts_without_developer_ui_items() {
    use ra_protocol::{
        frame::{JsonRpcMessage, JsonRpcRequest, RequestId},
        server::{AppServer, test_api::ThreadLoader},
        transport::{ConnectionId, ConnectionOrigin, TransportEvent, test_api::connection_opened},
    };
    use tokio::sync::mpsc;
    let home = TempDir::new().unwrap();
    let stop_gate = home.path().join("stop-ran");
    let config = hooks_config(home.path(), &format!(r#"
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "echo 'session context'"
[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = '''printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"UserPromptSubmit","additionalContext":"prompt context"}}}}' '''
[[hooks.PreToolUse]]
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{{"message":"rewritten"}},"additionalContext":"pre context"}}}}' '''
[[hooks.PostToolUse]]
[[hooks.PostToolUse.hooks]]
type = "command"
command = '''printf '%s' '{{"continue":false,"stopReason":"replacement feedback","hookSpecificOutput":{{"hookEventName":"PostToolUse","additionalContext":"post context"}}}}' '''
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = '''if [ -f '{}' ]; then exit 0; fi; touch '{}'; echo 'continue & verify' >&2; exit 2'''
"#, stop_gate.display(), stop_gate.display())).await;
    let hooks = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let model = Arc::new(InjectionWireModel(std::sync::atomic::AtomicUsize::new(0)));
    let agent = AgentSpec::builder().id(AgentId::new("wire-agent")).name("Wire")
        .tools(vec![Arc::new(InjectionWireTool { origin: ToolOrigin::new("echo").unwrap(), schema: ra_core::tool::ToolSchema::new("echo", serde_json::json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"],"additionalProperties":false})).unwrap() }) as Arc<dyn ra_core::tool::Tool>]).build().unwrap();
    let thread = ra_runtime::thread::AgentThread::builder(
        SessionId::new("wire-thread"),
        ra_runtime::agent::AgentBinding::direct(agent),
        Arc::new(InjectionWireResolver(model.clone())),
    )
    .with_config(ra_runtime::runner::RunConfig::new().with_user_hooks(hooks.user_hooks()))
    .build();
    let server = AppServer::new(home.path()).unwrap();
    let loader = ThreadLoader::new(&server);
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let (writer, mut messages) = mpsc::channel(128);
    events
        .send(connection_opened(
            ConnectionId::new(1),
            ConnectionOrigin::WebSocket,
            writer,
            None,
        ))
        .await
        .unwrap();
    events
        .send(TransportEvent::IncomingMessage {
            connection_id: ConnectionId::new(1),
            message: JsonRpcMessage::Request(
                JsonRpcRequest::new(RequestId::Integer(1), "initialize").with_params(
                    serde_json::json!({"clientInfo":{"name":"injection-test","version":"1"}}),
                ),
            ),
        })
        .await
        .unwrap();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), messages.recv())
            .await
            .unwrap()
            .unwrap();
        if serde_json::to_value(message.message()).unwrap()["id"] == 1 {
            break;
        }
    }
    loader.load(thread.clone(), &[ConnectionId::new(1)]).await;
    events.send(TransportEvent::IncomingMessage { connection_id: ConnectionId::new(1), message: JsonRpcMessage::Request(JsonRpcRequest::new(RequestId::Integer(2), "turn/start").with_params(serde_json::json!({"threadId":"wire-thread","input":[{"type":"text","text":"prompt"}]}))) }).await.unwrap();
    let mut projected = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), messages.recv())
            .await
            .unwrap()
            .unwrap();
        let message = serde_json::to_value(message.message()).unwrap();
        assert!(message.get("error").is_none(), "{message}");
        if message["method"] == "item/completed" {
            projected.push(message["params"]["item"].clone());
        }
        if message["method"] == "turn/completed" {
            break;
        }
    }
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    let prompts: Vec<_> = projected
        .iter()
        .filter(|i| i["type"] == "hookPrompt")
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["fragments"][0]["text"], "continue & verify");
    assert!(prompts[0]["fragments"][0]["hookRunId"].is_string());
    assert_eq!(
        projected
            .iter()
            .filter(|i| i["type"] == "userMessage")
            .count(),
        1
    );
    thread.shutdown().await;
    drop(events);
    task.await.unwrap();
}

#[tokio::test]
async fn stop_processing_suppresses_only_its_turns_legacy_notify_even_after_refresh() {
    let home = TempDir::new().unwrap();
    let mut config = hooks_config(
        home.path(),
        r#"
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = '''printf '%s' '{"continue":false,"stopReason":"stop delivery"}' '''
"#,
    )
    .await;
    config.legacy_notify_argv = Some(vec!["true".to_owned()]);
    let runtime =
        ProductHookRuntime::new(config.clone(), session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    assert_eq!(
        bound
            .dispatch(HookEvent::Stop(StopHookData::new(
                FinishReason::Final,
                None,
                &[],
                false
            )))
            .await
            .unwrap(),
        HookDecision::Deny {
            message: "stop delivery".to_owned()
        }
    );
    runtime.refresh(config);
    let payload = |turn_id: &str| ra_coding::hooks::HookPayload {
        session_id: SessionId::new("thread-1"),
        cwd: absolute(home.path()),
        client: None,
        triggered_at: "2026-10-29T00:00:00Z".parse().unwrap(),
        hook_event: ra_coding::hooks::HookEvent::AfterAgent {
            event: ra_coding::hooks::HookEventAfterAgent {
                thread_id: SessionId::new("thread-1"),
                turn_id: turn_id.to_owned(),
                input_messages: Vec::new(),
                last_assistant_message: None,
            },
        },
    };
    assert!(
        runtime
            .current()
            .dispatch(payload("turn-1"))
            .await
            .is_empty()
    );
    assert_eq!(runtime.current().dispatch(payload("turn-2")).await.len(), 1);
    assert!(runtime.take_unapplied_effects().is_empty());
}

#[tokio::test]
async fn apply_patch_rewrite_keeps_object_and_freeform_native_shapes() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(home.path(), r#"
[[hooks.PreToolUse]]
matcher = "^apply_patch$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"replacement patch"}}}' '''
"#).await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let origin = ToolOrigin::new("apply_patch").unwrap();
    let id = CallId::new("patch");
    for (arguments, expected) in [
        (
            serde_json::json!({"patch":"original patch"}),
            serde_json::json!({"patch":"replacement patch"}),
        ),
        (
            serde_json::json!("original patch"),
            serde_json::json!("replacement patch"),
        ),
    ] {
        let decision = bound
            .dispatch(HookEvent::PreToolUse(ToolHookData::new(
                &origin, &id, &arguments,
            )))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::UpdateInput { input: expected });
    }
}

#[tokio::test]
async fn session_start_command_matchers_receive_all_lifecycle_sources() {
    use ra_core::hook::SessionStartSource;
    let home = TempDir::new().unwrap();
    let mut config = String::new();
    for source in [
        SessionStartSource::Startup,
        SessionStartSource::Resume,
        SessionStartSource::Clear,
        SessionStartSource::Compact,
        SessionStartSource::Fork,
    ] {
        config.push_str(&format!("\n[[hooks.SessionStart]]\nmatcher = '{}'\n[[hooks.SessionStart.hooks]]\ntype = 'command'\ncommand = 'echo {}'\n", source.as_str(), source.as_str()));
    }
    let runtime = ProductHookRuntime::new(
        hooks_config(home.path(), &config).await,
        session(home.path()),
        mcp_executor(),
    )
    .unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let id = SessionId::new("thread-1");
    for source in [
        SessionStartSource::Startup,
        SessionStartSource::Resume,
        SessionStartSource::Clear,
        SessionStartSource::Compact,
        SessionStartSource::Fork,
    ] {
        assert_eq!(
            bound
                .dispatch(HookEvent::SessionStart {
                    session_id: &id,
                    resumed: source == SessionStartSource::Resume,
                    source
                })
                .await
                .unwrap(),
            HookDecision::Continue
        );
        assert_eq!(queue.take(), [Message::developer(source.as_str())]);
    }
}
