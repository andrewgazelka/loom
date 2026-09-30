use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

/// Bumped when the sandbox profile, the layout of the scratch directory or the result
/// format changes in a way that could change what a tool produces.
const FORMAT: u32 = 2;

/// One declared input file: the stored blob and whether it must be executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub hash: String,
    #[serde(default)]
    pub executable: bool,
}

/// A process run as a function. Every field is part of the key. `runtime` contributes its paths
/// and, for each path that is a regular file, its size, mtime and ctime in nanoseconds, so
/// replacing or rewriting such a file changes the key. The contents of a runtime DIRECTORY are
/// not keyed at all: only `tool_identity` covers them, so a directory of libraries or a sysroot
/// needs an identity string that changes when the directory does (a version banner, a store path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    /// Absolute path of the tool binary; its content hash is part of the key.
    pub tool: PathBuf,
    /// Extra text that distinguishes tool builds the binary hash cannot (a `--version` banner).
    #[serde(default)]
    pub tool_identity: String,
    /// Read-only paths the tool needs to run (its runtime closure); must include `tool`. See the
    /// type docs for what of them is keyed.
    pub runtime: Vec<PathBuf>,
    pub args: Vec<String>,
    /// The whole environment of the process. Nothing is inherited.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Files placed in the scratch directory, by relative path.
    #[serde(default)]
    pub inputs: BTreeMap<String, Input>,
    /// Relative paths of the files the tool must write; only these are kept.
    pub outputs: Vec<String>,
    /// Grant the host network. Part of the key.
    #[serde(default)]
    pub network: bool,
}

/// (size, mtime ns, ctime ns): changes whenever the file's bytes or metadata are written, and the
/// ctime cannot be set back by the file's owner.
pub(crate) type FileStamp = (u64, i64, i64);

pub(crate) fn file_stamp(metadata: &std::fs::Metadata) -> FileStamp {
    use std::os::unix::fs::MetadataExt;
    let nanos = |seconds: i64, nanoseconds: i64| {
        seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(nanoseconds)
    };
    (
        metadata.len(),
        nanos(metadata.mtime(), metadata.mtime_nsec()),
        nanos(metadata.ctime(), metadata.ctime_nsec()),
    )
}

pub(crate) fn relative(path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);
    ensure!(!path.is_empty(), "empty action path");
    ensure!(
        candidate
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "action path {path:?} must be relative with no `.` or `..`"
    );
    Ok(candidate.to_owned())
}

impl Action {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.tool.is_absolute(),
            "action tool must be an absolute path"
        );
        for path in self.inputs.keys().chain(&self.outputs) {
            relative(path)?;
        }
        ensure!(
            !self.outputs.is_empty(),
            "an action with no declared outputs has no result to keep"
        );
        for output in &self.outputs {
            ensure!(
                !self.inputs.contains_key(output),
                "output {output:?} is also an input"
            );
        }
        for (name, _) in &self.env {
            ensure!(
                !name.is_empty() && !name.contains('=') && !name.contains('\0'),
                "invalid environment name {name:?}"
            );
        }
        Ok(())
    }

    /// The action's identity: BLAKE3 over its canonical form and the tool's content hash.
    pub fn key(&self, tool_hash: &str) -> Result<String> {
        self.validate()?;
        let mut outputs = self.outputs.clone();
        outputs.sort();
        outputs.dedup();
        let mut runtime = self
            .runtime
            .iter()
            .map(|path| {
                let metadata = std::fs::metadata(path)
                    .with_context(|| format!("stat runtime path {}", path.display()))?;
                let stamp = metadata.is_file().then(|| file_stamp(&metadata));
                Ok((path.to_string_lossy().into_owned(), stamp))
            })
            .collect::<Result<Vec<_>>>()?;
        runtime.sort();
        let canonical = serde_json::to_vec(&serde_json::json!({
            "format": FORMAT,
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "tool": tool_hash,
            "tool_identity": self.tool_identity,
            "args": self.args,
            "env": self.env,
            "inputs": self.inputs,
            "outputs": outputs,
            "runtime": runtime,
            "network": self.network,
        }))
        .context("encode action")?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"loom-action-key-v1\0");
        hasher.update(&canonical);
        Ok(hasher.finalize().to_hex().to_string())
    }
}
