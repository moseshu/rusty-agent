# Migration guide

Each crate documents whether its surface is `Stable`, `Evolving`, or `Internal`. A `Stable`
surface changes incompatibly only in a new `0.x` release, and every such change is listed here
with what to do about it. Upgrade all crates together: crates from two tags are two copies of
`ra-core`, and their types do not interoperate.

## Unreleased

### Product MCP approval requests and automatic approval

Coding app-server threads now ask the client before product MCP calls execute. The effective
`tool_call_mcp_elicitation` feature selects `mcpServer/elicitation/request` (the upstream default)
or the blocking `item/tool/requestUserInput` fallback. The fallback refuses non-root agent calls
with the upstream parent-agent handoff message. MCP forms report waiting on approval; fallback
questions report waiting on user input. Declines and cancellations skip only the current call;
the model receives the rejection or `user cancelled MCP tool call` result and the turn continues.
Malformed or failed MCP form responses become declines; malformed or failed question responses
cancel only the call. Explicit turn interruption still withdraws the request, and a late answer
cannot approve another turn.

Only Auto can remember a decision for the session. Prompt/Writes downgrade session and persistent
answers to approval of the current invocation. Product approval happens inside the tool through
the existing recoverable interruption contract, so SDK `always_approve` cannot suppress a later
Prompt/Writes question. The generic SDK's approval behavior is unchanged.

The upstream never-policy automatic-approval check uses the effective shared PermissionProfile:
Disabled, External and managed full-disk-write profiles allow the call; restricted profiles refuse
calls that still require approval. Approve mode allows without a product prompt. The coding
adapter lowers its selected legacy policy through the executor's existing profile implementation,
including protected paths; listing a writable root alone does not imply unrestricted authority.

Auto's persistent choice uses the captured project/user/plugin configuration source, falling back
to a session grant if persistence fails. This does not implement the pending general loaded-thread
configuration hot-update capability. Refresh before initial startup now compares the startup
deadline as well as identity, so the first connection uses the newly selected manager budget.

The new elicitation wire records cover the product-generated standard approval form. Its empty
schema is JSON rather than the upstream general form-schema type; constructors/accessors retain
Rust public-record evolution safety. Arbitrary MCP-server forms, URL flows and OAuth/EMA lifecycle
are still separate, unimplemented transport capabilities. This change adds no account machinery.

### Product MCP policy and connection refresh corrections

The plugin MCP catalog now applies managed plugin requirements before registration, including
per-server command/URL matching and the global empty MCP allowlist. It retains upstream's
requirements-layer emptiness rules: a document containing only empty plugin maps is elided;
empty plugin maps in a retained document deny their servers.

Product MCP registrations now consume `auto`, `prompt`, `writes`, `approve` and per-tool approval
overrides. The SDK's explicitly registered servers retain their original `require_approval`
defaults. A reused healthy connection captures updated deadlines and approval policy for new
model steps; already discovered tools and resource bindings retain their previous policy.
`McpClient::with_binding_options` and `McpToolCatalog::with_binding_options` expose immutable
policy views over shared transport/catalog state. They do not create another cleanup owner or
change transport initialization. `McpServerManagerOptions::connect_timeout()` exposes the configured
deadline for pending-startup reconciliation.

Refresh replaces ordinary failed or closed connections and retains only compatible active
startups. Runtime disables remain disabled. A failed cleanup retains its lifecycle worker so
refresh cannot create a second connection over unconfirmed cleanup. These corrections do not
implement the remaining OAuth, EMA, executor transport or runtime context-injection gaps.

### Reviewed native-process and MCP registration API

The native `ra_exec::process` exports retain Codex's owned Child stdio handles and explicit
ProcessMode choices. MCP catalog provenance, conflict and authority records retain the source
surfaces. Their extension-safety exceptions are scoped to exact files and types, with source
and adaptation reasons in the gate policy. McpCredentialPolicy requires an explicit authority
choice; its added Default implementation was removed and the source Hash derive restored.
Its visibility is public because the catalog and product adapter live in different crates here.

The reviewed API reconciliation contains 52 additions in ra-exec, 70 in ra-mcp, and 462 additions
plus 22 definition/reexport changes in ra-protocol. It accepts the recorded account exclusions
and Rust representation adaptations, including retained protocol-mode labels; it does not claim
support for unimplemented newer MCP lifecycles. Runtime gaps remain tracked separately.
The exact accepted-item inventory has SHA-256
`c87d8c1196b13e59941bd425346b8617f7f22bf33a07e069ddc7abd10e44fb3e`.


### Public featured-plugin metadata and restored wire fields

`plugin/list` again returns `featuredPluginIds` (default `[]`). The fetch trigger retains the
source exact match for `openai-curated`. This product loads the API-key catalog named
`openai-api-curated`, so its lists return `[]` and do not request the featured service.
The ported fetch remains available for the exact-name branch and requests
`/plugins/featured?platform=codex` from the configured service base URL.
The fetch uses the application's route-aware HTTP factory, the source 10-second timeout and a
3-hour success cache keyed by the base URL. Failures log a warning and return `[]` without caching
the failure. `forceRefetch` retains the source featured cache behavior; it does not bypass this cache.
The API-key catalog name is not an alias for this trigger. The source remote-global branch is
not installed.

The source `chatgpt_base_url` key is restored for public metadata, defaulting to
`https://chatgpt.com/backend-api/`. Existing project-layer filtering still prevents repository
configuration from overriding this endpoint. No account authentication, account cookies or remote
plugin mutation services are introduced. Request originator and User-Agent use this product's name.

`PluginSummary` restores nullable `installPolicySource` and `mustShowInstallationInterstitial`;
`PluginDetail` restores nullable `scheduledTasks` and the source schedule value types. The local
converters return `None`, as upstream does. Restoring these protocol records does not install a
scheduled-task execution service or an account workspace policy service.


### Protocol record exceptions and consumer reconciliation access

The plugin/config/hook wire records in `ra_protocol::server::protocol` retain the ported
public-field and enum surfaces. The extension-safety gate now identifies upstream exceptions by
exact source file and type, for both structs and enums; a same-named type in another file does
not inherit an exception. This does not promise Rust source compatibility when upstream adds a
field or enum variant. Wire compatibility and the public API baseline remain separate reviews.

The unreleased product `ConsumerReconcile` now exposes read-only `generation()`, `unavailable()`
and `error()` accessors instead of public fields. Consumer observations keep the same values;
construction remains with the service. The test-only `SerializationAccess` enum is now
`non_exhaustive`.

Policy values are owned by `ra_config` and remain available through their existing public
`ra_protocol::server::protocol` reexports. The underlying definition crate and reflected type
names change. The API reconciliation accepts this owner move while preserving the public reexports,
variant payloads, wire defaults, aliases and custom deserialization. The shared SandboxMode
also gains the upstream configuration value's Default(ReadOnly) implementation and schema derive.
Code depending on reflected `type_name` strings must account for the new definition owner. This review does not close the
remaining product runtime capability gaps.

### Product configuration sources and composition primitives

`ra_config` owns the product source identities, metadata, source display,
`merge_toml_values`, `build_cli_overrides_layer`, shell-filter helpers and `version_for_toml`.
These APIs compose explicitly supplied values. They do not discover files, construct an effective
configuration stack, enforce requirements or configure a running host. Product loading remains
in progress; no configuration file or environment variable is newly read by these primitives.

The product configuration contract follows Codex, independently of `ra_core::config`'s generic
six-source helpers. The latter retain their API, source order, isolation defaults, naming constants
and behavior. Their documentation now describes host-selected generic sources instead of claiming
that the CLI already calls `SourceSelection::all()`. The product will not run values through a
second generic parse/merge/origin pass. The agreed product loader retains `.rusty-agent/config.toml`
for user/project files and does not automatically discover `config.local.toml` or generic `RA_*`
overrides. Dedicated home/platform source adaptation belongs to the actual loader implementation.

