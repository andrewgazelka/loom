//! Component builders. No successful response exists without component bytes.
pub mod registry;
mod sdk;
mod direct;
mod preparation;
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

pub struct Builder {
    store: loom_store::Store,
    root: PathBuf,
    cache: PathBuf,
    gate: Mutex<()>,
}
#[derive(Debug)]
pub struct BuildOutput {
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
        }
    }
    /// Resolve the Rust dependency lock before assigning the executable identity.
    /// Untrusted dependencies are fetched in the network-only sandbox phase.
    pub async fn prepare_rust_source(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<String, BuildError> {
        if definition.lang != Lang::Rust {
            return Ok(definition.source.clone());
        }
        if !definition.diagnostics.is_empty() {
            return Err(BuildError::Rejected("definition has diagnostics".into()));
        }
        if definition.hash.len() != 64
            || !definition.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(BuildError::Rejected("invalid definition hash".into()));
        }
        if is_vendored(definition) {
            return Ok(definition.source.clone());
        }
        let _guard = self.gate.lock().await;
        let mut bundle = if definition.source.trim_start().starts_with('{') {
            let bundle: SourceBundle = serde_json::from_str(&definition.source)
                .map_err(|error| BuildError::Rejected(error.to_string()))?;
            bundle.validate().map_err(BuildError::Rejected)?;
            bundle
        } else {
            let mut files = BTreeMap::new();
            files.insert(
                "src/lib.rs".into(),
                SourceFile::Text(definition.source.clone()),
            );
            files.insert("Cargo.toml".into(),SourceFile::Text("[package]\nname=\"loom-definition\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\nserde_json=\"1\"\n".into()));
            SourceBundle { files }
        };
        if bundle.files.keys().any(|name| name.starts_with(".cargo/")) {
            return Err(BuildError::Rejected(
                "Caller Cargo configuration is forbidden".into(),
            ));
        }
        let manifest = bundle
            .files
            .get("Cargo.toml")
            .and_then(SourceFile::as_text)
            .ok_or_else(|| BuildError::Rejected("Cargo.toml must be UTF-8".into()))?
            .parse::<toml::Value>()
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
        fn untrusted(value: &toml::Value) -> bool {
            value.as_table().is_some_and(|table| {
                table.iter().any(|(key, value)| {
                    if ["dependencies", "build-dependencies", "dev-dependencies"]
                        .contains(&key.as_str())
                    {
                        value.as_table().is_some_and(|deps| {
                            deps.iter()
                                .any(|(name, value)| !trusted_dependency(name, value))
                        })
                    } else {
                        untrusted(value)
                    }
                })
            })
        }
        let isolated = manifest.get("loom").and_then(|loom| loom.get("crates")).is_some() || untrusted(&manifest)
            || bundle.files.contains_key("build.rs")
            || manifest
                .get("package")
                .is_some_and(|package| package.get("build").is_some())
            || dependencies.values().any(is_vendored);
        let preparation_inputs = serde_json::json!({
            "manifest": manifest,
            "lock": bundle.files.get("Cargo.lock"),
            "definitions": definition.deps,
            "sdk": build_fingerprint(&self.root, Lang::Rust)?,
            "isolated": isolated,
        });
        let preparation_key = blake3::hash(&serde_json::to_vec(&preparation_inputs)
            .map_err(|error| BuildError::Rejected(error.to_string()))?).to_hex().to_string();
        if let Some(overlay) = preparation::load(&self.store, &preparation_key)? {
            bundle.files.extend(overlay);
            bundle.validate().map_err(BuildError::Rejected)?;
            return serde_json::to_string(&bundle).map_err(|error| BuildError::Rejected(error.to_string()));
        }
        let staging = self.cache.join("intake").join(&definition.hash);
        if staging.exists() {
            fs::remove_dir_all(&staging).await?;
        }
        let crate_dir = staging.join("crate");
        fs::create_dir_all(&crate_dir).await?;
        for dependency in dependencies.values() {
            if dependency.hash.len() != 64
                || !dependency.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(BuildError::Rejected("invalid dependency hash".into()));
            }
            materialize_rust(
                &self.store,
                &self.root,
                &staging,
                &staging.join("sources").join(&dependency.hash),
                dependency,
                dependencies,
                true,
                isolated,
            )
            .await?;
        }
        materialize_rust(
                &self.store,
            &self.root,
            &staging,
            &crate_dir,
            definition,
            dependencies,
            false,
            isolated,
        )
        .await?;
        let mut command = if isolated {
            let mut command = Command::new(self.root.join("loom-rustc/sandbox.sh"));
            command
                .arg("vendor")
                .arg(&staging)
                .arg(&crate_dir)
                .arg(staging.join("target"))
                .arg(&self.root);
            command
        } else {
            // Only the fixed repository guest + serde graph reaches this path.
            // Seed from its checked-in lock; metadata updates path package entries
            // without running any dependency build scripts.
            if !crate_dir.join("Cargo.lock").exists() {
                seed_build_lock(&self.root.join("Cargo.lock"), &crate_dir.join("Cargo.lock"))
                    .await?;
            }
            let mut command = Command::new("cargo");
            command
                .args(["metadata", "--format-version=1"])
                .current_dir(&crate_dir);
            command
        };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            command.kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| {
            BuildError::Rejected("Rust dependency preparation exceeded 300 seconds".into())
        })??;
        if !output.status.success() {
            return Err(BuildError::Rejected(format!(
                "dependency preparation: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        bundle.files.insert(
            "Cargo.lock".into(),
            SourceFile::from_bytes(fs::read(crate_dir.join("Cargo.lock")).await?),
        );
        if isolated {
            let hash = registry::snapshot_directory(&self.store, &crate_dir.join("vendor"))
                .map_err(|error| BuildError::Rejected(error.to_string()))?;
            bundle.files.insert(preparation::VENDOR_TREE.into(), SourceFile::Text(hash));
        }
        preparation::save(&self.store, &preparation_key, &bundle.files)?;
        bundle.validate().map_err(BuildError::Rejected)?;
        let result = serde_json::to_string(&bundle)
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
        fs::remove_dir_all(staging).await?;
        Ok(result)
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
        let inputs = build_fingerprint(&self.root, definition.lang)?;
        let cached_inputs = fs::read_to_string(directory.join("component.inputs"))
            .await
            .ok();
        if cached_inputs.as_deref() == Some(inputs.as_str())
            && let Ok(component) = fs::read(&component_path).await
        {
            validate_component(&component)?;
            return Ok(BuildOutput {
                component,
                ms: started.elapsed().as_millis() as u64,
                logs: "component cache hit".into(),
                diagnostics: Vec::new(),
                rustc_invocations: 0,
            });
        }
        let mut command = match definition.lang {
            Lang::Ts => {
                fs::write(directory.join("definition.ts"), &definition.source).await?;
                let mut module = String::new();
                for (name, hash) in &definition.deps {
                    if name.is_empty()
                        || !name.chars().all(|character| {
                            character.is_ascii_alphanumeric()
                                || character == '_'
                                || character == '$'
                        })
                        || name.as_bytes()[0].is_ascii_digit()
                    {
                        return Err(BuildError::Rejected(format!(
                            "invalid dependency alias {name}"
                        )));
                    }
                    module.push_str(&format!(
                        "export const {name} = {{hash:{}}};\n",
                        serde_json::to_string(hash)
                            .map_err(|error| BuildError::Rejected(error.to_string()))?
                    ));
                }
                fs::write(directory.join("dependencies.js"), module).await?;
                let mut command = Command::new("bun");
                command
                    .arg(self.root.join("loom-checker/build.ts"))
                    .arg(&directory)
                    .arg(&self.root);
                command
            }
            Lang::Rust => {
                if !is_vendored(definition) && dependencies.values().any(is_vendored) {
                    return Err(BuildError::Rejected("A crate using vendored dependencies must be prepared by the isolated vendor worker".into()));
                }
                for dependency in dependencies.values() {
                    if dependency.lang != Lang::Rust {
                        return Err(BuildError::Rejected("Rust path dependencies must be Rust crates; call TypeScript definitions by hash".into()));
                    }
                    if dependency.hash.len() != 64
                        || !dependency.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        return Err(BuildError::Rejected("invalid dependency hash".into()));
                    }
                    materialize_rust(
                        &self.store,
                        &self.root,
                        &self.cache,
                        &self.cache.join("sources").join(&dependency.hash),
                        dependency,
                        dependencies,
                        true,
                        is_vendored(dependency),
                    )
                    .await?;
                }
                let isolated = is_vendored(definition);
                materialize_rust(
                    &self.store,
                    &self.root,
                    &self.cache,
                    &directory,
                    definition,
                    dependencies,
                    false,
                    isolated,
                )
                .await?;
                sdk::reconcile(sdk::Rebuild {
                    root: &self.root,
                    cache: &self.cache,
                    directory: &directory,
                    definition,
                    isolated,
                })
                .await?;
                let built = direct::build(direct::Request {
                    root: &self.root,
                    cache: &self.cache,
                    directory: &directory,
                    definition,
                    sdk_fingerprint: &inputs,
                    store: &self.store,
                }).await?;
                if !built.diagnostics.is_empty() {
                    return Ok(BuildOutput { component: Vec::new(), ms: started.elapsed().as_millis() as u64,
                        logs: built.logs, diagnostics: built.diagnostics, rustc_invocations: built.rustc_invocations });
                }
                let component = {
                    wit_component::ComponentEncoder::default()
                        .module(&built.bytes).map_err(|error| BuildError::Rejected(error.to_string()))?
                        .adapter(wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME,
                            wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER)
                        .map_err(|error| BuildError::Rejected(error.to_string()))?
                        .validate(true).encode().map_err(|error| BuildError::Rejected(error.to_string()))?
                };
                validate_component(&component)?;
                fs::write(&component_path, &component).await?;
                fs::write(directory.join("component.inputs"), inputs).await?;
                fs::write(directory.join("build.log"), &built.logs).await?;
                return Ok(BuildOutput { component, ms: started.elapsed().as_millis() as u64,
                    logs: built.logs, diagnostics: built.diagnostics, rustc_invocations: built.rustc_invocations });
            }
        };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            command.kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| BuildError::Rejected("component build exceeded 300 seconds".into()))??;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let logs = format!("{stdout}{stderr}");
        fs::write(directory.join("build.log"), &logs).await?;
        let diagnostics = if definition.lang == Lang::Rust {
            cargo_diagnostics(&stdout)
        } else {
            Vec::new()
        };
        if !output.status.success() {
            if !diagnostics.is_empty() {
                return Ok(BuildOutput {
                    component: Vec::new(),
                    ms: started.elapsed().as_millis() as u64,
                    logs,
                    diagnostics,
                    rustc_invocations: 0,
                });
            }
            return Err(BuildError::Rejected(logs));
        }
        let component = fs::read(&component_path).await?;
        validate_component(&component)?;
        fs::write(directory.join("component.inputs"), inputs).await?;
        Ok(BuildOutput {
            component,
            ms: started.elapsed().as_millis() as u64,
            logs,
            diagnostics,
            rustc_invocations: 0,
        })
    }
}
fn trusted_dependency(name: &str, value: &toml::Value) -> bool {
    ["loom", "serde", "serde_json"].contains(&name)
        && value
            .get("package")
            .and_then(toml::Value::as_str)
            .is_none_or(|package| package == name || name == "loom" && package == "loom-guest-rs")
        && value.get("registry").is_none()
        && value.get("git").is_none()
        && value.get("path").is_none()
}

