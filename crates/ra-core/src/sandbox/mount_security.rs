//! Mount credentials: where they may be exposed, and keeping them out of durable state.
//!
//! A mount carries authority — keys, tokens, identity selectors, paths to credential files — and
//! two things can go wrong with it. It can be handed to a helper running *inside* a sandbox the
//! model controls, where whatever is in the container can read it; and it can be written into a
//! persisted session state, where it outlives the process that was trusted with it. This module
//! answers both, in three parts:
//!
//! - **classification** — [`configured_authority_fields`] names the authority a mount actually
//!   carries, which is not the same question as which fields look secret;
//! - **the boundary** — [`validate_manifest_mount_credential_boundaries`] runs before a sandbox or a
//!   helper has side effects, and refuses credentials that would reach a model-controlled container
//!   unless the application acknowledged that exact path;
//! - **durable state** — the sanitizers strip authority from a rendered manifest or session state,
//!   and [`rebind_manifest_mount_authority`] puts it back only from a manifest the host trusts now,
//!   and only when the credential-free shape of the two matches exactly.
//!
//! # A closed table, not a class check
//!
//! The reference decides which strategies are safe execution boundaries from a closed table keyed
//! by class provenance, so a custom subclass cannot promote itself into a trusted boundary. Here
//! the modelled strategies are enum variants, so provenance is structural: an
//! [`MountStrategy::Extension`] is a custom strategy, a [`MountProvider::Extension`] is a custom
//! mount, and an entry that merely *names* a built-in mount type without being one is a forgery.
//! The reference's table also lists strategies owned by hosted backends (Modal, Daytona, E2B and
//! others); none of those backends is ported, so their rows are absent and a payload naming one is
//! refused as unknown. They join the table with the backend that owns them.
//!
//! # What is not here
//!
//! The reference wraps its lifecycle calls in decorators that, when a call touched mount
//! authority, replace the failure and clear the interpreter frames that held it. That is about
//! Python tracebacks retaining local variables; a Rust error carries only what was put into it,
//! and nothing raised here puts a configuration value into a message or context. The one
//! observable part of that machinery — a persisted payload that fails to parse must not be quoted
//! back — is kept, in [`crate::sandbox::SandboxSessionState::parse`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map as JsonMap, Value};

use super::entries::mounts::{
    AZURE_BLOB_MOUNT_TYPE, BOX_MOUNT_TYPE, BUILTIN_MOUNT_TYPES, DOCKER_VOLUME_STRATEGY_TYPE,
    GCS_MOUNT_TYPE, IN_CONTAINER_STRATEGY_TYPE, Mount, MountPattern, MountProvider, MountStrategy,
    R2_MOUNT_TYPE, S3_FILES_MOUNT_TYPE, S3_MOUNT_TYPE, authority_fields, authority_file_fields,
    credential_set, url_carries_inline_authority, url_fields,
};
use super::entries::{
    DIR_ENTRY_TYPE, Entry, EntryContent, FILE_ENTRY_TYPE, GIT_REPO_ENTRY_TYPE,
    LOCAL_DIR_ENTRY_TYPE, LOCAL_FILE_ENTRY_TYPE,
};
use super::error::{ErrorCode, OpName, SandboxError};
use super::manifest::{DEFAULT_MANIFEST_ROOT, Manifest, MountCredentialAuthority};
use super::registry::TypeRegistry;
use super::workspace_paths::PosixPath;

/// The state field saying mount authority was stripped and must be rebound before resume.
///
/// The key is the reference's, spelled exactly, so a state written by either implementation is
/// read the same way by the other.
pub const REDACTED_MOUNT_AUTHORITY_KEY: &str = "__openai_agents_redacted_mount_authority";

/// A state field older writers used to claim a manifest carried no mount authority.
///
/// It is dropped on read and never trusted: whether a manifest carries authority is decided by
/// looking at it, not by what the payload says about itself.
pub const CREDENTIALLESS_MOUNT_AUTHORITY_KEY: &str =
    "__openai_agents_credentialless_mount_authority";

const CUSTOM_MOUNT_MESSAGE: &str =
    "custom mount implementations are not supported at the sandbox credential boundary";
const CUSTOM_STRATEGY_MESSAGE: &str =
    "custom mount strategies are not supported at the sandbox credential boundary";
const SERIALIZATION_MESSAGE: &str =
    "sandbox session state containing mount authority could not be serialized";
const CREDENTIAL_FILE_MESSAGE: &str = "credential files stored in the manifest are not supported \
                                       for cloud mounts; configure credentials outside the \
                                       sandbox manifest";

// --- classification ----------------------------------------------------------------------------

/// The fields whose values are written into an rclone configuration line.
///
/// Every one of them must stay on a single line: a value carrying a line break would add a line of
/// its own choosing to the configuration — another key, another remote — and the helper would read
/// it as if the host had written it.
#[must_use]
pub fn rclone_config_value_fields(mount_type: &str) -> &'static [&'static str] {
    match mount_type {
        AZURE_BLOB_MOUNT_TYPE => &["account", "endpoint", "identity_client_id", "account_key"],
        BOX_MOUNT_TYPE => &[
            "client_id",
            "client_secret",
            "access_token",
            "token",
            "box_config_file",
            "config_credentials",
            "root_folder_id",
            "impersonate",
            "owned_by",
        ],
        GCS_MOUNT_TYPE => &[
            "access_id",
            "secret_access_key",
            "region",
            "endpoint_url",
            "service_account_file",
            "service_account_credentials",
            "access_token",
        ],
        R2_MOUNT_TYPE => &[
            "account_id",
            "access_key_id",
            "secret_access_key",
            "custom_domain",
        ],
        S3_MOUNT_TYPE => &[
            "s3_provider",
            "endpoint_url",
            "region",
            "access_key_id",
            "secret_access_key",
            "session_token",
        ],
        _ => &[],
    }
}

/// Where a strategy executes, which decides whether its credentials meet the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// A helper inside the sandbox attaches the mount, so the credentials are in the container.
    InContainer,
    /// Something outside the sandbox attaches it, and the credentials stay there.
    External,
    /// A strategy nobody vouched for, which could be either.
    Unknown,
}

/// Where a strategy executes, and which backend owns it when one does.
const fn classify_strategy(strategy: &MountStrategy) -> (Boundary, Option<&'static str>) {
    match strategy {
        MountStrategy::InContainer { .. } => (Boundary::InContainer, None),
        MountStrategy::DockerVolume { .. } => (Boundary::External, Some("docker")),
        MountStrategy::Extension(_) => (Boundary::Unknown, None),
    }
}

/// The fields a strategy keeps in durable state, or `None` for a strategy nobody vouched for.
fn serialized_strategy_fields(strategy_type: &str) -> Option<&'static [&'static str]> {
    match strategy_type {
        IN_CONTAINER_STRATEGY_TYPE => Some(&["type", "pattern"]),
        DOCKER_VOLUME_STRATEGY_TYPE => Some(&["type", "driver", "driver_options"]),
        _ => None,
    }
}

/// Strategy fields that are live authority as a whole.
///
/// Third-party driver options cannot be classified by option name, so the complete field is
/// authority: allowed only where a trusted executor runs it, and removed from durable state as a
/// unit.
fn opaque_strategy_authority_fields(strategy_type: &str) -> &'static [&'static str] {
    match strategy_type {
        DOCKER_VOLUME_STRATEGY_TYPE => &["driver_options"],
        _ => &[],
    }
}

/// The fields a pattern keeps in durable state, or `None` for a tool outside the closed set.
fn serialized_pattern_fields(pattern_type: &str) -> Option<&'static [&'static str]> {
    match pattern_type {
        "fuse" => Some(&[
            "type",
            "allow_other",
            "log_type",
            "log_level",
            "cache_type",
            "cache_path",
            "cache_size_mb",
            "block_cache_block_size_mb",
            "block_cache_disk_timeout_sec",
            "file_cache_timeout_sec",
            "file_cache_max_size_mb",
            "attr_cache_timeout_sec",
            "entry_cache_timeout_sec",
            "negative_entry_cache_timeout_sec",
        ]),
        "mountpoint" | "s3files" => Some(&["type", "options"]),
        "rclone" => Some(&[
            "type",
            "mode",
            "remote_name",
            "extra_args",
            "nfs_addr",
            "nfs_mount_options",
            "config_file_path",
        ]),
        _ => None,
    }
}

