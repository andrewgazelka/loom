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
mod trusted_sources;

pub(crate) struct Built {
    pub bytes: Vec<u8>,
    pub logs: String,
    pub diagnostics: Vec<Diagnostic>,
    pub rustc_invocations: usize,
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ArtifactFile {
    hash: String,
    executable: bool,
}

fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}

impl Recipe {
    fn parse(bytes: &[u8], source: &Path) -> Result<Self, BuildError> {
        let values = bytes
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8(value.to_vec()).map_err(rejected))
            .collect::<Result<Vec<_>, _>>()?;
        let separator = values
            .iter()
            .position(|value| value == "LOOM_RUSTC_ARGUMENTS")
            .ok_or_else(|| rejected("rustc capture has no argument boundary"))?;
        let mut environment = BTreeMap::new();
        for field in &values[..separator] {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| rejected("invalid captured rustc environment"))?;
            if ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"].contains(&name) {
                continue;
            }
            environment.insert(name.into(), value.into());
        }
        let compiler = values
            .get(separator + 1)
            .ok_or_else(|| rejected("rustc capture has no compiler"))?
            .clone();
        Ok(Self {
            source: source.into(),
            compiler,
            environment,
            arguments: values[separator + 2..].to_vec(),
            artifacts: BTreeMap::new(),
            units: Vec::new(),
            layout: None,
        })
    }

    fn output(&self) -> Result<PathBuf, BuildError> {
        let value = |flag: &str| {
            self.arguments
                .windows(2)
                .find(|parts| parts[0] == flag)
                .map(|parts| parts[1].as_str())
                .ok_or_else(|| rejected(format!("root rustc invocation missing {flag}")))
        };
        Ok(PathBuf::from(value("--out-dir")?).join(format!("{}.wasm", value("--crate-name")?)))
    }

    fn relocate(
        &mut self,
        directory: &Path,
        output: &Path,
        incremental: &Path,
    ) -> Result<(), BuildError> {
        let old = self
            .source
            .to_str()
            .ok_or_else(|| rejected("non-UTF8 source path"))?;
        let new = directory
            .to_str()
            .ok_or_else(|| rejected("non-UTF8 source path"))?;
        for argument in &mut self.arguments {
            *argument = argument.replace(old, new);
        }
        for value in self.environment.values_mut() {
            *value = value.replace(old, new);
        }
        std::fs::create_dir_all(output)?;
        let mut index = 0;
        while index < self.arguments.len() {
            if self.arguments[index] == "--out-dir" {
                self.arguments[index + 1] = output.to_string_lossy().into_owned();
            }
            if self.arguments[index] == "-C"
                && self
                    .arguments
                    .get(index + 1)
                    .is_some_and(|value| value.starts_with("incremental="))
            {
                self.arguments.drain(index..index + 2);
                continue;
            }
            index += 1;
        }
        self.arguments.extend([
            "-C".into(),
            format!("incremental={}", incremental.display()),
        ]);
        let mut previous = std::mem::take(&mut self.arguments).into_iter();
        while let Some(argument) = previous.next() {
            if argument == "--cap-lints" {
                previous.next();
            } else if !argument.starts_with("--cap-lints=") {
                self.arguments.push(argument);
            }
        }
        self.arguments.push("-Funsafe-code".into());
        self.source = directory.into();
        Ok(())
    }

    fn capture_artifacts(&mut self, store: &Store, target: &Path) -> Result<(), BuildError> {
        fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), BuildError> {
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                let name = entry.file_name();
                if name == "incremental"
                    || name == ".fingerprint"
                    || name == "root-rustc.recipe.units"
                {
                    continue;
                }
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    collect(&path, files)?;
                } else if entry.file_type()?.is_file() {
                    files.push(path);
                }
            }
            Ok(())
        }
        let root_name = self
            .arguments
            .windows(2)
            .find(|parts| parts[0] == "--crate-name")
            .ok_or_else(|| rejected("root crate has no name"))?[1]
            .clone();
        let mut files = Vec::new();
        collect(target, &mut files)?;
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy();
            if name == format!("{root_name}.wasm")
                || name == format!("{root_name}.d")
                || name == "root-rustc.recipe"
                || name == "direct.sh"
                || name.ends_with(".pending")
            {
                continue;
            }
            let hash = store
                .put("rust-artifact", &std::fs::read(&path)?)
                .map_err(rejected)?;
            let executable = artifacts::executable(&path)?;
            self.artifacts
                .insert(path, ArtifactFile { hash, executable });
        }
        Ok(())
    }

    fn restore_artifacts(&self, store: &Store) -> Result<bool, BuildError> {
        let mut complete = true;
        for (path, artifact) in &self.artifacts {
            let hash = &artifact.hash;
            if let Ok(bytes) = std::fs::read(path) {
                if blake3::hash(&bytes).to_hex().as_str() == hash
                    && store.codec(hash).map_err(rejected)?.is_some()
                    && artifacts::executable(path)? == artifact.executable
                {
                    continue;
                }
            }
            let Some(bytes) = store.get(hash).map_err(rejected)? else {
                complete = false;
                continue;
            };
            if blake3::hash(&bytes).to_hex().as_str() != hash {
                return Err(rejected("corrupt Rust artifact in CAS"));
            }
            artifacts::restore(
                &artifacts::Output {
                    path: path.clone(),
                    hash: hash.clone(),
                    executable: artifact.executable,
                },
                &bytes,
            )?;
        }
        Ok(complete)
    }
}

