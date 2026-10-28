//! Syntax checks for extension safety with file-and-type scoped upstream exceptions.

use syn::{Item, Visibility};

/// Public enums allowed to stay exhaustive.
///
/// `NextStep` is deliberate: it is the single point where control flow converges, the framework
/// `match`es it itself, and adding a state **must** be a compile error. An outward-facing data
/// enum and an inward-facing control-flow enum are constraints in opposite directions, not a
/// contradiction.
///
/// `ResolvedInstructions` is the same shape of decision. Its variants distinguish static and
/// generated system instructions from the explicitly selected volatile tail extension. Turn
/// preparation `match`es it to decide the placement. A `_` arm could silently misplace a new
/// variant, which is the failure the type was introduced to make impossible.
///
/// `SummarySlot` is a fixed external format, not an open classification. A tenth slot would make
/// an existing compaction summary no longer conform to its own durable format, so consumers must
/// be able to match all nine slots and a format change has to be deliberate and breaking.
///
/// `ShellInvocation` is the reference's `shell: bool | list[str]`, a closed union every sandbox
/// backend `match`es to shape the command it runs. A `_` arm would run a new kind of invocation
/// through whichever shaping the arm picked, so a new variant has to stop every backend at compile
/// time until it says how to run it.
///
/// `FileMode` names the reference's fixed `FileMode` values, the octal digits of one permission
/// triplet. The set is POSIX's, not an open classification, and code that turns a mode into bits
/// must be able to cover all of it.
const EXHAUSTIVE_ALLOWED: &[(&str, &str)] = &[
    ("crates/ra-core/src/step.rs", "NextStep"),
    ("crates/ra-core/src/agent.rs", "ResolvedInstructions"),
    ("crates/ra-context/src/compaction/summary.rs", "SummarySlot"),
    ("crates/ra-core/src/sandbox/session.rs", "ShellInvocation"),
    // Codex protocol/src/mcp_policy.rs defines closed deserialization unions. Adding a form must
    // force matcher and validation consumers to handle it rather than selecting a fallback.
    ("crates/ra-config/src/mcp_policy.rs", "McpServerIdentity"),
    (
        "crates/ra-config/src/mcp_policy.rs",
        "McpServerValueMatcher",
    ),
    ("crates/ra-config/src/mcp_policy.rs", "McpServerRequirement"),
    // Native shell and PID isolation choices are closed source control-flow unions.
    // Adding a variant must force each launch/detection backend to handle it.
    (
        "crates/ra-exec/src/filesystem/runtime_options.rs",
        "LinuxSandboxPidNamespace",
    ),
    ("crates/ra-exec/src/shell_detect.rs", "ShellType"),
    ("crates/ra-core/src/sandbox/types.rs", "FileMode"),
    // Codex @ 815ef9bb: app-server-protocol/src/protocol/v2/config.rs.
    // Config omits forced_chatgpt_workspace_id, forced_login_method and apps (account services).
    // ConfigLayerSource/Metadata/Layer gain From<ra_config::...>; request records gain Eq.
    // These representation adaptations preserve the retained fields, defaults and serde rules.
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigLayerSource",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "AllowDenyRequirement",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "MergeStrategy",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "WriteStatus",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigWriteErrorCode",
    ),
    // Codex @ 815ef9bb: app-server-protocol/src/protocol/v2/plugin.rs.
    // PluginListMarketplaceKind retains Local, omitting Vertical, WorkspaceDirectory,
    // SharedWithMe and CreatedByMeRemote; PluginSource omits Remote (remote catalog/sharing).
    // Retained Local/Git/Npm source fields and tags keep the upstream shape.
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginListMarketplaceKind",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillScope",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "HookHandlerMetadata",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginSource",
    ),
    // Codex @ 815ef9bb: protocol/src/protocol.rs hook lifecycle records.
    // Keep the core snake_case representation, not app-server v2's camelCase projection.
    // HookTrustStatus keeps its existing non_exhaustive adaptation.
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookHandlerType",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookExecutionMode",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookScope",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookSource",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookRunStatus",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookOutputEntryKind",
    ),
    // Codex v2/plugin.rs policy-source and schedule unions retain their closed source shape.
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginInstallPolicySource",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "ScheduledTaskSchedule",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "ScheduledTaskWeekday",
    ),
    // Codex @ 815ef9bb utils/pty/src/child_command.rs: each launch backend handles these
    // closed process-group/session choices explicitly; adding a choice must revisit every backend.
    (
        "crates/ra-exec/src/native_process/child_command.rs",
        "ProcessMode",
    ),
    // Codex codex-mcp/src/server.rs: explicit credential authority remains a closed decision.
    // It is public here because catalog snapshots and ra-coding cross the upstream crate boundary.
    ("crates/ra-mcp/src/lib.rs", "McpCredentialPolicy"),
    // Codex rmcp-client/src/protocol_mode.rs: retain its two source compatibility selections.
    // This value port does not claim that the SDK transport implements the newer lifecycle.
    ("crates/ra-mcp/src/lib.rs", "McpProtocolMode"),
    // Codex codex-mcp/src/catalog.rs registration provenance, environment authority and actions.
    // McpServerSource::Extension omits only host_owned_apps, used by the excluded account Apps
    // policy exemption. The other variants and precedence/authority meanings retain the source.
    (
        "crates/ra-mcp/src/registration_catalog.rs",
        "McpServerSource",
    ),
    (
        "crates/ra-mcp/src/registration_catalog.rs",
        "McpEnvironmentAuthority",
    ),
    (
        "crates/ra-mcp/src/registration_catalog.rs",
        "McpServerConflictAction",
    ),
];

