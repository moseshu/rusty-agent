//! Built-in adapters outside the standard coding surface and open-class boundary probes.

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::Result,
    memory::{
        MemoryExcerpt, MemoryHits, MemoryListRequest, MemoryListing, MemoryReadRequest,
        MemorySearchRequest, MemoryStore,
    },
    sandbox::{
        CreateRequest, LazySkillSource, Manifest, SandboxClient, SandboxPathGrant, SandboxResult,
        SandboxSession, SkillLoadResult, SkillMetadata, User,
    },
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::agent::tool::AgentTool;
use ra_tools::{
    action_classes::{EDIT, READ},
    memory::{MemoryListTool, MemoryReadTool, MemorySearchTool},
    request_permissions::{RequestPermissionsPolicy, RequestPermissionsTool},
};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

struct Backend;
#[async_trait]
impl MemoryStore for Backend {
    async fn list(&self, _: MemoryListRequest) -> Result<MemoryListing> {
        unreachable!("inventory only")
    }
    async fn read(&self, _: MemoryReadRequest) -> Result<MemoryExcerpt> {
        unreachable!("inventory only")
    }
    async fn search(&self, _: MemorySearchRequest) -> Result<MemoryHits> {
        unreachable!("inventory only")
    }
}
#[async_trait]
impl RequestPermissionsPolicy for Backend {
    async fn request_permissions(
        &self,
        _: &ToolContext<'_>,
    ) -> std::result::Result<String, String> {
        unreachable!("inventory only")
    }
}
#[async_trait]
impl LazySkillSource for Backend {
    fn list_skill_metadata(
        &self,
        _: &str,
        _: &[SandboxPathGrant],
    ) -> SandboxResult<Vec<SkillMetadata>> {
        Ok(Vec::new())
    }
    async fn load_skill(
        &self,
        _: &str,
        _: &Arc<dyn SandboxSession>,
        _: &str,
        _: Option<&User>,
    ) -> SandboxResult<SkillLoadResult> {
        unreachable!("inventory only")
    }
}

struct ClassProbe {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
}
#[async_trait]
impl Tool for ClassProbe {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        unreachable!("inventory only")
    }
}

pub async fn additional_tools(workspace: &TempDir) -> Vec<(String, Arc<dyn Tool>)> {
    let mut tools: Vec<(String, Arc<dyn Tool>)> = Vec::new();
    let mut add = |family: &str, tool: Arc<dyn Tool>| {
        tools.push((format!("{family}/{}", tool.origin().qualified_name()), tool));
    };
    add(
        "memory",
        Arc::new(MemoryListTool::new(Arc::new(Backend)).unwrap()),
    );
    add(
        "memory",
        Arc::new(MemoryReadTool::new(Arc::new(Backend)).unwrap()),
    );
    add(
        "memory",
        Arc::new(MemorySearchTool::new(Arc::new(Backend)).unwrap()),
    );
    add(
        "coding",
        Arc::new(RequestPermissionsTool::new(Arc::new(Backend)).unwrap()),
    );
    for tool in ra_tools::mcp_ns::resource_tools(Vec::new()).unwrap() {
        add("mcp", tool);
    }
    let session: Arc<dyn SandboxSession> = ra_sandbox::unix_local::UnixLocalSandboxClient::new()
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root(workspace.path().to_str().unwrap())),
        )
        .await
        .unwrap()
        .into();
    add(
        "sandbox",
        Arc::new(ra_tools::sandbox::shell_tool::ExecCommandTool::new(session.clone()).unwrap()),
    );
    add(
        "sandbox",
        Arc::new(ra_tools::sandbox::shell_tool::WriteStdinTool::new(session.clone()).unwrap()),
    );
    add(
        "sandbox",
        Arc::new(
            ra_tools::sandbox::apply_patch_tool::SandboxApplyPatchTool::new(session.clone())
                .unwrap(),
        ),
    );
    add(
        "sandbox",
        Arc::new(ra_tools::sandbox::view_image::ViewImageTool::new(session.clone()).unwrap()),
    );
    let skills = ra_tools::sandbox::skills::Skills::builder()
        .lazy_from(Backend)
        .build()
        .unwrap()
        .bound_to(session);
    for tool in skills.try_tools().unwrap() {
        add("sandbox", tool);
    }
    let agent = AgentSpec::builder()
        .id(AgentId::new("nested"))
        .name("nested")
        .build()
        .unwrap();
    add(
        "agent",
        Arc::new(AgentTool::builder(agent).build().unwrap()),
    );
    for (name, classes) in [
        ("unclassified", vec![]),
        ("multiple_classes", vec![READ, EDIT]),
    ] {
        add("extension", Arc::new(ClassProbe {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(name, json!({"type":"object","properties":{},"required":[],"additionalProperties":false})).unwrap(),
            options: ToolOptions::new().with_action_classes(classes),
        }));
    }
    tools
}
