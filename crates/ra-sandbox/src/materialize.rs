//! Turning a manifest into workspace content.
//!
//! The manifest says what a workspace should contain; this puts it there. Everything it writes goes
//! through the session's own file and exec operations, so one applier serves every backend: the
//! source of a copied file is on the machine running the SDK, and the destination is wherever that
//! session's workspace happens to be.
//!
//! # Two applications, not one with a flag
//!
//! [`ManifestApplier::apply_manifest`] materializes everything. [`ManifestApplier::apply_ephemeral`]
//! materializes only what was deliberately never persisted, which is what a workspace that survived
//! a stop needs on the way back up. The reference spells these as one method with `only_ephemeral`,
//! and pairs it with a second flag for account provisioning that it then always passes as the
//! negation of the first — so the two entry points here are the two combinations that exist, and
//! the impossible third one cannot be written.
//!
//! # What runs at the same time, and what cannot
//!
//! Entries are materialized concurrently up to a limit, with two exceptions that are ordering
//! decisions rather than optimisations. A mount attaches somebody else's storage at a path, so it
//! is applied alone, with everything queued before it flushed first. What attaching one involves is
//! the [`MountLifecycle`]'s business; the applier only decides when. And two entries whose paths
//! overlap — a directory and something inside it — are applied in the order the manifest declared
//! them, because otherwise the one that creates the parent can lose a race with the one that fills
//! it.
//!
//! # An empty receipt does not mean an empty workspace
//!
//! [`MaterializationResult`] carries a checksum per file, and only entries that read their content
//! on this side of the sandbox can produce one. A git checkout copies its files with a command
//! inside the sandbox and reports nothing, which is the reference's trade and is recorded on the
//! result type itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use ra_core::sandbox::{
    Entry, EntryContent, EntryOwner, ExecRequest, ExecResult, Manifest, MaterializationResult,
    MaterializedFile, PosixPath, SandboxConcurrencyLimits, SandboxError, SandboxResult,
    SandboxSession, ShellInvocation, User, resolve_workspace_path,
};

pub(crate) mod errors;
mod gather;
mod git;
pub(crate) mod local;

use errors::{local_file_read, unsupported_entry};
use gather::gather_in_order;
use git::GitCheckout;
use local::LocalSource;

use crate::mounts::{BuiltinMountLifecycle, MountLifecycle};

/// Where a relative `src` in a manifest is measured from.
///
/// The directory the SDK process is running in, which is the reference's answer and the only one
/// that makes a manifest written next to a project mean what its author meant.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::SandboxConfigInvalid`] when this process has no readable
/// working directory.
pub fn manifest_base_dir() -> SandboxResult<PathBuf> {
    std::env::current_dir().map_err(|error| {
        SandboxError::new(
            ra_core::sandbox::ErrorCode::SandboxConfigInvalid,
            ra_core::sandbox::OpName::Materialize,
            format!("failed to read the working directory a manifest is measured from: {error}"),
        )
        .with_cause(error)
    })
}

/// Materializes manifests into one session's workspace.
pub struct ManifestApplier {
    session: Arc<dyn SandboxSession>,
    base_dir: PathBuf,
    limits: SandboxConcurrencyLimits,
    mounts: Arc<dyn MountLifecycle>,
}

