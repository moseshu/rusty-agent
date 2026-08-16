use std::{collections::BTreeMap, num::NonZeroU32, sync::Arc, time::Duration};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    budget::BudgetSnapshot,
    compat::SchemaVersion,
    context::RunContext,
    error::{Error, Result},
    item::{AgentId, CallId},
    state::{RunId, WorkStateHandle},
    tool::{
        DEFAULT_MAX_NO_PROGRESS_STREAK, DEFAULT_MAX_REPEAT_STREAK, Tool, ToolApprovalPolicy,
        ToolAvailability, ToolCaller, ToolConcurrency, ToolContext, ToolExposure,
        ToolFailureHandling, ToolGuardrailId, ToolLookupKey, ToolNamespace, ToolOptions,
        ToolOrigin, ToolOutput, ToolSchema, ToolServices, ToolTimeoutBehavior,
    },
};
use serde_json::{Value, json};

#[test]
fn test_tool_contract_01() {
    let github =
        ToolOrigin::namespaced(ToolNamespace::new("mcp.github").unwrap(), "search").unwrap();
    let internal =
        ToolOrigin::namespaced(ToolNamespace::new("mcp.internal").unwrap(), "search").unwrap();

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
fn test_tool_contract_02() {
    let bare = ToolLookupKey::bare("search").unwrap();
    let namespaced =
        ToolLookupKey::namespaced(ToolNamespace::new("plugin.catalog").unwrap(), "search").unwrap();
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
fn test_tool_contract_03() {
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
fn test_tool_contract_04() {
    let restored = ToolLookupKey::for_call(
        "tool_search",
        Some(ToolNamespace::new("tool_search").unwrap()),
    )
    .unwrap();

    assert!(restored.is_deferred_top_level());
    assert_eq!(restored.name(), "tool_search");
    assert!(restored.namespace().is_none());
    assert!(
        ToolLookupKey::namespaced(ToolNamespace::new("tool_search").unwrap(), "tool_search")
            .is_err()
    );
}

#[test]
fn test_tool_contract_05() {
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
fn test_tool_contract_06() {
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
    assert_ne!(
        restored,
        ToolLookupKey::deferred_top_level("search").unwrap()
    );
    assert_ne!(restored, ToolLookupKey::bare("other").unwrap());

    // A whole-origin round trip stays routable too; that is the path R9 restore takes.
    let mut wire = serde_json::to_value(ToolOrigin::new("search").unwrap()).unwrap();
    wire["lookup_key"]["future_routing_hint"] = json!({"tier": 2});
    let origin: ToolOrigin = serde_json::from_value(wire).unwrap();
    assert_eq!(registry.get(origin.lookup_key()), Some(&"实现"));
}

#[test]
fn test_tool_contract_07() {
    assert!(serde_json::from_value::<ToolNamespace>(json!(" namespace ")).is_err());
    assert!(serde_json::from_value::<ToolLookupKey>(json!({"kind": "bare", "name": ""})).is_err());
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
fn test_tool_contract_08() {
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
        .with_failure_handling(ToolFailureHandling::Custom)
        .with_max_repeat_streak(NonZeroU32::new(5).unwrap());

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
    assert_eq!(restored.max_repeat_streak(), NonZeroU32::new(5));
    assert_eq!(
        restored.max_no_progress_streak(),
        Some(DEFAULT_MAX_NO_PROGRESS_STREAK)
    );
    assert_eq!(
        restored.unknown().get("future_executor_policy"),
        Some(&json!({"version": 2}))
    );
    let normalized = serde_json::to_value(restored).unwrap();
    assert_eq!(
        normalized["allowed_callers"],
        json!(["direct", "programmatic"])
    );
    assert_eq!(normalized["input_guardrails"], json!(["read_before_edit"]));
    assert_eq!(normalized["output_guardrails"], json!(["secret_scan"]));
}

#[test]
fn test_tool_contract_09() {
    let options = ToolOptions::new();

    // A tool written before either field existed must not become concurrently executable or
    // invisible by omission. Both defaults are the answer that costs nothing to be wrong about.
    assert_eq!(options.concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(options.exposure(), ToolExposure::Advertised);
    assert!(options.is_advertised());
    assert!(!options.is_discoverable());

    // The repeat breaker is off until a tool asks for it. A refused call is still recorded as an
    // attempt, so a threshold latches: once reached, that tool stays refused for every identical
    // call in the run, and a tool whose arguments never vary has no way back.
    assert_eq!(options.max_repeat_streak(), None);
    assert_eq!(
        ToolOptions::new()
            .with_max_repeat_streak(DEFAULT_MAX_REPEAT_STREAK)
            .without_repeat_limit()
            .max_repeat_streak(),
        None
    );

    // The no-progress breaker is on, and the difference is what makes that safe: it counts
    // failures that told the run nothing new, firing clears the count, and no tool is lost for the
    // rest of the run. A tool whose repeated identical failure is not a stuck run — a readiness
    // probe — can still say so.
    assert_eq!(
        options.max_no_progress_streak(),
        Some(DEFAULT_MAX_NO_PROGRESS_STREAK)
    );
    assert_eq!(
        ToolOptions::new()
            .without_no_progress_limit()
            .max_no_progress_streak(),
        None
    );
    assert_eq!(
        ToolOptions::new()
            .with_max_no_progress_streak(NonZeroU32::new(9).unwrap())
            .max_no_progress_streak(),
        NonZeroU32::new(9)
    );
}

#[test]
fn test_tool_contract_10() {
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
fn test_tool_contract_11() {
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

/// Another host type, to check that reading application state is a checked read and not a cast.
#[derive(Debug)]
struct OtherHostContext;

/// The run every tool call in this file happens inside.
fn run_context() -> RunContext {
    let agent = agent();
    RunContext::new(RunId::new("run_tool_contract"), agent.as_ref())
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("agent_tool_contract"))
        .name("tool contract")
        .build()
        .unwrap()
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

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let host = context
            .run()
            .app_context::<HostContext>()
            .ok_or_else(|| Error::caller("HostContext is required"))?;
        let text = context
            .arguments()
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::caller("text is required"))?;
        Ok(ToolOutput::text(format!("{}{text}", host.prefix)))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
}

#[tokio::test]
async fn test_tool_contract_12() {
    let tool: Arc<dyn Tool> = Arc::new(EchoTool::new());
    let call_id = CallId::new("call_1");
    let arguments = json!({"text": "hello"});
    let run = run_context().with_app_context(Arc::new(HostContext { prefix: "host:" }));
    let context = ToolContext::new(&run, tool.as_ref(), &call_id, &arguments)
        .with_caller(ToolCaller::Programmatic);

    // Neither the model's arguments nor the host's own state belongs in a log line.
    let debug = format!("{context:?}");
    assert!(!debug.contains("hello"));
    assert!(!debug.contains("host:"));
    assert!(debug.contains("has_app_context: true"));

    tool.validate().unwrap();
    assert!(tool.is_enabled(&run).await.unwrap());
    assert!(tool.needs_approval(&context).await.unwrap());
    assert!(tool.options().allows_caller(ToolCaller::Programmatic));

    // The call knows which tool it resolved to, and knows it as the origin rather than as a name.
    assert_eq!(context.origin().lookup_key(), tool.origin().lookup_key());
    assert_eq!(context.call_id(), &call_id);

    let definition = tool.model_definition();
    assert_eq!(definition.name(), "echo");
    assert_eq!(
        definition.description(),
        Some("Echo text with the host prefix.")
    );
    assert!(definition.strict());

    let output = tool.call(context).await.unwrap();
    assert_eq!(output.as_text(), Some("host:hello"));
}

#[tokio::test]
async fn test_tool_contract_13() {
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
    let run = run_context();
    let tool = EchoTool::new();
    let task_state: Arc<dyn WorkStateHandle> = Arc::new(TaskState { plan: "step three" });
    let services = ToolServices::new().with_work_state(task_state);

    // The task-state port: a tool reads what the host mounted and gets its own type back.
    let context = ToolContext::new(&run, &tool, &call_id, &arguments).with_services(&services);
    let seen = context
        .services()
        .work_state()
        .expect("an attached task state has to be readable")
        .as_any()
        .downcast_ref::<TaskState>()
        .expect("the host has to get its own type back")
        .plan;
    assert_eq!(seen, "step three");
    assert!(format!("{context:?}").contains("work_state: true"));

    // A run that belongs to no task is the ordinary case, not a missing port.
    let bare = ToolContext::new(&run, &tool, &call_id, &arguments);
    assert!(bare.services().work_state().is_none());
}

#[tokio::test]
async fn test_tool_contract_14() {
    let mut tool = EchoTool::new();
    tool.options = ToolOptions::new()
        .with_availability(ToolAvailability::Dynamic)
        .with_approval(ToolApprovalPolicy::Dynamic);
    let call_id = CallId::new("call_dynamic");
    let arguments = json!({"text": "hello"});
    let run = run_context();
    let context = ToolContext::new(&run, &tool, &call_id, &arguments);

    assert!(tool.is_enabled(&run).await.is_err());
    assert!(tool.needs_approval(&context).await.is_err());
}

#[tokio::test]
async fn test_tool_contract_18() {
    // Application state has exactly one door, and going through it is a checked read: a tool
    // written for one host cannot reinterpret another host's object, and a run that attached
    // nothing answers the same way as one that attached something else.
    let attached = run_context().with_app_context(Arc::new(HostContext { prefix: "host:" }));
    assert_eq!(
        attached
            .app_context::<HostContext>()
            .map(|host| host.prefix),
        Some("host:")
    );
    assert!(attached.app_context::<OtherHostContext>().is_none());
    assert!(run_context().app_context::<HostContext>().is_none());

    // Everything a tool can learn about the run comes from the same object, so the prompt path and
    // the call path cannot disagree about who is running.
    let call_id = CallId::new("call_identity");
    let arguments = json!({"text": "hello"});
    let tool = EchoTool::new();
    let context = ToolContext::new(&attached, &tool, &call_id, &arguments);
    assert_eq!(context.run().run_id(), attached.run_id());
    assert_eq!(context.run().agent_id(), attached.agent_id());
    assert_eq!(context.run().agent_id().as_str(), "agent_tool_contract");
    assert_eq!(context.run().agent().name(), "tool contract");
}

#[test]
fn test_tool_contract_19() {
    // Run facts are read views taken from the state that owns them. The context reports spend; it
    // has no way to advance it, so a run cannot end up with two answers to what it has spent.
    let mut budget = BudgetSnapshot::new();
    budget.record_turn();
    let run = run_context().with_budget(budget.clone());

    assert_eq!(run.budget(), &budget);
    assert_eq!(run.budget().turns_used(), 1);

    // A later projection of the same run reports the newer spend; the earlier one is a copy and
    // stays where it was.
    let mut advanced = budget.clone();
    advanced.record_turn();
    let later = run_context().with_budget(advanced);
    assert_eq!(later.budget().turns_used(), 2);
    assert_eq!(run.budget().turns_used(), 1);
    assert_eq!(later.run_id(), run.run_id());
}

#[test]
fn test_tool_contract_15() {
    let mut tool = EchoTool::new();
    tool.schema = ToolSchema::new(
        "different_name",
        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
    )
    .unwrap();

    assert!(tool.validate().is_err());
}

#[test]
fn test_tool_contract_16() {
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
fn test_tool_contract_17() {
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
