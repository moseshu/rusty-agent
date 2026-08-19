use std::{borrow::Cow, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId},
    state::RunId,
    tool::{
        ResourceAccess, ResourceClaim, ResourceId, ResourceKind, Tool, ToolConcurrency,
        ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use serde_json::json;

#[test]
fn test_resource_kind_semantics() {
    let ws = ResourceKind::Workspace;
    let proc = ResourceKind::Process;
    let custom = ResourceKind::custom("database");

    assert_eq!(ws.as_str(), "workspace");
    assert_eq!(proc.as_str(), "process");
    assert_eq!(custom.as_str(), "database");

    assert_eq!(ws.to_string(), "workspace");
    assert_eq!(proc.to_string(), "process");
    assert_eq!(custom.to_string(), "database");

    assert_eq!(serde_json::to_value(&ws).unwrap(), json!("workspace"));
    assert_eq!(serde_json::to_value(&proc).unwrap(), json!("process"));
    assert_eq!(serde_json::to_value(&custom).unwrap(), json!("database"));

    let round_ws: ResourceKind = serde_json::from_str("\"workspace\"").unwrap();
    let round_proc: ResourceKind = serde_json::from_str("\"process\"").unwrap();
    let round_custom: ResourceKind = serde_json::from_str("\"database\"").unwrap();

    assert_eq!(round_ws, ResourceKind::Workspace);
    assert_eq!(round_proc, ResourceKind::Process);
    assert_eq!(
        round_custom,
        ResourceKind::Custom(Cow::Borrowed("database"))
    );

    assert_eq!(ResourceKind::from("workspace"), ResourceKind::Workspace);
    assert_eq!(ResourceKind::from("process"), ResourceKind::Process);
    assert_eq!(
        ResourceKind::from("custom_kind"),
        ResourceKind::Custom(Cow::Borrowed("custom_kind"))
    );
}

#[test]
fn test_resource_id_validation_and_accessors() {
    let valid_ws = ResourceId::workspace("primary_ws").unwrap();
    assert_eq!(valid_ws.kind(), &ResourceKind::Workspace);
    assert_eq!(valid_ws.value(), "primary_ws");
    assert_eq!(valid_ws.to_string(), "workspace:primary_ws");

    let valid_proc = ResourceId::process("proc-1234").unwrap();
    assert_eq!(valid_proc.kind(), &ResourceKind::Process);
    assert_eq!(valid_proc.value(), "proc-1234");
    assert_eq!(valid_proc.to_string(), "process:proc-1234");

    let valid_custom = ResourceId::custom("gpu", "device-0").unwrap();
    assert_eq!(
        valid_custom.kind(),
        &ResourceKind::Custom(Cow::Borrowed("gpu"))
    );
    assert_eq!(valid_custom.value(), "device-0");
    assert_eq!(valid_custom.to_string(), "gpu:device-0");

    assert!(ResourceId::workspace("").is_err());
    assert!(ResourceId::workspace("   ").is_err());
    assert!(ResourceId::workspace(" leading_space").is_err());
    assert!(ResourceId::workspace("trailing_space ").is_err());
    assert!(ResourceId::workspace("with\nnewline").is_err());
    assert!(ResourceId::workspace("with\0null").is_err());

    // Custom kind validation and normalization
    assert!(ResourceId::custom("", "valid_name").is_err());
    assert!(ResourceId::custom("  custom ", "valid_name").is_err());
    assert!(ResourceId::custom("custom\nkind", "valid_name").is_err());
    assert!(ResourceId::new(ResourceKind::Custom("".into()), "valid_name").is_err());
    assert!(ResourceId::new(ResourceKind::Custom("  ws ".into()), "valid_name").is_err());

    // Known categories supplied via custom constructor normalize to canonical variants
    let normalized_ws = ResourceId::custom("workspace", "repo").unwrap();
    let direct_ws = ResourceId::workspace("repo").unwrap();
    assert_eq!(normalized_ws, direct_ws);

    let raw_custom_ws = ResourceId::new(ResourceKind::Custom("workspace".into()), "repo").unwrap();
    assert_eq!(raw_custom_ws, direct_ws);

    let normalized_proc = ResourceId::custom("process", "pid-1").unwrap();
    let direct_proc = ResourceId::process("pid-1").unwrap();
    assert_eq!(normalized_proc, direct_proc);
}

#[test]
fn test_resource_id_serde_and_ordering() {
    let ws_a = ResourceId::workspace("repo_a").unwrap();
    let ws_b = ResourceId::workspace("repo_b").unwrap();
    let proc_a = ResourceId::process("repo_a").unwrap();

    assert!(ws_a < ws_b);
    assert_eq!(ws_a, ws_a.clone());
    assert_ne!(ws_a, proc_a);

    let serialized = serde_json::to_value(&ws_a).unwrap();
    let deserialized: ResourceId = serde_json::from_value(serialized).unwrap();
    assert_eq!(ws_a, deserialized);
}

#[test]
fn test_resource_id_deserialization_rejects_invalid_values() {
    for invalid in [
        json!({ "kind": "workspace", "value": "" }),
        json!({ "kind": "workspace", "value": " repo" }),
        json!({ "kind": "workspace", "value": "repo\nname" }),
        json!({ "kind": " custom ", "value": "repo" }),
    ] {
        assert!(
            serde_json::from_value::<ResourceId>(invalid).is_err(),
            "invalid resource ID must be rejected"
        );
    }
}

#[test]
fn test_resource_access_and_conflicts() {
    let shared = ResourceAccess::Shared;
    let exclusive = ResourceAccess::Exclusive;

    assert!(shared.is_shared());
    assert!(!shared.is_exclusive());
    assert!(!exclusive.is_shared());
    assert!(exclusive.is_exclusive());

    assert!(!shared.conflicts_with(ResourceAccess::Shared));
    assert!(shared.conflicts_with(ResourceAccess::Exclusive));
    assert!(exclusive.conflicts_with(ResourceAccess::Shared));
    assert!(exclusive.conflicts_with(ResourceAccess::Exclusive));

    assert_eq!(serde_json::to_value(shared).unwrap(), json!("shared"));
    assert_eq!(serde_json::to_value(exclusive).unwrap(), json!("exclusive"));

    let de_shared: ResourceAccess = serde_json::from_str("\"shared\"").unwrap();
    let de_exclusive: ResourceAccess = serde_json::from_str("\"exclusive\"").unwrap();
    assert_eq!(de_shared, ResourceAccess::Shared);
    assert_eq!(de_exclusive, ResourceAccess::Exclusive);
}

#[test]
fn test_resource_claim_semantics() {
    let res_a = ResourceId::workspace("repo_a").unwrap();
    let res_b = ResourceId::workspace("repo_b").unwrap();

    let claim_a_shared = ResourceClaim::shared(res_a.clone());
    let claim_a_exclusive = ResourceClaim::exclusive(res_a.clone());
    let claim_b_exclusive = ResourceClaim::exclusive(res_b.clone());

    assert_eq!(claim_a_shared.resource(), &res_a);
    assert_eq!(claim_a_shared.access(), ResourceAccess::Shared);
    assert!(claim_a_shared.is_shared());
    assert!(!claim_a_shared.is_exclusive());

    assert_eq!(claim_a_exclusive.resource(), &res_a);
    assert_eq!(claim_a_exclusive.access(), ResourceAccess::Exclusive);
    assert!(!claim_a_exclusive.is_shared());
    assert!(claim_a_exclusive.is_exclusive());

    assert!(!claim_a_shared.conflicts_with(&claim_a_shared));
    assert!(claim_a_shared.conflicts_with(&claim_a_exclusive));
    assert!(claim_a_exclusive.conflicts_with(&claim_a_shared));
    assert!(claim_a_exclusive.conflicts_with(&claim_a_exclusive));

    // Disjoint resources never conflict regardless of access mode
    assert!(!claim_a_exclusive.conflicts_with(&claim_b_exclusive));
    assert!(!claim_a_shared.conflicts_with(&claim_b_exclusive));

    let json_val = serde_json::to_value(&claim_a_exclusive).unwrap();
    let de_claim: ResourceClaim = serde_json::from_value(json_val).unwrap();
    assert_eq!(claim_a_exclusive, de_claim);
}

#[test]
fn test_tool_options_with_resource_claims() {
    let res_a = ResourceId::workspace("repo_a").unwrap();
    let res_b = ResourceId::workspace("repo_b").unwrap();

    let claim_a_shared = ResourceClaim::shared(res_a.clone());
    let claim_a_exclusive = ResourceClaim::exclusive(res_a.clone());
    let claim_b_shared = ResourceClaim::shared(res_b.clone());

    // Initially empty
    let options = ToolOptions::default().with_concurrency(ToolConcurrency::Parallel);
    assert!(options.resource_claims().is_empty());

    // Adding claims with duplicate deduplication (exclusive supersedes shared)
    let options = options
        .with_resource_claim(claim_a_shared)
        .with_resource_claim(claim_b_shared)
        .with_resource_claim(claim_a_exclusive.clone());

    assert_eq!(options.resource_claims().len(), 2);
    assert_eq!(options.resource_claims()[0], claim_a_exclusive);
    assert_eq!(options.resource_claims()[1], ResourceClaim::shared(res_b));

    // Serde roundtrip preserves resource claims
    let json_val = serde_json::to_value(&options).unwrap();
    let restored: ToolOptions = serde_json::from_value(json_val).unwrap();
    assert_eq!(options.resource_claims(), restored.resource_claims());
}

#[test]
fn test_resource_claim_deduplicate_helper() {
    let ws_a = ResourceId::workspace("repo_a").unwrap();
    let ws_b = ResourceId::workspace("repo_b").unwrap();
    let ws_c = ResourceId::workspace("repo_c").unwrap();

    let input = vec![
        ResourceClaim::shared(ws_b.clone()),
        ResourceClaim::shared(ws_a.clone()),
        ResourceClaim::exclusive(ws_a.clone()),
        ResourceClaim::shared(ws_c.clone()),
    ];

    let dedup = ResourceClaim::deduplicate(input);
    assert_eq!(dedup.len(), 3);
    // Canonical order by ResourceId: repo_a < repo_b < repo_c
    assert_eq!(dedup[0], ResourceClaim::exclusive(ws_a));
    assert_eq!(dedup[1], ResourceClaim::shared(ws_b));
    assert_eq!(dedup[2], ResourceClaim::shared(ws_c));
}

struct StaticClaimTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
}

