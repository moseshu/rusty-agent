//! Configured host services with observable backend calls, without replacement tool handlers.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use ra_coding::CodingHost;
use ra_core::{
    context::RunContext,
    error::{Error, Result},
    item::CallId,
    skill::{SkillCatalog, SkillDocument, SkillId, SkillSummary},
    user_input::{
        RequestUserInputAnswer, RequestUserInputArgs, RequestUserInputResponse, UserInputPort,
    },
    web::{WebAccess, WebDocument, WebFetchRequest, WebResult, WebSearchRequest, WebSearchResults},
};

#[derive(Default)]
pub struct Services {
    pub calls: Mutex<Vec<String>>,
    pub fail_listing: AtomicBool,
}

impl Services {
    pub fn install(self: &Arc<Self>, host: CodingHost) -> CodingHost {
        host.with_web_access(self.clone())
            .with_skill_catalog(self.clone())
            .with_user_input(self.clone())
            .with_collaboration()
    }
}

#[async_trait]
impl WebAccess for Services {
    async fn search(&self, request: WebSearchRequest) -> Result<WebSearchResults> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("search:{}", request.query()));
        Ok(WebSearchResults::new(vec![WebResult::new(
            "https://example.test/guide",
            "Rust guide",
        )]))
    }

    async fn fetch(&self, request: WebFetchRequest) -> Result<WebDocument> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("fetch:{}", request.address()));
        Ok(WebDocument::new(request.address(), "backend document body"))
    }
}

#[async_trait]
impl SkillCatalog for Services {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        if self.fail_listing.load(Ordering::SeqCst) {
            return Err(Error::caller("catalog unavailable"));
        }
        Ok(vec![SkillSummary::new(
            SkillId::new("rust-guide"),
            "Rust guide",
            "Review Rust changes.",
        )])
    }

    async fn load(&self, skill: &SkillId) -> Result<SkillDocument> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("skill:{}", skill.as_str()));
        Ok(SkillDocument::new(
            skill.clone(),
            "Rust guide",
            "skill instructions from the catalog",
        ))
    }
}

#[async_trait]
impl UserInputPort for Services {
    async fn request_user_input(
        &self,
        _run: &RunContext,
        _call_id: &CallId,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        assert!(!args.is_blocking());
        self.calls.lock().unwrap().push("user_input".to_owned());
        Some(RequestUserInputResponse::new().with_answer(
            args.questions()[0].id(),
            RequestUserInputAnswer::new(["Proceed"]),
        ))
    }
}
