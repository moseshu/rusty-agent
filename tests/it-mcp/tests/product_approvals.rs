// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product MCP approval through a real app-server thread and a counted stdio server.

use async_trait::async_trait;
use ra_coding::{
    CodingHost,
    mcp::CodingMcpRuntime,
    plugins::{ConsumerBinding, PluginConfigManager, PluginServices},
    skills::HostSkillsService,
    thread_settings::CodingThreadSettingsAdapter,
};
use ra_config::{
    AskForApproval, ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader, SandboxPolicy,
};
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::SessionId,
    state::RunId,
    tool::{ToolApprovalRequest, ToolContext, ToolOutput, ToolResolver},
};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_mcp::tool::McpDiscoveryOptions;
use ra_paths::AbsolutePathBuf;
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId},
    server::{
        AppServer, ThreadSettingsAdapter, protocol::ThreadPolicyOverrides, test_api::ThreadLoader,
    },
    transport::{
        ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{agent::AgentBinding, thread::AgentThread};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

const FIXTURE: &str = env!("CARGO_BIN_EXE_mcp_fixture");
const PATIENCE: Duration = Duration::from_secs(10);

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
}
impl Script {
    fn new(responses: VecDeque<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            inputs: Mutex::new(Vec::new()),
        }
    }
    fn last_output(&self, call_id: &str) -> Option<String> {
        self.inputs.lock().unwrap().last()?.iter().find_map(|item| {
            let ModelInputItem::ToolCallOutput(output) = item else {
                return None;
            };
            if output.call_id().as_str() != call_id {
                return None;
            }
            Some(
                ToolOutput::from_stored(output.output())
                    .ok()
                    .flatten()
                    .and_then(|output| output.as_text().map(str::to_owned))
                    .unwrap_or_else(|| match output.output() {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    }),
            )
        })
    }
}
#[async_trait]
impl Model for Script {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.inputs.lock().unwrap().push(request.input().to_vec());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("script exhausted"))
    }
}
struct Models(Arc<Script>);
impl ModelResolver for Models {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("test".into()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct Harness {
    script: Arc<Script>,
    home: tempfile::TempDir,
    runtime: Arc<CodingMcpRuntime>,
    events: mpsc::Sender<TransportEvent>,
    messages: mpsc::Receiver<QueuedOutgoingMessage>,
    _binding: ConsumerBinding,
    _services: Arc<PluginServices>,
    task: tokio::task::JoinHandle<()>,
}
impl Harness {
    async fn start(
        mode: &str,
        elicitation: bool,
        approval: AskForApproval,
        sandbox: SandboxPolicy,
        calls: usize,
    ) -> Self {
        let home = tempfile::tempdir().unwrap();
        let absolute = AbsolutePathBuf::from_absolute_path(home.path()).unwrap();
        std::fs::write(home.path().join("config.toml"), format!(
            "[features]\ntool_call_mcp_elicitation = {elicitation}\n[skills.bundled]\nenabled = false\n[mcp_servers.docs]\ncommand = {}\nargs = [\"--policy-fixture\"]\ndefault_tools_approval_mode = \"{mode}\"\n",
            toml::Value::String(FIXTURE.into())
        )).unwrap();
        let services = PluginServices::new(
            PluginConfigManager::new(
                ra_executor::LOCAL_FS.clone(),
                absolute.clone(),
                absolute.clone(),
                ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
                Arc::new(NoopThreadConfigLoader),
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ),
            Arc::new(HostSkillsService::new(absolute.clone(), false)),
        );
        let runtime = CodingMcpRuntime::new(vec![], McpDiscoveryOptions::new()).unwrap();
        let binding = services.bind_mcp_runtime(&runtime, absolute);
        assert_eq!(
            binding
                .wait_for(services.generation())
                .await
                .unwrap()
                .error(),
            None
        );
        runtime.connect_all().await.unwrap();
        let host = CodingHost::open(home.path())
            .unwrap()
            .with_mcp_runtime(runtime.clone());
        let agent = AgentSpec::builder()
            .id(AgentId::new("coding"))
            .name("coding")
            .tool_resolver(host.mcp_runtime().unwrap().clone())
            .build()
            .unwrap();
        let mut responses = VecDeque::new();
        for index in 0..calls {
            responses.push_back(ModelResponse::new(vec![RunItem::new(
                ItemId::new(format!("item-{index}")),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new(format!("call-{index}")),
                    "record",
                    json!({}),
                )),
            )]));
        }
        responses.push_back(ModelResponse::new(vec![RunItem::new(
            ItemId::new("final"),
            RunItemKind::Message(Message::assistant("complete", OutputPhase::Final)),
        )]));
        let script = Arc::new(Script::new(responses));
        let adapter = CodingThreadSettingsAdapter::new(home.path(), sandbox).unwrap();
        let config = adapter
            .prepare(
                &adapter.initialize(host.build_run_config()),
                &ThreadPolicyOverrides::new().with_approval_policy(approval),
            )
            .unwrap();
        let thread = AgentThread::builder(
            SessionId::new("mcp-approval"),
            AgentBinding::direct(agent),
            Arc::new(Models(script.clone())),
        )
        .with_config(config)
        .with_services(host.build_tool_services())
        .build();
        let server = AppServer::new(home.path()).unwrap();
        let loader = ThreadLoader::new(&server);
        let (events, incoming) = mpsc::channel(128);
        let task = tokio::spawn(server.run(incoming));
        let (writer, messages) = mpsc::channel(128);
        events
            .send(connection_opened(
                ConnectionId::new(1),
                ConnectionOrigin::WebSocket,
                writer,
                None,
            ))
            .await
            .unwrap();
        let mut harness = Self {
            script,
            home,
            runtime,
            events,
            messages,
            _binding: binding,
            _services: services,
            task,
        };
        harness.send(JsonRpcRequest::new(1, "initialize").with_params(json!({"clientInfo":{"name":"test", "version":"1"}, "capabilities":{"experimentalApi":true}}))).await;
        harness
            .until(|value| value["id"] == 1 && value.get("method").is_none())
            .await;
        harness.send(JsonRpcNotification::new("initialized")).await;
        loader
            .load_with_adapter(thread, &[ConnectionId::new(1)], Some(Arc::new(adapter)))
            .await;
        harness
    }
    async fn send(&self, message: impl Into<JsonRpcMessage>) {
        self.events
            .send(TransportEvent::IncomingMessage {
                connection_id: ConnectionId::new(1),
                message: message.into(),
            })
            .await
            .unwrap();
    }
    async fn next(&mut self) -> Value {
        let queued = tokio::time::timeout(PATIENCE, self.messages.recv())
            .await
            .unwrap()
            .unwrap();
        serde_json::to_value(queued.message()).unwrap()
    }
    async fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        loop {
            let value = self.next().await;
            if predicate(&value) {
                return value;
            }
        }
    }
    async fn run(&mut self, mut answer: impl FnMut(usize, &Value) -> Value) -> (usize, Value) {
        self.send(JsonRpcRequest::new(2, "turn/start").with_params(json!({"threadId":"mcp-approval", "input":[{"type":"text", "text":"run", "text_elements":[]}]}))).await;
        let mut count = 0;
        let mut status = Value::Null;
        loop {
            let value = self.next().await;
            if value["method"] == "thread/status/changed" {
                status = value["params"]["status"].clone();
            }
            if value["method"] == "mcpServer/elicitation/request"
                || value["method"] == "item/tool/requestUserInput"
            {
                let expected = if value["method"] == "item/tool/requestUserInput" {
                    "waitingOnUserInput"
                } else {
                    "waitingOnApproval"
                };
                assert!(
                    status["activeFlags"]
                        .as_array()
                        .is_some_and(|flags| flags.iter().any(|flag| flag == expected)),
                    "{status}"
                );
                let result = answer(count, &value);
                count += 1;
                let id: RequestId = serde_json::from_value(value["id"].clone()).unwrap();
                self.send(JsonRpcResponse::new(id, result)).await;
            }
            if value["method"] == "turn/completed" {
                return (count, value["params"].clone());
            }
        }
    }
    async fn count(&self) -> usize {
        let agent = AgentSpec::builder()
            .id(AgentId::new("probe"))
            .name("probe")
            .build()
            .unwrap();
        let run = RunContext::new(RunId::new("probe"), &agent);
        let tools = self
            .runtime
            .get_tools(&run, &Default::default())
            .await
            .unwrap();
        let tool = tools
            .iter()
            .find(|tool| tool.schema().name() == "call_count")
            .unwrap();
        let id = CallId::new("probe");
        let arguments = json!({});
        let context = ToolContext::new(&run, tool.as_ref(), &id, &arguments);
        let result = tool.call(context).await;
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                let approval = ToolApprovalRequest::of(&error).unwrap();
                tool.call(
                    ToolContext::new(&run, tool.as_ref(), &id, &arguments)
                        .with_resume_state(approval.state()),
                )
                .await
                .unwrap()
            }
        };
        output.as_text().unwrap().parse().unwrap()
    }
    async fn stop(self) {
        self.runtime.shutdown().await.unwrap();
        self.task.abort();
    }
}

