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

/// Reports the runtime's hook runs on a thread event channel of the test's own.
fn thread_events(
    runtime: &ProductHookRuntime,
) -> tokio::sync::mpsc::UnboundedReceiver<ra_runtime::thread::ThreadEvent> {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    runtime.bind_thread_events(events);
    receiver
}

/// The next completed hook run reported on `events`, which must follow its started report.
async fn next_completed(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<ra_runtime::thread::ThreadEvent>,
) -> ra_protocol::server::protocol::HookCompletedEvent {
    let mut started = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let ra_runtime::thread::ThreadEvent::Notice(notice) = event {
            if notice.method() == ra_protocol::server::protocol::HOOK_STARTED_METHOD {
                started = true;
            } else if notice.method() == ra_protocol::server::protocol::HOOK_COMPLETED_METHOD {
                assert!(started, "a run is reported started before it completes");
                return serde_json::from_value(notice.params().clone()).unwrap();
            }
        }
    }
}

/// Waits for `take` to return background context.
async fn background_context(take: impl Fn() -> Vec<Message>) -> Vec<Message> {
    for _ in 0..500 {
        let messages = take();
        if !messages.is_empty() {
            return messages;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("background context never arrived");
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
    let mut completed = thread_events(&runtime);
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
    let event = next_completed(&mut completed).await;
    assert_eq!(event.run.event_name, HookEventName::PreToolUse);

    let patch = ToolOrigin::new("apply_patch").unwrap();
    let arguments = serde_json::json!({"patch": ORIGINAL_PATCH});
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
        let delivered = if close_first {
            // A closed turn takes nothing; the result waits for the next turn's queue.
            background_context(|| {
                assert!(queue.is_empty());
                let next = ra_core::hook::HookContextQueue::default();
                let _next_bound = bound_with_queue(&runtime, &next);
                // Closed again at once, so a result that arrives later waits for the next try.
                next.close()
            })
            .await
        } else {
            background_context(|| queue.take()).await
        };
        assert_eq!(
            delivered,
            vec![Message::developer("background $unselected")]
        );
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
command = '''if [ -f '{}' ]; then exit 0; fi; touch '{}'; echo 'continue with "waves" & <tides>' >&2; exit 2'''
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
    assert_eq!(
        prompts[0]["fragments"][0]["text"],
        r#"continue with "waves" & <tides>"#
    );
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
            serde_json::json!({"patch": ORIGINAL_PATCH}),
            serde_json::json!({"patch":"replacement patch"}),
        ),
        (
            serde_json::json!(ORIGINAL_PATCH),
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

const ORIGINAL_PATCH: &str = "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch\n";

/// A patch that cannot be parsed runs no PreToolUse hook, as Codex parses a patch while resolving
/// its environment, before its hooks; the call then fails at decoding.
#[tokio::test]
async fn an_unparsable_patch_runs_no_pre_tool_use_hook() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("pre.log");
    let config = hooks_config(
        home.path(),
        &format!(
            r#"
[[hooks.PreToolUse]]
matcher = "^apply_patch$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''cat >> '{}'; printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no"}}}}' '''
"#,
            log.display()
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let origin = ToolOrigin::new("apply_patch").unwrap();
    let id = CallId::new("patch");
    for arguments in [
        serde_json::json!({"patch":"not a patch"}),
        serde_json::json!("not a patch"),
        serde_json::json!({"text": ORIGINAL_PATCH}),
    ] {
        let decision = bound
            .dispatch(HookEvent::PreToolUse(ToolHookData::new(
                &origin, &id, &arguments,
            )))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Continue, "{arguments}");
    }
    assert!(!log.exists(), "no hook ran");

    let arguments = serde_json::json!({"patch": ORIGINAL_PATCH});
    let decision = bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &origin, &id, &arguments,
        )))
        .await
        .unwrap();
    assert!(
        matches!(decision, HookDecision::Deny { .. }),
        "{decision:?}"
    );
    assert!(log.exists(), "a parsable patch runs its hook");
}

