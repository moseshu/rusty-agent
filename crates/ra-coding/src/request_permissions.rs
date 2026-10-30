//! The coding product's side of `request_permissions`: Codex's handler
//! (`core/src/tools/handlers/request_permissions.rs`) and the session calls it makes
//! (`Session::request_permissions_for_environment`, `notify_request_permissions_response`,
//! `record_granted_request_permissions_for_turn`), over the permission profile of
//! [`crate::permissions`].
//!
//! A call resolves the paths it names against the thread's cwd, checks the profile, and — unless
//! the approval policy asks nothing — asks the client through the [`ThreadServerRequests`] the app
//! server handed the thread. The grant is narrowed to what was requested and kept for the turn or
//! for the session, as the client chose; the model is given the response as Codex serializes it.
//! Later commands run with the grants the turn and the session hold (see
//! [`crate::thread_settings`]).
//!
//! # Differences from Codex
//!
//! - The thread has one environment, Codex's `local`, so `environment_id` may only name it.
//! - A thread no app server loaded has nobody to ask, and is granted nothing, as Codex's grant
//!   is when its client cannot answer.
//! - Codex's Guardian reviewer is not ported, so a grant that asks for strict automatic review of
//!   the turn's later commands is not kept: nothing is granted rather than more than was meant.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};

use async_trait::async_trait;
use ra_core::{state::RunId, tool::ToolContext};
use ra_paths::{LegacyAppPathString, PathUri};
use ra_protocol::server::{
    ThreadServerRequests,
    protocol::{
        AdditionalFileSystemPermissions, AdditionalNetworkPermissions,
        AdditionalPermissionProfile as WireAdditionalProfile, AskForApproval,
        FileSystemAccessMode as WireAccess, FileSystemPath as WirePath,
        FileSystemSandboxEntry as WireEntry, FileSystemSpecialPath as WireSpecial,
        GrantedPermissionProfile, PermissionGrantScope as WireScope,
        PermissionsRequestApprovalParams, PermissionsRequestApprovalResponse,
        RequestPermissionProfile as WireRequestProfile,
    },
};
use ra_tools::request_permissions::RequestPermissionsPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    approvals::LOCAL_ENVIRONMENT_ID,
    permissions::{
        AdditionalPermissionProfile, FileSystemAccessMode, FileSystemPath, FileSystemPermissions,
        FileSystemSandboxEntry, FileSystemSandboxPolicyContext, FileSystemSpecialPath,
        NetworkPermissions, intersect_permission_profiles_with_context, merge_permission_profiles,
        normalize_additional_permissions_with_context, path,
    },
    thread_settings::CodingThreadSettings,
};

/// Whether a grant lasts for its turn or for the session: Codex's `PermissionGrantScope`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PermissionGrantScope {
    /// The rest of the turn.
    #[default]
    Turn,
    /// The rest of the session.
    Session,
}

/// What a `request_permissions` call is given back: Codex's `RequestPermissionsResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct RequestPermissionsResponse {
    permissions: RequestPermissionProfile,
    scope: PermissionGrantScope,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    strict_auto_review: bool,
}

/// Codex's `RequestPermissionProfile`: an [`AdditionalPermissionProfile`] that refuses fields it
/// does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestPermissionProfile {
    network: Option<NetworkPermissions>,
    file_system: Option<FileSystemPermissions>,
}

impl From<RequestPermissionProfile> for AdditionalPermissionProfile {
    fn from(profile: RequestPermissionProfile) -> Self {
        let mut additional = Self::new();
        if let Some(network) = profile.network {
            additional = additional.with_network(network);
        }
        if let Some(file_system) = profile.file_system {
            additional = additional.with_file_system(file_system);
        }
        additional
    }
}

impl From<AdditionalPermissionProfile> for RequestPermissionProfile {
    fn from(profile: AdditionalPermissionProfile) -> Self {
        Self {
            network: profile.network().cloned(),
            file_system: profile.file_system().cloned(),
        }
    }
}

