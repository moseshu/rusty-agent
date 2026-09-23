//! The session-scoped dependency container, ported from the reference's `test_dependencies.py`.
//!
//! Three of the reference's cases lean on Python cancellation that Rust does not have: a factory
//! that catches its own cancellation and returns a value anyway, a close hook that raises
//! cancellation, and the 3.12 eager task factory. Where the behaviour behind them still exists it is
//! tested here in the form Rust can express, and the case says so.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    CloseDependency, Dependencies, DependenciesError, DependencyFactoryResult, DependencyValue,
    FactoryOptions, dependency_factory,
};
use tokio::sync::Notify;

/// Counts how many times it was closed.
#[derive(Default)]
struct Closable {
    calls: AtomicUsize,
}

impl Closable {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CloseDependency for Closable {
    async fn close(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// A close that waits to be released, so a test can stop waiting on it half way.
#[derive(Default)]
struct BlockingClosable {
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
    completed: AtomicBool,
}

#[async_trait]
impl CloseDependency for BlockingClosable {
    async fn close(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        self.completed.store(true, Ordering::SeqCst);
    }
}

/// Sets a flag when the factory future holding it is dropped, which is how a test sees that a
/// running factory was stopped rather than left to finish.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn text(value: &str) -> DependencyValue {
    DependencyValue::new(Arc::new(value.to_owned()))
}

fn text_of(value: &DependencyValue) -> String {
    value
        .downcast::<String>()
        .expect("a string")
        .as_ref()
        .clone()
}

fn failure(message: &str) -> DependencyFactoryResult {
    Err(Arc::new(std::io::Error::other(message.to_owned())))
}

#[tokio::test]
async fn with_values_binds_multiple_values() {
    let dependencies = Arc::new(
        Dependencies::with_values([("tests.first", text("one")), ("tests.second", text("two"))])
            .expect("bound"),
    );

    let first = dependencies
        .require("tests.first", None)
        .await
        .expect("first");
    let second = dependencies
        .require("tests.second", None)
        .await
        .expect("second");

    assert_eq!(text_of(&first), "one");
    assert_eq!(text_of(&second), "two");
}

#[tokio::test]
async fn a_bound_value_is_returned_as_bound() {
    let dependencies = Arc::new(Dependencies::new());
    let value = text("bound");
    dependencies
        .bind_value("tests.value", value.clone(), false)
        .expect("bound");

    assert!(
        dependencies
            .require("tests.value", None)
            .await
            .expect("value")
            .same_as(&value)
    );
    assert!(
        dependencies
            .get("tests.missing")
            .await
            .expect("no failure")
            .is_none()
    );
}

#[tokio::test]
async fn a_missing_dependency_names_its_key_and_consumer() {
    let dependencies = Arc::new(Dependencies::new());

    let error = dependencies
        .require("tests.missing", Some("RemoteSnapshot"))
        .await
        .expect_err("missing");

    assert!(matches!(error, DependenciesError::Missing { .. }));
    let message = error.to_string();
    assert!(message.contains("`tests.missing`"), "{message}");
    assert!(message.contains("for RemoteSnapshot"), "{message}");
}

#[test]
fn binding_a_key_twice_is_refused_unless_overwriting() {
    let dependencies = Dependencies::new();
    dependencies
        .bind_value("tests.duplicate", text("first"), false)
        .expect("bound");

    let error = dependencies
        .bind_value("tests.duplicate", text("second"), false)
        .expect_err("duplicate");

    assert!(matches!(error, DependenciesError::AlreadyBound { .. }));
    assert!(error.is_binding_error());
    assert!(error.to_string().contains("already bound"));
    dependencies
        .bind_value("tests.duplicate", text("second"), true)
        .expect("overwrite");
}

#[test]
fn the_empty_key_is_refused() {
    let dependencies = Dependencies::new();

    let value_error = dependencies
        .bind_value("", text("value"), false)
        .expect_err("empty key");
    let factory_error = dependencies
        .bind_factory(
            "",
            FactoryOptions::default(),
            dependency_factory(|_| async { Ok(text("value")) }),
        )
        .expect_err("empty key");

    for error in [value_error, factory_error] {
        assert!(matches!(error, DependenciesError::EmptyKey));
        assert_eq!(error.to_string(), "Dependency key must be non-empty");
    }
}

#[tokio::test]
async fn a_cached_factory_resolves_once() {
    let dependencies = Arc::new(Dependencies::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    dependencies
        .bind_factory(
            "tests.cached",
            FactoryOptions::default(),
            dependency_factory(move |_| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(DependencyValue::new(Arc::new(Closable::default())))
                }
            }),
        )
        .expect("bound");

