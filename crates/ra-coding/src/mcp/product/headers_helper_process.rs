// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! Launches the configured local helper and owns its process tree.
use ra_core::error::{Error, Result};
use ra_mcp::client::http::HttpHeadersHelperProcessError;
use std::{ffi::OsString, path::Path, process::Stdio};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

pub(super) async fn run(
    script: &str,
    cwd: &Path,
    core_env: Vec<(OsString, OsString)>,
    max_stdout: usize,
) -> Result<Vec<u8>> {
    #[cfg(windows)]
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    #[cfg(not(windows))]
    let shell = "sh";

    let mut command = Command::new(shell);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.args(["/Q", "/D", "/C"]);
        command.as_std_mut().raw_arg(format!(r#""{script}""#));
    }
    #[cfg(not(windows))]
    command.args(["-c", script]);
    #[cfg(unix)]
    command.process_group(0);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(cwd)
        .env_clear()
        .envs(core_env)
        .envs(super::stdio_env(None, &[])?)
        .kill_on_drop(true);

    let failed = || Error::config("MCP HTTP headers helper execution failed");
    let failed_to_start = |error: std::io::Error| {
        failed().with_source(HttpHeadersHelperProcessError::Start(error.kind()))
    };
    #[cfg(windows)]
    let (child, job) = ra_exec::process::JobObject::spawn_background_contained(&mut command)
        .map_err(failed_to_start)?;
    #[cfg(not(windows))]
    let child = command.spawn().map_err(failed_to_start)?;
    #[cfg(unix)]
    let group = child.id().ok_or_else(failed)?;
    let mut process = HelperProcess {
        child,
        #[cfg(unix)]
        group,
        #[cfg(windows)]
        job,
    };
    let stdout = process.child.stdout.take().ok_or_else(failed)?;
    let mut bytes = Vec::new();
    stdout
        .take(max_stdout as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| failed())?;
    // The adapter reports oversized output without waiting for a writer blocked on stdout.
    if bytes.len() <= max_stdout {
        let status = process.child.wait().await.map_err(|_| failed())?;
        if !status.success() {
            return Err(failed().with_source(HttpHeadersHelperProcessError::Exit(status)));
        }
    }
    Ok(bytes)
}

struct HelperProcess {
    child: Child,
    #[cfg(unix)]
    group: u32,
    #[cfg(windows)]
    job: ra_exec::process::JobObject,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = ra_exec::process::kill_process_group(self.group);
        #[cfg(windows)]
        let _ = self.job.terminate();
        let _ = self.child.start_kill();
    }
}