`ConfigLayerSource` retains distinct packaged, managed, system, base-user, selected-profile,
project, session and legacy-managed identities. Equality compares identity fields; partial order
compares precedence, so different project files can have equal precedence. Legacy managed layers
remain above session flags. The upstream `dot_codex_folder` field name is retained for API parity;
its value is the selected product project configuration directory.

Composition preserves recursive tables, replacement of ordinary arrays/scalars, canonical aliases,
sparse-overlay preservation of unrelated aliases, structured feature boolean/table compatibility,
credential-source reassignment, host-key normalization and shell-filter representation changes.
It is the ordinary config merge contract; requirements have separate field-specific semantics and
are not implemented by this helper. Session dotted-path writes preserve input order. Fingerprints
retain the exact `sha256:` encoding over recursively sorted JSON, including array order.

`ra-config` enables TOML's `preserve_order` feature as upstream does. Table iteration order is part
of the policy contract: when domain or filter keys normalize to the same key, the later declaration
wins; case-insensitive filter lookup returns the first matching declaration. Using sorted tables
instead can reverse allow/deny or include/exclude decisions even with identical merge code.

The three typed ConfigToml assertions associated with the ported merge fixtures are restored
alongside the schema and explicit product loader. Raw-value merge tests alone do not establish
loader, trust or requirements parity; those paths have separate integration coverage.

### Product MCP and path prerequisites

`ra_config` also owns the upstream MCP server input and transport
model, requirement provenance, sourced values, `Constrained` operations and MCP matching functions.
`ra_config` owns `EnvironmentMcpPolicy` and its complete family of managed MCP identities,
positional/value matchers and plugin requirements, ported from upstream `protocol/src/mcp_policy.rs`.
`ra_protocol::server::protocol` retains its existing public paths as reverse re-exports. This
ownership adapts the upstream lightweight protocol split: this repository's `ra-protocol` includes
app-server/runtime/session dependencies, which must not be pulled into config, MCP or the executor
by these value records. `ra-config` depends internally only on `ra-core` and `ra-paths`; moving
records does not change fields, serde, schema or matching behavior. The public-field records and
closed deserialization unions retain their upstream shape. These are value-level prerequisites: they do not constitute
`ConfigToml`, requirements composition, `ConfigLayerStack`, filesystem source loading,
plugin activation or runtime registration. `config::McpServerConfig` describes a file;
`mcp::McpServerConfig` continues to own an explicitly constructed SDK server registration.

MCP parsing preserves transport-specific errors, local/remote environment variable selectors,
legacy millisecond timeout fallback, positive tool budgets, unknown-field tolerance, OAuth
client secret redaction, startup readiness and credential namespace separation. Exact legacy
command identities continue to ignore arguments; matcher identities match every positional
argument and full URL value. Invalid full-match regexes remain validation errors. The validation
entry point is public in this Internal product crate so explicit callers and the external test
workspace can validate the same raw requirements without fabricating an effective stack.

The OpenAI-account `chatgpt` MCP auth variant is omitted under the repository's account exclusion.
Generic MCP OAuth, enterprise IdP profile/registration values and no-fallback EMA transport checks
are retained. Runtime-only enterprise registration cannot be deserialized from declarations and
is cleared before a failed resolution attempt. Stack provenance selection and enterprise policy
binding are still pending their actual stack consumer; calling the record-construction method is
not evidence that provenance has been checked. OpenAI user/workspace credential custody is not
ported.

`ra_paths` ports the original immutable `PathUri`, `LegacyAppPathString`, platform grammar,
byte-preserving fallback and configuration path validation. Foreign Windows and POSIX text is
preserved until resolution with the owning environment; a foreign URI is refused at native I/O
conversion. These types do not introduce remote filesystem authority. TypeScript export is omitted,
because no TypeScript export consumer is present. The utility has no internal crate dependency,
so Executor consumers need not depend on the coding product. The existing `ra_execpolicy` absolute-path/guard exports remain compatibility
re-exports of this same type. The existing native POSIX permission policy representation
is retained; the new URI module does not claim to upgrade that policy or provide Executor I/O.

The canonical native `ra_paths::AbsolutePathBuf` gains the original explicit-base/checked constructors,
join, parent/ancestor traversal, `Deref<Target = Path>`, owned `PathBuf` conversion and filesystem canonicalization. It owns the synchronous base/home
scope formerly implemented in `permissions::path`; that old public permission guard path remains a
re-export of the same guard, so configuration, manifests and permissions share one scope.
Protocol path adapters also call the shared constructors, including app-server homes and socket
paths; a synchronous home override is honored consistently. Permission adapters retain their
NUL-path rejection while delegating lexical normalization and home expansion to `ra_paths`. The
shared `expand_home_directory` function exposes the existing upstream expansion operation without
forcing relative paths absolute or consulting cwd; this visibility adaptation removes duplicate
implementations in existing consumers.
The guard remains thread-bound (`!Send`/`!Sync`), preserving the existing Rust adaptation for
thread-local state. Base guard drop clears the base as upstream does; home scopes nest and restore
on unwind. Host home fallback uses `dirs::home_dir`, matching upstream. No scope is entered by
default, and relative deserialization without a base still fails.

Both upstream symlink-preserving canonicalization functions now live in `ra_paths`. Existing
skills access uses `canonicalize_preserving_symlinks`; its nested-symlink decision helper is no
longer copied into coding. The permissive function retains logical paths when canonicalization
fails; `canonicalize_existing_preserving_symlinks` propagates those failures. Top-level system
aliases still canonicalize. The earlier claim that these helpers had no consumer was incorrect.

### Executor filesystems, direct environments and selected plugins

Configuration values moved into `ra_config`, preserving their definitions and existing
`ra_coding::config` exports. Environment services now consume the shared owner without depending
on a product crate. No effective product loader or generic source migration is implied.

`ra_filesystem` owns the source `ExecutorFileSystem` contract, read streams, URI permission
profiles, sandbox contexts, environment access keys and ancestor searches. `ra_executor_protocol`
owns the filesystem/discovery RPC records and selected capability roots. These internal records
retain upstream serde fields and defaults; TS and JSON-schema exports are omitted. The earlier
native additional-permission types in `ra_coding::permissions` remain the existing Host grant
representation; they are not the URI executor policy and are not substituted at remote ingress.

`ra_executor` supplies real configured environments, lazy connection ownership, stdio and
WebSocket transports, session resume, bounded RPC, remote filesystem access, metadata request
coalescing, read-stream cleanup, V1 discovery and its environment/sandbox-scoped cache. Successful
and permanent discovery results retain their source cache lifetime; transient failures retry.
Snapshots freeze actual discoveries in selected-root order. Missing environments and failed reads
never fall back to a Host path. `environments.toml` and `CODEX_EXEC_SERVER_URL` retain source
compatibility names. These explicit environment constructors do not install a global manager.

`ra_exec::filesystem::LocalFileSystem::with_runtime_paths` launches a native FS helper for
restricted operations. The new `ra-fs-helper` binary is the configured helper entry point.
`LocalFileSystem::unsandboxed` refuses requests requiring an unavailable helper. macOS uses the
source filesystem-only Seatbelt profile, read/write policy projection, protected metadata/daemon
paths and descriptor handoff. Linux requires a caller-supplied compatible external
`codex-linux-sandbox`; this repository does not ship that binary, and its existing bwrap entry
point does not accept the same CLI. Restricted Linux filesystem operations are unavailable without
that external helper; native launch has not been exercised on Linux. Windows native sandbox
backends remain unported and restricted operations report an error. Foreign Windows/UNC URIs
remain usable through a remote executor and are not interpreted by the orchestrator.

`ra_http` supplies the source route-aware HTTP/WebSocket factory, custom CA handling, destination
policy and system/environment proxy selection. ChatGPT cookie and account backend integration
are excluded. The two native OS proxy modules keep scoped source FFI exceptions; `ra_exec`
retains its separate native-process/descriptor exceptions. No workspace-wide unsafe allowance is
introduced. `ra_tls` owns one copy of the original crypto provider initialization, used by HTTP and
MCP adapters. Docker retains its existing ring-based initialization. Existing best-effort behavior for a preinstalled provider is unchanged.

