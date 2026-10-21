# rusty-agent

An agent framework written in Rust.

> **Status.** The kernel contracts, provider implementations, prompt assembly, and
> context management (budgeting, compaction, tool-output trimming) are in place,
> alongside the crate boundaries, error and cancellation model, logging, configuration,
> separate test workspace, and extension-safety contracts.
> The public API is `0.2.x`. Each crate's documentation states whether its surface is
> `Stable`, `Evolving`, or `Internal`; a `Stable` surface changes incompatibly only in a new
> `0.x` version, with migration notes.

## What this is

A general-purpose framework for building ReAct, graph, plan-and-execute, and multi-agent
systems. The existing `ra-coding` application consumes the framework, and `examples/`
demonstrates its public APIs. Additional product lines are developed separately after the
framework is complete; building a second product is outside the framework's delivery scope.

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
4. **MCP**: Model Context Protocol clients for stdio and Streamable HTTP transports, plus
   in-process tool servers.
5. **Prompts**: Prompt assembly, stable prefixes, cache planning, and incremental reminders.
6. **Context management**: Context budgeting, compaction, archival, and bounded tool output.
7. **Sessions**: Durable event records, resume, fork, and checkpoint support.
8. **Execution**: Local processes, background jobs, and sandbox backends for tool work.
9. **Approvals**: First-class permission checks and interrupts for human-in-the-loop flows.
10. **Evaluation and protocol**: Replay fixtures and trace assertions, together with control
   protocol frames and an application-server transport.

Observability uses Rust `tracing` across model providers. The framework does not automatically
send traces to OpenAI or another external service. The host application configures subscribers
and any optional exporters; choosing a model provider does not select an observability backend.

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
Reusable         ra-tools (general tools) · ra-patch (V4A patches)
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
ra-core = { git = "https://github.com/moseshu/rusty-agent", tag = "v0.2.2" }
ra-runtime = { git = "https://github.com/moseshu/rusty-agent", tag = "v0.2.2" }
```

`ra-core` holds the types and contracts and `ra-runtime` runs the agent loop. Together they
are enough for an agent with your own model and tools, as
[`examples/minimal_agent`](examples/minimal_agent) shows. Add more crates from the same tag
as you need them:

| Crate | Adds | Features |
| --- | --- | --- |
| `ra-model` | Model providers: OpenAI Responses and Chat, Anthropic Messages, OpenAI-compatible endpoints | `openai` (default), `anthropic`, `compat` |
| `ra-macros` | `#[tool]` function tools and the `ToolInput` derive | — |
| `ra-tools` | Reusable tools, and the sandbox capabilities: filesystem, shell, skills, memory, compaction | — |
| `ra-sandbox` | Sandbox backends: unix-local and Docker | `docker` connects to a real Docker daemon; `http-sink` posts session events over HTTP |

Take every crate from the same tag. Crates from two tags are two copies of `ra-core`, and
their types do not interoperate. The crates declare Rust 1.88 as their minimum supported
version.

On the current checkout, `rusty-agent` provides a unified import surface and a configured
`AgentRunner`: configure a shipped `ProviderRegistry` once, then call
`runner.run(agent, "Hello").await?`. Optional session, context, or configuration goes through
`runner.request(agent, input).with_session(session).run().await?`. The explicit `RunRequest`
entry remains available. See the [basic agent example](examples/basic_agent/README.md) for
sync, streaming, and settled-item views; the rustdoc of `AgentRunner` and `RunRequest` describes
every entry option, including checkpoint recovery.

```bash
cargo run -p basic_agent -- --check
```

The [example index](examples/README.md) continues with runnable sessions, streaming,
application context and lifecycle callbacks, handoffs, agents as tools, approval recovery, and
guardrails, and distinguishes live provider runs from the offline checks executed in CI.

### Function tools

On this checkout, `#[ra_macros::tool]` preserves a sync or async function and adds
`function_name::tool()?` for registration through `AgentSpec::builder().tool(...)`.
It generates argument schemas and decoding and uses the existing approval, failure,
timeout, and guardrail lifecycle. Sync bodies run on the blocking pool.

Run the [function-tool example](examples/custom_tools/README.md) without credentials:

```bash
cargo run -p custom_tools -- --check
```

The same example supports a shipped provider for live runs. The rustdoc of `ra_macros::tool`
describes its options and supported types. Handwritten `Tool` implementations remain available
for custom behavior.

### Upgrading

[MIGRATION.md](MIGRATION.md) lists every incompatible change to a `Stable` surface and how to
adapt, release by release. 0.2.0 takes `SessionPath` in place of `&str` for session I/O and
builds several sandbox request types through constructors and accessors instead of public
fields. 0.3.0, in development on `main`, is collecting further changes there as they land.

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

The provider-neutral contracts follow the
[OpenAI Agents SDK for Python](https://github.com/openai/openai-agents-python), and much of the
sandbox subsystem is ported from it: its modules are translated to Rust and its model-facing
prompts are carried verbatim. That SDK is MIT-licensed; its notice and the list of what was taken
from it are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