struct DynamicClaimTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for StaticClaimTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }
}

#[async_trait]
impl Tool for DynamicClaimTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }

    async fn resource_claims(&self, context: &ToolContext<'_>) -> Result<Vec<ResourceClaim>> {
        let ws_name = context
            .arguments()
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let ws = ResourceId::workspace(ws_name.to_owned())?;
        Ok(vec![ResourceClaim::exclusive(ws)])
    }
}

fn test_run_context() -> (Arc<AgentSpec>, RunContext) {
    let agent = AgentSpec::builder()
        .id(AgentId::new("agent-01"))
        .name("test-agent")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("run-01"), agent.as_ref());
    (agent, run)
}

#[tokio::test]
async fn test_tool_resource_claims_evaluation() {
    let (_agent, run_context) = test_run_context();

    let res_ws = ResourceId::workspace("my_workspace").unwrap();
    let static_tool = StaticClaimTool {
        origin: ToolOrigin::new("static_tool").unwrap(),
        schema: ToolSchema::new(
            "static_tool",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
        options: ToolOptions::default().with_resource_claim(ResourceClaim::shared(res_ws.clone())),
    };

    let call_id = CallId::new("call_01");
    let args = json!({"workspace": "dynamic_ws"});
    let tool_ctx = ToolContext::new(&run_context, &static_tool, &call_id, &args);

    let claims = static_tool.resource_claims(&tool_ctx).await.unwrap();
    assert_eq!(claims, vec![ResourceClaim::shared(res_ws)]);

    let dynamic_tool = DynamicClaimTool {
        origin: ToolOrigin::new("dynamic_tool").unwrap(),
        schema: ToolSchema::new(
            "dynamic_tool",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
    };
    let dyn_claims = dynamic_tool.resource_claims(&tool_ctx).await.unwrap();
    assert_eq!(
        dyn_claims,
        vec![ResourceClaim::exclusive(
            ResourceId::workspace("dynamic_ws").unwrap()
        )]
    );
}

#[test]
fn test_options_validation_rejects_exclusive_with_claims() {
    let ws = ResourceId::workspace("repo").unwrap();
    let claim = ResourceClaim::shared(ws);

    // Default concurrency is Exclusive: declaring claims must fail validation
    let invalid_exclusive = ToolOptions::default().with_resource_claim(claim.clone());
    assert!(invalid_exclusive.validate().is_err());

    // Explicit Exclusive + claim must fail validation
    let explicit_exclusive = ToolOptions::new()
        .with_concurrency(ToolConcurrency::Exclusive)
        .with_resource_claim(claim.clone());
    assert!(explicit_exclusive.validate().is_err());

    // Parallel concurrency with claims must pass validation
    let valid_parallel = ToolOptions::new()
        .with_concurrency(ToolConcurrency::Parallel)
        .with_resource_claim(claim);
    assert!(valid_parallel.validate().is_ok());

    // Exclusive concurrency without claims must pass validation
    let valid_exclusive = ToolOptions::new().with_concurrency(ToolConcurrency::Exclusive);
    assert!(valid_exclusive.validate().is_ok());

    let mut serialized = serde_json::to_value(valid_parallel).unwrap();
    serialized["concurrency"] = json!("exclusive");
    assert!(serde_json::from_value::<ToolOptions>(serialized).is_err());
}
