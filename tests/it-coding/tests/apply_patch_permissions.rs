//! A patch under fixed permission rules and the coding presets: whether it is asked about stays
//! with the permission stage, and a patch is verified before anybody is asked about it. A fixed
//! deny and `plan` refuse it before its hooks; a fixed allow, `acceptEdits` and
//! `bypassPermissions` apply it unasked; a fixed ask verifies it and announces it before the
//! permission hook; `dontAsk` refuses what its policy would have asked about. The presets are
//! this product's own; the order within them is Codex's handler order.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_coding::CodingPermissionMode;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    event::{
        FileEvent, HostEventBody, HostEventSink, InMemoryHostEventSink, file::PatchApplyStatus,
    },
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{CallId, ItemId, ModelResponse, RunItem, RunItemKind, ToolCall},
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    permission::{PermissionDecision, PermissionRule},
    state::{RunId, RunState},
    tool::{Tool, ToolContext, ToolOutput, ToolServices},
};
use ra_exec::fs::RootedFileSystem;
use ra_runtime::{
    agent::AgentBinding,
    hook::UserHookRegistration,
    permission::ActionClassPolicy,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use ra_tools::{apply_patch::ApplyPatchTool, exec_command::ApprovalResolver};
use serde_json::json;
use tempfile::TempDir;

struct ScriptedModel(Mutex<Vec<ModelResponse>>);

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let mut script = self.0.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        let response = futures::executor::block_on(self.get_response(request));
        stream::iter(vec![
            response.map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}

struct Resolver(Arc<ScriptedModel>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// Every tool hook and lifecycle callback, in order.
#[derive(Default)]
struct Journal(Mutex<Vec<String>>);

impl Journal {
    fn push(&self, entry: &str) {
        self.0.lock().unwrap().push(entry.to_owned());
    }
    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, entry: &str) -> usize {
        self.entries().iter().filter(|seen| *seen == entry).count()
    }
}

/// Records PreToolUse and answers PermissionRequest with `decision`.
struct Hooks {
    journal: Arc<Journal>,
    decision: HookDecision,
}

#[async_trait]
impl UserHook for Hooks {
    fn name(&self) -> &str {
        "hooks"
    }
    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        Ok(match event {
            HookEvent::PreToolUse(_) => {
                self.journal.push("pre");
                HookDecision::Continue
            }
            HookEvent::PermissionRequest(_) => {
                self.journal.push("permission");
                self.decision.clone()
            }
            _ => HookDecision::Continue,
        })
    }
}

struct Narration(Arc<Journal>);

#[async_trait]
impl LifecycleHook for Narration {
    fn name(&self) -> &str {
        "narration"
    }
    async fn on_tool_start(
        &self,
        _scope: LifecycleScope,
        _input: &ToolStartInput<'_>,
    ) -> Result<()> {
        self.0.push("start");
        Ok(())
    }
    async fn on_tool_end(&self, _scope: LifecycleScope, _input: &ToolEndInput<'_>) -> Result<()> {
        self.0.push("end");
        Ok(())
    }
}

const VALID: &str = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n";
const INVALID: &str =
    "*** Begin Patch\n*** Update File: missing.txt\n@@\n-nope\n+better\n*** End Patch\n";

/// One configuration a patch runs under.
#[derive(Clone)]
struct Setup {
    name: &'static str,
    policy: Option<ActionClassPolicy>,
    rule: Option<PermissionDecision>,
    resolver_asks: bool,
    hook: HookDecision,
}

impl Setup {
    fn rule(name: &'static str, rule: PermissionDecision) -> Self {
        Self {
            name,
            policy: None,
            rule: Some(rule),
            resolver_asks: true,
            hook: HookDecision::Allow,
        }
    }
    fn preset(mode: CodingPermissionMode, resolver_asks: bool) -> Self {
        Self {
            name: mode.label(),
            policy: Some(mode.policy()),
            rule: None,
            resolver_asks,
            hook: HookDecision::Allow,
        }
    }
    fn with_hook(mut self, hook: HookDecision) -> Self {
        self.hook = hook;
        self
    }
}

/// What a patch did under a setup.
struct Ran {
    workspace: TempDir,
    journal: Arc<Journal>,
    outcome: RunOutcome,
    state: RunState,
    text: Option<String>,
    sink: Arc<InMemoryHostEventSink>,
    request: Box<dyn Fn(Vec<ModelResponse>) -> RunRequest>,
}

impl Ran {
    fn wrote(&self) -> bool {
        self.workspace.path().join("added.txt").exists()
    }
    fn begins(&self) -> usize {
        self.sink
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.body(),
                    HostEventBody::File(FileEvent::PatchApplyBegin(_))
                )
            })
            .count()
    }
    fn ends(&self) -> Vec<PatchApplyStatus> {
        self.sink
            .events()
            .iter()
            .filter_map(|event| match event.body() {
                HostEventBody::File(FileEvent::PatchApplyEnd(end)) => Some(end.status()),
                _ => None,
            })
            .collect()
    }
}

