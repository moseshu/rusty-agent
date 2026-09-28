//! Skills kept in a directory on the host, indexed up front and staged one at a time.
//!
//! A port of the reference's `LocalDirLazySkillSource` (`sandbox/capabilities/skills.py`). The
//! source is a `local_dir` entry naming a directory whose subdirectories are skills, each with a
//! `SKILL.md`. Listing reads each `SKILL.md`'s frontmatter on the host; loading copies one skill
//! into the workspace through the session, as the configured entry would have been copied — same
//! permissions, same group — but only that skill, and only when a model asks for it.
//!
//! # What the source may read
//!
//! The same authority a manifest's own `local_dir` entries are held to: a directory under the
//! process's working directory, or one an extra path grant of the session's manifest reaches. A
//! source directory outside both is treated as having no skills rather than as an error, so an index
//! simply leaves them out until the manifest grants the directory. Symlinks are refused on the way
//! to the directory and skipped inside it: a skill directory or a `SKILL.md` that is a link is not
//! listed, which is what stops a link from pulling a file the grant never covered into the index.
//!
//! # Deviations from the reference
//!
//! - **A source that is not a host directory is refused when it is built.** The reference types the
//!   field as `LocalDir`, so a wrong entry fails validation; [`LocalDirLazySkillSource::new`] takes
//!   any [`Entry`] and refuses the others with a skills configuration failure.
//! - **UTF-8 decoding failures travel as a sandbox error with their original cause.** Like the
//!   reference's uncaught decode error, they fail the index; ordinary file read failures are skipped.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::sandbox::{
    Entry, EntryContent, ErrorCode, LazySkillSource, NO_SKILL_DESCRIPTION, PosixPath,
    SKILL_MARKDOWN, SandboxError, SandboxPathGrant, SandboxResult, SandboxSession, SessionPath,
    SkillLoadResult, SkillMetadata, User, parse_skill_frontmatter,
};

use crate::materialize::errors::local_dir_read;
use crate::materialize::local::LocalSource;
use crate::materialize::{ManifestApplier, manifest_base_dir};

/// Loads skills lazily from a directory on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDirLazySkillSource {
    source: Entry,
}

impl LocalDirLazySkillSource {
    /// Reads skills from the directory `source` copies from.
    ///
    /// The entry's permissions and group are what each loaded skill is given.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SkillsConfigInvalid`] for an entry that is not a `local_dir`.
    pub fn new(source: Entry) -> SandboxResult<Self> {
        if !matches!(source.content(), EntryContent::LocalDir { .. }) {
            return Err(SandboxError::skills_config(
                "lazy skill source must be a `local_dir` entry",
            )
            .with_context("field", "source")
            .with_context("source_type", source.entry_type()));
        }
        Ok(Self { source })
    }

    /// The entry skills are copied as.
    #[must_use]
    pub const fn source(&self) -> &Entry {
        &self.source
    }

    /// The source directory, or `None` when there is nothing to read: no source named, a source
    /// outside what the working directory and `source_grants` reach, one that is missing or a
    /// link on the way, or one that is not a directory.
    fn src_root(&self, source_grants: &[SandboxPathGrant]) -> SandboxResult<Option<PathBuf>> {
        let EntryContent::LocalDir { src: Some(src) } = self.source.content() else {
            return Ok(None);
        };
        let base_dir = manifest_base_dir()?;
        let src_root = match LocalSource::new(&base_dir, Path::new(src), source_grants)
            .resolve_root()
        {
            Ok(src_root) => src_root,
            Err(error) if error.error_code() == ErrorCode::LocalDirReadError => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(src_root.is_dir().then_some(src_root))
    }

    /// Reads one skill directory's index line, or `None` for a child that is not one.
    fn read_skill(child: &Path, skills_path: &PosixPath) -> SandboxResult<Option<SkillMetadata>> {
        let Ok(child_metadata) = std::fs::symlink_metadata(child) else {
            return Ok(None);
        };
        if !child_metadata.is_dir() {
            return Ok(None);
        }
        let skill_md = child.join(SKILL_MARKDOWN);
        let Ok(metadata) = std::fs::symlink_metadata(&skill_md) else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Ok(None);
        }
        let Ok(bytes) = std::fs::read(&skill_md) else {
            return Ok(None);
        };
        let markdown = String::from_utf8(bytes).map_err(|error| {
            SandboxError::skills_config("skill markdown is not valid UTF-8")
                .with_context("path", skill_md.to_string_lossy().as_ref())
                .with_cause(error)
        })?;
        let Some(directory_name) = child.file_name() else {
            return Ok(None);
        };
        let directory_name = directory_name.to_string_lossy().into_owned();
        let mut frontmatter = parse_skill_frontmatter(&markdown);
        Ok(Some(SkillMetadata::new(
            frontmatter
                .remove("name")
                .unwrap_or_else(|| directory_name.clone()),
            frontmatter
                .remove("description")
                .unwrap_or_else(|| NO_SKILL_DESCRIPTION.to_owned()),
            skills_path.join(&directory_name).as_str(),
        )))
    }
}

