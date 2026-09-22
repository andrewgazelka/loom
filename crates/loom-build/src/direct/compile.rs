use super::*;

pub(crate) struct Request<'a> {
    pub root: &'a Path,
    pub cache: &'a Path,
    pub directory: &'a Path,
    pub definition: &'a CheckedDef,
    /// The checked dependency closure, keyed by definition hash.
    pub dependencies: &'a BTreeMap<String, CheckedDef>,
    pub sdk_fingerprint: &'a str,
    pub store: &'a Store,
    /// Resolved once per build by `Builder::prepared`; never re-resolved here.
    pub toolchain: &'a crate::GuestToolchain,
    pub driver: &'a crate::identity::Driver,
    pub identity_directory: &'a Path,
}

/// Compile the root crate of `directory`. A graph key (manifest, lock, compiler
/// identity, SDK fingerprint, target, vendored input) selects a dependency
/// graph under `cache/rust-artifacts/<key>`; a graph with a stored recipe is
/// replayed as one direct rustc invocation, otherwise Cargo resolves it cold.
/// `Built::stages` names every millisecond of this function:
///
/// warm replay: `compiler_setup_ms` (path canonicalization, key, root
/// workspace copy) → `graph_load_ms` (recipe read, path rebase, unit sources)
/// → `artifact_restore_ms` (stamp check, or full CAS restore and repair) →
/// `root_rustc_ms` → `entry_abi_rustc_ms` → `identity_publish_ms`.
///
/// cold bootstrap: `compiler_setup_ms` → `graph_load_ms` (miss) →
/// `admission_ms` (cargo metadata, source policy) → `compiler_mirror_ms` →
/// `cargo_bootstrap_ms` → `root_rustc_ms` → `entry_abi_rustc_ms` →
/// `artifact_capture_ms` → `identity_publish_ms`.
pub(crate) async fn build(request: Request<'_>) -> Result<Built, BuildError> {
    let mut stages = crate::Stages::start();
    let Request {
        root,
        cache,
        directory,
        definition,
        dependencies,
        sdk_fingerprint,
        store,
        toolchain,
        driver,
        identity_directory,
    } = request;
    // Cargo canonicalizes paths (notably /tmp -> /private/tmp on macOS). Use
    // that same spelling for graph ownership and relocation, not string aliases.
    let root_path = std::fs::canonicalize(root)?;
    let cache_path = std::fs::canonicalize(cache)?;
    let directory_path = std::fs::canonicalize(directory)?;
    let root = root_path.as_path();
    let cache = cache_path.as_path();
    let directory = directory_path.as_path();
    let target_name = "wasm32-unknown-unknown";
    let sysroot = toolchain.sysroot.clone();
    let mut hasher = blake3::Hasher::new();
    // v9: the stored root recipe carries `-C debuginfo=1` (`Recipe::line_tables`);
    // graphs recorded before it would replay without line tables and are never
    // read. v8 added `ArtifactFile::len`.
    hasher.update(b"rustc-contract-v9-line-tables");
    let manifest_bytes = fs::read_to_string(directory.join("Cargo.toml"))
        .await?
        .replace(root.to_string_lossy().as_ref(), "$SDK")
        .replace(cache.to_string_lossy().as_ref(), "$CACHE")
        .into_bytes();
    let compiler_identity = format!(
        "{}\nhash-rustc:{}",
        toolchain.version, driver.toolchain_hash
    );
    let lock_bytes = fs::read(directory.join("Cargo.lock")).await?;
    for bytes in [
        manifest_bytes,
        lock_bytes.clone(),
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
    let published_identity_directory = identity_directory;
    let staged_identity_directory = target.join("item-identity").join(&definition.hash);
    fs::create_dir_all(&staged_identity_directory).await?;
    let identity_directory = staged_identity_directory.as_path();
    // Cross-crate references into definition dependencies hash the stored
    // item, so the driver needs each dependency's document at hand.
    crate::identity::stage_dependency_items(store, dependencies, identity_directory)?;
    let lineage = blake3::hash(definition.name.as_bytes())
        .to_hex()
        .to_string();
    let workspace = graph.join("root-sources").join(&lineage);
    materialize_root_workspace(directory, &workspace)?;
    let directory = workspace.as_path();
    let root_incremental = target.join("incremental").join(&lineage);
    let isolated = directory.join("vendor").is_dir();
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(directory.join("Cargo.toml")).await?)
            .map_err(rejected)?;
    let root_build_script = directory.join("build.rs").exists()
        || manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .is_some();
    stages.checkpoint("compiler_setup_ms");
    let graph_identity = serde_json::json!({"dependency_graph":key});
    let stored = if root_build_script {
        None
    } else {
        read_graph(store, &key)?
    };
    if let Some(mut recipe) = stored {
        let replay_compiler = driver.path.to_string_lossy().into_owned();
        recipe.rebase_graph(root, cache, directory, &sysroot, &replay_compiler)?;
        recipe.restore_sources(store, cache, &graph)?;
        stages.checkpoint("graph_load_ms");
        let mut notes = Vec::new();
        let restored = match recipe.artifacts_stamped(&graph) {
            Ok(()) => true,
            Err(reason) => {
                notes.push(format!(
                    "artifact restore: {reason}; verifying every artifact"
                ));
                let restored = recipe.restore_artifacts(store)?;
                if restored {
                    recipe.write_artifact_stamp(&graph)?;
                }
                restored
            }
        };
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
        let complete = restored || {
            let complete = recipe.restore_artifacts(store)?;
            if complete {
                recipe.write_artifact_stamp(&graph)?;
            }
            complete
        };
        stages.checkpoint("artifact_restore_ms");
        if complete {
            fs::create_dir_all(&root_incremental).await?;
            recipe.relocate(directory, &target.join("root-output"), &root_incremental)?;
            recipe.compiler = driver.path.to_string_lossy().into_owned();
            recipe
                .environment
                .extend(crate::identity::Driver::environment(identity_directory));
            let mut command = if isolated {
                fs::write(target.join("direct.sh"), recipe.shell()).await?;
                let mut command = Command::new(root.join("rustc/sandbox.sh"));
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
            driver.configure(&mut command, identity_directory);
            let output = run(command).await?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            let diagnostics = rustc_diagnostics(&stderr);
            stages.checkpoint("root_rustc_ms");
            let notes = notes.join("\n");
            let logs = format!(
                "{graph_identity}\ndirect rustc; dependency graph {key}\n{notes}\n{stderr}"
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
                    stages,
                });
            }
            super::entry_abi::compile(super::entry_abi::Request {
                recipe: &recipe,
                identity: identity_directory,
                root,
                cache,
                target: &target,
                isolated,
            })
            .await?;
            stages.checkpoint("entry_abi_rustc_ms");
            crate::identity::publish(identity_directory, published_identity_directory)?;
            let bytes = fs::read(recipe.output()?).await?;
            stages.checkpoint("identity_publish_ms");
            return Ok(Built {
                bytes,
                logs,
                diagnostics,
                rustc_invocations: repairs + 2,
                stages,
            });
        }
    } else {
        stages.checkpoint("graph_load_ms");
    }
    artifacts::initialize_index(store)?;
    let shareable = graph_shareable(
        root,
        cache,
        directory,
        &target,
        isolated,
        Some(&sysroot),
        toolchain,
    )
    .await?;
    if !shareable {
        return Err(rejected(
            "untrusted host build scripts and procedural macros are not admitted",
        ));
    }
    stages.checkpoint("admission_ms");
    let mirror = graph.join("unit-cache");
    compiler_cache::prepare(store, &mirror, &target, &compiler_identity)?;
    stages.checkpoint("compiler_mirror_ms");
    let helper_owner = std::env::var_os("LOOM_COMPILER_CACHE_OWNER")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?);
    let mut command = if isolated {
        let mut command = Command::new(root.join("rustc/sandbox.sh"));
        // Sandbox target must be under its writable source root.
        command
            .arg("build")
            .arg(cache)
            .arg(directory)
            .arg(&target)
            .arg(root);
        command
    } else {
        let mut command = Command::new(root.join("rustc/build.sh"));
        command.arg(directory).arg(&target);
        command
    };
    compiler_environment(&mut command);
    toolchain.configure(&mut command)?;
    command
        .env("RUSTC", &driver.path)
        .env("LOOM_LOCKED", "1")
        .env("LOOM_RUST_TARGET", target_name)
        .env("LOOM_CAS_SOURCES", cache.join("source-trees"))
        .env("LOOM_COMPILER_CACHE_OWNER", helper_owner)
        .env("LOOM_COMPILER_CACHE_MIRROR", &mirror)
        .env("LOOM_ROOT_INCREMENTAL", &root_incremental)
        .env("LOOM_TRUSTED_SOURCES", graph.join("trusted-sources"));
    let output = bootstrap(command).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let logs = format!(
        "{graph_identity}\ncargo dependency bootstrap; graph {key}; cross-graph publication {shareable}\n{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let diagnostics = cargo_diagnostics(&stdout);
    let rustc_invocations = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| *line == "rustc-invocation")
        .count();
    stages.checkpoint("cargo_bootstrap_ms");
    if !output.status.success() {
        if diagnostics.is_empty() {
            return Err(rejected(logs));
        }
        return Ok(Built {
            bytes: Vec::new(),
            logs,
            diagnostics,
            rustc_invocations,
            stages,
        });
    }
    crate::cargo_artifact(&stdout, &directory.join("Cargo.toml"), &target)?;
    let mut root_recipe = Recipe::parse(
        &fs::read(target.join("root-rustc.recipe")).await?,
        directory,
    )?;
    root_recipe.compiler = driver.path.to_string_lossy().into_owned();
    // Cargo compiled the root with `-C debuginfo=0`; the hashing replay below
    // and the entry ABI compile that overwrites its output carry line tables.
    root_recipe.line_tables();
    let mut hash_command = Command::new(&driver.path);
    compiler_environment(&mut hash_command);
    hash_command
        .args(&root_recipe.arguments)
        .envs(&root_recipe.environment)
        .current_dir(root_recipe.working_directory());
    driver.configure(&mut hash_command, identity_directory);
    let hash_command = if isolated {
        root_recipe
            .environment
            .extend(crate::identity::Driver::environment(identity_directory));
        fs::write(target.join("direct.sh"), root_recipe.shell()).await?;
        let mut command = Command::new(root.join("rustc/sandbox.sh"));
        compiler_environment(&mut command);
        command
            .arg("rustc")
            .arg(cache)
            .arg(directory)
            .arg(&target)
            .arg(root);
        driver.configure(&mut command, identity_directory);
        command
    } else {
        hash_command
    };
    let hash_output = run(hash_command).await?;
    if !hash_output.status.success() {
        return Err(rejected(format!(
            "hash-rustc driver {}: {}",
            driver.path.display(),
            String::from_utf8_lossy(&hash_output.stderr)
        )));
    }
    stages.checkpoint("root_rustc_ms");
    super::entry_abi::compile(super::entry_abi::Request {
        recipe: &root_recipe,
        identity: identity_directory,
        root,
        cache,
        target: &target,
        isolated,
    })
    .await?;
    stages.checkpoint("entry_abi_rustc_ms");
    if !root_build_script {
        let mut recipe = Recipe::parse(
            &fs::read(target.join("root-rustc.recipe")).await?,
            directory,
        )?;
        // The stored graph replays exactly what `hash_command` ran.
        recipe.line_tables();
        let mut source_roots = vec![
            cache.to_owned(),
            root.join("crates/loom-guest-rs"),
            root.join("crates/loom-proto"),
            sysroot.join("lib/rustlib/src/rust/library"),
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
        recipe.write_artifact_stamp(&graph)?;
        definition_rlibs::capture(
            store,
            &recipe.units,
            dependencies,
            &definition_rlibs::Context {
                compiler_identity: &compiler_identity,
                lock: &lock_bytes,
                target: target_name,
            },
        )?;
        recipe.layout = Some(Layout {
            root: root.into(),
            cache: cache.into(),
            sysroot,
        });
        write_graph(store, &key, &recipe)?;
    }
    stages.checkpoint("artifact_capture_ms");
    crate::identity::publish(identity_directory, published_identity_directory)?;
    let bytes = fs::read(root_recipe.output()?).await?;
    stages.checkpoint("identity_publish_ms");
    Ok(Built {
        bytes,
        logs,
        diagnostics,
        rustc_invocations: rustc_invocations + 2,
        stages,
    })
}
