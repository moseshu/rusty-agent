//! Failures and holds a test injects into a [`RolloutThreadDirectory`]'s writers and metadata
//! writes, so recovery paths that depend on a writer or a metadata write failing can be exercised
//! against the real directory.
//!
//! Only built with the `test-api` feature. A failure is injected before the operation reaches the
//! writer, so the writer is left as it was and the operation can be retried; a hold makes the
//! operation wait until the test releases it, then lets it proceed.
//!
//! [`RolloutThreadDirectory`]: super::RolloutThreadDirectory

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::rollout::{
        PersistContext, RolloutCompacted, RolloutItem, RolloutRecorder, RolloutTraceEvent,
        RolloutTraceMode,
    },
};
use tokio::sync::watch;

/// An operation a test can fail or hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RolloutFaultPoint {
    /// A writer's persist, which materializes the rollout and writes what is queued.
    Persist,
    /// A writer's shutdown.
    Shutdown,
    /// A writer's discard.
    Discard,
    /// A metadata write the directory is asked to record.
    MetadataWrite,
}

/// The failures and holds a test arms on a directory. Shared by every writer the directory
/// creates or resumes after it is attached.
#[derive(Debug, Default)]
pub struct RolloutFaults {
    failures: Mutex<Vec<(RolloutFaultPoint, usize)>>,
    holds: Mutex<Vec<(RolloutFaultPoint, watch::Receiver<bool>)>>,
    calls: Mutex<Vec<(RolloutFaultPoint, usize)>>,
}

/// Releases operations held at a point once dropped or [released](Self::release).
#[derive(Debug)]
pub struct RolloutFaultHold {
    release: watch::Sender<bool>,
}

impl RolloutFaultHold {
    /// Lets the held operations, and later ones at the same point, proceed.
    pub fn release(&self) {
        let _ = self.release.send(true);
    }
}

impl Drop for RolloutFaultHold {
    fn drop(&mut self) {
        self.release();
    }
}

impl RolloutFaults {
    /// No failures or holds armed.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Fails the next `count` operations at `point`, before they reach the writer or the store.
    pub fn fail(&self, point: RolloutFaultPoint, count: usize) {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        match failures.iter_mut().find(|(armed, _)| *armed == point) {
            Some((_, remaining)) => *remaining += count,
            None => failures.push((point, count)),
        }
    }

    /// Holds every operation at `point` until the returned handle is released or dropped.
    #[must_use]
    pub fn hold(&self, point: RolloutFaultPoint) -> RolloutFaultHold {
        let (release, released) = watch::channel(false);
        self.holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((point, released));
        RolloutFaultHold { release }
    }

    /// How many operations reached `point`, failed or not.
    #[must_use]
    pub fn calls(&self, point: RolloutFaultPoint) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(counted, _)| *counted == point)
            .map_or(0, |(_, count)| *count)
    }

    /// Counts the operation, waits while it is held, then fails it if a failure is armed.
    pub(crate) async fn enter(&self, point: RolloutFaultPoint) -> Result<()> {
        {
            let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
            match calls.iter_mut().find(|(counted, _)| *counted == point) {
                Some((_, count)) => *count += 1,
                None => calls.push((point, 1)),
            }
        }
        let held: Vec<_> = self
            .holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(armed, _)| *armed == point)
            .map(|(_, released)| released.clone())
            .collect();
        for mut released in held {
            let _ = released.wait_for(|released| *released).await;
        }
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, remaining)) = failures
            .iter_mut()
            .find(|(armed, remaining)| *armed == point && *remaining > 0)
        {
            *remaining -= 1;
            return Err(Error::session(
                SessionErrorKind::Io,
                format!("injected {point:?} failure"),
            ));
        }
        Ok(())
    }
}

/// The directory's own recorder with the armed faults in front of it.
pub(crate) struct FaultyRecorder {
    inner: Arc<dyn RolloutRecorder>,
    faults: Arc<RolloutFaults>,
}

impl FaultyRecorder {
    pub(crate) fn wrap(
        inner: Arc<dyn RolloutRecorder>,
        faults: Option<&Arc<RolloutFaults>>,
    ) -> Arc<dyn RolloutRecorder> {
        match faults {
            Some(faults) => Arc::new(Self {
                inner,
                faults: Arc::clone(faults),
            }),
            None => inner,
        }
    }
}

#[async_trait]
impl RolloutRecorder for FaultyRecorder {
    fn local_path(&self) -> Option<std::path::PathBuf> {
        self.inner.local_path()
    }

    fn record(&self, item: RolloutItem) {
        self.inner.record(item);
    }

    async fn commit_compaction(&self, compacted: RolloutCompacted) -> Result<()> {
        self.inner.commit_compaction(compacted).await
    }

    fn trace_mode(&self) -> RolloutTraceMode {
        self.inner.trace_mode()
    }

    fn record_trace(&self, event: RolloutTraceEvent) {
        self.inner.record_trace(event);
    }

    async fn flush(&self) -> Result<()> {
        self.inner.flush().await
    }

    async fn persist(&self, context: PersistContext) -> Result<()> {
        self.faults.enter(RolloutFaultPoint::Persist).await?;
        self.inner.persist(context).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.faults.enter(RolloutFaultPoint::Shutdown).await?;
        self.inner.shutdown().await
    }

    async fn shutdown_materialized(&self) -> Result<()> {
        self.persist(PersistContext::Standard).await?;
        self.flush().await?;
        self.shutdown().await
    }

    async fn discard(&self) -> Result<()> {
        self.faults.enter(RolloutFaultPoint::Discard).await?;
        self.inner.discard().await
    }
}
