//! `doctor sandbox`: what will confine this machine's commands, and proof that it does.
//!
//! **A self-check that only reports is not worth running.** "`/usr/bin/sandbox-exec` exists" is not
//! the question anyone has — the question is whether a command started through this product can
//! still write outside the workspace, and the only honest way to answer it is to try. So the report
//! below ends with a live check: a real command, under the real policy, attempting a real escape in
//! a directory created for the purpose.
//!
//! It lives in the product layer for the same reason [`crate::prompt::dump`] does. Which level a
//! host asks for and what the report says about it are product decisions; `ra-exec` owns the
//! mechanism and knows none of them, and the layering gate keeps the binary from reaching past this
//! crate to assemble a report of its own.

use std::fmt::Write as _;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use ra_core::error::{Error, Result, SandboxErrorKind};
use ra_exec::{
    command::{ExecLimits, ExecRequest},
    sandbox::{
        ExecEnvironment, NetworkAccess, SandboxBackend, SandboxLevel, SandboxPolicy,
        compiled_backend_names, platform_backend,
    },
    session::{ExecError, ExecExecutionResult, ProcessManager},
};

/// How one check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    /// The machine did what it should.
    Ok,
    /// It did not, and commands run here are not confined the way this report claims.
    Failed,
    /// Nothing was checked, because something earlier made the check meaningless.
    Skipped,
}

impl CheckResult {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "FAILED",
            Self::Skipped => "skipped",
        }
    }
}

/// One line of the self-check.
#[derive(Debug, Clone)]
pub struct Check {
    /// What was attempted, in the words of someone who would have to act on a failure.
    pub what: String,
    /// How it came out.
    pub result: CheckResult,
    /// What was actually observed, when that is not obvious from the result.
    pub detail: String,
}

impl Check {
    fn new(what: impl Into<String>, result: CheckResult, detail: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            result,
            detail: detail.into(),
        }
    }
}

/// What the report concludes, once every check has had its say.
///
/// **Three states, not two.** "Not confined" and "could not be checked" are different facts about a
/// machine, and collapsing them means telling someone whose image simply has no `bash` that their
/// sandbox does not work — a claim the evidence does not support. Both are still failures to prove
/// confinement, and both exit non-zero; what differs is what the reader should go and fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every check passed. Commands started here are confined as the report describes.
    Confined,
    /// A check failed, or nothing on this machine can confine a command at all.
    NotConfined,
    /// Nothing failed, but something could not be checked, so confinement is unproven.
    Unverified,
}

/// The whole report.
#[derive(Debug, Clone)]
pub struct SandboxDoctor {
    backend: Option<&'static str>,
    availability: std::result::Result<SandboxLevel, String>,
    checks: Vec<Check>,
    notes: Vec<String>,
}

impl SandboxDoctor {
    /// The backend this platform would use, if this build has one for it.
    #[must_use]
    pub const fn backend(&self) -> Option<&'static str> {
        self.backend
    }

    /// The strongest level the backend can deliver here, or why it can deliver none.
    pub fn availability(&self) -> std::result::Result<SandboxLevel, &str> {
        self.availability.as_ref().copied().map_err(String::as_str)
    }

    /// Every check the report ran.
    #[must_use]
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// Anything the report wants to say that is not a check, such as a directory it left behind.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// What this report concludes.
    ///
    /// A skipped check is never a pass: it is skipped exactly when the thing it would have proven
    /// went unproven. It is not a failure either — see [`Verdict`].
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.availability.is_err()
            || self
                .checks
                .iter()
                .any(|check| check.result == CheckResult::Failed)
        {
            return Verdict::NotConfined;
        }
        if self
            .checks
            .iter()
            .any(|check| check.result == CheckResult::Skipped)
        {
            return Verdict::Unverified;
        }
        Verdict::Confined
    }

    /// Whether this machine was shown to confine a command the way the product expects.
    ///
    /// Only [`Verdict::Confined`] counts. Both other verdicts mean the report could not make the
    /// claim, and a caller that must not run unconfined treats them identically.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.verdict() == Verdict::Confined
    }
}

