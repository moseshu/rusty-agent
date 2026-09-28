//! A Docker daemon stand-in for the Docker backend's tests.
//!
//! The reference's tests replace the `docker-py` client with fakes that record calls and answer from
//! a temporary directory: `_HostBackedDockerSession` interprets the handful of commands the session
//! runs against a host directory standing in for the container's filesystem, and the recorder
//! clients capture what containers, volumes and images were asked for. This is the same stand-in at
//! the seam this port has, the `DockerApi` trait: one fake, configurable per test, whose container
//! filesystem is a host directory when a test gives it one.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::StreamExt;
use ra_core::sandbox::{Manifest, SandboxSessionState, Snapshot};
use ra_sandbox::docker::{
    ContainerCreateSpec, DOCKER_BACKEND_ID, DockerApi, DockerApiError, DockerSandboxSession,
    DockerStateFields, ExecAttachment, ExecCreateRequest, ExecInspect, ExecRunOutput,
    ExecRunRequest, LENGTH_FRAMED_STDIN_SCRIPT,
};
use ra_sandbox::runtime_helpers::resolve_workspace_path_helper;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

/// The image the reference's tests use.
pub const IMAGE: &str = "python:3.14-slim";

/// Locks a mutex, ignoring poisoning from a panicked test thread.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One recorded one-shot command, as the reference's fakes record `exec_run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCall {
    pub cmd: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

/// One recorded attached command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedCall {
    pub container_id: String,
    pub request: ExecCreateRequest,
    pub stdin: Vec<u8>,
}

/// A hook that decides how a container creation goes.
pub type CreateHook =
    Box<dyn FnMut(&FakeDocker, &ContainerCreateSpec) -> Result<String, DockerApiError> + Send>;

/// A hook that answers a one-shot command instead of the host-backed interpreter.
pub type ExecHook =
    Box<dyn Fn(&ExecRunRequest) -> Result<ExecRunOutput, DockerApiError> + Send + Sync>;

/// How a volume behaves when it is removed.
#[derive(Debug, Clone)]
pub enum VolumeBehavior {
    /// Removed, and gone afterwards.
    Removes,
    /// Refuses with this error, and stays.
    Fails(DockerApiError),
}

/// The fake daemon.
#[derive(Default)]
pub struct FakeDocker {
    /// The host directory standing in for the container's `/`, when a test uses one.
    pub host_root: Option<PathBuf>,
    /// Images present on the daemon.
    pub images: Mutex<BTreeSet<String>>,
    /// Pulls asked for, as repository and tag.
    pub pulls: Mutex<Vec<(String, Option<String>)>>,
    /// Container creations asked for.
    pub creates: Mutex<Vec<ContainerCreateSpec>>,
    /// Decides a creation; without one, a creation succeeds with `created-<n>`.
    pub create_hook: Mutex<Option<CreateHook>>,
    /// Containers started.
    pub starts: Mutex<Vec<String>>,
    /// A failure every start returns.
    pub start_error: Mutex<Option<DockerApiError>>,
    /// Containers stopped.
    pub stops: Mutex<Vec<String>>,
    /// Container removals asked for, with whether they were forced.
    pub container_removals: Mutex<Vec<(String, bool)>>,
    /// A failure every container removal returns.
    pub container_remove_error: Mutex<Option<DockerApiError>>,
    /// Containers that exist, with their attribute documents.
    pub containers: Mutex<BTreeMap<String, Value>>,
    /// A failure every container inspection returns, instead of looking.
    pub inspect_error: Mutex<Option<DockerApiError>>,
    /// Container inspections asked for.
    pub inspects: Mutex<Vec<String>>,
    /// Volumes that exist, and how each behaves when removed.
    pub volumes: Mutex<BTreeMap<String, VolumeBehavior>>,
    /// Volume inspections asked for.
    pub volume_lookups: Mutex<Vec<String>>,
    /// Volume removals asked for.
    pub volume_removals: Mutex<Vec<String>>,
    /// One-shot commands run.
    pub execs: Mutex<Vec<ExecCall>>,
    /// Answers one-shot commands instead of the host-backed interpreter.
    pub exec_hook: Mutex<Option<ExecHook>>,
    /// The exit status the read probe reports, instead of looking.
    pub read_probe_exit_code: Mutex<Option<i32>>,
    /// The accounts the read probe ran as.
    pub read_probe_users: Mutex<Vec<Option<String>>>,
    /// Attached commands run, with the input they were fed.
    pub attached: Arc<Mutex<Vec<AttachedCall>>>,
    /// Exit statuses of attached commands, by exec id.
    exit_codes: Arc<Mutex<BTreeMap<String, Option<i64>>>>,
    /// Pending completions of attached commands, by exec id.
    completions: Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>,
    /// Whether the workspace probe of a fake without a host directory finds the workspace.
    pub workspace_exists: AtomicBool,
    /// Commands, by program name, that never finish: a hung process, as far as the caller can tell.
    pub hanging_programs: Mutex<BTreeSet<String>>,
    /// When set, a container creation records itself and then never finishes.
    pub hang_creates: AtomicBool,
    /// Releases a delayed create response after the daemon has already applied its side effects.
    pub create_response: tokio::sync::Notify,
    /// Delays removal to exercise cancellation during failure cleanup.
    pub hang_removals: AtomicBool,
    /// Releases a delayed removal response.
    pub remove_response: tokio::sync::Notify,
    /// Image lookups asked for.
    pub image_lookups: Mutex<Vec<String>>,
    /// One-shot commands and container stops, in the order they happened.
    pub events: Mutex<Vec<String>>,
    /// Paths downloaded as archives.
    pub archive_calls: Mutex<Vec<String>>,
    /// A failure every archive download returns.
    pub archive_error: Mutex<Option<DockerApiError>>,
    /// `cp -R` commands seen, by source.
    pub copies: Mutex<Vec<String>>,
}