fn allow(value: &Value, scope: Option<&str>) -> Value {
    if value["method"] == "mcpServer/elicitation/request" {
        let mut response = json!({"action":"accept", "content":null, "_meta":null});
        if let Some(scope) = scope {
            response["_meta"] = json!({"persist":scope});
        }
        response
    } else {
        let id = value["params"]["questions"][0]["id"].as_str().unwrap();
        json!({"answers":{id:{"answers":[if scope.is_some() { "Allow for this session" } else { "Allow" }]}}})
    }
}

#[tokio::test]
async fn both_client_request_paths_approve_and_remember_only_auto() {
    for elicitation in [false, true] {
        for mode in ["auto", "prompt", "writes"] {
            let mut harness = Harness::start(
                mode,
                elicitation,
                AskForApproval::OnRequest,
                SandboxPolicy::ReadOnly {
                    network_access: false,
                },
                2,
            )
            .await;
            let (asked, outcome) = harness
                .run(|_, request| {
                    assert_eq!(
                        request["method"],
                        if elicitation {
                            "mcpServer/elicitation/request"
                        } else {
                            "item/tool/requestUserInput"
                        }
                    );
                    if elicitation {
                        assert_eq!(request["params"]["mode"], "form");
                        assert_eq!(
                            request["params"]["requestedSchema"],
                            json!({"type":"object", "properties":{}})
                        );
                        assert_eq!(
                            request["params"]["_meta"]["persist"].is_null(),
                            mode != "auto"
                        );
                    } else {
                        assert_eq!(
                            request["params"]["questions"][0]["options"]
                                .as_array()
                                .unwrap()
                                .len(),
                            if mode == "auto" { 3 } else { 2 }
                        );
                    }
                    allow(request, Some("session"))
                })
                .await;
            assert_eq!(asked, if mode == "auto" { 1 } else { 2 });
            assert_eq!(outcome["turn"]["status"], "completed", "{outcome}");
            assert_eq!(harness.count().await, 2);
            harness.stop().await;
        }
    }
}

