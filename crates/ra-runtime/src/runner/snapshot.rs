//! Run-local host state shared by synchronous resolution and asynchronous processing.

use std::{any::Any, future::Future, sync::Arc};

use futures::future::BoxFuture;
use ra_core::cancel::CancelScope;

use super::RunConfig;

pub(super) type Value = Arc<dyn Any + Send + Sync>;
pub(super) type Factory = Arc<dyn Fn() -> BoxFuture<'static, Value> + Send + Sync>;

tokio::task_local! {
    static SNAPSHOT: Option<Value>;
}

pub(super) fn current() -> Option<Value> {
    SNAPSHOT.try_with(Clone::clone).ok().flatten()
}

pub(crate) async fn capture(config: &RunConfig, cancel: &CancelScope) -> Option<Value> {
    let capture = config.snapshot_factory.as_ref()?;
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        value = capture() => Some(value),
    }
}

pub(crate) async fn scope<T>(value: Option<Value>, work: impl Future<Output = T>) -> T {
    SNAPSHOT.scope(value, work).await
}
