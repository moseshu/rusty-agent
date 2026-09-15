# tests/ — a separate test workspace

## Why a workspace of its own

1. **The main dependency graph stays free of test-only crates.** `insta`, `wiremock`,
   and `rstest` appear only here. `cargo build --workspace` in the main repository
   never pulls a test-only dependency to produce a build, and `cargo tree` shows the
   real product dependencies and nothing else.
2. **Zero test code in the kernel and the services.** `#[cfg(test)] mod tests` is
   **forbidden** under `crates/`, enforced in CI by `cargo xtask no-inline-tests`.

**This directory is committed** — only `target/`, `Cargo.lock`, and the personal
scratch area `test-gemini/` are ignored. The assertions are the only evidence that
anything here is actually done; they have to be reviewable and runnable in CI, and
putting them in an ignored directory leaves that evidence on one machine.

## Layout

```
tests/
├── Cargo.toml        # a standalone workspace
├── it-core/          # one host crate per crate under test
│   ├── src/lib.rs    # empty; exists only so cargo treats this as a crate
│   └── tests/*.rs    # the actual test cases
├── it-model/
├── ...
├── it-eval/          # host for ra-eval (every library crate gets one)
└── it-e2e/           # cross-crate, end to end
```

The crates under `it-e2e/fixtures/` are compile-time negative fixtures. Each declares
an empty `[workspace]`, so none of them becomes a member of the test workspace. R0-1
uses them to verify that internal modules really are unreachable from downstream; they
run under `cargo check --offline` so a network failure cannot be mistaken for a
boundary failure.

Empty test files are never created ahead of time. A test file must be created together
with its first real assertion. `cargo xtask test` first **derives** the host mapping
**from `crates/`** — every library crate must have an `it-<suffix>` host that path-depends
on it — and rejects placeholder files with no test functions, before it ever starts
Cargo. The host table is not written down anywhere: add a library crate, and the gate
demands its host the same day. `it-e2e/tests/workspace_contract.rs` uses that same
derivation to assert the inverse — workspace isolation, zero inline tests, and the
modern `foo.rs + foo/` module layout.

## The `test-api` feature

Two crates (`ra-exec`, `ra-coding`) carry a `test-api` feature whose only contents are
thin re-exports of private functions, gated as `#[cfg(feature = "test-api")] pub mod
test_api`. It exists because the alternative is worse in both directions: making those
functions `pub` puts them in the public surface forever, and leaving them private means
the behaviour nobody can reach is also the behaviour nobody can protect.

What it is *for* is narrow, and both current cases are the same shape — **code that
cannot be exercised from a test on the machine most people are sitting at**:

- `ra-exec::sandbox::bwrap::test_api` — the Linux BPF filter and the descriptor that
  carries it to bubblewrap. Neither compiles on macOS. Before this door existed, the
  line that attaches the syscall policy could be deleted and every test on every
  platform still passed.
- `ra-coding::doctor::test_api` — fault injection for the sandbox self-check: a probe
  shell that does not exist, one that lies, one that never finishes. Each is a way the
  check used to report a pass it had not earned, and none can be produced by calling
  the public entry point.

Three rules keep it from turning into a second API:

1. **Re-exports only.** No logic lives in a `test_api` module; if a helper is worth
   writing, it is worth writing where the code is and re-exporting.
2. **It is not a workaround for an awkward public API.** If a test wants something
   because a *caller* would want it too, that is a missing public method, not a door.
3. **`--all-features` compiles it.** The `feature-matrix` gate covers both extremes, so
   a `test_api` that stops compiling fails CI like anything else.
4. **It lands in the public-surface baseline.** `cargo xtask public-api` parses source
   rather than compiling, so it records a `test_api` item even on a platform where the
   module is `cfg`'d out — which is the point: widening the door shows up as a baseline
   diff in review instead of passing unnoticed. (`ra-coding` has no baseline, being a
   product nothing depends on, so its door is reviewed the ordinary way.)

## Running

```bash
cargo xtask test                                        # everything (preferred entry point)
cargo xtask test -p it-core                             # a single host
```

The default runner is `cargo test`. The gate's output states which runner it used, so
that two green runs are actually comparable.

Plain cargo works too:

```bash
cargo test --manifest-path tests/Cargo.toml -p it-core
```

### Why it is slow, and what helps

This is **fifty-odd separate test binaries**. The assertions themselves cost almost
nothing — the log is a wall of `finished in 0.00s` — and the entire cost is compiling,
linking, and starting one process after another.

1. **`[profile.dev] debug = 0`**, already set in `tests/Cargo.toml`. Debug info is the
   largest thing the linker has to move, so turning it off cuts link time directly and
   keeps `tests/target` from reaching tens of gigabytes. A failing assertion still
   prints `file:line` — that comes from the panic location — only the backtrace loses
   line numbers. Pass `--config profile.dev.debug=2` when you need a debugger.

2. **Every newly linked binary costs an extra fifteen-odd seconds on its first run.**
   This is macOS performing an online check on an unsigned, un-notarized executable the
   first time it runs. The shape is unmistakable (numbers from one development machine):

   | Case | Time |
   | --- | --- |
   | Freshly linked, first execution | ~18s, at 0% CPU throughout |
   | The same binary again | 0.008s |
   | The same content at a different path | ~1s (cached by content, not by path) |

   One change to `ra-core` relinks fifty-odd binaries, which is **≈16 minutes of pure
   waiting** that has nothing to do with the tests; changing a single test file relinks
   only that one. Granting the terminal an exemption under **System Settings → Privacy &
   Security → Developer Tools** does **not** remove this — that setting governs "allow
   software that does not meet the policy to run", not the online lookup itself. What
   actually helps is a clear network path from the machine to Apple's verification
   endpoint; a proxy, VPN, or firewall blocking it produces exactly this fixed
   fifteen-second timeout.

3. **[cargo-nextest](https://nexte.st) is opt-in** (`cargo install cargo-nextest --locked`,
   configured in `tests/.config/nextest.toml`):

   ```bash
   RA_TEST_RUNNER=nextest cargo xtask test
   ```

   It parallelizes across binaries, which is precisely the disease here. **It is not the
   default:** running it over the whole repository on this machine stalls during the list
   phase — the enumeration subprocess hangs indefinitely at 0% CPU — while a single host
   returns instantly. The cause is unknown and only reproduces at scale. A gate that
   hangs is worse than a gate that is slow, so the default stays `cargo test`.

4. **Run only the affected hosts while iterating** and save the full run for the one
   before committing. Note that `-p it-core` and a full build resolve features
   differently, so alternating between them forces a rebuild; picking one and staying
   with it for a session is cheapest.

## Testing private items

Integration tests can only reach the `pub` API. When internal behavior genuinely has to
be covered, add a `testing` feature to the crate in question, expose a
`#[doc(hidden)] pub mod testing` in `src/` that re-exports the internal items, and turn
it on from the test side with `features = ["testing"]`.

**This is an exception, not a habit.** First ask whether the behavior belongs in the
public contract to begin with.
