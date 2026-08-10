//! R3-7 contracts for the agent loop and its two entry points.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    error::{Error, Result},
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ProviderKey, ResolvedModel,
    },
    state::ToolUse,
    tool::{
        Tool, ToolApprovalPolicy, ToolInvocation, ToolLookupKey, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
    usage::Usage,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{ContinuationInput, RunConfig, RunOutcome, RunRequest, RunStreamEvent, Runner},
};
use serde_json::json;

/// Replays a fixed script of responses, one per turn, and records the input it was handed.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    inputs: Mutex<Vec<usize>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            inputs: Mutex::new(Vec::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(request.input().len());
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            // The loop asked for a turn the script did not plan for. Answering with a final
            // message would hide the mismatch behind a passing test.
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

struct FixedResolver {
    model: Arc<ScriptedModel>,
    selectors: Arc<Mutex<Vec<Option<String>>>>,
}

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.selectors
            .lock()
            .unwrap()
            .push(model_name.map(str::to_owned));
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
}

impl ScriptedTool {
    fn new(name: &str) -> Self {
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
            calls: Arc::new(AtomicUsize::new(0)),
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
        Ok(ToolOutput::text("done"))
    }

    async fn needs_approval(&self, _invocation: &ToolInvocation<'_>) -> Result<bool> {
        Ok(!matches!(
            self.options.approval(),
            ToolApprovalPolicy::Never
        ))
    }
}

struct Host;

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
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

fn agent(tools: Vec<Arc<dyn Tool>>) -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .instructions("do the thing")
            .tools(tools)
            .build()
            .unwrap(),
    )
}

type Selectors = Arc<Mutex<Vec<Option<String>>>>;

fn request(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
) -> RunRequest {
    request_recording(tools, model, cancel, Arc::new(Mutex::new(Vec::new())))
}

fn request_recording(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
    selectors: Selectors,
) -> RunRequest {
    RunRequest::new(
        agent(tools),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
            selectors,
        }),
        Arc::new(Host),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

#[tokio::test]
async fn 工具调用与最终回答之间来回直到模型不再要东西() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let tool_calls = Arc::clone(&tool.calls);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")])
            .with_usage(Usage::new(10, 4)),
        ModelResponse::new(vec![message("msg-1", "改完了")]).with_usage(Usage::new(20, 6)),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(result.turns(), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));
    assert_eq!(
        result.final_message().unwrap().content()[0]
            .as_text()
            .unwrap(),
        "改完了"
    );

    // 第二轮的输入必须带上第一轮的调用与它的输出。少了配对输出，provider 会直接拒收。
    let inputs = model.inputs.lock().unwrap().clone();
    assert_eq!(inputs, [1, 3]);

    // 用量是从每一次调用求和出来的投影，不是一路累加的字段。
    assert_eq!(result.usage().input_tokens(), 30);
    assert_eq!(result.usage().output_tokens(), 10);
    assert_eq!(result.model_responses().len(), 2);
}

#[tokio::test]
async fn 到达轮次上限时软收尾而不是报错() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    // 模型每一轮都要工具，永远不收尾。
    let model = ScriptedModel::new(
        (0..8)
            .map(|turn| {
                ModelResponse::new(vec![tool_call(
                    &format!("c-{turn}"),
                    &format!("call-{turn}"),
                    "write_file",
                )])
            })
            .collect(),
    );
    let cancel = CancelScope::root();

    let result = Runner::run(
        request(vec![tool], &model, &cancel).with_config(RunConfig::new().with_max_turns(3)),
    )
    .await
    .unwrap();

    // 上限是 loop 自己的终止条件，不是失败：宿主要能拿到已经产出的东西。
    assert_eq!(result.turns(), 3);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert_eq!(result.model_responses().len(), 3);
}

#[tokio::test]
async fn 上限为零当场拒绝而不是当成不限() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();

    let error = Runner::run(
        request(Vec::new(), &model, &cancel).with_config(RunConfig::new().with_max_turns(0)),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("max_turns"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 待审批停下来是一种结局而不是一个错误() {
    let gated = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let tool_calls = Arc::clone(&gated.calls);
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![gated], &model, &cancel))
        .await
        .unwrap();

    // 报成 Err 的话，宿主没有任何办法回答它然后接着跑。
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("应当停下来问人，实际是 {:?}", result.outcome());
    };
    assert_eq!(items.len(), 1);
    assert!(items[0].kind().is_interruption());
    assert_eq!(result.outcome().finish_reason(), None);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(result.turns(), 1);
}

#[tokio::test]
async fn 取消不会被当成正常收尾() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "不该跑到")])]);
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap_err();

    // 报成 FinalOutput{Final} 会让 R15 认为不欠收尾、R17-3 走成功边。
    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 续跑输入的两种口径给出不同的历史() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    // 展示历史、会话历史、下一轮输入是三样东西。续跑输入是投影而不是第四个数组，
    // 所以它不可能和 `new_items` 讲不同的故事。
    let preserve = result.continuation_input(ContinuationInput::PreserveAll);
    let normalized = result.continuation_input(ContinuationInput::Normalized);
    assert_eq!(preserve.len(), 1 + result.new_items().len());
    assert_eq!(normalized.len(), preserve.len());
    assert_eq!(result.original_input().len(), 1);

    // 原始输入排在最前，产出按发生顺序跟在后面。
    assert!(matches!(preserve[0], ModelInputItem::Message(_)));
    assert!(matches!(preserve[1], ModelInputItem::ToolCall(_)));
    assert!(matches!(preserve[2], ModelInputItem::ToolCallOutput(_)));
}

