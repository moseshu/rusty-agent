//! The permissions the agent asks the client to grant, and the policy amendments it proposes:
//! Codex's v2 permission payloads (`protocol/v2/permissions.rs`).
//!
//! Paths are carried as the strings the server wrote, as Codex's `LegacyAppPathString` carries
//! them, so a client on another host reads them as spelled rather than reinterpreting them.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

/// The protocol of a network access awaiting approval: Codex's `NetworkApprovalProtocol`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkApprovalProtocol {
    /// Plain HTTP.
    Http,
    /// HTTPS.
    Https,
    /// A TCP connection through SOCKS5.
    Socks5Tcp,
    /// A UDP association through SOCKS5.
    Socks5Udp,
}

/// The network access a command approval is about: Codex's `NetworkApprovalContext`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkApprovalContext {
    host: String,
    protocol: NetworkApprovalProtocol,
}

impl NetworkApprovalContext {
    /// Access to `host` over `protocol`.
    #[must_use]
    pub fn new(host: impl Into<String>, protocol: NetworkApprovalProtocol) -> Self {
        Self {
            host: host.into(),
            protocol,
        }
    }

    /// The host to be reached.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The protocol to reach it over.
    #[must_use]
    pub const fn protocol(&self) -> NetworkApprovalProtocol {
        self.protocol
    }
}

/// A command prefix the client may allow without being asked again: Codex's
/// `ExecPolicyAmendment`, written as the array of its words.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecPolicyAmendment {
    command: Vec<String>,
}

impl ExecPolicyAmendment {
    /// The prefix made of `command`'s words.
    #[must_use]
    pub fn new(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            command: command.into_iter().map(Into::into).collect(),
        }
    }

    /// The words of the prefix.
    #[must_use]
    pub fn command(&self) -> &[String] {
        &self.command
    }
}

/// Whether a network rule allows or denies its host: Codex's `NetworkPolicyRuleAction`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkPolicyRuleAction {
    /// Allow the host.
    Allow,
    /// Deny the host.
    Deny,
}

/// A persistent rule for one host: Codex's `NetworkPolicyAmendment`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyAmendment {
    host: String,
    action: NetworkPolicyRuleAction,
}

impl NetworkPolicyAmendment {
    /// A rule that applies `action` to `host`.
    #[must_use]
    pub fn new(host: impl Into<String>, action: NetworkPolicyRuleAction) -> Self {
        Self {
            host: host.into(),
            action,
        }
    }

    /// The host the rule is for.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether the rule allows or denies it.
    #[must_use]
    pub const fn action(&self) -> NetworkPolicyRuleAction {
        self.action
    }
}

/// Network access asked for or granted: Codex's `AdditionalNetworkPermissions`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalNetworkPermissions {
    enabled: Option<bool>,
}

impl AdditionalNetworkPermissions {
    /// Leaves network access as it is.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables network access, or disables it.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Whether network access is enabled, if this says.
    #[must_use]
    pub const fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

/// How a filesystem entry may be accessed: Codex's `FileSystemAccessMode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileSystemAccessMode {
    /// Read only.
    Read,
    /// Read and write.
    Write,
    /// No access.
    Deny,
}

/// A location the sandbox names rather than spells: Codex's `FileSystemSpecialPath`.
///
/// `current_working_directory` is read as [`Self::ProjectRoots`], as in Codex.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSystemSpecialPath {
    /// The filesystem root.
    Root,
    /// The minimal set of system locations a process needs.
    Minimal,
    /// The project roots, or `subpath` under each.
    #[non_exhaustive]
    #[serde(alias = "current_working_directory")]
    ProjectRoots {
        /// A path under each root, if the entry is not the roots themselves.
        subpath: Option<String>,
    },
    /// The temporary directory of the environment.
    Tmpdir,
    /// `/tmp`.
    SlashTmp,
    /// A special path this version does not know, kept as it was written.
    #[non_exhaustive]
    Unknown {
        /// The name of the special path.
        path: String,
        /// A path under it, if any.
        subpath: Option<String>,
    },
}

impl FileSystemSpecialPath {
    /// The project roots, or `subpath` under each.
    #[must_use]
    pub fn project_roots(subpath: Option<String>) -> Self {
        Self::ProjectRoots { subpath }
    }

    /// A special path this version does not know.
    #[must_use]
    pub fn unknown(path: impl Into<String>, subpath: Option<String>) -> Self {
        Self::Unknown {
            path: path.into(),
            subpath,
        }
    }
}

/// What a filesystem entry covers: Codex's v2 `FileSystemPath`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileSystemPath {
    /// One path.
    #[non_exhaustive]
    Path {
        /// The path.
        path: String,
    },
    /// The paths a glob pattern matches.
    #[non_exhaustive]
    GlobPattern {
        /// The pattern.
        pattern: String,
    },
    /// A location the sandbox names.
    #[non_exhaustive]
    Special {
        /// The location.
        value: FileSystemSpecialPath,
    },
}

