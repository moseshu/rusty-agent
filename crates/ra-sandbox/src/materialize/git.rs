//! Materializing a git checkout, using the sandbox's own `git`.
//!
//! Unlike a copied host directory, nothing here reads the SDK's filesystem: the repository is
//! fetched by commands run *inside* the sandbox, into a temporary directory the checkout owns, and
//! copied from there into the workspace. That is why the receipt comes back empty — hashing the
//! result would mean reading every file back out of the sandbox, which the reference declines to
//! pay for and so does this.
//!
//! # Two ways to ask for a ref
//!
//! A ref that looks like a commit cannot be cloned with `--branch`, so it is fetched into an
//! initialised repository instead. "Looks like" is deliberately loose — seven to forty hex
//! characters — and a name that happens to be hex (`deadbeef` as a branch) is why the fetch falling
//! through to a named clone is not a fallback to be tidied away: it is how a repository with such a
//! branch still resolves. The error reported when both fail is the *fetch's*, because that is the
//! attempt that matched what the ref looked like.

use std::sync::{Arc, Mutex};

use ra_core::sandbox::{
    ExecRequest, ExecResult, PosixPath, SandboxError, SandboxResult, SandboxSession,
    ShellInvocation,
};
use uuid::Uuid;

use super::errors::{git_clone, git_copy, git_missing, git_subpath};

/// Owned checkout tasks whose cancellation cleanup must finish before a batch returns.
#[derive(Default)]
pub(super) struct CleanupTasks(Mutex<Vec<tokio::task::JoinHandle<()>>>);

impl CleanupTasks {
    pub(super) async fn wait(self) {
        let tasks = self
            .0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for task in tasks {
            if let Err(error) = task.await {
                tracing::warn!(%error, "checkout cleanup task failed");
            }
        }
    }
}

/// What a checkout entry declares.
pub(crate) struct GitCheckout<'a> {
    /// The host to clone from.
    pub(crate) host: &'a str,
    /// The repository path on that host.
    pub(crate) repo: &'a str,
    /// The tag, branch or commit to take.
    pub(crate) reference: &'a str,
    /// A directory inside the repository to take instead of the whole of it.
    pub(crate) subpath: Option<&'a str>,
}

