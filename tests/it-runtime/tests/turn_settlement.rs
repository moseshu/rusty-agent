//! R3-4 contracts for the fixed order that settles one turn.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentId,
    cancel::{CancelReason, CancelScope},
    error::{Error, Result, ToolErrorKind},
    finish::FinishReason,
    item::{
        CallId, ItemId, McpApprovalRequest, Message, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::ModelHandoffDefinition,
    step::NextStep,
    tool::{
        Tool, ToolApprovalPolicy, ToolCaller, ToolFailureHandling, ToolInvocation, ToolOptions,
        ToolOrigin, ToolOutput, ToolRuntimeContext, ToolSchema, ToolTimeoutBehavior,
    },
};
use ra_runtime::turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn};
use serde_json::{Value, json};

/// What a `ScriptedTool` does when it is finally invoked.
enum Behavior {
    Succeed(&'static str),
    Fail,
    Sleep(Duration),
}

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    behavior: Behavior,
    calls: Arc<AtomicUsize>,
    failures_handled: Arc<AtomicUsize>,
}

impl ScriptedTool {
    fn new(name: &str, behavior: Behavior) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options: ToolOptions::new(),
            behavior,
            calls: Arc::new(AtomicUsize::new(0)),
            failures_handled: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }
}

#[async_trait]
impl Tool for ScriptedTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, _invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            Behavior::Succeed(text) => Ok(ToolOutput::text(*text)),
            Behavior::Fail => Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                self.origin.qualified_name(),
                "工具内部失败：路径不存在",
            )),
            Behavior::Sleep(duration) => {
                tokio::time::sleep(*duration).await;
                Ok(ToolOutput::text("late"))
            }
        }
    }

    async fn needs_approval(&self, _invocation: &ToolInvocation<'_>) -> Result<bool> {
        match self.options.approval() {
            ToolApprovalPolicy::Never => Ok(false),
            _ => Ok(true),
        }
    }

    async fn handle_failure(
        &self,
        _invocation: &ToolInvocation<'_>,
        _error: &Error,
    ) -> Result<Option<ToolOutput>> {
        self.failures_handled.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ToolOutput::text("tool wrote its own explanation")))
    }
}

struct Host;

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn tool_call(id: &str, call_id: &str, name: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            name,
            json!({ "path": "a.txt" }),
        )),
    )
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn surface(tools: Vec<Arc<dyn Tool>>) -> TurnActionSurface {
    TurnActionSurface::new(tools, Vec::new()).unwrap()
}

/// Pulls the paired output payload for one call out of a settled turn.
fn output_for<'a>(items: &'a [RunItem], call_id: &str) -> &'a ra_core::item::ToolCallOutput {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                Some(output)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("call `{call_id}` 没有配对输出"))
}

#[tokio::test]
async fn 什么都没要的响应直接结算成最终输出() {
    let surface = surface(vec![Arc::new(ScriptedTool::new(
        "write_file",
        Behavior::Succeed("ok"),
    ))]);
    let response = ModelResponse::new(vec![message("msg-1", "完事了")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    // 「没要任何动作」是每个 provider 都一样表达的结构事实，不用去读文本判断意图。
    assert!(matches!(
        settled.next_step(),
        NextStep::FinalOutput {
            reason: FinishReason::Final
        }
    ));
    assert_eq!(settled.new_step_items().len(), 1);
    assert_eq!(settled.session_step_items().len(), 1);
}

#[tokio::test]
async fn 工具跑完后回到模型_输出按_call_id_配对() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Succeed("written")));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![
        message("msg-1", "我来写一下"),
        tool_call("call-item-1", "call-1", "write_file"),
    ]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(settled.next_step(), NextStep::RunAgain));

    // 模型自己的记录在前，回答它的观察在后——这一轮生成的全部东西按发生顺序排列。
    let ids = settled
        .new_step_items()
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["msg-1", "call-item-1", "call-1.output"]);

    let output = output_for(settled.new_step_items(), "call-1");
    assert!(!output.is_error());
    assert_eq!(output.output()["text"], json!("written"));
}