#[tokio::test]
async fn prompt_and_writes_downgrade_forged_persistent_decisions() {
    for mode in ["prompt", "writes"] {
        let mut harness = Harness::start(
            mode,
            true,
            AskForApproval::OnRequest,
            SandboxPolicy::DangerFullAccess,
            2,
        )
        .await;
        let original = std::fs::read_to_string(harness.home.path().join("config.toml")).unwrap();
        let (asked, _) = harness
            .run(|_, request| allow(request, Some("always")))
            .await;
        assert_eq!(asked, 2);
        assert_eq!(harness.count().await, 2);
        assert_eq!(
            std::fs::read_to_string(harness.home.path().join("config.toml")).unwrap(),
            original
        );
        harness.stop().await;
    }
}

#[tokio::test]
async fn rejected_and_cancelled_calls_never_execute() {
    for (elicitation, action) in [(true, "decline"), (true, "cancel"), (false, "cancel")] {
        let mut harness = Harness::start(
            "auto",
            elicitation,
            AskForApproval::OnRequest,
            SandboxPolicy::DangerFullAccess,
            1,
        )
        .await;
        let (asked, outcome) = harness
            .run(|_, request| {
                if elicitation {
                    json!({"action":action, "content":null, "_meta":null})
                } else {
                    let id = request["params"]["questions"][0]["id"].as_str().unwrap();
                    json!({"answers":{id:{"answers":["Cancel"]}}})
                }
            })
            .await;
        assert_eq!(asked, 1);
        assert_eq!(outcome["turn"]["status"], "completed");
        assert_eq!(
            harness.script.last_output("call-0").as_deref(),
            Some(if action == "cancel" {
                "user cancelled MCP tool call"
            } else {
                "user rejected MCP tool call"
            })
        );
        assert_eq!(harness.count().await, 0);
        harness.stop().await;
    }
}

