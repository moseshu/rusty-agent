//! Codex's session approval cache: `ApprovalStore` and `with_cached_approval` from
//! `tools/sandboxing.rs`, and the cache keys `tools/approvals.rs` derives for commands and patches.
//!
//! Codex wraps the approval prompt in `with_cached_approval`, which looks the keys up before asking
//! and stores them when the answer is `ApprovedForSession`. An interrupted run asks in one place
//! and receives the answer in another, so the two halves are separate here:
//! [`ApprovalStore::all_approved_for_session`] where a call would ask, and
//! [`ApprovalStore::remember`] where the host answers. Codex's `codex.approval.requested`
//! counter is not ported.

use std::{collections::HashMap, path::PathBuf};

use ra_patch::PatchPlan;
use ra_protocol::server::ReviewDecision;
use serde::Serialize;

use crate::{
    command_canonicalization::canonicalize_command_for_approval, exec_policy::SandboxPermissions,
};

/// The environment a coding thread runs its tools in. Codex keys approvals by the environment a
/// tool runs in; this build runs every tool in the local one, which Codex names `local`.
const LOCAL_ENVIRONMENT_ID: &str = "local";

/// Decisions cached for one thread, by serialized key.
#[derive(Clone, Default, Debug)]
pub(crate) struct ApprovalStore {
    // Store serialized keys for generic caching across requests.
    map: HashMap<String, ReviewDecision>,
}

impl ApprovalStore {
    pub(crate) fn get<K>(&self, key: &K) -> Option<ReviewDecision>
    where
        K: Serialize,
    {
        let s = serde_json::to_string(key).ok()?;
        self.map.get(&s).cloned()
    }

    pub(crate) fn put<K>(&mut self, key: K, value: ReviewDecision)
    where
        K: Serialize,
    {
        if let Ok(s) = serde_json::to_string(&key) {
            self.map.insert(s, value);
        }
    }

    /// Whether every key is already approved for the session, so the call skips its prompt.
    ///
    /// There will be one key in most cases, but `apply_patch` can modify multiple files at once.
    pub(crate) fn all_approved_for_session<K>(&self, keys: &[K]) -> bool
    where
        K: Serialize,
    {
        // To be defensive here, don't bother with checking the cache if keys are empty.
        if keys.is_empty() {
            return false;
        }
        keys.iter()
            .all(|key| matches!(self.get(key), Some(ReviewDecision::ApprovedForSession)))
    }

    /// Stores each key of a call the host approved for the session, individually, so future
    /// requests touching any subset can also skip prompting. Other decisions are not stored.
    pub(crate) fn remember<K>(&mut self, keys: Vec<K>, decision: &ReviewDecision)
    where
        K: Serialize,
    {
        if matches!(decision, ReviewDecision::ApprovedForSession) {
            for key in keys {
                self.put(key, ReviewDecision::ApprovedForSession);
            }
        }
    }
}

/// Cache key for approval decisions that can be reused across equivalent command launches:
/// Codex's `UnifiedExecApprovalKey`. Codex's `additional_permissions` is not accepted by this
/// build's `exec_command` yet, so it is not part of the key.
#[derive(Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct UnifiedExecApprovalKey {
    environment_id: String,
    executable: Option<String>,
    command: Vec<String>,
    cwd: PathBuf,
    tty: bool,
    sandbox_permissions: SandboxPermissions,
}

/// Codex's `ApplyPatchApprovalKey`: one path a patch touches.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub(crate) struct ApplyPatchApprovalKey {
    environment_id: String,
    path: PathBuf,
}

/// Codex's `ApprovalCacheKey`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(untagged)]
pub(crate) enum ApprovalCacheKey {
    ExecCommand(UnifiedExecApprovalKey),
    ApplyPatch(ApplyPatchApprovalKey),
}

/// A command key with the fingerprint of the requirements policy it was decided under, as Codex
/// pairs it with its environment's: a session approval does not outlive a change of that policy.
pub(crate) type CommandCacheKey = (ApprovalCacheKey, Option<Vec<String>>);

/// The command the call runs, as Codex's `ApprovalAction::ExecCommand` holds it.
pub(crate) struct CommandApproval<'a> {
    /// The argv the command is judged in, `[shell, "-lc" or "-c", cmd]`.
    pub(crate) command: &'a [String],
    /// The resolved working directory.
    pub(crate) cwd: PathBuf,
    pub(crate) tty: bool,
    pub(crate) sandbox_permissions: SandboxPermissions,
}

impl CommandApproval<'_> {
    /// Codex's `ApprovalAction::cache_keys` for a command, paired with the policy fingerprint.
    pub(crate) fn cache_keys(&self, policy_fingerprint: Option<&[String]>) -> Vec<CommandCacheKey> {
        let key = ApprovalCacheKey::ExecCommand(UnifiedExecApprovalKey {
            environment_id: LOCAL_ENVIRONMENT_ID.to_owned(),
            executable: self.command.first().cloned(),
            command: canonicalize_command_for_approval(self.command),
            cwd: self.cwd.clone(),
            tty: self.tty,
            sandbox_permissions: self.sandbox_permissions,
        });
        vec![(key, policy_fingerprint.map(<[String]>::to_vec))]
    }
}

/// Codex's `ApprovalAction::cache_keys` for a patch: one key per path it touches, a move's
/// destination included. Paths are resolved against `root`, the directory patch paths are
/// relative to.
pub(crate) fn patch_cache_keys(root: &std::path::Path, plan: &PatchPlan) -> Vec<ApprovalCacheKey> {
    plan.actions()
        .iter()
        .flat_map(ra_patch::PatchAction::target_paths)
        .map(|path| {
            ApprovalCacheKey::ApplyPatch(ApplyPatchApprovalKey {
                environment_id: LOCAL_ENVIRONMENT_ID.to_owned(),
                path: root.join(path).components().collect(),
            })
        })
        .collect()
}