/// The fields a pattern's nested `options` keeps in durable state.
fn serialized_pattern_option_fields(pattern_type: &str) -> &'static [&'static str] {
    match pattern_type {
        "mountpoint" => &["prefix", "region", "endpoint_url"],
        "s3files" => &["mount_target_ip", "access_point", "region", "extra_options"],
        _ => &[],
    }
}

/// The fields every entry carries.
const ENTRY_FIELDS: [&str; 6] = [
    "type",
    "description",
    "ephemeral",
    "group",
    "is_dir",
    "permissions",
];

/// The fields every mount carries beside its provider's.
const MOUNT_FIELDS: [&str; 3] = ["mount_path", "read_only", "mount_strategy"];

/// A built-in provider's own fields.
fn provider_fields(mount_type: &str) -> &'static [&'static str] {
    match mount_type {
        S3_MOUNT_TYPE => &[
            "bucket",
            "access_key_id",
            "secret_access_key",
            "session_token",
            "prefix",
            "region",
            "endpoint_url",
            "s3_provider",
        ],
        GCS_MOUNT_TYPE => &[
            "bucket",
            "access_id",
            "secret_access_key",
            "prefix",
            "region",
            "endpoint_url",
            "service_account_file",
            "service_account_credentials",
            "access_token",
        ],
        AZURE_BLOB_MOUNT_TYPE => &[
            "account",
            "container",
            "endpoint",
            "identity_client_id",
            "account_key",
        ],
        BOX_MOUNT_TYPE => &[
            "path",
            "client_id",
            "client_secret",
            "access_token",
            "token",
            "box_config_file",
            "config_credentials",
            "box_sub_type",
            "root_folder_id",
            "impersonate",
            "owned_by",
        ],
        R2_MOUNT_TYPE => &[
            "bucket",
            "account_id",
            "access_key_id",
            "secret_access_key",
            "custom_domain",
        ],
        S3_FILES_MOUNT_TYPE => &[
            "file_system_id",
            "subpath",
            "mount_target_ip",
            "access_point",
            "region",
            "extra_options",
        ],
        _ => &[],
    }
}

/// Whether a field belongs in the durable form of a built-in mount.
fn is_canonical_mount_field(mount_type: &str, field: &str) -> bool {
    ENTRY_FIELDS.contains(&field)
        || MOUNT_FIELDS.contains(&field)
        || provider_fields(mount_type).contains(&field)
}

/// What one supported strategy, provider and tool combination may expose inside the container.
struct Capability {
    strategy_type: &'static str,
    mount_type: &'static str,
    pattern_type: Option<&'static str>,
    /// Credentials that reach only this mount's storage.
    mount_scoped: &'static [&'static str],
    /// Credentials that reach past it: identities, credential files, whole configurations.
    broad: &'static [&'static str],
    /// Whether the helper discovers ambient credentials by itself, with no field to point at.
    implicit_broad: bool,
    /// Fields of which at least one must hold a usable value, because the helper cannot prompt.
    required_any: &'static [&'static str],
}

const RCLONE_CONFIG_FILE: &str = "mount_strategy.pattern.config_file_path";

/// The combinations whose credential exposure can be acknowledged at all.
///
/// Limited to the strategies this crate models. The reference also lists combinations owned by
/// hosted backends that are not ported; they join with those backends.
const CAPABILITIES: [Capability; 9] = [
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: S3_MOUNT_TYPE,
        pattern_type: Some("rclone"),
        mount_scoped: &["access_key_id", "secret_access_key", "session_token"],
        broad: &[RCLONE_CONFIG_FILE],
        implicit_broad: false,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: R2_MOUNT_TYPE,
        pattern_type: Some("rclone"),
        mount_scoped: &["access_key_id", "secret_access_key"],
        broad: &[RCLONE_CONFIG_FILE],
        implicit_broad: false,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: GCS_MOUNT_TYPE,
        pattern_type: Some("rclone"),
        mount_scoped: &[
            "access_id",
            "secret_access_key",
            "service_account_credentials",
            "access_token",
        ],
        broad: &["service_account_file", RCLONE_CONFIG_FILE],
        implicit_broad: false,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: AZURE_BLOB_MOUNT_TYPE,
        pattern_type: Some("rclone"),
        mount_scoped: &["account_key"],
        broad: &["identity_client_id", RCLONE_CONFIG_FILE],
        implicit_broad: false,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: BOX_MOUNT_TYPE,
        pattern_type: Some("rclone"),
        mount_scoped: &[
            "client_secret",
            "access_token",
            "token",
            "config_credentials",
        ],
        broad: &["box_config_file", RCLONE_CONFIG_FILE],
        implicit_broad: false,
        required_any: &[
            "access_token",
            "token",
            "config_credentials",
            "box_config_file",
        ],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: S3_MOUNT_TYPE,
        pattern_type: Some("mountpoint"),
        mount_scoped: &["access_key_id", "secret_access_key", "session_token"],
        broad: &[],
        implicit_broad: false,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: GCS_MOUNT_TYPE,
        pattern_type: Some("mountpoint"),
        mount_scoped: &["access_id", "secret_access_key"],
        broad: &[],
        implicit_broad: false,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: AZURE_BLOB_MOUNT_TYPE,
        pattern_type: Some("fuse"),
        mount_scoped: &["account_key"],
        broad: &["identity_client_id"],
        implicit_broad: true,
        required_any: &[],
    },
    Capability {
        strategy_type: IN_CONTAINER_STRATEGY_TYPE,
        mount_type: S3_FILES_MOUNT_TYPE,
        pattern_type: Some("s3files"),
        mount_scoped: &[],
        broad: &[
            "extra_options",
            "mount_strategy.pattern.options.extra_options",
        ],
        implicit_broad: true,
        required_any: &[],
    },
];

/// The capability of one combination, when it has one.
fn capability_for(
    mount_type: &str,
    strategy_type: &str,
    pattern_type: Option<&str>,
) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|capability| {
        capability.strategy_type == strategy_type
            && capability.mount_type == mount_type
            && capability.pattern_type == pattern_type
    })
}

/// The tool an in-container strategy uses, by name.
const fn pattern_type(strategy: &MountStrategy) -> Option<&'static str> {
    match strategy {
        MountStrategy::InContainer { pattern } => Some(pattern.as_str()),
        _ => None,
    }
}

/// The capability of a mount as it is declared.
fn mount_capability(mount: &Mount, strategy: &MountStrategy) -> Option<&'static Capability> {
    capability_for(
        mount.type_name(),
        strategy.type_name(),
        pattern_type(strategy),
    )
}

/// Which rclone configuration flags are known to carry no authority, with and without a value.
const RCLONE_SAFE_FLAG_ARGS: [&str; 1] = ["allow-other"];
const RCLONE_SAFE_VALUE_ARGS: [&str; 3] = ["buffer-size", "gid", "uid"];

/// What a list of extra rclone arguments amounts to.
struct RcloneArgs {
    /// Whether every argument is one of the few known to carry no authority.
    safe: bool,
    /// Every configuration file the arguments point rclone at.
    config_paths: Vec<String>,
    /// Whether a `--config` was given without a usable path.
    invalid_config_path: bool,
}

/// Classifies extra rclone arguments, keeping the exact configuration paths they name.
///
/// The known-safe subset is small on purpose: an argument outside it can switch rclone to another
/// credential source (`--s3-env-auth`, a profile, a header), so anything unrecognized makes the
/// whole list authority.
fn rclone_extra_args(args: &[Value]) -> RcloneArgs {
    let mut analysis = RcloneArgs {
        safe: true,
        config_paths: Vec::new(),
        invalid_config_path: false,
    };
    let next_value = |index: usize| match args.get(index + 1) {
        Some(Value::String(next)) if !next.starts_with('-') => Some(next.clone()),
        _ => None,
    };
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].as_str().filter(|arg| arg.starts_with('-')) else {
            analysis.safe = false;
            index += 1;
            continue;
        };
        let (option, value) = match arg.trim_start_matches('-').split_once('=') {
            Some((option, value)) => (option, Some(value)),
            None => (arg.trim_start_matches('-'), None),
        };
        let normalized = option.to_lowercase().replace('_', "-");
        if normalized == "config" {
            analysis.safe = false;
            match value {
                Some(path) if !path.is_empty() => {
                    analysis.config_paths.push(path.to_owned());
                    index += 1;
                }
                None if index + 1 < args.len() => {
                    match next_value(index) {
                        Some(path) => analysis.config_paths.push(path),
                        None => analysis.invalid_config_path = true,
                    }
                    index += 2;
                }
                _ => {
                    analysis.invalid_config_path = true;
                    index += 1;
                }
            }
            continue;
        }
        if RCLONE_SAFE_FLAG_ARGS.contains(&normalized.as_str()) && value.is_none() {
            index += 1;
            continue;
        }
        if RCLONE_SAFE_VALUE_ARGS.contains(&normalized.as_str()) {
            if value.is_some_and(|value| !value.is_empty()) {
                index += 1;
                continue;
            }
            if value.is_none() && index + 1 < args.len() && next_value(index).is_some() {
                index += 2;
                continue;
            }
        }
        analysis.safe = false;
        index += 1;
    }
    analysis
}