#[tokio::test]
async fn 流式路径把每一轮的记录当场推出来并且照样给出终态() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(request(vec![tool], &model, &cancel));
    let mut turns = Vec::new();
    let mut items = Vec::new();
    let mut finished = None;
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::TurnStarted { turn, agent } => turns.push((turn, agent)),
            RunStreamEvent::Item(item) => items.push(item.id().as_str().to_owned()),
            RunStreamEvent::Finished(outcome) => finished = Some(outcome),
            _ => panic!("未知事件"),
        }
    }

    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].0, 1);
    // 报幕用的是公共身份，宿主显示的就是用户配置的那个 agent。
    assert_eq!(turns[0].1.as_str(), "coder");
    assert_eq!(items, ["c-1", "call-1.output", "msg-1"]);
    assert!(matches!(
        finished,
        Some(RunOutcome::Completed {
            reason: FinishReason::Final
        })
    ));

    // 把事件读干净了，终态照样拿得到——两者是同一次 run 的两个视图，不是一根用完就没的管子。
    let result = stream.finish().await.unwrap();
    assert_eq!(result.turns(), 2);
    assert_eq!(result.new_items().len(), 3);
}

#[tokio::test]
async fn 只要终态时不必先把事件读完() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "完事了")])]);
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(request(Vec::new(), &model, &cancel));
    let result = stream.finish().await.unwrap();

    assert_eq!(result.turns(), 1);
    assert_eq!(
        result.final_message().unwrap().content()[0]
            .as_text()
            .unwrap(),
        "完事了"
    );
}

#[tokio::test]
async fn 两条路径产出同一个结果() {
    let script = || {
        vec![
            ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
            ModelResponse::new(vec![message("msg-1", "改完了")]),
        ]
    };
    let cancel = CancelScope::root();

    let direct_model = ScriptedModel::new(script());
    let direct = Runner::run(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &direct_model,
        &cancel,
    ))
    .await
    .unwrap();

    let streamed_model = ScriptedModel::new(script());
    let streamed = Runner::run_streamed(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &streamed_model,
        &cancel,
    ))
    .finish()
    .await
    .unwrap();

    // 两个入口共用同一个 loop 与同一套结算。会分叉的话，修好的那条路会把另一条的 bug 藏起来。
    assert_eq!(direct.turns(), streamed.turns());
    assert_eq!(
        direct
            .new_items()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        streamed
            .new_items()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        direct.outcome().finish_reason(),
        streamed.outcome().finish_reason()
    );
}

#[tokio::test]
async fn run_级别的模型覆盖每一轮都生效() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();
    let selectors: Selectors = Arc::new(Mutex::new(Vec::new()));

    Runner::run(
        request_recording(vec![tool], &model, &cancel, Arc::clone(&selectors))
            .with_config(RunConfig::new().with_model("run/override")),
    )
    .await
    .unwrap();

    // 每一轮都要带上，不能只在第一轮生效——一个被静默忽略的 run 级设置，
    // 症状是账单和延迟对不上，而不是一条报错。
    assert_eq!(
        selectors.lock().unwrap().clone(),
        [
            Some("run/override".to_owned()),
            Some("run/override".to_owned())
        ]
    );
}

#[tokio::test]
async fn 工具轨迹随结果交回以便下一段接着数() {
    // 每一段用各自的 call_id：provider 每次调用都会新铸一个，而 tracker 的重放判据
    // 正是按 call_id 认的——复用它会让第二段被当成第一段的重放。
    let script = |segment: u32| {
        vec![
            ModelResponse::new(vec![tool_call(
                &format!("c-{segment}"),
                &format!("call-{segment}"),
                "write_file",
            )]),
            ModelResponse::new(vec![message(&format!("msg-{segment}"), "改完了")]),
        ]
    };
    let cancel = CancelScope::root();

    let first_model = ScriptedModel::new(script(1));
    let first = Runner::run(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &first_model,
        &cancel,
    ))
    .await
    .unwrap();
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    assert_eq!(
        first
            .tool_use()
            .repeat_streak(&AgentId::new("coder"), &identity),
        1
    );

    // 接着跑第二段时把它带上。换成一个空 tracker，连续段会归零——
    // 「暂停再继续」就成了绕开 R3-6 熔断器的办法。
    let second_model = ScriptedModel::new(script(2));
    let second = Runner::run(
        request(
            vec![Arc::new(ScriptedTool::new("write_file"))],
            &second_model,
            &cancel,
        )
        .with_tool_use(first.tool_use().clone()),
    )
    .await
    .unwrap();

    assert_eq!(
        second
            .tool_use()
            .repeat_streak(&AgentId::new("coder"), &identity),
        2
    );
}

#[tokio::test]
async fn 丢掉流不会连累调用方自己的作用域() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "完事了")])]);
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(request(Vec::new(), &model, &cancel));
    drop(stream);

    // 丢掉流会取消**这一次** run（否则 provider 还在为没人等的结果花钱），但它取消的是
    // 一个子作用域。连调用方的作用域一起取消，等于一个 run 的生命周期决定了整个宿主的。
    assert!(!cancel.is_cancelled());

    // 同一个作用域还能再起一次 run。
    let again = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "又完事了")])]);
    let result = Runner::run(request(Vec::new(), &again, &cancel))
        .await
        .unwrap();
    assert_eq!(result.turns(), 1);
}
