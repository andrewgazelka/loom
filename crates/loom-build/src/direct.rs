//! Replay Cargo's exact root compiler contract while dependencies live in CAS.
//! Cargo remains the cold graph resolver and build-script/proc-macro executor.
use crate::{BuildError, cargo_diagnostics};
use loom_check::CheckedDef;
use loom_proto::Diagnostic;
use loom_store::Store;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::{fs, process::Command};
pub(crate) mod artifacts;
pub(crate) mod compiler_cache;
pub(crate) mod definition_rlibs;
mod trusted_sources;

pub(crate) struct Built {
    pub bytes: Vec<u8>,
    pub logs: String,
    pub diagnostics: Vec<Diagnostic>,
    pub rustc_invocations: usize,
    /// Every millisecond of `build` attributed to a named stage; the caller
    /// absorbs this chain into the build-wide one.
    pub stages: crate::Stages,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Recipe {
    source: PathBuf,
    compiler: String,
    environment: BTreeMap<String, String>,
    arguments: Vec<String>,
    artifacts: BTreeMap<PathBuf, ArtifactFile>,
    #[serde(default)]
    units: Vec<artifacts::Unit>,
    #[serde(default)]
    layout: Option<Layout>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Layout {
    root: PathBuf,
    cache: PathBuf,
    sysroot: PathBuf,
}

/// One file of a graph's `target` directory as captured after the cold Cargo
/// bootstrap. `len` lets the warm replay confirm presence with a `stat`
/// instead of reading and hashing every artifact (`Recipe::artifacts_present`).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ArtifactFile {
    hash: String,
    executable: bool,
    len: u64,
}

fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}

mod compile;
mod entry_abi;
pub use entry_abi::compiled_source;
mod recipe;
pub(crate) use compile::{Request, build};
mod graph;
use graph::{RepairContext, read_graph, repair_units, write_graph};
mod admission;
use admission::{graph_shareable, materialize_root_workspace};
#[cfg(test)]
mod tests;

async fn run(command: Command) -> Result<std::process::Output, BuildError> {
    run_with_deadline(
        command,
        std::time::Duration::from_secs(300),
        "root Rust compiler",
    )
    .await
}

async fn bootstrap(command: Command) -> Result<std::process::Output, BuildError> {
    run_with_deadline(
        command,
        std::time::Duration::from_secs(900),
        "Cargo dependency bootstrap",
    )
    .await
}

async fn run_with_deadline(
    mut command: Command,
    deadline: std::time::Duration,
    phase: &str,
) -> Result<std::process::Output, BuildError> {
    tokio::time::timeout(deadline, command.kill_on_drop(true).output())
        .await
        .map_err(|_| rejected(format!("{phase} exceeded {} seconds", deadline.as_secs())))?
        .map_err(BuildError::from)
}

/// The environment every guest-side cargo or rustc invocation runs in: cleared,
/// then only the named variables pass through, plus the host linker.
pub(crate) fn compiler_environment(command: &mut Command) {
    let preserved = [
        "PATH",
        "HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "CARGO_HOME",
        "TMPDIR",
        "RUSTC",
    ];
    command.env_clear();
    for name in preserved {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("LANG", "C.UTF-8");
    host_linker(command);
}

/// Build scripts, proc macros and the driver link for the host. On macOS rustc
/// puts its sysroot's `lib` on the linker's dyld search path, so a clang that
/// loads `libLLVM.dylib` by name (nixpkgs') resolves the guest toolchain's copy
/// and aborts (`Symbol not found: _LLVMInitializeLanaiAsmParser`); Apple's `cc`
/// links against no LLVM dylib. An explicit `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER`
/// in the process environment wins.
pub(crate) fn host_linker(command: &mut Command) {
    #[cfg(target_os = "macos")]
    {
        let linker = std::env::var_os("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER")
            .unwrap_or_else(|| "/usr/bin/cc".into());
        command.env("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER", linker);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = command;
    }
}

fn rustc_diagnostics(stderr: &str) -> Vec<Diagnostic> {
    let wrapped = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|message| {
            serde_json::json!({"reason":"compiler-message","message":message}).to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    cargo_diagnostics(&wrapped)
}
