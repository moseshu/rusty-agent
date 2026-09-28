//! A session whose workspace is a map in memory, standing in for the reference's
//! `FilesystemTestSandboxSession` and the `_FakeSession` family in its `test_runtime.py`.
//!
//! Files live in a map keyed by the path as given; a read of a path that was never written fails as
//! a backend's does. It counts the lifecycle steps the reference's fakes count, and can be told to
//! fail a stop or the dependency release.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest, OpName, SandboxError,
    SandboxResult, SandboxSession, SandboxSessionState, SessionPath, SessionResources, Snapshot,
};

/// The backend id the memory session reports.
pub const MEMORY_BACKEND_ID: &str = "memory";

pub struct MemorySession {
    state: Mutex<SandboxSessionState>,
    resources: SessionResources,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    running: AtomicBool,
    exec_result: Mutex<ExecResult>,
    stop_failure: Mutex<Option<String>>,
    close_dependencies_failure: Mutex<Option<String>>,
    pub stop_calls: AtomicUsize,
    pub shutdown_calls: AtomicUsize,
    pub close_dependency_calls: AtomicUsize,
    pub exec_calls: AtomicUsize,
    /// Every command it was asked to run, in order.
    pub exec_requests: Mutex<Vec<ExecRequest>>,
}

impl MemorySession {
    pub fn new(manifest: Manifest) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SandboxSessionState::new(
                MEMORY_BACKEND_ID,
                Snapshot::noop(),
                manifest,
            )),
            resources: SessionResources::new(),
            files: Mutex::new(BTreeMap::new()),
            running: AtomicBool::new(false),
            exec_result: Mutex::new(ExecResult::new(Vec::new(), Vec::new(), 0)),
            stop_failure: Mutex::new(None),
            close_dependencies_failure: Mutex::new(None),
            stop_calls: AtomicUsize::new(0),
            shutdown_calls: AtomicUsize::new(0),
            close_dependency_calls: AtomicUsize::new(0),
            exec_calls: AtomicUsize::new(0),
            exec_requests: Mutex::new(Vec::new()),
        })
    }

    pub fn empty() -> Arc<Self> {
        Self::new(Manifest::new().with_root("/workspace"))
    }

    /// Makes every stop fail with `message`.
    pub fn fail_stop(&self, message: &str) {
        *lock(&self.stop_failure) = Some(message.to_owned());
    }

    /// Makes releasing the dependencies fail with `message`.
    pub fn fail_close_dependencies(&self, message: &str) {
        *lock(&self.close_dependencies_failure) = Some(message.to_owned());
    }

    /// Answers every command with `result`.
    pub fn answer_exec_with(&self, result: ExecResult) {
        *lock(&self.exec_result) = result;
    }

    /// What was written at `path`, if anything.
    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        lock(&self.files).get(path).cloned()
    }

    pub fn stops(&self) -> usize {
        self.stop_calls.load(Ordering::SeqCst)
    }

    pub fn shutdowns(&self) -> usize {
        self.shutdown_calls.load(Ordering::SeqCst)
    }

    pub fn dependency_closes(&self) -> usize {
        self.close_dependency_calls.load(Ordering::SeqCst)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait]
impl SandboxSession for MemorySession {
    fn backend_id(&self) -> &str {
        MEMORY_BACKEND_ID
    }

    fn state(&self) -> SandboxSessionState {
        lock(&self.state).clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        self.exec_calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.exec_requests).push(request);
        Ok(lock(&self.exec_result).clone())
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }

    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Ok(Vec::new())
    }

    async fn rm(
        &self,
        path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        lock(&self.files).remove(path);
        Ok(())
    }

    async fn mkdir(
        &self,
        _path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn read(&self, path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        let path = path.as_str();
        lock(&self.files)
            .get(path)
            .cloned()
            .ok_or_else(|| SandboxError::workspace_read_not_found(path))
    }

    async fn write(
        &self,
        path: SessionPath<'_>,
        data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        lock(&self.files).insert(path.to_owned(), data);
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }

    async fn after_start(&self) -> SandboxResult<()> {
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> SandboxResult<()> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        match lock(&self.stop_failure).clone() {
            Some(message) => Err(SandboxError::new(
                ErrorCode::WorkspaceStopError,
                OpName::Stop,
                message,
            )),
            None => Ok(()),
        }
    }

    async fn shutdown(&self) -> SandboxResult<()> {
        self.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn close_dependencies(&self) -> SandboxResult<()> {
        self.close_dependency_calls.fetch_add(1, Ordering::SeqCst);
        self.resources.close_dependencies().await;
        match lock(&self.close_dependencies_failure).clone() {
            Some(message) => Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Shutdown,
                message,
            )),
            None => Ok(()),
        }
    }
}