    let first = dependencies
        .require("tests.cached", None)
        .await
        .expect("first");
    let second = dependencies
        .require("tests.cached", None)
        .await
        .expect("second");

    assert!(first.same_as(&second));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_cached_factory_resolves_once_for_concurrent_callers() {
    let dependencies = Arc::new(Dependencies::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (counter, factory_started, factory_release) = (
        Arc::clone(&calls),
        Arc::clone(&started),
        Arc::clone(&release),
    );
    dependencies
        .bind_factory(
            "tests.concurrent",
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(move |_| {
                let (counter, started, release) = (
                    Arc::clone(&counter),
                    Arc::clone(&factory_started),
                    Arc::clone(&factory_release),
                );
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Ok(DependencyValue::closable(Arc::new(Closable::default())))
                }
            }),
        )
        .expect("bound");

    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let dependencies = Arc::clone(&dependencies);
            tokio::spawn(async move { dependencies.require("tests.concurrent", None).await })
        })
        .collect();
    started.notified().await;
    release.notify_one();
    let mut values = Vec::new();
    for waiter in waiters {
        values.push(waiter.await.expect("joined").expect("value"));
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(values[0].same_as(&values[1]) && values[1].same_as(&values[2]));

    dependencies.close().await;
    assert_eq!(
        values[0].downcast::<Closable>().expect("closable").calls(),
        1
    );
}

#[tokio::test]
async fn a_cached_factory_survives_one_waiter_giving_up() {
    let dependencies = Arc::new(Dependencies::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (counter, factory_started, factory_release) = (
        Arc::clone(&calls),
        Arc::clone(&started),
        Arc::clone(&release),
    );
    dependencies
        .bind_factory(
            "tests.cancelled_waiter",
            FactoryOptions::default(),
            dependency_factory(move |_| {
                let (counter, started, release) = (
                    Arc::clone(&counter),
                    Arc::clone(&factory_started),
                    Arc::clone(&factory_release),
                );
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Ok(text("survived"))
                }
            }),
        )
        .expect("bound");

    let spawn_waiter = || {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.cancelled_waiter", None).await })
    };
    let cancelled = spawn_waiter();
    let surviving = spawn_waiter();
    started.notified().await;
    cancelled.abort();
    assert!(cancelled.await.expect_err("aborted").is_cancelled());

    release.notify_one();
    let value = surviving.await.expect("joined").expect("value");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        dependencies
            .require("tests.cancelled_waiter", None)
            .await
            .expect("cached")
            .same_as(&value)
    );
}

