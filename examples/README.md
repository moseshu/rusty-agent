# Examples

Runnable examples and sample code for getting familiar with the **`rusty-agent`**
framework.

---

## What this directory is for

The framework's core modules — the components under `crates/` and the surrounding
infrastructure — are still being built out against the development plan. As
capabilities land, this directory picks up the corresponding examples.

### Shipped

| Example | Topic | Notes |
| :--- | :--- | :--- |
| **[`basic_agent/`](basic_agent/README.md)** | First run with an existing provider | One `rusty-agent` dependency, configured `AgentRunner`, text input, sync/async, settled text/items, session and borrowed dynamic context; deterministic `--check`. |
| **[`custom_tools/`](custom_tools/README.md)** | Function tools | Sync/async `#[tool]`, parameter docs/defaults, injected context, and fallible output through the existing Runner; shipped provider plus deterministic `--check`. |
| **[`structured_output/`](structured_output/README.md)** | Typed final results | Type-generated schema, validated Rust values, ordinary/streamed `--check`, invalid JSON and type mismatch handling. |
| **`minimal_agent/`** | A minimal custom agent | Depends on `ra-core` and `ra-runtime` only. Brings its own `Tool`, `Model`, and `ModelResolver`, and runs the two rounds of "tool call → final answer" entirely offline. |

`minimal_agent` **is a gate, not a tutorial.** It exists to prove that a third party
can assemble a working agent from the kernel alone, so its dependencies are pinned to
those two crates by `ALLOWED_INTERNAL_DEPS` in `xtask/src/layering.rs`. Needing a third
crate to finish the example would mean a capability a third party requires has gone
into a reference product, and `cargo xtask layering` fails on the spot and names it.
**Relaxing that line is not a fix.** This covers milestone M1 and MVP acceptance item 6b.

Bringing its own `Model` is part of the same argument: the real providers live in
`ra-model`, and depending on them would only prove "you can run this project's
adapter", not "you can bring your own". The side benefit is that it needs no API key
and no network, so it runs in CI.

```bash
cargo run -p minimal_agent
```

### Planned

| Example | Topic | Concepts covered |
| :--- | :--- | :--- |
| **`sessions/`** | Multi-turn memory | Existing `InMemorySession` / `SqliteSession`, new input, and automatic history persistence. |
| **`streaming/`** | Streamed text and items | Existing `run_streamed`, `next_event`, and `finish`; show how to consume deltas and settled items without duplicating text. |
| **`app_context/`** | Context and dynamic instructions | Existing typed app-context access and dynamic instruction callbacks; keep local dependencies out of model input. |
| **[`handoffs/`](handoffs/README.md)** | Conversation transfer | Shipped OpenAI Responses provider, full history by default, explicit filtering, typed bookkeeping callback, registry validation, and an offline `--check` mode. |
| **`agents_as_tools/`** | Specialist agents as tools | Existing `as_tool()` / `AgentTool`; the parent agent consumes the specialist's result and retains control. |
| **`approvals/`** | Approval and recovery | Existing interruption/state APIs; resume with the original run identity and session. |
| **`guardrails/`** | Validation callbacks | Existing input/output guardrails and their errors; adopt convenience adapters as they land. |
| **`mcp_integration/`** | MCP integration | Connect to external or in-process MCP servers and register their tools. |

`basic_agent/`, `custom_tools/`, `handoffs/`, and `structured_output/` are shipped; the remaining rows describe planned example crates, not shipped commands. They are tracked
under R0-9 in the development plan. Existing-capability examples can be added now;
Typed-output examples accompany R1-16; the function-tool example accompanies R2-2a. The handoff example accompanies R12-1a.

The examples address a different need from `minimal_agent`: users should be able to
use shipped providers and framework APIs without reproducing extension-point fixtures.
Each example must compile in CI and have a deterministic offline check where possible.
Live-provider runs require explicit credentials and must be distinguished from mock runs.

---

## Running an example

Each example is its own crate in the workspace (`examples/*` are registered as
workspace members), so they run with `-p`, not `--example`:

```bash
cargo run -p minimal_agent
```

`minimal_agent` supplies its own model and needs no environment variables. Planned
examples that call a live provider will document the credentials they need:

```bash
export OPENAI_API_KEY="your-openai-api-key"
# or
export GOOGLE_API_KEY="your-google-api-key"
```