/// Runs the self-check and renders it.
///
/// # Errors
///
/// Returns an error only when the check itself could not be carried out — a scratch directory that
/// cannot be created, say. A machine whose sandbox does not work is a successful report with a
/// failed check in it, not an error: the command's job was to find out, and it did.
pub async fn render_sandbox_doctor() -> Result<(String, SandboxDoctor)> {
    let doctor = run_sandbox_doctor().await?;
    Ok((render(&doctor), doctor))
}

/// Runs the self-check.
///
/// # Errors
///
/// Returns an error when the check could not be set up; see [`render_sandbox_doctor`].
pub async fn run_sandbox_doctor() -> Result<SandboxDoctor> {
    let backend = platform_backend();
    let availability = match backend {
        Some(backend) => backend.available_level().map_err(|error| error.to_string()),
        None => Err(format!(
            "this build has no sandbox backend for {}",
            std::env::consts::OS
        )),
    };

    let (checks, notes) = match availability {
        Ok(level) => live_checks(level).await?,
        Err(_) => (
            vec![Check::new(
                "confine a real command",
                CheckResult::Skipped,
                "there is no backend to confine it with",
            )],
            Vec::new(),
        ),
    };

    Ok(SandboxDoctor {
        backend: backend.map(SandboxBackend::name),
        availability,
        checks,
        notes,
    })
}

/// The first thing every probe prints, and the evidence that it ran at all.
///
/// **Under a real backend, a missing shell does not look like a failure to start.** The program
/// actually launched is the sandbox wrapper — `sandbox-exec`, or bubblewrap — and that program
/// exists: it starts, fails to exec the shell inside, and exits non-zero. From the outside that is
/// indistinguishable from a command that ran and was refused, so a machine whose image simply has
/// no `bash` was being reported as a sandbox that does not confine. Measured as exit 71 under real
/// Seatbelt.
///
/// So "did the probe run" is answered by this marker rather than by the exit status. It is printed
/// before the probe attempts anything, which is also why a genuine denial always carries it: the
/// shell reached the first statement, then hit the wall.
const PROBE_READY: &str = "RA_PROBE_READY";

/// Whether the shell got as far as the probe's first statement.
fn probe_started(summary: &ra_exec::output::ExecOutputSummary) -> bool {
    summary.stdout().contains(PROBE_READY)
}

/// Why a probe produced no usable answer.
///
/// The distinction is the whole point: a probe that never started says nothing about the sandbox,
/// while a probe that ran and gave the wrong answer says a great deal. Reporting the first as a
/// failure is how a machine with no `bash` gets told its sandbox is broken.
#[derive(Debug)]
enum ProbeFailure {
    /// The probe program could not be started — a shell that is not there. Not the sandbox's doing.
    Unavailable(String),
    /// It ran, or tried to, and reached no conclusion.
    Failed(String),
}

impl ProbeFailure {
    const fn detail(&self) -> &String {
        match self {
            Self::Unavailable(detail) | Self::Failed(detail) => detail,
        }
    }

    /// How a check that depended on this probe should be recorded.
    const fn result(&self) -> CheckResult {
        match self {
            Self::Unavailable(_) => CheckResult::Skipped,
            Self::Failed(_) => CheckResult::Failed,
        }
    }
}

/// What one probe produced.
type ProbeResult = std::result::Result<ra_exec::output::ExecOutputSummary, ProbeFailure>;

