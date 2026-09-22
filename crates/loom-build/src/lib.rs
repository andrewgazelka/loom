//! Component builders. No successful response exists without component bytes.
mod artifact;
pub use artifact::cargo_diagnostics;
use artifact::{
    VENDOR_CONFIG, build_fingerprint, cargo_artifact, is_vendored, trusted_dependency,
    validate_component,
};
mod toolchain;
pub use toolchain::{GuestToolchain, resolve_guest_toolchain, resolve_guest_toolchain_with_driver};
mod identity;
#[cfg(test)]
mod identity_tests;
mod intake;
pub use intake::Preparation;
mod materialize;
use materialize::{Materialization, materialize_rust};
mod direct;
pub use direct::compiled_source;
mod dwarf;
mod handler_dependencies;
mod manifest;
mod preparation;
mod prepared;
pub use preparation::VENDOR_TREE;
pub mod registry;
mod sdk;
mod stages;
use stages::Stages;
mod threaded_module;
use loom_check::{CheckedDef, SourceBundle, SourceFile};
use loom_proto::{Diagnostic, Lang};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, process::Command, sync::Mutex};

// Build workspaces own mutable files; immutable SDK permissions must not leak in.
async fn seed_build_lock(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    fs::write(destination, fs::read(source).await?).await
}

/// `cargo fetch --locked` over the guest compiler's standard library workspace,
/// so a later `-Zbuild-std` bootstrap never reaches the network from inside the
/// builder lock. Runs once per (sysroot, library lock) for the life of this
/// process; the memo lives in `Builder::compiler_dependencies`, shared by every
/// builder clone, and a different sysroot or a changed library lock reruns the
/// fetch. A registry purged while the daemon runs is repaired by the next cold
/// Cargo bootstrap, which fetches what it lacks; nothing on the warm replay
/// path reads the registry.
async fn prepare_compiler_dependencies(
    memo: &std::sync::Mutex<Option<String>>,
    toolchain: &GuestToolchain,
) -> Result<(), BuildError> {
    let manifest = toolchain
        .sysroot
        .join("lib/rustlib/src/rust/library/Cargo.toml");
    let mut hasher = blake3::Hasher::new();
    hasher.update(toolchain.sysroot.as_os_str().as_encoded_bytes());
    hasher.update(&std::fs::read(manifest.with_file_name("Cargo.lock"))?);
    let key = hasher.finalize().to_hex().to_string();
    if memo
        .lock()
        .map_err(|_| BuildError::Rejected("compiler dependency memo poisoned".into()))?
        .as_deref()
        == Some(key.as_str())
    {
        return Ok(());
    }
    let mut command = Command::new(&toolchain.cargo);
    direct::compiler_environment(&mut command);
    toolchain.configure(&mut command)?;
    command
        .env("RUSTC_BOOTSTRAP", "1")
        .args(["fetch", "--locked", "--manifest-path"])
        .arg(manifest);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        command.kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| {
        BuildError::Rejected("compiler dependency intake exceeded 300 seconds".into())
    })??;
    if !output.status.success() {
        return Err(BuildError::Rejected(format!(
            "compiler dependency intake: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    *memo
        .lock()
        .map_err(|_| BuildError::Rejected("compiler dependency memo poisoned".into()))? = Some(key);
    Ok(())
}

/// A cached component is served only if it is the build of THIS definition: it
/// must export the SDK allocator and one `loom_call_<entry>` wrapper per
/// checked export. Anything else (a foreign module written under the same
/// hash, a build interrupted between steps) is rebuilt instead of served.
fn component_serves(component: &[u8], definition: &CheckedDef) -> Result<(), String> {
    let mut exports = std::collections::BTreeSet::new();
    for payload in wasmparser::Parser::new(0).parse_all(component) {
        if let wasmparser::Payload::ExportSection(section) = payload.map_err(|e| e.to_string())? {
            for export in section {
                exports.insert(export.map_err(|e| e.to_string())?.name.to_owned());
            }
        }
    }
    let mut required = vec!["loom_alloc".to_owned()];
    required.extend(
        definition
            .sig
            .exports
            .iter()
            .map(|entry| format!("loom_call_{}", entry.name)),
    );
    match required.iter().find(|name| !exports.contains(*name)) {
        None => Ok(()),
        Some(missing) => Err(format!("cached component lacks export {missing}")),
    }
}

/// The build cache has one writer at a time across processes as well as
/// tasks: every `Builder` on the same cache directory, in this daemon or in a
/// test process beside it, takes `<cache>/.builder.lock` (flock) before it
/// touches the shared cargo graph. Released when dropped.
struct CacheLock(std::fs::File);
impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.0);
    }
}
async fn lock_cache(cache: &Path) -> Result<CacheLock, BuildError> {
    std::fs::create_dir_all(cache)?;
    let path = cache.join(".builder.lock");
    let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        fs4::fs_std::FileExt::lock_exclusive(&file)?;
        Ok(file)
    })
    .await
    .map_err(|error| BuildError::Rejected(format!("build lock task: {error}")))??;
    Ok(CacheLock(file))
}