/// Codex's `write_stdin` permission-request input, built from the approval the tool prepared:
/// the terminal's session, the input, the launching call, this call, the environment, the
/// launch directory as a file URI, no terminal, and the review's override and grants. A terminal
/// that recorded no launching call or no absolute directory gives `null` for those keys. Asked
/// with no approval, or with one this product cannot read, the hook is given the call's
/// arguments.
#[tokio::test]
async fn write_stdin_permission_requests_describe_the_prepared_approval() {
    let home = TempDir::new().unwrap();
    let capture = home.path().join("hook-input.json");
    let config = hooks_config(
        home.path(),
        &format!(
            r#"
[[hooks.PermissionRequest]]
matcher = "write_stdin"
[[hooks.PermissionRequest.hooks]]
type = "command"
command = """cat > {capture}; printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{{"behavior":"allow"}}}}}}'"""
"#,
            capture = capture.display()
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let origin = ToolOrigin::new("write_stdin").unwrap();
    let call_id = CallId::new("call-input");
    let arguments = serde_json::json!({
        "session_id": "exec-7", "chars": "q\n", "until": "done", "match_text": null,
        "control": null, "yield_time_ms": 1000
    });
    let ask = |state: Option<serde_json::Value>| {
        let bound = &bound;
        let origin = &origin;
        let call_id = &call_id;
        let arguments = &arguments;
        let capture = &capture;
        async move {
            let data = ToolHookData::new(origin, call_id, arguments);
            let data = match &state {
                Some(state) => data.with_approval_state(state),
                None => data,
            };
            let decision = bound
                .dispatch(HookEvent::PermissionRequest(data))
                .await
                .unwrap();
            assert_eq!(decision, HookDecision::Allow);
            let input: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(capture).unwrap()).unwrap();
            assert_eq!(input["tool_name"], "write_stdin");
            input["tool_input"].clone()
        }
    };
    let prepared = |policy_state: serde_json::Value| serde_json::json!({ "run_id": "turn-1", "call_id": "call-input", "policy_state": policy_state });
    let workspace = home.path().canonicalize().unwrap();
    let cwd_uri = ra_paths::PathUri::from_abs_path(
        &ra_paths::AbsolutePathBuf::from_absolute_path_checked(&workspace).unwrap(),
    )
    .to_string();
    // The grant a terminal keeps after a granted directory write, as the product writes it.
    let grants = serde_json::json!({
        "network": {"enabled": true},
        "file_system": {"write": [workspace.join("allowed")]}
    });

    assert_eq!(
        ask(Some(prepared(serde_json::json!({
            "launch_call_id": "call-launch", "session_id": "exec-7", "cwd": workspace,
            "input": "q\n", "reason": "Send input to an existing terminal.",
            "sandbox_permissions": "with_additional_permissions", "additional_permissions": grants
        }))))
        .await,
        serde_json::json!({
            "session_id": "exec-7", "chars": "q\n", "parent_call_id": "call-launch",
            "approval_id": "call-input", "environment_id": "local", "cwd": cwd_uri, "tty": false,
            "sandbox_permissions": "with_additional_permissions", "additional_permissions": grants
        })
    );

    // No launching call, and no directory or one that is not absolute: the keys stay, as null.
    for cwd in [
        serde_json::Value::Null,
        serde_json::json!("relative/dir"),
        serde_json::json!("~/terminal"),
    ] {
        assert_eq!(
            ask(Some(prepared(serde_json::json!({
                "launch_call_id": null, "session_id": "exec-7", "cwd": cwd, "input": "q\n",
                "reason": "r", "sandbox_permissions": "require_escalated"
            }))))
            .await,
            serde_json::json!({
                "session_id": "exec-7", "chars": "q\n", "parent_call_id": null,
                "approval_id": "call-input", "environment_id": "local", "cwd": null,
                "tty": false, "sandbox_permissions": "require_escalated",
                "additional_permissions": null
            }),
            "{cwd}"
        );
    }

    // Nothing prepared, or something this product cannot read: the call's own arguments.
    for state in [
        None,
        Some(serde_json::json!({"apply_patch": {"item_started": true}})),
        Some(prepared(serde_json::json!({"unexpected": true}))),
    ] {
        assert_eq!(ask(state.clone()).await, arguments, "{state:?}");
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

fn network_request(turn_id: &str) -> ra_coding::network_approval::NetworkApprovalRequest {
    ra_coding::network_approval::NetworkApprovalRequest {
        model: Some("test-model".to_owned()),
        approval_policy: ra_config::AskForApproval::OnRequest,
        item_id: "approval-1".to_owned(),
        turn_id: turn_id.to_owned(),
        environment_id: "local".to_owned(),
        reason: "example.com is not in the allowed_domains".to_owned(),
        network_approval_context: ra_protocol::server::protocol::NetworkApprovalContext::new(
            "example.com".to_owned(),
            ra_protocol::server::protocol::NetworkApprovalProtocol::Https,
        ),
        target: "https://example.com:443".to_owned(),
        hook_command: "curl https://example.com".to_owned(),
        hook_run_id: "approval-1#registration-1".to_owned(),
    }
}

#[tokio::test]
async fn permission_request_hooks_decide_a_network_access_as_the_asking_command() {
    let home = TempDir::new().unwrap();
    let capture = home.path().join("hook-input.json");
    let config = hooks_config(
        home.path(),
        &format!(
            r#"
[[hooks.PermissionRequest]]
matcher = "Bash"
[[hooks.PermissionRequest.hooks]]
type = "command"
command = """cat > {capture}; printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{{"behavior":"allow"}}}}}}'"""
"#,
            capture = capture.display()
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let mut completed = thread_events(&runtime);

    let decision = runtime
        .run_permission_request_hooks(
            "turn-1",
            "approval-1#registration-1",
            "curl https://example.com",
            "network-access https://example.com:443",
        )
        .await;

    assert_eq!(
        decision,
        Some(ra_coding::hooks::PermissionRequestDecision::Allow)
    );
    let input: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
    assert_eq!(input["hook_event_name"], "PermissionRequest");
    assert_eq!(input["tool_name"], "Bash");
    assert_eq!(input["tool_input"]["command"], "curl https://example.com");
    assert_eq!(
        input["tool_input"]["description"],
        "network-access https://example.com:443"
    );
    let event = next_completed(&mut completed).await;
    assert_eq!(event.run.event_name, HookEventName::PermissionRequest);
    assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
}

#[tokio::test]
async fn a_thread_proxy_lets_permission_request_hooks_answer_before_its_client() {
    use ra_coding::thread_settings::network_proxy_test_api::ThreadNetworkProxyProbe;

    let home = TempDir::new().unwrap();
    let config = hooks_config(
        home.path(),
        r#"
[[hooks.PermissionRequest]]
matcher = "Bash"
[[hooks.PermissionRequest.hooks]]
type = "command"
command = """printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"blocked by policy hook"}}}'"""
"#,
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let proxy = ThreadNetworkProxyProbe::default();

    // No client is attached, so only a hook can answer: the request would abort otherwise.
    assert!(matches!(
        proxy
            .request_network_approval(network_request("turn-1"))
            .await,
        Err(ra_coding::network_approval::NetworkApprovalError::TurnAborted)
    ));
    proxy.attach_hooks(Arc::clone(&runtime));
    assert_eq!(
        proxy
            .request_network_approval(network_request("turn-1"))
            .await
            .unwrap_err(),
        ra_coding::network_approval::NetworkApprovalError::Rejected(
            "blocked by policy hook".to_owned()
        )
    );
}

/// Binds `runtime`'s hooks to the run `run_id`, a nested run of `parent` when one is given.
fn bound_run(
    runtime: &Arc<ProductHookRuntime>,
    run_id: &str,
    parent: Option<&str>,
    queue: &ra_core::hook::HookContextQueue,
) -> ra_runtime::hook::BoundUserHooks {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    let mut run = RunContext::new(RunId::new(run_id), &agent);
    if let Some(parent) = parent {
        run = run.with_parent_run_id(RunId::new(parent));
    }
    runtime.user_hooks().bind(
        Arc::new(run),
        CancelScope::root(),
        ToolServices::new().with_hook_context(queue.clone()),
    )
}

fn logging(log: &Path) -> String {
    format!("cat >> '{0}'; echo >> '{0}'", log.display())
}

fn logged(log: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn notices(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<ra_runtime::thread::ThreadEvent>,
) -> Vec<(String, Option<String>)> {
    let mut notices = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ra_runtime::thread::ThreadEvent::Notice(notice) = event {
            notices.push((
                notice.method().to_owned(),
                notice.turn_id().map(str::to_owned),
            ));
        }
    }
    notices
}

/// Codex's PreCompact and PostCompact hooks stop with `continue: false`, which the bridge returns
/// as a stop the runtime applies; the request names Codex's trigger label.
#[tokio::test]
async fn compaction_hooks_stop_with_their_reason_and_name_the_trigger() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("hooks.log");
    let stop = r#"printf '%s' '{"continue":false,"stopReason":"not now"}'"#;
    let config = hooks_config(
        home.path(),
        &format!(
            "[[hooks.PreCompact]]\n[[hooks.PreCompact.hooks]]\ntype = \"command\"\ncommand = '''{0}; {1}'''\n[[hooks.PostCompact]]\n[[hooks.PostCompact.hooks]]\ntype = \"command\"\ncommand = '''{0}; {1}'''\n",
            logging(&log),
            stop
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let mut events = thread_events(&runtime);
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_run(&runtime, "turn-1", None, &queue);
    let record = ra_core::item::ItemId::new("context-1.0");
    let compaction = ra_core::item::Compaction::new("summary", Vec::new());

    let pre = bound
        .dispatch(HookEvent::PreCompact {
            record_id: &record,
            trigger: ra_core::hook::CompactTrigger::Automatic,
        })
        .await
        .unwrap();
    let post = bound
        .dispatch(HookEvent::PostCompact {
            record_id: &record,
            trigger: ra_core::hook::CompactTrigger::Manual,
            compaction: &compaction,
        })
        .await
        .unwrap();

    let stopped = HookDecision::Deny {
        message: "not now".to_owned(),
    };
    assert_eq!((pre, post), (stopped.clone(), stopped));
    let inputs = logged(&log);
    assert_eq!(inputs[0]["hook_event_name"], "PreCompact");
    assert_eq!(inputs[0]["trigger"], "auto");
    assert_eq!(inputs[1]["hook_event_name"], "PostCompact");
    assert_eq!(inputs[1]["trigger"], "manual");
    assert_eq!(inputs[0]["turn_id"], "turn-1");
    assert_eq!(notices(&mut events).len(), 4);
}

/// Codex runs Interrupt hooks once for an interrupted turn. The runtime notifies every cancelled
/// run, so a nested run's notification runs no hook.
#[tokio::test]
async fn interrupt_hooks_run_for_the_turn_and_not_for_its_nested_runs() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("hooks.log");
    let config = hooks_config(
        home.path(),
        &format!(
            "[[hooks.Interrupt]]\n[[hooks.Interrupt.hooks]]\ntype = \"command\"\ncommand = '''{}'''\n",
            logging(&log)
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let root = bound_run(&runtime, "turn-1", None, &queue);
    let nested = bound_run(
        &runtime,
        "nested-1",
        Some("turn-1"),
        &ra_core::hook::HookContextQueue::default(),
    );
    let reason = ra_core::cancel::CancelReason::UserInterrupt;

    nested
        .dispatch(HookEvent::Interrupt { reason: &reason })
        .await
        .unwrap();
    assert!(logged(&log).is_empty());
    root.dispatch(HookEvent::Interrupt { reason: &reason })
        .await
        .unwrap();

    let inputs = logged(&log);
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["hook_event_name"], "Interrupt");
    assert_eq!(inputs[0]["turn_id"], "turn-1");
}

/// A hook a nested agent-tool run fires is requested and reported under the thread turn, Codex's
/// turn `sub_id`, rather than under the nested run's own identity.
#[tokio::test]
async fn a_nested_runs_hooks_belong_to_its_turn() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("hooks.log");
    let config = hooks_config(
        home.path(),
        &format!(
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = '''{}'''\n",
            logging(&log)
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let mut events = thread_events(&runtime);
    let _root = bound_run(
        &runtime,
        "turn-1",
        None,
        &ra_core::hook::HookContextQueue::default(),
    );
    let _child = bound_run(
        &runtime,
        "nested-1",
        Some("turn-1"),
        &ra_core::hook::HookContextQueue::default(),
    );
    let grandchild = bound_run(
        &runtime,
        "nested-2",
        Some("nested-1"),
        &ra_core::hook::HookContextQueue::default(),
    );
    let origin = ToolOrigin::new("echo").unwrap();
    let call_id = CallId::new("call-1");
    let arguments = serde_json::json!({});

    grandchild
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &origin, &call_id, &arguments,
        )))
        .await
        .unwrap();

    assert_eq!(logged(&log)[0]["turn_id"], "turn-1");
    assert_eq!(
        notices(&mut events),
        [
            (
                ra_protocol::server::protocol::HOOK_STARTED_METHOD.to_owned(),
                Some("turn-1".to_owned())
            ),
            (
                ra_protocol::server::protocol::HOOK_COMPLETED_METHOD.to_owned(),
                Some("turn-1".to_owned())
            ),
        ]
    );
}

/// A background handler's result reaches the current turn with its warning, and as in Codex's
/// notification filter its run is reported neither as started nor as completed.
#[tokio::test]
async fn background_results_bring_their_warnings_and_are_not_reported_as_runs() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(
        home.path(),
        r#"
[[hooks.PreToolUse]]
[[hooks.PreToolUse.hooks]]
type = "command"
async = true
command = '''printf '%s' '{"systemMessage":"background warning","hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"background context"}}' '''
"#,
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let mut events = thread_events(&runtime);
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_run(&runtime, "turn-1", None, &queue);
    let origin = ToolOrigin::new("echo").unwrap();
    let call_id = CallId::new("call-1");
    let arguments = serde_json::json!({});

    bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &origin, &call_id, &arguments,
        )))
        .await
        .unwrap();
    let context = background_context(|| queue.take()).await;

    assert_eq!(context, vec![Message::developer("background context")]);
    assert_eq!(queue.take_warnings(), vec!["background warning".to_owned()]);
    assert!(notices(&mut events).is_empty());
    runtime.stop_background().await;
}

/// Codex's session shutdown stops background work before its SessionEnd hooks: a background
/// handler still running is aborted and delivers nothing, and SessionEnd's own handlers still run.
#[tokio::test]
async fn session_end_stops_background_handlers_before_running() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("hooks.log");
    let started = home.path().join("started");
    let gate = home.path().join("gate");
    let finished = home.path().join("finished");
    let config = hooks_config(
        home.path(),
        &format!(
            r#"
[[hooks.PreToolUse]]
[[hooks.PreToolUse.hooks]]
type = "command"
async = true
command = '''touch '{started}'; while [ ! -f '{gate}' ]; do sleep 0.01; done; touch '{finished}'; printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","additionalContext":"late"}}}}' '''

[[hooks.SessionEnd]]
[[hooks.SessionEnd.hooks]]
type = "command"
command = '''{log}'''
"#,
            started = started.display(),
            gate = gate.display(),
            finished = finished.display(),
            log = logging(&log),
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_run(&runtime, "turn-1", None, &queue);
    let origin = ToolOrigin::new("echo").unwrap();
    let call_id = CallId::new("call-1");
    let arguments = serde_json::json!({});
    bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &origin, &call_id, &arguments,
        )))
        .await
        .unwrap();
    for _ in 0..500 {
        if started.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(started.exists(), "the background handler started");

    let session_id = SessionId::new("thread-1");
    bound
        .dispatch(HookEvent::SessionEnd {
            session_id: &session_id,
            reason: "other",
        })
        .await
        .unwrap();
    // A handler that survived would finish now and deliver its context.
    std::fs::write(&gate, "go").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(logged(&log)[0]["hook_event_name"], "SessionEnd");
    assert!(!finished.exists(), "the background handler was aborted");
    assert!(queue.take().is_empty());
}

/// Records the runs Stop and Interrupt ask about, and supplies no executor hooks.
#[derive(Default)]
struct ExecutorSources(std::sync::Mutex<Vec<String>>);

impl ra_coding::hooks::ExecutorHookSourceProvider for ExecutorSources {
    fn executor_hook_sources(
        &self,
        run: &RunContext,
    ) -> Vec<ra_coding::plugins::ExecutorPluginHookSource> {
        self.0.lock().unwrap().push(run.run_id().to_string());
        Vec::new()
    }
}

/// Codex adds the executor plugin hooks of the step a Stop or Interrupt concerns; the provider is
/// asked for that run at both, and not for a nested run's Interrupt, which runs no hook.
#[tokio::test]
async fn stop_and_interrupt_ask_for_the_steps_executor_hooks() {
    let home = TempDir::new().unwrap();
    let config = hooks_config(home.path(), "").await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let provider = Arc::new(ExecutorSources::default());
    runtime.set_executor_hook_sources(provider.clone());
    let root = bound_run(
        &runtime,
        "turn-1",
        None,
        &ra_core::hook::HookContextQueue::default(),
    );
    let nested = bound_run(
        &runtime,
        "nested-1",
        Some("turn-1"),
        &ra_core::hook::HookContextQueue::default(),
    );
    let message = Message::assistant("done", OutputPhase::Final);
    let reason = ra_core::cancel::CancelReason::UserInterrupt;

    root.dispatch(HookEvent::Stop(StopHookData::new(
        FinishReason::Final,
        Some(&message),
        &[],
        false,
    )))
    .await
    .unwrap();
    nested
        .dispatch(HookEvent::Interrupt { reason: &reason })
        .await
        .unwrap();
    root.dispatch(HookEvent::Interrupt { reason: &reason })
        .await
        .unwrap();

    assert_eq!(*provider.0.lock().unwrap(), ["turn-1", "turn-1"]);
}

#[tokio::test]
async fn each_dispatch_uses_its_run_model_and_product_policy() {
    use ra_coding::thread_settings::CodingThreadSettingsAdapter;
    use ra_protocol::server::{ThreadSettingsAdapter, protocol::ThreadPolicyOverrides};
    let home = TempDir::new().unwrap();
    let log = home.path().join("fields.jsonl");
    let config = hooks_config(home.path(), &format!(
        "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'command'\ncommand = '''{}'''\n", logging(&log)
    )).await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let adapter = CodingThreadSettingsAdapter::new(
        home.path(),
        ra_config::SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    for (model, policy) in [
        ("first-model", AskForApproval::OnRequest),
        ("changed-model", AskForApproval::Never),
    ] {
        let config = adapter
            .prepare(
                &ra_runtime::runner::RunConfig::new(),
                &ThreadPolicyOverrides::new().with_approval_policy(policy),
            )
            .unwrap();
        let run = RunContext::new(RunId::new("turn-1"), &agent)
            .with_resolved_model_name(model)
            .with_app_context(config.app_context().unwrap().clone());
        runtime
            .user_hooks()
            .bind(Arc::new(run), CancelScope::root(), ToolServices::new())
            .dispatch(HookEvent::UserPromptSubmit {
                input: &[ModelInputItem::Message(Message::user("hello"))],
            })
            .await
            .unwrap();
    }
    let inputs = logged(&log);
    assert_eq!(inputs[0]["model"], "first-model");
    assert_eq!(inputs[0]["permission_mode"], "default");
    assert_eq!(inputs[1]["model"], "changed-model");
    assert_eq!(inputs[1]["permission_mode"], "bypassPermissions");
}

#[tokio::test]
async fn network_hooks_use_the_captured_review_model_and_policy() {
    use ra_coding::thread_settings::network_proxy_test_api::ThreadNetworkProxyProbe;
    let home = TempDir::new().unwrap();
    let log = home.path().join("network-fields.jsonl");
    let config = hooks_config(home.path(), &format!(
        "[[hooks.PermissionRequest]]\n[[hooks.PermissionRequest.hooks]]\ntype = 'command'\ncommand = '''{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PermissionRequest\",\"decision\":{{\"behavior\":\"allow\"}}}}}}' '''\n", logging(&log)
    )).await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let proxy = ThreadNetworkProxyProbe::default();
    proxy.attach_hooks(runtime);
    let mut request = network_request("turn-1");
    request.model = Some("review-model".to_owned());
    request.approval_policy = AskForApproval::Never;
    proxy.request_network_approval(request).await.unwrap();
    let inputs = logged(&log);
    assert_eq!(inputs[0]["model"], "review-model");
    assert_eq!(inputs[0]["permission_mode"], "bypassPermissions");
    let mut request = network_request("turn-1");
    request.model = None;
    proxy.request_network_approval(request).await.unwrap();
    let inputs = logged(&log);
    assert_eq!(inputs[1]["model"], "test-model");
    assert_eq!(inputs[1]["permission_mode"], "default");
}

struct FailingTranscriptRecorder {
    path: std::path::PathBuf,
    calls: std::sync::Mutex<Vec<&'static str>>,
}

#[async_trait::async_trait]
impl ra_core::session::rollout::RolloutRecorder for FailingTranscriptRecorder {
    fn local_path(&self) -> Option<std::path::PathBuf> {
        Some(self.path.clone())
    }
    fn record(&self, _: ra_core::session::rollout::RolloutItem) {}
    async fn persist(
        &self,
        _: ra_core::session::rollout::PersistContext,
    ) -> ra_core::error::Result<()> {
        self.calls.lock().unwrap().push("persist");
        Err(ra_core::error::Error::caller("persistence unavailable"))
    }
    async fn flush(&self) -> ra_core::error::Result<()> {
        self.calls.lock().unwrap().push("flush");
        Err(ra_core::error::Error::caller("flush unavailable"))
    }
}

#[tokio::test]
async fn interrupt_and_session_end_flush_after_query_and_keep_running_on_storage_failure() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("lifecycle.jsonl");
    let config = hooks_config(home.path(), &["Interrupt", "SessionEnd"].map(|event| format!("[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = 'command'\ncommand = '''{}'''\n", logging(&log))).join("\n")).await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let recorder = Arc::new(FailingTranscriptRecorder {
        path: home.path().join("unavailable.jsonl"),
        calls: std::sync::Mutex::new(Vec::new()),
    });
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    // The thread supplies storage while the dispatcher is exercised independently of shutdown's
    // final flush, proving the barriers happen before each hook even when they fail.
    struct NoModel;
    impl ra_core::model::ModelResolver for NoModel {
        fn resolve_model(
            &self,
            _: Option<&str>,
        ) -> ra_core::error::Result<ra_core::model::ResolvedModel> {
            Err(ra_core::error::Error::caller("no inference in this test"))
        }
    }
    let thread = ra_runtime::thread::AgentThread::builder(
        SessionId::new("thread-1"),
        ra_runtime::agent::AgentBinding::direct(agent),
        Arc::new(NoModel),
    )
    .with_rollout_recorder(recorder.clone())
    .build();
    runtime.bind_transcript(thread.downgrade());
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    bound
        .dispatch(HookEvent::Interrupt {
            reason: &ra_core::cancel::CancelReason::UserInterrupt,
        })
        .await
        .unwrap();
    assert_eq!(*recorder.calls.lock().unwrap(), ["persist", "flush"]);
    assert_eq!(logged(&log).len(), 1);
    bound
        .dispatch(HookEvent::SessionEnd {
            session_id: &SessionId::new("thread-1"),
            reason: "other",
        })
        .await
        .unwrap();
    assert_eq!(
        *recorder.calls.lock().unwrap(),
        ["persist", "flush", "persist", "flush"]
    );
    let inputs = logged(&log);
    assert_eq!(inputs.len(), 2);
    for input in inputs {
        assert_eq!(input["transcript_path"], recorder.path.to_str().unwrap());
    }
}

/// A nested run records into its thread's rollout, so SubagentStop reports that transcript as the
/// parent's and no transcript of the agent's own, querying the thread once.
#[tokio::test]
async fn subagent_stop_reports_the_threads_transcript_and_no_agent_transcript() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("subagent-stop.jsonl");
    let config = hooks_config(
        home.path(),
        &format!(
            "[[hooks.SubagentStop]]\n[[hooks.SubagentStop.hooks]]\ntype = 'command'\ncommand = '''{}'''\n",
            logging(&log)
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let recorder = Arc::new(FailingTranscriptRecorder {
        path: home.path().join("thread.jsonl"),
        calls: std::sync::Mutex::new(Vec::new()),
    });
    struct NoModel;
    impl ra_core::model::ModelResolver for NoModel {
        fn resolve_model(
            &self,
            _: Option<&str>,
        ) -> ra_core::error::Result<ra_core::model::ResolvedModel> {
            Err(ra_core::error::Error::caller("no inference in this test"))
        }
    }
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    let thread = ra_runtime::thread::AgentThread::builder(
        SessionId::new("thread-1"),
        ra_runtime::agent::AgentBinding::direct(agent),
        Arc::new(NoModel),
    )
    .with_rollout_recorder(recorder.clone())
    .build();
    runtime.bind_transcript(thread.downgrade());
    let nested = bound_run(
        &runtime,
        "nested-1",
        Some("turn-1"),
        &ra_core::hook::HookContextQueue::default(),
    );
    let message = Message::assistant("done", OutputPhase::Final);

    nested
        .dispatch(HookEvent::SubagentStop {
            parent_run_id: &RunId::new("turn-1"),
            delivery: StopHookData::new(FinishReason::Final, Some(&message), &[], false),
        })
        .await
        .unwrap();

    let inputs = logged(&log);
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["hook_event_name"], "SubagentStop");
    assert_eq!(
        inputs[0]["transcript_path"],
        recorder.path.to_str().unwrap()
    );
    assert!(
        inputs[0]["agent_transcript_path"].is_null(),
        "{}",
        inputs[0]
    );
    assert_eq!(*recorder.calls.lock().unwrap(), ["persist"]);
}

/// Default assembly reserves exec_command; this fixture supplies a real dynamic instance directly.
#[tokio::test]
async fn dynamic_exec_named_tool_keeps_ordinary_hook_name_and_input() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("dynamic.jsonl");
    let config = hooks_config(home.path(), &format!("[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = 'command'\ncommand = '''{}'''\n[[hooks.PostToolUse]]\n[[hooks.PostToolUse.hooks]]\ntype = 'command'\ncommand = '''{}'''", logging(&log), logging(&log))).await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let bound = bound_with_queue(&runtime, &ra_core::hook::HookContextQueue::default());
    let tool = InjectionWireTool {
        origin: ToolOrigin::new("exec_command").unwrap(),
        schema: ra_core::tool::ToolSchema::new(
            "dynamic tool",
            serde_json::json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        )
        .unwrap(),
    };
    use ra_core::tool::Tool;
    let id = CallId::new("dynamic");
    let input = serde_json::json!({"cmd":"ordinary input"});
    let call = || ToolHookData::new(tool.origin(), &id, &input).with_tool(&tool);
    assert_eq!(
        bound.dispatch(HookEvent::PreToolUse(call())).await.unwrap(),
        HookDecision::Continue
    );
    assert_eq!(
        bound
            .dispatch(HookEvent::PostToolUse {
                call: call(),
                output: &ra_core::tool::ToolOutput::text("dynamic output")
            })
            .await
            .unwrap(),
        HookDecision::Continue
    );
    let payloads = logged(&log);
    assert_eq!(payloads.len(), 2);
    for payload in &payloads {
        assert_eq!(payload["tool_name"], "exec_command");
        assert_eq!(payload["tool_input"], input);
    }
    assert_eq!(payloads[1]["tool_response"], "dynamic output");
}