/// Structs allowed to have public fields, by the file they are declared in and their name.
///
/// The bar is narrow on purpose: an upstream record or one of the projections listed inline below,
/// whether it configures something or reports a result, where the repository's porting rule already
/// fixes the shape and a builder would be this implementation inventing a surface the reference
/// does not have. A type this port introduced itself, such as a request that bundles a reference
/// method's keyword arguments, does not qualify and uses a constructor or a builder, as the rest of
/// the crate's public structs do.
///
/// Scoped by path so the exemption cannot spread to a same-named type elsewhere, and listed one by
/// one so adding another is a decision somebody makes rather than a module-wide pass. Each entry
/// names the upstream type it mirrors; the comments list exclusions and representation changes
/// so the rationale travels with the source. This is not a general exemption for product records,
/// nor approval of every omitted service. These exemptions retain source compatibility costs.
const PUBLIC_FIELDS_ALLOWED: &[(&str, &str)] = &[
    // Private native records become nameable only through the feature-gated source-test facade.
    // The launch request retains the filesystem-facing fields of Codex's sandbox ExecRequest;
    // SandboxCwd and CancellableReader keep the source records used by the original tests.
    (
        "crates/ra-exec/src/filesystem/fs_sandbox.rs",
        "SandboxExecRequest",
    ),
    ("crates/ra-exec/src/filesystem/fs_sandbox.rs", "SandboxCwd"),
    (
        "crates/ra-exec/src/filesystem/local_file_system_read.rs",
        "CancellableReader",
    ),
    // Source runtime options, shell results and per-server parser outcomes retain their records.
    (
        "crates/ra-exec/src/filesystem/runtime_options.rs",
        "ExecServerRuntimeOptions",
    ),
    ("crates/ra-exec/src/shell_detect.rs", "DetectedShell"),
    (
        "crates/ra-mcp/src/plugin_config.rs",
        "PluginMcpServerParseError",
    ),
    (
        "crates/ra-mcp/src/plugin_config.rs",
        "PluginMcpConfigParseOutcome",
    ),
    // Codex protocol/src/mcp_policy.rs records retain their field-for-field upstream shape.
    ("crates/ra-config/src/mcp_policy.rs", "EnvironmentMcpPolicy"),
    (
        "crates/ra-config/src/mcp_policy.rs",
        "McpServerCommandMatcher",
    ),
    (
        "crates/ra-config/src/mcp_policy.rs",
        "PluginMcpRequirements",
    ),
    // Codex ext/skills/src/required.rs::EnvironmentSkillRequirements is a borrowed public record.
    (
        "crates/ra-tools/src/skills_ns/required.rs",
        "EnvironmentSkillRequirements",
    ),
    // Codex skill catalog and callback records retain the upstream public field surface.
    ("crates/ra-tools/src/skills_ns/catalog.rs", "SkillAuthority"),
    ("crates/ra-tools/src/skills_ns/catalog.rs", "SkillPackageId"),
    (
        "crates/ra-tools/src/skills_ns/catalog.rs",
        "SkillCatalogEntry",
    ),
    ("crates/ra-tools/src/skills_ns/catalog.rs", "SkillCatalog"),
    (
        "crates/ra-tools/src/skills_ns/catalog.rs",
        "SkillReadResult",
    ),
    (
        "crates/ra-tools/src/skills_ns/catalog.rs",
        "SkillProviderError",
    ),
    (
        "crates/ra-tools/src/skills_ns/provider.rs",
        "SkillListQuery",
    ),
    (
        "crates/ra-tools/src/skills_ns/provider.rs",
        "SkillReadRequest",
    ),
    // Agents SDK WebSearchTool, its image settings and OpenAI filter/location records. Their
    // public fields preserve the upstream configuration surface and forward-compatible filters.
    (
        "crates/ra-model/src/openai/responses/web_search.rs",
        "WebSearchTool",
    ),
    (
        "crates/ra-model/src/openai/responses/web_search.rs",
        "WebSearchToolImageSettings",
    ),
    (
        "crates/ra-model/src/openai/responses/web_search.rs",
        "WebSearchFilters",
    ),
    (
        "crates/ra-model/src/openai/responses/web_search.rs",
        "WebSearchUserLocation",
    ),
    // Codex ToolSpec::WebSearch fields, with the framework's optional image settings.
    (
        "crates/ra-model/src/openai/responses/web_search.rs",
        "ResponsesWebSearchTool",
    ),
    // The exact optional keyword record from `memory/session.py`,
    // `OpenAIResponsesCompactionArgs`; provider mode remains in its adapter.
    (
        "crates/ra-model/src/openai/compaction.rs",
        "OpenAiResponsesCompactionArgs",
    ),
    // Mount provider records: `sandbox/entries/mounts/providers/*.py`. Each is the provider's own
    // field list — bucket, container, credentials, endpoints — and a manifest written against the
    // reference has to read back here field for field.
    ("crates/ra-core/src/sandbox/entries/mounts.rs", "S3Mount"),
    ("crates/ra-core/src/sandbox/entries/mounts.rs", "GcsMount"),
    (
        "crates/ra-core/src/sandbox/entries/mounts.rs",
        "AzureBlobMount",
    ),
    ("crates/ra-core/src/sandbox/entries/mounts.rs", "BoxMount"),
    ("crates/ra-core/src/sandbox/entries/mounts.rs", "R2Mount"),
    (
        "crates/ra-core/src/sandbox/entries/mounts.rs",
        "S3FilesMount",
    ),
    // Mount pattern option records: `sandbox/entries/mounts/patterns.py`. Same argument — these are
    // the options each mount tool takes, spelled the way the reference spells them.
    (
        "crates/ra-core/src/sandbox/entries/mounts.rs",
        "FuseOptions",
    ),
    (
        "crates/ra-core/src/sandbox/entries/mounts.rs",
        "MountpointOptions",
    ),
    (
        "crates/ra-core/src/sandbox/entries/mounts.rs",
        "RcloneOptions",
    ),
    (
        "crates/ra-core/src/sandbox/entries/mounts.rs",
        "S3FilesOptions",
    ),
    // The manifest: `sandbox/manifest.py` `Manifest`. It is written by hand and read back from
    // serialized state, and its fields are the reference's.
    ("crates/ra-core/src/sandbox/manifest.rs", "Manifest"),
    // Accounts, permissions and results: `sandbox/types.py` `User`, `Group`, `Permissions`,
    // `ExecResult` and `ExposedPortEndpoint`.
    ("crates/ra-core/src/sandbox/types.rs", "User"),
    ("crates/ra-core/src/sandbox/types.rs", "Group"),
    ("crates/ra-core/src/sandbox/types.rs", "Permissions"),
    ("crates/ra-core/src/sandbox/types.rs", "ExecResult"),
    ("crates/ra-core/src/sandbox/types.rs", "ExposedPortEndpoint"),
    // A directory listing entry: `sandbox/files.py` `FileEntry`.
    ("crates/ra-core/src/sandbox/files.rs", "FileEntry"),
    // Interactive output: `sandbox/session/pty_types.py` `PtyExecUpdate`.
    ("crates/ra-core/src/sandbox/pty.rs", "PtyExecUpdate"),
    // Resolved mount tool configurations: `sandbox/entries/mounts/patterns.py`
    // `FuseMountConfig`, `MountpointMountConfig`, `RcloneMountConfig` and `S3FilesMountConfig`.
    ("crates/ra-sandbox/src/mounts/config.rs", "FuseMountConfig"),
    (
        "crates/ra-sandbox/src/mounts/config.rs",
        "MountpointMountConfig",
    ),
    (
        "crates/ra-sandbox/src/mounts/config.rs",
        "RcloneMountConfig",
    ),
    (
        "crates/ra-sandbox/src/mounts/config.rs",
        "S3FilesMountConfig",
    ),
    // The reference's `build_docker_volume_driver_config` returns the fixed tuple
    // `(driver, options, read_only)`; this names its three positions and adds nothing.
    (
        "crates/ra-sandbox/src/mounts/config.rs",
        "DockerVolumeDriverConfig",
    ),
    // Thread management payloads: Codex `app-server-protocol/src/protocol/v2/thread.rs`
    // `ThreadMetadataGitInfoUpdateParams`, `ThreadUnarchiveResponse`,
    // `ThreadMetadataUpdateResponse`, `ThreadListParams`, `ThreadListResponse`, `ThreadReadState`,
    // `ThreadArchivedNotification`, `ThreadDeletedNotification`, `ThreadUnarchivedNotification`
    // and `ThreadNameUpdatedNotification`. They are wire records whose fields are Codex's.
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadArchivedNotification",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadDeletedNotification",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadUnarchivedNotification",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadNameUpdatedNotification",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadMetadataGitInfoUpdateParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadUnarchiveResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadMetadataUpdateResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadListParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadListResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadReadState",
    ),
    // Field-for-field Codex read-state update and notification wire records.
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadReadStateUpdateParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadReadStateUpdateResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/thread_management.rs",
        "ThreadReadStateChangedNotification",
    ),
    // Persisted Git facts and their patch: Codex `protocol/src/protocol.rs` `GitInfo` and
    // `thread-store/src/types.rs` `GitInfoPatch`.
    ("crates/ra-session/src/store/git.rs", "GitInfo"),
    ("crates/ra-session/src/store/git.rs", "GitInfoPatch"),
    // Codex @ 815ef9bb: app-server-protocol/src/protocol/v2/config.rs.
    // Config omits forced_chatgpt_workspace_id, forced_login_method and apps (account services).
    // ConfigLayerSource/Metadata/Layer gain From<ra_config::...>; request records gain Eq.
    // These representation adaptations preserve the retained fields, defaults and serde rules.
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "SandboxWorkspaceWrite",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ToolsV2",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "AnalyticsConfig",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "BrowserUseConfig",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "BrowserUseOriginPolicyConfig",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ComputerUseConfig",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ComputerUseMacosConfig",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ComputerUseWindowsConfig",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ComputerUseWindowsExeConfig",
    ),
    ("crates/ra-protocol/src/server/protocol/config.rs", "Config"),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigLayerMetadata",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigLayer",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "OverriddenMetadata",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigWriteResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigReadParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigReadResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigValueWriteParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigBatchWriteParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "ConfigEdit",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "SkillsConfigWriteParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/config.rs",
        "SkillsConfigWriteResponse",
    ),
    // Codex @ 815ef9bb: app-server-protocol/src/protocol/v2/plugin.rs.
    // Remote catalog/sharing projection: PluginReadParams and PluginInstallParams omit
    // remote_marketplace_name; PluginInstallResponse omits apps_needing_auth.
    // PluginSummary omits remote_plugin_id, version, share_context, installed_at,
    // availability, disabled_reason and eligible_plan_types. The retained install_policy_source
    // and must_show_installation_interstitial are None in the source local converters.
    // PluginDetail omits share_url, apps and app_templates. Its retained scheduled_tasks is None
    // in the source local converter; the source remote converter forwards service data.
    // PluginListResponse retains featured_plugin_ids. The source trigger matches openai-curated
    // exactly; this product's openai-api-curated lists return [] without requesting the service.
    // The anonymous fetch and source cache remain available for the exact-name trigger.
    // PluginListParams retains upstream field order; force_refetch keeps only the local Git/npm
    // cache refresh, omitting the source's remote catalog refresh. Eq derives support requests.
    // These explicit projections retain public record construction, not full wire parity.
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillsListParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillsListResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillsExtraRootsSetParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "HooksListParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "HooksListResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceAddParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceAddResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceRemoveParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceRemoveResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceUpgradeParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceUpgradeResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceUpgradeErrorInfo",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginListParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginListResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginInstalledParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginInstalledResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceLoadErrorInfo",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginReadParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginReadResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillMetadata",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillInterface",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillDependencies",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillToolDependency",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillErrorInfo",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillsListEntry",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "HooksListEntry",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "HookMetadata",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "HookErrorInfo",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginMarketplaceEntry",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "MarketplaceInterface",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginSummary",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginDetail",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginHookSummary",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "SkillSummary",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginInterface",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginInstallParams",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginInstallResponse",
    ),
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "PluginUninstallParams",
    ),
    // Codex @ 815ef9bb: protocol/src/protocol.rs hook lifecycle records.
    // Keep the core snake_case representation, not app-server v2's camelCase projection.
    // HookTrustStatus keeps its existing non_exhaustive adaptation.
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookOutputEntry",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookRunSummary",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookStartedEvent",
    ),
    (
        "crates/ra-protocol/src/server/protocol/hooks.rs",
        "HookCompletedEvent",
    ),
    // Codex v2/plugin.rs ScheduledTaskSummary retains all four source fields.
    (
        "crates/ra-protocol/src/server/protocol/plugin_management.rs",
        "ScheduledTaskSummary",
    ),
    // Codex @ 815ef9bb utils/pty/src/child.rs: Child retains the three owned Tokio stdio
    // handles. The private inner field already prevents external literal construction.
    ("crates/ra-exec/src/native_process/child.rs", "Child"),
    // Codex @ 815ef9bb codex-mcp/src/catalog.rs: preserve the complete conflict result record
    // (name, outcome, contenders) used by the source precedence and veto tests.
    (
        "crates/ra-mcp/src/registration_catalog.rs",
        "McpServerConflict",
    ),
];

