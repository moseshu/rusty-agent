//! The `ra` entry point polls commands on threads with Codex's stack budget.
//!
//! The binary's `main` cannot be reached from here, so this drives the library function it calls.

use std::hint::black_box;

/// Uses about `depth` × 64 KiB of stack, more than a default 2 MiB thread has for `depth` 64.
fn deep(depth: usize) -> u8 {
    let frame = black_box([depth as u8; 64 * 1024]);
    if depth == 0 {
        frame[0]
    } else {
        frame[depth % frame.len()].wrapping_add(deep(depth - 1))
    }
}

#[test]
fn the_cli_budget_is_the_runtimes() {
    assert_eq!(
        ra_cli::THREAD_STACK_SIZE_BYTES,
        ra_runtime::runner::THREAD_STACK_SIZE_BYTES
    );
}

// 4 MiB of frames would overflow both the calling thread's and a default worker's stack.
#[test]
fn the_entry_point_and_its_workers_have_the_budget() {
    let (on_entry, on_worker) = ra_cli::run_with_stack_budget(|| async {
        let on_entry = deep(black_box(64));
        let on_worker = tokio::spawn(async { deep(black_box(64)) }).await.unwrap();
        (on_entry, on_worker)
    })
    .unwrap();
    assert_eq!(on_entry, on_worker);
}

#[test]
fn a_panic_in_the_entry_point_reaches_the_caller() {
    let caught = std::panic::catch_unwind(|| {
        ra_cli::run_with_stack_budget(|| async { panic!("entry point failed") })
    });
    let message = caught.unwrap_err();
    assert_eq!(message.downcast_ref::<&str>(), Some(&"entry point failed"));
}