The workspace continues using crates.io tungstenite. The pinned fork's HTTP CONNECT, SOCKS5,
SOCKS5h and environment proxy algorithms are copied into the WebSocket adapter. Fork-only proxy
URL errors become typed inner I/O errors because crates.io lacks those variants; the unsupported
scheme fallback still recognizes the typed cause. Both original MIT licenses are retained.
Tokio now requires 1.53: native owned-descriptor registration uses its explicit safety contract
instead of the deprecated `AsyncFd::new` API.

`plugins::ExecutorPluginProvider` resolves selected roots through the actual environment-owned
filesystem and retains that exact FS with each `ResolvedExecutorPlugin`. Ownership probes pipeline
up to 256 metadata calls while consuming results in ancestor/manifest priority order. Malformed
ancestor manifests still establish package ownership; discovery and metadata errors remain errors.
Resource descriptors bind every manifest/resource URI to its environment and reject root escapes.
Host and executor parsing share the original URI resolver and URI-keyed manifest cache.

`PluginMcpProvider::load` reads declared/inline/default MCP configuration through the bound FS.
`ra_mcp::plugin_config` ports the Host, executor and portable declaration parsers, including sibling
error isolation, executor cwd containment, remote environment-variable ownership and rejection of
plugin-selected enterprise authentication. `apply_configured_plugin_mcp_server_policies` retains
the selected-plugin restriction formula. Parsing and policy projection do not start MCP servers.

`executor_plugin_hook_sources` consumes discovery snapshots and the supplied enabled-tool catalog.
Only source-allowlisted inline cleanup handlers survive. File-backed hooks remain excluded;
source paths and environment attribution are retained. Browser routing requires the catalog's
connector identity and `_codex_apps.resource_uri`, then routes to the configured local MCP
endpoint with copied trusted metadata. This function admits declarations; execution still belongs
to the Host hook engine. `McpToolInfo::with_connector_id` carries trusted catalog identity with the
existing declaration, without extracting it from untrusted tool metadata.

The environment port is the filesystem-facing direct-transport subset. Process/HTTP execution,
Noise registry/provisioning and refresh, environment-config RPC, V2 discovery, shell snapshots,
and product installation/PATH metadata are not implemented here. Local metadata advertises only
implemented capabilities and reports the source unknown version. OTel metrics and remote span
parent propagation, and observation-only sandbox integrity diagnostics, are omitted; RPC wire trace
fields remain compatible. These omissions do not constitute a complete replacement of Codex's
exec-server. Full product plugin loading, Host contributions and hook dispatch remain separate
unfinished consumers; this change does not mark those tasks complete.

Internal source records retain upstream documentation and field names with file-scoped
`missing_docs` allowances. Specific lint allowances preserve original ownership signatures,
policy tables, native ABI conversions and lifecycle structure; no Clippy group is disabled.

### Host hook configuration and parsing prerequisites

`ra_coding::config` now owns the complete upstream hook declaration types, `HooksToml`,
`HookStateToml` and `ManagedHooksRequirementsToml`. The already committed
`plugins::hook_config` path remains a re-export of those same declaration types. This avoids a
second configuration implementation while preserving callers of the plugin manifest API.

`ra_coding::hooks` ports the original compiled matcher, complete command-output parser and all
output wire records, normalized-identity hashing, trust/enablement predicates and timeout
normalization. Matcher spelling remains part of identity; simple names and pipe alternatives
match exactly, regexes use upstream search semantics, and only occurrence-bearing events retain
matchers. Matcher applicability matches the core enum variants, with a fallback for its
non-exhaustive event set; future unknown event kinds do not acquire matcher behavior. Shutdown
events retain the one-second default/three-second cap, while ordinary events retain 600 seconds
without a new cap.

The parser retains unsupported-field diagnostics, decision precedence, context, legacy forms and
reserved-field behavior. Its output is product data, not an applied SDK permission decision.
Eleven original output-schema fixtures are copied unchanged and compared as JSON values; input
schema records and the upstream schema writer are not included. Upstream descriptions embedded
in those JSON Schemas are retained literally with a local doc-markdown lint allowance. Parser,
matcher and identity operations have Internal product visibility so explicit callers and the
external test workspace can exercise them without a fake effective configuration stack.

Trust hashing requires an already normalized, TOML-representable handler. It does not select a
Windows command, normalize context limits, substitute environment values, discover sources or
establish managed/built-in authority. Those remain discovery responsibilities. The original
managed/built-in precedence and ordinary saved-hash behavior are preserved as separate operations.
The upstream hash panic is reachable with valid JSON declarations: a timeout above `i64::MAX`,
or an additional-context limit above `i64::MAX` on 64-bit targets, cannot serialize as TOML.
Ordinary-event timeout normalization retains these large values. Two panic regression tests
preserve this upstream behavior; any rejection or warning during discovery must be decided and
documented separately when discovery is ported.
`ra_protocol::server::protocol::HookTrustStatus` owns the shared wire metadata; TS/JsonSchema
exports are omitted consistently with other protocol metadata and the Rust enum is non-exhaustive.

This batch does not implement the effective hook-state/config-stack consumer, file discovery,
command/MCP runners, placeholder/environment handling, async process ownership, output spilling or
UserHook dispatch adaptation. Full plugin enablement and configuration prerequisites remain open.
There is no default hook execution or implicit change to SDK lifecycle observers, existing hook
event decisions, tool permissions or sandbox behavior. The workspace's existing regex 1.13.1 is
reused (upstream locks 1.12.3); matcher fixtures verify the ported behavior without changing package
versions or the existing TOML preserve_order setting.

### Explicit plugin configuration values

`ra_config` owns plugin and marketplace configuration values in
`types.rs`, and the shared `AppToolApproval` / `McpServerToolConfig` types in
`mcp_types.rs`. `ra_coding::config` retains compatibility exports of these same types. Plugins consume these product configuration types. Values include optional
activation overrides, `_default`, per-server and per-tool policy, and marketplace origin metadata,
and can be deserialized from explicitly supplied TOML or JSON. This module does
not discover configuration files, resolve layers or requirements, enable installed plugins, or load
skills, MCP servers, apps or hooks. The product configuration loader remains a prerequisite for
those consumers; the existing framework configuration mechanism and source conventions are unchanged.

`PluginsConfigToml::default()` has no `_default` entry. An empty explicit `_default` table enables
plugins by default. `PluginConfig.enabled` remains `Option<bool>`. `allows_plugin` is a policy
predicate: without `_default` it returns true, including for entries with `enabled = false`.
The upstream loader applies `entry.enabled.unwrap_or(true) && policy.allows_plugin(id)`, where
`entry` is the configured plugin entry after layer merging and `policy` is the plugin policy value.
An entry with `enabled = false` remains disabled without `_default` because the first operand is
false. Callers must not use `allows_plugin` alone as a complete activation decision. Configuration
parsing does not validate plugin IDs.

Product `AppToolApproval` retains `auto`, `prompt`, `writes` and `approve`, independently of the
Agents SDK's MCP `require_approval` settings. Its restriction operation preserves the upstream
intersection, including `auto` with `writes` becoming `prompt`. Output budgets remain nonzero and
restriction takes the lower explicit limit without changing approval. These value operations do
not apply either of the upstream installed/selected MCP overlay paths to running servers.

The presence of `ema_auth`, including null or false, retains the unsupported-overlay marker through
serialization. A future loader must consume it as a server denial; parsing alone does not change
`enabled`. No retired authentication implementation is added. The upstream serde and JsonSchema
behavior is preserved: schema restrictions do not add `serde(deny_unknown_fields)` to these types.

### Explicit Git/npm plugin source materialization