#[tokio::test]
async fn 叫不出名字的工具也拿到一份配对失败观察并逼出下一轮() {
    let surface = surface(vec![Arc::new(ScriptedTool::new(
        "write_file",
        Behavior::Succeed("ok"),
    ))]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "wrte_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    // 悬空的 tool call 会让下一次请求非法，所以它必须被回答；而模型还没看过这条失败，
    // 所以这一轮不能收尾。这正是 `has_tools_or_approvals_to_run()` 把 not-found 算进去的兑现。
    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(output.output()["error"]["code"], json!("tool.not_found"));
    assert!(matches!(settled.next_step(), NextStep::RunAgain));
}

#[tokio::test]
async fn 需要审批时干净地停下来而不是阻塞在一个_await_上() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed("ok"))
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    // 审批在任何东西跑起来之前发生。
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let NextStep::Interruption { items } = settled.next_step() else {
        panic!("应当停下来问人");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id().as_str(), "call-1.approval");
    assert!(items[0].kind().is_interruption());

    // 恢复读的是会话，所以待审批项必须存进去；`SingleStepResult` 的校验也认这一条。
    assert!(
        settled
            .session_step_items()
            .iter()
            .any(|item| item.id().as_str() == "call-1.approval")
    );
}

#[tokio::test]
async fn 响应里的_mcp_审批与工具审批一起被问全() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed("ok"))
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic)),
    );
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![
        item(
            "approval-1",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "req-1",
                "docs",
                "search",
                json!({ "query": "x" }),
            )),
        ),
        tool_call("call-item-1", "call-1", "write_file"),
    ]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    let NextStep::Interruption { items } = settled.next_step() else {
        panic!("应当停下来问人");
    };
    let asked = items
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    // 停下来却只问一部分，剩下那条要等一个永远不会来的轮次。
    assert_eq!(asked, ["approval-1", "call-1.approval"]);
}

#[tokio::test]
async fn 工具失败默认变成模型读得懂的观察而不是终止_run() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Fail));
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(
        output.output()["error"]["code"],
        json!("tool.execution_failed")
    );
    assert_eq!(output.output()["error"]["tool"], json!("write_file"));

    // 框架的错误文案是写给日志和人看的，而且是中文的；它一旦进模型上下文，
    // 下一轮就会依赖它。载荷里只有 code 与工具名，没有任何散文——
    // 既没有工具自己的 message，也没有 `Error` 的 Display 模板。
    let rendered = output.output().to_string();
    let leaked = Error::tool(
        ToolErrorKind::ExecutionFailed,
        "write_file",
        "工具内部失败：路径不存在",
    )
    .to_string();
    assert!(!rendered.contains("路径不存在"));
    assert!(!rendered.contains("失败"));
    assert!(!rendered.contains(&leaked));
    assert_eq!(output.output()["error"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn 声明_propagate_的工具失败会终止这一轮() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Fail)
            .with_options(ToolOptions::new().with_failure_handling(ToolFailureHandling::Propagate)),
    );
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let error = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::ExecutionFailed,
            ..
        }
    ));
}

#[tokio::test]
async fn custom_失败处理让工具自己写给模型看的解释() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Fail)
            .with_options(ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom)),
    );
    let handled = Arc::clone(&tool.failures_handled);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(handled.load(Ordering::SeqCst), 1);
    let output = output_for(settled.new_step_items(), "call-1");
    // 这是唯一一条把散文送进模型上下文的路径，而且写它的是工具自己。
    assert_eq!(
        output.output()["text"],
        json!("tool wrote its own explanation")
    );
}