/// Whether an rclone remote name is an ordinary name rather than an on-the-fly remote.
///
/// A name starting with `:` is a whole backend definition inline — `:s3,access_key_id=…` — so
/// anything outside a plain name counts as authority.
fn rclone_remote_name_is_safe(remote_name: Option<&Value>) -> bool {
    let name = match remote_name {
        None | Some(Value::Null) => return true,
        Some(Value::String(name)) if name.is_empty() => return true,
        Some(Value::String(name)) => name,
        Some(_) => return false,
    };
    let mut chars = name.chars();
    name.trim() == name
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ' ' | '-'))
}

/// Whether a raw value carries inline authority when read as a URL.
///
/// A value that is not a string at all counts: nothing can vouch for what it holds.
fn value_carries_inline_authority(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(url)) => url_carries_inline_authority(url),
        Some(_) => true,
    }
}

/// Whether a raw value would break a configuration line.
fn contains_line_break(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains(['\r', '\n']))
}

/// Whether a raw value is set to something, by the reference's reading of truthiness.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// Whether a raw value is present at all.
const fn is_set(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null))
}

/// Whether a raw value is a string with something other than whitespace in it.
fn is_usable_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

/// The configured fields of a built-in mount whose values would break an rclone configuration line.
fn rclone_line_fields(fields: &JsonMap<String, Value>, mount_type: &str) -> Vec<&'static str> {
    rclone_config_value_fields(mount_type)
        .iter()
        .copied()
        .filter(|field| contains_line_break(fields.get(*field)))
        .collect()
}

/// Which of this mount's fields are authority, and set.
///
/// The question a host asks before letting a manifest cross a trust boundary. It is broader than
/// the provider's credential fields: an endpoint URL with credentials in it, a configuration value
/// that would inject a line, an rclone remote defined inline or pointed at a configuration file,
/// arguments that switch rclone to another credential source, and options handed to a helper
/// nobody can classify all count. Strategy and pattern fields are named by their path from the
/// mount (`mount_strategy.driver_options`, `mount_strategy.pattern.extra_args`).
#[must_use]
pub fn configured_authority_fields(mount: &Mount) -> BTreeSet<String> {
    let mount_type = mount.type_name();
    let fields = mount.to_fields();
    let mut configured = BTreeSet::new();

    for field in authority_fields(mount_type) {
        if is_set(fields.get(*field)) {
            configured.insert((*field).to_owned());
        }
    }
    for field in url_fields(mount_type) {
        if value_carries_inline_authority(fields.get(*field)) {
            configured.insert((*field).to_owned());
        }
    }
    configured.extend(
        rclone_line_fields(&fields, mount_type)
            .into_iter()
            .map(str::to_owned),
    );
    // A custom mount's own configuration cannot be classified by name, so all of it counts.
    if let MountProvider::Extension(payload) = mount.provider() {
        configured.extend(
            payload
                .fields()
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(name, _)| name.clone()),
        );
    }

    let strategy = mount.strategy();
    match strategy {
        MountStrategy::Extension(payload) => configured.extend(
            payload
                .fields()
                .iter()
                .filter(|(name, value)| name.as_str() != "type" && !value.is_null())
                .map(|(name, _)| format!("mount_strategy.{name}")),
        ),
        MountStrategy::DockerVolume { driver_options, .. } if !driver_options.is_empty() => {
            for field in opaque_strategy_authority_fields(DOCKER_VOLUME_STRATEGY_TYPE) {
                configured.insert(format!("mount_strategy.{field}"));
            }
        }
        _ => {}
    }
    if let MountStrategy::InContainer { pattern } = strategy {
        match pattern {
            MountPattern::Rclone(options) => {
                let remote_name = options.remote_name.clone().map(Value::from);
                if !rclone_remote_name_is_safe(remote_name.as_ref()) {
                    configured.insert("mount_strategy.pattern.remote_name".to_owned());
                }
                if options.config_file_path.is_some() {
                    configured.insert(RCLONE_CONFIG_FILE.to_owned());
                }
                let args: Vec<Value> = options
                    .extra_args
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect();
                if !rclone_extra_args(&args).safe {
                    configured.insert("mount_strategy.pattern.extra_args".to_owned());
                }
            }
            MountPattern::Mountpoint(options) => {
                if options
                    .endpoint_url
                    .as_deref()
                    .is_some_and(url_carries_inline_authority)
                {
                    configured.insert("mount_strategy.pattern.options.endpoint_url".to_owned());
                }
            }
            MountPattern::S3Files(options) if !options.extra_options.is_empty() => {
                configured.insert("mount_strategy.pattern.options.extra_options".to_owned());
            }
            _ => {}
        }
    }
    if mount_type == S3_FILES_MOUNT_TYPE && is_truthy(fields.get("extra_options")) {
        configured.insert("extra_options".to_owned());
    }
    configured
}

/// Whether a mount carries authority, or is of a kind that could hide some.
///
/// A custom mount or strategy is not read at all — its configuration is whatever it says it is —
/// and a helper that discovers ambient credentials by itself carries authority with no field set.
fn mount_has_or_may_hide_authority(mount: &Mount) -> bool {
    if !mount.provider().is_modelled() {
        return true;
    }
    let strategy = mount.strategy();
    if classify_strategy(strategy).0 == Boundary::Unknown {
        return true;
    }
    if mount_capability(mount, strategy).is_some_and(|capability| capability.implicit_broad) {
        return true;
    }
    !configured_authority_fields(mount).is_empty()
}

/// Whether a mount carries authority, or is of a kind that could hide some.
///
/// The question a mount lifecycle boundary asks before letting a failure out as it is: a custom
/// mount or strategy is assumed to, since this crate cannot read its configuration.
#[must_use]
pub fn mount_has_configured_authority(mount: &Mount) -> bool {
    mount_has_or_may_hide_authority(mount)
}

/// The failure a mount lifecycle boundary lets out in place of one that may carry authority.
///
/// Keeps the code, the operation and the retryability, which are what a caller branches on, and
/// drops the context and the cause, which are where a command line, its output or a credential
/// file's path would be. The message is kept only when it was marked safe; otherwise it is one of
/// two fixed sentences. The replacement is itself marked, so a further boundary leaves it alone.
#[must_use]
pub fn replace_protected_mount_error(error: &SandboxError) -> SandboxError {
    let message = if error.error_code() == ErrorCode::MountConfigInvalid {
        if error.has_safe_redacted_message() {
            error.message()
        } else {
            "sandbox mount configuration is invalid"
        }
    } else {
        "sandbox operation failed while using a protected mount configuration"
    };
    let replacement = SandboxError::new(error.error_code(), error.op(), message)
        .with_retryable(error.retryable());
    if error.has_safe_redacted_message() {
        replacement.with_safe_redacted_message()
    } else {
        replacement.with_data_redacted()
    }
}

/// Whether anything in a manifest carries mount authority, or could.
///
/// Decides whether a failure while handling the manifest may quote it: when this is true, errors
/// raised by persistence say that something failed without saying what it was working on.
#[must_use]
pub fn manifest_has_configured_mount_authority(manifest: &Manifest) -> bool {
    fn walk(entries: &BTreeMap<String, Entry>) -> bool {
        entries.values().any(|entry| match entry.content() {
            EntryContent::Mount(mount) => mount_has_or_may_hide_authority(mount),
            EntryContent::Dir { children } => walk(children),
            EntryContent::Extension(payload) => is_forged_mount(payload.type_name()),
            _ => false,
        })
    }
    walk(&manifest.entries)
}

// --- provenance ----------------------------------------------------------------------------------

/// Why a mount cannot cross the credential boundary at all, whatever it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provenance {
    /// The mount is not one of the built-in kinds.
    CustomMount,
    /// The strategy is not one of the built-in kinds.
    CustomStrategy,
}