pub(crate) struct Request<'a> {
    pub root: &'a Path,
    pub cache: &'a Path,
    pub directory: &'a Path,
    pub definition: &'a CheckedDef,
    pub sdk_fingerprint: &'a str,
    pub store: &'a Store,
}

pub(crate) async fn build(request: Request<'_>) -> Result<Built, BuildError> {
    let started = std::time::Instant::now();
    let Request {
        root,
        cache,
        directory,
        definition,
        sdk_fingerprint,
        store,
    } = request;
    // Cargo canonicalizes paths (notably /tmp -> /private/tmp on macOS). Use
    // that same spelling for graph ownership and relocation, not string aliases.
    let root_path = std::fs::canonicalize(root)?;
    let cache_path = std::fs::canonicalize(cache)?;
    let directory_path = std::fs::canonicalize(directory)?;
    let root = root_path.as_path();
    let cache = cache_path.as_path();
    let directory = directory_path.as_path();
    let target_name = "wasm32-wasip1";
    let compiler_owner = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let mut compiler_command = Command::new(&compiler_owner);
    compiler_environment(&mut compiler_command);
    let compiler = compiler_command.arg("-vV").output().await?;
    if !compiler.status.success() {
        return Err(rejected(String::from_utf8_lossy(&compiler.stderr)));
    }
    let mut sysroot_command = Command::new(&compiler_owner);
    compiler_environment(&mut sysroot_command);
    let sysroot_output = sysroot_command
        .args(["--print", "sysroot"])
        .output()
        .await?;
    if !sysroot_output.status.success() {
        return Err(rejected(String::from_utf8_lossy(&sysroot_output.stderr)));
    }
    let sysroot = PathBuf::from(
        String::from_utf8(sysroot_output.stdout)
            .map_err(rejected)?
            .trim(),
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"loom-rustc-contract-v2-opt2-cgu16-no-lto-forbid-user-unsafe");
    let manifest_bytes = fs::read_to_string(directory.join("Cargo.toml"))
        .await?
        .replace(root.to_string_lossy().as_ref(), "$SDK")
        .replace(cache.to_string_lossy().as_ref(), "$CACHE")
        .into_bytes();
    let compiler_identity = String::from_utf8(compiler.stdout).map_err(rejected)?;
    for bytes in [
        manifest_bytes,
        fs::read(directory.join("Cargo.lock")).await?,
        compiler_identity.as_bytes().to_vec(),
        sdk_fingerprint.as_bytes().to_vec(),
        target_name.as_bytes().to_vec(),
    ] {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    // A caller may submit a previously prepared bundle. Bind the dependency
    // source identity as well as Cargo.lock, including old inline vendor input.
    if let Ok(bundle) = serde_json::from_str::<loom_check::SourceBundle>(&definition.source) {
        for (name, source) in &bundle.files {
            if name == crate::preparation::VENDOR_TREE || name.starts_with("vendor/") {
                let bytes = source.bytes().map_err(rejected)?;
                hasher.update(&(name.len() as u64).to_le_bytes());
                hasher.update(name.as_bytes());
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            }
        }
    }
    let key = hasher.finalize().to_hex().to_string();
    let graph = cache.join("rust-artifacts").join(&key);
    fs::create_dir_all(&graph).await?;
    let target = graph.join("target");
    let isolated = directory.join("vendor").is_dir();
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(directory.join("Cargo.toml")).await?)
            .map_err(rejected)?;
    let root_build_script = directory.join("build.rs").exists()
        || manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .is_some();
    if !root_build_script && let Some(mut recipe) = read_graph(store, &key)? {
        let setup_ms = started.elapsed().as_millis();
        let restore_started = std::time::Instant::now();
        let replay_compiler = if isolated {
            sysroot.join("bin/rustc").to_string_lossy().into_owned()
        } else {
            compiler_owner.clone()
        };
        recipe.rebase_graph(root, cache, directory, &sysroot, &replay_compiler)?;
        recipe.restore_sources(store, cache, &graph)?;
        let restored = recipe.restore_artifacts(store)?;
        let repairs = if restored {
            0
        } else {
            repair_units(
                &recipe.units,
                RepairContext {
                    store,
                    root,
                    cache,
                    directory,
                    target: &target,
                    isolated,
                },
            )
            .await?
        };
        if restored || recipe.restore_artifacts(store)? {
            let restore_ms = restore_started.elapsed().as_millis();
            let lineage = target
                .join("incremental")
                .join(blake3::hash(definition.name.as_bytes()).to_hex().as_str());
            fs::create_dir_all(&lineage).await?;
            recipe.relocate(directory, &target.join("root-output"), &lineage)?;
            let command = if isolated {
                fs::write(target.join("direct.sh"), recipe.shell()).await?;
                let mut command = Command::new(root.join("loom-rustc/sandbox.sh"));
                command
                    .arg("rustc")
                    .arg(cache)
                    .arg(directory)
                    .arg(&target)
                    .arg(root);
                command
            } else {
                let mut command = Command::new(&recipe.compiler);
                compiler_environment(&mut command);
                command
                    .args(&recipe.arguments)
                    .envs(&recipe.environment)
                    .current_dir(directory);
                command
            };
            let compiler_started = std::time::Instant::now();
            let output = run(command).await?;
            let compiler_ms = compiler_started.elapsed().as_millis();
            let stderr = String::from_utf8_lossy(&output.stderr);
            let diagnostics = rustc_diagnostics(&stderr);
            let stages = serde_json::json!({"build_stages":{"compiler_setup_ms":setup_ms,
                "artifact_restore_ms":restore_ms,"root_rustc_ms":compiler_ms}});
            let graph_identity = serde_json::json!({"dependency_graph":key});
            let logs = format!(
                "{graph_identity}\ndirect rustc; dependency graph {key}\n{stages}\n{stderr}"
            );
            if !output.status.success() {
                if diagnostics.is_empty() {
                    return Err(rejected(logs));
                }
                return Ok(Built {
                    bytes: Vec::new(),
                    logs,
                    diagnostics,
                    rustc_invocations: repairs + 1,
                });
            }
            return Ok(Built {
                bytes: fs::read(recipe.output()?).await?,
                logs,
                diagnostics,
                rustc_invocations: repairs + 1,
            });
        }
    }
    artifacts::initialize_index(store)?;
    let shareable = graph_shareable(root, cache, directory, &target, isolated).await?;
    let mirror = graph.join("unit-cache");
    compiler_cache::prepare(store, &mirror, &target, &compiler_identity)?;
    let helper_owner = std::env::var_os("LOOM_COMPILER_CACHE_OWNER")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?);
    let mut command = if isolated {
        let mut command = Command::new(root.join("loom-rustc/sandbox.sh"));
        // Sandbox target must be under its writable source root.
        command
            .arg("build")
            .arg(cache)
            .arg(directory)
            .arg(&target)
            .arg(root);
        command
    } else {
        let mut command = Command::new(root.join("loom-rustc/build.sh"));
        command.arg(directory).arg(&target);
        command
    };
    compiler_environment(&mut command);
    command
        .env("LOOM_LOCKED", "1")
        .env("LOOM_RUST_TARGET", target_name)
        .env("LOOM_CAS_SOURCES", cache.join("source-trees"))
        .env("LOOM_COMPILER_CACHE_OWNER", helper_owner)
        .env("LOOM_COMPILER_CACHE_MIRROR", &mirror);
    let output = run(command).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let graph_identity = serde_json::json!({"dependency_graph":key});
    let logs = format!(
        "{graph_identity}\ncargo dependency bootstrap; graph {key}; cross-graph publication {shareable}\n{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let diagnostics = cargo_diagnostics(&stdout);
    let rustc_invocations = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| *line == "loom-rustc-invocation")
        .count();
    if !output.status.success() {
        if diagnostics.is_empty() {
            return Err(rejected(logs));
        }
        return Ok(Built {
            bytes: Vec::new(),
            logs,
            diagnostics,
            rustc_invocations,
        });
    }
    let artifact = crate::cargo_artifact(&stdout, &directory.join("Cargo.toml"), &target)?;
    if !root_build_script {
        let mut recipe = Recipe::parse(
            &fs::read(target.join("root-rustc.recipe")).await?,
            directory,
        )?;
        let mut source_roots = vec![
            cache.to_owned(),
            root.join("crates/loom-guest-rs"),
            root.join("crates/loom-guest-macros"),
            root.join("crates/loom-proto"),
        ];
        if !isolated {
            let cargo_home = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
                .ok_or_else(|| rejected("Cargo home is unavailable"))?;
            source_roots.push(cargo_home.join("registry/src"));
        }
        for source_root in &mut source_roots {
            *source_root = std::fs::canonicalize(&*source_root)?;
        }
        recipe.units = artifacts::capture(
            store,
            &target,
            &compiler_identity,
            &stdout,
            &source_roots,
            shareable,
        )?;
        recipe.capture_artifacts(store, &target)?;
        recipe.layout = Some(Layout {
            root: root.into(),
            cache: cache.into(),
            sysroot,
        });
        write_graph(store, &key, &recipe)?;
    }
    Ok(Built {
        bytes: fs::read(artifact).await?,
        logs,
        diagnostics,
        rustc_invocations,
    })
}

