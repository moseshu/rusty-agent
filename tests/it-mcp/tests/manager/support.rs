//! Task-bound lifecycle probes adapted from the Python manager tests.

use async_trait::async_trait;
use ra_core::error::{Error, Result};
use ra_mcp::{McpServer, McpServerManager, McpServerManagerOptions};
use rmcp::model::{CallToolResult, GetPromptResult, JsonObject, ListPromptsResult, Tool};
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore};

pub type Events = Arc<Mutex<Vec<(String, &'static str, tokio::task::Id)>>>;

pub struct Probe {
    pub name: String,
    pub events: Events,
    pub owner: Mutex<Option<tokio::task::Id>>,
    pub connects: AtomicUsize,
    pub cleanups: AtomicUsize,
    pub failures: AtomicUsize,
    pub connect_panics: AtomicUsize,
    pub allow_unconnected_cleanup: AtomicBool,
    pub connect_cancelled: AtomicBool,
    pub connect_error_with_join_source: AtomicBool,
    pub cleanup_error: AtomicUsize,
    pub block_connect: AtomicBool,
    pub block_cleanup: AtomicBool,
    pub connect_started: Notify,
    pub cleanup_started: Notify,
    pub cleanup_finished: Notify,
    pub connect_gate: Semaphore,
    pub cleanup_gate: Semaphore,
    pub open: AtomicBool,
    pub connect_dropped: AtomicUsize,
    pub cleanup_dropped: AtomicUsize,
}

impl Probe {
    pub fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            events: Arc::default(),
            owner: Mutex::new(None),
            connects: AtomicUsize::new(0),
            cleanups: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            connect_panics: AtomicUsize::new(0),
            allow_unconnected_cleanup: AtomicBool::new(false),
            connect_cancelled: AtomicBool::new(false),
            connect_error_with_join_source: AtomicBool::new(false),
            cleanup_error: AtomicUsize::new(0),
            block_connect: AtomicBool::new(false),
            block_cleanup: AtomicBool::new(false),
            connect_started: Notify::new(),
            cleanup_started: Notify::new(),
            cleanup_finished: Notify::new(),
            connect_gate: Semaphore::new(0),
            cleanup_gate: Semaphore::new(0),
            open: AtomicBool::new(false),
            connect_dropped: AtomicUsize::new(0),
            cleanup_dropped: AtomicUsize::new(0),
        })
    }

    pub fn connect_count(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
    pub fn cleanup_count(&self) -> usize {
        self.cleanups.load(Ordering::SeqCst)
    }

    fn event(&self, action: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((self.name.clone(), action, tokio::task::id()));
    }
}

struct Dropped<'a>(&'a AtomicUsize, bool);
impl Drop for Dropped<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl McpServer for Probe {
    fn name(&self) -> &str {
        &self.name
    }

    async fn connect(&self) -> Result<()> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.event("connect");
        if self
            .connect_panics
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            panic!("lifecycle probe connect panicked");
        }
        *self.owner.lock().unwrap() = Some(tokio::task::id());
        if self.open.swap(true, Ordering::SeqCst) {
            return Err(Error::config("connect called without cleanup"));
        }
        let mut dropped = Dropped(&self.connect_dropped, false);
        self.connect_started.notify_one();
        if self.block_connect.load(Ordering::SeqCst) {
            self.connect_gate.acquire().await.unwrap().forget();
        }
        dropped.1 = true;
        if self.connect_cancelled.load(Ordering::SeqCst) {
            return Err(Error::cancelled("server cancelled connect"));
        }
        if self.connect_error_with_join_source.load(Ordering::SeqCst) {
            return Err(error_with_panic_join_source().await);
        }
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            return Err(Error::config("connect failed after opening resource"));
        }
        Ok(())
    }

    async fn cleanup(&self) -> Result<()> {
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        self.event("cleanup");
        if self.allow_unconnected_cleanup.load(Ordering::SeqCst)
            && self.owner.lock().unwrap().is_none()
        {
            return Ok(());
        }
        assert_eq!(
            *self.owner.lock().unwrap(),
            Some(tokio::task::id()),
            "cleanup moved to a different task"
        );
        let mut dropped = Dropped(&self.cleanup_dropped, false);
        self.cleanup_started.notify_one();
        if self.block_cleanup.load(Ordering::SeqCst) {
            self.cleanup_gate.acquire().await.unwrap().forget();
        }
        dropped.1 = true;
        match self.cleanup_error.load(Ordering::SeqCst) {
            1 => return Err(Error::config("cleanup failed")),
            2 => return Err(Error::cancelled("server cancelled cleanup")),
            3 => {
                self.open.store(false, Ordering::SeqCst);
                *self.owner.lock().unwrap() = None;
                panic!("lifecycle probe cleanup panicked");
            }
            4 => return Err(error_with_panic_join_source().await),
            _ => {}
        }
        self.open.store(false, Ordering::SeqCst);
        self.cleanup_finished.notify_one();
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Tool>> {
        unreachable!("lifecycle probe")
    }
    async fn call_tool(
        &self,
        _: &str,
        _: Option<Value>,
        _: Option<JsonObject>,
    ) -> Result<CallToolResult> {
        unreachable!("lifecycle probe")
    }
    async fn list_prompts(&self) -> Result<ListPromptsResult> {
        unreachable!("lifecycle probe")
    }
    async fn get_prompt(&self, _: &str, _: Option<JsonObject>) -> Result<GetPromptResult> {
        unreachable!("lifecycle probe")
    }
}

async fn error_with_panic_join_source() -> Error {
    let source = tokio::spawn(async { panic!("server-owned child task panicked") })
        .await
        .expect_err("child task must panic");
    Error::config("server operation failed").with_source(source)
}

pub fn manager(probes: &[Arc<Probe>], options: McpServerManagerOptions) -> Arc<McpServerManager> {
    Arc::new(McpServerManager::with_options(
        probes
            .iter()
            .map(|probe| probe.clone() as Arc<dyn McpServer>),
        options,
    ))
}

pub async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

pub fn panic_join_source(error: &ra_core::error::Error) -> &tokio::task::JoinError {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(error) = cause.downcast_ref::<tokio::task::JoinError>() {
            return error;
        }
        source = cause.source();
    }
    panic!("expected the original panic JoinError in the source chain");
}
