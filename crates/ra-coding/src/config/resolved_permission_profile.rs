// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
use super::permission_profile_snapshot::PermissionProfileSnapshot;
use super::permission_profile_snapshot::ProfileWorkspaceRoot;
use ra_config::Constrained;
use ra_config::ConstraintResult;
use ra_filesystem::models::ActivePermissionProfile;
use ra_filesystem::models::PermissionProfile;
use ra_paths::PathUri;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PermissionProfileState {
    permission_profile: Constrained<PermissionProfileSnapshot>,
}

impl PermissionProfileState {
    pub(crate) fn from_constrained_legacy(
        constrained_permission_profile: Constrained<PermissionProfile>,
    ) -> ConstraintResult<Self> {
        let snapshot =
            PermissionProfileSnapshot::legacy(constrained_permission_profile.get().clone());
        Self::from_constrained_snapshot(constrained_permission_profile, snapshot)
    }

    pub(crate) fn from_constrained_active_profile(
        constrained_permission_profile: Constrained<PermissionProfile>,
        active_permission_profile: Option<ActivePermissionProfile>,
        profile_workspace_roots: Vec<PathUri>,
    ) -> ConstraintResult<Self> {
        let permission_profile = constrained_permission_profile.get().clone();
        let snapshot = match active_permission_profile {
            Some(active_permission_profile) => {
                PermissionProfileSnapshot::active_with_profile_workspace_roots(
                    permission_profile,
                    active_permission_profile,
                    profile_workspace_roots
                        .into_iter()
                        .map(ProfileWorkspaceRoot::from)
                        .collect(),
                )
            }
            None => PermissionProfileSnapshot::legacy(permission_profile),
        };
        Self::from_constrained_snapshot(constrained_permission_profile, snapshot)
    }

    pub(crate) fn from_constrained_snapshot(
        constrained_permission_profile: Constrained<PermissionProfile>,
        permission_profile: PermissionProfileSnapshot,
    ) -> ConstraintResult<Self> {
        let permission_profile = Constrained::new(permission_profile, move |candidate| {
            constrained_permission_profile.can_set(candidate.permission_profile())
        })?;
        Ok(Self { permission_profile })
    }

    pub(crate) fn permission_profile(&self) -> &PermissionProfile {
        self.permission_profile.get().permission_profile()
    }

    #[allow(dead_code, reason = "Retain the trusted session snapshot contract.")]
    pub(crate) fn snapshot(&self) -> PermissionProfileSnapshot {
        self.permission_profile.get().clone()
    }

    pub(crate) fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.permission_profile.get().active_permission_profile()
    }

    pub(crate) fn profile_workspace_roots(&self) -> &[ProfileWorkspaceRoot] {
        self.permission_profile.get().profile_workspace_roots()
    }

    pub(crate) fn can_set_legacy_permission_profile(
        &self,
        permission_profile: &PermissionProfile,
    ) -> ConstraintResult<()> {
        let candidate = PermissionProfileSnapshot::legacy(permission_profile.clone());
        self.permission_profile.can_set(&candidate)
    }

    pub(crate) fn set_legacy_permission_profile(
        &mut self,
        permission_profile: PermissionProfile,
    ) -> ConstraintResult<()> {
        self.permission_profile
            .set(PermissionProfileSnapshot::legacy(permission_profile))
    }

    pub(crate) fn set_permission_profile_snapshot(
        &mut self,
        snapshot: PermissionProfileSnapshot,
    ) -> ConstraintResult<()> {
        self.permission_profile.set(snapshot)
    }
}