impl Provenance {
    /// The refusal, worded as the reference words it.
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::CustomMount => CUSTOM_MOUNT_MESSAGE,
            Self::CustomStrategy => CUSTOM_STRATEGY_MESSAGE,
        }
    }

    fn into_error(self) -> SandboxError {
        SandboxError::mount_config(self.message())
    }
}

/// Whether an entry that is not a mount claims a built-in mount type.
///
/// The counterpart of the reference refusing a subclass of a canonical mount: something that says
/// it is an S3 mount without being one would have its fields read by rules written for a shape it
/// does not have.
fn is_forged_mount(type_name: &str) -> bool {
    BUILTIN_MOUNT_TYPES.contains(&type_name)
}

/// Checks that a mount and the strategy about to attach it are kinds this crate vouches for.
const fn mount_provenance(mount: &Mount, strategy: &MountStrategy) -> Result<(), Provenance> {
    if !mount.provider().is_modelled() {
        return Err(Provenance::CustomMount);
    }
    if matches!(classify_strategy(strategy).0, Boundary::Unknown) {
        return Err(Provenance::CustomStrategy);
    }
    Ok(())
}

/// Checks every mount in a manifest before any of their behavior or configuration is used.
///
/// Walks the declared tree rather than validated paths: provenance does not depend on where an
/// entry lands, and a path problem is reported by whatever resolves paths next.
pub(crate) fn manifest_mount_provenance(manifest: &Manifest) -> Result<(), Provenance> {
    fn walk(entries: &BTreeMap<String, Entry>) -> Result<(), Provenance> {
        for entry in entries.values() {
            match entry.content() {
                EntryContent::Mount(mount) => mount_provenance(mount, mount.strategy())?,
                EntryContent::Dir { children } => walk(children)?,
                EntryContent::Extension(payload) if is_forged_mount(payload.type_name()) => {
                    return Err(Provenance::CustomMount);
                }
                _ => {}
            }
        }
        Ok(())
    }
    walk(&manifest.entries)
}

/// Refuses a manifest holding a mount or strategy this crate does not vouch for.
///
/// # Errors
///
/// Returns [`ErrorCode::MountConfigInvalid`] naming which of the two it was.
///
/// `Internal`, as the reference's `_validate_manifest_mount_provenance` is, and hidden from the
/// documentation. It shipped public, so it becomes crate-private only in a major version.
#[doc(hidden)]
pub fn validate_manifest_mount_provenance(manifest: &Manifest) -> Result<(), SandboxError> {
    manifest_mount_provenance(manifest).map_err(validation_error)
}

/// A provenance refusal, marked as a validation failure whose message quotes no values.
fn validation_error(provenance: Provenance) -> SandboxError {
    provenance.into_error().with_safe_redacted_message()
}

// --- the boundary --------------------------------------------------------------------------------

/// Reduces a path to the absolute, normalized form credential paths are compared in.
///
/// A relative path is measured from the root. `..` is resolved and cannot climb above `/`, which
/// matches how the reference compares a credential path with the entries around it.
fn absolute_manifest_path(root: &str, value: &str) -> String {
    let joined = if value.starts_with('/') {
        value.to_owned()
    } else {
        format!("{root}/{value}")
    };
    let mut normalized: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                normalized.pop();
            }
            part => normalized.push(part),
        }
    }
    format!("/{}", normalized.join("/"))
}

