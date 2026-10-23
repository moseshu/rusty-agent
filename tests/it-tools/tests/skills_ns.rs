//! The Codex skills namespace port and its explicit Rust host adaptations.

use std::{collections::BTreeMap, sync::Arc};

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::{Error, Result},
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolCaller, ToolContext, ToolOutput},
};
use ra_tools::skills_ns::{SkillProvider, SkillProviders, catalog::*, provider::*, skill_tools};
use serde_json::{Value, json};

#[path = "skills_ns/aliases.rs"]
mod aliases;
#[path = "skills_ns/budget.rs"]
mod budget;
#[path = "skills_ns/list.rs"]
mod list;
#[path = "skills_ns/provider.rs"]
mod provider;
#[path = "skills_ns/read.rs"]
mod read;
#[path = "skills_ns/wire.rs"]
mod wire;

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    RunContext::new(RunId::new("skills-test"), &agent)
}

async fn call(tool: &dyn Tool, args: Value) -> Result<ToolOutput> {
    tool.call(ToolContext::new(&run(), tool, &CallId::new("call"), &args))
        .await
}

async fn response(tool: &dyn Tool, args: Value) -> Value {
    serde_json::from_str(call(tool, args).await.unwrap().as_text().unwrap()).unwrap()
}

fn error_message(error: Error) -> String {
    match error {
        Error::Tool { message, .. } => message,
        other => panic!("unexpected error: {other}"),
    }
}

fn entry(
    kind: SkillSourceKind,
    authority: &str,
    package: &str,
    resource: &str,
) -> SkillCatalogEntry {
    SkillCatalogEntry::new(
        SkillPackageId(package.to_string()),
        SkillAuthority::new(kind, authority),
        package.rsplit('/').next().unwrap_or(package),
        "Fix lint errors.",
        SkillResourceId::new(resource),
    )
}

#[derive(Clone)]
struct StaticProvider {
    catalog: SkillCatalog,
    contents: String,
}
impl SkillProvider for StaticProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Ok(self.catalog.clone()) })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            Ok(SkillReadResult {
                resource: request.resource,
                contents: self.contents.clone(),
            })
        })
    }
}
fn cloud(catalog: SkillCatalog, contents: &str) -> SkillProviders {
    SkillProviders::new().with_cloud_provider(Arc::new(StaticProvider {
        catalog,
        contents: contents.into(),
    }))
}
fn tools(providers: SkillProviders, budget: usize) -> Vec<Arc<dyn Tool>> {
    skill_tools(providers, BTreeMap::new(), budget).unwrap()
}

fn executor_source() -> SkillProviders {
    SkillProviders::new().with_executor_provider(Arc::new(ExecutorSkillProvider::new(
        SkillCatalog::default(),
    )))
}
fn roots(id: &str, workspace: ra_exec::fs::Workspace) -> BTreeMap<String, ra_exec::fs::Workspace> {
    [(id.to_string(), workspace)].into()
}
async fn call_in_run(tool: &dyn Tool, args: Value, run_id: &str) -> Result<ToolOutput> {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new(run_id), &agent);
    tool.call(ToolContext::new(&run, tool, &CallId::new("call"), &args))
        .await
}
