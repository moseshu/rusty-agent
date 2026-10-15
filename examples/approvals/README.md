# Approval and checkpoint recovery

`#[tool(needs_approval = true)]` pauses before the simulated note action executes.
The host serializes the interrupted `RunState`, deserializes it, approves or rejects
each pending call, then passes the state back as the next run input. It reattaches
the original session and application context. The run ID stays the same.

```bash
cargo run -p approvals -- --check
cargo run -p approvals -- --check --reject
cargo run -p approvals -- --check --stream
cargo run -p approvals -- --check --stream --reject
```

All four paths use the shipped Responses provider with local HTTP fixtures. They
verify the tool has not executed at interruption, the approved tool executes once,
the rejected tool never executes, and neither the paid-for model call nor the user
input is replayed. The rejection message is returned to the model as a tool result.
`--stream` uses streamed execution on both sides of the interruption and calls `finish`.

The sample host approves its harmless in-memory counter action by default; `--reject`
selects rejection. A product supplies its own decision UI or policy. The serialized
snapshot is round-tripped in memory and must stay under trusted host control; persist
those bytes to an application-owned store for recovery in another process. The live
context object and provider configuration are reattached separately. A session's
conversation history alone is not an executable checkpoint.

Dependencies: `rusty-agent` with default `openai`, `tokio` with `net`, and `serde_json`.
See [Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs), and the shared
[provider setup / HTTP fixture](../support.rs).

For a live run:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p approvals -- --reject
```
