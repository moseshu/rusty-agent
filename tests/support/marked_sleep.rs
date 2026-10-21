//! A long-running command a test can find on the host by its command line.
//!
//! A command cannot report its own host PID with `$$` here: under a sandbox that gives commands
//! their own PID namespace — bubblewrap on Linux — `$$` is the namespace's number, which on the
//! host names an unrelated process or none at all. The marked command line is visible from the
//! host whatever namespace the process runs in.

use std::{
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// A `sleep` whose duration no other process in this or a concurrent test run shares.
pub struct MarkedSleep {
    seconds: String,
}

impl MarkedSleep {
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            seconds: format!("30.{}{nanos:09}{sequence}", std::process::id()),
        }
    }

    /// The shell command that replaces the shell with the marked sleep.
    pub fn command(&self) -> String {
        format!("exec sleep {}", self.seconds)
    }

    /// Whether the marked sleep is still running.
    pub fn is_alive(&self) -> bool {
        let pattern = format!("^sleep {}$", self.seconds.replace('.', "\\."));
        Command::new("pgrep")
            .args(["-f", &pattern])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
