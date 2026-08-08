//! The crate boundary contract (R0-1).
//!
//! The positive path is guaranteed by this test's own imports compiling; the negative path hands
//! four separate fixtures to rustc, so an internal module cannot quietly be made `pub` again.

use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(unused_imports)]
use ra_model::{fallback, openai, protocol, provider, retry, usage};
#[allow(unused_imports)]
use ra_patch::{apply, parse, render};
#[allow(unused_imports)]
use ra_runtime::{agent, guard, runner, tool};

#[test]
fn intended_facades_are_public() {
    // Reaching this test means all facade imports above compiled from a downstream crate.
}

#[test]
fn internal_modules_are_rejected_by_rustc() {
    for fixture in [
        "runtime-internals",
        "model-codecs",
        "product-internals",
        "patch-fuzz",
    ] {
        let output = cargo_check_fixture(fixture);
        assert!(
            !output.status.success(),
            "fixture `{fixture}` should not compile; an internal module leaked into the public API"
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("private"),
            "fixture `{fixture}` failed for an unexpected reason:\n{stderr}"
        );
    }
}

fn cargo_check_fixture(name: &str) -> std::process::Output {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests_root = manifest_dir
        .parent()
        .expect("it-e2e must live directly under tests/");
    let manifest = manifest_dir.join("fixtures").join(name).join("Cargo.toml");
    let target_dir: PathBuf = tests_root.join("target").join("boundary-fixtures");

    Command::new(env!("CARGO"))
        .args(["check", "--quiet", "--offline", "--manifest-path"])
        .arg(manifest)
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .unwrap_or_else(|error| panic!("failed to run cargo for fixture `{name}`: {error}"))
}