`ra_coding::plugins::materialize::materialize_marketplace_plugin_source(&Path, &MarketplacePluginSource)`
ports the synchronous Codex source primitive. Pass a source resolved by the catalog. Local sources
return the same path without existence checks or staging; Git/npm sources return
`MaterializedMarketplacePluginSource`, whose public `path` points into an owned temporary directory.
Keep the result alive through `PluginStore::install`; dropping it removes temporary sources and
retains the original repository and any package already copied into the Store. No install,
enablement or configuration-write operation runs automatically.

Git defaults to Manual mode, retains the upstream safe-bare-repository setting, and disables
terminal prompts. Optional subdirectories use the upstream sparse checkout sequence. SHA takes
precedence over ref; after checkout, `rev-parse HEAD` must match the requested SHA case-insensitively.
Errors preserve status/stdout/stderr and temporary-directory cleanup. The internal Automatic mode
removes exactly the upstream repository-scoped variables, sets `GIT_OPTIONAL_LOCKS=0`, and holds a
trusted bare-repository guard during initial clones. Global Git configuration and transport settings
remain available. Explicit working-directory operations in Manual mode remove inherited `GIT_DIR`.
The trusted staging root is canonicalized and must remain under product home. Its two home-related
diagnostics adapt the upstream Codex branding to `product home`.

npm invokes `pack --ignore-scripts --pack-destination`, optional `--registry`, then `--` and the
package/version selector. It requires exactly one regular `.tgz`, checks its size against 50 MiB,
extracts at most 250 MiB of declared regular-file content, requires `extracted/package`, and checks
`package.json` name equality. Archive extraction retains the source's native-path checks, link and
special-entry rejection, cumulative overflow handling and errors. Extraction can leave prior files
when a later entry fails; the npm owner removes that failed temporary tree. The generic codec's
local pack/unpack implementation is retained internally without account upload or sharing APIs.

This primitive follows the upstream Git/npm transport behavior, including Git environment policy
and archive admission checks. Host installation, marketplace registration/updates, configuration
requirements and runtime refresh remain separate consumers. Completing the product's unified
network-policy task is not a prerequisite for porting plugin materialization or management. That
task's existing OAuth credential-handling and web/MCP policy responsibilities remain unchanged.
Existing upstream HTTP destination, routing and proxy constraints belong
with their HTTP consumers when those are ported. Local Git/fake-npm tests do not validate external
downloads. The source primitive gains no extra network-policy parameter or stricter catalog rules.
Windows background commands retain `CREATE_NO_WINDOW`; the Windows-only console test is ported but
has not run on this host.

### Read-only plugin marketplaces and source descriptors

`ra_coding::plugins::marketplace` adds the Codex catalog list/load/find APIs for explicitly
supplied Host roots and home discovery. Supported layouts, in precedence order, are
`.agents/plugins/marketplace.json`, `.agents/plugins/api_marketplace.json`,
`.claude-plugin/marketplace.json`, and `.cursor-plugin/marketplace.json`. Home precedes selected
roots; catalogs are deduplicated by native path, while distinct catalogs with the same name remain
distinct. A selected root can also be a supported manifest file or a nested directory in a Git
checkout. These functions read local catalogs and manifests without creating a Store or starting
Git, npm, HTTP requests, installation, authentication, or configuration writes.

Local sources resolve against the marketplace root, with the upstream `./` requirement and
Cursor-layout exception. Git/npm sources remain descriptors. Remote fallback manifests expose
metadata only: resource paths and interface assets are removed from parsed listing metadata,
while the generated JSON remains available to a future installer. A selected real local manifest
does not fall back to catalog metadata when parsing fails. Invalid source entries are skipped;
malformed catalog JSON or typed policy fields still fail the catalog. `find_marketplace_plugin`
selects the first resolvable duplicate. `find_installable_marketplace_plugin` then applies
installation and product restrictions without installing anything: missing products are unrestricted,
an explicit empty list rejects selection, and a nonempty list requires a matching explicit product.

The Local/Git marketplace-origin parser and staging admission helpers remain internal, as upstream;
registration and materialization consumers are not included yet. Origin parsing retains its own
reference precedence, local canonicalization, tilde expansion and URL rules; these intentionally
differ from plugin source descriptors. Standalone catalog functions use a disabled manifest cache;
the internal owner-aware list path accepts the existing Store-owned cache for later manager use.
`MarketplaceAddError` retains the upstream public declaration for future public registration
operations and is also returned by the feature-gated test bridge. Its presence does not add a
registration operation to the default build.

`ra_protocol::server::protocol::{PluginInstallPolicy, PluginAuthPolicy, Product}` now carries the
upstream protocol label sets, including their serde spellings and aliases.
Authentication policy is timing metadata only. No account backend or protocol management handlers
are introduced. `ra_coding::skills::SkillProduct` reexports this shared `Product`, preserving its
name, variants, `non_exhaustive` contract and deserialization behavior; serialization, hashing and
upstream Product helpers become available. All three protocol enums use `#[non_exhaustive]` as a
Rust API-evolution adaptation, consistent with other ported protocol labels: downstream matches
must handle future variants. Serde still rejects unknown labels; this attribute adds no wire
fallback. Existing skill filtering and its explicit source selection remain unchanged, and the
extension-safety gate needs no exception. The upstream `JsonSchema` and TypeScript derives are
omitted consistently with other protocol types until a protocol-schema export consumer is ported.

### Explicit local plugin Store and command migration

`ra_coding::plugins::store::PluginStore` adds local installation and removal. Construct it with
`try_new(product_home: PathBuf)` to receive a `PluginStoreError`, or use the upstream-compatible
`new` constructor, which panics for an invalid home. Construction resolves paths without creating
directories. Its home-resolution error context says `failed to resolve product home`, adapting
the upstream diagnostic's Codex branding.
`install(source: AbsolutePathBuf, id: PluginId)` selects a version from the source;
`install_with_version` accepts an explicit cache version. Both stage the local directory, skip
symlink entries, check the selected manifest's path and bytes, and activate under
`plugins/cache/<marketplace>/<name>/<version>`.

Legacy packages without a version use `local`. Agent Plugins use `1.0.0` when their version is
absent or blank; a portable version string with invalid directory characters maps to `agent-plugins-<24 hex characters>`
without changing manifest metadata. Same-version replacement backs up the previous package and
attempts restoration if activation fails. Failed restoration preserves the backup and reports its
path. New-version activation precedes pruning; an error can leave the new version on disk while
an undeletable older version remains active. This is the upstream filesystem lifecycle.

Legacy command files migrate only in staging to `.codex-plugin/migrated-command-skills`.
The source directory stays unchanged. Plugin migration requires a description, rejects unsupported
runtime template expansion and name collisions, and skips generated skills exceeding 4,000 bytes.
Migration errors emit warnings and do not fail installation. Agent Plugins skip command migration.
The generic `command_migration` profile APIs retain their separate explicit import/description
options; selecting one does not enable startup discovery or change plugin migration defaults.
Their explicit term variants currently rewrite to the literal `Codex`, as in the upstream
`external-agent-migration` consumer. The plugin installation profile supplies no term variants.
That external-agent migration feature is outside the current plugin slice; its product branding
remains undecided until its consumer is ported.

`uninstall` removes the package's version cache and retains persistent `plugins/data` contents.
It does not write enablement configuration or refresh runtime consumers. Store clones share the
source's 128-entry manifest cache; independent stores remain isolated. Revisions include content,
overlay and resource-root identity, with inputs above 64 KiB parsed outside the cache lock.
Standalone manifest APIs continue to use an uncached owner. Product configuration, marketplace
management, downloads, activation into skills/MCP/hooks, and account identity remain outside these
new local operations.

### Read-only Host plugin metadata

`ra_coding::plugins` adds explicit Host package inspection. `PluginId` preserves the upstream
plugin/marketplace key validation, and `manifest::load_plugin_manifest(&Path)` returns an optional
manifest with native `AbsolutePathBuf` resources. Pass an absolute package root; relative roots
are rejected. The parser selects schema-declared Agent Plugins `plugin.json` before the legacy
`.codex-plugin`, `.claude-plugin`, and `.cursor-plugin` locations. An invalid selected manifest
does not fall through to a lower-priority manifest. Agent Plugins retain fixed skill/MCP resources
and the upstream inline `com.openai` extension/legacy overlay precedence.

