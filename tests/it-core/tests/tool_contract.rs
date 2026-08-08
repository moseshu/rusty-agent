use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result},
    item::CallId,
    tool::{
        Tool, ToolApprovalPolicy, ToolAvailability, ToolCaller, ToolFailureHandling,
        ToolGuardrailId, ToolInvocation, ToolLookupKey, ToolNamespace, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema, ToolTimeoutBehavior,
    },
};
use serde_json::{Value, json};

#[test]
fn 两个_mcp_server_的同名工具靠_lookup_key_消歧() {
    let github = ToolOrigin::namespaced(ToolNamespace::new("mcp.github").unwrap(), "search")
        .unwrap();
    let internal = ToolOrigin::namespaced(ToolNamespace::new("mcp.internal").unwrap(), "search")
        .unwrap();

    assert_eq!(github.qualified_name(), "mcp.github.search");
    assert_eq!(internal.qualified_name(), "mcp.internal.search");
    assert_ne!(github.lookup_key(), internal.lookup_key());

    let mut restored = BTreeMap::new();
    restored.insert(github.lookup_key().clone(), "github implementation");
    restored.insert(internal.lookup_key().clone(), "internal implementation");
    assert_eq!(
        restored.get(github.lookup_key()),
        Some(&"github implementation")
    );
    assert_eq!(
        restored.get(internal.lookup_key()),
        Some(&"internal implementation")
    );
}

#[test]
fn lookup_key_三种形状互不等价且能稳定序列化() {
    let bare = ToolLookupKey::bare("search").unwrap();
    let namespaced =
        ToolLookupKey::namespaced(ToolNamespace::new("plugin.catalog").unwrap(), "search")
            .unwrap();
    let deferred = ToolLookupKey::deferred_top_level("search").unwrap();

    assert_ne!(bare, deferred);
    assert_ne!(bare, namespaced);
    assert_eq!(
        serde_json::to_value(&namespaced).unwrap(),
        json!({
            "kind": "namespaced",
            "namespace": "plugin.catalog",
            "name": "search"
        })
    );
    assert_eq!(
        serde_json::from_value::<ToolLookupKey>(serde_json::to_value(&deferred).unwrap()).unwrap(),
        deferred
    );
}

#[test]
fn synthetic_namespace_恢复为_deferred_而不是普通_namespaced() {
    let restored = ToolLookupKey::for_call(
        "tool_search",
        Some(ToolNamespace::new("tool_search").unwrap()),
    )
    .unwrap();

    assert!(restored.is_deferred_top_level());
    assert_eq!(restored.name(), "tool_search");
    assert!(restored.namespace().is_none());
    assert!(ToolLookupKey::namespaced(
        ToolNamespace::new("tool_search").unwrap(),
        "tool_search"
    )
    .is_err());
}

#[test]
fn origin_跨版本回写未知字段且拒绝矛盾身份() {
    let origin =
        ToolOrigin::namespaced(ToolNamespace::new("agent.reviewer").unwrap(), "inspect").unwrap();
    let mut wire = serde_json::to_value(&origin).unwrap();
    wire["future_source"] = json!({"plugin_version": 3});

    let restored: ToolOrigin = serde_json::from_value(wire).unwrap();
    assert_eq!(restored.lookup_key(), origin.lookup_key());
    assert_eq!(
        restored.unknown().get("future_source"),
        Some(&json!({"plugin_version": 3}))
    );
    assert_eq!(
        serde_json::to_value(restored).unwrap()["future_source"],
        json!({"plugin_version": 3})
    );

    let contradictory = json!({
        "schema_version": 1,
        "namespace": "mcp.github",
        "qualified_name": "mcp.internal.search",
        "lookup_key": {
            "kind": "namespaced",
            "namespace": "mcp.github",
            "name": "search"
        }
    });
    assert!(serde_json::from_value::<ToolOrigin>(contradictory).is_err());
}

#[test]
fn identity_值对象反序列化也不能绕过校验() {
    assert!(serde_json::from_value::<ToolNamespace>(json!(" namespace ")).is_err());
    assert!(
        serde_json::from_value::<ToolLookupKey>(json!({"kind": "bare", "name": ""})).is_err()
    );
    assert!(
        serde_json::from_value::<ToolLookupKey>(json!({
            "kind": "namespaced",
            "namespace": "search",
            "name": "search"
        }))
        .is_err()
    );
}