#[tokio::test]
async fn never_auto_approves_only_full_write_profiles_or_approve_mode() {
    for (mode, sandbox, expected) in [
        ("auto", SandboxPolicy::DangerFullAccess, 1),
        ("prompt", SandboxPolicy::DangerFullAccess, 1),
        (
            "auto",
            SandboxPolicy::ExternalSandbox {
                network_access: ra_config::NetworkAccess::Restricted,
            },
            1,
        ),
        (
            "auto",
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![
                    std::env::current_dir()
                        .unwrap()
                        .ancestors()
                        .last()
                        .unwrap()
                        .to_path_buf(),
                ],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            },
            0,
        ),
        (
            "auto",
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            0,
        ),
        (
            "prompt",
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            0,
        ),
        (
            "approve",
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            1,
        ),
    ] {
        let mut harness = Harness::start(mode, false, AskForApproval::Never, sandbox, 1).await;
        let (asked, _) = harness
            .run(|_, _| panic!("never must not ask the client"))
            .await;
        assert_eq!(asked, 0);
        assert_eq!(harness.count().await, expected);
        harness.stop().await;
    }
}

#[tokio::test]
async fn auto_persistent_accept_updates_the_config_and_remembers_the_session() {
    let mut harness = Harness::start(
        "auto",
        true,
        AskForApproval::OnRequest,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
        2,
    )
    .await;
    let (asked, _) = harness
        .run(|_, request| allow(request, Some("always")))
        .await;
    assert_eq!(asked, 1);
    assert_eq!(harness.count().await, 2);
    let config: toml::Value = std::fs::read_to_string(harness.home.path().join("config.toml"))
        .unwrap()
        .parse::<toml::Table>()
        .unwrap()
        .into();
    assert_eq!(
        config["mcp_servers"]["docs"]["tools"]["record"]["approval_mode"].as_str(),
        Some("approve")
    );
    harness.stop().await;
}