impl std::fmt::Debug for ManifestApplier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManifestApplier")
            .field("backend", &self.session.backend_id())
            .field("base_dir", &self.base_dir)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ManifestApplier {
    /// Materializes into `session`, measuring relative sources from `base_dir`.
    ///
    /// Shared ownership keeps the session alive for checkout cleanup after a caller drops an
    /// application future. Unlike Python cancellation, Rust dropping a future cannot await finally
    /// blocks; cleanup therefore runs in an owned task on the active Tokio runtime.
    #[must_use]
    pub fn new(session: Arc<dyn SandboxSession>, base_dir: PathBuf) -> Self {
        Self {
            session,
            base_dir,
            limits: SandboxConcurrencyLimits::default(),
            mounts: Arc::new(BuiltinMountLifecycle),
        }
    }

    /// Paces the work with these limits instead of the defaults.
    #[must_use]
    pub const fn with_limits(mut self, limits: SandboxConcurrencyLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Attaches mounts with `mounts` instead of [`BuiltinMountLifecycle`].
    ///
    /// For a host whose manifests use a mount strategy of its own.
    #[must_use]
    pub fn with_mount_lifecycle(mut self, mounts: Arc<dyn MountLifecycle>) -> Self {
        self.mounts = mounts;
        self
    }

    /// Materializes the whole manifest.
    ///
    /// `provision_accounts` asks for the users and groups the manifest names to be created first.
    /// It is a caller's decision rather than a property of the manifest, because a backend whose
    /// account database survived a stop must not create them twice.
    ///
    /// # Errors
    ///
    /// Returns the first entry that could not be materialized, with the rest of its batch
    /// cancelled.
    pub async fn apply_manifest(
        &self,
        manifest: &Manifest,
        provision_accounts: bool,
    ) -> SandboxResult<MaterializationResult> {
        let root = PosixPath::coerce(&manifest.root);
        self.session.mkdir(root.as_str(), true, None).await?;

        if provision_accounts {
            self.provision_accounts(manifest).await?;
        }

        let mut queued = Vec::new();
        for (declared, entry) in manifest.validated_entries()? {
            queued.push((resolve_workspace_path(&manifest.root, declared)?, entry));
        }
        self.apply_entries(queued).await
    }

    /// Materializes only the entries that were never meant to survive a stop.
    ///
    /// No accounts are provisioned: a workspace that is coming back up has whatever account
    /// database its backend kept, and this is called on exactly the paths where it did.
    ///
    /// # Errors
    ///
    /// As [`Self::apply_manifest`].
    pub async fn apply_ephemeral(
        &self,
        manifest: &Manifest,
    ) -> SandboxResult<MaterializationResult> {
        let root = PosixPath::coerce(&manifest.root);
        self.session.mkdir(root.as_str(), true, None).await?;

        let mut queued = Vec::new();
        for (declared, entry) in ephemeral_entries(manifest)? {
            queued.push((
                resolve_workspace_path(&manifest.root, declared.as_str())?,
                entry,
            ));
        }
        self.apply_entries(queued).await
    }

    /// Creates the users and groups the manifest names.
    ///
    /// Groups first, then every user — the manifest's own and each group's members, deduplicated —
    /// then the memberships. A user created by `useradd -U` gets a group of its own, which is what
    /// an entry naming a user rather than a group relies on.
    ///
    /// # Errors
    ///
    /// Returns [`ra_core::sandbox::ErrorCode::ExecNonzero`] for the first command that failed,
    /// carrying its streams.
    pub async fn provision_accounts(&self, manifest: &Manifest) -> SandboxResult<()> {
        let mut users: Vec<&ra_core::sandbox::User> = manifest.users.iter().collect();
        for group in &manifest.groups {
            users.extend(group.users.iter());
            self.exec_checked(vec!["groupadd".to_owned(), group.name.clone()])
                .await?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for user in users {
            if !seen.insert(user.name.clone()) {
                continue;
            }
            self.exec_checked(vec![
                "useradd".to_owned(),
                "-U".to_owned(),
                "-M".to_owned(),
                "-s".to_owned(),
                "/usr/sbin/nologin".to_owned(),
                user.name.clone(),
            ])
            .await?;
        }
        for group in &manifest.groups {
            for user in &group.users {
                self.exec_checked(vec![
                    "usermod".to_owned(),
                    "-aG".to_owned(),
                    group.name.clone(),
                    user.name.clone(),
                ])
                .await?;
            }
        }
        Ok(())
    }

    /// Materializes entries at the absolute paths given, without touching the rest of the manifest.
    ///
    /// For a running session a capability changed: only the delta is written.
    ///
    /// # Errors
    ///
    /// As [`Self::apply_manifest`].
    pub async fn apply_entry_list(
        &self,
        entries: &[(PosixPath, Entry)],
    ) -> SandboxResult<Vec<MaterializedFile>> {
        let queued = entries
            .iter()
            .map(|(dest, entry)| (dest.clone(), entry))
            .collect();
        let cleanup = git::CleanupTasks::default();
        let result = self.apply_entry_batch(queued, &cleanup).await;
        cleanup.wait().await;
        result
    }

    /// Joins cancellation cleanup before a batch result leaves the application.
    async fn apply_entries(
        &self,
        entries: Vec<(PosixPath, &Entry)>,
    ) -> SandboxResult<MaterializationResult> {
        let cleanup = git::CleanupTasks::default();
        let result = self.apply_entry_batch(entries, &cleanup).await;
        cleanup.wait().await;
        result.map(|files| files.into_iter().collect())
    }

    /// Materializes a batch, running what can run together and serialising what cannot.
    fn apply_entry_batch<'e>(
        &'e self,
        entries: Vec<(PosixPath, &'e Entry)>,
        cleanup: &'e git::CleanupTasks,
    ) -> BoxFuture<'e, SandboxResult<Vec<MaterializedFile>>> {
        async move {
            let mut written = Vec::new();
            let mut parallel: Vec<(PosixPath, &Entry)> = Vec::new();

            for (dest, entry) in entries {
                let alone = matches!(entry.content(), EntryContent::Mount(_))
                    || parallel
                        .iter()
                        .any(|(queued, _)| paths_overlap(&dest, queued));
                if alone {
                    written.extend(self.flush(&mut parallel, cleanup).await?);
                    written.extend(self.apply_entry(entry, dest, cleanup).await?);
                    continue;
                }
                parallel.push((dest, entry));
            }

            written.extend(self.flush(&mut parallel, cleanup).await?);
            Ok(written)
        }
        .boxed()
    }

    /// Runs everything queued so far, and empties the queue.
    async fn flush(
        &self,
        parallel: &mut Vec<(PosixPath, &Entry)>,
        cleanup: &git::CleanupTasks,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        if parallel.is_empty() {
            return Ok(Vec::new());
        }
        let batch = std::mem::take(parallel);
        let tasks = batch
            .into_iter()
            .map(|(dest, entry)| self.apply_entry(entry, dest, cleanup))
            .collect();
        Ok(gather_in_order(tasks, self.limits.manifest_entries())
            .await?
            .into_iter()
            .flatten()
            .collect())
    }

    /// Puts one entry at one path.
    fn apply_entry<'e>(
        &'e self,
        entry: &'e Entry,
        dest: PosixPath,
        cleanup: &'e git::CleanupTasks,
    ) -> BoxFuture<'e, SandboxResult<Vec<MaterializedFile>>> {
        async move {
            let written = match entry.content() {
                EntryContent::Dir { children } => {
                    self.session.mkdir(dest.as_str(), true, None).await?;
                    self.apply_metadata(entry, &dest).await?;
                    return self
                        .apply_entry_batch(
                            children
                                .iter()
                                .map(|(name, child)| (dest.join(name), child))
                                .collect(),
                            cleanup,
                        )
                        .await;
                }
                EntryContent::File { content } => {
                    self.session
                        .write(dest.as_str(), content.clone(), None)
                        .await?;
                    Vec::new()
                }
                EntryContent::LocalFile { src } => self.apply_local_file(src, &dest).await?,
                EntryContent::LocalDir { src } => {
                    self.apply_local_dir(src.as_deref(), &dest, None).await?
                }
                EntryContent::GitRepo {
                    host,
                    repo,
                    reference,
                    subpath,
                } => {
                    GitCheckout {
                        host,
                        repo,
                        reference,
                        subpath: subpath.as_deref(),
                    }
                    .apply(Arc::clone(&self.session), &dest, cleanup)
                    .await?;
                    Vec::new()
                }
                // No ownership or mode is applied afterwards, as the reference applies none: a
                // mount's permissions are the provider's, and fixed at the entry default.
                EntryContent::Mount(mount) => {
                    return self
                        .mounts
                        .apply(mount, self.session.as_ref(), &dest, &self.base_dir)
                        .await;
                }
                // A host registered the type, so the manifest parses; putting it in a workspace
                // would take the code that came with it, which does not reach this crate.
                EntryContent::Extension(payload) => {
                    return Err(unsupported_entry(
                        payload.type_name(),
                        "this entry type is registered but has no materialization here",
                    ));
                }
                // The content family is open, so a build that parses a manifest may be older than
                // the kind of entry in it.
                _ => {
                    return Err(unsupported_entry(
                        entry.entry_type(),
                        "this build does not know what this entry puts in a workspace",
                    ));
                }
            };
            self.apply_metadata(entry, &dest).await?;
            Ok(written)
        }
        .boxed()
    }

    /// Copies one host file into the workspace, and reports what it hashed to.
    async fn apply_local_file(
        &self,
        src: &str,
        dest: &PosixPath,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        let declared = Path::new(src);
        // Quoted by every refusal below, and absolute even when the manifest wrote a relative path:
        // a reader looking at "notes.txt" cannot tell which directory it was looked for in.
        let quoted = local::absolute_source(&self.base_dir, declared);
        let grants = self.session.state().manifest().extra_path_grants.clone();
        // The source is read as a one-file directory: the parent is the root the walk pins itself
        // to, and the file name is the only child. That is what keeps one file and one file of a
        // copied directory on the same code path, and therefore under the same refusals.
        let parent = declared
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let Some(name) = declared.file_name() else {
            return Err(local_file_read(&quoted).with_context("reason", "path_not_found"));
        };

        let source = LocalSource::new(&self.base_dir, parent, &grants);
        let src_root = source
            .resolve_root()
            .map_err(|error| rewrap(error, &quoted))?;
        let file = source
            .open_file(&src_root, Path::new(name))
            .map_err(|error| rewrap(error, &quoted))?;
        let (bytes, sha256) = local::read_and_hash(file, &src_root.join(name))?;

        if let Some(parent) = parent_path(dest) {
            self.session.mkdir(parent.as_str(), true, None).await?;
        }
        self.session.write(dest.as_str(), bytes, None).await?;
        Ok(vec![MaterializedFile::new(dest.clone(), sha256)])
    }

    /// Copies a host directory entry into the workspace as `user`.
    ///
    /// The reference's `LocalDir.apply(..., user=...)`, the one entry type it materializes on behalf
    /// of a user: every directory and file is created as that user, and the entry's own group and
    /// permissions are applied only when there is none — a user who cannot `chgrp` or `chmod` what
    /// it was just handed would otherwise fail a copy that succeeded. `user` of `None` is the same
    /// as materializing the entry as part of a manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ra_core::sandbox::ErrorCode::LocalDirReadError`] for a source that cannot be
    /// read, a refusal for an entry that is not a host directory, and the session's failure to
    /// write.
    pub async fn apply_local_dir_as(
        &self,
        entry: &Entry,
        dest: &PosixPath,
        user: Option<&User>,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        let EntryContent::LocalDir { src } = entry.content() else {
            return Err(unsupported_entry(
                entry.entry_type(),
                "only a host directory is materialized on behalf of a user",
            ));
        };
        let written = self.apply_local_dir(src.as_deref(), dest, user).await?;
        if user.is_none() {
            self.apply_metadata(entry, dest).await?;
        }
        Ok(written)
    }

    /// Copies one host directory into the workspace, or just creates the directory.
    async fn apply_local_dir(
        &self,
        src: Option<&str>,
        dest: &PosixPath,
        user: Option<&User>,
    ) -> SandboxResult<Vec<MaterializedFile>> {
        let Some(src) = src else {
            self.session
                .mkdir(dest.as_str(), true, user.cloned())
                .await?;
            return Ok(Vec::new());
        };

        let grants = self.session.state().manifest().extra_path_grants.clone();
        let source = LocalSource::new(&self.base_dir, Path::new(src), &grants);
        let src_root = source.resolve_root()?;
        self.session
            .mkdir(dest.as_str(), true, user.cloned())
            .await?;

        let children = source.list_files(&src_root)?;
        let tasks = children
            .iter()
            .map(|child| self.copy_one(&source, &src_root, child, dest, user))
            .collect();
        gather_in_order(tasks, self.limits.local_dir_files()).await
    }

    /// Copies one file of a host directory into the workspace.
    async fn copy_one(
        &self,
        source: &LocalSource<'_>,
        src_root: &Path,
        rel_child: &Path,
        dest_root: &PosixPath,
        user: Option<&User>,
    ) -> SandboxResult<MaterializedFile> {
        let child_dest = dest_root.join(&rel_child.to_string_lossy());
        let file = source.open_file(src_root, rel_child)?;
        let (bytes, sha256) = local::read_and_hash(file, &src_root.join(rel_child))?;
        if let Some(parent) = parent_path(&child_dest) {
            self.session
                .mkdir(parent.as_str(), true, user.cloned())
                .await?;
        }
        self.session
            .write(child_dest.as_str(), bytes, user.cloned())
            .await?;
        Ok(MaterializedFile::new(child_dest, sha256))
    }

    /// Gives a materialized path the ownership and permissions the entry declared.
    async fn apply_metadata(&self, entry: &Entry, dest: &PosixPath) -> SandboxResult<()> {
        if let Some(owner) = entry.group() {
            let name = match owner {
                EntryOwner::Group(group) => group.name.clone(),
                EntryOwner::User(user) => user.name.clone(),
                // Both modelled forms are a name to hand the path to. A third one would mean
                // something else, and guessing which account it named is how content ends up owned
                // by the wrong one.
                _ => {
                    return Err(unsupported_entry(
                        entry.entry_type(),
                        "this build does not know what account this entry is owned by",
                    ));
                }
            };
            self.exec_checked(vec!["chgrp".to_owned(), name, dest.as_str().to_owned()])
                .await?;
        }
        // The directory bit is masked off: it says what kind of thing is at the path, which is not
        // something `chmod` is being asked to change.
        let mode = format!("{:04o}", entry.permissions().to_mode() & 0o7777);
        self.exec_checked(vec!["chmod".to_owned(), mode, dest.as_str().to_owned()])
            .await
            .map(|_| ())
    }

    /// Runs an argument vector, and treats a non-zero exit as a failure.
    async fn exec_checked(&self, command: Vec<String>) -> SandboxResult<ExecResult> {
        let result = self
            .session
            .exec(ExecRequest::new(command.clone()).with_shell(ShellInvocation::None))
            .await?;
        if result.ok() {
            Ok(result)
        } else {
            Err(SandboxError::exec_nonzero(result, command))
        }
    }
}

