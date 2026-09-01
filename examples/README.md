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
| **`01_basic_agent.rs`** | Basic agent conversation | Initializing a provider client, configuring the system preamble, and running a multi-turn conversation. |
| **`02_custom_tools.rs`** | Custom tools and function calling | Registering a Rust function as a tool with the `#[tool]` macro, and handling argument schemas and errors. |
| **`03_react_loop.rs`** | The ReAct loop | Reasoning-and-acting dual-channel output and self-correction, driven by the `NextStep` state machine. |
| **`04_subagent_as_tool.rs`** | Sub-agents and context isolation | Wrapping a sub-agent as a tool via `Agent::as_tool()` for file-level context isolation. |
| **`05_mcp_integration.rs`** | MCP integration | Connecting to external or in-process Model Context Protocol tool and resource servers. |

---

## Running an example

Each example is its own crate in the workspace (`examples/*` are registered as
workspace members), so they run with `-p`, not `--example`:

```bash
cargo run -p minimal_agent
```

`minimal_agent` supplies its own model and needs no environment variables. The
planned examples talk to real providers and will need credentials:

```bash
export OPENAI_API_KEY="your-openai-api-key"
# or
export GOOGLE_API_KEY="your-google-api-key"
```
