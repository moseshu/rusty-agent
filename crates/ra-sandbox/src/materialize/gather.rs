//! Running several units of materialization at once, and keeping the answers in order.

use std::future::Future;

use futures::StreamExt;
use ra_core::sandbox::SandboxResult;

/// Awaits `tasks` with at most `max_concurrency` in flight, and returns their results in order.
///
/// `None` means unbounded: every task starts at once. The results are indexed by the task that
/// produced them, never by the order they happened to finish, because a receipt that reordered
/// itself under load would describe a different materialization each run.
///
/// The first observed failure drops the remaining futures. Callers must await any asynchronous
/// cleanup those futures own before returning the error, as the reference cancels and joins workers.
///
/// # Errors
///
/// Returns the first observed failure, regardless of its position in the input.
pub(crate) async fn gather_in_order<T, F>(
    tasks: Vec<F>,
    max_concurrency: Option<usize>,
) -> SandboxResult<Vec<T>>
where
    F: Future<Output = SandboxResult<T>>,
{
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    // Clamped to the number of tasks: a window wider than the work does nothing, and a window of
    // zero would stall. A zero limit is refused where the limits are built, so this only guards
    // against arithmetic, not against configuration.
    let in_flight = max_concurrency.unwrap_or(tasks.len()).clamp(1, tasks.len());

    let count = tasks.len();
    let mut results: Vec<Option<T>> = (0..count).map(|_| None).collect();
    let indexed = tasks
        .into_iter()
        .enumerate()
        .map(|(index, task)| async move { task.await.map(|value| (index, value)) });
    let mut running = futures::stream::iter(indexed).buffer_unordered(in_flight);
    while let Some(result) = running.next().await {
        let (index, value) = result?;
        results[index] = Some(value);
    }
    Ok(results.into_iter().flatten().collect())
}
