//! Product command hooks reaching the runtime's host hook dispatcher, from real config layers.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ra_coding::hooks::{HooksConfig, ProductHookRuntime, ProductHookSession, UnappliedHookEffect};
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

fn bound(runtime: &Arc<ProductHookRuntime>) -> ra_runtime::hook::BoundUserHooks {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    runtime.user_hooks().bind(
        Arc::new(RunContext::new(RunId::new("turn-1"), &agent)),
        CancelScope::root(),
        ToolServices::new(),
    )
}

#[tokio::test]
async fn pre_tool_use_block_becomes_a_denial_and_rewrites_are_reported_unapplied() {
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
    let bound = bound(&runtime);
    let call_id = CallId::new("call-1");
    let arguments = serde_json::json!({"command": "rm -rf build"});
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
    let decision = bound
        .dispatch(HookEvent::PreToolUse(ToolHookData::new(
            &patch, &call_id, &arguments,
        )))
        .await
        .unwrap();
    assert_eq!(decision, HookDecision::Continue);
    assert_eq!(
        runtime.take_unapplied_effects(),
        [
            UnappliedHookEffect::AdditionalContext {
                event: HookEventName::PreToolUse,
                contexts: vec!["patch noted".to_owned()],
            },
            UnappliedHookEffect::UpdatedInput {
                tool_use_id: "call-1".to_owned(),
                input: serde_json::json!({"command": "patched"}),
            },
        ]
    );
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
    let bound = bound(&runtime);
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
    assert_eq!(
        decision,
        HookDecision::Block {
            prompt: "run the tests first".to_owned()
        }
    );

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
async fn prompt_stop_and_context_are_reported_without_being_applied() {
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
    let bound = bound(&runtime);
    let session_id = SessionId::new("thread-1");
    let input = [ModelInputItem::Message(Message::user("delete everything"))];

    assert_eq!(
        bound
            .dispatch(HookEvent::SessionStart {
                session_id: &session_id,
                resumed: false
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
        HookDecision::Continue
    );

    assert_eq!(
        runtime.take_unapplied_effects(),
        [
            UnappliedHookEffect::AdditionalContext {
                event: HookEventName::SessionStart,
                contexts: vec!["repository uses pnpm".to_owned()],
            },
            UnappliedHookEffect::StopProcessing {
                event: HookEventName::UserPromptSubmit,
                reason: Some("prompt rejected".to_owned()),
            },
        ]
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