#[test]
fn managed_root_write_uses_the_source_narrowing_rules() {
    use ra_coding::mcp::{
        McpPermissionPromptAutoApproveContext, mcp_permission_prompt_is_auto_approved,
    };
    use ra_filesystem::{
        models::{ManagedFileSystemPermissions, PermissionProfile},
        permissions::{
            FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSpecialPath,
            NetworkSandboxPolicy,
        },
    };
    let context = McpPermissionPromptAutoApproveContext::default();
    let root = FileSystemSandboxEntry::new(
        FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        FileSystemAccessMode::Write,
    );
    let narrowed = FileSystemSandboxEntry::new(
        FileSystemPath::Special {
            value: FileSystemSpecialPath::project_roots(None),
        },
        FileSystemAccessMode::Read,
    );
    for (file_system, expected) in [
        (ManagedFileSystemPermissions::Unrestricted, true),
        (
            ManagedFileSystemPermissions::Restricted {
                entries: vec![root.clone()],
                glob_scan_max_depth: None,
            },
            true,
        ),
        (
            ManagedFileSystemPermissions::Restricted {
                entries: vec![root, narrowed],
                glob_scan_max_depth: None,
            },
            false,
        ),
    ] {
        for network in [
            NetworkSandboxPolicy::Enabled,
            NetworkSandboxPolicy::Restricted,
        ] {
            let profile = PermissionProfile::Managed {
                file_system: file_system.clone(),
                network,
            };
            assert_eq!(
                mcp_permission_prompt_is_auto_approved(AskForApproval::Never, &profile, context),
                expected
            );
            assert!(!mcp_permission_prompt_is_auto_approved(
                AskForApproval::OnRequest,
                &profile,
                context
            ));
            assert!(mcp_permission_prompt_is_auto_approved(
                AskForApproval::Never,
                &profile,
                McpPermissionPromptAutoApproveContext {
                    tool_approval_mode: Some(ra_config::AppToolApproval::Approve)
                }
            ));
        }
    }
}

#[tokio::test]
async fn sdk_always_approve_does_not_bypass_later_prompt_calls() {
    use ra_core::cancel::CancelScope;
    use ra_runtime::runner::{RunOutcome, RunRequest, Runner};
    for mode in ["prompt", "writes"] {
        let harness = Harness::start(
            mode,
            true,
            AskForApproval::OnRequest,
            SandboxPolicy::DangerFullAccess,
            0,
        )
        .await;
        let agent = AgentSpec::builder()
            .id(AgentId::new("sdk"))
            .name("sdk")
            .tool_resolver(harness.runtime.clone())
            .build()
            .unwrap();
        let mut responses = VecDeque::new();
        for index in 0..2 {
            responses.push_back(ModelResponse::new(vec![RunItem::new(
                ItemId::new(format!("sdk-{index}")),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new(format!("sdk-{index}")),
                    "record",
                    json!({}),
                )),
            )]));
        }
        responses.push_back(ModelResponse::new(vec![RunItem::new(
            ItemId::new("final"),
            RunItemKind::Message(Message::assistant("complete", OutputPhase::Final)),
        )]));
        let models = Arc::new(Models(Arc::new(Script::new(responses))));
        let request = || {
            RunRequest::new(
                AgentBinding::direct(agent.clone()),
                models.clone(),
                RunId::new("sdk-run"),
                CancelScope::root(),
                vec![],
            )
        };
        let mut result = Runner::run(request()).await.unwrap();
        for count in 0..2 {
            let RunOutcome::Interrupted { items } = result.outcome() else {
                panic!("every product prompt needs its own approval");
            };
            assert_eq!(items.len(), 1);
            assert_eq!(harness.count().await, count);
            let mut state: ra_core::state::RunState =
                serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
            state.approve(&items[0], true).unwrap();
            result = Runner::run(request().with_state(state)).await.unwrap();
        }
        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
        assert_eq!(harness.count().await, 2);
        harness.stop().await;
    }
}

#[tokio::test]
async fn fallback_rejects_non_root_calls_before_requesting_approval() {
    let harness = Harness::start(
        "auto",
        false,
        AskForApproval::OnRequest,
        SandboxPolicy::DangerFullAccess,
        0,
    )
    .await;
    let agent = AgentSpec::builder()
        .id(AgentId::new("child"))
        .name("child")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("child"), &agent).with_parent_run_id(RunId::new("parent"));
    let tools = harness
        .runtime
        .get_tools(&run, &Default::default())
        .await
        .unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool.schema().name() == "record")
        .unwrap();
    let id = CallId::new("child");
    let args = json!({});
    let error = tool
        .call(ToolContext::new(&run, tool.as_ref(), &id, &args))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("requires the root thread"));
    assert!(ToolApprovalRequest::of(&error).is_none());
    assert_eq!(harness.count().await, 0);
    harness.stop().await;
}

