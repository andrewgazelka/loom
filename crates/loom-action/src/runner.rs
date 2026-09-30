use crate::{Action, key::relative, materialize::place_inputs};
use anyhow::{Context, Result, ensure};
use loom_process::{ProcessSandbox, ProcessSpec};
use loom_store::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

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
    tool_hashes: Mutex<HashMap<(PathBuf, u64, SystemTime), String>>,
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
            scratch: scratch.canonicalize().context("resolve action scratch directory")?,
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

    /// Write a result's output to `destination` (cloned or linked from the store when large).
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

    fn tool_hash(&self, tool: &Path) -> Result<(PathBuf, String)> {
        let tool = tool
            .canonicalize()
            .with_context(|| format!("resolve tool {}", tool.display()))?;
        let metadata = std::fs::metadata(&tool)?;
        ensure!(metadata.is_file(), "tool {} is not a regular file", tool.display());
        let stamp = (tool.clone(), metadata.len(), metadata.modified()?);
        if let Some(hash) = self.tool_hashes.lock().unwrap().get(&stamp) {
            return Ok((tool, hash.clone()));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(std::fs::File::open(&tool)?)?;
        let hash = hasher.finalize().to_hex().to_string();
        self.tool_hashes.lock().unwrap().insert(stamp, hash.clone());
        Ok((tool, hash))
    }

    /// A recorded result whose every blob is still stored; anything less is a miss.
    fn lookup(&self, key: &str) -> Result<Option<ActionResult>> {
        let Some(result_hash) = self.store.action_result(key)? else {
            return Ok(None);
        };
        let Some(bytes) = self.store.get(&result_hash)? else {
            return Ok(None);
        };
        let result: ActionResult = serde_json::from_slice(&bytes).context("decode action result")?;
        let blobs = result
            .outputs
            .values()
            .map(|output| output.hash.as_str())
            .chain([result.stdout.as_str(), result.stderr.as_str()]);
        for hash in blobs {
            if self.store.size_of(hash)?.is_none() {
                return Ok(None);
            }
        }
        Ok(Some(result))
    }

    pub async fn run(&self, action: &Action) -> Result<Outcome> {
        let started = Instant::now();
        action.validate()?;
        let (tool, tool_hash) = self.tool_hash(&action.tool)?;
        let key = action.key(&tool_hash)?;
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
            .kill_on_drop(true);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| anyhow::anyhow!("action timed out after {:?}", self.timeout))?
            .context("spawn sandboxed tool")?;

        let stdout = self.store.put("blob", &output.stdout)?;
        let stderr = self.store.put("blob", &output.stderr)?;
        let exit_code = output.status.code().unwrap_or(-1);
        let mut outputs = BTreeMap::new();
        let mut missing = Vec::new();
        if exit_code == 0 {
            for name in &action.outputs {
                let path = root.join(relative(name)?);
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        use std::os::unix::fs::PermissionsExt;
                        outputs.insert(
                            name.clone(),
                            OutputFile {
                                hash: self.store.put_file("blob", &path)?,
                                executable: metadata.permissions().mode() & 0o111 != 0,
                            },
                        );
                    }
                    _ => missing.push(name.clone()),
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
        let result_hash = self.store.put("action-result", &serde_json::to_vec(&result)?)?;
        self.store.record_action_result(&key, &result_hash)?;
        Ok(Outcome {
            key,
            result,
            cached: false,
            elapsed: started.elapsed(),
        })
    }
}