fn is_vendored(definition: &CheckedDef) -> bool {
    serde_json::from_str::<SourceBundle>(&definition.source)
        .is_ok_and(|bundle| bundle.files.keys().any(|name| name.starts_with("vendor/") || name == preparation::VENDOR_TREE))
}

const VENDOR_CONFIG: &str = include_str!("../../../loom-rustc/vendor-config.toml");

fn build_fingerprint(root: &Path, lang: Lang) -> Result<String, BuildError> {
    fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if !["node_modules", "target", ".git"]
                    .contains(&entry.file_name().to_string_lossy().as_ref())
                {
                    collect(&path, files)?;
                }
            } else if path.extension().is_some_and(|extension| {
                ["ts", "rs", "toml", "wit", "lock", "json", "sh"]
                    .iter()
                    .any(|allowed| extension == *allowed)
            }) {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = vec![root.join("loom-wit/handler.wit")];
    match lang {
        Lang::Ts => {
            collect(&root.join("loom-guest-ts"), &mut files)?;
            files.extend([
                root.join("loom-checker/build.ts"),
                root.join("loom-checker/bun.lock"),
            ]);
        }
        Lang::Rust => {
            for directory in [
                "crates/loom-guest-rs",
                "crates/loom-guest-macros",
                "crates/loom-proto",
                "loom-rustc",
            ] {
                collect(&root.join(directory), &mut files)?;
            }
            files.push(root.join("Cargo.lock"));
        }
    }
    files.sort();
    let mut hash = blake3::Hasher::new();
    hash.update(b"loom-component-build-v2-dag-cbor");
    hash.update(include_bytes!("direct.rs"));
    hash.update(include_bytes!("direct/artifacts.rs"));
    hash.update(include_bytes!("preparation.rs"));
    for path in files {
        let relative = path
            .strip_prefix(root)
            .map_err(|error| BuildError::Rejected(error.to_string()))?
            .to_string_lossy();
        let bytes = std::fs::read(&path)?;
        hash.update(&(relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(hash.finalize().to_hex().to_string())
}

async fn materialize_rust(
    store: &loom_store::Store,
    root: &Path,
    cache: &Path,
    directory: &Path,
    definition: &CheckedDef,
    dependencies: &BTreeMap<String, CheckedDef>,
    dependency: bool,
    isolated: bool,
) -> Result<(), BuildError> {
    let mut files = if definition.source.trim_start().starts_with('{') {
        let bundle: loom_check::SourceBundle = serde_json::from_str(&definition.source)
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
        bundle.validate().map_err(BuildError::Rejected)?;
        bundle.files
    } else {
        BTreeMap::from([(
            "src/lib.rs".to_string(),
            SourceFile::Text(definition.source.clone()),
        )])
    };
    if files.keys().any(|name| name.starts_with("loom-crates/")) {
        return Err(BuildError::Rejected("loom-crates source paths are host-owned".into()));
    }
    let mut manifest = if let Some(source) = files.remove("Cargo.toml") {
        source
            .as_text()
            .ok_or_else(|| BuildError::Rejected("Cargo.toml must be UTF-8".into()))?
            .parse::<toml::Value>()
            .map_err(|error| BuildError::Rejected(error.to_string()))?
    } else {
        "[package]\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\nserde_json=\"1\"\n".parse::<toml::Value>().map_err(|error|BuildError::Rejected(error.to_string()))?
    };
    let table = manifest
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("Cargo.toml must be a table".into()))?;
    fn validate_manifest(value: &toml::Value, isolated: bool) -> Result<(), BuildError> {
        if let Some(table) = value.as_table() {
            for (name, value) in table {
                if ["patch", "replace"].contains(&name.as_str()) {
                    return Err(BuildError::Rejected(format!(
                        "Cargo [{name}] overrides are unavailable"
                    )));
                }
                if ["dependencies", "build-dependencies", "dev-dependencies"]
                    .contains(&name.as_str())
                {
                    if let Some(deps) = value.as_table() {
                        for (name, dependency) in deps {
                            if dependency.get("path").is_some()
                                || dependency.get("git").is_some()
                                || dependency.get("registry").is_some()
                            {
                                return Err(BuildError::Rejected(format!(
                                    "{name}: use locked crates.io or loom.deps, not path/git"
                                )));
                            }
                            if !isolated && !trusted_dependency(name, dependency) {
                                return Err(BuildError::Rejected(format!(
                                    "{name} requires the isolated vendored build worker"
                                )));
                            }
                        }
                    }
                } else {
                    validate_manifest(value, isolated)?;
                }
            }
        }
        Ok(())
    }
    validate_manifest(&toml::Value::Table(table.clone()), isolated)?;
    let crates = loom_check::crate_dependencies(&toml::to_string(&toml::Value::Table(table.clone())).map_err(|error| BuildError::Rejected(error.to_string()))?)
        .map_err(BuildError::Rejected)?;
    let crate_aliases: std::collections::BTreeSet<_> = crates.keys().cloned().collect();
    let mut materialized_crates = std::collections::BTreeSet::new();
    for (alias, dependency) in crates {
        let relative = format!("loom-crates/{}", dependency.hash);
        let destination = directory.join(&relative);
        if materialized_crates.insert(dependency.hash.clone()) {
            if destination.exists() { fs::remove_dir_all(&destination).await?; }
            registry::CrateRegistry::new(store.clone()).materialize(&dependency.hash, &destination)
                .map_err(|error| BuildError::Rejected(error.to_string()))?;
        }
        let source = std::fs::read_to_string(destination.join("Cargo.toml"))?;
        let crate_manifest: toml::Value = source.parse().map_err(|error: toml::de::Error| BuildError::Rejected(error.to_string()))?;
        let package = crate_manifest.get("package").and_then(|package| package.get("name")).and_then(toml::Value::as_str).ok_or_else(|| BuildError::Rejected("crate package name missing".into()))?;
        let mut specification = toml::map::Map::new();
        specification.insert("path".into(), toml::Value::String(relative));
        specification.insert("package".into(), toml::Value::String(package.into()));
        specification.insert("features".into(), toml::Value::Array(dependency.features.into_iter().map(toml::Value::String).collect()));
        specification.insert("default-features".into(), toml::Value::Boolean(dependency.default_features));
        let dependencies = table.entry("dependencies").or_insert_with(|| toml::Value::Table(Default::default())).as_table_mut().ok_or_else(|| BuildError::Rejected("dependencies must be a table".into()))?;
        if dependencies.insert(alias.clone(), toml::Value::Table(specification)).is_some() {
            return Err(BuildError::Rejected(format!("crate alias {alias} declared twice")));
        }
    }
    table.remove("loom");
    table.insert("workspace".into(), toml::Value::Table(Default::default()));
    let package = table
        .get_mut("package")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| BuildError::Rejected("Cargo.toml requires [package]".into()))?;
    if package.contains_key("workspace")
        || package
            .get("metadata")
            .and_then(|metadata| metadata.get("component"))
            .is_some()
    {
        return Err(BuildError::Rejected("Workspace redirects and component metadata are host-owned; the guest boundary is loom-wit".into()));
    }
    if !isolated && (package.contains_key("build") || files.contains_key("build.rs")) {
        return Err(BuildError::Rejected(
            "User build scripts require an isolated build worker, which is not configured".into(),
        ));
    }
    if dependency {
        package.insert(
            "name".into(),
            toml::Value::String(format!("loom-definition-{}", &definition.hash[..16])),
        );
    } else {
        package
            .entry("name")
            .or_insert_with(|| toml::Value::String("loom-definition".into()));
    }
    let library = table
        .entry("lib")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("[lib] must be a table".into()))?;
    if library.get("proc-macro").and_then(toml::Value::as_bool) == Some(true) {
        return Err(BuildError::Rejected(
            "A definition cannot be a procedural macro crate".into(),
        ));
    }
    library.insert("path".into(), toml::Value::String("src/lib.rs".into()));
    library.insert(
        "crate-type".into(),
        toml::Value::Array(vec![toml::Value::String(
            if dependency { "rlib" } else { "cdylib" }.into(),
        )]),
    );
    let features = table
        .entry("features")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("[features] must be a table".into()))?;
    features.insert("loom-dependency".into(), toml::Value::Array(Vec::new()));
    let manifest_deps = table
        .entry("dependencies")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("[dependencies] must be a table".into()))?;
    for (name, value) in manifest_deps.iter() {
        if crate_aliases.contains(name) { continue; }
        if value.get("path").is_some()
            || value.get("git").is_some()
            || value.get("registry").is_some()
        {
            return Err(BuildError::Rejected(format!(
                "dependency {name}: use loom.deps hashes or locked crates.io sources"
            )));
        }
        if !isolated && !trusted_dependency(name, value) {
            return Err(BuildError::Rejected(format!(
                "dependency {name} requires the isolated vendored build worker, which is not configured"
            )));
        }
    }
    let mut guest_dependency = toml::map::Map::new();
    guest_dependency.insert(
        "package".into(),
        toml::Value::String("loom-guest-rs".into()),
    );

    guest_dependency.insert(
        "path".into(),
        toml::Value::String(
            root.join("crates/loom-guest-rs")
                .to_string_lossy()
                .into_owned(),
        ),
    );
    manifest_deps.insert("loom".into(), toml::Value::Table(guest_dependency));
    for (name, hash) in &definition.deps {
        let target = dependencies
            .get(hash)
            .ok_or_else(|| BuildError::Rejected(format!("missing dependency source {hash}")))?;
        if target.lang != Lang::Rust {
            return Err(BuildError::Rejected(format!(
                "{name} is not a Rust crate; invoke it through loom::call"
            )));
        }
        let mut entry = toml::map::Map::new();
        entry.insert(
            "package".into(),
            toml::Value::String(format!("loom-definition-{}", &hash[..16])),
        );
        entry.insert(
            "path".into(),
            toml::Value::String(
                cache
                    .join("sources")
                    .join(hash)
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        entry.insert(
            "features".into(),
            toml::Value::Array(vec![toml::Value::String("loom-dependency".into())]),
        );
        manifest_deps.insert(name.clone(), toml::Value::Table(entry));
    }
    if files.keys().any(|name| name.starts_with("vendor/") || name == preparation::VENDOR_TREE) {
        files.insert(
            ".cargo/config.toml".into(),
            SourceFile::Text(VENDOR_CONFIG.into()),
        );
    } else if files.keys().any(|name| name.starts_with(".cargo/")) {
        return Err(BuildError::Rejected(
            "Caller cargo configuration is forbidden".into(),
        ));
    }
    if dependency {
        for (name, contents) in &mut files {
            if !name.ends_with(".rs") || name.starts_with("vendor/") {
                continue;
            }
            let source = contents
                .text_mut()
                .ok_or_else(|| BuildError::Rejected(format!("{name} must be UTF-8")))?;
            let mut file =
                syn::parse_file(source).map_err(|error| BuildError::Rejected(error.to_string()))?;
            for item in &mut file.items {
                if let syn::Item::Fn(function) = item {
                    for attribute in &mut function.attrs {
                        if attribute
                            .path()
                            .segments
                            .last()
                            .is_some_and(|segment| segment.ident == "def")
                        {
                            let hash = &definition.hash;
                            *attribute = syn::parse_quote!(#[loom::def(hash = #hash)]);
                        }
                    }
                }
            }
            *source = prettyplease::unparse(&file);
        }
    }
    fs::create_dir_all(directory).await?;
    if let Some(tree) = files.get(preparation::VENDOR_TREE) {
        let hash = tree.as_text().ok_or_else(|| BuildError::Rejected("vendor tree must be a hash".into()))?;
        preparation::materialize_vendor(store, cache, directory, hash)?;
    }
    for (name, source) in files {
        let path = directory.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(path, source.bytes().map_err(BuildError::Rejected)?).await?;
    }
    fs::write(
        directory.join("Cargo.toml"),
        toml::to_string(&manifest).map_err(|error| BuildError::Rejected(error.to_string()))?,
    )
    .await?;
    Ok(())
}

fn cargo_artifact(output: &str, manifest: &Path, target: &Path) -> Result<PathBuf, BuildError> {
    let manifest = std::fs::canonicalize(manifest)?;
    let target = std::fs::canonicalize(target)?;
    let mut artifact = None;
    for line in output.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(path) = message["manifest_path"].as_str() else {
            continue;
        };
        if std::fs::canonicalize(path).ok().as_ref() != Some(&manifest) {
            continue;
        }
        if !message["target"]["crate_types"]
            .as_array()
            .is_some_and(|types| types.iter().any(|kind| kind == "cdylib"))
        {
            continue;
        }
        if let Some(paths) = message["filenames"].as_array() {
            for path in paths.iter().filter_map(serde_json::Value::as_str) {
                if Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "wasm")
                {
                    let path = std::fs::canonicalize(path)?;
                    if !path.starts_with(&target) {
                        return Err(BuildError::Rejected(
                            "Cargo artifact escaped the build target".into(),
                        ));
                    }
                    artifact = Some(path);
                }
            }
        }
    }
    artifact.ok_or_else(|| {
        BuildError::Rejected("Cargo did not report a component-producing root artifact".into())
    })
}