/// Actually confines a command and watches what it can and cannot do.
async fn live_checks(level: SandboxLevel) -> Result<(Vec<Check>, Vec<String>)> {
    let (workspace_guard, workspace) = scratch_dir_in(&std::env::temp_dir())?;
    let (outside_guard, outside) = scratch_dir_in(&std::env::temp_dir())?;

    let policy = SandboxPolicy::new(level)
        .with_network(NetworkAccess::Denied)
        .with_writable_root(&workspace)
        .with_readable_root(&workspace);
    let manager = ProcessManager::default()
        .with_environment(ExecEnvironment::new().with_sandbox(policy.clone()));

    let mut checks = Vec::new();

    let inside = run(
        &manager,
        &workspace,
        &format!("echo {PROBE_READY}; echo confined > inside.txt"),
    )
    .await;
    checks.push(match inside {
        Ok(summary)
            if probe_started(&summary)
                && summary.exit_code() == Some(0)
                && workspace.join("inside.txt").is_file() =>
        {
            Check::new(
                "write inside the workspace root",
                CheckResult::Ok,
                "allowed, as it must be",
            )
        }
        Ok(summary) if !probe_started(&summary) => Check::new(
            "write inside the workspace root",
            CheckResult::Skipped,
            format!(
                "the probe never reached its first statement, so nothing was tried: exit {:?}: {}",
                summary.exit_code(),
                first_line(summary.stderr())
            ),
        ),
        Ok(summary) => Check::new(
            "write inside the workspace root",
            CheckResult::Failed,
            format!(
                "the sandbox is confining more than it should; the command said: {}",
                first_line(summary.stderr())
            ),
        ),
        Err(failure) => Check::new(
            "write inside the workspace root",
            failure.result(),
            failure.detail().clone(),
        ),
    });

    let escape_path = outside.join("escaped.txt");
    let escape = ExecRequest::new(format!(
        "echo {PROBE_READY}; if (echo escaped > \"$1\"); then exit 0; else exit 42; fi"
    ))
    .with_cwd(&workspace)
    .with_args(vec![
        "doctor".to_owned(),
        escape_path.to_string_lossy().into_owned(),
    ]);
    let escaped = run_request(&manager, escape).await;
    checks.push(if escape_path.exists() {
        Check::new(
            "write outside every writable root",
            CheckResult::Failed,
            format!("the file was created at {}", escape_path.display()),
        )
    } else {
        denial_check("write outside every writable root", &escaped)
    });

    let allowed_manager = ProcessManager::default().with_environment(
        ExecEnvironment::new().with_sandbox(policy.with_network(NetworkAccess::Allowed)),
    );
    checks.push(
        network_check(
            &manager,
            &allowed_manager,
            &workspace,
            Path::new("/bin/bash"),
        )
        .await,
    );

    // Cancellation is asynchronous. Preserve directories if a probe has not been reaped yet.
    // No unchecked recursive deletion is performed on a canonicalized caller-supplied path.
    let mut notes = Vec::new();
    if !manager.active_sessions().await.is_empty()
        || !allowed_manager.active_sessions().await.is_empty()
    {
        let kept = workspace_guard.keep();
        let also_kept = outside_guard.keep();
        // Said out loud rather than left in `/tmp` for someone to find: a probe still being reaped
        // may still be writing here, so deleting now would be deleting a live directory — but a
        // command that quietly leaves two directories behind on every run is its own small problem.
        notes.push(format!(
            "left {} and {} in place: a probe had not been reaped yet, so removing them was not safe",
            kept.display(),
            also_kept.display()
        ));
    }

    Ok((checks, notes))
}

/// Opens a listener on this machine and checks that the confined command cannot reach it.
///
/// A name on the internet would not do: the check has to tell "the sandbox refused" apart from
/// "this machine is offline", and only a socket opened here can. The listener is closed as soon as
/// the probe has run.
async fn network_check(
    manager: &ProcessManager,
    allowed_manager: &ProcessManager,
    cwd: &Path,
    shell: &Path,
) -> Check {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
        return Check::new(
            "reach the network",
            CheckResult::Skipped,
            "no loopback listener could be opened to probe against",
        );
    };
    let Ok(address) = listener.local_addr() else {
        return Check::new(
            "reach the network",
            CheckResult::Skipped,
            "the probe listener has no address",
        );
    };
    if TcpStream::connect(address).is_err() {
        return Check::new(
            "reach the network",
            CheckResult::Skipped,
            "the probe listener is not reachable from here, so a denial would prove nothing",
        );
    }

    let port = address.port();
    let probe = format!(
        "echo {PROBE_READY}; if (exec 3<>/dev/tcp/127.0.0.1/{port}); then exit 0; else exit 42; fi"
    );
    let request = ExecRequest::new(probe)
        .with_cwd(cwd)
        .with_shell(Some(shell));
    let control = run_request(allowed_manager, request.clone()).await;
    match control {
        Ok(summary) if summary.exit_code() == Some(0) && probe_started(&summary) => {}
        // No probe ran, so nothing was learned either way. A host whose image has no `bash` is not
        // a host whose sandbox leaks, and saying so would send someone to fix the wrong thing.
        // Under a backend this arrives as a wrapper that started and exited non-zero without the
        // marker; with no backend it arrives as a failure to spawn. Both mean the same thing.
        Ok(summary) if !probe_started(&summary) => {
            return Check::new(
                "reach the network",
                CheckResult::Skipped,
                format!(
                    "the probe shell never reached its first statement, so nothing was tried: \
                     exit {:?}: {}",
                    summary.exit_code(),
                    first_line(summary.stderr())
                ),
            );
        }
        Err(ProbeFailure::Unavailable(detail)) => {
            return Check::new(
                "reach the network",
                CheckResult::Skipped,
                format!("the probe shell could not be started, so nothing was tried: {detail}"),
            );
        }
        other => {
            return Check::new(
                "reach the network",
                CheckResult::Failed,
                format!(
                    "the allowed-network control probe did not connect: {}",
                    probe_detail(&other)
                ),
            );
        }
    }
    let denied = run_request(manager, request).await;
    drop(listener);
    denial_check("reach the network", &denied)
}