#[test]
fn tool_options_集中承载执行策略并按毫秒往返() {
    let input_guard = ToolGuardrailId::new("read_before_edit").unwrap();
    let output_guard = ToolGuardrailId::new("secret_scan").unwrap();
    let options = ToolOptions::new()
        .with_availability(ToolAvailability::Dynamic)
        .with_approval(ToolApprovalPolicy::Always)
        .with_defer_loading(true)
        .with_allowed_callers([
            ToolCaller::Programmatic,
            ToolCaller::Direct,
            ToolCaller::Direct,
        ])
        .with_timeout(Duration::from_millis(750))
        .with_timeout_behavior(ToolTimeoutBehavior::Propagate)
        .with_input_guardrail(input_guard.clone())
        .with_input_guardrail(input_guard)
        .with_output_guardrail(output_guard)
        .with_failure_handling(ToolFailureHandling::Custom);

    assert_eq!(
        options.allowed_callers(),
        Some([ToolCaller::Direct, ToolCaller::Programmatic].as_slice())
    );
    assert!(options.allows_caller(ToolCaller::Direct));
    assert_eq!(options.input_guardrails().len(), 1);
    assert_eq!(options.output_guardrails().len(), 1);

    let mut wire = serde_json::to_value(&options).unwrap();
    assert_eq!(wire["timeout"], 750);
    wire["future_executor_policy"] = json!({"version": 2});
    let restored = serde_json::from_value::<ToolOptions>(wire).unwrap();
    assert_eq!(restored.availability(), options.availability());
    assert_eq!(restored.approval(), options.approval());
    assert_eq!(restored.timeout(), options.timeout());
    assert_eq!(
        restored.unknown().get("future_executor_policy"),
        Some(&json!({"version": 2}))
    );
}

#[derive(Debug)]
struct HostContext {
    prefix: &'static str,
}

struct EchoTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("echo").unwrap(),
            schema: ToolSchema::new(
                "echo",
                json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
            )
            .unwrap()
            .with_description("Echo text with the host prefix."),
            options: ToolOptions::new().with_approval(ToolApprovalPolicy::Always),
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

    async fn call(&self, invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        let context = invocation
            .context()
            .as_any()
            .downcast_ref::<HostContext>()
            .ok_or_else(|| Error::caller("HostContext is required"))?;
        let text = invocation
            .arguments()
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::caller("text is required"))?;
        Ok(ToolOutput::text(format!("{}{text}", context.prefix)))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
}

#[tokio::test]
async fn tool_trait_对象安全且上下文_审批_与模型投影都可用() {
    let tool: Arc<dyn Tool> = Arc::new(EchoTool::new());
    let call_id = CallId::new("call_1");
    let arguments = json!({"text": "hello"});
    let context = HostContext { prefix: "host:" };
    let invocation = ToolInvocation::new(&call_id, &arguments)
        .with_caller(ToolCaller::Programmatic)
        .with_context(&context);

    let debug = format!("{invocation:?}");
    assert!(!debug.contains("hello"));

    tool.validate().unwrap();
    assert!(tool.is_enabled(&context).await.unwrap());
    assert!(tool.needs_approval(&invocation).await.unwrap());
    assert!(tool.options().allows_caller(ToolCaller::Programmatic));

    let definition = tool.model_definition();
    assert_eq!(definition.name(), "echo");
    assert_eq!(definition.description(), Some("Echo text with the host prefix."));
    assert!(definition.strict());

    let output = tool.call(invocation).await.unwrap();
    assert_eq!(output.as_text(), Some("host:hello"));
}

#[tokio::test]
async fn dynamic_policy_没有对应实现时明确失败而不是静默启用() {
    let mut tool = EchoTool::new();
    tool.options = ToolOptions::new()
        .with_availability(ToolAvailability::Dynamic)
        .with_approval(ToolApprovalPolicy::Dynamic);
    let call_id = CallId::new("call_dynamic");
    let arguments = json!({"text": "hello"});
    let invocation = ToolInvocation::new(&call_id, &arguments);

    assert!(tool.is_enabled(&HostContext { prefix: "" }).await.is_err());
    assert!(tool.needs_approval(&invocation).await.is_err());
}

#[test]
fn trait_validation_拒绝_schema_与_origin_名称漂移() {
    let mut tool = EchoTool::new();
    tool.schema = ToolSchema::new(
        "different_name",
        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
    )
    .unwrap();

    assert!(tool.validate().is_err());
}

#[test]
fn 手写_schema_不能只声明_strict_而不满足_strict() {
    // 少 additionalProperties / required 的 schema 配 strict=true 必被 provider 拒绝，
    // 所以构造期就要挡住——手写与 MCP 工具走的正是这条路径，不经过 derive 的规范化。
    let incomplete = json!({
        "type": "object",
        "properties": {"city": {"type": "string"}}
    });
    assert!(ToolSchema::new("weather", incomplete.clone()).is_err());

    // 显式声明不要 strict 时，同一份 schema 可以原样使用。
    let loose = ToolSchema::loose("weather", incomplete).unwrap();
    assert!(!loose.strict_json_schema());
    assert!(loose.validate().is_ok());
    assert!(!loose.to_model_definition().strict());

    // 嵌套对象同样受检，不只是根。
    assert!(
        ToolSchema::new(
            "weather",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["filter"],
                "properties": {
                    "filter": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            })
        )
        .is_err()
    );
}

#[test]
fn 伪造的_strict_声明在反序列化时也过不去() {
    let schema = ToolSchema::loose(
        "weather",
        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    )
    .unwrap();
    let mut wire = serde_json::to_value(&schema).unwrap();
    wire["strict_json_schema"] = json!(true);
    wire.as_object_mut().unwrap().remove("input_schema_hash");

    assert!(serde_json::from_value::<ToolSchema>(wire).is_err());
}