async fn run(mut command: Command) -> Result<std::process::Output, BuildError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        command.kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| rejected("Rust compiler exceeded 300 seconds"))?
    .map_err(BuildError::from)
}

fn compiler_environment(command: &mut Command) {
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
}

impl Recipe {
    fn rebase_graph(
        &mut self,
        root: &Path,
        cache: &Path,
        directory: &Path,
        sysroot: &Path,
        compiler: &str,
    ) -> Result<(), BuildError> {
        let layout = self
            .layout
            .clone()
            .ok_or_else(|| rejected("compiler graph has no path layout"))?;
        let old_source = self.source.clone();
        let replace = |value: &str| {
            value
                .replace(
                    old_source.to_string_lossy().as_ref(),
                    directory.to_string_lossy().as_ref(),
                )
                .replace(
                    layout.cache.to_string_lossy().as_ref(),
                    cache.to_string_lossy().as_ref(),
                )
                .replace(
                    layout.root.to_string_lossy().as_ref(),
                    root.to_string_lossy().as_ref(),
                )
                .replace(
                    layout.sysroot.to_string_lossy().as_ref(),
                    sysroot.to_string_lossy().as_ref(),
                )
        };
        fn rebase(recipe: &mut Recipe, replace: &impl Fn(&str) -> String) {
            recipe.source = PathBuf::from(replace(recipe.source.to_string_lossy().as_ref()));
            for argument in &mut recipe.arguments {
                *argument = replace(argument);
            }
            for value in recipe.environment.values_mut() {
                *value = replace(value);
            }
            recipe.artifacts = std::mem::take(&mut recipe.artifacts)
                .into_iter()
                .map(|entry| {
                    (
                        PathBuf::from(replace(entry.0.to_string_lossy().as_ref())),
                        entry.1,
                    )
                })
                .collect();
        }
        rebase(self, &replace);
        self.compiler = compiler.into();
        for unit in &mut self.units {
            rebase(&mut unit.recipe, &replace);
            unit.recipe.compiler = compiler.into();
            for output in &mut unit.outputs {
                output.path = PathBuf::from(replace(output.path.to_string_lossy().as_ref()));
            }
        }
        self.layout = Some(Layout {
            root: root.into(),
            cache: cache.into(),
            sysroot: sysroot.into(),
        });
        Ok(())
    }

