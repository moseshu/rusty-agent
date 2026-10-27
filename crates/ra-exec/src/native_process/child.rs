// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Local child ownership and platform selection, independent of process transports.
//!
//! Every child exposes Tokio stdio handles. Reap-only children retain ownership until
//! reaped, including when a wait is cancelled or the async runtime shuts down.

use std::io;
use std::process::ExitStatus;

use tokio::io::AsyncReadExt;
use tokio::process::ChildStderr;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "posix_child.rs"]
pub(super) mod posix;

#[cfg(unix)]
#[path = "child_reaper.rs"]
pub(super) mod reaper;

/// A local subprocess with owned stdio and cancellation-safe exit handling.
pub(crate) struct Child {
    pub(super) inner: ChildKind,
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

pub(super) enum ChildKind {
    Tokio(tokio::process::Child),
    #[cfg(unix)]
    TokioReapOnly {
        child: Option<tokio::process::Child>,
        reaper: std::sync::mpsc::Sender<reaper::ChildToReap>,
    },
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Native(posix::NativeChild),
}

impl Child {
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn id(&self) -> Option<u32> {
        match &self.inner {
            ChildKind::Tokio(child) => child.id(),
            #[cfg(unix)]
            ChildKind::TokioReapOnly { child, .. } => {
                child.as_ref().and_then(tokio::process::Child::id)
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            ChildKind::Native(child) => child.id(),
        }
    }

    /// Close retained stdin and wait without giving up ownership on cancellation.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.stdin.take();
        match &mut self.inner {
            ChildKind::Tokio(child) => child.wait().await,
            #[cfg(unix)]
            ChildKind::TokioReapOnly { child, .. } => {
                child
                    .as_mut()
                    .ok_or_else(|| io::Error::other("child already transferred to reaper"))?
                    .wait()
                    .await
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            ChildKind::Native(child) => child.wait().await,
        }
    }

    /// Drain both output pipes while waiting, retaining the configured drop policy on cancellation.
    pub(crate) async fn wait_with_output(mut self) -> io::Result<std::process::Output> {
        let mut stdout = self.stdout.take();
        let mut stderr = self.stderr.take();
        let mut output = Vec::new();
        let mut diagnostic = Vec::new();
        let (status, (), ()) = tokio::try_join!(
            self.wait(),
            async {
                if let Some(stdout) = stdout.as_mut() {
                    stdout.read_to_end(&mut output).await?;
                }
                Ok::<_, io::Error>(())
            },
            async {
                if let Some(stderr) = stderr.as_mut() {
                    stderr.read_to_end(&mut diagnostic).await?;
                }
                Ok::<_, io::Error>(())
            },
        )?;
        // Keep the pipes open until the child exits, even after EOF, matching Tokio.
        // See https://github.com/tokio-rs/tokio/issues/4309.
        drop(stdout);
        drop(stderr);
        Ok(std::process::Output {
            status,
            stdout: output,
            stderr: diagnostic,
        })
    }

    /// Kill the direct child and reap it. Process-tree policy belongs to the caller.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.stdin.take();
        match &mut self.inner {
            ChildKind::Tokio(child) => child.kill().await,
            #[cfg(unix)]
            ChildKind::TokioReapOnly { child, .. } => {
                child
                    .as_mut()
                    .ok_or_else(|| io::Error::other("child already transferred to reaper"))?
                    .kill()
                    .await
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            ChildKind::Native(child) => child.kill().await,
        }
    }
}

impl Drop for ChildKind {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Self::TokioReapOnly { child, reaper } = self
            && let Some(child) = child.take()
            && child.id().is_some()
        {
            // Transfer the handle, not only its PID: Tokio must not enqueue it
            // into a stopped runtime's orphan queue before our worker reaps it.
            let _ = reaper.send(reaper::ChildToReap::Tokio(child));
        }
    }
}