impl FakeDocker {
    /// A daemon with nothing on it.
    pub fn new() -> Self {
        Self::default()
    }

    /// A daemon whose container filesystem is `host_root`.
    pub fn host_backed(host_root: &Path) -> Self {
        Self {
            host_root: Some(host_root.to_path_buf()),
            ..Self::default()
        }
    }

    /// Makes an image present.
    pub fn with_image(self, image: &str) -> Self {
        lock(&self.images).insert(image.to_owned());
        self
    }

    /// Makes a container exist, with this attribute document.
    pub fn with_container(self, id: &str, attrs: Value) -> Self {
        lock(&self.containers).insert(id.to_owned(), attrs);
        self
    }

    /// Makes a volume exist.
    pub fn with_volume(self, name: &str, behavior: VolumeBehavior) -> Self {
        lock(&self.volumes).insert(name.to_owned(), behavior);
        self
    }

    /// Adds a volume after construction, as a creation that has side effects does.
    pub fn add_volume(&self, name: &str) {
        lock(&self.volumes).insert(name.to_owned(), VolumeBehavior::Removes);
    }

    /// Decides creations with `hook`.
    pub fn on_create(
        &self,
        hook: impl FnMut(&FakeDocker, &ContainerCreateSpec) -> Result<String, DockerApiError>
        + Send
        + 'static,
    ) {
        *lock(&self.create_hook) = Some(Box::new(hook));
    }

    /// Answers one-shot commands with `hook`.
    pub fn on_exec(
        &self,
        hook: impl Fn(&ExecRunRequest) -> Result<ExecRunOutput, DockerApiError> + Send + Sync + 'static,
    ) {
        *lock(&self.exec_hook) = Some(Box::new(hook));
    }

    /// The names of the volumes that still exist.
    pub fn volume_names(&self) -> BTreeSet<String> {
        lock(&self.volumes).keys().cloned().collect()
    }

    /// The host path standing in for a container path.
    pub fn host_path(&self, container_path: &str) -> PathBuf {
        let root = self.host_root.as_ref().expect("a host-backed fake");
        root.join(container_path.trim_start_matches('/'))
    }

    /// The container path a host path stands in for.
    fn container_path(&self, host_path: &Path) -> String {
        let root = self.host_root.as_ref().expect("a host-backed fake");
        let relative = host_path.strip_prefix(root).unwrap_or(host_path);
        format!("/{}", relative.to_string_lossy())
    }

