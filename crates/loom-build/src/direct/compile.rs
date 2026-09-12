use super::*;

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
    let target_name = "wasm32-unknown-unknown";
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
    hasher.update(b"rustc-contract-v4-core-handlers-residual-rows");
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
            fs::create_dir_all(&root_incremental).await?;
            recipe.relocate(directory, &target.join("root-output"), &root_incremental)?;
            let command = if isolated {
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
    let shareable =
        graph_shareable(root, cache, directory, &target, isolated, Some(&sysroot)).await?;
    if !shareable {
        return Err(rejected(
            "untrusted host build scripts and procedural macros are not admitted",
        ));
    }
    let mirror = graph.join("unit-cache");
    compiler_cache::prepare(store, &mirror, &target, &compiler_identity)?;
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
    command
        .env("LOOM_LOCKED", "1")
        .env("LOOM_RUST_TARGET", target_name)
        .env("LOOM_CAS_SOURCES", cache.join("source-trees"))
        .env("LOOM_COMPILER_CACHE_OWNER", helper_owner)
        .env("LOOM_COMPILER_CACHE_MIRROR", &mirror)
        .env("LOOM_ROOT_INCREMENTAL", &root_incremental)
        .env("LOOM_TRUSTED_SOURCES", graph.join("trusted-sources"));
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
        .filter(|line| *line == "rustc-invocation")
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