impl GitCheckout<'_> {
    /// Fetches the repository and copies it into `dest`.
    ///
    /// # Errors
    ///
    /// Returns [`ra_core::sandbox::ErrorCode::GitSubpathError`] for a subpath that does not name
    /// somewhere inside the repository, [`ra_core::sandbox::ErrorCode::GitMissingInImage`] when the
    /// sandbox has no `git`, [`ra_core::sandbox::ErrorCode::GitCloneError`] when the ref cannot be
    /// fetched, and [`ra_core::sandbox::ErrorCode::GitCopyError`] when the fetched tree cannot be
    /// copied into the workspace.
    pub(crate) async fn apply(
        &self,
        session: Arc<dyn SandboxSession>,
        dest: &PosixPath,
        cleanup: &CleanupTasks,
    ) -> SandboxResult<()> {
        let subpath = self.validated_subpath()?;

        let probe = exec(
            session.as_ref(),
            vec!["command -v git >/dev/null 2>&1".to_owned()],
        )
        .await?;
        if !probe.ok() {
            return Err(git_missing(self.repo, self.reference));
        }

        let temporary = format!(
            "/tmp/sandbox-git-{}-{}",
            session.state().session_id().simple(),
            Uuid::new_v4().simple()
        );
        remove(session.as_ref(), &temporary).await?;

        // A borrowed session cannot outlive a dropped application future. Own the checkout and
        // session in a worker so cancellation can stop the operation and still await cleanup.
        let host = self.host.to_owned();
        let repo = self.repo.to_owned();
        let reference = self.reference.to_owned();
        let declared_subpath = self.subpath.map(str::to_owned);
        let dest = dest.clone();
        let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
        let (completed, result) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let checkout = GitCheckout {
                host: &host,
                repo: &repo,
                reference: &reference,
                subpath: declared_subpath.as_deref(),
            };
            let url = format!("https://{host}/{repo}.git");
            let operation = checkout.fetch_and_copy(
                session.as_ref(),
                &url,
                &temporary,
                subpath.as_ref(),
                &dest,
            );
            let outcome = tokio::select! {
                biased;
                _ = cancelled => None,
                outcome = operation => Some(outcome),
            };
            // The operation future has been dropped before cleanup begins. Backends must stop
            // cancelled exec operations so a checkout cannot recreate files after this removal.
            let removed = remove(session.as_ref(), &temporary).await;
            if let Some(outcome) = outcome {
                let _ = completed.send(removed.and(outcome));
            } else if let Err(error) = removed {
                tracing::warn!(%error, "cancelled checkout cleanup failed");
            }
        });
        cleanup
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        let outcome = result.await.map_err(|error| {
            SandboxError::exec_transport(
                Vec::new(),
                Some(&format!("checkout task failed: {error}")),
            )
        })?;
        drop(cancel_on_drop);
        outcome
    }

    /// Fetches into `temporary` and copies the requested tree out of it.
    async fn fetch_and_copy(
        &self,
        session: &dyn SandboxSession,
        url: &str,
        temporary: &str,
        subpath: Option<&PosixPath>,
        dest: &PosixPath,
    ) -> SandboxResult<()> {
        let mut clone = if looks_like_commit(self.reference) {
            self.fetch_commit(session, url, temporary).await?
        } else {
            self.clone_named_ref(session, url, temporary).await?
        };
        let mut fetch_failure = None;
        if !clone.ok() && looks_like_commit(self.reference) {
            fetch_failure = Some(clone);
            remove(session, temporary).await?;
            clone = self.clone_named_ref(session, url, temporary).await?;
        }
        if !clone.ok() {
            let reported = fetch_failure.unwrap_or(clone);
            let mut error = git_clone(
                url,
                self.reference,
                &String::from_utf8_lossy(&reported.stderr),
            )
            .with_context("repo", self.repo);
            if let Some(subpath) = self.subpath {
                error = error.with_context("subpath", subpath);
            }
            return Err(error);
        }

        let src_root = subpath.map_or_else(
            || temporary.to_owned(),
            |subpath| format!("{temporary}/{}", subpath.as_str()),
        );
        session.mkdir(dest.as_str(), true, None).await?;
        let copy = exec_direct(
            session,
            vec![
                "cp".to_owned(),
                "-R".to_owned(),
                "--".to_owned(),
                format!("{src_root}/."),
                format!("{dest}/"),
            ],
        )
        .await?;
        if !copy.ok() {
            let mut error = git_copy(
                &src_root,
                dest.as_str(),
                &String::from_utf8_lossy(&copy.stderr),
            )
            .with_context("repo", self.repo)
            .with_context("ref", self.reference);
            if let Some(subpath) = self.subpath {
                error = error.with_context("subpath", subpath);
            }
            return Err(error);
        }
        Ok(())
    }

    /// The subpath, checked to name somewhere inside the repository.
    ///
    /// `None` covers the spellings that all mean the whole repository — absent, empty, and anything
    /// that tidies down to `.` — so the copy is taken from the checkout root rather than from a
    /// path with a stray `.` in the middle of it.
    fn validated_subpath(&self) -> SandboxResult<Option<PosixPath>> {
        let Some(declared) = self.subpath else {
            return Ok(None);
        };
        if declared.is_empty() {
            return Ok(None);
        }
        let trimmed = declared.trim();
        if trimmed.is_empty() {
            return Err(git_subpath(self.repo, declared, "empty"));
        }
        let subpath = PosixPath::new(trimmed);
        if subpath.as_str() == "." {
            return Ok(None);
        }
        if subpath.is_absolute() {
            return Err(git_subpath(self.repo, declared, "absolute"));
        }
        // Checked against what was written rather than the tidied form: a backslash is a separator
        // on one platform and an ordinary character in a filename on this one, and a subpath that
        // means two different directories depending on where it is read is not one to resolve.
        if declared.contains('\\') || ra_core::sandbox::windows_absolute_path(trimmed).is_some() {
            return Err(git_subpath(self.repo, declared, "windows_path"));
        }
        if subpath.parts().contains(&"..") {
            return Err(git_subpath(self.repo, declared, "parent_traversal"));
        }
        Ok(Some(subpath))
    }

    /// Clones one named ref at depth one.
    async fn clone_named_ref(
        &self,
        session: &dyn SandboxSession,
        url: &str,
        temporary: &str,
    ) -> SandboxResult<ExecResult> {
        exec_direct(
            session,
            vec![
                "git".to_owned(),
                "clone".to_owned(),
                "--depth".to_owned(),
                "1".to_owned(),
                "--no-tags".to_owned(),
                "--branch".to_owned(),
                self.reference.to_owned(),
                url.to_owned(),
                temporary.to_owned(),
            ],
        )
        .await
    }

    /// Fetches one commit into a repository initialised for the purpose.
    ///
    /// Returns the first step that failed, so the caller reports the command that actually broke
    /// rather than the last one in the sequence.
    async fn fetch_commit(
        &self,
        session: &dyn SandboxSession,
        url: &str,
        temporary: &str,
    ) -> SandboxResult<ExecResult> {
        let steps: [Vec<String>; 3] = [
            vec!["git".to_owned(), "init".to_owned(), temporary.to_owned()],
            vec![
                "git".to_owned(),
                "-C".to_owned(),
                temporary.to_owned(),
                "remote".to_owned(),
                "add".to_owned(),
                "origin".to_owned(),
                url.to_owned(),
            ],
            vec![
                "git".to_owned(),
                "-C".to_owned(),
                temporary.to_owned(),
                "fetch".to_owned(),
                "--depth".to_owned(),
                "1".to_owned(),
                "--no-tags".to_owned(),
                "origin".to_owned(),
                self.reference.to_owned(),
            ],
        ];
        for step in steps {
            let result = exec_direct(session, step).await?;
            if !result.ok() {
                return Ok(result);
            }
        }
        exec_direct(
            session,
            vec![
                "git".to_owned(),
                "-C".to_owned(),
                temporary.to_owned(),
                "checkout".to_owned(),
                "--detach".to_owned(),
                "FETCH_HEAD".to_owned(),
            ],
        )
        .await
    }
}

/// Whether a ref is spelled the way a commit is.
fn looks_like_commit(reference: &str) -> bool {
    (7..=40).contains(&reference.len()) && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Removes the checkout's temporary directory, whether or not it is there.
async fn remove(session: &dyn SandboxSession, path: &str) -> SandboxResult<()> {
    exec_direct(
        session,
        vec![
            "rm".to_owned(),
            "-rf".to_owned(),
            "--".to_owned(),
            path.to_owned(),
        ],
    )
    .await
    .map(|_| ())
}

/// Runs a command line through the session's shell.
async fn exec(session: &dyn SandboxSession, command: Vec<String>) -> SandboxResult<ExecResult> {
    session.exec(ExecRequest::new(command)).await
}

/// Runs an argument vector with no shell between.
async fn exec_direct(
    session: &dyn SandboxSession,
    command: Vec<String>,
) -> SandboxResult<ExecResult> {
    session
        .exec(ExecRequest::new(command).with_shell(ShellInvocation::None))
        .await
}