/// A denial needs evidence that the intended operation was attempted by the shell.
fn denial_check(what: &str, result: &ProbeResult) -> Check {
    if matches!(result, Ok(summary) if summary.exit_code() == Some(42) && probe_started(summary)) {
        return Check::new(what, CheckResult::Ok, "denied");
    }
    let unavailable = match result {
        // The wrapper ran and the shell inside it did not, which says nothing about confinement.
        Ok(summary) if !probe_started(summary) => Some(format!(
            "the probe never reached its first statement: exit {:?}: {}",
            summary.exit_code(),
            first_line(summary.stderr())
        )),
        Err(failure @ ProbeFailure::Unavailable(_)) => Some(failure.detail().clone()),
        _ => None,
    };
    if let Some(detail) = unavailable {
        return Check::new(
            what,
            CheckResult::Skipped,
            format!("the probe could not be run, so nothing was tried: {detail}"),
        );
    }
    Check::new(
        what,
        CheckResult::Failed,
        format!("denial was not established: {}", probe_detail(result)),
    )
}

fn probe_detail(result: &ProbeResult) -> String {
    match result {
        Ok(summary) => format!(
            "exit {:?}: {}",
            summary.exit_code(),
            first_line(summary.stderr())
        ),
        Err(failure) => failure.detail().clone(),
    }
}

async fn run(manager: &ProcessManager, cwd: &Path, command: &str) -> ProbeResult {
    run_request(manager, ExecRequest::new(command).with_cwd(cwd)).await
}

/// A yielded probe is cancelled and reported as inconclusive, never counted as a denial.
///
/// A spawn failure is [`ProbeFailure::Unavailable`] and everything else is
/// [`ProbeFailure::Failed`] — including a sandbox that could not be set up, which is a real
/// inability to confine rather than a probe that was never run.
async fn run_request(manager: &ProcessManager, request: ExecRequest) -> ProbeResult {
    let request = request.with_login(false).with_limits(
        ExecLimits::default()
            .with_initial_yield_timeout(Duration::from_secs(2))
            .with_total_timeout(Some(Duration::from_secs(3))),
    );
    match manager.execute(request, None).await {
        Ok(ExecExecutionResult::Completed(summary)) => Ok(summary),
        Ok(ExecExecutionResult::Yielded { session_id, .. }) => {
            manager.cancel(&session_id).await;
            Err(ProbeFailure::Failed(
                "the probe did not finish before its deadline".to_owned(),
            ))
        }
        Err(error @ ExecError::Spawn { .. }) => Err(ProbeFailure::Unavailable(format!(
            "the probe program could not be started: {error}"
        ))),
        Err(error) => Err(ProbeFailure::Failed(format!(
            "the probe could not run: {error}"
        ))),
        Ok(_) => Err(ProbeFailure::Failed(
            "the probe did not complete".to_owned(),
        )),
    }
}

/// Owns only a fresh, private directory; existing directories and symlinks are never adopted.
fn scratch_dir_in(parent: &Path) -> Result<(tempfile::TempDir, std::path::PathBuf)> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("ra-doctor-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let guard = builder.tempdir_in(parent).map_err(|error| {
        Error::sandbox(
            SandboxErrorKind::Setup,
            format!("cannot create doctor scratch directory: {error}"),
        )
    })?;
    let path = guard.path().canonicalize().map_err(|error| {
        Error::sandbox(
            SandboxErrorKind::Setup,
            format!("cannot resolve doctor scratch directory: {error}"),
        )
    })?;
    Ok((guard, path))
}