/// Codex's `RequestPermissionsArgs`; its `environment_id` is read separately, first.
#[derive(Debug, Deserialize)]
struct RequestPermissionsArgs {
    #[serde(default)]
    reason: Option<String>,
    permissions: RequestPermissionProfile,
}

/// Codex's `RequestPermissionsEnvironmentArgs`.
#[derive(Debug, Deserialize)]
struct RequestPermissionsEnvironmentArgs {
    #[serde(default, rename = "environment_id", alias = "environmentId")]
    environment_id: Option<String>,
}

/// A thread's way to its client and the permissions granted so far: Codex's pending request
/// table's counterpart and its turn and session grants.
#[derive(Debug, Default)]
pub(crate) struct ThreadPermissions {
    requests: OnceLock<ThreadServerRequests>,
    session: Mutex<Option<AdditionalPermissionProfile>>,
    turns: Mutex<HashMap<RunId, AdditionalPermissionProfile>>,
}

impl ThreadPermissions {
    /// Keeps the way the app server gave the thread to ask its client.
    pub(crate) fn attach(&self, requests: ThreadServerRequests) {
        let _ = self.requests.set(requests);
    }

    /// What the session was granted: Codex's `granted_session_permissions`.
    pub(crate) fn session(&self) -> Option<AdditionalPermissionProfile> {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the turn `run_id` was granted: Codex's `granted_turn_permissions`.
    pub(crate) fn turn(&self, run_id: &RunId) -> Option<AdditionalPermissionProfile> {
        self.turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run_id)
            .cloned()
    }

    /// Forgets the grants of a turn that ended, as Codex's turn state goes with its turn.
    pub(crate) fn finish_turn(&self, run_id: &RunId) {
        self.turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(run_id);
    }

    /// Adds `response`'s grant to its turn's or the session's: Codex's
    /// `record_granted_request_permissions_for_turn` and `record_granted_permissions`.
    fn record(&self, run_id: &RunId, response: &RequestPermissionsResponse) {
        let granted = AdditionalPermissionProfile::from(response.permissions.clone());
        if granted.is_empty() {
            return;
        }
        match response.scope {
            PermissionGrantScope::Turn => {
                let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(merged) = merge_permission_profiles(turns.get(run_id), Some(&granted)) {
                    turns.insert(run_id.clone(), merged);
                }
            }
            PermissionGrantScope::Session => {
                let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(merged) = merge_permission_profiles(session.as_ref(), Some(&granted)) {
                    *session = Some(merged);
                }
            }
        }
    }
}

/// The coding product's [`RequestPermissionsPolicy`].
#[derive(Debug, Default)]
pub(crate) struct CodingPermissionRequests;

#[async_trait]
impl RequestPermissionsPolicy for CodingPermissionRequests {
    async fn request_permissions(
        &self,
        context: &ToolContext<'_>,
    ) -> std::result::Result<String, String> {
        let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
            return Err("request_permissions requires a primary environment".to_owned());
        };
        let environment_args: RequestPermissionsEnvironmentArgs =
            parse_arguments(context.arguments().clone())?;
        if let Some(environment_id) = environment_args.environment_id.as_deref()
            && environment_id != LOCAL_ENVIRONMENT_ID
        {
            return Err(format!("unknown turn environment id `{environment_id}`"));
        }
        let paths = PolicyPaths::new(settings.cwd());
        let policy_context = paths.context();
        let mut arguments = context.arguments().clone();
        resolve_permission_path_strings(&mut arguments, &policy_context)?;
        let args: RequestPermissionsArgs = parse_arguments(arguments)?;
        let permissions = normalize_additional_permissions_with_context(
            args.permissions.into(),
            &policy_context,
        )?;
        if permissions.is_empty() {
            return Err("request_permissions requires at least one permission".to_owned());
        }
        let response = request_permissions_for_environment(
            settings,
            context,
            args.reason,
            permissions,
            &policy_context,
        )
        .await
        .ok_or_else(|| {
            "request_permissions was cancelled before receiving a response".to_owned()
        })?;
        serde_json::to_string(&response)
            .map_err(|error| format!("failed to serialize request_permissions response: {error}"))
    }
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments)
        .map_err(|error| format!("failed to parse function arguments: {error}"))
}