/// The entries that are not meant to survive a stop, each with where it was declared.
///
/// An ephemeral directory is taken whole, children and all, including any child that is not itself
/// ephemeral: the directory is the thing that was never persisted, so what is inside it was never
/// persisted either and has to be rebuilt with it. That is why a descendant of an already-selected
/// path is skipped rather than materialized a second time.
fn ephemeral_entries(manifest: &Manifest) -> SandboxResult<Vec<(PosixPath, &Entry)>> {
    let mut selected: Vec<(PosixPath, &Entry)> = Vec::new();
    for (path, entry) in manifest.iter_entries()? {
        if selected.iter().any(|(taken, _)| path.is_under(taken)) {
            continue;
        }
        if entry.is_ephemeral() {
            selected.push((path, entry));
        }
    }
    Ok(selected)
}

/// Whether one path is the other, or contains it.
fn paths_overlap(left: &PosixPath, right: &PosixPath) -> bool {
    left.is_under(right) || right.is_under(left)
}

/// The directory a path is in, or `None` when it names something at the filesystem root.
fn parent_path(path: &PosixPath) -> Option<PosixPath> {
    let parts = path.parts();
    if parts.len() < 2 {
        return None;
    }
    let parent = path.join("..").normalized();
    (parent != *path).then_some(parent)
}

/// Reports a one-file copy as a file failure, keeping what the directory walk found out.
///
/// The walk is shared with `local_dir`, so its refusals name a directory. A manifest that declared
/// one file should be told about that file — but `reason=symlink_not_supported` is why it failed,
/// and dropping it to re-wrap the error would leave a caller with no way to tell a missing source
/// from a redirected one.
fn rewrap(error: SandboxError, src: &Path) -> SandboxError {
    if error.error_code() != ra_core::sandbox::ErrorCode::LocalDirReadError {
        return error;
    }
    let mut rewrapped = local_file_read(src);
    for (key, value) in error.context() {
        if key != "src" {
            rewrapped = rewrapped.with_context(key.clone(), value.clone());
        }
    }
    rewrapped.with_sandbox_cause(error)
}