/// Whether `ancestor` is a strict ancestor of `path`, both already absolute and normalized.
fn is_strict_ancestor(ancestor: &str, path: &str) -> bool {
    if ancestor == "/" {
        return path != "/";
    }
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether an entry can place content at a credential path.
///
/// Anything declared at the path or above it counts, except a directory that is only structure: an
/// empty directory, or a host directory with no source. A file with inline content there *is* the
/// credential, carried in the manifest.
fn manifest_materializes_path(manifest: &Manifest, target: &str) -> Result<bool, SandboxError> {
    let target = absolute_manifest_path(&manifest.root, target);
    for (path, entry) in manifest.iter_entries()? {
        let entry_path = absolute_manifest_path(&manifest.root, path.as_str());
        if entry_path != target && !is_strict_ancestor(&entry_path, &target) {
            continue;
        }
        let structural = matches!(
            entry.content(),
            EntryContent::Dir { .. } | EntryContent::LocalDir { src: None }
        );
        if !structural {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Renders a list of field names for an error's context.
fn names<'a>(fields: impl IntoIterator<Item = &'a str>) -> Value {
    Value::from(fields.into_iter().map(str::to_owned).collect::<Vec<_>>())
}

/// Checks one mount, attached at one path by one strategy.
#[allow(clippy::too_many_lines)] // One linear sequence of checks, in the reference's order.
fn mount_boundary_error(
    manifest: &Manifest,
    mount: &Mount,
    mount_path: &str,
    provider_backend_id: Option<&str>,
) -> Result<(), SandboxError> {
    let mount_type = mount.type_name();
    let strategy = mount.strategy();
    let (boundary, strategy_backend_id) = classify_strategy(strategy);
    let executes_in_container = boundary == Boundary::InContainer;

    if let (Some(provider), Some(owner)) = (provider_backend_id, strategy_backend_id)
        && provider != owner
    {
        let message = if strategy.type_name() == DOCKER_VOLUME_STRATEGY_TYPE {
            "docker-volume mounts are not supported by this sandbox backend"
        } else {
            "mount strategy is not supported by this sandbox backend"
        };
        return Err(SandboxError::mount_config(message)
            .with_context("mount_type", mount_type)
            .with_context("strategy_type", strategy.type_name())
            .with_context("sandbox_backend", provider));
    }

    let fields = mount.to_fields();
    for field in authority_file_fields(mount_type) {
        if let Some(Value::String(path)) = fields.get(*field)
            && !path.is_empty()
            && manifest_materializes_path(manifest, path)?
        {
            return Err(SandboxError::mount_config(CREDENTIAL_FILE_MESSAGE)
                .with_context("mount_type", mount_type)
                .with_context("credential_field", *field));
        }
    }
    if let MountStrategy::InContainer {
        pattern: MountPattern::Rclone(options),
    } = strategy
        && let Some(config_file_path) = &options.config_file_path
        && manifest_materializes_path(manifest, config_file_path)?
    {
        return Err(SandboxError::mount_config(CREDENTIAL_FILE_MESSAGE)
            .with_context("mount_type", mount_type)
            .with_context("credential_field", RCLONE_CONFIG_FILE));
    }

    let line_fields = rclone_line_fields(&fields, mount_type);
    if executes_in_container
        && matches!(pattern_type(strategy), Some("rclone"))
        && !line_fields.is_empty()
    {
        return Err(SandboxError::mount_config(
            "cloud mount configuration values must not contain line breaks",
        )
        .with_context("mount_type", mount_type)
        .with_context("configuration_fields", names(line_fields)));
    }

    if executes_in_container {
        let incomplete = incomplete_credential_set_fields(&fields, mount_type);
        if !incomplete.is_empty() {
            return Err(SandboxError::mount_config(
                "in-container access credentials require a complete non-empty credential set",
            )
            .with_context("mount_type", mount_type)
            .with_context("credential_fields", names(incomplete)));
        }
        let blank: Vec<&str> = {
            let mut blank: Vec<&str> = authority_fields(mount_type)
                .iter()
                .copied()
                .filter(|field| is_set(fields.get(*field)) && !is_usable_text(fields.get(*field)))
                .collect();
            blank.sort_unstable();
            blank
        };
        if !blank.is_empty() {
            return Err(SandboxError::mount_config(
                "in-container mount authentication values must not be empty or whitespace-only",
            )
            .with_context("mount_type", mount_type)
            .with_context("credential_fields", names(blank)));
        }
    }

    let authority = configured_authority_fields(mount);
    if boundary == Boundary::External {
        return Ok(());
    }

    let scoped_acknowledged = manifest.acknowledges_in_container_mount_credential_exposure(
        mount_path,
        MountCredentialAuthority::MountScoped,
    );
    let broad_acknowledged = manifest.acknowledges_in_container_mount_credential_exposure(
        mount_path,
        MountCredentialAuthority::Broad,
    );
    let capability = mount_capability(mount, strategy);
    let implicit_broad = capability.is_some_and(|capability| capability.implicit_broad);

    if let Some(capability) = capability
        && !capability.required_any.is_empty()
        && !capability
            .required_any
            .iter()
            .any(|field| is_usable_text(fields.get(*field)))
    {
        return Err(SandboxError::mount_config(
            "in-container Box mounts require a non-interactive authentication source; configure \
             a token, access token, or JWT credentials before activation",
        )
        .with_context("mount_type", mount_type));
    }
    if authority.is_empty() && !scoped_acknowledged && !broad_acknowledged && !implicit_broad {
        return Ok(());
    }
    let Some(capability) = capability else {
        return Err(SandboxError::mount_config(
            "credential-bearing in-container mounts require an SDK-supported strategy, mount \
             type, and pattern combination before exposure can be acknowledged; use a \
             credentialless helper or an external/provider-native mount strategy",
        )
        .with_context("mount_type", mount_type)
        .with_context("strategy_type", strategy.type_name())
        .with_context(
            "pattern_type",
            pattern_type(strategy).map_or(Value::Null, Value::from),
        )
        .with_context(
            "credential_fields",
            names(authority.iter().map(String::as_str)),
        ));
    };

    let supported =
        |field: &str| capability.mount_scoped.contains(&field) || capability.broad.contains(&field);
    let unsupported: Vec<&str> = authority
        .iter()
        .map(String::as_str)
        .filter(|field| !supported(field))
        .collect();
    if !unsupported.is_empty() {
        return Err(SandboxError::mount_config(
            "the selected in-container mount capability does not support exposing the configured \
             credential fields; use supported credentials, a credentialless helper, or an \
             external/provider-native mount strategy",
        )
        .with_context("mount_type", mount_type)
        .with_context("strategy_type", strategy.type_name())
        .with_context("credential_fields", names(unsupported)));
    }

    let supports_scoped = !capability.mount_scoped.is_empty();
    let supports_broad = !capability.broad.is_empty() || capability.implicit_broad;
    if scoped_acknowledged && !supports_scoped {
        return Err(SandboxError::mount_config(
            "the selected in-container mount capability does not support mount-scoped credentials",
        )
        .with_context("mount_type", mount_type)
        .with_context("strategy_type", strategy.type_name()));
    }
    if broad_acknowledged && !supports_broad {
        return Err(SandboxError::mount_config(
            "the selected in-container mount capability does not support broad credential \
             authority",
        )
        .with_context("mount_type", mount_type)
        .with_context("strategy_type", strategy.type_name()));
    }

    let scoped_fields: Vec<&str> = authority
        .iter()
        .map(String::as_str)
        .filter(|field| capability.mount_scoped.contains(field))
        .collect();
    let broad_fields: Vec<&str> = authority
        .iter()
        .map(String::as_str)
        .filter(|field| capability.broad.contains(field))
        .collect();
    if !scoped_fields.is_empty() && !scoped_acknowledged {
        return Err(SandboxError::mount_config(
            "mount-scoped credentials cannot be exposed to a helper inside a model-controlled \
             sandbox by default; use a credentialless or external/provider-native strategy, or \
             explicitly acknowledge exposure for this exact path with \
             Manifest::with_in_container_mount_credential_exposure_acknowledged()",
        )
        .with_context("mount_type", mount_type)
        .with_context("credential_fields", names(scoped_fields)));
    }
    if (!broad_fields.is_empty() || implicit_broad) && !broad_acknowledged {
        return Err(SandboxError::mount_config(
            "broad credential authority cannot be exposed to a helper inside a model-controlled \
             sandbox by default; use a credentialless or external/provider-native strategy, or \
             explicitly acknowledge broad exposure for this exact path with \
             Manifest::with_in_container_mount_broad_credential_exposure_acknowledged()",
        )
        .with_context("mount_type", mount_type)
        .with_context("credential_fields", names(broad_fields)));
    }
    Ok(())
}

/// The fields of an incomplete inline credential set, sorted.
///
/// A set is active once any of its fields is set; once active, every field in it must be set, and
/// every required one must hold more than whitespace. A key without its secret would leave the
/// helper falling back to whatever ambient credentials the sandbox has.
fn incomplete_credential_set_fields(
    fields: &JsonMap<String, Value>,
    mount_type: &str,
) -> Vec<&'static str> {
    let Some((configured, required)) = credential_set(mount_type) else {
        return Vec::new();
    };
    if configured.iter().all(|field| !is_set(fields.get(*field))) {
        return Vec::new();
    }
    let mut incomplete: BTreeSet<&'static str> = configured
        .iter()
        .copied()
        .filter(|field| is_set(fields.get(*field)) && !is_usable_text(fields.get(*field)))
        .collect();
    incomplete.extend(
        required
            .iter()
            .copied()
            .filter(|field| !is_usable_text(fields.get(*field))),
    );
    incomplete.into_iter().collect()
}

/// Checks every mount in a manifest before a sandbox or a helper has side effects.
///
/// `provider_backend_id` is the backend that is going to run the manifest; a strategy owned by a
/// different backend is refused. `None` skips that one check, for a caller that does not yet know.
///
/// # Errors
///
/// Returns [`ErrorCode::MountConfigInvalid`] for the first mount that fails, with the offending
/// field *names* in the context — never their values — or the path failure of a manifest whose
/// mounts cannot be resolved.
pub fn validate_manifest_mount_credential_boundaries(
    manifest: &Manifest,
    provider_backend_id: Option<&str>,
) -> Result<(), SandboxError> {
    validate_manifest_mount_provenance(manifest)?;
    for (mount, mount_path) in manifest.mount_targets()? {
        mount_boundary_error(manifest, mount, mount_path.as_str(), provider_backend_id)
            .map_err(SandboxError::with_safe_redacted_message)?;
    }
    Ok(())
}

/// Checks the strategy that is actually about to run inside a sandbox.
///
/// A backend may attach a mount with a strategy other than the declared one, so the check runs
/// again at activation against what will execute. Without a manifest the mount is checked as the
/// only entry of an empty one, at `<root>/mount` unless a path is given.
///
/// # Errors
///
/// As [`validate_manifest_mount_credential_boundaries`].
pub fn validate_mount_activation_credential_boundary(
    mount: &Mount,
    strategy: &MountStrategy,
    manifest: Option<&Manifest>,
    mount_path: Option<&str>,
    provider_backend_id: Option<&str>,
) -> Result<(), SandboxError> {
    mount_provenance(mount, strategy).map_err(validation_error)?;
    let activation_mount = mount.clone().with_strategy(strategy.clone());
    let standalone;
    let manifest = if let Some(manifest) = manifest {
        manifest
    } else {
        standalone = Manifest::new().with_entry("mount", Entry::mount(activation_mount.clone()));
        &standalone
    };
    let mount_path = mount_path.map_or_else(
        || {
            PosixPath::coerce(&manifest.root)
                .join("mount")
                .as_str()
                .to_owned()
        },
        str::to_owned,
    );
    mount_boundary_error(
        manifest,
        &activation_mount,
        &mount_path,
        provider_backend_id,
    )
    .map_err(SandboxError::with_safe_redacted_message)
}

// --- durable state -------------------------------------------------------------------------------

/// Why a raw manifest or session state could not be sanitized.
///
/// The messages are fixed and never quote the payload: the payload is what carries the
/// credentials, and a refusal is exactly the moment something might print it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct InvalidRawManifest {
    message: &'static str,
}

impl InvalidRawManifest {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }

    /// What was wrong, without the value that was.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

/// How a raw entry is treated, decided by its discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawKind {
    Mount(&'static str),
    Dir,
    LocalDir,
    File,
    /// A type somebody registered that is not a mount; its fields are its own business.
    Registered,
    /// A type nobody registered, which the reader will refuse anyway.
    Unregistered,
}

impl RawKind {
    fn of(entry: &JsonMap<String, Value>, registered: &dyn Fn(&str) -> bool) -> Self {
        let Some(Value::String(entry_type)) = entry.get("type") else {
            return Self::Unregistered;
        };
        if let Some(mount_type) = BUILTIN_MOUNT_TYPES
            .iter()
            .find(|mount_type| **mount_type == entry_type)
        {
            return Self::Mount(mount_type);
        }
        match entry_type.as_str() {
            DIR_ENTRY_TYPE => Self::Dir,
            LOCAL_DIR_ENTRY_TYPE => Self::LocalDir,
            FILE_ENTRY_TYPE => Self::File,
            LOCAL_FILE_ENTRY_TYPE | GIT_REPO_ENTRY_TYPE => Self::Registered,
            other if registered(other) => Self::Registered,
            _ => Self::Unregistered,
        }
    }

    /// Whether `children` is read as nested entries.
    ///
    /// A registered custom type may have a field called `children` that means something else to
    /// it, so only a directory — and a type nobody claims — is descended into.
    const fn descends(self) -> bool {
        matches!(self, Self::Dir | Self::Unregistered)
    }
}

/// Whether a raw entry tree has the shape the sanitizer can walk.
fn raw_entry_tree_is_valid(entries: &Value, registered: &dyn Fn(&str) -> bool) -> bool {
    let Value::Object(entries) = entries else {
        return false;
    };
    entries.values().all(|entry| {
        let Value::Object(entry) = entry else {
            return false;
        };
        if !matches!(entry.get("type"), Some(Value::String(_))) {
            return false;
        }
        if !RawKind::of(entry, registered).descends() {
            return true;
        }
        entry
            .get("children")
            .is_none_or(|children| raw_entry_tree_is_valid(children, registered))
    })
}

/// Collects every raw entry with its workspace path and the keys that reach it.
fn collect_raw_entries(
    entries: &JsonMap<String, Value>,
    registered: &dyn Fn(&str) -> bool,
    parent: Option<(&PosixPath, &[String])>,
    collected: &mut Vec<(PosixPath, Vec<String>)>,
) {
    for (name, value) in entries {
        let Value::Object(entry) = value else {
            continue;
        };
        let (path, keys) = match parent {
            None => (PosixPath::coerce(name), vec![name.clone()]),
            Some((prefix, keys)) => {
                let mut keys = keys.to_vec();
                keys.push(name.clone());
                (prefix.join(PosixPath::coerce(name).as_str()), keys)
            }
        };
        collected.push((path.clone(), keys.clone()));
        if !RawKind::of(entry, registered).descends() {
            continue;
        }
        if let Some(Value::Object(children)) = entry.get("children") {
            collect_raw_entries(children, registered, Some((&path, &keys)), collected);
        }
    }
}

/// The raw entry reached by a list of keys.
fn raw_entry_mut<'a>(
    entries: &'a mut JsonMap<String, Value>,
    keys: &[String],
) -> Option<&'a mut JsonMap<String, Value>> {
    let (first, rest) = keys.split_first()?;
    let entry = entries.get_mut(first)?.as_object_mut()?;
    if rest.is_empty() {
        return Some(entry);
    }
    raw_entry_mut(entry.get_mut("children")?.as_object_mut()?, rest)
}