#[async_trait]
impl LazySkillSource for LocalDirLazySkillSource {
    fn list_skill_metadata(
        &self,
        skills_path: &str,
        source_grants: &[SandboxPathGrant],
    ) -> SandboxResult<Vec<SkillMetadata>> {
        let Some(src_root) = self.src_root(source_grants)? else {
            return Ok(Vec::new());
        };
        let listing = std::fs::read_dir(&src_root)
            .map_err(|error| local_dir_read(&src_root).with_cause(error))?;
        let mut children = Vec::new();
        for child in listing {
            children.push(
                child
                    .map_err(|error| local_dir_read(&src_root).with_cause(error))?
                    .path(),
            );
        }
        children.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

        let skills_path = PosixPath::new(skills_path);
        let mut metadata = Vec::new();
        for child in &children {
            if let Some(skill) = Self::read_skill(child, &skills_path)? {
                metadata.push(skill);
            }
        }
        Ok(metadata)
    }

    async fn load_skill(
        &self,
        skill_name: &str,
        session: &Arc<dyn SandboxSession>,
        skills_path: &str,
        user: Option<&User>,
    ) -> SandboxResult<SkillLoadResult> {
        let manifest = session.state().manifest().clone();
        let source_grants = &manifest.extra_path_grants;
        let Some(src_root) = self.src_root(source_grants)? else {
            return Err(
                SandboxError::skills_config("lazy skill source directory is unavailable")
                    .with_context("skill_name", skill_name),
            );
        };

        let mut matches: Vec<SkillMetadata> = self
            .list_skill_metadata(skills_path, source_grants)?
            .into_iter()
            .filter(|skill| skill.name() == skill_name || skill.directory_name() == skill_name)
            .collect();
        if matches.is_empty() {
            return Err(SandboxError::skills_config("lazy skill not found")
                .with_context("skill_name", skill_name)
                .with_context("skills_path", skills_path));
        }
        if matches.len() > 1 {
            return Err(SandboxError::skills_config("lazy skill name is ambiguous")
                .with_context("skill_name", skill_name)
                .with_context(
                    "matching_paths",
                    matches
                        .iter()
                        .map(|skill| skill.path().as_str().to_owned())
                        .collect::<Vec<_>>(),
                ));
        }
        let metadata = matches.remove(0);

        let skill_dest = PosixPath::coerce(&manifest.root).join(metadata.path().as_str());
        let skill_md = skill_dest.join(SKILL_MARKDOWN);
        // A probe: the skill is usually not there yet, and a trace should not call that an error.
        match session
            .read_expecting(
                SessionPath::Posix(&skill_md),
                user.cloned(),
                &[ErrorCode::WorkspaceReadNotFound],
            )
            .await
        {
            Ok(_) => {
                return Ok(SkillLoadResult::loaded(
                    "already_loaded",
                    metadata.name(),
                    metadata.path().as_str(),
                ));
            }
            Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => {}
            Err(error) => return Err(error),
        }

        // Copied through the configured entry, pointed at this one skill, so the loaded skill gets
        // the permissions and group the entry declares — as the eager `from` route applies them.
        let skill_source = self.source.clone().with_source(
            src_root
                .join(metadata.directory_name())
                .to_string_lossy()
                .into_owned(),
        );
        ManifestApplier::new(Arc::clone(session), manifest_base_dir()?)
            .with_limits(session.concurrency_limits())
            .apply_local_dir_as(&skill_source, &skill_dest, user)
            .await?;
        Ok(SkillLoadResult::loaded(
            "loaded",
            metadata.name(),
            metadata.path().as_str(),
        ))
    }
}
