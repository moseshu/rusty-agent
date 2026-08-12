//! R3-1c contracts for immutable agent declarations and their builder.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentInstructions, AgentSpec, ToolUseBehavior},
    error::Result,
    model::ModelSettings,
    tool::{Tool, ToolInvocation, ToolNamespace, ToolOrigin, ToolOutput, ToolSchema},
};
use serde_json::json;

struct EchoTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl EchoTool {
    fn new() -> Self {
        Self::with_origin(ToolOrigin::new("echo").unwrap())
    }

    fn namespaced(namespace: &str) -> Self {
        Self::with_origin(
            ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), "echo").unwrap(),
        )
    }

    fn with_origin(origin: ToolOrigin) -> Self {
        Self {
            origin,
            schema: ToolSchema::new(
                "echo",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("echo"))
    }
}

#[test]
fn builder_缺少必填身份时返回明确错误() {
    let missing_id = AgentSpec::builder().name("Reviewer").build().unwrap_err();
    assert!(missing_id.to_string().contains("requires a stable `id`"));

    let missing_name = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .build()
        .unwrap_err();
    assert!(
        missing_name
            .to_string()
            .contains("requires a display `name`")
    );

    assert!(
        AgentSpec::builder()
            .id(AgentId::new(" reviewer "))
            .name("Reviewer")
            .build()
            .is_err()
    );
    assert!(
        AgentSpec::builder()
            .id(AgentId::new("reviewer"))
            .name(" ")
            .build()
            .is_err()
    );
}

#[test]
fn spec_保存未解析模型与四层合并中的_agent_层() {
    let settings = ModelSettings::new()
        .with_temperature(0.2)
        .with_max_tokens(1_024);
    let agent = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .instructions("Review carefully.\nReturn concrete findings.")
        .model("openai/gpt-5")
        .model_settings(settings)
        .build()
        .unwrap();

    assert_eq!(agent.id().as_str(), "reviewer");
    assert_eq!(agent.name(), "Reviewer");
    assert_eq!(
        agent.instructions().and_then(AgentInstructions::as_static),
        Some("Review carefully.\nReturn concrete findings.")
    );
    assert_eq!(agent.model(), Some("openai/gpt-5"));
    assert_eq!(agent.model_settings().temperature(), Some(0.2));
    assert_eq!(agent.model_settings().max_tokens(), Some(1_024));
}

#[test]
fn 同名_agent_以稳定_id_区分并可反查() {
    let first = AgentSpec::builder()
        .id(AgentId::new("reviewer-primary"))
        .name("Reviewer")
        .build()
        .unwrap();
    let second = AgentSpec::builder()
        .id(AgentId::new("reviewer-shadow"))
        .name("Reviewer")
        .build()
        .unwrap();

    let by_id = BTreeMap::from([
        (first.id().clone(), Arc::clone(&first)),
        (second.id().clone(), Arc::clone(&second)),
    ]);

    assert_eq!(first.name(), second.name());
    assert_ne!(first.id(), second.id());
    assert!(Arc::ptr_eq(by_id.get(first.id()).unwrap(), &first));
    assert!(Arc::ptr_eq(by_id.get(second.id()).unwrap(), &second));
}

#[test]
fn spec_通过_arc_共享且派生_builder_复用工具实现() {
    let tool: Arc<dyn Tool> = Arc::new(EchoTool::new());
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(Arc::clone(&tool))
        .build()
        .unwrap();
    let shared = Arc::clone(&agent);
    assert!(Arc::ptr_eq(&agent, &shared));

    let variant = agent
        .to_builder()
        .name("Worker with a different display label")
        .build()
        .unwrap();
    assert_eq!(variant.id(), agent.id());
    assert_ne!(variant.name(), agent.name());
    assert!(Arc::ptr_eq(&variant.tools()[0], &tool));
}

#[test]
fn builder_拒绝重复工具身份() {
    let first: Arc<dyn Tool> = Arc::new(EchoTool::new());
    let second: Arc<dyn Tool> = Arc::new(EchoTool::new());

    let error = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([first, second])
        .build()
        .unwrap_err();

    assert!(error.to_string().contains("more than once"));
}

#[test]
fn builder_拒绝两个投射到同一模型面名字的工具() {
    let first: Arc<dyn Tool> = Arc::new(EchoTool::namespaced("server_a"));
    let second: Arc<dyn Tool> = Arc::new(EchoTool::namespaced("server_b"));

    // 查找键不同（命名空间把它们分开了），模型面名字相同——分不开就会每轮栽在 provider 上。
    assert_ne!(
        first.origin().lookup_key(),
        second.origin().lookup_key(),
        "两个命名空间应当产生不同的查找键"
    );
    assert_eq!(
        first.model_definition().name(),
        second.model_definition().name()
    );

    let error = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([Arc::clone(&first), second])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("advertises the tool name `echo`"));

    assert!(
        AgentSpec::builder()
            .id(AgentId::new("worker"))
            .name("Worker")
            .tool(first)
            .build()
            .is_ok()
    );
}

#[test]
fn 派生的_builder_能清空继承来的可选字段() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("Inherited instructions.")
        .model("openai/gpt-5")
        .tool(Arc::new(EchoTool::new()))
        .build()
        .unwrap();

    let stripped = agent
        .to_builder()
        .clear_instructions()
        .clear_model()
        .clear_tools()
        .build()
        .unwrap();

    assert!(stripped.instructions().is_none());
    assert_eq!(stripped.model(), None);
    assert!(stripped.tools().is_empty());
    // 派生不动原件：不可变契约的全部意义就在这里。
    assert!(agent.instructions().is_some());
    assert_eq!(agent.model(), Some("openai/gpt-5"));
    assert_eq!(agent.tools().len(), 1);
}

#[test]
fn debug_不泄漏指令或模型设置中的敏感值() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("private-system-instructions")
        .model_settings(ModelSettings::new().with_extra_header("authorization", "secret-token"))
        .build()
        .unwrap();

    let debug = format!("{agent:?}");
    assert!(!debug.contains("private-system-instructions"));
    assert!(!debug.contains("secret-token"));
    assert!(debug.contains("worker"));
}

#[test]
fn shared_spec_满足并发与静态生命周期契约() {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Arc<AgentSpec>>();
}

#[test]
fn tool_use_behavior_默认继续模型并在派生时保留() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
        .build()
        .unwrap();
    assert!(matches!(
        agent.tool_use_behavior(),
        ToolUseBehavior::StopOnFirstTool
    ));

    let variant = agent.to_builder().name("Worker v2").build().unwrap();
    assert!(matches!(
        variant.tool_use_behavior(),
        ToolUseBehavior::StopOnFirstTool
    ));

    let default_agent = AgentSpec::builder()
        .id(AgentId::new("default-worker"))
        .name("Default Worker")
        .build()
        .unwrap();
    assert!(matches!(
        default_agent.tool_use_behavior(),
        ToolUseBehavior::RunLlmAgain
    ));
}