pub struct Builder {
    store: loom_store::Store,
    root: PathBuf,
    cache: PathBuf,
    gate: Arc<Mutex<()>>,
    driver_path: Option<PathBuf>,
    /// Last toolchain + driver resolution for this cache directory; see `prepared`.
    prepared: Arc<prepared::Memo>,
    /// Key of the last successful standard-library fetch; see `prepare_compiler_dependencies`.
    compiler_dependencies: Arc<std::sync::Mutex<Option<String>>>,
}
#[derive(Debug)]
pub struct BuildOutput {
    pub identity: Option<loom_proto::BuildIdentity>,
    pub component: Vec<u8>,
    pub ms: u64,
    pub logs: String,
    pub diagnostics: Vec<Diagnostic>,
    /// Compiler processes performing a compilation, excluding version probes.
    pub rustc_invocations: usize,
}
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("component build I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("component build rejected: {0}")]
    Rejected(String),
}
impl Builder {
    pub fn new(root: PathBuf, store: loom_store::Store) -> Self {
        let cache = std::env::var_os("LOOM_BUILD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".loom-build"));
        Self {
            store,
            root,
            cache,
            gate: Arc::new(Mutex::new(())),
            driver_path: std::env::var_os("LOOM_HASH_RUSTC").map(PathBuf::from),
            prepared: Arc::default(),
            compiler_dependencies: Arc::default(),
        }
    }
    /// Mutable build workspaces have one owner. Tenant services select their
    /// cache directory explicitly; process-wide environment overrides cannot
    /// make tenant eviction or materialization touch a sibling's workspace.
    /// The pinned driver is built under the cache, so the toolchain memo is
    /// per cache directory too.
    pub fn for_cache_directory(&self, cache: PathBuf) -> Self {
        Self {
            store: self.store.clone(),
            root: self.root.clone(),
            cache,
            gate: Arc::new(Mutex::new(())),
            driver_path: self.driver_path.clone(),
            prepared: Arc::default(),
            compiler_dependencies: self.compiler_dependencies.clone(),
        }
    }
    pub fn cache_directory(&self) -> &Path {
        &self.cache
    }
    /// Selecting a driver changes what the memo resolves; drop it.
    pub fn with_driver_path(mut self, path: PathBuf) -> Self {
        self.driver_path = Some(path);
        self.prepared = Arc::default();
        self
    }
    pub fn for_store(&self, store: loom_store::Store) -> Self {
        Self {
            store,
            root: self.root.clone(),
            cache: self.cache.clone(),
            gate: self.gate.clone(),
            driver_path: self.driver_path.clone(),
            prepared: self.prepared.clone(),
            compiler_dependencies: self.compiler_dependencies.clone(),
        }
    }
    pub async fn preflight(&self) -> Result<(), BuildError> {
        let _guard = self.gate.lock().await;
        self.prepared().await.map(|_| ())
    }
    /// `source` as the pinned guest toolchain's rustfmt prints it: `<sysroot>/
    /// bin/rustfmt --edition 2024 --emit stdout`, the source on stdin, at most
    /// ten seconds, in the same cleared environment as every other compiler
    /// invocation, with the toolchain's sysroot as working directory so no
    /// caller `rustfmt.toml` is found. Uses the same memoized toolchain
    /// resolution as a build, so the first call in a process pays that
    /// resolution (and, before any build, the driver preparation). A rustfmt
    /// failure is an error naming its diagnostic; nothing is formatted
    /// approximately.
    pub async fn format_rust(&self, source: &str) -> Result<String, BuildError> {
        let (toolchain, _) = self.prepared().await?;
        let rustfmt = toolchain.sysroot.join("bin/rustfmt");
        let owner = format!("rustfmt {}", rustfmt.display());
        let mut command = Command::new(&rustfmt);
        direct::compiler_environment(&mut command);
        command
            .args(["--edition", "2024", "--emit", "stdout"])
            .current_dir(&toolchain.sysroot)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| BuildError::Rejected(format!("{owner}: {error}")))?;
        let mut stdin = child.stdin.take().expect("rustfmt stdin is piped");
        let input = source.as_bytes().to_vec();
        let output = tokio::time::timeout(Duration::from_secs(10), async move {
            use tokio::io::AsyncWriteExt;
            // rustfmt reads all of stdin before it writes anything, so the
            // write completes before the output is awaited.
            stdin.write_all(&input).await?;
            stdin.shutdown().await?;
            drop(stdin);
            child.wait_with_output().await
        })
        .await
        .map_err(|_| BuildError::Rejected(format!("{owner} exceeded 10 seconds")))?
        .map_err(|error| BuildError::Rejected(format!("{owner}: {error}")))?;
        if !output.status.success() {
            return Err(BuildError::Rejected(format!(
                "{owner}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| BuildError::Rejected(format!("{owner}: {error}")))
    }
    /// The guest toolchain and hash-rustc driver, resolved once and reused while
    /// `prepared::fingerprint` and the resolved binaries are unchanged.
    async fn prepared(&self) -> Result<(Arc<GuestToolchain>, Arc<identity::Driver>), BuildError> {
        self.prepared
            .prepare(&self.root, &self.cache, self.driver_path.as_deref())
            .await
    }
    pub async fn build(&self, definition: &CheckedDef) -> Result<BuildOutput, BuildError> {
        self.build_with_dependencies(definition, &BTreeMap::new())
            .await
    }
    /// Compile `definition` against its already checked dependency closure.
    /// `logs` ends with one `build_stages` JSON line attributing the whole
    /// `ms` span to named stages (`stages.rs`); a `build_stages_warning` line
    /// follows when more than five percent belongs to no stage.
    pub async fn build_with_dependencies(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<BuildOutput, BuildError> {
        let _guard = self.gate.lock().await;
        let _cache_lock = lock_cache(&self.cache).await?;
        let started = Instant::now();
        let mut stages = Stages::start();
        if !definition.diagnostics.is_empty() {
            return Err(BuildError::Rejected(
                "definition has checker diagnostics".into(),
            ));
        }
        if definition.hash.len() != 64
            || !definition.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(BuildError::Rejected("invalid definition hash".into()));
        }
        let directory = self.cache.join(&definition.hash);
        fs::create_dir_all(&directory).await?;
        let component_path = directory.join("component.wasm");
        let (toolchain, driver) = self.prepared().await?;
        stages.checkpoint("toolchain_prepare_ms");
        let inputs = format!(
            "{}:{}",
            build_fingerprint(&self.root)?,
            driver.toolchain_hash
        );
        let cached_inputs = fs::read_to_string(directory.join("component.inputs"))
            .await
            .ok();
        if cached_inputs.as_deref() == Some(inputs.as_str())
            && let Ok(component) = fs::read(&component_path).await
            && loom_proto::core_protocol::is_current(&component)
            && component_serves(&component, definition).is_ok()
        {
            validate_component(&component)?;
            return Ok(BuildOutput {
                identity: Some(driver.ingest(&self.store, &directory, definition, &component)?),
                component,
                ms: started.elapsed().as_millis() as u64,
                logs: "component cache hit".into(),
                diagnostics: Vec::new(),
                rustc_invocations: 0,
            });
        }
        if !is_vendored(definition) && dependencies.values().any(is_vendored) {
            return Err(BuildError::Rejected("A crate using vendored dependencies must be prepared by the isolated vendor worker".into()));
        }
        for dependency in dependencies.values() {
            if dependency.hash.len() != 64
                || !dependency.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(BuildError::Rejected("invalid dependency hash".into()));
            }
            materialize_rust(Materialization {
                store: &self.store,
                root: &self.root,
                cache: &self.cache,
                directory: &self.cache.join("sources").join(&dependency.hash),
                definition: dependency,
                dependencies,
                dependency: true,
                isolated: is_vendored(dependency),
            })
            .await?;
        }
        let isolated = is_vendored(definition);
        materialize_rust(Materialization {
            store: &self.store,
            root: &self.root,
            cache: &self.cache,
            directory: &directory,
            definition,
            dependencies,
            dependency: false,
            isolated,
        })
        .await?;
        sdk::reconcile(sdk::Rebuild {
            toolchain: &toolchain,
            store: &self.store,
            root: &self.root,
            cache: &self.cache,
            directory: &directory,
            definition,
            isolated,
        })
        .await?;
        stages.checkpoint("input_materialization_ms");
        let mut built = direct::build(direct::Request {
            root: &self.root,
            cache: &self.cache,
            directory: &directory,
            definition,
            dependencies,
            sdk_fingerprint: &inputs,
            toolchain: &toolchain,
            driver: &driver,
            identity_directory: &directory,
            store: &self.store,
        })
        .await?;
        stages.absorb(built.stages);
        if !built.diagnostics.is_empty() {
            built.logs.push('\n');
            built.logs.push_str(&stages.log_lines());
            return Ok(BuildOutput {
                identity: None,
                component: Vec::new(),
                ms: started.elapsed().as_millis() as u64,
                logs: built.logs,
                diagnostics: built.diagnostics,
                rustc_invocations: built.rustc_invocations,
            });
        }
        let mut component = match threaded_module::prepare(&built.bytes) {
            Ok(component) => component,
            Err(error) => {
                // Keep the exact input so the refusal can be reproduced offline
                // (`LOOM_DWARF_FIXTURE=<path> cargo test -p loom-build relocates_a_real -- --ignored`).
                let kept = self
                    .cache
                    .join("rejected-modules")
                    .join(format!("{}.wasm", definition.hash));
                let note = match std::fs::create_dir_all(kept.parent().unwrap())
                    .and_then(|()| std::fs::write(&kept, &built.bytes))
                {
                    Ok(()) => format!("; input module kept at {}", kept.display()),
                    Err(io) => format!("; input module not kept: {io}"),
                };
                return Err(BuildError::Rejected(format!("{error}{note}")));
            }
        };
        loom_proto::core_protocol::stamp(&mut component);
        if !loom_proto::core_protocol::is_current(&component) {
            return Err(BuildError::Rejected(
                "Rust compiler did not produce a core module".into(),
            ));
        }
        validate_component(&component)?;
        stages.checkpoint("component_encode_ms");
        let identity = driver.ingest(&self.store, &directory, definition, &component)?;
        stages.checkpoint("identity_ingest_ms");
        fs::write(&component_path, &component).await?;
        fs::write(directory.join("component.inputs"), inputs).await?;
        stages.checkpoint("output_persist_ms");
        built.logs.push('\n');
        built.logs.push_str(&stages.log_lines());
        fs::write(directory.join("build.log"), &built.logs).await?;
        Ok(BuildOutput {
            identity: Some(identity),
            component,
            ms: started.elapsed().as_millis() as u64,
            logs: built.logs,
            diagnostics: built.diagnostics,
            rustc_invocations: built.rustc_invocations,
        })
    }
}
impl Builder {
    /// Serialize filesystem maintenance with component builds using this builder.
    pub async fn with_cache_exclusive<T>(
        &self,
        operation: impl FnOnce(&std::path::Path) -> T,
    ) -> T {
        let _guard = self.gate.lock().await;
        let _cache_lock = lock_cache(&self.cache).await;
        operation(&self.cache)
    }
}
#[cfg(test)]
mod tests;

mod compiler_cache_entry;
pub use compiler_cache_entry::compiler_cache_entry;
