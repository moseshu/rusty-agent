// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain source skill metadata, loading and snapshot contracts."
)]
mod discovery;
mod host;
mod host_merge;
mod metadata;
mod namespace;
pub(crate) use host::{HostSkillRoot, HostSkillRootSnapshot, load_host_skill_root};
pub(crate) use host_merge::{
    load_and_merge_host_skill_roots, load_and_merge_host_skill_roots_with_request_snapshots,
};
pub(crate) const MAX_CONCURRENT_ROOT_SCANS: usize = 8;
pub(super) const SKILLS_FILENAME: &str = "SKILL.md";
pub(super) const SKILLS_METADATA_DIR: &str = "agents";
pub(super) const SKILLS_METADATA_FILENAME: &str = "openai.yaml";
pub(super) const MAX_QUALIFIED_NAME_LEN: usize = 129;
pub(super) const MAX_SCAN_DEPTH: usize = 6;
pub(super) const MAX_SKILLS_DIRS_PER_ROOT: usize = 2000;
