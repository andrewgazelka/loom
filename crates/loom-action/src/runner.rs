use crate::{
    Action,
    key::{FileStamp, file_stamp, relative},
    materialize::place_inputs,
};
use anyhow::{Context, Result, ensure};
use loom_process::{ProcessSandbox, ProcessSpec};
use loom_store::{ObjectError, Store};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The most a tool may write to each of stdout and stderr; more kills it and fails the action.
const OUTPUT_LIMIT: usize = 16 << 20;

/// Stands for the run's scratch directory in `Action::args` and `Action::env` values.
pub const ROOT_TOKEN: &str = "@ROOT@";

/// A kept output file: its blob in the store and its executable bit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    pub hash: String,
    pub executable: bool,
}

/// What a run produced. Stored as one document; `stdout` and `stderr` are blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub outputs: BTreeMap<String, OutputFile>,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub key: String,
    pub result: ActionResult,
    /// True when the store answered and nothing ran.
    pub cached: bool,
    /// Wall time of this call: the lookup on a hit, the whole run on a miss.
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub hits: u64,
    pub runs: u64,
    pub failures: u64,
}

/// Runs actions against a store, keeping scratch directories under `scratch`.
pub struct Runner {
    store: Store,
    scratch: PathBuf,
    timeout: Duration,
    tool_hashes: Mutex<HashMap<(PathBuf, FileStamp), String>>,
    hits: AtomicU64,
    runs: AtomicU64,
    failures: AtomicU64,
}

impl Runner {
    pub fn new(store: Store, scratch: impl Into<PathBuf>) -> Result<Self> {
        let scratch = scratch.into();
        std::fs::create_dir_all(&scratch).context("create action scratch directory")?;
        Ok(Self {
            store,
            scratch: scratch
                .canonicalize()
                .context("resolve action scratch directory")?,
            timeout: Duration::from_secs(600),
            tool_hashes: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            runs: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        })
    }

