use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use ra_core::{
    compat::SchemaVersion,
    error::{Error, Result},
    item::CallId,
    state::WorkStateHandle,
    tool::{
        Tool, ToolApprovalPolicy, ToolAvailability, ToolCaller, ToolConcurrency, ToolExposure,
        ToolFailureHandling, ToolGuardrailId, ToolInvocation, ToolLookupKey, ToolNamespace,
        ToolOptions, ToolOrigin, ToolOutput, ToolSchema, ToolTimeoutBehavior,
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
            "schema_version": 1,
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
fn lookup_key_未来字段原样往返但不参与身份() {
    let first_wire = json!({
        "schema_version": 2,
        "kind": "bare",
        "name": "search",
        "future_hint": {"tier": "a"}
    });
    let second_wire = json!({
        "schema_version": 2,
        "kind": "bare",
        "name": "search",
        "future_hint": {"tier": "b"}
    });

    let first: ToolLookupKey = serde_json::from_value(first_wire.clone()).unwrap();
    let second: ToolLookupKey = serde_json::from_value(second_wire).unwrap();

    // Fidelity: the version and the unknown fields are written back verbatim.
    assert_eq!(first.schema_version().get(), 2);
    assert_eq!(
        first.unknown().get("future_hint"),
        Some(&json!({"tier": "a"}))
    );
    assert_eq!(serde_json::to_value(&first).unwrap(), first_wire);

    // Identity is kind + name + namespace only. `Compatibility::Newer` requires a record from a
    // newer build to stay readable and usable; letting an arbitrary added field join identity
    // would turn that promise into a silent routing miss for every older build.
    assert_eq!(first, second);

    // A genuine new identity dimension arrives as a `ToolLookupKind`. That enum is closed, so an
    // older build fails outright instead of guessing a route from a field it cannot interpret.
    assert!(
        serde_json::from_value::<ToolLookupKey>(json!({
            "schema_version": 3,
            "kind": "partitioned",
            "name": "search"
        }))
        .is_err()
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
fn 更高版本写下的_key_仍然路由得到同一个工具() {
    let key = ToolLookupKey::bare("search").unwrap();
    let mut registry = BTreeMap::new();
    registry.insert(key.clone(), "实现");

    // Unknown fields and the schema version are forward-compatibility material, not identity:
    // folding them in turns "written by a newer build, read by an older one" into a lost route.
    let mut wire = serde_json::to_value(&key).unwrap();
    wire["schema_version"] = json!(2);
    wire["future_routing_hint"] = json!({"tier": 2});
    let restored: ToolLookupKey = serde_json::from_value(wire).unwrap();

    assert_eq!(restored, key);
    assert_eq!(registry.get(&restored), Some(&"实现"));
    assert_eq!(
        restored.unknown().get("future_routing_hint"),
        Some(&json!({"tier": 2}))
    );
    assert_eq!(restored.schema_version(), SchemaVersion::new(2));

    // A real identity difference is still distinguished.
    assert_ne!(restored, ToolLookupKey::deferred_top_level("search").unwrap());
    assert_ne!(restored, ToolLookupKey::bare("other").unwrap());

    // A whole-origin round trip stays routable too; that is the path R9 restore takes.
    let mut wire = serde_json::to_value(ToolOrigin::new("search").unwrap()).unwrap();
    wire["lookup_key"]["future_routing_hint"] = json!({"tier": 2});
    let origin: ToolOrigin = serde_json::from_value(wire).unwrap();
    assert_eq!(registry.get(origin.lookup_key()), Some(&"实现"));
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
        .with_exposure(ToolExposure::Deferred)
        .with_concurrency(ToolConcurrency::Parallel)
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
    wire["allowed_callers"] = json!(["programmatic", "direct", "direct"]);
    wire["input_guardrails"] = json!(["read_before_edit", "read_before_edit"]);
    wire["output_guardrails"] = json!(["secret_scan", "secret_scan"]);
    wire["future_executor_policy"] = json!({"version": 2});
    let restored = serde_json::from_value::<ToolOptions>(wire).unwrap();
    assert_eq!(restored.availability(), options.availability());
    assert_eq!(restored.approval(), options.approval());
    assert_eq!(restored.exposure(), ToolExposure::Deferred);
    assert_eq!(restored.concurrency(), ToolConcurrency::Parallel);
    assert_eq!(restored.timeout(), options.timeout());
    assert_eq!(
        restored.allowed_callers(),
        Some([ToolCaller::Direct, ToolCaller::Programmatic].as_slice())
    );
    assert_eq!(restored.input_guardrails().len(), 1);
    assert_eq!(restored.output_guardrails().len(), 1);
    assert_eq!(
        restored.unknown().get("future_executor_policy"),
        Some(&json!({"version": 2}))
    );
    let normalized = serde_json::to_value(restored).unwrap();
    assert_eq!(normalized["allowed_callers"], json!(["direct", "programmatic"]));
    assert_eq!(normalized["input_guardrails"], json!(["read_before_edit"]));
    assert_eq!(normalized["output_guardrails"], json!(["secret_scan"]));
}

#[test]
fn 两个默认都是保守的那一边() {
    let options = ToolOptions::new();

    // A tool written before either field existed must not become concurrently executable or
    // invisible by omission. Both defaults are the answer that costs nothing to be wrong about.
    assert_eq!(options.concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(options.exposure(), ToolExposure::Advertised);
    assert!(options.is_advertised());
    assert!(!options.is_discoverable());
}

#[test]
fn hidden_既不广播也不可被发现() {
    let hidden = ToolOptions::new().with_exposure(ToolExposure::Hidden);
    let deferred = ToolOptions::new().with_exposure(ToolExposure::Deferred);

    // `is_discoverable` is not `!is_advertised()`: `Hidden` is neither, and defining discovery as
    // the negation of advertising would index exactly the tools that must never be offered.
    assert!(!hidden.is_advertised());
    assert!(!hidden.is_discoverable());
    assert!(!deferred.is_advertised());
    assert!(deferred.is_discoverable());
}

#[test]
fn defer_loading_这个旧键当场拒收而不是落进_unknown() {
    // It used to be this field. Left unrecognized it would round-trip into `unknown` and the tool
    // would read as `Advertised` — schema budget spent every turn, and nothing says so.
    let error = serde_json::from_value::<ToolOptions>(json!({
        "schema_version": 1,
        "defer_loading": true
    }))
    .unwrap_err();

    assert!(error.to_string().contains("exposure"), "{error}");
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
async fn 任务态句柄透传到工具且不挂时是_none() {
    struct TaskState {
        plan: &'static str,
    }
    impl WorkStateHandle for TaskState {
        fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
            self
        }
    }

    let call_id = CallId::new("call_work_state");
    let arguments = json!({"text": "hello"});
    let task_state: Arc<dyn WorkStateHandle> = Arc::new(TaskState { plan: "第三步" });

    // R3-13 留的位：工具读得到宿主挂上来的任务态，且拿得回自己的具体类型。
    let invocation = ToolInvocation::new(&call_id, &arguments).with_work_state(task_state.as_ref());
    let seen = invocation
        .work_state()
        .expect("挂了任务态就该读得到")
        .as_any()
        .downcast_ref::<TaskState>()
        .expect("必须能取回宿主自己的类型");
    assert_eq!(seen.plan, "第三步");
    assert!(format!("{invocation:?}").contains("work_state"));

    // 不属于任何任务的 run 是常态，不是缺失。
    assert!(ToolInvocation::new(&call_id, &arguments).work_state().is_none());
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
    // A schema missing additionalProperties or required, paired with strict=true, is certain to
    // be rejected by the provider, so construction has to stop it. Hand-written and MCP tools take
    // exactly this path and never pass through the derive's normalization.
    let incomplete = json!({
        "type": "object",
        "properties": {"city": {"type": "string"}}
    });
    assert!(ToolSchema::new("weather", incomplete.clone()).is_err());

    // Declared non-strict, the very same schema is usable as is.
    let loose = ToolSchema::loose("weather", incomplete).unwrap();
    assert!(!loose.strict_json_schema());
    assert!(loose.validate().is_ok());
    assert!(!loose.to_model_definition().strict());

    // Nested objects are checked too, not just the root.
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

    for invalid_required in [
        json!("city"),
        json!(["city", "city"]),
        json!(["city", 7]),
        json!(["city", "undeclared"]),
    ] {
        assert!(
            ToolSchema::new(
                "weather",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": invalid_required,
                    "properties": {"city": {"type": "string"}}
                })
            )
            .is_err()
        );
    }
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
