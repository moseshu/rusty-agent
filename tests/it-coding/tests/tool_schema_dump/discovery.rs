//! Obtain the discovery schema from real turn preparation, without duplicating its description.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::Result,
    model::{
        ApiProtocol, Model, ModelResolver, ModelSelector, ModelSettings, ModelToolDefinition,
        ProviderKey, ResolvedModel,
    },
    state::{RunId, ToolUseTracker},
    tool::{Tool, ToolContext, ToolExposure, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
use ra_runtime::{
    agent::AgentBinding,
    turn::prepare::{TurnPreparationRequest, prepare_turn},
};
use serde_json::json;

/// Only the deferred catalog entry is a fixture; discovery is supplied by the runtime.
struct CatalogEntry {
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for CatalogEntry {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_exposure(ToolExposure::Deferred)
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        unreachable!("schema preparation must not execute a deferred tool")
    }
}

struct Resolver(Arc<dyn Model>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                None,
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

pub(super) async fn definition() -> ModelToolDefinition {
    let entry = CatalogEntry {
        origin: ToolOrigin::new("catalog_entry").expect("fixture identity"),
        schema: ToolSchema::loose("catalog_entry", json!({"type":"object"}))
            .expect("fixture schema"),
    };
    let binding = AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("discovery-schema"))
            .name("Discovery schema")
            .tools(vec![Arc::new(entry) as Arc<dyn Tool>])
            .build()
            .expect("discovery agent"),
    );
    let resolver = Resolver(Arc::new(
        OpenAiResponsesModel::new("schema-test", OpenAiAuth::new("test-secret"))
            .expect("model adapter"),
    ));
    let run = RunContext::new(RunId::new("discovery-schema"), binding.public());
    let prepared = prepare_turn(TurnPreparationRequest::new(
        &binding,
        &resolver,
        &run,
        &CancelScope::root(),
        &ToolUseTracker::new(),
        vec![],
    ))
    .await
    .expect("prepare native discovery");
    assert_eq!(prepared.request().tools().len(), 1);
    let definition = &prepared.request().tools()[0];
    assert!(definition.kind().is_tool_search());
    definition.clone()
}
