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
