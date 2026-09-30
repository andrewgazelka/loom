use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::{Component, Path, PathBuf}};

/// Bumped when the sandbox profile, the layout of the scratch directory or the result
/// format changes in a way that could change what a tool produces.
const FORMAT: u32 = 1;

/// One declared input file: the stored blob and whether it must be executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub hash: String,
    #[serde(default)]
    pub executable: bool,
}

/// A process run as a function. Every field except `runtime`'s contents is part of the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    /// Absolute path of the tool binary; its content hash is part of the key.
    pub tool: PathBuf,
    /// Extra text that distinguishes tool builds the binary hash cannot (a `--version` banner).
    #[serde(default)]
    pub tool_identity: String,
    /// Read-only paths the tool needs to run (its runtime closure); must include `tool`.
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
        ensure!(self.tool.is_absolute(), "action tool must be an absolute path");
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
        let mut runtime: Vec<String> = self
            .runtime
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
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