impl FileSystemPath {
    /// One path.
    #[must_use]
    pub fn path(path: impl Into<String>) -> Self {
        Self::Path { path: path.into() }
    }

    /// The paths `pattern` matches.
    #[must_use]
    pub fn glob_pattern(pattern: impl Into<String>) -> Self {
        Self::GlobPattern {
            pattern: pattern.into(),
        }
    }

    /// A location the sandbox names.
    #[must_use]
    pub const fn special(value: FileSystemSpecialPath) -> Self {
        Self::Special { value }
    }
}

/// Access to one filesystem entry: Codex's v2 `FileSystemSandboxEntry`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemSandboxEntry {
    path: FileSystemPath,
    access: FileSystemAccessMode,
}

impl FileSystemSandboxEntry {
    /// `access` to `path`.
    #[must_use]
    pub const fn new(path: FileSystemPath, access: FileSystemAccessMode) -> Self {
        Self { path, access }
    }

    /// What the entry covers.
    #[must_use]
    pub const fn path(&self) -> &FileSystemPath {
        &self.path
    }

    /// How it may be accessed.
    #[must_use]
    pub const fn access(&self) -> FileSystemAccessMode {
        self.access
    }
}

/// Filesystem access asked for or granted: Codex's `AdditionalFileSystemPermissions`.
///
/// `read` and `write` are the roots of Codex's older form, which it means to replace with
/// `entries`; both forms are carried as Codex carries them.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalFileSystemPermissions {
    read: Option<Vec<String>>,
    write: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    glob_scan_max_depth: Option<NonZeroUsize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<FileSystemSandboxEntry>>,
}

impl AdditionalFileSystemPermissions {
    /// No filesystem access.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the roots that may be read.
    #[must_use]
    pub fn with_read(mut self, read: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.read = Some(read.into_iter().map(Into::into).collect());
        self
    }

    /// Sets the roots that may be written.
    #[must_use]
    pub fn with_write(mut self, write: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.write = Some(write.into_iter().map(Into::into).collect());
        self
    }

    /// Limits how deep a glob entry is expanded.
    #[must_use]
    pub const fn with_glob_scan_max_depth(mut self, depth: NonZeroUsize) -> Self {
        self.glob_scan_max_depth = Some(depth);
        self
    }

    /// Sets the entries, the form that replaces the read and write roots.
    #[must_use]
    pub fn with_entries(
        mut self,
        entries: impl IntoIterator<Item = FileSystemSandboxEntry>,
    ) -> Self {
        self.entries = Some(entries.into_iter().collect());
        self
    }

    /// The roots that may be read, in the older form.
    #[must_use]
    pub fn read(&self) -> Option<&[String]> {
        self.read.as_deref()
    }

    /// The roots that may be written, in the older form.
    #[must_use]
    pub fn write(&self) -> Option<&[String]> {
        self.write.as_deref()
    }

    /// How deep a glob entry is expanded, if limited.
    #[must_use]
    pub const fn glob_scan_max_depth(&self) -> Option<NonZeroUsize> {
        self.glob_scan_max_depth
    }

    /// The entries, if given in that form.
    #[must_use]
    pub fn entries(&self) -> Option<&[FileSystemSandboxEntry]> {
        self.entries.as_deref()
    }
}

/// The permissions a `request_permissions` call asks for: Codex's `RequestPermissionProfile`.
///
/// Members it does not define are refused rather than ignored, as Codex refuses them, so a request
/// for a kind of permission this version cannot express is not read as a smaller one.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestPermissionProfile {
    network: Option<AdditionalNetworkPermissions>,
    file_system: Option<AdditionalFileSystemPermissions>,
}

impl RequestPermissionProfile {
    /// Asks for nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks for network access.
    #[must_use]
    pub const fn with_network(mut self, network: AdditionalNetworkPermissions) -> Self {
        self.network = Some(network);
        self
    }

    /// Asks for filesystem access.
    #[must_use]
    pub fn with_file_system(mut self, file_system: AdditionalFileSystemPermissions) -> Self {
        self.file_system = Some(file_system);
        self
    }

    /// The network access asked for.
    #[must_use]
    pub const fn network(&self) -> Option<&AdditionalNetworkPermissions> {
        self.network.as_ref()
    }

    /// The filesystem access asked for.
    #[must_use]
    pub const fn file_system(&self) -> Option<&AdditionalFileSystemPermissions> {
        self.file_system.as_ref()
    }
}

/// Permissions one command asks for beyond the sandbox's: Codex's `AdditionalPermissionProfile`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalPermissionProfile {
    network: Option<AdditionalNetworkPermissions>,
    file_system: Option<AdditionalFileSystemPermissions>,
}

impl AdditionalPermissionProfile {
    /// Nothing beyond the sandbox's.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds network access.
    #[must_use]
    pub const fn with_network(mut self, network: AdditionalNetworkPermissions) -> Self {
        self.network = Some(network);
        self
    }