    /// The host-backed interpreter: `_HostBackedDockerSession._exec_internal`, command for command.
    fn interpret(&self, request: &ExecRunRequest) -> ExecRunOutput {
        let cmd: Vec<&str> = request.cmd().iter().map(String::as_str).collect();
        let ok = |stdout: Vec<u8>| ExecRunOutput::new(stdout, Vec::new(), Some(0));
        let fail = |stderr: &str, code: i64| {
            ExecRunOutput::new(Vec::new(), stderr.as_bytes().to_vec(), Some(code))
        };
        let helper = resolve_workspace_path_helper();

        if cmd.len() >= 3
            && cmd[..2] == ["sh", "-c"]
            && cmd[2].contains("RUSTY_AGENT_INSTALL_RUNTIME_HELPER_V1")
        {
            return ok(Vec::new());
        }
        if cmd.len() >= 5 && cmd[..2] == ["sh", "-c"] && cmd[2].contains("READ_PATH_PROBE_V3") {
            lock(&self.read_probe_users).push(request.user().map(str::to_owned));
            if let Some(code) = *lock(&self.read_probe_exit_code) {
                return fail("", i64::from(code));
            }
            let exists = self.host_path(cmd[4]).exists();
            return if exists { ok(Vec::new()) } else { fail("", 1) };
        }
        if cmd.first() == Some(&helper.install_path()) {
            return self.resolve(&cmd);
        }
        match cmd.as_slice() {
            ["mkdir", "-p", path] => {
                std::fs::create_dir_all(self.host_path(path)).expect("mkdir -p");
                ok(Vec::new())
            }
            ["mkdir", path] => match std::fs::create_dir(self.host_path(path)) {
                Ok(()) => ok(Vec::new()),
                Err(error) => fail(&error.to_string(), 1),
            },
            ["cp", "-R", "--", source, destination] => {
                lock(&self.copies).push((*source).to_owned());
                copy_tree(&self.host_path(source), &self.host_path(destination));
                ok(Vec::new())
            }
            ["cp", "--", source, destination] => {
                match std::fs::copy(self.host_path(source), self.host_path(destination)) {
                    Ok(_) => ok(Vec::new()),
                    Err(error) => fail(&error.to_string(), 1),
                }
            }
            ["cat", "--", path] => match std::fs::read(self.host_path(path)) {
                Ok(bytes) => ok(bytes),
                Err(error) => fail(&error.to_string(), 1),
            },
            ["rm", "--", path] | ["rm", "-rf", "--", path] => {
                let recursive = cmd[1] == "-rf";
                let target = self.host_path(path);
                let metadata = std::fs::symlink_metadata(&target);
                match metadata {
                    Ok(metadata) if metadata.is_symlink() || metadata.is_file() => {
                        let _ = std::fs::remove_file(&target);
                        ok(Vec::new())
                    }
                    Ok(metadata) if metadata.is_dir() && recursive => {
                        let _ = std::fs::remove_dir_all(&target);
                        ok(Vec::new())
                    }
                    Err(_) if recursive => ok(Vec::new()),
                    _ => fail("is a directory", 1),
                }
            }
            ["ls", "-la", "--", path] => {
                let output = std::process::Command::new("ls")
                    .args(["-la", "--"])
                    .arg(self.host_path(path))
                    .output()
                    .expect("ls");
                ExecRunOutput::new(
                    output.stdout,
                    output.stderr,
                    output.status.code().map(i64::from),
                )
            }
            ["test", "-d", path] => {
                if self.host_path(path).is_dir() {
                    ok(Vec::new())
                } else {
                    fail("", 1)
                }
            }
            ["sh", "-lc", script] if script.starts_with("pkill ") => ok(Vec::new()),
            // Materialization applies each entry's mode; the host directory already has one.
            ["chmod", _, path] if self.host_path(path).exists() => ok(Vec::new()),
            _ => panic!("unexpected command: {cmd:?}"),
        }
    }