The `model` module retains generic resource mapping, inline MCP objects and typed hook declarations.
`installed::active_plugin_version` only inspects an explicitly supplied cache directory: `local`
wins, otherwise versions use semantic ordering when both parse and lexical ordering otherwise.
These APIs do not install packages, read product configuration, discover plugins at startup, activate
skills/MCP servers, execute hooks, or provide Executor URI authority. Existing host assembly and
configuration-source behavior are unchanged. This additive surface belongs to the Internal
product crate, rather than the provider-neutral framework contract.

Home-relative Host roots use the current thread's `permissions::AbsolutePathBufGuard` home override;
setting its deserialization base still does not make a relative package root acceptable. Component
paths must retain the Host convention after joining, including at the filesystem root. Codex's
account-backed remote-plugin identity sidecar types and parser are excluded.

### Inherited role tools and explicit restrictions

`ReadOnlySpecialist` and `Planner` now inherit the same tools as `Main` for the selected host and
profile. Shell, patch, web, and collaboration tools follow the ordinary host/profile selection.
Roles do not replace the inherited sandbox or approval policy. `OneOffAnswer` remains tool-free.

Hosts can explicitly select `CodingHost::with_read_only_role_restrictions(true)`. This product
extension, disabled by default, withholds `apply_patch` and the entire collaboration tool group
from specialist/planner roles, including messaging, follow-up tasks, interruption, waiting and
listing agents. It retains shell and web. It removes dedicated tool
entries; shell commands remain subject to the inherited sandbox and role instructions, so this
setting is not a filesystem read-only sandbox. Tool counts, profile floors and prompt fragments
are narrowed by exactly the withheld entries. Role guidance uses the advertised tool inventory.

`Planner` ports three parts of Codex Plan mode: instructions, `update_plan` refusal, and
`request_user_input` behavior. Repository mutations are prohibited by its instructions, builds
and tests may write caches/artifacts, `update_plan` remains declared but returns Codex's Plan-mode
refusal, and root-thread questions carry `isBlocking=true` and await the response. Child-thread
questions retain the root-only refusal. Spawned Planner guidance adapts questions to the parent
agent and returns the plan and unresolved questions to that parent. The embedded upstream template
is retained; emitted guidance adapts unavailable tools, the root/child audience, and plain-text
`<proposed_plan>` markup. It does not promise special client rendering.
Required adaptation text and section boundaries are checked during prompt assembly. Missing or
ambiguous markers, reversed boundaries, or unadapted references to unavailable control tools return
a configuration error instead of silently emitting incomplete guidance. The current template's
rendered text is unchanged by these checks.

The Plan runtime's proposed-plan stream parser and separate plan items, deferred mailbox delivery,
and restrictions on automatic turns entering/leaving Plan mode are not ported. Plans remain plain
assistant text. These are recorded gaps, not a claim of complete Plan-mode parity.

ReadOnlySpecialist reuses the upstream Plan template's allowed/non-allowed operation lists,
including cache-writing builds/tests and prohibitions on formatters or code generation that change
repository files. Its role allowance is 640 estimated tokens, with a 3,360-token prefix ceiling.
Planner's allowance remains 3,072 and its ceiling 5,792; other roles retain 192 and 2,912.
Prompt hashes and the assembly baselines change; tool schemas and
`TOOL_SCHEMA_REVISION` do not.

Mode selection is bound at agent construction, rather than through Codex's mutable turn settings.
Reassemble with another role to change it. Standalone framework tool users can select
`with_mode(ModeKind::Plan)` on `UpdatePlanTool`, `TodoCapability`, or `RequestUserInputTool`;
constructors preserve Default mode. Existing `AgentControl::with_max_depth` still limits spawning
and hides collaboration tools at the limit. Without that opt-in the pinned Codex v2 path, and
Rusty's existing control, impose no depth limit; this change does not add a new depth default.

### Explicit Host skill discovery

Host disk discovery is disabled by default. Enable it with
`CodingHost::with_skill_discovery(SkillDiscoveryConfig::new())`, using the types in
`ra_coding::skills`. This discovers the workspace's `.rusty-agent/skills/` and any explicitly
selected `PluginSkillRoot` directories. Each plugin supplies its owning root and namespace;
recursive discovery is the default, with `DirectChildren` available for Agent Plugin layouts.

`with_config_rules(SkillConfigRules { entries })` applies Codex's ordered enablement rules after
product filtering, plugin qualification, and canonical-path deduplication. Each `SkillConfigRule`
contains a `SkillConfigRuleSelector::Name(full_name)` or `::Path(canonical_document_path)` and
an `enabled` boolean. Every matching rule is applied in order; later rules override earlier ones,
including across name and path selectors. Name selectors affect every matching document. Disabled
entries remain in the catalog with `enabled = false`, and are omitted from the model's catalog.
Reenabling a skill preserves `policy.allow_implicit_invocation = false` and product restrictions.

The default rules are empty. Build rules from configuration entries with
`SkillConfigRules::from_configs([SkillConfig { path, name, enabled }, ...])`, passing entries in
increasing precedence order. As Codex's `[[skills.config]]` loader does, it canonicalizes existing
paths, trims names, ignores entries with an empty name or with both or neither selectors, and lets
a repeated selector replace its earlier rule. `SkillConfig` implements serde for host configuration
files; this API does not read Codex's user/session TOML layers. Relative paths are ignored with a
warning, because there is no configuration base directory to resolve them against. Directly
constructed `SkillConfigRules` are used as given: names match exactly, and path selectors are
native `PathBuf` values in place of Codex's `PathUri`.
Rules only affect discovered Host entries; Executor and Cloud providers keep their own enablement.

`with_allowlist(["release", "acme:review"])` is an explicit product extension applied after these
rules. It selects complete, case-sensitive names after plugin qualification. No allowlist retains
all discovered Host entries, including disabled ones; an empty allowlist selects none. Allowlisting
a disabled skill retains `enabled = false`; enabling a skill cannot bypass the allowlist.
**The allowlist is a context filter, not a sandbox.** Excluded skill files remain on disk and
can still be read by `read_file` or shell commands under their existing permissions. Discovery
adds no filesystem grants and does not authorize plugin scripts, MCP servers, or hooks. External
plugin files still require an ordinary file/command tool with appropriate access.

Only metadata enters the run catalog. Host entries use file locators and ordinary file tools;
Executor and Cloud entries keep `skills.list` / `skills.read`. Allocation follows Codex's
`render_catalogs` entry point: Cloud is rendered first with a cap of `total - total / 4`, then
Executor and Host share the budget left after Cloud's actual metadata cost. If filesystem entries
are omitted and a joint probe can retain every source's entries, the Cloud cap is lowered and
both buckets are rendered again. Description truncation alone does not trigger rebalancing.
Cloud preference for matching qualified names applies only to Executor; matching Host entries
remain in the Host catalog. Source ordering, aliases and Host full descriptions are preserved.
Core surfaces retain Host catalogs while omitting the namespace tools.

Each run uses the upstream first-step allocation with no previous world-state snapshot. Rusty
does not retain the Cloud cap or its catalog fingerprint across steps/resume. Host and Executor
inventories refresh on each new run and remain fixed within that run; this is the existing Rust
run-based adaptation of Codex's step-based lifecycle.

`agents/openai.yaml` policy preserves `allow_implicit_invocation` and `products` restrictions.
Choose a restricted product with `with_restriction_product(SkillProduct::Codex)` when needed;
without a product, product-restricted entries are excluded. Invalid optional metadata is ignored.
Presentation metadata and dependency declarations are validated for parsing but do not install
UI assets or dependencies. The legacy merged `with_skill_catalog` extension remains exclusive
with Host discovery and available namespace providers.