/// An ordinary function a host registered under the name `apply_patch`.
struct PlainApplyPatch {
    origin: ToolOrigin,
    schema: ra_core::tool::ToolSchema,
}

#[async_trait::async_trait]
impl ra_core::tool::Tool for PlainApplyPatch {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ra_core::tool::ToolSchema {
        &self.schema
    }
    async fn call(
        &self,
        _: ra_core::tool::ToolContext<'_>,
    ) -> ra_core::error::Result<ra_core::tool::ToolOutput> {
        Ok(ra_core::tool::ToolOutput::text("ran"))
    }
}

/// Codex chooses the hook payload by the handler a call selected, so a host's ordinary function
/// named `apply_patch` is hooked as an ordinary function: its PreToolUse receives argument text
/// that was not JSON as a string, under its own name without the patch tool's `Write` and `Edit`
/// aliases, and a rewrite reaches it as given. The patch tool itself keeps its patch identity.
#[tokio::test]
async fn a_function_named_apply_patch_is_hooked_as_the_function_it_is() {
    let home = TempDir::new().unwrap();
    let named = home.path().join("named.log");
    let write = home.path().join("write.log");
    let config = hooks_config(
        home.path(),
        &format!(
            r#"
[[hooks.PreToolUse]]
matcher = "^apply_patch$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''cat >> '{}'; echo >> '{}'; printf '%s' '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{{"message":"rewritten"}}}}}}' '''
[[hooks.PreToolUse]]
matcher = "^Write$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''cat >> '{}'; echo >> '{}' '''
"#,
            named.display(),
            named.display(),
            write.display(),
            write.display()
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let plain = PlainApplyPatch {
        origin: ToolOrigin::new("apply_patch").unwrap(),
        schema: ra_core::tool::ToolSchema::new(
            "apply_patch",
            serde_json::json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"],"additionalProperties":false}),
        )
        .unwrap(),
    };
    let id = CallId::new("plain");

    let text = serde_json::json!("{bad");
    let decision = bound
        .dispatch(HookEvent::PreToolUse(
            ToolHookData::new(ra_core::tool::Tool::origin(&plain), &id, &text)
                .with_unparsed_arguments()
                .with_tool(&plain),
        ))
        .await
        .unwrap();
    assert_eq!(
        decision,
        HookDecision::UpdateInput {
            input: serde_json::json!({"message":"rewritten"})
        }
    );
    let parsed = serde_json::json!({"message":"original"});
    let decision = bound
        .dispatch(HookEvent::PreToolUse(
            ToolHookData::new(ra_core::tool::Tool::origin(&plain), &id, &parsed).with_tool(&plain),
        ))
        .await
        .unwrap();
    assert_eq!(
        decision,
        HookDecision::UpdateInput {
            input: serde_json::json!({"message":"rewritten"})
        }
    );
    let inputs = logged(&named);
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["tool_name"], "apply_patch");
    assert_eq!(inputs[0]["tool_input"], text);
    assert_eq!(inputs[1]["tool_input"], parsed);
    assert!(
        logged(&write).is_empty(),
        "the patch tool's alias is not this function's"
    );

    // The patch tool under the same name: its input is the patch command, its aliases match,
    // and argument text that was not JSON runs no hook.
    let workspace = TempDir::new().unwrap();
    let patch_tool = ra_tools::apply_patch::ApplyPatchTool::new(Arc::new(
        ra_exec::fs::RootedFileSystem::open(workspace.path()).unwrap(),
    ))
    .unwrap();
    let patch = serde_json::json!({"patch": ORIGINAL_PATCH});
    bound
        .dispatch(HookEvent::PreToolUse(
            ToolHookData::new(ra_core::tool::Tool::origin(&patch_tool), &id, &patch)
                .with_tool(&patch_tool),
        ))
        .await
        .unwrap();
    let inputs = logged(&named);
    assert_eq!(inputs.len(), 3);
    assert_eq!(
        inputs[2]["tool_input"],
        serde_json::json!({"command": ORIGINAL_PATCH})
    );
    assert_eq!(logged(&write).len(), 1);
    let unparsed = serde_json::json!(ORIGINAL_PATCH);
    let decision = bound
        .dispatch(HookEvent::PreToolUse(
            ToolHookData::new(ra_core::tool::Tool::origin(&patch_tool), &id, &unparsed)
                .with_unparsed_arguments()
                .with_tool(&patch_tool),
        ))
        .await
        .unwrap();
    assert_eq!(decision, HookDecision::Continue);
    assert_eq!(logged(&named).len(), 3);
    assert_eq!(logged(&write).len(), 1);
}

/// Codex's `spawn_agent_function_tools_use_agent_matcher_alias`: `spawn_agent` in the default
/// namespace (none, or `functions`) and in `multi_agent_v1` is named `spawn_agent` and selected by
/// `Agent`, whatever the namespace.
#[tokio::test]
async fn spawn_agent_is_named_spawn_agent_and_matched_by_agent() {
    let home = TempDir::new().unwrap();
    let log = home.path().join("agent.log");
    let config = hooks_config(
        home.path(),
        &format!(
            r#"
[[hooks.PreToolUse]]
matcher = "^Agent$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = '''cat >> '{0}'; echo >> '{0}' '''
"#,
            log.display()
        ),
    )
    .await;
    let runtime = ProductHookRuntime::new(config, session(home.path()), mcp_executor()).unwrap();
    let queue = ra_core::hook::HookContextQueue::default();
    let bound = bound_with_queue(&runtime, &queue);
    let namespaced = |namespace: &str| {
        ToolOrigin::namespaced(
            ra_core::tool::ToolNamespace::new(namespace).unwrap(),
            "spawn_agent",
        )
        .unwrap()
    };
    let origins = [
        ToolOrigin::new("spawn_agent").unwrap(),
        namespaced("functions"),
        namespaced("multi_agent_v1"),
    ];
    let id = CallId::new("call-1");
    let arguments = serde_json::json!({"message": "inspect this repo"});
    for origin in &origins {
        bound
            .dispatch(HookEvent::PreToolUse(ToolHookData::new(
                origin, &id, &arguments,
            )))
            .await
            .unwrap();
    }
    let inputs = logged(&log);
    assert_eq!(inputs.len(), 3);
    for input in inputs {
        assert_eq!(input["tool_name"], "spawn_agent");
        assert_eq!(input["tool_input"], arguments);
    }
}
