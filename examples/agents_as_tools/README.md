# Agents as tools

The manager calls a French translator using `specialist.as_tool()`. The specialist
receives the generated `input` argument and returns its final text as the tool output.
The manager consumes that output and produces the answer. A handoff instead passes
the conversation to another agent; see [handoffs](../handoffs/README.md).

```bash
cargo run -p agents_as_tools -- --check
```

The local HTTP check uses the shipped Responses provider for all three calls. It
verifies the specialist receives only `Hello`, its output reaches the next manager
request, and the final result still identifies the manager as the last agent.
Nested runs share the configured provider, but do not inherit the parent's full history
or session by default. Advanced nested approval, structured-input, and streaming options
remain available on `AgentToolBuilder`; this example uses the default text contract.

Dependencies: `rusty-agent` with default `openai`, `tokio` with `net`, and `serde_json`.
See [Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs), and the shared
[provider setup / HTTP fixture](../support.rs).

For a live run:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p agents_as_tools
```