fn validate_component(bytes: &[u8]) -> Result<(), BuildError> {
    if !bytes.starts_with(b"\0asm\x0d\0\x01\0") {
        return Err(BuildError::Rejected(
            "builder output is not a WebAssembly component".into(),
        ));
    }
    Ok(())
}
pub fn cargo_diagnostics(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value.get("reason")?.as_str()? != "compiler-message" {
                return None;
            }
            let message = value.get("message")?;
            if message.get("level")?.as_str()? != "error" {
                return None;
            }
            let span = message
                .get("spans")
                .and_then(|spans| spans.as_array())
                .and_then(|spans| spans.iter().find(|span| span["is_primary"] == true));
            Some(Diagnostic {
                lang: Lang::Rust,
                file: span
                    .and_then(|span| span["file_name"].as_str())
                    .unwrap_or("src/lib.rs")
                    .into(),
                line: span
                    .and_then(|span| span["line_start"].as_u64())
                    .unwrap_or(1) as usize,
                col: span
                    .and_then(|span| span["column_start"].as_u64())
                    .unwrap_or(1) as usize,
                code: message["code"]["code"]
                    .as_str()
                    .unwrap_or("RUST_BUILD")
                    .into(),
                message: message["message"]
                    .as_str()
                    .unwrap_or("Rust build failed")
                    .into(),
                snippet: span
                    .and_then(|span| span["text"][0]["text"].as_str())
                    .map(str::to_owned),
                hint: message["children"]
                    .as_array()
                    .and_then(|children| children.iter().find(|child| child["level"] == "help"))
                    .and_then(|child| child["message"].as_str())
                    .map(str::to_owned),
            })
        })
        .collect()
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
mod tests {
    use super::*;
    #[tokio::test]
    async fn crate_pins_materialize_but_caller_paths_and_overlays_are_rejected() {
        let store = loom_store::Store::memory().unwrap();
        let manifest_hash = store.put("blob", b"[package]\nname='tiny'\nversion='1.0.0'\n").unwrap();
        let tree = loom_proto::Tree { entries: vec![loom_proto::TreeEntry { name: "Cargo.toml".into(), reference: store.reference(&manifest_hash, loom_proto::RAW_CODEC).unwrap(), directory: false, executable: false }] };
        let hash = store.put_value("tree", &tree).unwrap();
        let manifest = format!("[package]\nname='loom-definition'\nversion='0.1.0'\nedition='2024'\n[loom.crates]\ntiny={{hash='{hash}'}}\n");
        let mut files = BTreeMap::new();
        files.insert("Cargo.toml".into(), SourceFile::Text(manifest));
        files.insert("src/lib.rs".into(), SourceFile::Text("pub fn main() -> i64 { 42 }".into()));
        let mut bundle = SourceBundle { files };
        let mut definition = CheckedDef { hash: "a".repeat(64), lang: Lang::Rust, name: "test".into(), source: serde_json::to_string(&bundle).unwrap(), deps: BTreeMap::new(), sig: Default::default(), diagnostics: vec![] };
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let directory = std::env::temp_dir().join(format!("loom-crate-paths-{}", std::process::id()));
        if directory.exists() { fs::remove_dir_all(&directory).await.unwrap(); }
        materialize_rust(&store, &root, &directory, &directory, &definition, &BTreeMap::new(), false, true).await.unwrap();
        let emitted: toml::Value = fs::read_to_string(directory.join("Cargo.toml")).await.unwrap().parse().unwrap();
        assert_eq!(emitted["dependencies"]["tiny"]["path"].as_str(), Some(format!("loom-crates/{hash}").as_str()));
        assert_eq!(fs::read(directory.join(format!("loom-crates/{hash}/Cargo.toml"))).await.unwrap(), store.get(&manifest_hash).unwrap().unwrap());
        bundle.files.insert(format!("loom-crates/{hash}/Cargo.toml"), SourceFile::Text("tampered".into()));
        definition.source = serde_json::to_string(&bundle).unwrap();
        assert!(materialize_rust(&store, &root, &directory, &directory, &definition, &BTreeMap::new(), false, true).await.unwrap_err().to_string().contains("host-owned"));
        bundle.files.remove(&format!("loom-crates/{hash}/Cargo.toml"));
        bundle.files.insert("Cargo.toml".into(), SourceFile::Text(format!("[package]\nname='loom-definition'\nversion='0.1.0'\n[dependencies]\ntiny={{path='loom-crates/{hash}'}}\n")));
        definition.source = serde_json::to_string(&bundle).unwrap();
        assert!(materialize_rust(&store, &root, &directory, &directory, &definition, &BTreeMap::new(), false, true).await.unwrap_err().to_string().contains("not path/git"));
        fs::remove_dir_all(directory).await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn immutable_sdk_lock_becomes_mutable_build_input() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("loom-readonly-lock-{}", std::process::id()));
        fs::create_dir_all(&directory).await.unwrap();
        let source = directory.join("sdk.lock");
        let destination = directory.join("build.lock");
        fs::write(&source, b"version = 4\n").await.unwrap();
        fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444))
            .await
            .unwrap();
        seed_build_lock(&source, &destination).await.unwrap();
        assert_ne!(
            fs::metadata(&destination)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o200,
            0
        );
        fs::write(&destination, b"updated build lock")
            .await
            .unwrap();
        assert_eq!(fs::read(&source).await.unwrap(), b"version = 4\n");
        assert_eq!(
            fs::metadata(&source).await.unwrap().permissions().mode() & 0o222,
            0
        );
        fs::remove_dir_all(directory).await.unwrap();
    }
    #[test]
    fn cargo_artifact_uses_the_root_manifest_and_reported_filename() {
        let root =
            std::env::temp_dir().join(format!("loom-artifact-01a084e8-{}", std::process::id()));
        std::fs::create_dir_all(root.join("target")).unwrap();
        let manifest = root.join("Cargo.toml");
        std::fs::write(&manifest, "[package]").unwrap();
        let expected = root.join("target/custom_library.wasm");
        std::fs::write(&expected, b"new artifact").unwrap();
        std::fs::write(root.join("target/loom_definition.wasm"), b"stale artifact").unwrap();
        let message = serde_json::json!({"reason":"compiler-artifact","manifest_path":manifest,"target":{"crate_types":["cdylib"]},"filenames":[expected]});
        let actual = cargo_artifact(&message.to_string(), &manifest, &root.join("target")).unwrap();
        assert_eq!(std::fs::read(actual).unwrap(), b"new artifact");
        assert!(cargo_artifact("", &manifest, &root.join("target")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_core_module_as_component() {
        assert!(validate_component(b"\0asm\x01\0\0\0").is_err());
    }
    #[test]
    fn parses_cargo_error_amid_benign_warnings() {
        let diagnostics = cargo_diagnostics(
            "warning: unrelated\n{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"mismatched types\",\"code\":{\"code\":\"E0308\"},\"spans\":[],\"children\":[]}}\n",
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E0308");
    }
}