    fn restore_sources(
        &mut self,
        store: &Store,
        cache: &Path,
        graph: &Path,
    ) -> Result<(), BuildError> {
        #[derive(serde::Deserialize)]
        struct SourceIdentity {
            source_tree: String,
        }
        for unit in &mut self.units {
            if unit.recipe.source.is_dir() {
                continue;
            }
            let inputs: SourceIdentity = store
                .get_value(&unit.key)
                .map_err(rejected)?
                .ok_or_else(|| rejected("compilation source identity missing"))?;
            let directory = graph.join("sources").join(&unit.key);
            crate::preparation::materialize_tree(store, cache, &directory, &inputs.source_tree)?;
            let old = unit.recipe.source.to_string_lossy().into_owned();
            let new = std::fs::canonicalize(directory)?;
            for argument in &mut unit.recipe.arguments {
                *argument = argument.replace(&old, new.to_string_lossy().as_ref());
            }
            for value in unit.recipe.environment.values_mut() {
                *value = value.replace(&old, new.to_string_lossy().as_ref());
            }
            unit.recipe.source = new;
        }
        Ok(())
    }

    fn shell(&self) -> String {
        fn quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }
        let mut script = String::from("#!/bin/sh\nset -eu\n");
        script.push_str(&format!(
            "cd {}\n",
            quote(self.source.to_string_lossy().as_ref())
        ));
        for (name, value) in &self.environment {
            script.push_str(&format!("export {}={}\n", name, quote(value)));
        }
        script.push_str("exec ");
        script.push_str(&quote(&self.compiler));
        for argument in &self.arguments {
            script.push(' ');
            script.push_str(&quote(argument));
        }
        script.push('\n');
        script
    }
}

