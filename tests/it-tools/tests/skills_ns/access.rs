// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Concurrent callbacks retain their own immutable filesystem permissions.

use super::*;
use ra_core::capability::Capability;
use ra_tools::{
    capability::SkillsNsCapability,
    skills_ns::{SkillExecutorAccessPolicy, SkillFilePermissions},
};

struct AccessPolicy;
struct Permissions(bool);

impl SkillExecutorAccessPolicy for AccessPolicy {
    fn capture(&self, context: &RunContext) -> Result<Option<Arc<dyn SkillFilePermissions>>> {
        Ok(Some(Arc::new(Permissions(
            context.run_id().as_str() == "allowed",
        ))))
    }
}

impl SkillFilePermissions for Permissions {
    fn same_permissions(&self, other: &dyn SkillFilePermissions) -> bool {
        (other as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .is_some_and(|other| self.0 == other.0)
    }
    fn check_read(&self, _: &std::fs::File, _: &std::path::Path) -> std::io::Result<()> {
        if self.0 {
            Ok(())
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }
    }
}

#[tokio::test]
async fn concurrent_callbacks_do_not_share_permissions() {
    let fixture = read::ExecutorFixture::new("reference contents");
    let capability = SkillsNsCapability::new(
        fixture.providers.clone(),
        roots("selected", fixture.workspace.clone()),
        1000,
    )
    .unwrap()
    .with_executor_access_policy(Arc::new(AccessPolicy))
    .unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("worker")
        .build()
        .unwrap();
    let allowed_run = RunContext::new(RunId::new("allowed"), &agent);
    let denied_run = RunContext::new(RunId::new("denied"), &agent);
    let allowed = capability.bind(&allowed_run).unwrap().unwrap();
    let denied = capability.bind(&denied_run).unwrap().unwrap();
    assert!(allowed.context_processor().is_none());
    assert!(denied.context_processor().is_none());
    let allowed_tools = allowed.tools();
    let denied_tools = denied.tools();
    let allowed_call = CallId::new("allowed-call");
    let denied_call = CallId::new("denied-call");
    let args = fixture.reference_args();
    let (allowed_result, denied_result) = tokio::join!(
        allowed_tools[1].call(ToolContext::new(
            &allowed_run,
            allowed_tools[1].as_ref(),
            &allowed_call,
            &args
        )),
        denied_tools[1].call(ToolContext::new(
            &denied_run,
            denied_tools[1].as_ref(),
            &denied_call,
            &args
        ))
    );
    let page: Value = serde_json::from_str(allowed_result.unwrap().as_text().unwrap()).unwrap();
    assert_eq!(page["contents"], "reference contents");
    assert_eq!(
        error_message(denied_result.unwrap_err()),
        "failed to read skill resource"
    );
}