/// The paths a thread's permissions are interpreted with: Codex's `FileSystemSandboxContext` for
/// the local environment.
pub(crate) struct PolicyPaths {
    cwd: PathBuf,
    workspace_roots: [PathBuf; 1],
    user_home_dir: Option<PathBuf>,
    temporary_directories: Vec<PathBuf>,
}

impl PolicyPaths {
    /// The paths of a thread whose cwd, and one project root, is `cwd`.
    pub(crate) fn new(cwd: &Path) -> Self {
        Self::for_command(cwd, cwd)
    }

    /// The paths of a command running in `cwd` in a thread whose project root is `workspace_root`:
    /// Codex's `file_system_sandbox_policy_context_for_cwd`.
    pub(crate) fn for_command(workspace_root: &Path, cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace_roots: [workspace_root.to_path_buf()],
            user_home_dir: std::env::home_dir(),
            temporary_directories: local_temporary_directories(),
        }
    }

    pub(crate) fn context(&self) -> FileSystemSandboxPolicyContext<'_> {
        let context = FileSystemSandboxPolicyContext::new(&self.cwd, &self.workspace_roots)
            .with_temporary_directories(&self.temporary_directories);
        match &self.user_home_dir {
            Some(home) => context.with_user_home_dir(home),
            None => context,
        }
    }
}

/// `TMPDIR`, if it names an absolute path: Codex's `local_temporary_directories`.
fn local_temporary_directories() -> Vec<PathBuf> {
    std::env::var_os("TMPDIR")
        .filter(|path| !path.is_empty())
        .and_then(|path| path::normalize(Path::new(&path)))
        .into_iter()
        .collect()
}

/// Asks the client for `permissions` unless the approval policy asks nothing, and keeps what it
/// grants: Codex's `request_permissions_for_environment` and `notify_request_permissions_response`.
async fn request_permissions_for_environment(
    settings: &CodingThreadSettings,
    context: &ToolContext<'_>,
    reason: Option<String>,
    permissions: AdditionalPermissionProfile,
    policy_context: &FileSystemSandboxPolicyContext<'_>,
) -> Option<RequestPermissionsResponse> {
    match settings.approval_policy() {
        AskForApproval::Never
        | AskForApproval::Granular {
            request_permissions: false,
            ..
        } => return Some(RequestPermissionsResponse::default()),
        _ => {}
    }
    let Some(requests) = settings.thread_permissions().requests.get() else {
        return Some(RequestPermissionsResponse::default());
    };
    let run_id = ra_runtime::agent::tool::current_root_run_id()
        .unwrap_or_else(|| context.run().run_id().clone());
    let mut params = PermissionsRequestApprovalParams::new(
        requests.thread_id(),
        run_id.as_str(),
        context.call_id().as_str(),
        now_unix_timestamp_ms(),
        policy_context.cwd().to_string_lossy(),
        wire_request_profile(&permissions),
    )
    .with_environment_id(LOCAL_ENVIRONMENT_ID);
    if let Some(reason) = reason {
        params = params.with_reason(reason);
    }
    let response = requests.request_permissions(params).await?;
    let response = normalize_request_permissions_response(
        permissions,
        core_response(&response),
        policy_context,
    );
    settings.thread_permissions().record(&run_id, &response);
    Some(response)
}

/// Milliseconds since the Unix epoch: Codex's `now_unix_timestamp_ms`.
fn now_unix_timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