fn text_of(items: &[RunItem]) -> Option<String> {
    items.iter().find_map(|item| match item.kind() {
        RunItemKind::ToolCallOutput(output) => {
            Some(match ToolOutput::from_stored(output.output()) {
                Ok(Some(stored)) => stored.as_text().unwrap_or_default().to_owned(),
                _ => output.output().to_string(),
            })
        }
        _ => None,
    })
}

fn calls(patch: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("c-1"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "apply_patch",
            json!({ "patch": patch }),
        )),
    )])
}

fn finishes() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(ra_core::item::Message::assistant(
            "done",
            ra_core::item::OutputPhase::Final,
        )),
    )])
}

async fn run(setup: &Setup, patch: &str) -> Ran {
    let workspace = TempDir::new().unwrap();
    let journal = Arc::new(Journal::default());
    let sink = Arc::new(InMemoryHostEventSink::new());
    let asks = setup.resolver_asks;
    let resolver: Arc<ApprovalResolver> = Arc::new(move |_context: &ToolContext<'_>| Ok(asks));
    let tool: Arc<dyn Tool> = Arc::new(
        ApplyPatchTool::new(Arc::new(RootedFileSystem::open(workspace.path()).unwrap()))
            .unwrap()
            .with_approval_resolver(resolver),
    );
    let mut config = RunConfig::new()
        .with_lifecycle_hook(Arc::new(Narration(Arc::clone(&journal))) as Arc<dyn LifecycleHook>);
    for event in [HookEventName::PreToolUse, HookEventName::PermissionRequest] {
        config = config.with_user_hook(UserHookRegistration::new(
            event,
            Arc::new(Hooks {
                journal: Arc::clone(&journal),
                decision: setup.hook.clone(),
            }) as Arc<dyn UserHook>,
        ));
    }
    if let Some(policy) = &setup.policy {
        config = config.with_permission_policy(policy.clone());
    }
    if let Some(rule) = setup.rule {
        config =
            config.with_permission_rules([PermissionRule::new(rule).with_tool_name("apply_patch")]);
    }
    let services = ToolServices::new().with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);
    let request = {
        let tool = Arc::clone(&tool);
        Box::new(move |script: Vec<ModelResponse>| {
            let agent = AgentSpec::builder()
                .id(AgentId::new("coder"))
                .name("Coder")
                .instructions("edit")
                .tools(vec![Arc::clone(&tool)])
                .build()
                .unwrap();
            RunRequest::new(
                AgentBinding::direct(agent),
                Arc::new(Resolver(Arc::new(ScriptedModel(Mutex::new(script))))),
                RunId::new("run-patch-permissions"),
                CancelScope::root(),
                Vec::new(),
            )
            .with_services(services.clone())
            .with_config(config.clone())
        }) as Box<dyn Fn(Vec<ModelResponse>) -> RunRequest>
    };
    let result = Runner::run(request(vec![calls(patch), finishes()]))
        .await
        .unwrap();
    Ran {
        text: text_of(result.new_items()),
        outcome: result.outcome().clone(),
        state: result.state().clone(),
        workspace,
        journal,
        sink,
        request,
    }
}

/// Refused before its hooks: no PreToolUse, no verification, no item, nothing written.
fn assert_refused_unseen(name: &str, ran: &Ran) {
    assert!(
        matches!(ran.outcome, RunOutcome::Completed { .. }),
        "{name}"
    );
    assert!(
        ran.journal.entries().is_empty(),
        "{name}: {:?}",
        ran.journal.entries()
    );
    assert_eq!(ran.begins(), 0, "{name}");
    assert!(!ran.wrote(), "{name}");
}

#[tokio::test]
async fn a_fixed_deny_and_plan_refuse_a_patch_before_its_hooks() {
    for setup in [
        Setup::rule("deny rule", PermissionDecision::Deny),
        Setup::preset(CodingPermissionMode::Plan, true),
    ] {
        for patch in [VALID, INVALID] {
            let ran = run(&setup, patch).await;
            assert_refused_unseen(setup.name, &ran);
            assert!(
                !ran.text
                    .as_deref()
                    .unwrap_or_default()
                    .contains("verification failed"),
                "{}: not verified",
                setup.name
            );
        }
    }
}