`SkillListQuery::executor_roots` now supplies selected authority ids and canonical local paths to
providers. Unselected executor authorities are removed before prompting, listing, or package
resolution, so a direct read now reports `skill package is not available` for them. The default
tool schema, default static prompt, and schema revision are unchanged.


### Per-turn coding search fallback

`CodingHost::with_web_access` preserves the released local-function behavior on every protocol.
`with_web_access_fallback(access)` instead keeps native Responses search and selects local
`web_search`/`web_fetch` on Chat and Anthropic turns. It preserves existing `with_web_search`
settings; calling `with_web_search` afterward updates settings without removing the fallback.
Selecting legacy `with_web_access` removes both native configuration and the fallback. Selecting
fallback afterward restores default native settings if needed.

Cached remains the default. A plain `WebAccess` fallback can satisfy only Live mode without
additional search options. It does not automatically turn Cached into Live; the existing
permission-profile mode resolution still applies. Use `with_web_access_fallback_adapter` and
`WebSearchFallbackAdapter` for a backend that explicitly supports Cached, Indexed, domain filters,
location or context settings. The adapter receives `WebSearchPolicy` and returns an implementation
bound to the complete policy. Cached does not expose `web_fetch` or recommend it in result guidance.
`WebSearchTool::with_fetch_guidance(false)` supports search-only result guidance while preserving
the default web pair. Fetch domain restrictions are a
Rusty product policy, including adapter enforcement during redirects. Unsupported policies omit
the local candidates while retaining available native Responses search.

`CodingWebSearch::enabled` is the common capability gate (default true); `supported` continues to
mean hosted endpoint support. `mode_for_turn` now returns only the effective permission-resolved
mode, independent of either support gate. `prefer_standalone` explicitly selects an available
fallback even on Responses turns. An unrelated same-name dynamic/MCP function does not suppress
hosted search. Hosted history replay into Chat remains unsupported.

`ToolAvailability::DynamicForProtocol` and `Tool::is_enabled_for_protocol` are opt-in extensions.
Ordinary callbacks retain their existing order before model resolution. The new callback runs
once after model resolution, before final collision handling, discovery caching and budgeting.
Agents without protocol-dependent candidates retain the original early collision/discovery/budget
error order. For an agent with any such candidate, the entire frozen inventory is finalized after
model resolution, including errors involving only ordinary tools. A model-resolution failure can
therefore precede an ordinary-tool collision or budget error in that explicitly selected path.
Approval recovery resolves an already-issued local candidate by its original lookup key and
recipient binding without reselecting its protocol shape. Protocol-dependent candidates do not
participate in the reconstructed discovery surface, so they cannot shadow an approved MCP tool.
The coding fallback candidates carry the adapter-owned binding
`{"coding_web_fallback":"web_search"}` or `{"coding_web_fallback":"web_fetch"}`. This distinguishes
their approvals from same-name resolver functions that have no recipient binding. The marker stays
in approval checkpoints; it does not change model tool definitions or replay input.
Fallback recovery also checks the current common capability gate, effective mode and domain scope
before binding the backend. If current access constraints no longer permit the issued policy, the
call fails closed. Otherwise it retains the issued policy, including its location/context guidance;
loosening current settings does not widen an approved call. This is a coding-product policy.
`HostedToolResolver::resolve_with_tools` defaults to the existing
resolver and optionally exposes the actual enabled local handlers to product adapters.

Protocol-selected search candidates are omitted from the stable inventory. Tool Use now permits
the tools actually declared in the request, changing the stable prefix hash. Schema revision is 11.

### Native Responses web search declarations

Use `ra_model::openai::responses::web_search::WebSearchTool::to_definition()` with
`AgentSpecBuilder::hosted_tool` to declare native search. The framework constructor preserves
`search_context_size="medium"`, optional filters and location, explicit `external_web_access=false`,
content kinds and image settings. Image search automatically includes `web_search_call.results`.
Hosted declarations participate in the real request tool table, budgets and tool-choice
reconciliation. A hosted-only agent retains `auto`/`required` before its first request;
`ToolChoice::Tool("web_search".into())` produces the native built-in selector, as in the SDK.
That selector is reserved even when an ordinary function has the same name.

`AgentSpec::tools()` remains the local-handler projection for compatibility;
`declared_tools()` exposes the ordered local/hosted union. `clear_tools()` clears both kinds,
while `clear_hosted_tools()` removes only hosted declarations. Runtime handler replacement
preserves interleaving. `ToolSurface::tools()`/`into_tools()` likewise project local handlers;
use `hosted_tools()` and `model_definitions()` when inspecting the complete advertised table.

Coding hosts now assemble native Responses search by default. `with_web_search(CodingWebSearch)`
selects disabled, cached (default), live or indexed mode, configured context/domains/location and
explicit model/endpoint capabilities. Codex's per-turn permission-profile resolution is preserved:
unrestricted execution prefers permitted live search, and endpoints without external access fall
back to cached search or disable it. This does not add network authorization policy.
`with_web_access` explicitly selects the existing local `web_search`/`web_fetch` extension.
Native assembly does not require that backend or advertise a separate `web_fetch` function.
Each turn resolves native search against its selected protocol: coding hosts omit it for Chat
and Anthropic. Explicit framework Responses declarations still fail on those adapters.
Use the WebAccess extension to select local web functions for those protocols.
The coding prompt's stable "Available tools" inventory lists only local entries, because whether
a turn declares hosted search depends on its protocol. Its Tool Use rule permits tools actually declared in the request, which changes the stable prefix hash.

These changes are implemented and awaiting review. No release version has been selected.

### Hosted tool records and tool-choice resets

Responses now retains file search, web search, image generation, code interpreter, and
server-executed tool search items in history. Replays preserve their provider payloads except
for output-only `created_by`, which the upstream input projection removes. Unknown output
types warn and are skipped; known unsupported actions, approval requests, and program items
still fail. Input-only computer, MCP approval, local shell, and apply-patch output items also
warn and are skipped when returned as model output, matching the framework contract.
Provider-executed tool activity without a message now requests another model turn, including
a response containing only `mcp_call` (previously it ended with empty final output).

`AgentSpecBuilder::reset_tool_choice(bool)` defaults to `true`, matching the framework contract.
Reset policy belongs to the public agent and uses its cumulative run usage, including hosted
calls but excluding server tool search records. Set it to `false` to retain an explicit choice.
A reset clears every resolved choice to `None`, including `auto` and `none`; layers are unchanged.
Tool-surface reconciliation preserves declared hosted entries as well as local tools. With no
formal declarations, the existing dynamic-availability downgrade remains unchanged across
providers. Hosted tools supplied only through legacy `extra_body` remain invisible to this
neutral inventory; use the formal WebSearch declaration to avoid losing `auto`/`required`.

The generic context estimator still counts image-generation base64 results as text. Large image
results can therefore inflate token estimates; porting the adapter's fixed image estimate remains
follow-up work.

These changes are implemented and awaiting review; no release version has been selected.

## 0.3.0

This section lists the incompatible changes from 0.2.x.

### Model settings resolve by named layer

Model settings now resolve in provider → model → agent → run order. For scalar settings, the last
layer that sets a value wins. Model registration defaults fill fields the agent and run leave
unset; this is a deliberate registry extension and differs from the OpenAI Agents SDK, which
discards all implicit model-specific defaults when the agent has any explicit model setting.
`retry` continues to merge across layers.

`metadata`, `extra_headers`, and `extra_query` replace the whole map. Use
`with_metadata_map`, `with_extra_headers_map`, or `with_extra_query_map` with an empty map to clear
an inherited value. Provider-bucketed `extra_body` uses a local shallow merge of top-level keys:
later layers replace the complete value for a matching key, including nested objects. This is not
the SDK's `extra_body` behavior (which replaces the map); the SDK's `extra_args` uses a shallow
top-level merge, but it is a separate API.

`timeout` follows the same later-layer-wins rule; the run deadline remains controlled by
`BudgetLimit`. `max_tokens` in `ModelRegistration::with_defaults` is only a default. Declare a hard
model limit with `ModelRegistration::with_max_output_tokens`; requests above it now fail with a
configuration error before calling the model instead of being silently truncated. Provider
requirements such as Anthropic's fallback `max_tokens` remain in provider defaults.

