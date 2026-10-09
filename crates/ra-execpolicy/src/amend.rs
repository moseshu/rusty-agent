//! Appending rules to a rules file: Codex's `execpolicy/src/amend.rs`.
//!
//! A rule is appended as one line under an advisory lock on the file, after a missing newline, and
//! only when the file does not already hold that exact line. The directory is created if missing,
//! but not its parents.

use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    decision::Decision,
    rule::{NetworkRuleProtocol, normalize_network_rule_host},
};

/// Why a rule could not be appended.
#[derive(Debug, Error)]
pub enum AmendError {
    /// The prefix is empty.
    #[error("prefix rule requires at least one token")]
    EmptyPrefix,
    /// The network rule is invalid.
    #[error("invalid network rule: {0}")]
    InvalidNetworkRule(String),
    /// The policy path has no directory.
    #[error("policy path has no parent: {path}")]
    MissingParent {
        /// The path.
        path: PathBuf,
    },
    /// Its directory could not be created.
    #[error("failed to create policy directory {dir}: {source}")]
    CreatePolicyDir {
        /// The directory.
        dir: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// A prefix token could not be quoted.
    #[error("failed to format prefix tokens: {source}")]
    SerializePrefix {
        /// The cause.
        source: serde_json::Error,
    },
    /// A network rule field could not be quoted.
    #[error("failed to serialize network rule field: {source}")]
    SerializeNetworkRule {
        /// The cause.
        source: serde_json::Error,
    },
    /// The file could not be opened.
    #[error("failed to open policy file {path}: {source}")]
    OpenPolicyFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file could not be written.
    #[error("failed to write to policy file {path}: {source}")]
    WritePolicyFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file could not be locked.
    #[error("failed to lock policy file {path}: {source}")]
    LockPolicyFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file could not be rewound.
    #[error("failed to seek policy file {path}: {source}")]
    SeekPolicyFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file could not be read.
    #[error("failed to read policy file {path}: {source}")]
    ReadPolicyFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file's metadata could not be read.
    #[error("failed to read metadata for policy file {path}: {source}")]
    PolicyMetadata {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
}

/// Note this thread uses advisory file locking and performs blocking I/O, so it should be used with
/// `tokio::task::spawn_blocking` when called from an async context.
pub fn blocking_append_allow_prefix_rule(
    policy_path: &Path,
    prefix: &[String],
) -> Result<(), AmendError> {
    if prefix.is_empty() {
        return Err(AmendError::EmptyPrefix);
    }

    let tokens = prefix
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AmendError::SerializePrefix { source })?;
    let pattern = format!("[{}]", tokens.join(", "));
    let rule = format!(r#"prefix_rule(pattern={pattern}, decision="allow")"#);
    append_rule_line(policy_path, &rule)
}

/// Note this function uses advisory file locking and performs blocking I/O, so it should be used
/// with `tokio::task::spawn_blocking` when called from an async context.
pub fn blocking_append_network_rule(
    policy_path: &Path,
    host: &str,
    protocol: NetworkRuleProtocol,
    decision: Decision,
    justification: Option<&str>,
) -> Result<(), AmendError> {
    let host = normalize_network_rule_host(host)
        .map_err(|err| AmendError::InvalidNetworkRule(err.to_string()))?;
    if let Some(raw) = justification
        && raw.trim().is_empty()
    {
        return Err(AmendError::InvalidNetworkRule(
            "justification cannot be empty".to_string(),
        ));
    }

    let host = serde_json::to_string(&host)
        .map_err(|source| AmendError::SerializeNetworkRule { source })?;
    let protocol = serde_json::to_string(protocol.as_policy_string())
        .map_err(|source| AmendError::SerializeNetworkRule { source })?;
    let decision = serde_json::to_string(match decision {
        Decision::Allow => "allow",
        Decision::Prompt => "prompt",
        Decision::Forbidden => "deny",
    })
    .map_err(|source| AmendError::SerializeNetworkRule { source })?;

    let mut args = vec![
        format!("host={host}"),
        format!("protocol={protocol}"),
        format!("decision={decision}"),
    ];
    if let Some(justification) = justification {
        let justification = serde_json::to_string(justification)
            .map_err(|source| AmendError::SerializeNetworkRule { source })?;
        args.push(format!("justification={justification}"));
    }
    let rule = format!("network_rule({})", args.join(", "));
    append_rule_line(policy_path, &rule)
}

fn append_rule_line(policy_path: &Path, rule: &str) -> Result<(), AmendError> {
    let dir = policy_path
        .parent()
        .ok_or_else(|| AmendError::MissingParent {
            path: policy_path.to_path_buf(),
        })?;
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(ref source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(AmendError::CreatePolicyDir {
                dir: dir.to_path_buf(),
                source,
            });
        }
    }

    append_locked_line(policy_path, rule)
}

fn append_locked_line(policy_path: &Path, line: &str) -> Result<(), AmendError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(policy_path)
        .map_err(|source| AmendError::OpenPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    // Codex takes `File::lock`, stable only since Rust 1.89; this workspace supports 1.88. The
    // same exclusive lock on the whole file is taken through `fd-lock`, which uses `flock` on Unix
    // and `LockFileEx` on Windows, as `File::lock` does, and releases it when the guard drops.
    let mut lock = fd_lock::RwLock::new(file);
    let mut file = lock.write().map_err(|source| AmendError::LockPolicyFile {
        path: policy_path.to_path_buf(),
        source,
    })?;

    file.seek(SeekFrom::Start(0))
        .map_err(|source| AmendError::SeekPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|source| AmendError::ReadPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;

    if contents.lines().any(|existing| existing == line) {
        return Ok(());
    }

    if !contents.is_empty() && !contents.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|source| AmendError::WritePolicyFile {
                path: policy_path.to_path_buf(),
                source,
            })?;
    }

    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|source| AmendError::WritePolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;

    Ok(())
}