#[tokio::test]
async fn a_fixed_allow_accept_edits_and_bypass_apply_a_patch_unasked() {
    for setup in [
        Setup::rule("allow rule", PermissionDecision::Allow),
        Setup::preset(CodingPermissionMode::AcceptEdits, true),
        Setup::preset(CodingPermissionMode::BypassPermissions, true),
    ] {
        let ran = run(&setup, VALID).await;
        assert!(
            matches!(ran.outcome, RunOutcome::Completed { .. }),
            "{}",
            setup.name
        );
        assert_eq!(
            ran.journal.entries(),
            ["pre", "start", "end"],
            "{}",
            setup.name
        );
        assert!(ran.wrote(), "{}", setup.name);
        assert_eq!(ran.begins(), 1, "{}", setup.name);
        assert_eq!(ran.ends(), [PatchApplyStatus::Completed], "{}", setup.name);

        let ran = run(&setup, INVALID).await;
        assert_eq!(
            ran.journal.entries(),
            ["pre", "start", "end"],
            "{}",
            setup.name
        );
        assert!(
            ran.text
                .as_deref()
                .unwrap()
                .starts_with("apply_patch verification failed: "),
            "{}: {:?}",
            setup.name,
            ran.text
        );
        assert_eq!(ran.begins(), 0, "{}", setup.name);
    }
}

#[tokio::test]
async fn a_fixed_ask_verifies_and_announces_a_patch_before_its_permission_hook() {
    let setup = Setup::rule("ask rule", PermissionDecision::Ask);

    // Approved by the hook: announced once, before the hook, and applied.
    let ran = run(&setup, VALID).await;
    assert!(matches!(ran.outcome, RunOutcome::Completed { .. }));
    assert_eq!(ran.journal.entries(), ["pre", "permission", "start", "end"]);
    assert!(ran.wrote());
    assert_eq!(ran.begins(), 1);
    assert_eq!(ran.ends(), [PatchApplyStatus::Completed]);

    // Broken: never asked about.
    let ran = run(&setup, INVALID).await;
    assert!(matches!(ran.outcome, RunOutcome::Completed { .. }));
    assert_eq!(ran.journal.entries(), ["pre"]);
    assert!(
        ran.text
            .as_deref()
            .unwrap()
            .starts_with("apply_patch verification failed: ")
    );
    assert_eq!(ran.begins(), 0);
}

#[tokio::test]
async fn a_fixed_ask_left_to_the_host_applies_once_approved_without_announcing_again() {
    let setup = Setup::rule("ask rule", PermissionDecision::Ask).with_hook(HookDecision::Continue);
    let ran = run(&setup, VALID).await;
    let RunOutcome::Interrupted { items } = &ran.outcome else {
        panic!("expected an approval, got {:?}", ran.outcome);
    };
    assert_eq!(ran.journal.entries(), ["pre", "permission"]);
    assert_eq!(ran.begins(), 1, "announced before the host is asked");
    assert!(!ran.wrote());

    let mut checkpoint = ran.state.clone();
    checkpoint.approve(&items[0], false).unwrap();
    let resumed = Runner::run((ran.request)(vec![finishes()]).with_state(checkpoint))
        .await
        .unwrap();
    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    assert!(ran.wrote());
    assert_eq!(ran.begins(), 1, "not announced again");
    assert_eq!(ran.ends(), [PatchApplyStatus::Completed]);
    assert_eq!(ran.journal.count("permission"), 1, "asked once");
}

#[tokio::test]
async fn dont_ask_refuses_what_its_policy_would_ask_about() {
    // The policy would ask: refused without a question, unverified, with no item.
    for patch in [VALID, INVALID] {
        let ran = run(&Setup::preset(CodingPermissionMode::DontAsk, true), patch).await;
        assert!(matches!(ran.outcome, RunOutcome::Completed { .. }));
        assert_eq!(ran.journal.entries(), ["pre"]);
        assert_eq!(ran.begins(), 0);
        assert!(!ran.wrote());
        assert!(
            !ran.text
                .as_deref()
                .unwrap_or_default()
                .contains("verification failed"),
            "{:?}",
            ran.text
        );
    }

    // The policy would not ask: applied as usual.
    let ran = run(&Setup::preset(CodingPermissionMode::DontAsk, false), VALID).await;
    assert_eq!(ran.journal.entries(), ["pre", "start", "end"]);
    assert!(ran.wrote());
    assert_eq!(ran.ends(), [PatchApplyStatus::Completed]);
}
