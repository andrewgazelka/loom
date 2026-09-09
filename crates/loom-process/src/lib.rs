//! Durable process lifecycle. Callers supply a trusted, sandboxed command after
//! machine authorization; cwd containment alone is not a filesystem sandbox.
mod capture;
use anyhow::{Context, Result, ensure};
pub use capture::{FileChange, FilesystemCapture, UnavailablePath};
use loom_store::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{mpsc, watch},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub machine: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub root: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Explicit machine-root-relative regular files to observe before and after.
    #[serde(default)]
    pub capture_paths: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    Completed,
    Cancelled,
    Interrupted,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessState {
    pub id: String,
    pub spec: ProcessSpec,
    pub phase: Phase,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
    #[serde(default)]
    pub filesystem_changes: Vec<FileChange>,
    #[serde(default)]
    pub filesystem_capture: FilesystemCapture,
}
struct Running {
    state: watch::Receiver<ProcessState>,
    cancel: mpsc::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}
struct Inner {
    store: Store,
    running: Mutex<BTreeMap<String, Running>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(running) = self.running.get_mut() {
            for process in running.values() {
                process.task.abort();
            }
        }
    }
}
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}
impl Supervisor {
    /// Open once per daemon. In-flight stacks cannot survive restart: record an
    /// interrupted terminal event; resumption is an explicit new start.
    pub fn new(store: Store) -> Result<Self> {
        for mut state in replay(&store)?.into_values() {
            if state.phase == Phase::Running {
                state.phase = Phase::Interrupted;
                state.error = Some("daemon restarted; start a new process to resume".into());
                state.filesystem_capture.unavailable_reason =
                    Some("Process was interrupted; after-state capture is unavailable".into());
                record(&store, &state)?;
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                running: Mutex::new(BTreeMap::new()),
            }),
        })
    }
    pub fn list(&self) -> Result<Vec<ProcessState>> {
        Ok(replay(&self.inner.store)?.into_values().collect())
    }
    pub fn status(&self, id: &str) -> Result<ProcessState> {
        replay(&self.inner.store)?
            .remove(id)
            .context("unknown process")
    }
    pub async fn start(&self, spec: ProcessSpec) -> Result<ProcessState> {
        self.inner
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .retain(|_, process| !process.task.is_finished());
        ensure!(!spec.machine.is_empty(), "machine is required");
        let root = spec.root.canonicalize().context("machine root")?;
        let cwd = spec.cwd.canonicalize().context("process cwd")?;
        ensure!(cwd.starts_with(&root), "process cwd escapes machine root");
        let capture_store = self.inner.store.clone();
        let capture_machine = spec.machine.clone();
        let capture_paths = spec.capture_paths.clone();
        let capture = tokio::task::spawn_blocking(move || {
            capture::Capture::begin(&capture_store, capture_machine, &root, &capture_paths)
        })
        .await
        .context("filesystem capture worker")?;
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("start process")?;
        let guard = ProcessGroup { pid: child.id() };
        let stdout = child.stdout.take().context("stdout pipe")?;
        let stderr = child.stderr.take().context("stderr pipe")?;
        let initial = ProcessState {
            id: format!("process-{}", uuid::Uuid::new_v4()),
            spec,
            phase: Phase::Running,
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            filesystem_changes: Vec::new(),
            filesystem_capture: capture.report.clone(),
        };
        record(&self.inner.store, &initial)?;
        let (state_tx, state_rx) = watch::channel(initial.clone());
        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let store = self.inner.store.clone();
        let mut state = initial.clone();
        let task = tokio::spawn(async move {
            let _guard = guard;
            let (output_tx, mut output_rx) = mpsc::channel::<Chunk>(16);
            let out_task = tokio::spawn(read_output(stdout, false, output_tx.clone()));
            let err_task = tokio::spawn(read_output(stderr, true, output_tx));
            let readers = Readers { out_task, err_task };
            let mut exited = false;
            let mut streams_done = false;
            let mut stdout_bytes = Vec::new();
            let mut stderr_bytes = Vec::new();
            while !exited || !streams_done {
                tokio::select! {
                    _ = cancel_rx.recv(), if !exited => {
                        _guard.kill();
                        let _ = child.kill().await;
                        state.phase = Phase::Cancelled;
                    }
                    result = child.wait(), if !exited => {
                        exited = true;
                        match result {
                            Ok(status) => { state.code = status.code(); if state.phase == Phase::Running { state.phase = Phase::Completed; } }
                            Err(error) => { state.phase = Phase::Failed; state.error = Some(error.to_string()); }
                        }
                        // Descendants must not outlive their supervised leader or retain its pipes.
                        _guard.kill();
                    }
                    output = output_rx.recv(), if !streams_done => {
                        match output {
                            Some(chunk) => {
                                if let Some(error) = chunk.error {
                                    state.phase = Phase::Failed;
                                    state.error = Some(error);
                                    _guard.kill();
                                    let _ = child.kill().await;
                                } else if stdout_bytes.len() + stderr_bytes.len() + chunk.bytes.len() > 8 * 1024 * 1024 {
                                    state.phase = Phase::Failed;
                                    state.error = Some("process output exceeded 8 MiB".into());
                                    _guard.kill();
                                    let _ = child.kill().await;
                                } else {
                                    if chunk.stderr { stderr_bytes.extend_from_slice(&chunk.bytes); state.stderr = String::from_utf8_lossy(&stderr_bytes).into_owned(); }
                                    else { stdout_bytes.extend_from_slice(&chunk.bytes); state.stdout = String::from_utf8_lossy(&stdout_bytes).into_owned(); }
                                    if let Err(error) = store.append("system", &serde_json::json!({"type":"process_output","process":state.id,"stderr":chunk.stderr,"bytes":chunk.bytes}), 0) {
                                        state.phase = Phase::Failed;
                                        state.error = Some(error.to_string());
                                        _guard.kill();
                                        let _ = child.kill().await;
                                    }
                                    state_tx.send_replace(state.clone());
                                }
                            }
                            None => streams_done = true,
                        }
                    }
                }
            }
            drop(readers);
            let capture_store = store.clone();
            match tokio::task::spawn_blocking(move || capture.finish(&capture_store)).await {
                Ok(captured) => {
                    state.filesystem_changes = captured.changes;
                    state.filesystem_capture = captured.report;
                }
                Err(error) => {
                    state.filesystem_capture.unavailable_reason =
                        Some(format!("Filesystem capture worker failed: {error}"));
                }
            }
            if let Err(error) = record(&store, &state) {
                state.phase = Phase::Failed;
                state.error = Some(format!("persist process outcome: {error}"));
            }
            state_tx.send_replace(state);
        });
        self.inner
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .insert(
                initial.id.clone(),
                Running {
                    state: state_rx,
                    cancel: cancel_tx,
                    task,
                },
            );
        Ok(initial)
    }
    pub async fn wait(&self, id: &str) -> Result<ProcessState> {
        let receiver = self
            .inner
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .get(id)
            .map(|p| p.state.clone());
        let Some(mut receiver) = receiver else {
            return self.status(id);
        };
        // The sender closes only after pipes drain and the terminal state is durable.
        while receiver.changed().await.is_ok() {}
        Ok(receiver.borrow().clone())
    }
    pub async fn cancel(&self, id: &str) -> Result<ProcessState> {
        let sender = self
            .inner
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .get(id)
            .map(|p| p.cancel.clone());
        if let Some(sender) = sender {
            let _ = sender.send(()).await;
        }
        self.wait(id).await
    }
}
struct Chunk {
    stderr: bool,
    bytes: Vec<u8>,
    error: Option<String>,
}
async fn read_output(
    mut stream: impl AsyncRead + Unpin,
    stderr: bool,
    output: mpsc::Sender<Chunk>,
) {
    let mut buffer = vec![0; 4096];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Err(error) => {
                let _ = output
                    .send(Chunk {
                        stderr,
                        bytes: Vec::new(),
                        error: Some(format!("read process output: {error}")),
                    })
                    .await;
                break;
            }
            Ok(n) => {
                if output
                    .send(Chunk {
                        stderr,
                        bytes: buffer[..n].to_vec(),
                        error: None,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}
struct Readers {
    out_task: tokio::task::JoinHandle<()>,
    err_task: tokio::task::JoinHandle<()>,
}
impl Drop for Readers {
    fn drop(&mut self) {
        self.out_task.abort();
        self.err_task.abort();
    }
}
struct ProcessGroup {
    pid: Option<u32>,
}
impl ProcessGroup {
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}
fn record(store: &Store, state: &ProcessState) -> Result<()> {
    store.append(
        "system",
        &serde_json::json!({"type":"process_state","state":state}),
        0,
    )?;
    Ok(())
}
fn replay(store: &Store) -> Result<BTreeMap<String, ProcessState>> {
    let mut states: BTreeMap<String, ProcessState> = BTreeMap::new();
    #[derive(Default)]
    struct Output {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }
    let mut outputs: BTreeMap<String, Output> = BTreeMap::new();
    let mut after = 0;
    loop {
        let events = store.events(Some("system"), after, 1000)?;
        if events.is_empty() {
            break;
        }
        for event in events {
            after = event.seq;
            match event.event["type"].as_str() {
                Some("process_state") => {
                    let state: ProcessState = serde_json::from_value(event.event["state"].clone())?;
                    outputs.remove(&state.id);
                    states.insert(state.id.clone(), state);
                }
                Some("process_output") => {
                    if let Some(state) = event.event["process"]
                        .as_str()
                        .and_then(|id| states.get_mut(id))
                    {
                        let bytes: Vec<u8> = serde_json::from_value(event.event["bytes"].clone())?;
                        let output = outputs.entry(state.id.clone()).or_default();
                        if event.event["stderr"] == true {
                            output.stderr.extend_from_slice(&bytes);
                            state.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                        } else {
                            output.stdout.extend_from_slice(&bytes);
                            state.stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(states)
}

/// Hold across an exec wait so dropped guest fibers cancel their subprocess.
/// Explicit process.start calls omit this guard and persist until cancelled.
pub struct CancelOnDrop {
    sender: mpsc::Sender<()>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.sender.try_send(());
    }
}
impl Supervisor {
    pub fn cancel_on_drop(&self, id: &str) -> Result<CancelOnDrop> {
        let sender = self
            .inner
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .get(id)
            .context("unknown running process")?
            .cancel
            .clone();
        Ok(CancelOnDrop { sender })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn executable(name: &str) -> String {
        let path = std::env::var_os("PATH").expect("test PATH is required");
        let executable = std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("test executable {name} missing from PATH"));
        std::path::absolute(executable)
            .expect("test executable must have an absolute path")
            .to_str()
            .expect("test executable path must be UTF-8")
            .into()
    }
    fn spec(root: &std::path::Path, source: &str) -> ProcessSpec {
        let mut env = BTreeMap::new();
        env.insert("LOOM_TEST_SLEEP".into(), executable("sleep"));
        ProcessSpec {
            machine: "test-machine".into(),
            program: executable("sh"),
            args: vec!["-c".into(), source.into()],
            cwd: root.into(),
            root: root.into(),
            env,
            capture_paths: Vec::new(),
        }
    }
    #[tokio::test]
    async fn delayed_output_completion_and_restart_are_durable() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("process.sqlite");
        let store = Store::open(&path)?;
        let supervisor = Supervisor::new(store.clone())?;
        let process = supervisor
            .start(spec(
                temp.path(),
                "printf first; \"$LOOM_TEST_SLEEP\" 0.2; printf last; printf warning >&2; exit 7",
            ))
            .await?;
        assert_eq!(supervisor.status(&process.id)?.phase, Phase::Running);
        let state = supervisor.wait(&process.id).await?;
        assert_eq!(state.code, Some(7));
        assert_eq!(state.stdout, "firstlast");
        assert_eq!(state.stderr, "warning");
        assert_eq!(state.phase, Phase::Completed);
        assert!(
            store
                .events(Some("system"), 0, 1000)?
                .iter()
                .any(|e| e.event["type"] == "process_output")
        );
        drop(supervisor);
        let supervisor = Supervisor::new(Store::open(path)?)?;
        assert_eq!(supervisor.status(&process.id)?.stdout, "firstlast");
        assert_eq!(supervisor.status(&process.id)?.phase, Phase::Completed);
        Ok(())
    }
    #[tokio::test]
    async fn cancel_and_drop_kill_descendants_and_restart_marks_interrupted() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("process.sqlite");
        let supervisor = Supervisor::new(Store::open(&path)?)?;
        let process = supervisor
            .start(spec(
                temp.path(),
                "(\"$LOOM_TEST_SLEEP\" 0.4; printf escaped > escaped) & printf ready; wait",
            ))
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while supervisor.status(&process.id)?.stdout != "ready" {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        let state = supervisor.cancel(&process.id).await?;
        assert_eq!(state.phase, Phase::Cancelled);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(!temp.path().join("escaped").exists());
        let pending = supervisor
            .start(spec(temp.path(), "\"$LOOM_TEST_SLEEP\" 30"))
            .await?;
        drop(supervisor);
        tokio::task::yield_now().await;
        let supervisor = Supervisor::new(Store::open(path)?)?;
        assert_eq!(supervisor.status(&pending.id)?.phase, Phase::Interrupted);
        let resumed = supervisor
            .start(spec(temp.path(), "printf resumed"))
            .await?;
        assert_ne!(resumed.id, pending.id);
        assert_eq!(supervisor.wait(&resumed.id).await?.stdout, "resumed");
        Ok(())
    }
    #[tokio::test]
    async fn dropped_exec_guard_cancels_and_outside_cwd_is_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let supervisor = Supervisor::new(Store::memory()?)?;
        let process = supervisor
            .start(spec(temp.path(), "\"$LOOM_TEST_SLEEP\" 30"))
            .await?;
        drop(supervisor.cancel_on_drop(&process.id)?);
        assert_eq!(supervisor.wait(&process.id).await?.phase, Phase::Cancelled);
        let mut outside = spec(temp.path(), "true");
        outside.cwd = "/".into();
        assert!(supervisor.start(outside).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn capture_records_actual_file_changes_and_survives_restart() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("modified"), b"before")?;
        std::fs::write(temp.path().join("deleted"), b"deleted bytes")?;
        std::fs::write(temp.path().join("unchanged"), b"same")?;
        let store = Store::open(temp.path().join("loom.sqlite"))?;
        let supervisor = Supervisor::new(store.clone())?;
        let mut input = spec(
            temp.path(),
            "printf after > modified; printf created > created; \"$LOOM_TEST_RM\" deleted",
        );
        input.env.insert("LOOM_TEST_RM".into(), executable("rm"));
        input.capture_paths = ["modified", "created", "deleted", "unchanged"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let process = supervisor.start(input).await?;
        let completed = supervisor.wait(&process.id).await?;
        assert_eq!(completed.code, Some(0));
        assert_eq!(completed.filesystem_changes.len(), 3);
        assert!(completed.filesystem_capture.unavailable_reason.is_none());
        let modified = completed
            .filesystem_changes
            .iter()
            .find(|change| change.path == "modified")
            .context("modified file")?;
        assert_eq!(
            store.get(modified.before.as_deref().context("before CID")?)?,
            Some(b"before".to_vec())
        );
        assert_eq!(
            store.get(modified.after.as_deref().context("after CID")?)?,
            Some(b"after".to_vec())
        );
        let created = completed
            .filesystem_changes
            .iter()
            .find(|change| change.path == "created")
            .context("created file")?;
        assert!(created.before.is_none());
        assert_eq!(
            store.get(created.after.as_deref().context("created CID")?)?,
            Some(b"created".to_vec())
        );
        let deleted = completed
            .filesystem_changes
            .iter()
            .find(|change| change.path == "deleted")
            .context("deleted file")?;
        assert!(deleted.after.is_none());
        assert_eq!(
            store.get(deleted.before.as_deref().context("deleted CID")?)?,
            Some(b"deleted bytes".to_vec())
        );
        drop(supervisor);
        let reopened = Supervisor::new(Store::open(temp.path().join("loom.sqlite"))?)?;
        assert_eq!(
            serde_json::to_value(reopened.status(&process.id)?.filesystem_changes)?,
            serde_json::to_value(completed.filesystem_changes)?
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capture_unavailable_paths_do_not_fail_process_or_follow_symlinks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("secret"), b"outside secret")?;
        std::os::unix::fs::symlink(outside.path().join("secret"), temp.path().join("link"))?;
        std::os::unix::fs::symlink(outside.path(), temp.path().join("parent-link"))?;
        std::fs::create_dir(temp.path().join("directory"))?;
        std::fs::write(temp.path().join("large"), vec![0; 1024 * 1024 + 1])?;
        let store = Store::memory()?;
        let supervisor = Supervisor::new(store.clone())?;
        let mut input = spec(temp.path(), "printf success");
        input.capture_paths = [
            "link",
            "parent-link/secret",
            "directory",
            "large",
            "../escape",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let process = supervisor.start(input).await?;
        let completed = supervisor.wait(&process.id).await?;
        assert_eq!(completed.code, Some(0));
        assert_eq!(completed.stdout, "success");
        assert!(completed.filesystem_changes.is_empty());
        assert_eq!(completed.filesystem_capture.unavailable_paths.len(), 5);
        let no_capture = supervisor
            .start(spec(temp.path(), "printf uncaptured > other"))
            .await?;
        let no_capture = supervisor.wait(&no_capture.id).await?;
        assert!(no_capture.filesystem_changes.is_empty());
        assert!(no_capture.filesystem_capture.unavailable_reason.is_some());
        Ok(())
    }
}