    /// A tool that runs longer than this is killed and the action fails. Not part of the key.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn stats(&self) -> Stats {
        Stats {
            hits: self.hits.load(Ordering::Relaxed),
            runs: self.runs.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
        }
    }

    /// The bytes of a stored blob named by a result (an output, stdout or stderr).
    pub fn read(&self, hash: &str) -> Result<Vec<u8>> {
        self.store
            .get(hash)?
            .with_context(|| format!("blob {hash} is not in the store"))
    }

    /// Write a result's output to `destination` (an independent copy, cloned on APFS).
    pub fn restore_output(&self, output: &OutputFile, destination: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if output.executable {
            std::fs::write(destination, self.read(&output.hash)?)?;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        } else {
            self.store.restore_to(&output.hash, destination)
        }
    }

    /// The tool's resolved path, content hash and the stamp of the file that was hashed. The hash is
    /// reused while the stamp (size, mtime, ctime) is unchanged.
    fn tool_hash(&self, tool: &Path) -> Result<(PathBuf, String, FileStamp)> {
        let tool = tool
            .canonicalize()
            .with_context(|| format!("resolve tool {}", tool.display()))?;
        let file = File::open(&tool).with_context(|| format!("open tool {}", tool.display()))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "tool {} is not a regular file",
            tool.display()
        );
        let stamp = file_stamp(&metadata);
        let cache_key = (tool.clone(), stamp);
        if let Some(hash) = self.tool_hashes.lock().unwrap().get(&cache_key) {
            return Ok((tool, hash.clone(), stamp));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(&file)?;
        let hash = hasher.finalize().to_hex().to_string();
        self.tool_hashes
            .lock()
            .unwrap()
            .insert(cache_key, hash.clone());
        Ok((tool, hash, stamp))
    }

    /// A recorded result whose every blob is still stored and intact; anything less is a miss.
    fn lookup(&self, key: &str) -> Result<Option<ActionResult>> {
        let Some(result_hash) = self.store.action_result(key)? else {
            return Ok(None);
        };
        // A record that is damaged or cannot be decoded is a miss, not an error: the run that
        // follows records a new document under the key and replaces it (putting the same bytes
        // again also repairs a corrupt row). Any other store failure propagates, so it shows
        // before a tool runs and not after.
        let bytes = match self.store.get(&result_hash) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) if ObjectError::is_in(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let Ok(result) = serde_json::from_slice::<ActionResult>(&bytes) else {
            return Ok(None);
        };
        let blobs = result
            .outputs
            .values()
            .map(|output| output.hash.as_str())
            .chain([result.stdout.as_str(), result.stderr.as_str()]);
        // Hashed, not just present: a same-size corrupt object must not be a hit forever. The
        // store remembers what it verified, so the restore that follows does not hash again.
        for hash in blobs {
            if !self.store.verify_object(hash)? {
                return Ok(None);
            }
        }
        Ok(Some(result))
    }

    pub async fn run(&self, action: &Action) -> Result<Outcome> {
        let started = Instant::now();
        action.validate()?;
        let (tool, tool_hash, tool_stamp) = self.tool_hash(&action.tool)?;
        let runtime_before = action.runtime_stamps()?;
        let key = action.key_with(&tool_hash, &runtime_before)?;
        if let Some(result) = self.lookup(&key)? {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Outcome {
                key,
                result,
                cached: true,
                elapsed: started.elapsed(),
            });
        }
        self.runs.fetch_add(1, Ordering::Relaxed);
        let work = tempfile::Builder::new()
            .prefix("action-")
            .tempdir_in(&self.scratch)
            .context("create action work directory")?;
        let root = work.path().canonicalize()?;
        place_inputs(&self.store, &root, &action.inputs)?;

        let sandbox = ProcessSandbox {
            readonly: action.runtime.clone(),
            network: action.network,
            sysctl_read: true,
        };
        // `@ROOT@` in an argument or environment value is this run's scratch directory: a tool can
        // map it to a fixed name (`--remap-path-prefix=@ROOT@=/work`) so its output does not depend on
        // where it ran. The key hashes the text with the token, not the path.
        let substitute = |text: &str| text.replace(ROOT_TOKEN, &root.to_string_lossy());
        let spec = ProcessSpec {
            machine: "action".into(),
            program: tool.to_string_lossy().into_owned(),
            args: action.args.iter().map(|arg| substitute(arg)).collect(),
            cwd: root.clone(),
            root: root.clone(),
            env: action
                .env
                .iter()
                .map(|(name, value)| (name.clone(), substitute(value)))
                .collect(),
            capture_paths: Vec::new(),
        };
        let wrapped = sandbox.wrapped_spec(&spec)?;
        let mut command = tokio::process::Command::new(&wrapped.program);
        command
            .args(&wrapped.args)
            .current_dir(&wrapped.cwd)
            .env_clear()
            .envs(&wrapped.env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Its own process group, so a kill reaches everything the tool started.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().context("spawn sandboxed tool")?;
        let stdout_pipe = child.stdout.take().context("tool stdout is not piped")?;
        let stderr_pipe = child.stderr.take().context("tool stderr is not piped")?;
        let finished = tokio::time::timeout(self.timeout, async {
            let (stdout, stderr) = tokio::try_join!(
                read_capped(stdout_pipe, "stdout"),
                read_capped(stderr_pipe, "stderr")
            )?;
            let status = child.wait().await.context("wait for sandboxed tool")?;
            anyhow::Ok((status, stdout, stderr))
        })
        .await;
        let (status, stdout_bytes, stderr_bytes) = match finished {
            Ok(Ok(finished)) => finished,
            failed => {
                // Too much output, or the clock ran out: nothing the tool started may outlive the error.
                if let Some(group) = child.id() {
                    // SAFETY: plain signal to the group this run created; no memory is involved.
                    unsafe { libc::killpg(group as libc::pid_t, libc::SIGKILL) };
                }
                let _ = child.kill().await;
                return Err(match failed {
                    Ok(Err(error)) => error,
                    _ => anyhow::anyhow!("action timed out after {:?}", self.timeout),
                });
            }
        };
        // The tool is run by path. A binary replaced while it ran is not the binary that was keyed.
        let (_, _, tool_after) = self.tool_hash(&tool)?;
        ensure!(
            tool_after == tool_stamp,
            "tool {} changed while the action ran",
            tool.display()
        );

        let stdout = self.store.put("blob", &stdout_bytes)?;
        let stderr = self.store.put("blob", &stderr_bytes)?;
        let exit_code = status.code().unwrap_or(-1);
        let mut outputs = BTreeMap::new();
        let mut missing = Vec::new();
        if exit_code == 0 {
            for name in &action.outputs {
                match open_output(&root, name)? {
                    Some((file, executable)) => {
                        outputs.insert(
                            name.clone(),
                            OutputFile {
                                hash: self.store.put_open_file("blob", file)?,
                                executable,
                            },
                        );
                    }
                    None => missing.push(name.clone()),
                }
            }
        }
        let result = ActionResult {
            exit_code,
            stdout,
            stderr,
            outputs,
        };
        if exit_code != 0 || !missing.is_empty() {
            self.failures.fetch_add(1, Ordering::Relaxed);
            ensure!(
                missing.is_empty(),
                "the tool exited 0 but did not write declared output(s) {missing:?}"
            );
            return Ok(Outcome {
                key,
                result,
                cached: false,
                elapsed: started.elapsed(),
            });
        }
        // A runtime file rewritten while the tool ran means the result may not belong to the key:
        // hand it back, but record nothing.
        if !matches!(action.runtime_stamps(), Ok(now) if now == runtime_before) {
            return Ok(Outcome {
                key,
                result,
                cached: false,
                elapsed: started.elapsed(),
            });
        }
        let result_hash = self
            .store
            .put("action-result", &serde_json::to_vec(&result)?)?;
        self.store.record_action_result(&key, &result_hash)?;
        Ok(Outcome {
            key,
            result,
            cached: false,
            elapsed: started.elapsed(),
        })
    }
}