/// Removes every field not in `safe`, reporting whether anything went.
fn strip_to(fields: &mut JsonMap<String, Value>, safe: &[&str]) -> bool {
    let before = fields.len();
    fields.retain(|name, _| safe.contains(&name.as_str()));
    fields.len() != before
}

/// Strips one built-in mount's authority, returning whether anything went and which credential
/// files it named.
#[allow(clippy::too_many_lines)] // One pass over the mount, its strategy and its pattern.
fn sanitize_raw_mount(
    entry: &mut JsonMap<String, Value>,
    mount_type: &'static str,
) -> Result<(bool, Vec<String>), InvalidRawManifest> {
    let mut redacted = false;
    let mut file_paths = Vec::new();

    let before = entry.len();
    entry.retain(|name, _| is_canonical_mount_field(mount_type, name));
    redacted |= entry.len() != before;

    let file_fields = authority_file_fields(mount_type);
    for field in authority_fields(mount_type) {
        let Some(value) = entry.get(*field).filter(|value| !value.is_null()).cloned() else {
            continue;
        };
        entry.insert((*field).to_owned(), Value::Null);
        redacted = true;
        if file_fields.contains(field) {
            let Value::String(path) = value else {
                return Err(InvalidRawManifest::new(
                    "sandbox manifest credential-file path has an invalid shape",
                ));
            };
            file_paths.push(path);
        }
    }
    for field in url_fields(mount_type) {
        if value_carries_inline_authority(entry.get(*field)) {
            entry.insert((*field).to_owned(), Value::Null);
            redacted = true;
        }
    }
    for field in rclone_config_value_fields(mount_type) {
        if contains_line_break(entry.get(*field)) {
            entry.insert((*field).to_owned(), Value::from(""));
            redacted = true;
        }
    }
    if mount_type == S3_FILES_MOUNT_TYPE && is_truthy(entry.get("extra_options")) {
        entry.insert("extra_options".to_owned(), Value::Object(JsonMap::new()));
        redacted = true;
    }

    let strategy = match entry.get_mut("mount_strategy") {
        None | Some(Value::Null) => return Ok((redacted, file_paths)),
        Some(Value::Object(strategy)) => strategy,
        Some(_) => {
            return Err(InvalidRawManifest::new(
                "sandbox manifest mount strategy has an invalid shape",
            ));
        }
    };
    let raw_strategy_type = strategy
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let strategy_type = raw_strategy_type.clone().unwrap_or_else(|| {
        strategy.insert("type".to_owned(), Value::Null);
        redacted = true;
        String::new()
    });
    let strategy_fields = serialized_strategy_fields(&strategy_type);
    if raw_strategy_type.is_some() && strategy_fields.is_none() {
        return Err(InvalidRawManifest::new(
            "sandbox manifest mount strategy has an unknown type",
        ));
    }
    let strip_unknown_strategy = strategy_fields.is_none();
    if let Some(strategy_fields) = strategy_fields {
        redacted |= strip_to(strategy, strategy_fields);
    }
    for field in opaque_strategy_authority_fields(&strategy_type) {
        if is_truthy(strategy.get(*field)) {
            let emptied = if strategy.get(*field).is_some_and(Value::is_object) {
                Value::Object(JsonMap::new())
            } else {
                Value::Null
            };
            strategy.insert((*field).to_owned(), emptied);
            redacted = true;
        }
    }

    match strategy.get_mut("pattern") {
        None | Some(Value::Null) => {}
        Some(Value::Object(pattern)) => {
            let (pattern_redacted, pattern_paths) =
                sanitize_raw_pattern(pattern, mount_type, &strategy_type)?;
            redacted |= pattern_redacted;
            file_paths.extend(pattern_paths);
        }
        Some(_) => {
            return Err(InvalidRawManifest::new(
                "sandbox manifest mount pattern has an invalid shape",
            ));
        }
    }
    if strip_unknown_strategy {
        redacted |= strip_to(strategy, &["type"]);
    }
    Ok((redacted, file_paths))
}

/// Strips one pattern's authority, returning whether anything went and which credential files it
/// named.
fn sanitize_raw_pattern(
    pattern: &mut JsonMap<String, Value>,
    mount_type: &str,
    strategy_type: &str,
) -> Result<(bool, Vec<String>), InvalidRawManifest> {
    let mut redacted = false;
    let mut file_paths = Vec::new();

    let raw_pattern_type = pattern
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if raw_pattern_type.is_none() {
        pattern.insert("type".to_owned(), Value::Null);
        redacted = true;
    }
    // A helper that discovers ambient credentials has nothing to strip, and still needs its
    // runtime-only acknowledgement rebound after the state is read back.
    if capability_for(mount_type, strategy_type, raw_pattern_type.as_deref())
        .is_some_and(|capability| capability.implicit_broad)
    {
        redacted = true;
    }
    let pattern_fields: &[&str] = match raw_pattern_type.as_deref() {
        None => &["type"],
        Some(pattern_type) => serialized_pattern_fields(pattern_type).ok_or(
            InvalidRawManifest::new("sandbox manifest mount pattern has an unknown type"),
        )?,
    };

    if !rclone_remote_name_is_safe(pattern.get("remote_name")) {
        pattern.insert("remote_name".to_owned(), Value::Null);
        redacted = true;
    }
    if let Some(config_file_path) = pattern
        .get("config_file_path")
        .filter(|value| !value.is_null())
        .cloned()
    {
        pattern.insert("config_file_path".to_owned(), Value::Null);
        redacted = true;
        let Value::String(config_file_path) = config_file_path else {
            return Err(InvalidRawManifest::new(
                "sandbox manifest rclone config-file path has an invalid shape",
            ));
        };
        file_paths.push(config_file_path);
    }
    let extra_args = pattern
        .get("extra_args")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let analysis = match &extra_args {
        Value::Array(args) => rclone_extra_args(args),
        _ => RcloneArgs {
            safe: false,
            config_paths: Vec::new(),
            invalid_config_path: false,
        },
    };
    if analysis.invalid_config_path {
        return Err(InvalidRawManifest::new(
            "sandbox manifest rclone config argument has an invalid shape",
        ));
    }
    if !analysis.safe {
        if is_truthy(Some(&extra_args)) {
            pattern.insert("extra_args".to_owned(), Value::Array(Vec::new()));
            redacted = true;
        }
        file_paths.extend(analysis.config_paths);
    }

    match pattern.get_mut("options") {
        None | Some(Value::Null) => {}
        Some(Value::Object(options)) => {
            if value_carries_inline_authority(options.get("endpoint_url")) {
                options.insert("endpoint_url".to_owned(), Value::Null);
                redacted = true;
            }
            if is_truthy(options.get("extra_options"))
                && (mount_type == S3_FILES_MOUNT_TYPE
                    || raw_pattern_type
                        .as_deref()
                        .is_none_or(|kind| kind == "s3files"))
            {
                options.insert("extra_options".to_owned(), Value::Object(JsonMap::new()));
                redacted = true;
            }
            let option_fields = raw_pattern_type
                .as_deref()
                .map_or(&[][..], serialized_pattern_option_fields);
            redacted |= strip_to(options, option_fields);
        }
        Some(options) => {
            *options = Value::Object(JsonMap::new());
            redacted = true;
        }
    }
    redacted |= strip_to(pattern, pattern_fields);
    Ok((redacted, file_paths))
}