#[tokio::test]
async fn a_failed_cached_factory_is_retried_by_the_next_resolution() {
    let dependencies = Arc::new(Dependencies::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (counter, factory_started, factory_release) = (
        Arc::clone(&calls),
        Arc::clone(&started),
        Arc::clone(&release),
    );
    dependencies
        .bind_factory(
            "tests.retry",
            FactoryOptions::default(),
            dependency_factory(move |_| {
                let (counter, started, release) = (
                    Arc::clone(&counter),
                    Arc::clone(&factory_started),
                    Arc::clone(&factory_release),
                );
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        started.notify_one();
                        release.notified().await;
                        return failure("factory failed");
                    }
                    Ok(text("recovered"))
                }
            }),
        )
        .expect("bound");

    let spawn_waiter = || {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.retry", None).await })
    };
    let (first, second) = (spawn_waiter(), spawn_waiter());
    started.notified().await;
    release.notify_one();

    // Both callers waited on the one failing run, and both are told why it failed.
    for waiter in [first, second] {
        let error = waiter.await.expect("joined").expect_err("failed");
        assert!(matches!(error, DependenciesError::Factory { .. }));
        assert_eq!(error.to_string(), "factory failed");
    }
    let recovered = dependencies
        .require("tests.retry", None)
        .await
        .expect("retry");
    assert_eq!(text_of(&recovered), "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// The reference rebinds before the stale factory has started and checks it never does. A spawned
/// task cannot be held unstarted from outside, so this rebinds while the factory is parked instead:
/// the same promise — the stale factory is stopped, its waiter is told the key was rebound, and the
/// new binding answers — seen from the other side of the factory's first await.
#[tokio::test]
async fn rebinding_stops_the_stale_factory_and_tells_its_waiter() {
    let dependencies = Arc::new(Dependencies::new());
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let (factory_started, factory_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    dependencies
        .bind_factory(
            "tests.rebind",
            FactoryOptions::default(),
            dependency_factory(move |_| {
                let (started, stopped) =
                    (Arc::clone(&factory_started), Arc::clone(&factory_stopped));
                async move {
                    let _flag = DropFlag(stopped);
                    started.notify_one();
                    futures::future::pending::<()>().await;
                    Ok(text("stale"))
                }
            }),
        )
        .expect("bound");

    let stale = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.rebind", None).await })
    };
    started.notified().await;
    dependencies
        .bind_factory(
            "tests.rebind",
            FactoryOptions::default().with_overwrite(true),
            dependency_factory(|_| async { Ok(text("replacement")) }),
        )
        .expect("rebound");

    let error = stale.await.expect("joined").expect_err("stale");
    assert!(matches!(error, DependenciesError::Rebound { .. }));
    assert!(error.to_string().contains("rebound"));
    assert!(stopped.load(Ordering::SeqCst));
    let replacement = dependencies
        .require("tests.rebind", None)
        .await
        .expect("replacement");
    assert_eq!(text_of(&replacement), "replacement");
}

/// In the reference the stale factory catches its cancellation and returns the very object the new
/// binding also returns, and that object must survive until close. A Rust factory cannot return
/// after being stopped, so the stale run produces nothing here; what remains to hold is that the
/// rebind neither closes the shared object nor closes it twice.
#[tokio::test]
async fn a_rebind_leaves_an_owned_object_open_until_close() {
    let dependencies = Arc::new(Dependencies::new());
    let value = Arc::new(Closable::default());
    let started = Arc::new(Notify::new());
    let (stale_value, factory_started) = (Arc::clone(&value), Arc::clone(&started));
    let owned = FactoryOptions::default().with_owns_result(true);
    dependencies
        .bind_factory(
            "tests.aliased",
            owned,
            dependency_factory(move |_| {
                let (value, started) = (Arc::clone(&stale_value), Arc::clone(&factory_started));
                async move {
                    started.notify_one();
                    futures::future::pending::<()>().await;
                    Ok(DependencyValue::closable(value))
                }
            }),
        )
        .expect("bound");
    let stale = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.aliased", None).await })
    };
    started.notified().await;

    let replacement_value = Arc::clone(&value);
    dependencies
        .bind_factory(
            "tests.aliased",
            owned.with_overwrite(true),
            dependency_factory(move |_| {
                let value = Arc::clone(&replacement_value);
                async move { Ok(DependencyValue::closable(value)) }
            }),
        )
        .expect("rebound");

    let error = stale.await.expect("joined").expect_err("stale");
    assert!(matches!(error, DependenciesError::Rebound { .. }));
    let resolved = dependencies
        .require("tests.aliased", None)
        .await
        .expect("value");
    assert!(Arc::ptr_eq(
        &resolved.downcast::<Closable>().expect("closable"),
        &value
    ));
    assert_eq!(value.calls(), 0);

    dependencies.close().await;
    assert_eq!(value.calls(), 1);
}

