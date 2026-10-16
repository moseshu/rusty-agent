# Streaming

Consume `RunStreamEvent` through `next_event`, then always call `finish` to observe
the final result or error. This example calls a local function tool before the answer.

```bash
cargo run -p streaming -- --check
cargo run -p streaming -- --check --items
```

Default rendering selects provider-neutral `ModelStreamDelta::OutputText` events and flushes text
as it arrives. `with_partial_messages(true)` enables those events. `--items` prints settled
records instead. The program observes both views for validation but renders only the
selected one. It checks tool-call and tool-output records, exactly one settled message,
and agreement between concatenated deltas and the final text. The local HTTP fixture
exercises the shipped provider's SSE parser; it does not measure network timing.

Dependencies: `rusty-agent` with default `openai`, `tokio` with `net`, and `serde_json`.
See [Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs), and the shared
[provider setup / HTTP fixture](../support.rs).

For a live run:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p streaming
```

Text deltas are provisional. `RawResponse` remains available for provider diagnostics. Settled items are authoritative
records. Appending both to the same answer duplicates text. For complete messages,
`next_text` is an optional Rust convenience view; it returns whole messages, not deltas.
All readers consume the same cursor. Dropping the stream cancels the run.
