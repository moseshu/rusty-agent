# rusty-agent

An agent framework written in Rust, plus the reference products built with it.

> **Status.** The kernel contracts, provider implementations, prompt assembly, and
> context management (budgeting, compaction, tool-output trimming) are in place,
> alongside the crate boundaries, error and cancellation model, logging, configuration,
> separate test workspace, and extension-safety contracts.
> The public API is `0.2.x`. Each crate's documentation states whether its surface is
> `Stable`, `Evolving`, or `Internal`; a `Stable` surface changes incompatibly only in a new
> `0.x` version, with migration notes.

## What this is

Two things, not one:

1. **A general-purpose agent framework** — for building ReAct, graph-engineering,
   plan-and-execute, and multi-agent systems;
2. **Reference products built on that framework** — their job is to force the
   framework's abstractions into the right shape.

The goal is a high-performance agent runtime: a single-binary deployment, no
interpreter or GC in the hot path, bounded and predictable memory under long
runs, and concurrency that the type system makes safe rather than convention.
The conceptual model it starts from is not new, and is credited below.

Use it when an application needs to own its agent loop, tool execution, session history,
approval policy, and provider choice instead of delegating those boundaries to a hosted
agent runtime. The framework is suitable for interactive coding agents, workflow and
graph agents, long-running tool-using assistants, and embedded or self-hosted products.

### Core concepts

1. **Agent runtime**: Runs model turns, dispatches tools, and settles each agent turn.
2. **Model providers**: A shared model contract with OpenAI Responses, OpenAI Chat,
   Anthropic Messages, and OpenAI-compatible adapters.
3. **Tools**: Typed tool contracts, input schemas, registration, and execution dispatch.
4. **MCP**: Model Context Protocol clients for stdio, SSE, and HTTP transports, plus
   in-process tool servers.
5. **Prompts**: Prompt assembly, stable prefixes, cache planning, and incremental reminders.
6. **Context management**: Context budgeting, compaction, archival, and bounded tool output.
7. **Sessions**: Durable event records, resume, fork, and checkpoint support.
8. **Execution**: Local processes, background jobs, and sandbox backends for tool work.
9. **Approvals**: First-class permission checks and interrupts for human-in-the-loop flows.
10. **Evaluation and protocol**: Replay fixtures and trace assertions, together with control
   protocol frames and an application-server transport.

## Why Rust

Agent runtimes spend much of their time coordinating fallible, concurrent work: streaming
model responses, launching or supervising tools, enforcing approvals, preserving session
history, and fitting a growing conversation into a context window. Rust makes those
boundaries explicit in the program's types and ownership model.

That choice is practical rather than ideological:

- A deployable agent can be a single native binary, without an interpreter or a garbage
  collector on the execution path.
- Ownership and async types make cancellation, resource lifetime, and concurrent tool
  execution explicit instead of relying on convention.
- The provider-neutral core can remain a small, strongly typed contract while provider
  wire formats, sandboxing, and product policy stay in their own layers.
- Durable session records, bounded tool-result projections, and context compaction can be
  implemented without silently retaining unbounded in-memory copies.

Rust does not make model output deterministic or eliminate network failure. It gives the
host application a more explicit and testable way to manage those realities.

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

## Installation

The crates are not published to crates.io. Depend on the ones you need from this repository
at a release tag:

```toml
[dependencies]
ra-core = { git = "https://github.com/moseshu/rusty-agent", tag = "v0.2.1" }
ra-runtime = { git = "https://github.com/moseshu/rusty-agent", tag = "v0.2.1" }
```

`ra-core` holds the types and contracts and `ra-runtime` runs the agent loop. Together they
are enough for an agent with your own model and tools, as
[`examples/minimal_agent`](examples/minimal_agent) shows. Add more crates from the same tag
as you need them:

| Crate | Adds | Features |
| --- | --- | --- |
| `ra-model` | Model providers: OpenAI Responses and Chat, Anthropic Messages, OpenAI-compatible endpoints | `openai` (default), `anthropic`, `compat` |
| `ra-tools` | Reusable tools, and the sandbox capabilities: filesystem, shell, skills, memory, compaction | — |
| `ra-sandbox` | Sandbox backends: unix-local and Docker | `docker` connects to a real Docker daemon; `http-sink` posts session events over HTTP |

Take every crate from the same tag. Crates from two tags are two copies of `ra-core`, and
their types do not interoperate. The crates declare Rust 1.88 as their minimum supported
version.

### Upgrading from v0.1.0

0.2.0 changes the public API. Session I/O takes `SessionPath` in place of `&str`, and several
sandbox request types are built through constructors and read through accessors instead of
public fields. Section 13.6 of the
[R8-P0 checklist](Docs/R8-P0_上游沙箱契约与差异清单.md) (in Chinese) lists each change and
how to adapt.

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

A command is only confined if the platform's sandbox backend actually works — Seatbelt
on macOS, bubblewrap on Linux. This reports what will confine commands here and then
proves it, by writing outside the workspace and reaching for the network on purpose:

```bash
cargo run -p ra-cli --bin ra -- doctor sandbox
```

It exits non-zero when it cannot conclude that this machine confines a command, and
distinguishes "not confined" from "could not be checked" in its output.

## Acknowledgements

This project draws conceptual inspiration from the
[OpenAI Agents SDK for Python](https://github.com/openai/openai-agents-python).

## License

Apache-2.0. See [LICENSE](LICENSE).