/// Empties inline credential files, and refuses a credential path anything else would populate.
///
/// A file with inline content at a credential path is the credential, so its content goes. Any
/// other entry at or above the path — a host file, a checkout, a mount, a custom entry — would put
/// something there on restore that nobody can vouch for, so the manifest cannot be restored safely.
fn sanitize_raw_credential_file_sources(
    root: &str,
    entries: &mut JsonMap<String, Value>,
    raw_entries: &[(PosixPath, Vec<String>)],
    authority_file_paths: &BTreeSet<String>,
    registered: &dyn Fn(&str) -> bool,
) -> Result<bool, InvalidRawManifest> {
    let mut redacted = false;
    for target in authority_file_paths {
        for (path, keys) in raw_entries {
            let entry_path = absolute_manifest_path(root, path.as_str());
            let Some(entry) = raw_entry_mut(entries, keys) else {
                continue;
            };
            let kind = RawKind::of(entry, registered);
            if entry_path == *target && kind == RawKind::File {
                entry.insert("content".to_owned(), Value::from(""));
                redacted = true;
                continue;
            }
            if entry_path == *target || is_strict_ancestor(&entry_path, target) {
                let structural = kind == RawKind::Dir
                    || (kind == RawKind::LocalDir && !is_set(entry.get("src")));
                if structural {
                    continue;
                }
                return Err(InvalidRawManifest::new(
                    "sandbox manifest credential-file source cannot be restored safely",
                ));
            }
        }
    }
    Ok(redacted)
}

/// Sanitizes a raw manifest, deciding which types are registered with `registered`.
fn sanitize_raw_manifest(
    payload: &Value,
    registered: &dyn Fn(&str) -> bool,
) -> Result<(Value, bool), InvalidRawManifest> {
    let Value::Object(fields) = payload else {
        return Ok((payload.clone(), false));
    };
    let mut manifest = fields.clone();
    if let Some(entries) = manifest.get("entries")
        && !raw_entry_tree_is_valid(entries, registered)
    {
        return Err(InvalidRawManifest::new(
            "sandbox manifest entries have an invalid shape",
        ));
    }
    let root = manifest
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MANIFEST_ROOT)
        .to_owned();

    let mut raw_entries = Vec::new();
    if let Some(Value::Object(entries)) = manifest.get("entries") {
        collect_raw_entries(entries, registered, None, &mut raw_entries);
    }
    let mut redacted = false;
    if let Some(Value::Object(entries)) = manifest.get_mut("entries") {
        let mut authority_file_paths = BTreeSet::new();
        for (_, keys) in &raw_entries {
            let Some(entry) = raw_entry_mut(entries, keys) else {
                continue;
            };
            match RawKind::of(entry, registered) {
                RawKind::Mount(mount_type) => {
                    let (entry_redacted, file_paths) = sanitize_raw_mount(entry, mount_type)?;
                    redacted |= entry_redacted;
                    authority_file_paths.extend(
                        file_paths
                            .iter()
                            .map(|path| absolute_manifest_path(&root, path)),
                    );
                }
                RawKind::Unregistered if entry.contains_key("mount_strategy") => {
                    return Err(InvalidRawManifest::new(
                        "sandbox manifest contains an unknown mount-like entry",
                    ));
                }
                _ => {}
            }
        }
        redacted |= sanitize_raw_credential_file_sources(
            &root,
            entries,
            &raw_entries,
            &authority_file_paths,
            registered,
        )?;
    }
    Ok((Value::Object(manifest), redacted))
}

/// Strips mount authority from a manifest in its documented raw shape.
///
/// Returns the sanitized copy and whether anything was removed. `entries` is the entry registry the
/// payload will be read through: a registered custom type is left alone, while an unregistered one
/// that looks like a mount is refused. A payload that is not an object is returned as it is, for
/// the reader to refuse.
///
/// # Errors
///
/// Returns [`InvalidRawManifest`] for a shape the sanitizer cannot vouch for: a malformed entry tree
/// or strategy, a strategy or pattern type nobody vouches for, an unknown mount-like entry, a
/// credential path given as something other than a string, or a credential file anything but inline
/// content would populate.
pub fn sanitize_raw_manifest_mount_authority(
    payload: &Value,
    entries: &TypeRegistry,
) -> Result<(Value, bool), InvalidRawManifest> {
    sanitize_raw_manifest(payload, &|type_name| entries.is_registered(type_name))
}

/// Sanitizes a raw session state, deciding which types are registered with `registered`.
pub(crate) fn sanitize_raw_session_state(
    payload: &Value,
    registered: &dyn Fn(&str) -> bool,
) -> Result<(Value, bool), InvalidRawManifest> {
    let Value::Object(fields) = payload else {
        return Ok((payload.clone(), false));
    };
    let mut state = fields.clone();
    if state
        .get("manifest")
        .is_some_and(|manifest| !manifest.is_object())
    {
        return Err(InvalidRawManifest::new(
            "sandbox manifest has an invalid shape",
        ));
    }
    state.remove(CREDENTIALLESS_MOUNT_AUTHORITY_KEY);
    let mut redacted = false;
    if let Some(manifest) = state.get("manifest") {
        let (manifest, manifest_redacted) = sanitize_raw_manifest(manifest, registered)?;
        state.insert("manifest".to_owned(), manifest);
        redacted = manifest_redacted;
    }
    if redacted || state.get(REDACTED_MOUNT_AUTHORITY_KEY) == Some(&Value::Bool(true)) {
        state.insert(REDACTED_MOUNT_AUTHORITY_KEY.to_owned(), Value::Bool(true));
        redacted = true;
    }
    Ok((Value::Object(state), redacted))
}

/// Strips mount authority from a session state in its documented raw shape.
///
/// Sanitizes the embedded manifest, drops the obsolete credentialless claim, and sets
/// [`REDACTED_MOUNT_AUTHORITY_KEY`] when anything was removed now or had been removed before. The
/// state's other fields are backend business and are left as they are.
///
/// # Errors
///
/// As [`sanitize_raw_manifest_mount_authority`], and for a `manifest` that is not an object.
pub fn sanitize_raw_session_state_mount_authority(
    payload: &Value,
    entries: &TypeRegistry,
) -> Result<(Value, bool), InvalidRawManifest> {
    sanitize_raw_session_state(payload, &|type_name| entries.is_registered(type_name))
}

/// Why a checkpoint's sandbox resume state was refused.
///
/// Says which of the two checks failed and nothing about the payload, which is where credentials
/// would be.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRunStateSandbox {
    /// The envelope is not an object, or a session state or `sessions_by_agent` entry in it is not
    /// one.
    #[error("RunState sandbox resume state has an invalid envelope")]
    Envelope,
    /// A session state's manifest has a shape the sanitizer cannot vouch for.
    #[error("RunState sandbox resume state contains an invalid manifest")]
    Manifest,
}

/// Whether a resume envelope has the documented shape: an object whose `session_state`, if
/// present, is an object, and whose `sessions_by_agent`, if present, maps to objects whose own
/// `session_state`, if present, is an object.
fn run_state_sandbox_envelope_is_valid(payload: &Value) -> bool {
    let Value::Object(envelope) = payload else {
        return false;
    };
    if envelope
        .get("session_state")
        .is_some_and(|state| !state.is_object())
    {
        return false;
    }
    match envelope.get("sessions_by_agent") {
        None | Some(Value::Null) => true,
        Some(Value::Object(sessions)) => sessions.values().all(|entry| match entry {
            Value::Object(entry) => entry.get("session_state").is_none_or(Value::is_object),
            _ => false,
        }),
        Some(_) => false,
    }
}