    /// The resolver, as the reference's fake emulates it.
    fn resolve(&self, cmd: &[&str]) -> ExecRunOutput {
        let root = self.host_root.clone().expect("a host-backed fake");
        let canonical_root = std::fs::canonicalize(&root).unwrap_or(root.clone());
        let host = |path: &str| resolve_lenient(&self.host_path(path));
        let to_container = |path: &Path| {
            let relative = path.strip_prefix(&canonical_root).unwrap_or(path);
            format!("/{}", relative.to_string_lossy())
        };
        let for_write = cmd[3];
        let candidate = host(cmd[2]);
        let workspace_root = host(cmd[1]);
        if candidate.starts_with(&workspace_root) {
            return ExecRunOutput::new(to_container(&candidate).into_bytes(), Vec::new(), Some(0));
        }
        let grants = &cmd[4..];
        assert_eq!(grants.len() % 2, 0);
        let mut best: Option<(PathBuf, String, bool)> = None;
        for pair in grants.chunks(2) {
            let grant_root = host(pair[0]);
            if grant_root == canonical_root {
                return ExecRunOutput::new(
                    Vec::new(),
                    format!(
                        "extra path grant must not resolve to filesystem root: {}",
                        pair[0]
                    )
                    .into_bytes(),
                    Some(113),
                );
            }
            if !candidate.starts_with(&grant_root) {
                continue;
            }
            let deeper = best.as_ref().is_none_or(|(best_root, _, _)| {
                grant_root.components().count() > best_root.components().count()
            });
            if deeper {
                best = Some((grant_root, pair[0].to_owned(), pair[1] == "1"));
            }
        }
        if let Some((_, original, read_only)) = best {
            if for_write == "1" && read_only {
                return ExecRunOutput::new(
                    Vec::new(),
                    format!(
                        "read-only extra path grant: {original}\nresolved path: {}\n",
                        to_container(&candidate)
                    )
                    .into_bytes(),
                    Some(114),
                );
            }
            return ExecRunOutput::new(to_container(&candidate).into_bytes(), Vec::new(), Some(0));
        }
        ExecRunOutput::new(Vec::new(), b"workspace escape".to_vec(), Some(111))
    }

    /// Carries out a framed attached command once its input is complete, and says how it ended.
    fn complete_attached(&self, request: &ExecCreateRequest, stdin: &[u8]) -> Option<i64> {
        let cmd: Vec<&str> = request.cmd().iter().map(String::as_str).collect();
        assert_eq!(cmd[..4], ["sh", "-c", LENGTH_FRAMED_STDIN_SCRIPT, "sh"]);
        let length: usize = cmd[4].parse().expect("a framed length");
        let payload = &stdin[..length.min(stdin.len())];
        if self.host_root.is_none() {
            return Some(0);
        }
        match &cmd[5..] {
            ["sh", "-lc", script, "sh", path] if *script == r#"cat > "$1""# => {
                std::fs::write(self.host_path(path), payload).expect("cat >");
                Some(0)
            }
            ["sh", "-lc", script, "sh", path]
                if *script == r#"mkdir -p "$(dirname "$1")" && cat > "$1""# =>
            {
                let target = self.host_path(path);
                std::fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
                std::fs::write(target, payload).expect("cat >");
                Some(0)
            }
            ["tar", "-x", "-C", root] => {
                let mut archive = tar::Archive::new(payload);
                archive.unpack(self.host_path(root)).expect("tar -x");
                Some(0)
            }
            other => panic!("unexpected framed command: {other:?}"),
        }
    }
}

