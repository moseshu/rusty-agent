// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::plugins::PluginSkillRoot;
use crate::skills::LoadedSkillRoot;
use crate::skills::SkillRootSnapshotCache;
use crate::skills::SkillRootSnapshots;

#[derive(Default)]
struct PluginSkillSnapshotCache {
    snapshots_by_root: Mutex<HashMap<PluginSkillRoot, LoadedSkillRoot>>,
}

impl SkillRootSnapshotCache<PluginSkillRoot> for PluginSkillSnapshotCache {
    fn get(&self, root: &PluginSkillRoot) -> Option<LoadedSkillRoot> {
        self.snapshots_by_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .cloned()
    }

    fn insert(&self, root: PluginSkillRoot, snapshot: LoadedSkillRoot) {
        self.snapshots_by_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root, snapshot);
    }
}

pub(crate) fn new_plugin_skill_snapshots() -> SkillRootSnapshots<PluginSkillRoot> {
    SkillRootSnapshots::new(Arc::new(PluginSkillSnapshotCache::default()))
}
