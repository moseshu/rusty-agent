# Conversation handoff

This example uses the shipped OpenAI Responses provider. Triage transfers control to
a refund specialist, which receives the full conversation by default. The typed
`RefundTransfer` payload is validated and passed to the callback for bookkeeping;
it does not replace the specialist's conversation input.

Dependencies: `rusty-agent` with default `openai`, `schemars`, `serde`, `serde_json`,
and `tokio` with `net`. See [Cargo.toml](Cargo.toml)
and [the runnable source](src/main.rs).

Check the complete transfer against local HTTP/SSE fixtures without external credentials:

```sh
cargo run -p handoffs -- --check
cargo run -p handoffs -- --check --filter
```

The check uses the shipped provider for both model calls and verifies schema registration,
the target's input, one typed bookkeeping callback, the final agent identity, and the
authoritative session records. `--filter` removes the transfer pair from model input
while preserving it in the session. Shared provider and fixture code is in [support.rs](../support.rs).

For a live run, set `OPENAI_API_KEY` and `RA_MODEL` to a model available at your
endpoint, then run `cargo run -p handoffs`. `OPENAI_BASE_URL` and `RA_REQUEST` are
optional. A live run makes paid model calls; no live call is made by `--check`.

Run with `--filter` to explicitly forward only the opening input. The filter sets
`input_items`, which narrows what the specialist sees without changing what the
session stores; a filter that rewrites `new_items` instead changes the stored turn
too, as in the Python SDK. Per-handoff filters replace the run-level default;
server-managed conversations reject handoff input filters.

Targets remain registered through `AgentRegistry`. Declaration or registry errors
stop before a model call; invalid typed parameters and callback failures stop the
transfer before the target runs. See the handoff contract
for recovery, migration, and the deferred beta history options.