    /// Adds filesystem access.
    #[must_use]
    pub fn with_file_system(mut self, file_system: AdditionalFileSystemPermissions) -> Self {
        self.file_system = Some(file_system);
        self
    }

    /// The network access added.
    #[must_use]
    pub const fn network(&self) -> Option<&AdditionalNetworkPermissions> {
        self.network.as_ref()
    }

    /// The filesystem access added.
    #[must_use]
    pub const fn file_system(&self) -> Option<&AdditionalFileSystemPermissions> {
        self.file_system.as_ref()
    }
}

/// The permissions the client grants: Codex's `GrantedPermissionProfile`.
///
/// What is not granted is left out, as Codex leaves it out.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantedPermissionProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network: Option<AdditionalNetworkPermissions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_system: Option<AdditionalFileSystemPermissions>,
}

impl GrantedPermissionProfile {
    /// Grants nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants network access.
    #[must_use]
    pub const fn with_network(mut self, network: AdditionalNetworkPermissions) -> Self {
        self.network = Some(network);
        self
    }

    /// Grants filesystem access.
    #[must_use]
    pub fn with_file_system(mut self, file_system: AdditionalFileSystemPermissions) -> Self {
        self.file_system = Some(file_system);
        self
    }

    /// The network access granted.
    #[must_use]
    pub const fn network(&self) -> Option<&AdditionalNetworkPermissions> {
        self.network.as_ref()
    }

    /// The filesystem access granted.
    #[must_use]
    pub const fn file_system(&self) -> Option<&AdditionalFileSystemPermissions> {
        self.file_system.as_ref()
    }
}

/// How long granted permissions last: Codex's `PermissionGrantScope`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionGrantScope {
    /// For the rest of the turn.
    #[default]
    Turn,
    /// For the rest of the session.
    Session,
}

/// The parameters of `item/permissions/requestApproval`: Codex's
/// `PermissionsRequestApprovalParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRequestApprovalParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    #[serde(default)]
    environment_id: Option<String>,
    started_at_ms: i64,
    cwd: String,
    reason: Option<String>,
    permissions: RequestPermissionProfile,
}

impl PermissionsRequestApprovalParams {
    /// Asks for `permissions` for the item `item_id` of `turn_id` in `thread_id`, working in `cwd`,
    /// at `started_at_ms` milliseconds after the Unix epoch.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        started_at_ms: i64,
        cwd: impl Into<String>,
        permissions: RequestPermissionProfile,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            environment_id: None,
            started_at_ms,
            cwd: cwd.into(),
            reason: None,
            permissions,
        }
    }

    /// Sets the environment the permissions are for.
    #[must_use]
    pub fn with_environment_id(mut self, environment_id: impl Into<String>) -> Self {
        self.environment_id = Some(environment_id.into());
        self
    }

    /// Sets why the permissions are asked for.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// The environment the permissions are for.
    #[must_use]
    pub fn environment_id(&self) -> Option<&str> {
        self.environment_id.as_deref()
    }

    /// When the approval was asked, in milliseconds after the Unix epoch.
    #[must_use]
    pub const fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// The directory relative paths are resolved against.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Why the permissions are asked for.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The permissions asked for.
    #[must_use]
    pub const fn permissions(&self) -> &RequestPermissionProfile {
        &self.permissions
    }
}

/// The answer to `item/permissions/requestApproval`: Codex's
/// `PermissionsRequestApprovalResponse`.
///
/// A grant is for the rest of the turn unless it says otherwise. `strictAutoReview` asks that
/// every later command of the turn be reviewed; Codex accepts it only for a grant scoped to the
/// turn.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRequestApprovalResponse {
    permissions: GrantedPermissionProfile,
    #[serde(default)]
    scope: PermissionGrantScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strict_auto_review: Option<bool>,
}

impl PermissionsRequestApprovalResponse {
    /// Grants `permissions` for the rest of the turn.
    #[must_use]
    pub fn new(permissions: GrantedPermissionProfile) -> Self {
        Self {
            permissions,
            scope: PermissionGrantScope::Turn,
            strict_auto_review: None,
        }
    }

    /// Sets how long the grant lasts.
    #[must_use]
    pub const fn with_scope(mut self, scope: PermissionGrantScope) -> Self {
        self.scope = scope;
        self
    }

    /// Asks that every later command of the turn be reviewed, or not.
    #[must_use]
    pub const fn with_strict_auto_review(mut self, strict_auto_review: bool) -> Self {
        self.strict_auto_review = Some(strict_auto_review);
        self
    }

    /// The permissions granted.
    #[must_use]
    pub const fn permissions(&self) -> &GrantedPermissionProfile {
        &self.permissions
    }

    /// How long the grant lasts.
    #[must_use]
    pub const fn scope(&self) -> PermissionGrantScope {
        self.scope
    }

    /// Whether every later command of the turn is to be reviewed, if the client said.
    #[must_use]
    pub const fn strict_auto_review(&self) -> Option<bool> {
        self.strict_auto_review
    }
}
