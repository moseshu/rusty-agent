// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::collections::HashMap;
use std::sync::Arc;

use ra_executor_protocol::capabilities::CapabilityRootLocation;
use ra_executor_protocol::capabilities::SelectedCapabilityRoot;

use crate::EnvironmentManager;

/// A passive view of selected capability roots and unavailable environments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectedCapabilityRootsStatus {
    /// Selected roots whose environments are ready.
    pub ready_roots: Vec<SelectedCapabilityRoot>,
    /// Missing environments and terminal connection failures.
    pub warnings: Vec<String>,
}

impl EnvironmentManager {
    /// Inspects selected roots without starting or waiting for an environment.
    ///
    /// Starting or recovering environments are omitted. Missing environments and terminal
    /// connection failures are returned as warnings so read-only catalog clients can distinguish
    /// them from an empty catalog.
    ///
    /// Environment IDs are stable identities, so callers can safely resolve a returned root by
    /// ID when they read it.
    pub fn inspect_selected_capability_roots(
        &self,
        selected_roots: &[SelectedCapabilityRoot],
    ) -> SelectedCapabilityRootsStatus {
        let (candidates, mut warnings) = {
            let environments = self
                .environments
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut candidates = Vec::with_capacity(selected_roots.len());
            let mut warnings = Vec::new();
            for selected_root in selected_roots {
                let CapabilityRootLocation::Environment { environment_id, .. } =
                    &selected_root.location;
                let Some(environment) = environments.get(environment_id) else {
                    warnings.push(format!(
                        "selected capability root `{}` references unavailable environment `{environment_id}`",
                        selected_root.id
                    ));
                    continue;
                };
                candidates.push((selected_root.clone(), Arc::clone(environment)));
            }
            (candidates, warnings)
        };
        let mut readiness = HashMap::new();
        for (selected_root, environment) in &candidates {
            let CapabilityRootLocation::Environment { environment_id, .. } =
                &selected_root.location;
            if readiness.contains_key(environment_id) {
                continue;
            }
            let ready = match environment.readiness_result() {
                Some(Ok(())) => true,
                Some(Err(error)) => {
                    warnings.push(format!(
                        "selected capability environment `{environment_id}` is unavailable: {error}"
                    ));
                    false
                }
                None => false,
            };
            readiness.insert(environment_id.clone(), ready);
        }

        let ready_roots = candidates
            .into_iter()
            .filter(|(selected_root, _)| {
                let CapabilityRootLocation::Environment { environment_id, .. } =
                    &selected_root.location;
                readiness.get(environment_id).copied().unwrap_or(false)
            })
            .map(|(selected_root, _)| selected_root)
            .collect();
        SelectedCapabilityRootsStatus {
            ready_roots,
            warnings,
        }
    }
}