/// Fault-injection entry points for the external integration-test workspace.
#[cfg(feature = "test-api")]
pub mod test_api {
    use super::{Check, Path, ProcessManager, Result};

    /// Runs the network check with a supplied shell to exercise missing or broken probes.
    pub async fn network_check(
        manager: &ProcessManager,
        allowed: &ProcessManager,
        cwd: &Path,
        shell: &Path,
    ) -> Check {
        super::network_check(manager, allowed, cwd, shell).await
    }

    /// Creates a scratch directory under a controlled parent for ownership tests.
    ///
    /// The guard is `tempfile`'s own type on purpose: the test has to drop it to observe that the
    /// directory goes away, and wrapping it would hide the only behaviour under test. That does put
    /// a third-party type in a signature — acceptable here, where the whole module exists behind a
    /// feature no shipped build turns on.
    pub fn scratch_dir_in(parent: &Path) -> Result<(tempfile::TempDir, std::path::PathBuf)> {
        super::scratch_dir_in(parent)
    }

    /// Evaluates a report containing an injected check.
    #[must_use]
    pub fn healthy_with(check: Check) -> bool {
        report_with(check).is_healthy()
    }

    /// The verdict a report containing an injected check would reach.
    #[must_use]
    pub fn verdict_with(check: Check) -> super::Verdict {
        report_with(check).verdict()
    }

    fn report_with(check: Check) -> super::SandboxDoctor {
        super::SandboxDoctor {
            backend: Some("test"),
            availability: Ok(super::SandboxLevel::Isolated),
            checks: vec![check],
            notes: Vec::new(),
        }
    }
}

/// The first line of a message, which is the part that says what happened.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_owned()
}

/// Renders the report.
fn render(doctor: &SandboxDoctor) -> String {
    let mut out = String::from("sandbox\n");
    let _ = writeln!(
        out,
        "  backend      {}",
        doctor.backend.unwrap_or("none for this platform")
    );
    match &doctor.availability {
        Ok(level) => {
            let _ = writeln!(
                out,
                "  delivers     {level} (the strongest level available here)"
            );
        }
        Err(reason) => {
            let _ = writeln!(out, "  delivers     nothing: {reason}");
        }
    }
    let _ = writeln!(out, "  compiled     {}", compiled_backends());

    out.push_str("\n  levels\n");
    for (level, description) in LEVEL_DESCRIPTIONS {
        let _ = writeln!(out, "    {level:<16}{description}");
    }

    out.push_str("\n  checks\n");
    for check in &doctor.checks {
        let _ = writeln!(
            out,
            "    {:<8}{}\n             {}",
            check.result.label(),
            check.what,
            check.detail
        );
    }

    if !doctor.notes.is_empty() {
        out.push_str("\n  notes\n");
        for note in &doctor.notes {
            let _ = writeln!(out, "    {note}");
        }
    }

    let _ = writeln!(
        out,
        "\n  {}",
        match doctor.verdict() {
            Verdict::Confined => {
                "commands started here are confined as this report describes."
            }
            Verdict::NotConfined => {
                "commands started here are NOT confined as a host asking for a sandbox would expect."
            }
            Verdict::Unverified => {
                "confinement could NOT be verified here: nothing failed, but a check could not be \n                   run, so this report cannot say commands are confined. See the skipped check above."
            }
        }
    );
    out
}

/// What each level actually grants, in the report's own words.
///
/// Spelled out rather than left to the level's name, because "isolated" is a word and the question
/// a reader has is which of their files a command can still read.
const LEVEL_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "isolated",
        "reads: the declared roots and the platform's own runtime; writes: the declared roots",
    ),
    (
        "workspace-write",
        "reads: everywhere; writes: the declared roots",
    ),
    (
        "unconfined",
        "nothing beyond the baseline: environment, ceilings, starting directory",
    ),
];

/// Which backends this build contains, asked of the crate whose features they are.
fn compiled_backends() -> String {
    let names = compiled_backend_names();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}