/// The grant narrowed to what was requested: Codex's `normalize_request_permissions_response`.
/// Strict automatic review is refused for a session, as Codex refuses it, and here for a turn too,
/// since its reviewer is not ported.
fn normalize_request_permissions_response(
    requested: AdditionalPermissionProfile,
    response: RequestPermissionsResponse,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> RequestPermissionsResponse {
    if response.strict_auto_review {
        if response.scope == PermissionGrantScope::Session {
            tracing::error!(
                "strict auto review is only supported for turn-scoped permission grants"
            );
        } else {
            tracing::error!("strict auto review needs a reviewer this runtime does not have");
        }
        return RequestPermissionsResponse::default();
    }
    let granted = AdditionalPermissionProfile::from(response.permissions.clone());
    if granted.is_empty() {
        return response;
    }
    RequestPermissionsResponse {
        permissions: intersect_permission_profiles_with_context(requested, granted, context).into(),
        scope: response.scope,
        strict_auto_review: false,
    }
}

/// Codex's `resolve_permission_path_strings`: the `read` and `write` lists become path entries,
/// and every path entry is resolved against the cwd, `~` naming the home directory.
fn resolve_permission_path_strings(
    arguments: &mut Value,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Result<(), String> {
    let Some(file_system) = arguments
        .pointer_mut("/permissions/file_system")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    let mut legacy_entries = Vec::new();
    for (field, access) in [("read", "read"), ("write", "write")] {
        let Some(paths) = file_system.remove(field) else {
            continue;
        };
        if paths.is_null() {
            continue;
        }
        let Value::Array(paths) = paths else {
            file_system.insert(field.to_owned(), paths);
            continue;
        };
        for mut path in paths {
            resolve_permission_path_string(&mut path, context)?;
            legacy_entries.push(serde_json::json!({
                "path": {"type": "path", "path": path},
                "access": access,
            }));
        }
    }
    if !legacy_entries.is_empty() {
        file_system
            .entry("entries")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| "request_permissions file_system.entries must be an array".to_owned())?
            .extend(legacy_entries);
    }
    if let Some(entries) = file_system.get_mut("entries").and_then(Value::as_array_mut) {
        for entry in entries {
            if entry.pointer("/path/type").and_then(Value::as_str) == Some("path")
                && let Some(path) = entry.pointer_mut("/path/path")
            {
                resolve_permission_path_string(path, context)?;
            }
        }
    }
    Ok(())
}

/// Codex's `resolve_permission_path_string`: the path is resolved in the executor cwd's own path
/// convention, so a native Windows path is accepted on a Windows cwd and refused on a POSIX one.
pub(crate) fn resolve_permission_path_string(
    value: &mut Value,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Result<(), String> {
    let Some(raw) = value.as_str() else {
        return Ok(());
    };
    let cwd = PathUri::from_host_native_path(context.cwd()).map_err(|error| error.to_string())?;
    let convention = cwd
        .infer_path_convention()
        .ok_or_else(|| format!("request_permissions cwd `{cwd}` has no path convention"))?;
    let home = context
        .user_home_dir()
        .map(PathUri::from_host_native_path)
        .transpose()
        .map_err(|error| error.to_string())?;
    let path = LegacyAppPathString::from_string(raw)
        .resolve_against(&cwd, home.as_ref())
        .map_err(|error| error.to_string())?;
    if path.is_opaque() || std::str::from_utf8(&path.decoded_path_bytes()).is_err() {
        return Err("permission path cannot be represented losslessly".to_owned());
    }
    let path =
        LegacyAppPathString::from_path_uri(&path, convention).map_err(|error| error.to_string())?;
    *value = Value::String(path.into_string());
    Ok(())
}

/// The profile as the request carries it: Codex's conversion into its v2 profile.
fn wire_request_profile(profile: &AdditionalPermissionProfile) -> WireRequestProfile {
    let mut wire = WireRequestProfile::new();
    if let Some(network) = profile.network() {
        wire = wire.with_network(wire_network(network));
    }
    if let Some(file_system) = profile.file_system() {
        wire = wire.with_file_system(wire_file_system(file_system));
    }
    wire
}

/// The permissions a command runs with, as its approval request carries them: Codex's conversion
/// into its v2 `AdditionalPermissionProfile`.
pub(crate) fn wire_additional_profile(
    profile: &AdditionalPermissionProfile,
) -> WireAdditionalProfile {
    let mut wire = WireAdditionalProfile::new();
    if let Some(network) = profile.network() {
        wire = wire.with_network(wire_network(network));
    }
    if let Some(file_system) = profile.file_system() {
        wire = wire.with_file_system(wire_file_system(file_system));
    }
    wire
}

fn wire_network(network: &NetworkPermissions) -> AdditionalNetworkPermissions {
    match network.enabled() {
        Some(enabled) => AdditionalNetworkPermissions::new().with_enabled(enabled),
        None => AdditionalNetworkPermissions::new(),
    }
}

/// Codex's `From<FileSystemPermissions> for AdditionalFileSystemPermissions`: the entries always,
/// and the `read` / `write` lists too when every entry reads or writes a path.
fn wire_file_system(file_system: &FileSystemPermissions) -> AdditionalFileSystemPermissions {
    let entries: Vec<WireEntry> = file_system.entries().iter().map(wire_entry).collect();
    let mut read = Vec::new();
    let mut write = Vec::new();
    let legacy = file_system.glob_scan_max_depth().is_none()
        && file_system.entries().iter().all(|entry| {
            let FileSystemPath::Path { path } = entry.path() else {
                return false;
            };
            let path = path.to_string_lossy().into_owned();
            match entry.access() {
                FileSystemAccessMode::Read => read.push(path),
                FileSystemAccessMode::Write => write.push(path),
                _ => return false,
            }
            true
        });
    let mut wire = AdditionalFileSystemPermissions::new().with_entries(entries);
    if legacy && !read.is_empty() {
        wire = wire.with_read(read);
    }
    if legacy && !write.is_empty() {
        wire = wire.with_write(write);
    }
    if let Some(depth) = file_system.glob_scan_max_depth() {
        wire = wire.with_glob_scan_max_depth(depth);
    }
    wire
}

fn wire_entry(entry: &FileSystemSandboxEntry) -> WireEntry {
    let path = match entry.path() {
        FileSystemPath::Path { path } => WirePath::path(path.to_string_lossy()),
        FileSystemPath::GlobPattern { pattern } => WirePath::glob_pattern(pattern.clone()),
        FileSystemPath::Special { value } => WirePath::special(match value {
            FileSystemSpecialPath::Root => WireSpecial::Root,
            FileSystemSpecialPath::Minimal => WireSpecial::Minimal,
            FileSystemSpecialPath::ProjectRoots { subpath } => {
                WireSpecial::project_roots(subpath.clone())
            }
            FileSystemSpecialPath::Tmpdir => WireSpecial::Tmpdir,
            FileSystemSpecialPath::SlashTmp => WireSpecial::SlashTmp,
            FileSystemSpecialPath::Unknown { path, subpath } => {
                WireSpecial::unknown(path.clone(), subpath.clone())
            }
        }),
    };
    WireEntry::new(
        path,
        match entry.access() {
            FileSystemAccessMode::Read => WireAccess::Read,
            FileSystemAccessMode::Write => WireAccess::Write,
            _ => WireAccess::Deny,
        },
    )
}

/// The client's answer as the session reads it: Codex's conversion of its v2 response.
fn core_response(response: &PermissionsRequestApprovalResponse) -> RequestPermissionsResponse {
    RequestPermissionsResponse {
        permissions: core_granted_profile(response.permissions()).into(),
        scope: match response.scope() {
            WireScope::Session => PermissionGrantScope::Session,
            _ => PermissionGrantScope::Turn,
        },
        strict_auto_review: response.strict_auto_review().unwrap_or(false),
    }
}

/// A granted profile with its paths as local paths. The app server has already refused a grant
/// whose paths are not absolute; one that still cannot be read grants nothing.
fn core_granted_profile(granted: &GrantedPermissionProfile) -> AdditionalPermissionProfile {
    let mut profile = AdditionalPermissionProfile::new();
    if let Some(network) = granted.network() {
        profile = profile.with_network(NetworkPermissions::new(network.enabled()));
    }
    if let Some(file_system) = granted.file_system() {
        match core_file_system(file_system) {
            Some(file_system) => profile = profile.with_file_system(file_system),
            None => return AdditionalPermissionProfile::new(),
        }
    }
    profile
}

/// Codex's `TryFrom<AdditionalFileSystemPermissions> for FileSystemPermissions`: the entries when
/// there are any, the `read` / `write` lists otherwise.
fn core_file_system(wire: &AdditionalFileSystemPermissions) -> Option<FileSystemPermissions> {
    let permissions = if let Some(entries) = wire.entries() {
        FileSystemPermissions::new(entries.iter().map(core_entry).collect::<Option<Vec<_>>>()?)
    } else {
        FileSystemPermissions::from_read_write_roots(
            local_paths(wire.read())?,
            local_paths(wire.write())?,
        )
    };
    Some(permissions.with_glob_scan_max_depth(wire.glob_scan_max_depth()))
}

fn core_entry(entry: &WireEntry) -> Option<FileSystemSandboxEntry> {
    let path = match entry.path() {
        WirePath::Path { path, .. } => FileSystemPath::Path {
            path: local_path(path)?,
        },
        WirePath::GlobPattern { pattern, .. } => FileSystemPath::glob(pattern.clone()),
        WirePath::Special { value, .. } => FileSystemPath::special(match value {
            WireSpecial::Root => FileSystemSpecialPath::Root,
            WireSpecial::Minimal => FileSystemSpecialPath::Minimal,
            WireSpecial::ProjectRoots { subpath, .. } => {
                FileSystemSpecialPath::project_roots(subpath.clone())
            }
            WireSpecial::Tmpdir => FileSystemSpecialPath::Tmpdir,
            WireSpecial::SlashTmp => FileSystemSpecialPath::SlashTmp,
            WireSpecial::Unknown { path, subpath, .. } => FileSystemSpecialPath::Unknown {
                path: path.clone(),
                subpath: subpath.clone(),
            },
            _ => return None,
        }),
        _ => return None,
    };
    let access = match entry.access() {
        WireAccess::Read => FileSystemAccessMode::Read,
        WireAccess::Write => FileSystemAccessMode::Write,
        _ => FileSystemAccessMode::Deny,
    };
    Some(FileSystemSandboxEntry::new(path, access))
}

/// A granted path list as local paths: `Some(None)` for no list, `None` if a path cannot be read.
#[allow(clippy::option_option)] // Absent and unreadable lists differ, as Codex's `io::Result<Option<_>>` keeps them.
fn local_paths(list: Option<&[String]>) -> Option<Option<Vec<PathBuf>>> {
    match list {
        None => Some(None),
        Some(paths) => paths
            .iter()
            .map(|path| local_path(path))
            .collect::<Option<_>>()
            .map(Some),
    }
}

/// A granted path as a local path: absolute once a leading `~` names the home directory.
fn local_path(raw: &str) -> Option<PathBuf> {
    let home_relative = raw
        .strip_prefix("~/")
        .or_else(|| (raw == "~").then_some(""));
    match home_relative {
        Some(rest) => path::join(&std::env::home_dir()?, rest.trim_start_matches('/')),
        None => path::normalize(Path::new(raw)),
    }
}
