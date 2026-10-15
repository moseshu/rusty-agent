# Basic agent

This example imports the framework and `#[tool]` from `rusty-agent` alone. It configures
`ProviderRegistry` once and calls `runner.run(agent, "Greet Rust.").await`. No custom
resolver, binding, run ID, cancellation scope, or one-message input list is required.

Dependencies: `rusty-agent` with its default `openai` feature, `async-trait`, and `tokio`.
See [Cargo.toml](Cargo.toml) and [the runnable source](src/main.rs).

```bash
cargo run -p basic_agent -- --check
cargo run -p basic_agent -- --check --sync
cargo run -p basic_agent -- --check --stream
cargo run -p basic_agent -- --check --items
```

`--check` registers a deterministic offline provider using the shipped registry. It makes no
network request. Live mode uses the shipped OpenAI Responses adapter:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p basic_agent -- --stream
```

The async path also attaches a session and an owned application context and evaluates a
dynamic instruction callback that borrows the context across an await. `--stream` reads complete,
settled assistant messages with `next_text`; `--items` reads settled records with `next_item`.
Both call `finish` to get the final result or error. They share the existing event cursor and
skip events outside their selected view. They never append raw deltas to settled messages.

The current generated `ResolvedPrompt` becomes a trailing user message, unlike Python's
system-instruction callback. This existing role-priority deviation is recorded as D-003
and tracked by task R4-11a; see the [quickstart](../../Docs/Framework_Quickstart.md#state-context-and-streamed-output).

`--sync` runs twice on the same thread to demonstrate executor and provider reuse. Call it
outside a Tokio runtime. In async applications await `run` instead.

Advanced configuration and checkpoint recovery are described in the
[run entry contract](../../Docs/Run_Entry_Contract.md).