Custom `ModelResolver` implementations that know a model's hard output limit must attach it to the
resolved model. After constructing `ResolvedModel`, call `with_output_token_limit(limit)`:

```rust,ignore
ResolvedModel::new(selector, model, provider_defaults, model_defaults)
    .with_output_token_limit(4_096)
```

Without this declaration, the runtime cannot enforce that model's hard limit.

`ModelSettings::resolve` has been removed because its four same-typed positional layers were easy
to pass in the wrong order. Build the layers by name; any layer you omit is treated as empty:

```rust,ignore
SettingsLayers::new()
    .with_provider(&provider_defaults)
    .with_model(&model_defaults)
    .with_agent(&agent_settings)
    .with_run(&run_overrides)
    .resolve(&provider_key)
```

The `ModelSettings` serialized schema is now version 2 so an explicitly empty map remains distinct
from an unset map. When reading schema version 1 settings, empty `metadata`, `extra_headers`, and
`extra_query` maps are treated as unset to preserve the old format's meaning, and the in-memory
settings are upgraded to version 2 for subsequent writes. Schema version 2 empty maps continue to
clear inherited values. Resolved trace settings retain their own schema version because their
shape did not change.

### Run limits follow the framework defaults

`ra_runtime::runner::DEFAULT_MAX_TURNS` is now 10 (previously 32). An exhausted turn
limit returns `Error::Budget { kind: BudgetKind::MaxTurns, .. }` before the next model
request. The existing `Error::Run` checkpoint wrapper retains that classification and exposes
the generated history through `error.run_state()`, adapting the SDK error's `run_data`.
A final answer on the last allowed turn still succeeds. Install a `RunErrorHandler`
to produce a final delivery on exhaustion; returning `None` from the handler preserves the error.
`AgentTool` defaults to 10 nested turns and applies its failure policy to exhaustion.

`RunConfig::with_max_turns` accepts either a number or `None` for unlimited turns.
`AgentToolBuilder::max_turns` accepts a number; `None` selects the default 10 turns,
matching the SDK's `as_tool(max_turns=None)`. Omitting the call also keeps 10.
Both builders accept zero, which exhausts before any model request, and are no longer
`const fn`. `RunConfig::max_turns()` now returns `Option<u32>`; callers must handle `None`.

`RunConfig::with_budget` keeps the configured turn limit when the supplied `BudgetLimit` has
none, so setting only a token ceiling no longer drops the turn limit. A turn limit in the
supplied budget still replaces the configured one; use `with_max_turns(None)` for unlimited
turns.

Function-tool concurrency is unlimited by default. `with_max_function_tool_concurrency`
accepts a positive number or `None`; its getter now returns `Option<usize>`, and its builder
is no longer `const fn`. This changes the global dispatch cap only: bare `ToolOptions`
remain `Exclusive`, while macro-generated function tools remain `Parallel`.

`CodingHost::build_run_config()` explicitly selects unlimited turns, following Codex's coding
loop, and keeps function-tool concurrency at 8. To retain the previous product turn ceiling,
apply `with_max_turns(32)` to that config. Other hosts keep the framework defaults unless they
select their own limits.

RunState schema 14 saves the turn limit, including explicitly unlimited runs. A resumed run
uses its saved limit and spent turns even if the new request supplies a different limit.
Tokens and deadlines remain configuration supplied on each continuation.

Checkpoints written before schema 14 carry no turn limit, so they resume under the limit the
request supplies, which is then saved. With the new default of 10, an older paused run that
has already used 10 or more turns fails with the turn-limit error as soon as it resumes. To
resume such a run under its original default, configure `with_max_turns(32)` for that
continuation. In coding products, a checkpoint that predates schema 14 instead adopts the
host's new unlimited policy. Checkpoints that already save a finite limit retain it on resume;
the new host policy does not rewrite an in-progress run's saved limit.

### Coding permission modes move out of core

Replace `ra_core::permission::PermissionMode` with `ra_coding::CodingPermissionMode` in
coding products. Its labels, Display output and camelCase serde spellings are unchanged.
Replace `with_permission_mode(mode)` with `with_permission_policy(mode.policy())`.
Provider-neutral hosts can omit the policy entirely to retain normal SDK approval behavior.

`PermissionScope` and `ToolOptions::with_permission_scope` are replaced by the open
`ActionClass` identifier and `ToolOptions::with_action_classes([class, ...])`. The plural
builder sorts and deduplicates classes. An empty list is unclassified; it does not inherit
Execute. ToolOptions schema 2 reads legacy schema 1 `permission_scope`, including the old
implicit Execute default. Built-in adapter constants are in `ra_tools::action_classes`:
`READ`, `EDIT`, and `EXECUTE`. Generic adapters below ra-tools declare their local effects
without depending upward on the coding product's vocabulary module.

`ActionClassPolicy` is an optional local runtime extension, separate from the framework
approval contract. `allow_only` checks every declared class before rules; matching rules
precede `auto_allow` and `auto_allow_all`. Restricted and class-based automatic approvals
require a nonempty class list. Disabling prompts changes only `prompts`: it preserves the
restriction, automatic approvals and rules. Coding settings apply this overlay only for
`AskForApproval::Never`; other policies preserve the base engine, including an absent policy
and DontAsk. Framework automatic approval does not bypass tool-internal approval or sandbox checks.

The unused callback types `ToolApprovalHandler`, `ToolApprovalDecision`, `ToolApprovalAllow`,
`ToolApprovalDeny`, `ToolPermissionContext` and `PermissionUpdate*` have been removed without replacements. Use the existing tool approval
contract and pending interruption APIs.

### Sticky decisions belong to their agent

An `always` answer now binds the exact tool identity and its owning agent, across handoffs and
JSON checkpoints. `matches_origin_for_agent` evaluates these rules. Name-only `matches` and
`matches_origin` return false for agent-owned rules, because their inputs cannot prove ownership.
Configured rules without an owner retain their wildcard behavior.

Rust uses the registry-unique `AgentId` as durable ownership rather than Python object identity.
Hosts must preserve those IDs when rebuilding an agent registry; assigning an existing ID to a
new capability intentionally reuses that logical identity. This is the necessary adaptation for
an owned, serializable Rust agent graph. Sticky answers without agent provenance return a caller
error before recording any answer; the host may still answer that call once.

RunState schema 15 records the new rule field. Reading schemas through 14 drops ownerless
session rules, so future calls require fresh approval. Exact pending-call answers remain intact.
The migration runs only for old schemas. A current-schema rule is not silently discarded.

### Callback-local executor skill read permissions

Coding skill reads now construct an accessor inside each callback using that callback's
`RunContext`: its baseline comes from the model step's captured `CodingThreadSettings`, while
session and originating-turn `request_permissions` grants are read live. Because permission
requests serialize tool execution, later `skills.read` calls in the same response see an approved
grant. A baseline update published during a step takes effect on the next step; outstanding calls
retain their issuing step's baseline. Turn grants expire with the turn. No ContextProcessor or
per-step refresh hook is involved, including in caller-managed continuations.

The earlier development version incorrectly froze grants at model-step preparation and reused
bind-time baseline settings. Its same-step denial behavior has been removed.

Legacy sandbox policies keep their existing read semantics within explicitly selected skill
roots. Configure an optional skill-read filter with
`CodingThreadSettingsAdapter::with_file_system_read_policy(FileSystemPermissions)`. This builder
now returns `Result<Self>` and validates entries immediately; an explicitly empty filter denies
all live skill reads. For a named profile, register the legacy policy with `with_profile`, then
attach the skill-read entries with `with_file_system_read_profile`; both builders validate before
installation. A named selection selects both components; a legacy sandbox selection clears the
skill-read component. Preloaded discovery contents still do not reopen the main prompt. Host and
Cloud keep their existing source lifecycles.