/// Checks public records and enums in one workspace-relative source file.
///
/// Parsing failures are returned rather than treated as a passing check. Both path separators
/// are accepted so the same exact file exceptions apply on Unix and Windows.
pub fn check_source(path: &str, source: &str) -> Result<Vec<String>, syn::Error> {
    let parsed = syn::parse_file(source)?;
    let mut violations = Vec::new();
    check_items(&parsed.items, &path.replace('\\', "/"), &mut violations);
    Ok(violations)
}

fn is_allowed(entries: &[(&str, &str)], path: &str, name: &str) -> bool {
    entries
        .iter()
        .any(|(allowed_path, allowed_name)| *allowed_path == path && *allowed_name == name)
}

fn check_items(items: &[Item], where_: &str, violations: &mut Vec<String>) {
    for item in items {
        match item {
            Item::Enum(e) if matches!(e.vis, Visibility::Public(_)) => {
                let name = e.ident.to_string();
                let has_attr = e
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("non_exhaustive"));
                if !has_attr && !is_allowed(EXHAUSTIVE_ALLOWED, where_, &name) {
                    violations.push(format!(
                        "{where_}：公开枚举 `{name}` 缺 `#[non_exhaustive]`（扩展安全 ①）——\
                         加一个变体就会让所有下游 match 编译失败。\
                         若它是已核验的上游移植或封闭控制流，按文件和类型登记例外并写明理由"
                    ));
                }
            }
            Item::Struct(s) if matches!(s.vis, Visibility::Public(_)) => {
                let name = s.ident.to_string();
                if is_allowed(PUBLIC_FIELDS_ALLOWED, where_, &name) {
                    continue;
                }
                for field in &s.fields {
                    if matches!(field.vis, Visibility::Public(_)) {
                        let field_name = field
                            .ident
                            .as_ref()
                            .map_or_else(|| "0".to_owned(), ToString::to_string);
                        violations.push(format!(
                            "{where_}：公开结构体 `{name}` 有公开字段 `{field_name}`（扩展安全 ②）——\
                             字面量构造一旦流行，加字段就是破坏性变更。用构造函数或 builder"
                        ));
                    }
                }
            }
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    check_items(inner, where_, violations);
                }
            }
            _ => {}
        }
    }
}