#[tokio::test]
async fn 超时按声明的行为分流() {
    let visible = Arc::new(
        ScriptedTool::new("slow", Behavior::Sleep(Duration::from_millis(80)))
            .with_options(ToolOptions::new().with_timeout(Duration::from_millis(5))),
    );
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "slow")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface(vec![visible]),
        &Host,
        &cancel,
    ))
    .await
    .unwrap();
    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(output.output()["error"]["code"], json!("tool.timeout"));

    let propagating = Arc::new(
        ScriptedTool::new("slow", Behavior::Sleep(Duration::from_millis(80))).with_options(
            ToolOptions::new()
                .with_timeout(Duration::from_millis(5))
                .with_timeout_behavior(ToolTimeoutBehavior::Propagate),
        ),
    );
    let error = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface(vec![propagating]),
        &Host,
        &cancel,
    ))
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::Timeout,
            ..
        }
    ));
}

#[tokio::test]
async fn 取消永远不会被降级成一条工具观察() {
    let tool = Arc::new(ScriptedTool::new(
        "slow",
        Behavior::Sleep(Duration::from_secs(30)),
    ));
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "slow")]);
    let cancel = CancelScope::root();

    let scope = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        scope.cancel(CancelReason::UserInterrupt);
    });

    let error = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap_err();

    // 把取消报成一条工具失败，会让 loop 越过那个叫它停下来的东西继续花钱。
    assert!(matches!(error, Error::Cancelled { .. }));
}

#[tokio::test]
async fn 已取消的作用域一个工具都不跑() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Succeed("ok")));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::Shutdown);

    let error = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 已取消时不看模型这轮要了什么都报成取消() {
    // 上一条测试只覆盖了「解析得到工具」那一格。真正的契约是：一轮是否报成取消，
    // 不能取决于模型这次恰好点了什么名字——而剩下三格各自会settle成一个不同的谎。
    let cases: [(&str, Vec<RunItem>); 3] = [
        // 只有未解析调用：会生成 `tool.not_found` 观察并settle成 `RunAgain`。
        (
            "not_found",
            vec![tool_call("call-item-1", "call-1", "vanished")],
        ),
        // 只有 MCP 审批：会settle成 `Interruption`，向宿主要一个此刻没人该回答的决定。
        (
            "mcp_approval",
            vec![item(
                "approval-1",
                RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                    "req-1",
                    "docs",
                    "search",
                    json!({ "query": "x" }),
                )),
            )],
        ),
        // 纯消息、无动作：最糟的一格——会settle成 `FinalOutput{Final}`，而 `is_complete()`
        // 为真意味着 R15 认为不欠收尾、R17-3 走成功边。取消被报成了「agent 自己做完了」。
        ("no_action", vec![message("msg-1", "完事了")]),
    ];

    for (label, output) in cases {
        let response = ModelResponse::new(output);
        let cancel = CancelScope::root();
        cancel.cancel(CancelReason::UserInterrupt);

        let settled = settle_turn(TurnSettlementRequest::new(
            &response,
            &surface(vec![Arc::new(ScriptedTool::new(
                "write_file",
                Behavior::Succeed("ok"),
            ))]),
            &Host,
            &cancel,
        ))
        .await;

        let error = settled
            .map(|settled| format!("{:?}", settled.next_step()))
            .unwrap_err();
        assert!(
            matches!(error, Error::Cancelled { .. }),
            "{label} 应当报成取消，实际是：{error}"
        );
    }
}

#[tokio::test]
async fn 不接受这个调用方类别的工具从模型侧看就是不存在() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed("ok"))
            .with_options(ToolOptions::new().with_allowed_callers([ToolCaller::Programmatic])),
    );
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(output.output()["error"]["code"], json!("tool.not_found"));
}

#[tokio::test]
async fn 交接落到_r17_之前明确报错而不是当成模型什么都没要() {
    let handoff = ModelHandoffDefinition::new(
        AgentId::new("reviewer"),
        "transfer_to_reviewer",
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    );
    let surface = TurnActionSurface::new(Vec::new(), vec![handoff]).unwrap();
    let response = ModelResponse::new(vec![tool_call(
        "call-item-1",
        "call-1",
        "transfer_to_reviewer",
    )]);
    let cancel = CancelScope::root();

    let error = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap_err();

    assert!(error.to_string().contains("reviewer"));
    assert!(error.to_string().contains("R17"));
}