/// The reference's factory catches the close's cancellation and returns an owned object, which the
/// close must still release. A stopped Rust factory produces nothing to release, so what this holds
/// is the rest: the running factory is stopped by the close and its waiter is told the container
/// closed.
#[tokio::test]
async fn closing_stops_a_running_factory_and_tells_its_waiter() {
    let dependencies = Arc::new(Dependencies::new());
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let (factory_started, factory_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    dependencies
        .bind_factory(
            "tests.close_in_flight",
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(move |_| {
                let (started, stopped) =
                    (Arc::clone(&factory_started), Arc::clone(&factory_stopped));
                async move {
                    let _flag = DropFlag(stopped);
                    started.notify_one();
                    futures::future::pending::<()>().await;
                    Ok(DependencyValue::closable(Arc::new(Closable::default())))
                }
            }),
        )
        .expect("bound");
    let waiter = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.close_in_flight", None).await })
    };
    started.notified().await;

    dependencies.close().await;

    let error = waiter.await.expect("joined").expect_err("closed");
    assert!(error.to_string().contains("closed"), "{error}");
    assert!(stopped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_value_produced_before_close_is_refused_to_a_waiter_that_resumes_after_it() {
    let dependencies = Arc::new(Dependencies::new());
    let value = Arc::new(Closable::default());
    let factory_value = Arc::clone(&value);
    dependencies
        .bind_factory(
            "tests.close_before_resume",
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(move |_| {
                let value = Arc::clone(&factory_value);
                async move { Ok(DependencyValue::closable(value)) }
            }),
        )
        .expect("bound");

    // Polled once to start the factory, then left alone until after the close: the factory
    // finishes in between, and the waiter only looks at its value once the container has closed.
    let mut waiter = Box::pin(dependencies.require("tests.close_before_resume", None));
    assert!(futures::poll!(&mut waiter).is_pending());
    // The test runtime is single-threaded: yielding once lets the queued factory task run, and it
    // finishes in that one run because it never waits on anything.
    tokio::task::yield_now().await;
    dependencies.close().await;

    let error = waiter.await.expect_err("closed");
    assert!(matches!(
        error,
        DependenciesError::ClosedWhileResolving { .. }
    ));
    assert_eq!(value.calls(), 1);
}

#[tokio::test]
async fn an_uncached_factory_runs_on_every_resolution() {
    let dependencies = Arc::new(Dependencies::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    dependencies
        .bind_factory(
            "tests.uncached",
            FactoryOptions::default().with_cache(false),
            dependency_factory(move |_| {
                let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Ok(text(&format!("value-{call}"))) }
            }),
        )
        .expect("bound");

    let first = dependencies
        .require("tests.uncached", None)
        .await
        .expect("first");
    let second = dependencies
        .require("tests.uncached", None)
        .await
        .expect("second");

    assert_eq!(text_of(&first), "value-1");
    assert_eq!(text_of(&second), "value-2");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_factory_may_resolve_other_dependencies_itself() {
    let dependencies = Arc::new(Dependencies::new());
    dependencies
        .bind_value("tests.base", text("base"), false)
        .expect("bound");
    dependencies
        .bind_factory(
            "tests.derived",
            FactoryOptions::default(),
            dependency_factory(|dependencies| async move {
                let base = dependencies
                    .require_as::<String>("tests.base", Some("tests.derived"))
                    .await
                    .map_err(|error| -> ra_core::sandbox::DependencyFactoryError {
                        Arc::new(error)
                    })?;
                Ok(text(&format!("{base}-derived")))
            }),
        )
        .expect("bound");

    let derived = dependencies
        .require_as::<String>("tests.derived", None)
        .await
        .expect("derived");

    assert_eq!(derived.as_str(), "base-derived");
}

/// Not in the reference's file, but its `shield=False` for uncached factories says it: nobody else
/// is waiting on an uncached run, so the one caller giving up stops it.
#[tokio::test]
async fn giving_up_on_an_uncached_factory_stops_it() {
    let dependencies = Arc::new(Dependencies::new());
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let (factory_started, factory_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    dependencies
        .bind_factory(
            "tests.uncached_cancel",
            FactoryOptions::default().with_cache(false),
            dependency_factory(move |_| {
                let (started, stopped) =
                    (Arc::clone(&factory_started), Arc::clone(&factory_stopped));
                async move {
                    let _flag = DropFlag(stopped);
                    started.notify_one();
                    futures::future::pending::<()>().await;
                    Ok(text("never"))
                }
            }),
        )
        .expect("bound");
    let waiter = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.uncached_cancel", None).await })
    };
    started.notified().await;

    waiter.abort();
    assert!(waiter.await.expect_err("aborted").is_cancelled());
    while !stopped.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn close_releases_owned_results_once_and_is_idempotent() {
    let dependencies = Arc::new(Dependencies::new());
    let owned = FactoryOptions::default().with_owns_result(true);
    dependencies
        .bind_factory(
            "tests.cached_owned",
            owned,
            dependency_factory(|_| async {
                Ok(DependencyValue::closable(Arc::new(Closable::default())))
            }),
        )
        .expect("bound");
    dependencies
        .bind_factory(
            "tests.uncached_owned",
            owned.with_cache(false),
            dependency_factory(|_| async {
                Ok(DependencyValue::closable(Arc::new(Closable::default())))
            }),
        )
        .expect("bound");

    let cached = dependencies
        .require_as::<Closable>("tests.cached_owned", None)
        .await
        .expect("cached");
    let uncached_a = dependencies
        .require_as::<Closable>("tests.uncached_owned", None)
        .await
        .expect("first");
    let uncached_b = dependencies
        .require_as::<Closable>("tests.uncached_owned", None)
        .await
        .expect("second");
    assert!(!Arc::ptr_eq(&uncached_a, &uncached_b));

    dependencies.close().await;
    dependencies.close().await;

    assert_eq!(cached.calls(), 1);
    assert_eq!(uncached_a.calls(), 1);
    assert_eq!(uncached_b.calls(), 1);
}

#[tokio::test]
async fn a_close_keeps_going_when_its_caller_stops_waiting() {
    let dependencies = Arc::new(Dependencies::new());
    let value = Arc::new(BlockingClosable::default());
    let factory_value = Arc::clone(&value);
    dependencies
        .bind_factory(
            "tests.cancelled_close",
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(move |_| {
                let value = Arc::clone(&factory_value);
                async move { Ok(DependencyValue::closable(value)) }
            }),
        )
        .expect("bound");
    dependencies
        .require("tests.cancelled_close", None)
        .await
        .expect("value");

    let close_waiter = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.close().await })
    };
    value.started.notified().await;
    close_waiter.abort();
    assert!(close_waiter.await.expect_err("aborted").is_cancelled());

    value.release.notify_one();
    dependencies.close().await;

    assert_eq!(value.calls.load(Ordering::SeqCst), 1);
    assert!(value.completed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_closed_container_resolves_no_factory_but_still_returns_bound_values() {
    let dependencies = Arc::new(Dependencies::new());
    dependencies
        .bind_value("tests.value", text("bound"), false)
        .expect("bound");
    dependencies
        .bind_factory(
            "tests.factory",
            FactoryOptions::default(),
            dependency_factory(|_| async { Ok(text("built")) }),
        )
        .expect("bound");

    dependencies.close().await;

    assert!(dependencies.is_closed());
    let error = dependencies
        .require("tests.factory", None)
        .await
        .expect_err("closed");
    assert!(matches!(error, DependenciesError::Closed { .. }));
    assert_eq!(
        error.to_string(),
        "Dependencies container is closed; cannot resolve `tests.factory`"
    );
    let value = dependencies
        .require("tests.value", None)
        .await
        .expect("value");
    assert_eq!(text_of(&value), "bound");
}

#[tokio::test]
async fn a_bound_value_is_never_closed() {
    let dependencies = Arc::new(Dependencies::new());
    let value = Arc::new(Closable::default());
    dependencies
        .bind_value(
            "tests.bound_value",
            DependencyValue::closable(Arc::clone(&value)),
            false,
        )
        .expect("bound");

    dependencies
        .require("tests.bound_value", None)
        .await
        .expect("value");
    dependencies.close().await;

    assert_eq!(value.calls(), 0);
}

#[tokio::test]
async fn a_copy_shares_bindings_but_not_state() {
    let template = Dependencies::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    template
        .bind_factory(
            "tests.per_session",
            FactoryOptions::default().with_owns_result(true),
            dependency_factory(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Ok(DependencyValue::closable(Arc::new(Closable::default()))) }
            }),
        )
        .expect("bound");
    let first = Arc::new(template.clone_bindings());
    let second = Arc::new(template.clone_bindings());

    let from_first = first
        .require_as::<Closable>("tests.per_session", None)
        .await
        .expect("first");
    let from_second = second
        .require_as::<Closable>("tests.per_session", None)
        .await
        .expect("second");
    first.close().await;

    // Each copy ran the factory for itself, and closing one released only what it owned.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(!Arc::ptr_eq(&from_first, &from_second));
    assert_eq!(from_first.calls(), 1);
    assert_eq!(from_second.calls(), 0);
    assert!(!second.is_closed());
}

/// A factory that signals, then blocks its worker thread inside one poll before producing an owned
/// resource. An abort requested during that poll cannot take effect until the poll returns, which
/// is exactly the window in which a stopped factory can still produce something.
fn blocking_factory(
    started: Arc<Notify>,
    produced: Arc<Mutex<Option<Arc<Closable>>>>,
) -> ra_core::sandbox::DependencyFactory {
    dependency_factory(move |_| {
        let (started, produced) = (Arc::clone(&started), Arc::clone(&produced));
        async move {
            started.notify_one();
            std::thread::sleep(std::time::Duration::from_millis(200));
            let resource = Arc::new(Closable::default());
            *produced.lock().expect("slot") = Some(Arc::clone(&resource));
            Ok(DependencyValue::closable(resource))
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_close_waits_for_a_rebound_factory_that_was_still_running() {
    let dependencies = Arc::new(Dependencies::new());
    let started = Arc::new(Notify::new());
    let produced = Arc::new(Mutex::new(None));
    dependencies
        .bind_factory(
            "tests.late",
            FactoryOptions::default().with_owns_result(true),
            blocking_factory(Arc::clone(&started), Arc::clone(&produced)),
        )
        .expect("bound");
    let stale = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.late", None).await })
    };
    started.notified().await;

    // Rebound while the old factory is inside its poll: the abort cannot stop it before it produces.
    dependencies
        .bind_factory(
            "tests.late",
            FactoryOptions::default().with_overwrite(true),
            dependency_factory(|_| async { Ok(text("replacement")) }),
        )
        .expect("rebound");
    dependencies.close().await;

    let resource = produced.lock().expect("slot").clone().expect("produced");
    assert_eq!(resource.calls(), 1);
    assert!(stale.await.expect("joined").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_close_waits_for_an_abandoned_uncached_factory_that_was_still_running() {
    let dependencies = Arc::new(Dependencies::new());
    let started = Arc::new(Notify::new());
    let produced = Arc::new(Mutex::new(None));
    dependencies
        .bind_factory(
            "tests.late_uncached",
            FactoryOptions::default()
                .with_cache(false)
                .with_owns_result(true),
            blocking_factory(Arc::clone(&started), Arc::clone(&produced)),
        )
        .expect("bound");
    let waiter = {
        let dependencies = Arc::clone(&dependencies);
        tokio::spawn(async move { dependencies.require("tests.late_uncached", None).await })
    };
    started.notified().await;

    // The one caller gives up while the factory is inside its poll.
    waiter.abort();
    assert!(waiter.await.expect_err("aborted").is_cancelled());
    dependencies.close().await;

    let resource = produced.lock().expect("slot").clone().expect("produced");
    assert_eq!(resource.calls(), 1);
}

#[tokio::test]
async fn a_cached_factory_that_panicked_is_run_again_by_the_next_resolution() {
    let dependencies = Arc::new(Dependencies::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    dependencies
        .bind_factory(
            "tests.panicked",
            FactoryOptions::default(),
            dependency_factory(move |_| {
                let call = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert!(call > 0, "the first run panics");
                    Ok(text("recovered"))
                }
            }),
        )
        .expect("bound");

    let error = dependencies
        .require("tests.panicked", None)
        .await
        .expect_err("panicked");
    assert!(matches!(error, DependenciesError::FactoryAborted { .. }));

    let recovered = dependencies
        .require("tests.panicked", None)
        .await
        .expect("retried");
    assert_eq!(text_of(&recovered), "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