fn read_graph(store: &Store, key: &str) -> Result<Option<Recipe>, BuildError> {
    let hash = store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_build_graphs (key TEXT PRIMARY KEY, recipe_hash TEXT NOT NULL)")?;
        let mut statement = connection.prepare("SELECT recipe_hash FROM rust_build_graphs WHERE key=?")?;
        let mut rows = statement.query_map([key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }).map_err(rejected)?;
    let Some(hash) = hash else {
        return Ok(None);
    };
    store.get_value(&hash).map_err(rejected)
}

fn write_graph(store: &Store, key: &str, recipe: &Recipe) -> Result<(), BuildError> {
    let hash = store
        .put_value("rust-build-recipe", recipe)
        .map_err(rejected)?;
    store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_build_graphs (key TEXT PRIMARY KEY, recipe_hash TEXT NOT NULL)")?;
        connection.execute("INSERT INTO rust_build_graphs VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET recipe_hash=excluded.recipe_hash", [key, &hash])?;
        Ok(())
    }).map_err(rejected)
}

struct RepairContext<'a> {
    store: &'a Store,
    root: &'a Path,
    cache: &'a Path,
    directory: &'a Path,
    target: &'a Path,
    isolated: bool,
}

async fn repair_units(
    units: &[artifacts::Unit],
    context: RepairContext<'_>,
) -> Result<usize, BuildError> {
    let mut invocations = 0;
    for unit in units {
        let mut missing = false;
        for output in &unit.outputs {
            if context
                .store
                .codec(&output.hash)
                .map_err(rejected)?
                .is_none()
            {
                missing = true;
                continue;
            }
            if let Ok(bytes) = std::fs::read(&output.path) {
                if blake3::hash(&bytes).to_hex().as_str() == output.hash {
                    continue;
                }
            }
            let bytes = context
                .store
                .get(&output.hash)
                .map_err(rejected)?
                .ok_or_else(|| rejected("Rust artifact disappeared during restoration"))?;
            artifacts::restore(output, &bytes)?;
        }
        if !missing {
            continue;
        }
        let command = if context.isolated {
            fs::write(context.target.join("direct.sh"), unit.recipe.shell()).await?;
            let mut command = Command::new(context.root.join("loom-rustc/sandbox.sh"));
            command
                .arg("rustc")
                .arg(context.cache)
                .arg(context.directory)
                .arg(context.target)
                .arg(context.root);
            command
        } else {
            let mut command = Command::new(&unit.recipe.compiler);
            compiler_environment(&mut command);
            command
                .args(&unit.recipe.arguments)
                .envs(&unit.recipe.environment)
                .current_dir(&unit.recipe.source);
            command
        };
        let output = run(command).await?;
        invocations += 1;
        if !output.status.success() {
            return Err(rejected(format!(
                "rebuild {}: {}",
                unit.name,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        for artifact in &unit.outputs {
            let bytes = fs::read(&artifact.path).await?;
            let hash = context
                .store
                .put("rust-artifact", &bytes)
                .map_err(rejected)?;
            if hash != artifact.hash {
                return Err(rejected(format!(
                    "non-reproducible artifact for {}: expected {}, got {hash}",
                    unit.name, artifact.hash
                )));
            }
        }
    }
    Ok(invocations)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_execution_is_admitted_before_build_scripts_run() {
        let directory =
            std::env::temp_dir().join(format!("loom-host-admission-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("src")).unwrap();
        std::fs::write(directory.join("Cargo.toml"), "[package]\nname='unreviewed-host-code'\nversion='0.1.0'\nedition='2024'\n[workspace]\n").unwrap();
        std::fs::write(
            directory.join("Cargo.lock"),
            "version = 4\n[[package]]\nname = \"unreviewed-host-code\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(directory.join("src/lib.rs"), "pub fn value() -> u8 { 1 }").unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        assert!(
            graph_shareable(
                root,
                &directory,
                &directory,
                &directory.join("target"),
                false
            )
            .await
            .unwrap()
        );
        std::fs::write(
            directory.join("build.rs"),
            "fn main() { std::fs::write(\"executed\", b\"bad\").unwrap(); }",
        )
        .unwrap();
        assert!(
            !graph_shareable(
                root,
                &directory,
                &directory,
                &directory.join("target"),
                false
            )
            .await
            .unwrap()
        );
        assert!(!directory.join("executed").exists());
        let store = Store::memory().unwrap();
        artifacts::initialize_index(&store).unwrap();
        let key = "ab".repeat(32);
        let mut unit = artifacts::Unit {
            key: key.clone(),
            name: "known_dependency".into(),
            recipe: Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap(),
            outputs: vec![artifacts::Output {
                path: directory.join("dependency.rlib"),
                hash: store
                    .put("rust-artifact", b"original compiler output")
                    .unwrap(),
                executable: false,
            }],
            dependencies: Vec::new(),
        };
        artifacts::publish_units(&store, true, &[unit.clone()]).unwrap();
        let original = store
            .with_connection(|connection| {
                Ok(connection.query_row(
                    "SELECT artifact_hash FROM rust_artifacts WHERE key=?",
                    [&key],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        unit.outputs[0].hash = store
            .put("rust-artifact", b"poisoned sibling output")
            .unwrap();
        let admitted = graph_shareable(
            root,
            &directory,
            &directory,
            &directory.join("target"),
            false,
        )
        .await
        .unwrap();
        artifacts::publish_units(&store, admitted, &[unit]).unwrap();
        let after = store
            .with_connection(|connection| {
                Ok(connection.query_row(
                    "SELECT artifact_hash FROM rust_artifacts WHERE key=?",
                    [&key],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            after, original,
            "unreviewed host code replaced a shared artifact"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn capture_preserves_spaces_and_package_values() {
        let recipe = Recipe::parse(b"CARGO_PKG_DESCRIPTION=a = b\0LOOM_RUSTC_ARGUMENTS\0/opt/rust c\0--crate-name\0loom_definition\0--out-dir\0/a b\0", Path::new("/old")).unwrap();
        assert_eq!(recipe.environment["CARGO_PKG_DESCRIPTION"], "a = b");
        assert_eq!(recipe.compiler, "/opt/rust c");
        assert_eq!(
            recipe.output().unwrap(),
            Path::new("/a b/loom_definition.wasm")
        );
    }
    #[test]
    fn direct_compiler_errors_become_checker_diagnostics() {
        let messages = rustc_diagnostics(
            "warning\n{\"level\":\"error\",\"message\":\"bad type\",\"code\":{\"code\":\"E0308\"},\"spans\":[],\"children\":[]}\n",
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].code, "E0308");
    }
    #[test]
    fn corrupt_materialization_is_replaced_from_cas() {
        let store = Store::memory().unwrap();
        let directory = std::env::temp_dir().join(format!(
            "loom-artifact-integrity-01a0866a-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("dependency.rlib");
        let hash = store.put("rust-artifact", b"verified artifact").unwrap();
        std::fs::write(&path, b"corrupted artifact").unwrap();
        let mut recipe = Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap();
        recipe.artifacts.insert(
            path.clone(),
            ArtifactFile {
                hash,
                executable: false,
            },
        );
        assert!(recipe.restore_artifacts(&store).unwrap());
        assert_eq!(std::fs::read(path).unwrap(), b"verified artifact");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restored_build_script_retains_execute_permission() {
        use std::os::unix::fs::PermissionsExt;
        let store = Store::memory().unwrap();
        let directory = std::env::temp_dir().join(format!(
            "loom-artifact-executable-01a0866a-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("build-script-build");
        let hash = store
            .put("rust-artifact", b"compiled build script")
            .unwrap();
        std::fs::write(&path, b"compiled build script").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut recipe = Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap();
        recipe.artifacts.insert(
            path.clone(),
            ArtifactFile {
                hash,
                executable: true,
            },
        );
        assert!(recipe.restore_artifacts(&store).unwrap());
        assert_ne!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o111,
            0
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

async fn graph_shareable(
    root: &Path,
    cache: &Path,
    directory: &Path,
    target: &Path,
    isolated: bool,
) -> Result<bool, BuildError> {
    let mut command = if isolated {
        let mut command = Command::new(root.join("loom-rustc/sandbox.sh"));
        command
            .arg("metadata")
            .arg(cache)
            .arg(directory)
            .arg(target)
            .arg(root);
        command
    } else {
        let mut command = Command::new("cargo");
        command.current_dir(directory).args([
            "metadata",
            "--locked",
            "--offline",
            "--format-version=1",
        ]);
        command
    };
    compiler_environment(&mut command);
    let output = run(command).await?;
    if !output.status.success() {
        return Err(rejected(String::from_utf8_lossy(&output.stderr)));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(rejected)?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| rejected("Cargo metadata has no packages"))?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or_else(|| rejected("Cargo metadata has no resolved graph"))?;
    let mut pending = Vec::new();
    for package in packages {
        let targets = package["targets"]
            .as_array()
            .ok_or_else(|| rejected("Cargo package has no targets"))?;
        if targets.iter().any(|target| {
            target["kind"].as_array().is_some_and(|kinds| {
                kinds
                    .iter()
                    .any(|kind| kind == "proc-macro" || kind == "custom-build")
            })
        }) {
            pending.push(
                package["id"]
                    .as_str()
                    .ok_or_else(|| rejected("Cargo package has no identity"))?
                    .to_owned(),
            );
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let package = packages
            .iter()
            .find(|package| package["id"] == id)
            .ok_or_else(|| rejected("Cargo host dependency is missing"))?;
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or_else(|| rejected("Cargo host dependency has no source"))?;
        if !trusted_sources::approved(root, Path::new(manifest))? {
            return Ok(false);
        }
        let node = nodes
            .iter()
            .find(|node| node["id"] == id)
            .ok_or_else(|| rejected("Cargo host dependency has no resolution"))?;
        for dependency in node["deps"]
            .as_array()
            .ok_or_else(|| rejected("Cargo host node has no dependencies"))?
        {
            if let Some(id) = dependency["pkg"].as_str() {
                pending.push(id.into());
            }
        }
    }
    Ok(true)
}