/// Resolves every existing component of a path, and keeps the rest as written.
fn resolve_lenient(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&existing) {
            let mut resolved = resolved;
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Copies a file or a directory tree, as `shutil.copytree` or `copy2` would.
fn copy_tree(source: &Path, destination: &Path) {
    let metadata = std::fs::symlink_metadata(source).expect("a copy source");
    if metadata.is_dir() {
        std::fs::create_dir_all(destination).expect("mkdir");
        for entry in std::fs::read_dir(source).expect("read_dir") {
            let entry = entry.expect("an entry");
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        }
    } else if metadata.is_symlink() {
        let target = std::fs::read_link(source).expect("a link");
        std::os::unix::fs::symlink(target, destination).expect("symlink");
    } else {
        std::fs::copy(source, destination).expect("copy");
    }
}

#[async_trait]
impl DockerApi for FakeDocker {
    async fn inspect_image(&self, image: &str) -> Result<(), DockerApiError> {
        lock(&self.image_lookups).push(image.to_owned());
        if lock(&self.images).contains(image) {
            Ok(())
        } else {
            Err(DockerApiError::not_found(format!("no such image: {image}")))
        }
    }

    async fn pull_image(&self, repository: &str, tag: Option<&str>) -> Result<(), DockerApiError> {
        lock(&self.pulls).push((repository.to_owned(), tag.map(str::to_owned)));
        Ok(())
    }

    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, DockerApiError> {
        lock(&self.creates).push(spec.clone());
        let hook = lock(&self.create_hook).take();
        if self.hang_creates.load(Ordering::SeqCst) {
            let outcome = if let Some(mut hook) = hook {
                let outcome = hook(self, spec);
                *lock(&self.create_hook) = Some(hook);
                outcome
            } else {
                let id = format!("created-{}", lock(&self.creates).len());
                lock(&self.containers).insert(id.clone(), json!({"State": {"Status": "created"}}));
                Ok(id)
            };
            self.create_response.notified().await;
            return outcome;
        }
        let Some(mut hook) = hook else {
            let id = format!("created-{}", lock(&self.creates).len());
            lock(&self.containers).insert(id.clone(), json!({"State": {"Status": "created"}}));
            return Ok(id);
        };
        let outcome = hook(self, spec);
        *lock(&self.create_hook) = Some(hook);
        outcome
    }

    async fn start_container(&self, id: &str) -> Result<(), DockerApiError> {
        lock(&self.starts).push(id.to_owned());
        if let Some(error) = lock(&self.start_error).clone() {
            return Err(error);
        }
        if let Some(attrs) = lock(&self.containers).get_mut(id) {
            attrs["State"] = json!({"Status": "running"});
        }
        Ok(())
    }

    async fn stop_container(&self, id: &str) -> Result<(), DockerApiError> {
        lock(&self.stops).push(id.to_owned());
        lock(&self.events).push(format!("stop:{id}"));
        Ok(())
    }

    async fn remove_container(&self, id: &str, force: bool) -> Result<(), DockerApiError> {
        lock(&self.container_removals).push((id.to_owned(), force));
        if self.hang_removals.load(Ordering::SeqCst) {
            self.remove_response.notified().await;
        }
        if let Some(error) = lock(&self.container_remove_error).clone() {
            return Err(error);
        }
        lock(&self.containers).remove(id);
        Ok(())
    }

    async fn inspect_container(&self, id: &str) -> Result<Value, DockerApiError> {
        lock(&self.inspects).push(id.to_owned());
        if let Some(error) = lock(&self.inspect_error).clone() {
            return Err(error);
        }
        lock(&self.containers)
            .get(id)
            .cloned()
            .ok_or_else(|| DockerApiError::not_found(format!("no such container: {id}")))
    }

    async fn inspect_volume(&self, name: &str) -> Result<(), DockerApiError> {
        lock(&self.volume_lookups).push(name.to_owned());
        if lock(&self.volumes).contains_key(name) {
            Ok(())
        } else {
            Err(DockerApiError::not_found(format!("no such volume: {name}")))
        }
    }

    async fn remove_volume(&self, name: &str) -> Result<(), DockerApiError> {
        lock(&self.volume_removals).push(name.to_owned());
        let behavior = lock(&self.volumes).get(name).cloned();
        match behavior {
            None => Err(DockerApiError::not_found(format!("no such volume: {name}"))),
            Some(VolumeBehavior::Fails(error)) => Err(error),
            Some(VolumeBehavior::Removes) => {
                lock(&self.volumes).remove(name);
                Ok(())
            }
        }
    }

    async fn exec_create(
        &self,
        container_id: &str,
        request: &ExecCreateRequest,
    ) -> Result<String, DockerApiError> {
        let mut attached = lock(&self.attached);
        attached.push(AttachedCall {
            container_id: container_id.to_owned(),
            request: request.clone(),
            stdin: Vec::new(),
        });
        Ok(format!("exec-{}", attached.len()))
    }

    async fn exec_start(
        &self,
        exec_id: &str,
        _tty: bool,
    ) -> Result<ExecAttachment, DockerApiError> {
        let index: usize = exec_id
            .strip_prefix("exec-")
            .and_then(|n| n.parse().ok())
            .expect("an exec id this fake issued");
        let (writer, mut reader) = tokio::io::duplex(64 * 1024);
        let attached = Arc::clone(&self.attached);
        // Completing the command needs `&self`, which the task cannot hold; it collects the input,
        // and the completion runs when the exit status is asked for.
        let collect = tokio::spawn(async move {
            let mut stdin = Vec::new();
            let _ = reader.read_to_end(&mut stdin).await;
            lock(&attached)[index - 1].stdin = stdin;
        });
        lock(&self.completions).insert(exec_id.to_owned(), collect);
        Ok(ExecAttachment::new(
            futures::stream::empty().boxed(),
            Box::pin(writer),
        ))
    }

    async fn exec_inspect(&self, exec_id: &str) -> Result<ExecInspect, DockerApiError> {
        let pending = lock(&self.completions).remove(exec_id);
        if let Some(pending) = pending {
            pending.await.expect("the input collector");
        }
        let index: usize = exec_id
            .strip_prefix("exec-")
            .and_then(|n| n.parse().ok())
            .expect("an exec id this fake issued");
        let call = lock(&self.attached)[index - 1].clone();
        let exit_code = *lock(&self.exit_codes)
            .entry(exec_id.to_owned())
            .or_insert_with(|| self.complete_attached(&call.request, &call.stdin));
        Ok(ExecInspect::new(false, exit_code))
    }

    async fn exec_run(
        &self,
        _container_id: &str,
        request: &ExecRunRequest,
    ) -> Result<ExecRunOutput, DockerApiError> {
        lock(&self.execs).push(ExecCall {
            cmd: request.cmd().to_vec(),
            workdir: request.workdir().map(str::to_owned),
            user: request.user().map(str::to_owned),
        });
        lock(&self.events).push(format!("exec:{}", request.cmd().join(" ")));
        let hangs = request
            .cmd()
            .first()
            .is_some_and(|program| lock(&self.hanging_programs).contains(program));
        if hangs {
            std::future::pending::<()>().await;
        }
        if let Some(hook) = lock(&self.exec_hook).as_ref() {
            return hook(request);
        }
        if self.host_root.is_none() {
            // `_ExecRunContainer`: everything succeeds, except that the workspace probe answers
            // whether the test said the workspace exists.
            let probe = ["test", "-d", "/workspace"];
            let exit_code = if request.cmd().iter().map(String::as_str).eq(probe) {
                i64::from(!self.workspace_exists.load(Ordering::SeqCst))
            } else {
                0
            };
            return Ok(ExecRunOutput::new(Vec::new(), Vec::new(), Some(exit_code)));
        }
        Ok(self.interpret(request))
    }

    async fn get_archive(
        &self,
        _container_id: &str,
        path: &str,
    ) -> Result<Vec<u8>, DockerApiError> {
        lock(&self.archive_calls).push(path.to_owned());
        if let Some(error) = lock(&self.archive_error).clone() {
            return Err(error);
        }
        if path == "/workspace" {
            return Err(DockerApiError::api(500, "root archive unsupported"));
        }
        let host = self.host_path(path);
        let name = host.file_name().expect("a name").to_owned();
        let mut builder = tar::Builder::new(Vec::new());
        builder.follow_symlinks(false);
        builder.append_dir_all(&name, &host).expect("archive");
        Ok(builder.into_inner().expect("archive bytes"))
    }
}

/// A Docker session state over `manifest`, naming `container`.
pub fn docker_state(manifest: Manifest, container_id: &str) -> SandboxSessionState {
    DockerStateFields::new(IMAGE, container_id).apply(SandboxSessionState::new(
        DOCKER_BACKEND_ID,
        Snapshot::noop(),
        manifest,
    ))
}

/// A session over a host-backed fake, as `_HostBackedDockerSession` builds one.
pub fn host_backed_session(
    host_root: &Path,
    manifest: Manifest,
) -> (Arc<FakeDocker>, DockerSandboxSession) {
    let fake = Arc::new(
        FakeDocker::host_backed(host_root)
            .with_container("container", json!({"State": {"Status": "running"}})),
    );
    let session = DockerSandboxSession::new(fake.clone(), docker_state(manifest, "container"))
        .expect("a docker state");
    (fake, session)
}

/// The member names of a tar archive.
pub fn archive_member_names(archive: &[u8]) -> Vec<String> {
    let mut reader = tar::Archive::new(archive);
    reader
        .entries()
        .expect("entries")
        .map(|entry| {
            entry
                .expect("an entry")
                .path()
                .expect("a path")
                .to_string_lossy()
                .trim_end_matches('/')
                .to_owned()
        })
        .collect()
}

/// A tar archive with one file of five bytes per name, as the reference's `_tar_bytes`.
pub fn tar_bytes(names: &[&str]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for name in names {
        let payload = b"pwned";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        // Written raw so an absolute or climbing name survives, as the reference writes it.
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_cksum();
        builder.append(&header, &payload[..]).expect("append");
    }
    builder.into_inner().expect("tar bytes")
}

/// A tar archive with one symlink member, as the reference's `_tar_symlink_bytes`.
pub fn tar_symlink_bytes(name: &str, target: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    let bytes = name.as_bytes();
    header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
    header.set_link_name(target).expect("link name");
    header.set_cksum();
    builder.append(&header, &[][..]).expect("append");
    builder.into_inner().expect("tar bytes")
}

/// The container path a host path stands in for, for assertions.
pub fn container_path_of(fake: &FakeDocker, host_path: &Path) -> String {
    fake.container_path(host_path)
}