#[tokio::test]
async fn 一轮里的多个调用各自拿到自己的观察() {
    let ok = Arc::new(ScriptedTool::new("read_file", Behavior::Succeed("content")));
    let failing = Arc::new(ScriptedTool::new("write_file", Behavior::Fail));
    let surface = surface(vec![ok, failing]);
    let response = ModelResponse::new(vec![
        tool_call("call-item-1", "call-1", "read_file"),
        tool_call("call-item-2", "call-2", "write_file"),
        tool_call("call-item-3", "call-3", "vanished"),
    ]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &response,
        &surface,
        &Host,
        &cancel,
    ))
    .await
    .unwrap();

    // 三个调用、三份配对输出。少一份下一次请求就是非法的。
    assert!(!output_for(settled.new_step_items(), "call-1").is_error());
    assert!(output_for(settled.new_step_items(), "call-2").is_error());
    assert!(output_for(settled.new_step_items(), "call-3").is_error());
    assert!(matches!(settled.next_step(), NextStep::RunAgain));
}

#[tokio::test]
async fn 结算结果带着原始输入与前序项且通过全部对账() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Succeed("ok")));
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();
    let original: Vec<ra_core::item::ModelInputItem> =
        vec![ra_core::item::ModelInputItem::Message(Message::user("开始"))];

    let settled = settle_turn(
        TurnSettlementRequest::new(&response, &surface, &Host, &cancel)
            .with_original_input(original.clone())
            .with_pre_step_items(vec![message("old-1", "上一轮")]),
    )
    .await
    .unwrap();

    assert_eq!(settled.original_input(), original.as_slice());
    assert_eq!(settled.pre_step_items().len(), 1);
    // `generated_items()` 是下一轮输入的来源：前序在前，本轮在后。
    let ids = settled
        .generated_items()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["old-1", "call-item-1", "call-1.output"]);
    // 分类结果必须是这条响应的分类，会话必须存下模型说过的每一条——都由 builder 校验过了。
    assert_eq!(
        settled.processed_response().new_items().len(),
        response.output().len()
    );
}

/// 上一条测试的前提：`Error` 的 Display 确实是面向人的中文模板，所以它绝不能进模型上下文。
#[test]
fn 框架错误的显示文案确实是面向人的中文() {
    let error = Error::tool(ToolErrorKind::ExecutionFailed, "write_file", "boom");
    let rendered: String = error.to_string();
    assert!(rendered.contains("失败"));
    assert!(rendered.contains("boom"));
    // 送回模型的那份只保留机器可读的两项。
    let observation: Value = json!({ "error": { "code": error.code(), "tool": "write_file" } });
    assert_eq!(observation["error"]["code"], json!("tool.execution_failed"));
    assert!(!observation.to_string().contains("boom"));
}

/// A settled turn is shareable, like everything else the loop carries across an `await`.
#[tokio::test]
async fn 结算结果可跨_await_共享() {
    let surface = surface(Vec::new());
    let response = ModelResponse::new(vec![message("msg-1", "完事了")]);
    let cancel = CancelScope::root();
    let settled = Arc::new(
        settle_turn(TurnSettlementRequest::new(
            &response,
            &surface,
            &Host,
            &cancel,
        ))
        .await
        .unwrap(),
    );
    let cloned = Arc::clone(&settled);
    let joined = tokio::spawn(async move { cloned.new_step_items().len() })
        .await
        .unwrap();
    assert_eq!(joined, 1);
}

/// The lock is only here to prove `Host` stays `Send + Sync` as a runtime context.
#[allow(dead_code)]
fn 上下文可跨线程(_context: &dyn ToolRuntimeContext) {
    let _guard: Mutex<()> = Mutex::new(());
}