/// Strips mount authority from every session state a run's checkpoint carries.
///
/// The reference runs this on a checkpoint's sandbox envelope both when it is written and when it
/// is read. Each state in it was sanitized by the client that serialized it, but the envelope also
/// carries entries copied forward from earlier checkpoints for agents that did not run, and
/// whatever a host put there itself; none of those passed through a client on the way.
///
/// Returns the sanitized copy and whether anything was removed. `entries` decides which custom
/// entry types count as registered, as for [`sanitize_raw_session_state_mount_authority`].
///
/// # Errors
///
/// Returns [`InvalidRunStateSandbox::Envelope`] for an envelope without the documented shape, and
/// [`InvalidRunStateSandbox::Manifest`] for a state whose manifest cannot be sanitized.
pub fn sanitize_run_state_sandbox_mount_authority(
    payload: &Value,
    entries: &TypeRegistry,
) -> Result<(Value, bool), InvalidRunStateSandbox> {
    if !run_state_sandbox_envelope_is_valid(payload) {
        return Err(InvalidRunStateSandbox::Envelope);
    }
    let registered = |type_name: &str| entries.is_registered(type_name);
    let sanitize = |state: &Value| {
        sanitize_raw_session_state(state, &registered).map_err(|_| InvalidRunStateSandbox::Manifest)
    };
    let Value::Object(mut envelope) = payload.clone() else {
        return Err(InvalidRunStateSandbox::Envelope);
    };
    let mut redacted = false;
    if let Some(state) = envelope.get("session_state") {
        let (state, state_redacted) = sanitize(state)?;
        envelope.insert("session_state".to_owned(), state);
        redacted |= state_redacted;
    }
    if let Some(Value::Object(sessions)) = envelope.get_mut("sessions_by_agent") {
        for entry in sessions.values_mut() {
            let Value::Object(fields) = entry else {
                return Err(InvalidRunStateSandbox::Envelope);
            };
            if let Some(state) = fields.get("session_state") {
                let (state, state_redacted) = sanitize(state)?;
                fields.insert("session_state".to_owned(), state);
                redacted |= state_redacted;
            } else {
                let (state, state_redacted) = sanitize(entry)?;
                *entry = state;
                redacted |= state_redacted;
            }
        }
    }
    Ok((Value::Object(envelope), redacted))
}

/// The types of the custom entries a typed manifest holds.
///
/// A typed manifest only holds a custom entry if the host registered or built it, so for its own
/// rendering these count as registered.
fn custom_entry_types(manifest: &Manifest) -> BTreeSet<String> {
    fn walk(entries: &BTreeMap<String, Entry>, types: &mut BTreeSet<String>) {
        for entry in entries.values() {
            match entry.content() {
                EntryContent::Extension(payload) => {
                    types.insert(payload.type_name().to_owned());
                }
                EntryContent::Dir { children } => walk(children, types),
                _ => {}
            }
        }
    }
    let mut types = BTreeSet::new();
    walk(&manifest.entries, &mut types);
    types
}

/// The failure to report when a manifest could not be rendered in its durable form.
///
/// A manifest that carries authority gets a fixed message, because the failure happened while its
/// credentials were being handled.
fn durable_form_error(manifest: &Manifest, message: String) -> SandboxError {
    if manifest_has_configured_mount_authority(manifest) {
        SandboxError::mount_config(SERIALIZATION_MESSAGE)
    } else {
        SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::PersistWorkspace,
            message,
        )
    }
}

/// Renders a manifest in its durable form: no mount authority in it.
///
/// Returns the rendering and whether anything was removed. The reference hands back a typed
/// manifest re-read from this; reading one back needs the host's registries, and every caller here
/// wants the rendering anyway, so this stops at the rendering.
///
/// # Errors
///
/// Returns [`ErrorCode::MountConfigInvalid`] for a custom mount or strategy, and for a manifest
/// carrying authority that cannot be rendered or sanitized; a manifest without authority that
/// cannot be rendered reports why.
pub fn sanitize_manifest_mount_authority(
    manifest: &Manifest,
) -> Result<(Value, bool), SandboxError> {
    validate_manifest_mount_provenance(manifest)?;
    let rendered = serde_json::to_value(manifest)
        .map_err(|error| durable_form_error(manifest, error.to_string()))?;
    let custom_types = custom_entry_types(manifest);
    sanitize_raw_manifest(&rendered, &|type_name| custom_types.contains(type_name)).map_err(
        |error| {
            if manifest_has_configured_mount_authority(manifest) {
                SandboxError::mount_config(SERIALIZATION_MESSAGE)
            } else {
                SandboxError::mount_config(error.message())
            }
        },
    )
}

/// The durable form of every mount in a sanitized rendering, by workspace path.
fn durable_mounts(durable: &Value, registered: &dyn Fn(&str) -> bool) -> BTreeMap<String, Value> {
    let mut raw_entries = Vec::new();
    if let Some(Value::Object(entries)) = durable.get("entries") {
        collect_raw_entries(entries, registered, None, &mut raw_entries);
    }
    let mut mounts = BTreeMap::new();
    let Some(Value::Object(entries)) = durable.get("entries") else {
        return mounts;
    };
    let mut entries = entries.clone();
    for (path, keys) in raw_entries {
        if let Some(entry) = raw_entry_mut(&mut entries, &keys)
            && matches!(RawKind::of(entry, registered), RawKind::Mount(_))
        {
            mounts.insert(path.as_str().to_owned(), Value::Object(entry.clone()));
        }
    }
    mounts
}

/// Replaces each mount entry with the trusted one declared at the same path.
fn replace_mount_entries(
    entries: &mut BTreeMap<String, Entry>,
    prefix: Option<&PosixPath>,
    trusted: &BTreeMap<String, Entry>,
) {
    for (name, entry) in entries.iter_mut() {
        let relative = PosixPath::coerce(name);
        let path = prefix.map_or_else(|| relative.clone(), |prefix| prefix.join(relative.as_str()));
        if entry.content().is_mount() {
            if let Some(replacement) = trusted.get(path.as_str()) {
                *entry = replacement.clone();
            }
            continue;
        }
        if let Some(children) = entry.children_mut() {
            replace_mount_entries(children, Some(&path), trusted);
        }
    }
}

/// Restores live mount authority from a manifest the host trusts now.
///
/// Only when the credential-free shape of the two matches exactly: the same root, the same mounts at
/// the same paths, and each one identical once its authority is stripped. Anything less and the
/// persisted state would be borrowing credentials for a mount the trusted manifest never declared.
/// The trusted manifest must itself pass the credential boundary for the backend that will run it,
/// and its exposure acknowledgements come along — they are runtime decisions and were never
/// persisted.
///
/// # Errors
///
/// Returns the trusted manifest's boundary failure, a provenance failure of either manifest, or
/// [`ErrorCode::MountConfigInvalid`] when the topologies differ.
pub fn rebind_manifest_mount_authority(
    persisted: &Manifest,
    trusted: &Manifest,
    provider_backend_id: &str,
) -> Result<Manifest, SandboxError> {
    let boundary =
        validate_manifest_mount_credential_boundaries(trusted, Some(provider_backend_id));
    let (persisted_durable, _) = sanitize_manifest_mount_authority(persisted)?;
    let (trusted_durable, _) = sanitize_manifest_mount_authority(trusted)?;
    boundary?;

    let persisted_types = custom_entry_types(persisted);
    let trusted_types = custom_entry_types(trusted);
    let topology_matches = persisted.root == trusted.root
        && durable_mounts(&persisted_durable, &|type_name| {
            persisted_types.contains(type_name)
        }) == durable_mounts(&trusted_durable, &|type_name| {
            trusted_types.contains(type_name)
        });
    if !topology_matches {
        return Err(SandboxError::mount_config(
            "sandbox mount configuration can be rebound only from a current trusted mount \
             configuration with exactly matching credential-free topology",
        )
        .with_context("sandbox_backend", provider_backend_id)
        .with_safe_redacted_message());
    }

    let trusted_mounts: BTreeMap<String, Entry> = trusted
        .iter_entries()?
        .into_iter()
        .filter(|(_, entry)| entry.content().is_mount())
        .map(|(path, entry)| (path.as_str().to_owned(), entry.clone()))
        .collect();
    let mut rebound = persisted.clone();
    replace_mount_entries(&mut rebound.entries, None, &trusted_mounts);
    rebound.copy_mount_credential_exposure_policy_from(trusted);
    Ok(rebound)
}