/// Read at most `OUTPUT_LIMIT` bytes of a tool's output stream; one more byte is an error.
async fn read_capped(reader: impl AsyncRead + Unpin, name: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("read tool {name}"))?;
    ensure!(
        bytes.len() <= OUTPUT_LIMIT,
        "the tool wrote more than {} MiB to {name}",
        OUTPUT_LIMIT >> 20
    );
    Ok(bytes)
}

/// The declared output `name` under `root` as an open handle and whether it is executable, or
/// `None` when the tool did not leave a regular file there. The tool controls the scratch tree,
/// so the path is resolved fully and must stay under `root` (a symlinked directory pointing at
/// the host's files is an error, not an output). The file is opened `O_NOFOLLOW | O_NONBLOCK`
/// (a final symlink fails, a FIFO cannot block), must be a regular file, and the path must still
/// resolve to where it was vetted, so what is stored is what was vetted.
fn open_output(root: &Path, name: &str) -> Result<Option<(File, bool)>> {
    use std::os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let path = root.join(relative(name)?);
    let Ok(resolved) = path.canonicalize() else {
        return Ok(None);
    };
    ensure!(
        resolved.starts_with(root),
        "declared output {name:?} resolves outside the work directory"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&resolved)
        .with_context(|| format!("open output {name:?}"))?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Ok(None);
    }
    ensure!(
        path.canonicalize().is_ok_and(|again| again == resolved),
        "declared output {name:?} changed while it was collected"
    );
    // Blocking mode again for the reader that stores it; a regular file never needed the flag.
    // SAFETY: the descriptor is open and owned by `file` for the whole block.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    Ok(Some((file, opened.permissions().mode() & 0o111 != 0)))
}
