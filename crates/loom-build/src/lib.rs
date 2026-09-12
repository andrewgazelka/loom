//! Component builders. No successful response exists without component bytes.
mod artifact;
pub use artifact::cargo_diagnostics;
use artifact::{
    VENDOR_CONFIG, build_fingerprint, cargo_artifact, is_vendored, trusted_dependency,
    validate_component,
};
mod identity;
#[cfg(test)]
mod identity_tests;
mod intake;
mod materialize;
use materialize::{Materialization, materialize_rust};
mod direct;
mod handler_dependencies;
mod manifest;
mod preparation;
pub mod registry;
mod sdk;
mod threaded_module;
use loom_check::{CheckedDef, SourceBundle, SourceFile};
use loom_proto::{Diagnostic, Lang};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};
use tokio::{fs, process::Command, sync::Mutex};

// Build workspaces own mutable files; immutable SDK permissions must not leak in.
async fn seed_build_lock(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    fs::write(destination, fs::read(source).await?).await
}

// Fetching the compiler workspace resolves archives without executing build
// scripts. Do this before cache lookup: prepared definition metadata can outlive
// the host Cargo cache that supplies independently verified compiler sources.
async fn prepare_compiler_dependencies() -> Result<(), BuildError> {
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(compiler)
        .args(["--print", "sysroot"])
        .output()
        .await?;
    if !output.status.success() {
        return Err(BuildError::Rejected(format!(
            "compiler sysroot: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let sysroot = String::from_utf8(output.stdout)
        .map_err(|error| BuildError::Rejected(error.to_string()))?;
    let manifest = Path::new(sysroot.trim()).join("lib/rustlib/src/rust/library/Cargo.toml");
    let mut command = Command::new("cargo");
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "CARGO_HOME",
        "TMPDIR",
        "RUSTC",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
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
    Ok(())
}

pub struct Builder {
    store: loom_store::Store,
    root: PathBuf,
    cache: PathBuf,
    gate: Mutex<()>,
    driver_path: Option<PathBuf>,
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
            gate: Mutex::new(()),
            driver_path: std::env::var_os("LOOM_HASH_RUSTC").map(PathBuf::from),
        }
    }
    pub fn with_driver_path(mut self, path: PathBuf) -> Self {
        self.driver_path = Some(path);
        self
    }
    pub fn for_store(&self, store: loom_store::Store) -> Self {
        Self {
            store,
            root: self.root.clone(),
            cache: self.cache.clone(),
            gate: Mutex::new(()),
            driver_path: self.driver_path.clone(),
        }
    }
    pub async fn preflight(&self) -> Result<(), BuildError> {
        self.driver().await.map(|_| ())
    }
    async fn driver(&self) -> Result<identity::Driver, BuildError> {
        identity::Driver::prepare_with_path(&self.root, &self.cache, self.driver_path.as_deref())
            .await
    }
    pub async fn build(&self, definition: &CheckedDef) -> Result<BuildOutput, BuildError> {
        self.build_with_dependencies(definition, &BTreeMap::new())
            .await
    }
    pub async fn build_with_dependencies(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<BuildOutput, BuildError> {
        let _guard = self.gate.lock().await;
        let started = Instant::now();
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
        let driver = self.driver().await?;
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
            store: &self.store,
            root: &self.root,
            cache: &self.cache,
            directory: &directory,
            definition,
            isolated,
        })
        .await?;
        let materialization_ms = started.elapsed().as_millis();
        let mut built = direct::build(direct::Request {
            root: &self.root,
            cache: &self.cache,
            directory: &directory,
            definition,
            sdk_fingerprint: &inputs,
            driver: &driver,
            identity_directory: &directory,
            store: &self.store,
        })
        .await?;
        if !built.diagnostics.is_empty() {
            return Ok(BuildOutput {
                identity: None,
                component: Vec::new(),
                ms: started.elapsed().as_millis() as u64,
                logs: built.logs,
                diagnostics: built.diagnostics,
                rustc_invocations: built.rustc_invocations,
            });
        }
        let encoding_started = Instant::now();
        let mut component = threaded_module::prepare(&built.bytes).map_err(BuildError::Rejected)?;
        loom_proto::core_protocol::stamp(&mut component);
        if !loom_proto::core_protocol::is_current(&component) {
            return Err(BuildError::Rejected(
                "Rust compiler did not produce a core module".into(),
            ));
        }
        built.logs.push_str(&format!(
            "\n{}\n",
            serde_json::json!({"build_stages":{
                    "input_materialization_ms":materialization_ms,
                    "component_encode_ms":encoding_started.elapsed().as_millis()}})
        ));
        validate_component(&component)?;
        let identity = driver.ingest(&self.store, &directory, definition, &component)?;
        fs::write(&component_path, &component).await?;
        fs::write(directory.join("component.inputs"), inputs).await?;
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
        operation(&self.cache)
    }
}
#[cfg(test)]
mod tests;

mod compiler_cache_entry;
pub use compiler_cache_entry::compiler_cache_entry;