#[tokio::test]
async fn malformed_approval_responses_skip_without_execution() {
    for elicitation in [true, false] {
        let mut harness = Harness::start(
            "auto",
            elicitation,
            AskForApproval::OnRequest,
            SandboxPolicy::DangerFullAccess,
            1,
        )
        .await;
        let (asked, outcome) = harness.run(|_, _| json!({"unexpected":"response"})).await;
        assert_eq!(asked, 1);
        assert_eq!(outcome["turn"]["status"], "completed");
        assert_eq!(
            harness.script.last_output("call-0").as_deref(),
            Some(if elicitation {
                "user rejected MCP tool call"
            } else {
                "user cancelled MCP tool call"
            })
        );
        assert_eq!(harness.count().await, 0);
        harness.stop().await;
    }
}

#[tokio::test]
async fn interrupt_withdraws_mcp_approval_and_ignores_a_late_accept() {
    let mut harness = Harness::start(
        "auto",
        true,
        AskForApproval::OnRequest,
        SandboxPolicy::DangerFullAccess,
        1,
    )
    .await;
    harness.send(JsonRpcRequest::new(2, "turn/start").with_params(json!({"threadId":"mcp-approval", "input":[{"type":"text", "text":"run", "text_elements":[]}]}))).await;
    let request = harness
        .until(|value| value["method"] == "mcpServer/elicitation/request")
        .await;
    harness
        .send(
            JsonRpcRequest::new(3, "turn/interrupt").with_params(
                json!({"threadId":"mcp-approval", "turnId":request["params"]["turnId"]}),
            ),
        )
        .await;
    let outcome = harness
        .until(|value| value["method"] == "turn/completed")
        .await;
    assert_eq!(outcome["params"]["turn"]["status"], "interrupted");
    let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
    harness
        .send(JsonRpcResponse::new(id, allow(&request, Some("always"))))
        .await;
    assert_eq!(harness.count().await, 0);
    let config = std::fs::read_to_string(harness.home.path().join("config.toml")).unwrap();
    assert!(!config.contains("approval_mode = \"approve\""));
    harness.stop().await;
}

#[tokio::test]
async fn client_errors_skip_calls_with_source_reasons() {
    for elicitation in [true, false] {
        let mut harness = Harness::start(
            "auto",
            elicitation,
            AskForApproval::OnRequest,
            SandboxPolicy::DangerFullAccess,
            1,
        )
        .await;
        harness.send(JsonRpcRequest::new(2, "turn/start").with_params(json!({"threadId":"mcp-approval", "input":[{"type":"text", "text":"run", "text_elements":[]}]}))).await;
        let request = harness
            .until(|value| {
                value["method"] == "mcpServer/elicitation/request"
                    || value["method"] == "item/tool/requestUserInput"
            })
            .await;
        let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
        harness
            .send(ra_protocol::frame::JsonRpcError::new(
                id,
                ra_protocol::frame::JsonRpcErrorObject::invalid_request("client cannot answer"),
            ))
            .await;
        let outcome = harness
            .until(|value| value["method"] == "turn/completed")
            .await;
        assert_eq!(outcome["params"]["turn"]["status"], "completed");
        assert_eq!(
            harness.script.last_output("call-0").as_deref(),
            Some(if elicitation {
                "user rejected MCP tool call"
            } else {
                "user cancelled MCP tool call"
            })
        );
        assert_eq!(harness.count().await, 0);
        harness.stop().await;
    }
}
