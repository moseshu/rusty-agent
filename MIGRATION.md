# Migration guide

Each crate documents whether its surface is `Stable`, `Evolving`, or `Internal`. A `Stable`
surface changes incompatibly only in a new `0.x` release, and every such change is listed here
with what to do about it. Upgrade all crates together: crates from two tags are two copies of
`ra-core`, and their types do not interoperate.

## 0.3.0 (unreleased)

0.3.0 is in development on `main`. This section lists the incompatible changes that have
landed so far; further changes planned for this release are added here as they land.

### Dynamic instructions are system instructions

Static, synchronous, and asynchronous instruction callbacks now all supply the model's system
instructions, as `Agent.get_system_prompt` does in the OpenAI Agents SDK. A callback runs once per
prepared model turn, and its text no longer enters conversation input or the session. Each
provider adapter places it where its protocol expects system text: Responses `instructions`, a
leading Chat Completions system message, or the Anthropic `system` block.

| Change | Who is affected | What to do |
| --- | --- | --- |
| Generated instructions are sent as system instructions instead of a trailing user message | Agents built with `AgentInstructions::dynamic` / `dynamic_fn`, or with `AgentSpecBuilder::dynamic_instructions` / `dynamic_instructions_fn` | Nothing, if system placement is what you want. To keep the old trailing-user placement, use `AgentInstructions::dynamic_tail` or `dynamic_tail_fn` |
| `ResolvedInstructions` gains a `Tail` variant; `Generated` now means system text | Code that matches `ResolvedInstructions` exhaustively | Add a `Tail` arm. Handle `Generated` as system text |
| Capability assembly no longer trims static instructions, and no longer rejects generated instructions | Hosts that relied on surrounding whitespace being removed, or on the rejection | Trim the text yourself if you need it trimmed. Only a tail source is still refused |
| Empty instructions follow each provider's wire contract | Hosts that inspect requests | Responses sends an empty string, Chat Completions omits the system message, and Anthropic omits the `system` field |

New in 0.3.0: `AgentInstructions::dynamic_sync_fn` and `dynamic_async_fn`, and the builder methods `dynamic_instructions_sync_fn` and `dynamic_instructions_async_fn`.

### `WorkStateHandle` is removed

`ra_core::state::WorkStateHandle`, `ToolServices::with_work_state`, and
`ToolServices::work_state` are removed. They were the mount point reserved for a graph
orchestration engine that is no longer planned, and nothing in the framework read them.

Keep cross-run task state in your application and reach it from tools through
`RunContext::app_context`. A fixed workflow is ordinary Rust control flow around
`Runner::run` calls. `RunState` keeps its `work_state_ref` and `graph_cursor` fields, so
checkpoints written by earlier versions still load.

### `ApplyPatchResult::output` becomes the reader (announced in 0.2.2)

0.2.2 deprecated the `ApplyPatchResult::output` constructor. Before 0.3.0 is released, the
constructor is removed and the `output_text` reader is renamed `output`, matching the field name
in the OpenAI Agents SDK.

| 0.2.x | 0.3.0 |
| --- | --- |
| `ApplyPatchResult::output(text)` | `ApplyPatchResult::default().with_output(text)` |
| `result.output_text()` | `result.output()` |

Move to `with_output` now; it already exists in 0.2.2.

## 0.2.0

Session I/O takes `SessionPath` in place of `&str`, several sandbox request types are built
through constructors and read through accessors instead of public fields, and a few sandbox
defaults and error codes follow the OpenAI Agents SDK.

| Change | Who is affected | What to do |
| --- | --- | --- |
| Session I/O takes `SessionPath<'_>` (eleven path-taking methods), and `validate_path_access` returns `PosixPath` | Third-party backends that implement `SandboxSession` | Take `SessionPath<'_>` and read its text with `as_str()`; callers pass `path.into()` |
| `ra_sandbox::listing::parse_ls_la` is deprecated in favour of `try_parse_ls_la`, which returns a `Result` and fails the whole listing when a permission field is invalid | Hosts that parse `ls -la` output themselves | Switch to `try_parse_ls_la`. The old function keeps its signature and lenient behaviour and only adds a deprecation warning |
| An `ls` whose output cannot be parsed reports `ListingUnreadable`, which is not retryable | Hosts that route on error codes | Do not retry it as a transport error |
| `PermissionsParseError` gains `ReadFlag`, `WriteFlag`, and `ExecFlag` | Code that matches the enum | The enum is `#[non_exhaustive]` and already needs a wildcard arm; distinguish the three flags if you need to |
| `ConcurrencyLimitError` messages use the SDK's configuration field names | Code that compares error strings | Match on the variant instead |
| `SandboxSessionEventBase::with_ts` is no longer a `const fn`, and timestamps are truncated to whole microseconds | Callers in a const context, or code that relies on nanosecond precision | Drop the const use; read timestamps at microsecond precision |
| Numeric timestamps are parsed as the SDK parses them: a millisecond threshold, negative values, and years 0001 to 9999 | External event sources that write numeric timestamps | Write values above 2e10 as milliseconds, or use RFC 3339 |
| A run-level capability and a sandbox agent capability of the same family are refused | Hosts that install the same family in both places | Install each family in one place |
| `SandboxAgentConfig::new()` and `default()` mean "the default capability set", no longer "no capabilities" | Runs that build a sandbox agent with `new()` and inject no default set: they used to run without capabilities and are now refused with a configuration error before a session is created | For the default set, call `SandboxRunConfig::with_default_capabilities(ra_tools::sandbox::default_capabilities)` on the run or build the agent with `ra_tools::sandbox::sandbox_agent_config()`. For no capabilities, use `SandboxAgentConfig::empty()`. `new().with_capability(..)` is unaffected |
| Capability sampling parameters deep-merge `extra_body` | Hosts that relied on a later capability replacing an earlier one's object wholesale | Have the later capability write different keys |
| `ExecRequest`, `PtyStartRequest`, `PtyWriteRequest`, and `CreateRequest` have private fields | Hosts and backends that build them as literals or read their fields | Build them with `new` plus `with_*` / `as_user` (new: `PtyStartRequest::with_timeout_s`, `CreateRequest::with_snapshot_source`, `ExecRequest::without_user`), and read through the methods of the same name, such as `request.command()` |
| The integer inside `PtyProcessId` is private | Code that writes `PtyProcessId(n)` or reads `.0` | Use `PtyProcessId::new(n)` and `.get()` |
| `UnsupportedScheme` and `EphemeralMountRemoval` have private fields | Code that reads them | Use `scheme()` on the former; build the latter only through `new` and its two constructors |
| `SandboxErrorDetails` is `#[non_exhaustive]` | Code that matches it exhaustively | Add a wildcard arm |
