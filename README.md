# rusty-agent

An agent framework written in Rust, plus the reference products built with it.

> **Status.** The kernel contracts, provider implementations, prompt assembly, and
> context management (budgeting, compaction, tool-output trimming) are in place,
> alongside the crate boundaries, error and cancellation model, logging, configuration,
> separate test workspace, and extension-safety contracts.
> The public API is `0.1.x` and carries no stability guarantee.

## What this is

Two things, not one:

1. **A general-purpose agent framework** — for building ReAct, graph-engineering,
   plan-and-execute, and multi-agent systems;
2. **Reference products built on that framework** — their job is to force the
   framework's abstractions into the right shape.

## Layering

Deciding where a piece of code belongs takes two questions:

| Question | If yes |
| --- | --- |
| Would this change for an agent in another domain? | Product content |
| It wouldn't — but would you rewrite it for another product? | Reusable component |
| Both answers are no | Kernel mechanism |

```
Product          ra-coding (coding agent) · third-party products
Reusable         ra-flow (orchestration and graph engine, planned) · ra-tools (general tools) · ra-patch (V4A patches)
Kernel           ra-runtime (loop kernel) · ra-core (types and contracts)
Shared services  ra-model · ra-prompt · ra-context · ra-session · ra-exec · ra-mcp
```

Dependencies flow one way, enforced in CI: the kernel does not depend on reusable
components or products, reusable components do not depend on products, and products
have zero dependencies on each other.

## Crates

| Crate | Responsibility |
| --- | --- |
| `ra-core` | Shared types and contracts: `RunItem` / `ModelRequest` / `Tool` / `Capability` / `Guard` / `Permission` / `RunState` |
| `ra-macros` | `#[derive(ToolInput)]` and `#[tool]` procedural macros |
| `ra-model` | Provider implementations: OpenAI Responses, OpenAI Chat, Anthropic Messages, OpenAI-compatible |
| `ra-prompt` | Prompt assembly, stable prefixes, cache planning, incremental reminders |
| `ra-context` | Context budgeting, compaction, eviction, archiving |
| `ra-runtime` | Loop kernel: turn settlement, tool dispatch, guards and hooks, approval interrupts |
| `ra-session` | Event log, session storage, resume, fork, checkpoint |
| `ra-exec` | Processes, background jobs, sandbox backends |
| `ra-mcp` | MCP client (stdio / SSE / HTTP) and in-process tool servers |
| `ra-tools` | General-purpose tools shared across products |
| `ra-protocol` | Control-protocol frames, transport, app server |
| `ra-eval` | Fixtures, replay, trace assertions, cost and discipline reports |
| `ra-patch` | V4A `apply_patch` parsing and application |
| `ra-coding` | Reference product: a coding agent |
| `ra-cli` | Command-line entry point |

## Building

Requires Rust 1.97.1, pinned in `rust-toolchain.toml`.

```bash
cargo check --workspace
```

Behavior tests live in a separate `tests/` workspace, so the main dependency graph
stays free of test-only crates. Run them, along with the repository's gates, with:

```bash
cargo xtask all
```

## License

Apache-2.0. See [LICENSE](LICENSE).
