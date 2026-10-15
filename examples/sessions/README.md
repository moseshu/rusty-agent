# Sessions

Pass only the new user input on each run and attach the same session. The runner loads
the earlier history and persists new input and output automatically. Two turns have two
run IDs; a session ID identifies their shared conversation.

```bash
cargo run -p sessions -- --check
cargo run -p sessions -- --check --sqlite
```

Both commands use the shipped Responses provider with a local HTTP/SSE fixture, without
external credentials. The check asserts the second request contains the first question,
answer, and new question exactly once and that storage contains four records.
`--sqlite` closes and reopens a temporary SQLite file between turns. The temporary
directory is removed when the example exits; use an application-owned path for durable use.

Dependencies: `rusty-agent` with default `openai` and explicit `sqlite`, `tokio` with `net`,
`serde_json`, and `tempfile`. See [Cargo.toml](Cargo.toml) for the complete workspace dependencies.
Shared provider setup and HTTP fixture code are in [support.rs](../support.rs);
the application workflow is in [src/main.rs](src/main.rs).

For a live run:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p sessions -- --sqlite
```

The provider is configured once and reused for both turns, then closed explicitly.
Do not append the previous result to the next input while also attaching the session:
that would submit the same history twice.