**The extra skill-read filter is a product extension, not a filesystem security boundary.** It
applies only to executor skill reads and live skill injection. Other tools, including `read_file`
and shell commands, retain their own policies and can read a file excluded by this filter.

`SkillReadContext` has one executor variant, `Executor { access }`. Providers must read through
`access.read_text`; the pre-release raw `Workspace` variant and `ExecutorWithPermissions` variant
have been removed. Standalone callers can create an accessor with
`SkillExecutorAccess::new(workspace, None)`, explicitly retaining their selected-root contract.
`SkillExecutorAccessPolicy::capture` now receives the current callback's `RunContext` and returns
optional immutable `SkillFilePermissions`. `same_permissions` compares full captured values;
there is no string cache key or JSON encoding of native cwd, home, or temporary-directory paths.

Pagination keeps a weak filesystem identity and the captured permission object, compared by value.
File edits alone retain the cached snapshot. Changed access rereads and revalidates the cursor;
a denied read returns `failed to read skill resource`, while changed contents after an allowed
cache miss produce the existing stale-cursor error.

Local descriptor checks reject a deny matching either the requested path or the opened target.
Policy roots and literal glob prefixes use Codex's `canonicalize_preserving_symlinks` and
`should_preserve_logical_path` rules. Paths containing nested symlinks retain their complete
logical spelling; an allow or deny on such a link does not expand to its canonical target.
Without a nested symlink, top-level aliases such as macOS `/tmp` and `/var` are resolved.
Missing descendants are normalized through the first existing ancestor using the same rule.
The resource is opened once through its selected directory capability and read from that same
descriptor. The earlier development version's full symlink resolution and nested-alias test
expectations have been removed.
This adapter does not provide Codex's separate kernel-sandboxed filesystem worker. Restricted
reads use descriptor paths on macOS/Linux; other platforms return Unsupported. Linux and complete
platform `Minimal` defaults are not newly verified by these changes. The local descriptor adapter
and the skill-only filter remain conditional review recommendations awaiting user confirmation;
this record does not treat the reviewer recommendation as user acceptance.

The existing `Capability::contribute_turn_input` callback now takes
`(&self, context: &RunContext, input: &[ModelInputItem])` so live explicit skill injection receives
the same step settings as tool callbacks. Existing implementations must accept the new parameter;
the default still returns no messages. No permission types enter the provider-neutral core, and
tool schemas and persisted run-state formats do not change.

### Contextual input contributions and skill mentions

`Capability::contribute_turn_input` is an optional runtime extension for contextual messages;
SDK dynamic/system instructions keep their existing behavior. The runner supplies only newly
accepted input, excluding session history and input dropped by a session callback. Blocking input
guardrails run first. Returned messages enter authoritative history atomically after all installed
contributors succeed; they reach Session persistence and rollout recording as ordinary items.

RunState schema 16 adds `pending_input_contributions`, defaulting to an empty list for old states.
It retains accepted input across a failed or cancelled contribution and is cleared after commit.
Errors at this new preparation boundary carry a checkpoint. Older runtimes preserve the field as
unknown data but cannot finish the pending contribution before inference. Existing model-error
checkpoint rules are unchanged.

Explicitly configured Codex skill catalogs now load `$name` or `[$name](path)` selections before
inference. Extension main prompts are bounded to 8,000 UTF-8 bytes, names to 256 and locators to
1,024; full resource metadata and `skills.read` contents remain intact. Core Host skills retain
complete documents and locators, while Agent Plugin documents use the 8,000-byte bound. Hidden
skills remain explicitly invocable; disabled skills do not. The merged `skill` compatibility
extension and sandbox `load_skill` retain their separate contracts.

Selected `<skill>` user-role fragments remain in model history but are hidden from client thread
items and excluded from stored/user-fork turn boundaries. Marker recognition also handles older
records without annotations. Core Host text links expand `~` and require absolute native paths;
`file://` and relative text links are rejected. Only structured Skill inputs resolve relative paths
against the process cwd.

`SkillCatalogPromptConfig::with_required_skills(environment_id, names)` opts into source-scoped
prerequisites. It checks enabled discovery entries bound to that environment before each model
call, without reading their bodies. Core surfaces retain these checks even when namespace tools
are withheld. Reconfigure the selected requirements on the next run or checkpoint resume to
remove an environment requirement; no environment manager or OpenAI account backend is added.

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

The `ApplyPatchResult::output` constructor deprecated in 0.2.2 is removed. The `output_text`
reader is renamed `output`, matching the field name in the OpenAI Agents SDK. The optional
status and output values, their defaults, and the `with_output` / `with_status` builders are
unchanged.

| 0.2.x | 0.3.0 |
| --- | --- |
| `ApplyPatchResult::output(text)` | `ApplyPatchResult::default().with_output(text)` |
| `result.output_text()` | `result.output()` |

Use `with_output` to construct a result; it is also available in 0.2.2.

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

### Executor plugin delivery boundary

Explicit selected roots flow through `ExecutorCapabilityDiscoveryCache`,
`ExecutorPluginProvider`, the bound filesystem and `PluginMcpProvider`. Resource descriptors retain
the environment that owns their URI. Declaration loading and policy overlays return values;
`executor_plugin_hook_sources` returns only admitted inline cleanup sources from a supplied step
snapshot and enabled-tool lookup. These entry points do not constitute an effective config loader,
a global MCP registry or hook execution. Their callers own activation, registration, dispatch and
refresh through the product's corresponding consumers. A missing executor resource remains a
failure even when an identically named Host file exists.

### Executor review corrections

The environment-info RPC timeout is 30 seconds and the environment-status RPC timeout is
10 seconds, matching the source defaults. The session-recovery regression uses the production
environment-info timeout.

Linux filesystem-helper preparation now uses the source WSL1/bubblewrap support check,
`linux_sandbox_arg0_override` and permission-profile argument lowering, specialized to the source
filesystem-helper call with no managed network. The launcher preserves the helper working root,
permission-policy cwd and sandbox override separately. MXC selection is rejected on non-Windows
hosts. The repository does not build or bundle `codex-linux-sandbox`; restricted Linux filesystem
operations remain unavailable unless the caller supplies a compatible external executable in
`codex_linux_sandbox_exe`. The existing bwrap entry point does not implement that CLI. Linux and
Windows `ra-exec` compilation is checked separately from native sandbox execution. Windows native
restricted-token/elevated/MXC backends remain unported and return an explicit error.

Rendezvous URLs selected by the source `role=harness` query rule are rejected before environment
registration or dialing; relay framing is not implemented. Direct JSON-RPC is not used as a fallback.
The hidden `test-api` facades expose the production algorithms to the external tests workspace.
Enabling `test-api`, including through feature unification or `--all-features`, does not change
ordinary client constructors: recovery remains 25 seconds. A feature-gated lazy-client constructor
accepts a recovery duration for source fixtures, and that value is owned by the resulting client.
WebSocket fixtures pass their 25ms heartbeat through the existing stream constructor parameter.
The unused axum server constructor, its message adapter, server-only heartbeat constant and direct
axum dependency are deferred with the server; ordinary client WebSockets retain the source's lack
of periodic server-side pings. Environment snapshot records have one definition each, with
feature-gated fixture construction/access instead of conditional copies. Passive selected-root
inspection is ported without starting an environment. Process execution, Noise registry recovery/backoff, server-side
file-handle registry and OTel propagation tests remain deferred with their implementations; the
implementation record lists every omitted source case rather than treating them as covered.


The native spawn ports also pass target-specific Clippy without changing the source error or
ownership contract. Linux uses explicit raw pointers and checked bounded conversions; Windows
retains native status/coordinate bit patterns, source command ownership and escaped diagnostics.
Allowances for retained, uncalled source APIs are attached to the particular field, type,
implementation or function; the port no longer suppresses dead code for entire modules. The
unused filesystem-helper `seatbelt_network_policy.sbpl` is removed: the fixed restricted helper
continues to use the same empty dynamic network-policy fragment as upstream's no-proxy call.
